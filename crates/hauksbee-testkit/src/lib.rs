//! Locating the test assets that are not in this repository.
//!
//! Some suites need real boards: the public board corpus (fetchable, see
//! `corpus.toml` and `scripts/fetch-corpus.sh`) and a handful of private
//! designs that cannot be redistributed. Neither can be committed here, so
//! tests have to cope with their absence.
//!
//! The rule this module exists to enforce: **a test that cannot run must not
//! report success.** An early `return` on a missing fixture leaves a green tick
//! next to a test that verified nothing, which is exactly the vacuous pass the
//! product refuses to emit for boards. Use [`corpus_or_skip`] and friends, and
//! set `HAUKSBEE_REQUIRE_CORPUS=1` (as CI does) to turn absence into failure.

use std::path::{Path, PathBuf};

/// The repository root, derived from the calling crate's manifest directory.
///
/// `CARGO_MANIFEST_DIR` is set per-crate at compile time, so this takes it as
/// an argument rather than reading its own (which would point at this crate).
pub fn repo_root(manifest_dir: &str) -> PathBuf {
    // crates/<name>/ -> repository root
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(manifest_dir))
}

/// True when a missing asset should fail the run instead of skipping it.
pub fn require_assets() -> bool {
    std::env::var_os("HAUKSBEE_REQUIRE_CORPUS").is_some()
}

/// The board corpus, if this machine has one.
///
/// `HAUKSBEE_CORPUS_DIR` wins; otherwise `board-corpus/` beside the checkout,
/// which is where `scripts/fetch-corpus.sh` puts it by default.
pub fn corpus_dir(manifest_dir: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HAUKSBEE_CORPUS_DIR") {
        let p = PathBuf::from(dir);
        return p.is_dir().then_some(p);
    }
    let root = repo_root(manifest_dir);
    for candidate in [
        root.join("board-corpus"),
        root.parent()?.join("board-corpus"),
    ] {
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// One board inside the corpus, by path relative to the corpus root.
pub fn corpus_board(manifest_dir: &str, rel: &str) -> Option<PathBuf> {
    let p = corpus_dir(manifest_dir)?.join(rel);
    p.exists().then_some(p)
}

/// A corpus path, or `None` with a visible note saying the test did not run.
///
/// Under `HAUKSBEE_REQUIRE_CORPUS=1` this panics instead, so a CI run cannot
/// pass by skipping. `what` names the test so the note says which one idled.
pub fn corpus_or_skip(manifest_dir: &str, rel: &str, what: &str) -> Option<PathBuf> {
    match corpus_board(manifest_dir, rel) {
        Some(p) => Some(p),
        None => missing(what, rel, "board corpus", "scripts/fetch-corpus.sh"),
    }
}

/// A private asset that is not redistributable, keyed off an env var so a
/// maintainer can point at their own checkout.
///
/// `env_var` names the override; `rel` is the path under it. Absent, this
/// behaves like [`corpus_or_skip`]: a loud note, or a panic under
/// `HAUKSBEE_REQUIRE_CORPUS=1`.
pub fn private_asset(env_var: &str, rel: &str, what: &str) -> Option<PathBuf> {
    let base = std::env::var_os(env_var)?;
    let p = PathBuf::from(base).join(rel);
    if p.exists() {
        return Some(p);
    }
    missing(
        what,
        rel,
        env_var,
        &format!("set {env_var} to a checkout containing it"),
    )
}

fn missing(what: &str, rel: &str, source: &str, remedy: &str) -> Option<PathBuf> {
    let msg = format!("{what}: {rel} not found via {source}. Get it with: {remedy}");
    assert!(!require_assets(), "HAUKSBEE_REQUIRE_CORPUS=1 and {msg}");
    eprintln!("NOT RUN  {msg}");
    None
}
