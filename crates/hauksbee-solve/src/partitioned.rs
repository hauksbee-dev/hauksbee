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
//! Islands exchange boundary node voltages once per step in a DOUBLE-BUFFERED
//! JACOBI sweep (dev-plan 03 §3.3): every island reads its inputs from the
//! frozen previous-generation buffer and writes the nodes it owns into a write
//! buffer; the buffers swap at sweep end. No island ever reads another's
//! this-sweep output, so the sweep result is a pure function of (previous
//! buffer, island state) — bit-for-bit independent of the order islands run
//! in, and therefore of thread count and scheduling when the compute phase is
//! parallelized ([`crate::ParallelPolicy`]). Sweeps repeat until the largest
//! weighted boundary change drops below the Newton tolerance convention
//! (reltol/vntol), replacing the old fixed 3/2/1 counts; a coupling that
//! cannot converge within [`COUPLING_SWEEP_CAP`] sweeps fails the step loudly
//! (see [`PartitionedTransient::relax_step`]) instead of accepting a
//! half-relaxed exchange.
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
//!
//! ## Why Jacobi and not Gauss-Seidel
//!
//! The pre-S4 sweep was sequential Gauss-Seidel: island *k+1* could read island
//! *k*'s freshly written value. That ordering is exactly what a parallel sweep
//! cannot preserve, and a lock-ordered parallel Gauss-Seidel would serialize the
//! very dependency chain we want to spread across cores while still being
//! "deterministic" only relative to an arbitrary lock order. Jacobi's
//! order-independence is a property of the math, not of a lock discipline. The
//! trade is convergence rate (Jacobi is the weaker relaxation), which is why the
//! sweep count is convergence-gated rather than fixed. In every partition the
//! current analyzer produces, island inputs are outer-written nodes (source pins
//! and torn rails), never another island's owned node — the union-find fuses any
//! shared free node into one island — so on real boards today the Jacobi sweep
//! computes bit-for-bit what the Gauss-Seidel sweep did.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies; shared with `partition.rs`, whose analysis this executes):
//! docs/how-and-why/hauksbee-solve/partition.md

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use rayon::prelude::*;

use crate::linear::LinearIsland;
use crate::newton::{dc_operating_point, newton_solve, Workspace};
use crate::options::{Integration, ParallelPolicy, SolverOptions, StepControl};
use crate::orchestrate::balance::{
    ensure_balanced, settle_rails, BalancePolicy, RailChannel, RailLoads,
};
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
    /// The nodes this island OWNS (writes to the exchange buffer), as
    /// `(global node, index into ws.x)` pairs: every mapped non-ground node
    /// that is not a boundary input. Precomputed at build so the scatter phase
    /// is a flat copy with no per-write layout lookups, and so the build-time
    /// single-writer check (see `verify_single_writer`) has an explicit owned
    /// set to verify rather than an implicit "whatever write_back skips".
    owned: Vec<(NodeId, usize)>,
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

/// Sweep-count cap for the convergence-gated Jacobi relaxation (plan §3.3).
/// Jacobi's spectral radius on a genuinely tight coupling can sit near 1, so an
/// uncapped loop would hang exactly on the boards that need help most; a
/// coupling that has not relaxed to tolerance within this many sweeps is a sign
/// the cut is stronger than the partitioner believed, and the step FAILS over
/// to the caller's escalation path (the staged orchestrator re-solves the group
/// fused/monolithic — see `orchestrate::staged::solve_group`) rather than
/// silently accepting a half-converged exchange. 16 is the plan's starting
/// value; every real partition today converges in <= 2 sweeps (island inputs
/// are outer-written pins/rails), so the cap only bites on imposed partitions
/// with genuine inter-island feedback.
const COUPLING_SWEEP_CAP: usize = 16;

/// [`ParallelPolicy::Auto`] engages the thread pool only when at least this
/// many NONLINEAR islands exist. Measured on the shunt-fed mirror arrays
/// (the graded boards, plan §9.1): a warm per-block re-solve at quiescence is
/// sub-microsecond, so the 24-block array LOSES to pool coordination at any
/// worker count, the 90-block array breaks even, and the 240-block array wins
/// — the threshold sits between 24 and 90. Boards whose islands carry real
/// per-step Newton work clear the per-task overhead far earlier; explicit
/// `Threads(n)` bypasses this gate for them.
const PAR_MIN_NONLINEAR_ISLANDS: usize = 32;

/// Cap on the per-engine pool size under [`ParallelPolicy::Auto`]. Island
/// solves are fine-grained (a warm quiescent block re-solve is well under a
/// microsecond), so extra workers add coordination and cache traffic faster
/// than they add arithmetic. Measured on the 240-block mirror array (M1,
/// 4P+4E): 2 workers win 1.36-1.37x end-to-end run after run (~2.3x on the
/// balance march alone in the cleanest probe window), 4 workers only break
/// even, 8 lose outright (efficiency cores, plus spin-waiting workers taxing
/// the serial sections between passes). Auto therefore sizes a QUARTER of
/// the logical CPUs — the measured optimum here, conservative everywhere —
/// floored at 2 and capped here. `Threads(n)` overrides both for callers who
/// know their machine and their board's per-island weight.
const PAR_MAX_THREADS: usize = 4;

/// Minimum islands per parallel task. A warm-started per-block Newton solve
/// on a mirror-array island is sub-microsecond (a handful of devices, an
/// ~8x8 factorization, 1-2 iterations), far below rayon's per-task dispatch
/// cost, so task-per-island dispatch LOSES to the sequential loop — measured
/// directly on the 240-block array. Batching islands per task restores the
/// arithmetic-to-overhead ratio; the value is tuned on the graded mirror
/// arrays (see the S4 measurement commit).
const PAR_MIN_ISLANDS_PER_TASK: usize = 32;

