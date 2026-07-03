//! CLI-level tests for two first-time-user defects on `hauksbee run`:
//!
//! A. A missing `--firmware` file must be a clean, actionable error, NOT a
//!    SIGSEGV (exit 139) out of libsimavr's native loader.
//! B. The "other board file(s) found nearby" advisory must never land on stdout
//!    (it pollutes piped / report / JSON output), and `--quiet` must silence it.
//!
//! These drive the actual compiled binary so the exit-code and output contracts
//! are tested end to end.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// A board that sits beside two sibling `.kicad_pcb` files, so the "found
/// nearby" advisory would fire for it.
fn board_with_siblings() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

// --- Defect A: missing firmware is a clean error, not a segfault ------------

#[test]
fn missing_firmware_is_clean_error_not_segfault() {
    let board = board_with_siblings();
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--firmware",
        "does_not_exist.hex",
        "--headless",
        "--seconds",
        "0.05",
    ]);
    // A SIGSEGV leaves `code()` == None (killed by signal 11) and the shell sees
    // 139. The guard must instead exit with the ordinary error code (1).
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected clean error exit 1, got {:?} (stderr: {})",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Actionable: names the tried path and says the file is missing.
    assert!(
        stderr.contains("does_not_exist.hex"),
        "stderr names the tried path: {stderr}"
    );
    assert!(
        stderr.contains("no firmware file"),
        "stderr explains the failure: {stderr}"
    );
    // No native crash chatter or stack trace.
    assert!(
        !stderr.contains("panicked"),
        "no panic / stack trace: {stderr}"
    );
}

// --- Defect B: the nearby-boards note never pollutes stdout -----------------

#[test]
fn nearby_boards_note_never_on_stdout_in_report_mode() {
    let board = board_with_siblings();
    let out = run(&["run", board.to_str().unwrap(), "--report"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("found nearby"),
        "the advisory must not pollute report stdout: {stdout}"
    );
}

#[test]
fn nearby_boards_note_never_on_stdout_in_json_mode() {
    let board = board_with_siblings();
    let out = run(&["run", board.to_str().unwrap(), "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("found nearby") && !stdout.contains("note:"),
        "JSON stdout must be free of the advisory: {stdout}"
    );
}

#[test]
fn quiet_flag_is_accepted_and_silences_the_note() {
    let board = board_with_siblings();
    // --quiet is a global flag: it must parse in any position and suppress the
    // advisory on both streams. (Under a piped test stdout the note is already
    // TTY-suppressed; this asserts --quiet is a real, accepted switch and never
    // reaches stdout.)
    let out = run(&["run", board.to_str().unwrap(), "--report", "--quiet"]);
    assert!(
        out.status.success(),
        "run --report --quiet succeeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("found nearby"));
    assert!(
        !stderr.contains("found nearby"),
        "--quiet silences the advisory on stderr too: {stderr}"
    );
}
