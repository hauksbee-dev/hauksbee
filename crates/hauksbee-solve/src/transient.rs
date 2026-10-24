//! The transient driver: time-marching with companion models, adaptive step
//! control, and event-aware timestep refinement.
//!
//! Each accepted step solves a Newton operating point at `t + dt`, estimates
//! the local truncation error of the reactive elements, and either accepts the
//! step (advancing history) or shrinks `dt` and retries. Comparators and
//! switches are watched for threshold crossings; when one is straddled, the
//! step is bisected to land near the crossing so edges aren't smeared.
//!

use crate::error::{SolveError, SolvePhase, SolveResult};
use crate::newton::{dc_start_point, newton_solve, newton_solve_event, Workspace};
use crate::options::{DcInit, Integration, SolverOptions, StepControl, Strategy, SwitchModel};
use crate::stamp::{ComparatorLatch, IntegCoeffs};
use crate::system::ReactiveState;
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
use std::collections::HashMap;

/// Collected transient results.
#[derive(Debug, Clone, Default)]
pub struct Waveforms {
    /// Sample times (s).
    pub time: Vec<f64>,
    /// `node_voltages[node][sample]`, indexed by `NodeId.0` (ground included,
    /// always 0).
    pub node_voltages: Vec<Vec<f64>>,
    /// Branch currents for voltage sources / inductors, keyed by device name.
    pub branch_currents: Vec<(String, Vec<f64>)>,
}

impl Waveforms {
    /// Voltage waveform of a node by name, if present.
    pub fn node(&self, circuit: &Circuit, name: &str) -> Option<&[f64]> {
        for id in 0..circuit.node_count() {
            if circuit.node_name(crate::node_id(id)) == name {
                return self.node_voltages.get(id).map(Vec::as_slice);
            }
        }
        None
    }

    /// Last value of a named node.
    #[must_use]
    pub fn final_node(&self, circuit: &Circuit, name: &str) -> Option<f64> {
        self.node(circuit, name).and_then(|w| w.last().copied())
    }

    /// Empty table for `n_nodes` nodes (ground at index 0), no branch currents.
    pub(crate) fn nodes_only(n_nodes: usize) -> Self {
        Self {
            time: Vec::new(),
            node_voltages: vec![Vec::new(); n_nodes],
            branch_currents: Vec::new(),
        }
    }

    /// Zero-filled node table on a fixed sample `grid` (the torn executors'
    /// assembly surface), no branch currents.
    pub(crate) fn on_grid(n_nodes: usize, grid: &[f64]) -> Self {
        Self {
            time: grid.to_vec(),
            node_voltages: vec![vec![0.0; grid.len()]; n_nodes],
            branch_currents: Vec::new(),
        }
    }

    /// Append one accepted step's node block (`x[k-1]` is node `k`; a
    /// partitioned engine's `x` may stop short of unmapped nodes, which read 0).
    pub(crate) fn push_nodes(&mut self, s: &StepSample) {
        self.time.push(s.time);
        for (node, series) in self.node_voltages.iter_mut().enumerate() {
            let v = if node == 0 {
                0.0
            } else {
                s.x.get(node - 1).copied().unwrap_or(0.0)
            };
            series.push(v);
        }
    }

    /// Node-voltage table for `n_nodes` filled by `run`, which streams every
    /// accepted step into the sink it is handed. Shared by every executor that
    /// marches an engine that only streams samples (the partitioned engine,
    /// the capture solves): none of them assembles a table of its own.
    pub(crate) fn collect_nodes(
        n_nodes: usize,
        run: impl FnOnce(&mut dyn FnMut(StepSample)) -> SolveResult<()>,
    ) -> SolveResult<Self> {
        let mut wf = Self::nodes_only(n_nodes);
        run(&mut |s| wf.push_nodes(&s))?;
        Ok(wf)
    }
}

/// A single accepted step, handed to streaming consumers.
#[derive(Debug, Clone)]
pub struct StepSample<'a> {
    pub time: f64,
    /// All unknowns: node voltages `[0..n_nodes]` then branch currents.
    pub x: &'a [f64],
    /// `x` is the march's starting state but no state of the circuit: the
    /// t = 0 sample of a march that opened on the staged ladder's relaxed
    /// seed ([`crate::DcStart::RelaxedSeed`]) or on an estimate assembled
    /// without a DC root. A consumer may keep it as the point the march
    /// left from, never read a voltage, current or power off it; every
    /// later sample is a solution of the circuit.
    pub stand_in: bool,
}

/// Numerical diagnostics measured at the final accepted transient point.
///
/// `None` means the selected solver path does not currently expose an equation
/// residual; callers must preserve that absence rather than writing zero.
#[derive(Debug, Clone, Default)]
pub struct TransientDiagnostics {
    pub final_residual: Option<(f64, usize)>,
    /// The relay latch at that final point, for a caller that continues the
    /// SAME circuit in a later run (a co-sim chunk); `None` when none is tracked.
    pub switch_latch: Option<HashMap<DeviceId, bool>>,
    /// How the march's own t = 0 DC operating point was reached; `None` when
    /// the march did not start from one (a power-on from rest, or a
    /// continuation that opened on carried state).
    pub operating_point: Option<OperatingPointReport>,
    /// Everything beyond the unknowns that the march ENDED on, for a caller
    /// that continues the same circuit in a later run (see [`MarchCarry`]).
    /// `None` from the partitioned engine, which keeps no such state.
    pub carry: Option<MarchCarry>,
}

/// What a monolithic march ends on besides its unknown vector.
///
/// A continuation (a co-sim chunk, [`DcInit::Continue`]) needs it to pick up
/// exactly where the previous window closed, as one uninterrupted march would.
///
/// A continuation used to re-solve a DC operating point only to discard it,
/// for its side effects: the relay latch it left, and whether it needed the
/// staged ladder (which arms the staged regularizers for the march). Both are
/// facts about the RUN, not about the window, and re-deriving them per window
/// made them depend on the window: a board whose steady state is a different
/// latch than the one the march reached re-latched at every boundary, and the
/// regularizers could arm in one window and not the next. Carrying them is the
/// physical answer (state is continuous across a window boundary) and costs
/// nothing; the re-solve cost ~95% of a continuation window on a stiff board.
///
/// The reactive history VALUES are carried too: they hold device states a
/// fresh seed cannot re-derive from the unknowns, chiefly an op-amp's lagged
/// output EMF, which a re-seed snapped to its DC target at every boundary.
/// The continuation still opens on a flat history (derivative 0, previous
/// value = current), as a seeded start does; see [`Transient::with_carry`].
#[derive(Debug, Clone)]
pub struct MarchCarry {
    switch_latch: Option<HashMap<DeviceId, bool>>,
    staged: bool,
    reactive: ReactiveState,
}

impl MarchCarry {
    /// The relay latch the march ended on (`None` when none is tracked).
    #[must_use]
    pub const fn switch_latch(&self) -> Option<&HashMap<DeviceId, bool>> {
        self.switch_latch.as_ref()
    }

    /// Whether the march ran with the staged regularizers armed (its run's
    /// t = 0 DC point needed the staged ladder).
    #[must_use]
    pub const fn staged(&self) -> bool {
        self.staged
    }

    /// Whether this carry fits `circuit` (same device count; a structural
    /// edit between runs invalidates it).
    const fn fits(&self, circuit: &Circuit) -> bool {
        self.reactive.x1.len() == circuit.devices.len()
    }
}

/// What a transient's t = 0 DC operating point is, beyond converging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatingPointReport {
    /// The staged ladder found no root and the march started from its
    /// relaxed (diodes-open) seed instead (see
    /// [`Workspace::relaxed_seed_adopted`]).
    pub relaxed_seed_adopted: bool,
    /// Comparators held at a rail because they have no self-consistent DC
    /// state, released at t = 0 (see [`Workspace::pinned_comparators`]).
    pub pinned_comparators: Vec<(DeviceId, bool)>,
}

/// The transient engine.
pub struct Transient {
    opts: SolverOptions,
    switch_latch_seed: Option<HashMap<DeviceId, bool>>,
    carry: Option<MarchCarry>,
}

impl Transient {
    /// New engine with the given options.
    #[must_use]
    pub const fn new(opts: SolverOptions) -> Self {
        Self {
            opts,
            switch_latch_seed: None,
            carry: None,
        }
    }

    /// Continue from the state a previous march of the SAME circuit ended on
    /// (its [`TransientDiagnostics::carry`]). Honoured only on a continuation:
    /// [`DcInit::Continue`] with a `dc_seed` of the right size. The march then
    /// opens on the carried unknowns, latch, regularizer arming and reactive
    /// history values (on a flat history, as a seeded start opens), and solves
    /// no DC operating point at all. A carry that does
    /// not fit the circuit is ignored (the continuation re-derives its state
    /// as before).
    #[must_use]
    pub fn with_carry(mut self, carry: Option<MarchCarry>) -> Self {
        self.carry = carry;
        self
    }

    /// Start with the relay latch a previous run ENDED on. A latching relay is
    /// state, exactly like a capacitor's charge: a co-sim that builds a fresh
    /// workspace per chunk would otherwise reset a 555's SR latch every chunk.
    #[must_use]
    pub fn with_switch_latch(mut self, latch: Option<HashMap<DeviceId, bool>>) -> Self {
        self.switch_latch_seed = latch;
        self
    }

    /// Run to `tstop`, collecting every accepted step into [`Waveforms`].
    ///
    /// # Errors
    ///
    /// A failed t=0 operating point, or a step that cannot converge (or hits a
    /// singular system or behavioral fault) even at the minimum timestep.
    pub fn run(&self, circuit: &Circuit, tstop: f64) -> SolveResult<Waveforms> {
        self.run_with_diagnostics(circuit, tstop)
            .map(|(waveforms, _)| waveforms)
    }

