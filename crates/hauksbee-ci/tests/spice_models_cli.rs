//! `hauksbee-ci run --spice-models <file>`: the same vendor SPICE card layer
//! `hauksbee run` has, so a board binds the same way in a pipeline as it does
//! interactively.
//!
//! The spec asserts `no_faults` over a board whose only active part is a
//! transistor the built-in library does not cover. Without the vendor card the
//! run refuses to vouch for itself; with it, the same spec is judgeable.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

fn engine_fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-engine/tests/fixtures")
        .join(rel)
}

fn spec_in(dir: &std::path::Path) -> PathBuf {
    let board = engine_fixture("spice_card_bjt.kicad_sch");
    let spec = dir.join("spice_card.toml");
    std::fs::write(
        &spec,
        format!(
            "name = \"vendor spice card binds a transistor\"\n\
             board = \"{}\"\n\
             duration_ms = 1\n\n\
             [[supply]]\n\
             net = \"+5V\"\n\
             kind = \"bench\"\n\
             volts = 5.0\n\
             current_limit_a = 1.0\n\
             r_out_ohms = 0.05\n\n\
             [[assert]]\n\
             kind = \"no_faults\"\n",
            board.display()
        ),
    )
    .expect("write spec");
    spec
}

fn run_json(args: &[&str]) -> serde_json::Value {
    let out = Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee-ci binary runs");
    let so = String::from_utf8_lossy(&out.stdout).into_owned();
    serde_json::from_str(&so).unwrap_or_else(|e| panic!("--json must stay JSON: {e}\n{so}"))
}

#[test]
fn a_vendor_card_makes_the_same_spec_judgeable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = spec_in(dir.path());
    let spec = spec.to_str().expect("temp path is utf-8");

    // Q1 has no model anywhere, so the run declines rather than return a
    // clean result it cannot stand behind.
    let without = run_json(&["run", spec, "--json"]);
    assert_eq!(without["run_valid"], false, "{without}");
    assert_eq!(without["passed"], false, "{without}");

    let lib = engine_fixture("vendor_bjt.lib");
    let with = run_json(&[
        "run",
        spec,
        "--json",
        "--spice-models",
        lib.to_str().expect("fixture path is utf-8"),
    ]);
    assert_eq!(with["run_valid"], true, "{with}");
    assert_eq!(with["passed"], true, "{with}");
}

#[test]
fn a_spice_file_that_is_not_there_is_a_spec_error_not_a_silent_skip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = spec_in(dir.path());
    let out = Command::new(bin())
        .args([
            "run",
            spec.to_str().expect("temp path is utf-8"),
            "--spice-models",
            "/nonexistent/vendor/models.lib",
        ])
        .output()
        .expect("hauksbee-ci binary runs");
    assert!(!out.status.success());
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("/nonexistent/vendor/models.lib"), "{text}");
}
