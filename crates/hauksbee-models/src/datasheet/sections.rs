//! The staged harness's first stage: a card drafted by section agents.
//!
//! One agent reading a whole datasheet, then writing the whole card, held 110k
//! tokens of context. Each section agent has a narrow brief and its own sandbox
//! copy: **identity** (id, kind, match, package, pin map) runs first and is
//! small; **ratings**, **electrical** (params, DC rows, description, behaviour) and
//! **switching** (timing rows) then run in parallel, each told the identity
//! agent's keys, so the package and pad roles are one decision. [`merge`] joins the fragments with no model call.
//! Keys two fragments write differently are dropped and reported, never picked
//! between; a fragment with no usable entry is reported too. Either sends the
//! card to a fix agent first; a clean merge goes straight to the validator.

use anyhow::{anyhow, bail, Result};
use std::path::Path;
use std::thread::ScopedJoinHandle;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use super::{
    behavioral_family_base_kind, behavioral_hint_for_kind, conventions_for_kind,
    digital_hint_for_kind, extract_toml_block, format_examples_for_kind, optional_params_for_kind,
    param_bounds_for_kind, pin_numbering, pin_roles_for_kind, rating_key_guide,
    required_params_for_kind, run_and_record, Workspace, ENVELOPE_RULES, EXTRACTION_RULES,
    PIN_ROLE_GUIDANCE, RATINGS_RULES, SOURCE_QUANTITY_GUIDANCE,
};
use crate::extract_config::Resolved;
use crate::extract_harness as harness;
use crate::extract_progress as progress;

/// How an agent's file ends, so an unfinished one is told from a finished one.
const FINISH: &str = " End `model.toml` with the line `# end`.";

/// The switching agent's output shape. Its tables hold about 200 rows; written
/// as full records they were 64 KB in one Write (about 55k tokens of context in
/// one turn, past the stop). A parameter's section, page, unit and shared
/// conditions are written once per block, and `basis` is cited by the merge.
const TABLE_SHAPE: &str = r#"OUTPUT SHAPE: write each parameter of a datasheet table as ONE `[[models.table]]` block, never as `[[models.source.characteristic]]` records, and add no `# Source:` comments (the merge writes each row's `basis` from `section`, `page`, `parameter` and the columns the row prints). The block holds what its rows share; a row holds the min/typ/max it prints (omit the others) and, in `conditions`, only what differs from the block's. A row may override a block key such as `unit`:
```toml
[[models]]
[[models.table]]
section = "Table 7.6"  # as the datasheet names it
page = 7
parameter = "tPLH propagation delay, A to B"
unit = "ns"
conditions = "VCCA = 1.8 V ± 0.15 V, over the operating temperature range, see Figure 1"
rows = [
  { conditions = "VCCB = 1.8 V ± 0.15 V", min = 3, max = 17.7 },
  { conditions = "VCCB = 2.5 V ± 0.2 V", min = 2.2, max = 10.3 },
]
[[models.table]]
section = "Table 7.6"
page = 7
parameter = "tPZH enable time, DIR to A"
unit = "ns"
conditions = "VCCA = 1.8 V ± 0.15 V, over the operating temperature range, see Figure 1"
rows = [{ conditions = "VCCB = 1.8 V ± 0.15 V", max = 24.4 }]
# end
```
Write one file per datasheet table, each in one Write straight from the page text: the first as `model.toml`, then `model-2.toml`, `model-3.toml`. Each is a `[[models]]` entry with that table's blocks. End every file but the last with the line `# more`, and the last with `# end`."#;

pub(super) const NAMES: [&str; 4] = ["identity", "ratings", "electrical", "switching"];

/// What the merge made of the section fragments.
#[derive(Debug)]
pub(super) struct Merged {
    pub card: String,
    /// One line per key two sections wrote differently (the key is not in `card`).
    pub conflicts: Vec<String>,
    /// Sections that wrote no usable `[[models]]` entry.
    pub missing: Vec<&'static str>,
    /// What the fragments leave undone: an agent's unfinished output, malformed
    /// compact rows, behaviour the card's own data call for.
    pub gaps: Vec<String>,
}

impl Merged {
    /// What a fix agent is told first; empty when the merge was clean.
    pub fn problems(&self) -> String {
        let missing = self
            .missing
            .iter()
            .map(|m| format!("the {m} section agent wrote nothing usable"));
        let gaps = self.conflicts.iter().chain(&self.gaps).cloned();
        let all: Vec<String> = missing.chain(gaps).collect();
        if all.is_empty() {
            return String::new();
        }
        format!(
            "\n\nmodel.toml was merged from section agents. Resolve these from the datasheet \
             first (a conflicting key or missing section is absent from model.toml):\n- {}",
            all.join("\n- ")
        )
    }
}

/// The values of `item`, comments and formatting aside, for comparison.
fn plain(item: &Item) -> Option<toml::Value> {
    let mut holder = Table::new();
    holder.insert("v", item.clone());
    // A table under a key prints as a `[v]` section the parse below rejects
    // here, so tables are compared as a document of their own.
    let text = match item {
        Item::Table(t) => DocumentMut::from(t.clone()).to_string(),
        _ => holder.to_string(),
    };
    let parsed = text.parse::<toml::Table>().ok()?;
    match item {
        Item::Table(_) => Some(toml::Value::Table(parsed)),
        _ => parsed.get("v").cloned(),
    }
}

