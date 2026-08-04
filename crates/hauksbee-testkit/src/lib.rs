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
///
/// Two layouts are accepted, and that is not tidiness: the tests were written
/// against a hand-built corpus laid out as `famous/<id>/...`, and
/// `scripts/fetch-corpus.sh` writes `<id>/...` with no `famous/` level. They
/// had never agreed, so every corpus test skipped for anyone who followed
/// CONTRIBUTING and ran the fetch: the directory existed, so the skip did not
/// fire, and no board was ever found at the path a test asked for. A gate that
/// silently matches nothing is worse than no gate, because it reports as
/// evidence. Resolving both layouts fixes it for both users without forcing
/// either to move their corpus.
pub fn corpus_board(manifest_dir: &str, rel: &str) -> Option<PathBuf> {
    let root = corpus_dir(manifest_dir)?;
    let direct = root.join(rel);
    if direct.exists() {
        return Some(direct);
    }
    // The fetch layout: the same path with the `famous/` level removed.
    if let Some(stripped) = rel.strip_prefix("famous/") {
        let p = root.join(stripped);
        if p.exists() {
            return Some(p);
        }
    }
    // The hand-built layout, for a rel that did not carry the prefix.
    let p = root.join("famous").join(rel);
    p.exists().then_some(p)
}

/// The directory the boards sit *directly* under, for a sweep that walks the
/// whole corpus rather than naming one board.
///
/// `<corpus>/famous` in the hand-built layout, `<corpus>` itself in the fetch
/// layout. A sweep that joined `famous` unconditionally walked nothing under
/// the fetch layout and reported the empty walk as a pass.
pub fn corpus_boards_root(manifest_dir: &str) -> Option<PathBuf> {
    let root = corpus_dir(manifest_dir)?;
    let famous = root.join("famous");
    Some(if famous.is_dir() { famous } else { root })
}

/// The first of several candidate relative paths that resolves.
///
/// Layout tolerance is not enough when the two corpora hold different upstream
/// revisions of the same board: the hand-built corpus pinned
/// `rp2040_minimal_kicad/minimal/RP2040_minimal_r2`, the fetch pins
/// `rp2040_minimal_kicad/RPI-RP2040-MINIMAL_R3-S1_public`, and neither name
/// resolves in the other tree. Listing both keeps the gate live on either,
/// where naming only one silently skips for half the world.
pub fn corpus_board_any(manifest_dir: &str, rels: &[&str]) -> Option<PathBuf> {
    rels.iter().find_map(|rel| corpus_board(manifest_dir, rel))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not several: it mutates `HAUKSBEE_CORPUS_DIR`, which is
    /// process-wide, and cargo runs tests in one binary concurrently.
    #[test]
    fn resolves_a_board_under_either_corpus_layout() {
        let base = std::env::temp_dir().join("hauksbee_testkit_layouts");
        let _ = std::fs::remove_dir_all(&base);
        let handbuilt = base.join("handbuilt");
        let fetched = base.join("fetched");
        std::fs::create_dir_all(handbuilt.join("famous/acme/rev_a")).unwrap();
        std::fs::create_dir_all(fetched.join("acme/rev_b")).unwrap();

        let rels = ["famous/acme/rev_a", "famous/acme/rev_b"];
        // `manifest_dir` is unused while the env var is set, so any path does.
        let md = env!("CARGO_MANIFEST_DIR");

        std::env::set_var("HAUKSBEE_CORPUS_DIR", &handbuilt);
        assert_eq!(
            corpus_boards_root(md).unwrap(),
            handbuilt.join("famous"),
            "the hand-built layout keeps the famous/ level"
        );
        assert_eq!(
            corpus_board(md, "famous/acme/rev_a").unwrap(),
            handbuilt.join("famous/acme/rev_a")
        );
        assert_eq!(
            corpus_board_any(md, &rels).unwrap(),
            handbuilt.join("famous/acme/rev_a"),
            "the revision this tree pins"
        );

        std::env::set_var("HAUKSBEE_CORPUS_DIR", &fetched);
        assert_eq!(
            corpus_boards_root(md).unwrap(),
            fetched,
            "the fetch layout has no famous/ level"
        );
        assert_eq!(
            corpus_board(md, "famous/acme/rev_b").unwrap(),
            fetched.join("acme/rev_b"),
            "the famous/ prefix must be strippable"
        );
        assert_eq!(
            corpus_board_any(md, &rels).unwrap(),
            fetched.join("acme/rev_b"),
            "the revision this tree pins"
        );

        assert!(corpus_board_any(md, &["famous/acme/rev_z"]).is_none());
        std::env::remove_var("HAUKSBEE_CORPUS_DIR");
        let _ = std::fs::remove_dir_all(&base);
    }
}
