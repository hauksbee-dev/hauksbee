//! Temperature-dependent ratings: a card's printed rating rows (a MOSFET's ID at
//! TA = 25 °C and at TA = 70 °C), read at the temperature the scenario puts the
//! part at rather than collapsed to one number.
//!
//! Between printed rows the limit is interpolated linearly, which sits on or
//! below both the linear PD derating and the concave √-law ID derating. Past the
//! hottest row it falls linearly to zero at Tj,max, never along the rows' own
//! slope; below the coldest row it holds that row's value. Rows that cannot be
//! read as one continuous, non-rising curve leave the key to its scalar, the
//! least favourable row.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use hauksbee_ir::evidence::Assumption;
use regex::{Match, Regex};

use crate::package::package_body;
use crate::schema::{Characteristic, ModelEntry, Ratings};

/// Reserved component property naming the mounting (board, copper) a part is
/// rated for, matched against its card's `mounting`-tagged rows; the CI spec's
/// `[[override]] mounting` writes it.
pub const MOUNTING_PROPERTY: &str = "hauksbee_mounting";

/// The temperature a printed rating row is specified at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TempRef {
    Ambient,
    /// TC and the mounting-base temperature, read at the case.
    Case,
    /// Lead, solder-point, heatsink and board temperatures, read at Tj (an
    /// upper bound): the card carries no thermal resistance to them.
    Lead,
    Junction,
}

const NUM: &str = r"([-+]?\d+(?:\.\d+)?)\s*(?:°\s*C|℃|C\b|(K)\b)?";
const SYM: &str = r"\bT\s*_?(?i:(amb|a|case|c|mb|l|sp|sb|s|pcb|board|j))\b";
const OP: &str = r"\s*(≤|<=|<|≥|>=|>)\s*";

/// `TA = 25 °C`, `TC = 25 to 100 °C`, `TJ ≤ 150 °C`.
static SYMBOL_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    let range = format!(r"(?:\s*(?:to|\.\.\.?|…|~)\s*{NUM})?");
    let op = r"\s*(=|:|≤|<=|<|≥|>=|>)?\s*";
    Regex::new(&format!(r"{SYM}{op}{NUM}{range}")).expect("a valid pattern")
});
/// `−40 °C ≤ TA ≤ 85 °C`, `25 °C ≤ TA`.
static RANGE_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"{NUM}{OP}{SYM}(?:{OP}{NUM})?")).expect("a valid pattern")
});
/// A pulsed or time-limited row: `tp = 10 µs`, `T ≤ 10 s`, `duty cycle`,
/// `single pulse`; a bare `t` only before a time, never a temperature symbol.
static PULSED: LazyLock<Regex> = LazyLock::new(|| {
    let time = r"\bt\s*(?:≤|<=?|=)\s*\d+(?:\.\d+)?\s*(?:[mµun]?s|sec|min)\b";
    Regex::new(&format!(r"(?i)\bt_?p\b|{time}|duty|pulse")).expect("a valid pattern")
});