/// Build the per-engine rayon pool for a policy, given how many nonlinear
/// islands the partition produced. `None` means "run sequential", which is
/// always numerically identical (the Jacobi sweep is order-free), so a pool
/// build failure quietly degrades to sequential rather than failing the run.
fn build_pool(policy: ParallelPolicy, n_nonlinear: usize) -> Option<rayon::ThreadPool> {
    let threads = match policy {
        ParallelPolicy::Off => return None,
        ParallelPolicy::Threads(n) => n.max(1),
        ParallelPolicy::Auto => {
            if n_nonlinear < PAR_MIN_NONLINEAR_ISLANDS {
                return None;
            }
            let avail = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            if avail < 2 {
                return None; // a pool on one CPU is pure overhead
            }
            // A quarter of the logical CPUs, floored at 2 and capped (see
            // PAR_MAX_THREADS for the measurements behind this shape).
            (avail / 4).max(2).min(PAR_MAX_THREADS)
        }
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .ok()
}

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
    /// Per linear island: reconstructed free-node voltage buffer, pre-allocated
    /// (sized `li.n_free()`) and reused across sweeps instead of a per-sweep
    /// `vec![0.0; n_free]`. `node_voltages` fully overwrites every entry before
    /// any read, so reuse is a pure lifetime change (plan §4.2).
    lin_vfree: Vec<Vec<f64>>,
    nonlinear: Vec<NonlinearIsland>,
    /// Global voltage source ids (cut points) and their global +/- nodes.
    sources: Vec<DeviceId>,
    /// Detected shunt-fed rail tears, solved by scalar balance each step.
    tears: Vec<RailTearState>,
    /// Global node-voltage exchange buffer, indexed by NodeId.0. Between
    /// sweeps this is the committed (read) generation; `sweep` writes the next
    /// generation into `vbuf_next` and swaps, so no island ever reads another
    /// island's this-sweep output (the double-buffered Jacobi discipline).
    vbuf: Vec<f64>,
    /// The write half of the double buffer. Only `sweep` touches it: seeded
    /// each sweep by copying `vbuf` (so unowned slots — source pins, torn
    /// rails, ground — carry through unchanged), overlaid with every island's
    /// owned outputs, then swapped into place.
    vbuf_next: Vec<f64>,
    /// True iff some island READS a node another island OWNS, i.e. the Jacobi
    /// exchange carries real inter-island data flow and relaxation sweeps can
    /// make progress. False for every partition the analyzer produces today
    /// (inputs are outer-written source pins / torn rails), in which case one
    /// sweep is exact coupling-wise and the relaxation loop is skipped rather
    /// than measured — a re-solved Newton island jitters by its own step
    /// tolerance, which would flap a measured gate without conveying anything.
    coupled: bool,
    /// Per-engine rayon pool (plan §3.4): `Some` when the policy engaged.
    /// Owned by THIS engine — never a global pool — so a caller that runs
    /// many engines concurrently cannot oversubscribe through us. Entered
    /// ONCE per step (`in_pool`), because entry from an outside thread is a
    /// blocking handoff whose cost rivals a whole quiescent island pass; the
    /// phase-(a) `par_iter`s inside then run on the ambient (= this) pool
    /// with the workers already hot across the sweep and every balance pass.
    pool: Option<rayon::ThreadPool>,
    /// `pool.is_some()`, readable while the pool itself is temporarily moved
    /// out during `in_pool`. Phase (a) branches on this: when true, the code
    /// is by construction executing inside the engine's own pool.
    par: bool,
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
        let mut lin_vfree = Vec::new();
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
                // `model_temp()` (not raw `temperature_c`): the reducer bakes
                // resistor values into A/B at compile time, and the effective
                // temperature is what the monolithic stamp derates tc1
                // resistors with — TNOM when the temperature effect is off.
                match LinearIsland::compile(circuit, isl, opts.gmin, opts.model_temp()) {
                    Some(li) => {
                        let n = li.n_states();
                        let free: Vec<NodeId> = collect_free_nodes(isl, &li);
                        lin_state.push(vec![0.0; n]);
                        lin_input_buf.push(vec![0.0; li.n_inputs_total()]);
                        lin_vfree.push(vec![0.0; li.n_free()]);
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

        // ---- Single-writer-per-slot invariant (plan §3.2, hazard A). ----
        // Owned sets must be pairwise disjoint across ALL islands (linear and
        // nonlinear alike), and no island may own an outer-written slot: a
        // torn rail belongs to the scalar balance, not to any island. This is
        // ENFORCED, not assumed, because `Partition`'s fields are public and
        // `try_build_from_partition` accepts partitions from external decision
        // layers: a violation means two islands would scatter into the same
        // exchange slot and the result would depend on execution order. Debug
        // builds scream; release builds refuse the torn build and the caller
        // falls back to the exact monolithic path.
        let nl_owned: Vec<Vec<NodeId>> = nonlinear
            .iter()
            .map(|nl| nl.owned.iter().map(|&(gn, _)| gn).collect())
            .collect();
        let mut owned_sets: Vec<&[NodeId]> =
            Vec::with_capacity(lin_free_nodes.len() + nl_owned.len());
        owned_sets.extend(lin_free_nodes.iter().map(|v| v.as_slice()));
        owned_sets.extend(nl_owned.iter().map(|v| v.as_slice()));
        let claimed = match verify_single_writer(&owned_sets, &rail_nodes, n_nodes) {
            Ok(claimed) => claimed,
            Err(msg) => {
                debug_assert!(false, "partition ownership violation: {msg}");
                return None;
            }
        };

        // Does any island READ a slot some island OWNS? Only then does the
        // Jacobi exchange carry inter-island data flow that relaxation sweeps
        // can tighten; every partition the analyzer produces today reads only
        // outer-written pins/rails, so this is normally false and the step
        // loop runs exactly one sweep (see `relax_step`).
        let coupled = linear
            .iter()
            .flat_map(|li| li.inputs().iter())
            .chain(nonlinear.iter().flat_map(|nl| nl.boundary.iter().map(|(bn, _)| bn)))
            .any(|n| claimed[n.0 as usize]);

        let pool = build_pool(opts.parallel, nonlinear.len());
        let par = pool.is_some();

        Some(PartitionedTransient {
            opts: *opts,
            linear,
            lin_state,
            lin_input_buf,
            lin_free_nodes,
            lin_vfree,
            nonlinear,
            sources,
            tears,
            vbuf: vec![0.0; n_nodes + 1],
            vbuf_next: vec![0.0; n_nodes + 1],
            coupled,
            pool,
            par,
            n_nodes,
        })
    }

    /// Run `f` inside this engine's pool (or inline when sequential). The one
    /// pool-entry point: callers wrap a whole step's solve so the blocking
    /// outside-thread handoff is paid once per step, not once per relaxation
    /// sweep or balance pass, and the phase-(a) `par_iter`s inside execute on
    /// the ambient — that is, this engine's own — pool.
    fn in_pool<R: Send>(&mut self, f: impl FnOnce(&mut Self) -> R + Send) -> R {
        match self.pool.take() {
            Some(pool) => {
                let r = pool.install(|| f(self));
                self.pool = Some(pool);
                r
            }
            None => f(self),
        }
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

        let mut t = 0.0;
        let eps = dt * 1e-9;
        while t < tstop - eps {
            let h = dt.min(tstop - t);
            let tnext = t + h;

            // The cached exponential is exact only for the step it was built
            // at. The truncated FINAL step (`h < dt`, whenever `tstop` is not
            // an integer multiple of `dt` — the common case) must rebuild it,
            // or the last sample (and a co-sim chunk's exit state) silently
            // replays a full-dt advance over a shorter interval.
            // `ensure_cache` is a no-op while `h == dt`, so every interior
            // step keeps the amortized one-mat-vec fast path untouched.
            if h != dt {
                for li in &mut self.linear {
                    li.ensure_cache(h);
                }
            }

            // Update cut-source-driven boundary voltages in the exchange buffer
            // at the new time (zero-order hold input for this step).
            self.apply_sources(circuit, tnext);

            // One pool entry for the whole step's solve (see `in_pool`).
            self.in_pool(|me| {
                if me.tears.is_empty() {
                    // Convergence-gated Jacobi relaxation (replaces the fixed
                    // 3/2/1 sweep counts; see `relax_step` for the guard).
                    me.relax_step(circuit, h, tnext)
                } else {
                    me.step_with_rail_balance(circuit, h, tnext)
                }
            })?;

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
            par: self.par,
            h,
            tnext,
        };
        let report = settle_rails(
            &mut loads,
            &channels,
            self.opts.gmin,
            self.opts.vntol,
            self.opts.abstol,
            &BalancePolicy::default(),
        )?;
        // settle_rails returns Ok even when it exhausts its pass budget without
        // converging. A caller that can escalate MUST refuse a non-converged
        // balance rather than stream a silently-unbalanced rail; ensure_balanced
        // routes that into the same Err channel as a per-island Newton failure
        // and relax_step's coupling refusal, which staged::solve_group escalates
        // by re-solving the group fused on the monolithic engine.
        ensure_balanced(&report)?;
        Ok(())
    }

    /// Advance one step on the tear-free path: the exact time-advance sweep,
    /// then — only when the partition carries real inter-island coupling —
    /// convergence-gated relaxation sweeps until the boundary exchange settles
    /// under the reltol/vntol convention (replacing the old fixed 3/2/1
    /// counts, which could silently under- or over-relax).
    ///
    /// The divergence guard mirrors the tear-refusal doctrine: a coupling that
    /// cannot prove itself within [`COUPLING_SWEEP_CAP`] sweeps is refused
    /// loudly (the error names the stalled node), never silently truncated.
    /// The error leaves `run_streaming` through the same channel as a
    /// per-island Newton failure, which is the channel the staged orchestrator
    /// already escalates by re-solving the group fused on the monolithic
    /// engine (`orchestrate::staged::solve_group`).
    fn relax_step(&mut self, circuit: &Circuit, h: f64, tnext: f64) -> Result<(), String> {
        // The time step itself: island states advance from accepted history.
        // Its delta measures the physical step change, not coupling error, so
        // it never participates in the convergence decision.
        self.sweep(circuit, h, tnext, true)?;
        if !self.coupled {
            // No island reads a slot any island owns, so the sweep read only
            // frozen outer-written inputs and the exchange is already exact:
            // further sweeps could only re-polish each island's own Newton
            // root by its step tolerance, which is noise, not coupling.
            return Ok(());
        }
        let mut last = (f64::INFINITY, 0u32);
        for _ in 1..COUPLING_SWEEP_CAP {
            last = self.sweep(circuit, h, tnext, false)?;
            if last.0 <= 1.0 {
                return Ok(());
            }
        }
        Err(format!(
            "inter-island coupling failed to relax within {COUPLING_SWEEP_CAP} sweeps at \
             t={tnext:.6e}: weighted boundary change {:.3e} (tolerance 1.0) still moving at \
             node {} — the cut is stronger than the partitioner believed; refusing the \
             partitioned step",
            last.0, last.1
        ))
    }

    /// Test-only accessor for the S1 allocation-hygiene gate (plan §4.4): run
    /// one sweep directly so the `alloc_audit` counter can measure the per-step
    /// inner loop in isolation. Keeps `sweep` itself private; not engine API.
    #[cfg(test)]
    pub(crate) fn sweep_for_audit(
        &mut self,
        circuit: &Circuit,
        h: f64,
        tnext: f64,
        first: bool,
    ) -> Result<(), String> {
        // Through `in_pool` like every real caller, so a parallel-policy audit
        // could never leak onto the global rayon pool.
        self.in_pool(|me| me.sweep(circuit, h, tnext, first).map(|_| ()))
    }

    /// One relaxation sweep over all islands, as an explicit double-buffered
    /// Jacobi exchange (plan §3.3), in two phases:
    ///
    /// * **Compute** (phase a): every island reads the frozen `vbuf` and
    ///   computes its owned outputs into private scratch — `lin_vfree` for
    ///   linear islands (the S1 buffer, plan §4.2), the island's own workspace
    ///   for nonlinear ones. No shared writes anywhere, so island order cannot
    ///   affect a single bit of the result; this is the phase
    ///   [`crate::ParallelPolicy`] may hand to the thread pool.
    /// * **Scatter** (phase b): `vbuf_next` is seeded from `vbuf` (unowned
    ///   slots — source pins, torn rails, ground — carry through unchanged),
    ///   every island's owned scratch lands in its disjoint slots, and the
    ///   buffers swap. Kept serial: it is a memcpy plus scattered stores, and
    ///   the disjointness that would make it safely parallel also makes it too
    ///   cheap to bother.
    ///
    /// Returns the largest weighted change `|Δv| / (reltol·max|v| + vntol)`
    /// over all owned slots and the global node where it occurred; `<= 1.0`
    /// means the exchange has relaxed to the solver's own tolerance convention.
    ///
    /// `first` advances island state from accepted history (the actual time
    /// step); later sweeps re-solve in place to relax coupling without
    /// committing.
    fn sweep(
        &mut self,
        circuit: &Circuit,
        h: f64,
        tnext: f64,
        first: bool,
    ) -> Result<(f64, u32), String> {
        // ---- Phase a: compute owned outputs into per-island scratch. ----
        // Sequential and pooled arms run the SAME per-island code on the same
        // frozen inputs; the pool only changes which core runs which island,
        // which the phase's no-shared-writes structure makes unobservable.
        // Both arms visit EVERY island and surface the lowest-indexed failure,
        // so the error message (not just the waveform) is independent of
        // execution order and thread count.
        let vbuf: &[f64] = &self.vbuf;
        let opts = &self.opts;
        let first_err: Option<String> = if self.par {
            // Inside the engine's own pool (every caller reaches `sweep`
            // through `in_pool`), so the ambient `par_iter`s below execute on
            // it, never on the global rayon pool.
            {
                let (_, nl_err) = rayon::join(
                    || {
                        (
                            self.linear.as_slice(),
                            self.lin_state.as_mut_slice(),
                            self.lin_input_buf.as_mut_slice(),
                            self.lin_vfree.as_mut_slice(),
                        )
                            .into_par_iter()
                            .with_min_len(PAR_MIN_ISLANDS_PER_TASK)
                            .for_each(|(li, state, buf, vfree)| {
                                linear_phase_a(
                                    li, circuit, vbuf, tnext, first, state, buf, vfree,
                                )
                            })
                    },
                    || {
                        self.nonlinear
                            .par_iter_mut()
                            .with_min_len(PAR_MIN_ISLANDS_PER_TASK)
                            .enumerate()
                            .filter_map(|(i, nl)| {
                                nl.phase_a(vbuf, h, tnext, first, opts)
                                    .err()
                                    .map(|e| (i, e))
                            })
                            .min_by_key(|(i, _)| *i)
                    },
                );
                nl_err.map(|(_, e)| e)
            }
        } else {
            {
                for (((li, state), buf), vfree) in self
                    .linear
                    .iter()
                    .zip(self.lin_state.iter_mut())
                    .zip(self.lin_input_buf.iter_mut())
                    .zip(self.lin_vfree.iter_mut())
                {
                    linear_phase_a(li, circuit, vbuf, tnext, first, state, buf, vfree);
                }
                let mut first_err: Option<String> = None;
                for nl in self.nonlinear.iter_mut() {
                    if let Err(e) = nl.phase_a(vbuf, h, tnext, first, opts) {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
                first_err
            }
        };
        if let Some(e) = first_err {
            return Err(e);
        }

        // ---- Phase b: scatter owned scratch into the write buffer. ----
        self.vbuf_next.copy_from_slice(&self.vbuf);
        let reltol = self.opts.reltol;
        let vntol = self.opts.vntol;
        let mut worst = 0.0f64;
        let mut worst_node = 0u32;
        {
            let read: &[f64] = &self.vbuf;
            let write: &mut [f64] = &mut self.vbuf_next;
            let mut scatter = |gn: NodeId, vnew: f64| {
                let ni = gn.0 as usize;
                let vold = read[ni];
                let w = (vnew - vold).abs() / (reltol * vnew.abs().max(vold.abs()) + vntol);
                if w > worst {
                    worst = w;
                    worst_node = gn.0;
                }
                write[ni] = vnew;
            };
            for (free, vfree) in self.lin_free_nodes.iter().zip(self.lin_vfree.iter()) {
                for (f, gn) in free.iter().enumerate() {
                    scatter(*gn, vfree[f]);
                }
            }
            for nl in &self.nonlinear {
                for &(gn, xi) in &nl.owned {
                    scatter(gn, nl.ws.x[xi]);
                }
            }
        }
        std::mem::swap(&mut self.vbuf, &mut self.vbuf_next);
        Ok((worst, worst_node))
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
    ///
    /// The primary path is a monolithic whole-circuit DC solve, distributed to
    /// the islands. When that DC does not exist (the flagship mega group's
    /// collapse, #59), the whole-circuit solve is exactly the thing this engine
    /// was torn to avoid, so it falls back to a DECOMPOSED seed
    /// ([`Self::seed_decomposed`]): boundary estimates into the exchange buffer,
    /// then a per-island DC (robust to a block whose own DC also fails), letting
    /// the first marching steps' rail balance reconcile. The success path is
    /// byte-for-byte the legacy behaviour: the fallback only engages when the
    /// global DC errors, so the `rail_tear` bit-gate (whose fixtures have a
    /// converging DC) is untouched.
    fn seed(&mut self, circuit: &Circuit) -> Result<(), String> {
        // Use the monolithic DC solve to get a globally-consistent operating
        // point, then distribute it to island states and the exchange buffer.
        let mut ws = Workspace::new(circuit);
        if dc_operating_point(&mut ws, circuit, &self.opts).is_err() {
            return self.seed_decomposed(circuit);
        }
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

    /// Decomposed seed for when the whole-circuit DC has no solution.
    ///
    /// The exchange buffer is filled with boundary ESTIMATES (cut-source pins at
    /// t=0, each torn rail at its feed-side voltage), then every island is DC'd
    /// against those estimates on its OWN small matrix. A block whose own DC
    /// also fails (a comparator astable has no fixed point at all) does not abort
    /// the seed: [`NonlinearIsland::seed_from_vbuf`] projects the boundary
    /// estimates onto its nodes and lets the first marching steps integrate out
    /// of the guess. The rails are NOT explicitly settled at t=0 here (a full
    /// t=0 settle is awkward before the first `commit`); the accepted v1 is to
    /// let the first `step_with_rail_balance` reconcile them, so the very first
    /// window carries the rail's approach to balance rather than a settled
    /// operating point. That first-window transient is honest data, bounded, and
    /// vanishes once the balance converges (typically within the first step).
    fn seed_decomposed(&mut self, circuit: &Circuit) -> Result<(), String> {
        // 1. Boundary estimates into the exchange buffer.
        for v in self.vbuf.iter_mut() {
            *v = 0.0;
        }
        // Cut sources (and stacked supplies) resolved at t=0.
        self.apply_sources(circuit, 0.0);
        // Each torn rail starts at its feed voltage. Iterated so a cascade
        // (a rail whose feed is another torn rail) propagates from the source
        // outward; the loop is bounded by the tear count.
        for _ in 0..self.tears.len().max(1) {
            for t in &self.tears {
                let vfeed = if t.feed.is_ground() {
                    0.0
                } else {
                    self.vbuf[t.feed.0 as usize]
                };
                self.vbuf[t.rail.0 as usize] = vfeed;
            }
        }

        // 2. Linear island states from the boundary estimates.
        let vbuf_v = |n: NodeId, vbuf: &[f64]| -> f64 {
            if n.is_ground() {
                0.0
            } else {
                vbuf[n.0 as usize]
            }
        };
        for (idx, li) in self.linear.iter().enumerate() {
            let st = &mut self.lin_state[idx];
            for (k, (id, is_cap)) in li.state_devices().enumerate() {
                st[k] = if is_cap {
                    match &circuit.devices[id.0 as usize] {
                        Device::Capacitor { a, b, ic, .. } => {
                            ic.unwrap_or_else(|| vbuf_v(*a, &self.vbuf) - vbuf_v(*b, &self.vbuf))
                        }
                        _ => 0.0,
                    }
                } else {
                    match &circuit.devices[id.0 as usize] {
                        Device::Inductor { ic, .. } => ic.unwrap_or(0.0),
                        _ => 0.0,
                    }
                };
            }
        }

        // 3. Per-island DC from the boundary estimates (robust to failure),
        //    writing each island's internal nodes back so the emitted t=0
        //    sample and the next step's boundary reads are consistent.
        for nl in &mut self.nonlinear {
            nl.seed_from_vbuf(&self.vbuf, &self.opts);
            nl.write_back(&mut self.vbuf);
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
    /// True iff the engine's pool is engaged (see [`PartitionedTransient::par`]);
    /// the balance runs inside `in_pool`, so the ambient `par_iter` here is the
    /// engine's own pool. The per-trial block re-solves are the hot loop of a
    /// torn board, and every block reads only the frozen buffer, so they
    /// parallelize with the same order-free argument as the sweep's phase (a).
    par: bool,
    h: f64,
    tnext: f64,
}

impl RailLoads for PartitionedRailLoads<'_> {
    fn resolve(&mut self, i: usize, v_rail: f64) -> Result<(), String> {
        let rail = self.tears[i].rail;
        self.vbuf[rail.0 as usize] = v_rail;
        // Phase (a): every block touching the rail re-solves against the
        // frozen buffer (boundary slots are outer-written — never any block's
        // owned slot, enforced at build — so compute order is unobservable).
        // Phase (b): scatter owned outputs back, serially. Splitting the
        // phases is what lets (a) run on the pool; on the sequential arm the
        // split is bit-neutral for the same reason it is safe in parallel.
        let vbuf: &[f64] = self.vbuf;
        let (h, tnext, opts) = (self.h, self.tnext, self.opts);
        let err: Option<(usize, String)> = if self.par {
            self.nonlinear
                .par_iter_mut()
                .with_min_len(PAR_MIN_ISLANDS_PER_TASK)
                .enumerate()
                .filter(|(_, nl)| nl.touches_rail(rail))
                .filter_map(|(k, nl)| {
                    nl.phase_a(vbuf, h, tnext, false, opts).err().map(|e| (k, e))
                })
                .min_by_key(|(k, _)| *k)
        } else {
            {
                let mut first_err = None;
                for (k, nl) in self.nonlinear.iter_mut().enumerate() {
                    if nl.touches_rail(rail) {
                        if let Err(e) = nl.phase_a(vbuf, h, tnext, false, opts) {
                            if first_err.is_none() {
                                first_err = Some((k, e));
                            }
                        }
                    }
                }
                first_err
            }
        };
        if let Some((_, e)) = err {
            return Err(e);
        }
        for nl in self.nonlinear.iter() {
            if nl.touches_rail(rail) {
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

/// Phase (a) of the Jacobi sweep for one linear island: gather inputs from the
/// frozen read buffer (boundary voltages, then current-source values evaluated
/// at the end of the step — ZOH over the interval), advance the exact
/// matrix-exponential state on the first sweep, and reconstruct free-node
/// voltages into the island's private `vfree` scratch (pre-allocated per S1,
/// plan §4.2; `node_voltages` overwrites every entry, so no clearing needed).
///
/// Reads only `vbuf` and island-private state; writes only island-private
/// scratch. That is the whole safety argument for running every island's
/// phase (a) concurrently, and it is enforced by the signature: nothing here
/// can reach another island's data.
#[allow(clippy::too_many_arguments)]
fn linear_phase_a(
    li: &LinearIsland,
    circuit: &Circuit,
    vbuf: &[f64],
    tnext: f64,
    first: bool,
    state: &mut [f64],
    buf: &mut [f64],
    vfree: &mut [f64],
) {
    let inputs = li.inputs();
    let n_vin = inputs.len();
    for (k, n) in inputs.iter().enumerate() {
        buf[k] = vbuf[n.0 as usize];
    }
    for (k, (id, _, _)) in li.isources().iter().enumerate() {
        if let hauksbee_ir::Device::Isource { kind, .. } = &circuit.devices[id.0 as usize] {
            buf[n_vin + k] = kind.eval(tnext);
        }
    }
    if first {
        li.step(state, buf);
    }
    li.node_voltages(state, buf, vfree);
}

/// Enforce the single-writer-per-slot invariant the Jacobi scatter relies on
/// (plan §3.2, hazard A): every exchange-buffer slot is written by AT MOST one
/// island, and never by an island when the outer loop owns it (a torn rail is
/// written by the scalar balance; ground is never written). Returns the claim
/// mask (slot -> owned by some island) on success, which the caller reuses to
/// detect whether any island READS an island-owned slot (the `coupled` flag).
///
/// The baseline "ideal Vsources are the only cut points" partition satisfies
/// disjointness by construction — the union-find fuses any shared free node
/// into one island — but [`Partition`]'s fields are public and
/// [`PartitionedTransient::try_build_from_partition`] accepts partitions from
/// external decision layers (the decompose analysis, tests, future tearing
/// passes), so the invariant is CHECKED rather than assumed.
fn verify_single_writer(
    owned_sets: &[&[NodeId]],
    outer_written: &[NodeId],
    n_nodes: usize,
) -> Result<Vec<bool>, String> {
    /// Slot written by the outer loop (balance-torn rail), not by any island.
    const OUTER: usize = usize::MAX;
    /// Slot nobody has claimed yet.
    const FREE: usize = usize::MAX - 1;
    let mut owner: Vec<usize> = vec![FREE; n_nodes + 1];
    for n in outer_written {
        if !n.is_ground() {
            owner[n.0 as usize] = OUTER;
        }
    }
    for (isl, set) in owned_sets.iter().enumerate() {
        for n in *set {
            let ni = n.0 as usize;
            match owner[ni] {
                FREE => owner[ni] = isl,
                OUTER => {
                    return Err(format!(
                        "island {isl} claims node {ni}, which the outer loop writes (torn rail)"
                    ))
                }
                other => {
                    return Err(format!(
                        "node {ni} owned by two islands ({other} and {isl}); \
                         a parallel scatter would alias"
                    ))
                }
            }
        }
    }
    Ok(owner
        .iter()
        .map(|&o| o != FREE && o != OUTER)
        .collect())
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

        // Retarget control references (F/H `ctrl_src`, behavioral `I(...)`
        // deps) from GLOBAL device ids to the sub-circuit's LOCAL ids
        // (`clone_remapped` walks nodes only; a DeviceId would otherwise
        // silently point at whatever occupies that index in `sub`). The
        // partitioner demotes a control Vsource from cut to island member
        // precisely so it is present here; if a partition from an external
        // decision layer split them anyway, there is no column for the stamp
        // to write and the only honest move is to refuse the build —
        // `try_build*` then falls back to the exact monolithic path.
        for li in 0..isl.devices.len() {
            for (slot, gctrl) in sub.devices[li].controlling_sources().into_iter().enumerate() {
                let Some(local) = isl.devices.iter().position(|&d| d == gctrl) else {
                    return None;
                };
                sub.devices[li].retarget_controlling_source_slot(slot, DeviceId(local as u32));
            }
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
        let mut ws = Workspace::new(&sub);
        // Same convergence doctrine as the monolithic transient driver: a
        // behavioral source's presence arms the per-step Armijo line search
        // for this island's Newton (a no-op on already-converging steps;
        // B-free islands take the false branch and stay bit-identical).
        if sub
            .devices
            .iter()
            .any(|d| matches!(d, Device::Behavioral { .. }))
        {
            ws.set_tran_line_search(true);
        }
        let n_dev = sub.devices.len();
        let size = ws.layout.size;
        // Owned slots: every mapped non-ground node that is not a boundary
        // input, resolved once to (global node, ws.x index) so the per-sweep
        // scatter is a flat copy with no layout lookups or boundary scans.
        let mut owned = Vec::new();
        for ln in 1..l2g.len() {
            let gn = l2g[ln];
            if gn.is_ground() || boundary.iter().any(|(bn, _)| *bn == gn) {
                continue;
            }
            if let Some(i) = ws.layout.node(NodeId(ln as u32)) {
                owned.push((gn, i));
            }
        }
        Some(NonlinearIsland {
            sub,
            ws,
            state: ReactiveState::new(n_dev),
            x_accepted: vec![0.0; size],
            l2g,
            boundary,
            owned,
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
        seed_sub_reactive(&mut self.state, &self.sub, &self.ws, opts);
        Ok(())
    }

    /// Seed the sub-circuit's accepted state from the exchange buffer's boundary
    /// ESTIMATES, used by the decomposed seed when no global DC exists. Sets the
    /// boundary sources from `vbuf`, then tries the island's own DC. If that DC
    /// also fails (the island is itself an astable with no fixed point), the
    /// boundary estimates are projected onto the island's nodes as the accepted
    /// start and the first marching steps integrate out of it. Never errors: a
    /// per-island DC failure is expected here, not fatal.
    fn seed_from_vbuf(&mut self, vbuf: &[f64], opts: &SolverOptions) {
        for (gn, sid) in &self.boundary {
            let v = vbuf[gn.0 as usize];
            if let Device::Vsource { kind, .. } = &mut self.sub.devices[sid.0 as usize] {
                *kind = SourceKind::Dc(v);
            }
        }
        if dc_operating_point(&mut self.ws, &self.sub, opts).is_err() {
            // Project the boundary estimates onto every mapped node; unmapped
            // internal nodes stay at zero (power-on rest for this window).
            for xi in self.ws.x.iter_mut() {
                *xi = 0.0;
            }
            for ln in 1..self.l2g.len() {
                let gn = self.l2g[ln];
                if gn.is_ground() {
                    continue;
                }
                if let Some(i) = self.ws.layout.node(NodeId(ln as u32)) {
                    self.ws.x[i] = vbuf[gn.0 as usize];
                }
            }
        }
        self.x_accepted.copy_from_slice(&self.ws.x);
        seed_sub_reactive(&mut self.state, &self.sub, &self.ws, opts);
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

    /// Phase (a) of the Jacobi sweep for this island: refresh the boundary
    /// pins from the frozen read buffer and solve the sub-circuit trial state
    /// on this island's own workspace. Writes nothing shared — the owned
    /// outputs stay in `ws.x` until the scatter phase reads them — so every
    /// island's phase (a) can run concurrently.
    fn phase_a(
        &mut self,
        vbuf: &[f64],
        h: f64,
        tnext: f64,
        first: bool,
        opts: &SolverOptions,
    ) -> Result<(), String> {
        self.refresh_boundary(vbuf);
        self.step(h, tnext, first, opts)
    }

    /// Write the island's owned node voltages back to the global buffer.
    /// (The Jacobi sweep scatters through `owned` directly; this remains the
    /// single-buffer write for the rail-balance resolve and the decomposed
    /// seed, where the caller sequences reads and writes explicitly.)
    fn write_back(&self, vbuf: &mut [f64]) {
        for &(gn, xi) in &self.owned {
            vbuf[gn.0 as usize] = self.ws.x[xi];
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

fn seed_sub_reactive(
    state: &mut ReactiveState,
    sub: &Circuit,
    ws: &Workspace,
    opts: &SolverOptions,
) {
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
            // Charge-storing diode (dev-plan 04 §3.1): its slots hold CHARGE,
            // seeded at the island's DC junction voltage.
            Device::Diode { a, k, model, .. }
                if crate::stamp::diode_has_charge(model, &opts.effects) =>
            {
                state.x1[i] = sub_diode_q(model, node_v(ws, *a) - node_v(ws, *k), opts);
                state.x2[i] = state.x1[i];
                state.dx1[i] = 0.0;
            }
            // Charge-storing BJT (dev-plan 04 §3.2): both junction charges
            // seeded at the island's DC intrinsic junction voltages (the
            // sub-workspace's own layout allocated any internal nodes, so
            // the resolution rule is the monolithic one verbatim). Without
            // this arm a torn-island BJT would read zero charge history on
            // every step — the failure the diode arm already guards against.
            Device::Bjt { c, b, e, model, .. }
                if crate::stamp::bjt_has_charge(model, &opts.effects) =>
            {
                let (q_be, q_bc) = sub_bjt_q(ws, id, *c, *b, *e, model, opts);
                state.x1[i] = q_be;
                state.x2[i] = q_be;
                state.dx1[i] = 0.0;
                state.xb[0].x1[i] = q_bc;
                state.xb[0].x2[i] = q_bc;
                state.xb[0].dx1[i] = 0.0;
            }
            // Charge-storing MOSFET (dev-plan 04 §3.3): all four charges
            // (A = Q_gs, xb[0] = Q_gd, xb[1] = Q_bd, xb[2] = Q_bs) seeded at
            // the island's DC junction voltages — the monolithic driver's
            // arm, mirrored for the same reason as the diode's and BJT's.
            Device::Mosfet { d, g, s, b, model, .. }
                if crate::stamp::mos_has_charge(model, &opts.effects) =>
            {
                let (q_gs, q_gd, q_bd, q_bs) = sub_mos_q(ws, *d, *g, *s, *b, model, opts);
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

/// Diode stored charge at junction voltage `vd`, through the same model code
/// the stamp uses (the sub-island mirror of the monolithic driver's helper).
fn sub_diode_q(model: &hauksbee_ir::DiodeModel, vd: f64, opts: &SolverOptions) -> f64 {
    let (idc, gd) =
        crate::stamp::diode_eval(model, vd, opts.model_temp(), opts.effects.temperature);
    crate::stamp::diode_charge(model, vd, idc, gd).0
}

/// BJT stored charges `(Q_be, Q_bc)` at the sub-island's solution, through
/// the same model code and intrinsic-node resolution the stamp uses (the
/// sub-island mirror of the monolithic driver's helper).
fn sub_bjt_q(
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

/// MOSFET stored charges `(Q_gs, Q_gd, Q_bd, Q_bs)` at the island solution,
/// through the same model code the stamp uses (the sub-island mirror of the
/// monolithic driver's helper).
fn sub_mos_q(
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

// The graded-board fixtures (single source of truth in benches/, see the
// header there); `#[path]` resolves against `src/`, not the nested inline
// `tests` module, so the include lives at file level like alloc_audit's.
#[cfg(test)]
#[path = "../benches/fixtures.rs"]
#[allow(dead_code)]
mod test_fixtures;

#[cfg(test)]
mod tests {
    use super::test_fixtures as fixtures;
    use super::*;

    /// INTERNAL TIMING PROBE (ignored; prints, asserts nothing). Breaks a torn
    /// mirror-array step into its components so parallelization decisions are
    /// made on measured hot spots, not guesses. Run with:
    /// `cargo test -p hauksbee-solve --release --lib -- --ignored --nocapture probe_step`
    #[test]
    #[ignore]
    fn probe_step_breakdown() {
        use std::time::Instant;
        // Warm-up pass (cold-binary/page-fault effects otherwise land on the
        // first policy measured), then interleave nothing: each config is
        // rebuilt fresh and the march is long enough to dominate.
        for par in [
            ParallelPolicy::Off,
            ParallelPolicy::Off,
            ParallelPolicy::Threads(1),
            ParallelPolicy::Threads(2),
            ParallelPolicy::Threads(3),
            ParallelPolicy::Threads(4),
            ParallelPolicy::Threads(6),
            ParallelPolicy::Threads(8),
            ParallelPolicy::Off,
        ] {
            let (c, _m) = fixtures::build_shunt_array(240);
            let opts = SolverOptions {
                integration: Integration::Trapezoidal,
                reltol: 1e-9,
                vntol: 1e-9,
                max_newton: 200,
                gmin: 1e-9,
                parallel: par,
                ..fixed_opts(1e-6)
            };
            let mut e = PartitionedTransient::try_build(&c, &opts).expect("tears");
            let t0 = Instant::now();
            e.seed(&c).expect("seed");
            let t_seed = t0.elapsed();
            let dt = 1e-6;
            for li in &mut e.linear {
                li.ensure_cache(dt);
            }
            let steps = 200;
            let (mut t_src, mut t_bal, mut t_commit, mut t_gather) =
                (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            let mut xg = vec![0.0; e.global_x_len()];
            let mut t = 0.0;
            for _ in 0..steps {
                let tnext = t + dt;
                let t0 = Instant::now();
                e.apply_sources(&c, tnext);
                t_src += t0.elapsed().as_secs_f64();
                let t0 = Instant::now();
                e.in_pool(|me| me.step_with_rail_balance(&c, dt, tnext))
                    .expect("step");
                t_bal += t0.elapsed().as_secs_f64();
                let t0 = Instant::now();
                e.commit(&c, dt);
                t_commit += t0.elapsed().as_secs_f64();
                let t0 = Instant::now();
                e.gather_into(&mut xg);
                t_gather += t0.elapsed().as_secs_f64();
                t = tnext;
            }
            println!(
                "{par:?}: seed {:.2}ms | per step: sources {:.1}us, balance {:.1}us, commit {:.1}us, gather {:.1}us",
                t_seed.as_secs_f64() * 1e3,
                t_src / steps as f64 * 1e6,
                t_bal / steps as f64 * 1e6,
                t_commit / steps as f64 * 1e6,
                t_gather / steps as f64 * 1e6,
            );
        }
    }

    /// Disjoint owned sets pass, and the claim mask marks exactly the owned
    /// slots (not the outer-written rail, not unclaimed nodes).
    #[test]
    fn single_writer_accepts_disjoint_ownership() {
        let a = [NodeId(1), NodeId(2)];
        let b = [NodeId(4)];
        let owned: Vec<&[NodeId]> = vec![&a, &b];
        let claimed = verify_single_writer(&owned, &[NodeId(3)], 5).expect("disjoint sets pass");
        assert_eq!(claimed, vec![false, true, true, false, true, false]);
    }

    /// Two islands claiming the same slot is the write-aliasing hazard the
    /// parallel scatter must never see; the check must name the node.
    #[test]
    fn single_writer_rejects_overlapping_ownership() {
        let a = [NodeId(1), NodeId(2)];
        let b = [NodeId(2), NodeId(3)];
        let owned: Vec<&[NodeId]> = vec![&a, &b];
        let err = verify_single_writer(&owned, &[], 4).unwrap_err();
        assert!(
            err.contains("node 2") && err.contains("two islands"),
            "error must name the aliased node: {err}"
        );
    }

    /// An island claiming a balance-torn rail would fight the scalar balance
    /// for the slot (the outer loop writes it); refused explicitly.
    #[test]
    fn single_writer_rejects_island_owning_a_torn_rail() {
        let a = [NodeId(1), NodeId(2)];
        let owned: Vec<&[NodeId]> = vec![&a];
        let err = verify_single_writer(&owned, &[NodeId(2)], 3).unwrap_err();
        assert!(
            err.contains("outer loop") && err.contains("node 2"),
            "error must name the rail conflict: {err}"
        );
    }

    use hauksbee_ir::{Device, SourceKind};

    fn fixed_opts(dt: f64) -> SolverOptions {
        SolverOptions {
            step: StepControl::Fixed { dt },
            ..SolverOptions::default()
        }
    }

    /// END-TO-END proof the build-time single-writer check is live, not
    /// vacuous: a hand-built partition in which two islands both claim node
    /// `b` must be refused at construction. `Partition`'s fields are public
    /// precisely so external decision layers can impose partitions, which is
    /// exactly how a buggy layer could smuggle in an aliasing cut.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "partition ownership violation")]
    fn aliased_partition_is_refused_at_build() {
        let mut c = Circuit::new();
        let a = c.node("a");
        let b = c.node("b");
        let v1 = c.add(Device::Vsource {
            name: "V1".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let r1 = c.add(Device::Resistor {
            name: "R1".into(),
            a,
            b,
            ohms: 1e3,
            tc1: None,
        });
        let r2 = c.add(Device::Resistor {
            name: "R2".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        // Both islands own `b`: island 0 (devices R1, boundary a) and island 1
        // (devices R2, no boundary). `linear: false` routes both through the
        // sub-circuit path; the aliasing is in the ownership, not the physics.
        let part = Partition {
            islands: vec![
                Island {
                    devices: vec![r1],
                    nodes: vec![a, b],
                    linear: false,
                    boundary_in: vec![a],
                },
                Island {
                    devices: vec![r2],
                    nodes: vec![b],
                    linear: false,
                    boundary_in: vec![],
                },
            ],
            sources: vec![v1],
            n_nodes: c.max_node() as usize,
            tears: Vec::new(),
        };
        let opts = fixed_opts(1e-6);
        let _ = PartitionedTransient::try_build_from_partition(&c, &opts, part);
    }

    /// Build the cross-coupled comparator ring: an ODD-inversion feedback loop
    /// split at its (current-free) sense couplings. U1 is non-inverting from x
    /// to y; U2 is inverting from y to x — so no consistent discrete state
    /// exists and a Jacobi relaxation between the two islands flips forever.
    /// This is precisely the hazard the divergence guard exists for: a
    /// FEEDBACK loop imposed on the exchange as if it were feedforward.
    fn comparator_ring() -> (Circuit, Partition) {
        let mut c = Circuit::new();
        let vref = c.node("ref");
        let x = c.node("x");
        let y = c.node("y");
        let vsrc = c.add(Device::Vsource {
            name: "VREF".into(),
            p: vref,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(0.5),
        });
        let u1 = c.add(Device::Comparator {
            name: "U1".into(),
            out: y,
            inp: x,
            inn: vref,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 0.0,
        });
        let u2 = c.add(Device::Comparator {
            name: "U2".into(),
            out: x,
            inp: vref,
            inn: y,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 0.0,
        });
        let part = Partition {
            islands: vec![
                Island {
                    devices: vec![u1],
                    nodes: vec![x, y, vref],
                    linear: false,
                    boundary_in: vec![x, vref],
                },
                Island {
                    devices: vec![u2],
                    nodes: vec![x, y, vref],
                    linear: false,
                    boundary_in: vec![y, vref],
                },
            ],
            sources: vec![vsrc],
            n_nodes: c.max_node() as usize,
            tears: Vec::new(),
        };
        (c, part)
    }

    /// DIVERGENCE GUARD (plan §3.3): a coupling that cannot relax within the
    /// sweep cap must FAIL the step loudly — never hang, never silently accept
    /// a half-converged exchange. The comparator ring flips generation after
    /// generation, so the guard must trip at the cap and name the stalled
    /// node; the error leaves through the same channel as a per-island Newton
    /// failure, which the staged orchestrator escalates to a fused monolithic
    /// re-solve.
    #[test]
    fn unconvergent_coupling_fails_the_step_loudly() {
        let (c, part) = comparator_ring();
        let opts = fixed_opts(1e-6);
        let mut engine = PartitionedTransient::try_build_from_partition(&c, &opts, part)
            .expect("the ring partition is well-formed (disjoint owners), so the build succeeds");
        assert!(engine.coupled, "the ring must register as inter-island coupled");
        let err = engine
            .run_streaming(&c, 10e-6, |_| {})
            .expect_err("an odd-inversion ring can never satisfy the coupling tolerance");
        assert!(
            err.contains("failed to relax"),
            "the guard must refuse, not mislabel: {err}"
        );
    }

    /// The small-island guard (plan §3.4): `ParallelPolicy::Auto` must DECLINE
    /// to build a pool for a board with too few nonlinear islands to amortize
    /// dispatch (the RC-fan shape: many trivial linear islands, zero Newton
    /// solves), must ENGAGE on the mirror array (24 nonlinear blocks), and
    /// `Threads(n)` must force a pool regardless.
    #[test]
    fn auto_policy_declines_small_boards_and_engages_large_ones() {
        // 6-leg RC fan: 6 linear islands off one pinned rail, 0 nonlinear.
        let mut c = Circuit::new();
        let rail = c.node("rail");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: rail,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        for k in 0..6 {
            let mid = c.node(&format!("leg{k}"));
            c.add(Device::Resistor {
                name: format!("R{k}"),
                a: rail,
                b: mid,
                ohms: 1e3,
                tc1: None,
            });
            c.add(Device::Capacitor {
                name: format!("C{k}"),
                a: mid,
                b: NodeId::GROUND,
                farads: 1e-9,
                ic: Some(0.0),
            });
        }
        let opts = fixed_opts(1e-6);
        let engine = PartitionedTransient::try_build(&c, &opts).expect("fan partitions");
        assert!(
            engine.pool.is_none(),
            "Auto must decline a pool for a linear fan with no nonlinear islands"
        );
        let forced = PartitionedTransient::try_build(
            &c,
            &SolverOptions {
                parallel: ParallelPolicy::Threads(2),
                ..opts
            },
        )
        .expect("fan partitions");
        assert!(
            forced.pool.is_some(),
            "Threads(n) must force a pool even below the Auto threshold"
        );
        // The pool-size decision itself, without building boards for every
        // case: below threshold declines, at threshold engages.
        assert!(build_pool(ParallelPolicy::Auto, PAR_MIN_NONLINEAR_ISLANDS - 1).is_none());
        assert!(build_pool(ParallelPolicy::Auto, PAR_MIN_NONLINEAR_ISLANDS).is_some());
        assert!(build_pool(ParallelPolicy::Off, 1_000).is_none());
    }

    /// The positive half of the convergence gate: a genuinely coupled but
    /// FEEDFORWARD partition (linear RC island driving a comparator island
    /// through a current-free sense boundary) relaxes within the cap and
    /// reproduces the monolithic solve. This exercises the relaxation loop for
    /// real — the analyzer's own partitions never couple islands, so without
    /// an imposed partition the loop would be dead code in the test suite.
    #[test]
    fn coupled_feedforward_partition_converges_and_matches_monolithic() {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        let m = c.node("m");
        let vref = c.node("ref");
        let o = c.node("o");
        let v1 = c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let vr = c.add(Device::Vsource {
            name: "VREF".into(),
            p: vref,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(0.5),
        });
        let r1 = c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: m,
            ohms: 1e3,
            tc1: None,
        });
        let c1 = c.add(Device::Capacitor {
            name: "C1".into(),
            a: m,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        let cmp = c.add(Device::Comparator {
            name: "CMP".into(),
            out: o,
            inp: m,
            inn: vref,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 0.0,
        });
        let part = Partition {
            islands: vec![
                Island {
                    devices: vec![r1, c1],
                    nodes: vec![vin, m],
                    linear: true,
                    boundary_in: vec![vin],
                },
                Island {
                    devices: vec![cmp],
                    nodes: vec![o, m, vref],
                    linear: false,
                    boundary_in: vec![m, vref],
                },
            ],
            sources: vec![v1, vr],
            n_nodes: c.max_node() as usize,
            tears: Vec::new(),
        };
        let dt = 5e-8;
        let tstop = 10e-6; // 10 tau: m has settled at 1 V, o latched high
        let opts = fixed_opts(dt);
        let mut engine = PartitionedTransient::try_build_from_partition(&c, &opts, part)
            .expect("well-formed feedforward partition builds");
        assert!(engine.coupled, "the sense boundary must register as coupling");
        let n = engine.global_x_len();
        let mut last = vec![0.0; n];
        engine
            .run_streaming(&c, tstop, |s| last.copy_from_slice(s.x))
            .expect("a feedforward coupling relaxes within the cap");

        // Monolithic oracle on the same circuit.
        let mono = crate::transient::Transient::new(SolverOptions {
            partitioning: crate::options::Partitioning::Off,
            ..opts
        })
        .run(&c, tstop)
        .expect("monolithic oracle");
        let m_mono = mono.final_node(&c, "m").expect("m present");
        let o_mono = mono.final_node(&c, "o").expect("o present");
        // The streamed x carries node k's voltage at index k-1 (gather_into).
        let m_part = last[m.0 as usize - 1];
        let o_part = last[o.0 as usize - 1];
        assert!(
            (m_mono - m_part).abs() <= 1e-6,
            "membrane diverged: mono {m_mono} vs torn {m_part}"
        );
        assert!(
            (o_mono - o_part).abs() <= 1e-6,
            "comparator output diverged: mono {o_mono} vs torn {o_part}"
        );
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
            // Charge-storing diode: the capacitor roll, in CHARGE (dx1 is
            // dQ/dt, the capacitive branch current the trapezoidal history
            // term needs next step).
            Device::Diode { a, k, model, .. }
                if crate::stamp::diode_has_charge(model, &opts.effects) =>
            {
                let q_new = sub_diode_q(model, node_v(ws, *a) - node_v(ws, *k), opts);
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
            // Charge-storing BJT: the diode's roll applied to both charge
            // banks (A = Q_be, B = Q_bc), mirroring the monolithic driver.
            Device::Bjt { c, b, e, model, .. }
                if crate::stamp::bjt_has_charge(model, &opts.effects) =>
            {
                let (q_be, q_bc) = sub_bjt_q(ws, id, *c, *b, *e, model, opts);
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
            // Charge-storing MOSFET: the roll applied to all four banks,
            // mirroring the monolithic driver.
            Device::Mosfet { d, g, s, b, model, .. }
                if crate::stamp::mos_has_charge(model, &opts.effects) =>
            {
                let (q_gs, q_gd, q_bd, q_bs) = sub_mos_q(ws, *d, *g, *s, *b, model, opts);
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
