//! `hauksbee watch <target>`: re-run the right check on every file change.
//!
//! The pitch is "CI for hardware on every change", so this closes the local
//! loop: point it at a board, a Board-as-Code `.board`, or a `hauksbee-ci` spec
//! and it re-runs the appropriate check whenever a file it depends on changes.
//!
//! ## What re-runs
//!
//! | target kind            | detected by            | re-runs                              |
//! |------------------------|------------------------|--------------------------------------|
//! | board layout / netlist | `.kicad_pcb`/`.kicad_sch`/`.brd`/`.d356`/`.net`/gerber | `hauksbee run <board> --check --strict` |
//! | Board-as-Code          | `.board` (or DSL header) | `hauksbee check-code <file>`       |
//! | hauksbee-ci spec       | `.toml`                | `hauksbee-ci run <spec>`             |
//!
//! Each run is a **subprocess** re-invocation of the real CLI (this binary for
//! boards / `.board`, the sibling `hauksbee-ci` for specs), not an in-process
//! call. That is deliberate: the engine cannot depend on `hauksbee-ci` (it would
//! be a dependency cycle), so a spec re-run must shell out regardless; making all
//! three kinds uniform also means a panicking or `process::exit`-ing run cannot
//! take the long-lived watcher down with it, and the child's exit code is the
//! authoritative verdict with nothing to re-derive.
//!
//! ## Exit-code passthrough
//!
//! In stream mode the process stays alive across runs, so "passthrough" means: on
//! Ctrl-C the watcher exits with the **last completed run's** exit code, so a
//! wrapping script sees the most recent verdict. `--once` runs a single check and
//! exits directly with that run's code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for the filesystem to go quiet after a change before firing a
/// run. Editors save a file as several syscalls (truncate, write, rename), which
/// fire a burst of events within a few milliseconds; this settle window coalesces
/// that burst so one save triggers exactly one run. Long enough to swallow an
/// editor's write burst, short enough to feel instant.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// The last completed run's exit code, so the Ctrl-C handler can exit with it
/// (exit-code passthrough). Seeded to 0; updated after every completed run.
static LAST_EXIT: AtomicI32 = AtomicI32::new(0);

/// What kind of thing we are watching, which decides the command we re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A board layout or netlist → the full static check suite (`run --check`).
    Board,
    /// A `.kicad_sch` schematic → same suite, but sub-sheets are siblings.
    Schematic,
    /// A Board-as-Code `.board` DSL file → `check-code`.
    BoardCode,
    /// A `hauksbee-ci` spec TOML → the spec through `hauksbee-ci`.
    Spec,
}

/// Board layout / netlist extensions that route to the static check suite.
/// Gerber layers (`.gbr` and the classic per-layer suffixes) and Excellon drill
/// files count too, since the extractor sniffs them.
const BOARD_EXTS: &[&str] = &[
    "kicad_pcb", "brd", "d356", "net", "pcbdoc", // layouts / netlists
    "gbr", "gtl", "gbl", "gto", "gbo", "gts", "gbs", "gko", "gm1", "drl", "xln", // gerbers / drill
];

impl Target {
    /// Detect the target kind from the path (extension first, then the
    /// Board-as-Code header sniff the rest of the CLI uses). Returns a loud,
    /// actionable error listing the accepted targets when nothing matches, so an
    /// unrecognized target refuses rather than silently doing the wrong check.
    pub fn detect(path: &Path) -> anyhow::Result<Target> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("kicad_sch") => return Ok(Target::Schematic),
            Some("board") => return Ok(Target::BoardCode),
            Some("toml") => return Ok(Target::Spec),
            Some(e) if BOARD_EXTS.contains(&e) => return Ok(Target::Board),
            _ => {}
        }
        // No decisive extension: a `.board` saved under another name is still
        // recognized by its DSL header (mirrors `run`'s content sniff).
        if let Ok(text) = std::fs::read_to_string(path) {
            if crate::commands::common::is_board_code_header(&text) {
                return Ok(Target::BoardCode);
            }
        }
        anyhow::bail!("{}", accepted_targets_msg(path));
    }
}

