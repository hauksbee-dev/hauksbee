//! `hauksbee run --spice-models <file>`: the vendor SPICE card layer.
//!
//! The layer (priority 40, above every model directory) existed in the model
//! library with no way to reach it from the CLI, so a vendor `.lib` sitting
//! beside a board could not become a binding however it was named or placed.
//! These tests pin the two halves of the fix: the card binds, and the file the
//! user typed is reported on when it cannot serve.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

fn report(args: &[&str]) -> serde_json::Value {
    let out = run(args);
    let so = String::from_utf8_lossy(&out.stdout).into_owned();
    serde_json::from_str(&so).unwrap_or_else(|e| panic!("--json must stay JSON: {e}\n{so}"))
}

/// The board's Q1 carries `2SD1664R`, a real part the built-in library does
/// not cover, so it is the case the SPICE layer exists for: nothing in the
/// tree claims it and the vendor's own card is the only model there is.
fn board() -> String {
    fixture("spice_card_bjt.kicad_sch")
        .to_str()
        .expect("fixture path is utf-8")
        .to_string()
}

#[test]
fn an_unmodelled_transistor_refuses_to_be_judged_without_the_vendor_card() {
    // The baseline the flag changes. Without a model for Q1 the run declines
    // to vouch for itself rather than return a vacuous clean result.
    let b = board();
    let doc = report(&["run", &b, "--check", "--strict", "--json"]);
    assert_eq!(doc["verdict"], "invalid", "{doc}");
    assert_eq!(doc["bind"]["unresolved"], 1);
    let unresolved = doc["bind"]["active_path_unresolved"][0]["reference"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(unresolved, "Q1");

    let out = run(&["run", &b, "--check", "--strict", "--json"]);
    assert_eq!(out.status.code(), Some(3), "invalid for analysis is exit 3");
}

#[test]
fn a_vendor_model_card_binds_the_part_and_the_run_becomes_judgeable() {
    let b = board();
    let lib = fixture("vendor_bjt.lib");
    let doc = report(&[
        "run",
        &b,
        "--check",
        "--strict",
        "--json",
        "--spice-models",
        lib.to_str().expect("fixture path is utf-8"),
    ]);
    assert_eq!(doc["verdict"], "pass", "{doc}");
    assert_eq!(doc["bind"]["unresolved"], 0);
    assert_eq!(
        doc["bind"]["active_path_unresolved"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );

    let out = run(&[
        "run",
        &b,
        "--check",
        "--strict",
        "--json",
        "--spice-models",
        lib.to_str().expect("fixture path is utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_spice_file_that_is_not_there_names_the_path_and_what_to_point_it_at() {
    let b = board();
    let out = run(&[
        "run",
        &b,
        "--check",
        "--json",
        "--spice-models",
        "/nonexistent/vendor/models.lib",
    ]);
    assert!(!out.status.success());
    let err =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(err.contains("/nonexistent/vendor/models.lib"), "{err}");
    assert!(err.contains("--spice-models"), "{err}");
}

#[test]
fn a_file_with_no_cards_in_it_says_so_instead_of_binding_nothing_quietly() {
    let temp = tempfile::tempdir().expect("tempdir");
    let empty = temp.path().join("empty.lib");
    std::fs::write(&empty, "* nothing but a comment\n.end\n").expect("write");

    let b = board();
    let out = run(&[
        "run",
        &b,
        "--check",
        "--json",
        "--spice-models",
        empty.to_str().expect("temp path is utf-8"),
    ]);
    assert!(!out.status.success());
    let err =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(err.contains("empty.lib"), "{err}");
    assert!(err.contains(".model"), "{err}");
}
