//! Provenance: every number a drafted card carries must be printed in the
//! text the drafter had.
//!
//! A cheaper drafter that is short of a row does not always say so. It writes
//! a plausible figure (a typical `θJA` from another package, a rounded supply
//! limit, a number remembered from a sibling part), and nothing in the schema
//! can tell it from a read one. This check can: a value read from a datasheet
//! is printed on a page of it, up to the unit scale the card stores it in.
//!
//! The rule, per numeric field of the card:
//!
//! * the value (sign ignored; datasheets print a negative limit as a magnitude
//!   in a column headed "min") appears among the numbers of a workspace page
//!   (the datasheet or a companion document fetched for it), after scaling by
//!   a power of ten that is a multiple of three (mA to A, µF to F, MHz to Hz)
//!   or ±2 (percent);
//! * a `supply_outputs` limit may instead be the nominal level less or plus a
//!   printed percentage or a printed deviation, since datasheets print the
//!   accuracy, not the limit;
//! * fields the harness computes (`param_names::derivations`, the junction
//!   limit convention), identifiers, pin maps, references and the match rules
//!   are not datasheet numbers and are skipped;
//! * a row whose `basis` or `conditions` says it is derived, computed,
//!   calculated or assumed documents its own origin and is skipped.
//!
//! Anything else is a [`Finding`] naming the field, worded as the fix.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::card_coverage::{Finding, Severity};
use crate::schema::ModelEntry;

/// Values of 0 and 1 are structural (a flag, a count, a ratio), not read.
const TRIVIAL: f64 = 1.0;

/// How close two figures must be to count as the same printed number.
const REL_TOL: f64 = 1e-6;

/// Powers of ten a card may scale a printed figure by.
const SCALES: [i32; 11] = [0, 3, -3, 6, -6, 9, -9, 12, -12, 2, -2];

/// Top-level keys that hold identity, bindings or pin maps, not datasheet numbers.
const SKIPPED_KEYS: &[&str] = &[
    "id",
    "description",
    "kind",
    "match",
    "pins",
    "passive_class",
    "variant",
    "variant_of",
    "pin_package",
];

/// Keys under `source` that are provenance themselves.
const SKIPPED_SOURCE: &[&str] = &["references", "pinout", "tier", "validation"];

/// Words in a row's `basis` or `conditions` that document a non-printed origin.
const DOCUMENTED: &[&str] = &["derived", "computed", "calculated", "assumed", "assumption"];

static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\d[\d,]*)?\.?\d+(?:[eE][-+]?\d+)?").expect("number pattern"));

/// One numeric field of a card and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// A path a person can find in the card: `params.iq_a`,
    /// `source.characteristic[1].max`.
    pub path: String,
    pub value: f64,
}

/// The magnitudes of every number the pages print, as sorted bit patterns.
#[derive(Debug, Clone, Default)]
pub struct PrintedNumbers(BTreeSet<u64>);

impl PrintedNumbers {
    /// Read every number in `pages`. A figure printed with a thousands comma
    /// (`1,000`) is held both ways; a leading-dot figure (`.5`) is held too.
    #[must_use]
    pub fn from_pages<S: AsRef<str>>(pages: &[S]) -> Self {
        let mut set = BTreeSet::new();
        for page in pages {
            for m in NUMBER.find_iter(page.as_ref()) {
                let text = m.as_str();
                if let Ok(v) = text.replace(',', "").parse::<f64>() {
                    set.insert(key(v));
                }
                if text.contains(',') {
                    // 1,5 printed with a decimal comma.
                    if let Ok(v) = text.replace(',', ".").parse::<f64>() {
                        set.insert(key(v));
                    }
                }
            }
        }
        Self(set)
    }

    /// Whether the pages print `v` at some allowed unit scale.
    #[must_use]
    pub fn has(&self, v: f64) -> bool {
        let v = v.abs();
        if !v.is_finite() {
            return true;
        }
        SCALES.iter().any(|&j| {
            let scaled = v * 10f64.powi(j);
            self.near(scaled)
        })
    }

