//! Physical-range sanity for the unit-suffixed fields of a model card.
//!
//! A field named for its unit (`cout_min_f`, `iq_a`, `supply_static_ua`,
//! `pole_hz`) has a range real datasheets never leave, whatever the part. A
//! value outside it is almost always a unit slip: `cout_min_f = 1.0` for the
//! datasheet's 1 µF (a farad field written in microfarads) made the output-
//! capacitor check demand a million microfarads of the board.
//!
//! [`ROWS`] is the one table. The extraction validator and `models lint` reach it
//! through [`crate::validation::validate`], the extraction prompt quotes it
//! ([`prompt_guide`]), and the binder and the checks that read a card's value as a
//! requirement drop an out-of-range one ([`quarantine`]) instead of demanding it of
//! the board. A row names its fields by pattern (`*` once, `|` between
//! alternatives), so a new field with a known suffix is covered without an edit,
//! and `tests` hold every typed unit-suffixed field of the schema to a row.
//!
//! Ranges are in the field's base SI unit (a `_ua` field is read as amperes after
//! scaling), chosen wider than anything a datasheet prints for the field's meaning
//! and narrower than a 1e3 / 1e6 / 1e9 slip lands. A zero is always in range: it
//! is how a card says "none". Nothing here clamps or rewrites a value.

use std::sync::LazyLock;

use hauksbee_ir::units::format_engineering;

use crate::schema::{ModelEntry, ParamValue};

/// How a field's value is compared with its range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Zero, or a magnitude in `lo..=hi`, of either sign (a voltage, a current).
    Magnitude,
    /// Zero, or a value in `lo..=hi`; a negative value is out of range (a
    /// capacitance, a resistance, a time).
    NonNegative,
    /// A signed value in `lo..=hi` (a temperature).
    Span,
}

/// One row of the table: the fields it covers and the range their meaning allows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row {
    /// Field names: exact, or one `*` (`*_ohms`, `theta_*_c_per_w`), `|` between
    /// alternatives.
    pub pattern: &'static str,
    /// The base unit the range is in (what a value reads as after its suffix scale).
    pub unit: &'static str,
    pub kind: Kind,
    /// Smallest plausible nonzero magnitude (or value), in `unit`.
    pub lo: f64,
    /// Largest plausible magnitude (or value), in `unit`.
    pub hi: f64,
    /// What the field means, quoted in the message ("an output capacitor's stability
    /// requirement"); empty for a row that covers a whole unit.
    pub what: &'static str,
}

const fn row(
    pattern: &'static str,
    unit: &'static str,
    kind: Kind,
    (lo, hi): (f64, f64),
    what: &'static str,
) -> Row {
    Row {
        pattern,
        unit,
        kind,
        lo,
        hi,
        what,
    }
}

use Kind::{Magnitude, NonNegative, Span};

