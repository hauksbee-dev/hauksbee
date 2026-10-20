//! Prompt caching for datasheet extraction: stable-first prompts, per-slot sandboxes
//! and (codex) forks of a warmed base session.
//!
//! Every agent call is a [`Prompt`]: a **base** that is byte-identical for every part
//! of the same kind and stage (the generic rules, the `ds` toolkit docs, the per-kind
//! checklist blocks, the format examples), then a per-part **tail** (the part, the
//! board binding, the template cards, the pages, the retry feedback). A backend that
//! cannot do better sends `base + tail` as one prompt, which still puts the shared
//! text first so a prefix cache (OpenAI's automatic one, Anthropic's) can hit it.
//!
//! * **codex** keys its cache on the session id and gives a fresh `codex exec` a new
//!   one, so nothing is shared between processes. The supported way to share is an
//!   ephemeral fork of a persisted base session (`codex exec --ephemeral fork <id>`),
//!   which inherits the base's session id: see [`codex`].
//! * **Claude Code** caches by content prefix, so the base goes in the system prompt
//!   (`--append-system-prompt-file`) with the per-machine sections moved out of it
//!   (`--exclude-dynamic-system-prompt-sections`), and the sandbox path is stable per
//!   slot: see [`claude_flags`].
//!
//! The measurements behind this are in `target/launch/teacher-traces/CACHING-RESULTS.md`.

pub mod codex;
pub mod slots;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

/// One agent call's prompt, split at the point where the part-specific text starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    /// Byte-identical for every part of the same kind and stage.
    pub base: String,
    /// The part, its pages and any retry feedback.
    pub tail: String,
}

impl Prompt {
    #[must_use]
    pub fn new(base: impl Into<String>, tail: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            tail: tail.into(),
        }
    }

    /// The prompt as one text, for a backend that takes no split.
    #[must_use]
    pub fn full(&self) -> String {
        format!("{}{}", self.base, self.tail)
    }

    /// This prompt with `more` appended to the tail (retry feedback, handoff notes).
    #[must_use]
    pub fn then(&self, more: &str) -> Self {
        Self {
            base: self.base.clone(),
            tail: format!("{}{more}", self.tail),
        }
    }

    /// This prompt with `more` appended to the base: text that is the same for
    /// every part of a run (the ladder's confidence clause).
    #[must_use]
    pub fn with_base(&self, more: &str) -> Self {
        Self {
            base: format!("{}{more}", self.base),
            tail: self.tail.clone(),
        }
    }

    /// The same split with `f` applied to the tail only.
    #[must_use]
    pub fn map_tail(&self, f: impl FnOnce(&str) -> String) -> Self {
        Self {
            base: self.base.clone(),
            tail: f(&self.tail),
        }
    }
}

impl From<String> for Prompt {
    fn from(tail: String) -> Self {
        Self {
            base: String::new(),
            tail,
        }
    }
}

impl From<&Prompt> for Prompt {
    fn from(p: &Self) -> Self {
        p.clone()
    }
}

/// The whole text, base then tail.
impl std::fmt::Display for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.base, self.tail)
    }
}

impl From<&str> for Prompt {
    fn from(tail: &str) -> Self {
        tail.to_string().into()
    }
}

/// What the caller asked of caching, from `HAUKSBEE_EXTRACT_CACHE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Stable-first prompts only: every call is a fresh process with the whole prompt.
    Off,
    /// Everything the backend supports and this install proves it can do (default).
    Auto,
    /// As `Auto`, without the one-time capability probe (the unit tests' fake agents).
    Force,
}

pub const ENV_CACHE: &str = "HAUKSBEE_EXTRACT_CACHE";

impl Mode {
    /// `off`/`0`/`false`, `force`, else `auto`.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(ENV_CACHE).as_deref().map(str::trim) {
            Ok("off" | "0" | "false" | "no") => Self::Off,
            Ok("force") => Self::Force,
            _ => Self::Auto,
        }
    }

    /// Whether the codex fork path may be used at all.
    #[must_use]
    pub fn forks(self) -> bool {
        match self {
            Self::Off => false,
            Self::Force => true,
            Self::Auto => !cfg!(test),
        }
    }
}

/// Flags that make a `claude -p` call's prefix stable across parts: the shared base as
/// the appended system prompt (read from `base_file`, outside the sandbox) and the
/// per-machine sections (cwd, platform, memory paths, git status) moved into the first
/// user message, so only the sandbox path and the tail differ between calls. The cache
/// TTL is asked for explicitly: a subscription already gets one hour for the main
/// conversation, an API key five minutes, and a batch can idle between parts.
#[must_use]
pub fn claude_flags(base_file: Option<&Path>) -> Vec<std::ffi::OsString> {
    let mut flags: Vec<std::ffi::OsString> =
        vec!["--exclude-dynamic-system-prompt-sections".into()];
    if let Some(file) = base_file {
        flags.push("--append-system-prompt-file".into());
        flags.push(file.into());
    }
    flags
}