/// The reference and signed span (lo <= hi, °C) of the temperature a row's
/// conditions name; a single temperature is a span of one point, and kelvin
/// are read as °C.
///
/// TA or TC wins over a junction temperature named beside it. A junction
/// inequality (`TJ ≤ 175 °C`) is the part's bound, and an unpaired upper bound
/// (`TA < 70 °C`) the conditions' limit, not the row's temperature.
#[must_use]
pub fn temp_span(conditions: &str) -> Option<(TempRef, f64, f64)> {
    let text = conditions.replace(['−', '–', '‒'], "-");
    let read = |n: Option<Match>, k: Option<Match>| {
        let v: f64 = n?.as_str().parse().ok()?;
        Some(if k.is_some() { v - 273.15 } else { v })
    };
    let span = |sym: &str, lo: f64, hi: Option<f64>, op: Option<&str>, upper: bool| {
        let reference = match sym.to_ascii_lowercase().as_str() {
            "a" | "amb" => TempRef::Ambient,
            "c" | "case" | "mb" => TempRef::Case,
            "j" => TempRef::Junction,
            _ => TempRef::Lead,
        };
        let bound = op.is_some_and(|o| !matches!(o, "=" | ":"));
        let hi = hi.unwrap_or(lo);
        let kept = !upper && (reference != TempRef::Junction || !bound);
        kept.then_some((reference, lo.min(hi), lo.max(hi)))
    };
    let range_first = RANGE_FIRST.captures_iter(&text).filter_map(|c| {
        let hi = read(c.get(6), c.get(7));
        let upper = hi.is_none() && c[3].starts_with(['≥', '>']);
        span(&c[4], read(c.get(1), c.get(2))?, hi, Some(&c[3]), upper)
    });
    let symbol_first = SYMBOL_FIRST.captures_iter(&text).filter_map(|c| {
        let (op, hi) = (c.get(2).map(|o| o.as_str()), read(c.get(5), c.get(6)));
        let upper = hi.is_none() && op.is_some_and(|o| o.starts_with(['≤', '<']));
        span(&c[1], read(c.get(3), c.get(4))?, hi, op, upper)
    });
    range_first
        .chain(symbol_first)
        .min_by_key(|s| s.0 == TempRef::Junction)
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

/// `v` of `key` ordered so that lower is less favourable: a ceiling as is, a
/// thermal resistance negated.
fn rank(key: &str, v: f64) -> f64 {
    if key.starts_with("max_") {
        v
    } else {
        -v
    }
}

/// One rating's rows printed at one `(reference, package, mounting)`: their
/// `(°C, value)` points, sorted by temperature.
pub type Group = ((TempRef, Option<String>, Option<String>), Vec<(f64, f64)>);

/// One rating's printed rows by [`Group`]. Empty when no row is tagged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curve(pub Vec<Group>);

/// `key`'s rows tagged `rating = key` as a curve, with every reason they cannot
/// be trusted as one: a row with no readable temperature or value, a pulsed
/// row, a group that rises with temperature. A thermal resistance needs no
/// temperature.
fn read_rows(entry: &ModelEntry, key: &str) -> (Curve, Vec<String>) {
    let (mut problems, mut groups) = (Vec::new(), BTreeMap::<_, Vec<(f64, f64)>>::new());
    let rows = entry.source.iter().flat_map(|s| &s.characteristic);
    for row in rows.filter(|r| r.rating.as_deref() == Some(key)) {
        let what = format!("characteristic '{}' (rating = \"{key}\")", row.parameter);
        let unheated = (key.starts_with("theta_")).then_some((TempRef::Ambient, 25.0, 25.0));
        let span = temp_span(&row.conditions).or(unheated);
        let value = rating_value(key, row);
        if span.is_none() {
            problems.push(format!(
                "{what}: name its temperature in conditions (TA, TC or TJ = ... °C)"
            ));
        }
        if value.is_none() {
            problems.push(format!(
                "{what}: its unit or columns cannot be read as {key}"
            ));
        }
        if key.starts_with("max_") && PULSED.is_match(&row.conditions) {
            problems.push(format!(
                "{what}: a pulsed or time-limited row is no continuous rating; leave it untagged"
            ));
        }
        if let (Some((at, lo, hi)), Some(v)) = (span, value) {
            let points = groups.entry((at, row.package.clone(), row.mounting.clone()));
            points.or_default().extend([(lo, v), (hi, v)]);
        }
    }
    let curve = Curve(
        (groups.into_iter())
            .map(|(tags, mut points)| {
                // Two rows at one temperature: keep the least favourable.
                points.sort_by(|a, b| {
                    a.0.total_cmp(&b.0)
                        .then(rank(key, a.1).total_cmp(&rank(key, b.1)))
                });
                points.dedup_by(|later, kept| later.0.total_cmp(&kept.0).is_eq());
                (tags, points)
            })
            .collect(),
    );
    if (curve.0.iter()).any(|(_, p)| p.windows(2).any(|w| rank(key, w[1].1) > rank(key, w[0].1))) {
        problems.push(format!("rating = \"{key}\" rows rise with temperature: tag only rows printed under one set of conditions (one VGS, continuous)"));
    }
    (curve, problems)
}

/// `key`'s curve from the card's tagged rows; empty, leaving the key to its
/// scalar, when no row is tagged or any tagged row cannot be read as the curve.
#[must_use]
pub fn curve(entry: &ModelEntry, key: &str) -> Curve {
    let (curve, problems) = read_rows(entry, key);
    if problems.is_empty() {
        curve
    } else {
        Curve::default()
    }
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
        let reads = self.0.iter().map(|((r, ..), p)| read(p, at(*r)));
        reads.reduce(f64::min)
    }

    /// The hottest temperature (°C) any row is printed at.
    #[must_use]
    pub fn hottest(&self) -> Option<f64> {
        let last = self.0.iter().filter_map(|(_, p)| p.last().map(|l| l.0));
        last.reduce(f64::max)
    }
}

