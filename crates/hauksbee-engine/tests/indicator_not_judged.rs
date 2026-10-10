//! A clean report must not imply an indicator's meaning was checked.
//!
//! `READY_N` through a non-inverting buffer lights its LED when the board is
//! not ready, and nothing on the path is out of range, so no check fires.
//! Without a stated outcome the plain, JSON and web surfaces must each say the
//! indicator's polarity versus intent was not checked, naming the LED and the
//! source net traced back through the buffer, and none of them may fail over
//! it.

use crate::support::{run_board, stdout};

const LINE: &str = "indicator meaning and signal polarity versus intent: not checked";

/// `READY_N` -> U1 (74LVC1G17 buffer) -> 1 kOhm -> red LED D1 to ground.
fn board() -> std::path::PathBuf {
    let pad = |n: &str, x: u32, net: &str| {
        format!("pad \"{n}\" smd rect at {x} 0 size 2 1.5 layers [F.Cu F.Mask] net \"{net}\"\n")
    };
    let comp = |r: &str, lib: &str, val: &str, x: u32, pads: String| {
        format!(
            "comp {r} lib \"{lib}\" val \"{val}\" layer \"F.Cu\" at {x} 20 rot 0 {{\n{pads}}}\n"
        )
    };
    let u1: String = ["NC", "READY_N", "GND", "LED_DRV", "VCC"]
        .iter()
        .zip(1..)
        .map(|(net, i)| pad(&i.to_string(), i, net))
        .collect();
    let parts = comp("U1", "Package_TO_SOT_SMD:SOT-23-5", "74LVC1G17", 10, u1)
        + &comp(
            "R1",
            "Resistor_SMD:R_0805_2012Metric",
            "1k",
            30,
            pad("1", 1, "LED_DRV") + &pad("2", 2, "LED_A"),
        )
        + &comp(
            "D1",
            "LED_SMD:LED_0805_2012Metric",
            "LED_RED",
            40,
            pad("K", 1, "GND") + &pad("A", 3, "LED_A"),
        );
    let nets = ["VCC", "GND", "READY_N", "LED_DRV", "LED_A", "NC"]
        .map(|n| format!("net \"{n}\"\n"))
        .concat();
    let dir = std::env::temp_dir().join(format!("hauksbee_indicator_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("indicator.board");
    std::fs::write(
        &path,
        format!("board version 20171130\nfn main {{\n{nets}{parts}}}\n"),
    )
    .unwrap();
    path
}

#[test]
fn plain_and_json_say_the_indicator_polarity_was_not_checked() {
    let path = board();
    let plain = run_board(&path, &["--check", "--plain"]);
    let text = stdout(&plain);
    let not_covered = text
        .lines()
        .find(|l| l.starts_with("NOT COVERED:"))
        .unwrap_or_else(|| panic!("a NOT COVERED line:\n{text}"));
    assert!(not_covered.contains(LINE), "{not_covered}");
    assert!(
        not_covered.contains("D1 (from READY_N, name suggests active-low, via U1 buffer)"),
        "{not_covered}"
    );

    let json = run_board(&path, &["--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json report");
    let note = v["triage"]["not_covered"].as_str().unwrap_or_default();
    assert!(note.contains(LINE), "{note}");
    // Not judged is not a failure: nothing about it reaches the order buckets.
    for bucket in ["do_not_order", "inspect"] {
        let entries = v["triage"][bucket].to_string();
        assert!(!entries.contains("polarity"), "{bucket}: {entries}");
    }
    assert_eq!(plain.status.code(), json.status.code());
}

#[test]
fn the_web_report_carries_the_same_note_without_failing() {
    let bytes = std::fs::read(board()).unwrap();
    let report = hauksbee_engine::frontdoor::analyze("indicator.board", &bytes);
    assert!(report.ok, "{:?}", report.error);
    let triage = report.triage.as_ref().expect("triage");
    assert!(triage.not_covered.contains(LINE), "{}", triage.not_covered);
    assert!(
        report.notes.iter().any(|n| n.message.contains(LINE)),
        "{:?}",
        report.notes
    );
    // The unrouted test board has findings of its own; none is the indicator.
    let findings: Vec<&str> = report
        .sections
        .iter()
        .flat_map(|s| s.findings.iter().map(|f| f.what.as_str()))
        .collect();
    assert!(
        findings.iter().all(|f| !f.contains("polarity")),
        "{findings:?}"
    );
}
