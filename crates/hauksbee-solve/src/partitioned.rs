//! The partitioned transient orchestrator.
//!
//! This is where the architecture's "partition before solving" pays off. After
//! [`Partition::analyze`] splits the circuit at ideal-source boundaries, this
//! driver advances each island with the cheapest method that fits it:
//!
//! * **Linear islands** ([`LinearIsland`]) advance by the cached matrix
//!   exponential — exact for the fixed step, one dense mat-vec per step. This is
//!   the Tarski closed-form trick generalized to an arbitrary linear subnetwork.
//! * **Nonlinear islands** are extracted into a small sub-circuit (boundary
//!   nodes become pinned voltage sources carrying the exchanged value) and
//!   solved with the existing MNA + Newton engine on their *own* small matrix —
//!   far cheaper than one global factorization.
//!
//! Islands exchange boundary node voltages once per step in a Gauss-Seidel
//! sweep: each island reads the latest values its neighbours produced and writes
//! back the nodes it owns. With `granularity == 1.0` the driver runs a few extra
//! relaxation sweeps per step to tighten the coupling; at lower granularity it
//! does a single sweep (faster, looser).
//!
//! ## Accuracy tradeoff
//!
//! Splitting the solve introduces a one-step lag in the inter-island coupling
//! (a node shared by two islands is updated by one island using the other's
//! previous-sweep value). For weakly-coupled partitions — the common case, since
//! we only cut at low-impedance source boundaries — the error is O(dt) per shared
//! node and vanishes with the step, well inside the documented 0.5% target. When
//! the cut would be strongly coupled, [`Partition`] keeps the devices in one
//! island (a nonlinear device taints its whole island), so the strongly-coupled
//! core is never split. `Partitioning::Off` disables all of this and is bit-for-
//! bit identical to the monolithic engine.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};

use crate::linear::LinearIsland;
use crate::newton::{dc_operating_point, newton_solve, Workspace};
use crate::options::{Integration, SolverOptions, StepControl};
use crate::orchestrate::balance::{settle_rails, BalancePolicy, RailChannel, RailLoads};
use crate::partition::{Island, Partition};
use crate::stamp::IntegCoeffs;
use crate::system::ReactiveState;
use crate::transient::StepSample;

/// A nonlinear island lowered to a self-contained sub-circuit plus its solver
/// workspace. Boundary input nodes appear as pinned voltage sources whose value
/// is refreshed from the global exchange buffer each step.
struct NonlinearIsland {
    /// The extracted sub-circuit.
    sub: Circuit,
    /// Solver workspace for the sub-circuit (own small matrix).
    ws: Workspace,
    /// Reactive history for the sub-circuit's devices.
    state: ReactiveState,
    /// Accepted unknowns from the previous step (sub-circuit layout).
    x_accepted: Vec<f64>,
    /// Map: sub-circuit non-ground NodeId.0 -> global NodeId, for write-back.
    l2g: Vec<NodeId>,
    /// Boundary input nodes (global) and the sub Vsource id pinning each.
    boundary: Vec<(NodeId, DeviceId)>,
    first_step: bool,
}

/// A linear island with at most this many states is small enough that the exact
/// matrix-exponential advance beats re-solving it via MNA every step.
const SMALL_ISLAND_STATES: usize = 48;

/// If, after tearing a shunt-fed rail, the largest nonlinear block is still
/// bigger than this, the tear did not genuinely fragment the core (something
/// else — e.g. an analog-switch mesh — keeps it fused). Tearing then buys nothing
/// and we fall back to the monolithic path. Sized well above a real per-neuron
/// block (~65 devices on the Tarski board) but far below the fused 5k-device
/// island, so the win is taken only when it is real.
const TEAR_MAX_BLOCK_DEVICES: usize = 600;

/// Count the reactive states (caps + inductors) in an island.
fn count_states(circuit: &Circuit, isl: &Island) -> usize {
    isl.devices
        .iter()
        .filter(|&&id| {
            matches!(
                circuit.devices[id.0 as usize],
                Device::Capacitor { .. } | Device::Inductor { .. }
            )
        })
        .count()
}

/// Runtime state for one shunt-fed rail tear (see [`crate::partition::RailTear`]).
/// The rail voltage is an unknown solved by a scalar balance each step.
struct RailTearState {
    rail: NodeId,
    feed: NodeId,
    r_shunt: f64,
}

