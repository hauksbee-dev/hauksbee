//! Progress reporting must never reach a consumer that is parsing the output.
//!
//! The reason the progress line exists at all is that a co-sim on a real board
//! runs for minutes with nothing on screen, which is indistinguishable from a
//! hang. The reason it needs guarding is that the same run feeds `--json` to a
//! parser and a CI log to a human reading a result, and a hundred progress
//! lines in either is a regression, not a feature.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// A spec small enough to finish in seconds, on a board that ships with the
/// crate.
fn tiny_spec(dir: &Path) -> PathBuf {
    let board = repo_root().join("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let spec = dir.join("tiny.toml");
    std::fs::write(
        &spec,
        format!(
            "name = \"tiny\"\n\
             board = \"{}\"\n\
             duration_ms = 5\n\
             [[supply]]\n\
             net = \"+5V\"\n\
             kind = \"ideal\"\n\
             volts = 5.0\n\
             [[assert]]\n\
             kind = \"no_faults\"\n",
            board.display()
        ),
    )
    .expect("write spec");
    spec
}

fn run(spec: &Path, extra: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .arg("run")
        .arg(spec)
        .args(extra)
        .output()
        .expect("run hauksbee-ci");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn a_redirected_run_prints_no_progress_at_all() {
    // Command::output gives the child pipes, not a terminal, which is the same
    // shape as `hauksbee-ci run spec > log 2>&1` in someone's CI.
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = tiny_spec(dir.path());
    let (stdout, stderr) = run(&spec, &[]);
    assert!(
        !stderr.contains("simulating"),
        "a log file must not fill with progress lines:\n{stderr}"
    );
    assert!(
        !stderr.contains('\r'),
        "and must not collect carriage returns:\n{stderr:?}"
    );
    assert!(
        stdout.contains("hauksbee-ci: tiny"),
        "the result itself still prints:\n{stdout}"
    );
}

#[test]
fn json_output_is_json_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = tiny_spec(dir.path());
    let (stdout, stderr) = run(&spec, &["--json"]);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must parse as JSON ({e}):\n{stdout}"));
    assert!(
        !stderr.contains("simulating"),
        "progress must not ride along with a parsed run:\n{stderr}"
    );
}

#[test]
fn progress_never_uses_stdout() {
    // Even on the human path, the line goes to stderr. Someone piping stdout to
    // a file and watching the terminal is a normal thing to do, and it must
    // leave the file clean and the progress visible.
    let src = std::fs::read_to_string(repo_root().join("crates/hauksbee-ci/src/progress.rs"))
        .expect("read progress.rs");
    assert!(
        !src.contains("stdout()"),
        "progress.rs must not touch stdout"
    );
    assert!(src.contains("stderr()"), "progress.rs writes to stderr");
}
