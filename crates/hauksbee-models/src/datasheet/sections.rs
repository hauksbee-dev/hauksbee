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
use std::thread::ScopedJoinHandle;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use super::{
    behavioral_family_base_kind, behavioral_hint_for_kind, conventions_for_kind,
    digital_hint_for_kind, extract_toml_block, format_examples_for_kind, optional_params_for_kind,
    param_bounds_for_kind, pin_roles_for_kind, required_params_for_kind, run_and_record, Workspace,
    ENVELOPE_RULES, EXTRACTION_RULES, PIN_NUMBERING, PIN_ROLE_GUIDANCE, RATINGS_RULES,
    SOURCE_QUANTITY_GUIDANCE,
};
use crate::extract_config::Resolved;
use crate::extract_harness as harness;
use crate::extract_progress as progress;

pub(super) const NAMES: [&str; 4] = ["identity", "ratings", "electrical", "switching"];

/// What the merge made of the section fragments.
#[derive(Debug)]
pub(super) struct Merged {
    pub card: String,
    /// One line per key two sections wrote differently (the key is not in `card`).
    pub conflicts: Vec<String>,
    /// Sections that wrote no usable `[[models]]` entry.
    pub missing: Vec<&'static str>,
}

impl Merged {
    /// What a fix agent is told first; empty when the merge was clean.
    pub fn problems(&self) -> String {
        let missing = self
            .missing
            .iter()
            .map(|m| format!("the {m} section agent wrote nothing usable"));
        let all: Vec<String> = missing.chain(self.conflicts.iter().cloned()).collect();
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
                for row in b {
                    let same =
                        |x: &Table| same_values(&Item::Table(x.clone()), &Item::Table(row.clone()));
                    if !a.iter().any(same) {
                        a.push(row.clone());
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

/// The one `[[models]]` entry a fragment holds. An abstention's `identity_only`,
/// `warning` and `unlocked_by` belong under `[models.params]`; one written at the
/// top of the entry is moved there rather than sent to a fix agent.
fn entry(raw: &str) -> Option<Table> {
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
    let mut merged: Vec<&str> = Vec::new();
    for (name, raw) in sections {
        let Some(fragment) = raw.as_deref().and_then(entry) else {
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
    Ok(Merged {
        card: document(card),
        conflicts,
        missing,
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
                "`id = \"{lower}\"`, `kind = \"{base}\"`, `[models.match]` (`value_re = '(?i)^{re}$'`), \
                 `[models.pins]` and `[[models.source.pinout]]`"
            ),
            "pin functions or package pinout, ordering or package information",
            "",
            format!(
                "{roles}\n\n{PIN_ROLE_GUIDANCE}\n\n{PIN_NUMBERING}\n\nMap the package the \
                 orderable part number names. If it names none and the datasheet has several, \
                 take the one whose pad map the most packages share, else the first listed. Name \
                 each `package` with its pin count as printed (`SOT-23-6 (DBV)`: a bare `SOT-23` \
                 reads as 3 pins).{}",
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
                crate::schema::Ratings::KEY_GUIDE
            ),
        ),
        "switching" => (
            "a `[[models.source.characteristic]]` per row of the switching-characteristics table \
             (propagation, enable, disable, transition and skew times, per supply pairing it \
             prints) and per capacitance row"
                .to_string(),
            "switching characteristics and capacitance",
            " At most two images.",
            format!("{SOURCE_QUANTITY_GUIDANCE}\n\n{EXTRACTION_RULES}"),
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
    let example = format_examples_for_kind(kind).first().map_or_else(String::new, |e| {
        format!("FORMAT EXAMPLE ONLY, a different part ({}); copy its shape, never its numbers:\n{}", e.part, e.card)
    });
    format!(
        "You are one of the agents drafting the Hauksbee model card for {part} (kind `{kind}`). \
         YOUR SECTION: {section}. Write `model.toml` as a `[[models]]` entry holding ONLY these \
         keys: {owns}. Leave every other key to the other agents. Do not run `hauksbee models \
         lint`: a fragment fails it.\n\n\
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
    let run = |name: &'static str, known: &str| -> Result<Option<String>> {
        let fork = ws.fork()?;
        let at = serde_json::json!({"stage": format!("section {name}"), "agent": fork.label(), "attempt": 1, "attempts": 1});
        progress::event("stage", at);
        let brief = prompt(part, kind, name, known) + &harness::page_list(fork.path());
        // A stopped or failed agent keeps whatever it already wrote.
        if let Err(e) = run_and_record(resolved, &brief, &fork, budget, billing) {
            say!("[model-extract] the {name} section agent ended early: {e:#}");
        }
        let written = std::fs::read_to_string(fork.answer_path()).ok();
        // The pin-map agent's self-reported confidence is the card's.
        let said = std::fs::read(fork.path().join(harness::CONFIDENCE));
        if let (Ok(said), true) = (said, name == NAMES[0]) {
            std::fs::write(ws.path().join(harness::CONFIDENCE), said)?;
        }
        // A saved fragment replays the merge and the briefs for free.
        if let (Some(dir), Some(text)) = (std::env::var_os("HAUKSBEE_SECTION_DUMP"), &written) {
            let _ = std::fs::write(
                std::path::Path::new(&dir).join(format!("{name}.toml")),
                text,
            );
        }
        Ok(written)
    };
    let identity = run(NAMES[0], "")?;
    let known = identity.as_deref().and_then(entry).map(document);
    let known = known.unwrap_or_default();
    let drafted = std::thread::scope(|scope| {
        let mut agents = Vec::new();
        for name in &NAMES[1..] {
            agents.push(scope.spawn(|| run(name, &known)));
        }
        agents
            .into_iter()
            .map(ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    let mut sections = vec![(NAMES[0], identity)];
    for (name, text) in NAMES[1..].iter().zip(drafted) {
        let text = text.map_err(|_| anyhow!("a section agent panicked"))?;
        sections.push((*name, text?));
    }
    merge(&sections)
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
        let known = document(entry(IDENTITY).unwrap());
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
            timing.contains("switching-characteristics table")
                && !timing.contains("[models.logic]")
        );
    }
}