/// The table of physical ranges.
///
/// Specific rows come first: the first row whose pattern names a field is its
/// range. The unit rows last cover every other field spelled with a unit suffix
/// (`units::suffix_unit`), so a name the table has no row for still gets its
/// unit's range.
pub const ROWS: &[Row] = &[
    // Capacitors a card states a requirement for.
    row(
        "cout_min_f|cout_recommended_f",
        "F",
        NonNegative,
        (100e-12, 10e-3),
        "an output capacitor's stability requirement",
    ),
    row(
        "cout_esr_min_ohm|cout_esr_max_ohm",
        "ohm",
        NonNegative,
        (1e-3, 1e3),
        "an output capacitor's ESR window",
    ),
    row(
        "xtal_cap_min_f|xtal_cap_max_f",
        "F",
        NonNegative,
        (1e-12, 1e-9),
        "a crystal load capacitor",
    ),
    // The feedback pin current of a ground-referenced adjustable regulator.
    row(
        "ifb_a",
        "A",
        Magnitude,
        (1e-12, 1e-3),
        "a regulator's feedback pin current",
    ),
    // A crystal's datasheet capacitances: the specified load, the shunt (holder)
    // and the motional capacitance.
    row(
        "cl_f",
        "F",
        NonNegative,
        (1e-12, 100e-12),
        "a crystal's specified load capacitance",
    ),
    row(
        "c0_f",
        "F",
        NonNegative,
        (100e-15, 50e-12),
        "a crystal's shunt capacitance",
    ),
    row(
        "c1_f",
        "F",
        NonNegative,
        (100e-18, 200e-15),
        "a crystal's motional capacitance",
    ),
    // Regulators and supply draws.
    row(
        "dropout_v|bat_dropout_v",
        "V",
        NonNegative,
        (1e-3, 20.0),
        "a regulator's input-to-output headroom",
    ),
    row(
        "iq_a|i_shdn_a",
        "A",
        NonNegative,
        (10e-12, 1.0),
        "a regulator's supply current",
    ),
    row(
        "supply_static_ua",
        "A",
        NonNegative,
        (10e-12, 10.0),
        "an IC's quiescent supply current",
    ),
    row(
        "en_on_v|en_off_v",
        "V",
        Magnitude,
        (10e-3, 100.0),
        "an enable pin's threshold",
    ),
    // Levels the glossary documents in volts (no unit suffix in the name).
    row(
        "vout|vref|vih|vil|voh|vol|out_lo|out_hi|rail_lo|rail_hi|swing_lo|swing_hi|hysteresis",
        "V",
        Magnitude,
        (1e-6, 1e3),
        "a voltage level this product's boards carry",
    ),
    // Timing and frequency.
    row(
        "tpd_s",
        "s",
        NonNegative,
        (10e-12, 10e-3),
        "a logic propagation delay",
    ),
    row(
        "switching_hz",
        "Hz",
        NonNegative,
        (1e3, 100e6),
        "a converter's switching frequency",
    ),
    row(
        "default_clock_hz",
        "Hz",
        NonNegative,
        (1e3, 2e9),
        "a core clock",
    ),
    row(
        "leak_s",
        "S",
        NonNegative,
        (1e-15, 1e3),
        "an output-off conductance (siemens, not seconds)",
    ),
    // The programming constant of `I = k / R`: volts by name, ohm-amperes by meaning.
    row(
        "k_volts|low_k_volts",
        "A*ohm",
        Magnitude,
        (1e-3, 1e7),
        "a current-programming constant (amperes times the resistor's ohms)",
    ),
    // Ratings (the typed `[models.ratings]` keys).
    row(
        "max_voltage_v",
        "V",
        NonNegative,
        (0.1, 1e4),
        "a rated working voltage",
    ),
    row(
        "max_current_a|max_pin_current_a|min_current_a",
        "A",
        NonNegative,
        (1e-6, 1e3),
        "a rated continuous current",
    ),
    row(
        "max_surge_current_a",
        "A",
        NonNegative,
        (1e-3, 1e5),
        "a rated surge current",
    ),
    row(
        "max_power_w",
        "W",
        NonNegative,
        (100e-6, 1e6),
        "a rated dissipation",
    ),
    row(
        "max_junction_temp_c",
        "C",
        Span,
        (50.0, 400.0),
        "a maximum junction temperature",
    ),
    row(
        "theta_*_c_per_w",
        "K/W",
        NonNegative,
        (0.01, 5e3),
        "a thermal resistance",
    ),
    row("*temp_c", "C", Span, (-273.15, 500.0), "a temperature"),
    // Pin ratings the board checks read from a card.
    row(
        "iol_a",
        "A",
        NonNegative,
        (100e-6, 1.0),
        "an open-drain output's low-level sink rating",
    ),
    row(
        "iih_a",
        "A",
        NonNegative,
        (1e-12, 10e-3),
        "a logic input's high-level leakage",
    ),
    // Every other field spelled with a unit suffix.
    row("*_v|*_volts", "V", Magnitude, (1e-6, 1e4), ""),
    row("*_a|*_ma|*_ua", "A", Magnitude, (1e-12, 1e4), ""),
    row("*_ohm|*_ohms", "ohm", NonNegative, (100e-6, 1e12), ""),
    row("*_s", "s", NonNegative, (1e-12, 1e7), ""),
    row("*_f|*_pf", "F", NonNegative, (1e-15, 10.0), ""),
    row("*_hz", "Hz", NonNegative, (1e-3, 100e9), ""),
    row("*_w", "W", NonNegative, (1e-9, 1e6), ""),
];