/// The refusal message for an unrecognized `watch` target: name what was given
/// and enumerate exactly what `watch` accepts.
fn accepted_targets_msg(path: &Path) -> String {
    format!(
        "don't know how to watch '{}'. `hauksbee watch` accepts:\n  \
         - a board layout or netlist: .kicad_pcb, .kicad_sch, .brd, .d356, .net, gerbers (runs the static checks)\n  \
         - a Board-as-Code file: .board (runs check-code)\n  \
         - a hauksbee-ci spec: .toml (runs the spec through hauksbee-ci)",
        path.display()
    )
}

/// A run's verdict, derived from the child's exit code. `Fail` is "the check ran
/// and found a problem"; `Error` is "the check could not run" (bad input, a spec
/// error); `Invalid` is a spec whose analog co-sim did not converge (05 §3b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Invalid,
    Error,
}

impl Verdict {
    /// Map a target kind + child exit code to a verdict. The exit-code contracts
    /// come from each subcommand's own docs (`run --strict` gates on 2;
    /// `hauksbee-ci` uses 0/1/2/3; `check-code` exits 1 when unhealthy).
    pub fn classify(target: Target, code: i32) -> Verdict {
        match target {
            Target::Board | Target::Schematic => match code {
                0 => Verdict::Pass,
                2 => Verdict::Fail,
                _ => Verdict::Error,
            },
            Target::BoardCode => match code {
                0 => Verdict::Pass,
                _ => Verdict::Fail,
            },
            Target::Spec => match code {
                0 => Verdict::Pass,
                1 => Verdict::Fail,
                3 => Verdict::Invalid,
                _ => Verdict::Error, // 2 = spec/usage error
            },
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Invalid => "INVALID",
            Verdict::Error => "ERROR",
        }
    }
}

/// Build the subprocess command for one run of `target` on `path`.
///
/// Boards / `.board` re-invoke THIS executable (`current_exe`); a spec re-invokes
/// the sibling `hauksbee-ci`. `--plain` streams plain-language reports for the
/// board suite (the other kinds have no plain flag).
fn build_command(target: Target, path: &Path, plain: bool) -> anyhow::Result<Command> {
    let self_exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot locate the hauksbee executable to re-run: {e}"))?;
    let mut cmd = match target {
        Target::Board | Target::Schematic => {
            let mut c = Command::new(&self_exe);
            c.arg("run").arg(path).arg("--check").arg("--strict");
            if plain {
                c.arg("--plain");
            }
            c
        }
        Target::BoardCode => {
            let mut c = Command::new(&self_exe);
            c.arg("check-code").arg(path);
            c
        }
        Target::Spec => {
            let mut c = Command::new(ci_binary(&self_exe));
            c.arg("run").arg(path);
            c
        }
    };
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    Ok(cmd)
}

