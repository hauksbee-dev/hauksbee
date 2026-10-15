//! Value plausibility: the wrong-value classes measured in extraction drafts,
//! each one rule with the prompt wording and the check it is held to.
//!
//! [`prompt_guide`] puts every rule's `guide` in the extraction prompt and
//! [`problems`] / [`raw_problems`] refuse a draft breaking one, quoting the same
//! guide, so the drafter is told exactly what the validator then holds it to.
//! Nothing here changes a value: a refused draft is redrafted.
//!
//! Measured cases (round-3 drafts): a "0.7 VDD" row written as 0.7 V; input
//! thresholds at a 1.8 V supply beside output levels at 3 V; an enable's max
//! column written as its nominal threshold; an EEPROM's write current as its
//! static draw; a buck's internal bias-LDO dropout as its `dropout_v`.

use std::sync::LazyLock;

use regex::Regex;

use crate::param_names::{cited_rows, has_symbol, row_column};
use crate::schema::{Characteristic, ComponentKind, ModelEntry, ParamValue};

/// A supply rail as rows and ratiometric limits name it (VDD, VCC, VDDIO, VS).
pub const RAIL: &str = r"(V(?:DD|CC|IO|S|L|REF|BAT)[A-Z0-9_]*)";

/// A fraction of a rail: `0.7 VDD`, `0.7 × VDDIO`, `70 % VCC`, `0.3 of VS`.
pub static FRACTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(\d*\.?\d+)\s*(%)?\s*(?:[×x*·]|of)?\s*{RAIL}\b"))
        .expect("a valid pattern")
});

/// The logic-level keys one supply corner must cover.
const LEVELS: [(&str, &str); 6] = [
    ("vih", "VIH"),
    ("vil", "VIL"),
    ("voh", "VOH"),
    ("vol", "VOL"),
    ("out_hi", "VOH"),
    ("out_lo", "VOL"),
];

/// One plausibility rule: its id, the prompt's wording, and its check.
pub struct Rule {
    pub id: &'static str,
    pub guide: &'static str,
    check: fn(&ModelEntry) -> Vec<String>,
}

/// Every rule, in prompt order.
pub const RULES: &[Rule] = &[
    Rule {
        id: "ratiometric",
        guide: "a level printed as a fraction of a supply (0.7 VDD, 70 % VCC, 0.3 × VDDIO) is no \
                volt value: never write its coefficient as volts in a param, a row column or a \
                param's comment; quote it in the row's `conditions` with that column omitted \
                (Hauksbee scales it by the supply the card's other levels are read at)",
        check: ratiometric,
    },
    Rule {
        id: "one_corner",
        guide: "read every logic level (vih, vil, voh, vol, out_lo, out_hi) at one supply \
                voltage: where rows are printed at several supplies, take each level from the \
                row at the same supply",
        check: one_corner,
    },
    Rule {
        id: "nominal_is_typ",
        guide: "a nominal (any key the column rule reads from typ: a threshold, setpoint, \
                reference or frequency, in [models.params] or a behavioural block) is its row's \
                typ when the row prints one, never its min or max column; only a key naming a \
                guaranteed bound (vih, vol, en_on_v, dropout_v, *_min, *_max) reads a bound",
        check: nominal_is_typ,
    },
    Rule {
        id: "static_supply",
        guide: "supply_static_ua is the quiescent draw with the bus and clocks idle (an \
                ICC/IDD row marked standby, static, quiescent, interface inactive, or at a \
                0 Hz clock); a row measured while reading, writing, programming, erasing, with \
                the interface active or at a running clock rate is a dynamic draw, never the \
                static one",
        check: static_supply,
    },
    Rule {
        id: "converter_dropout",
        guide: "a switching regulator ([models.behavioral.converter]) has no dropout_v: a \
                dropout row on its datasheet belongs to an internal bias regulator (VCC, \
                INTVCC, EXTVCC) and stays a characteristic record",
        check: converter_dropout,
    },
];

/// The VALUE PLAUSIBILITY block every model prompt carries.
#[must_use]
pub fn prompt_guide() -> String {
    let lines: Vec<String> = RULES
        .iter()
        .map(|r| format!("   - {}: {}", r.id, r.guide))
        .collect();
    format!(
        "   VALUE PLAUSIBILITY (the validator refuses a draft breaking one):\n{}",
        lines.join("\n")
    )
}

