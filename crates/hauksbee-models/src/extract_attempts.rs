//! A model library's record of extractions that did not end in a card.
//!
//! One person extracting for board after board into one shared library meets
//! the same hard parts every time: the ones whose extraction failed, and the
//! ones no datasheet could be found for. Without a memory, every later board
//! sends them to the model again, at full cost, for the same answer.
//!
//! The library keeps a small ledger beside its cards ([`FILE`]): one row per
//! part whose last attempt failed or found no datasheet, with the *inputs* that
//! attempt had ([`Inputs`]: the datasheet it was given, the backend, model and
//! effort, the card kind, hauksbee's version) hashed into a key. A later batch
//! asks [`Ledger::verdict`]: the same inputs and a failed attempt mean the part
//! is skipped, and any change of input (a new datasheet supplied, another model,
//! a new release) means it is tried again by itself. `--retry-failed` and
//! `--force` ignore the ledger. A search that found nothing is believed for
//! [`NO_DATASHEET_DAYS`] days, because the web changes; a failed extraction is
//! believed until its inputs change.
//!
//! Only outcomes that say something about the part are recorded: a rate limit,
//! a crashed run or a search that could not even start is the service's
//! trouble, not the part's, and is never remembered. A part that is later saved
//! is forgotten. The file is advisory: unreadable or missing, it is empty.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::Path;

/// The ledger's file name, in the library directory the cards are written to.
pub const FILE: &str = "extract-attempts.json";

/// How long a search that found no datasheet is believed.
pub const NO_DATASHEET_DAYS: u64 = 7;

const DAY_SECS: u64 = 24 * 60 * 60;

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    /// The extraction ran and was refused (the model, the validator).
    Failed,
    /// No datasheet could be obtained, so nothing was sent.
    NoDatasheet,
}

/// Everything that decides whether a repeat attempt could end differently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    /// The part number the card is for.
    pub part: String,
    /// The datasheet links the board names, or the fingerprint of a local file.
    pub sources: Vec<String>,
    /// The fingerprint of a PDF a person supplied (or an earlier fetch kept).
    pub supplied: Option<String>,
    pub backend: String,
    pub model: String,
    pub effort: String,
    /// The card kind asked for (empty: named from the datasheet).
    pub kind: String,
    /// hauksbee's version: a new release may extract differently.
    pub version: String,
}

/// A short stand-in for a file's contents: its SHA-256, or `None` when it
/// cannot be read.
#[must_use]
pub fn fingerprint(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex(&Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl Inputs {
    /// The hash these inputs are compared by.
    #[must_use]
    pub fn key(&self) -> String {
        let mut text = String::new();
        let mut field = |name: &str, value: &str| {
            let _ = writeln!(text, "{name}={value}");
        };
        field("part", &self.part.to_ascii_lowercase());
        for s in &self.sources {
            field("source", s);
        }
        field("supplied", self.supplied.as_deref().unwrap_or(""));
        field("backend", &self.backend);
        field("model", &self.model);
        field("effort", &self.effort);
        field("kind", &self.kind);
        field("version", &self.version);
        hex(&Sha256::digest(text))[..16].to_string()
    }
}

/// One remembered attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub part: String,
    /// [`Inputs::key`] of the attempt.
    pub inputs: String,
    pub result: Ended,
    /// Why it ended so, as the run said it (one line).
    pub reason: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
    pub backend: String,
    pub model: String,
    pub version: String,
}

/// What the ledger says about a part about to be attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing known that argues against trying.
    Try,
    /// The same inputs failed before.
    SkipFailed { reason: String, ago: String },
    /// The same inputs found no datasheet recently.
    SkipNoDatasheet { reason: String, ago: String },
}

/// The ledger as read from a library directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub attempts: Vec<Attempt>,
}

/// Seconds since the Unix epoch now.
#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `secs` of age as people say it.
#[must_use]
pub fn ago(secs: u64) -> String {
    match secs / DAY_SECS {
        0 => "earlier today".into(),
        1 => "1 day ago".into(),
        n => format!("{n} days ago"),
    }
}

/// Whether a reason is the service's trouble and not the part's, so it says
/// nothing about whether trying again would help.
#[must_use]
pub fn is_transient(reason: &str) -> bool {
    crate::extract_capacity::is_rate_limited(reason)
        || reason == "the extraction crashed"
        || reason.starts_with("the web search for its datasheet failed")
}

const fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn one_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 160 {
        let cut: String = line.chars().take(157).collect();
        format!("{cut}...")
    } else {
        line
    }
}

