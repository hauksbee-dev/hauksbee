//! A waiver must actually change a build, and must stop doing so on its date.
//!
//! The unit tests in `waiver.rs` cover matching and parsing. These drive the
//! real binary against a real board, because the thing worth proving is not
//! that the matcher matches: it is that a red `--check --strict` turns green
//! for a stated reason, and turns red again when the reason expires. A waiver
//! system that silently fails to apply is a check nobody can trust, and one
//! that silently keeps applying past its date is a check nobody is running.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The board ships two deliberate GND/+5V pour shorts, so `--check --strict`
/// exits 2 on it untouched. That makes it the honest fixture for this.
const BOARD: &str = "examples/boards/boot_gate.kicad_pcb";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn hauksbee_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hauksbee"))
}

/// Copy the board into `dir` so a waiver file can sit beside it without
/// touching the checked-in fixture.
fn stage_board(dir: &Path) -> PathBuf {
    let src = repo_root().join("crates/hauksbee-ci").join(BOARD);
    assert!(src.is_file(), "fixture board missing at {}", src.display());
    let dst = dir.join("board.kicad_pcb");
    std::fs::copy(&src, &dst).expect("copy board");
    dst
}

fn write_waivers(dir: &Path, until: &str, nets: &str) {
    std::fs::write(
        dir.join(hauksbee_engine::waiver::DEFAULT_WAIVER_FILE),
        format!(
            r#"
[[waive]]
check = "drc"
kind = "short"
nets = {nets}
reason = "fixture: the pour bridges these on purpose"
until = "{until}"
"#
        ),
    )
    .expect("write waivers");
}

/// `--check --strict` on the board, returning (exit code, stdout+stderr).
///
/// Asserts the run actually produced a report. A usage error also exits 2, and
/// a test that accepted it would pass while proving nothing about the gate.
fn check_strict(board: &Path) -> (i32, String) {
    let out = Command::new(hauksbee_bin())
        .arg("run")
        .args([board.to_str().unwrap(), "--check", "--strict"])
        .output()
        .expect("run hauksbee");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        s.contains("Copper spacing"),
        "hauksbee did not run the check at all:\n{s}"
    );
    (out.status.code().unwrap_or(-1), s)
}

#[test]
fn the_board_gates_red_before_any_waiver_exists() {
    // Everything below is meaningless if the fixture is already green, so this
    // asserts the premise rather than assuming it.
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    let (code, _) = check_strict(&board);
    assert_eq!(code, 2, "the fixture board must fail --check --strict");
}

#[test]
fn an_active_waiver_turns_the_gate_green_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    write_waivers(dir.path(), "2099-01-01", r#"["GND", "+5V"]"#);

    let (code, out) = check_strict(&board);
    assert_eq!(
        code, 0,
        "an active waiver takes the finding out of the gate:\n{out}"
    );
    assert!(
        out.contains("Waived"),
        "the waived section must appear:\n{out}"
    );
    assert!(
        out.contains("the pour bridges these on purpose"),
        "the reason travels with the report, or the waiver is just a mute button:\n{out}"
    );
}

#[test]
fn an_expired_waiver_puts_the_finding_back_in_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    write_waivers(dir.path(), "2020-01-01", r#"["GND", "+5V"]"#);

    let (code, out) = check_strict(&board);
    assert_eq!(
        code, 2,
        "expiry is the whole point: a lapsed waiver stops covering anything:\n{out}"
    );
    assert!(
        out.contains("lapsed"),
        "and the report explains the red rather than leaving it mysterious:\n{out}"
    );
}

#[test]
fn a_waiver_matching_nothing_is_called_out() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    write_waivers(dir.path(), "2099-01-01", r#"["NOSUCHNET"]"#);

    let (code, out) = check_strict(&board);
    assert_eq!(code, 2, "it covers nothing, so the real shorts still gate");
    assert!(
        out.contains("matched nothing"),
        "a waiver outliving its finding is how the file rots:\n{out}"
    );
}

// ===========================================================================
// Single-check parity: `--drc --strict`, `--lint --strict`, `--si --strict`.
//
// The aggregate `--check --strict` consults the waiver file, so if the
// narrower commands did not, the SAME board would be green under one and red
// under the other, and the waiver would look like it silently stopped
// applying. Each family gets the full life-cycle: gates without a waiver,
// green with an active one, red again once it expires, and red (with a
// warning) when the file is malformed.
// ===========================================================================