/// The partitioned engine for a fixed-step run.
pub struct PartitionedTransient {
    opts: SolverOptions,
    linear: Vec<LinearIsland>,
    /// Per linear island: its state vector (caps/inductors) and input scratch.
    lin_state: Vec<Vec<f64>>,
    lin_input_buf: Vec<Vec<f64>>,
    /// Per linear island: free-node voltage scratch and the global node of each.
    lin_free_nodes: Vec<Vec<NodeId>>,
    nonlinear: Vec<NonlinearIsland>,
    /// Global voltage source ids (cut points) and their global +/- nodes.
    sources: Vec<DeviceId>,
    /// Detected shunt-fed rail tears, solved by scalar balance each step.
    tears: Vec<RailTearState>,
    /// Global node-voltage exchange buffer, indexed by NodeId.0.
    vbuf: Vec<f64>,
    n_nodes: usize,
}

impl PartitionedTransient {
    /// Try to build a partitioned engine. Returns `None` (caller falls back to
    /// monolithic) when partitioning is off, the step is adaptive, or the
    /// circuit has no separable linear island to gain from.
    pub fn try_build(circuit: &Circuit, opts: &SolverOptions) -> Option<PartitionedTransient> {
        if matches!(opts.partitioning, crate::options::Partitioning::Off) {
            return None;
        }
        // The closed-form linear path requires a fixed timestep.
        if !matches!(opts.step, StepControl::Fixed { .. }) {
            return None;
        }
        // Detect shunt-fed rail tears first; if any, the analysis splits the
        // array into per-block islands that couple only through the scalar rail
        // voltage (bordered-block-diagonal). This is what unfuses the Tarski
        // synapse array (one ANALOG_VDD rail behind a 1 kΩ sense shunt).
        let mut part = Partition::analyze_with_tears(circuit);

        // A tear only helps if it actually FRAGMENTS the fused core into small
        // blocks. If a giant nonlinear island survives the tear (e.g. the array
        // is still fused through another path — analog switches that the static
        // graph can't cut), tearing buys nothing and only risks a harder per-block
        // seed. In that case drop the tears and fall back to the plain analysis,
        // so the monolithic path handles the board correctly (no regression).
        let largest_nl_devices = part
            .islands
            .iter()
            .filter(|i| !i.linear)
            .map(|i| i.devices.len())
            .max()
            .unwrap_or(0);
        if !part.tears.is_empty() && largest_nl_devices > TEAR_MAX_BLOCK_DEVICES {
            part = Partition::analyze(circuit);
        }

        let has_tears = !part.tears.is_empty();

        // Decide whether partitioning actually helps. The closed-form linear
        // fast path wins on *small* islands (few states, where one exact matrix-
        // exponential step replaces a stiff Newton solve) and on *many* islands
        // (each tiny matrix beats one giant global factorization). It loses on a
        // single large linear island: the sparse MNA engine already solves a
        // long ladder in O(n) per step, which no dense/matrix-free exponential
        // can beat. So we partition only when the work genuinely fragments.
        let n_islands = part.islands.len();
        let largest_linear = part
            .islands
            .iter()
            .filter(|i| i.linear)
            .map(|i| count_states(circuit, i))
            .max()
            .unwrap_or(0);

        // A large nonlinear island that survived tearing must be cold-seeded by
        // the partitioned path every chunk. On a stiff diode-laden core (e.g. the
        // Tarski synapse array + pulse-stretcher diodes) that cold seed needs the
        // monolithic staged-DC homotopy, which the per-island path does not run.
        // Routing such a board to the monolithic reference is EXACT (Off is
        // bit-identical) and is where the staged convergence aid lives, so defer
        // to it rather than build a partitioned solve that cannot seed the core.
        let big_nonlinear_core = largest_nl_devices > TEAR_MAX_BLOCK_DEVICES
            && circuit
                .devices
                .iter()
                .any(|d| matches!(d, Device::Diode { .. }));
        if big_nonlinear_core {
            return None;
        }

        let many_islands = n_islands >= 4;
        let small_linear_speedup =
            part.has_linear_island() && largest_linear <= SMALL_ISLAND_STATES;
        // A rail tear genuinely fragments a fused nonlinear core, so it is always
        // worth taking even if the per-island linear heuristics don't trigger.
        if !has_tears && !many_islands && !small_linear_speedup {
            // One big monolithic-friendly block (or a lone large ladder): let the
            // reference sparse engine handle it.
            return None;
        }

        Self::build_from_partition(circuit, opts, part)
    }

