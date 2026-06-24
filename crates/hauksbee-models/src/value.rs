//! Engineering-notation component value parser.
//!
//! Parses strings like `"10k"`, `"4k7"`, `"0.1uF"`, `"100n"`, `"2R2"`,
//! `"10MEG"`, `"22uH"`, `"4.7nF"` into a [`ParsedValue`] with a numeric
//! magnitude and optional unit string.
//!
//! The parser is intentionally lenient about case and spacing so it handles
//! real-world BOM entries.

use std::fmt;

/// A parsed component value.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedValue {
    /// Magnitude in base SI units (Ω, F, H, dimensionless, …).
    pub si: f64,
    /// Multiplier suffix that was found (e.g. `"k"`, `"u"`, `"M"`).
    pub suffix: Option<String>,
    /// Unit found after the multiplier (e.g. `"F"`, `"H"`, `"R"`, `"Ω"`).
    pub unit: Option<String>,
}

impl fmt::Display for ParsedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.si)?;
        if let Some(u) = &self.unit {
            write!(f, " {}", u)?;
        }
        Ok(())
    }
}

/// Parse a component value string into a [`ParsedValue`].
///
/// Returns `None` when the string cannot be interpreted as a numeric value
/// (e.g. `"NC"`, `"DNP"`, part-number strings, or empty strings).
///
/// # Examples
///
/// ```
/// use hauksbee_models::value::parse_value;
/// assert_eq!(parse_value("10k").unwrap().si, 10_000.0);
/// assert_eq!(parse_value("4k7").unwrap().si, 4_700.0);
/// assert!((parse_value("0.1uF").unwrap().si - 1e-7).abs() < 1e-20);
/// assert!((parse_value("100n").unwrap().si - 1e-7).abs() < 1e-20);
/// assert_eq!(parse_value("2R2").unwrap().si, 2.2);
/// assert_eq!(parse_value("10MEG").unwrap().si, 10_000_000.0);
/// assert_eq!(parse_value("22uH").unwrap().si, 22e-6);
/// ```
pub fn parse_value(s: &str) -> Option<ParsedValue> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Normalise: unicode units, and European comma-decimal separator.
    let cleaned = normalise_unicode(s);
    // Replace comma-decimal only when it looks like "5,1K" (digit-comma-digit),
    // not when comma is used as a thousands separator (rare in BOM values).
    let cleaned = normalise_comma_decimal(&cleaned);
    parse_inner(&cleaned)
}

/// Replace a single comma acting as a decimal separator (e.g. "5,1K" -> "5.1K").
fn normalise_comma_decimal(s: &str) -> String {
    // Only replace if there's exactly one comma and it's surrounded by digits
    let comma_count = s.bytes().filter(|&b| b == b',').count();
    if comma_count == 1 {
        let idx = s.find(',').unwrap();
        let before_comma = &s[..idx];
        let after_comma = &s[idx + 1..];
        // Check: last char before comma is digit AND first char after is digit
        let prev_digit = before_comma
            .chars()
            .next_back()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        let next_digit = after_comma
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        if prev_digit && next_digit {
            return format!("{}.{}", before_comma, after_comma);
        }
    }
    s.to_string()
}

/// Normalise unicode characters that appear in BOM values.
fn normalise_unicode(s: &str) -> String {
    s.replace('\u{00b5}', "u") // µ → u (micro sign)
        .replace('\u{03bc}', "u") // μ → u (greek mu)
        .replace('\u{03a9}', "R") // Ω → R
        .replace('\u{2126}', "R") // Ω (ohm sign) → R
}

/// Core parser, operating on an ASCII string (after unicode normalisation).
///
/// Grammar (case-insensitive after the leading digit):
/// ```text
/// value  = digits [ '.' digits ] [ suffix ] [ unit ]
///        | digits suffix digits unit?   ("4k7" style)
/// suffix = 'f' | 'p' | 'n' | 'u' | 'm' | 'k' | 'K' | 'meg' | 'M' | 'g' | 'R'
/// unit   = 'F' | 'H' | 'R' | 'OHM' | 'V' | 'A'   (optional, informational)
/// ```
fn parse_inner(s: &str) -> Option<ParsedValue> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    // --- consume leading digits (and optional leading decimal) ---------------
    let mut i = 0;
    // Allow leading sign
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // Optional decimal with digits after
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == start {
        // No leading digits — not a numeric value.
        return None;
    }
    let before = &s[..i];
    let rest = &s[i..];

    // --- try to parse suffix (and optional interleaved decimal part) ---------
    let (multiplier, suffix_str, after_suffix) = parse_suffix(rest);

    // Check for "4k7" style: suffix followed by digits (the fractional part)
    let (mantissa_str, unit) = if let Some(frac_start) = after_suffix
        .find(|c: char| c.is_ascii_digit())
        .filter(|_| !after_suffix.is_empty() && after_suffix.as_bytes()[0].is_ascii_digit())
    {
        // Collect the trailing digit group
        let frac_bytes = after_suffix.as_bytes();
        let mut j = 0;
        while j < frac_bytes.len() && frac_bytes[j].is_ascii_digit() {
            j += 1;
        }
        let frac_digits = &after_suffix[..j];
        let after_frac = &after_suffix[j..];
        let _ = frac_start; // suppress warning
                            // Rebuild as "4.7"
        let combined = format!("{}.{}", before, frac_digits);
        let unit = parse_unit(after_frac.trim());
        (combined, unit)
    } else {
        let unit = parse_unit(after_suffix.trim());
        (before.to_string(), unit)
    };

    let mantissa: f64 = mantissa_str.parse().ok()?;
    let si = mantissa * multiplier;

    // Convert to base SI unit based on the unit (if present)
    let si_final = apply_unit(si, unit.as_deref());

    Some(ParsedValue {
        si: si_final,
        suffix: suffix_str.map(|s| s.to_string()),
        unit,
    })
}

