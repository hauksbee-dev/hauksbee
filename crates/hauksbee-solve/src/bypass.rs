//! The classic SPICE device-evaluation bypass (dev-plan 03 §6): skip
//! re-evaluating a nonlinear device whose inputs did not move, replaying its
//! previously recorded stamp instead of recomputing the `exp()`-heavy model.
//!
//! # What is cacheable, and for how long
//!
//! A nonlinear device's stamped contribution at iterate `x` is a function of
//! three groups of inputs, with three different lifetimes:
//!
//! * **Per iteration**: the unknowns the device reads (`ctx.x` at its
//!   terminals, plus a series-R BJT's internal unknowns) and the pn-limiting
//!   anchor (`ctx.x_prev`). These are the tangent + equivalent current; the
//!   thing bypass exists to reuse.
//! * **Per step**: the integration factor `coeffs.g`, the charge-companion
//!   history (`state.x1/dx1/x2`; the diode/BJT/MOS charge companions landed
//!   in dev-plan 04 carry per-step history in their RHS terms), the source
//!   time, and `gmin`. Constant across one step's whole Newton iteration
//!   sequence, DIFFERENT on the next step.
//! * **Per run**: the model parameters, temperature, effects toggles.
//!
//! Bypass operates WITHIN one step's Newton iteration sequence, so the cache
//! may hold everything in the first two groups **as long as it never survives
//! a `newton_solve` call**: a `generation` counter bumps at solve start and a
//! device's record is only replayable inside the generation that recorded it.
//! That single rule covers every cross-step hazard at once, dt changes, LTE
//! retries at a different h, event retries, companion-history advance,
//! because each of those is a fresh `newton_solve` call. (The first-two-
//! iterations rule below re-evaluates everything at the start of every solve
//! anyway; the generation is the structural guarantee, not the only line of
//! defense.)
//!
//! # The movement test
//!
//! Before evaluating device `d` on iteration ≥ 3, compare every unknown in
//! its READ set against the values at its last recorded evaluation: if all of
//! them satisfy `|v − v_last| ≤ 0.1·(reltol·max(|v|,|v_last|) + vntol)` (the
//! plan's tightened SPICE `bypasstol`), replay the record; else evaluate
//! fresh and re-record. The comparison is written NaN-safe (`!(Δ ≤ tol)`
//! counts as moved), so a poisoned iterate always re-evaluates and hits the
//! stamps' own non-finite guards.
//!
//! The READ set is the device's node unknowns (`Device::nodes()` through the
//! layout, for a MOSFET that includes the optional bulk; for a VSwitch its
//! control pair, though switches are excluded below) plus a series-R BJT's
//! device-private internal unknowns (dev-plan 04 §3.2): the intrinsic
//! junction voltages live there, so "the internal unknowns' movement counts
//! as terminal movement". Ground is not an unknown and never moves.
//!
//! # SPICE's safety discipline (all enforced by the caller + this module)
//!
//! * never bypass on the first two iterations of a solve (`force_eval`);
//! * never on DC solves, event-frozen solves (`cmp_freeze`/`switch_freeze`),
//!   or the trials immediately after an event-resolved accept (the transient
//!   driver holds bypass there, mirroring its extrapolation-seed skip);
//! * the Armijo line-search residual evaluations keep the full `stamp_all`:
//!   the census arc (7A) proved those norms sit on the cancellation noise
//!   floor, so the residual the line search compares must stay order-exact,
//!   bypass never touches `residual_inf_norm_at`;
//! * the accepted step must match the no-bypass reference to reltol (§6.2's
//!   gate), bypass may change the iterate PATH, never the answer.
//!
//! # Exclusion list (refuse-rather-than-fake)
//!
//! Bypassed: **Diode, BJT, MOSFET**: the `exp()`-heavy junction devices,
//! exactly SPICE's classic set, and the devices whose evaluation dominates a
//! quiescent board's assembly.
//!
//! Excluded, each with its reason:
//!
//! * **Behavioral (B-source)**: its FD Jacobian probes the expression at
//!   perturbed dependency values every evaluation, and its fault channel
//!   (`take_behavioral_fault`) must see every iterate, a cached stamp would
//!   silently skip the very evaluation that detects `ln(-2)` at a new point.
//! * **Comparator**: bang-bang output with hysteresis read from the CURRENT
//!   iterate's output voltage; the discrete decision is the event the march
//!   bisects on, exactly the "disable bypass near breakpoints" case; also
//!   nearly free to evaluate (no exp).
//! * **VSwitch**: the event-flip device of the flagship board (the
//!   event-freeze machinery exists for it), and under break-before-make its
//!   stamp reads its SIBLING leg's control nodes, inputs outside its own
//!   terminal set. Cheap tanh, dangerous semantics: excluded.
//! * **OpAmp**: rail-clamp discontinuity decides its stamp shape; evaluation
//!   is a handful of multiplies. Nothing to win.
//! * Linear devices (R, C, L, sources, E/F/G/H, coupling): their matrix parts
//!   are constant or already backbone-compiled (plan.rs); their RHS history
//!   terms change per step, which the cache cannot outlive anyway. No exp to
//!   skip, bypassing them buys nothing and risks the reactive history.
//!
//! # Replay fidelity
//!
//! A fresh evaluation stamps through [`RecordingSink`], which resolves each
//! write to its frozen-pattern slot and applies it with `add_at`; the same
//! `+=` on the same slot `SparseMatrix::add` performs, so a bypass-armed
//! assembly in which nothing qualifies for skipping is bit-identical to the
//! interpreted walk. A replay re-adds the recorded raw writes in the original
//! order, so a bypassed device contributes bit-identically to what
//! re-evaluating it at its LAST-evaluated point would have stamped.

