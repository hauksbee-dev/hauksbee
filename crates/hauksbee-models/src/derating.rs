//! Temperature-dependent ratings: a card's printed rating rows (a MOSFET's ID at
//! TA = 25 °C and at TA = 70 °C), read at the temperature the scenario puts the
//! part at rather than collapsed to one number.
//!
//! Between printed rows the limit is interpolated linearly, which sits on or
//! below both the linear PD derating and the concave √-law ID derating. Past the
//! hottest row it falls linearly to zero at Tj,max, never along the rows' own
//! slope; below the coldest row it holds that row's value.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::schema::{Characteristic, ModelEntry, Ratings};

/// The temperature a printed rating row is specified at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TempRef {
    Ambient,
    /// TC, and the case-like mounting-base, lead and solder-point temperatures.
    Case,
    Junction,
}

/// Tj,max a curve declines to past its hottest row when the card states none.
pub const ASSUMED_TJ_MAX_C: f64 = 125.0;

const NUM: &str = r"([-+]?\d+(?:\.\d+)?)\s*(?:°\s*C|℃|C\b)?";
const SYM: &str = r"\bT\s*_?(?i:(amb|a|case|c|mb|l|sp|j))\b";

/// `TA = 25 °C`, `TC = 25 to 100 °C`, `TJ ≤ 150 °C`.
static SYMBOL_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    let range = format!(r"(?:\s*(?:to|\.\.\.?|…|~|–|-)\s*{NUM})?");
    Regex::new(&format!(r"{SYM}\s*(?:=|:|≤|<=|<)?\s*{NUM}{range}")).expect("a valid pattern")
});
/// `−40 °C ≤ TA ≤ 85 °C`.
static RANGE_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    let le = r"\s*(?:≤|<=|<)\s*";
    Regex::new(&format!(r"{NUM}{le}{SYM}{le}{NUM}")).expect("a valid pattern")
});

/// The reference and signed span (lo <= hi, °C) of the temperature a row's
/// conditions name; a single temperature is a span of one point.
#[must_use]
pub fn temp_span(conditions: &str) -> Option<(TempRef, f64, f64)> {
    let text = conditions.replace('−', "-");
    let (sym, lo, hi) = if let Some(c) = RANGE_FIRST.captures(&text) {
        (c.get(2)?, c.get(1)?, c.get(3))
    } else {
        let c = SYMBOL_FIRST.captures(&text)?;
        (c.get(1)?, c.get(2)?, c.get(3))
    };
    let reference = match sym.as_str().to_ascii_lowercase().as_str() {
        "a" | "amb" => TempRef::Ambient,
        "j" => TempRef::Junction,
        _ => TempRef::Case,
    };
    let lo: f64 = lo.as_str().parse().ok()?;
    let hi: f64 = hi.map_or(Ok(lo), |h| h.as_str().parse()).ok()?;
    Some((reference, lo.min(hi), lo.max(hi)))
}

/// Whether `key` names a `[models.ratings]` key.
#[must_use]
pub fn is_rating_key(key: &str) -> bool {
    Ratings::KEY_GUIDE
        .lines()
        .any(|l| l.split_whitespace().next() == Some(key))
}

/// A tagged row's magnitude in its key's unit.
///
/// A thermal resistance reads its max (else typ) in °C/W, anything else the
/// column and unit [`crate::param_names::row_value`] reads; a P-channel ID row
/// keeps its printed sign, and ratings are magnitudes.
#[must_use]
pub fn rating_value(key: &str, row: &Characteristic) -> Option<f64> {
    let value = if key.starts_with("theta_") {
        let per_watt = matches!(row.unit.replace(['°', ' '], "").as_str(), "C/W" | "K/W");
        row.max.or(row.typ).filter(|_| per_watt)
    } else {
        crate::param_names::row_value(key, row).ok()
    };
    value.map(f64::abs)
}

/// The least favourable of two values of `key`: the lower ceiling, else the
/// higher (a thermal resistance, a regulation floor, a gate threshold).
#[must_use]
pub fn worse(key: &str, a: f64, b: f64) -> f64 {
    if key.starts_with("max_") {
        a.min(b)
    } else {
        a.max(b)
    }
}

/// One rating's printed rows: per (reference, package, mounting) group, the
/// `(°C, value)` points sorted by temperature. Empty when no row is tagged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curve(pub Vec<(TempRef, Vec<(f64, f64)>)>);

