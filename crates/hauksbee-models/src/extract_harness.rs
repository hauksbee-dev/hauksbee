//! The staged harness and the escalation ladder around an agent extraction.
//!
//! A small model reads a datasheet well in short, focused sessions and badly in
//! one long one: by the time it has paged through forty pages of PDF its context
//! is full of curves it never needed. So a model with a context budget
//! ([`crate::extract_config::ModelChoice::context_budget`]) runs in stages, each
//! a fresh agent in the same sandbox, each killed if its context passes the budget:
//!
//! 1. **locate**: from `page-index.md` and only the pages it names, write
//!    `notes.md`: the package, the pin map, the key pages and values, and what
//!    is ambiguous. Compact notes, not a transcript, are what the next agent gets.
//! 2. **draft**: the full extraction prompt plus those notes; writes `model.toml`.
//! 3. **fix**: a draft the validator refuses goes to a fresh agent as
//!    `previous.toml` with the error, rather than redoing the reading.
//!
//! Every tier, staged or not, then faces the same gates as before (validation,
//! retries with feedback, review before save). A tier that still fails, runs out
//! of time or budget, reports low confidence, or leaves its pin map unresolved
//! hands over to the next model in the ladder ([`crate::extract_config::Resolved::ladder`]),
//! which inherits the notes; the card records which tier wrote it and why each
//! escalation happened.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::extract_cache::Prompt;
use crate::extract_progress::event;

pub const NOTES: &str = "notes.md";
pub const CONFIDENCE: &str = "confidence.txt";
pub const PREVIOUS: &str = "previous.toml";
/// Every lint warning an in-session `ds check` has shown the agent, one JSON
/// string a line: what the post-session validator may treat as already seen.
pub const SEEN_WARNINGS: &str = "ds-check-seen.jsonl";

/// Record `warnings` as shown to the agent by `ds check` in the sandbox `dir`.
pub fn record_seen_warnings(dir: &Path, warnings: &[String]) {
    use std::io::Write as _;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(SEEN_WARNINGS))
    else {
        return;
    };
    for w in warnings {
        let _ = writeln!(file, "{}", Value::String(w.clone()));
    }
}

/// The warnings `ds check` has shown the agent in `dir` since [`forget_seen_warnings`].
#[must_use]
pub fn seen_warnings(dir: &Path) -> std::collections::BTreeSet<String> {
    let text = std::fs::read_to_string(dir.join(SEEN_WARNINGS)).unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<String>(l).ok())
        .collect()
}

/// Start a tier with nothing seen: its agent has not been shown what an earlier one was.
pub fn forget_seen_warnings(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(SEEN_WARNINGS));
}

/// Told to the agent that takes over from one stopped at its context budget
/// before it wrote `model.toml` (a stopped agent that did write one has that
/// draft validated and fixed instead).
pub const CONTINUE_AFTER_STOP: &str = "\n\nThe previous agent ran out of context before it \
    wrote model.toml. Do not repeat its reading: take what you can from the notes, open a page \
    only for what they lack, and write the whole table to model.toml early, then refine it with \
    Edit.";

/// Appended to a validation failure in a staged run.
///
/// `model.toml` already holds the failed draft: rewriting or re-reading the
/// whole card cost an agent 15-30k of its small context, a targeted edit a few
/// hundred tokens.
pub const EDIT_IN_PLACE: &str = "model.toml holds that draft. Fix exactly those issues with small \
    Edit calls: find the lines with `grep -n`, read only them (Read with offset and limit), and \
    never read or rewrite the whole file. Then run `hauksbee models lint model.toml` and repeat \
    until it is clean.";

/// An agent run stopped by its stream watcher (its context passed the budget).
#[derive(Debug, Clone)]
pub struct Stopped(pub String);

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped: {}", self.0)
    }
}

impl std::error::Error for Stopped {}

/// Reads an agent's stdout line by line while it runs; `Some(reason)` stops it.
pub type Watch = Box<dyn FnMut(&str) -> Option<String> + Send>;