use hauksbee_ir::{Circuit, Device, DeviceId};

use crate::sparse::SparseMatrix;
use crate::stamp::{stamp_device, StampCtx, StampSink};
use crate::system::Layout;

/// One bypassable device's cache: its read set, the input values at the last
/// fresh evaluation, and the recorded stamp.
struct DevCache {
    id: DeviceId,
    /// Unknown indices this device's stamp reads (node unknowns + BJT
    /// internal unknowns). The movement test runs over exactly this set.
    read_idx: Vec<u32>,
    /// `ctx.x` values at `read_idx` when the record below was made.
    last_v: Vec<f64>,
    /// Generation the record belongs to; replayable only while it equals the
    /// state's current generation (i.e. within one `newton_solve` call).
    gen: u64,
    /// Recorded matrix writes `(row, position-in-row, value)`, raw and in
    /// stamp order (see the module doc on replay fidelity).
    mat: Vec<(u32, u32, f64)>,
    /// Recorded RHS writes `(row, value)`.
    rhs: Vec<(u32, f64)>,
}

/// Workspace-owned bypass state: one cache per bypassable device, an id→cache
/// index, the solve generation, and the observability counters.
pub(crate) struct BypassState {
    caches: Vec<DevCache>,
    /// `device id -> caches index`, `u32::MAX` = not bypassable.
    index: Vec<u32>,
    /// Bumped once per `newton_solve` call (see [`BypassState::begin_solve`]).
    gen: u64,
    /// Model evaluations performed (fresh stamps of bypassable devices).
    pub evals: u64,
    /// Model evaluations skipped (replays).
    pub skips: u64,
}