/// Parse an SI multiplier suffix. Returns `(multiplier, suffix_char, remainder)`.
fn parse_suffix(s: &str) -> (f64, Option<&str>, &str) {
    if s.is_empty() {
        return (1.0, None, s);
    }
    let upper = s.to_uppercase();
    // MEG must come before M to avoid shadowing
    if upper.starts_with("MEG") {
        return (1e6, Some("MEG"), &s[3..]);
    }
    // GIG / G
    if upper.starts_with("GIG") {
        return (1e9, Some("GIG"), &s[3..]);
    }
    match s.as_bytes()[0] {
        b'f' | b'F' => (1e-15, Some("f"), &s[1..]),
        b'p' | b'P' => (1e-12, Some("p"), &s[1..]),
        b'n' | b'N' => (1e-9, Some("n"), &s[1..]),
        b'u' | b'U' => (1e-6, Some("u"), &s[1..]),
        // 'm' is milli; 'M' alone (not MEG) is also mega in SPICE (we treat M
        // after digits conservatively: lowercase 'm' = milli, uppercase 'M' = mega).
        b'm' => (1e-3, Some("m"), &s[1..]),
        b'M' => (1e6, Some("M"), &s[1..]),
        b'k' | b'K' => (1e3, Some("k"), &s[1..]),
        b'g' | b'G' => (1e9, Some("G"), &s[1..]),
        b't' | b'T' => (1e12, Some("T"), &s[1..]),
        // 'R' as multiplier means ×1 (e.g. "2R2" = 2.2 Ω)
        b'R' | b'r' => (1.0, Some("R"), &s[1..]),
        _ => (1.0, None, s),
    }
}

/// Extract a trailing unit string. Returns `Some(unit)` or `None`.
fn parse_unit(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let upper = s.to_uppercase();
    // Strip leading whitespace already handled by the caller.
    let unit = if upper.starts_with("OHM") || upper.starts_with("OH") {
        "Ω"
    } else if upper.starts_with('R') {
        "Ω"
    } else if upper.starts_with('F') {
        "F"
    } else if upper.starts_with('H') {
        "H"
    } else if upper.starts_with('V') {
        "V"
    } else if upper.starts_with('A') {
        "A"
    } else {
        return None;
    };
    Some(unit.to_string())
}

/// Adjust SI value for unit-specific conversions.
/// Currently all base units map directly (Ω, F, H are already base SI);
/// this is a hook for future unit-aware scaling.
fn apply_unit(si: f64, _unit: Option<&str>) -> f64 {
    // All passive values are already in their base SI unit after multiplier
    // application (Ohms, Farads, Henries). No further conversion needed.
    si
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(s: &str, expected_si: f64) {
        let v = parse_value(s).unwrap_or_else(|| panic!("parse_value({:?}) returned None", s));
        let rel_err = (v.si - expected_si).abs() / expected_si.max(1e-30);
        assert!(
            rel_err < 1e-9,
            "parse_value({:?}) = {} (si={:.6e}), expected {:.6e} (rel_err={:.2e})",
            s,
            v,
            v.si,
            expected_si,
            rel_err
        );
    }

    #[test]
    fn test_basic() {
        check("10k", 10_000.0);
        check("10K", 10_000.0);
        check("100", 100.0);
        check("1.5", 1.5);
        check("0.1", 0.1);
    }

    #[test]
    fn test_multipliers() {
        check("1p", 1e-12);
        check("1n", 1e-9);
        check("1u", 1e-6);
        check("1m", 1e-3);
        check("1k", 1e3);
        check("1MEG", 1e6);
        check("1meg", 1e6);
        check("1M", 1e6);
        check("1G", 1e9);
    }

    #[test]
    fn test_interleaved_decimal() {
        // "4k7" style: suffix acts as decimal point
        check("4k7", 4_700.0);
        check("4K7", 4_700.0);
        check("2R2", 2.2);
        check("0R1", 0.1);
        check("1n5", 1.5e-9);
        check("2k2", 2_200.0);
        check("4M7", 4.7e6);
    }

    #[test]
    fn test_with_units() {
        check("10k", 10_000.0);
        check("0.1uF", 1e-7);
        check("100nF", 100e-9);
        check("100n", 100e-9);
        check("22uH", 22e-6);
        check("4.7nF", 4.7e-9);
        check("220uF", 220e-6);
        check("10nF", 10e-9);
        check("220nF", 220e-9);
        check("22uF/25V", 22e-6); // tolerate /25V suffix
    }

    #[test]
    fn test_resistor_values() {
        check("220", 220.0);
        check("470", 470.0);
        check("1K", 1_000.0);
        check("2.2K", 2_200.0);
        check("5,1K", 5_100.0); // comma as decimal separator (European BOM)
        check("6.2K", 6_200.0);
        check("10K", 10_000.0);
        check("22K", 22_000.0);
        check("62K", 62_000.0);
        check("10MEG", 10e6);
    }

    #[test]
    fn test_comma_decimal() {
        // European BOM format uses comma as decimal separator
        check("5,1K", 5_100.0);
        check("2,2K", 2_200.0);
    }

    #[test]
    fn test_unicode() {
        check("10µF", 10e-6);
        check("4.7μF", 4.7e-6);
    }

    #[test]
    fn test_non_values() {
        assert!(parse_value("NC").is_none());
        assert!(parse_value("DNP").is_none());
        assert!(parse_value("").is_none());
        assert!(parse_value("BC847").is_none());
    }

    #[test]
    fn test_edge_cases() {
        check("0", 0.0); // actually zero resistance (jumper)
        check("0R", 0.0);
        check("0R0", 0.0);
    }
}
