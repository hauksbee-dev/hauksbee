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
mod api_backend;
mod bjt_regex_polarity;
mod codex_behavioral_fixture;
mod codex_prompt_delivery;
mod connector_rating_resolve;
mod corpus_batch_resolve;
mod corpus_coverage_ratchet;
mod declared_coverage;
mod digital_pin_maps;
mod exact_override_tiebreak;
mod external_five_coverage;
mod extract_model_choice;
mod layer_docs;
mod mito_open_part_noise;
mod module_carrier_resolve;
mod negative_rail_validation;
mod pack_format;
mod pack_layering;
mod pack_store;
mod passive_class;
mod passive_rating_resolve;
mod pedalboard_core_resolve;
mod peripheral_behavior_resolve;
mod power_fet_afe_resolve;
mod power_ic_resolve;
mod quad_opamp_pinout;
mod schottky_1n58xx;
mod spi_cs_pin_roles;
mod user_dir_layering;
mod vreg_78xx_resolve;
mod vreg_79xx_negative;
mod zener_family;
