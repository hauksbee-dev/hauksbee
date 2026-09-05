//! Compatibility path for the bind report.
//!
//! The bind model and its table rendering now live beside the `--report`
//! surface that prints them, in [`crate::reports::bind`], so a component's
//! per-row verdict and the only renderer of those rows sit in one module rather
//! than two. This module re-exports that model under the historical
//! `hauksbee_engine::report::*` path, which downstream crates, integration
//! tests and the crate root all still import. Add new bind items to
//! [`crate::reports::bind`]; add a line here only when an existing name must
//! keep working from the old path.

pub use crate::reports::bind::{natural_ref_key, BindOutcome, BindReport, BindRow};

pub(crate) use crate::reports::bind::is_active_fallback_device;