/// The watcher for one run: activity events and the context budget.
///
/// Events come from Claude Code's `stream-json` (and codex's `--json`) lines;
/// the budget (0 = none) is checked on every assistant turn, whose usage says
/// how full the context now is. `agent` names the sandbox, so parallel agents'
/// usage events can be told apart.
#[must_use]
pub fn watch(model: &str, agent: &str, budget: u64) -> Watch {
    let (model, agent) = (model.to_string(), agent.to_string());
    Box::new(move |line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v["type"] == "item.started" && v["item"]["type"] == "command_execution" {
            tool_event("shell", &json!({ "command": v["item"]["command"] }), &agent);
        }
        if v["type"] != "assistant" {
            return None;
        }
        for c in v["message"]["content"].as_array().into_iter().flatten() {
            if c["type"] == "tool_use" {
                tool_event(c["name"].as_str().unwrap_or("tool"), &c["input"], &agent);
            }
        }
        let u = &v["message"]["usage"];
        let keys = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ];
        let context: u64 = keys.iter().filter_map(|k| u[k].as_u64()).sum();
        if context > 0 {
            event(
                "usage",
                json!({ "model": model, "agent": agent, "context": context }),
            );
        }
        (budget > 0 && context > budget)
            .then(|| format!("{model} passed its {budget}-token context budget ({context} tokens)"))
    })
}

fn tool_event(name: &str, input: &Value, agent: &str) {
    let detail = ["file_path", "command", "pattern"]
        .iter()
        .find_map(|k| input[k].as_str())
        .unwrap_or_default();
    let base = detail.rsplit('/').next().unwrap_or(detail);
    let page = base
        .split_once("page-")
        .and_then(|(_, rest)| rest.split('.').next()?.parse::<usize>().ok());
    if name == "Read" {
        event(
            "read",
            json!({ "path": base, "page": page, "agent": agent }),
        );
    } else {
        let short: String = detail.chars().take(100).collect();
        event(
            "tool",
            json!({ "name": name, "detail": short, "agent": agent }),
        );
    }
}

/// The last `result` event of a Claude Code `stream-json` run.
#[must_use]
pub fn claude_result(stdout: &str) -> Option<Value> {
    let events = stdout.lines().rev();
    let mut parsed = events.filter_map(|l| serde_json::from_str::<Value>(l).ok());
    parsed.find(|e| e["type"] == "result")
}

/// `pages/page-NNN.txt` per page and `page-index.md` listing each page's topics
/// and first line, so a staged agent opens two pages, not forty.
pub fn write_pages<'a>(dir: &Path, pages: impl Iterator<Item = (usize, &'a str, Vec<&'a str>)>) {
    let mut index = String::from(
        "# Page index\n\nEach page's text is pages/page-NNN.txt; rendered pages are page-NNN.png.\n\n\
         | page | topics found | first line |\n|---|---|---|\n",
    );
    let _ = std::fs::create_dir_all(dir.join("pages"));
    for (n, text, topics) in pages {
        let _ = std::fs::write(dir.join(format!("pages/page-{n:03}.txt")), text);
        let first = text.lines().map(str::trim).find(|l| !l.is_empty());
        let first: String = first.unwrap_or_default().chars().take(70).collect();
        let first = first.replace('|', "/");
        let _ = writeln!(index, "| {n} | {} | {first} |", topics.join(", "));
    }
    let _ = std::fs::write(dir.join("page-index.md"), index);
}