    /// Collect waveforms and the measured final equation residual.
    ///
    /// # Errors
    ///
    /// As [`Transient::run`].
    pub fn run_with_diagnostics(
        &self,
        circuit: &Circuit,
        tstop: f64,
    ) -> SolveResult<(Waveforms, TransientDiagnostics)> {
        let mut wf = Waveforms::nodes_only(circuit.node_count());
        // Pre-name branch current outputs, pairing each Vsource/Inductor with the
        // ABSOLUTE index of its branch-current unknown in `x`. Branch unknowns are
        // NOT contiguous per device kind: `Layout` assigns one to every
        // branch-owning device, Vsource, Inductor, VCVS, CCVS and a V-output
        // behavioral source, interleaved in circuit order. A running offset over
        // the Vsource/Inductor-only list therefore lands on the wrong slot as soon
        // as a dependent voltage source precedes one of them; read each device's
        // real index via `Layout::branch(id)` instead. (The node block can also be
        // larger than `node_count()`, a series-resistance BJT owns private
        // internal unknowns there, which `Layout::branch` already accounts for.)
        let layout = crate::system::Layout::new(circuit);
        let mut branch_outputs: Vec<usize> = Vec::new();
        for (id, dev) in circuit.iter() {
            if matches!(dev, Device::Vsource { .. } | Device::Inductor { .. }) {
                if let Some(bi) = layout.branch(id) {
                    wf.branch_currents
                        .push((dev.name().to_string(), Vec::new()));
                    branch_outputs.push(bi);
                }
            }
        }

        let diagnostics = self.run_streaming_with_diagnostics(circuit, tstop, |s| {
            // A stand-in start is no state of the circuit, so no waveform
            // reports it: the waveform opens at the first solved step.
            if s.stand_in {
                return;
            }
            wf.push_nodes(&s);
            for (slot, &idx) in wf.branch_currents.iter_mut().zip(branch_outputs.iter()) {
                slot.1.push(s.x.get(idx).copied().unwrap_or(0.0));
            }
        })?;
        Ok((wf, diagnostics))
    }

    /// Run to `tstop`, invoking `sink` with each accepted step. This is what the
    /// live UI will consume; it never buffers the whole waveform.
    ///
    /// # Errors
    ///
    /// As [`Transient::run`].
    pub fn run_streaming<F: FnMut(StepSample)>(
        &self,
        circuit: &Circuit,
        tstop: f64,
        sink: F,
    ) -> SolveResult<()> {
        self.run_streaming_with_diagnostics(circuit, tstop, sink)
            .map(|_| ())
    }

    /// [`Transient::run_streaming`], also returning the final-point diagnostics.
    ///
    /// # Errors
    ///
    /// As [`Transient::run`].
    pub fn run_streaming_with_diagnostics<F: FnMut(StepSample)>(
        &self,
        circuit: &Circuit,
        tstop: f64,
        sink: F,
    ) -> SolveResult<TransientDiagnostics> {
        self.run_streaming_seeded_with_diagnostics(circuit, tstop, None, sink)
    }

    /// Like [`Transient::run_streaming`], but warm-starts the t=0 DC operating
    /// point from `dc_seed` (a prior operating point of the same circuit, e.g.
    /// the previous co-sim chunk's final unknowns). Lets a stiff nonlinear board
    /// skip the cold-start homotopy each chunk; exact (same root, fewer Newton
    /// iters), with a cold fallback when the seed does not fit or fails.
    ///
    /// # Errors
    ///
    /// As [`Transient::run`].
    pub fn run_streaming_seeded<F: FnMut(StepSample)>(
        &self,
        circuit: &Circuit,
        tstop: f64,
        dc_seed: Option<&[f64]>,
        sink: F,
    ) -> SolveResult<()> {
        self.run_streaming_seeded_with_diagnostics(circuit, tstop, dc_seed, sink)
            .map(|_| ())
    }

