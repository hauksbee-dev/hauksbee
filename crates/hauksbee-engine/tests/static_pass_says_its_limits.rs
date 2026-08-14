//! A clean static report must say what it did not check.
//!
//! Found by running the flagship board the way a newcomer would: upload it,
//! read the report. Every part bound, every section came back healthy, and
//! nothing anywhere mentioned that a whole class of fault was still unexamined.
//! The flagship regression this project was built around is precisely that
//! class: a rail that collapses on a fuzzed power-up, invisible to any static
//! check.
//!
//! Nothing in the old report was false. A reader who stopped at "Looks healthy"
//! was misled by omission, which is the failure mode this project exists to
//! prevent.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn plain_check(board: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .arg("run")
        .arg(repo_root().join(board))
        .args(["--check", "--plain"])
        .output()
        .expect("run hauksbee");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn a_clean_report_names_the_faults_it_cannot_see() {
    // blinky is clean, which is exactly when the omission was dangerous.
    let text = plain_check("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb");
    assert!(
        text.contains("Looks healthy"),
        "premise: this board reports clean, or the test proves nothing:\n{text}"
    );
    assert!(
        text.contains("What this pass did not check"),
        "a clean static report has to say what it did not look at:\n{text}"
    );
    assert!(
        text.contains("brownout") || text.contains("sags on inrush"),
        "and name the kind of fault it cannot see, not just gesture at it:\n{text}"
    );
    if cfg!(feature = "avr") {
        assert!(
            text.contains("hauksbee-ci init"),
            "and give the command that reaches them:\n{text}"
        );
    } else {
        assert!(
            text.contains("does not include its co-simulation backend"),
            "and name why this build cannot reach firmware checks:\n{text}"
        );
    }
}

#[test]
fn the_advice_matches_whether_a_processor_bound() {
    // Telling someone to boot firmware on a board with no processor is advice
    // that wastes their time and costs trust in the rest of the report.
    let with_mcu = plain_check("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb");
    if cfg!(feature = "avr") {
        assert!(
            with_mcu.contains("co-simulation support in this build"),
            "an AVR-capable build can offer Blinky firmware co-sim:\n{with_mcu}"
        );
        assert!(with_mcu.contains("hauksbee-ci run <spec>"), "{with_mcu}");
    } else {
        assert!(
            with_mcu.contains("does not include its co-simulation backend"),
            "a permissive build must name its AVR capability boundary:\n{with_mcu}"
        );
        assert!(
            !with_mcu.contains("hauksbee-ci run <spec>"),
            "a build without AVR must not recommend an impossible firmware run:\n{with_mcu}"
        );
    }

    let without = plain_check("testdata/boards/button_pullup.kicad_pcb");
    assert!(
        without.contains("No processor bound"),
        "a resistor and a switch cannot boot anything:\n{without}"
    );
    assert!(
        !without.contains("hauksbee-ci run <spec>"),
        "so it must not suggest running firmware there:\n{without}"
    );
}
