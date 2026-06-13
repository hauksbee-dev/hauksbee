//! Corpus calibration: a standard ESP32 burst scenario on the real Olimex
//! ESP32-EVB (REV-L) must PASS.
//!
//! This is the calibration side of the transient layer. The Olimex EVB is a
//! robustly-supplied board: in the CI model its +3.3V / +5V rails are
//! externally supplied (a wall adapter / USB, not a tiny cell), so a normal
//! ESP32 WiFi-TX burst should NOT brown it out. If this went red, the load
//! model or the supply model would be miscalibrated (too pessimistic). It
//! passing is what lets the Inkplate-class red be trusted as a real failure
//! rather than an artefact.
//!
//! Corpus-gated: skipped when the board-corpus symlink is absent;
//! `HAUKSBEE_REQUIRE_CORPUS=1` turns absence into a hard failure.

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous")
}

fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

fn run_body(name: &str, body: &str) -> hauksbee_ci::CiResult {
    let dir = std::env::temp_dir().join("hauksbee_ci_olimex_burst");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    run(&RunConfig { spec: p }).expect("spec runs")
}

#[test]
fn olimex_evb_wifi_burst_on_robust_supply_passes() {
    let board = corpus().join("olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch");
    if !board.exists() {
        assert!(!require_corpus(), "corpus required but Olimex EVB missing");
        eprintln!("corpus Olimex EVB missing; skipping");
        return;
    }

    // The ESP32-WROOM module's 3V3 supply is the +3.3V rail. We attach the WiFi
    // burst load to the +3.3V rail explicitly (robust against the part's exact
    // reference designator), and supply +3.3V / +5V from a stiff wall adapter
    // (the EVB is wall/USB powered, with a 3A-class AMS1117 LDO and bulk caps).
    let body = format!(
        r#"name = "Olimex ESP32-EVB: WiFi burst on robust supply (calibration)"
board = "{board}"
duration_ms = 40
frame_ms = 0.2

[[supply]]
net = "+3.3V"
kind = "wall"
volts = 3.3
r_out_ohms = 0.1
ripple_vpp = 0.02
ripple_hz = 100.0

[[supply]]
net = "+5V"
kind = "wall"
volts = 5.0
r_out_ohms = 0.1

# Honest decoupling on the real board's caps.
[decoupling]
parasitics = true

# A standard ESP32 WiFi-TX burst train (240 mA bursts over a 40 mA baseline)
# attached to the +3.3V rail. We bind it by an explicit supply net rather than a
# part ref so the test is robust to the EVB's designators; the rail is the real
# net the ESP32 module's VDD sits on.
[[profile]]
id = "burst_load"
description = "ESP32 WiFi-TX burst train attached to the 3V3 rail"
[[profile.segment]]
level_a = 0.040
rise_s = 0.001
duration_s = 0.0
[[profile.segment]]
level_a = 0.240
rise_s = 0.0005
duration_s = 0.010
period_s = 0.100
idle_a = 0.040

[[scenario]]
id = "burst"
part = "U1"
profile = "burst_load"
supply_net = "+3.3V"
start_ms = 1.0

[[assert]]
kind = "rail_window"
name = "3V3 holds through WiFi burst on a robust supply"
scenario = "burst"
net = "+3.3V"
min = 3.0
dip_below = 3.1
for_max_ms = 5.0

[[assert]]
kind = "no_faults"
"#,
        board = board.display()
    );

    let result = run_body("olimex_burst.toml", &body);
    eprintln!("{}", result.render_human());
    assert!(
        result.passed(),
        "Olimex EVB WiFi-burst calibration must be GREEN (robust supply rides the burst):\n{}",
        result.render_human()
    );
}