/// The current, power and `theta_JA` rows the stress monitor reads a card's
/// limits from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Derating {
    pub current: Curve,
    pub power: Curve,
    /// Read at no temperature: the Tj check takes the highest row left.
    pub theta_ja: Curve,
    /// What reading one fitted part's rows rests on ([`Self::fitted`]).
    pub caveats: Vec<Assumption>,
}

impl Derating {
    #[must_use]
    pub fn of(entry: &ModelEntry) -> Self {
        Self {
            current: curve(entry, "max_current_a"),
            power: curve(entry, "max_power_w"),
            theta_ja: curve(entry, "theta_ja_c_per_w"),
            caveats: Vec::new(),
        }
    }

    /// The rows that apply to `reference` fitted with `footprint` and, when its
    /// [`MOUNTING_PROPERTY`] names one, `mounting`.
    ///
    /// A row tagged with a package applies only when that package's body is the
    /// footprint's; untagged rows apply to every package and mounting. When the
    /// rows name packages and none is the footprint's, or name several
    /// mountings and none was chosen, every row is kept, so each limit is its
    /// least favourable row's, and a caveat says so; a chosen mounting no row
    /// names keeps them all too, and undermines the verdict until it is fixed.
    #[must_use]
    pub fn fitted(&self, reference: &str, footprint: &str, mounting: Option<&str>) -> Self {
        let body = |p: &str| package_body(p).map(|b| b.name);
        let (r, fits, mut caveats) = (reference, body(footprint), Vec::new());
        let curves = [&self.current, &self.power, &self.theta_ja];
        let groups = || curves.into_iter().flat_map(|c| &c.0);
        let named: BTreeSet<&str> = groups().filter_map(|((_, p, _), _)| p.as_deref()).collect();
        let matched = fits.is_some() && named.iter().any(|p| body(p) == fits);
        let on_package =
            |((_, p, _), _): &Group| !matched || p.as_deref().is_none_or(|p| body(p) == fits);
        let on_it = groups().filter(|g| on_package(g));
        let mountings: BTreeSet<&str> = on_it.filter_map(|((.., m), _)| m.as_deref()).collect();
        if !named.is_empty() && !matched {
            caveats.push(Assumption::partial_model_for_component(&format!("{r}-package"), r, "",
                &format!("{r}'s rating rows are printed for packages {named:?} and its footprint '{footprint}' is none of them, so each rating is its least favourable row"),
                "fit the footprint of the part's package, or tag its rows with that package, then re-run"));
        }
        let same = |m: &str| (mountings.iter()).find(|x| x.trim().eq_ignore_ascii_case(m.trim()));
        let chosen = mounting.map(|m| same(m).copied().ok_or(m));
        match chosen {
            Some(Err(m)) => caveats.push(Assumption::operating_limit_violated(&format!("{r}-mounting"), r,
                &format!("{r}'s {MOUNTING_PROPERTY} '{m}' names no mounting its card rates it for (it rates {mountings:?})"),
                &format!("set {r}'s mounting to one its card rates, or add that mounting's rows to the card, then re-run"))),
            None if mountings.len() > 1 => caveats.push(Assumption::default_parameter_for_component(r, r, "mounting",
                &format!("the least favourable of {mountings:?}"))),
            _ => {}
        }
        let chosen = chosen.and_then(Result::ok);
        let on_mounting =
            |((.., m), _): &Group| chosen.is_none_or(|c| m.as_deref().is_none_or(|x| x == c));
        let mut out = self.clone();
        out.caveats = caveats;
        for c in [&mut out.current, &mut out.power, &mut out.theta_ja] {
            c.0.retain(|g| on_package(g) && on_mounting(g));
        }
        out
    }