/// Run `hauksbee run <board> <flag> --strict`, returning (exit code, stdout+stderr).
fn single_check_strict(board: &Path, flag: &str) -> (i32, String) {
    let out = Command::new(hauksbee_bin())
        .arg("run")
        .args([board.to_str().unwrap(), flag, "--strict"])
        .output()
        .expect("run hauksbee");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

/// A tiny board whose only lint finding is a Medium `missing_i2c_pullup`: an
/// entirely on-board SDA bus (two ICs, no resistor, no connector), which is the
/// high-confidence regime the lint gates on.
fn stage_lint_board(dir: &Path) -> PathBuf {
    let p = dir.join("board.kicad_pcb");
    std::fs::write(
        &p,
        r#"(kicad_pcb (version 20240101) (generator pcbnew)
  (net 0 "")
  (net 1 "SDA")
  (net 2 "GND")
  (footprint "Package_SO:SOIC-8" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "BUSDEV-A")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "SDA"))
    (pad "2" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 2 "GND")))
  (footprint "Package_SO:SOIC-8" (layer "F.Cu") (at 10 0)
    (property "Reference" "U2")
    (property "Value" "BUSDEV-B")
    (pad "1" smd rect (at 10 0) (size 1 1) (layers "F.Cu") (net 1 "SDA"))
    (pad "2" smd rect (at 10 2) (size 1 1) (layers "F.Cu") (net 2 "GND")))
)"#,
    )
    .expect("write lint board");
    p
}

/// A board whose only SI finding is the IPC-2221 trace-ampacity one: a TP4054
/// in its 400 mA constant-current phase, programmed by the published 1.66 kOhm
/// application value, whose VBAT rail is a 0.05 mm hair of a trace. This is an
/// operating-current assertion, not a device capability rating. Same recipe as
/// tests/si_ampacity_ripple.rs.
fn stage_si_board(dir: &Path) -> PathBuf {
    let p = dir.join("board.kicad_pcb");
    std::fs::write(
        &p,
        r#"(kicad_pcb (version 20240101) (generator pcbnew)
  (net 0 "")
  (net 1 "VBAT")
  (net 2 "VIN")
  (net 3 "GND")
  (net 4 "PROG")
  (footprint "Package_TO_SOT_SMD:SOT-23-5" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "TP4054")
    (pad "1" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
    (pad "2" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "GND"))
    (pad "3" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VBAT"))
    (pad "4" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 2 "VIN"))
    (pad "5" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 4 "PROG")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 4 0)
    (property "Reference" "R1")
    (property "Value" "1.66k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "PROG"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 3 "GND")))
  (segment (start 0 0) (end 10 0) (width 0.05) (layer "F.Cu") (net 1))
)"#,
    )
    .expect("write si board");
    p
}

/// Write a one-entry waiver file beside the board.
fn write_waiver(dir: &Path, check: &str, kind: &str, nets: &str, until: &str) {
    std::fs::write(
        dir.join(hauksbee_engine::waiver::DEFAULT_WAIVER_FILE),
        format!(
            r#"
[[waive]]
check = "{check}"
kind = "{kind}"
nets = {nets}
reason = "fixture: judged wrong on this board on purpose"
until = "{until}"
"#
        ),
    )
    .expect("write waivers");
}

/// A waiver file with no `reason`, which the loader refuses. Every single-check
/// command must warn and gate, exactly like `--check` does.
fn write_malformed_waiver(dir: &Path, check: &str, kind: &str, nets: &str) {
    std::fs::write(
        dir.join(hauksbee_engine::waiver::DEFAULT_WAIVER_FILE),
        format!("[[waive]]\ncheck = \"{check}\"\nkind = \"{kind}\"\nnets = {nets}\nuntil = \"2099-01-01\"\n"),
    )
    .expect("write malformed waivers");
}

