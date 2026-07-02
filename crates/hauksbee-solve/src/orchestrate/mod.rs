//! The orchestration layer: executors for what `decompose` decided.
//!
//! [`crate::decompose`] is pure analysis: it looks at a circuit and produces
//! a [`Decomposition`](crate::decompose::verify::Decomposition) saying where
//! the circuit tears, why each tear is exact, and what was refused. Nothing
//! in that layer solves anything. This layer is the other half: given a
//! decomposition, actually run it.
//!
//! The split is deliberate and load-bearing. The original `tarski_decomp`
//! fused deciding and executing into one 781-line function, and the saga
//! (`docs/dev-plans/research/tarski-saga.md`) records what that cost: every
//! decision was invisible (no way to ask "what did it tear and why" without
//! reading a debugger), and every executor bug looked like a decision bug
//! (and vice versa; the STEP-1 dead-membrane hunt burned days deciding which
//! side was lying). Here a decision is a datum with a certificate, an
//! execution is a mechanism with a gate, and each can be tested against the
//! other's contract.
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
pub mod staged;

pub use balance::{settle_rails, BalancePolicy, BalanceReport, RailChannel, RailLoads};
pub use staged::{run_staged, StagedResult};