/// Locate the `hauksbee-ci` binary: prefer a sibling of this executable (the two
/// are installed together), else fall back to a bare name resolved on `PATH`.
fn ci_binary(self_exe: &Path) -> PathBuf {
    let name = format!("hauksbee-ci{}", std::env::consts::EXE_SUFFIX);
    if let Some(dir) = self_exe.parent() {
        let sibling = dir.join(&name);
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from(name)
}

/// The set of files whose change re-runs the check, plus the directories to hand
/// to notify. We watch whole directories (non-recursive) and filter events down
/// to `files`, which is notify's recommended pattern and means an unrelated file
/// changing in the same directory never triggers a run.
#[derive(Debug, Clone)]
pub struct WatchSet {
    /// Normalized (canonical-parent + filename) paths that trigger a re-run.
    files: BTreeSet<PathBuf>,
    /// Unique existing parent directories to watch.
    dirs: Vec<PathBuf>,
    /// Human-readable list of what is watched, for the startup banner.
    display: Vec<PathBuf>,
}

impl WatchSet {
    /// Derive the dependency set for `target` at `path`.
    ///
    /// - **Board**: the board file + its sibling `.kicad_pro` (DRC reads netclass
    ///   clearances from there).
    /// - **Schematic**: every `*.kicad_sch` in the directory (KiCad keeps a
    ///   sheet hierarchy's sub-sheets beside the root sheet) + the `.kicad_pro`.
    /// - **BoardCode**: just the `.board` file.
    /// - **Spec**: the spec TOML + the board and firmware it names + any sensor
    ///   `spec_file`s, resolved relative to the spec's directory (a minimal TOML
    ///   extract of just those path fields — the engine cannot reach hauksbee-ci's
    ///   loader without a dependency cycle). A spec that fails to parse degrades
    ///   to watching the spec file alone; the run itself then surfaces the error.
    pub fn derive(target: Target, path: &Path) -> WatchSet {
        let mut raw: Vec<PathBuf> = vec![path.to_path_buf()];
        match target {
            Target::Board => {
                raw.push(path.with_extension("kicad_pro"));
            }
            Target::Schematic => {
                raw.push(path.with_extension("kicad_pro"));
                if let Some(dir) = path.parent() {
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        for e in rd.flatten() {
                            let p = e.path();
                            if p.extension().and_then(|x| x.to_str()) == Some("kicad_sch") {
                                raw.push(p);
                            }
                        }
                    }
                }
            }
            Target::BoardCode => {}
            Target::Spec => raw.extend(spec_referenced_paths(path)),
        }
        Self::from_paths(raw)
    }

    fn from_paths(raw: Vec<PathBuf>) -> WatchSet {
        let mut files = BTreeSet::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut display: Vec<PathBuf> = Vec::new();
        for p in raw {
            if let Some(norm) = normalize(&p) {
                if files.insert(norm.clone()) {
                    display.push(p.clone());
                    if let Some(dir) = norm.parent() {
                        let dir = dir.to_path_buf();
                        if !dirs.contains(&dir) {
                            dirs.push(dir);
                        }
                    }
                }
            }
        }
        WatchSet { files, dirs, display }
    }

    /// Does a filesystem event on `path` fall inside the watched set? Compared on
    /// the normalized (canonical-parent + filename) form so an editor that
    /// replaces a file (new inode) still matches.
    pub fn matches(&self, path: &Path) -> bool {
        normalize(path)
            .map(|n| self.files.contains(&n))
            .unwrap_or(false)
    }

    /// The directories to register with notify.
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}

/// Normalize a path to `canonical(parent)/filename`. Canonicalizing the PARENT
/// (not the file) means a file that was just deleted and recreated by an editor
/// still normalizes to a stable key. Returns `None` if the parent does not exist.
fn normalize(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?;
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let canon = std::fs::canonicalize(parent).ok()?;
    Some(canon.join(name))
}

/// Minimal extraction of the file paths a `hauksbee-ci` spec references (`board`,
/// `firmware`, and each sensor `spec_file`), resolved relative to the spec's
/// directory the same way `hauksbee_ci::Spec` resolves them. Not a full parse:
/// only the path fields matter for the watch set, and pulling in the whole spec
/// type would require depending on hauksbee-ci (a cycle).
fn spec_referenced_paths(spec_path: &Path) -> Vec<PathBuf> {
    #[derive(serde::Deserialize)]
    struct SpecRefs {
        board: Option<PathBuf>,
        firmware: Option<PathBuf>,
        #[serde(default, rename = "sensor")]
        sensors: Vec<SensorRef>,
    }
    #[derive(serde::Deserialize)]
    struct SensorRef {
        spec_file: Option<String>,
    }
    let base = spec_path.parent().unwrap_or_else(|| Path::new("."));
    let resolve = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        }
    };
    let Ok(text) = std::fs::read_to_string(spec_path) else {
        return Vec::new();
    };
    let Ok(refs) = toml::from_str::<SpecRefs>(&text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(b) = &refs.board {
        out.push(resolve(b));
    }
    if let Some(f) = &refs.firmware {
        out.push(resolve(f));
    }
    for s in &refs.sensors {
        if let Some(sf) = &s.spec_file {
            out.push(resolve(Path::new(sf)));
        }
    }
    out
}