    /// Build from a partition whose tears an EXTERNAL decision layer chose
    /// (the decompose analysis, via [`Partition::analyze_imposing_tears`]).
    /// None of this module's legacy profitability heuristics run: the caller's
    /// cost model and structural guards already ruled, and second-guessing
    /// them here would put two deciders in charge of one tear. Still requires
    /// a fixed step (the mechanics need it), still returns `None` when a
    /// sub-island cannot be constructed, and the caller falls back to the
    /// exact monolithic path in that case.
    pub fn try_build_from_partition(
        circuit: &Circuit,
        opts: &SolverOptions,
        part: Partition,
    ) -> Option<PartitionedTransient> {
        if !matches!(opts.step, StepControl::Fixed { .. }) {
            return None;
        }
        // Power-on starts have no DC operating point to seed the islands
        // from; this engine cold-seeds a global DC unconditionally, so it
        // must decline rather than silently solve a DC the caller asked to
        // skip (the monolithic path owns DcInit::FromZero).
        if !matches!(opts.dc_init, crate::options::DcInit::Solve) {
            return None;
        }
        Self::build_from_partition(circuit, opts, part)
    }

    /// Shared construction tail: lower a partition into runnable islands and
    /// tear states. Decision logic lives in the callers.
    fn build_from_partition(
        circuit: &Circuit,
        opts: &SolverOptions,
        part: Partition,
    ) -> Option<PartitionedTransient> {
        // Rail nodes: islands touching them are kept as nonlinear sub-blocks so
        // their rail-boundary current is readable for the scalar balance.
        let rail_nodes: Vec<NodeId> = part.tears.iter().map(|t| t.rail).collect();

        let n_nodes = part.n_nodes;
        let mut linear = Vec::new();
        let mut lin_state = Vec::new();
        let mut lin_input_buf = Vec::new();
        let mut lin_free_nodes = Vec::new();
        let mut nonlinear = Vec::new();

        let touches_rail = |isl: &crate::partition::Island| -> bool {
            !rail_nodes.is_empty()
                && isl.boundary_in.iter().any(|n| rail_nodes.contains(n))
        };
        for isl in &part.islands {
            // A linear island that loads a torn rail must be solved as a sub-block
            // (not state-space reduced) so its rail-boundary current is readable
            // for the scalar balance. Pure linear islands off the rail keep the
            // fast closed-form path.
            if isl.linear && !touches_rail(isl) {
                match LinearIsland::compile(circuit, isl, opts.gmin) {
                    Some(li) => {
                        let n = li.n_states();
                        let free: Vec<NodeId> = collect_free_nodes(isl, &li);
                        lin_state.push(vec![0.0; n]);
                        lin_input_buf.push(vec![0.0; li.n_inputs_total()]);
                        lin_free_nodes.push(free);
                        linear.push(li);
                    }
                    None => {
                        // Couldn't reduce (e.g. pure-resistive island): treat it
                        // as a nonlinear sub-circuit so it's still solved.
                        nonlinear.push(NonlinearIsland::build(circuit, isl, opts)?);
                    }
                }
            } else {
                nonlinear.push(NonlinearIsland::build(circuit, isl, opts)?);
            }
        }

        // Build runtime tear states. Each rail's loads are the nonlinear sub-
        // blocks that pin it as a boundary; their rail currents are read directly
        // during the balance, so no extra bookkeeping is needed here.
        let tears: Vec<RailTearState> = part
            .tears
            .iter()
            .map(|t| RailTearState {
                rail: t.rail,
                feed: t.feed,
                r_shunt: t.r_shunt,
            })
            .collect();

        // Order cut sources so each is applied only after the node it references
        // is resolved: grounded-reference sources first, then sources whose
        // reference is a node a prior source already pins. This makes the single
        // in-order sweep in `apply_sources` correct for stacked floating rails.
        let sources = order_sources(circuit, &part.sources);

        Some(PartitionedTransient {
            opts: *opts,
            linear,
            lin_state,
            lin_input_buf,
            lin_free_nodes,
            nonlinear,
            sources,
            tears,
            vbuf: vec![0.0; n_nodes + 1],
            n_nodes,
        })
    }

