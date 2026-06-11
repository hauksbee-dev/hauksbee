//! Galvani transient circuit solver.
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
//! use galvani_ir::{Circuit, Device, NodeId, SourceKind};
//! use galvani_solve::{SolverOptions, Transient};
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

mod newton;
mod options;
mod sparse;
mod stamp;
mod system;
mod transient;

pub use newton::{dc_operating_point, Workspace};
pub use options::{DeviceEffects, Integration, SolverOptions, StepControl};
pub use sparse::{SparseMatrix, Symbolic};
pub use system::Layout;
pub use transient::{StepSample, Transient, Waveforms};
