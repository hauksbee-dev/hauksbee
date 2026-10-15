//! Tables in the docs that restate code tables are GENERATED from the code and
//! pinned by this test, so they cannot drift (single source of truth).
//!
//! Regenerate after changing a table with
//! `UPDATE_DOCS=1 cargo test -p hauksbee-models --test it docs_generated`.
//! A doc outside the checkout (a packaged crate) is skipped.

use hauksbee_models::chip_size::CHIP_SIZES;
use hauksbee_models::schema::{ComponentKind, Ratings};
use hauksbee_models::validation::rating_applies;
use std::fmt::Write as _;
use std::path::PathBuf;

fn doc(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs")
        .join(rel)
}

/// A fraction of a watt as the docs print it (`1/16 W`, `1 W`).
fn watts(w: f64) -> String {
    match (1..=32u32).find(|d| (w * f64::from(*d)).fract().abs() < 1e-9) {
        Some(1) | None => format!("{w} W"),
        Some(d) => format!("{}/{d} W", (w * f64::from(d)).round()),
    }
}

fn chip_tables() -> String {
    let mut s = String::from("| Chip size | theta_JA (C/W) |\n|-----------|----------------|\n");
    for c in CHIP_SIZES {
        if let Some(theta) = c.theta_ja_c_per_w {
            writeln!(s, "| {:<9} | {:<14} |", c.imperial, theta).unwrap();
        }
    }
    let rows: Vec<String> = CHIP_SIZES
        .iter()
        .map(|c| format!("{} (metric {}) {}", c.imperial, c.metric, watts(c.watts)))
        .collect();
    writeln!(
        s,
        "\nResistor rating by size: {}. A size with no theta_JA row has no derived figure.",
        rows.join(", ")
    )
    .unwrap();
    s
}

fn rating_keys() -> String {
    let mut s = String::from(
        "| Key | Meaning | Kinds it applies to |\n|-----|---------|---------------------|\n",
    );
    for line in Ratings::KEY_GUIDE.lines() {
        let (key, meaning) = line.split_once('#').expect("a `key # meaning` line");
        let key = key.trim();
        let kinds: Vec<String> = ComponentKind::ALL
            .into_iter()
            .filter(|k| *k != ComponentKind::Ignore && rating_applies(*k, key))
            .map(|k| {
                serde_json::to_value(k)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let applies = if kinds.len() == ComponentKind::ALL.len() - 1 {
            "every kind".to_string()
        } else {
            kinds.join(", ")
        };
        let meaning = meaning.trim().replace('|', "\\|");
        writeln!(s, "| `{key}` | {meaning} | {applies} |").unwrap();
    }
    s
}

/// Check (or with `UPDATE_DOCS`, rewrite) the region `name` of `rel`.
fn pinned(rel: &str, name: &str, body: &str) {
    let path = doc(rel);
    let Ok(mut text) = std::fs::read_to_string(&path) else {
        eprintln!("skipping {rel}: not in this checkout");
        return;
    };
    let begin = format!(
        "<!-- BEGIN GENERATED: {name} (source: crates/hauksbee-models/tests/suite/docs_generated.rs) -->"
    );
    let end = format!("<!-- END GENERATED: {name} -->");
    let want = format!("{begin}\n{body}{end}");
    let b = text
        .find(&begin)
        .unwrap_or_else(|| panic!("{rel}: add\n{begin}\n{end}"));
    let e = text.find(&end).unwrap_or_else(|| panic!("{rel}: no {end}")) + end.len();
    if text[b..e] == want {
        return;
    }
    assert!(
        std::env::var("UPDATE_DOCS").is_ok(),
        "{rel} region '{name}' is out of sync with the code. Regenerate with: \
         UPDATE_DOCS=1 cargo test -p hauksbee-models --test it docs_generated"
    );
    text.replace_range(b..e, &want);
    std::fs::write(&path, text).unwrap();
}

#[test]
fn docs_generated_chip_sizes() {
    pinned("checks/THERMAL.md", "chip-sizes", &chip_tables());
}

#[test]
fn docs_generated_rating_keys() {
    pinned("models/MODELS.md", "rating-keys", &rating_keys());
}
