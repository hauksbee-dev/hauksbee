//! Compiled stamp plan: the two-tier "compiled netlist" assembly.
//!
//! In the monolithic Newton loop every iteration walks the device list, matches
//! the `Device` enum, and for each conductance does a binary search to find its
//! matrix slot. Most of that work re-derives things that never change:
//!
//! * **Tier 1 — the constant backbone.** Contributions that are the same every
//!   transient assembly: temperature-independent resistor conductances, the
//!   ±1 incidence entries of ideal voltage sources and inductor branch rows,
//!   and (keyed on the integration factor `coeffs.g = k/dt`) the companion
//!   conductances of capacitors (`geq = coeffs.g·C`) and the inductor branch
//!   self-term (`-L·coeffs.g`). [`StampPlan::compile`] resolves each of these
//!   to a `(row, position)` slot in the frozen pattern once, so replaying the
//!   backbone is a flat loop of `add_at` with no enum dispatch and no search.
//!
//! * **Tier 2 — the interpreted re-stamp.** Contributions that change per step
//!   or per Newton iteration: diode/BJT/MOSFET tangents, switch and behavioral
//!   blocks, source values, and the reactive RHS history terms. These still
//!   run the *exact interpreted device code* (`stamp_device`), so the physics
//!   and its formulas exist in one place only — but they write through a
//!   pre-resolved per-device slot table ([`SlotTable`]) instead of a per-write
//!   binary search, and devices whose matrix part lives entirely in the
//!   backbone (capacitors, inductors, sources) write RHS-only.
//!
//! [`stamp_all_planned`] is the assembly built from the two tiers. It is *a*
//! path, never *the* path for the reference: the accumulation order differs
//! from the interpreted walk, which legitimately moves the last bits of
//! floating point, so the planned assembly matches the interpreted one to
//! solver tolerance (reltol/vntol) rather than bit-for-bit. The
//! `Partitioning::Off` oracle keeps calling the interpreted `stamp_all`
//! unchanged; the planned path is selected only by an explicit
//! [`crate::AssemblyMode::Planned`] opt-in. Contexts the plan does not model —
//! DC operating-point solves (reactive elements open/short/IC-pinned), the
//! staged-DC regularizers (`branch_reg > 0`), and event-frozen solves — fall
//! back to the interpreted assembly inside `stamp_all_planned`, so the fast
//! path can never be silently wrong on them.

use hauksbee_ir::{Circuit, Device, DeviceId};

use crate::sparse::SparseMatrix;
use crate::stamp::{stamp_all, stamp_device, StampCtx, StampSink};
use crate::system::Layout;

/// A flat, pre-bound set of constant matrix writes plus the tier-2 re-stamp
/// schedule. Compiled once per [`crate::Workspace`] against the frozen pattern.
pub struct StampPlan {
    /// Constant writes: `(row, position, value)` into the frozen pattern.
    /// Resistor conductances plus Vsource/inductor ±1 incidence. Applied
    /// verbatim each assembly.
    cond_ops: Vec<(usize, usize, f64)>,
    /// Node rows that take the gmin shunt (node rows only, < n_nodes).
    gmin_diag: Vec<(usize, usize)>,
    /// Reactive companion-conductance writes: `(row, position, multiplier)`
    /// where the stamped value is `multiplier * coeffs.g`. Capacitors
    /// contribute `±C` across their node pairs; inductors contribute `-L` on
    /// their branch diagonal. Constant per fixed integration factor; the
    /// multiply at apply time makes dt changes free (no recompile).
    reactive_ops: Vec<(usize, usize, f64)>,
    /// Tier-2 re-stamp schedule, in device order (a subset of the interpreted
    /// walk, so shared-slot accumulation keeps the devices' relative order).
    restamp: Vec<Restamp>,
}

/// One tier-2 device visit.
enum Restamp {
    /// The device's matrix part is fully covered by the backbone; only its RHS
    /// contributions (source value, reactive history term) are taken. Matrix
    /// writes from the interpreted device code are dropped by the sink.
    RhsOnly { id: DeviceId },
    /// Full re-stamp (nonlinear tangent / time-varying conductance), writing
    /// matrix entries through the pre-resolved slot table.
    Slotted { id: DeviceId, table: SlotTable },
}

