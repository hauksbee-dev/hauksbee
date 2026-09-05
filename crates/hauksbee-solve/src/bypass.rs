//! The classic SPICE device-evaluation bypass: skip re-evaluating a nonlinear
//! device whose inputs did not move, replaying its previously recorded stamp
//! instead of recomputing the `exp()`-heavy model.
//!
//! # What is cacheable, and for how long
//!
//! A nonlinear device's stamped contribution at iterate `x` depends on inputs
//! with three lifetimes: **per iteration** (the unknowns it reads at its
//! terminals, a series-R BJT's internal unknowns, and the pn-limiting anchor
//! `ctx.x_prev`), **per step** (the integration factor `coeffs.g`, the
//! charge-companion history `state.x1/dx1/x2`, the source time, `gmin`), and
//! **per run** (model parameters, temperature, effects toggles).
//!
//! Bypass operates WITHIN one step's Newton iteration sequence, so the cache
//! may hold the first two groups as long as it never survives a `newton_solve`
//! call: a `generation` counter bumps at solve start and a record is only
//! replayable inside the generation that recorded it. That single rule covers
//! every cross-step hazard at once (dt changes, LTE retries at a different h,
//! event retries, companion-history advance), because each is a fresh
//! `newton_solve` call.
//!
//! # The movement test
//!
//! Before evaluating device `d` on iteration >= 3, compare every unknown in
//! its READ set against the values at its last recorded evaluation: if all
//! satisfy `|v - v_last| <= 0.1*(reltol*max(|v|,|v_last|) + vntol)` (a
//! tightened SPICE `bypasstol`), replay the record; else evaluate fresh and
//! re-record. The comparison is NaN-safe (`!(delta <= tol)` counts as moved),
//! so a poisoned iterate always re-evaluates and hits the stamps' own
//! non-finite guards.
//!
//! The READ set is the device's node unknowns (`Device::nodes()` through the
//! layout) plus a series-R BJT's device-private internal unknowns, where the
//! intrinsic junction voltages live. Ground is not an unknown and never moves.
//!
//! # Safety discipline (enforced by the caller and this module)
//!
//! * never bypass on the first two iterations of a solve (`force_eval`);
//! * never on DC solves, event-frozen solves (`cmp_freeze`/`switch_freeze`),
//!   or the trials immediately after an event-resolved accept;
//! * the Armijo line-search residual evaluations keep the full `stamp_all`:
//!   those norms sit on the cancellation noise floor, so the residual the line
//!   search compares must stay order-exact;
//! * the accepted step must match the no-bypass reference to reltol: bypass
//!   may change the iterate PATH, never the answer.
//!
//! # Exclusion list (refuse rather than fake)
//!
//! Bypassed: **Diode, BJT, MOSFET**, the `exp()`-heavy junction devices whose
//! evaluation dominates a quiescent board's assembly. Excluded:
//!
//! * **Behavioral (B-source)**: its FD Jacobian probes the expression at
//!   perturbed dependency values every evaluation, and its fault channel
//!   (`take_behavioral_fault`) must see every iterate.
//! * **Comparator**: bang-bang output with hysteresis read from the CURRENT
//!   iterate; the discrete decision is the event the march bisects on.
//! * **VSwitch**: the event-flip device, and under break-before-make its stamp
//!   reads its SIBLING leg's control nodes, inputs outside its own terminals.
//! * **OpAmp**: a rail-clamp discontinuity decides its stamp shape, and
//!   evaluation is a handful of multiplies.
//! * Linear devices (R, C, L, sources, E/F/G/H, coupling): matrix parts are
//!   constant or backbone-compiled (plan.rs), RHS history changes per step,
//!   and there is no `exp()` to skip.
//!
//! # Replay fidelity
//!
//! A fresh evaluation stamps through [`RecordingSink`], which resolves each
//! write to its frozen-pattern slot and applies it with `add_at`, the same
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
            // as terminal movement.
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

    /// (evaluations, skips) since construction; the observability the bypass
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{NewtonBypass, SolverOptions};
    use crate::stamp::{reserve_pattern, stamp_all, IntegCoeffs};
    use crate::system::ReactiveState;
    use crate::test_fixtures::{bjt, cap, comparator, diode, res, stamp_ctx, sw, trapz, vdc, GND};
    use hauksbee_ir::{BjtModel, Circuit, Device, DiodeModel, NodeId, SourceKind};

    /// Source, resistors, cap, charge-carrying diode, BJT, MOSFET, switch,
    /// comparator: every stamp class the walk visits. Returns `(circuit, n1)`.
    fn mixed_board() -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let (vin, n1, n2, n3, n4) = (
            c.node("vin"),
            c.node("n1"),
            c.node("n2"),
            c.node("n3"),
            c.node("n4"),
        );
        vdc(&mut c, "V1", vin, 3.0);
        res(&mut c, "R1", vin, n1, 1e3);
        cap(&mut c, "C1", n1, GND, 1e-9);
        diode(
            &mut c,
            "D1",
            n1,
            n2,
            DiodeModel {
                cjo: 4e-12,
                tt: 10e-9,
                ..DiodeModel::default()
            },
        );
        res(&mut c, "R2", n2, GND, 2e3);
        bjt(&mut c, "Q1", vin, n2, n3, &BjtModel::default());
        res(&mut c, "RE", n3, GND, 100.0);
        c.add(Device::Mosfet {
            name: "M1".into(),
            d: n1,
            g: n2,
            s: GND,
            b: None,
            model: Default::default(),
        });
        sw(&mut c, "S1", n1, n4, n2, (2.0, 1.0), 10.0);
        comparator(&mut c, "K1", n4, n1, n2, 0.05);
        (c, n1)
    }

    struct Rig {
        c: Circuit,
        n1: NodeId,
        layout: Layout,
        m: SparseMatrix,
        state: ReactiveState,
        opts: SolverOptions,
        x: Vec<f64>,
    }

    impl Rig {
        fn new(x_of: impl Fn(usize) -> f64) -> Rig {
            let (c, n1) = mixed_board();
            let layout = Layout::new(&c);
            let mut m = SparseMatrix::new(layout.size);
            reserve_pattern(&c, &layout, &mut m);
            let x = (0..layout.size).map(x_of).collect();
            let state = ReactiveState::new(c.devices.len());
            Rig {
                c,
                n1,
                layout,
                m,
                state,
                opts: SolverOptions::default(),
                x,
            }
        }

        /// One bypass-armed assembly; returns the rows and RHS.
        fn assemble(
            &mut self,
            st: &mut BypassState,
            force: bool,
        ) -> (Vec<Vec<(usize, f64)>>, Vec<f64>) {
            let mut ctx = stamp_ctx(
                &self.c,
                &self.layout,
                &self.opts,
                &self.x,
                &self.state,
                trapz(1e-7),
            );
            ctx.time = 1e-6;
            ctx.gmin = 1e-12;
            self.m.clear_values();
            let mut rhs = vec![0.0f64; self.layout.size];
            stamp_all_bypass(&ctx, st, &mut self.m, &mut rhs, force);
            (
                (0..self.layout.size)
                    .map(|i| self.m.row(i).to_vec())
                    .collect(),
                rhs,
            )
        }

        fn interpreted(&mut self) -> (Vec<Vec<(usize, f64)>>, Vec<f64>) {
            let mut ctx = stamp_ctx(
                &self.c,
                &self.layout,
                &self.opts,
                &self.x,
                &self.state,
                trapz(1e-7),
            );
            ctx.time = 1e-6;
            ctx.gmin = 1e-12;
            self.m.clear_values();
            let mut rhs = vec![0.0f64; self.layout.size];
            stamp_all(&ctx, &mut self.m, &mut rhs);
            (
                (0..self.layout.size)
                    .map(|i| self.m.row(i).to_vec())
                    .collect(),
                rhs,
            )
        }
    }

    fn assert_bit_identical(
        a: &(Vec<Vec<(usize, f64)>>, Vec<f64>),
        b: &(Vec<Vec<(usize, f64)>>, Vec<f64>),
    ) {
        assert_eq!(a.0, b.0, "rows differ");
        for (i, (x, y)) in a.1.iter().zip(b.1.iter()).enumerate() {
            assert_eq!(x.to_bits(), y.to_bits(), "rhs {i} differs");
        }
    }

    /// A fresh bypass assembly is bit-identical to the interpreted walk; an
    /// unmoved iterate replays bit-identically; a moved terminal re-evaluates
    /// only its readers; a new generation re-evaluates everything; a NaN
    /// iterate never replays.
    #[test]
    fn bypass_replays_exactly_and_invalidates_correctly() {
        let mut rig = Rig::new(|i| ((i as f64) * 0.61).sin() * 2.0 + 0.4);
        for (i, v) in rig.state.x1.iter_mut().enumerate() {
            *v = 0.2 * (i as f64 + 1.0);
        }
        let reference = rig.interpreted();
        let mut st = BypassState::build(&rig.c, &rig.layout);
        st.begin_solve();
        let fresh = rig.assemble(&mut st, true);
        assert_bit_identical(&fresh, &reference);
        assert_eq!(
            st.counters(),
            (3, 0),
            "diode+bjt+mosfet evaluated, none skipped"
        );

        let replayed = rig.assemble(&mut st, false);
        assert_bit_identical(&replayed, &fresh);
        assert_eq!(st.counters(), (3, 3));

        let n1_idx = rig.layout.node(rig.n1).unwrap();
        rig.x[n1_idx] += 0.5;
        rig.assemble(&mut st, false);
        let (evals, _) = st.counters();
        assert!(
            evals > 3 && evals < 6,
            "moving one node re-evaluates its readers only (evals={evals})"
        );
        st.begin_solve();
        rig.assemble(&mut st, false);
        assert_eq!(
            st.counters().0 - evals,
            3,
            "generation bump re-evaluates all three"
        );

        let mut rig = Rig::new(|_| 0.4);
        let mut st = BypassState::build(&rig.c, &rig.layout);
        st.begin_solve();
        rig.assemble(&mut st, true);
        rig.x.iter_mut().for_each(|v| *v = f64::NAN);
        rig.assemble(&mut st, false);
        assert_eq!(
            st.counters(),
            (6, 0),
            "a NaN iterate must not replay any cache"
        );
    }

    /// At the `newton_solve` level on a stiff diode/BJT board: bypass actually
    /// skips evaluations and converges to the no-bypass root within tolerance.
    #[test]
    fn newton_solve_with_bypass_skips_and_matches() {
        use crate::newton::{newton_solve, Workspace};
        let mut c = Circuit::new();
        let (vin, mid, out) = (c.node("vin"), c.node("mid"), c.node("out"));
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: GND,
            kind: SourceKind::Sin {
                offset: 3.5,
                amplitude: 2.0,
                freq: 1e5,
                delay: 0.0,
                theta: 0.0,
                phase: 0.0,
            },
        });
        res(&mut c, "R1", vin, mid, 1e3);
        diode(&mut c, "D1", mid, GND, DiodeModel::default());
        res(&mut c, "R2", vin, out, 4.7e3);
        bjt(&mut c, "Q1", out, out, GND, &BjtModel::default());

        let coeffs =
            IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, 1e-7, false);
        let run = |bypass: NewtonBypass| {
            let mut opts = SolverOptions::default();
            opts.newton_bypass = bypass;
            opts.ladder =
                crate::options::RobustnessLadder::none().with(crate::options::Strategy::LineSearch);
            let mut ws = Workspace::new(&c);
            let state = ReactiveState::new(c.devices.len());
            crate::newton::dc_operating_point(&mut ws, &c, &opts).expect("dc converges");
            let r = newton_solve(
                &mut ws, &c, &opts, 4.3e-7, 1e-7, coeffs, &state, false, false, opts.gmin, 1.0,
            );
            assert!(
                r.converged && r.iters > 2,
                "converged={} iters={}",
                r.converged,
                r.iters
            );
            (ws.x.clone(), ws.bypass_counters())
        };
        let (x_ref, counters_off) = run(NewtonBypass::Off);
        assert_eq!(counters_off, (0, 0), "bypass Off never builds the cache");
        let (x_byp, (evals, skips)) = run(NewtonBypass::On);
        assert!(evals > 0 && skips > 0, "evals={evals} skips={skips}");
        let opts = SolverOptions::default();
        for i in 0..x_ref.len() {
            let tol =
                opts.reltol * x_ref[i].abs().max(x_byp[i].abs()) + opts.vntol.max(opts.abstol);
            assert!(
                (x_ref[i] - x_byp[i]).abs() <= tol,
                "unknown {i}: {} vs {}",
                x_byp[i],
                x_ref[i]
            );
        }
    }
}
