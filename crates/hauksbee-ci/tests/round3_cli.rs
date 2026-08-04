//! Round-3 audit fixes, hauksbee-ci half: board-file-as-spec detection (B2)
//! and the width-capped TOML error context, proven against the real binary.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().expect("binary runs")
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn run_on_a_board_file_names_the_actual_fix_without_dumping_the_board() {
    let b = blinky_board();
    let out = run(&["run", b.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "spec-error exit");
    let err = stderr(&out);
    assert!(
        err.contains("is a board, not a spec"),
        "names what happened: {err}"
    );
    assert!(
        err.contains("hauksbee-ci init"),
        "names the scaffolding command: {err}"
    );
    assert!(
        !err.contains("(kicad_pcb"),
        "the board file must not be dumped as TOML context: {err}"
    );
}

#[test]
fn renamed_board_content_is_sniffed_too() {
    // A board renamed .toml must be caught by content, not only extension.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("board.toml");
    let text = std::fs::read_to_string(blinky_board()).unwrap();
    std::fs::write(&p, text).unwrap();
    let out = run(&["run", p.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("is a board, not a spec"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn toml_error_context_lines_are_width_capped() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("giant.toml");
    // One enormous malformed line: the TOML snippet must be capped, not dumped.
    std::fs::write(&p, format!("name = \"x\"\nboard = {}\n", "z".repeat(5000))).unwrap();
    let out = run(&["run", p.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    let longest = err.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        longest <= 400,
        "TOML context must be width-capped, longest line was {longest}:\n{err}"
    );
}