    /// [`Transient::run_streaming_seeded`], also returning the final-point
    /// diagnostics.
    ///
    /// # Errors
    ///
    /// As [`Transient::run`].
    #[allow(clippy::too_many_lines)] // the reference step loop: one accept/reject cycle per step
    pub fn run_streaming_seeded_with_diagnostics<F: FnMut(StepSample)>(
        &self,
        circuit: &Circuit,
        tstop: f64,
        dc_seed: Option<&[f64]>,
        mut sink: F,
    ) -> SolveResult<TransientDiagnostics> {
        const PRED_MAX_SCALE: f64 = 2.0;
        let opts = &self.opts;

        // Partitioned fast path: only taken when Auto and the topology/step make
        // it safe and profitable. Otherwise we fall through to the reference
        // monolithic engine below (which `Partitioning::Off` always uses), so
        // results never regress and Off is bit-identical to the classic solver.
        //
        // BUT skip it when a warm DC seed is supplied (a co-sim march continuing
        // from the previous chunk). The partitioned path rebuilds and re-seeds a
        // cold global DC operating point plus each island's cold DC every chunk;
        // with a good seed the monolithic path below converges in ~1 Newton
        // iteration, which is far cheaper than that per-chunk cold seeding on a
        // stiff nonlinear board. Both paths solve to the same operating point
        // (Off is bit-identical), so switching is exact.
        //
        // ALSO skip it under DcInit::FromZero: the power-on path has no DC
        // operating point at all (that is the whole point), so the partitioned
        // engine's per-island cold DC seeding has nothing to do. Bail to the
        // monolithic path, which honors FromZero below.
        if dc_seed.is_none() && opts.dc_init == DcInit::Solve {
            if let Some(mut pt) = crate::partitioned::PartitionedTransient::try_build(circuit, opts)
            {
                return pt
                    .run_streaming(circuit, tstop, sink)
                    .map(|()| TransientDiagnostics::default());
            }
        }

        let mut ws = Workspace::new(circuit);
        ws.set_switch_latch(self.switch_latch_seed.clone());
        let n_dev = circuit.devices.len();

        // Convergence doctrine for behavioral sources:
        // B-sources are where decks go to diverge, an arbitrary expression's
        // tangent can overshoot Newton exactly like the traveling stiff-mesh
        // case the Armijo line search was built for. Their PRESENCE arms the
        // per-step line search unconditionally: it is a no-op on steps that
        // already converge (alpha = 1 accepts the full step, and the ladder
        // rung is bit-identical when never reached), so ordinary B-decks pay
        // one residual evaluation per iteration and stubborn ones get the
        // globalization instead of a dt_min death. Decks WITHOUT a B-source
        // take the `false` branch and stay bit-identical (the large-board
        // fixture hash pins that). Heavier rungs (TransientDyn, staged
        // regularizers) stay caller-granted; the corpus converges without
        // them, and a non-converged march still REFUSES loudly with the
        // device-named fault rather than emitting a wrong waveform.
        if circuit
            .devices
            .iter()
            .any(|d| matches!(d, hauksbee_ir::Device::Behavioral { .. }))
        {
            ws.set_tran_line_search(true);
        }

        // Seed t = 0. Normally a DC operating point (warm-started from the prior
        // chunk when a seed is supplied); under FromZero a power-on start with
        // no DC solve at all: x(0) = 0 and reactive history zeroed further down.
        let from_zero = opts.dc_init == DcInit::FromZero;
        // A continuation that carries BOTH halves of the state -- the unknowns
        // and the relay latch -- has nothing to ask a DC solve: the point would
        // be discarded below, and a board whose latch drives the node that flips
        // it (a free-running 555 astable) has no operating point to find, so
        // every chunk would walk the whole homotopy ladder to a failure
        // (measured: 95% of the wall time of a 2 ms run). A caller that carries
        // the latch, or the whole `MarchCarry`, gets this; a continuation that
        // carries only the unknowns still solves, for the arming it needs.
        let continuing =
            opts.dc_init == DcInit::Continue && dc_seed.is_some_and(|s| s.len() == ws.x.len());
        // A full carry (see `MarchCarry`) supersedes the latch-only seed.
        let carry = self
            .carry
            .as_ref()
            .filter(|c| continuing && c.fits(circuit));
        if let Some(c) = carry {
            ws.set_switch_latch(c.switch_latch.clone());
        }
        let carried = continuing && (carry.is_some() || self.switch_latch_seed.is_some());
        // Known softness (review note): the staged-DC regularizers arm on
        // ws.used_staged_dc(), which only a DC solve can set, so a power-on
        // march runs the bare per-step path. A stiff board that needed the
        // regularized rescue fails honestly (the caller keeps its original
        // DC error); arming them under FromZero on nonlinear circuits is a
        // plausible improvement when the strategy ladder lands.
        // `.ic V(node)=val` under `uic`: the named node voltages are the given
        // initial conditions, everything else powers on from rest. (SPICE-compat
        // With `uic`, `.ic` values seed the start directly, no DC solve.)
        if from_zero {
            // Power-on: the unknown vector rests at zero. No DC solve to fail;
            // the ramp (paired Ramped sources) integrates the state up from here.
            for v in &mut ws.x {
                *v = 0.0;
            }
            for &(nid, val) in &circuit.initial_conditions {
                if let Some(i) = ws.layout.node(nid) {
                    ws.x[i] = val;
                }
            }
            // DEVICE initial conditions (`Cxxx a b <val> IC=v`, `Lxxx ... IC=i`)
            // are part of what `uic` MEANS in SPICE, not an optional extra: "the
            // initial conditions given on the capacitor and inductor lines and on
            // .ic lines are used". Ignoring them was a wrong ANSWER, not a
            // convergence choice. On a switched-mirror deck's integrator node
            // (`CM0 mem0 0 1n IC=5`) ngspice starts at 5.000 V while we started
            // at 0 and charged up through the 47 kΩ pull-up, so the whole
            // pre-stimulus window read 5*(1-exp(-t/47µs)) instead of a flat 5 V:
            // 2.82 V against 5 V at t = 39 µs, a 44% error on a quantity the deck
            // states outright.
            //
            // A grounded capacitor's IC pins its live node directly. A floating
            // one states a DIFFERENCE, which is a constraint rather than an
            // assignment, so it is left to the reactive history below (the
            // companion drives the pair to the stated difference on the first
            // step) and to any `.ic` card the deck also gave. `.ic` wins where
            // both speak, having named the node explicitly.
            let named: std::collections::HashSet<u32> = circuit
                .initial_conditions
                .iter()
                .map(|&(nid, _)| nid.0)
                .collect();
            for dev in &circuit.devices {
                let (live, other, v0) = match dev {
                    hauksbee_ir::Device::Capacitor {
                        a, b, ic: Some(v), ..
                    } => (*a, *b, *v),
                    _ => continue,
                };
                let (live, v0) = if other.is_ground() {
                    (live, v0)
                } else if live.is_ground() {
                    (other, -v0)
                } else {
                    continue;
                };
                if named.contains(&live.0) {
                    continue;
                }
                if let Some(i) = ws.layout.node(live) {
                    ws.x[i] = v0;
                }
            }
        } else if !carried {
            // The DC solve manages its own staged regularizers internally and
            // leaves the workspace flag at 0. A continuation discards the point,
            // so one it cannot reach (a limited rail collapsed at DC under bulk
            // that holds it up) does not stop the march; each step must converge.
            // The march's starting state, which may be the staged ladder's
            // relaxed seed (`ws.relaxed_seed_adopted()`, handled below): the
            // march integrates the full circuit forward from it.
            if let Err(e) = dc_start_point(&mut ws, circuit, opts, dc_seed) {
                if !continuing {
                    return Err(e);
                }
            }
        }

        // Only when the DC point itself needed the staged fallback (the standard
        // plain+gmin+source ladder failed) is the board stiff enough that the
        // same reverse-biased-diode + cap-isolated-node degeneracy destabilizes
        // each transient step. Arm the staged regularizers (negligible branch
        // series R + node damping + node-block convergence) for the whole march
        // in that case only; an ordinary diode circuit (e.g. a rectifier) solved
        // on the normal ladder keeps the bit-identical classic transient path.
        // A carried march keeps the arming its run's t = 0 DC decided.
        let staged = carry.map_or_else(|| ws.used_staged_dc(), |c| c.staged);
        let transient_dyn = opts.ladder.has(Strategy::TransientDyn);
        if staged || transient_dyn {
            // Arm the staged regularizers (negligible branch series R + node
            // damping + node-block convergence) AND the VSwitch/diode control
            // limiting (which is gated on branch_reg>0). On a gated pulse
            // path the per-step Newton fails right at the gating-switch flip
            // (a multi-decade conductance snap as a control node climbs through
            // the switch transition); the control limiting tracks the switch
            // through that transition. Armed when the DC needed staging OR when
            // the caller grants Strategy::TransientDyn even on a cleanly-DC-solved but
            // dynamically-stiff march (the RAMP-from-rest pulse case).
            ws.set_staged_branch_reg(1e-2);
            // Arm the per-step transient event-freeze retry: when a bare per-step
            // Newton fails (the gating SPDT flip), retry the step with the
            // comparator+switch states frozen Gauss-Seidel + break-before-make
            // before cutting the timestep. Gated behind Strategy::TransientDyn (the
            // explicit pulse-path opt-in) so an ordinary staged board (e.g. the
            // DAC-rail co-sim, which needs the staged regularizers but whose
            // per-step Newton converges on the bare path) keeps its exact prior
            // behaviour and timing -- the retry only fires for callers chasing
            // the pulse transient. The retry itself is a no-op unless the bare
            // step actually fails, so even when armed it never changes a step
            // that already converged.
            if transient_dyn {
                crate::diagnostics::note(Strategy::TransientDyn);
                ws.set_tran_event(true);
                // Arm the GLOBAL Armijo line-search in the per-step Newton on the
                // same stiff-board opt-in. It is the globalization for the
                // post-reset traveling fused-mesh limit cycle (a huge Newton
                // correction whose maxdV rotates across switch-mesh / BJT-mirror
                // nodes, which per-node oscillation damping cannot catch). A no-op
                // on steps that already converge (alpha=1 full step), so it never
                // changes an already-good step; it only backtracks the overshoot.
                ws.set_tran_line_search(true);
            }
            // The dynamic re-pivot LU is enabled LOCALLY by the event-freeze retry
            // (newton_solve_event) only on the step that flips, so the bulk of
            // clean steps stay on the fast frozen path.
            // Strategy::DynamicPivotEveryStep additionally forces it on for
            // EVERY step (the old, slower lever, kept for stubborn boards);
            // the default arming relies on the per-step retry.
            if opts.ladder.has(Strategy::DynamicPivotEveryStep) {
                crate::diagnostics::note(Strategy::DynamicPivotEveryStep);
                ws.symbolic.set_allow_dynamic(true);
            }
        }

        let mut state = ReactiveState::new(n_dev);
        // Seed the reactive history from `ws.x`, which is now the DC operating
        // point (the normal path) or the `uic` start assembled just above.
        //
        // `seed_reactive_state` is the single place that reads a device's own `ic`
        // and prefers it over the node-derived value, so calling it is exactly
        // what makes `uic` honour `IC=`. It is unconditional: with `ws.x` at
        // rest and no `ic` anywhere it reproduces the all-zero construction
        // value, so a genuine power-on-from-rest march (`FromZero` + `Ramped`
        // sources, no IC anywhere) is unchanged.
        // A continuation opens where the previous window closed, not at the
        // board's steady state: overwrite the operating point just solved with
        // the carried unknowns, BEFORE the reactive history is seeded from
        // them. A continuation without a full carry still ran its DC solve
        // above, so the staged regularizers stay armed exactly as they were.
        if let Some(seed) = dc_seed.filter(|_| continuing) {
            ws.x.copy_from_slice(seed);
            ws.clear_relaxed_seed_adopted();
        }
        let operating_point = (!from_zero && !continuing).then(|| OperatingPointReport {
            relaxed_seed_adopted: ws.relaxed_seed_adopted(),
            pinned_comparators: ws.pinned_comparators().to_vec(),
        });
        // A full carry hands over the histories themselves; re-deriving them
        // from the unknowns would reset every state the unknowns do not hold.
        // The march still opens like a seeded one, with a backward-Euler step
        // (`first_step`) on a FLAT history: the window boundary is where the
        // caller may have stepped a source. Only the values carry; `dx1` and
        // `x2` restart as a seed leaves them (0 and `x1`). The companions pick
        // their history formula from `opts.integration`, not from the step, so
        // a trapezoidal one adds `dx1` even on that BE step (a carried slope
        // counted twice ran a 74HC14 RC oscillator 15% fast at 1 us windows);
        // and the first step's LTE reads `x2` as one step `h` back, which a
        // carried `x2` is not (a diode-fed buck dropped its 3.3 V rail).
        if let Some(c) = carry {
            state.clone_from(&c.reactive);
            state.dx1.fill(0.0);
            state.x2.clone_from(&state.x1);
            for bank in &mut state.xb {
                bank.dx1.fill(0.0);
                bank.x2.clone_from(&bank.x1);
            }
        } else {
            seed_reactive_state(&mut state, circuit, &ws, opts);
        }
        if ws.relaxed_seed_adopted() {
            start_forward_junctions_uncharged(&mut state, circuit, &mut ws, opts);
        }

        let mut t = 0.0;
        let mut dt = match opts.step {
            StepControl::Fixed { dt } => dt,
            StepControl::Adaptive { dt_initial, .. } => dt_initial.min(tstop),
        };
        let (dt_min, dt_max) = match opts.step {
            StepControl::Fixed { dt } => (dt, dt),
            StepControl::Adaptive { dt_min, dt_max, .. } => (dt_min, dt_max.min(tstop)),
        };

        // Emit the operating point at t = 0.
        sink(StepSample {
            time: 0.0,
            x: &ws.x,
            stand_in: ws.relaxed_seed_adopted(),
        });

        let mut first_step = true;
        // The previous accepted step ended on an undelayed comparator's
        // decision, so its output steps as this one opens: integrate this step
        // first order from a flat derivative history, as the first one.
        // Trapezoidal carries each capacitor's pre-flip current into the step
        // and splits the step's charge between the two rails' drive, a
        // half-step lag per edge (measured: an RC relaxation oscillator's
        // period 3.6% long).
        let mut restart_order = false;
        let mut x_accepted = ws.x.clone();
        let mut steps_taken: u64 = 0;
        let max_steps: u64 = 50_000_000; // safety valve
                                         // Spacing of the reactive history's two back samples (x1..x2), i.e.
                                         // the previous ACCEPTED step: the LTE divided difference needs it to
                                         // tell real curvature from a step-size change on a slope. 0.0 = no
                                         // accepted step yet.
        let mut h_prev_accepted: f64 = 0.0;
        let mut failed_steps: u32 = 0; // against `opts.max_failed_steps`

        // Source breakpoints (PWL vertices, PULSE corners): mandatory step
        // landings for the ADAPTIVE controller. The fixed-step path is left
        // untouched on purpose: a fixed dt is a user contract (and the
        // bit-for-bit reference discipline pins its behavior); if a fixed
        // grid strides a source corner, that is the user's stated sampling
        // choice. See breakpoint_table for why the adaptive path needs this.
        let breakpoints = match opts.step {
            StepControl::Adaptive { .. } => breakpoint_table(circuit, tstop),
            StepControl::Fixed { .. } => Vec::new(),
        };
        let mut next_bp = 0usize;
        let adaptive = matches!(opts.step, StepControl::Adaptive { .. });
        // On the staged-armed march, the one whose comparators would
        // otherwise stamp the smooth transfer, comparators latch: they decide
        // from the accepted state and their outputs follow the decision with
        // the part's delay (`ComparatorLatch`).
        let latched = staged || transient_dyn;
        let comparators: Vec<(DeviceId, f64)> = circuit
            .iter()
            .filter_map(|(id, d)| match d {
                Device::Comparator { tpd_s, .. } => Some((id, *tpd_s)),
                _ => None,
            })
            .collect();
        // A latched comparator with a stated delay slews: its output is a
        // continuous control the event locator can aim at. Any other steps.
        let stepped: Vec<NodeId> = circuit
            .iter()
            .filter_map(|(_, d)| match d {
                Device::Comparator { out, tpd_s, .. } if !(latched && *tpd_s > 0.0) => Some(*out),
                _ => None,
            })
            .collect();

        // Extrapolated trial seed. Seeding the bare trial Newton from the
        // previous ACCEPTED point starts a full step away from the root on a
        // charging/discharging trajectory; a linear extrapolation through the
        // last two accepted points measures closer to the root on the large
        // majority of converged trials. A closer start both cuts iterations
        // and can skip the far-from-root small-alpha line-search grind
        // entirely. EXACT root, different iterate sequence:
        // Newton converges to the same operating point within tolerance from
        // either seed, and every safety mechanism (Armijo line search, staged
        // damping, stall bail, event retry, step cut) still guards the path.
        // SCOPE: only the TransientDyn-armed ADAPTIVE march (the large-board
        // pulse-path bundle, which is physics-gated, never bit-pinned); every
        // fixed-step march and every unarmed path keeps the previous-point
        // seed bit-identically.
        //
        // The extrapolation ratio is clamped to the controller's own 2.0
        // growth factor: after an event-resolved step the resume dt can jump
        // decades above the tiny flip step, and extrapolating a ns-scale
        // difference across a us-scale step would manufacture a wild seed.
        // The step AFTER an event-resolved accept is not extrapolated at all
        // (pred_skip_once): the accepted pair straddles the discontinuity the
        // event loop just resolved, so the linear history is a lie there.
        let predictor_armed = transient_dyn && matches!(opts.step, StepControl::Adaptive { .. });
        let mut pred_skip_once = false;
        // The accepted point BEFORE the current one: the predictor's second
        // history sample. Maintained only when the predictor wants it.
        let mut x_accepted_prev: Option<Vec<f64>> = None;
        let mut final_residual = None;

        #[allow(clippy::while_float)] // t advances by a positive step and lands exactly on tstop
        while t < tstop - 1e-18 {
            steps_taken += 1;
            if steps_taken > max_steps {
                return Err(SolveError::NonConvergence {
                    message: format!("exceeded step budget at t={t}"),
                    phase: SolvePhase::Transient,
                    time: Some(t),
                    dt: None,
                    iterations: usize::try_from(steps_taken).ok(),
                    blame: None,
                });
            }
            // Floor against LTE-rejection thrash FIRST, then clamp to the time
            // remaining to tstop. Order matters: clamping to (tstop - t) first
            // and flooring second would push the final partial step, when the
            // remaining time is below dt_min, back up past tstop, overshooting
            // the requested stop by up to dt_min. The exact landing on tstop
            // wins over the floor: dt_min is a thrash guard, not a sampling
            // contract (same rationale as the breakpoint landing below).
            let mut h = dt;
            if h < dt_min {
                h = dt_min;
            }
            h = h.min(tstop - t);
            // Never stride across a source corner: shorten the trial step to
            // land EXACTLY on the next breakpoint. dt itself is not reduced,
            // so after the corner the controller resumes at its own rhythm.
            // The landing step may dip below dt_min (at most once per corner,
            // when the controller has already ground down near the corner):
            // dt_min is a floor against LTE-rejection thrash, not a sampling
            // contract, and an exact landing is the whole point of the table.
            // `bp_landing` records the truncation so the LTE accept below can
            // keep that promise: the landing sliver's LTE says nothing about a
            // dt-sized step, and growing from the sliver would collapse dt to
            // ~2x the sliver right after every corner (see the accept branch).
            let mut bp_landing = false;
            if !breakpoints.is_empty() {
                while next_bp < breakpoints.len() {
                    let bp = breakpoints[next_bp];
                    if bp <= t + f64::max(1e-18, bp * 1e-12) {
                        next_bp += 1;
                    } else {
                        break;
                    }
                }
                if next_bp < breakpoints.len() {
                    let bp = breakpoints[next_bp];
                    if t + h > bp {
                        h = bp - t;
                        bp_landing = true;
                    }
                }
            }

            // Never stride past a corner of a latched comparator's output
            // (its hold ending, its slew reaching the rail): a kink the step
            // must land on, exactly as a source corner (and with the same
            // `bp_landing` promise).
            if latched && adaptive {
                if let Some(left) = next_output_corner(&comparators, &state) {
                    if left > dt_min && h > left {
                        h = left;
                        bp_landing = true;
                    }
                }
            }

            // The relay latch this step enters with: the trial commits its own,
            // and the (adaptive-only) event locator must aim at the threshold
            // live BEFORE it.
            let latch0 = (adaptive && opts.effects.switch_model == SwitchModel::Hysteretic)
                .then(|| ws.switch_latch().cloned().unwrap_or_default());
            // Trial solve at t + h, seeded from the previous accepted point,
            // or from the clamped linear extrapolation when armed (above).
            ws.x.copy_from_slice(&x_accepted);
            if predictor_armed && !first_step && !pred_skip_once && h_prev_accepted > 0.0 {
                if let Some(prev) = x_accepted_prev.as_ref() {
                    let scale = (h / h_prev_accepted).min(PRED_MAX_SCALE);
                    for i in 0..ws.x.len() {
                        ws.x[i] = scale.mul_add(x_accepted[i] - prev[i], x_accepted[i]);
                    }
                }
            }
            // Bypass hold: the trials that follow
            // an event-resolved accept must not bypass; the accepted pair
            // straddles the discontinuity the event loop just resolved, the
            // same reason the extrapolation seed is skipped there. Mirrors
            // `pred_skip_once`'s lifetime (true from an event-resolved accept
            // until the next ordinary accept). A bool store, read only when
            // `NewtonBypass::On` is armed; inert on every default run.
            ws.set_bypass_hold(pred_skip_once);
            // `h_prev_accepted` is the spacing of the reactive history pair
            // (x1..x2); Gear-2's variable-step BDF2 stencil needs it whenever
            // the adaptive controller changed h. 0.0 (no accepted step yet)
            // means "assume uniform", exact because the seeded history is flat.
            let coeffs = IntegCoeffs::for_step(
                opts.integration,
                h,
                h_prev_accepted,
                first_step || restart_order,
            );
            let r = newton_solve(
                &mut ws,
                circuit,
                opts,
                t + h,
                h,
                coeffs,
                &state,
                false,
                false,
                opts.gmin,
                1.0,
            );

            let mut converged = r.converged;
            let used_event = if !converged && ws.tran_event() {
                // The bare per-step Newton limit-cycled (the gating SPDT flip
                // under mirror current, or a comparator-driven reset). Retry this step
                // through the event-freeze loop: freeze comparator+switch states
                // per inner solve, re-derive Gauss-Seidel with break-before-make,
                // until consistent. Re-seed ws.x to the accepted point first so the
                // freeze derives from the step's entry state. The dynamic-pivot LU
                // is enabled for the inner solves so the diode-reshaped matrix stays
                // factorable.
                let had_dyn = ws.symbolic.allow_dynamic();
                ws.symbolic.set_allow_dynamic(true);
                // Two-mode retry. The smooth-comparator pass resolves the
                // integrator-crosses-UP fire (the output comparator's C_adapt
                // feedback needs a continuous transfer there); the frozen pass
                // resolves the reset discharge (the smooth comparator's
                // high gain re-couples the collapsing integrator into the
                // pulse->switch loop and diverges, so the comparator must be
                // frozen like every other discrete state). The tuning option,
                // when set, tries the smooth mode FIRST (the historical default
                // for the FIRE step); either way the other mode is tried if the
                // first fails, so a single step is solved by whichever regime
                // fits.
                let prefer_smooth = opts.event_retry.smooth_comparator_first;
                let modes = [prefer_smooth, !prefer_smooth];
                for &cmp_smooth in &modes {
                    ws.x.copy_from_slice(&x_accepted);
                    if newton_solve_event(
                        &mut ws,
                        circuit,
                        opts,
                        t + h,
                        h,
                        coeffs,
                        &state,
                        opts.gmin,
                        cmp_smooth,
                    ) {
                        converged = true;
                        break;
                    }
                }
                ws.symbolic.set_allow_dynamic(had_dyn);
                converged
            } else {
                false
            };

            if !converged {
                // Cut the step hard and retry.
                failed_steps += 1;
                let over_budget = opts.max_failed_steps.is_some_and(|n| failed_steps > n);
                if h <= dt_min * 1.0001 || over_budget {
                    // Refuse loudly with the cause (a behavioral fault names
                    // its device), never emit a truncated waveform (exit-3
                    // discipline at the CLI).
                    let why = if over_budget {
                        format!("Newton failed {failed_steps} times by t={t} (march budget)")
                    } else {
                        format!("Newton failed at t={t} even at dt_min={dt_min}")
                    };
                    return Err(ws.failure(
                        circuit,
                        why,
                        SolvePhase::Transient,
                        Some(t),
                        Some(h),
                        None,
                    ));
                }
                dt = (h * 0.25).max(dt_min);
                continue;
            }

            // Event check: did a comparator/switch control cross threshold?
            // SKIP when this step was resolved by the event-freeze loop: that
            // Gauss-Seidel already handled the discrete crossing CONSISTENTLY
            // (every comparator + switch state re-derived to a fixed point with
            // break-before-make). Bisecting toward the crossing here would throw
            // away that converged solution and re-seed a mid-transition step the
            // bare Newton can't solve; the exact dt_min thrash that re-opened the
            // wall. With the event loop owning the discontinuity, accept the step.
            if !used_event && adaptive {
                let latch = latch0.as_ref().map(|l0| (l0, ws.switch_latch()));
                let xs = (x_accepted.as_slice(), ws.x.as_slice());
                let cmp_latch = latched.then_some(&state);
                if let Some(frac) = crossing_fraction(
                    circuit,
                    xs,
                    &ws.layout_nodes(),
                    (latch, cmp_latch),
                    &stepped,
                ) {
                    if h > dt_min * 4.0 {
                        // Bisect toward the crossing for a sharper edge.
                        let refined = (h * frac).clamp(dt_min, h);
                        if (refined - h).abs() > dt_min {
                            dt = refined;
                            continue;
                        }
                    }
                }
            }

            // Also skip LTE rejection for an event-resolved step: the reactive
            // curvature across a comparator/switch flip is huge (the pulse edge),
            // so the LTE estimate would reject and shrink dt forever right at the
            // event the loop just resolved. Accept the event step (the event loop
            // converged it to a true root at t+h); the next ordinary step resumes
            // normal LTE control. Only the event-resolved step is exempted.

            // LTE control for adaptive stepping.
            let accept;
            let mut next_dt = dt;
            match opts.step {
                _ if used_event => {
                    // Event-resolved step: accept, and set the next step to a
                    // MODERATE size, not a fraction of h. The flip step is often
                    // reached at a tiny h (the bare-Newton step-cut shrank dt while
                    // approaching the discontinuity); continuing at h*0.5 would
                    // leave dt microscopic, and a microscopic dt makes the reactive
                    // companion conductance (C/dt, e.g. a nF-scale output integrator /
                    // dt) astronomically stiff -- which is exactly what makes the
                    // FOLLOW-ON step's Newton singular and re-opens the wall right
                    // after the reset switch closes. Resume at ~the post-flip
                    // physical time constant (the integrator discharge RC
                    // through the switch on-resistance), tracked but not microscopic; LTE
                    // grows it back once the fast tail passes. Floored to a sane
                    // value, never below half the current h (so a genuinely fine
                    // flip step doesn't get coarsened).
                    accept = true;
                    let resume = (h * 0.5).max(1e-7);
                    next_dt = resume.clamp(dt_min, dt_max);
                }
                StepControl::Fixed { .. } => accept = true,
                StepControl::Adaptive { .. } => {
                    let err = lte_estimate(circuit, &ws, &state, h, h_prev_accepted, opts);
                    if err <= 1.0 || h <= dt_min * 1.0001 {
                        accept = true;
                        if bp_landing {
                            // Breakpoint-truncated accept: the trial h was cut
                            // to the sliver `bp - t`, so its LTE is measured on
                            // the sliver and (err small) the growth factor
                            // saturates at 2.0, recomputing next_dt from this
                            // h would collapse dt to ~2x the sliver and force
                            // a geometric regrow after EVERY corner. The
                            // controller's dt was tuned on full-size steps and
                            // remains its honest target, so keep it (next_dt
                            // is initialized to dt above): a corner costs
                            // exactly one exact landing, as the truncation
                            // comment promises. A rejected landing still
                            // shrinks from the truncated h below; the error
                            // there WAS measured on that h.
                        } else {
                            // Grow/shrink for next step from the error ratio.
                            let safety = 0.9;
                            let factor = if err > 0.0 {
                                (safety * err.powf(-1.0 / 3.0)).clamp(0.5, 2.0)
                            } else {
                                2.0
                            };
                            next_dt = (h * factor).clamp(dt_min, dt_max);
                        }
                    } else {
                        accept = false;
                        let factor = (0.9 * err.powf(-1.0 / 3.0)).clamp(0.1, 0.9);
                        next_dt = (h * factor).max(dt_min);
                    }
                }
            }

            if !accept {
                dt = next_dt;
                continue;
            }

            // Measure the equation residual only at the final accepted point.
            // Doing this at every timestep would add a full nonlinear stamp to
            // the solver's hot loop; at the final point it is exact and costs
            // one diagnostic stamp per result.
            if t + h >= tstop - f64::max(1e-18, tstop.abs() * 1e-12) {
                final_residual =
                    Some(ws.transient_residual_argmax(circuit, opts, t + h, coeffs, &state));
            }

            // Accept: advance time, update reactive history, emit.
            t += h;
            let flips = advance_reactive_state(
                &mut state,
                circuit,
                &ws,
                h,
                opts,
                first_step || restart_order,
            );
            let cmp_flipped = flips.any;
            restart_order = latched && flips.stepped;
            if restart_order {
                state.restart_derivatives();
            }
            if first_step {
                ws.clear_junction_entries();
            }
            if predictor_armed {
                match x_accepted_prev.as_mut() {
                    Some(p) => p.copy_from_slice(&x_accepted),
                    None => x_accepted_prev = Some(x_accepted.clone()),
                }
            }
            // Skip the extrapolation seed after any accepted step that crossed a
            // discontinuity: an event-resolved step, or a bare step whose
            // hysteretic switch latch flipped. The accepted pair straddles the
            // flip either way, so a line through them launches the fast nodes
            // far past the new branch (measured on the mirror deck: the
            // diode-connected base extrapolated from 5 mV to 4.8 V, seeding a
            // 238 A junction imbalance that no dt cut could recover, because
            // every retry re-extrapolated the same line).
            pred_skip_once = used_event || ws.latch_flipped() || (latched && cmp_flipped);
            x_accepted.copy_from_slice(&ws.x);
            first_step = false;
            h_prev_accepted = h;
            dt = next_dt;
            // A comparator that just decided starts its delayed transition:
            // open at a quarter of the time to its first corner rather than at
            // the sliver the crossing landing left (the corner landing above
            // bounds the step, LTE still rejects one that is too long).
            if latched && cmp_flipped && adaptive {
                if let Some(left) = next_output_corner(&comparators, &state) {
                    dt = dt.max(0.25 * left).clamp(dt_min, dt_max);
                }
            }
            sink(StepSample {
                time: t,
                x: &ws.x,
                stand_in: false,
            });
        }
        Ok(TransientDiagnostics {
            final_residual,
            switch_latch: ws.switch_latch().cloned(),
            operating_point,
            carry: Some(MarchCarry {
                switch_latch: ws.switch_latch().cloned(),
                staged,
                reactive: state,
            }),
        })
    }
}

