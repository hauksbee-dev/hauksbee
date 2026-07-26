//! The decomposition engine: automatic, self-verifying circuit tearing.
//!
//! Large boards defeat monolithic MNA not because they are large but because
//! they are coupled in shapes the solver cannot exploit: hundreds of nonlinear
//! blocks fused through a handful of shared rails and zero-current control
//! signals. Those same shapes are what makes them decomposable. A supply rail
//! feeding N blocks through one series impedance is a bordered-block-diagonal
//! system: N independent solves plus one scalar balance equation (diakoptics;
//! Kron 1953). A comparator output that drives only switch-select pins carries
//! information but zero current, so the downstream circuit can be solved with
//! the upstream waveform replayed as a source, exactly.
//!
//! This module family makes the circuit itself say where it tears, replacing
//! the board-specific `tarski_decomp` implementation whose net-name lists and
//! tuned constants proved the concept (`docs/learn/tarski-saga.md`
//! is the full story; `docs/dev-plans/02-tearing-architecture.md` is the
//! design this implements).
//!
//! Submodules land in dependency order:
//! * [`conduction`]: terminal classification and the conduction graph, the
//!   primitive everything else rests on. A tear is only exact if electrical
//!   reachability is computed over terminals that actually carry current.
//! * [`feedforward`]: sense-boundary discovery, the reverse-path proof via
//!   strongly-connected components, and the stage DAG that orders the solves.
//! * [`rails`]: stiff-rail detection with a cost-model decision (and the
//!   convergence-escalation override) for balance-tear candidates.
//! * [`stiff`]: stiff-node tear candidates, the conducted-both-sides cut
//!   nodes (the flagship's hidden `V_out` nets) that neither free-tear
//!   discovery nor the rail pass can see. Detection only nominates; the
//!   orchestrator measures stiffness at capture time.
//! * [`drivers`]: the absorption pass that copies small linear driver groups
//!   into their consumers instead of tearing (exact: sense-only boundaries
//!   carry zero current, so even replication cannot disagree).
//! * [`verify`]: tear certificates (the honesty object) and the boundary
//!   completeness gate that decides whether a decomposition may be trusted
//!   at all.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/decompose.md

pub mod conduction;
pub mod drivers;
pub mod feedforward;
pub mod rails;
pub mod stiff;
pub mod verify;

pub use conduction::{ConductionGraph, SenseEdge};
pub use drivers::{driver_assignments, DriverAssignment, DriverPolicy};
pub use feedforward::{FreeTearEdge, StageDag};
pub use rails::{detect_balance_tears, BalanceTearCandidate, RailPolicy, TearDecision, TearMotive};
pub use stiff::{detect_stiff_candidates, StiffCandidate, StiffPolicy};
pub use verify::{
    Decomposition, Evidence, RefusedAnalysis, TearCertificate, TearKind, TearRecord, ToleranceClaim,
};
