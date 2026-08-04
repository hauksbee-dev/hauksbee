//! Round-3 audit fixes, engine CLI half: contracts that only the compiled
//! binary can prove (error-handler routing, exit codes, output envelopes).

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

// ── B1: --example resolution goes through the normal error handler ──────────

#[test]
fn run_unknown_example_uses_the_lowercase_error_handler() {
    let out = run(&["run", "--example", "bogus", "--check"]);
    let err = stderr(&out);
    assert!(
        err.contains("error: no embedded example board named 'bogus'"),
        "the normal lowercase handler formats it: {err}"
    );
    assert!(
        !err.contains("Error:") && !err.to_lowercase().contains("backtrace"),
        "anyhow's default Error/backtrace rendering must not leak: {err}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "input errors exit 1, like every other run input error"
    );
}

#[test]
fn run_unknown_example_under_json_emits_the_json_envelope() {
    let out = run(&["run", "--example", "bogus", "--json"]);
    let so = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(so.trim())
        .unwrap_or_else(|e| panic!("--json must stay JSON on this error path: {e}\n{so}"));
    assert_eq!(v["ok"], serde_json::Value::Bool(false));
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no embedded example board named 'bogus'"),
        "the error field carries the message: {so}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn sim_unknown_example_uses_the_lowercase_error_handler() {
    let out = run(&["sim", "--example", "bogus"]);
    let err = stderr(&out);
    assert!(
        err.contains("error: no embedded example deck named 'bogus'"),
        "the normal handler formats it: {err}"
    );
    assert!(
        !err.contains("Error:") && !err.to_lowercase().contains("backtrace"),
        "anyhow's default rendering must not leak: {err}"
    );
    assert_eq!(out.status.code(), Some(1));
}

// ── B2 (engine half): a board file handed to `models lint` ──────────────────

#[test]
fn models_lint_on_a_board_names_the_actual_fix_without_dumping_the_board() {
    let b = blinky_board();
    let out = run(&["models", "lint", b.to_str().unwrap()]);
    let err = stderr(&out);
    assert!(
        err.contains("is a board, not a model spec"),
        "names what happened: {err}"
    );
    assert!(
        err.contains("hauksbee models resolve"),
        "names the command they meant: {err}"
    );
    assert!(
        !err.contains("(kicad_pcb"),
        "the board content must not be dumped as error context: {err}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn models_lint_toml_error_context_is_width_capped() {
    // A non-board TOML file whose failing line is enormous: the parser's
    // caret snippet must be width-capped, not dumped whole.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("giant.toml");
    let giant = format!("[models]]\n# {}\n", "x".repeat(5000));
    std::fs::write(&p, giant).unwrap();
    let out = run(&["models", "lint", p.to_str().unwrap()]);
    assert_ne!(out.status.code(), Some(0));
    let err = stderr(&out);
    let longest = err.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        longest <= 400,
        "error context lines must be width-capped, longest was {longest}:\n{err}"
    );
}