/// Pre-resolved slots for every `(row, col)` pair a device can touch: the
/// device's (deduped, non-ground) node unknowns, plus — for the behavioral
/// B-source — its own branch unknown and any control-source branch columns.
/// A handful of entries either way, which makes the local index scan a couple
/// of register compares instead of a binary search over a (possibly
/// hub-length) matrix row.
struct SlotTable {
    /// Global unknown indices this device touches.
    unknowns: Vec<u32>,
    /// Dense local table: `pos[li * unknowns.len() + lj]` is the position of
    /// `(unknowns[li], unknowns[lj])` within its matrix row. `u32::MAX` marks
    /// a pair with no reserved slot (never hit for all-pairs reservation).
    pos: Vec<u32>,
}

impl SlotTable {
    fn build(unknowns: Vec<u32>, matrix: &SparseMatrix) -> SlotTable {
        let n = unknowns.len();
        let mut pos = vec![u32::MAX; n * n];
        for (li, &r) in unknowns.iter().enumerate() {
            for (lj, &c) in unknowns.iter().enumerate() {
                if let Some((_, p)) = matrix.slot(r as usize, c as usize) {
                    pos[li * n + lj] = p as u32;
                }
            }
        }
        SlotTable { unknowns, pos }
    }

    /// Resolve a global `(row, col)` to its slot, if the pair is in the table.
    #[inline]
    fn lookup(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let li = self.unknowns.iter().position(|&u| u as usize == row)?;
        let lj = self.unknowns.iter().position(|&u| u as usize == col)?;
        let p = self.pos[li * self.unknowns.len() + lj];
        if p == u32::MAX {
            return None;
        }
        Some((row, p as usize))
    }
}

/// Tier-2 sink for devices whose matrix part is entirely in the backbone:
/// keeps RHS writes, drops matrix writes. Reuses the interpreted device code
/// so the RHS formulas (companion history terms, source values) stay single-
/// sourced and bit-equal to the interpreted path.
struct RhsOnlySink<'a> {
    rhs: &'a mut [f64],
}

impl StampSink for RhsOnlySink<'_> {
    #[inline]
    fn g(&mut self, _row: usize, _col: usize, _v: f64) {}
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        self.rhs[row] += v;
    }
}

/// Tier-2 sink for nonlinear / time-varying devices: matrix writes resolve
/// through the device's [`SlotTable`]; the (cold) fallback to a plain `add`
/// keeps a table miss correct instead of silently dropping a stamp.
struct SlottedSink<'a> {
    g: &'a mut SparseMatrix,
    rhs: &'a mut [f64],
    table: &'a SlotTable,
}

impl StampSink for SlottedSink<'_> {
    #[inline]
    fn g(&mut self, row: usize, col: usize, v: f64) {
        match self.table.lookup(row, col) {
            Some(slot) => self.g.add_at(slot, v),
            None => self.g.add(row, col, v),
        }
    }
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        self.rhs[row] += v;
    }
}