fn pattern_matches(pattern: &str, name: &str) -> bool {
    pattern.split('|').any(|alt| match alt.split_once('*') {
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail)
        }
        None => alt == name,
    })
}

/// A row that covers a whole unit (`*_v`) rather than particular fields.
fn is_unit_row(row: &Row) -> bool {
    row.pattern.split('|').all(|alt| alt.starts_with('*'))
}

/// The row for field `name`, if the table has one.
///
/// A `_per_` name (`theta_ja_c_per_w`, a ratio) is a unit's field only through a
/// row that names it: its trailing suffix is not its unit.
#[must_use]
pub fn row_for(name: &str) -> Option<&'static Row> {
    let ratio = name.contains("_per_");
    ROWS.iter()
        .find(|r| pattern_matches(r.pattern, name) && !(ratio && is_unit_row(r)))
}

/// The multiplier from `name`'s own unit (`_ua` is 1e-6) to the row's base unit.
fn scale_of(name: &str) -> f64 {
    if name.contains("_per_") {
        return 1.0;
    }
    crate::units::suffix_unit(name).map_or(1.0, |(scale, _)| scale)
}

impl Row {
    /// Whether `base` (in this row's unit) is inside the range.
    #[must_use]
    pub fn admits(&self, base: f64) -> bool {
        if !base.is_finite() {
            return true;
        }
        match self.kind {
            Magnitude => base == 0.0 || (self.lo..=self.hi).contains(&base.abs()),
            NonNegative => base == 0.0 || (self.lo..=self.hi).contains(&base),
            Span => (self.lo..=self.hi).contains(&base),
        }
    }

    /// The range as a sentence fragment: `100 pF to 10 mF`.
    fn range_text(&self) -> String {
        let (lo, hi) = (self.lo, self.hi);
        let ends = |v| format_engineering(v, self.unit);
        match self.kind {
            Magnitude => format!("{} to {} in magnitude (or zero)", ends(lo), ends(hi)),
            NonNegative => format!("{} to {} (or zero)", ends(lo), ends(hi)),
            Span => format!("{} to {}", ends(lo), ends(hi)),
        }
    }
}

/// One value outside its range.
#[derive(Debug, Clone, PartialEq)]
pub struct Implausible {
    /// Where the value sits: `param 'cout_min_f'`, or a dotted path for a block.
    pub place: String,
    /// The field name (the last path element).
    pub key: String,
    /// The value as written, in the field's own unit.
    pub value: f64,
    /// Whether the value is a `[models.params]` entry (which [`quarantine`] can drop).
    pub param: bool,
    row: &'static Row,
    scale: f64,
}

impl Implausible {
    /// The factors of 1e3, 1e6 or 1e9 that bring the value into range, as
    /// (factor, rescaled value in the field's own unit), nearest slip first.
    #[must_use]
    pub fn rescales(&self) -> Vec<(f64, f64)> {
        const FACTORS: [f64; 6] = [1e-3, 1e3, 1e-6, 1e6, 1e-9, 1e9];
        let hits = FACTORS.iter().filter_map(|f| {
            let moved = self.value * f;
            (moved != 0.0 && self.row.admits(moved * self.scale)).then_some((*f, moved))
        });
        hits.collect()
    }

