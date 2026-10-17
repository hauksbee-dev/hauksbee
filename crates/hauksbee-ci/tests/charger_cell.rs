//! A linear charger's output is a current-limited source, so a cell declared
//! on it is the charger's load rather than a rival source.

use crate::support;
use hauksbee_ci::RunConfig;
use std::path::Path;

fn exit_code(dir: &Path, name: &str, extra: &str) -> i32 {
    let board = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/charger.kicad_pcb");
    let spec = dir.join(format!("{name}.toml"));
    let body = format!(
        "name = \"{name}\"\nboard = {}\nduration_ms = 3\n\
         [[supply]]\nnet = \"VIN\"\nkind = \"ideal\"\nvolts = 5.0\n{extra}\
         [[assert]]\nkind = \"voltage\"\nnet = \"VBAT\"\nmin = 3.0\nmax = 4.25\nafter_ms = 1.0\n",
        support::toml_path(&board)
    );
    std::fs::write(&spec, body).expect("write spec");
    let cfg = RunConfig {
        spec,
        ..RunConfig::default()
    };
    let r = hauksbee_ci::run(&cfg).expect("run the spec");
    // The fixture is placement-only: its waived not-routed row stays listed.
    support::assert_placement_waived(&r);
    r.exit_code()
}

#[test]
fn a_cell_declared_on_a_chargers_output_is_accepted() {
    let d = tempfile::tempdir().expect("tempdir");
    let cell = "[[supply]]\nnet = \"VBAT\"\nkind = \"battery\"\nchemistry = \"liion\"\n";
    let seen = (
        exit_code(d.path(), "with-cell", cell),
        exit_code(d.path(), "open", ""),
    );
    assert_eq!(seen, (0, 0), "(with cell, open) exit codes");
}
