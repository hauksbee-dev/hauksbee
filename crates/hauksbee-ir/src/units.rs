//! Engineering values and units: the one prefix table, the one strict parser
//! for human-typed quantities, and the one formatter.
//!
//! # Two grammars, on purpose
//!
//! **SPICE decks** keep SPICE grammar ([`crate::parse_spice_number`]):
//! case-insensitive, `M`/`m` = milli, `MEG` = mega, `MIL` = 25.4e-6. Users
//! paste real decks, and a deck that means 1 mF by `1M` must not change
//! meaning when it reaches Hauksbee.
//!
//! **Everything a person types for Hauksbee itself** (CLI flags, spec TOML
//! strings, model cards, BOM value fields) uses engineering notation through
//! this module's [`split_prefix`] table: `M` = mega, `m` = milli, `MEG` (any
//! case) = mega, `u`/`µ`/`μ` = micro, `k`/`K` = kilo. That is the reading
//! every datasheet and BOM uses, and it is the only place the two grammars
//! disagree (SPICE `1M` is 1e-3, here it is 1e6). [`parse_quantity`] refuses
//! the spellings where the case of `m` is the only clue and the writer was
//! plainly not being careful with case (`1mhz`, `1MHZ` is fine), and callers
//! echo what they understood (for example `--ac` prints
//! `AC sweep 1 Hz to 1 MHz, 20 points/decade`), so a SPICE habit can never
//! silently turn a megahertz into a millihertz.
//!
//! BOM ingestion (`hauksbee_models::value`) layers its lenient real-world
//! rules (size codes, tolerance letters, rating tails) on the same prefix
//! table; it never asks the user, so it reads the one case it cannot refuse
//! (`16mhz` on a crystal) as mega and documents that there.
//!
//! # Output
//!
//! [`format_engineering`] is the one formatter: three significant digits,
//! trailing zeros trimmed, carry at a decade edge (`999.9k` prints `1M`).
//! Every figure a message states goes through this family
//! ([`format_volts`], [`format_amps`], [`format_watts`], [`format_celsius`],
//! [`format_percent`]): negative zero prints as `0`, a value that rounds to
//! nothing never keeps its sign, NaN and infinity are never printed as
//! numbers ([`NotFinite`], rendered as words), and the unit is always there.
//! CLI and CI text use ASCII `u` for micro so every printed value can be
//! pasted straight back into a flag or spec; the web is served
//! [`WEB_MICRO_SIGN`] (`µ`, U+00B5) and nothing else.

use std::fmt;

/// The micro sign the web UI renders (U+00B5 MICRO SIGN, not U+03BC Greek mu).
/// CLI and CI text print ASCII `u` instead; [`parse_quantity`] reads all three.
pub const WEB_MICRO_SIGN: char = '\u{00b5}';

/// Split a leading SI multiplier off `s`: `(scale, canonical symbol, rest)`.
///
/// Engineering reading, not SPICE: `MEG`/`GIG` in any case, then `f` (femto,
/// lowercase only: `F` is the farad), `p`/`P`, `n`/`N`, `u`/`U`/`µ`/`μ`, `m`
/// (milli), `M` (mega), `k`/`K`, `g`/`G`, `t`/`T`. `None` when `s` does not
/// start with one.
#[must_use]
pub fn split_prefix(s: &str) -> Option<(f64, &'static str, &str)> {
    let head = s.get(..3).map(str::to_ascii_uppercase);
    match head.as_deref() {
        Some("MEG") => return Some((1e6, "MEG", &s[3..])),
        Some("GIG") => return Some((1e9, "GIG", &s[3..])),
        _ => {}
    }
    let c = s.chars().next()?;
    let (scale, symbol) = match c {
        'f' => (1e-15, "f"),
        'p' | 'P' => (1e-12, "p"),
        'n' | 'N' => (1e-9, "n"),
        'u' | 'U' | '\u{00b5}' | '\u{03bc}' => (1e-6, "u"),
        'm' => (1e-3, "m"),
        'M' => (1e6, "M"),
        'k' | 'K' => (1e3, "k"),
        'g' | 'G' => (1e9, "G"),
        't' | 'T' => (1e12, "T"),
        _ => return None,
    };
    Some((scale, symbol, &s[c.len_utf8()..]))
}

