//! Codex forking: warm one shared base per (model, effort, schema, flags, base text),
//! then run each part as an ephemeral fork of it.
//!
//! Why. Codex's prompt cache is keyed on the session id, and every `codex exec` is a
//! new session, so a fresh process never reads what the previous part's process wrote:
//! measured, a fresh call re-reads its ~19k-token shared prefix at full price every time
//! (0 to 49% cached, erratically) while the first request of a fork of a warmed base
//! reads about 95% of it from cache. `codex exec fork <id>` inherits the base's session
//! id, so the base's cache entry serves every fork.
//!
//! Confinement is exactly a fresh call's: the fork command carries the same `-c`
//! permission profile, `--ignore-user-config`, `--ignore-rules`, feature switches and
//! output schema, and runs with the part's own sandbox as its working directory (a fork
//! takes no `--cd`; the process cwd is used). Probed with codex-cli 0.159.0: a fork
//! cannot read or write a sibling part's sandbox, `$HOME`, `/tmp` or `$TMPDIR`, and
//! writes its own sandbox only. It does not apply with a profile (`-p`), `--sandbox` or
//! `--approve-for-me`, which `exec fork` rejects; those runs stay fresh calls.
//!
//! A fork's `turn.completed.usage` includes everything its parent session used, so the
//! base's own usage is subtracted from what the fork reports ([`without_base`]).
//!
//! The base session is persisted under `$CODEX_HOME/sessions` and holds only the generic
//! briefing, never a datasheet. They are deleted by [`release_bases`] and, for a process
//! that died first, by [`sweep_stale`] at the next warm-up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde_json::Value;

use super::Tokens;

/// A warmed base session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// The session id forks are made from.
    pub id: String,
    /// What warming it used, which every fork's reported usage includes.
    pub used: Tokens,
}

#[derive(Debug, Clone)]
enum State {
    Warm(Base),
    /// Warming failed (or a fork found the base gone): this process stops trying.
    Failed,
}

type Cell = Arc<Mutex<Option<State>>>;

static POOL: Mutex<Option<HashMap<String, Cell>>> = Mutex::new(None);

fn cell(key: &str) -> Cell {
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        pool.get_or_insert_with(HashMap::new)
            .entry(key.to_string())
            .or_default(),
    )
}

/// The base for `key`, warming it with `warm` the first time. Callers racing for
/// the same key wait for the one warm-up; `None` means "no base: run a fresh call".
pub fn base_for(key: &str, warm: impl FnOnce() -> anyhow::Result<Base>) -> Option<Base> {
    let cell = cell(key);
    let mut state = cell.lock().unwrap_or_else(PoisonError::into_inner);
    if state.is_none() {
        sweep_stale();
        *state = Some(match warm() {
            Ok(base) => {
                remember(&base.id);
                State::Warm(base)
            }
            Err(e) => {
                crate::extract_progress::say(&format!(
                    "[model-extract] cache: warming the shared codex base failed ({e:#}); \
                     this run uses fresh calls"
                ));
                State::Failed
            }
        });
    }
    match state.as_ref() {
        Some(State::Warm(base)) => Some(base.clone()),
        _ => None,
    }
}

/// A fork found its base unusable: stop forking from it in this process.
pub fn invalidate(key: &str) {
    *cell(key).lock().unwrap_or_else(PoisonError::into_inner) = Some(State::Failed);
}

/// The session id from a codex `--json` stream's `thread.started` event.
#[must_use]
pub fn session_id(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let event: Value = serde_json::from_str(line).ok()?;
        (event["type"] == "thread.started")
            .then(|| event["thread_id"].as_str().map(str::to_string))
            .flatten()
    })
}

/// The warm-up call's instruction: the shared briefing, then a request to do
/// nothing, so the base session ends after one cheap reply with the briefing in
/// its history.
#[must_use]
pub fn warm_prompt(base: &str) -> String {
    format!(
        "{base}\n\n---\nThe text above is the shared briefing for a series of extraction \
         tasks. The task for one part arrives in the next message. Right now do nothing: no \
         tool calls, no files. Reply with the single word READY."
    )
}

