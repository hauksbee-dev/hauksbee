//! Embed the git commit hash into the binary so `hauksbee --version` can name
//! the exact build (an agent operator diffing behaviour across sessions needs
//! more than a crate version that changes once a release).
//!
//! Best-effort by design: outside a git checkout (a crates.io build, a source
//! tarball) `GIT_HASH` is simply absent and the version string falls back to
//! the bare crate version via `option_env!`. No build dependency, no failure
//! mode: a missing `git` binary or repo just means no hash.

use std::process::Command;

fn main() {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let hash = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !hash.is_empty() {
                println!("cargo:rustc-env=GIT_HASH={hash}");
            }
        }
    }
    // Re-run when the checked-out commit changes (branch switch or new commit).
    // Harmless when the paths do not exist (non-git builds).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
}