/// Which pages exist in the sandbox, so an agent need not `ls` for them: every
/// page has a text file, and only some have a render.
#[must_use]
pub fn page_list(dir: &Path) -> String {
    let numbers = |sub: &Path, ext: &str| -> Vec<usize> {
        let entries = std::fs::read_dir(sub).into_iter().flatten().flatten();
        let mut found: Vec<usize> = entries
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let digits = name.strip_prefix("page-")?.strip_suffix(ext)?;
                digits.parse().ok()
            })
            .collect();
        found.sort_unstable();
        found
    };
    let texts = numbers(&dir.join("pages"), ".txt");
    let images = numbers(dir, ".png");
    if texts.is_empty() && images.is_empty() {
        return String::new();
    }
    let join = |v: &[usize]| {
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = String::from("\n\nPages, so you need not `ls`:");
    if let (Some(first), Some(last)) = (texts.first(), texts.last()) {
        let _ = write!(
            out,
            " text of pages {first}-{last} is in `pages/page-NNN.txt`;"
        );
    }
    if images.is_empty() {
        out.push_str(" no page is rendered yet.");
    } else {
        let _ = write!(
            out,
            " rendered `page-NNN.png` exist for pages {}.",
            join(&images)
        );
    }
    out
}

/// The first stage: find the facts and hand off compact notes. The instruction
/// is the base (the same for every part of a kind), the part and kind the tail.
#[must_use]
pub fn locate_parts(part: &str, kind: &str, needs: &str) -> Prompt {
    let base = format!(
        "You are the first of several short agents drafting a Hauksbee model card for the part \
         named under THE PART after these instructions. Your only job is to LOCATE the facts and \
         hand off compact notes; a later agent writes the card from them.\n\n\
         Files in your working directory: `page-index.md` lists every PDF page with the topics \
         found on it and its first line; `pages/page-NNN.txt` is each page's text; \
         `page-NNN.png` are renders of the most relevant pages. Your context is small: do NOT \
         open `datasheet.pdf` or `datasheet.txt` whole. When a page you need has no image, \
         render just that page: `pdftoppm -png -r 150 -f N -l N -singlefile datasheet.pdf page-NNN`. \
         Use only files in your working directory.\n\n\
         The card will need: {needs}\n\n\
         Write `{NOTES}` (under 150 lines) with exactly these sections:\n\
         ## Part and package: the orderable code(s) that are the part, the package each uses, \
         and which package the card should model.\n\
         ## Pin map: a table `pin | name | function` for that package, read off the pinout \
         image AND the pin-function table (say on which pages, and whether they agree).\n\
         ## Key pages: page numbers of the absolute maximum ratings, recommended operating \
         conditions, electrical characteristics, pin functions or package drawing, and any \
         function table or programming equation.\n\
         ## Key values: up to 40 lines `name = value unit (min/typ/max, conditions, page N)`.\n\
         ## Ambiguities: anything unresolved (package variants with different pinouts, an \
         unclear variant, a missing table).\n\
         End with one line `CONFIDENCE: high`, `medium` or `low` (low when the pin map or \
         package is ambiguous). Do not write model.toml."
    );
    Prompt::new(base, format!("\n\nTHE PART: {part} (kind `{kind}`)"))
}

/// What a drafting agent is told about the notes already in the sandbox:
/// for a staged draft they are its map; for an escalated tier they are a
/// cheaper pass's map, to verify rather than trust.
#[must_use]
pub fn handoff(dir: &Path, staged: bool, escalated_because: Option<&str>) -> String {
    let notes = std::fs::read_to_string(dir.join(NOTES)).unwrap_or_default();
    let notes: String = notes.chars().take(12_000).collect();
    let mut out = String::new();
    if let Some(why) = escalated_because {
        let _ = write!(
            out,
            "\n\n## Escalated\n\nA cheaper model already tried this part and was escalated \
             because: {why}. Do the extraction yourself; anything it left is a lead to check.",
        );
    }
    out += &page_list(dir);
    if !notes.trim().is_empty() {
        let trust = if staged {
            "Agents drafted model.toml from the datasheet first. Their notes are below, so do not \
             re-read them or `ls`. Open a page (page-NNN.png, or pages/page-NNN.txt where \
             there is no image) only for a value the notes lack or flag as doubtful: each image \
             costs about 5k tokens of your small context. Do not open datasheet.pdf or \
             datasheet.txt whole; this overrides the instruction above."
        } else {
            "Notes from the cheaper pass, a map only: verify the pin map and every value \
             against the pages yourself."
        };
        let _ = write!(out, "\n\n## Handoff notes ({NOTES})\n\n{trust}\n\n{notes}");
    }
    if staged {
        let _ = write!(
            out,
            "\n\nIf model.toml already exists it is the last draft (`{PREVIOUS}` is a copy), which \
             failed the check quoted below: do not read or rewrite it whole, fix exactly what \
             failed with small Edit calls, and re-read a page only if a fix needs a value. \
             Otherwise write model.toml as soon as you have the pin map and the required values, \
             then refine it with Edit: a stage that runs out of context keeps only what is on \
             disk."
        );
    }
    out
}