/// Every rule `entry` breaks, each with its guide.
#[must_use]
pub fn problems(entry: &ModelEntry) -> Vec<String> {
    let found = RULES.iter().flat_map(|rule| {
        (rule.check)(entry)
            .into_iter()
            .map(move |why| format!("{}: {why}; rule: {}", rule.id, rule.guide))
    });
    found.collect()
}

/// A param line of the raw reply whose own comment prints its value as a rail
/// fraction (`vih = 0.7 # VIH = 0.7 VCC`): the TOML parse drops comments, so
/// this reads the text.
#[must_use]
pub fn raw_problems(raw: &str) -> Vec<String> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*([a-z][a-z0-9_]*)\s*=\s*([-+]?\d*\.?\d+(?:[eE][-+]?\d+)?)\s*#(.*)$")
            .expect("a valid pattern")
    });
    let rule = &RULES[0];
    let lines = raw.lines().filter_map(|l| LINE.captures(l));
    let lines = lines.filter(|c| volts(&c[1]));
    let bad = lines.filter_map(|c| {
        let v: f64 = c[2].parse().ok()?;
        let (coefficient, rail) = fractions(&c[3]).into_iter().find(|f| same(f.0, v))?;
        Some(format!(
            "{}: `{} = {v}` writes the coefficient of '{coefficient} {rail}' its comment \
             quotes as volts; rule: {}",
            rule.id, &c[1], rule.guide
        ))
    });
    bad.collect()
}

/// Every rail fraction `text` prints, as (coefficient in 0..=1, rail).
pub fn fractions(text: &str) -> Vec<(f64, String)> {
    let found = FRACTION.captures_iter(text).filter_map(|c| {
        let f: f64 = c[1].parse().ok()?;
        let f = if c.get(2).is_some() { f / 100.0 } else { f };
        (f > 0.0 && f <= 1.0).then(|| (f, c[3].to_string()))
    });
    found.collect()
}

fn same(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 + 1e-6 * a.abs().max(b.abs())
}

/// A key's (scale, base unit): its suffix, else its glossary unit.
fn key_unit(key: &str) -> Option<(f64, &'static str)> {
    crate::units::suffix_unit(key)
        .or_else(|| crate::param_names::param_unit(key).map(|u| (1.0, u)))
        .filter(|(_, u)| !u.is_empty() && *u != "-")
}

/// `x` of row `r`, in units of `key` (none for a unit mismatch).
fn in_key_units(key: &str, r: &Characteristic, x: f64) -> Option<f64> {
    let (scale, unit) = key_unit(key)?;
    Some(x * crate::units::scale_between(&r.unit, unit)? / scale)
}

fn volts(key: &str) -> bool {
    key_unit(key).is_some_and(|(_, u)| u == "V")
}

/// The source's characteristic rows and the card's row-form params, named.
fn rows(entry: &ModelEntry) -> Vec<(String, &Characteristic)> {
    let cited = cited_rows(entry)
        .iter()
        .map(|r| (format!("row '{}'", r.parameter), r));
    let params = entry
        .params
        .rows()
        .map(|(k, r)| (format!("param `{k}`"), r));
    cited.chain(params).collect()
}