    /// Run to `tstop`, streaming each accepted step to `sink`. The sample's `x`
    /// is the global node-voltage vector reconstructed from all islands (branch
    /// currents are not reassembled here; node voltages are the probe surface).
    pub fn run_streaming<F: FnMut(StepSample)>(
        &mut self,
        circuit: &Circuit,
        tstop: f64,
        mut sink: F,
    ) -> Result<(), String> {
        let dt = match self.opts.step {
            StepControl::Fixed { dt } => dt,
            _ => return Err("partitioned path requires fixed step".into()),
        };

        // Seed all islands from a DC operating point, then exchange.
        self.seed(circuit)?;

        // Cache exponentials for every linear island at this dt.
        for li in &mut self.linear {
            li.ensure_cache(dt);
        }

        // Emit t = 0.
        let mut xglobal = vec![0.0f64; self.global_x_len()];
        self.gather_into(&mut xglobal);
        sink(StepSample {
            time: 0.0,
            x: &xglobal,
        });

        let relax_sweeps = if self.opts.granularity >= 0.999 {
            3
        } else if self.opts.granularity > 0.0 {
            2
        } else {
            1
        };

        let mut t = 0.0;
        let eps = dt * 1e-9;
        while t < tstop - eps {
            let h = dt.min(tstop - t);
            let tnext = t + h;

            // Update cut-source-driven boundary voltages in the exchange buffer
            // at the new time (zero-order hold input for this step).
            self.apply_sources(circuit, tnext);

            if self.tears.is_empty() {
                // Gauss-Seidel sweeps over islands (unchanged legacy path).
                for sweep in 0..relax_sweeps {
                    self.sweep(circuit, h, tnext, sweep == 0)?;
                }
            } else {
                self.step_with_rail_balance(circuit, h, tnext, relax_sweeps)?;
            }

            // Commit accepted state in every island (advance reactive history).
            self.commit(circuit, h);

            t = tnext;
            self.gather_into(&mut xglobal);
            sink(StepSample {
                time: t,
                x: &xglobal,
            });
        }
        Ok(())
    }

