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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub mod avr_firmware;
pub mod circuits;

/// Print `SKIP: <why>` and return from the enclosing test when `cond` holds.
///
/// `skip_if!(cond => value, "why")` returns `value` instead of `()`, for
/// helpers that hand back an `Option`. Only for environment gaps a developer machine may legitimately have (a
/// missing emulator, an unbuilt firmware image). Missing corpus assets go
/// through [`corpus_or_skip`] so `HAUKSBEE_REQUIRE_CORPUS=1` can fail them.
#[macro_export]
macro_rules! skip_if {
    ($cond:expr, $($why:tt)+) => {
        if $cond {
            eprintln!("SKIP: {}", format_args!($($why)+));
            return;
        }
    };
    ($cond:expr => $ret:expr, $($why:tt)+) => {
        if $cond {
            eprintln!("SKIP: {}", format_args!($($why)+));
            return $ret;
        }
    };
}

/// Unwrap an `Option`, or print `SKIP: <why>` and return from the enclosing
/// function.
///
/// The `let ... else` counterpart of [`skip_if!`]; `=> value` returns
/// something other than `()`.
#[macro_export]
macro_rules! skip_none {
    ($opt:expr, $($why:tt)+) => {
        match $opt {
            Some(value) => value,
            None => {
                eprintln!("SKIP: {}", format_args!($($why)+));
                return;
            }
        }
    };
    ($opt:expr => $ret:expr, $($why:tt)+) => {
        match $opt {
            Some(value) => value,
            None => {
                eprintln!("SKIP: {}", format_args!($($why)+));
                return $ret;
            }
        }
    };
}

/// The repository root, derived from the calling crate's manifest directory.
///
/// `CARGO_MANIFEST_DIR` is set per-crate at compile time, so this takes it as
/// an argument rather than reading its own (which would point at this crate).
pub fn repo_root(manifest_dir: &str) -> PathBuf {
    // crates/<name>/ -> repository root
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from(manifest_dir), Path::to_path_buf)
}

/// `<repo>/testdata/<rel>`, derived from the calling crate's manifest directory.
#[must_use]
pub fn testdata(manifest_dir: &str, rel: &str) -> PathBuf {
    repo_root(manifest_dir).join("testdata").join(rel)
}

/// True when a missing asset should fail the run instead of skipping it.
#[must_use]
pub fn require_assets() -> bool {
    std::env::var_os("HAUKSBEE_REQUIRE_CORPUS").is_some()
}

/// A scratch path under the OS temp dir, unique to this test binary's process.
///
/// The path is `<tmp>/hauksbee_<prefix>_<tag>_<pid>`. Not created; callers that
/// need it to exist use [`fresh_scratch_dir`] or create it themselves (e.g.
/// when a helper like `extract_fresh` owns the create/remove lifecycle).
#[must_use]
pub fn scratch_path(prefix: &str, tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hauksbee_{prefix}_{tag}_{}", std::process::id()))
}

