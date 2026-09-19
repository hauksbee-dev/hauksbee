//! Shared helpers for the crate's split integration-test files.

use hauksbee_ir::evidence::RunDate;

/// 2026-08-01. A run builds its own clock reading; a fixed value keeps tests
/// off the wall clock. Shared by every test file that needs a stable "now".
pub fn today() -> RunDate {
    RunDate::from_epoch_days(20_666)
}
