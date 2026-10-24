//! The label table (`labels.rs`) and the copies of it: the generated region of
//! `frontend/src/lib/labels.ts`, and the `labels` block of
//! `schemas/hauksbee-vocabulary.json` (that file's own drift test is in
//! `run_record_schema_drift.rs`; `tests/labels.test.ts` in the frontend holds
//! the other direction, labels.ts against the JSON).
//!
//! Also holds the table to the ids the engine can emit, read from the Rust
//! definitions and the model cards, so a new capability, stage or emulator
//! backend cannot ship without words.
//!
//! Regenerate the generated region with the flag every generated contract of
//! this crate uses:
//!     `UPDATE_RUN_RECORD_SCHEMA=1` cargo test -p hauksbee-ir --test it drift

use std::collections::BTreeSet;
use std::path::PathBuf;

use hauksbee_ir::labels::{
    self, dropped_duplicates, entries, explain_for, label_for, GROUP_CAPABILITY, TS_BEGIN, TS_END,
};

const REGEN: &str = "UPDATE_RUN_RECORD_SCHEMA=1 cargo test -p hauksbee-ir --test it drift";

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo(rel))
        .unwrap_or_else(|e| panic!("{rel} is readable from the crate: {e}"))
        .replace("\r\n", "\n")
}

/// Everything between (and including) the two markers of a labels.ts, and the
/// text either side of it.
fn split_region(text: &str) -> (&str, &str, &str) {
    let start = text
        .find(TS_BEGIN)
        .unwrap_or_else(|| panic!("frontend/src/lib/labels.ts has no `{TS_BEGIN}` line"));
    let end_at = text
        .find(TS_END)
        .unwrap_or_else(|| panic!("frontend/src/lib/labels.ts has no `{TS_END}` line"));
    let end = end_at + TS_END.len() + 1;
    (&text[..start], &text[start..end], &text[end..])
}

#[test]
fn labels_ts_matches_the_table() {
    let rel = "frontend/src/lib/labels.ts";
    let text = read(rel);
    let (before, region, after) = split_region(&text);
    let generated = labels::typescript();
    if std::env::var_os("UPDATE_RUN_RECORD_SCHEMA").is_some() {
        std::fs::write(repo(rel), format!("{before}{generated}{after}")).unwrap();
        eprintln!("regenerated {rel}");
        return;
    }
    assert!(
        region == generated,
        "{rel} drifted from crates/hauksbee-ir/src/labels.rs (the table is edited there, the \
         TypeScript region is generated); regenerate with: {REGEN}"
    );
}

#[test]
fn every_id_has_one_row_and_plain_words() {
    let all = entries();
    let mut seen = BTreeSet::new();
    for e in &all {
        assert!(seen.insert(&e.id), "{} appears twice", e.id);
        assert!(!e.label.trim().is_empty(), "{} has no label", e.id);
        assert!(
            !e.label.contains('_'),
            "{}: label {:?} shows an underscore",
            e.id,
            e.label
        );
        if e.id.contains('_') || e.id.contains(':') {
            assert_ne!(e.label, e.id, "{}: the label is the raw id", e.id);
        }
        if let Some(x) = &e.explain {
            assert!(
                x.ends_with('.'),
                "{}: explanation {x:?} is not a sentence",
                e.id
            );
            assert!(
                !x.contains('_'),
                "{}: explanation {x:?} shows an underscore",
                e.id
            );
        }
    }
    // The documented overlaps: `pass` and `fail` are a verdict word and a
    // glossary term (the verdict row wins); a coverage stage and its glossary
    // term share an id (the stage row owns it, with the glossary's own words).
    assert_eq!(
        dropped_duplicates(),
        ["pass", "fail", "identity_only", "executable_partial"],
        "an id gained a second owner; give one of them a distinct id"
    );
}

#[test]
fn an_unknown_id_is_humanised_never_raw() {
    for id in ["some_new_capability", "x_y_z", "uart_tx_9"] {
        let said = label_for(id);
        assert!(!said.contains('_'), "{id} -> {said}");
        assert!(!said.is_empty());
    }
    assert_eq!(label_for("some_new_capability"), "Some new capability");
    assert_eq!(explain_for("some_new_capability"), None);
}