    /// Advance one step while exactly closing every shunt-fed rail's KCL balance.
    ///
    /// For a single tear node the system is bordered-block-diagonal: every block
    /// depends on the rest only through `v(rail)`, so the step reduces to the
    /// scalar rail balance owned by [`crate::orchestrate::balance`] (secant
    /// iteration, voltage-referred tolerance, the gmin double-count correction;
    /// see that module for the full story). This method contributes only the
    /// adapter: how THIS engine's blocks re-solve when a rail moves.
    ///
    /// With multiple independent tears the loop relaxes them round-robin; since
    /// the tears found on this board share the same feed (`+5V`) but separate
    /// rails, the coupling between them is only second order and the outer loop
    /// converges in a handful of passes.
    ///
    /// The balance report is intentionally not fatal here: this engine's
    /// behaviour on a non-converging balance (proceed, let the step-level
    /// machinery judge) is what the `rail_tear` gate certifies. The strategy
    /// ladder's Decompose rung is where non-convergence escalates instead.
    fn step_with_rail_balance(
        &mut self,
        circuit: &Circuit,
        h: f64,
        tnext: f64,
        _relax_sweeps: usize,
    ) -> Result<(), String> {
        // One sweep advances trial state from accepted history at the current
        // rail estimates (linear islands + every nonlinear block solved once).
        self.sweep(circuit, h, tnext, true)?;

        // Cascade wiring: a tear whose FEED is another tear's rail is a child
        // of that rail, fed through its own shunt. The parent's KCL must carry
        // that inter-rail current (the shunt lives between two held rails, so
        // it is in no block); we hand each channel the list of children it
        // feeds so the balance loop subtracts the analytic draw (see
        // [`RailChannel::children`] and `orchestrate::balance`).
        let channels: Vec<RailChannel> = self
            .tears
            .iter()
            .map(|t| {
                let children: Vec<(usize, f64)> = self
                    .tears
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.feed == t.rail)
                    .map(|(j, c)| (j, c.r_shunt))
                    .collect();
                RailChannel {
                    rail: t.rail,
                    shunt_ohms: t.r_shunt,
                    children,
                }
            })
            .collect();
        let mut loads = PartitionedRailLoads {
            nonlinear: &mut self.nonlinear,
            vbuf: &mut self.vbuf,
            tears: &self.tears,
            opts: &self.opts,
            h,
            tnext,
        };
        settle_rails(
            &mut loads,
            &channels,
            self.opts.gmin,
            self.opts.vntol,
            self.opts.abstol,
            &BalancePolicy::default(),
        )?;
        Ok(())
    }

    /// One Gauss-Seidel sweep: advance each island reading the latest exchange
    /// buffer and writing its owned node voltages back. `first` advances the
    /// trial state from accepted history; later sweeps re-solve in place to
    /// relax coupling without committing.
    fn sweep(&mut self, circuit: &Circuit, h: f64, tnext: f64, first: bool) -> Result<(), String> {
        // Linear islands.
        for (idx, li) in self.linear.iter().enumerate() {
            // Gather inputs: boundary voltages, then current-source values
            // evaluated at the end of the step (ZOH over the interval).
            let inputs = li.inputs();
            let n_vin = inputs.len();
            let buf = &mut self.lin_input_buf[idx];
            for (k, n) in inputs.iter().enumerate() {
                buf[k] = self.vbuf[n.0 as usize];
            }
            for (k, (id, _, _)) in li.isources().iter().enumerate() {
                if let hauksbee_ir::Device::Isource { kind, .. } = &circuit.devices[id.0 as usize] {
                    buf[n_vin + k] = kind.eval(tnext);
                }
            }
            // Advance state only on the first sweep (the exact step); later
            // sweeps just re-read outputs with refreshed inputs.
            if first {
                li.step(&mut self.lin_state[idx], buf);
            }
            // Reconstruct free-node voltages and write back.
            let free = &self.lin_free_nodes[idx];
            let mut vfree = vec![0.0f64; li.n_free()];
            li.node_voltages(&self.lin_state[idx], buf, &mut vfree);
            for (f, gn) in free.iter().enumerate() {
                self.vbuf[gn.0 as usize] = vfree[f];
            }
        }

        // Nonlinear islands.
        for nl in &mut self.nonlinear {
            nl.refresh_boundary(&self.vbuf);
            nl.step(h, tnext, first, &self.opts)?;
            nl.write_back(&mut self.vbuf);
        }
        Ok(())
    }

    /// Commit reactive history after an accepted step.
    fn commit(&mut self, circuit: &Circuit, h: f64) {
        // Linear islands keep their state vector directly (already advanced).
        for nl in &mut self.nonlinear {
            nl.commit(h, &self.opts);
        }
        let _ = (circuit, h);
    }

    /// Seed every island from a DC operating point and fill the exchange buffer.
    fn seed(&mut self, circuit: &Circuit) -> Result<(), String> {
        // Use the monolithic DC solve to get a globally-consistent operating
        // point, then distribute it to island states and the exchange buffer.
        let mut ws = Workspace::new(circuit);
        dc_operating_point(&mut ws, circuit, &self.opts)?;
        // Fill exchange buffer with node voltages (ground stays 0).
        for ni in 1..=self.n_nodes {
            let v = ws
                .layout
                .node(NodeId(ni as u32))
                .map(|i| ws.x[i])
                .unwrap_or(0.0);
            self.vbuf[ni] = v;
        }
        // Seed linear island states (cap voltages, inductor currents).
        for (idx, li) in self.linear.iter().enumerate() {
            let st = &mut self.lin_state[idx];
            for (k, (id, is_cap)) in li.state_devices().enumerate() {
                st[k] = if is_cap {
                    match &circuit.devices[id.0 as usize] {
                        Device::Capacitor { a, b, ic, .. } => {
                            ic.unwrap_or_else(|| node_v(&ws, *a) - node_v(&ws, *b))
                        }
                        _ => 0.0,
                    }
                } else {
                    match &circuit.devices[id.0 as usize] {
                        Device::Inductor { ic, .. } => ic.unwrap_or_else(|| {
                            ws.layout.branch(id).map(|br| ws.x[br]).unwrap_or(0.0)
                        }),
                        _ => 0.0,
                    }
                };
            }
        }
        // Seed nonlinear islands from the same DC point.
        for nl in &mut self.nonlinear {
            nl.seed_from_global(&ws, circuit, &self.opts)?;
        }
        Ok(())
    }

    /// Stamp cut-source values into the exchange buffer at time `t`.
    ///
    /// A source pins `v(p) - v(n) = val`. Whichever terminal is *not* already
    /// pinned (ground or a previously-resolved source node) is the one this
    /// source fixes. `self.sources` is ordered so that a source whose reference
    /// terminal is another source's pinned node comes later (see `try_build`),
    /// which lets a single in-order sweep resolve stacked floating supplies.
    fn apply_sources(&mut self, circuit: &Circuit, t: f64) {
        for &sid in &self.sources {
            if let Device::Vsource { p, n, kind, .. } = &circuit.devices[sid.0 as usize] {
                let val = kind.eval(t);
                if !p.is_ground() {
                    // Fix v(p) relative to n's (ground or resolved) value.
                    let vn = if n.is_ground() {
                        0.0
                    } else {
                        self.vbuf[n.0 as usize]
                    };
                    self.vbuf[p.0 as usize] = vn + val;
                } else if !n.is_ground() {
                    // Positive terminal on ground (negative rail): v(n) = -val.
                    self.vbuf[n.0 as usize] = -val;
                }
            }
        }
    }

    fn global_x_len(&self) -> usize {
        // Node voltages [0..n_nodes] map to unknown indices [0..n_nodes-1]
        // exactly like the monolithic layout's node block; branch currents are
        // appended but the partitioned probe surface only fills node voltages.
        self.n_nodes
    }

    /// Gather the exchange buffer into a global unknown vector (node block).
    fn gather_into(&self, x: &mut [f64]) {
        for ni in 1..=self.n_nodes {
            x[ni - 1] = self.vbuf[ni];
        }
    }
}