// --- reactive state bookkeeping ---------------------------------------------

/// One reactive state a device carries at the solution in `ws`.
struct Reactive {
    /// `DeviceId.0`.
    dev: usize,
    /// History bank: 0 is the primary, 1..=3 the secondary banks (the
    /// [`ReactiveState`] packing table).
    bank: usize,
    /// The state's value at the solution: capacitor voltage, inductor current,
    /// or a junction charge.
    value: f64,
    /// The device's declared initial condition, which the seed prefers.
    ic: Option<f64>,
    /// Absolute floor of the state's LTE tolerance.
    atol: f64,
}

/// The time to the nearest corner of any latched comparator's output
/// trajectory in `comparators` (id, `tpd_s`); see
/// [`ComparatorLatch::next_corner`].
fn next_output_corner(comparators: &[(DeviceId, f64)], state: &ReactiveState) -> Option<f64> {
    comparators
        .iter()
        .filter_map(|&(id, tpd_s)| ComparatorLatch::of(state, id)?.next_corner(tpd_s))
        .reduce(f64::min)
}

/// Visit every reactive state of every device at the solution in `ws`, in
/// device order, through the same model code and junction-voltage rules the
/// stamp uses. A charge-storing diode / BJT / MOSFET participates through its
/// CHARGES (the state its companion integrates, `chgtol` as the floor: SPICE's
/// classic charge-based LTE); a charge-free one carries no state and is
/// skipped, so charge-free decks' step sequences are untouched.
fn for_each_reactive(
    circuit: &Circuit,
    ws: &Workspace,
    opts: &SolverOptions,
    mut f: impl FnMut(Reactive),
) {
    let effects = &opts.effects;
    for (id, dev) in circuit.iter() {
        let dev_i = id.0 as usize;
        let mut push = |bank: usize, value: f64, ic: Option<f64>, atol: f64| {
            f(Reactive {
                dev: dev_i,
                bank,
                value,
                ic,
                atol,
            });
        };
        match dev {
            Device::Capacitor { a, b, ic, .. } => push(
                0,
                node_v(ws, *a) - node_v(ws, *b),
                *ic,
                opts.chgtol.max(opts.vntol),
            ),
            // The floor is SPICE's `chgtol` on the FLUX `L*i`, as for the
            // junction charges: a 0.6 nH ESL whose current carries ~1e-8 A of
            // step-to-step solve noise holds 6e-18 Wb of it, which a 1e-9 A
            // current floor read as curvature at every step (Olimex EVB).
            Device::Inductor { ic, henries, .. } => push(
                0,
                branch_i(ws, id),
                *ic,
                opts.abstol.max(1e-9).max(opts.chgtol / henries),
            ),
            Device::Diode { a, k, model, .. } if crate::stamp::diode_has_charge(model, effects) => {
                push(
                    0,
                    diode_q(model, diode_vj(ws, id, *a, *k), opts),
                    None,
                    opts.chgtol,
                );
            }
            // Bank A = Q_be, xb[0] = Q_bc.
            Device::Bjt { c, b, e, model, .. } if crate::stamp::bjt_has_charge(model, effects) => {
                let q: [f64; 2] = bjt_q(ws, id, *c, *b, *e, model, opts).into();
                for (bank, q) in q.into_iter().enumerate() {
                    push(bank, q, None, opts.chgtol);
                }
            }
            // Bank A = Q_gs, xb[0] = Q_gd, xb[1] = Q_bd, xb[2] = Q_bs.
            Device::Mosfet {
                d, g, s, b, model, ..
            } if crate::stamp::mos_has_charge(model, effects) => {
                let q: [f64; 4] = mos_q(ws, id, [*d, *g, *s], *b, model, opts).into();
                for (bank, q) in q.into_iter().enumerate() {
                    push(bank, q, None, opts.chgtol);
                }
            }
            _ => {}
        }
    }
}