    /// Whether a printed figure equals `x` to [`REL_TOL`].
    fn near(&self, x: f64) -> bool {
        if x == 0.0 {
            return self.0.contains(&key(0.0));
        }
        // Round to 6 significant digits and probe the neighbouring keys, since
        // the same figure prints as 3.3 and parses as 3.3 but scales to
        // 3.3000000000000003.
        let r = round_sig(x, 7);
        self.0.contains(&key(r))
            || self
                .0
                .range(key(x * (1.0 - REL_TOL))..=key(x * (1.0 + REL_TOL)))
                .next()
                .is_some()
    }

    /// The figures between `lo` and `hi` that are plausible tolerances.
    fn any_near(&self, x: f64) -> bool {
        SCALES.iter().any(|&j| self.near(x.abs() * 10f64.powi(j)))
    }
}

/// Positive floats order as their bit patterns do.
const fn key(v: f64) -> u64 {
    v.abs().to_bits()
}

fn round_sig(x: f64, digits: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let mag = x.abs().log10().floor();
    let p = 10f64.powf(f64::from(digits) - 1.0 - mag);
    (x * p).round() / p
}

/// Every datasheet-sourced numeric field of `entry`.
#[must_use]
pub fn card_numbers(entry: &ModelEntry) -> Vec<Field> {
    let Ok(Value::Object(root)) = serde_json::to_value(entry) else {
        return Vec::new();
    };
    let computed: BTreeSet<&str> = crate::param_names::derivations(entry)
        .into_iter()
        .map(|d| d.0)
        .collect();
    let mut out = Vec::new();
    for (k, v) in &root {
        if SKIPPED_KEYS.contains(&k.as_str()) {
            continue;
        }
        match (k.as_str(), v) {
            ("params", Value::Object(params)) => {
                for (name, value) in params {
                    if computed.contains(name.as_str()) {
                        continue;
                    }
                    walk(value, &format!("params.{name}"), &mut out);
                }
            }
            ("source", Value::Object(source)) => {
                for (name, value) in source {
                    if !SKIPPED_SOURCE.contains(&name.as_str()) {
                        walk(value, &format!("source.{name}"), &mut out);
                    }
                }
            }
            ("ratings", Value::Object(ratings)) => {
                let junction = crate::conventions::junction_limit(entry).is_some();
                for (name, value) in ratings {
                    if name == "max_junction_temp_c" && junction {
                        continue;
                    }
                    walk(value, &format!("ratings.{name}"), &mut out);
                }
            }
            _ => walk(v, k, &mut out),
        }
    }
    out
}

fn documented(obj: &serde_json::Map<String, Value>) -> bool {
    ["basis", "conditions", "reason"].iter().any(|k| {
        obj.get(*k).and_then(Value::as_str).is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            DOCUMENTED.iter().any(|w| s.contains(w))
        })
    })
}

