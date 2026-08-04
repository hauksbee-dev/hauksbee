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

/// The name of the corpus directory, as `scripts/fetch-corpus.sh` writes it and
/// `corpus.toml` declares it in `meta.default_dir`.
const CORPUS_DIR_NAME: &str = "board-corpus";

/// How far up the tree to look for the corpus before giving up.
///
/// Deep enough for a git worktree nested inside the checkout
/// (`<checkout>/.claude/worktrees/<name>` is three levels down), shallow enough
/// that the walk cannot wander out of the user's project tree and adopt some
/// unrelated `board-corpus/` from a home directory.
const CORPUS_SEARCH_DEPTH: usize = 6;

/// The main worktree of the checkout `manifest_dir` belongs to, if this is a
/// linked worktree and the main one can be identified.
///
/// A linked worktree's `.git` is a FILE reading `gitdir: <common>/worktrees/<n>`,
/// where `<common>` is the main checkout's `.git` directory. The main worktree is
/// that directory's parent. Parsed rather than shelled out to, so this stays
/// usable from a test that must not spawn processes.
fn main_worktree(root: &Path) -> Option<PathBuf> {
    let dotgit = root.join(".git");
    if !dotgit.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir = Path::new(text.strip_prefix("gitdir:")?.trim());
    // <common>/worktrees/<name> -> <common> -> the checkout that owns it.
    let worktrees = gitdir.parent()?;
    (worktrees.file_name()? == "worktrees")
        .then(|| worktrees.parent().and_then(Path::parent))
        .flatten()
        .map(Path::to_path_buf)
}

