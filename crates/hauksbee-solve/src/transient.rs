//! The transient driver: time-marching with companion models, adaptive step
//! control, and event-aware timestep refinement.
//!
//! Each accepted step solves a Newton operating point at `t + dt`, estimates
//! the local truncation error of the reactive elements, and either accepts the
//! step (advancing history) or shrinks `dt` and retries. Comparators and
//! switches are watched for threshold crossings; when one is straddled, the
//! step is bisected to land near the crossing so edges aren't smeared.

use crate::newton::{dc_operating_point_seeded, newton_solve, newton_solve_event, Workspace};
use crate::options::{DcInit, Integration, SolverOptions, StepControl, Strategy};
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

        // Branch unknowns follow the node block in layout order. The node
        // block can be larger than the netlist's node count: a series-
        // resistance BJT owns device-private internal unknowns there
        // (dev-plan 04 §3.2), so the offset comes from the layout, not from
        // `node_count()` (for internal-node-free circuits the two are equal).
        let branch_start = crate::system::Layout::new(circuit).n_nodes;
        self.run_streaming(circuit, tstop, |s| {
            wf.time.push(s.time);
            for node in 0..n_nodes {
                let v = if node == 0 { 0.0 } else { s.x[node - 1] };
                wf.node_voltages[node].push(v);
            }
            for (bi, slot) in wf.branch_currents.iter_mut().enumerate() {
                let idx = branch_start + bi;
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

        // Convergence doctrine for behavioral sources (dev-plan 04 §2.5):
        // B-sources are where decks go to diverge — an arbitrary expression's
        // tangent can overshoot Newton exactly like the traveling stiff-mesh
        // case the Armijo line search was built for. Their PRESENCE arms the
        // per-step line search unconditionally: it is a no-op on steps that
        // already converge (alpha = 1 accepts the full step, and the ladder
        // rung is bit-identical when never reached), so ordinary B-decks pay
        // one residual evaluation per iteration and stubborn ones get the
        // globalization instead of a dt_min death. Decks WITHOUT a B-source
        // take the `false` branch and stay bit-identical (the flagship
        // fixture hash pins that). Heavier rungs (TransientDyn, staged
        // regularizers) stay caller-granted — the corpus converges without
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
        // Known softness (review note): the staged-DC regularizers arm on
        // ws.used_staged_dc(), which only a DC solve can set, so a power-on
        // march runs the bare per-step path. A stiff board that needed the
        // regularized rescue fails honestly (the caller keeps its original
        // DC error); arming them under FromZero on nonlinear circuits is a
        // plausible improvement when the strategy ladder lands.
        // `.ic V(node)=val` under `uic`: the named node voltages are the given
        // initial conditions, everything else powers on from rest. (SPICE-compat
        // §4.1: with `uic`, `.ic` values seed the start directly, no DC solve.)
        let has_ic = !circuit.initial_conditions.is_empty();
        if from_zero {
            // Power-on: the unknown vector rests at zero. No DC solve to fail;
            // the ramp (paired Ramped sources) integrates the state up from here.
            for v in ws.x.iter_mut() {
                *v = 0.0;
            }
            for &(nid, val) in &circuit.initial_conditions {
                if let Some(i) = ws.layout.node(nid) {
                    ws.x[i] = val;
                }
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
        let transient_dyn = opts.ladder.has(Strategy::TransientDyn);
        if ws.used_staged_dc() || transient_dyn {
            // Arm the staged regularizers (negligible branch series R + node
            // damping + node-block convergence) AND the VSwitch/diode control
            // limiting (which is gated on branch_reg>0). On the synapse spike
            // path the per-step Newton fails right at the spike-gate switch flip
            // (a multi-decade conductance snap as a neuron V_out climbs through
            // the switch transition); the control limiting tracks the switch
            // through that transition. Armed when the DC needed staging OR when
            // the caller grants Strategy::TransientDyn even on a cleanly-DC-solved but
            // dynamically-stiff march (the RAMP-from-rest spike case).
            ws.set_staged_branch_reg(1e-2);
            // Arm the per-step transient event-freeze retry: when a bare per-step
            // Newton fails (the spike-gate SPDT flip), retry the step with the
            // comparator+switch states frozen Gauss-Seidel + break-before-make
            // before cutting the timestep. Gated behind Strategy::TransientDyn (the
            // explicit spike-path opt-in) so an ordinary staged board (e.g. the
            // DAC-rail co-sim, which needs the staged regularizers but whose
            // per-step Newton converges on the bare path) keeps its exact prior
            // behaviour and timing -- the retry only fires for callers chasing
            // the spike transient. The retry itself is a no-op unless the bare
            // step actually fails, so even when armed it never changes a step
            // that already converged.
            if transient_dyn {
                crate::diagnostics::note(Strategy::TransientDyn);
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
        // Under FromZero the reactive history stays at its all-zero construction
        // value (x1 = x2 = 0, dx1 = 0 for every cap and inductor): the board
        // powers on from rest, ignoring any device initial conditions. Otherwise
        // seed it from the DC operating point as usual.
        //
        // EXCEPTION: FromZero WITH `.ic` node voltages — seed the reactive state
        // from the now-nonzero ws.x so a capacitor spanning an `.ic` node starts
        // at the corresponding voltage (`seed_reactive_state` derives the cap
        // voltage from its node values), giving the physically-correct initial
        // charge instead of a first-step current spike.
        if !from_zero || has_ic {
            seed_reactive_state(&mut state, circuit, &ws, opts);
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
        // Spacing of the reactive history's two back samples (x1..x2), i.e.
        // the previous ACCEPTED step: the LTE divided difference needs it to
        // tell real curvature from a step-size change on a slope. 0.0 = no
        // accepted step yet.
        let mut h_prev_accepted: f64 = 0.0;

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

        // Step census (HAUKSBEE_STEP_CENSUS=1): the accepted grid alone cannot
        // attribute a march's wall (rejected trials, bisection retries and
        // Newton cuts leave no trace in the waveform), so the loop counts its
        // own discards. None when unset; every hook below is `if let Some`,
        // and the report prints on Drop so an erroring march still accounts.
        let mut census = crate::census::StepCensus::begin(
            tstop,
            circuit.devices.len(),
            ws.layout.size,
            matches!(opts.step, StepControl::Adaptive { .. }),
        );
        // Extrapolated trial seed (lever 3, mechanism (a)). The bare trial
        // Newton always started from the previous ACCEPTED point; on a
        // charging/discharging trajectory the root at t+h is a full step away,
        // and the census seed-shadow measured a linear extrapolation through
        // the last two accepted points CLOSER to the root on 89% of the joint
        // march's converged trials (>=10x closer on 57%). A closer start both
        // cuts iterations and can skip the far-from-root small-alpha
        // line-search grind entirely. EXACT root, different iterate sequence:
        // Newton converges to the same operating point within tolerance from
        // either seed, and every safety mechanism (Armijo line search, staged
        // damping, stall bail, event retry, step cut) still guards the path.
        // SCOPE: only the TransientDyn-armed ADAPTIVE march (the flagship
        // spike-path bundle, which is physics-gated, never bit-pinned); every
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
        let predictor_armed =
            transient_dyn && matches!(opts.step, StepControl::Adaptive { .. });
        const PRED_MAX_SCALE: f64 = 2.0;
        let mut pred_skip_once = false;
        // The accepted point BEFORE the current one: the predictor's second
        // history sample, also read by the census seed-shadow. Maintained only
        // when the predictor or the census wants it.
        let mut x_accepted_prev: Option<Vec<f64>> = None;

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

            // Trial solve at t + h, seeded from the previous accepted point,
            // or from the clamped linear extrapolation when armed (above).
            let t_trial = census.as_ref().map(|_| std::time::Instant::now());
            ws.x.copy_from_slice(&x_accepted);
            if predictor_armed && !first_step && !pred_skip_once && h_prev_accepted > 0.0 {
                if let Some(prev) = x_accepted_prev.as_ref() {
                    let scale = (h / h_prev_accepted).min(PRED_MAX_SCALE);
                    for i in 0..ws.x.len() {
                        ws.x[i] = x_accepted[i] + scale * (x_accepted[i] - prev[i]);
                    }
                }
            }
            // Bypass hold (dev-plan 03 §6 discipline): the trials that follow
            // an event-resolved accept must not bypass — the accepted pair
            // straddles the discontinuity the event loop just resolved, the
            // same reason the extrapolation seed is skipped there. Mirrors
            // `pred_skip_once`'s lifetime (true from an event-resolved accept
            // until the next ordinary accept). A bool store, read only when
            // `NewtonBypass::On` is armed; inert on every default run.
            ws.set_bypass_hold(pred_skip_once);
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

            if let Some(c) = census.as_mut() {
                c.newton_calls += 1;
                c.newton_iters += r.iters as u64;
                // Seed shadow: with the root in hand (a converged bare trial),
                // compare the start iterate the trial actually used
                // (x_accepted) against a linear extrapolation through the last
                // two accepted points, both as node-block inf-distances to the
                // root. Measures how many contraction decades a predictor seed
                // would buy WITHOUT changing any behaviour.
                if r.converged && h_prev_accepted > 0.0 {
                    if let Some(prev) = x_accepted_prev.as_ref() {
                        let n_nodes = ws.layout.n_nodes;
                        let scale = h / h_prev_accepted;
                        let mut d_start = 0.0f64;
                        let mut d_extrap = 0.0f64;
                        for i in 0..n_nodes {
                            let root = ws.x[i];
                            let cur = x_accepted[i];
                            let pred = cur + scale * (cur - prev[i]);
                            d_start = d_start.max((root - cur).abs());
                            d_extrap = d_extrap.max((root - pred).abs());
                        }
                        crate::census::predictor_shadow(d_start, d_extrap);
                    }
                }
            }

            let mut converged = r.converged;
            let mut used_event = false;
            if !converged && ws.tran_event() {
                let t_event = census.as_ref().map(|_| std::time::Instant::now());
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
                        &mut ws, circuit, opts, t + h, h, coeffs, &state, opts.gmin, cmp_smooth,
                    ) {
                        converged = true;
                        break;
                    }
                }
                ws.symbolic.set_allow_dynamic(had_dyn);
                used_event = converged;
                if let (Some(c), Some(t0)) = (census.as_mut(), t_event) {
                    c.ns_event_retry += t0.elapsed().as_nanos() as u64;
                }
            }

            if !converged {
                if let (Some(c), Some(t0)) = (census.as_mut(), t_trial) {
                    c.newton_fail_cuts += 1;
                    c.ns_newton_fail += t0.elapsed().as_nanos() as u64;
                }
                // Cut the step hard and retry.
                if h <= dt_min * 1.0001 {
                    // A behavioral-expression fault on the final attempt names
                    // the device: refuse loudly with the cause, never emit a
                    // truncated waveform (exit-3 discipline at the CLI).
                    let fault = ws
                        .behavioral_fault()
                        .map(|f| format!("; {f}"))
                        .unwrap_or_default();
                    return Err(format!(
                        "Newton failed at t={t} even at dt_min={dt_min}{fault}"
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
            // bare Newton can't solve — the exact dt_min thrash that re-opened the
            // wall. With the event loop owning the discontinuity, accept the step.
            if !used_event {
            if let Some(frac) = crossing_fraction(circuit, &x_accepted, &ws.x, &ws.layout_nodes()) {
                // Census: which devices' controls straddled a threshold on this
                // trial. A second read-only scan, run only when the census is
                // live, so the default path keeps the single-pass check.
                if let Some(c) = census.as_mut() {
                    crossing_census(circuit, &x_accepted, &ws.x, &ws.layout_nodes(), &mut c.crossings);
                }
                if matches!(opts.step, StepControl::Adaptive { .. }) && h > dt_min * 4.0 {
                    // Bisect toward the crossing for a sharper edge.
                    let refined = (h * frac).clamp(dt_min, h);
                    if (refined - h).abs() > dt_min {
                        if let (Some(c), Some(t0)) = (census.as_mut(), t_trial) {
                            c.event_bisections += 1;
                            c.ns_bisected += t0.elapsed().as_nanos() as u64;
                        }
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
                    let t_lte = census.as_ref().map(|_| std::time::Instant::now());
                    let err = lte_estimate(circuit, &ws, &state, h, h_prev_accepted, opts);
                    if let (Some(c), Some(t0)) = (census.as_mut(), t_lte) {
                        c.ns_lte_estimate += t0.elapsed().as_nanos() as u64;
                    }
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
                if let (Some(c), Some(t0)) = (census.as_mut(), t_trial) {
                    c.lte_rejected += 1;
                    c.ns_lte_rejected += t0.elapsed().as_nanos() as u64;
                }
                dt = next_dt;
                continue;
            }

            if let (Some(c), Some(t0)) = (census.as_mut(), t_trial) {
                c.accept(h);
                if used_event {
                    c.event_resolved += 1;
                }
                c.ns_accepted += t0.elapsed().as_nanos() as u64;
                // `t + h` here is bitwise the value `t` holds after the
                // `t += h` below (one addition either way), so the hash covers
                // exactly the (time, x) pairs the sink receives.
                c.hash_sample(t + h, &ws.x);
            }

            // Accept: advance time, update reactive history, emit.
            t += h;
            advance_reactive_state(&mut state, circuit, &ws, &x_accepted, h, opts, first_step);
            if census.is_some() || predictor_armed {
                match x_accepted_prev.as_mut() {
                    Some(p) => p.copy_from_slice(&x_accepted),
                    None => x_accepted_prev = Some(x_accepted.clone()),
                }
            }
            pred_skip_once = used_event;
            x_accepted.copy_from_slice(&ws.x);
            first_step = false;
            h_prev_accepted = h;
            dt = next_dt;
            sink(StepSample { time: t, x: &ws.x });
        }
        Ok(())
    }
}

// --- reactive state bookkeeping ---------------------------------------------

/// At the operating point, capacitor voltage = node-voltage difference and
/// inductor current = its branch current; derivatives are zero (DC). A
/// charge-storing diode (dev-plan 04 §3.1) seeds its CHARGE `Q(vd)` at the
/// operating-point junction voltage — its `ReactiveState` slots hold charge,
/// not voltage, so the companion stamp's history terms integrate `i = dQ/dt`
/// on the same machinery the linear capacitor uses.
fn seed_reactive_state(
    state: &mut ReactiveState,
    circuit: &Circuit,
    ws: &Workspace,
    opts: &SolverOptions,
) {
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
            Device::Diode { a, k, model, .. }
                if crate::stamp::diode_has_charge(model, &opts.effects) =>
            {
                state.x1[i] = diode_q(model, node_v(ws, *a) - node_v(ws, *k), opts);
                state.x2[i] = state.x1[i];
                state.dx1[i] = 0.0;
            }
            // Charge-storing BJT (dev-plan 04 §3.2): both junction charges,
            // seeded at the operating-point INTRINSIC junction voltages
            // (internal nodes when series resistance is stamped). Bank A is
            // Q_be, bank B is Q_bc — the packing `ReactiveState` documents.
            Device::Bjt { c, b, e, model, .. }
                if crate::stamp::bjt_has_charge(model, &opts.effects) =>
            {
                let (q_be, q_bc) = bjt_q(ws, id, *c, *b, *e, model, opts);
                state.x1[i] = q_be;
                state.x2[i] = q_be;
                state.dx1[i] = 0.0;
                state.xb[0].x1[i] = q_bc;
                state.xb[0].x2[i] = q_bc;
                state.xb[0].dx1[i] = 0.0;
            }
            // Charge-storing MOSFET (dev-plan 04 §3.3): all four charges at
            // the operating-point junction voltages — bank A = Q_gs,
            // xb[0] = Q_gd, xb[1] = Q_bd, xb[2] = Q_bs (the `ReactiveState`
            // packing table).
            Device::Mosfet { d, g, s, b, model, .. }
                if crate::stamp::mos_has_charge(model, &opts.effects) =>
            {
                let (q_gs, q_gd, q_bd, q_bs) = mos_q(ws, *d, *g, *s, *b, model, opts);
                state.x1[i] = q_gs;
                state.x2[i] = q_gs;
                state.dx1[i] = 0.0;
                for (bank, q) in [(0, q_gd), (1, q_bd), (2, q_bs)] {
                    state.xb[bank].x1[i] = q;
                    state.xb[bank].x2[i] = q;
                    state.xb[bank].dx1[i] = 0.0;
                }
            }
            _ => {}
        }
    }
}

/// MOSFET stored charges `(Q_gs, Q_gd, Q_bd, Q_bs)` at the solution in `ws`,
/// through the same model code and junction-voltage rule the stamp uses.
fn mos_q(
    ws: &Workspace,
    d: NodeId,
    g: NodeId,
    s: NodeId,
    b: Option<NodeId>,
    model: &hauksbee_ir::MosfetModel,
    opts: &SolverOptions,
) -> (f64, f64, f64, f64) {
    let (vgs, vgd, vbd, vbs) =
        crate::stamp::mos_junction_voltages(&ws.layout, &ws.x, d, g, s, b, model);
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

/// Diode stored charge at junction voltage `vd` (the §3.1 companion's state
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
    id: hauksbee_ir::DeviceId,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    model: &hauksbee_ir::BjtModel,
    opts: &SolverOptions,
) -> (f64, f64) {
    let (vbe, vbc) = crate::stamp::bjt_junction_voltages(
        &ws.layout,
        &ws.x,
        id,
        c,
        b,
        e,
        model,
        &opts.effects,
    );
    crate::stamp::bjt_charges_at(model, vbe, vbc, opts.model_temp(), opts.effects.temperature)
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
            // Charge-storing diode: the same roll as a capacitor, in CHARGE.
            // dx1 is dQ/dt, i.e. the capacitive branch current the next step's
            // trapezoidal history term needs.
            Device::Diode { a, k, model, .. }
                if crate::stamp::diode_has_charge(model, &opts.effects) =>
            {
                let q_new = diode_q(model, node_v(ws, *a) - node_v(ws, *k), opts);
                let q_old = state.x1[i];
                let dq = if trapz {
                    2.0 * (q_new - q_old) / h - state.dx1[i]
                } else {
                    (q_new - q_old) / h
                };
                state.x2[i] = q_old;
                state.x1[i] = q_new;
                state.dx1[i] = dq;
            }
            // Charge-storing BJT: the diode's roll applied to both banks
            // (bank A = Q_be, bank B = Q_bc); each dx is that junction's
            // dQ/dt, the capacitive current its trapezoidal history needs.
            Device::Bjt { c, b, e, model, .. }
                if crate::stamp::bjt_has_charge(model, &opts.effects) =>
            {
                let (q_be, q_bc) = bjt_q(ws, id, *c, *b, *e, model, opts);
                let q_old = state.x1[i];
                let dq = if trapz {
                    2.0 * (q_be - q_old) / h - state.dx1[i]
                } else {
                    (q_be - q_old) / h
                };
                state.x2[i] = q_old;
                state.x1[i] = q_be;
                state.dx1[i] = dq;
                let qb_old = state.xb[0].x1[i];
                let dqb = if trapz {
                    2.0 * (q_bc - qb_old) / h - state.xb[0].dx1[i]
                } else {
                    (q_bc - qb_old) / h
                };
                state.xb[0].x2[i] = qb_old;
                state.xb[0].x1[i] = q_bc;
                state.xb[0].dx1[i] = dqb;
            }
            // Charge-storing MOSFET: the same roll applied to all four banks
            // (A = Q_gs, xb[0] = Q_gd, xb[1] = Q_bd, xb[2] = Q_bs); each dx
            // is that junction's dQ/dt, the capacitive current its
            // trapezoidal history needs.
            Device::Mosfet { d, g, s, b, model, .. }
                if crate::stamp::mos_has_charge(model, &opts.effects) =>
            {
                let (q_gs, q_gd, q_bd, q_bs) = mos_q(ws, *d, *g, *s, *b, model, opts);
                let q_old = state.x1[i];
                let dq = if trapz {
                    2.0 * (q_gs - q_old) / h - state.dx1[i]
                } else {
                    (q_gs - q_old) / h
                };
                state.x2[i] = q_old;
                state.x1[i] = q_gs;
                state.dx1[i] = dq;
                for (bank, q_new) in [(0, q_gd), (1, q_bd), (2, q_bs)] {
                    let q_old = state.xb[bank].x1[i];
                    let dq = if trapz {
                        2.0 * (q_new - q_old) / h - state.xb[bank].dx1[i]
                    } else {
                        (q_new - q_old) / h
                    };
                    state.xb[bank].x2[i] = q_old;
                    state.xb[bank].x1[i] = q_new;
                    state.xb[bank].dx1[i] = dq;
                }
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

/// Dimensionless local-truncation-error norm (>1 means reject): a TRUE
/// divided-difference curvature estimate over the three newest samples,
/// normalized by `reltol*|x| + atol`.
///
/// Why divided differences and not the raw second difference: the samples are
/// an adaptive (NON-uniform) grid, and the raw `x_new - 2*x1 + x2` is nonzero
/// for a perfectly LINEAR trajectory whenever the step size changes (it equals
/// `slope * (h - h_prev)`), so every step-size change on a charging slope
/// manufactured fake curvature. Measured on the flagship joint capture march
/// (the step census): 44% of ALL trial solves were LTE-rejected, and a gentler
/// post-reject growth cap did not move that fraction AT ALL (44.1% before and
/// after), which is the fingerprint of a rejection signal that does not
/// depend on how the step got proposed. The divided-difference second
/// derivative `dd2` is exact-zero on linear trajectories, reduces to the SAME
/// `x_new - 2*x1 + x2` on a uniform grid (h == h_prev), and `h^2 * dd2` is
/// the classic uniform-grid curvature the 1/12 trapezoidal coefficient was
/// calibrated against.
///
/// `h_prev` is the previous ACCEPTED step (the spacing of x1..x2 in the
/// reactive history); 0.0 means "no history yet" (first trial), which falls
/// back to `h` and reproduces the old first-step behaviour exactly (the
/// seeded history has x2 == x1, so the h_prev term vanishes).
fn lte_estimate(
    circuit: &Circuit,
    ws: &Workspace,
    state: &ReactiveState,
    h: f64,
    h_prev: f64,
    opts: &SolverOptions,
) -> f64 {
    let mut worst = 0.0f64;
    // Non-uniform divided-difference curvature of one state history (see the
    // doc comment above): shared by the single-state tail below and the BJT's
    // two charge banks, so the arithmetic exists once.
    let err_of = |x_new: f64, x1: f64, x2: f64, atol: f64| {
        let hp = if h_prev > 0.0 { h_prev } else { h };
        let dd2 = 2.0 * ((x_new - x1) / h - (x1 - x2) / hp) / (h + hp);
        let curv = (h * h * dd2).abs();
        let tol = opts.reltol * x_new.abs().max(x1.abs()) + atol;
        (curv / 12.0) / tol.max(1e-30)
    };
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
            // A charge-storing diode is a reactive element: it participates in
            // truncation-error control through its CHARGE (the state its
            // companion integrates), with `chgtol` as the absolute floor —
            // SPICE's classic charge-based LTE. Charge-free diodes (`cjo == 0`,
            // `tt == 0`, or the toggle off) hit the `continue` below exactly as
            // before, so existing decks' step sequences are untouched.
            Device::Diode { a, k, model, .. }
                if crate::stamp::diode_has_charge(model, &opts.effects) =>
            {
                let q = diode_q(model, node_v(ws, *a) - node_v(ws, *k), opts);
                (q, opts.chgtol)
            }
            // A charge-storing BJT participates through BOTH junction
            // charges (chgtol floor each) — two states, two curvature
            // checks, same divided-difference estimator. Charge-free BJTs
            // hit the `continue` exactly as before.
            Device::Bjt { c, b, e, model, .. }
                if crate::stamp::bjt_has_charge(model, &opts.effects) =>
            {
                let (q_be, q_bc) = bjt_q(ws, id, *c, *b, *e, model, opts);
                worst = worst.max(err_of(q_be, state.x1[i], state.x2[i], opts.chgtol));
                worst = worst.max(err_of(q_bc, state.xb[0].x1[i], state.xb[0].x2[i], opts.chgtol));
                continue;
            }
            // A charge-storing MOSFET participates through all four charges
            // (chgtol floor each) — gate charge is what shapes its switching
            // edges, so it must gate the step size exactly as a capacitor
            // would. Charge-free MOSFETs hit the `continue` exactly as before.
            Device::Mosfet { d, g, s, b, model, .. }
                if crate::stamp::mos_has_charge(model, &opts.effects) =>
            {
                let (q_gs, q_gd, q_bd, q_bs) = mos_q(ws, *d, *g, *s, *b, model, opts);
                worst = worst.max(err_of(q_gs, state.x1[i], state.x2[i], opts.chgtol));
                for (bank, q) in [(0, q_gd), (1, q_bd), (2, q_bs)] {
                    worst = worst.max(err_of(
                        q,
                        state.xb[bank].x1[i],
                        state.xb[bank].x2[i],
                        opts.chgtol,
                    ));
                }
                continue;
            }
            _ => continue,
        };
        // Non-uniform second derivative from the three newest samples, scaled
        // by h^2 into the uniform-grid curvature the 1/12 coefficient expects.
        // On h == h_prev this equals x_new - 2*x1 + x2 (up to rounding); on a
        // linear trajectory it is zero regardless of the step-size history.
        // Trapezoidal error coefficient ~ 1/12, scaled into a [0,1]-ish norm
        // (the shared `err_of` above).
        worst = worst.max(err_of(x_new, state.x1[i], state.x2[i], atol));
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

/// Census-only twin of [`crossing_fraction`]: record EVERY device whose
/// control straddled its threshold on this trial step, by name. Separate from
/// the detection scan (which reports only the earliest fraction and stays on
/// the hot path) so the default march keeps its single pass; this one runs
/// only when HAUKSBEE_STEP_CENSUS is live, to answer "which devices drive the
/// global bisections".
fn crossing_census(
    circuit: &Circuit,
    x0: &[f64],
    x1: &[f64],
    node_idx: &dyn Fn(NodeId) -> Option<usize>,
    out: &mut std::collections::HashMap<String, u64>,
) {
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
            *out.entry(dev.name().to_string()).or_insert(0) += 1;
        }
    }
}

impl Workspace {
    /// A node->unknown index closure for event checks.
    fn layout_nodes(&self) -> impl Fn(NodeId) -> Option<usize> + '_ {
        move |n: NodeId| self.layout.node(n)
    }
}
