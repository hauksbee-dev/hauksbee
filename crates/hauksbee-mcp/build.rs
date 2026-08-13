//! Embed a verified Hauksbee source commit for release identity.

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
        .expect("canonical Hauksbee MCP manifest directory");
    let source_root = manifest_dir
        .join("../..")
        .canonicalize()
        .expect("canonical Hauksbee source root");
    let owns_workspace_layout = source_root
        .join("crates/hauksbee-mcp")
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
        if let Ok(out) = Command::new("git")
            .args(["-C", source_root.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
        {
            let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success()
                && hash.len() == 40
                && hash
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                println!("cargo:rustc-env=GIT_HASH={hash}");
            }
        }
    }
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