/// Every directory the corpus is looked for in, nearest first.
///
/// Two starting points, because both are load-bearing. The checkout the test was
/// compiled from covers the ordinary case and, by walking up, a worktree nested
/// inside the checkout. The main worktree covers a worktree created OUTSIDE it,
/// which no amount of walking up would ever reach.
///
/// The walk exists because `corpus_dir` used to check exactly two directories,
/// the repository root and its parent. Most work on this project happens in a git
/// worktree under `<checkout>/.claude/worktrees/<name>`, where neither of those
/// two is the checkout, so no corpus was found, every corpus test skipped, and
/// the skip read as a pass. An agent's whole body of corpus evidence turned out
/// never to have run.
pub fn corpus_search_dirs(manifest_dir: &str) -> Vec<PathBuf> {
    let root = repo_root(manifest_dir);
    let mut out: Vec<PathBuf> = Vec::new();
    for start in [Some(root.clone()), main_worktree(&root)]
        .into_iter()
        .flatten()
    {
        for ancestor in start
            .ancestors()
            .take(CORPUS_SEARCH_DEPTH)
            // `/board-corpus` is nobody's corpus, and adopting it would be worse
            // than finding none.
            .filter(|a| a.parent().is_some())
        {
            let candidate = ancestor.join(CORPUS_DIR_NAME);
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// The board corpus, if this machine has one.
///
/// `HAUKSBEE_CORPUS_DIR` wins; otherwise the nearest `board-corpus/` at or above
/// the checkout, which is where `scripts/fetch-corpus.sh` puts it by default.
/// See [`corpus_search_dirs`] for the search order and why it is a walk.
pub fn corpus_dir(manifest_dir: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HAUKSBEE_CORPUS_DIR") {
        let p = PathBuf::from(dir);
        // A set-but-wrong override is a mistake, never a reason to skip. Returning
        // `None` here sent a typo'd path down the same road as "this machine has
        // no corpus", and the suite went quiet on a corpus the operator believed
        // they had pointed it at.
        assert!(
            p.is_dir(),
            "HAUKSBEE_CORPUS_DIR is set to {} which is not a directory. Fix the \
             path or unset it; a corpus override that resolves to nothing would \
             silently skip every corpus gate.",
            p.display()
        );
        return Some(p);
    }
    corpus_search_dirs(manifest_dir)
        .into_iter()
        .find(|c| c.is_dir())
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

/// [`corpus_boards_root`], or `None` with a visible note; a panic under
/// `HAUKSBEE_REQUIRE_CORPUS=1`.
///
/// The plain resolver is fine when the caller has its own skip note. Suites that
/// were written as `corpus_dir(..).unwrap_or_default().join("famous")` had no
/// note to speak of: `unwrap_or_default()` turns an absent corpus into a
/// relative path that also does not exist, so absence and wrong-layout looked
/// identical and both read as "skip". `what` names the suite so the note says
/// which one idled.
pub fn corpus_boards_root_or_skip(manifest_dir: &str, what: &str) -> Option<PathBuf> {
    match corpus_boards_root(manifest_dir) {
        Some(p) => Some(p),
        None => missing(
            what,
            "the board corpus root (either <corpus>/famous/ or <corpus>/)",
            "board corpus",
            &fetch_remedy(manifest_dir),
        ),
    }
}

/// What to tell someone whose corpus did not resolve, naming every directory
/// that was looked in.
///
/// An absent corpus has to say where it looked. The failure that motivated the
/// search walk was invisible precisely because the note said only "not found",
/// so a worktree user read it as "I have no corpus" rather than "the resolver
/// cannot see the corpus I do have".
fn fetch_remedy(manifest_dir: &str) -> String {
    let mut s = String::from("scripts/fetch-corpus.sh, or set HAUKSBEE_CORPUS_DIR");
    if std::env::var_os("HAUKSBEE_CORPUS_DIR").is_none() {
        s.push_str(". Looked in:");
        for d in corpus_search_dirs(manifest_dir) {
            s.push_str(&format!("\n           {}", d.display()));
        }
    }
    s
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
        None => missing(what, rel, "board corpus", &fetch_remedy(manifest_dir)),
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

/// Record how many boards a corpus gate actually scanned, and fail on zero.
///
/// Call this in every corpus gate that got as far as having a corpus root. The
/// count goes to stderr so a passing run says what it covered rather than
/// leaving "ok" to imply it, and zero is a failure whether or not
/// `HAUKSBEE_REQUIRE_CORPUS` is set: a gate that examined nothing has proved
/// nothing, and reporting that as a pass is precisely the vacuous green this
/// product exists to refuse. This is not the same guard as [`corpus_or_skip`],
/// which catches an absent corpus; this catches a *present* corpus whose layout
/// or contents mean no board was ever opened.
pub fn scanned(gate: &str, n: usize) {
    eprintln!("SCANNED  {gate}: {n} board(s)");
    assert!(
        n > 0,
        "{gate} scanned 0 boards. The corpus root resolved but no board in the \
         list was found or loadable, so this gate proves nothing. Check the \
         corpus layout (scripts/fetch-corpus.sh) before trusting a pass."
    );
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

    /// The failure this crate exists to prevent, reproduced against the
    /// resolver's own search list rather than the filesystem: a test compiled
    /// from a worktree under `<checkout>/.claude/worktrees/<name>` must still
    /// find the corpus that sits beside the checkout.
    ///
    /// `corpus_search_dirs` is checked instead of `corpus_dir` because the latter
    /// reads a process-wide env var and touches the real disk, and because the
    /// bug was never about whether a directory existed: it was about the search
    /// never reaching the directory that did.
    #[test]
    fn the_search_reaches_the_checkout_from_a_nested_worktree() {
        // repo_root() strips two levels, so hand it a plausible crate dir.
        let worktree = "/w/proj/.claude/worktrees/agent-1";
        let dirs = corpus_search_dirs(&format!("{worktree}/crates/hauksbee-extract"));
        let has = |p: &str| dirs.iter().any(|d| d == Path::new(p));
        assert!(has(&format!("{worktree}/board-corpus")), "{dirs:?}");
        assert!(
            has("/w/proj/board-corpus"),
            "the checkout the worktree belongs to: {dirs:?}"
        );
        assert!(
            has("/w/board-corpus"),
            "the hand-built corpus sits beside the checkout: {dirs:?}"
        );
        assert!(
            !has("/board-corpus"),
            "the walk must not reach the filesystem root: {dirs:?}"
        );
    }

    /// The two-directory search that shipped before, stated as the thing that
    /// must never be true again.
    #[test]
    fn the_search_is_a_walk_and_not_two_directories() {
        let dirs = corpus_search_dirs("/home/dev/work/proj/crates/hauksbee-extract");
        assert!(
            dirs.len() > 2,
            "corpus_dir checked only the repo root and its parent, which is why \
             every corpus gate skipped from a worktree: {dirs:?}"
        );
    }

    /// A linked worktree created OUTSIDE the checkout is reachable only through
    /// git's own bookkeeping, so the `.git` file is parsed for it.
    #[test]
    fn a_worktree_outside_the_checkout_resolves_through_git() {
        let base = std::env::temp_dir().join("hauksbee_testkit_worktree");
        let _ = std::fs::remove_dir_all(&base);
        let checkout = base.join("proj");
        let worktree = base.join("elsewhere/wt");
        std::fs::create_dir_all(checkout.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}/.git/worktrees/wt\n", checkout.display()),
        )
        .unwrap();

        assert_eq!(
            main_worktree(&worktree).as_deref(),
            Some(checkout.as_path())
        );
        let dirs = corpus_search_dirs(&format!("{}/crates/x", worktree.display()));
        assert!(
            dirs.contains(&checkout.join("board-corpus")),
            "no ancestor of {} is the checkout, so only git can find it: {dirs:?}",
            worktree.display()
        );

        // An ordinary checkout has a `.git` DIRECTORY and no main worktree to
        // redirect to, which must not be mistaken for a parse failure worth
        // reporting.
        assert!(main_worktree(&checkout).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
