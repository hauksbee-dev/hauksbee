//! Source-bound smoke gate for `qc/benchmarks/ngspice_vs_hauksbee`.
//!
//! This test deliberately does not run either simulator or assert a timing
//! number. It protects the benchmark's eligibility contract: every declared
//! source deck must still load through the solver's own SPICE front door, carry
//! a transient window, and expose parseable probes. Numerical comparison and
//! timing stay in the bounded machine-readable harness.

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::Probe;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn declared_board_benchmark_sources_are_loadable() {
    let root = repo_root();
    let manifest_path = root.join("qc/benchmarks/ngspice_vs_hauksbee/manifest.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("benchmark manifest exists"),
    )
    .expect("benchmark manifest is JSON");
    let cases = manifest["cases"].as_array().expect("cases array");
    assert!(cases.len() >= 5, "first gate must retain a real matrix");
    assert!(
        manifest.get("threshold").is_none(),
        "thresholds require a retained fresh campaign"
    );

    for case in cases {
        let id = case["id"].as_str().expect("case id");
        let source = root.join(case["source"].as_str().expect("source path"));
        let deck = fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("{id}: read {}: {e}", source.display()));
        // The first power-path source intentionally keeps a relative include
        // because the external harness executes it from its source directory.
        // The loader API resolves includes relative to the process cwd, so make
        // that one path absolute for this in-process smoke check without
        // changing the source bytes used by the benchmark.
        let load_deck = if deck.contains(".include diode.lib") {
            deck.replace(
                ".include diode.lib",
                &format!(
                    ".include {}",
                    source.parent().unwrap().join("diode.lib").display()
                ),
            )
        } else {
            deck.clone()
        };
        let (circuit, directives) = SpiceLoader::load_with_directives(&load_deck)
            .unwrap_or_else(|e| panic!("{id}: source deck must load: {e}"));
        assert!(
            !circuit.devices.is_empty(),
            "{id}: empty circuit is not eligible"
        );
        assert!(
            directives.tran.is_some(),
            "{id}: first matrix requires a .tran deck"
        );
        for probe in case["probes"].as_array().expect("probe array") {
            let expression = probe.as_str().expect("probe string");
            Probe::parse(expression)
                .unwrap_or_else(|e| panic!("{id}: invalid probe {expression}: {e}"));
        }
    }
}