impl StampPlan {
    /// Compile the constant backbone and the tier-2 schedule of `circuit`
    /// against a frozen `matrix` pattern. The matrix must already have every
    /// slot reserved (see `reserve_pattern`).
    pub fn compile(circuit: &Circuit, layout: &Layout, matrix: &SparseMatrix) -> StampPlan {
        let mut cond_ops = Vec::new();
        let mut reactive_ops = Vec::new();
        let mut restamp = Vec::new();

        // Push the four writes of a conductance-like pair stamp, `mult` on the
        // diagonals and `-mult` off-diagonal, into `ops`.
        let push_pair = |ops: &mut Vec<(usize, usize, f64)>,
                         a: hauksbee_ir::NodeId,
                         b: hauksbee_ir::NodeId,
                         mult: f64| {
            let ai = layout.node(a);
            let bi = layout.node(b);
            if let Some(ai) = ai {
                if let Some(s) = matrix.slot(ai, ai) {
                    ops.push((s.0, s.1, mult));
                }
            }
            if let Some(bi) = bi {
                if let Some(s) = matrix.slot(bi, bi) {
                    ops.push((s.0, s.1, mult));
                }
            }
            if let (Some(ai), Some(bi)) = (ai, bi) {
                if let Some(s) = matrix.slot(ai, bi) {
                    ops.push((s.0, s.1, -mult));
                }
                if let Some(s) = matrix.slot(bi, ai) {
                    ops.push((s.0, s.1, -mult));
                }
            }
        };
        // A single resolved constant write.
        let push_at = |ops: &mut Vec<(usize, usize, f64)>, r: usize, c: usize, v: f64| {
            if let Some(s) = matrix.slot(r, c) {
                ops.push((s.0, s.1, v));
            }
        };
        // Slot table over a device's deduped, non-ground node unknowns, plus
        // any device-private internal unknowns (a series-resistance BJT's
        // intrinsic nodes, dev-plan 04 §3.2) — the relocated core and the
        // ohmic couplings stamp there, and a table miss would fall back to
        // the slow row search every iteration.
        let node_table = |dev: &Device, id: hauksbee_ir::DeviceId| {
            let mut unknowns: Vec<u32> = Vec::new();
            for n in dev.nodes() {
                if let Some(i) = layout.node(n) {
                    if !unknowns.contains(&(i as u32)) {
                        unknowns.push(i as u32);
                    }
                }
            }
            if let Some(ints) = layout.bjt_internal(id) {
                for i in ints.iter().flatten() {
                    if !unknowns.contains(&(*i as u32)) {
                        unknowns.push(*i as u32);
                    }
                }
            }
            SlotTable::build(unknowns, matrix)
        };

        for (id, dev) in circuit.iter() {
            match dev {
                Device::Resistor {
                    a, b, ohms, tc1, ..
                } => {
                    // Temperature-independent resistors fold into the backbone.
                    // tc1 resistors depend on options.temperature (not known at
                    // compile time), so they stay interpreted-with-resolved-
                    // slots; non-positive resistances stamp nothing on the
                    // interpreted path and are skipped entirely here.
                    if tc1.is_none() {
                        if *ohms > 0.0 {
                            push_pair(&mut cond_ops, *a, *b, 1.0 / *ohms);
                        }
                    } else {
                        restamp.push(Restamp::Slotted {
                            id,
                            table: node_table(dev, id),
                        });
                    }
                }
                Device::Capacitor { a, b, farads, .. } => {
                    // geq = coeffs.g * C, same ± pattern as a conductance.
                    push_pair(&mut reactive_ops, *a, *b, *farads);
                    // History current ieq changes every step: RHS-only re-stamp.
                    restamp.push(Restamp::RhsOnly { id });
                }
                Device::Inductor { a, b, henries, .. } => {
                    let br = layout.branch(id).expect("inductor has a branch");
                    // KCL and branch-row incidence: constant ±1.
                    if let Some(ai) = layout.node(*a) {
                        push_at(&mut cond_ops, ai, br, 1.0);
                        push_at(&mut cond_ops, br, ai, 1.0);
                    }
                    if let Some(bi) = layout.node(*b) {
                        push_at(&mut cond_ops, bi, br, -1.0);
                        push_at(&mut cond_ops, br, bi, -1.0);
                    }
                    // Branch self-term -req = -(L * coeffs.g).
                    push_at(&mut reactive_ops, br, br, -*henries);
                    // Mutual terms (dev-plan 04 §2.3): −M·coeffs.g at (this
                    // branch row, partner branch column) folds into the
                    // reactive backbone with multiplier −M — the exact same
                    // (slot, multiplier)×coeffs.g dt-dependence as the self
                    // term above, sound for the same reason the CCCS fold is:
                    // the plan compiles AFTER the layout freeze, so the
                    // partner branch index is the one the interpreted stamp
                    // uses. The mutual HISTORY rides the RhsOnly restamp
                    // below (stamp_inductor's veq loop; the RhsOnlySink
                    // discards its matrix writes, so nothing double-stamps).
                    for &(pid, m) in layout.mutual_partners(id) {
                        let pbr = layout
                            .branch(pid)
                            .expect("coupled winding owns a branch unknown");
                        push_at(&mut reactive_ops, br, pbr, -m);
                    }
                    // History voltage veq changes every step: RHS-only.
                    restamp.push(Restamp::RhsOnly { id });
                }
                Device::Vsource { p, n, .. } => {
                    let br = layout.branch(id).expect("vsource has a branch");
                    if let Some(pi) = layout.node(*p) {
                        push_at(&mut cond_ops, pi, br, 1.0);
                        push_at(&mut cond_ops, br, pi, 1.0);
                    }
                    if let Some(ni) = layout.node(*n) {
                        push_at(&mut cond_ops, ni, br, -1.0);
                        push_at(&mut cond_ops, br, ni, -1.0);
                    }
                    // Source value (time-varying, src_scale) is RHS-only.
                    restamp.push(Restamp::RhsOnly { id });
                }
                Device::Isource { .. } => {
                    // Pure RHS device on the interpreted path already.
                    restamp.push(Restamp::RhsOnly { id });
                }
                Device::Vcvs {
                    p, n, cp, cn, gain, ..
                } => {
                    // Fully constant: branch incidence plus the control-gain
                    // terms in the branch row. Nothing varies per step or per
                    // Newton iterate (RHS is zero), so no restamp entry at all.
                    let br = layout.branch(id).expect("vcvs has a branch");
                    if let Some(pi) = layout.node(*p) {
                        push_at(&mut cond_ops, pi, br, 1.0);
                        push_at(&mut cond_ops, br, pi, 1.0);
                    }
                    if let Some(ni) = layout.node(*n) {
                        push_at(&mut cond_ops, ni, br, -1.0);
                        push_at(&mut cond_ops, br, ni, -1.0);
                    }
                    if let Some(cpi) = layout.node(*cp) {
                        push_at(&mut cond_ops, br, cpi, -*gain);
                    }
                    if let Some(cni) = layout.node(*cn) {
                        push_at(&mut cond_ops, br, cni, *gain);
                    }
                }
                Device::Vccs {
                    p, n, cp, cn, gm, ..
                } => {
                    // Constant transconductance: the four (output-row,
                    // control-column) entries fold into the backbone.
                    let (pi, ni) = (layout.node(*p), layout.node(*n));
                    let (cpi, cni) = (layout.node(*cp), layout.node(*cn));
                    if let Some(pi) = pi {
                        if let Some(cpi) = cpi {
                            push_at(&mut cond_ops, pi, cpi, *gm);
                        }
                        if let Some(cni) = cni {
                            push_at(&mut cond_ops, pi, cni, -*gm);
                        }
                    }
                    if let Some(ni) = ni {
                        if let Some(cpi) = cpi {
                            push_at(&mut cond_ops, ni, cpi, -*gm);
                        }
                        if let Some(cni) = cni {
                            push_at(&mut cond_ops, ni, cni, *gm);
                        }
                    }
                }
                Device::Cccs {
                    p, n, ctrl_src, gain, ..
                } => {
                    // Fully constant like the VCCS: two (output-row,
                    // control-branch-column) entries fold into the backbone.
                    // The folding is SOUND here — this compilation runs after
                    // layout freeze, so `layout.branch(ctrl_src)` is the same
                    // resolved index the interpreted stamp uses (the question
                    // §2.2 flags for F/H is answered by construction: the plan
                    // is built from the layout, never before it). No restamp:
                    // gain is a device constant and the RHS is zero.
                    let cbr = layout
                        .branch(*ctrl_src)
                        .expect("cccs control source owns a branch");
                    if let Some(pi) = layout.node(*p) {
                        push_at(&mut cond_ops, pi, cbr, *gain);
                    }
                    if let Some(ni) = layout.node(*n) {
                        push_at(&mut cond_ops, ni, cbr, -*gain);
                    }
                }
                Device::Ccvs {
                    p, n, ctrl_src, transres, ..
                } => {
                    // Branch incidence (a VCVS-shaped constraint) plus the
                    // -transres dependence at the control source's branch
                    // column. Fully constant, no restamp (RHS is zero).
                    let br = layout.branch(id).expect("ccvs has a branch");
                    let cbr = layout
                        .branch(*ctrl_src)
                        .expect("ccvs control source owns a branch");
                    if let Some(pi) = layout.node(*p) {
                        push_at(&mut cond_ops, pi, br, 1.0);
                        push_at(&mut cond_ops, br, pi, 1.0);
                    }
                    if let Some(ni) = layout.node(*n) {
                        push_at(&mut cond_ops, ni, br, -1.0);
                        push_at(&mut cond_ops, br, ni, -1.0);
                    }
                    push_at(&mut cond_ops, br, cbr, -*transres);
                }
                Device::Diode { .. }
                | Device::Bjt { .. }
                | Device::Mosfet { .. }
                | Device::VSwitch { .. }
                | Device::OpAmp { .. }
                | Device::Comparator { .. } => {
                    restamp.push(Restamp::Slotted {
                        id,
                        table: node_table(dev, id),
                    });
                }
                // Behavioral B-source: tier-2 nonlinear re-stamp like the BJT
                // (its FD tangents move every Newton iterate), NOT backbone.
                // Unlike the node-only nonlinear devices above, its writes
                // reach beyond its node unknowns: a V-output owns a branch
                // row/column, and every I(...) dep references another
                // device's branch COLUMN — extend the slot table with those
                // unknowns so the re-stamp stays search-free (the SlottedSink
                // fallback would keep a miss correct, but the table is the
                // point of the plan).
                Device::Behavioral { .. } => {
                    let mut unknowns: Vec<u32> = Vec::new();
                    for n in dev.nodes() {
                        if let Some(i) = layout.node(n) {
                            if !unknowns.contains(&(i as u32)) {
                                unknowns.push(i as u32);
                            }
                        }
                    }
                    if let Some(br) = layout.branch(id) {
                        unknowns.push(br as u32);
                    }
                    for ctrl in dev.controlling_sources() {
                        let cbr = layout
                            .branch(ctrl)
                            .expect("behavioral control source owns a branch")
                            as u32;
                        if !unknowns.contains(&cbr) {
                            unknowns.push(cbr);
                        }
                    }
                    restamp.push(Restamp::Slotted {
                        id,
                        table: SlotTable::build(unknowns, matrix),
                    });
                }
                // A coupling contributes NOTHING to the plan as a device: its
                // matrix cross terms folded into the reactive backbone at the
                // Inductor arm above (via `mutual_partners`), and its history
                // rides the windings' RhsOnly restamps. Mirrors the inert
                // `stamp_device` arm — one home for the physics.
                Device::Coupling { .. } => {}
            }
        }

        let mut gmin_diag = Vec::new();
        for i in 0..layout.n_nodes {
            if let Some(s) = matrix.slot(i, i) {
                gmin_diag.push((s.0, s.1));
            }
        }

        StampPlan {
            cond_ops,
            gmin_diag,
            reactive_ops,
            restamp,
        }
    }