/// Entry point from the binary: detect, derive the watch set, then run the loop.
pub fn run(target_path: PathBuf, plain: bool, once: bool) -> anyhow::Result<()> {
    let target = Target::detect(&target_path)?; // refuses here for unknown targets
    let watch_set = WatchSet::derive(target, &target_path);

    if once {
        // Plumbing check: one run, exit directly with its code (passthrough).
        let outcome = execute(target, &target_path, plain, 1, "startup");
        std::process::exit(outcome.code);
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(watch_loop(target, target_path, watch_set, plain))
}

/// One completed run's result. The verdict is printed inside [`execute`]; the
/// loop only needs the exit code (for Ctrl-C / `--once` passthrough).
struct Outcome {
    code: i32,
}

/// The live loop: run once immediately, then re-run on every settled change until
/// Ctrl-C. Uses tokio only for a portable Ctrl-C (already a dependency); the file
/// events come from notify on a std channel bridged into an async receiver.
async fn watch_loop(
    target: Target,
    path: PathBuf,
    watch_set: WatchSet,
    plain: bool,
) -> anyhow::Result<()> {
    use notify::{RecursiveMode, Watcher};

    print_banner(target, &path, &watch_set);

    // notify -> std mpsc -> tokio unbounded, so the async loop can await events.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(raw_tx)
        .map_err(|e| anyhow::anyhow!("cannot start the file watcher: {e}"))?;
    let mut watched_any = false;
    for dir in watch_set.dirs() {
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => watched_any = true,
            Err(e) => eprintln!("note: cannot watch {}: {e}", dir.display()),
        }
    }
    if !watched_any {
        anyhow::bail!(
            "nothing to watch: none of the target's directories could be registered \
             (does the file exist?)"
        );
    }

    // Bridge the blocking std receiver onto an async channel on a blocking thread.
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    std::thread::spawn(move || {
        while let Ok(res) = raw_rx.recv() {
            if let Ok(ev) = res {
                if ev_tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    // One-shot Ctrl-C notification.
    let (sig_tx, mut sig_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = sig_tx.send(()).await;
        }
    });

    // Run #1 immediately, for instant feedback and a defined last-verdict.
    let mut run_number = 1u64;
    let first = execute(target, &path, plain, run_number, "startup");
    LAST_EXIT.store(first.code, Ordering::SeqCst);

    loop {
        // 1. Wait for the first relevant change (or Ctrl-C).
        let changed = loop {
            tokio::select! {
                _ = sig_rx.recv() => exit_with_last(),
                ev = ev_rx.recv() => {
                    let Some(ev) = ev else { exit_with_last(); };
                    if let Some(p) = relevant_path(&ev, &watch_set) {
                        break p;
                    }
                }
            }
        };

        // 2. Debounce: absorb the editor's write burst. Break once DEBOUNCE has
        //    elapsed since the last RELEVANT event, so a noisy unrelated file
        //    cannot hold the window open forever.
        let mut last_relevant = Instant::now();
        loop {
            let elapsed = last_relevant.elapsed();
            if elapsed >= DEBOUNCE {
                break;
            }
            let wait = DEBOUNCE - elapsed;
            tokio::select! {
                _ = sig_rx.recv() => exit_with_last(),
                _ = tokio::time::sleep(wait) => break,
                ev = ev_rx.recv() => {
                    let Some(ev) = ev else { exit_with_last(); };
                    if relevant_path(&ev, &watch_set).is_some() {
                        last_relevant = Instant::now();
                    }
                }
            }
        }

        // 3. Run. A change that arrives DURING the run is buffered in the channel
        //    and coalesced afterward into exactly one queued re-run (no pile-up).
        run_number += 1;
        let what = display_name(&changed);
        let outcome = execute(target, &path, plain, run_number, &what);
        LAST_EXIT.store(outcome.code, Ordering::SeqCst);

        // Drain events that occurred during the run so a burst mid-run collapses
        // to a single follow-up (the loop above will fire once), not one per event.
        while ev_rx.try_recv().is_ok() {}
    }
}

/// Exit with the last completed run's code (Ctrl-C passthrough). Prints a newline
/// so the shell prompt starts clean after the `^C`.
fn exit_with_last() -> ! {
    println!();
    std::process::exit(LAST_EXIT.load(Ordering::SeqCst));
}

/// If an event touches a watched file, return that path; else `None`.
fn relevant_path(ev: &notify::Event, set: &WatchSet) -> Option<PathBuf> {
    ev.paths.iter().find(|p| set.matches(p)).cloned()
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| p.display().to_string())
}

/// Run one check as a subprocess, stream its output under a separator, print a
/// one-line verdict, and return the outcome.
fn execute(target: Target, path: &Path, plain: bool, run_number: u64, changed: &str) -> Outcome {
    print_separator(run_number, changed);
    let started = Instant::now();
    let mut cmd = match build_command(target, path, plain) {
        Ok(c) => c,
        Err(e) => {
            println!("  ERROR  could not build the check command: {e}");
            return Outcome { code: 1 };
        }
    };
    let output = cmd.output();
    let elapsed = started.elapsed();
    match output {
        Ok(out) => {
            // Echo the child's streams so the real report is visible live.
            use std::io::Write;
            let _ = std::io::stdout().write_all(&out.stdout);
            let _ = std::io::stderr().write_all(&out.stderr);
            let code = out.status.code().unwrap_or(-1);
            let verdict = Verdict::classify(target, code);
            let summary = summarize(target, &out.stdout);
            print_verdict(verdict, code, elapsed, summary.as_deref());
            Outcome { code }
        }
        Err(e) => {
            println!(
                "  ERROR  could not launch the check ({e}). Is the hauksbee{} binary on PATH?",
                if matches!(target, Target::Spec) { "-ci" } else { "" }
            );
            Outcome { code: 1 }
        }
    }
}

/// Pull a compact finding/assertion count out of the child's stdout when it has a
/// recognizable summary line (hauksbee-ci prints "N/M assertions passed"). Best
/// effort: the full report is already streamed above, so `None` just omits the
/// suffix rather than fabricating a count.
fn summarize(target: Target, stdout: &[u8]) -> Option<String> {
    if !matches!(target, Target::Spec) {
        return None;
    }
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains("assertions passed"))?;
    // e.g. "3/4 assertions passed in 0.20s - RED" -> "3/4 assertions passed"
    let idx = line.find("assertions passed")? + "assertions passed".len();
    Some(line[..idx].trim().to_string())
}