/// The environment `claude -p` is given for caching: the main conversation's TTL.
pub const CLAUDE_CACHE_ENV: [(&str, &str); 1] = [("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h")];

/// A pool key from the parts that decide whether two calls may share a base session.
#[must_use]
pub fn pool_key(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A directory for files shared by every call of this process (the Claude base
/// prompts): `<temp>/hauksbee-extract-shared-<pid>`, created on first use.
///
/// # Errors
///
/// Fails when the directory cannot be created.
pub fn shared_dir() -> std::io::Result<PathBuf> {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Ok(dir.clone());
    }
    let root = std::env::temp_dir().canonicalize()?;
    let dir = root.join(format!("hauksbee-extract-shared-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(DIR.get_or_init(|| dir).clone())
}

/// Write `base` to a content-addressed file in [`shared_dir`] and return its path.
///
/// # Errors
///
/// Fails when the file cannot be written.
pub fn base_file(base: &str) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(base.as_bytes());
    let name: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = shared_dir()?.join(format!("base-{name}.md"));
    if !path.is_file() {
        std::fs::write(&path, base)?;
    }
    Ok(path)
}

/// One call's token counts, with the cache split out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    /// All input tokens, the cached and the cache-written ones included.
    pub input: u64,
    /// Input tokens read from the prompt cache.
    pub cached: u64,
    /// Input tokens written to the prompt cache (Claude bills these; codex reports 0).
    pub written: u64,
    pub output: u64,
}

impl Tokens {
    /// From a codex `turn.completed` `usage` object.
    #[must_use]
    pub fn from_codex(usage: &Value) -> Option<Self> {
        let n = |k: &str| usage[k].as_u64();
        Some(Self {
            input: n("input_tokens")?,
            cached: n("cached_input_tokens").unwrap_or(0),
            written: n("cache_write_input_tokens").unwrap_or(0),
            output: n("output_tokens")?,
        })
    }

    /// Summed over the models of a Claude Code `result` event's `modelUsage`.
    #[must_use]
    pub fn from_claude(result: &Value) -> Option<Self> {
        let models = result["modelUsage"].as_object()?;
        let total = models.values().fold(Self::default(), |sum, m| {
            let n = |k: &str| m[k].as_u64().unwrap_or(0);
            let (read, written) = (n("cacheReadInputTokens"), n("cacheCreationInputTokens"));
            sum.plus(Self {
                input: n("inputTokens") + read + written,
                cached: read,
                written,
                output: n("outputTokens"),
            })
        });
        (!models.is_empty()).then_some(total)
    }

    #[must_use]
    pub const fn plus(self, o: Self) -> Self {
        Self {
            input: self.input + o.input,
            cached: self.cached + o.cached,
            written: self.written + o.written,
            output: self.output + o.output,
        }
    }

    /// `self - o`, each field stopping at zero.
    #[must_use]
    pub const fn minus(self, o: Self) -> Self {
        Self {
            input: self.input.saturating_sub(o.input),
            cached: self.cached.saturating_sub(o.cached),
            written: self.written.saturating_sub(o.written),
            output: self.output.saturating_sub(o.output),
        }
    }

    /// Write these counts into a codex `usage` object (fields it already has).
    pub fn write_codex(self, usage: &mut Value) {
        for (key, n) in [
            ("input_tokens", self.input),
            ("cached_input_tokens", self.cached),
            ("cache_write_input_tokens", self.written),
            ("output_tokens", self.output),
        ] {
            if usage.get(key).is_some() {
                usage[key] = n.into();
            }
        }
    }

    /// Share of the input read from cache, `None` with no input.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
    pub fn hit_ratio(self) -> Option<f64> {
        (self.input > 0).then(|| self.cached as f64 / self.input as f64)
    }
}

#[derive(Default, Clone, Copy)]
struct Ledger {
    calls: usize,
    tokens: Tokens,
}

/// The current extraction's calls (cleared by [`reset`]) and the whole process's (a batch).
static RUN: Mutex<Ledger> = Mutex::new(Ledger {
    calls: 0,
    tokens: Tokens {
        input: 0,
        cached: 0,
        written: 0,
        output: 0,
    },
});
static BATCH: Mutex<Ledger> = Mutex::new(Ledger {
    calls: 0,
    tokens: Tokens {
        input: 0,
        cached: 0,
        written: 0,
        output: 0,
    },
});

fn add(ledger: &Mutex<Ledger>, t: Tokens) {
    let mut l = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    l.calls += 1;
    l.tokens = l.tokens.plus(t);
}

/// A new extraction: its cache ratio starts again (the batch's keeps counting).
pub fn reset() {
    *RUN.lock().unwrap_or_else(PoisonError::into_inner) = Ledger::default();
}

fn percent(t: Tokens) -> String {
    t.hit_ratio()
        .map_or_else(|| "n/a".to_string(), |r| format!("{:.0}%", r * 100.0))
}

/// The line one call adds to the log: its input, cache-read and cache-write tokens.
#[must_use]
pub fn call_line(backend: &str, how: &str, t: Tokens) -> String {
    format!(
        "cache: {backend} {how}: {} input, {} read from cache ({}), {} written to cache, {} output",
        t.input,
        t.cached,
        percent(t),
        t.written,
        t.output
    )
}

/// Record one call's cache use (`how`: `fresh`, `fork`, `base warm-up`) and say so.
pub fn record_call(backend: &str, how: &str, t: Tokens) {
    add(&RUN, t);
    add(&BATCH, t);
    crate::extract_progress::say(&format!("[model-extract] {}", call_line(backend, how, t)));
    crate::extract_progress::event(
        "cache",
        serde_json::json!({
            "backend": backend, "how": how, "input": t.input, "cache_read": t.cached,
            "cache_write": t.written, "output": t.output, "hit_ratio": t.hit_ratio(),
        }),
    );
}

/// This extraction's cache line, empty before any call.
#[must_use]
pub fn summary() -> String {
    let l = *RUN.lock().unwrap_or_else(PoisonError::into_inner);
    ledger_line("this extraction", l)
}

/// The whole process's (a batch's) cache line, empty before any call.
#[must_use]
pub fn batch_summary() -> String {
    let l = *BATCH.lock().unwrap_or_else(PoisonError::into_inner);
    ledger_line("batch", l)
}

fn ledger_line(what: &str, l: Ledger) -> String {
    if l.calls == 0 {
        return String::new();
    }
    let t = l.tokens;
    format!(
        "prompt cache ({what}): {} of {} input tokens read from cache over {} call(s), {} \
         written, {} uncached",
        percent(t),
        t.input,
        l.calls,
        t.written,
        t.input.saturating_sub(t.cached)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_joins_and_extends_without_touching_the_base() {
        let p = Prompt::new("rules\n\n", "part X");
        assert_eq!(p.full(), "rules\n\npart X");
        let q = p.then("\nfeedback");
        assert_eq!(q.base, p.base);
        assert_eq!(q.full(), "rules\n\npart X\nfeedback");
        assert_eq!(Prompt::from("only").base, "");
    }

    #[test]
    fn the_claude_flags_name_the_base_file_and_drop_the_dynamic_sections() {
        let f = claude_flags(Some(Path::new("/x/base.md")));
        assert_eq!(f[0], "--exclude-dynamic-system-prompt-sections");
        assert_eq!(f[1], "--append-system-prompt-file");
        assert_eq!(f[2], "/x/base.md");
        assert_eq!(claude_flags(None).len(), 1);
    }

    #[test]
    fn tokens_read_from_both_backends_and_the_ratio_is_cached_over_input() {
        let codex = serde_json::json!({"input_tokens": 1000, "cached_input_tokens": 750,
            "cache_write_input_tokens": 5, "output_tokens": 20});
        let t = Tokens::from_codex(&codex).unwrap();
        assert_eq!((t.input, t.cached, t.written, t.output), (1000, 750, 5, 20));
        assert_eq!(t.hit_ratio(), Some(0.75));
        let claude = serde_json::json!({"modelUsage": {
            "claude-haiku": {"inputTokens": 10, "cacheReadInputTokens": 90,
                "cacheCreationInputTokens": 400, "outputTokens": 7},
            "claude-sonnet": {"inputTokens": 5, "outputTokens": 1}}});
        let c = Tokens::from_claude(&claude).unwrap();
        assert_eq!((c.input, c.cached, c.written, c.output), (505, 90, 400, 8));
        assert_eq!(Tokens::from_claude(&serde_json::json!({})), None);
        assert_eq!(Tokens::default().hit_ratio(), None);
        let line = call_line("codex", "fork", t);
        assert!(
            line.contains("1000 input") && line.contains("750 read from cache (75%)"),
            "{line}"
        );
    }

    #[test]
    fn a_base_file_is_named_by_its_content() {
        let (a, b) = (base_file("one").unwrap(), base_file("two").unwrap());
        assert_ne!(a, b);
        assert_eq!(a, base_file("one").unwrap());
        assert_eq!(std::fs::read_to_string(a).unwrap(), "one");
    }
}