/// The whole life-cycle for one single-check command: red bare, green under an
/// active waiver (with the reason on the report), red again expired (named as
/// lapsed), and red with a warning when the file is malformed.
fn assert_waiver_parity(dir: &Path, board: &Path, flag: &str, check: &str, kind: &str, nets: &str) {
    // (a) The premise: without a waiver the finding gates, otherwise the rest
    // of this proves nothing.
    let (code, out) = single_check_strict(board, flag);
    assert_eq!(code, 2, "{flag} --strict must gate bare:\n{out}");

    // (b) An active waiver takes the finding out of the gate, out loud.
    write_waiver(dir, check, kind, nets, "2099-01-01");
    let (code, out) = single_check_strict(board, flag);
    let expected_code = if flag == "--lint" { 3 } else { 0 };
    assert_eq!(
        code, expected_code,
        "{flag} must remove the waived finding from the gate without turning independently undermined evidence green:\n{out}"
    );
    assert!(
        out.contains("Waived"),
        "the waived section must appear on {flag}:\n{out}"
    );
    assert!(
        out.contains("judged wrong on this board on purpose"),
        "the reason travels with the {flag} report too:\n{out}"
    );

    // (c) Expiry puts it back.
    write_waiver(dir, check, kind, nets, "2020-01-01");
    let (code, out) = single_check_strict(board, flag);
    assert_eq!(
        code, 2,
        "a lapsed waiver must stop covering {flag} findings:\n{out}"
    );
    assert!(
        out.contains("lapsed"),
        "{flag} explains the red rather than leaving it mysterious:\n{out}"
    );

    // (d) A malformed file warns and fails closed.
    write_malformed_waiver(dir, check, kind, nets);
    let (code, out) = single_check_strict(board, flag);
    assert_eq!(
        code, 2,
        "a typo must not silently disable a check on {flag}:\n{out}"
    );
    assert!(
        out.contains("ignoring the waiver file"),
        "and {flag} says so instead of pretending the file was fine:\n{out}"
    );
}

#[test]
fn drc_single_check_honours_the_same_waivers_as_check() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    assert_waiver_parity(
        dir.path(),
        &board,
        "--drc",
        "drc",
        "short",
        r#"["GND", "+5V"]"#,
    );
}

#[test]
fn lint_single_check_honours_the_same_waivers_as_check() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_lint_board(dir.path());
    assert_waiver_parity(
        dir.path(),
        &board,
        "--lint",
        "lint",
        "missing_i2c_pullup",
        r#"["SDA"]"#,
    );
}

#[test]
fn si_single_check_honours_the_same_waivers_as_check() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_si_board(dir.path());
    assert_waiver_parity(
        dir.path(),
        &board,
        "--si",
        "si",
        "trace_ampacity",
        r#"["VBAT"]"#,
    );
}

/// The machine surface must carry the waived list too: a `--drc --json` whose
/// verdict quietly dropped a short would be worse than no waivers at all.
#[test]
fn single_check_json_carries_the_waived_findings() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    write_waivers(dir.path(), "2099-01-01", r#"["GND", "+5V"]"#);

    let out = Command::new(hauksbee_bin())
        .arg("run")
        .args([board.to_str().unwrap(), "--drc", "--json", "--strict"])
        .output()
        .expect("run hauksbee");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the waiver gates --drc --json --strict green too"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("one valid JSON document");
    let waived = v
        .get("waived")
        .and_then(|w| w.as_array())
        .expect("waived array present");
    assert!(
        !waived.is_empty()
            && waived
                .iter()
                .any(|w| w.get("kind").and_then(|k| k.as_str()) == Some("short")),
        "the overruled shorts ride the JSON report: {stdout}"
    );
}

/// A waiver for a check a narrower command never ran must not be called stale
/// there: `--drc` cannot know whether an SI waiver still earns its place.
#[test]
fn a_foreign_checks_waiver_is_not_reported_stale_by_a_narrower_command() {
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    // An SI waiver beside a board being run under --drc: out of scope there.
    write_waiver(
        dir.path(),
        "si",
        "trace_ampacity",
        r#"["+3V3"]"#,
        "2099-01-01",
    );

    let (code, out) = single_check_strict(&board, "--drc");
    assert_eq!(code, 2, "the real shorts still gate");
    assert!(
        !out.contains("matched nothing"),
        "--drc never ran the SI checks, so it cannot judge an SI waiver stale:\n{out}"
    );
}

#[test]
fn a_malformed_waiver_file_fails_closed() {
    // A typo must not silently disable a check. The findings gate, and the
    // reason is on stderr.
    let dir = tempfile::tempdir().unwrap();
    let board = stage_board(dir.path());
    std::fs::write(
        dir.path()
            .join(hauksbee_engine::waiver::DEFAULT_WAIVER_FILE),
        "[[waive]]\ncheck = \"drc\"\nkind = \"short\"\nnets = [\"GND\"]\nuntil = \"2099-01-01\"\n",
    )
    .unwrap();

    let (code, out) = check_strict(&board);
    assert_eq!(
        code, 2,
        "no reason means no waiver, so the board still gates"
    );
    assert!(
        out.contains("ignoring the waiver file"),
        "and the run says so instead of pretending the file was fine:\n{out}"
    );
}
