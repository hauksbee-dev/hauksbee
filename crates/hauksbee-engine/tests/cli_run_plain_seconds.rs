//! `hauksbee run <board> --plain --seconds N`: an explicit `--seconds` beside
//! bare `--plain` runs the static check and then co-simulates for that long,
//! ending on ONE verdict that covers both. Bare `--plain` stays the static
//! triage, and a flag the chosen surface does not read is said aloud rather
//! than dropped.

use crate::support::{board, run_board, stderr, stdout};
use std::path::PathBuf;

/// The smallest committed board that binds and simulates.
fn blinky() -> PathBuf {
    board("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

/// What the co-sim leg prints in plain mode that the static check never does.
const COSIM_MARKER: &str = "numerical qualification";

fn verdict_lines(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|line| line.starts_with("VERDICT:"))
        .collect()
}

#[test]
fn explicit_seconds_with_bare_plain_checks_then_cosimulates_and_closes_on_one_verdict() {
    let out = run_board(&blinky(), &["--plain", "--seconds", "0.05"]);
    let so = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    // The static check printed (its sections), then the co-sim ran.
    assert!(so.contains("== Signal integrity =="), "static check: {so}");
    assert!(so.contains(COSIM_MARKER), "co-sim leg: {so}");
    let static_at = so.find("== Signal integrity ==").unwrap();
    let cosim_at = so.rfind(COSIM_MARKER).unwrap();
    assert!(static_at < cosim_at, "static first, co-sim after: {so}");
    // One verdict, last, covering both.
    let lines = verdict_lines(&so);
    assert_eq!(lines.len(), 1, "exactly one VERDICT line: {so}");
    assert!(
        so.trim_end()
            .lines()
            .last()
            .is_some_and(|last| last.starts_with("VERDICT:")),
        "the verdict closes the run: {so}"
    );
    // The co-sim leg simulated the time asked for, on the same board.
    assert!(so.contains("simulated 0.050s"), "{so}");
}

#[test]
fn bare_plain_without_seconds_is_still_only_the_static_check() {
    let plain = run_board(&blinky(), &["--plain"]);
    let check = run_board(&blinky(), &["--check", "--plain"]);
    assert_eq!(plain.status.code(), Some(0), "stderr: {}", stderr(&plain));
    let so = stdout(&plain);
    assert_eq!(so, stdout(&check), "bare --plain is --check --plain");
    assert_eq!(verdict_lines(&so).len(), 1, "{so}");
    assert!(
        !so.contains(COSIM_MARKER) && !so.contains("most active nets"),
        "no co-sim leg: {so}"
    );
    assert!(
        !stderr(&plain).contains("was not used"),
        "nothing typed, nothing to say: {}",
        stderr(&plain)
    );
}

#[test]
fn the_combined_verdict_is_never_softer_than_the_static_check_alone() {
    // The static triage of this board decides the tag; the co-sim leg must not
    // turn a non-pass into a pass.
    let alone = stdout(&run_board(&blinky(), &["--plain"]));
    let both = stdout(&run_board(&blinky(), &["--plain", "--seconds", "0.05"]));
    let tag = |out: &str| {
        let line = verdict_lines(out)[0].to_string();
        let after = line.trim_start_matches("VERDICT: ");
        after.split(':').next().unwrap_or_default().to_string()
    };
    let rank = |tag: &str| match tag {
        t if t.starts_with("failing") => 2,
        t if t.starts_with("clean") || t.starts_with("inspect") => 0,
        _ => 1,
    };
    assert!(
        rank(&tag(&both)) >= rank(&tag(&alone)),
        "combined `{}` is softer than static `{}`",
        verdict_lines(&both)[0],
        verdict_lines(&alone)[0]
    );
}

#[test]
fn a_seconds_a_static_report_ignores_is_said_not_dropped() {
    let out = run_board(&blinky(), &["--lint", "--seconds", "5"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let err = stderr(&out);
    let said: Vec<&str> = err
        .lines()
        .filter(|line| line.contains("--seconds") && line.contains("was not used"))
        .collect();
    assert_eq!(said.len(), 1, "exactly one notice: {err}");
    assert!(
        said[0].contains("--lint") && said[0].contains("--headless"),
        "names the surface that dropped it and where it applies: {}",
        said[0]
    );
    assert!(
        !stdout(&out).contains(COSIM_MARKER),
        "a lint run does not simulate"
    );
}

#[test]
fn a_seconds_a_surface_reads_or_a_default_one_is_not_flagged() {
    for args in [
        &["--headless", "--seconds", "0.05"][..],
        &["--plain", "--seconds", "0.05"],
        &["--thermal", "--seconds", "0.05"],
        &["--lint"],
        &["--plain"],
    ] {
        let out = run_board(&blinky(), args);
        assert!(
            !stderr(&out).contains("was not used"),
            "{args:?} wrongly flagged: {}",
            stderr(&out)
        );
    }
}

#[test]
fn check_as_a_subcommand_says_its_seconds_went_unused_too() {
    let out = crate::support::run(&["check", blinky().to_str().unwrap(), "--seconds", "2"]);
    assert!(
        stderr(&out).contains("--seconds") && stderr(&out).contains("was not used"),
        "{}",
        stderr(&out)
    );
}

/// A board whose static check already fails (a gating finding, small and
/// committed).
fn failing_board() -> PathBuf {
    board("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb")
}

#[test]
fn a_board_the_static_check_fails_is_failing_in_the_one_combined_verdict() {
    let out = run_board(&failing_board(), &["--plain", "--seconds", "0.05"]);
    let so = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "report only: {}", stderr(&out));
    assert!(so.contains(COSIM_MARKER), "the co-sim still ran: {so}");
    let lines = verdict_lines(&so);
    assert_eq!(lines.len(), 1, "exactly one VERDICT line: {so}");
    assert!(
        lines[0].starts_with("VERDICT: failing:") && lines[0].contains("exit 0: report only"),
        "{}",
        lines[0]
    );
}

#[test]
fn the_static_checks_strict_exit_is_still_taken_after_the_combined_verdict() {
    let out = run_board(
        &failing_board(),
        &["--plain", "--seconds", "0.05", "--strict"],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let so = stdout(&out);
    assert_eq!(verdict_lines(&so).len(), 1, "{so}");
    // Plain mode streams the strict line on stdout, after the one verdict.
    let verdict_at = so.find("VERDICT:").unwrap();
    let strict_at = so.find("FAILED under --strict").expect(&so);
    assert!(verdict_at < strict_at, "verdict first, then the exit: {so}");
}
