//! A design whose root names a sub-sheet file that is not there is still read:
//! the sheets that are present are checked and a finding on them still gates,
//! while the verdict names the missing sheet as scope the run did not cover, so
//! a clean result over the rest cannot read as a clean design.
//!
//! The Easyduino ESP32-S3 fixtures are verbatim upstream root schematics whose
//! `References.kicad_sch` sheet is not part of the fixture.

use crate::support::{board, run_board};
use std::path::PathBuf;

fn easyduino(side: &str) -> PathBuf {
    board(&format!(
        "../../qc/defect_regressions/fixtures/easyduino_esp32s3_uart_tx_crossed/{side}/Easyduino_ESP32S3.kicad_sch"
    ))
}

fn json_run(side: &str) -> (i32, serde_json::Value) {
    let out = run_board(&easyduino(side), &["--check", "--json", "--strict"]);
    let code = out.status.code().expect("exit code");
    let doc = serde_json::from_slice(&out.stdout).expect("a JSON report on stdout");
    (code, doc)
}

fn names_missing_sheet(doc: &serde_json::Value) -> bool {
    doc["assumptions"].as_array().is_some_and(|a| {
        a.iter().any(|x| {
            x["kind"] == "not_exercised"
                && x["statement"]
                    .as_str()
                    .is_some_and(|s| s.contains("sub-sheet 'References.kicad_sch'"))
        })
    })
}

#[test]
fn a_finding_on_the_present_sheets_still_gates_with_a_sheet_missing() {
    let (code, doc) = json_run("faulty");
    assert_eq!(code, 2, "the UART conflict still fails the gate");
    assert_eq!(doc["verdict"], "fail");
    assert!(
        doc["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["kind"] == "pin_function"),
        "the fault on the present sheets is still found"
    );
    assert!(names_missing_sheet(&doc), "the missing sheet is named");
}

#[test]
fn a_clean_read_with_a_sheet_missing_is_not_a_pass() {
    let (code, doc) = json_run("fixed");
    assert_eq!(code, 3, "an unread sheet cannot give a clean exit");
    assert_eq!(doc["verdict"], "invalid");
    assert_eq!(doc["serious_count"], 0);
    assert!(names_missing_sheet(&doc), "the missing sheet is named");
}