/// Whether two items hold the same values; one that cannot be read is never the same.
fn same_values(a: &Item, b: &Item) -> bool {
    plain(a).is_some_and(|a| Some(a) == plain(b))
}

fn join_tables(dst: &mut Table, src: &Table, path: &str, who: (&str, &str), out: &mut Vec<String>) {
    for (key, theirs) in src {
        let at = if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        };
        let Some(ours) = dst.get_mut(key) else {
            // A key already dropped for a disagreement stays out.
            if !out.iter().any(|c| c.starts_with(&format!("{at}: "))) {
                dst.insert(key, theirs.clone());
            }
            continue;
        };
        match (&mut *ours, theirs) {
            (Item::Table(a), Item::Table(b)) => join_tables(a, b, &at, who, out),
            (Item::ArrayOfTables(a), Item::ArrayOfTables(b)) => {
                // Each row is read once; one that cannot be read is never a duplicate.
                let read = |x: &Table| plain(&Item::Table(x.clone()));
                let mut seen: Vec<toml::Value> = a.iter().filter_map(read).collect();
                for row in b {
                    let theirs = read(row);
                    if !theirs.as_ref().is_some_and(|t| seen.contains(t)) {
                        a.push(row.clone());
                        seen.extend(theirs);
                    }
                }
            }
            // Lists of plain values (`missing`, `implements`) are sets: their union.
            (Item::Value(Value::Array(a)), Item::Value(Value::Array(b)))
                if a.iter()
                    .chain(b.iter())
                    .all(|v| !v.is_array() && !v.is_inline_table()) =>
            {
                let text = |v: &Value| v.clone().decorated("", "").to_string();
                for v in b {
                    if !a.iter().any(|x| text(x) == text(v)) {
                        a.push(v.clone());
                    }
                }
            }
            (a, b) if same_values(a, b) => {}
            (a, b) => {
                let show = |i: &Item| i.to_string().trim().replace('\n', " ");
                let (a, b) = (show(a), show(b));
                out.push(format!("{at}: {} wrote {a}, {} wrote {b}", who.0, who.1));
                dst.remove(key);
            }
        }
    }
}

/// The columns a compact row prints, as its `basis` names them.
fn columns(row: &Table) -> String {
    let printed = ["min", "typ", "max"].into_iter();
    printed
        .filter(|c| row.contains_key(c))
        .collect::<Vec<_>>()
        .join("/")
}

/// The text of `key` in a table.
fn text(t: &dyn TableLike, key: &str) -> Option<String> {
    t.get(key).and_then(Item::as_str).map(str::to_string)
}

/// Expand the `[[models.table]]` blocks of an entry into the
/// `[[models.source.characteristic]]` rows they stand for: a block's keys other
/// than `section` and `page` are shared by its `rows`, a row's `conditions`
/// come before the block's, and a row's `basis` is cited from the block's
/// `section` and `page`, the row's parameter and the columns it prints. A
/// block or row that cannot be read goes into `problems`.
fn expand_tables(entry: &mut Table, problems: &mut Vec<String>) {
    let Some(Item::ArrayOfTables(blocks)) = entry.remove("table") else {
        return;
    };
    let mut made = ArrayOfTables::new();
    for (n, block) in blocks.iter().enumerate() {
        let Some(rows) = block.get("rows").and_then(Item::as_array) else {
            problems.push(format!("table block {}: it has no `rows` array", n + 1));
            continue;
        };
        let page = block.get("page").and_then(Item::as_integer);
        let cite = text(block, "section").map(|s| match page {
            Some(p) => format!("{s} (p. {p})"),
            None => s,
        });
        if cite.is_none() {
            problems.push(format!("table block {}: it names no `section`", n + 1));
        }
        for (m, row) in rows.iter().enumerate() {
            let mut out = Table::new();
            for (k, v) in block {
                if !["rows", "section", "page"].contains(&k) {
                    out.insert(k, v.clone());
                }
            }
            let row = row.as_inline_table();
            for (k, v) in row.into_iter().flatten() {
                out.insert(k, Item::Value(v.clone().decorated("", "")));
            }
            let (Some(row), Some(name)) = (row, text(&out, "parameter")) else {
                let at = format!("table block {}, row {}", n + 1, m + 1);
                problems.push(format!(
                    "{at}: it is no inline table, or has no `parameter`"
                ));
                continue;
            };
            let when = [text(row, "conditions"), text(block, "conditions")];
            let when = when.into_iter().flatten().collect::<Vec<_>>();
            out.insert("conditions", toml_edit::value(when.join(", ")));
            if !out.contains_key("basis") {
                let cited = [cite.clone(), Some(name), Some(columns(&out))];
                let cited = cited.into_iter().flatten().collect::<Vec<_>>();
                out.insert("basis", toml_edit::value(cited.join(", ")));
            }
            let rank = |k: &str| {
                let order = [
                    "parameter",
                    "min",
                    "typ",
                    "max",
                    "unit",
                    "conditions",
                    "basis",
                ];
                order.iter().position(|c| *c == k).unwrap_or(order.len())
            };
            out.sort_values_by(|a, _, b, _| rank(a).cmp(&rank(b)));
            made.push(out);
        }
    }
    let source = entry.entry("source").or_insert(Item::Table(Table::new()));
    let rows = source.as_table_like_mut().map(|s| {
        s.entry("characteristic")
            .or_insert(ArrayOfTables::new().into())
    });
    if let Some(Item::ArrayOfTables(rows)) = rows {
        for row in made {
            rows.push(row);
        }
    }
}

