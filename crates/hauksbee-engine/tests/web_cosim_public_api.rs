//! Downstream compile fixture for the public web co-sim presentation type.
//!
//! `WebCosimSection` is constructible outside `hauksbee-engine`; this fixture
//! pins the complete first-release literal and the repository's planned 0.1.0
//! release line together.

use hauksbee_engine::frontdoor::WebCosimSection;
use hauksbee_engine::result::CosimFallbackWindow;
use hauksbee_engine::scheduler::TimingCoverage;

#[test]
fn version_0_1_public_literal_includes_the_interactive_coverage_fields() {
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
        timing_coverage: vec![TimingCoverage {
            mcu_ref: "U1".to_string(),
            backend: "simavr:atmega328p".to_string(),
            cycle_exact: true,
            timestamp_precision_s: 62.5e-9,
            minimum_guaranteed_pulse_s: 62.5e-9,
            chunk_s: 1e-3,
        }],
        timing_refusals: vec!["transition budget exceeded".to_string()],
        fallback_windows: vec![CosimFallbackWindow {
            start_s: 0.001,
            end_s: 0.002,
            method: "backward-euler".to_string(),
            fidelity_note: "first-order".to_string(),
            error_estimate_v: Some(0.012),
        }],
        boot_gates: Vec::new(),
        firmware_exercised: true,
        substituted: false,
        error_budget: None,
    };

    let json = serde_json::to_value(section).expect("public report type serializes");
    assert_eq!(json["timing_coverage"][0]["mcu_ref"], "U1");
    assert_eq!(json["timing_refusals"][0], "transition budget exceeded");
    assert_eq!(json["fallback_windows"][0]["method"], "backward-euler");
}
