//! The staged harness's first stage: three section agents in parallel.
//!
//! One agent reading a whole datasheet, then writing the whole card, held 110k
//! tokens of context at its peak. Each section agent has a narrow brief and its
//! own sandbox copy, so its context holds only the pages its section needs:
//!
//! 1. **identity**: id, kind, match rule, package and pin map.
//! 2. **ratings**: absolute maximum ratings, recommended operating conditions.
//! 3. **electrical**: params, electrical-characteristic rows, description.
//!
//! Each writes a `[[models]]` fragment holding only its keys. [`merge`] joins
//! them with no model call. Keys more than one fragment writes must agree; a
//! disagreement is dropped from the card and reported, never resolved by picking
//! one. A section that wrote nothing usable is reported too. Both force a fix
//! agent to resolve them, and the usual lint and fix loop then runs on the card.

use anyhow::{bail, Result};
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

pub(super) const NAMES: [&str; 3] = ["identity", "ratings", "electrical"];

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
            "\n\nmodel.toml was merged from three section agents that did not see each other's \
             work. Resolve these from the datasheet first (a conflicting key is absent from \
             model.toml; a missing section is absent too):\n- {}",
            all.join("\n- ")
        )
    }
}

/// The values of `item`, comments and formatting aside, for comparison.
fn plain(item: &Item) -> Option<toml::Value> {
    let mut holder = Table::new();
    holder.insert("v", item.clone());
    holder.to_string().parse::<toml::Table>().ok()?.remove("v")
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
                    let same = |x: &Table| {
                        plain(&Item::Table(x.clone())) == plain(&Item::Table(row.clone()))
                    };
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
            (a, b) if plain(a) == plain(b) => {}
            (a, b) => {
                let show = |i: &Item| i.to_string().trim().replace('\n', " ");
                out.push(format!(
                    "{at}: {} wrote {}, {} wrote {}",
                    who.0,
                    show(a),
                    who.1,
                    show(b)
                ));
                dst.remove(key);
            }
        }
    }
}