    /// The refusal text: field, value, likely intended value, plausible range.
    #[must_use]
    pub fn message(&self) -> String {
        let row = self.row;
        let unit_of = |own: f64| {
            if (self.scale - 1.0).abs() < f64::EPSILON {
                format_engineering(own, row.unit)
            } else {
                format!(
                    "{own:e} = {}",
                    format_engineering(own * self.scale, row.unit)
                )
            }
        };
        let what = if row.what.is_empty() {
            String::new()
        } else {
            format!(" for {}", row.what)
        };
        let likely: Vec<String> = self
            .rescales()
            .iter()
            .take(3)
            .map(|(f, moved)| format!("{f:e}x gives {}", unit_of(*moved)))
            .collect();
        let likely = if likely.is_empty() {
            String::new()
        } else {
            format!("; a unit slip is likely ({})", likely.join(", "))
        };
        format!(
            "{} = {} is outside the plausible range {}{what}{likely}: a field named {} holds {} \
             exactly as its name says, so rewrite the value in that unit",
            self.place,
            unit_of(self.value),
            row.range_text(),
            self.key,
            unit_name(&self.key, row),
        )
    }
}

/// The unit a field's name promises, spelled out (`farads`, `microamperes`).
fn unit_name(key: &str, row: &Row) -> String {
    let (scale, _) = crate::units::suffix_unit(key).unwrap_or((1.0, ""));
    let prefix = if (scale - 1e-3).abs() < 1e-12 {
        "milli"
    } else if (scale - 1e-6).abs() < 1e-15 {
        "micro"
    } else if (scale - 1e-12).abs() < 1e-21 {
        "pico"
    } else {
        ""
    };
    let base = match row.unit {
        "F" => "farads",
        "V" => "volts",
        "A" => "amperes",
        "ohm" => "ohms",
        "s" => "seconds",
        "Hz" => "hertz",
        "W" => "watts",
        "S" => "siemens",
        "C" => "degrees Celsius",
        "K/W" => "kelvin per watt",
        other => other,
    };
    format!("{prefix}{base}")
}

/// `value` of field `name` against its row: the violation, if it has one.
fn violation(place: String, name: &str, value: f64, param: bool) -> Option<Implausible> {
    let row = row_for(name)?;
    let scale = scale_of(name);
    (!row.admits(value * scale)).then(|| Implausible {
        place,
        key: name.to_string(),
        value,
        param,
        row,
        scale,
    })
}

/// Whether `value` is a plausible value for field `name`: true when the table has no
/// row for it.
#[must_use]
pub fn plausible(name: &str, value: f64) -> bool {
    violation(String::new(), name, value, false).is_none()
}

