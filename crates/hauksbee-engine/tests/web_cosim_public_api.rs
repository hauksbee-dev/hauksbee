//! Downstream compile fixtures for the public interactive presentation types.
//!
//! These literals are copied from `main`, before interactive coverage parity
//! was added. They must remain source-compatible: an additive JSON field does
//! not justify breaking a Rust consumer at the planned first release.

use hauksbee_engine::frontdoor::WebCosimSection;
use hauksbee_engine::tui::cosim::CosimUpdate;

#[test]
fn unchanged_main_era_web_cosim_literal_still_compiles() {
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        "0.1.0",
        "the unreleased workspace and its first release tag stay on one version line"
    );

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
