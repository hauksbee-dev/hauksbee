//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "compat_drift.rs"]
mod compat_drift;
#[path = "evidence_release_contract.rs"]
mod evidence_release_contract;
#[path = "evidence_surface.rs"]
mod evidence_surface;
#[path = "mos_classification.rs"]
mod mos_classification;
#[path = "node_interning.rs"]
mod node_interning;
#[path = "serde_roundtrip.rs"]
mod serde_roundtrip;