/// Every numeric field of `entry` outside its range.
///
/// `[models.params]` first, then every other block (ratings, envelope, behavioral, current program, ...) by path.
/// `[models.source]` rows state datasheet prints in their own units and are not read.
#[must_use]
pub fn problems(entry: &ModelEntry) -> Vec<Implausible> {
    fn walk(path: &str, key: &str, v: &serde_json::Value, out: &mut Vec<Implausible>) {
        match v {
            serde_json::Value::Number(n) => {
                let hit = n
                    .as_f64()
                    .and_then(|x| violation(path.to_string(), key, x, false));
                out.extend(hit);
            }
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    walk(&format!("{path}.{k}"), k, v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    walk(&format!("{path}[{i}]"), key, v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (key, value) in &entry.params.0 {
        let value = match value {
            ParamValue::Float(_) | ParamValue::Int(_) | ParamValue::Row(_) => {
                entry.params.get_f64(key)
            }
            _ => None,
        };
        let hit = value.and_then(|x| violation(format!("param '{key}'"), key, x, true));
        out.extend(hit);
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(entry) {
        for (k, v) in &map {
            if !matches!(k.as_str(), "params" | "source" | "id" | "match") {
                walk(k, k, v, &mut out);
            }
        }
    }
    out
}

/// A card read with its out-of-range values distrusted.
#[derive(Debug, Clone)]
pub struct Quarantined {
    /// The card with every out-of-range `[models.params]` entry removed, so a
    /// consumer reads it as not stated, never as the absurd number.
    pub model: ModelEntry,
    /// Every out-of-range value found, in a param or any other block.
    pub found: Vec<Implausible>,
}

impl Quarantined {
    /// One line naming the card and its first values, for a disclosure.
    #[must_use]
    pub fn summary(&self) -> String {
        let shown: Vec<String> = self
            .found
            .iter()
            .take(2)
            .map(|f| {
                let used = if f.param {
                    "not used"
                } else {
                    "kept as stated"
                };
                format!("{} ({used})", f.message())
            })
            .collect();
        let more = self.found.len().saturating_sub(2);
        let more = if more > 0 {
            format!("; and {more} more")
        } else {
            String::new()
        };
        format!(
            "card '{}' states a value outside its physical range, so its numbers are suspect: {}{more}",
            self.model.id,
            shown.join("; ")
        )
    }
}

/// `entry` with its out-of-range values distrusted, or `None` when it has none.
///
/// The engine's guard point: a card that reaches the binder or a check without having
/// passed [`crate::validation::validate`] (a built-in or an older saved card) must
/// not turn a unit slip into a requirement on the board.
#[must_use]
pub fn quarantine(entry: &ModelEntry) -> Option<Quarantined> {
    let found = problems(entry);
    if found.is_empty() {
        return None;
    }
    let mut model = entry.clone();
    for f in found.iter().filter(|f| f.param) {
        model.params.0.remove(&f.key);
    }
    Some(Quarantined { model, found })
}

/// The extraction prompt's line on ranges, drawn from [`ROWS`] so the drafter is told
/// what the validator then holds it to.
#[must_use]
pub fn prompt_guide() -> String {
    static GUIDE: LazyLock<String> = LazyLock::new(|| {
        let fields: Vec<String> = ROWS
            .iter()
            .filter(|r| !r.what.is_empty() && r.pattern.split('|').all(|a| !a.contains('*')))
            .take(6)
            .map(|r| {
                let name = r.pattern.split('|').next().unwrap_or(r.pattern);
                format!("{name} {}", r.range_text())
            })
            .collect();
        format!(
            "   - physical_range: a field named for its unit holds that unit exactly (_f farads, \
             _v volts, _a amperes, _ua microamperes, _ohm ohms, _hz hertz, _s seconds), and \
             every one has a range datasheets never leave ({}; the validator refuses a value \
             outside its range as a probable 1e3, 1e6 or 1e9 unit slip, naming the field and \
             the rescale that fits): a 1 uF capacitor is cout_min_f = 1e-6, never 1.0",
            fields.join("; ")
        )
    });
    GUIDE.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::DbFile;

    fn entry(params: &str) -> ModelEntry {
        let src = format!(
            "[[models]]\nid = \"t\"\nkind = \"vreg\"\n[models.match]\nvalue_re = \"^T$\"\n\
             [models.params]\n{params}\n"
        );
        toml::from_str::<DbFile>(&src).unwrap().models.remove(0)
    }

    #[test]
    fn in_range_values_pass() {
        for (name, v) in [
            ("cout_min_f", 1e-6),
            ("cout_min_f", 0.0),
            ("cout_recommended_f", 22e-6),
            ("cout_esr_max_ohm", 22.0),
            ("iq_a", 55e-6),
            ("supply_static_ua", 50.0),
            ("supply_static_ua", 50_000.0),
            ("pole_hz", 2.5),
            ("tpd_s", 2.4e-9),
            ("dropout_v", 0.3),
            ("max_junction_temp_c", 150.0),
            ("vout", -5.0),
            ("some_made_up_name", 1e30),
        ] {
            assert!(plausible(name, v), "{name} = {v}");
        }
    }

    #[test]
    fn out_of_range_values_fail() {
        for (name, v) in [
            ("cout_min_f", 1.0),
            ("cout_min_f", -1e-6),
            ("cout_recommended_f", 22.0),
            ("iq_a", 55.0),
            ("supply_static_ua", 5e10),
            ("dropout_v", 300.0),
            ("tpd_s", 2.4),
            ("max_junction_temp_c", 1500.0),
            ("vout", 3300.0),
            ("rsat_ohms", 1e15),
        ] {
            assert!(!plausible(name, v), "{name} = {v}");
        }
    }

    #[test]
    fn the_one_farad_capacitor_is_refused_with_its_rescale() {
        let found = problems(&entry("cout_min_f = 1.0"));
        assert_eq!(found.len(), 1, "{found:?}");
        let msg = found[0].message();
        for needle in [
            "param 'cout_min_f'",
            "= 1 F",
            "100 pF to 10 mF",
            "1e-6x gives 1 uF",
            "1e-3x gives 1 mF",
            "farads",
        ] {
            assert!(msg.contains(needle), "{needle:?} in {msg}");
        }
        let fixed = problems(&entry("cout_min_f = 1.0e-6"));
        assert!(fixed.is_empty(), "{fixed:?}");
    }

    #[test]
    fn a_scaled_suffix_is_read_in_its_own_unit() {
        // 5 mA written 5000 in a microamp field is fine; 5e10 uA is 50 kA.
        assert!(plausible("supply_static_ua", 5000.0));
        let found = problems(&entry("supply_static_ua = 5e10"));
        assert_eq!(found.len(), 1);
        let msg = found[0].message();
        assert!(msg.contains("supply_static_ua"), "{msg}");
        assert!(msg.contains("microamperes"), "{msg}");
        assert!(
            found[0]
                .rescales()
                .iter()
                .any(|(f, _)| (*f - 1e-6).abs() < 1e-12),
            "{msg}"
        );
    }

    #[test]
    fn rescale_offers_only_factors_that_land_in_range() {
        let f = problems(&entry("iq_a = 5500.0"));
        assert_eq!(f.len(), 1);
        // 5500 A: 1e-6 gives 5.5 mA (in range); 1e-3 gives 5.5 A (out); 1e-9 gives 5.5 uA.
        let factors: Vec<f64> = f[0].rescales().iter().map(|(f, _)| *f).collect();
        assert_eq!(factors, [1e-6, 1e-9], "{factors:?}");
        // Nothing lands in range: no guess is made.
        let f = problems(&entry("iq_a = 1e30"));
        assert!(f[0].rescales().is_empty());
        assert!(!f[0].message().contains("unit slip"));
    }

    #[test]
    fn ratings_and_blocks_are_read_by_path() {
        let mut e = entry("vout = 3.3");
        e.ratings.max_junction_temp_c = Some(2000.0);
        let found = problems(&e);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].place.contains("max_junction_temp_c"), "{found:?}");
        assert!(!found[0].param);
    }

    #[test]
    fn quarantine_drops_only_the_bad_params() {
        let e = entry("vout = 3.3\ncout_min_f = 1.0\niq_a = 1e-4");
        let q = quarantine(&e).expect("a suspect card");
        assert_eq!(q.model.params.get_f64("cout_min_f"), None);
        assert_eq!(q.model.params.get_f64("vout"), Some(3.3));
        assert_eq!(q.model.params.get_f64("iq_a"), Some(1e-4));
        assert!(q.summary().contains("not used"), "{}", q.summary());
        assert!(quarantine(&entry("vout = 3.3")).is_none());
    }

    #[test]
    fn a_ratio_name_is_not_taken_for_its_trailing_unit() {
        // theta_ja_c_per_w ends in _w, but it is K/W: only its own row reads it.
        assert!(row_for("theta_ja_c_per_w").is_some_and(|r| r.unit == "K/W"));
        assert!(row_for("v_per_a").is_none());
    }

    /// Every typed unit-suffixed `f64` field the schema declares, every rating key and
    /// every param the vocabulary names with a unit suffix has a row.
    #[test]
    fn every_unit_suffixed_schema_field_has_a_row() {
        let mut names: Vec<String> = crate::schema::Ratings::keys().map(str::to_string).collect();
        for kind in [
            crate::schema::ComponentKind::Passive,
            crate::schema::ComponentKind::Diode,
            crate::schema::ComponentKind::Nmos,
            crate::schema::ComponentKind::Vreg,
            crate::schema::ComponentKind::Opamp,
            crate::schema::ComponentKind::Comparator,
            crate::schema::ComponentKind::Timer,
            crate::schema::ComponentKind::AnalogSwitch,
            crate::schema::ComponentKind::Digital,
            crate::schema::ComponentKind::Dac,
            crate::schema::ComponentKind::Adc,
            crate::schema::ComponentKind::Mcu,
        ] {
            names.extend(
                crate::param_names::known_param_names(kind)
                    .iter()
                    .map(|s| (*s).to_string()),
            );
        }
        // The typed fields of the schema and behavioral sources: `pub name: f64` or
        // `pub name: Option<f64>`.
        for src in [
            include_str!("schema.rs"),
            include_str!("behavioral.rs"),
            include_str!("sensor_spec.rs"),
        ] {
            for line in src.lines() {
                let Some(rest) = line.trim().strip_prefix("pub ") else {
                    continue;
                };
                let Some((name, ty)) = rest.split_once(':') else {
                    continue;
                };
                let ty = ty.trim().trim_end_matches(',');
                if matches!(ty, "f64" | "Option<f64>") {
                    names.push(name.trim().to_string());
                }
            }
        }
        let suffixed = |n: &str| {
            crate::units::suffix_unit(n).is_some()
                || n.ends_with("temp_c")
                || (n.starts_with("theta_") && n.ends_with("_c_per_w"))
        };
        let missing: Vec<&String> = names
            .iter()
            .filter(|n| suffixed(n) && row_for(n).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "unit-suffixed fields without a range row: {missing:?}"
        );
        assert!(
            names.iter().filter(|n| suffixed(n)).count() >= 25,
            "the scan found too few fields to mean anything: {names:?}"
        );
    }

    /// Every suffix the rest of the product reads as a unit has a unit row whose base
    /// unit is the suffix's own.
    #[test]
    fn every_known_unit_suffix_has_a_unit_row_of_its_unit() {
        for suffix in [
            "_v", "_volts", "_a", "_ma", "_ua", "_ohm", "_ohms", "_s", "_f", "_pf", "_hz", "_w",
        ] {
            let name = format!("probe{suffix}");
            let (_, base) = crate::units::suffix_unit(&name).expect("a known suffix");
            let row = row_for(&name).unwrap_or_else(|| panic!("no row for {name}"));
            assert_eq!(row.unit, base, "{name}");
            assert!(is_unit_row(row), "{name} falls to a specific row");
        }
    }

    #[test]
    fn rows_are_well_formed() {
        for r in ROWS {
            assert!(r.lo <= r.hi, "{}", r.pattern);
            assert!(r.lo.is_finite() && r.hi.is_finite(), "{}", r.pattern);
            if r.kind != Span {
                assert!(
                    r.lo > 0.0,
                    "{}: a zero floor admits nothing smaller",
                    r.pattern
                );
            }
        }
    }

    #[test]
    fn the_prompt_quotes_the_table() {
        let guide = prompt_guide();
        assert!(guide.contains("physical_range"), "{guide}");
        assert!(guide.contains("cout_min_f"), "{guide}");
        assert!(guide.contains("100 pF to 10 mF"), "{guide}");
    }

    /// Every tracked card (the built-in database, model packs and the extracted-card
    /// fixtures) is inside the table. A failure is a real unit slip in a card: check
    /// the datasheet and fix the card, or widen the row when the field's meaning
    /// allows it.
    #[test]
    fn no_tracked_card_is_out_of_range() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files: Vec<(String, String)> = crate::BUILTIN_TOML_FILES
            .iter()
            .map(|(n, s)| ((*n).to_string(), (*s).to_string()))
            .collect();
        for dir in ["model-packs", "testdata"] {
            let mut stack = vec![root.join(dir)];
            while let Some(d) = stack.pop() {
                let Ok(read) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in read.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().is_some_and(|x| x == "toml") {
                        if let Ok(s) = std::fs::read_to_string(&p) {
                            if s.contains("[[models]]") {
                                files.push((p.display().to_string(), s));
                            }
                        }
                    }
                }
            }
        }
        let mut bad = Vec::new();
        let mut checked = 0;
        for (name, src) in &files {
            let Ok(db) = toml::from_str::<DbFile>(src) else {
                continue;
            };
            for entry in &db.models {
                checked += 1;
                bad.extend(
                    problems(entry)
                        .iter()
                        .map(|p| format!("{name}: model '{}': {}", entry.id, p.message())),
                );
            }
        }
        assert!(checked > 500, "only {checked} cards scanned");
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
