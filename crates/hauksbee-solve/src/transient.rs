//! The transient driver: time-marching with companion models, adaptive step
//! control, and event-aware timestep refinement.
//!
//! Each accepted step solves a Newton operating point at `t + dt`, estimates
//! the local truncation error of the reactive elements, and either accepts the
//! step (advancing history) or shrinks `dt` and retries. Comparators and
//! switches are watched for threshold crossings; when one is straddled, the
//! step is bisected to land near the crossing so edges aren't smeared.

use crate::newton::{dc_operating_point_seeded, newton_solve, newton_solve_event, Workspace};
use crate::options::{DcInit, Integration, SolverOptions, StepControl};
use crate::stamp::IntegCoeffs;
use crate::system::ReactiveState;
use hauksbee_ir::{Circuit, Device, NodeId};

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
            if circuit.node_name(NodeId(id as u32)) == name {
                return self.node_voltages.get(id).map(Vec::as_slice);
            }
        }
        None
    }

    /// Last value of a named node.
    pub fn final_node(&self, circuit: &Circuit, name: &str) -> Option<f64> {
        self.node(circuit, name).and_then(|w| w.last().copied())
    }
}

/// A single accepted step, handed to streaming consumers.
#[derive(Debug, Clone)]
pub struct StepSample<'a> {
    pub time: f64,
    /// All unknowns: node voltages `[0..n_nodes]` then branch currents.
    pub x: &'a [f64],
}

/// The transient engine.
pub struct Transient {
    opts: SolverOptions,
}

impl Transient {
    /// New engine with the given options.
    pub fn new(opts: SolverOptions) -> Self {
        Transient { opts }
    }

    /// Run to `tstop`, collecting every accepted step into [`Waveforms`].
    pub fn run(&self, circuit: &Circuit, tstop: f64) -> Result<Waveforms, String> {
        let n_nodes = circuit.node_count();
        let mut wf = Waveforms {
            time: Vec::new(),
            node_voltages: vec![Vec::new(); n_nodes],
            branch_currents: Vec::new(),
        };
        // Pre-name branch current outputs.
        let mut branch_names = Vec::new();
        for (_, dev) in circuit.iter() {
            if matches!(dev, Device::Vsource { .. } | Device::Inductor { .. }) {
                branch_names.push(dev.name().to_string());
            }
        }
        for name in &branch_names {
            wf.branch_currents.push((name.clone(), Vec::new()));
        }

        self.run_streaming(circuit, tstop, |s| {
            wf.time.push(s.time);
            for node in 0..n_nodes {
                let v = if node == 0 { 0.0 } else { s.x[node - 1] };
                wf.node_voltages[node].push(v);
            }
            for (bi, slot) in wf.branch_currents.iter_mut().enumerate() {
                // Branch unknowns follow the node block in layout order.
                let idx = n_nodes - 1 + bi;
                slot.1.push(s.x.get(idx).copied().unwrap_or(0.0));
            }
        })?;
        Ok(wf)
    }

    /// Run to `tstop`, invoking `sink` with each accepted step. This is what the
    /// live UI will consume; it never buffers the whole waveform.
    pub fn run_streaming<F: FnMut(StepSample)>(
        &self,
        circuit: &Circuit,
        tstop: f64,
        sink: F,
    ) -> Result<(), String> {
        self.run_streaming_seeded(circuit, tstop, None, sink)
    }

