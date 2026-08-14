//! Embed the git commit hash into the binary so `hauksbee-ci --version` can
//! name the exact build. The installed pre-commit hook records this string and
//! warns at run time when the binary on PATH is a different build, so a stale
//! hook is visible instead of silently diverging (see src/integrate.rs).
//!
//! Best-effort by design: outside this repository's own Git root (a crates.io
//! build, source tarball, or vendored copy inside a consumer repo) `GIT_HASH`
//! is absent. Never borrow the enclosing consumer repository's HEAD.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    for path in [
        "../../Cargo.toml",
        "../../Cargo.lock",
        "../../crates",
        "../../vendor",
        "../../frontend/src",
        "../../frontend/dist",
        "../../.github",
        "../../app",
        "../../docker",
        "../../docs",
        "../../editors",
        "../../evidence",
        "../../integrations",
        "../../licenses",
        "../../qc",
        "../../scripts",
        "../../site",
        "../../examples",
        "../../testdata",
        "../../README.md",
        "../../COMPLIANCE.md",
        "../../LICENSE",
        "../../NOTICE",
        "../../rust-toolchain.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=HAUKSBEE_SOURCE_COMMIT");
    println!("cargo:rerun-if-env-changed=HAUKSBEE_RELEASE_TAG");
    let release_tag = std::env::var("HAUKSBEE_RELEASE_TAG").ok();
    if let Some(tag) = release_tag.as_deref() {
        let version = std::env::var("CARGO_PKG_VERSION")
            .expect("Cargo must provide CARGO_PKG_VERSION for a release-tagged build");
        let expected = format!("v{version}");
        if tag != expected {
            panic!(
                "HAUKSBEE_RELEASE_TAG must equal this package version ({})",
                expected
            );
        }
        println!("cargo:rustc-env=GIT_TAG={tag}");
    }
    if let Ok(hash) = std::env::var("HAUKSBEE_SOURCE_COMMIT") {
        if hash.len() != 40
            || !hash
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            panic!("HAUKSBEE_SOURCE_COMMIT must be one lowercase 40-character hexadecimal commit");
        }
        println!("cargo:rustc-env=GIT_HASH={hash}");
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .canonicalize()
        .expect("canonical Hauksbee CI manifest directory");
    let source_root = manifest_dir
        .join("../..")
        .canonicalize()
        .expect("canonical Hauksbee source root");
    let owns_workspace_layout = source_root
        .join("crates/hauksbee-ci")
        .canonicalize()
        .ok()
        .as_ref()
        == Some(&manifest_dir);
    let top = Command::new("git")
        .args([
            "-C",
            source_root.to_str().unwrap(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output();
    let owns_git_root = top.ok().filter(|out| out.status.success()).and_then(|out| {
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
            .canonicalize()
            .ok()
    }) == Some(source_root.clone());
    let source_is_clean = Command::new("git")
        .args([
            "-C",
            source_root.to_str().unwrap(),
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--",
        ])
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.is_empty());
    if owns_workspace_layout && owns_git_root && source_is_clean {
        let out = Command::new("git")
            .args(["-C", source_root.to_str().unwrap(), "rev-parse", "HEAD"])
            .output();
        if let Ok(o) = out {
            let hash = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success()
                && hash.len() == 40
                && hash
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                println!("cargo:rustc-env=GIT_HASH={hash}");
            }
        }
        if release_tag.is_none() {
            if let Ok(version) = std::env::var("CARGO_PKG_VERSION") {
                let expected = format!("v{version}");
                let tag = Command::new("git")
                    .args([
                        "-C",
                        source_root.to_str().unwrap(),
                        "describe",
                        "--tags",
                        "--exact-match",
                        "HEAD",
                    ])
                    .output();
                if tag.is_ok_and(|out| {
                    out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == expected
                }) {
                    println!("cargo:rustc-env=GIT_TAG={expected}");
                }
            }
        }
    }
    // Re-run when the checked-out commit changes (branch switch or new commit).
    // Harmless when the paths do not exist (non-git builds).
    let dot_git = source_root.join(".git");
    if dot_git.is_file() {
        println!("cargo:rerun-if-changed={}", dot_git.display());
    }
    for selector in ["--git-dir", "--git-common-dir"] {
        if let Ok(out) = Command::new("git")
            .args(["-C", source_root.to_str().unwrap(), "rev-parse", selector])
            .output()
        {
            if out.status.success() {
                let raw = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
                let dir = if raw.is_absolute() {
                    raw
                } else {
                    source_root.join(raw)
                };
                for name in ["HEAD", "index", "packed-refs", "refs"] {
                    println!("cargo:rerun-if-changed={}", dir.join(name).display());
                }
            }
        }
    }
}
