//! Hauksbee transient circuit solver.
//!
//! A dense/small-sparse modified-nodal-analysis engine with real device
//! physics. The pipeline: build a [`Workspace`] from a [`Circuit`] (which
//! analyzes the fixed sparsity once), find the DC operating point with Newton
//! plus homotopy fallbacks, then time-march with companion models and adaptive
//! step control. Every physical effect is a toggle in [`SolverOptions`].
//!
//! This is the foundation the architecture's partitioning pass builds on: the
//! IR already tags each device linear / event-driven, and the solver keeps a
//! frozen pattern + reusable factorization so an island solver can reuse the
//! same machinery per partition. State-space / compilation come later.
//!
//! ```
//! use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
//! use hauksbee_solve::{SolverOptions, Transient};
//!
//! // RC low-pass: V1 -> R -> C -> gnd.
//! let mut c = Circuit::new();
//! let vin = c.node("in");
//! let out = c.node("out");
//! c.add(Device::Vsource { name: "V1".into(), p: vin, n: NodeId::GROUND,
//!     kind: SourceKind::Dc(1.0) });
//! c.add(Device::Resistor { name: "R1".into(), a: vin, b: out, ohms: 1e3, tc1: None });
//! c.add(Device::Capacitor { name: "C1".into(), a: out, b: NodeId::GROUND,
//!     farads: 1e-6, ic: Some(0.0) });
//!
//! let t = Transient::new(SolverOptions::fixed(1e-5));
//! let wf = t.run(&c, 5e-3).unwrap();
//! // After ~5 time constants the output is near 1 V.
//! assert!(wf.final_node(&c, "out").unwrap() > 0.99);
//! ```

mod ac;
// S1 allocation-hygiene enforcement gate (plan §4.4): a counting global
// allocator + zero-alloc per-step-loop tests, compiled ONLY for the crate's
// own test binary. Non-test builds never see it.
#[cfg(test)]
mod alloc_audit;
#[cfg(test)]
#[global_allocator]
static AUDIT_ALLOC: alloc_audit::CountingAlloc = alloc_audit::CountingAlloc;

mod census;
mod cmatrix;
mod diagnostics;
pub mod decompose;
mod linear;
mod loop_stability;
mod newton;
mod options;
pub mod orchestrate;
mod partition;
mod partitioned;
mod plan;
pub mod sim;
mod sparse;
mod stamp;
mod system;
mod transient;

pub use ac::{AcAnalysis, AcPoint, AcResponse, AcSpec, Sweep};
pub use cmatrix::ComplexSystem;
pub use linear::LinearIsland;
pub use loop_stability::{margins_from_bode, phase_margin, LoopStability, StabilityMargins};
pub use newton::{dc_operating_point, dc_operating_point_seeded, Workspace};
pub use diagnostics::{peek_strategy_activations, take_strategy_activations};
pub use options::{
    DcInit, DeviceEffects, EventRetryTuning, Integration, Partitioning, RobustnessLadder,
    SolverOptions, StepControl, Strategy,
};
pub use partition::{Island, Partition};
pub use plan::StampPlan;
pub use sim::{default_probes, run_op, run_tran, Probe, SimOutput};
pub use sparse::{SparseMatrix, Symbolic};
pub use system::Layout;
pub use transient::{StepSample, Transient, Waveforms};
