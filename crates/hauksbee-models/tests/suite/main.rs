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

mod abs_max_provenance;
mod agent_cli_invocation;
mod analog_active_resolve;
mod api_backend;
mod avr_package_and_clock_cards;
mod bat54_family;
mod bjt_regex_polarity;
mod boot_mode_cards;
mod bridge_dac_mcu_cards;
mod codex_behavioral_fixture;
mod codex_prompt_delivery;
mod connector_rating_resolve;
mod corpus_batch_resolve;
mod corpus_coverage_ratchet;
mod corpus_gap_cards;
mod declared_coverage;
mod digital_pin_maps;
mod diode_1n4148_packages;
mod docs_generated;
mod espressif_module_pad_tables;
mod exact_override_tiebreak;
mod external_five_coverage;
mod extract_model_choice;
mod ina18x_pinouts;
mod layer_docs;
mod ldo_logic_cards;
mod lm1117_cards;
mod mcu_adc_and_strap_tables;
mod mcu_electrical_cards;
mod mito_open_part_noise;
mod module_carrier_resolve;
mod negative_rail_validation;
mod official_active;
mod official_converters_analog;
mod official_diodes;
mod official_env1;
mod official_env2;
mod official_io_drivers;
mod official_mcu1;
mod official_mcu2;
mod official_memory;
mod official_motion_power;
mod official_power;
mod official_serial_logic;
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
mod schottky_rectifier_cards;
mod si5351_cards;
mod spi_cs_pin_roles;
mod stm32f0_nrst;
mod user_dir_layering;
mod vreg_78xx_resolve;
mod vreg_79xx_negative;
mod zener_family;

use hauksbee_models::{ComponentQuery, ModelEntry, ModelLibrary, OperatingEnvelope, Resolution};

/// Resolve against the shared built-in library with only the given fields set.
fn resolve_query(value: Option<&str>, mpn: Option<&str>, footprint: Option<&str>) -> Resolution {
    ModelLibrary::builtin_shared().resolve(&ComponentQuery {
        value: value.map(Into::into),
        mpn: mpn.map(Into::into),
        footprint: footprint.map(Into::into),
        ..Default::default()
    })
}

/// The model [`resolve_query`] binds, panicking with the query when none does.
fn expect_query(value: Option<&str>, mpn: Option<&str>, footprint: Option<&str>) -> ModelEntry {
    resolve_query(value, mpn, footprint)
        .model
        .unwrap_or_else(|| {
            panic!("value {value:?} / mpn {mpn:?} / footprint {footprint:?} did not resolve")
        })
}

/// The id of the model [`resolve_query`] binds, or `None` when nothing binds.
fn query_id(value: Option<&str>, mpn: Option<&str>, footprint: Option<&str>) -> Option<String> {
    resolve_query(value, mpn, footprint).model.map(|m| m.id)
}

/// Resolve a value-only fixture without inventing MPN or footprint metadata.
fn resolve_value(value: &str) -> Resolution {
    resolve_query(Some(value), None, None)
}

fn expect_model(value: &str) -> ModelEntry {
    resolve_value(value)
        .model
        .unwrap_or_else(|| panic!("{value} did not resolve to any model"))
}

/// Resolve exactly the way a layout-only board does. `mpn` is set to the value
/// string as well as `value`, because that is what `binder.rs` does for a
/// component with no MPN property, which is every component on a `.kicad_pcb`,
/// a `.brd` or a gerber set. Match rules are AND-ed, so leaving `mpn` empty would
/// make every entry carrying an `mpn_re` resolve to nothing, testing a query
/// shape no board produces.
fn resolve_part(value: &str) -> Option<ModelEntry> {
    resolve_query(Some(value), Some(value), None).model
}

/// A numeric param the entry must carry.
fn param(m: &ModelEntry, k: &str) -> f64 {
    m.params
        .get_f64(k)
        .unwrap_or_else(|| panic!("model {:?} has no param {k}", m.id))
}

/// Shockley forward voltage at `i` with series resistance, from the entry's own
/// `n`/`is`/`rs` and the thermal voltage `vt` the caller's datasheet assumes.
fn vf_at(m: &ModelEntry, i: f64, vt: f64) -> f64 {
    (param(m, "n") * vt).mul_add((i / param(m, "is")).ln(), i * param(m, "rs"))
}

/// `pad=role` pairs from one whitespace-separated spec, for [`assert_pins`]
/// and [`assert_pin_map`]: `"1=gnd 2=in 3=out"`.
fn pins(spec: &str) -> Vec<(&str, &str)> {
    spec.split_whitespace()
        .map(|pair| pair.split_once('=').expect("pad=role"))
        .collect()
}

/// Assert each listed pad carries its role (other pads are not checked).
fn assert_pins(m: &ModelEntry, pairs: &[(&str, &str)]) {
    for (pad, role) in pairs {
        assert_eq!(
            m.pins.get(*pad).map(String::as_str),
            Some(*role),
            "{}: pad {pad} must be {role} (datasheet pinout), got {:?}",
            m.id,
            m.pins.get(*pad)
        );
    }
}

/// Assert the entry id and the exact, complete pad->role map (no extra pads,
/// no missing pads, no swaps).
fn assert_pin_map(m: &ModelEntry, id: &str, expected: &[(&str, &str)]) {
    assert_eq!(m.id, id, "resolved to the wrong model id");
    assert_pins(m, expected);
    assert_eq!(
        m.pins.len(),
        expected.len(),
        "{id}: pad map has {} entries, the datasheet package has {}",
        m.pins.len(),
        expected.len(),
    );
}

/// Whether `m` declares a supply range on `pin` of exactly `min..=max` whose
/// basis cites `cites`; `abs_max` is also checked when `Some`.
fn has_supply_range(
    m: &ModelEntry,
    pin: &str,
    (min, max): (f64, f64),
    abs_max: Option<f64>,
    cites: &str,
) -> bool {
    m.envelope.iter().any(|entry| {
        matches!(
            entry,
            OperatingEnvelope::SupplyRange { pin: p, min_v, max_v, abs_max_v, basis, .. }
                if p == pin
                    && min_v.to_bits() == min.to_bits()
                    && max_v.to_bits() == max.to_bits()
                    && abs_max.is_none_or(|a| *abs_max_v == Some(a))
                    && basis.contains(cites)
        )
    })
}