/// What a fork is sent: the part's own text, introduced as the task the briefing is for.
#[must_use]
pub fn fork_prompt(tail: &str) -> String {
    format!("The task for this part follows; the shared briefing above applies to it.\n\n{tail}")
}

/// `usage` of the `turn.completed` events of a codex `--json` stream.
#[must_use]
pub fn tokens_of(stdout: &str) -> Option<Tokens> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "turn.completed")
        .filter_map(|e| Tokens::from_codex(&e["usage"]))
        .reduce(Tokens::plus)
}

/// `stdout` with the base's usage taken out of each `turn.completed` event.
///
/// A fork reports its parent's accumulated usage too, and the base was already
/// recorded when it was warmed. Other lines pass through untouched.
#[must_use]
pub fn without_base(stdout: &str, base: Tokens) -> String {
    let mut out = String::with_capacity(stdout.len());
    for line in stdout.lines() {
        let fixed = serde_json::from_str::<Value>(line)
            .ok()
            .filter(|e| e["type"] == "turn.completed")
            .and_then(|mut e| {
                let own = Tokens::from_codex(&e["usage"])?.minus(base);
                own.write_codex(&mut e["usage"]);
                serde_json::to_string(&e).ok()
            });
        out.push_str(fixed.as_deref().unwrap_or(line));
        out.push('\n');
    }
    out
}

/// `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

fn manifest(pid: u32) -> Option<PathBuf> {
    let root = std::env::temp_dir().canonicalize().ok()?;
    Some(
        root.join(format!("hauksbee-extract-shared-{pid}"))
            .join("bases.txt"),
    )
}

/// Write the session id to this process's manifest, so a later run can delete the
/// rollout if this one dies before [`release_bases`].
fn remember(id: &str) {
    use std::io::Write as _;
    let Some(path) = manifest(std::process::id()) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{id}");
    }
}