fn columns(r: &Characteristic) -> [(&'static str, Option<f64>); 3] {
    [("min", r.min), ("typ", r.typ), ("max", r.max)]
}

/// A volt row recording, in some column, the coefficient of a rail fraction
/// its own text prints (`min = 0.7` beside "0.7 VDD"): (column, coefficient, rail).
pub fn fraction_as_volts(r: &Characteristic) -> Option<(&'static str, f64, String)> {
    let scale = crate::units::scale_between(&r.unit, "V")?;
    let quoted = fractions(&format!("{} {} {}", r.parameter, r.conditions, r.basis));
    columns(r).into_iter().find_map(|(col, x)| {
        let v = x? * scale;
        let f = quoted.iter().find(|f| same(f.0, v))?;
        Some((col, f.0, f.1.clone()))
    })
}

fn ratiometric(entry: &ModelEntry) -> Vec<String> {
    let mut out = Vec::new();
    for (name, r) in rows(entry) {
        out.extend(fraction_as_volts(r).map(|(col, f, rail)| {
            format!("{name} records {col} = {f} V where its text prints '{f} {rail}'")
        }));
    }
    for (key, value) in &entry.params.0 {
        let Some(v) = value.as_f64().filter(|_| volts(key)) else {
            continue;
        };
        let sym = LEVELS.iter().find(|(k, _)| k == key).map(|(_, s)| *s);
        let records = entry.source.iter().flat_map(|s| &s.uncertainty);
        let quotes = records.filter_map(|u| match u {
            hauksbee_ir::evidence::ModelUncertainty::Unknown { parameter, reason } => {
                let ours = parameter == key || sym.is_some_and(|s| has_symbol(parameter, s));
                ours.then(|| format!("{parameter} {reason}"))
            }
            hauksbee_ir::evidence::ModelUncertainty::Interval { .. } => None,
        });
        let hit = quotes.flat_map(|q| fractions(&q)).find(|f| same(f.0, v));
        out.extend(hit.map(|f| {
            format!(
                "`{key} = {v}` is the coefficient of the '{} {}' its own unknown record quotes",
                f.0, f.1
            )
        }));
    }
    out
}

/// The supply a row's text prints (`VDD = 3 V`, `at 3 V VDD`), in V.
pub fn printed_supply(text: &str) -> Option<f64> {
    static AFTER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bV_?(?:CC|DD|S)\s*[=:≈]\s*(\d*\.?\d+)\s*V\b").expect("a valid pattern")
    });
    static BEFORE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\d*\.?\d+)\s*V\s+V_?(?:CC|DD|S)\b").expect("a valid pattern")
    });
    let c = AFTER.captures(text).or_else(|| BEFORE.captures(text))?;
    c[1].parse().ok().filter(|v: &f64| *v > 0.0)
}

/// For each logic level the card states as a plain number, the supplies of
/// the rows printing it (in its guaranteed column) that state one.
pub fn level_supplies(entry: &ModelEntry) -> Vec<(&'static str, Vec<f64>)> {
    let mut out = Vec::new();
    for (key, sym) in LEVELS {
        let Some(v) = entry.params.0.get(key).and_then(ParamValue::as_f64) else {
            continue;
        };
        let col = row_column(key);
        let printing = cited_rows(entry)
            .iter()
            .filter(|r| has_symbol(&r.parameter, sym));
        let printing = printing.filter(|r| {
            let x = match col {
                "min" => r.min.or(r.typ),
                "max" => r.max.or(r.typ),
                _ => r.typ,
            };
            x.and_then(|x| in_key_units(key, r, x))
                .is_some_and(|x| same(x, v))
        });
        let supplies: Vec<f64> = printing
            .filter_map(|r| printed_supply(&r.conditions))
            .collect();
        if !supplies.is_empty() {
            out.push((key, supplies));
        }
    }
    out
}

/// The one supply every stated level's rows share, if any is printed.
pub fn levels_corner(entry: &ModelEntry) -> Option<f64> {
    let sets = level_supplies(entry);
    let first = sets.first()?;
    let shared = first.1.iter().find(|s| {
        sets.iter()
            .all(|(_, set)| set.iter().any(|t| (*t - **s).abs() <= 0.02 * **s))
    });
    shared.copied()
}

fn one_corner(entry: &ModelEntry) -> Vec<String> {
    let sets = level_supplies(entry);
    if sets.len() < 2 || levels_corner(entry).is_some() {
        return Vec::new();
    }
    let read: Vec<String> = sets
        .iter()
        .map(|(k, s)| format!("{k} at {s:?} V"))
        .collect();
    vec![format!(
        "the stated levels come from rows at different supplies ({})",
        read.join(", ")
    )]
}

/// Words of a key or row that can tie one to the other (unit tokens dropped).
fn words(text: &str) -> Vec<String> {
    const UNITS: &[&str] = &[
        "v", "a", "ma", "ua", "hz", "khz", "ohm", "ohms", "s", "f", "pf", "w", "c", "k",
    ];
    let lower = text.to_lowercase();
    let split = lower.split(|c: char| !c.is_alphanumeric());
    let kept = split.filter(|w| w.len() >= 2 && !UNITS.contains(w));
    kept.map(str::to_string).collect()
}

