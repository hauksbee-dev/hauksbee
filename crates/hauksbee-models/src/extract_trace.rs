//! Kept traces of each extraction agent run: the prompt it was given, its
//! command line and its event stream.
//!
//! The agent CLIs run with `--ephemeral` / `--no-session-persistence`, so they
//! keep no transcript of their own, and without one a bad card cannot be
//! traced to a step (did the drafter never open the pin table, or misread
//! it?). So the harness keeps its own copy of what it already holds: per run a
//! directory `trace/<part>-<UTC stamp>/` beside the drafted card (or under
//! `~/.hauksbee/extract-traces/` when the card goes to the model directory),
//! with `call-N.argv`, `call-N.prompt.md` and `call-N.events.jsonl` per agent
//! call, the layout `rollout-summary.py` reads. Only the last
//! [`KEEP_RUNS`] runs are kept. `--no-trace` or `HAUKSBEE_EXTRACT_NO_TRACE=1`
//! turns it off.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// How many run directories a trace root keeps; older ones are removed.
pub const KEEP_RUNS: usize = 10;

/// One run's trace directory.
#[derive(Debug)]
pub struct Trace {
    dir: PathBuf,
    calls: AtomicUsize,
}

impl Trace {
    /// The run's directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Keep one agent call. Best-effort: a trace that cannot be written never
    /// fails the extraction.
    pub fn record(&self, argv: &[String], prompt: &str, events: &str) {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = std::fs::write(self.dir.join(format!("call-{n}.argv")), argv.join("\n") + "\n");
        let _ = std::fs::write(self.dir.join(format!("call-{n}.prompt.md")), prompt);
        let _ = std::fs::write(self.dir.join(format!("call-{n}.events.jsonl")), events);
    }
}

/// Whether the environment turns traces off.
fn disabled_by_env() -> bool {
    std::env::var("HAUKSBEE_EXTRACT_NO_TRACE")
        .is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

/// The trace root for an output directory: `<out-dir>/trace`, or the user's
/// `~/.hauksbee/extract-traces` when the card goes to the model directory.
#[must_use]
pub fn root(out_dir: Option<&Path>) -> PathBuf {
    out_dir.map_or_else(
        || {
            crate::datasheet::default_out_dir()
                .parent()
                .map_or_else(std::env::temp_dir, Path::to_path_buf)
                .join("extract-traces")
        },
        |d| d.join("trace"),
    )
}

/// Start a run's trace, pruning the root to the newest [`KEEP_RUNS`].
/// `None` when traces are off or the directory cannot be made.
#[must_use]
pub fn begin(out_dir: Option<&Path>, part: &str, keep: bool) -> Option<Arc<Trace>> {
    if !keep || disabled_by_env() {
        return None;
    }
    let root = root(out_dir);
    let stamp = utc_stamp();
    let name = format!("{}-{stamp}", crate::datasheet::sanitise_filename(part));
    let mut dir = root.join(&name);
    let mut n = 1;
    while dir.exists() {
        n += 1;
        dir = root.join(format!("{name}-{n}"));
    }
    std::fs::create_dir_all(&dir).ok()?;
    prune(&root, KEEP_RUNS);
    Some(Arc::new(Trace {
        dir,
        calls: AtomicUsize::new(0),
    }))
}

/// Remove all but the `keep` most recently modified run directories.
fn prune(root: &Path, keep: usize) {
    let mut runs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    runs.sort();
    let excess = runs.len().saturating_sub(keep);
    for (_, dir) in runs.into_iter().take(excess) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// `YYYYMMDDTHHMMSSZ` for now, without a date crate.
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
const fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 1..=31 and 1..=12
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_are_kept_and_old_runs_pruned() {
        let out = tempfile::tempdir().unwrap();
        for i in 0..(KEEP_RUNS + 3) {
            let t = begin(Some(out.path()), &format!("P{i}"), true).unwrap();
            t.record(&["codex".into(), "exec".into()], "prompt", "{\"type\":\"x\"}\n");
            // Distinct modification times on coarse filesystems.
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let runs: Vec<_> = std::fs::read_dir(out.path().join("trace")).unwrap().collect();
        assert_eq!(runs.len(), KEEP_RUNS);
        let t = begin(Some(out.path()), "Q", true).unwrap();
        t.record(&["claude".into()], "p", "e");
        t.record(&["claude".into()], "p2", "e2");
        assert_eq!(std::fs::read_to_string(t.dir().join("call-2.prompt.md")).unwrap(), "p2");
        assert!(t.dir().join("call-1.events.jsonl").is_file());
        assert!(begin(Some(out.path()), "Q", false).is_none());
    }

    #[test]
    fn days_convert_to_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_744), (2026, 10, 18));
    }
}
