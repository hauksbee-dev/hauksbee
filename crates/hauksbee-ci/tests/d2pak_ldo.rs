//! A fixed LDO in a D2PAK whose tab shares the output's pad number, fitted
//! with exactly its card's 10 uF minimum output capacitor. From 5 V the rail
//! holds inside the card's output interval, and an input below the card's upper
//! output limit plus its dropout is refused rather than read as a regulated
//! rail. A layout whose pads name their pins (KiCad's `ADJ/GND_1`, `OUT_2`,
//! `IN_3`: one symbol shared by the adjustable and fixed parts) binds by those
//! names; the same board with bare pad numbers binds by the card's assumed
//! numbering and says so on every pad.

use crate::support;
use hauksbee_ci::RunConfig;
use std::path::Path;

/// The run's exit code and whether it reported any pad role as inferred.
fn run_at(dir: &Path, fixture: &str, volts: f64) -> (i32, bool) {
    let board = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let spec = dir.join(format!("d2pak-{fixture}-{volts}.toml"));
    let body = format!(
        "name = \"d2pak ldo\"\nboard = {}\nduration_ms = 3\n\
         [[supply]]\nnet = \"VIN\"\nkind = \"ideal\"\nvolts = {volts}\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.234\nmax = 3.366\nafter_ms = 1.0\n",
        support::toml_path(&board)
    );
    std::fs::write(&spec, body).expect("write spec");
    let cfg = RunConfig {
        spec,
        ..RunConfig::default()
    };
    let report = hauksbee_ci::run(&cfg).expect("run the spec");
    // Both forms are pre-layout: the waived not-routed row stays listed.
    support::assert_placement_waived(&report);
    let text = report.render_human();
    let inferred = ["1", "2", "3"].map(|pad| text.contains(&format!("inferred-pin-role:U1/{pad}")));
    assert!(
        inferred.iter().all(|&i| i == inferred[0]),
        "{fixture} at {volts} V: every pad inferred or none: {text}"
    );
    (report.exit_code(), inferred[0])
}

#[test]
fn a_d2pak_ldo_regulates_from_5v_and_refuses_an_input_short_of_its_dropout() {
    let d = tempfile::tempdir().expect("tempdir");
    let named = "d2pak_ldo_named.kicad_pcb";
    let seen = (
        run_at(d.path(), named, 5.0),
        run_at(d.path(), named, 4.6).0,
        run_at(d.path(), "d2pak_ldo.board", 5.0),
    );
    assert_eq!(
        seen,
        ((0, false), 3, (0, true)),
        "(named 5.0 V, named 4.6 V, bare-numbered 5.0 V): (exit code, pad roles inferred)"
    );
}