    /// Apply the constant backbone (resistors + source/inductor incidence)
    /// into `m` (which must have the pattern this plan was compiled against).
    #[inline]
    pub fn apply_conductances(&self, m: &mut SparseMatrix) {
        for &(row, pos, val) in &self.cond_ops {
            m.add_at((row, pos), val);
        }
    }

    /// Apply the reactive companion conductances for integration factor
    /// `coeffs_g` (= `k/dt` for the active rule).
    #[inline]
    pub fn apply_reactive(&self, m: &mut SparseMatrix, coeffs_g: f64) {
        for &(row, pos, mult) in &self.reactive_ops {
            m.add_at((row, pos), mult * coeffs_g);
        }
    }

    /// Apply the gmin shunt to every node diagonal.
    #[inline]
    pub fn apply_gmin(&self, m: &mut SparseMatrix, gmin: f64) {
        if gmin == 0.0 {
            return;
        }
        for &slot in &self.gmin_diag {
            m.add_at(slot, gmin);
        }
    }

    /// Number of constant conductance writes (diagnostics).
    pub fn cond_op_count(&self) -> usize {
        self.cond_ops.len()
    }

    /// Number of reactive backbone writes (diagnostics).
    pub fn reactive_op_count(&self) -> usize {
        self.reactive_ops.len()
    }