/// The one `[[models]]` entry a fragment holds, its compact tables expanded
/// (see [`expand_tables`]). An abstention's `identity_only`,
/// `warning` and `unlocked_by` belong under `[models.params]`; one written at the
/// top of the entry is moved there rather than sent to a fix agent.
fn entry(raw: &str, problems: &mut Vec<String>) -> Option<Table> {
    let text = extract_toml_block(raw, "[[models]]");
    let doc = text.parse::<DocumentMut>().ok()?;
    let models = doc.get("models")?.as_array_of_tables()?;
    let mut table = (models.len() == 1).then(|| models.get(0).cloned())??;
    for key in ["identity_only", "warning", "unlocked_by"] {
        if let Some(moved) = table.remove(key) {
            let params = table.entry("params").or_insert(Item::Table(Table::new()));
            params.as_table_like_mut()?.insert(key, moved);
        }
    }
    expand_tables(&mut table, problems);
    Some(table)
}

fn document(card: Table) -> String {
    let mut models = ArrayOfTables::new();
    models.push(card);
    let mut doc = DocumentMut::new();
    doc.insert("models", Item::ArrayOfTables(models));
    doc.to_string()
}

/// Join the sections' fragments (`None` = the agent wrote none) into one card.
///
/// # Errors
///
/// Fails when no section wrote a usable `[[models]]` entry.
pub(super) fn merge(sections: &[(&'static str, Option<String>)]) -> Result<Merged> {
    let (mut card, mut conflicts, mut missing) = (Table::new(), Vec::new(), Vec::new());
    let (mut merged, mut gaps): (Vec<&str>, Vec<String>) = (Vec::new(), Vec::new());
    for (name, raw) in sections {
        let Some(fragment) = raw.as_deref().and_then(|r| entry(r, &mut gaps)) else {
            missing.push(*name);
            continue;
        };
        let who = (merged.join("+"), *name);
        join_tables(&mut card, &fragment, "", (&who.0, who.1), &mut conflicts);
        merged.push(name);
    }
    if merged.is_empty() {
        bail!("no section agent wrote a usable [[models]] entry");
    }
    gaps.extend(unmodelled_levels(&card));
    Ok(Merged {
        card: document(card),
        conflicts,
        missing,
        gaps,
    })
}

/// A digital card whose fragments carry I/O level characteristics (VOH, VOL,
/// VIH, VIL as params or rows) but no executable behaviour and an
/// `identity_only` abstention: a card that lints but cannot simulate. The
/// behaviour is a missing section, not a settled abstention. It is not built
/// here: which pins are inputs, outputs, enabled or sensed is the function
/// table's, and a guess would be a wrong model.
fn unmodelled_levels(card: &Table) -> Option<String> {
    const LEVELS: [&str; 4] = ["voh", "vol", "vih", "vil"];
    let digital = card.get("kind").and_then(Item::as_str) == Some("digital");
    let params = card.get("params").and_then(Item::as_table_like)?;
    let abstains = params.get("identity_only").and_then(Item::as_bool) == Some(true);
    let behaves = ["logic", "behavioral", "peripheral", "current_program"];
    if !digital || !abstains || behaves.iter().any(|k| card.contains_key(k)) {
        return None;
    }
    let rows = card.get("source").and_then(|s| s.get("characteristic"));
    let rows = rows
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flatten();
    let level = |p: &str| {
        p.split(|c: char| !c.is_alphanumeric())
            .next()
            .is_some_and(|w| LEVELS.contains(&w.to_lowercase().as_str()))
    };
    let row_levels = rows.filter_map(|r| r.get("parameter")?.as_str()).any(level);
    (LEVELS.iter().any(|k| params.contains_key(k)) || row_levels).then(|| {
        "the card abstains from behaviour (`identity_only`) with no `[models.logic]` although it \
         carries I/O level characteristics (voh/vol/vih/vil): write the behaviour from the \
         datasheet's function table (inputs, outputs, tristate, sense) and drop the abstention, \
         or keep it and state in `warning` what the function table cannot express"
            .to_string()
    })
}

/// The brief of one section agent; `known` is what the identity agent wrote.
fn prompt(part: &str, kind: &str, section: &str, known: &str) -> String {
    let base = behavioral_family_base_kind(kind).unwrap_or(kind);
    let (lower, re) = (part.to_lowercase(), regex::escape(&part.to_uppercase()));
    let roles = format!(
        "PIN ROLE NAMES for kind=\"{base}\": {}",
        pin_roles_for_kind(kind)
    );
    let (owns, topics, images, guide) = match section {
        "identity" => (
            format!(
                "`id = \"{lower}\"`, `kind = \"{base}\"`, `[models.match]` (`value_re = '(?i)^{re}'`: {}), \
                 `[models.pins]` and `[[models.source.pinout]]`",
                crate::validation::MATCH_PATTERN_RULE
            ),
            "pin functions or package pinout, ordering or package information",
            "",
            format!(
                "{roles}\n\n{PIN_ROLE_GUIDANCE}\n\n{}\n\nMap the package the \
                 orderable part number names, else split by numbering (P4 SPLIT). Name each \
                 `package` with its pin count as printed (`SOT-23-6 (DBV)`: a bare `SOT-23` \
                 reads as 3 pins).{}",
                pin_numbering(),
                harness::CONFIDENCE_CLAUSE
            ),
        ),
        "ratings" => (
            "`[models.ratings]`, `[[models.envelope]]` and a `[[models.source.characteristic]]` \
             per absolute-maximum, thermal and recommended-operating row that has no key"
                .to_string(),
            "absolute maximum ratings, recommended operating conditions, and the thermal table",
            " At most two images.",
            format!(
                "{roles}\nName an envelope pin by a role of the identity agent's `[models.pins]`. \
                 Ratings are for its package.\n\n{RATINGS_RULES}\n{}\n\n{ENVELOPE_RULES}\n\n{EXTRACTION_RULES}",
                rating_key_guide(kind)
            ),
        ),
        "switching" => (
            "`[[models.table]]` blocks holding every row of the switching-characteristics \
             tables (propagation, enable, disable, transition and skew times, per supply pairing \
             they print) and of the capacitance tables"
                .to_string(),
            "switching characteristics and capacitance",
            " At most two images.",
            format!(
                "{TABLE_SHAPE}\n\n{SOURCE_QUANTITY_GUIDANCE}\n\n{}",
                crate::schema::PRINTED_REFERENCE
            ),
        ),
        _ => (
            "`description`, `[models.params]`, `[models.coverage]`, every behaviour table the kind \
             needs (`[models.logic]` for a digital part) and a \
             `[[models.source.characteristic]]` per row of the DC electrical-characteristics \
             table (the switching table's rows belong to the switching agent)"
                .to_string(),
            "electrical characteristics, and the switching characteristics for the params",
            " At most two images.",
            format!(
                "`description`: one line naming the part and its package, then `does not model ...` \
                 for each behaviour the params leave out (rule 4). A param that names a pad \
                 (`supply_pin`, `gnd_pin`) takes its pad number from the identity agent's \
                 `[models.pins]`: the package it chose is the card's, so a part number that names \
                 none is no reason for `identity_only`; behaviour you cannot model is. \
                 `identity_only`, `warning` and `unlocked_by` go under `[models.params]`.\n\n\
                 For kind=\"{base}\", the required params are:\n{}{}\n{}\n{}\n\n{SOURCE_QUANTITY_GUIDANCE}\n\n\
                 {EXTRACTION_RULES}\n{}\n{}",
                required_params_for_kind(kind),
                conventions_for_kind(kind),
                optional_params_for_kind(kind),
                digital_hint_for_kind(kind),
                param_bounds_for_kind(kind),
                behavioral_hint_for_kind(kind),
            ),
        ),
    };
    let known = if known.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nThe identity agent wrote these keys (do not repeat them):\n```toml\n{known}```"
        )
    };
    // The switching agent's rows are compact: a full-card example would teach the long shape.
    let examples = format_examples_for_kind(kind);
    let example = examples.first().filter(|_| section != "switching");
    let example = example.map_or_else(String::new, |e| {
        format!(
            "FORMAT EXAMPLE ONLY, a different part ({}); copy its shape, never its numbers:\n{}",
            e.part, e.card
        )
    });
    let finish = if section == "switching" { "" } else { FINISH };
    format!(
        "You are one of the agents drafting the Hauksbee model card for {part} (kind `{kind}`). \
         YOUR SECTION: {section}. Write `model.toml` as a `[[models]]` entry holding ONLY these \
         keys: {owns}. Leave every other key to the other agents. Do not run `hauksbee models \
         lint`: a fragment fails it.{finish}\n\n\
         Your context is small. Start from `page-index.md` and open only the pages on {topics}: \
         `pages/page-NNN.txt` first, `page-NNN.png` (about 5k tokens each) only for a table whose \
         text columns are scrambled.{images} Never open `datasheet.pdf` or `datasheet.txt` whole. \
         Omit what the datasheet does not state.{known}\n\n{guide}\n\n{example}"
    )
}