/// An op-amp's RAW (unclipped) ideal output target at the solution in `ws`, with
/// its rails; `None` for any other device. Unclipped because the pole lags the
/// raw target and only then saturates ([`crate::stamp::opamp_transient_output`]);
/// callers wanting the DC point clamp it. Op-amps with output dynamics
/// (`pole_hz/slew`) keep the drive EMF as their state; ideal ones never read it.
fn opamp_target(ws: &Workspace, dev: &Device) -> Option<(f64, f64, f64)> {
    let Device::OpAmp {
        inp,
        inn,
        reference,
        gain,
        rail_lo,
        rail_hi,
        ..
    } = dev
    else {
        return None;
    };
    let vref = reference.map(|n| node_v(ws, n)).unwrap_or(0.0);
    let raw = vref + gain * (node_v(ws, *inp) - node_v(ws, *inn));
    let (lo, hi) = hauksbee_ir::Rail::band(*rail_lo, *rail_hi, |n| node_v(ws, n));
    Some((raw, lo, hi))
}

/// At the operating point every state is flat: capacitor voltage = node
/// difference, inductor current = its branch current, junction charge
/// `Q(v)` at the operating-point junction voltage, derivatives zero. This is
/// the single place a device's own `ic` is read and preferred over the
/// node-derived value, which is exactly what makes `uic` honour `IC=`; with
/// `ws.x` at rest and no `ic` anywhere it reproduces the all-zero
/// construction value. The op-amp EMF seeds at the DC point's clipped target:
/// a single pole passes DC unattenuated, so the filtered EMF IS the target.
pub fn seed_reactive_state(
    state: &mut ReactiveState,
    circuit: &Circuit,
    ws: &Workspace,
    opts: &SolverOptions,
) {
    for_each_reactive(circuit, ws, opts, |r| {
        state.seed(r.bank, r.dev, r.ic.unwrap_or(r.value));
    });
    for (id, dev) in circuit.iter() {
        if let Some((raw, lo, hi)) = opamp_target(ws, dev) {
            state.seed(0, id.0 as usize, raw.clamp(lo, hi));
        }
        // A comparator's latch (`ComparatorLatch`) starts where the
        // operating point's output stands, pinned or decided: its position
        // across the swing, and the side of mid-swing it is on.
        if let Device::Comparator {
            out,
            out_lo,
            out_hi,
            supply,
            ..
        } = dev
        {
            let (lo, hi) = crate::stamp::comparator_swing_volts(
                |n| node_v(ws, n),
                (*out_lo, *out_hi),
                *supply,
            );
            let v = node_v(ws, *out);
            let frac = if hi > lo {
                ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ComparatorLatch::settled(frac > 0.5, frac).store(state, id);
        }
    }
}

/// Start every diode that the staged DC's adopted relaxed point leaves
/// forward-biased as an unbiased junction, 0 V: its stored charge, and the
/// anchor its first step's junction limiting climbs from.
///
/// That point solved each diode as a 1 GΩ stand-in, which is a sound model
/// of a reverse-biased or unbiased junction and of nothing else: a junction
/// it leaves forward-biased carried nanoamps there, not what its own equation
/// gives at that voltage. Read through the full model, a stretch diode the
/// stand-in held 4.95 V forward stores `tt * Is * e^100`, ~1e26 C, and the
/// first step's companion history turns that into a ~1e31 V Newton step
/// (measured on a comparator-fed pulse stretcher). The power-on premise the
/// staged DC adopts that point under is that such a junction has not yet
/// conducted, so it starts uncharged and the march charges it. Its limiting
/// starts there too: anchored on the seed's own 4.95 V, pnjlim sees no step
/// and Newton walks the exponential down from above one `n*Vt` per
/// iteration, too slowly for any step to converge. Reverse-biased junctions
/// keep their seed voltage and the depletion charge it implies, which the
/// stand-in does model. The anchors last until the first accepted step.
fn start_forward_junctions_uncharged(
    state: &mut ReactiveState,
    circuit: &Circuit,
    ws: &mut Workspace,
    opts: &SolverOptions,
) {
    for (id, dev) in circuit.iter() {
        if let Device::Diode { a, k, model, .. } = dev {
            if diode_vj(ws, id, *a, *k) > 0.0 {
                ws.enter_junction_at(id, 0.0);
                if crate::stamp::diode_has_charge(model, &opts.effects) {
                    state.seed(0, id.0 as usize, diode_q(model, 0.0, opts));
                }
            }
        }
    }
}

/// Which comparators' latched decisions flipped on an accepted step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatchFlips {
    /// Some latched comparator re-decided.
    pub any: bool,
    /// One without a propagation delay did, so its output STEPS at the start
    /// of the next step: a discontinuity a multi-step integration rule must
    /// restart across.
    pub stepped: bool,
}

