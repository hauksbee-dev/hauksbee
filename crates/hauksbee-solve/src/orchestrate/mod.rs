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
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/orchestrate.md

pub mod balance;
pub mod capture;
pub mod staged;

pub use balance::{settle_rails, BalancePolicy, BalanceReport, RailChannel, RailLoads};
pub use capture::{
    execute_composed_group, execute_stiff_group, execute_stiff_group_held,
    execute_stiff_group_held_capped, BoundaryKind, CapturePolicy, ComposedPolicy, StiffExecution,
    StiffOutcome,
};
pub use staged::{run_staged, StagedResult};

/// March `engine` over `circuit` to `tstop`, collecting every accepted step
/// into a [`Waveforms`] whose node index 0 is ground. The torn executors share
/// this because a partitioned engine only streams samples; it never assembles
/// a waveform table of its own.
fn collect_waveforms(
    engine: &mut crate::partitioned::PartitionedTransient,
    circuit: &hauksbee_ir::Circuit,
    tstop: f64,
) -> crate::error::SolveResult<crate::transient::Waveforms> {
    let n_nodes = circuit.node_count();
    let mut wf = crate::transient::Waveforms {
        time: Vec::new(),
        node_voltages: vec![Vec::new(); n_nodes],
        branch_currents: Vec::new(),
    };
    engine.run_streaming(circuit, tstop, |s| {
        wf.time.push(s.time);
        for node in 0..n_nodes {
            let v = if node == 0 {
                0.0
            } else {
                s.x.get(node - 1).copied().unwrap_or(0.0)
            };
            wf.node_voltages[node].push(v);
        }
    })?;
    Ok(wf)
}
