//! `hauksbee run --spice-models <file>`: the vendor SPICE card layer.
//!
//! The layer (priority 40, above every model directory) existed in the model
//! library with no way to reach it from the CLI. These tests pin the two halves
//! of the fix: the card binds, and the file the user typed is reported on when
//! it cannot serve.

use crate::support::run;
use std::path::PathBuf;
use std::process::Output;

fn fixture(rel: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
        .to_str()
        .expect("fixture path is utf-8")
        .to_string()
}

fn report(out: &Output) -> serde_json::Value {
    let so = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&so).unwrap_or_else(|e| panic!("--json must stay JSON: {e}\n{so}"))
}

/// The board's Q1 carries `2SD1664R`, a real part the built-in library does
/// not cover: nothing in the tree claims it and the vendor's card is the only
/// model there is. Without it the run declines to vouch for itself; with it
/// the same board is judgeable.
#[test]
fn a_vendor_model_card_turns_an_unjudgeable_run_into_a_pass() {
    let b = fixture("spice_card_bjt.kicad_sch");
    let without = run(&["run", &b, "--check", "--strict", "--json"]);
    let doc = report(&without);
    assert_eq!(doc["verdict"], "invalid", "{doc}");
    assert_eq!(doc["bind"]["unresolved"], 1);
    assert_eq!(doc["bind"]["active_path_unresolved"][0]["reference"], "Q1");
    assert_eq!(
        without.status.code(),
        Some(3),
        "invalid for analysis is exit 3"
    );

    let lib = fixture("vendor_bjt.lib");
    let with = run(&[
        "run",
        &b,
        "--check",
        "--strict",
        "--json",
        "--spice-models",
        &lib,
    ]);
    let doc = report(&with);
    assert_eq!(doc["verdict"], "pass", "{doc}");
    assert_eq!(doc["bind"]["unresolved"], 0);
    assert_eq!(
        doc["bind"]["active_path_unresolved"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    assert_eq!(with.status.code(), Some(0));
}

/// A file that is not there, or that carries no card, is an error naming the
/// path: the user named it expecting the parts in it to bind.
#[test]
fn a_file_that_cannot_serve_is_named_rather_than_binding_nothing_quietly() {
    let b = fixture("spice_card_bjt.kicad_sch");
    let temp = tempfile::tempdir().expect("tempdir");
    let empty = temp.path().join("empty.lib");
    std::fs::write(&empty, "* nothing but a comment\n.end\n").expect("write");
    for (path, wants) in [
        ("/nonexistent/vendor/models.lib", "--spice-models"),
        (empty.to_str().expect("temp path is utf-8"), ".model"),
    ] {
        let out = run(&["run", &b, "--check", "--json", "--spice-models", path]);
        assert!(!out.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        // The name, not the whole path: a JSON error escapes the backslashes
        // a Windows temp path carries.
        let name = std::path::Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(text.contains(name) && text.contains(wants), "{text}");
    }
}
