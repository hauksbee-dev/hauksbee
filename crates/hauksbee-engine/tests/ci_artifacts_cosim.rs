//! Co-sim faults must reach the `--junit` / `--sarif` CI artifacts.
//!
//! The artifacts are written once from the static suite (so report selectors
//! can produce them) and REWRITTEN after a headless co-sim with the stress
//! faults appended as `cosim` findings: the file a pipeline archives has to
//! carry the fault that broke the board, not only the copper checks.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// A red LED driven straight off +5V through a 25 ohm 0402: ~96 mA through a
/// 25 mA LED and ~230 mW in a 62 mW resistor, so the stress monitor raises
/// overcurrent / overpower / overtemperature faults within a few ms.
const FAULTING_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "LED_A")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 25 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+5V"))
    (pad 2 smd rect (at 2 0) (net 3 "LED_A"))
  )
  (module LED_THT:LED_D5.0mm (layer F.Cu)
    (at 105 100)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value RED_LED (at 0 2) (layer F.Fab))
    (pad A thru_hole circle (at 0 0) (net 3 "LED_A"))
    (pad K thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

#[test]
fn headless_cosim_faults_reach_junit_and_sarif() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("fault_led.kicad_pcb");
    std::fs::write(&board, FAULTING_BOARD).expect("board fixture");
    let junit = dir.path().join("out.xml");
    let sarif = dir.path().join("out.sarif");

    let out = Command::new(bin())
        .arg("run")
        .arg(&board)
        .args(["--headless", "--seconds", "0.05", "--junit"])
        .arg(&junit)
        .arg("--sarif")
        .arg(&sarif)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "headless run failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        xml.contains("<testsuite name=\"cosim\""),
        "JUnit must carry a cosim suite:\n{xml}"
    );
    assert!(
        xml.contains("overpower R1"),
        "the overpower fault must be a cosim testcase:\n{xml}"
    );

    let sarif_text = std::fs::read_to_string(&sarif).expect("sarif written");
    let doc: serde_json::Value = serde_json::from_str(&sarif_text).expect("valid SARIF JSON");
    let results = doc["runs"][0]["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["ruleId"]
            .as_str()
            .is_some_and(|id| id.starts_with("cosim/"))),
        "SARIF results must include cosim faults:\n{sarif_text}"
    );
}
