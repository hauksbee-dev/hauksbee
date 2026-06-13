//! CLI-level tests for the accessibility surfaces added to `hauksbee run`:
//!
//! - `--strict` (alias `--fail-on-findings`): exits non-zero when a report finds
//!   a real problem, while the default (no flag) stays exit 0.
//! - `--plain` (alias `--explain`): prints the plain-language verdict instead of
//!   the expert table.
//!
//! These exercise the actual compiled binary so the exit-code contract that a CI
//! pipeline depends on is tested end to end, not just the library predicates.

use std::path::PathBuf;
use std::process::Command;

/// The compiled `hauksbee` binary (Cargo sets this for the engine crate's tests).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// Workspace-relative example boards, resolved from this crate's manifest dir.
fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// A board that contains two real copper shorts (GND <-> +5V on both layers).
fn shorted_board() -> PathBuf {
    board("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb")
}

/// A board that is clean for the connectivity lint.
fn clean_board() -> PathBuf {
    board("../../testdata/boards/button_pullup.kicad_pcb")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

#[test]
fn drc_without_strict_exits_zero_even_with_shorts() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc"]);
    assert!(
        out.status.success(),
        "default --drc must stay exit 0 (existing-script contract); got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("short"), "expert table should list the shorts");
}

#[test]
fn drc_strict_exits_nonzero_on_shorts() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--strict"]);
    assert!(
        !out.status.success(),
        "--strict must fail the gate when shorts exist"
    );
}

#[test]
fn fail_on_findings_alias_works() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--fail-on-findings"]);
    assert!(
        !out.status.success(),
        "--fail-on-findings is the documented alias of --strict"
    );
}

#[test]
fn strict_on_clean_board_exits_zero() {
    let b = clean_board();
    // Lint is clean on this board, so --strict must NOT fail.
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--strict"]);
    assert!(
        out.status.success(),
        "--strict must exit 0 when there are no findings; got {:?}",
        out.status.code()
    );
}

#[test]
fn plain_drc_prints_verdict_and_what_why_fix() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--plain"]);
    assert!(out.status.success(), "--plain alone does not change exit code");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Verdict line + the three plain sections, in everyday language.
    assert!(stdout.contains("serious"), "should lead with a verdict counting serious issues");
    assert!(stdout.contains("Why it matters:"), "each finding explains why");
    assert!(stdout.contains("What to do:"), "each finding suggests a fix");
    // No raw enum-style token leaks into the plain text.
    assert!(!stdout.contains("ViolationKind"));
}

#[test]
fn explain_alias_matches_plain() {
    let b = shorted_board();
    let a = run(&["run", b.to_str().unwrap(), "--drc", "--plain"]);
    let c = run(&["run", b.to_str().unwrap(), "--drc", "--explain"]);
    assert_eq!(
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&c.stdout),
        "--explain is an alias of --plain"
    );
}

#[test]
fn plain_clean_board_reads_healthy() {
    let b = clean_board();
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--plain"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(stdout.contains("healthy"), "a clean board should read as healthy");
}

#[test]
fn plain_and_strict_compose() {
    // Plain output AND a non-zero exit on the same run.
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--plain", "--strict"]);
    assert!(!out.status.success(), "strict still gates with --plain on");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Why it matters:"), "plain text still printed");
}
