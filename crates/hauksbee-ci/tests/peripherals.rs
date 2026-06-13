//! Peripheral integration through the hauksbee-ci spec surface:
//!   - a timed pushbutton press makes a net respond (the second required proof);
//!   - a VCD sink records the right transitions to a gtkwave-compatible file
//!     (the third required proof).

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn testdata(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(rel)
}

fn write_tmp(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("hauksbee_ci_periph_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn ci_button_press_drives_a_net() {
    // The checked-in spec: press BTN at 100ms, release at 150ms, on a board
    // where BTN is pulled to +5V through 10k. The net must settle back high and
    // toggle exactly twice from the timed press/release.
    let cfg = RunConfig {
        spec: testdata("ci/button_press.toml"),
    };
    let result = run(&cfg).expect("run button_press spec");
    assert!(
        result.passed(),
        "button-press spec should be green:\n{}",
        result.render_human()
    );
    // Specifically: the toggle assertion must be present and pass (proves the
    // net moved in response to the timed button).
    assert!(
        result.results.iter().any(|r| r.kind == "toggle" && r.passed),
        "the BTN toggle assertion must pass"
    );
}

#[test]
fn ci_vcd_sink_records_transitions() {
    // A board whose net is driven by a timed stimulus (square pulse train). A
    // vcd_sink logs that net; we assert a known transition count and that the
    // written VCD is gtkwave-shaped.
    let board = testdata("boards/vcd_pulse.kicad_pcb");
    let board_txt = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 3 "CLK")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "CLK"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;
    std::fs::create_dir_all(board.parent().unwrap()).unwrap();
    std::fs::write(&board, board_txt).unwrap();

    let vcd_out = std::env::temp_dir()
        .join("hauksbee_ci_periph_tests")
        .join("clk.vcd");
    let _ = std::fs::remove_file(&vcd_out);

    // 1 kHz square wave on CLK for 10 ms via a PWL stimulus pulse train: 0/5 V
    // every 0.5 ms. The sink logs CLK; we expect ~20 transitions in 10 ms.
    let mut pwl = String::new();
    let mut t = 0.0;
    let mut level = 0.0;
    while t <= 10.0 {
        pwl.push_str(&format!("  [{t}, {level}],\n"));
        level = if level == 0.0 { 5.0 } else { 0.0 };
        t += 0.5;
    }
    let spec = format!(
        r#"name = "vcd sink records a clock"
board = "{board}"
duration_ms = 10
frame_ms = 0.1

[[peripheral]]
id = "SIG"
type = "stimulus"
net = "CLK"
waveform = "pwl"
pwl = [
{pwl}]

[[peripheral]]
id = "VCD"
type = "vcd_sink"
nets = ["CLK"]
vcd_path = "{vcd}"

# The square wave is ~1 kHz -> ~20 edges over 10 ms. Allow slack for chunk
# sampling at the 0.1 ms frame rate.
[[assert]]
kind = "peripheral"
id = "VCD"
field = "transitions"
min = 15
max = 25
"#,
        board = board.display(),
        vcd = vcd_out.display(),
        pwl = pwl,
    );
    let spec_path = write_tmp("vcd_sink.toml", &spec);

    let cfg = RunConfig { spec: spec_path };
    let result = run(&cfg).expect("run vcd spec");
    assert!(
        result.passed(),
        "vcd-sink spec should be green:\n{}",
        result.render_human()
    );

    // The VCD file must exist and be well-formed.
    let vcd = std::fs::read_to_string(&vcd_out).expect("VCD written");
    assert!(vcd.contains("$timescale 1ps"), "VCD has a timescale:\n{vcd}");
    assert!(vcd.contains("$var wire 1"), "VCD declares the CLK wire");
    assert!(vcd.contains("$enddefinitions"), "VCD header is complete");
    // Count value-change lines (e.g. "1!" / "0!").
    let edges = vcd
        .lines()
        .filter(|l| l.len() == 2 && (l.starts_with('0') || l.starts_with('1')))
        .count();
    assert!(
        (15..=27).contains(&edges),
        "VCD records ~20 transitions, got {edges}:\n{vcd}"
    );
}
