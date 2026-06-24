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
fn ampacity_prints_capacity_only_report() {
    let path = std::env::temp_dir().join(format!(
        "hauksbee-ampacity-{}-{}.kicad_pcb",
        std::process::id(),
        "power"
    ));
    std::fs::write(
        &path,
        r#"(kicad_pcb (version 20260206) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "+BATT")
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
)"#,
    )
    .expect("write temp board");
    let out = run(&["run", path.to_str().unwrap(), "--ampacity"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("capacity only"));
    assert!(stdout.contains("supply a current"));
    assert!(stdout.contains("+BATT"));
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
    assert!(
        stdout.contains("short"),
        "expert table should list the shorts"
    );
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
    assert!(
        out.status.success(),
        "--plain alone does not change exit code"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Verdict line + the three plain sections, in everyday language.
    assert!(
        stdout.contains("serious"),
        "should lead with a verdict counting serious issues"
    );
    assert!(
        stdout.contains("Why it matters:"),
        "each finding explains why"
    );
    assert!(
        stdout.contains("What to do:"),
        "each finding suggests a fix"
    );
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
    assert!(
        stdout.contains("healthy"),
        "a clean board should read as healthy"
    );
}

#[test]
fn ac_all_requested_nodes_missing_is_invalid_not_valid() {
    // Honesty hole fix (Finding 1): when EVERY requested --ac-node is absent from
    // the circuit, the AC sweep produced no data for any of them. The tool must
    // refuse that as INVALID for the requested analysis (valid:false + exit 3),
    // not silently report `ac: { valid: true, nets: [] }` with exit 0.
    let b = clean_board();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--ac",
        "1:1e6:20",
        "--ac-node",
        "/NONEXISTENT",
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "all-missing AC nodes must exit 3 (EXIT_INVALID_FOR_ANALYSIS), got {:?}; stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json must emit parseable JSON");
    assert_eq!(
        v["ac"]["valid"],
        serde_json::Value::Bool(false),
        "ac.valid must be false when no requested node exists; got: {stdout}"
    );
    let reason = v["ac"]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("/NONEXISTENT"),
        "the reason must name the missing requested node(s); got: {reason}"
    );
}

#[test]
fn ac_all_requested_nodes_missing_text_warns_and_exits_three() {
    // Same honesty hole on the TEXT surface: a WARNING line + exit 3, never a
    // table presented as a valid result.
    let b = clean_board();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--ac",
        "1:1e6:20",
        "--ac-node",
        "/NONEXISTENT",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "text path must also exit 3; got {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WARNING") && stderr.contains("not valid"),
        "expected a WARNING that the AC result is not valid; got: {stderr}"
    );
}

#[test]
fn plain_and_strict_compose() {
    // Plain output AND a non-zero exit on the same run.
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--plain", "--strict"]);
    assert!(!out.status.success(), "strict still gates with --plain on");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Why it matters:"),
        "plain text still printed"
    );
}