fn print_banner(target: Target, path: &Path, set: &WatchSet) {
    let kind = match target {
        Target::Board => "board (static check suite)",
        Target::Schematic => "schematic (static check suite)",
        Target::BoardCode => "Board-as-Code (check-code)",
        Target::Spec => "hauksbee-ci spec",
    };
    println!("hauksbee watch: {} — {}", path.display(), kind);
    println!("watching {} file(s) for changes (Ctrl-C to stop):", set.display.len());
    for p in &set.display {
        println!("  - {}", p.display());
    }
}

fn rule() -> String {
    "─".repeat(72)
}

fn print_separator(run_number: u64, changed: &str) {
    println!("\n{}", rule());
    println!(" run #{run_number}  {}  changed: {changed}", clock());
    println!("{}", rule());
}

fn print_verdict(verdict: Verdict, code: i32, elapsed: Duration, summary: Option<&str>) {
    let suffix = summary.map(|s| format!("  ·  {s}")).unwrap_or_default();
    println!(
        "  {}  (exit {code})  in {:.2}s{suffix}",
        verdict.tag(),
        elapsed.as_secs_f64()
    );
}

/// A dependency-free UTC HH:MM:SS clock for the run separator. Wall-clock time is
/// only cosmetic here (ordering the runs), so UTC without a timezone crate is fine.
fn clock() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_board_layouts() {
        assert_eq!(Target::detect(Path::new("b.kicad_pcb")).unwrap(), Target::Board);
        assert_eq!(Target::detect(Path::new("b.brd")).unwrap(), Target::Board);
        assert_eq!(Target::detect(Path::new("b.net")).unwrap(), Target::Board);
        assert_eq!(Target::detect(Path::new("top.gtl")).unwrap(), Target::Board);
        // Case-insensitive extension.
        assert_eq!(Target::detect(Path::new("B.KICAD_PCB")).unwrap(), Target::Board);
    }

    #[test]
    fn detects_schematic_boardcode_and_spec() {
        assert_eq!(Target::detect(Path::new("b.kicad_sch")).unwrap(), Target::Schematic);
        assert_eq!(Target::detect(Path::new("b.board")).unwrap(), Target::BoardCode);
        assert_eq!(Target::detect(Path::new("ci.toml")).unwrap(), Target::Spec);
    }

    #[test]
    fn refuses_unknown_target_with_accepted_list() {
        let err = Target::detect(Path::new("notes.txt")).unwrap_err().to_string();
        assert!(err.contains("don't know how to watch"), "{err}");
        assert!(err.contains(".kicad_pcb"), "{err}");
        assert!(err.contains(".board"), "{err}");
        assert!(err.contains(".toml"), "{err}");
    }

    #[test]
    fn verdict_classification_per_target() {
        // Board suite: run --check --strict gates on exit 2.
        assert_eq!(Verdict::classify(Target::Board, 0), Verdict::Pass);
        assert_eq!(Verdict::classify(Target::Board, 2), Verdict::Fail);
        assert_eq!(Verdict::classify(Target::Board, 1), Verdict::Error);
        // check-code: nonzero is a failed check.
        assert_eq!(Verdict::classify(Target::BoardCode, 0), Verdict::Pass);
        assert_eq!(Verdict::classify(Target::BoardCode, 1), Verdict::Fail);
        // hauksbee-ci: 0 green / 1 red / 2 spec-error / 3 invalid-for-analysis.
        assert_eq!(Verdict::classify(Target::Spec, 0), Verdict::Pass);
        assert_eq!(Verdict::classify(Target::Spec, 1), Verdict::Fail);
        assert_eq!(Verdict::classify(Target::Spec, 2), Verdict::Error);
        assert_eq!(Verdict::classify(Target::Spec, 3), Verdict::Invalid);
    }

    #[test]
    fn watch_set_matches_only_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("b.kicad_pcb");
        std::fs::write(&board, "x").unwrap();
        let unrelated = dir.path().join("README.md");
        std::fs::write(&unrelated, "y").unwrap();

        let set = WatchSet::derive(Target::Board, &board);
        assert!(set.matches(&board), "the board file must match");
        assert!(!set.matches(&unrelated), "an unrelated sibling must not match");
        // The .kicad_pro companion is in the set even before it exists on disk
        // (its parent dir does), so creating it later triggers a run.
        let pro = dir.path().join("b.kicad_pro");
        assert!(set.matches(&pro), "the .kicad_pro companion must match");
        // One directory to watch.
        assert_eq!(set.dirs().len(), 1);
    }

    #[test]
    fn spec_watch_set_includes_board_and_firmware() {
        let dir = tempfile::tempdir().unwrap();
        let hw = dir.path().join("hardware");
        let fw = dir.path().join("firmware");
        std::fs::create_dir_all(&hw).unwrap();
        std::fs::create_dir_all(&fw).unwrap();
        let board = hw.join("board.kicad_pcb");
        let firmware = fw.join("app.elf");
        std::fs::write(&board, "b").unwrap();
        std::fs::write(&firmware, "f").unwrap();
        let spec = dir.path().join("ci.toml");
        std::fs::write(
            &spec,
            "name = \"x\"\nboard = \"hardware/board.kicad_pcb\"\n\
             firmware = \"firmware/app.elf\"\n[[assert]]\nkind = \"no_faults\"\n",
        )
        .unwrap();

        let set = WatchSet::derive(Target::Spec, &spec);
        assert!(set.matches(&spec), "the spec file itself");
        assert!(set.matches(&board), "the referenced board");
        assert!(set.matches(&firmware), "the referenced firmware");
        // Board and firmware live in different dirs, so both are watched.
        assert!(set.dirs().len() >= 2, "distinct dirs watched: {:?}", set.dirs());
    }

    #[test]
    fn malformed_spec_watch_set_falls_back_to_spec_only() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("broken.toml");
        std::fs::write(&spec, "this is = not [ valid toml").unwrap();
        let set = WatchSet::derive(Target::Spec, &spec);
        assert!(set.matches(&spec));
        assert_eq!(set.dirs().len(), 1);
    }

    #[test]
    fn debounce_is_a_short_settle_window() {
        // Guardrail: the settle window stays in the documented 200-300ms band.
        assert!(DEBOUNCE >= Duration::from_millis(200));
        assert!(DEBOUNCE <= Duration::from_millis(300));
    }

    #[test]
    fn boardcode_detected_by_header_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mystery");
        std::fs::write(&f, "// Board-as-Code v1\nboard version 1\n").unwrap();
        assert_eq!(Target::detect(&f).unwrap(), Target::BoardCode);
    }
}