/// `key`'s curve from the card's rows tagged `rating = key`.
///
/// Rows whose package and mounting match are kept (an untagged side matches
/// anything). A row naming no temperature is read at the datasheet default,
/// TA = 25 °C.
#[must_use]
pub fn curve(
    entry: &ModelEntry,
    key: &str,
    package: Option<&str>,
    mounting: Option<&str>,
) -> Curve {
    let fits = |want: Option<&str>, has: &Option<String>| {
        want.is_none_or(|w| has.as_deref().is_none_or(|h| h == w))
    };
    let rows = entry.source.iter().flat_map(|s| &s.characteristic);
    let rows = rows.filter(|r| r.rating.as_deref() == Some(key));
    let mut groups: BTreeMap<_, Vec<(f64, f64)>> = BTreeMap::new();
    for row in rows.filter(|r| fits(package, &r.package) && fits(mounting, &r.mounting)) {
        let Some(v) = rating_value(key, row) else {
            continue;
        };
        let (at, lo, hi) = temp_span(&row.conditions).unwrap_or((TempRef::Ambient, 25.0, 25.0));
        let points = groups.entry((at, &row.package, &row.mounting)).or_default();
        points.extend([(lo, v), (hi, v)]);
    }
    Curve(
        groups
            .into_iter()
            .map(|((at, ..), mut points)| {
                // Two rows at one temperature: keep the least favourable.
                let rank = |v: f64| if key.starts_with("max_") { v } else { -v };
                points.sort_by(|a, b| a.0.total_cmp(&b.0).then(rank(a.1).total_cmp(&rank(b.1))));
                points.dedup_by(|later, kept| later.0.total_cmp(&kept.0).is_eq());
                (at, points)
            })
            .collect(),
    )
}

impl Curve {
    /// The least favourable group's limit, each group read at `at(reference)`
    /// (°C); `None` when no row is tagged.
    #[must_use]
    pub fn limit(&self, at: impl Fn(TempRef) -> f64, tj_max_c: f64) -> Option<f64> {
        let read = |points: &[(f64, f64)], t: f64| {
            let (first, last) = (points[0], points[points.len() - 1]);
            if t <= first.0 {
                first.1
            } else if t > last.0 {
                let span = tj_max_c - last.0;
                if span > 0.0 {
                    (last.1 * (tj_max_c - t) / span).max(0.0)
                } else {
                    0.0
                }
            } else {
                let i = points.partition_point(|p| p.0 < t);
                let ((t0, v0), (t1, v1)) = (points[i - 1], points[i]);
                (v1 - v0).mul_add((t - t0) / (t1 - t0), v0)
            }
        };
        self.0.iter().map(|(r, p)| read(p, at(*r))).reduce(f64::min)
    }

    /// The hottest temperature (°C) any row is printed at.
    #[must_use]
    pub fn hottest(&self) -> Option<f64> {
        let last = self.0.iter().filter_map(|(_, p)| p.last().map(|l| l.0));
        last.reduce(f64::max)
    }
}

/// The current and power curves the stress monitor reads a card's limits from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Derating {
    pub current: Curve,
    pub power: Curve,
}

impl Derating {
    #[must_use]
    pub fn of(entry: &ModelEntry) -> Self {
        Self {
            current: curve(entry, "max_current_a", None, None),
            power: curve(entry, "max_power_w", None, None),
        }
    }

    /// The hottest temperature (°C) either curve is printed at.
    #[must_use]
    pub fn hottest(&self) -> Option<f64> {
        self.current
            .hottest()
            .into_iter()
            .chain(self.power.hottest())
            .reduce(f64::max)
    }
}