/// Byte length of the leading decimal mantissa of `s`: optional sign, digits,
/// optional `.digits`, and an `e[+-]digits` exponent only when digits follow
/// the `e`. 0 when no digit or point follows the sign.
#[must_use]
pub fn mantissa_len(s: &str) -> usize {
    let b = s.as_bytes();
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let start = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let mut i = start + digits(start);
    if b.get(i) == Some(&b'.') {
        i += 1 + digits(i + 1);
    }
    if i > start && matches!(b.get(i), Some(b'e' | b'E')) {
        let exp = i + 1 + usize::from(matches!(b.get(i + 1), Some(b'+' | b'-')));
        let n = digits(exp);
        if n > 0 {
            i = exp + n;
        }
    }
    if i == start {
        0
    } else {
        i
    }
}

/// Why [`parse_quantity`] refused a value. The message names the input and
/// says what to type instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitError(String);

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnitError {}

/// Parse a human-typed quantity in engineering notation, in base SI units.
///
/// `unit` is what the value measures (`"Hz"`, `"V"`, `"Ω"`, or `""` for a
/// bare number); it may be written after the value in any case (`Ω` also as
/// `R`, `ohm`, `ohms`), or left off. Accepted: `1000`, `2.5e6`, `1k`, `1M`
/// (mega), `10MEG`, `4.7u`, `4.7µ`, `1 kHz`, and the RKM forms `2k2`, `4u7`,
/// `4R7` (`R` only for ohms).
///
/// # Errors
///
/// A missing number, a stray tail (`1kk`, `1x`), a unit other than `unit`,
/// or the ambiguous `m` before a carelessly cased unit (`1mhz`: write `1MHz`
/// for mega or `1mHz` for milli).
pub fn parse_quantity(s: &str, unit: &str) -> Result<f64, UnitError> {
    let t = s.trim();
    let bad = |why: &str| UnitError(format!("'{s}': {why}"));
    let n = mantissa_len(t);
    let mantissa = &t[..n];
    let Some(base) = mantissa.parse::<f64>().ok().filter(|_| n > 0) else {
        return Err(bad("expected a number such as 1k, 2.5e6, 10MEG or 4u7"));
    };
    let rest = t[n..].trim_start();
    // A multiplier, or `R` (x1) when the quantity is ohms: `47R`, `4R7`.
    let (scale, symbol, after) = if unit == "\u{03a9}" && rest.starts_with(['R', 'r']) {
        (1.0, "R", &rest[1..])
    } else {
        split_prefix(rest).unwrap_or((1.0, "", rest))
    };
    // An RKM decimal-point letter: `2k2`, `4u7`, `4R7`.
    let frac = after.bytes().take_while(u8::is_ascii_digit).count();
    let (value, tail) = if frac > 0 && !symbol.is_empty() && !mantissa.contains(['.', 'e', 'E']) {
        let joined = format!("{mantissa}.{}", &after[..frac]);
        (
            joined.parse::<f64>().map_err(|_| bad("not a number"))?,
            &after[frac..],
        )
    } else if symbol == "R" {
        (base, rest) // `47R`: the R is the ohm unit, not a decimal point
    } else {
        (base, after)
    };
    let tail = tail.trim_start();
    if !tail.is_empty() && !unit_matches(tail, unit) {
        let want = if unit.is_empty() {
            "no unit".to_string()
        } else {
            format!("'{unit}'")
        };
        return Err(bad(&format!("unexpected '{tail}' (expected {want})")));
    }
    if symbol == "m" && !tail.is_empty() && tail != unit {
        return Err(bad(&format!(
            "ambiguous: 'm' is milli; write 1M{unit} for mega or 1m{unit} for milli"
        )));
    }
    Ok(value * scale)
}