/// Draft the card: identity first, then every other section at once, each
/// agent in its own sandbox copy bounded to `budget` tokens of context.
///
/// # Errors
///
/// Fails when no sandbox copy can be made or no agent wrote a usable fragment.
pub(super) fn draft(
    resolved: &Resolved,
    ws: &Workspace,
    (part, kind): (&str, &str),
    budget: u64,
    billing: crate::extract_usage::Billing,
) -> Result<Merged> {
    let run = |name: &'static str, known: &str| -> Result<Wrote> {
        let fork = ws.fork()?;
        let at = serde_json::json!({"stage": format!("section {name}"), "agent": fork.label(), "attempt": 1, "attempts": 1});
        progress::event("stage", at);
        let brief = prompt(part, kind, name, known) + &harness::page_list(fork.path());
        // A stopped or failed agent keeps whatever it already wrote.
        if let Err(e) = run_and_record(resolved, &brief, &fork, budget, billing) {
            say!("[model-extract] the {name} section agent ended early: {e:#}");
        }
        let files = files_written(fork.path());
        // The pin-map agent's self-reported confidence is the card's.
        let said = std::fs::read(fork.path().join(harness::CONFIDENCE));
        if let (Ok(said), true) = (said, name == NAMES[0]) {
            std::fs::write(ws.path().join(harness::CONFIDENCE), said)?;
        }
        // A saved fragment replays the merge and the briefs for free.
        if let Some(dir) = std::env::var_os("HAUKSBEE_SECTION_DUMP") {
            for (n, text) in &files {
                let file = format!("{name}-{n}.toml");
                let _ = std::fs::write(std::path::Path::new(&dir).join(file), text);
            }
        }
        Ok(Wrote {
            finished: finished(&files),
            parts: files.into_iter().map(|f| f.1).collect(),
        })
    };
    let identity = run(NAMES[0], "")?;
    let known = identity
        .parts
        .first()
        .and_then(|t| entry(t, &mut Vec::new()));
    let known = known.map(document).unwrap_or_default();
    let drafted = std::thread::scope(|scope| {
        let mut agents = Vec::new();
        for name in &NAMES[1..] {
            let label = crate::extract_progress::label();
            agents.push(scope.spawn(|| {
                crate::extract_progress::set_label(label);
                run(name, &known)
            }));
        }
        agents
            .into_iter()
            .map(ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    let mut wrote = vec![(NAMES[0], identity)];
    for (name, w) in NAMES[1..].iter().zip(drafted) {
        wrote.push((*name, w.map_err(|_| anyhow!("a section agent panicked"))??));
    }
    let (mut sections, mut unfinished) = (Vec::new(), Vec::new());
    for (name, w) in wrote {
        if w.parts.is_empty() {
            sections.push((name, None));
        } else if !w.finished {
            unfinished.push(format!(
                "the {name} section agent's output is unfinished (it stopped, or its last file does \
                 not end with `# end`): rows or keys of its section may be missing"
            ));
        }
        sections.extend(w.parts.into_iter().map(|t| (name, Some(t))));
    }
    let mut merged = merge(&sections)?;
    merged.gaps.extend(unfinished);
    Ok(merged)
}

/// What one section agent left behind.
struct Wrote {
    /// Its `[[models]]` files, in order.
    parts: Vec<String>,
    /// Whether the last file ends with the closing line.
    finished: bool,
}

/// The files an agent wrote in its sandbox, numbered: `model.toml` is 1 and
/// `model-N.toml` is N.
fn files_written(dir: &Path) -> Vec<(u32, String)> {
    let mut files: Vec<(u32, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| {
            let name = f.file_name().to_string_lossy().into_owned();
            let n = match name.as_str() {
                "model.toml" => 1,
                _ => name
                    .strip_prefix("model-")?
                    .strip_suffix(".toml")?
                    .parse()
                    .ok()?,
            };
            Some((n, std::fs::read_to_string(f.path()).ok()?))
        })
        .collect();
    files.sort_unstable();
    files
}

/// Whether numbered files are a whole fragment: no file missing between the
/// first and the last, and the last ends with the line `# end` (a closing
/// code fence after it is allowed). An agent stopped midway has not written it.
fn finished(files: &[(u32, String)]) -> bool {
    let whole = files.iter().zip(1..).all(|((n, _), k)| *n == k);
    let last = files.last().and_then(|f| {
        let lines = f.1.lines().map(str::trim);
        lines.rev().find(|l| !l.is_empty() && !l.starts_with("```"))
    });
    whole && last == Some("# end")
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: &str =
        "[[models]]\nid = \"x1\"\nkind = \"digital\"\n[models.match]\nvalue_re = '^X1$'\n\
        [models.pins]\n\"1\" = \"vcca\" # pin table\n\"2\" = \"gnd\"\n";
    const RATINGS: &str = "```toml\n[[models]]\n[models.ratings]\nmax_voltage_v = 6.5 # Source: AMR\n\
        [[models.envelope]]\nkind = \"supply_range\"\npin = \"vcca\"\nmin_v = 1.65\nmax_v = 5.5\nbasis = \"ROC\"\n```";
    const ELECTRICAL: &str =
        "[[models]]\nid = \"x1\"\ndescription = \"X1 does not model timing\"\n\
        [models.params]\nvoh = 2.4\n[models.coverage]\nmissing = [\"timing\"]\n";

    /// Rows two fragments both write under one key are all kept; only an
    /// identical row is written once.
    #[test]
    fn rows_under_a_shared_key_are_appended() {
        let rows = |tag: &str| {
            format!(
                "[[models]]\n[[models.source.characteristic]]\nparameter = \"{tag}\"\nmin = 1.0\n\
                 [[models.source.characteristic]]\nparameter = \"shared\"\nmin = 2.0\n"
            )
        };
        let merged = merge(&[("a", Some(rows("a"))), ("b", Some(rows("b")))]).unwrap();
        assert_eq!(
            merged
                .card
                .matches("[[models.source.characteristic]]")
                .count(),
            3
        );
        assert!(merged.problems().is_empty());
    }

    fn sections(parts: [Option<&str>; 3]) -> Vec<(&'static str, Option<String>)> {
        let parts = parts.map(|p| p.map(str::to_string));
        NAMES.iter().copied().zip(parts).collect()
    }

    fn card(m: &Merged) -> toml::Table {
        m.card.parse().unwrap()
    }

    #[test]
    fn disjoint_sections_join_into_one_card_with_comments_kept() {
        let m = merge(&sections([Some(IDENTITY), Some(RATINGS), Some(ELECTRICAL)])).unwrap();
        assert!(m.conflicts.is_empty() && m.missing.is_empty() && m.problems().is_empty());
        let entry = &card(&m)["models"][0];
        assert_eq!(entry["id"].as_str(), Some("x1"));
        assert_eq!(entry["pins"]["2"].as_str(), Some("gnd"));
        assert_eq!(entry["ratings"]["max_voltage_v"].as_float(), Some(6.5));
        assert_eq!(entry["envelope"].as_array().map(Vec::len), Some(1));
        assert_eq!(entry["params"]["voh"].as_float(), Some(2.4));
        assert!(m.card.contains("# pin table") && m.card.contains("# Source: AMR"));
    }

    #[test]
    fn keys_two_sections_agree_on_are_kept_and_lists_are_unioned() {
        let a = "[[models]]\nid = \"x1\"\n[models.coverage]\nmissing = [\"a\", \"b\"]\n\
            [[models.source.uncertainty]]\nparameter = \"model\"\n";
        let b = a.replace("\"a\", \"b\"", "\"b\", \"c\"");
        let m = merge(&sections([Some(a), Some(&b), None])).unwrap();
        assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
        let entry = &card(&m)["models"][0];
        assert_eq!(entry["coverage"]["missing"].as_array().unwrap().len(), 3);
        assert_eq!(entry["source"]["uncertainty"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn an_abstention_written_at_the_top_of_the_entry_moves_under_params() {
        let a = "[[models]]\nid = \"x1\"\nidentity_only = true\nwarning = \"w\"\n\
            [models.params]\nunlocked_by = \"u\"\n";
        let b = "[[models]]\n[models.params]\nidentity_only = true\n";
        let m = merge(&sections([Some(a), Some(b), None])).unwrap();
        let entry = &card(&m)["models"][0];
        assert!(entry.get("identity_only").is_none() && entry.get("warning").is_none());
        let params = entry["params"].as_table().unwrap();
        assert_eq!(params["identity_only"].as_bool(), Some(true));
        assert_eq!(params["warning"].as_str(), Some("w"));
        assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
    }

    #[test]
    fn a_disagreement_is_reported_and_neither_value_is_picked() {
        let other = "[[models]]\nid = \"x2\"\nkind = \"digital\"\n[models.pins]\n\"1\" = \"vcc\"\n\"2\" = \"gnd\"\n";
        let m = merge(&sections([Some(IDENTITY), Some(other), Some(ELECTRICAL)])).unwrap();
        let entry = &card(&m)["models"][0];
        assert!(entry.get("id").is_none(), "{}", m.card);
        assert!(entry["pins"].get("1").is_none() && entry["pins"]["2"].as_str() == Some("gnd"));
        assert_eq!(entry["kind"].as_str(), Some("digital"));
        assert_eq!(m.conflicts.len(), 2, "{:?}", m.conflicts);
        let first = "id: identity wrote \"x1\", ratings wrote \"x2\"";
        assert!(m.conflicts[0].starts_with(first), "{:?}", m.conflicts);
        assert!(m.conflicts[1].starts_with("pins.1:"), "{:?}", m.conflicts);
        assert!(m.problems().contains("- id: identity wrote"));
    }

    #[test]
    fn a_section_with_nothing_usable_is_reported_missing() {
        let two = "[[models]]\nid = \"a\"\n[[models]]\nid = \"b\"\n";
        let bad = [
            None,
            Some(""),
            Some("not toml ["),
            Some("id = \"x\"\n"),
            Some(two),
        ];
        for bad in bad {
            let m = merge(&sections([Some(IDENTITY), bad, Some(ELECTRICAL)])).unwrap();
            assert_eq!(m.missing, ["ratings"], "{bad:?}");
            assert!(m
                .problems()
                .contains("the ratings section agent wrote nothing usable"));
            assert!(card(&m)["models"][0].get("ratings").is_none());
        }
        let err = merge(&sections([None, None, Some("")])).unwrap_err();
        assert!(err.to_string().contains("no section agent"), "{err}");
    }

    #[test]
    fn each_brief_is_narrow_and_the_later_ones_carry_the_identity_keys() {
        let full = super::super::build_prompt("SN74LVC1T45", "digital", "");
        let known = document(entry(IDENTITY, &mut Vec::new()).unwrap());
        for name in NAMES {
            let brief = prompt("SN74LVC1T45", "digital", name, &known);
            assert!(brief.contains(&format!("YOUR SECTION: {name}")) && brief.len() < full.len());
            let own = name == "identity";
            assert_eq!(brief.contains("PIN NUMBERING"), own, "{name}");
            assert_eq!(brief.contains("confidence.txt"), own, "{name}");
        }
        let later = prompt("SN74LVC1T45", "digital", "electrical", &known);
        assert!(later.contains("do not repeat them") && later.contains("\"1\" = \"vcca\""));
        assert!(!prompt("X1", "digital", "ratings", "").contains("do not repeat them"));
        // The switching table's rows have one owner.
        assert!(later.contains("belong to the switching agent"));
        let timing = prompt("X1", "digital", "switching", &known);
        assert!(
            timing.contains("switching-characteristics")
                && !timing.contains("[models.logic]")
                && timing.contains("OUTPUT SHAPE")
                && timing.contains("`# more`")
        );
        // Every other agent ends its one file with the closing line.
        assert!(
            later.contains("End `model.toml` with the line `# end`")
                && !later.contains("OUTPUT SHAPE")
        );
    }

    /// Switching rows as the TI SN74LVC1T45 datasheet prints them (Section 5.6,
    /// VCCA = 1.8 V), in the compact shape, and a typical-only capacitance row.
    const TABLES: &str = r#"[[models]]
[[models.table]]
section = "Section 5.6"
page = 7
parameter = "tPLH propagation delay, A to B"
unit = "ns"
conditions = "VCCA = 1.8V ±0.15V, recommended operating free-air temperature range, see Figure 6-1"
rows = [
  { conditions = "VCCB = 1.8V ±0.15V", min = 3, max = 17.7 },
  { conditions = "VCCB = 2.5V ±0.2V", min = 2.2, max = 10.3 },
]
[[models.table]]
section = "Section 5.10"
page = 9
parameter = "CpdA power dissipation capacitance"
unit = "pF"
rows = [{ conditions = "VCCA = VCCB = 5V", typ = 21 }, { conditions = "VCCA = VCCB = 3.3V", typ = 20, unit = "nF" }]
# end
"#;

    fn characteristics(m: &Merged) -> Vec<crate::schema::Characteristic> {
        let rows = &card(m)["models"][0]["source"]["characteristic"];
        let row = |r: &toml::Value| r.clone().try_into().unwrap();
        rows.as_array().unwrap().iter().map(row).collect()
    }

    #[test]
    fn compact_tables_expand_to_the_rows_they_stand_for() {
        let m = merge(&sections([Some(IDENTITY), Some(TABLES), None])).unwrap();
        assert!(m.gaps.is_empty(), "{:?}", m.gaps);
        let rows = characteristics(&m);
        assert_eq!(rows.len(), 4);
        let at = &rows[1];
        assert_eq!((at.min, at.typ, at.max), (Some(2.2), None, Some(10.3)));
        assert_eq!(at.unit, "ns");
        assert_eq!(
            at.conditions,
            "VCCB = 2.5V ±0.2V, VCCA = 1.8V ±0.15V, recommended operating free-air temperature range, see Figure 6-1"
        );
        assert_eq!(
            at.basis,
            "Section 5.6 (p. 7), tPLH propagation delay, A to B, min/max"
        );
        // No shared conditions: the row's own; a row's key overrides the block's.
        assert_eq!(rows[2].conditions, "VCCA = VCCB = 5V");
        assert_eq!(
            rows[2].basis,
            "Section 5.10 (p. 9), CpdA power dissipation capacitance, typ"
        );
        assert_eq!((rows[2].unit.as_str(), rows[3].unit.as_str()), ("pF", "nF"));
        assert!(!m.card.contains("models.table"));
    }

    #[test]
    fn a_table_block_or_row_it_cannot_read_is_reported_not_dropped() {
        let bad = "[[models]]\n[[models.table]]\nsection = \"S\"\nparameter = \"a\"\nunit = \"V\"\n\
            [[models.table]]\nparameter = \"b\"\nunit = \"V\"\nrows = [{ min = 1 }, 3, { parameter = \"c\", max = 2 }]\n";
        let m = merge(&sections([Some(IDENTITY), Some(bad), Some(ELECTRICAL)])).unwrap();
        let said = m.problems();
        assert!(
            said.contains("table block 1: it has no `rows` array"),
            "{said}"
        );
        assert!(
            said.contains("table block 2: it names no `section`"),
            "{said}"
        );
        assert!(
            said.contains("table block 2, row 2: it is no inline table"),
            "{said}"
        );
        // The rows that read are kept, with no section to cite.
        let basis = characteristics(&m);
        assert_eq!(
            basis.iter().map(|r| r.basis.as_str()).collect::<Vec<_>>(),
            ["b, min", "c, max"]
        );
    }

    #[test]
    fn one_agents_files_join_and_an_unfinished_one_is_told_from_a_finished_one() {
        let rows = |tag: &str| {
            format!("[[models]]\n[[models.table]]\nsection = \"S\"\nparameter = \"{tag}\"\nunit = \"V\"\nrows = [{{ max = 1 }}]\n")
        };
        let parts = [Some(rows("a")), Some(rows("b"))].map(|t| ("switching", t));
        assert_eq!(characteristics(&merge(&parts).unwrap()).len(), 2);
        let file = |n: u32, tail: &str| (n, format!("[[models]]\n{tail}\n"));
        assert!(finished(&[file(1, "# end")]));
        assert!(finished(&[file(1, "# more"), file(2, "# end\n```\n")]));
        // Stopped between files, a file lost, nothing written, or a table cut short.
        assert!(!finished(&[file(1, "# more"), file(2, "# more")]));
        assert!(!finished(&[file(1, "# more"), file(3, "# end")]));
        assert!(!finished(&[]));
        assert!(!finished(&[(
            1,
            "[[models]]\n[[models.table]]\nsection = \"S\"\nrows = [".into()
        )]));
    }

    #[test]
    fn files_written_are_read_in_order_and_others_ignored() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("model-2.toml", "b"),
            ("model.toml", "a"),
            ("model-x.toml", "?"),
            ("notes.md", "n"),
        ] {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        assert_eq!(
            files_written(dir.path()),
            [(1, "a".to_string()), (2, "b".to_string())]
        );
    }

    /// The electrical agent of a real run abstained (`identity_only`, no
    /// `[models.logic]`) on a part whose rows give its I/O levels; its params
    /// as it wrote them (TI SN74LVC1T45, Tables 5.3 and 5.5).
    const ABSTAINED: &str = r#"[[models]]
description = "dual-supply 1-bit direction-controlled level translator"
[models.params]
identity_only = true
warning = "ABSTAIN: the [models.logic.sense] syntax is not confirmed here, so no [models.logic] table is written"
unlocked_by = "confirmed [models.logic.sense] schema for bidirectional pins A and B"
supply_pin = "1" # VCCA, pad 1
gnd_pin = "2" # GND, pad 2
voh = { min = 2.4, unit = "V", conditions = "IOH = -24 mA; VI = VIH; VCCA = 3 V; VCCB = 3 V", basis = "Table 5.5, VOH, MIN" }
vol = { max = 0.55, unit = "V", conditions = "IOL = 24 mA; VI = VIL; VCCA = 3 V; VCCB = 3 V", basis = "Table 5.5, VOL, MAX" }
"#;

    #[test]
    fn an_abstention_with_io_levels_and_no_behaviour_is_a_gap_for_the_fix_agent() {
        let run = |electrical: &str, kind: &str| {
            let identity = IDENTITY.replace("digital", kind);
            let m = merge(&sections([Some(&identity), None, Some(electrical)])).unwrap();
            (m.problems(), m.gaps.len())
        };
        let (said, gaps) = run(ABSTAINED, "digital");
        assert_eq!(gaps, 1);
        assert!(
            said.contains("abstains from behaviour") && said.contains("[models.logic]"),
            "{said}"
        );
        // Behaviour written, no abstention, no level data, or another kind: nothing to add.
        let logic = format!("{ABSTAINED}[models.logic]\ninputs = [\"a\"]\noutputs = [\"b\"]\n");
        assert_eq!(run(&logic, "digital").1, 0);
        assert_eq!(
            run(
                &ABSTAINED.replace("identity_only = true", "identity_only = false"),
                "digital"
            )
            .1,
            0
        );
        assert_eq!(run(ABSTAINED, "opamp").1, 0);
        let bare = "[[models]]\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n";
        assert_eq!(run(bare, "digital").1, 0);
        // A level printed only as a row counts too.
        let rows = format!("{bare}[[models.source.characteristic]]\nparameter = \"VOH high-level output voltage\"\nmin = 2.4\nunit = \"V\"\nconditions = \"c\"\nbasis = \"b\"\n");
        assert_eq!(run(&rows, "digital").1, 1);
    }

    /// Replay of recorded fragments for free: `HAUKSBEE_REPLAY_DIR` holds one
    /// agent's files (`model.toml`, `model-2.toml`, ...); the merged card is
    /// written beside them as `merged.toml`.
    #[test]
    #[ignore = "replays saved fragments from HAUKSBEE_REPLAY_DIR"]
    fn replay_saved_fragments() {
        let dir = std::path::PathBuf::from(std::env::var_os("HAUKSBEE_REPLAY_DIR").unwrap());
        let files = files_written(&dir);
        assert!(finished(&files), "unfinished");
        let parts: Vec<_> = files
            .into_iter()
            .map(|f| ("switching", Some(f.1)))
            .collect();
        let merged = merge(&parts).unwrap();
        assert!(merged.gaps.is_empty(), "{:?}", merged.gaps);
        std::fs::write(dir.join("merged.toml"), merged.card).unwrap();
    }

    /// The rating keys a prompt offers: its `  key  # meaning` guide lines.
    fn offered(prompt: &str) -> Vec<&'static str> {
        let guide_line = |k: &str, l: &str| {
            let rest = l.trim_start().strip_prefix(k);
            rest.is_some_and(|r| r.trim_start().starts_with('#'))
        };
        crate::schema::Ratings::keys()
            .filter(|k| prompt.lines().any(|l| guide_line(k, l)))
            .collect()
    }

    /// Whether validation accepts `key` stated on a card of `kind`.
    fn validation_accepts(kind: crate::schema::ComponentKind, key: &str) -> bool {
        let value = if key == "polarized" { "true" } else { "1.0" };
        let ratings: crate::schema::Ratings = toml::from_str(&format!("{key} = {value}")).unwrap();
        let entry = crate::schema::ModelEntry {
            kind,
            ratings,
            ..Default::default()
        };
        let errors = crate::validation::validate(&entry)
            .err()
            .unwrap_or_default();
        !errors
            .iter()
            .any(|e| e.message.contains(&format!("rating '{key}'")))
    }

    /// Prompts cannot ask for what validation rejects, and cannot omit what it
    /// wants: for every kind, the keys both prompts offer are exactly the keys
    /// validation accepts, and every key validation expects is offered.
    #[test]
    fn prompts_offer_exactly_the_rating_keys_validation_accepts() {
        for kind in crate::schema::ComponentKind::ALL {
            let name = serde_json::to_value(kind).unwrap();
            let name = name.as_str().unwrap();
            let full = super::super::build_prompt("X1", name, "");
            let ratings_brief = prompt("X1", name, "ratings", "");
            let accepted: Vec<&str> = crate::schema::Ratings::keys()
                .filter(|k| validation_accepts(kind, k))
                .collect();
            assert_eq!(offered(&full), accepted, "full prompt for {name}");
            assert_eq!(
                offered(&ratings_brief),
                accepted,
                "ratings brief for {name}"
            );
            for (key, _) in crate::validation::expected_ratings(kind) {
                assert!(
                    accepted.contains(key),
                    "{name} expects {key}, validation rejects it"
                );
            }
        }
    }

    #[test]
    fn one_wording_for_the_scalar_of_a_tagged_rating() {
        let brief = prompt("X1", "diode", "ratings", "");
        assert_eq!(
            brief.matches(standard_condition!()).count(),
            2,
            "KEY_GUIDE and RATINGS_RULES"
        );
    }
}