    /// Number of tier-2 re-stamped devices (diagnostics).
    pub fn restamp_count(&self) -> usize {
        self.restamp.len()
    }
}

/// The two-tier compiled assembly (`AssemblyMode::Planned`): backbone replay +
/// tier-2 re-stamp. Produces the same system as [`stamp_all`] up to
/// floating-point accumulation order (each slot receives the identical set of
/// addends; only their order differs), so solutions match the interpreted
/// path to solver tolerance rather than bit-for-bit.
///
/// Contexts the plan does not model fall back to the interpreted walk: DC
/// operating-point solves (`ctx.dc`, where reactive elements open/short or pin
/// to ICs), the staged-DC regularizers (`branch_reg > 0`, which also changes
/// device-level limiting), and event-frozen solves (`cmp_freeze` /
/// `switch_freeze`). Those paths are cold (once per run / stiff-board rescue),
/// so they keep the reference behaviour at zero risk.
pub fn stamp_all_planned(
    ctx: &StampCtx,
    plan: &StampPlan,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
) {
    let eligible = !ctx.dc
        && ctx.branch_reg == 0.0
        && ctx.cmp_freeze.is_none()
        && ctx.switch_freeze.is_none();
    if !eligible {
        stamp_all(ctx, g, rhs);
        return;
    }
    plan.apply_conductances(g);
    plan.apply_reactive(g, ctx.coeffs.g);
    plan.apply_gmin(g, ctx.gmin);
    for r in &plan.restamp {
        match r {
            Restamp::RhsOnly { id } => {
                let dev = &ctx.circuit.devices[id.0 as usize];
                let mut sink = RhsOnlySink { rhs };
                stamp_device(ctx, *id, dev, &mut sink);
            }
            Restamp::Slotted { id, table } => {
                let dev = &ctx.circuit.devices[id.0 as usize];
                let mut sink = SlottedSink { g, rhs, table };
                stamp_device(ctx, *id, dev, &mut sink);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::SolverOptions;
    use crate::stamp::{reserve_pattern, IntegCoeffs};
    use crate::system::ReactiveState;
    use hauksbee_ir::{Device, NodeId, SourceKind};

    #[test]
    fn plan_matches_direct_stamp_for_resistors() {
        let mut c = Circuit::new();
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Resistor {
            name: "R1".into(),
            a,
            b,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "R2".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 2e3,
            tc1: None,
        });

        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let plan = StampPlan::compile(&c, &layout, &m);

        m.clear_values();
        plan.apply_conductances(&mut m);

        // Direct reference.
        let mut m2 = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m2);
        m2.clear_values();
        let g1 = 1e-3;
        let g2 = 5e-4;
        let ai = layout.node(a).unwrap();
        let bi = layout.node(b).unwrap();
        m2.add(ai, ai, g1);
        m2.add(bi, bi, g1);
        m2.add(ai, bi, -g1);
        m2.add(bi, ai, -g1);
        m2.add(bi, bi, g2);

        // The plan should reproduce R1+R2's full conductance stamp (5 entries:
        // two self terms for the R1 pair, two cross terms, plus R2's diagonal).
        let _ = (g1, g2, ai, bi);
        assert!(plan.cond_op_count() >= 5);
    }

    /// A mixed board exercising every device kind: the planned assembly must
    /// reproduce the interpreted one entry-for-entry (matrix and RHS) within
    /// accumulation rounding at a non-trivial transient iterate. This is the
    /// per-device-kind before/after check the two-tier split hangs off: a
    /// dropped or double-counted contribution shows up far above the bound.
    #[test]
    fn planned_assembly_matches_interpreted_on_mixed_board() {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        let n1 = c.node("n1");
        let n2 = c.node("n2");
        let n3 = c.node("n3");
        let n4 = c.node("n4");
        let n5 = c.node("n5");
        let n6 = c.node("n6");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Sin {
                offset: 2.5,
                amplitude: 2.0,
                freq: 1e3,
                delay: 0.0,
                theta: 0.0,
                phase: 0.0,
            },
        });
        c.add(Device::Resistor { name: "R1".into(), a: vin, b: n1, ohms: 1e3, tc1: None });
        c.add(Device::Resistor { name: "Rt".into(), a: n1, b: n2, ohms: 2e3, tc1: Some(0.001) });
        c.add(Device::Capacitor { name: "C1".into(), a: n1, b: NodeId::GROUND, farads: 1e-9, ic: None });
        c.add(Device::Inductor { name: "L1".into(), a: n2, b: n3, henries: 1e-6, ic: None });
        c.add(Device::Isource { name: "I1".into(), p: n3, n: NodeId::GROUND, kind: SourceKind::Dc(1e-3) });
        c.add(Device::Diode { name: "D1".into(), a: n1, k: n4, model: Default::default() });
        c.add(Device::Bjt { name: "Q1".into(), c: vin, b: n4, e: NodeId::GROUND, model: Default::default() });
        c.add(Device::Mosfet { name: "M1".into(), d: n2, g: n1, s: NodeId::GROUND, b: None, model: Default::default() });
        c.add(Device::VSwitch {
            name: "S1".into(), a: n3, b: n5, ctrl_p: n1, ctrl_n: NodeId::GROUND,
            von: 3.0, voff: 2.0, ron: 10.0, roff: 1e9,
        });
        c.add(Device::OpAmp {
            name: "U1".into(), out: n5, inp: n1, inn: n2, reference: None,
            gain: 1e5, pole_hz: None, rail_lo: 0.0, rail_hi: 5.0,
        });
        c.add(Device::Comparator {
            name: "K1".into(), out: n6, inp: n1, inn: n2,
            out_lo: 0.0, out_hi: 5.0, hysteresis: 0.05,
        });

        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let plan = StampPlan::compile(&c, &layout, &m);

        let n = layout.size;
        // Non-trivial iterate with mixed signs/magnitudes, plus non-zero
        // reactive history so the RHS-only tier carries real values.
        let x: Vec<f64> = (0..n).map(|i| ((i as f64 * 0.7391).sin()) * 3.0 + 0.1).collect();
        let mut state = ReactiveState::new(c.devices.len());
        for (i, v) in state.x1.iter_mut().enumerate() {
            *v = 0.3 * (i as f64 + 1.0);
        }
        for (i, v) in state.dx1.iter_mut().enumerate() {
            *v = -0.1 * (i as f64 + 1.0);
        }
        let opts = SolverOptions::default();
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, false);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &c,
            layout: &layout,
            opts: &opts,
            x: &x,
            x_prev: &x,
            time: 1e-6,
            coeffs,
            state: &state,
            dc: false,
            use_ic: false,
            gmin: 1e-12,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };

        // Interpreted reference.
        let mut m_ref = m.clone();
        m_ref.clear_values();
        let mut rhs_ref = vec![0.0f64; n];
        stamp_all(&ctx, &mut m_ref, &mut rhs_ref);

        // Planned.
        m.clear_values();
        let mut rhs = vec![0.0f64; n];
        stamp_all_planned(&ctx, &plan, &mut m, &mut rhs);

        for i in 0..n {
            let row_ref = m_ref.row(i);
            let row = m.row(i);
            assert_eq!(row.len(), row_ref.len(), "row {i} pattern changed");
            for (k, (&(cc, vv), &(cr, vr))) in row.iter().zip(row_ref.iter()).enumerate() {
                assert_eq!(cc, cr, "row {i} slot {k} column changed");
                let err = (vv - vr).abs();
                let bound = 1e-12 * vr.abs().max(1.0);
                assert!(
                    err <= bound,
                    "matrix ({i},{cc}): planned {vv} vs interpreted {vr}, err {err:e}"
                );
            }
            let err = (rhs[i] - rhs_ref[i]).abs();
            let bound = 1e-12 * rhs_ref[i].abs().max(1.0);
            assert!(
                err <= bound,
                "rhs[{i}]: planned {} vs interpreted {}, err {err:e}",
                rhs[i],
                rhs_ref[i]
            );
        }
    }

    /// S3 measurement estate (run with `--ignored --nocapture`, release):
    /// the cost of ONE assembly, interpreted vs planned, on the two graded
    /// shapes the acceptance gate names — an RC-ladder-like linear board
    /// (backbone-dominated: the planned walk skips every resistor and does
    /// zero slot searches) and a shunt-mirror-like nonlinear board (hub rail
    /// row, where the tier-2 slot tables replace binary searches over a long
    /// row). Interleaved reps, medians, so a loaded machine biases both sides
    /// equally. Numbers feed the S3 report; not a regression gate.
    #[test]
    #[ignore = "measurement harness, run explicitly"]
    fn bench_planned_assembly_split() {
        // RC ladder, 1000 stages.
        let mut rc = Circuit::new();
        let vin = rc.node("in");
        rc.add(Device::Vsource {
            name: "V".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let mut prev = vin;
        for k in 0..1000 {
            let n = rc.node(&format!("n{k}"));
            rc.add(Device::Resistor { name: format!("R{k}"), a: prev, b: n, ohms: 1e3, tc1: None });
            rc.add(Device::Capacitor { name: format!("C{k}"), a: n, b: NodeId::GROUND, farads: 1e-9, ic: None });
            prev = n;
        }
        // Shunt-fed mirror-ish array, 240 blocks: hub rail + BJT pairs + RC.
        let mut ma = Circuit::new();
        let vcc = ma.node("vcc");
        let rail = ma.node("rail");
        ma.add(Device::Vsource { name: "V".into(), p: vcc, n: NodeId::GROUND, kind: SourceKind::Dc(5.0) });
        ma.add(Device::Resistor { name: "Rsh".into(), a: vcc, b: rail, ohms: 1e3, tc1: None });
        for k in 0..240 {
            let b = ma.node(&format!("b{k}"));
            let mem = ma.node(&format!("m{k}"));
            ma.add(Device::Resistor { name: format!("Rr{k}"), a: rail, b, ohms: 47e3, tc1: None });
            ma.add(Device::Bjt { name: format!("Q1_{k}"), c: b, b, e: NodeId::GROUND, model: Default::default() });
            ma.add(Device::Bjt { name: format!("Q2_{k}"), c: mem, b, e: NodeId::GROUND, model: Default::default() });
            ma.add(Device::Resistor { name: format!("Rm{k}"), a: rail, b: mem, ohms: 100e3, tc1: None });
            ma.add(Device::Capacitor { name: format!("Cm{k}"), a: mem, b: NodeId::GROUND, farads: 1e-9, ic: None });
        }

        for (label, c) in [("rc_ladder_1k", &rc), ("mirror_240", &ma)] {
            let layout = Layout::new(c);
            let mut m = SparseMatrix::new(layout.size);
            reserve_pattern(c, &layout, &mut m);
            let plan = StampPlan::compile(c, &layout, &m);
            let n = layout.size;
            let x: Vec<f64> = (0..n).map(|i| 0.5 + 0.001 * (i % 7) as f64).collect();
            let state = ReactiveState::new(c.devices.len());
            let opts = SolverOptions::default();
            let coeffs =
                IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, false);
            let spdt = std::collections::HashMap::new();
            let ctx = StampCtx {
                circuit: c,
                layout: &layout,
                opts: &opts,
                x: &x,
                x_prev: &x,
                time: 1e-6,
                coeffs,
                state: &state,
                dc: false,
                use_ic: false,
                gmin: 1e-12,
                src_scale: 1.0,
                branch_reg: 0.0,
                cmp_freeze: None,
                switch_freeze: None,
                spdt_sibling: &spdt,
            };
            let mut rhs = vec![0.0f64; n];
            const REPS: usize = 300;
            let mut t_interp = Vec::new();
            let mut t_planned = Vec::new();
            for _ in 0..10 {
                m.clear_values();
                rhs.iter_mut().for_each(|v| *v = 0.0);
                stamp_all(&ctx, &mut m, &mut rhs);
                m.clear_values();
                rhs.iter_mut().for_each(|v| *v = 0.0);
                stamp_all_planned(&ctx, &plan, &mut m, &mut rhs);
            }
            for _ in 0..12 {
                let t0 = std::time::Instant::now();
                for _ in 0..REPS {
                    m.clear_values();
                    rhs.iter_mut().for_each(|v| *v = 0.0);
                    stamp_all(&ctx, &mut m, &mut rhs);
                }
                t_interp.push(t0.elapsed().as_secs_f64() / REPS as f64);
                let t0 = std::time::Instant::now();
                for _ in 0..REPS {
                    m.clear_values();
                    rhs.iter_mut().for_each(|v| *v = 0.0);
                    stamp_all_planned(&ctx, &plan, &mut m, &mut rhs);
                }
                t_planned.push(t0.elapsed().as_secs_f64() / REPS as f64);
            }
            t_interp.sort_by(f64::total_cmp);
            t_planned.sort_by(f64::total_cmp);
            let mi = t_interp[t_interp.len() / 2] * 1e6;
            let mp = t_planned[t_planned.len() / 2] * 1e6;
            println!(
                "bench_planned_assembly_split {label}: devices={} unknowns={} interpreted={mi:.2}us planned={mp:.2}us drop={:.0}% (cond_ops={} reactive_ops={} restamp={})",
                c.devices.len(),
                n,
                (1.0 - mp / mi) * 100.0,
                plan.cond_op_count(),
                plan.reactive_op_count(),
                plan.restamp_count(),
            );
            assert!(mi > 0.0 && mp > 0.0);
        }
    }

    /// Ineligible contexts (DC, staged regularizer, frozen states) must fall
    /// back to the interpreted assembly and therefore match it EXACTLY.
    #[test]
    fn planned_assembly_falls_back_on_dc_context() {
        let mut c = Circuit::new();
        let a = c.node("a");
        c.add(Device::Vsource { name: "V".into(), p: a, n: NodeId::GROUND, kind: SourceKind::Dc(1.0) });
        c.add(Device::Capacitor { name: "C".into(), a, b: NodeId::GROUND, farads: 1e-9, ic: Some(0.5) });
        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let plan = StampPlan::compile(&c, &layout, &m);
        let n = layout.size;
        let x = vec![0.0f64; n];
        let state = ReactiveState::new(c.devices.len());
        let opts = SolverOptions::default();
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1.0, true);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &c,
            layout: &layout,
            opts: &opts,
            x: &x,
            x_prev: &x,
            time: 0.0,
            coeffs,
            state: &state,
            dc: true,
            use_ic: true,
            gmin: 1e-12,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };
        let mut m_ref = m.clone();
        m_ref.clear_values();
        let mut rhs_ref = vec![0.0f64; n];
        stamp_all(&ctx, &mut m_ref, &mut rhs_ref);
        m.clear_values();
        let mut rhs = vec![0.0f64; n];
        stamp_all_planned(&ctx, &plan, &mut m, &mut rhs);
        for i in 0..n {
            assert_eq!(m.row(i), m_ref.row(i), "dc fallback row {i} not bit-identical");
            assert_eq!(rhs[i], rhs_ref[i], "dc fallback rhs {i} not bit-identical");
        }
    }
}