/// The [`RailLoads`] adapter for this engine: a moved rail re-solves exactly
/// the nonlinear sub-blocks that pin it as a boundary (in place, no history
/// advance); off-rail blocks are untouched. Boundary currents come from the
/// blocks' pinned-source branch unknowns (see [`NonlinearIsland::rail_current`]).
struct PartitionedRailLoads<'a> {
    nonlinear: &'a mut Vec<NonlinearIsland>,
    vbuf: &'a mut Vec<f64>,
    tears: &'a [RailTearState],
    opts: &'a SolverOptions,
    h: f64,
    tnext: f64,
}

impl RailLoads for PartitionedRailLoads<'_> {
    fn resolve(&mut self, i: usize, v_rail: f64) -> Result<(), String> {
        let rail = self.tears[i].rail;
        self.vbuf[rail.0 as usize] = v_rail;
        for nl in self.nonlinear.iter_mut() {
            if nl.touches_rail(rail) {
                nl.refresh_boundary(self.vbuf);
                nl.step(self.h, self.tnext, false, self.opts)?;
                nl.write_back(self.vbuf);
            }
        }
        Ok(())
    }

    fn rail_voltage(&self, i: usize) -> f64 {
        self.vbuf[self.tears[i].rail.0 as usize]
    }

    fn feed_voltage(&self, i: usize) -> f64 {
        let feed = self.tears[i].feed;
        if feed.is_ground() {
            0.0
        } else {
            self.vbuf[feed.0 as usize]
        }
    }

    fn current_drawn(&self, i: usize) -> f64 {
        let rail = self.tears[i].rail;
        self.nonlinear.iter().map(|nl| nl.rail_current(rail)).sum()
    }

    fn n_loads(&self, i: usize) -> usize {
        let rail = self.tears[i].rail;
        self.nonlinear.iter().filter(|nl| nl.touches_rail(rail)).count()
    }
}

/// Order cut voltage sources so each source's *reference* terminal is resolved
/// before the source is applied. A source resolves the node `v(p)` (when `n` is
/// ground/resolved) or `v(n)` (when `p` is ground). We greedily emit sources
/// whose reference node is already pinned, starting from ground; any cycle (a
/// pathological floating loop) is appended in original order as a fallback.
fn order_sources(circuit: &Circuit, sources: &[DeviceId]) -> Vec<DeviceId> {
    let mut resolved = std::collections::HashSet::new();
    resolved.insert(NodeId::GROUND);
    let mut out = Vec::with_capacity(sources.len());
    let mut pending: Vec<DeviceId> = sources.to_vec();
    loop {
        let before = out.len();
        pending.retain(|&sid| {
            if let Device::Vsource { p, n, .. } = &circuit.devices[sid.0 as usize] {
                // Resolvable if exactly one terminal is already resolved.
                let pr = resolved.contains(p);
                let nr = resolved.contains(n);
                if pr ^ nr {
                    out.push(sid);
                    resolved.insert(*p);
                    resolved.insert(*n);
                    return false;
                }
                if pr && nr {
                    // Redundant (both pinned) — emit and drop.
                    out.push(sid);
                    return false;
                }
            }
            true
        });
        if out.len() == before {
            break; // no progress; remaining are cyclic/floating
        }
    }
    out.extend(pending);
    out
}

