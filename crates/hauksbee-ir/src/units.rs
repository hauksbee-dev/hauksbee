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

/// Format `v` (base SI units) in engineering notation: three significant
/// digits, trailing zeros trimmed, ASCII `u` for micro. With a `unit` the
/// result is SI-typeset with a space (`4.7 uF`, `1 MHz`); with `""` it is the
/// bare, re-parseable form (`10.9k`, `18.2n`, `-1M`). Zero and non-finite
/// values print as Rust's plain `{v}`.
#[must_use]
pub fn format_engineering(v: f64, unit: &str) -> String {
    const SUFFIX: &[(f64, &str)] = &[
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "k"),
        (1.0, ""),
        (1e-3, "m"),
        (1e-6, "u"),
        (1e-9, "n"),
        (1e-12, "p"),
    ];
    let sep = if unit.is_empty() { "" } else { " " };
    if v == 0.0 || !v.is_finite() {
        return format!("{v}{sep}{unit}");
    }
    // Three significant digits on the scaled mantissa (which lives in [1, 1000)).
    let sig_digits = |m: f64| usize::from(m < 100.0) + usize::from(m < 10.0);
    let round_to = |m: f64, digits: usize| {
        let factor = [1.0, 10.0, 100.0][digits];
        (m * factor).round() / factor
    };
    let mag = v.abs();
    let mut idx = SUFFIX
        .iter()
        .position(|(s, _)| mag >= *s)
        .unwrap_or(SUFFIX.len() - 1);
    let mut mantissa = mag / SUFFIX[idx].0;
    let mut digits = sig_digits(mantissa);
    // Rounding can carry the mantissa to 1000 (999999 -> "1000k"): promote to
    // the next-larger suffix so it stays in [1, 1000). G has nothing above it.
    if round_to(mantissa, digits) >= 1000.0 && idx > 0 {
        idx -= 1;
        mantissa = mag / SUFFIX[idx].0;
        digits = sig_digits(mantissa);
    }
    let s = format!("{mantissa:.digits$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    let sign = if v < 0.0 { "-" } else { "" };
    format!("{sign}{s}{sep}{}{unit}", SUFFIX[idx].1)
}

#[cfg(test)]
mod tests {
    use super::{format_engineering, parse_quantity, split_prefix};

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
}