    /// The highest `theta_JA` (C/W) row left; `None` when no row is tagged.
    #[must_use]
    pub fn theta_ja(&self) -> Option<f64> {
        let rows = self.theta_ja.0.iter().flat_map(|(_, p)| p);
        rows.map(|p| p.1).reduce(f64::max)
    }

    /// The hottest temperature (°C) either curve is printed at.
    #[must_use]
    pub fn hottest(&self) -> Option<f64> {
        (self.current.hottest().into_iter())
            .chain(self.power.hottest())
            .reduce(f64::max)
    }
}

/// Every problem with the card's `rating`-tagged rows: a key that is no
/// rating, rows [`curve`] cannot read, and a scalar that is not the least
/// favourable row, rows the curve skips included.
#[must_use]
pub fn tag_problems(entry: &ModelEntry) -> Vec<String> {
    let scalars = toml::Table::try_from(&entry.ratings).unwrap_or_default();
    let rows = entry.source.iter().flat_map(|s| &s.characteristic);
    let mut keys: Vec<&str> = rows.filter_map(|r| r.rating.as_deref()).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut out = Vec::new();
    for key in keys {
        if !is_rating_key(key) {
            out.push(format!("rating = \"{key}\" is not a [models.ratings] key"));
            continue;
        }
        out.extend(read_rows(entry, key).1);
        let rows = entry.source.iter().flat_map(|s| &s.characteristic);
        let tagged = rows.filter(|r| r.rating.as_deref() == Some(key));
        let values = tagged.filter_map(|r| rating_value(key, r));
        let Some(worst) = values.min_by(|a, b| rank(key, *a).total_cmp(&rank(key, *b))) else {
            continue;
        };
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

    #[test]
    fn temperatures_are_read_signed_with_their_reference() {
        use TempRef::{Ambient, Case, Junction, Lead};
        let cases = [
            ("TA = 70 °C, VGS = 10 V", Some((Ambient, 70.0, 70.0))),
            ("Tamb = −40 °C", Some((Ambient, -40.0, -40.0))),
            ("TA = –40 °C", Some((Ambient, -40.0, -40.0))),
            ("TC=100°C", Some((Case, 100.0, 100.0))),
            ("Tmb = 25 C", Some((Case, 25.0, 25.0))),
            ("Tsp = 25 °C", Some((Lead, 25.0, 25.0))),
            ("TPCB = 70 °C", Some((Lead, 70.0, 70.0))),
            ("TJ = −40 to 125 °C", Some((Junction, -40.0, 125.0))),
            ("−40 °C ≤ TA ≤ 85 °C", Some((Ambient, -40.0, 85.0))),
            ("25 °C ≤ TA", Some((Ambient, 25.0, 25.0))),
            ("TA ≥ 85 °C", Some((Ambient, 85.0, 85.0))),
            ("TJ ≤ 175 °C, TC = 25 °C", Some((Case, 25.0, 25.0))),
            ("TA = 25 °C -10 V", Some((Ambient, 25.0, 25.0))),
            ("TJ ≤ 175 °C", None),
            ("−55 °C ≤ TJ ≤ 150 °C", None),
            ("VGS = −10 V, ID = −3 A", None),
            ("TA < 70 °C", None),
            ("85 °C ≥ TA", None),
            ("Tj = 423.15 K", Some((Junction, 150.0, 150.0))),
        ];
        for (text, want) in cases {
            assert_eq!(temp_span(text), want, "{text}");
        }
    }

    fn card(rows: &[(f64, &str)]) -> ModelEntry {
        let row = |(v, at): &(f64, &str)| {
            format!("[[models.source.characteristic]]\nparameter = \"ID\"\nrating = \"max_current_a\"\nmax = {v}\nunit = \"A\"\nconditions = \"{at}\"\nbasis = \"b\"\n")
        };
        let rows = rows.iter().map(row).collect::<Vec<_>>().concat();
        let text = format!("[[models]]\nid = \"q\"\nkind = \"nmos\"\ndescription = \"d\"\n[models.ratings]\nmax_current_a = 4.7\n[models.source]\ntier = \"datasheet-derived\"\n{rows}");
        let db: crate::schema::DbFile = toml::from_str(&text).expect("a card");
        db.models.into_iter().next().expect("one card")
    }

    #[test]
    fn rows_interpolate_hold_below_and_fall_to_zero_at_tj_max() {
        let entry = card(&[(5.7, "TA = 25 °C"), (4.7, "TA = 70 °C")]);
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
    fn rows_that_are_not_one_falling_continuous_curve_are_refused() {
        let cases: [(&[(f64, &str)], &str); 6] = [
            (&[(5.7, "TA = 25 °C")], "must be 5.7"),
            (&[(4.7, "VGS = 10 V")], "name its temperature"),
            (
                &[(4.7, "TA = 25 °C"), (5.0, "TA = 70 °C, T ≤ 10 s")],
                "pulsed",
            ),
            (&[(4.7, "TA = 25 °C"), (4.0, "VGS = 4.5 V")], "must be 4,"),
            (
                &[(4.7, "TA = 25 °C"), (4.9, "TA = 70 °C")],
                "rise with temperature",
            ),
            (
                &[
                    (4.7, "VGS = 4.5 V, TA = 25 °C"),
                    (5.7, "VGS = 10 V, TA = 25 °C"),
                    (4.9, "VGS = 10 V, TA = 70 °C"),
                ],
                "rise with temperature",
            ),
        ];
        for (rows, want) in cases {
            let entry = card(rows);
            let found = tag_problems(&entry);
            assert!(
                found.iter().any(|p| p.contains(want)),
                "{rows:?}: {found:?}"
            );
            let refused = !found.iter().all(|p| p.contains("must be"));
            assert!(
                !refused || Derating::of(&entry).current.0.is_empty(),
                "{rows:?} still read as a curve"
            );
        }
        let mut odd = card(&[(4.7, "TA = 25 °C")]);
        odd.source.as_mut().expect("source").characteristic[0].rating = Some("max_amps".into());
        assert!(tag_problems(&odd)[0].contains("not a [models.ratings] key"));
    }

    /// Builtin cards skip `validate`, so a curated row that cannot be read
    /// would silently leave its key to the scalar: every one must be clean.
    #[test]
    fn every_builtin_card_tags_its_rating_rows_readably() {
        for (name, src) in crate::BUILTIN_TOML_FILES {
            let db: crate::schema::DbFile = toml::from_str(src).expect(name);
            for entry in &db.models {
                let problems = tag_problems(entry);
                assert!(problems.is_empty(), "{name}: {}: {problems:?}", entry.id);
            }
        }
    }
}