/// Free (owned, solvable) nodes of a linear island, in island-local order.
fn collect_free_nodes(isl: &Island, li: &LinearIsland) -> Vec<NodeId> {
    let mut free: Vec<(usize, NodeId)> = Vec::new();
    for n in &isl.nodes {
        if let Some(l) = li.local_of(*n) {
            free.push((l, *n));
        }
    }
    free.sort_by_key(|(l, _)| *l);
    free.into_iter().map(|(_, n)| n).collect()
}

fn node_v(ws: &Workspace, node: NodeId) -> f64 {
    ws.layout.node(node).map(|i| ws.x[i]).unwrap_or(0.0)
}

impl NonlinearIsland {
    /// Extract a nonlinear island into a sub-circuit with pinned-source boundaries.
    fn build(circuit: &Circuit, isl: &Island, opts: &SolverOptions) -> Option<NonlinearIsland> {
        let n_nodes_global = circuit.max_node() as usize;
        let mut g2l: Vec<Option<NodeId>> = vec![None; n_nodes_global + 1];
        g2l[0] = Some(NodeId::GROUND);
        let mut sub = Circuit::new();
        sub.temp_c = circuit.temp_c;
        let mut l2g: Vec<NodeId> = vec![NodeId::GROUND]; // index 0 = ground

        // Map every node the island touches.
        let map_node = |sub: &mut Circuit,
                        g2l: &mut Vec<Option<NodeId>>,
                        l2g: &mut Vec<NodeId>,
                        gn: NodeId|
         -> NodeId {
            if gn.is_ground() {
                return NodeId::GROUND;
            }
            if let Some(ln) = g2l[gn.0 as usize] {
                return ln;
            }
            let ln = sub.node(&format!("n{}", gn.0));
            g2l[gn.0 as usize] = Some(ln);
            // l2g is dense by local id; push to align.
            while (l2g.len() as u32) <= ln.0 {
                l2g.push(NodeId::GROUND);
            }
            l2g[ln.0 as usize] = gn;
            ln
        };

        // Copy devices, remapping nodes.
        for &id in &isl.devices {
            let dev = &circuit.devices[id.0 as usize];
            let remap = |g2l: &mut Vec<Option<NodeId>>,
                         l2g: &mut Vec<NodeId>,
                         sub: &mut Circuit,
                         n: NodeId| { map_node(sub, g2l, l2g, n) };
            let nd = clone_remapped(dev, |n| {
                // closure capturing requires the helper; do it inline.
                remap(&mut g2l, &mut l2g, &mut sub, n)
            });
            sub.add(nd);
        }

        // Add a pinned voltage source for each boundary input node.
        let mut boundary = Vec::new();
        for &bn in &isl.boundary_in {
            let ln = map_node(&mut sub, &mut g2l, &mut l2g, bn);
            let sid = sub.add(Device::Vsource {
                name: format!("VB{}", bn.0),
                p: ln,
                n: NodeId::GROUND,
                kind: SourceKind::Dc(0.0),
            });
            boundary.push((bn, sid));
        }

        let _ = (opts, &g2l);
        let ws = Workspace::new(&sub);
        let n_dev = sub.devices.len();
        let size = ws.layout.size;
        Some(NonlinearIsland {
            sub,
            ws,
            state: ReactiveState::new(n_dev),
            x_accepted: vec![0.0; size],
            l2g,
            boundary,
            first_step: true,
        })
    }

    /// Seed the sub-circuit's accepted state from the global DC operating point.
    fn seed_from_global(
        &mut self,
        global_ws: &Workspace,
        _circuit: &Circuit,
        opts: &SolverOptions,
    ) -> Result<(), String> {
        // Set boundary sources to the global DC node voltages, then solve the
        // sub-circuit's own DC point so its internal nodes are consistent.
        for (gn, sid) in &self.boundary {
            let v = node_v(global_ws, *gn);
            if let Device::Vsource { kind, .. } = &mut self.sub.devices[sid.0 as usize] {
                *kind = SourceKind::Dc(v);
            }
        }
        dc_operating_point(&mut self.ws, &self.sub, opts)?;
        self.x_accepted.copy_from_slice(&self.ws.x);
        // Seed reactive history from the sub DC point.
        seed_sub_reactive(&mut self.state, &self.sub, &self.ws);
        Ok(())
    }