/// Delete the persisted rollout(s) of session `id` under `home`; how many went.
fn delete_rollouts(home: &Path, id: &str) -> usize {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return 0;
    }
    let mut removed = 0;
    let dirs = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    };
    for year in dirs(&home.join("sessions")) {
        for month in dirs(&year) {
            for day in dirs(&month) {
                for file in dirs(&day) {
                    let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
                    let ours = name.is_some_and(|n| {
                        n.starts_with("rollout-") && n.ends_with(&format!("-{id}.jsonl"))
                    });
                    if ours && std::fs::remove_file(&file).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
    }
    removed
}

fn release_manifest(home: &Path, manifest: &Path) -> usize {
    let ids = std::fs::read_to_string(manifest).unwrap_or_default();
    let removed = ids.lines().map(|id| delete_rollouts(home, id.trim())).sum();
    let _ = std::fs::remove_file(manifest);
    removed
}

/// Delete this process's base sessions from the codex session store. Call when a
/// batch ends; anything missed is swept by the next run ([`sweep_stale`]).
pub fn release_bases() -> usize {
    *POOL.lock().unwrap_or_else(PoisonError::into_inner) = None;
    let (Some(home), Some(path)) = (codex_home(), manifest(std::process::id())) else {
        return 0;
    };
    release_manifest(&home, &path)
}

/// Delete the base sessions of processes that are gone (a crash or a kill left them),
/// at most once per process.
pub fn sweep_stale() {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        return;
    }
    let (Some(home), Ok(root)) = (codex_home(), std::env::temp_dir().canonicalize()) else {
        return;
    };
    let entries = std::fs::read_dir(&root).into_iter().flatten().flatten();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name
            .strip_prefix("hauksbee-extract-shared-")
            .and_then(|p| p.parse::<u32>().ok());
        let Some(_) = pid.filter(|p| *p != std::process::id() && !alive(*p)) else {
            continue;
        };
        release_manifest(&home, &entry.path().join("bases.txt"));
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// Whether process `pid` is running (signal 0 through `kill -0`; unknown counts as alive).
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map_or(true, |s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORK_STREAM: &str = concat!(
        "{\"type\":\"thread.started\",\"thread_id\":\"01a1545a-c16f-7e93-b4f7-24e707a5aec0\"}\n",
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"READY\"}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":38720,\"cached_input_tokens\":18816,",
        "\"cache_write_input_tokens\":0,\"output_tokens\":24,\"reasoning_output_tokens\":12}}\n",
    );

    #[test]
    fn the_session_id_is_the_first_thread_started() {
        assert_eq!(
            session_id(FORK_STREAM).as_deref(),
            Some("01a1545a-c16f-7e93-b4f7-24e707a5aec0")
        );
        assert_eq!(session_id("not json\n{\"type\":\"turn.started\"}"), None);
    }

    #[test]
    fn a_forks_usage_loses_the_bases_and_keeps_the_rest_of_the_stream() {
        let base = Tokens {
            input: 19_828,
            cached: 18_944,
            written: 0,
            output: 12,
        };
        let fixed = without_base(FORK_STREAM, base);
        let own = tokens_of(&fixed).unwrap();
        assert_eq!(own.input, 38_720 - 19_828);
        assert_eq!(own.cached, 0, "saturates at zero: {own:?}");
        assert_eq!(own.output, 12);
        assert_eq!(fixed.lines().count(), FORK_STREAM.lines().count());
        assert!(fixed.contains("\"agent_message\""));
        // A stream with no usage, and a base larger than the report, are left safe.
        assert_eq!(without_base("garbage\n", base), "garbage\n");
        let bigger = Tokens {
            input: 1 << 40,
            ..base
        };
        assert_eq!(
            tokens_of(&without_base(FORK_STREAM, bigger)).unwrap().input,
            0
        );
    }

    #[test]
    fn the_warm_prompt_asks_for_nothing_and_the_fork_prompt_carries_only_the_tail() {
        let warm = warm_prompt("RULES");
        assert!(warm.starts_with("RULES\n\n---"));
        assert!(warm.contains("Reply with the single word READY"));
        assert!(fork_prompt("PART X").ends_with("PART X"));
        assert!(!fork_prompt("PART X").contains("RULES"));
    }

    #[test]
    fn a_base_is_warmed_once_per_key_and_a_failure_means_fresh_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let warmed = AtomicUsize::new(0);
        let warm = || {
            warmed.fetch_add(1, Ordering::SeqCst);
            Ok(Base {
                id: "abc-123".into(),
                used: Tokens::default(),
            })
        };
        let key = "test-key-once";
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| assert_eq!(base_for(key, warm).unwrap().id, "abc-123"));
            }
        });
        assert_eq!(warmed.load(Ordering::SeqCst), 1);
        invalidate(key);
        assert_eq!(
            base_for(key, warm),
            None,
            "an invalidated base is not re-warmed"
        );
        assert_eq!(warmed.load(Ordering::SeqCst), 1);
        let failing = || anyhow::bail!("no codex");
        assert_eq!(base_for("test-key-fails", failing), None);
        assert_eq!(
            base_for("test-key-fails", warm),
            None,
            "one failure is final"
        );
    }

    #[test]
    fn only_rollouts_of_the_named_session_are_deleted() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/10/19");
        std::fs::create_dir_all(&day).unwrap();
        let mine = day.join("rollout-2026-10-19T10-00-00-0aa1-bb2.jsonl");
        let other = day.join("rollout-2026-10-19T10-00-01-0aa1-bb3.jsonl");
        let odd = day.join("notes-0aa1-bb2.jsonl");
        for f in [&mine, &other, &odd] {
            std::fs::write(f, "{}").unwrap();
        }
        assert_eq!(delete_rollouts(home.path(), "0aa1-bb2"), 1);
        assert!(!mine.exists() && other.exists() && odd.exists());
        assert_eq!(
            delete_rollouts(home.path(), "../../etc"),
            0,
            "ids are hex and dashes only"
        );
        let list = home.path().join("bases.txt");
        std::fs::write(&list, "0aa1-bb3\n\n").unwrap();
        assert_eq!(release_manifest(home.path(), &list), 1);
        assert!(!other.exists() && !list.exists());
    }
}