/// Join the sections' fragments (`None` = the agent wrote none) into one card.
///
/// # Errors
///
/// Fails when no section wrote a usable `[[models]]` entry.
pub(super) fn merge(sections: &[(&'static str, Option<String>)]) -> Result<Merged> {
    let entry = |raw: &str| -> Option<Table> {
        let doc = extract_toml_block(raw, "[[models]]")
            .parse::<DocumentMut>()
            .ok()?;
        let models = doc.get("models")?.as_array_of_tables()?;
        (models.len() == 1).then(|| models.get(0).cloned())?
    };
    let (mut card, mut conflicts, mut missing) = (Table::new(), Vec::new(), Vec::new());
    let mut merged: Vec<&str> = Vec::new();
    for (name, raw) in sections {
        let Some(fragment) = raw.as_deref().and_then(entry) else {
            missing.push(*name);
            continue;
        };
        join_tables(
            &mut card,
            &fragment,
            "",
            (&merged.join("+"), name),
            &mut conflicts,
        );
        merged.push(name);
    }
    if merged.is_empty() {
        bail!("no section agent wrote a usable [[models]] entry");
    }
    let mut models = ArrayOfTables::new();
    models.push(card);
    let mut doc = DocumentMut::new();
    doc.insert("models", Item::ArrayOfTables(models));
    Ok(Merged {
        card: doc.to_string(),
        conflicts,
        missing,
    })
}

/// The brief of one section agent.
fn prompt(part: &str, kind: &str, section: &str) -> String {
    let base = behavioral_family_base_kind(kind).unwrap_or(kind);
    let (lower, re) = (part.to_lowercase(), regex::escape(&part.to_uppercase()));
    let roles = format!(
        "PIN ROLE NAMES for kind=\"{base}\": {}",
        pin_roles_for_kind(kind)
    );
    let (owns, guide) = match section {
        "identity" => (
            format!(
                "`id = \"{lower}\"`, `kind = \"{base}\"`, `[models.match]` (`value_re = '(?i)^{re}$'`), \
                 `[models.pins]` and `[[models.source.pinout]]`"
            ),
            format!(
                "{roles}\n\n{PIN_ROLE_GUIDANCE}\n\n{PIN_NUMBERING}\n\nWrite `[models.pins]` and the \
                 pinout for the package the orderable part number names.{}",
                harness::CONFIDENCE_CLAUSE
            ),
        ),
        "ratings" => (
            "`[models.ratings]`, `[[models.envelope]]` and a `[[models.source.characteristic]]` \
             per absolute-maximum, thermal and recommended-operating row that has no key"
                .to_string(),
            format!(
                "{roles}\nName an envelope pin by the lowercase role of the pin table's printed \
                 name; another agent writes the pin map with the same roles. Ratings are for the \
                 package the orderable part number names.\n\n{RATINGS_RULES}\n{}\n\n{ENVELOPE_RULES}\n\n{EXTRACTION_RULES}",
                crate::schema::Ratings::KEY_GUIDE
            ),
        ),
        _ => (
            "`description`, `[models.params]`, `[models.coverage]` and a \
             `[[models.source.characteristic]]` per electrical-characteristic row"
                .to_string(),
            format!(
                "`description`: one line naming the part and its package, then `does not model ...` \
                 for each behaviour the params leave out (rule 4). A param that names a pad \
                 (`supply_pin`, `gnd_pin`) takes the pad number the package's pin table prints. \
                 `identity_only`, `warning` and `unlocked_by` go under `[models.params]`, never at \
                 the top of the entry.\n\n\
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
    let example = format_examples_for_kind(kind).first().map_or_else(String::new, |e| {
        format!("FORMAT EXAMPLE ONLY, a different part ({}); copy its shape, never its numbers:\n{}", e.part, e.card)
    });
    format!(
        "You are one of three agents drafting the Hauksbee model card for {part} (kind `{kind}`) \
         in parallel; none sees the others' work. YOUR SECTION: {section}. Write `model.toml` as a \
         `[[models]]` entry holding ONLY these keys: {owns}. Leave every other key to the other \
         agents. Do not run `hauksbee models lint`: a fragment fails it.\n\n\
         Your context is small. Start from `page-index.md`, open `pages/page-NNN.txt` or \
         `page-NNN.png` only for the pages your section needs (about 5k tokens per image), and \
         never open `datasheet.pdf` or `datasheet.txt` whole. Omit what the datasheet does not \
         state.\n\n{guide}\n\n{example}"
    )
}

/// Run the three section agents at once, each in its own sandbox copy and
/// bounded to `budget` tokens of context, and merge what they wrote.
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
    let forks = NAMES
        .iter()
        .map(|_| ws.fork())
        .collect::<Result<Vec<_>>>()?;
    let written = std::thread::scope(|scope| {
        // Collected so every agent is spawned before the first is joined.
        #[allow(clippy::needless_collect)]
        let jobs: Vec<_> = NAMES
            .iter()
            .zip(&forks)
            .map(|(name, fork)| {
                scope.spawn(move || {
                    progress::event(
                        "stage",
                        serde_json::json!({"stage": format!("section {name}"), "agent": fork.label(), "attempt": 1, "attempts": 1}),
                    );
                    let brief = prompt(part, kind, name) + &harness::page_list(fork.path());
                    // A stopped or failed agent keeps whatever it already wrote.
                    if let Err(e) = run_and_record(resolved, &brief, fork, budget, billing) {
                        say!("[model-extract] the {name} section agent ended early: {e:#}");
                    }
                    std::fs::read_to_string(fork.answer_path()).ok()
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| j.join().ok().flatten())
            .collect::<Vec<_>>()
    });
    // The pin-map agent's self-reported confidence is the card's.
    let said = std::fs::read(forks[0].path().join(harness::CONFIDENCE));
    if let Ok(said) = said {
        std::fs::write(ws.path().join(harness::CONFIDENCE), said)?;
    }
    merge(&NAMES.iter().copied().zip(written).collect::<Vec<_>>())
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

    fn sections(parts: [Option<&str>; 3]) -> Vec<(&'static str, Option<String>)> {
        NAMES
            .iter()
            .copied()
            .zip(parts.map(|p| p.map(str::to_string)))
            .collect()
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
        assert!(
            m.card.contains("# pin table") && m.card.contains("# Source: AMR"),
            "{}",
            m.card
        );
    }

    #[test]
    fn keys_two_sections_agree_on_are_kept_and_lists_are_unioned() {
        let a = "[[models]]\nid = \"x1\"\n[models.coverage]\nmissing = [\"a\", \"b\"]\n\
            [[models.source.uncertainty]]\nparameter = \"model\"\n";
        let b = "[[models]]\nid = \"x1\"\n[models.coverage]\nmissing = [\"b\", \"c\"]\n\
            [[models.source.uncertainty]]\nparameter = \"model\"\n";
        let m = merge(&sections([Some(a), Some(b), None])).unwrap();
        assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
        let entry = &card(&m)["models"][0];
        assert_eq!(entry["coverage"]["missing"].as_array().unwrap().len(), 3);
        assert_eq!(entry["source"]["uncertainty"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_keys_table_stays_its_own_table_after_the_merge() {
        let a = "[[models]]\nid = \"x1\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\n";
        let m = merge(&sections([Some(a), Some(RATINGS), Some(IDENTITY)])).unwrap();
        eprintln!("{}", m.card);
        let entry = &card(&m)["models"][0];
        assert!(entry.get("identity_only").is_none(), "{}", m.card);
        assert_eq!(entry["params"]["identity_only"].as_bool(), Some(true));
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
        assert!(
            m.conflicts[0].starts_with("id: identity wrote \"x1\", ratings wrote \"x2\""),
            "{:?}",
            m.conflicts
        );
        assert!(m.conflicts[1].starts_with("pins.1:"), "{:?}", m.conflicts);
        assert!(m.problems().contains("- id: identity wrote"));
    }

    #[test]
    fn a_section_with_nothing_usable_is_reported_missing() {
        let two = "[[models]]\nid = \"a\"\n[[models]]\nid = \"b\"\n";
        for bad in [
            None,
            Some(""),
            Some("not toml ["),
            Some("id = \"x\"\n"),
            Some(two),
        ] {
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
    fn each_brief_is_narrow_and_names_only_its_own_section() {
        let full = super::super::build_prompt("SN74LVC1T45", "digital", "");
        for name in NAMES {
            let brief = prompt("SN74LVC1T45", "digital", name);
            assert!(brief.contains(&format!("YOUR SECTION: {name}")));
            assert!(brief.len() < full.len(), "{name}");
            assert_eq!(
                brief.contains("PIN NUMBERING"),
                name == "identity",
                "{name}"
            );
            assert_eq!(
                brief.contains("confidence.txt"),
                name == "identity",
                "{name}"
            );
        }
    }
}