impl BypassState {
    /// Build the caches for `circuit` against `layout`. Only Diode/BJT/MOSFET
    /// get an entry (the module-doc exclusion list); everything else keeps
    /// the plain interpreted stamp.
    pub(crate) fn build(circuit: &Circuit, layout: &Layout) -> BypassState {
        let mut caches = Vec::new();
        let mut index = vec![u32::MAX; circuit.devices.len()];
        for (id, dev) in circuit.iter() {
            let bypassable = matches!(
                dev,
                Device::Diode { .. } | Device::Bjt { .. } | Device::Mosfet { .. }
            );
            if !bypassable {
                continue;
            }
            let mut read_idx: Vec<u32> = Vec::new();
            for n in dev.nodes() {
                if let Some(i) = layout.node(n) {
                    if !read_idx.contains(&(i as u32)) {
                        read_idx.push(i as u32);
                    }
                }
            }
            // A series-R BJT's intrinsic unknowns: the junction voltages the
            // model actually evaluates live there, so their movement counts
            // as terminal movement (dev-plan 03 §6 brief).
            if let Some(ints) = layout.bjt_internal(id) {
                for i in ints.iter().flatten() {
                    if !read_idx.contains(&(*i as u32)) {
                        read_idx.push(*i as u32);
                    }
                }
            }
            // A series-R MOSFET's intrinsic drain/source unknowns: the channel,
            // body-diode and gate-charge voltages the model evaluates live
            // there, so their movement counts as terminal movement too.
            if let Some(ints) = layout.mos_internal(id) {
                for i in ints.iter().flatten() {
                    if !read_idx.contains(&(*i as u32)) {
                        read_idx.push(*i as u32);
                    }
                }
            }
            index[id.0 as usize] = caches.len() as u32;
            let n_reads = read_idx.len();
            caches.push(DevCache {
                id,
                read_idx,
                last_v: vec![0.0; n_reads],
                gen: 0,
                mat: Vec::new(),
                rhs: Vec::new(),
            });
        }
        BypassState {
            caches,
            index,
            gen: 0,
            evals: 0,
            skips: 0,
        }
    }

    /// Whether any device on this board is bypassable at all (a linear board
    /// or a switch/comparator-only board has nothing to skip).
    pub(crate) fn has_candidates(&self) -> bool {
        !self.caches.is_empty()
    }

    /// Invalidate every record: called at the top of each armed
    /// `newton_solve`, so no cache survives a step / retry / dt change (the
    /// per-step inputs (companion history, coeffs, time) moved).
    pub(crate) fn begin_solve(&mut self) {
        self.gen = self.gen.wrapping_add(1);
    }

    /// (evaluations, skips) since construction; the observability the §6.2
    /// gate wants (skip rate measured, not guessed).
    pub(crate) fn counters(&self) -> (u64, u64) {
        (self.evals, self.skips)
    }
}

/// Sink for a fresh evaluation of a bypassable device: applies each write to
/// the matrix/RHS exactly as `MatrixSink` would (same slot, same `+=`) AND
/// records it for later replay. Slot resolution goes through the frozen
/// pattern (`reserve_pattern` reserved every coordinate a device can touch,
/// so the `expect` is a structural invariant, and it fails loudly rather
/// than silently dropping a stamp).
struct RecordingSink<'a> {
    g: &'a mut SparseMatrix,
    rhs: &'a mut [f64],
    mat_rec: &'a mut Vec<(u32, u32, f64)>,
    rhs_rec: &'a mut Vec<(u32, f64)>,
}

impl StampSink for RecordingSink<'_> {
    #[inline]
    fn g(&mut self, row: usize, col: usize, v: f64) {
        let slot = self
            .g
            .slot(row, col)
            .expect("bypass: device write outside the reserved pattern");
        self.g.add_at(slot, v);
        self.mat_rec.push((slot.0 as u32, slot.1 as u32, v));
    }
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        self.rhs[row] += v;
        self.rhs_rec.push((row as u32, v));
    }
}

/// Plain pass-through sink for the non-bypassable devices (identical writes
/// to `stamp.rs`'s `MatrixSink`, which is private to that module).
struct PlainSink<'a> {
    g: &'a mut SparseMatrix,
    rhs: &'a mut [f64],
}

impl StampSink for PlainSink<'_> {
    #[inline]
    fn g(&mut self, row: usize, col: usize, v: f64) {
        self.g.add(row, col, v);
    }
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        self.rhs[row] += v;
    }
}