fn unit_matches(tail: &str, unit: &str) -> bool {
    if unit == "\u{03a9}" {
        return matches!(
            tail.to_ascii_lowercase().as_str(),
            "\u{03c9}" | "r" | "ohm" | "ohms"
        ) || tail == "\u{03a9}"
            || tail == "\u{2126}";
    }
    !unit.is_empty() && tail.eq_ignore_ascii_case(unit)
}

/// A number that cannot be stated as a quantity: it is not finite.
///
/// The formatters below refuse to print NaN or infinity as if it were a
/// reading. [`try_format_engineering`] and [`try_format_fixed`] hand the
/// caller this typed outcome so the message can say what happened in words
/// ("did not solve"); the infallible formatters render [`NotFinite::words`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotFinite {
    /// NaN: an undefined result.
    NotANumber,
    /// Positive or negative infinity: an unbounded result.
    Unbounded,
}

impl NotFinite {
    /// Why `v` has no figure, or `None` when it is finite.
    #[must_use]
    pub const fn of(v: f64) -> Option<Self> {
        if v.is_nan() {
            Some(Self::NotANumber)
        } else if v.is_infinite() {
            Some(Self::Unbounded)
        } else {
            None
        }
    }

    /// The reader-facing words for the missing figure.
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::NotANumber => "not a number",
            Self::Unbounded => "unbounded",
        }
    }
}

impl fmt::Display for NotFinite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.words())
    }
}

impl std::error::Error for NotFinite {}

/// Significant digits of [`format_engineering`] (bare, re-parseable values).
const ENGINEERING_SIG: usize = 3;
/// Significant digits of the measured-quantity formatters
/// ([`format_volts`], [`format_amps`], [`format_watts`]): one more than the
/// bare form, so a figure just past its limit does not print as the limit
/// itself (`1.004 A` against `1 A`).
const QUANTITY_SIG: usize = 4;

/// `s` with trailing fractional zeros trimmed (`"4.70"` -> `"4.7"`, `"390"` stays).
fn trim_fraction(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Whether a rendered magnitude is nothing but zeros (`"0"`, `"0.00"`).
fn is_zero_text(s: &str) -> bool {
    s.chars().all(|c| c == '0' || c == '.')
}

/// The shared engineering renderer: `micro` is the micro prefix to print.
fn render_engineering(v: f64, unit: &str, sig: usize, micro: &str) -> Result<String, NotFinite> {
    if let Some(why) = NotFinite::of(v) {
        return Err(why);
    }
    let prefixes: [(f64, &str); 8] = [
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "k"),
        (1.0, ""),
        (1e-3, "m"),
        (1e-6, micro),
        (1e-9, "n"),
        (1e-12, "p"),
    ];
    let sep = if unit.is_empty() { "" } else { " " };
    // `sig` significant digits on a mantissa that lives in [1, 1000).
    let decimals =
        |m: f64| sig.saturating_sub(1 + usize::from(m >= 10.0) + usize::from(m >= 100.0));
    let round_to = |m: f64, d: usize| {
        let factor = 10f64.powi(i32::try_from(d).unwrap_or(0));
        (m * factor).round() / factor
    };
    let mag = v.abs();
    // Below half the smallest prefix (a picounit) nothing is left to print: a
    // figure that small is zero, whichever sign it came with.
    if mag < prefixes[prefixes.len() - 1].0 / 2.0 {
        return Ok(format!("0{sep}{unit}"));
    }
    // Past the largest prefix (1000 G) a figure is absurd for any quantity a
    // board has; scientific notation keeps it readable instead of a wall of digits.
    if mag >= 1e12 {
        let s = format!("{mag:.*e}", sig - 1);
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let sign = if v < 0.0 { "-" } else { "" };
        return Ok(format!(
            "{sign}{}e{exp}{sep}{unit}",
            trim_fraction(mantissa)
        ));
    }
    let mut idx = prefixes
        .iter()
        .position(|(s, _)| mag >= *s)
        .unwrap_or(prefixes.len() - 1);
    let mut mantissa = mag / prefixes[idx].0;
    let mut digits = decimals(mantissa);
    // Rounding can carry the mantissa to 1000 (999999 -> "1000k"): promote to
    // the next-larger prefix so it stays in [1, 1000). G has nothing above it.
    if round_to(mantissa, digits) >= 1000.0 && idx > 0 {
        idx -= 1;
        mantissa = mag / prefixes[idx].0;
        digits = decimals(mantissa);
    }
    let s = format!("{mantissa:.digits$}");
    let s = trim_fraction(&s);
    // A value that rounds to nothing is zero, whatever sign it came with:
    // never "-0 A".
    if is_zero_text(s) {
        return Ok(format!("0{sep}{unit}"));
    }
    let sign = if v < 0.0 { "-" } else { "" };
    Ok(format!("{sign}{s}{sep}{}{unit}", prefixes[idx].1))
}