/// Asked of every tier of a ladder: the self-report the next tier is chosen on.
pub const CONFIDENCE_CLAUSE: &str = "\n\nFinally write `confidence.txt`: its first line one \
    word, `high`, `medium` or `low`; its second line why. Say `low` when the pin map or package \
    variant is ambiguous or a required value had to be abstained from for want of a readable \
    table; a cheaper model saying low hands the part to a stronger one, which is the right \
    outcome, not a failure.";

/// The tier's own confidence: (`high`/`medium`/`low`/`unknown`, its reason).
#[must_use]
pub fn confidence(dir: &Path) -> (String, String) {
    let text = std::fs::read_to_string(dir.join(CONFIDENCE)).unwrap_or_default();
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let word = lines.next().unwrap_or_default().to_ascii_lowercase();
    let level = ["high", "medium", "low"]
        .into_iter()
        .find(|l| word.starts_with(l));
    let why: String = lines.next().unwrap_or_default().chars().take(200).collect();
    (level.unwrap_or("unknown").to_string(), why)
}

/// Why a valid draft should still go to a stronger tier, if it should.
#[must_use]
pub fn doubt(dir: &Path, pins: Option<String>) -> Option<String> {
    if let Some(warning) = pins {
        return Some(format!("pin map unresolved: {warning}"));
    }
    let (level, why) = confidence(dir);
    (level == "low").then(|| format!("self-reported low confidence: {why}"))
}

/// The first line of an error, short enough for an event or a card comment.
#[must_use]
pub fn reason(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    let first = text.lines().next().unwrap_or_default();
    first.chars().take(240).collect()
}