/// Bypass-aware full assembly: the interpreted walk of `stamp_all`, with
/// bypassable devices either replayed (unmoved inputs, not `force_eval`) or
/// freshly evaluated-and-recorded. The prologue (gmin shunt, staged branch
/// regularizer) and the device order are the interpreted walk's, verbatim.
///
/// `force_eval` is SPICE's first-two-iterations rule: the caller passes
/// `true` on iterations 1 and 2 of each solve, so every device is evaluated
/// at least twice per step before any skip can happen.
pub(crate) fn stamp_all_bypass(
    ctx: &StampCtx,
    st: &mut BypassState,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
    force_eval: bool,
) {
    let (evals0, skips0) = (st.evals, st.skips);
    // Prologue: identical to `stamp_into` (stamp.rs).
    if ctx.gmin > 0.0 {
        for i in 0..ctx.layout.n_nodes {
            g.add(i, i, ctx.gmin);
        }
    }
    if ctx.branch_reg > 0.0 {
        for i in ctx.layout.n_nodes..ctx.layout.size {
            g.add(i, i, -ctx.branch_reg);
        }
    }
    for (id, dev) in ctx.circuit.iter() {
        let ci = st.index[id.0 as usize];
        if ci == u32::MAX {
            let mut sink = PlainSink { g, rhs };
            stamp_device(ctx, id, dev, &mut sink);
            continue;
        }
        let cache = &mut st.caches[ci as usize];
        debug_assert_eq!(cache.id, id);
        // Movement test against the last-evaluated inputs. NaN-safe: a
        // non-finite iterate never compares "unmoved" (the negated `<=`), so
        // it re-evaluates and hits the stamps' own poisoning guards.
        let replayable = !force_eval && cache.gen == st.gen && {
            let mut unmoved = true;
            for (k, &ui) in cache.read_idx.iter().enumerate() {
                let v = ctx.x[ui as usize];
                let vl = cache.last_v[k];
                let tol = 0.1 * (ctx.opts.reltol * v.abs().max(vl.abs()) + ctx.opts.vntol);
                if !((v - vl).abs() <= tol) {
                    unmoved = false;
                    break;
                }
            }
            unmoved
        };
        if replayable {
            for &(r, p, v) in &cache.mat {
                g.add_at((r as usize, p as usize), v);
            }
            for &(r, v) in &cache.rhs {
                rhs[r as usize] += v;
            }
            st.skips += 1;
        } else {
            cache.mat.clear();
            cache.rhs.clear();
            {
                let mut sink = RecordingSink {
                    g,
                    rhs,
                    mat_rec: &mut cache.mat,
                    rhs_rec: &mut cache.rhs,
                };
                stamp_device(ctx, id, dev, &mut sink);
            }
            for (k, &ui) in cache.read_idx.iter().enumerate() {
                cache.last_v[k] = ctx.x[ui as usize];
            }
            cache.gen = st.gen;
            st.evals += 1;
        }
    }
    crate::census::bypass_assembly(st.evals - evals0, st.skips - skips0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::SolverOptions;
    use crate::stamp::{reserve_pattern, stamp_all, IntegCoeffs};
    use crate::system::ReactiveState;
    use hauksbee_ir::{BjtModel, Circuit, Device, DiodeModel, NodeId, SourceKind};

    /// A mixed nonlinear board: source, resistors, cap, diode (with charge),
    /// BJT, MOSFET, switch, comparator, every stamp class the walk visits.
    fn mixed_board() -> Circuit {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        let n1 = c.node("n1");
        let n2 = c.node("n2");
        let n3 = c.node("n3");
        let n4 = c.node("n4");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.0),
        });
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: n1,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "C1".into(),
            a: n1,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: None,
        });
        c.add(Device::Diode {
            name: "D1".into(),
            a: n1,
            k: n2,
            model: DiodeModel {
                cjo: 4e-12,
                tt: 10e-9,
                ..DiodeModel::default()
            },
        });
        c.add(Device::Resistor {
            name: "R2".into(),
            a: n2,
            b: NodeId::GROUND,
            ohms: 2e3,
            tc1: None,
        });
        c.add(Device::Bjt {
            name: "Q1".into(),
            c: vin,
            b: n2,
            e: n3,
            model: BjtModel::default(),
        });
        c.add(Device::Resistor {
            name: "RE".into(),
            a: n3,
            b: NodeId::GROUND,
            ohms: 100.0,
            tc1: None,
        });
        c.add(Device::Mosfet {
            name: "M1".into(),
            d: n1,
            g: n2,
            s: NodeId::GROUND,
            b: None,
            model: Default::default(),
        });
        c.add(Device::VSwitch {
            name: "S1".into(),
            a: n1,
            b: n4,
            ctrl_p: n2,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Comparator {
            name: "K1".into(),
            out: n4,
            inp: n1,
            inn: n2,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 0.05,
        });
        c
    }

    fn ctx<'a>(
        c: &'a Circuit,
        layout: &'a Layout,
        opts: &'a SolverOptions,
        x: &'a [f64],
        x_prev: &'a [f64],
        state: &'a ReactiveState,
        coeffs: IntegCoeffs,
        spdt: &'a std::collections::HashMap<DeviceId, DeviceId>,
    ) -> StampCtx<'a> {
        StampCtx {
            circuit: c,
            layout,
            opts,
            x,
            x_prev,
            time: 1e-6,
            coeffs,
            state,
            dc: false,
            use_ic: false,
            gmin: 1e-12,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
            switch_freeze: None,
            switch_latch: None,
            spdt_sibling: spdt,
        }
    }

    /// A bypass-armed assembly in which nothing qualifies to skip (fresh
    /// generation / force_eval) must be BIT-IDENTICAL to the interpreted
    /// walk: the RecordingSink's slot-resolved `add_at` is the same `+=` as
    /// `SparseMatrix::add`.
    #[test]
    fn fresh_bypass_assembly_is_bit_identical_to_interpreted() {
        let c = mixed_board();
        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let n = layout.size;
        let x: Vec<f64> = (0..n)
            .map(|i| ((i as f64) * 0.61).sin() * 2.0 + 0.4)
            .collect();
        let mut state = ReactiveState::new(c.devices.len());
        for (i, v) in state.x1.iter_mut().enumerate() {
            *v = 0.2 * (i as f64 + 1.0);
        }
        let opts = SolverOptions::default();
        let coeffs =
            IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, 1e-7, false);
        let spdt = std::collections::HashMap::new();
        let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);

        let mut m_ref = m.clone();
        m_ref.clear_values();
        let mut rhs_ref = vec![0.0f64; n];
        stamp_all(&cx, &mut m_ref, &mut rhs_ref);

        let mut st = BypassState::build(&c, &layout);
        st.begin_solve();
        m.clear_values();
        let mut rhs = vec![0.0f64; n];
        stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs, true);

        for i in 0..n {
            assert_eq!(m.row(i), m_ref.row(i), "row {i} not bit-identical");
            assert_eq!(
                rhs[i].to_bits(),
                rhs_ref[i].to_bits(),
                "rhs {i} not bit-identical"
            );
        }
        assert_eq!(
            st.counters(),
            (3, 0),
            "diode+bjt+mosfet evaluated, none skipped"
        );
    }

    /// With an unmoved iterate on iteration ≥3, the bypassable devices replay
    /// and the assembled system is bit-identical to a fresh stamp at the same
    /// point (the record IS that stamp).
    #[test]
    fn unmoved_iterate_replays_bit_identically() {
        let c = mixed_board();
        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let n = layout.size;
        let x: Vec<f64> = (0..n)
            .map(|i| ((i as f64) * 0.37).cos() * 1.5 + 0.2)
            .collect();
        let state = ReactiveState::new(c.devices.len());
        let opts = SolverOptions::default();
        let coeffs =
            IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, 1e-7, false);
        let spdt = std::collections::HashMap::new();
        let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);

        let mut st = BypassState::build(&c, &layout);
        st.begin_solve();
        // Iteration 1 (forced eval, records).
        m.clear_values();
        let mut rhs1 = vec![0.0f64; n];
        stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs1, true);
        let ref_rows: Vec<Vec<(usize, f64)>> = (0..n).map(|i| m.row(i).to_vec()).collect();
        // Iteration 3 (same x: everything replays).
        m.clear_values();
        let mut rhs3 = vec![0.0f64; n];
        stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs3, false);
        for i in 0..n {
            assert_eq!(m.row(i), &ref_rows[i][..], "replayed row {i} differs");
            assert_eq!(
                rhs3[i].to_bits(),
                rhs1[i].to_bits(),
                "replayed rhs {i} differs"
            );
        }
        let (evals, skips) = st.counters();
        assert_eq!(
            (evals, skips),
            (3, 3),
            "second assembly must replay all three"
        );
    }

    /// A moved terminal re-evaluates exactly the devices that read it; a
    /// fresh generation (new solve) re-evaluates everything.
    #[test]
    fn movement_and_generation_invalidate() {
        let c = mixed_board();
        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let n = layout.size;
        let mut x: Vec<f64> = (0..n).map(|i| 0.3 + 0.05 * i as f64).collect();
        let state = ReactiveState::new(c.devices.len());
        let opts = SolverOptions::default();
        let coeffs =
            IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, 1e-7, false);
        let spdt = std::collections::HashMap::new();

        let mut st = BypassState::build(&c, &layout);
        st.begin_solve();
        {
            let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);
            m.clear_values();
            let mut rhs = vec![0.0f64; n];
            stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs, true);
        }
        // Move n1 (the diode anode / MOSFET drain) well past bypasstol; the
        // BJT reads vin/n2/n3 only and keeps replaying.
        let n1 = (0..c.node_count())
            .find(|&i| c.node_name(hauksbee_ir::NodeId(i as u32)) == "n1")
            .expect("n1 exists");
        let n1_idx = layout
            .node(hauksbee_ir::NodeId(n1 as u32))
            .expect("n1 unknown");
        x[n1_idx] += 0.5;
        {
            let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);
            m.clear_values();
            let mut rhs = vec![0.0f64; n];
            stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs, false);
        }
        let (evals, _skips) = st.counters();
        assert!(
            evals > 3 && evals < 6,
            "moving one node must re-evaluate its readers only (evals={evals})"
        );
        // New solve: generation bump forces everything fresh even unmoved.
        st.begin_solve();
        {
            let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);
            m.clear_values();
            let mut rhs = vec![0.0f64; n];
            stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs, false);
        }
        let (evals2, skips2) = st.counters();
        assert_eq!(
            evals2 - evals,
            3,
            "generation bump must re-evaluate all three"
        );
        let _ = skips2;
    }

    /// End-to-end at the `newton_solve` level: a stiff diode/BJT board whose
    /// cold per-step solve takes >2 iterations must (a) actually skip
    /// evaluations with bypass armed, and (b) converge to the same root as
    /// the no-bypass reference within Newton tolerance.
    #[test]
    fn newton_solve_with_bypass_skips_and_matches() {
        use crate::newton::{newton_solve, Workspace};
        let mut c = Circuit::new();
        let vin = c.node("vin");
        let mid = c.node("mid");
        let out = c.node("out");
        // Sine drive: the DC point (offset 3.5 V) seeds the solve, then the
        // transient-shaped solve lands a quarter period later at 5.5 V, so
        // Newton must walk both junctions up a real swing from a warm seed.
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Sin {
                offset: 3.5,
                amplitude: 2.0,
                freq: 1e5,
                delay: 0.0,
                theta: 0.0,
                phase: 0.0,
            },
        });
        // Independent junction branches off the rail: each is the classic
        // R + junction divider whose cold Newton walks in pnjlim-limited
        // steps (several iterations), and they settle at different rates, so
        // the tail iterations have quiescent devices to skip.
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: mid,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Diode {
            name: "D1".into(),
            a: mid,
            k: NodeId::GROUND,
            model: DiodeModel::default(),
        });
        c.add(Device::Resistor {
            name: "R2".into(),
            a: vin,
            b: out,
            ohms: 4.7e3,
            tc1: None,
        });
        c.add(Device::Bjt {
            name: "Q1".into(),
            c: out,
            b: out,
            e: NodeId::GROUND,
            model: BjtModel::default(),
        });

        let coeffs =
            IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, 1e-7, false);
        let run = |bypass: crate::options::NewtonBypass| {
            let mut opts = SolverOptions::default();
            opts.newton_bypass = bypass;
            // The flagship substrate bypass targets runs with the Armijo
            // line search armed; exercise the same interplay here.
            opts.ladder =
                crate::options::RobustnessLadder::none().with(crate::options::Strategy::LineSearch);
            let mut ws = Workspace::new(&c);
            let state = ReactiveState::new(c.devices.len());
            // DC operating point at the sine's offset (bypass never runs on
            // DC solves), then a transient-shaped solve after the source has
            // moved a realistic per-step amount (~0.5 V): a warm-basin walk
            // that takes several real iterations. (A whole-quarter-period
            // 2 V jump 2-cycles the bare Newton, real marches never move a
            // source that far in one step, dt control forbids it.)
            crate::newton::dc_operating_point(&mut ws, &c, &opts).expect("dc converges");
            let r = newton_solve(
                &mut ws, &c, &opts, 4.3e-7, 1e-7, coeffs, &state, false, false, opts.gmin, 1.0,
            );
            assert!(r.converged, "solve must converge (iters {})", r.iters);
            assert!(
                r.iters > 2,
                "fixture must need >2 iterations, got {}",
                r.iters
            );
            (ws.x.clone(), ws.bypass_counters(), r.iters)
        };
        let (x_ref, (e0, s0), _) = run(crate::options::NewtonBypass::Off);
        assert_eq!((e0, s0), (0, 0), "bypass Off must never build the cache");
        let (x_byp, (evals, skips), _) = run(crate::options::NewtonBypass::On);
        assert!(evals > 0, "bypass On must evaluate");
        assert!(skips > 0, "fixture must actually skip some evaluations");
        let opts = SolverOptions::default();
        for i in 0..x_ref.len() {
            let tol =
                opts.reltol * x_ref[i].abs().max(x_byp[i].abs()) + opts.vntol.max(opts.abstol);
            assert!(
                (x_ref[i] - x_byp[i]).abs() <= tol,
                "unknown {i}: bypass root {} vs reference {} exceeds tolerance",
                x_byp[i],
                x_ref[i]
            );
        }
    }

    /// A NaN iterate must never replay (the negated-`<=` movement test).
    #[test]
    fn nan_iterate_never_bypasses() {
        let c = mixed_board();
        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let n = layout.size;
        let mut x: Vec<f64> = vec![0.4; n];
        let state = ReactiveState::new(c.devices.len());
        let opts = SolverOptions::default();
        let coeffs =
            IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, 1e-7, false);
        let spdt = std::collections::HashMap::new();
        let mut st = BypassState::build(&c, &layout);
        st.begin_solve();
        {
            let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);
            m.clear_values();
            let mut rhs = vec![0.0f64; n];
            stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs, true);
        }
        for v in x.iter_mut() {
            *v = f64::NAN;
        }
        {
            let cx = ctx(&c, &layout, &opts, &x, &x, &state, coeffs, &spdt);
            m.clear_values();
            let mut rhs = vec![0.0f64; n];
            stamp_all_bypass(&cx, &mut st, &mut m, &mut rhs, false);
        }
        let (evals, skips) = st.counters();
        assert_eq!(skips, 0, "a NaN iterate must not replay any cache");
        assert_eq!(evals, 6);
    }
}