fn walk(v: &Value, path: &str, out: &mut Vec<Field>) {
    match v {
        Value::Number(n) => {
            if let Some(x) = n.as_f64() {
                if x.abs() > TRIVIAL || (x != 0.0 && x.abs() < TRIVIAL) {
                    out.push(Field {
                        path: path.to_string(),
                        value: x,
                    });
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            if documented(map) {
                return;
            }
            for (k, item) in map {
                walk(item, &format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

/// The values of `entry` that no page prints, each with the field it is in.
///
/// A `supply_outputs` min or max passes when it is the nominal level moved by
/// a printed percentage or a printed deviation.
#[must_use]
pub fn unprinted(entry: &ModelEntry, printed: &PrintedNumbers) -> Vec<Field> {
    let mut out: Vec<Field> = card_numbers(entry)
        .into_iter()
        .filter(|f| !printed.has(f.value))
        .collect();
    out.retain(|f| !is_accuracy_limit(entry, f, printed));
    out
}

fn is_accuracy_limit(entry: &ModelEntry, f: &Field, printed: &PrintedNumbers) -> bool {
    let Some(rest) = f.path.strip_prefix("supply_outputs[") else {
        return false;
    };
    let Some((idx, field)) = rest.split_once("].") else {
        return false;
    };
    let Some(row) = idx
        .parse::<usize>()
        .ok()
        .and_then(|i| entry.supply_outputs.get(i))
    else {
        return false;
    };
    if !matches!(field, "min" | "max") || row.nominal == 0.0 {
        return false;
    }
    let delta = (f.value - row.nominal).abs();
    let percent = delta / row.nominal.abs() * 100.0;
    printed.any_near(delta) || printed.near(round_sig(percent, 6))
}

/// The provenance findings for `entry` against the workspace `pages`: one
/// warning per card, naming up to ten fields.
#[must_use]
pub fn findings<S: AsRef<str>>(entry: &ModelEntry, pages: &[S]) -> Vec<Finding> {
    if pages.iter().all(|p| p.as_ref().trim().is_empty()) {
        return Vec::new();
    }
    let printed = PrintedNumbers::from_pages(pages);
    let missing = unprinted(entry, &printed);
    if missing.is_empty() {
        return Vec::new();
    }
    let total = missing.len();
    let list: Vec<String> = missing
        .iter()
        .take(10)
        .map(|f| format!("{} = {}", f.path, f.value))
        .collect();
    vec![Finding {
        severity: Severity::Warn,
        check: "provenance",
        message: format!(
            "{total} number(s) in card '{}' are printed on no page the run holds (the datasheet \
             or a fetched companion document): {}{}. Each value is read from a row of the \
             datasheet, in the card's unit (mA to A, µF to F and percent are fine). `ds find` \
             the row and correct the value; if it is derived from printed rows, say so in the \
             row's `basis` (\"derived: ...\"); if the datasheet has no such row, remove the field \
             and add an `unknown` uncertainty record",
            entry.id,
            list.join("; "),
            if total > 10 { "; ..." } else { "" }
        ),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(toml_text: &str) -> ModelEntry {
        let db: crate::schema::DbFile = toml::from_str(toml_text).expect("toml");
        db.models.into_iter().next().expect("one")
    }

    const CARD: &str = r#"
[[models]]
id = "ldo"
kind = "vreg"
[models.match]
value_re = "(?i)^TLV70033$"
[models.pins]
1 = "in"
2 = "gnd"
3 = "out"
[models.params]
vout = 3.3
dropout_v = 0.238
iq_a = 25.0e-6
[models.ratings]
theta_ja_c_per_w = 235.9
max_voltage_v = 6.0
[[models.supply_outputs]]
role = "out"
nominal = 3.3
min = 3.267
max = 3.333
basis = "accuracy +-1%"
"#;

    const PAGE: &str = "Output voltage 3.3 V accuracy 1% at IOUT = 1 mA\n\
        Dropout VDO 238 mV at 200 mA\nIQ 25 µA typ\nRθJA DDC 235.9 °C/W\n\
        VIN absolute maximum −0.3 to 6 V";

    #[test]
    fn printed_values_pass_at_any_documented_scale() {
        let card = entry(CARD);
        let found = findings(&card, &[PAGE]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_unprinted_value_is_named_by_field() {
        let card = entry(&CARD.replace("235.9", "307.6"));
        let found = findings(&card, &[PAGE]);
        assert_eq!(found.len(), 1);
        let m = &found[0].message;
        assert!(m.contains("ratings.theta_ja_c_per_w = 307.6"), "{m}");
        assert!(!m.contains("dropout_v"), "{m}");
        assert_eq!(found[0].check, "provenance");
    }

    #[test]
    fn accuracy_limits_derive_from_a_printed_percentage() {
        let off = entry(&CARD.replace("3.333", "3.4"));
        let found = findings(&off, &[PAGE]);
        assert!(found[0].message.contains("supply_outputs[0].max"));
    }

    #[test]
    fn a_documented_derivation_and_a_blind_run_raise_nothing() {
        let derived = entry(&format!(
            "{CARD}\n[models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n\
             [[models.source.characteristic]]\nparameter = \"x\"\nmax = 77.7\nunit = \"V\"\n\
             conditions = \"c\"\nbasis = \"derived: 2 x 38.85\"\n"
        ));
        assert!(findings(&derived, &[PAGE]).is_empty());
        let blind: [&str; 0] = [];
        assert!(findings(&entry(&CARD.replace("235.9", "999.9")), &blind).is_empty());
    }

    #[test]
    fn thousands_commas_and_leading_dots_are_numbers() {
        let p = PrintedNumbers::from_pages(&["1,000 mA and .5 V"]);
        assert!(p.has(1.0e-3 * 1000.0));
        assert!(p.has(0.5));
        assert!(p.has(1000.0));
    }
}
