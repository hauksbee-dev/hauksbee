//! `findings[]` is the complete, gate-consistent list: every copper short and
//! clearance group is a finding with a board location, the findings that gate
//! are exactly the ones `--strict` fails on, and `serious_count` counts the
//! serious findings. SARIF, `JUnit` and annotations are fed from this list, so a
//! gap here is a gap in every CI artifact.

use crate::support::{board, run_board, stderr, stdout, testdata};
use serde_json::Value;
use std::path::PathBuf;

fn boards() -> Vec<PathBuf> {
    let mut boards: Vec<PathBuf> = ["blinky", "power_resistor", "boot_gate", "tolerance_divider"]
        .iter()
        .map(|b| board(&format!("../hauksbee-ci/examples/boards/{b}.kicad_pcb")))
        .collect();
    boards.extend(
        [
            "boards/pic_programmer.kicad_pcb",
            "boards/stickhub.kicad_pcb",
        ]
        .iter()
        .filter_map(|b| testdata(b)),
    );
    boards
}

/// The `--check --json --strict` document, its exit code and the number of
/// subjects its `--strict` failure line names (0 when it did not fail).
fn strict_check(path: &std::path::Path) -> (Value, Option<i32>, usize) {
    let out = run_board(path, &["--check", "--json", "--strict"]);
    let doc: Value = serde_json::from_str(&stdout(&out))
        .unwrap_or_else(|e| panic!("{}: JSON ({e}): {}", path.display(), stderr(&out)));
    let named = stderr(&out)
        .lines()
        .find_map(|l| l.strip_prefix("FAILED under --strict: "))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or(0);
    (doc, out.status.code(), named)
}

fn findings(doc: &Value) -> &[Value] {
    doc["findings"].as_array().map_or(&[], Vec::as_slice)
}

#[test]
fn findings_are_complete_located_and_agree_with_the_gate() {
    for path in boards() {
        let (doc, code, named) = strict_check(&path);
        let name = path.display();
        let all = findings(&doc);
        let gating = all.iter().filter(|f| f["gating"] == true).count();
        assert_eq!(
            gating, named,
            "{name}: gating findings vs the --strict line (exit {code:?})"
        );
        if code != Some(2) {
            assert_eq!(gating, 0, "{name}: nothing gates unless --strict exits 2");
        }
        let serious = all.iter().filter(|f| f["severity"] == "serious").count();
        assert_eq!(
            doc["serious_count"].as_u64(),
            Some(serious as u64),
            "{name}: serious_count counts the serious findings"
        );
        let drc: Vec<&Value> = all.iter().filter(|f| f["check"] == "drc").collect();
        let count = |key: &str| doc["drc"][key].as_array().map_or(0, Vec::len);
        let kind = |k: &str| drc.iter().filter(|f| f["kind"] == k).count();
        assert_eq!(
            kind("short"),
            count("shorts"),
            "{name}: every short is a finding"
        );
        assert_eq!(
            kind("clearance"),
            count("violations"),
            "{name}: every below-rule clearance group is a finding"
        );
        for f in &drc {
            assert!(
                f["location_mm"].as_array().is_some_and(|l| l.len() == 2),
                "{name}: a copper finding carries its board location: {f}"
            );
            assert!(f["layer"].is_string(), "{name}: and its layer: {f}");
        }
    }
}

#[test]
fn the_boot_gate_board_gates_on_its_two_shorts_and_unrouted_pad() {
    let (doc, code, named) =
        strict_check(&board("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb"));
    assert_eq!(code, Some(2), "{doc:#}");
    assert_eq!(named, 3, "{doc:#}");
    let shorts: Vec<&Value> = findings(&doc)
        .iter()
        .filter(|f| f["check"] == "drc" && f["kind"] == "short" && f["gating"] == true)
        .collect();
    assert_eq!(shorts.len(), 2, "{doc:#}");
    assert!(
        shorts
            .iter()
            .all(|f| f["refs"].as_array().is_some_and(|r| !r.is_empty())),
        "a short names the parts whose pads it joins: {shorts:#?}"
    );
}

#[test]
fn a_pass_on_default_spacing_rules_is_conditional_with_how_to_lift_it() {
    let path = board("../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    for args in [&["--check", "--json"][..], &["--drc", "--json"][..]] {
        let out = run_board(&path, args);
        let doc: Value = serde_json::from_str(&stdout(&out)).expect("JSON");
        assert_eq!(doc["verdict"], "pass", "{args:?}: {doc:#}");
        let conditions = doc["verdict_conditions"].as_array().expect("conditions");
        assert!(
            conditions.iter().any(|c| c.as_str().is_some_and(
                |c| c.contains("DEFAULT") && c.ends_with("to remove this assumption.")
            )),
            "{args:?}: {doc:#}"
        );
    }
    let text = stdout(&run_board(&path, &["--check"]));
    assert!(
        text.lines()
            .any(|l| l.starts_with("VERDICT: clean, conditionally:")),
        "{text}"
    );
}