/// Every problem with the card's `rating`-tagged rows.
///
/// These are a key that is no rating, a row naming neither a temperature nor a
/// package or mounting, a value that cannot be read as its key, and a scalar
/// that is not the least favourable row.
#[must_use]
pub fn tag_problems(entry: &ModelEntry) -> Vec<String> {
    let scalars = toml::Table::try_from(&entry.ratings).unwrap_or_default();
    let (mut out, mut least) = (Vec::new(), BTreeMap::<&str, f64>::new());
    for row in entry.source.iter().flat_map(|s| &s.characteristic) {
        let Some(key) = row.rating.as_deref() else {
            continue;
        };
        let what = format!("characteristic '{}' (rating = \"{key}\")", row.parameter);
        if !is_rating_key(key) {
            out.push(format!("{what}: '{key}' is not a [models.ratings] key"));
            continue;
        }
        if temp_span(&row.conditions).is_none() && row.package.is_none() && row.mounting.is_none() {
            out.push(format!("{what}: name its temperature in conditions (TA, TC or TJ = ... °C), or its package or mounting"));
        }
        match rating_value(key, row) {
            Some(v) => {
                let worst = least.get(key).map_or(v, |w| worse(key, *w, v));
                least.insert(key, worst);
            }
            None => out.push(format!(
                "{what}: its unit or columns cannot be read as {key}"
            )),
        }
    }
    for (key, worst) in least {
        let scalar = scalars.get(key).and_then(toml::Value::as_float);
        if scalar.is_none_or(|s| (s - worst).abs() > 1e-9 * worst.abs()) {
            out.push(format!("ratings.{key} must be {worst}, the least favourable of its tagged rows: the scalar is the fallback, and Hauksbee reads the tagged rows at the scenario's temperature"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str) -> Option<(TempRef, f64, f64)> {
        temp_span(text)
    }

    #[test]
    fn temperatures_are_read_signed_with_their_reference() {
        use TempRef::{Ambient, Case, Junction};
        assert_eq!(span("TA = 70 °C, VGS = 10 V"), Some((Ambient, 70.0, 70.0)));
        assert_eq!(span("Tamb = −40 °C"), Some((Ambient, -40.0, -40.0)));
        assert_eq!(span("TC=100°C"), Some((Case, 100.0, 100.0)));
        assert_eq!(span("Tmb = 25 C"), Some((Case, 25.0, 25.0)));
        assert_eq!(span("TJ = −40 to 125 °C"), Some((Junction, -40.0, 125.0)));
        assert_eq!(span("−40 °C ≤ TA ≤ 85 °C"), Some((Ambient, -40.0, 85.0)));
        assert_eq!(span("VGS = −10 V, ID = −3 A"), None);
    }

    fn card(rows: &[(&str, f64, &str)]) -> ModelEntry {
        let rows = rows.iter().map(|(key, v, at)| format!("[[models.source.characteristic]]\nparameter = \"row\"\nrating = \"{key}\"\nmax = {v}\nunit = \"A\"\nconditions = \"{at}\"\nbasis = \"b\"\n")).collect::<Vec<_>>().concat();
        let text = format!("[[models]]\nid = \"q\"\nkind = \"nmos\"\ndescription = \"d\"\n[models.ratings]\nmax_current_a = 4.7\n[models.source]\ntier = \"datasheet-derived\"\n{rows}");
        let db: crate::schema::DbFile = toml::from_str(&text).expect("a card");
        db.models.into_iter().next().expect("one card")
    }

    #[test]
    fn rows_interpolate_hold_below_and_fall_to_zero_at_tj_max() {
        let entry = card(&[
            ("max_current_a", 5.7, "TA = 25 °C"),
            ("max_current_a", 4.7, "TA = 70 °C"),
        ]);
        let c = Derating::of(&entry).current;
        // Held below the coldest row, linear between rows, and past the
        // hottest row to zero at Tj,max (150 C here), not along the rows' slope.
        let expect = [
            (-40.0, 5.7),
            (25.0, 5.7),
            (47.5, 5.2),
            (70.0, 4.7),
            (110.0, 2.35),
            (160.0, 0.0),
        ];
        for (t, want) in expect {
            let got = c.limit(|_| t, 150.0).expect("rows");
            assert!((got - want).abs() < 1e-12, "{t} C: {got}");
        }
        assert_eq!(c.hottest(), Some(70.0));
        assert_eq!(tag_problems(&entry), Vec::<String>::new());
    }

    #[test]
    fn tagged_rows_are_validated() {
        let hot_scalar = card(&[("max_current_a", 5.7, "TA = 25 °C")]);
        assert!(tag_problems(&hot_scalar)[0].contains("must be 5.7"));
        let bad = card(&[
            ("max_current_a", 4.7, "VGS = 10 V"),
            ("max_amps", 1.0, "TA = 25 °C"),
        ]);
        let found = tag_problems(&bad);
        assert!(
            found[0].contains("name its temperature")
                && found[1].contains("not a [models.ratings] key"),
            "{found:?}"
        );
    }
}
