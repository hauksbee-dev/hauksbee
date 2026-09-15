//! The orchestration layer: executors for what `decompose` decided.
//!
//! [`crate::decompose`] is pure analysis: it looks at a circuit and produces
//! a [`Decomposition`](crate::decompose::verify::Decomposition) saying where
//! the circuit tears, why each tear is exact, and what was refused. Nothing
//! in that layer solves anything. This layer is the other half: given a
//! decomposition, actually run it.
//!
//! The split is deliberate and load-bearing. Fusing deciding and executing
//! into one function makes every decision invisible (no way to ask "what did
//! it tear and why" without a debugger) and makes every executor bug look
//! like a decision bug, and vice versa. Here a decision is a datum with a
//! certificate, an execution is a mechanism with a gate, and each can be
//! tested against the other's contract.
//!
//! Submodules:
//! * [`balance`]: the scalar rail-balance outer loop for balance tears. The
//!   proven mechanics from the partitioned engine (secant iteration,
//!   voltage-referred tolerance, the gmin double-count correction), extracted
//!   so every executor closes rail KCL through one audited implementation.
//! * [`staged`]: the capture/replay executor for the stage DAG: solve
//!   upstream groups in dependency order, capture free-tear waveforms on the
//!   accepted-step grid, replay them downstream as PWL sources, absorb
//!   driver groups by copying, and fill the certificate's capture-grid
//!   tolerance with the grid actually used.
//!

pub mod balance;
pub mod capture;
pub mod staged;

pub use balance::{settle_rails, BalancePolicy, BalanceReport, RailChannel, RailLoads};
pub use capture::{
    execute_composed_group, execute_stiff_group, execute_stiff_group_held_capped, BoundaryKind,
    CapturePolicy, ComposedPolicy, StiffExecution, StiffOutcome,
};
pub use staged::{run_staged, StagedResult};

/// The uniform accepted-step grid a fixed-dt run marches (mirrors the run
/// loop: the last step shortens to land exactly on tstop).
pub(crate) fn uniform_grid(dt: f64, tstop: f64) -> Vec<f64> {
    let mut grid = vec![0.0];
    let mut t = 0.0;
    let eps = dt * 1e-9;
    while t < tstop - eps {
        t += dt.min(tstop - t);
        grid.push(t);
    }
    grid
}

/// First-order-hold sample of a captured series at time `t` (clamped at the
/// ends, exactly like PWL replay).
pub(crate) fn lerp_at(times: &[f64], vals: &[f64], t: f64) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    match times.binary_search_by(|x| x.partial_cmp(&t).expect("non-finite sample time")) {
        Ok(i) => vals[i],
        Err(0) => vals[0],
        Err(i) if i >= times.len() => *vals.last().unwrap(),
        Err(i) => {
            let (t0, t1) = (times[i - 1], times[i]);
            let w = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            vals[i - 1] + w * (vals[i] - vals[i - 1])
        }
    }
}

/// One node of a run, first-order-hold resampled onto `grid` (the reading a
/// replay consumer gets).
pub(crate) fn resample(wf: &crate::transient::Waveforms, node: usize, grid: &[f64]) -> Vec<f64> {
    grid.iter()
        .map(|&t| lerp_at(&wf.time, &wf.node_voltages[node], t))
        .collect()
}
