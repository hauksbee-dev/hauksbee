//! Downstream compile fixtures for the public interactive presentation types.
//!
//! These literals are copied from `main`, before interactive coverage parity
//! was added. They must remain source-compatible: an additive JSON field does
//! not justify breaking a Rust consumer at the planned first release.

use hauksbee_engine::frontdoor::WebCosimSection;
use hauksbee_engine::tui::cosim::CosimUpdate;
use std::path::{Path, PathBuf};

#[test]
fn unchanged_main_era_web_cosim_literal_still_compiles() {
    let section = WebCosimSection {
        ran: true,
        seconds_simulated: 0.2,
        uart_output: String::new(),
        findings: Vec::new(),
        gpio_nets: Vec::new(),
        analog_valid: true,
        failed_windows: Vec::new(),
        spi_framing: Vec::new(),
        boot_gates: Vec::new(),
        firmware_exercised: true,
        substituted: false,
        error_budget: None,
    };

    let json = serde_json::to_value(section).expect("public report type serializes");
    assert_eq!(json["ran"], true);
}

#[test]
fn unchanged_main_era_tui_update_literal_still_compiles() {
    let update = CosimUpdate {
        sim_ms: 1.0,
        wall_s: 0.1,
        chunk_ms: 0.5,
        uart_lines: Vec::new(),
        gpio_nets: Vec::new(),
        uart_seen: false,
        gpio_active: false,
        gpio_driven: false,
        substitution: None,
        analog_valid: true,
        heuristic_spi_buses: vec!["SPI1".to_string()],
        failed_chunk_count: 0,
        done: false,
        error: None,
        net_voltages: Default::default(),
    };

    assert_eq!(update.heuristic_spi_buses, ["SPI1"]);
}

#[test]
fn unchanged_main_era_tui_entrypoint_signature_still_compiles() {
    let _run: fn(&Path, &str, Option<&Path>, Option<PathBuf>) -> anyhow::Result<()> =
        hauksbee_engine::tui::run;
}

#[test]
fn public_firmware_analysis_exposes_the_same_typed_coverage_as_json() {
    let _detailed: fn(&str, &[u8], &str, &[u8]) -> hauksbee_engine::WebFirmwareAnalysis =
        hauksbee_engine::analyze_with_firmware_detailed;

    fn consume(result: hauksbee_engine::WebFirmwareAnalysis) {
        let _report = result.report;
        let _timing = result.coverage.timing_coverage;
        let _refusals = result.coverage.timing_refusals;
        let _fallbacks = result.coverage.fallback_windows;
    }
    let _ = consume;
}
