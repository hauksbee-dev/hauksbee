//! Shared helpers for the engine crate's integration-test binary.

use std::path::PathBuf;
use std::process::{Command, Output};

/// The compiled `hauksbee` binary (Cargo sets this for the engine crate's tests).
#[allow(dead_code)]
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// The compiled `hauksbee` binary as an owned path.
#[allow(dead_code)]
pub fn hauksbee_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hauksbee"))
}

/// Workspace-relative example boards, resolved from this crate's manifest dir.
#[allow(dead_code)]
pub fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Run the compiled binary and wait for it to exit.
#[allow(dead_code)]
pub fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

/// Decode a captured `Output`'s stdout as UTF-8.
#[allow(dead_code)]
pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Decode a captured `Output`'s stderr as UTF-8.
#[allow(dead_code)]
pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The repo root, two levels up from this crate's manifest dir.
#[allow(dead_code)]
pub fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}
