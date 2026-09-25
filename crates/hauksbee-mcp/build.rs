//! Embed a verified Hauksbee source commit for release identity.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HAUKSBEE_SOURCE_COMMIT");
    if let Ok(hash) = std::env::var("HAUKSBEE_SOURCE_COMMIT") {
        if !is_commit(&hash) {
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
    if !owns_workspace_layout {
        return;
    }
    let top = Command::new("git")
        .arg("-C")
        .arg(&source_root)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let owns_git_root = top.ok().filter(|out| out.status.success()).and_then(|out| {
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
            .canonicalize()
            .ok()
    }) == Some(source_root.clone());
    if !owns_git_root {
        return;
    }
    // Watch owned source trees, including new untracked files. A missing path reruns this on
    // every build (a linked worktree's git dir has no packed-refs), so watch only what exists.
    let watch = |path: &Path| {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    };
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
        "../../examples",
        "../../testdata",
        "../../README.md",
        "../../COMPLIANCE.md",
        "../../LICENSE",
        "../../NOTICE",
        "../../rust-toolchain.toml",
    ] {
        watch(Path::new(path));
    }
    let source_is_clean = Command::new("git")
        .arg("--no-optional-locks") // Do not refresh the watched index during a build.
        .arg("-C")
        .arg(&source_root)
        .args(["status", "--porcelain", "--untracked-files=normal", "--"])
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.is_empty());
    if source_is_clean {
        if let Ok(out) = Command::new("git")
            .args(["-C", source_root.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
        {
            let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && is_commit(&hash) {
                println!("cargo:rustc-env=GIT_HASH={hash}");
            }
        }
    }
    let dot_git = source_root.join(".git");
    if dot_git.is_file() {
        println!("cargo:rerun-if-changed={}", dot_git.display());
    }
    // HEAD and index are this worktree's own; the shared common dir adds only branch refs.
    let own = ["HEAD", "index", "packed-refs", "refs"];
    for (selector, names) in [("--git-dir", &own[..]), ("--git-common-dir", &own[2..])] {
        if let Ok(out) = Command::new("git")
            .args(["-C", source_root.to_str().unwrap(), "rev-parse", selector])
            .output()
        {
            if out.status.success() {
                let dir = source_root.join(String::from_utf8_lossy(&out.stdout).trim());
                for name in names {
                    watch(&dir.join(name));
                }
            }
        }
    }
}

fn is_commit(hash: &str) -> bool {
    hash.len() == 40 && hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}
