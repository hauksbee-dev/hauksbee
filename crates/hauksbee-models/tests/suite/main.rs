//! Every hauksbee-models integration test, in one binary.
//!
//! Cargo builds one test executable per file directly under `tests/`, and each
//! one links the whole crate graph again. Twelve files meant twelve links for
//! a crate whose tests run in under a second. Declaring them as modules of a
//! single target keeps the files separate to read and edit while paying the
//! link cost once.
//!
//! Adding a test file: drop it in `tests/suite/` and add a `mod` line here. A
//! file that is not listed does not run, so the list is the source of truth.

mod analog_active_resolve;
mod bjt_regex_polarity;
mod codex_behavioral_fixture;
mod digital_pin_maps;
mod exact_override_tiebreak;
mod pack_format;
mod pack_layering;
mod pack_store;
mod power_fet_afe_resolve;
mod power_ic_resolve;
mod user_dir_layering;
mod vreg_78xx_resolve;
mod vreg_79xx_negative;