/// Record and announce the tier that produced the answer, and stamp it on the card.
#[must_use]
pub fn finish(
    raw: &str,
    dir: &Path,
    who: (&str, &str),
    tier: (usize, usize),
    why: &[String],
) -> String {
    let (backend, model) = who;
    let (level, _) = confidence(dir);
    event(
        "final",
        json!({ "model": model, "tier": tier.0, "tiers": tier.1, "escalations": why, "confidence": level }),
    );
    let escalated = why.iter().fold(String::new(), |mut s, w| {
        let _ = write!(s, "; escalated: {w}");
        s
    });
    let line = format!(
        "{backend} {model} (tier {} of {}){escalated}",
        tier.0, tier.1
    );
    crate::extract_progress::say(&format!(
        "[model-extract] final tier: {line}; confidence {level}"
    ));
    let line = line.replace('\n', " ");
    // What the drafter was shown and given beyond the datasheet, so a card can
    // be traced to its worked examples and companion documents.
    let mut stamp = format!("# extracted by {line}\n");
    for note in [
        crate::card_templates::index_note(dir),
        crate::companion_docs::index_note(dir),
    ]
    .into_iter()
    .flatten()
    {
        let _ = writeln!(stamp, "# {}", note.replace('\n', " "));
    }
    format!("{stamp}{raw}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watcher_stops_a_run_past_its_budget_and_names_pages_read() {
        let mut w = watch("claude-haiku-5-5", "a", 1000);
        let turn = |ctx: u64| {
            json!({"type": "assistant", "message": {
                "content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "/w/page-004.png"}}],
                "usage": {"input_tokens": 2, "cache_read_input_tokens": ctx}}})
            .to_string()
        };
        assert_eq!(w("not json"), None);
        assert_eq!(w(&turn(500)), None);
        let stop = w(&turn(5000)).unwrap();
        assert!(
            stop.contains("1000-token context budget (5002 tokens)"),
            "{stop}"
        );
        assert!(
            watch("m", "a", 0)(&turn(5_000_000)).is_none(),
            "no budget, no stop"
        );
    }

    #[test]
    fn confidence_doubt_and_the_result_event_are_read_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(confidence(dir.path()).0, "unknown");
        assert_eq!(doubt(dir.path(), None), None);
        std::fs::write(
            dir.path().join(CONFIDENCE),
            "LOW.\nSOT-23 pinout differs by suffix\n",
        )
        .unwrap();
        assert_eq!(
            doubt(dir.path(), None).unwrap(),
            "self-reported low confidence: SOT-23 pinout differs by suffix"
        );
        let pins = doubt(dir.path(), Some("pad 7 unknown".into())).unwrap();
        assert!(pins.starts_with("pin map unresolved"));
        let out = "{\"type\":\"system\"}\n{\"type\":\"result\",\"result\":\"done\"}\n";
        assert_eq!(claude_result(out).unwrap()["result"], "done");
        let card = finish(
            "[[models]]\n",
            dir.path(),
            ("claude-code", "m"),
            (2, 3),
            &["x".into()],
        );
        assert!(card
            .starts_with("# extracted by claude-code m (tier 2 of 3); escalated: x\n[[models]]"));
    }

    #[test]
    fn the_page_index_names_every_page_and_its_topics() {
        let dir = tempfile::tempdir().unwrap();
        let pages = [
            (1, "  \nLM358 | dual op amp\nmore", vec![]),
            (2, "Pin Functions", vec!["pins"]),
        ];
        write_pages(dir.path(), pages.into_iter());
        let index = std::fs::read_to_string(dir.path().join("page-index.md")).unwrap();
        assert!(index.contains("| 1 |  | LM358 / dual op amp |") && index.contains("| 2 | pins |"));
        assert!(dir.path().join("pages/page-002.txt").is_file());
    }

    #[test]
    fn the_page_list_spares_an_agent_an_ls() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(page_list(dir.path()), "", "no pages, nothing to say");
        let pages = [(1, "a", vec![]), (2, "b", vec![]), (10, "c", vec![])];
        write_pages(dir.path(), pages.into_iter());
        std::fs::write(dir.path().join("page-010.png"), b"png").unwrap();
        std::fs::write(dir.path().join("page-002.png"), b"png").unwrap();
        std::fs::write(dir.path().join("datasheet.png"), b"not a page").unwrap();
        let list = page_list(dir.path());
        assert!(list.contains("text of pages 1-10"), "{list}");
        assert!(list.contains("for pages 2, 10."), "{list}");
        // The draft prompt carries it with the notes, so the agent opens neither.
        std::fs::write(dir.path().join(NOTES), "## Pin map\n1 VCC\n").unwrap();
        let prompt = handoff(dir.path(), true, None);
        assert!(prompt.contains("for pages 2, 10.") && prompt.contains("1 VCC"));
        assert!(prompt.contains("do not re-read them or `ls`"), "{prompt}");
    }

    #[test]
    fn a_staged_fix_edits_the_draft_in_place_not_rewrites_it() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = handoff(dir.path(), true, None);
        assert!(
            prompt.contains("do not read or rewrite it whole"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("write the whole corrected table"),
            "{prompt}"
        );
        assert!(!handoff(dir.path(), false, None).contains("rewrite it whole"));
        // A continuation after a stop with no draft on disk says so, and does not point at one.
        assert!(CONTINUE_AFTER_STOP.contains("before it wrote model.toml"));
        assert!(!CONTINUE_AFTER_STOP.contains("previous.toml"));
    }
}
