//! `hauksbee-ci run --spice-models <file>`: the same vendor SPICE card layer
//! `hauksbee run` has, so a board binds the same way in a pipeline as it does
//! interactively.

use crate::support;
use std::path::Path;
use std::process::{Command, Output};

fn engine_fixture(rel: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-engine/tests/fixtures")
        .join(rel)
        .to_str()
        .expect("fixture path is utf-8")
        .to_string()
}

fn run(args: &[&str]) -> Output {
    Command::new(support::bin())
        .args(args)
        .output()
        .expect("hauksbee-ci binary runs")
}

/// A `no_faults` spec over a board whose only active part is a transistor the
/// built-in library does not cover: without the vendor card the run refuses to
/// vouch for itself, with it the same spec is judgeable, and a file that is
/// not there is a spec error rather than a silent skip.
#[test]
fn a_vendor_card_makes_the_same_spec_judgeable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = dir.path().join("spice_card.toml");
    std::fs::write(
        &spec,
        format!(
            "name = \"vendor spice card binds a transistor\"\nboard = {}\nduration_ms = 1\n\n\
             [[supply]]\nnet = \"+5V\"\nkind = \"bench\"\nvolts = 5.0\ncurrent_limit_a = 1.0\n\
             r_out_ohms = 0.05\n\n[[assert]]\nkind = \"no_faults\"\n",
            support::toml_path(Path::new(&engine_fixture("spice_card_bjt.kicad_sch")))
        ),
    )
    .expect("write spec");
    let spec = spec.to_str().expect("temp path is utf-8");
    let json = |out: Output| -> serde_json::Value {
        let so = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(&so).unwrap_or_else(|e| panic!("--json must stay JSON: {e}\n{so}"))
    };

    let without = json(run(&["run", spec, "--json"]));
    assert_eq!(
        (&without["run_valid"], &without["passed"]),
        (&false.into(), &false.into()),
        "{without}"
    );
    let lib = engine_fixture("vendor_bjt.lib");
    let with = json(run(&["run", spec, "--json", "--spice-models", &lib]));
    assert_eq!(
        (&with["run_valid"], &with["passed"]),
        (&true.into(), &true.into()),
        "{with}"
    );

    let out = run(&[
        "run",
        spec,
        "--spice-models",
        "/nonexistent/vendor/models.lib",
    ]);
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(text.contains("/nonexistent/vendor/models.lib"), "{text}");
}