/// [`scratch_path`] with a per-call counter, freshly created.
///
/// The counter keeps tests that share a tag from deleting each other's files
/// when `cargo test` threads them in one process; pid-reuse leftovers go first.
///
/// # Panics
///
/// Panics if the scratch directory cannot be created.
#[must_use]
pub fn fresh_scratch_dir(prefix: &str, tag: &str) -> PathBuf {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = scratch_path(prefix, &format!("{tag}_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
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
#[must_use]
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
///
/// # Panics
///
/// Panics if `HAUKSBEE_CORPUS_DIR` is set to a path that is not a directory.
#[must_use]
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
#[must_use]
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
#[must_use]
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
#[must_use]
pub fn corpus_boards_root_or_skip(manifest_dir: &str, what: &str) -> Option<PathBuf> {
    corpus_boards_root(manifest_dir).or_else(|| {
        missing(
            what,
            "the board corpus root (either <corpus>/famous/ or <corpus>/)",
            "board corpus",
            &fetch_remedy(manifest_dir),
        )
    })
}

/// Every file under `root` (recursive) whose extension is one of `exts`, in
/// directory-stack visit order (LIFO, not sorted).
///
/// The shared shape behind the corpus-wide "walk every native CAD file and
/// check it" sweeps: a plain iterative walk with no directory pruning, no
/// `walkdir` dependency for the one thing these suites need. A caller that
/// must skip whole subtrees (e.g. `.git`, or a corpus.toml exclusion) still
/// writes its own walk; this is for the common case of "every match, full
/// depth".
#[must_use]
pub fn walk_files_with_ext(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if exts.contains(&p.extension().and_then(|x| x.to_str()).unwrap_or("")) {
                out.push(p);
            }
        }
    }
    out
}

/// `KiCad` board text for a minimal Arduino Nano + 10k pull resistor fixture.
///
/// Net 1 GND, net 2 +5V, net 3 BUS (A1 pad 4 = GND, pad 27 = +5V, pad 21 =
/// BUS; R1 across BUS/GND). Shared by the AVR and host-serial co-sim tests
/// that only need "an Arduino Nano on a net", not a specific board's real
/// layout.
pub const ARDUINO_NANO_GPIO_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "BUS")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4 thru_hole circle (at 0 4) (size 1 1) (net 1 "GND"))
    (pad 27 thru_hole circle (at 0 27) (size 1 1) (net 2 "+5V"))
    (pad 21 thru_hole circle (at 0 21) (size 1 1) (net 3 "BUS"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 3 "BUS"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

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
            let _ = write!(s, "\n           {}", d.display());
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
#[must_use]
pub fn corpus_board_any(manifest_dir: &str, rels: &[&str]) -> Option<PathBuf> {
    rels.iter().find_map(|rel| corpus_board(manifest_dir, rel))
}

/// A corpus path, or `None` with a visible note saying the test did not run.
///
/// Under `HAUKSBEE_REQUIRE_CORPUS=1` this panics instead, so a CI run cannot
/// pass by skipping. `what` names the test so the note says which one idled.
#[must_use]
pub fn corpus_or_skip(manifest_dir: &str, rel: &str, what: &str) -> Option<PathBuf> {
    corpus_board(manifest_dir, rel)
        .or_else(|| missing(what, rel, "board corpus", &fetch_remedy(manifest_dir)))
}

/// A private asset that is not redistributable, keyed off an env var so a
/// maintainer can point at their own checkout.
///
/// `env_var` names the override; `rel` is the path under it. Absent, this
/// behaves like [`corpus_or_skip`]: a loud note, or a panic under
/// `HAUKSBEE_REQUIRE_CORPUS=1`.
#[must_use]
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

/// A corpus asset that is deliberately absent from `corpus.toml`.
///
/// It is not ours to redistribute, with no fetchable, pinnable upstream (the manifest's
/// "Local-only boards" section). `HAUKSBEE_REQUIRE_CORPUS=1` promises that
/// everything the manifest can supply is present; it cannot conjure these, so
/// absence is always a loud skip, never a panic. In a maintainer's corpus that
/// carries the asset, the test runs as normal.
#[must_use]
pub fn local_only_asset(manifest_dir: &str, rel: &str, what: &str) -> Option<PathBuf> {
    let root = corpus_dir(manifest_dir)?;
    let p = root.join(rel);
    if p.is_file() {
        return Some(p);
    }
    eprintln!(
        "NOT RUN  {what}: {rel} is a local-only corpus asset (see corpus.toml) \
         and this corpus does not carry it"
    );
    None
}

/// The marker `scripts/fetch-corpus.sh` writes into a board directory that
/// `corpus.toml` declares `known_good = false`.
pub const NOT_KNOWN_GOOD_MARKER: &str = ".hauksbee-not-known-good";

/// True when this board is fetched for format or axis coverage but is NOT part of
/// the known-good set a silence gate may grade itself on.
///
/// Two kinds of entry carry the marker, and `corpus.toml` says which per board:
/// a reference design that was never a manufactured product (`KiCad`'s own demos),
/// and a shipped board on which a gate fires and the finding has not yet been
/// adjudicated. Neither may be counted as "a known-good board this check stayed
/// quiet on", because the first was never known-good and the second is exactly
/// the question under investigation.
///
/// The marker is looked for at the board's own directory and above it up to the
/// corpus root, because a board's design files sit at whatever depth its upstream
/// chose and the marker is written once per entry.
#[must_use]
pub fn not_known_good(board: &Path, corpus_root: &Path) -> Option<String> {
    let mut dir = if board.is_dir() {
        Some(board)
    } else {
        board.parent()
    };
    while let Some(d) = dir {
        let marker = d.join(NOT_KNOWN_GOOD_MARKER);
        if marker.is_file() {
            return Some(
                std::fs::read_to_string(&marker)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            );
        }
        if d == corpus_root {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Print what a gate declined to grade itself on, and why.
///
/// Every exclusion is announced. A gate that quietly narrowed its own input set
/// is the same failure as one that scanned nothing: the pass says more than the
/// run earned. Pair this with [`scanned`], which reports what it did cover.
///
/// The reason is printed in full once and abbreviated after that. One entry can
/// hold twenty layouts, and twenty copies of the same paragraph buries the counts
/// a reader came for.
pub fn excluded(gate: &str, board: &str, why: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .map(|mut s| s.insert(why.to_string()))
        .unwrap_or(true);
    if first {
        eprintln!("NOT KNOWN-GOOD  {gate}: {board} excluded. {why}");
    } else {
        eprintln!("NOT KNOWN-GOOD  {gate}: {board} excluded (same reason as above)");
    }
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
///
/// # Panics
///
/// Panics if `n` is zero: a gate that scanned no boards has proved nothing.
pub fn scanned(gate: &str, n: usize) {
    eprintln!("SCANNED  {gate}: {n} board(s)");
    assert!(
        n > 0,
        "{gate} scanned 0 boards. The corpus root resolved but no board in the \
         list was found or loadable, so this gate proves nothing. Check the \
         corpus layout (scripts/fetch-corpus.sh) before trusting a pass."
    );
}

/// Days since 1970-01-01 for a `YYYY-MM-DD` string (proleptic Gregorian,
/// Hinnant's algorithm), or `None` if the string does not parse.
///
/// Shared by every corpus gate that records a dated exception on the same
/// terms `hauksbee-waivers.toml` does: a reason and an expiry, checked
/// against [`today_days`] so a stale exception goes red instead of
/// suppressing forever. A dev-dependency for this would be heavier than the
/// dozen lines, and this is the only date arithmetic any of these suites need.
#[must_use]
pub fn until_days(until: &str) -> Option<i64> {
    let mut it = until.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Today, in days since the epoch. Callers capture this once per run so a
/// long sweep cannot have an exception lapse partway through it.
#[must_use]
pub fn today_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .map_or(0, |secs| secs.div_euclid(86_400))
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