/// After an accepted step, roll every history forward (see
/// [`ReactiveState::roll`]). The op-amp EMF rolls through the SAME single-pole +
/// slew update the stamp used this step (`opamp_transient_output` is shared
/// code), from the frozen previous EMF and the ACCEPTED inputs, so at
/// convergence it reproduces exactly the EMF the final Newton iteration
/// stamped; updated on acceptance only, never mid-Newton.
///
/// Returns which comparators' latched decisions flipped on this step.
pub fn advance_reactive_state(
    state: &mut ReactiveState,
    circuit: &Circuit,
    ws: &Workspace,
    h: f64,
    opts: &SolverOptions,
    first: bool,
) -> LatchFlips {
    let mut flipped = LatchFlips::default();
    // The derivative must be backed out with whatever rule actually ran this
    // step. A multi-step rule (trapezoidal) falls back to backward Euler on the
    // very first step, so the derivative there is the BE one.
    let trapz = opts.integration == Integration::Trapezoidal && !first;
    for_each_reactive(circuit, ws, opts, |r| {
        state.roll(r.bank, r.dev, r.value, h, trapz);
    });
    for (id, dev) in circuit.iter() {
        if let Device::Comparator {
            inp,
            inn,
            hysteresis,
            tpd_s,
            ..
        } = dev
        {
            // The accepted step moved the output along its slew, and the
            // accepted inputs decide the side the next step slews toward
            // (`ComparatorLatch`).
            if let Some(latch) = ComparatorLatch::of(state, id) {
                let d = node_v(ws, *inp) - node_v(ws, *inn);
                let (latch, decided) = latch.advanced(*tpd_s, h, d, *hysteresis);
                flipped.any |= decided;
                flipped.stepped |= decided && *tpd_s <= 0.0;
                latch.store(state, id);
            }
            continue;
        }
        let Device::OpAmp { pole_hz, slew, .. } = dev else {
            continue;
        };
        let i = id.0 as usize;
        let (target, lo, hi) = opamp_target(ws, dev).expect("an op-amp has a target");
        if let Some((v_out, _)) =
            crate::stamp::opamp_transient_output(state.x1[i], target, *pole_hz, *slew, (lo, hi), h)
        {
            state.x2[i] = state.x1[i];
            state.x1[i] = v_out;
            state.dx1[i] = 0.0;
        }
    }
    flipped
}

/// MOSFET stored charges `(Q_gs, Q_gd, Q_bd, Q_bs)` at the solution in `ws`,
/// through the same model code and junction-voltage rule the stamp uses.
fn mos_q(
    ws: &Workspace,
    id: DeviceId,
    dgs: [NodeId; 3],
    b: Option<NodeId>,
    model: &hauksbee_ir::MosfetModel,
    opts: &SolverOptions,
) -> (f64, f64, f64, f64) {
    let sr = opts.effects.series_resistance;
    let (vgs, vgd, vbd, vbs) =
        crate::stamp::mos_junction_voltages(&ws.layout, &ws.x, id, dgs, b, model, sr);
    crate::stamp::mos_charges_at(
        model,
        vgs,
        vgd,
        vbd,
        vbs,
        opts.model_temp(),
        opts.effects.temperature,
    )
}

/// Diode stored charge at junction voltage `vd` (the companion's state
/// variable), evaluated through the same model code the stamp uses.
fn diode_q(model: &hauksbee_ir::DiodeModel, vd: f64, opts: &SolverOptions) -> f64 {
    let (idc, gd) =
        crate::stamp::diode_eval(model, vd, opts.model_temp(), opts.effects.temperature);
    crate::stamp::diode_charge(model, vd, idc, gd).0
}

/// BJT stored charges `(Q_be, Q_bc)` at the solution in `ws`, through the same
/// model code and node-resolution rule the stamp uses (folded junction
/// voltages, measured at the intrinsic nodes when series resistance applies).
fn bjt_q(
    ws: &Workspace,
    id: DeviceId,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    model: &hauksbee_ir::BjtModel,
    opts: &SolverOptions,
) -> (f64, f64) {
    let (vbe, vbc) =
        crate::stamp::bjt_junction_voltages(&ws.layout, &ws.x, id, c, b, e, model, &opts.effects);
    crate::stamp::bjt_charges_at(model, vbe, vbc, opts.model_temp(), opts.effects.temperature)
}

/// A diode's JUNCTION voltage, which is not its terminal voltage when the model
/// carries a series resistance: the junction sits on the intrinsic anode and
/// `rs` bridges it out. The charge state has to be integrated against the same
/// voltage the stamp linearised, or the co-simulation stops converging rather
/// than merely drifting.
#[inline]
pub fn diode_vj(ws: &Workspace, id: DeviceId, a: NodeId, k: NodeId) -> f64 {
    let anode = ws
        .layout
        .diode_internal(id)
        .map_or_else(|| node_v(ws, a), |i| ws.x[i]);
    anode - node_v(ws, k)
}

pub fn node_v(ws: &Workspace, node: NodeId) -> f64 {
    ws.layout.node(node).map_or(0.0, |i| ws.x[i])
}

/// A device's branch current at the solution in `ws` (0 when it owns none).
pub fn branch_i(ws: &Workspace, id: DeviceId) -> f64 {
    ws.layout.branch(id).map_or(0.0, |br| ws.x[br])
}

