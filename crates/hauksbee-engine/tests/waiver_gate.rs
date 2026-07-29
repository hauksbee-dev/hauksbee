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