/// Every numeric leaf of the params and behavioural blocks, as (key, value).
fn numeric_leaves(entry: &ModelEntry) -> Vec<(String, f64)> {
    fn walk(key: &str, v: &serde_json::Value, out: &mut Vec<(String, f64)>) {
        match v {
            serde_json::Value::Number(n) => out.extend(n.as_f64().map(|x| (key.to_string(), x))),
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    walk(k, v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    walk(key, v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (key, value) in &entry.params.0 {
        out.extend(value.as_f64().map(|x| (key.clone(), x)));
    }
    if let Ok(v) = serde_json::to_value(&entry.behavioral) {
        walk("", &v, &mut out);
    }
    out
}

fn nominal_is_typ(entry: &ModelEntry) -> Vec<String> {
    let rows = cited_rows(entry);
    let mut out = Vec::new();
    for (key, v) in numeric_leaves(entry) {
        if row_column(&key) != "typ" || key_unit(&key).is_none() {
            continue;
        }
        let ours = words(&key);
        let typ_of = |r: &Characteristic| r.typ.and_then(|t| in_key_units(&key, r, t));
        if rows.iter().any(|r| typ_of(r).is_some_and(|t| same(t, v))) {
            continue;
        }
        let related = |r: &&Characteristic| words(&r.parameter).iter().any(|w| ours.contains(w));
        let hit = rows.iter().filter(related).find_map(|r| {
            let t = typ_of(r)?;
            let (col, _) = [("min", r.min), ("max", r.max)]
                .into_iter()
                .find(|(_, x)| {
                    x.and_then(|x| in_key_units(&key, r, x))
                        .is_some_and(|x| same(x, v))
                })?;
            Some(format!(
                "`{key} = {v}` is the {col} column of row '{}' ({}), which prints typ {t}",
                r.parameter, r.conditions
            ))
        });
        out.extend(hit);
    }
    out
}

/// A supply-current row measured with the part doing something: reading,
/// writing, programming, an active interface or a running clock (`fSCL =
/// 400 kHz`); `f = 0 Hz` and "inactive" are idle.
pub fn dynamic(r: &Characteristic) -> bool {
    static CLOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bf_?[A-Za-z]*\s*[=:≈]\s*(\d*\.?\d+)\s*([kKM]?Hz)").expect("a valid pattern")
    });
    let text = format!("{} {}", r.parameter, r.conditions).to_lowercase();
    let busy = text.split(|c: char| !c.is_alphanumeric()).any(|w| {
        matches!(
            w,
            "write"
                | "writing"
                | "read"
                | "reading"
                | "program"
                | "programming"
                | "erase"
                | "erasing"
                | "active"
        )
    });
    let clock = format!("{} {}", r.parameter, r.conditions);
    let running = CLOCK
        .captures_iter(&clock)
        .any(|c| c[1].parse::<f64>().is_ok_and(|f| f > 0.0));
    busy || running
}

/// A row for a mode with the part switched off (shutdown, sleep, power-down,
/// disabled): no static draw while the rail is up. Standby is the bus-idle state.
pub fn shutdown(r: &Characteristic) -> bool {
    let text = r
        .parameter
        .to_lowercase()
        .replace("power-down", "powerdown");
    let text = text.replace("power down", "powerdown");
    let mut words = text.split(|c: char| !c.is_alphanumeric());
    words.any(|w| matches!(w, "shutdown" | "sleep" | "off" | "disabled" | "powerdown"))
}

/// A row for a standby or other named idle state, ranked after an operating one.
pub fn standby(r: &Characteristic) -> bool {
    let text = r.parameter.to_lowercase();
    let mut words = text.split(|c: char| !c.is_alphanumeric());
    words.any(|w| matches!(w, "standby" | "idle"))
}

/// The supply-current rows a static draw is read from (ICC, IDD, IQ).
pub fn supply_rows(entry: &ModelEntry) -> impl Iterator<Item = &Characteristic> {
    const SYMS: &[&str] = &["ICC*", "IDD*", "IQ", "ISUPPLY", "ISB", "ICCS"];
    cited_rows(entry).iter().filter(|r| {
        let text = r.parameter.to_lowercase();
        SYMS.iter().any(|s| has_symbol(&r.parameter, s))
            || text.contains("supply current")
            || text.contains("quiescent current")
            || text.contains("standby current")
    })
}

fn static_supply(entry: &ModelEntry) -> Vec<String> {
    let key = "supply_static_ua";
    let Some(v) = entry.params.0.get(key).and_then(ParamValue::as_f64) else {
        return Vec::new();
    };
    let rows = supply_rows(entry).filter(|r| !shutdown(r));
    let (busy, idle): (Vec<_>, Vec<_>) = rows.partition(|r| dynamic(r));
    let Some(quiet) = idle.iter().find(|r| !standby(r)).or_else(|| idle.first()) else {
        return Vec::new();
    };
    let printed = |r: &Characteristic| {
        columns(r).into_iter().any(|(_, x)| {
            x.and_then(|x| in_key_units(key, r, x))
                .is_some_and(|x| same(x, v))
        })
    };
    if idle.iter().any(|r| printed(r)) {
        return Vec::new();
    }
    let hit = busy.iter().find(|r| printed(r));
    hit.map(|r| {
        format!(
            "`{key} = {v}` is the dynamic row '{}' ({}); the static draw is row '{}' ({})",
            r.parameter, r.conditions, quiet.parameter, quiet.conditions
        )
    })
    .into_iter()
    .collect()
}

fn converter_dropout(entry: &ModelEntry) -> Vec<String> {
    let converter = entry.kind == ComponentKind::Vreg && entry.behavioral.converter.is_some();
    if !converter || !entry.params.0.contains_key("dropout_v") {
        return Vec::new();
    }
    vec!["this switching regulator states dropout_v; drop it from [models.params]".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first entry of `toml`, its `[[models.source.characteristic]]` rows
    /// given the source header they need.
    fn entry(toml: &str) -> ModelEntry {
        let header =
            "[models.source]\ntier = \"datasheet-derived\"\n[[models.source.characteristic]]";
        let toml = toml.replacen("[[models.source.characteristic]]", header, 1);
        let db: crate::schema::DbFile = toml::from_str(&toml).expect("a db file");
        db.models.into_iter().next().expect("an entry")
    }

    fn rule_ids(e: &ModelEntry) -> Vec<&'static str> {
        RULES
            .iter()
            .filter(|r| !(r.check)(e).is_empty())
            .map(|r| r.id)
            .collect()
    }

    const ROW: &str = "[[models.source.characteristic]]\n";

    /// PCF8563 round-3 draft: "VIH ... min 0.7, unit V, Lower limit 0.7 VDD".
    #[test]
    fn a_rail_fraction_recorded_as_volts_is_refused() {
        let card = format!(
            "[[models]]\nid = \"x\"\nkind = \"digital\"\n{ROW}parameter = \"VIH high-level input voltage\"\nmin = 0.7\nmax = 5.5\nunit = \"V\"\nconditions = \"Lower limit 0.7 VDD\"\nbasis = \"Table 29, HIGH-level input voltage, min/max\"\n"
        );
        let problems = problems(&entry(&card));
        assert!(
            problems.iter().any(|p| p.starts_with("ratiometric")),
            "{problems:?}"
        );
        // Quoted with the column left out, the same row passes.
        let fixed = card.replace("min = 0.7\n", "");
        assert!(rule_ids(&entry(&fixed)).is_empty());
    }

    /// 24AA025UID round-3 draft: the coefficient as a param, quoted in its comment.
    #[test]
    fn a_param_whose_comment_quotes_a_rail_fraction_is_refused() {
        let raw = "[models.params]\nvih = 0.7 # Source: Table 1-1, D1 min, VIH = 0.7 VCC\nvil = 0.3 # Source: Table 1-1, D2 max, VIL = 0.3 VCC\nvol = 0.4 # VOL max at VCC = 4.5 V\n";
        let found = raw_problems(raw);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("`vih = 0.7`") && found[0].contains("0.7 VCC"));
        assert!(raw_problems("vih = 2.31 # 0.7 VCC at VCC = 3.3 V\n").is_empty());
    }

    /// CY8CMBR3108 rows: levels read at different supplies are refused.
    #[test]
    fn levels_from_different_supply_corners_are_refused() {
        let rows = format!(
            "{ROW}parameter = \"VOH, output voltage HIGH level\"\nmin = 2.4\nunit = \"V\"\nconditions = \"IOH = −4 mA at 3 V VDD\"\nbasis = \"Table 16, VOH, min\"\n\
             {ROW}parameter = \"VOH, output voltage HIGH level\"\nmin = 1.3\nunit = \"V\"\nconditions = \"IOH = −1 mA at 1.8 V VDD\"\nbasis = \"Table 16, VOH, min\"\n\
             {ROW}parameter = \"VOL, output voltage LOW level\"\nmax = 0.6\nunit = \"V\"\nconditions = \"IOL = 4 mA at 1.8 V VDD\"\nbasis = \"Table 16, VOL, max\"\n\
             {ROW}parameter = \"VOL, output voltage LOW level\"\nmax = 0.6\nunit = \"V\"\nconditions = \"IOL = 10 mA at 3 V VDD\"\nbasis = \"Table 16, VOL, max\"\n"
        );
        let card = |voh: f64| {
            format!("[[models]]\nid = \"x\"\nkind = \"digital\"\n[models.params]\nvoh = {voh}\nvol = 0.6\n{rows}")
        };
        assert!(rule_ids(&entry(&card(2.4))).is_empty());
        assert_eq!(levels_corner(&entry(&card(2.4))), Some(3.0));
        // 1.3 V is VOH at 1.8 V and VOL is shared, so the corner is 1.8 V.
        assert_eq!(levels_corner(&entry(&card(1.3))), Some(1.8));
        let mixed = format!(
            "[[models]]\nid = \"x\"\nkind = \"digital\"\n[models.params]\nvoh = 1.3\nvol = 0.4\n{rows}{ROW}parameter = \"VOL\"\nmax = 0.4\nunit = \"V\"\nconditions = \"IOL = 3 mA at 3 V VDD\"\nbasis = \"b\"\n"
        );
        assert_eq!(rule_ids(&entry(&mixed)), vec!["one_corner"]);
    }

    /// MAX17760 round-3 draft: the EN/UVLO max column written as the threshold.
    #[test]
    fn a_bound_column_written_as_a_nominal_is_refused() {
        let card = |v: f64| {
            format!(
                "[[models]]\nid = \"x\"\nkind = \"vreg\"\n[models.behavioral.converter]\ntopology = \"buck\"\nout_pin = \"sw\"\nin_pin = \"in\"\nvout_setpoint = 5.0\n[models.behavioral.converter.enable]\npin = \"en\"\nhigh_threshold_v = {v}\n{ROW}parameter = \"EN/UVLO rising threshold\"\nmin = 1.19\ntyp = 1.215\nmax = 1.24\nunit = \"V\"\nconditions = \"VEN/UVLO rising\"\nbasis = \"Electrical Characteristics, min/typ/max\"\n"
            )
        };
        let bad = problems(&entry(&card(1.24)));
        assert!(bad.iter().any(|p| p.contains("max column")), "{bad:?}");
        assert!(rule_ids(&entry(&card(1.215))).is_empty());
    }

    /// 24AA025UID round-3 draft: the write current as the static draw.
    #[test]
    fn a_dynamic_supply_row_as_the_static_draw_is_refused() {
        let rows = format!(
            "{ROW}parameter = \"ICC write, operating current\"\ntyp = 0.1\nmax = 3\nunit = \"mA\"\nconditions = \"VCC = 5.5 V, SCL = 400 kHz\"\nbasis = \"Table 1-1, D8\"\n\
             {ROW}parameter = \"ICC read, operating current\"\nmax = 1\nunit = \"mA\"\nconditions = \"VCC = 5.5 V, SCL = 400 kHz\"\nbasis = \"Table 1-1, D9\"\n\
             {ROW}parameter = \"ICCS, standby current\"\ntyp = 0.01\nmax = 1\nunit = \"µA\"\nconditions = \"SCL = SDA = VCC = 5.5 V\"\nbasis = \"Table 1-1, D10\"\n"
        );
        let card = |v: f64| {
            format!("[[models]]\nid = \"x\"\nkind = \"digital\"\n[models.params]\nsupply_static_ua = {v}\n{rows}")
        };
        let bad = problems(&entry(&card(100.0)));
        assert!(
            bad.iter().any(|p| p.contains("ICCS, standby current")),
            "{bad:?}"
        );
        assert!(rule_ids(&entry(&card(1.0))).is_empty());
    }

    /// MAX17760 round-3 draft: the VCC LDO's dropout on a buck.
    #[test]
    fn a_converter_with_dropout_is_refused() {
        let card = "[[models]]\nid = \"x\"\nkind = \"vreg\"\n[models.params]\ndropout_v = 0.27\n[models.behavioral.converter]\ntopology = \"buck\"\nout_pin = \"sw\"\nin_pin = \"in\"\nvout_setpoint = 5.0\n";
        assert_eq!(rule_ids(&entry(card)), vec!["converter_dropout"]);
    }

    #[test]
    fn the_prompt_guide_names_every_rule() {
        let guide = prompt_guide();
        assert!(RULES
            .iter()
            .all(|r| guide.contains(r.id) && guide.contains(r.guide)));
    }
}
