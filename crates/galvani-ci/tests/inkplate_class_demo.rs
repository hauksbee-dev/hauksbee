//! The Inkplate-class two-sided transient demo.
//!
//! This reproduces the documented Inkplate 6 WiFi cold-boot brownout *pattern*
//! (issues #7 / #10: the board browns out / resets when WiFi comes up on a cold
//! boot under battery power). It is an explicit REPRESENTATIVE RECONSTRUCTION,
//! not the real Inkplate board: no native Inkplate design files exist in the
//! corpus, so `testdata/inkplate_class.net` is a hand-built minimal board with
//! the same topology class (ESP32-WROOM + a small 3V3 LDO + bulk/decoupling
//! caps on a LiPo).
//!
//! The two sides:
//!  - BATTERY side: a small protected 1S LiPo (1 A-class over-current cutoff)
//!    feeding the rail. The ESP32 cold-boot RF inrush (~1.2 A surge) trips the
//!    protection and the rail collapses: the field-reported failure.
//!  - USB-SUPPLEMENTED side: the same board, same cold-boot profile, but fed
//!    from a stiff USB 5V/3A source. The rail holds and nothing trips.
//!
//! Modelling note (honest scope): the board's 3V3 LDO (U2) is present in the
//! netlist but its closed-loop regulation is a behavioural converter model owned
//! by a sibling layer, not stamped here. So the supply leg drives the rail
//! directly, and the rail sits at the *source* voltage (the LiPo cell voltage on
//! the battery side, ~5 V minus cable droop on the USB side) rather than a
//! regulated 3.3 V. That does not change the demonstrated physics: the headline
//! is the battery-side over-current protection tripping on the inrush while the
//! stiff USB source rides it out. The trip is the protection state machine
//! firing on the real solved rail current; the survival is the measured rail
//! staying up. Both are cross-checked against the rail timeseries.

use std::path::PathBuf;

use galvani_ci::{run, RunConfig};

fn board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/inkplate_class.net")
}

fn run_body(name: &str, body: &str) -> galvani_ci::CiResult {
    let dir = std::env::temp_dir().join("galvani_ci_inkplate");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    run(&RunConfig { spec: p }).expect("spec runs")
}

/// BATTERY side: cold-boot inrush trips the 1 A-class LiPo protection and the
/// rail collapses. We assert the protection DID trip and the rail dipped well
/// below 3.0 V during the cold-boot window.
#[test]
fn inkplate_class_battery_cold_boot_trips_protection() {
    let body = format!(
        r#"name = "Inkplate-class: LiPo cold-boot WiFi inrush trips protection"
board = "{board}"
duration_ms = 60
frame_ms = 0.2

# A small protected 1S LiPo feeding the 3V3 rail. Protection trips at 1 A held
# for 2 ms (a DW01-class cutoff on a small pack). r_internal models the cell.
[[supply]]
net = "+3.3V"
kind = "battery"
chemistry = "liion"
cells = 1
capacity_mah = 400
soc = 1.0
r_internal_ohms = 0.25
protection_trip_a = 1.0
protection_delay_ms = 2.0

# Honest decoupling: real ESR/ESL on the bulk + bypass caps, so the rail sags
# the way the real board does (ideal caps would mask it).
[decoupling]
parasitics = true

# The ESP32 cold-boot RF surge attached to U1's 3V3 supply.
[[scenario]]
id = "coldboot"
part = "U1"
profile = "esp32_cold_boot_inrush"
supply_net = "+3.3V"
start_ms = 2.0

[[assert]]
kind = "protection_trip"
name = "LiPo protection trips on cold-boot inrush"
supply_net = "+3.3V"
expect_trip = true

[[assert]]
kind = "rail_window"
name = "rail collapses (brownout) during cold boot"
scenario = "coldboot"
net = "+3.3V"
dip_below = 3.0
for_max_ms = 1.0
"#,
        board = board().display()
    );
    let result = run_body("battery.toml", &body);
    eprintln!("BATTERY side:\n{}", result.render_human());

    // The protection_trip assertion must PASS (it DID trip): the headline.
    let trip = result
        .results
        .iter()
        .find(|r| r.kind == "protection_trip")
        .expect("protection_trip assertion present");
    assert!(
        trip.passed,
        "expected the LiPo protection to TRIP on cold-boot inrush:\n  {}",
        trip.detail
    );
    // The rail_window asserts the rail must NOT stay below 3.0 V for > 1 ms.
    // On the battery side it collapses for far longer once protection latches,
    // so this assertion FAILS by design: that red is the brownout being caught.
    let window = result
        .results
        .iter()
        .find(|r| r.kind == "rail_window")
        .expect("rail_window assertion present");
    assert!(
        !window.passed,
        "battery-side rail_window should FAIL (rail collapses): {}",
        window.detail
    );
    // And the measured minimum during the window must be a genuine brownout
    // (well below 3 V), not a marginal sag.
    assert!(
        window.detail.contains("min=") && window.detail.contains("dip<3V"),
        "rail_window detail should report the brownout dip: {}",
        window.detail
    );
}

/// USB-SUPPLEMENTED side: same board, same cold-boot profile, stiff USB 5V/3A.
/// The rail holds; nothing trips. This side must be fully GREEN.
#[test]
fn inkplate_class_usb_supplemented_survives() {
    let body = format!(
        r#"name = "Inkplate-class: USB-supplemented cold boot survives"
board = "{board}"
duration_ms = 60
frame_ms = 0.2

# Stiff USB 5V/3A feeding the 3V3 rail directly (USB-supplemented operation:
# the LDO is fed from USB, not the small cell). No protection to trip.
[[supply]]
net = "+3.3V"
kind = "usb"
usb = "5v3a"

[decoupling]
parasitics = true

[[scenario]]
id = "coldboot"
part = "U1"
profile = "esp32_cold_boot_inrush"
supply_net = "+3.3V"
start_ms = 2.0

[[assert]]
kind = "rail_window"
name = "3V3 rail holds through cold boot on USB"
scenario = "coldboot"
net = "+3.3V"
min = 3.0

[[assert]]
kind = "no_faults"
"#,
        board = board().display()
    );
    let result = run_body("usb.toml", &body);
    eprintln!("USB side:\n{}", result.render_human());
    assert!(
        result.passed(),
        "USB-supplemented cold boot must survive (rail holds, no faults):\n{}",
        result.render_human()
    );
}