/// Quoted strings of every `<key> = [ ... ]` line in a model card file.
fn card_list(file: &str, key: &str) -> Vec<String> {
    let prefix = format!("{key} = [");
    read(&format!("crates/hauksbee-models/db/{file}"))
        .lines()
        .filter(|l| l.starts_with(&prefix))
        .flat_map(|l| {
            l.split('"')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn card_capabilities(file: &str) -> BTreeSet<String> {
    ["implements", "missing"]
        .iter()
        .flat_map(|k| card_list(file, k))
        .collect()
}

/// Every capability id an MCU card declares has words of its own: these are the
/// ids the inspector, the models table and the coverage text show for a chip.
#[test]
fn every_mcu_capability_id_has_a_row() {
    let ids = card_capabilities("mcu.toml");
    assert!(ids.len() > 50, "found only {} ids in mcu.toml", ids.len());
    let missing: Vec<&String> = ids
        .iter()
        .filter(|id| entries().iter().all(|e| &e.id != *id))
        .collect();
    assert!(
        missing.is_empty(),
        "capability ids in db/mcu.toml with no row in labels.rs: {missing:?}"
    );
}

/// Every generic MOSFET stand-in the card file declares (the family and its
/// corners) has a row: the binder names them in sentences about assumed
/// corners, and a reader never sees the raw id.
#[test]
fn every_generic_mosfet_model_has_a_row() {
    let ids: BTreeSet<String> = read("crates/hauksbee-models/db/mosfet.toml")
        .lines()
        .filter_map(|l| l.strip_prefix("id = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .filter(|id| id.starts_with("generic_") && id.contains("mos_") && !id.ends_with("_pkg"))
        .map(str::to_string)
        .collect();
    assert!(
        ids.len() >= 12,
        "found only {} generic MOSFET ids",
        ids.len()
    );
    let missing: Vec<&String> = ids
        .iter()
        .filter(|id| entries().iter().all(|e| &e.id != *id))
        .collect();
    assert!(
        missing.is_empty(),
        "generic MOSFET ids in db/mosfet.toml with no row: {missing:?}"
    );
}

/// No row describes a capability no card declares (a renamed id must not leave
/// its words behind), and the cards declare no id that humanises badly.
#[test]
fn capability_rows_match_the_cards() {
    let mut declared = BTreeSet::new();
    for file in std::fs::read_dir(repo("crates/hauksbee-models/db"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| {
            std::path::Path::new(n)
                .extension()
                .is_some_and(|x| x == "toml")
        })
    {
        declared.extend(card_capabilities(&file));
    }
    assert!(
        declared.len() > 500,
        "found only {} ids in the cards",
        declared.len()
    );
    for e in entries().iter().filter(|e| e.group == GROUP_CAPABILITY) {
        assert!(
            declared.contains(&e.id),
            "{} is in labels.rs but in no model card",
            e.id
        );
    }
    for id in &declared {
        let said = label_for(id);
        assert!(!said.contains('_') && !said.is_empty(), "{id} -> {said:?}");
        assert!(
            said.chars()
                .next()
                .is_some_and(|c| c.is_uppercase() || c.is_ascii_digit()),
            "{id} -> {said:?} does not start like a sentence"
        );
    }
}

/// The coverage stages are the engine's `CoverageStage` variants.
#[test]
fn coverage_stage_rows_are_the_engines_variants() {
    let src = read("crates/hauksbee-engine/src/commands/models.rs");
    let body = src
        .split("pub enum CoverageStage {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("models.rs defines CoverageStage");
    let snake = |name: &str| {
        let mut out = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        }
        out
    };
    let variants: Vec<String> = body
        .lines()
        .map(|l| l.trim().trim_end_matches(','))
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .map(snake)
        .collect();
    assert_eq!(variants.len(), 6, "{variants:?}");
    for id in &variants {
        let row = entries().into_iter().find(|e| &e.id == id);
        assert!(
            row.is_some_and(|e| e.group == labels::GROUP_COVERAGE_STAGE),
            "{id}"
        );
    }
}

/// Every emulator backend a card names or the engine embeds has a chip name.
#[test]
fn backend_rows_cover_the_cards_and_soc_descriptors() {
    let mut ids = BTreeSet::new();
    for file in [
        "mcu.toml",
        "mcu_boards.toml",
        "mcu_modules.toml",
        "unmodelled.toml",
    ] {
        for line in read(&format!("crates/hauksbee-models/db/{file}")).lines() {
            if let Some(rest) = line.trim_start().strip_prefix("backend = \"") {
                if let Some(id) = rest.split('"').next() {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    for line in read("crates/hauksbee-mcu/src/soc.rs").lines() {
        let t = line.trim_start();
        if t.starts_with("\"renode:") || t.starts_with("\"qemu:") {
            if let Some(id) = t[1..].split('"').next() {
                ids.insert(id.to_string());
            }
        }
    }
    ids.retain(|id| {
        id.split_once(':')
            .is_some_and(|(_, p)| !p.is_empty() && !p.contains('.'))
    });
    assert!(ids.len() > 15, "{ids:?}");
    for id in &ids {
        let row = entries().into_iter().find(|e| &e.id == id);
        assert!(
            row.is_some_and(|e| e.group == labels::GROUP_BACKEND),
            "backend {id} has no row in labels.rs"
        );
    }
}

/// Every kind the model schema accepts (`validation::KIND_NAMES`) has a row, so
/// the inspector's "bound model" never shows a raw kind.
#[test]
fn model_kind_rows_are_the_schemas_kinds() {
    let src = read("crates/hauksbee-models/src/validation.rs");
    let body = src
        .split("pub const KIND_NAMES: &[&str] = &[")
        .nth(1)
        .and_then(|rest| rest.split("];").next())
        .expect("validation.rs defines KIND_NAMES");
    let kinds: Vec<&str> = body.split('"').skip(1).step_by(2).collect();
    assert_eq!(kinds.len(), 18, "{kinds:?}");
    for kind in &kinds {
        let row = entries().into_iter().find(|e| e.id == *kind);
        assert!(
            row.is_some_and(|e| e.group == labels::GROUP_MODEL_KIND),
            "model kind {kind} has no row in labels.rs"
        );
        // The binder spells a kind from its Rust name, lower-cased.
        let compact = kind.replace('_', "");
        assert!(!label_for(&compact).contains('_'), "{compact}");
        assert_eq!(label_for(&compact), label_for(kind), "{compact}");
    }
}