    /// Like [`Transient::run_streaming`], but warm-starts the t=0 DC operating
    /// point from `dc_seed` (a prior operating point of the same circuit, e.g.
    /// the previous co-sim chunk's final unknowns). Lets a stiff nonlinear board
    /// skip the cold-start homotopy each chunk; exact (same root, fewer Newton
    /// iters), with a cold fallback when the seed does not fit or fails.
    pub fn run_streaming_seeded<F: FnMut(StepSample)>(
        &self,
        circuit: &Circuit,
        tstop: f64,
        dc_seed: Option<&[f64]>,
        mut sink: F,
    ) -> Result<(), String> {
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
                return pt.run_streaming(circuit, tstop, sink);
            }
        }

        let mut ws = Workspace::new(circuit);
        let n_dev = circuit.devices.len();

        // Seed t = 0. Normally a DC operating point (warm-started from the prior
        // chunk when a seed is supplied); under FromZero a power-on start with
        // no DC solve at all: x(0) = 0 and reactive history zeroed further down.
        let from_zero = opts.dc_init == DcInit::FromZero;
        // Known softness (review note): the staged-DC regularizers arm on
        // ws.used_staged_dc(), which only a DC solve can set, so a power-on
        // march runs the bare per-step path. A stiff board that needed the
        // regularized rescue fails honestly (the caller keeps its original
        // DC error); arming them under FromZero on nonlinear circuits is a
        // plausible improvement when the strategy ladder lands.
        if from_zero {
            // Power-on: the unknown vector rests at zero. No DC solve to fail;
            // the ramp (paired Ramped sources) integrates the state up from here.
            for v in ws.x.iter_mut() {
                *v = 0.0;
            }
        } else {
            // The DC solve manages its own staged regularizers internally and
            // leaves the workspace flag at 0.
            dc_operating_point_seeded(&mut ws, circuit, opts, dc_seed)?;
        }

        // Only when the DC point itself needed the staged fallback (the standard
        // plain+gmin+source ladder failed) is the board stiff enough that the
        // same reverse-biased-diode + cap-isolated-node degeneracy destabilizes
        // each transient step. Arm the staged regularizers (negligible branch
        // series R + node damping + node-block convergence) for the whole march
        // in that case only; an ordinary diode circuit (e.g. a rectifier) solved
        // on the normal ladder keeps the bit-identical classic transient path.
        let transient_dyn = std::env::var("HAUKSBEE_TRANSIENT_DYN").is_ok();
        if ws.used_staged_dc() || transient_dyn {
            // Arm the staged regularizers (negligible branch series R + node
            // damping + node-block convergence) AND the VSwitch/diode control
            // limiting (which is gated on branch_reg>0). On the synapse spike
            // path the per-step Newton fails right at the spike-gate switch flip
            // (a multi-decade conductance snap as a neuron V_out climbs through
            // the switch transition); the control limiting tracks the switch
            // through that transition. Armed when the DC needed staging OR when
            // the caller opts in (TRANSIENT_DYN) even on a cleanly-DC-solved but
            // dynamically-stiff march (the RAMP-from-rest spike case).
            ws.set_staged_branch_reg(1e-2);
            // Arm the per-step transient event-freeze retry: when a bare per-step
            // Newton fails (the spike-gate SPDT flip), retry the step with the
            // comparator+switch states frozen Gauss-Seidel + break-before-make
            // before cutting the timestep. Gated behind TRANSIENT_DYN (the
            // explicit spike-path opt-in) so an ordinary staged board (e.g. the
            // DAC-rail co-sim, which needs the staged regularizers but whose
            // per-step Newton converges on the bare path) keeps its exact prior
            // behaviour and timing -- the retry only fires for callers chasing
            // the spike transient. The retry itself is a no-op unless the bare
            // step actually fails, so even when armed it never changes a step
            // that already converged.
            if transient_dyn {
                ws.set_tran_event(true);
                // Arm the GLOBAL Armijo line-search in the per-step Newton on the
                // same stiff-board opt-in. It is the globalization for the
                // post-reset traveling fused-mesh limit cycle (a huge Newton
                // correction whose maxdV rotates across synapse-mesh / BJT-mirror
                // nodes, which per-node oscillation damping cannot catch). A no-op
                // on steps that already converge (alpha=1 full step), so it never
                // changes an already-good step; it only backtracks the overshoot.
                ws.set_tran_line_search(true);
            }
            // The dynamic re-pivot LU is enabled LOCALLY by the event-freeze retry
            // (newton_solve_event) only on the step that flips, so the bulk of
            // clean steps stay on the fast frozen path. HAUKSBEE_TRANSIENT_DYN=1
            // additionally forces it on for EVERY step (the old, slower lever),
            // kept for diagnosis; the default arming relies on the per-step retry.
            if std::env::var("HAUKSBEE_TRANSIENT_DYN_GLOBAL").is_ok() {
                ws.symbolic.set_allow_dynamic(true);
            }
        }

        let mut state = ReactiveState::new(n_dev);
        // Under FromZero the reactive history stays at its all-zero construction
        // value (x1 = x2 = 0, dx1 = 0 for every cap and inductor): the board
        // powers on from rest, ignoring any device initial conditions. Otherwise
        // seed it from the DC operating point as usual.
        if !from_zero {
            seed_reactive_state(&mut state, circuit, &ws);
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
        });

        let mut first_step = true;
        let mut x_accepted = ws.x.clone();
        let mut steps_taken: u64 = 0;
        let max_steps: u64 = 50_000_000; // safety valve

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

        while t < tstop - 1e-18 {
            steps_taken += 1;
            if steps_taken > max_steps {
                return Err(format!("exceeded step budget at t={t}"));
            }
            let mut h = dt.min(tstop - t);
            if h < dt_min {
                h = dt_min;
            }
            // Never stride across a source corner: shorten the trial step to
            // land EXACTLY on the next breakpoint. dt itself is not reduced,
            // so after the corner the controller resumes at its own rhythm.
            // The landing step may dip below dt_min (at most once per corner,
            // when the controller has already ground down near the corner):
            // dt_min is a floor against LTE-rejection thrash, not a sampling
            // contract, and an exact landing is the whole point of the table.
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
                    }
                }
            }

            // Trial solve at t + h.
            ws.x.copy_from_slice(&x_accepted);
            let coeffs = IntegCoeffs::for_step(opts.integration, h, first_step);
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
            let mut used_event = false;
            if !converged && ws.tran_event() {
                // The bare per-step Newton limit-cycled (the spike-gate SPDT flip
                // under synapse current, or the refractory reset). Retry this step
                // through the event-freeze loop: freeze comparator+switch states
                // per inner solve, re-derive Gauss-Seidel with break-before-make,
                // until consistent. Re-seed ws.x to the accepted point first so the
                // freeze derives from the step's entry state. The dynamic-pivot LU
                // is enabled for the inner solves so the diode-reshaped matrix stays
                // factorable.
                let had_dyn = ws.symbolic.allow_dynamic();
                ws.symbolic.set_allow_dynamic(true);
                // Two-mode retry. The smooth-comparator pass resolves the
                // membrane-crosses-UP fire (the output comparator's C_adapt
                // feedback needs a continuous transfer there); the frozen pass
                // resolves the refractory-reset discharge (the smooth comparator's
                // high gain re-couples the collapsing membrane into the
                // spike->switch loop and diverges, so the comparator must be
                // frozen like every other discrete state). The env var, when set,
                // forces the smooth mode FIRST (the historical default for the
                // FIRE step); either way the other mode is tried if the first
                // fails, so a single step is solved by whichever regime fits.
                let prefer_smooth = std::env::var("HAUKSBEE_TRAN_CMP_SMOOTH").is_ok();
                let modes = [prefer_smooth, !prefer_smooth];
                for &cmp_smooth in &modes {
                    ws.x.copy_from_slice(&x_accepted);
                    if newton_solve_event(
                        &mut ws, circuit, opts, t + h, h, coeffs, &state, opts.gmin, cmp_smooth,
                    ) {
                        converged = true;
                        break;
                    }
                }
                ws.symbolic.set_allow_dynamic(had_dyn);
                used_event = converged;
            }

            if !converged {
                // Cut the step hard and retry.
                if h <= dt_min * 1.0001 {
                    return Err(format!("Newton failed at t={t} even at dt_min={dt_min}"));
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
            // bare Newton can't solve — the exact dt_min thrash that re-opened the
            // wall. With the event loop owning the discontinuity, accept the step.
            if !used_event {
            if let Some(frac) = crossing_fraction(circuit, &x_accepted, &ws.x, &ws.layout_nodes()) {
                if matches!(opts.step, StepControl::Adaptive { .. }) && h > dt_min * 4.0 {
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
            // curvature across a comparator/switch flip is huge (the spike edge),
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
                    // companion conductance (C/dt, e.g. the 10 nF output membrane /
                    // dt) astronomically stiff -- which is exactly what makes the
                    // FOLLOW-ON step's Newton singular and re-opens the wall right
                    // after the refractory switch closes. Resume at ~the post-flip
                    // physical time constant (the membrane discharge RC ~ 70 ns
                    // through the 7 Ohm switch), tracked but not microscopic; LTE
                    // grows it back once the fast tail passes. Floored to a sane
                    // value, never below the old h*0.5 (so a genuinely fine flip
                    // step doesn't get coarsened).
                    accept = true;
                    let resume = (h * 0.5).max(1e-7);
                    next_dt = resume.clamp(dt_min, dt_max);
                }
                StepControl::Fixed { .. } => accept = true,
                StepControl::Adaptive { .. } => {
                    let err = lte_estimate(circuit, &ws, &state, h, opts);
                    if err <= 1.0 || h <= dt_min * 1.0001 {
                        accept = true;
                        // Grow/shrink for next step from the error ratio.
                        let safety = 0.9;
                        let factor = if err > 0.0 {
                            (safety * err.powf(-1.0 / 3.0)).clamp(0.5, 2.0)
                        } else {
                            2.0
                        };
                        next_dt = (h * factor).clamp(dt_min, dt_max);
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

            // Accept: advance time, update reactive history, emit.
            t += h;
            advance_reactive_state(&mut state, circuit, &ws, &x_accepted, h, opts, first_step);
            x_accepted.copy_from_slice(&ws.x);
            first_step = false;
            dt = next_dt;
            sink(StepSample { time: t, x: &ws.x });
        }
        Ok(())
    }
}

// --- reactive state bookkeeping ---------------------------------------------

/// At the operating point, capacitor voltage = node-voltage difference and
/// inductor current = its branch current; derivatives are zero (DC).
fn seed_reactive_state(state: &mut ReactiveState, circuit: &Circuit, ws: &Workspace) {
    for (id, dev) in circuit.iter() {
        let i = id.0 as usize;
        match dev {
            Device::Capacitor { a, b, ic, .. } => {
                state.x1[i] = ic.unwrap_or_else(|| node_v(ws, *a) - node_v(ws, *b));
                state.x2[i] = state.x1[i];
                state.dx1[i] = 0.0;
            }
            Device::Inductor { ic, .. } => {
                let cur = ws.layout.branch(id).map(|br| ws.x[br]).unwrap_or(0.0);
                state.x1[i] = ic.unwrap_or(cur);
                state.x2[i] = state.x1[i];
                state.dx1[i] = 0.0;
            }
            _ => {}
        }
    }
}

/// After an accepted step, roll history forward: x2 <- x1, x1 <- new value,
/// dx1 <- new derivative (for the trapezoidal predictor).
fn advance_reactive_state(
    state: &mut ReactiveState,
    circuit: &Circuit,
    ws: &Workspace,
    _x_prev: &[f64],
    h: f64,
    opts: &SolverOptions,
    first: bool,
) {
    // The derivative must be backed out with whatever rule actually ran this
    // step. A multi-step rule (trapezoidal) falls back to backward Euler on the
    // very first step, so the derivative there is the BE one.
    let trapz = opts.integration == Integration::Trapezoidal && !first;
    for (id, dev) in circuit.iter() {
        let i = id.0 as usize;
        match dev {
            Device::Capacitor { a, b, .. } => {
                let v_new = node_v(ws, *a) - node_v(ws, *b);
                let v_old = state.x1[i];
                // dv/dt consistent with the integration rule used this step.
                let dv = if trapz {
                    2.0 * (v_new - v_old) / h - state.dx1[i]
                } else {
                    (v_new - v_old) / h
                };
                state.x2[i] = v_old;
                state.x1[i] = v_new;
                state.dx1[i] = dv;
            }
            Device::Inductor { .. } => {
                let i_new = ws.layout.branch(id).map(|br| ws.x[br]).unwrap_or(0.0);
                let i_old = state.x1[i];
                let di = if trapz {
                    2.0 * (i_new - i_old) / h - state.dx1[i]
                } else {
                    (i_new - i_old) / h
                };
                state.x2[i] = i_old;
                state.x1[i] = i_new;
                state.dx1[i] = di;
            }
            _ => {}
        }
    }
}

#[inline]
fn node_v(ws: &Workspace, node: NodeId) -> f64 {
    match ws.layout.node(node) {
        Some(i) => ws.x[i],
        None => 0.0,
    }
}

// --- LTE estimate -----------------------------------------------------------

/// Dimensionless local-truncation-error norm (>1 means reject). Uses the
/// divided-difference of reactive-element states, the classic SPICE estimator:
/// `lte ~ C2 * h^2 * d3x/dt3`, normalized by `reltol*|x| + atol`.
fn lte_estimate(
    circuit: &Circuit,
    ws: &Workspace,
    state: &ReactiveState,
    h: f64,
    opts: &SolverOptions,
) -> f64 {
    let mut worst = 0.0f64;
    for (id, dev) in circuit.iter() {
        let i = id.0 as usize;
        let (x_new, atol) = match dev {
            Device::Capacitor { a, b, .. } => {
                (node_v(ws, *a) - node_v(ws, *b), opts.chgtol.max(opts.vntol))
            }
            Device::Inductor { .. } => {
                let cur = ws.layout.branch(id).map(|br| ws.x[br]).unwrap_or(0.0);
                (cur, opts.abstol.max(1e-9))
            }
            _ => continue,
        };
        // Second difference as a curvature proxy: |x_new - 2 x1 + x2|.
        let curv = (x_new - 2.0 * state.x1[i] + state.x2[i]).abs();
        let tol = opts.reltol * x_new.abs().max(state.x1[i].abs()) + atol;
        // Trapezoidal error coefficient ~ 1/12; scale into a [0,1]-ish norm.
        let err = (curv / 12.0) / tol.max(1e-30);
        let _ = h;
        worst = worst.max(err);
    }
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
/// away (lore #8 is the co-sim face of the same failure). Registering source
/// corners as mandatory step landings is the classic SPICE cure ("the
/// breakpoint table"), and is what makes the co-sim's PWL edge drive
/// (docs/dev-plans/05-cosim-fidelity.md section 1.3) honest on the analog
/// side.
///
/// PULSE corner enumeration is capped: a fast periodic source over a long
/// window enumerates corners only up to [`MAX_BREAKPOINTS`]; beyond the cap
/// the controller is left to its own rhythm (once locked onto a periodic
/// waveform the accepted-step history carries the curvature signal, so the
/// cap loses the guarantee only for the tail, not the lock-on). PWL lists are
/// finite by construction and are never truncated.
const MAX_BREAKPOINTS: usize = 100_000;

fn breakpoint_table(circuit: &Circuit, tstop: f64) -> Vec<f64> {
    let mut bps: Vec<f64> = Vec::new();
    for (_, dev) in circuit.iter() {
        let kind = match dev {
            Device::Vsource { kind, .. } | Device::Isource { kind, .. } => kind,
            _ => continue,
        };
        push_source_corners(kind, tstop, &mut bps);
    }
    bps.sort_by(|a, b| a.partial_cmp(b).expect("breakpoints are finite"));
    // Dedup within a relative epsilon: two corners closer than the controller
    // could ever distinguish are one landing.
    bps.dedup_by(|a, b| (*a - *b).abs() <= f64::max(1e-18, *b * 1e-12));
    bps.truncate(MAX_BREAKPOINTS);
    bps
}

/// Push one source's time-domain corners into `bps` (times strictly inside
/// `(0, tstop)`). Recursive so a `Ramped` envelope contributes both its own
/// full-amplitude corner at `scale_to` AND every corner of the inner source.
fn push_source_corners(kind: &hauksbee_ir::SourceKind, tstop: f64, bps: &mut Vec<f64>) {
    use hauksbee_ir::SourceKind;
    let push = |t: f64, bps: &mut Vec<f64>| {
        if t > 0.0 && t < tstop {
            bps.push(t);
        }
    };
    match kind {
        SourceKind::Pwl(points) => {
            for pt in points {
                push(pt.t, bps);
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
            // discontinuity itself is the thing to land on).
            let step = if *period > 0.0 { *period } else { tstop };
            let one_shot = *period <= 0.0;
            let mut t0 = *delay;
            while t0 < tstop && bps.len() < MAX_BREAKPOINTS {
                push(t0, bps);
                push(t0 + rise, bps);
                push(t0 + rise + width, bps);
                push(t0 + rise + width + fall, bps);
                if one_shot {
                    break;
                }
                t0 += step;
            }
        }
        SourceKind::Ramped { scale_to, inner } => {
            // The ramp's own kink at full amplitude, then the inner's corners
            // (the inner is not time-shifted, so its corner times are unchanged).
            push(*scale_to, bps);
            push_source_corners(inner, tstop, bps);
        }
        SourceKind::Dc(_) | SourceKind::Sin { .. } => {}
    }
}

// --- event detection --------------------------------------------------------

/// If any comparator/switch control voltage crosses its midpoint between the
/// accepted state and the trial state, return the approximate fraction of the
/// step at which it happens (for bisection). `None` if nothing crosses.
fn crossing_fraction(
    circuit: &Circuit,
    x0: &[f64],
    x1: &[f64],
    node_idx: &dyn Fn(NodeId) -> Option<usize>,
) -> Option<f64> {
    let mut earliest: Option<f64> = None;
    let vat = |x: &[f64], n: NodeId| node_idx(n).map(|i| x[i]).unwrap_or(0.0);
    for (_, dev) in circuit.iter() {
        let (cp, cn, mid) = match dev {
            Device::Comparator { inp, inn, .. } => (*inp, *inn, 0.0),
            Device::VSwitch {
                ctrl_p,
                ctrl_n,
                von,
                voff,
                ..
            } => (*ctrl_p, *ctrl_n, 0.5 * (von + voff)),
            _ => continue,
        };
        let d0 = vat(x0, cp) - vat(x0, cn) - mid;
        let d1 = vat(x1, cp) - vat(x1, cn) - mid;
        if d0.signum() != d1.signum() && (d1 - d0).abs() > 1e-15 {
            let frac = (d0 / (d0 - d1)).clamp(0.0, 1.0);
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