impl Ledger {
    /// The ledger in `dir`; empty when there is none or it cannot be read.
    #[must_use]
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the ledger to `dir`, whole and atomically (an empty one removes
    /// the file).
    ///
    /// # Errors
    ///
    /// The directory or file could not be written.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let file = dir.join(FILE);
        if self.attempts.is_empty() {
            return match std::fs::remove_file(&file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(&tmp, &file)
    }

    /// Apply `change` to the ledger as it is on disk now, and write it back, so
    /// two batches sharing a library do not overwrite one another's rows.
    ///
    /// # Errors
    ///
    /// The ledger could not be written.
    pub fn update(dir: &Path, change: impl FnOnce(&mut Self)) -> std::io::Result<()> {
        let mut ledger = Self::load(dir);
        change(&mut ledger);
        ledger.save(dir)
    }

    /// The remembered attempt for `part`.
    #[must_use]
    pub fn find(&self, part: &str) -> Option<&Attempt> {
        self.attempts.iter().find(|a| same(&a.part, part))
    }

    /// Remember that the attempt with `inputs` ended as `result`, replacing any
    /// earlier row for the part. A transient `reason` is not remembered (an
    /// earlier row for the part is kept).
    pub fn record(&mut self, inputs: &Inputs, result: Ended, reason: &str, at: u64) {
        if is_transient(reason) {
            return;
        }
        self.forget(&inputs.part);
        self.attempts.push(Attempt {
            part: inputs.part.clone(),
            inputs: inputs.key(),
            result,
            reason: one_line(reason),
            at,
            backend: inputs.backend.clone(),
            model: inputs.model.clone(),
            version: inputs.version.clone(),
        });
    }

    /// Forget `part` (its card was saved).
    pub fn forget(&mut self, part: &str) {
        self.attempts.retain(|a| !same(&a.part, part));
    }

    /// Whether a batch at time `now` should attempt a part with `inputs`.
    #[must_use]
    pub fn verdict(&self, inputs: &Inputs, now: u64) -> Verdict {
        let Some(last) = self.find(&inputs.part).filter(|a| a.inputs == inputs.key()) else {
            return Verdict::Try;
        };
        let age = now.saturating_sub(last.at);
        let (reason, ago) = (last.reason.clone(), ago(age));
        match last.result {
            Ended::Failed => Verdict::SkipFailed { reason, ago },
            Ended::NoDatasheet if age < NO_DATASHEET_DAYS * DAY_SECS => {
                Verdict::SkipNoDatasheet { reason, ago }
            }
            Ended::NoDatasheet => Verdict::Try,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(part: &str) -> Inputs {
        Inputs {
            part: part.into(),
            sources: vec!["https://example.com/a.pdf".into()],
            supplied: None,
            backend: "codex".into(),
            model: "m1".into(),
            effort: "high".into(),
            kind: String::new(),
            version: "1.0.0".into(),
        }
    }

    const DAY: u64 = DAY_SECS;

    #[test]
    fn a_failed_part_is_skipped_while_its_inputs_are_unchanged() {
        let mut ledger = Ledger::default();
        let i = inputs("LM358");
        assert_eq!(ledger.verdict(&i, 1000), Verdict::Try, "nothing known");
        ledger.record(&i, Ended::Failed, "the validator refused pin 3", 1000);
        // However long ago: a failed extraction is believed until inputs change.
        match ledger.verdict(&i, 1000 + 400 * DAY) {
            Verdict::SkipFailed { reason, ago } => {
                assert_eq!(reason, "the validator refused pin 3");
                assert_eq!(ago, "400 days ago");
            }
            other => panic!("expected a skip, got {other:?}"),
        }
        assert!(
            matches!(
                ledger.verdict(&inputs("lm358"), 2000),
                Verdict::SkipFailed { .. }
            ),
            "part names compare without case"
        );
    }

    #[test]
    fn any_change_of_input_retries_the_part() {
        let mut ledger = Ledger::default();
        let i = inputs("LM358");
        ledger.record(&i, Ended::Failed, "refused", 1000);
        let changed: [(&str, Inputs); 6] = [
            (
                "a new datasheet supplied",
                Inputs {
                    supplied: Some("abc".into()),
                    ..i.clone()
                },
            ),
            (
                "another datasheet link",
                Inputs {
                    sources: vec!["https://example.com/b.pdf".into()],
                    ..i.clone()
                },
            ),
            (
                "another model",
                Inputs {
                    model: "m2".into(),
                    ..i.clone()
                },
            ),
            (
                "another backend",
                Inputs {
                    backend: "claude-code".into(),
                    ..i.clone()
                },
            ),
            (
                "another effort",
                Inputs {
                    effort: "max".into(),
                    ..i.clone()
                },
            ),
            (
                "a new version",
                Inputs {
                    version: "1.0.1".into(),
                    ..i.clone()
                },
            ),
        ];
        for (what, inputs) in changed {
            assert_eq!(ledger.verdict(&inputs, 2000), Verdict::Try, "{what}");
        }
        let kind = Inputs {
            kind: "opamp".into(),
            ..i
        };
        assert_eq!(ledger.verdict(&kind, 2000), Verdict::Try, "a chosen kind");
    }

    #[test]
    fn a_search_that_found_nothing_is_believed_for_a_week() {
        let mut ledger = Ledger::default();
        let i = inputs("MODULE-X");
        ledger.record(
            &i,
            Ended::NoDatasheet,
            "the web search found no usable datasheet",
            5 * DAY,
        );
        assert!(matches!(
            ledger.verdict(&i, 5 * DAY + 3 * DAY),
            Verdict::SkipNoDatasheet { ago, .. } if ago == "3 days ago"
        ));
        assert_eq!(
            ledger.verdict(&i, 5 * DAY + NO_DATASHEET_DAYS * DAY),
            Verdict::Try,
            "a week on, search again"
        );
    }

    #[test]
    fn the_services_trouble_is_not_the_parts_and_is_not_remembered() {
        let mut ledger = Ledger::default();
        let i = inputs("LM358");
        for why in [
            "codex: 429 Too Many Requests",
            "the extraction crashed",
            "the web search for its datasheet failed: codex exited 1",
        ] {
            ledger.record(&i, Ended::Failed, why, 1);
            assert!(ledger.attempts.is_empty(), "{why}");
        }
        // An earlier real failure survives a later transient one.
        ledger.record(&i, Ended::Failed, "refused", 1);
        ledger.record(&i, Ended::Failed, "rate limit hit", 2);
        assert_eq!(ledger.attempts.len(), 1);
        assert_eq!(ledger.attempts[0].reason, "refused");
    }

    #[test]
    fn recording_replaces_the_row_and_saving_forgets_it() {
        let mut ledger = Ledger::default();
        let i = inputs("LM358");
        ledger.record(&i, Ended::NoDatasheet, "found nothing", 1);
        ledger.record(&i, Ended::Failed, "line one\nline two", 2);
        assert_eq!(ledger.attempts.len(), 1);
        assert_eq!(ledger.attempts[0].reason, "line one");
        ledger.forget("lm358");
        assert!(ledger.attempts.is_empty());
    }

    #[test]
    fn the_ledger_round_trips_through_its_file_and_merges_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Ledger::load(dir.path()),
            Ledger::default(),
            "no file: empty"
        );
        let (a, b) = (inputs("A"), inputs("B"));
        Ledger::update(dir.path(), |l| l.record(&a, Ended::Failed, "no", 1)).unwrap();
        // A second writer that never saw the first one's row keeps it.
        Ledger::update(dir.path(), |l| l.record(&b, Ended::NoDatasheet, "none", 2)).unwrap();
        let ledger = Ledger::load(dir.path());
        assert_eq!(ledger.attempts.len(), 2);
        assert!(matches!(ledger.verdict(&a, 3), Verdict::SkipFailed { .. }));
        assert!(matches!(
            ledger.verdict(&b, 3),
            Verdict::SkipNoDatasheet { .. }
        ));
        Ledger::update(dir.path(), |l| {
            l.forget("A");
            l.forget("B");
        })
        .unwrap();
        assert!(
            !dir.path().join(FILE).exists(),
            "an empty ledger leaves no file"
        );
    }

    #[test]
    fn an_unreadable_ledger_is_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{ not json").unwrap();
        assert_eq!(Ledger::load(dir.path()), Ledger::default());
    }

    #[test]
    fn a_supplied_file_changes_the_key_by_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("x.pdf");
        std::fs::write(&pdf, b"one").unwrap();
        let first = fingerprint(&pdf).unwrap();
        std::fs::write(&pdf, b"two").unwrap();
        assert_ne!(first, fingerprint(&pdf).unwrap());
        assert_eq!(fingerprint(&dir.path().join("none.pdf")), None);
    }

    #[test]
    fn ages_read_as_people_say_them() {
        assert_eq!(ago(0), "earlier today");
        assert_eq!(ago(DAY + 5), "1 day ago");
        assert_eq!(ago(9 * DAY), "9 days ago");
    }
}