/// Format `v` (base SI units) in engineering notation, or say why it has no
/// figure.
///
/// Three significant digits, trailing zeros trimmed, ASCII `u` for micro. With a `unit` the
/// result is SI-typeset with a space (`4.7 uF`, `1 MHz`); with `""` it is the
/// bare, re-parseable form (`10.9k`, `18.2n`, `-1M`). Negative zero and any
/// value that rounds to nothing print as `0`; NaN and infinity are a typed
/// [`NotFinite`], never a printed number.
///
/// # Errors
///
/// [`NotFinite`] when `v` is NaN or infinite.
pub fn try_format_engineering(v: f64, unit: &str) -> Result<String, NotFinite> {
    render_engineering(v, unit, ENGINEERING_SIG, "u")
}

/// [`try_format_engineering`] for a message that has no other way to say it:
/// a non-finite `v` reads as words ([`NotFinite::words`]), never as a number.
#[must_use]
pub fn format_engineering(v: f64, unit: &str) -> String {
    try_format_engineering(v, unit).unwrap_or_else(|why| why.words().to_string())
}

/// [`format_engineering`] with the micro sign the web UI renders
/// ([`WEB_MICRO_SIGN`]) instead of ASCII `u`.
#[must_use]
pub fn format_engineering_web(v: f64, unit: &str) -> String {
    render_engineering(v, unit, ENGINEERING_SIG, &WEB_MICRO_SIGN.to_string())
        .unwrap_or_else(|why| why.words().to_string())
}

/// A measured quantity with its unit, or `missing` when it has no figure.
///
/// Engineering prefix chosen by magnitude (`mA` reads as `mA`, never `0.00 A`),
/// four significant digits, the unit always present, `0` for negative zero.
/// `missing` is the caller's words for "no figure here" (`"did not solve"`).
#[must_use]
pub fn format_quantity_or(v: f64, unit: &str, missing: &str) -> String {
    render_engineering(v, unit, QUANTITY_SIG, "u").unwrap_or_else(|_| missing.to_string())
}

