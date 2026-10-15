//! `run --check` honours a class-only MOSFET's assumed corner set the way
//! `hauksbee-ci` does: the board is judged at every corner, passes only if
//! every corner passes (and then only conditionally, naming the assumption),
//! and fails citing the corner that fails.
//!
//! The board is a P-channel high-side switch: Q1 (`Q_PMOS_GSD`, no part
//! number) with source on +5V, a 1k load to ground, 10k gate-to-source and
//! 100 ohm from the gate to the drive. Driven from 0 V the gate sees -5 V and
//! every corner turns on; driven from a 2.5 V rail it sees -2.5 V, inside the
//! class's threshold window, so the low-threshold corner conducts and the
//! high-threshold corner is not guaranteed to.

use crate::support::{run_board, stdout};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

const CONDITION: &str = "Q1 passes across assumed corners (generic_pmos_sot_corner_high_vth, \
                         generic_pmos_sot_corner_low_vth); put the MPN on Q1 to remove this \
                         assumption.";

fn json(board: &str) -> serde_json::Value {
    let out = run_board(&fixture(board), &["--check", "--json"]);
    serde_json::from_str(&stdout(&out)).expect("--check --json is JSON")
}

#[test]
fn a_full_gate_drive_passes_on_the_corner_condition() {
    let doc = json("generic_pmos_switch_gate_0v.kicad_pcb");
    assert_eq!(doc["verdict"], "pass", "{doc:#}");
    assert_eq!(doc["ok"], true);
    // The corner condition, plus only the default-spacing-rules one (the
    // fixture has no project file beside it).
    let conditions = doc["verdict_conditions"].as_array().expect("conditions");
    assert_eq!(conditions[0], CONDITION, "{doc:#}");
    assert!(
        conditions[1..].iter().all(|c| c
            .as_str()
            .is_some_and(|c| c.starts_with("copper spacing was judged against a DEFAULT"))),
        "{doc:#}"
    );
    // The swept assumption is qualified; the unswept stand-in one is gone.
    let ids: Vec<&str> = (doc["assumptions"].as_array().unwrap().iter())
        .filter_map(|a| a["id"].as_str())
        .collect();
    assert!(ids.contains(&"reduced-fidelity:corners/Q1"), "{ids:?}");
    assert!(!ids.contains(&"substitute-model:Q1"), "{ids:?}");
    let out = run_board(
        &fixture("generic_pmos_switch_gate_0v.kicad_pcb"),
        &["--check"],
    );
    let text = stdout(&out);
    let verdict = text.lines().rfind(|l| l.starts_with("VERDICT:")).unwrap();
    assert!(
        verdict.starts_with("VERDICT: clean, conditionally:") && verdict.contains(CONDITION),
        "{verdict}"
    );
}

#[test]
fn a_gate_drive_inside_the_threshold_window_fails_citing_the_corner() {
    let doc = json("generic_pmos_switch_gate_2v5.kicad_pcb");
    assert_eq!(doc["verdict"], "fail", "{doc:#}");
    // A condition qualifies a pass only; a fail carries none.
    assert!(doc.get("verdict_conditions").is_none(), "{doc:#}");
    let findings = doc["findings"].as_array().expect("findings");
    let under: Vec<&serde_json::Value> = (findings.iter())
        .filter(|f| f["kind"] == "gate_underdrive")
        .collect();
    assert_eq!(under.len(), 1, "{findings:#?}");
    let message = under[0]["message"].as_str().unwrap();
    assert!(
        message.contains("generic_pmos_sot_corner_low_vth")
            && message.contains("generic_pmos_sot_corner_high_vth")
            && message.contains("put the MPN on Q1"),
        "{message}"
    );
    assert_eq!(under[0]["severity"], "serious");
    assert_eq!(under[0]["nets"], serde_json::json!(["G"]));
}

/// The web front door runs the same swept lint and carries the same condition.
#[test]
fn the_web_report_carries_the_same_corner_verdicts() {
    let analyze = |name: &str| {
        let bytes = std::fs::read(fixture(name)).expect("fixture readable");
        hauksbee_engine::frontdoor::analyze(name, &bytes)
    };
    let pass = analyze("generic_pmos_switch_gate_0v.kicad_pcb");
    assert_eq!(pass.serious, 0, "{:?}", pass.sections);
    assert_eq!(pass.verdict_conditions[0], CONDITION);
    assert_eq!(
        serde_json::json!(pass.verdict_conditions),
        json("generic_pmos_switch_gate_0v.kicad_pcb")["verdict_conditions"],
        "the web and the CLI rest the pass on the same conditions"
    );
    assert!(pass.headline.contains(CONDITION), "{}", pass.headline);
    let fail = analyze("generic_pmos_switch_gate_2v5.kicad_pcb");
    assert_eq!(fail.serious, 1, "{:?}", fail.sections);
    assert!(fail.verdict_conditions.is_empty());
    assert!(!fail.headline.contains(CONDITION), "{}", fail.headline);
    assert!(
        fail.verdict_reason
            .as_deref()
            .is_some_and(|r| r.contains("generic_pmos_sot_corner_low_vth")),
        "{:?}",
        fail.verdict_reason
    );
}