    /// Refresh boundary source values from the global exchange buffer.
    fn refresh_boundary(&mut self, vbuf: &[f64]) {
        for (gn, sid) in &self.boundary {
            let v = vbuf[gn.0 as usize];
            if let Device::Vsource { kind, .. } = &mut self.sub.devices[sid.0 as usize] {
                *kind = SourceKind::Dc(v);
            }
        }
    }

    /// Solve the sub-circuit for the trial state at `tnext`.
    fn step(
        &mut self,
        h: f64,
        _tnext: f64,
        first: bool,
        opts: &SolverOptions,
    ) -> Result<(), String> {
        if first {
            self.ws.x.copy_from_slice(&self.x_accepted);
        }
        let coeffs = IntegCoeffs::for_step(opts.integration, h, self.first_step);
        let r = newton_solve(
            &mut self.ws,
            &self.sub,
            opts,
            // time only affects sub-sources, which are all DC-pinned; pass 0.
            0.0,
            h,
            coeffs,
            &self.state,
            false,
            false,
            opts.gmin,
            1.0,
        );
        if !r.converged {
            let names: Vec<&str> = self.sub.devices.iter().map(|d| d.name()).take(8).collect();
            return Err(format!(
                "nonlinear island Newton failed at h={h} (island of {}: {})",
                self.sub.devices.len(),
                names.join(", ")
            ));
        }
        Ok(())
    }

    /// Current this island draws OUT of a given global rail node.
    ///
    /// The boundary source is stamped `p = rail_local, n = ground`. The MNA stamp
    /// adds `+1·i` to node `p`'s KCL row, where `i` is the source branch unknown,
    /// so the rail node's balance reads `I_island + i = 0` ⇒ the current the
    /// island draws from the rail is `-i`. Returns 0 if this island does not
    /// touch `rail`.
    fn rail_current(&self, rail: NodeId) -> f64 {
        for (gn, sid) in &self.boundary {
            if *gn == rail {
                if let Some(bi) = self.ws.layout.branch(*sid) {
                    return -self.ws.x[bi];
                }
            }
        }
        0.0
    }

    /// True if this island pins the given global rail as a boundary input.
    fn touches_rail(&self, rail: NodeId) -> bool {
        self.boundary.iter().any(|(gn, _)| *gn == rail)
    }

    /// Write the island's owned node voltages back to the global buffer.
    fn write_back(&self, vbuf: &mut [f64]) {
        for ln in 1..self.l2g.len() {
            let gn = self.l2g[ln];
            if gn.is_ground() {
                continue;
            }
            // Don't overwrite boundary inputs (owned elsewhere).
            if self.boundary.iter().any(|(bn, _)| *bn == gn) {
                continue;
            }
            if let Some(i) = self.ws.layout.node(NodeId(ln as u32)) {
                vbuf[gn.0 as usize] = self.ws.x[i];
            }
        }
    }

    /// Commit accepted state and advance reactive history.
    fn commit(&mut self, h: f64, opts: &SolverOptions) {
        advance_sub_reactive(
            &mut self.state,
            &self.sub,
            &self.ws,
            h,
            opts,
            self.first_step,
        );
        self.x_accepted.copy_from_slice(&self.ws.x);
        self.first_step = false;
    }
}

/// Clone a device with each NodeId passed through `f` (node remapping).
///
/// The per-variant walk lives once, on the IR type: see [`Device::map_nodes`].
fn clone_remapped(dev: &Device, mut f: impl FnMut(NodeId) -> NodeId) -> Device {
    let mut d = dev.clone();
    d.map_nodes(&mut f);
    d
}

// Reactive-state helpers mirroring the monolithic transient driver, applied to
// a sub-circuit. Kept here so the partitioned path is self-contained.

fn seed_sub_reactive(state: &mut ReactiveState, sub: &Circuit, ws: &Workspace) {
    for (id, dev) in sub.iter() {
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

fn advance_sub_reactive(
    state: &mut ReactiveState,
    sub: &Circuit,
    ws: &Workspace,
    h: f64,
    opts: &SolverOptions,
    first: bool,
) {
    let trapz = opts.integration == Integration::Trapezoidal && !first;
    for (id, dev) in sub.iter() {
        let i = id.0 as usize;
        match dev {
            Device::Capacitor { a, b, .. } => {
                let v_new = node_v(ws, *a) - node_v(ws, *b);
                let v_old = state.x1[i];
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