/// Several quantities of one unit joined by `sep`.
///
/// The unit is stated once when every one prints with the same prefix
/// (`1.7..3.6 V`, `1.2/1.5/1.8 A`), and on each one otherwise (`0 V..50 mV`).
/// A value with no figure reads as words.
#[must_use]
pub fn format_series(values: &[f64], sep: &str, unit: &str) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|v| format_quantity_or(*v, unit, NotFinite::NotANumber.words()))
        .collect();
    let suffixes: Vec<Option<&str>> = parts
        .iter()
        .map(|p| p.rsplit_once(' ').map(|(_, u)| u))
        .collect();
    let shared =
        suffixes.first().copied().flatten().is_some() && suffixes.iter().all(|u| *u == suffixes[0]);
    let last = parts.len().saturating_sub(1);
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| match p.rsplit_once(' ') {
            Some((figure, _)) if shared && i < last => figure,
            _ => p.as_str(),
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// A number a message can state: an `f64`, or a reference to one, so a
/// formatter takes whatever a caller has in hand (`*v`, `v`, `&v`).
pub trait Figure: Copy {
    /// The value.
    fn figure(self) -> f64;
}

impl Figure for f64 {
    fn figure(self) -> f64 {
        self
    }
}

impl Figure for &f64 {
    fn figure(self) -> f64 {
        *self
    }
}

impl Figure for &&f64 {
    fn figure(self) -> f64 {
        **self
    }
}

/// Volts for a message: `3.3 V`, `50 mV`, `0 V`; see [`format_quantity_or`].
#[must_use]
pub fn format_volts(v: impl Figure) -> String {
    format_quantity_or(v.figure(), "V", NotFinite::NotANumber.words())
}

/// Amps for a message: `1.5 A`, `12.5 mA`, `4.995 kA`; see [`format_quantity_or`].
#[must_use]
pub fn format_amps(a: impl Figure) -> String {
    format_quantity_or(a.figure(), "A", NotFinite::NotANumber.words())
}

/// Watts for a message: `1.5 W`, `250 mW`; see [`format_quantity_or`].
#[must_use]
pub fn format_watts(w: impl Figure) -> String {
    format_quantity_or(w.figure(), "W", NotFinite::NotANumber.words())
}

/// `v` to `decimals` decimals with `unit` (a space before it, none before
/// `%`), or why it has no figure.
///
/// For quantities with no engineering prefix (degrees Celsius, percent). A
/// value that rounds to zero at this precision prints as `0` with no sign
/// (never `-0%`); `signed` writes an explicit `+` on positive values. With
/// `trim`, trailing fractional zeros go (`125 C`, `91.7 C`).
///
/// # Errors
///
/// [`NotFinite`] when `v` is NaN or infinite.
pub fn try_format_fixed(
    v: f64,
    decimals: usize,
    unit: &str,
    signed: bool,
    trim: bool,
) -> Result<String, NotFinite> {
    if let Some(why) = NotFinite::of(v) {
        return Err(why);
    }
    let body = format!("{:.decimals$}", v.abs());
    let body = if trim { trim_fraction(&body) } else { &body };
    let sign = if is_zero_text(body) {
        ""
    } else if v < 0.0 {
        "-"
    } else if signed {
        "+"
    } else {
        ""
    };
    let sep = if unit.is_empty() || unit == "%" {
        ""
    } else {
        " "
    };
    Ok(format!("{sign}{body}{sep}{unit}"))
}

/// A bare number to at most `decimals` decimals, trailing zeros trimmed.
///
/// `3.3`, `125`, never `-0`; a non-finite value reads as words. For the figure
/// in a range or a list whose unit is stated once (`1.7-3.6 V`).
#[must_use]
pub fn format_number(v: impl Figure, decimals: usize) -> String {
    try_format_fixed(v.figure(), decimals, "", false, true)
        .unwrap_or_else(|why| why.words().to_string())
}

/// A temperature as every report prints it: `91.7 C`, one decimal at most,
/// a space before the unit, never `-0 C`; a non-finite value reads as words.
#[must_use]
pub fn format_celsius(celsius: impl Figure) -> String {
    format_celsius_to(celsius, 1)
}

/// [`format_celsius`] to at most `decimals` decimals (trailing zeros trimmed):
/// a ceiling the user typed (`125.25 C`) keeps the figures they gave.
#[must_use]
pub fn format_celsius_to(celsius: impl Figure, decimals: usize) -> String {
    try_format_fixed(celsius.figure(), decimals, "C", false, true)
        .unwrap_or_else(|why| why.words().to_string())
}

/// A copper clearance gap in millimetres as every report prints it: three
/// decimals (`0.153 mm`), but a gap that is above zero and too small for three
/// decimals reads `< 0.001 mm`, never `0.000 mm`.
///
/// `0.000 mm` means the copper touches. A gap between the touch band
/// (`SHORT_TOUCH_EPS_MM`, 1e-9 mm) and half a micrometre is not touching, and
/// printing it as `0.000 mm` would read as a short that is not one (or hide a
/// real, tiny gap). An exactly-zero or negative gap (contact or overlap) keeps
/// its fixed-decimal figure; a non-finite value reads as words.
#[must_use]
pub fn format_gap_mm(gap_mm: impl Figure) -> String {
    let gap = gap_mm.figure();
    if gap > 0.0 && gap < 0.0005 {
        return "< 0.001 mm".to_string();
    }
    try_format_fixed(gap, 3, "mm", false, false).unwrap_or_else(|why| why.words().to_string())
}

/// A percentage to `decimals` decimals (`12.5%`), never `-0%`; a non-finite
/// value reads as words.
#[must_use]
pub fn format_percent(pct: impl Figure, decimals: usize) -> String {
    try_format_fixed(pct.figure(), decimals, "%", false, false)
        .unwrap_or_else(|why| why.words().to_string())
}

/// [`format_percent`] with an explicit `+` on positive deviations (`+3%`,
/// `-12%`); a deviation that rounds to zero is `0%`, not `-0%`.
#[must_use]
pub fn format_percent_signed(pct: impl Figure, decimals: usize) -> String {
    try_format_fixed(pct.figure(), decimals, "%", true, false)
        .unwrap_or_else(|why| why.words().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        format_amps, format_celsius, format_engineering, format_engineering_web, format_gap_mm,
        format_number, format_percent, format_percent_signed, format_quantity_or, format_series,
        format_volts, format_watts, parse_quantity, split_prefix, try_format_engineering,
        try_format_fixed, NotFinite,
    };

    #[test]
    fn gap_mm_never_prints_a_nonzero_gap_as_zero() {
        // Touching (exactly zero) and overlap keep their fixed figure.
        assert_eq!(format_gap_mm(0.0), "0.000 mm");
        assert_eq!(format_gap_mm(-0.0), "0.000 mm");
        // Anything above zero that three decimals would round to 0.000 is
        // called out: the band between the touch epsilon (1e-9 mm) and 5e-7 mm
        // is a real, non-touching gap.
        for gap in [1e-12, 2e-9, 1e-8, 1e-7, 4.9e-7, 4.99e-4] {
            assert_eq!(format_gap_mm(gap), "< 0.001 mm", "{gap}");
        }
        // Gaps that round to a visible figure keep three decimals.
        assert_eq!(format_gap_mm(0.0006), "0.001 mm");
        assert_eq!(format_gap_mm(0.127), "0.127 mm");
        assert_eq!(format_gap_mm(0.2), "0.200 mm");
        // Non-finite reads as words, not a number.
        assert_eq!(format_gap_mm(f64::NAN), NotFinite::NotANumber.words());
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * b.abs().max(1e-30)
    }

    /// Every accepted human form, per consumer: `--ac` frequencies (Hz),
    /// resistances (RKM), and bare numbers.
    #[test]
    fn accepted_forms_table() {
        let table: &[(&str, &str, f64)] = &[
            // --ac and any other frequency flag
            ("1", "Hz", 1.0),
            ("10", "Hz", 10.0),
            ("1e6", "Hz", 1e6),
            ("2.5e6", "Hz", 2.5e6),
            ("1k", "Hz", 1e3),
            ("1K", "Hz", 1e3),
            ("1M", "Hz", 1e6),
            ("1MEG", "Hz", 1e6),
            ("10meg", "Hz", 10e6),
            ("1Meg", "Hz", 1e6),
            ("1G", "Hz", 1e9),
            ("1MHz", "Hz", 1e6),
            ("1MHZ", "Hz", 1e6),
            ("1 kHz", "Hz", 1e3),
            ("32.768kHz", "Hz", 32_768.0),
            ("1mHz", "Hz", 1e-3),
            ("1m", "Hz", 1e-3),
            ("100hz", "Hz", 100.0),
            // resistances
            ("4R7", "\u{03a9}", 4.7),
            ("2k2", "\u{03a9}", 2200.0),
            ("10k", "\u{03a9}", 1e4),
            ("10kohm", "\u{03a9}", 1e4),
            ("10k\u{03a9}", "\u{03a9}", 1e4),
            ("1M\u{2126}", "\u{03a9}", 1e6),
            ("47R", "\u{03a9}", 47.0),
            // capacitances and micro spellings
            ("4u7", "F", 4.7e-6),
            ("4.7uF", "F", 4.7e-6),
            ("4.7\u{00b5}F", "F", 4.7e-6),
            ("4.7\u{03bc}F", "F", 4.7e-6),
            ("100n", "F", 100e-9),
            ("10p", "F", 10e-12),
            ("1f", "F", 1e-15),
            // bare numbers and signs
            ("-3.3", "", -3.3),
            ("+5", "", 5.0),
            (".5", "", 0.5),
            ("2k2", "", 2200.0),
        ];
        for &(s, unit, want) in table {
            let got = parse_quantity(s, unit).unwrap_or_else(|e| panic!("{s} [{unit}]: {e}"));
            assert!(close(got, want), "{s} [{unit}] = {got}, want {want}");
        }
    }

    /// Refused forms: garbage, wrong units, the SPICE-only `mil`, and the
    /// ambiguous careless-case milli.
    #[test]
    fn refused_and_ambiguous_forms() {
        let table: &[(&str, &str, &str)] = &[
            ("", "Hz", "expected a number"),
            ("banana", "Hz", "expected a number"),
            ("1kk", "Hz", "unexpected"),
            ("1x", "Hz", "unexpected"),
            ("1mil", "Hz", "unexpected"),
            ("5V", "Hz", "unexpected"),
            ("4R7", "Hz", "unexpected"),
            ("1mhz", "Hz", "ambiguous"),
            ("1mHZ", "Hz", "ambiguous"),
            ("10 5", "", "unexpected"),
            ("1.5k2", "", "unexpected"),
        ];
        for &(s, unit, why) in table {
            let err = parse_quantity(s, unit).expect_err(s).to_string();
            assert!(err.contains(why), "{s}: {err}");
        }
    }

    /// The one place the human grammar and SPICE disagree: `M`.
    #[test]
    fn capital_m_is_mega_here_and_milli_in_spice() {
        assert!(close(parse_quantity("1M", "Hz").unwrap(), 1e6));
        assert!(close(crate::parse_spice_number("1M").unwrap(), 1e-3));
        assert_eq!(split_prefix("MEGx").map(|p| p.1), Some("MEG"));
        assert_eq!(split_prefix("Fx"), None, "F is the farad, not femto");
    }

    #[test]
    fn golden_format_strings() {
        let table: &[(f64, &str, &str)] = &[
            (10_900.0, "", "10.9k"),
            (1.82e-8, "", "18.2n"),
            (5.0, "", "5"),
            (999_999.0, "", "1M"),
            (-999_999.0, "", "-1M"),
            (999.6, "", "1k"),
            (4700.0, "", "4.7k"),
            (10_000.0, "", "10k"),
            (4.7e-6, "F", "4.7 uF"),
            (1.0, "Hz", "1 Hz"),
            (1e6, "Hz", "1 MHz"),
            (32_768.0, "Hz", "32.8 kHz"),
            (0.0, "V", "0 V"),
            (2.5e9, "", "2.5G"),
        ];
        for &(v, unit, want) in table {
            assert_eq!(format_engineering(v, unit), want, "{v} {unit}");
        }
    }

    /// Every bare formatted value parses back to itself (three digits).
    #[test]
    fn format_round_trips_through_parse() {
        for v in [1.0, 4.7e-6, 2.2e3, 1e6, 3.3e-12, 47.0, 1.5e9] {
            let back = parse_quantity(&format_engineering(v, ""), "").unwrap();
            assert!(close(back, v), "{v}");
        }
    }

    /// Negative zero and values that round to nothing print as a plain `0`.
    #[test]
    fn negative_zero_and_tiny_negatives_print_as_zero() {
        assert_eq!(format_engineering(-0.0, "V"), "0 V");
        assert_eq!(format_engineering(-0.0, ""), "0");
        assert_eq!(format_volts(-0.0), "0 V");
        assert_eq!(format_amps(-0.0), "0 A");
        // Below the smallest prefix it rounds away, sign and all.
        assert_eq!(format_amps(-1e-15), "0 A");
        assert_eq!(format_engineering(-1e-15, "A"), "0 A");
        assert_eq!(format_percent(-0.0, 0), "0%");
        assert_eq!(format_percent_signed(-0.4, 0), "0%");
        assert_eq!(format_percent_signed(-0.004, 1), "0.0%");
        assert_eq!(format_celsius(-0.0), "0 C");
        assert_eq!(format_celsius(-0.04), "0 C");
        // A real negative keeps its sign.
        assert_eq!(format_engineering(-1.5e-3, "A"), "-1.5 mA");
        assert_eq!(format_percent_signed(-12.4, 0), "-12%");
        assert_eq!(format_celsius(-5.0), "-5 C");
    }

    /// NaN and infinity are a typed outcome, never a printed number.
    #[test]
    fn non_finite_values_are_words_never_numbers() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(try_format_engineering(v, "A").is_err(), "{v}");
            assert!(try_format_fixed(v, 1, "C", false, true).is_err(), "{v}");
            for text in [
                format_engineering(v, "A"),
                format_engineering(v, ""),
                format_engineering_web(v, "F"),
                format_volts(v),
                format_amps(v),
                format_watts(v),
                format_celsius(v),
                format_percent(v, 1),
                format_percent_signed(v, 1),
            ] {
                let lower = text.to_ascii_lowercase();
                assert!(
                    !lower.contains("nan") && !lower.contains("inf"),
                    "{v} printed as {text:?}"
                );
            }
        }
        assert_eq!(NotFinite::of(f64::NAN), Some(NotFinite::NotANumber));
        assert_eq!(NotFinite::of(f64::NEG_INFINITY), Some(NotFinite::Unbounded));
        assert_eq!(NotFinite::of(1.0), None);
        assert_eq!(
            format_quantity_or(f64::NAN, "V", "did not solve"),
            "did not solve"
        );
        assert_eq!(
            format_quantity_or(f64::INFINITY, "A", "did not solve"),
            "did not solve"
        );
        assert_eq!(format_quantity_or(3.3, "V", "did not solve"), "3.3 V");
    }

    /// The prefix follows the magnitude and the unit is always carried.
    #[test]
    fn quantities_pick_a_prefix_and_carry_the_unit() {
        let table: &[(&dyn Fn() -> String, &str)] = &[
            (&|| format_amps(1e-3), "1 mA"),
            (&|| format_amps(0.0125), "12.5 mA"),
            (&|| format_amps(1.5), "1.5 A"),
            (&|| format_amps(4995.0), "4.995 kA"),
            (&|| format_amps(2.5e-6), "2.5 uA"),
            (&|| format_amps(-3e20), "-3e20 A"),
            (&|| format_amps(1.5e12), "1.5e12 A"),
            (&|| format_amps(1.004), "1.004 A"),
            (&|| format_volts(3.3), "3.3 V"),
            (&|| format_volts(0.05), "50 mV"),
            (&|| format_volts(12.0), "12 V"),
            (&|| format_volts(-5.0), "-5 V"),
            (&|| format_watts(0.25), "250 mW"),
            (&|| format_watts(24_950.0), "24.95 kW"),
            (&|| format_celsius(91.74), "91.7 C"),
            (&|| format_celsius(125.0), "125 C"),
            (&|| format_series(&[1.7, 3.6], "..", "V"), "1.7..3.6 V"),
            (&|| format_series(&[-0.0, 0.05], "..", "V"), "0 V..50 mV"),
            (
                &|| format_series(&[1.2, 1.5, 1.8], "/", "A"),
                "1.2/1.5/1.8 A",
            ),
            (&|| format_number(-0.0, 2), "0"),
            (&|| format_number(3.30, 2), "3.3"),
            (&|| format_percent(12.5, 1), "12.5%"),
            (&|| format_percent(50.0, 0), "50%"),
            (&|| format_percent_signed(3.4, 0), "+3%"),
            (&|| format_engineering_web(4.7e-6, "F"), "4.7 \u{b5}F"),
        ];
        for (f, want) in table {
            assert_eq!(&f(), want);
        }
    }
}