// --- LTE estimate -----------------------------------------------------------

/// Dimensionless local-truncation-error norm (>1 means reject): a TRUE
/// divided-difference curvature estimate over the three newest samples,
/// normalized by `reltol*|x| + atol`.
///
/// Why divided differences and not the raw second difference: the samples are
/// an adaptive (NON-uniform) grid, and the raw `x_new - 2*x1 + x2` is nonzero
/// for a perfectly LINEAR trajectory whenever the step size changes (it equals
/// `slope * (h - h_prev)`), so every step-size change on a charging slope
/// manufactured fake curvature: on a stiff adaptive march that alone
/// LTE-rejected 44% of all trial solves, independently of how the step was
/// proposed. The divided-difference second
/// derivative `dd2` is exact-zero on linear trajectories, reduces to the SAME
/// `x_new - 2*x1 + x2` on a uniform grid (h == `h_prev`), and `h^2 * dd2` is
/// the classic uniform-grid curvature the 1/12 trapezoidal coefficient was
/// calibrated against.
///
/// `h_prev` is the previous ACCEPTED step (the spacing of x1..x2 in the
/// reactive history); 0.0 means "no history yet" (first trial), which falls
/// back to `h`, which is the uniform-grid estimate (the seeded history has
/// x2 == x1, so the `h_prev` term vanishes).
fn lte_estimate(
    circuit: &Circuit,
    ws: &Workspace,
    state: &ReactiveState,
    h: f64,
    h_prev: f64,
    opts: &SolverOptions,
) -> f64 {
    let hp = if h_prev > 0.0 { h_prev } else { h };
    let mut worst = 0.0f64;
    for_each_reactive(circuit, ws, opts, |r| {
        let (x1, x2) = state.bank(r.bank);
        let (x_new, x1, x2) = (r.value, x1[r.dev], x2[r.dev]);
        let dd2 = 2.0 * ((x_new - x1) / h - (x1 - x2) / hp) / (h + hp);
        let curv = (h * h * dd2).abs();
        let tol = opts.reltol.mul_add(x_new.abs().max(x1.abs()), r.atol);
        worst = worst.max((curv / 12.0) / tol.max(1e-30));
    });
    worst
}

// --- source breakpoints ------------------------------------------------------

/// Collect the time-domain corner points of every independent source: PWL
/// vertices and PULSE edge corners. The step controller must never stride
/// across one of these without landing on it.
///
/// Why this exists: the adaptive controller's LTE estimator is a second
/// difference over ACCEPTED steps, so it only sees dynamics the steps already
/// sampled. A sub-step stimulus (a 1 us PWL pulse arriving after a long quiet
/// stretch during which dt grew to hundreds of microseconds) produces
/// identical endpoints and no curvature signal: the pulse is silently aliased
/// away (the co-sim face of the same failure is a pulse narrower than one
/// co-sim chunk, which the analog side never resolves at all). Registering
/// source corners as mandatory step landings is the classic SPICE cure ("the
/// breakpoint table"), and is what keeps the co-sim's PWL edge drive honest
/// on the analog side.
///
/// PULSE corner enumeration is capped: a fast periodic source over a long
/// window enumerates corners only up to [`MAX_BREAKPOINTS`]; beyond the cap
/// the controller is left to its own rhythm (once locked onto a periodic
/// waveform the accepted-step history carries the curvature signal, so the
/// cap loses the guarantee only for the tail, not the lock-on). PWL vertices
/// and ramp kinks are finite by construction and are never truncated; they
/// are collected into a separate always-kept list ([`Corners`]) so the cap
/// counts only PULSE corners and a dense periodic source can never evict a
/// late PWL landing.
const MAX_BREAKPOINTS: usize = 100_000;

/// Corner times split by whether they may be truncated. Keeping the two kinds
/// apart is what lets the [`MAX_BREAKPOINTS`] cap bound only the unbounded
/// PULSE enumeration without ever evicting a finite PWL vertex, see
/// [`breakpoint_table`].
#[derive(Default)]
struct Corners {
    /// PWL vertices and ramp kinks, finite by construction, NEVER truncated.
    always: Vec<f64>,
    /// PULSE periodic corners, capped at [`MAX_BREAKPOINTS`] collectively.
    capped: Vec<f64>,
}

fn breakpoint_table(circuit: &Circuit, tstop: f64) -> Vec<f64> {
    let mut corners = Corners::default();
    for (_, dev) in circuit.iter() {
        let (Device::Vsource { kind, .. } | Device::Isource { kind, .. }) = dev else {
            continue;
        };
        push_source_corners(kind, tstop, &mut corners);
    }
    // Cap ONLY the PULSE-derived corners: a fast periodic source over a long
    // window enumerates unboundedly, and once locked onto a periodic waveform
    // the accepted-step history carries the curvature signal. The PWL vertices
    // and ramp kinks in `always` are the mandatory landings the co-sim's edge
    // drive depends on and are finite by construction, so a dense PULSE
    // elsewhere must never push a late PWL corner past the cap. One merged,
    // time-sorted list truncated at the cap does exactly that: it silently
    // drops a late PWL vertex sitting behind an earlier PULSE burst.
    corners.capped.truncate(MAX_BREAKPOINTS);
    let mut bps = corners.always;
    bps.append(&mut corners.capped);
    bps.sort_by(|a, b| a.partial_cmp(b).expect("breakpoints are finite"));
    // Dedup within a relative epsilon: two corners closer than the controller
    // could ever distinguish are one landing.
    bps.dedup_by(|a, b| (*a - *b).abs() <= f64::max(1e-18, *b * 1e-12));
    bps
}

/// Push one source's time-domain corners into `bps` (times strictly inside
/// `(0, tstop)`). Recursive so a `Ramped` envelope contributes both its own
/// full-amplitude corner at `scale_to` AND every corner of the inner source.
fn push_source_corners(kind: &hauksbee_ir::SourceKind, tstop: f64, corners: &mut Corners) {
    use hauksbee_ir::SourceKind;
    let in_window = |t: f64| t > 0.0 && t < tstop;
    match kind {
        SourceKind::Pwl(points) => {
            // PWL vertices are always-kept (finite by construction).
            for pt in points {
                if in_window(pt.t) {
                    corners.always.push(pt.t);
                }
            }
        }
        SourceKind::Pulse {
            delay,
            rise,
            fall,
            width,
            period,
            ..
        } => {
            // Corners per period: leading edge start/end, trailing edge
            // start/end. Zero-length edges still get their corner (the
            // discontinuity itself is the thing to land on). Capped: the cap
            // now counts only PULSE corners, so PWL vertices never contend for
            // the budget.
            let step = if *period > 0.0 { *period } else { tstop };
            let one_shot = *period <= 0.0;
            let mut t0 = *delay;
            while t0 < tstop && corners.capped.len() < MAX_BREAKPOINTS {
                for t in [t0, t0 + rise, t0 + rise + width, t0 + rise + width + fall] {
                    if in_window(t) {
                        corners.capped.push(t);
                    }
                }
                if one_shot {
                    break;
                }
                t0 += step;
            }
        }
        SourceKind::Ramped { scale_to, inner } => {
            // The ramp's own kink at full amplitude (always-kept), then the
            // inner's corners (the inner is not time-shifted, so its corner
            // times are unchanged).
            if in_window(*scale_to) {
                corners.always.push(*scale_to);
            }
            push_source_corners(inner, tstop, corners);
        }
        SourceKind::Dc(_) | SourceKind::Sin { .. } => {}
    }
}

// --- event detection --------------------------------------------------------

/// Each hysteretic switch's latched-closed state, keyed by device.
type SwitchLatch = HashMap<DeviceId, bool>;

/// If a comparator or relay control crossed the threshold LIVE at the accepted
/// point `x0`, or the device switched, on the way to the trial `x1`: the
/// approximate fraction of the step at which it crossed (for bisection).
///
/// Live, not central: a hysteretic device only acts at the edge of its band on
/// the side it is leaving, so aiming at the band centre put a sample where
/// nothing happens and left the real edge on the step grid (measured: an
/// adaptive march capped at 100 us flipped a 0.25 V-band comparator at 0.7 ms
/// against a 0.625 ms trip). A comparator's side is its output rail, exactly as
/// `stamp_comparator` decides it. A hysteretic relay's is its latch, `latch`
/// holding the one the step ENTERED with and the one the trial left; `None` is
/// the memoryless smooth switch, located at its centred ramp's midpoint. An
/// event is the control crossing that trip OR the device changing state: Newton
/// can converge on an iterate whose comparator has not yet re-read its moved
/// input, leaving the control a hair past the trip at the accepted point, and
/// the flip then arrives in a step whose control never crosses (measured: a
/// falling edge landed one 0.1 ns step late). Such a step is cut back to the
/// accepted point (`frac` clamps to 0), so the flip lands there instead.
///
/// A control driven by a comparator output (`stepped`) is skipped: it is a
/// rail-to-rail step, so every trial straddling it interpolates to the same
/// fraction and bisection halves `h` down to `dt_min` (measured: 1e-17 s on
/// an integrate-and-fire loop). Its edge is the driving comparator's, which
/// this locates on that comparator's own continuous input.
fn crossing_fraction(
    circuit: &Circuit,
    (x0, x1): (&[f64], &[f64]),
    node_idx: &dyn Fn(NodeId) -> Option<usize>,
    (latch, cmp_latch): (
        Option<(&SwitchLatch, Option<&SwitchLatch>)>,
        Option<&ReactiveState>,
    ),
    stepped: &[NodeId],
) -> Option<f64> {
    let mut earliest: Option<f64> = None;
    let vat = |x: &[f64], n: NodeId| node_idx(n).map_or(0.0, |i| x[i]);
    for (id, dev) in circuit.iter() {
        let (cp, cn, trip, flipped) = match dev {
            Device::Comparator {
                out,
                inp,
                inn,
                out_lo,
                out_hi,
                hysteresis,
                ..
            } => {
                // A latched comparator holds its decision across the step
                // (`ComparatorLatch`): its trip is that decision's,
                // and its output slewing past mid-swing is not an event.
                cmp_latch
                    .and_then(|s| ComparatorLatch::of(s, id))
                    .map_or_else(
                        || {
                            let high = |x: &[f64]| vat(x, *out) > 0.5 * (out_lo + out_hi);
                            let trip = crate::stamp::comparator_trip(high(x0), *hysteresis);
                            (*inp, *inn, trip, high(x0) != high(x1))
                        },
                        |held| {
                            let trip = crate::stamp::comparator_trip(held.high, *hysteresis);
                            (*inp, *inn, trip, false)
                        },
                    )
            }
            Device::VSwitch {
                ctrl_p,
                ctrl_n,
                von,
                voff,
                ..
            } => {
                let on = |l: Option<&SwitchLatch>| l.and_then(|l| l.get(&id)) == Some(&true);
                match latch {
                    Some((l0, l1)) if on(Some(l0)) => (*ctrl_p, *ctrl_n, *voff, !on(l1)),
                    Some((_, l1)) => (*ctrl_p, *ctrl_n, *von, on(l1)),
                    None => (*ctrl_p, *ctrl_n, 0.5 * (von + voff), false),
                }
            }
            _ => continue,
        };
        if [cp, cn].iter().any(|n| stepped.contains(n)) {
            continue;
        }
        let d0 = vat(x0, cp) - vat(x0, cn) - trip;
        let d1 = vat(x1, cp) - vat(x1, cn) - trip;
        if flipped || ((d0 > 0.0) != (d1 > 0.0) && (d1 - d0).abs() > 1e-15) {
            let frac = if d0 - d1 == 0.0 {
                0.0
            } else {
                (d0 / (d0 - d1)).clamp(0.0, 1.0)
            };
            earliest = Some(earliest.map_or(frac, |e| e.min(frac)));
        }
    }
    earliest
}

impl Workspace {
    /// A node->unknown index closure for event checks.
    fn layout_nodes(&self) -> impl Fn(NodeId) -> Option<usize> + '_ {
        move |n: NodeId| self.layout.node(n)
    }
}

#[cfg(test)]
mod breakpoint_cap_tests {
    use super::{breakpoint_table, MAX_BREAKPOINTS};
    use hauksbee_ir::{Circuit, Device, NodeId, PwlPoint, SourceKind};

    /// R5 regression: a late PWL vertex must survive even when a fast PULSE
    /// source enumerates far more than `MAX_BREAKPOINTS` corners ahead of it.
    /// One shared list truncated at the cap keeps only the earliest 100k
    /// times across ALL sources, so the PWL landing at 0.9 s is silently
    /// dropped behind the PULSE burst that fills the cap in the first ~25 ms,
    /// exactly the stride-over-a-pulse aliasing the breakpoint table exists
    /// to prevent.
    #[test]
    fn pwl_vertex_survives_a_cap_exhausting_pulse() {
        let mut c = Circuit::new();
        let a = c.node("a");
        let b = c.node("b");
        // 1 us period over a 1 s window: ~4M corners, so the cap fills within
        // the first ~25 ms and no PULSE corner past that is enumerated.
        c.add(Device::Vsource {
            name: "Vpulse".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Pulse {
                v1: 0.0,
                v2: 5.0,
                delay: 0.0,
                rise: 1e-9,
                fall: 1e-9,
                width: 0.5e-6,
                period: 1e-6,
            },
        });
        // A single sharp PWL vertex at 0.9 s, long after the cap-filling burst.
        c.add(Device::Vsource {
            name: "Vpwl".into(),
            p: b,
            n: NodeId::GROUND,
            kind: SourceKind::Pwl(vec![
                PwlPoint { t: 0.0, v: 0.0 },
                PwlPoint { t: 0.9, v: 5.0 },
                PwlPoint {
                    t: 0.9 + 1e-6,
                    v: 0.0,
                },
            ]),
        });
        let bps = breakpoint_table(&c, 1.0);
        assert!(
            bps.iter().any(|&t| (t - 0.9).abs() < 1e-12),
            "the late PWL vertex at 0.9 s must be in the breakpoint table, \
             not evicted by the earlier PULSE burst"
        );
        // The PULSE corners are still bounded (the cap did its job).
        assert!(
            bps.len() <= MAX_BREAKPOINTS + 8,
            "pulse corners stay capped, got {} breakpoints",
            bps.len()
        );
    }
}

#[cfg(test)]
mod branch_output_tests {
    use super::Transient;
    use crate::{DcInit, SolverOptions};
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind, SpiceLoader};

    /// The reported branch current of a Vsource/Inductor must be read from that
    /// device's OWN branch unknown, not a running offset over the
    /// Vsource/Inductor-only list. A VCVS (which also owns a branch) defined
    /// BEFORE the inductor in device order shifts every later branch under
    /// such an offset, so I(L1) reads the VCVS's slot (~0 A) instead of the
    /// real inductor current.
    #[test]
    fn inductor_current_correct_with_a_vcvs_ahead_of_it() {
        let mut c = Circuit::new();
        let inn = c.node("in");
        let b = c.node("b");
        let cc = c.node("c");
        let a = c.node("a");
        // V1 (branch), R1: 1 V through 1k into node b.
        c.add(Device::vsource(
            "V1",
            inn,
            NodeId::GROUND,
            SourceKind::Dc(1.0),
        ));
        c.add(Device::resistor("R1", inn, b, 1000.0));
        // A dependent voltage source (VCVS) that owns a branch, defined BEFORE L1
        // so it interleaves ahead of the inductor's branch unknown.
        c.add(Device::Vcvs {
            name: "E1".into(),
            p: cc,
            n: NodeId::GROUND,
            cp: a,
            cn: NodeId::GROUND,
            gain: 2.0,
        });
        c.add(Device::resistor("Rc", cc, NodeId::GROUND, 1000.0));
        c.add(Device::resistor("Ra", a, NodeId::GROUND, 1000.0));
        // The inductor whose current we check: b -> ground.
        c.add(Device::inductor("L1", b, NodeId::GROUND, 1e-3));

        let wf = Transient::new(SolverOptions::default())
            .run(&c, 1e-3)
            .unwrap();
        let (_, l1) = wf
            .branch_currents
            .iter()
            .find(|(n, _)| n == "L1")
            .expect("L1 branch current reported");
        let i_final = l1.last().copied().unwrap();
        // Steady state: inductor is a short, so I(L1) = V1/R1 = 1 mA (magnitude).
        assert!(
            (i_final.abs() - 1e-3).abs() < 5e-5,
            "I(L1) must be ~1 mA (read from its own branch), got {i_final:e}"
        );
    }

    /// A continuation opens from its seed, so a DC point it cannot reach does
    /// not stop it: 1 mA * (1 + V^2) into 1 uF has no DC root (it only ever
    /// charges), yet 10 us from a 0 V seed is well posed, V = tan(t / 1 ms).
    #[test]
    fn a_continuation_marches_past_an_unreachable_dc_point() {
        let deck = "unreachable dc\nC1 n 0 1u\nB1 0 n I={1e-3*(1+V(n)^2)}\n.end\n";
        let c = SpiceLoader::load(deck).expect("deck loads");
        let opts = SolverOptions {
            dc_init: DcInit::Continue,
            ..SolverOptions::default()
        };
        let mut v = f64::NAN;
        let run = Transient::new(opts).run_streaming_seeded(&c, 1e-5, Some(&[0.0]), |s| v = s.x[0]);
        assert!(
            run.is_ok() && (v - 0.01f64.tan()).abs() < 1e-6,
            "{run:?}: V(10 us) = {v}"
        );
    }
}

#[cfg(test)]
mod relaxed_seed_tests {
    use super::{diode_q, start_forward_junctions_uncharged};
    use crate::newton::Workspace;
    use crate::options::SolverOptions;
    use crate::system::ReactiveState;
    use crate::test_fixtures::{diode, vdc};
    use hauksbee_ir::{Circuit, DiodeModel};

    /// From a relaxed seed (every diode a 1 GΩ stand-in), a junction the
    /// seed leaves forward-biased starts the march uncharged, its history at
    /// `Q(0)`, not at the charge its own equation gives at the seed's volts;
    /// a reverse-biased one keeps the history it had.
    #[test]
    fn a_forward_seed_junction_starts_uncharged_and_a_reverse_one_keeps_its_charge() {
        let mut c = Circuit::new();
        let (hi, mid) = (c.node("HI"), c.node("MID"));
        vdc(&mut c, "VHI", hi, 5.0);
        vdc(&mut c, "VMID", mid, 0.05);
        let model = DiodeModel {
            cjo: 1e-12,
            tt: 5e-9,
            ..DiodeModel::default()
        };
        let fwd = diode(&mut c, "Dfwd", hi, mid, model);
        let rev = diode(&mut c, "Drev", mid, hi, model);
        let opts = SolverOptions::default();
        let mut ws = Workspace::new(&c);
        ws.x[ws.layout.node(hi).unwrap()] = 5.0;
        ws.x[ws.layout.node(mid).unwrap()] = 0.05;
        if let Some(i) = ws.layout.diode_internal(fwd) {
            ws.x[i] = 5.0;
        }
        if let Some(i) = ws.layout.diode_internal(rev) {
            ws.x[i] = 0.05;
        }
        let mut state = ReactiveState::new(c.devices.len());
        for id in [fwd, rev] {
            state.seed(0, id.0 as usize, 1.0);
        }
        start_forward_junctions_uncharged(&mut state, &c, &mut ws, &opts);
        let x1 = state.bank(0).0;
        assert_eq!(
            x1[fwd.0 as usize].to_bits(),
            diode_q(&model, 0.0, &opts).to_bits()
        );
        assert_eq!(x1[rev.0 as usize].to_bits(), 1.0f64.to_bits());
    }
}
