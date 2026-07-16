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
    // Chip-size codes are packages, not magnitudes: "0402" must never read as
    // 402 Ω, and in "0402_47k" the code is a naming prefix, not the value.
    let mut v: &str = &cleaned;
    while let Some(rest) = strip_size_code(v) {
        if rest.is_empty() {
            return None; // bare "0402" / "0603" / ... — not a value
        }
        v = rest;
    }
    // A JEDEC semiconductor part number ("1N4007", "1N5819", "2N3904", "2N7000")
    // is NOT a passive magnitude, but its `<digit>N<digits>` shape collides with
    // the RKM nano form: parse_inner reads "1N4007" as 1 + nano + ".4007" =
    // 1.4007 nF. Returning Some() there defeats the binder's generic-diode
    // fallback (which keys off parse_value() == None for non-passive values),
    // silently deleting a conducting rectifier/Schottky/zener path. A real RKM
    // value has a SHORT fractional part ("4n7", "1n5"); a JEDEC number is a single
    // leading digit, then N, then 3+ serial digits — reject that whole form.
    if is_jedec_semiconductor(v) {
        return None;
    }
    // A value expressed purely in volts ("5V1", "3V3", "12V") is not an R/C/L
    // magnitude — it is a zener/TVS breakdown or a rating. parse_inner would
    // read "5V1" as 5.0 V (silently dropping the ".1") and, worse, returning
    // Some() defeats the signal-diode fallback in the binder (which keys off
    // parse_value() == None for non-passive values), leaving the part open.
    // Return None so the diode/reference-class fallbacks handle it.
    parse_inner(v).filter(|p| p.unit.as_deref() != Some("V"))
}

/// A JEDEC semiconductor part number of the `<digit>N<serial>` family (1N4007,
/// 2N3904, 3N201): exactly one leading digit, then `N`/`n`, then 3 or more serial
/// digits and nothing else. The 3-digit floor keeps genuine RKM nano values
/// ("4n7", "1n5", "2n2" — 1–2 fractional digits) parsing normally while catching
/// the part numbers that would otherwise read as ~1.4 nF.
fn is_jedec_semiconductor(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() < 4 {
        return false;
    }
    if !b[0].is_ascii_digit() || b[0] == b'0' {
        return false;
    }
    let upper_n = b[1] == b'N';
    if !(upper_n || b[1] == b'n') {
        return false;
    }
    // The serial is a digit run optionally followed by a suffix letter (1N4148W,
    // 1N914B, 1N34A) — allow the trailing letters, require the rest all-digit.
    let serial = &b[2..];
    let ndigits = serial.iter().take_while(|c| c.is_ascii_digit()).count();
    if !serial[ndigits..].iter().all(u8::is_ascii_alphabetic) {
        return false;
    }
    // An UPPERCASE 'N' is the JEDEC spelling (1N34, 1N60, 1N21, 2N3904), so a
    // 2+-digit serial there is a part number and must return None (a bare
    // `parse_inner` would read it as the RKM nano form ~1.x nF and defeat the
    // binder's generic-diode fallback — the R33 failure mode, previously left
    // open for the short 2-digit germanium detectors). RKM nano values only ever
    // use LOWERCASE 'n' ("4n7", "1n5"), so a lowercase form still needs 3+ serial
    // digits before it is treated as a part number rather than a 1–2-digit value.
    let min_serial = if upper_n { 2 } else { 3 };
    ndigits >= min_serial
}

/// EIA imperial chip-size codes that leak into BOM value fields ("0402",
/// "0402_47k"). `"0402".parse::<f64>()` is 402 — silently accepting one binds
/// a 47 kΩ part at 402 Ω.
const FOOTPRINT_SIZE_CODES: [&str; 9] = [
    "0201", "0402", "0603", "0805", "1206", "1210", "1812", "2010", "2512",
];

/// If `s` starts with a chip-size code, return what follows: `Some("")` for a
/// bare code, `Some(rest)` when a `_`/`-`/space separator follows (the real
/// value). `None` when the leading digits are NOT a size code ("12065" and
/// "0402.5" are ordinary numbers).
fn strip_size_code(s: &str) -> Option<&str> {
    for code in FOOTPRINT_SIZE_CODES {
        if let Some(rest) = s.strip_prefix(code) {
            return match rest.chars().next() {
                None => Some(rest),
                Some('_') | Some('-') | Some(' ') | Some('\t') => {
                    Some(rest[1..].trim_start())
                }
                _ => None,
            };
        }
    }
    None
}

/// Normalise commas: a European decimal separator ("5,1K" -> "5.1K") vs a
/// thousands grouping ("10,000" -> "10000", "1,000,000" -> "1000000"). A
/// 3-digit group after the comma is treated as a thousands separator, a 1-2
/// digit group as a decimal — so "10,000" is 10000, not 10.0.
fn normalise_comma_decimal(s: &str) -> String {
    let comma_count = s.bytes().filter(|&b| b == b',').count();
    // Multiple commas are thousands grouping if each is followed by 3 digits.
    if comma_count > 1 {
        if is_thousands_grouped(s) {
            return s.replace(',', "");
        }
        return s.to_string();
    }
    if comma_count == 1 {
        let idx = s.find(',').unwrap();
        let before_comma = &s[..idx];
        let after_comma = &s[idx + 1..];
        let prev_digit = before_comma
            .chars()
            .next_back()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        // Length of the digit run immediately after the comma.
        let run = after_comma.chars().take_while(|c| c.is_ascii_digit()).count();
        if prev_digit && run >= 1 {
            // A single comma is a thousands separator for a grouped integer: a
            // nonzero integer part with no leading zero and a 3-digit group. The
            // presence of a trailing UNIT after the group does not change that —
            // "4,700uF" is 4700 uF, exactly as "4,700" is 4700. The leading-zero
            // guard (`int_grouped`) is what separates thousands from a European
            // decimal: "0,047uF" is 0.047 uF (leading-zero integer part), while
            // "4,700uF" is grouped. Requiring the group to be the whole string
            // used to mis-read every unit-suffixed grouped value as a decimal, a
            // 1000x under-count (4700 uF read as 4.7 uF).
            let int_grouped = before_comma.bytes().all(|b| b.is_ascii_digit())
                && before_comma.as_bytes().first() != Some(&b'0');
            if run == 3 && int_grouped {
                return format!("{}{}", before_comma, after_comma);
            }
            return format!("{}.{}", before_comma, after_comma);
        }
    }
    s.to_string()
}

/// True when every comma in `s` is followed by exactly three digits — the
/// signature of thousands grouping ("1,000,000").
fn is_thousands_grouped(s: &str) -> bool {
    for (i, b) in s.bytes().enumerate() {
        if b == b',' {
            let run = s[i + 1..].chars().take_while(|c| c.is_ascii_digit()).count();
            if run != 3 {
                return false;
            }
        }
    }
    true
}

/// EIA tolerance letters (F=±1%, G=±2%, J=±5%, K=±10%, M=±20%) collide with the
/// unit/multiplier letters. A lone trailing 'F' after a RESISTANCE-scale
/// multiplier is the ±1% tolerance code, not the Farad unit: "10KF" is a 10 kΩ
/// 1% resistor (not 10 kilofarad) and "100RF" is 100 Ω 1%. Sub-farad prefixes
/// (p/n/u/m) and a bare "1F" are genuine Farads and are left alone.
fn fixup_tolerance_unit(unit: Option<String>, suffix: Option<&str>) -> Option<String> {
    if unit.as_deref() != Some("F") {
        return unit;
    }
    match suffix {
        // 'R' already means ohms; the trailing F is tolerance → ohmic value.
        Some("R") => Some("Ω".to_string()),
        // Resistance-scale multipliers: F is a tolerance code, not a unit.
        Some("k") | Some("M") | Some("G") | Some("T") | Some("MEG") | Some("GIG") => None,
        // No multiplier, or a capacitance-scale prefix (p/n/u/m): genuine Farad.
        _ => unit,
    }
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
    // RKM / IEC 60062 leading-letter form: when the magnitude is < 1 the
    // decimal-point letter comes FIRST — "R47" = 0.47 Ω, "R1" = 0.1 Ω,
    // "R047" = 0.047 Ω (exactly the marking on a current-sense shunt). The
    // grammar below requires a leading digit, so this form was silently
    // rejected (returning None => the part read as an OPEN / vanished) even
    // though the middle-letter ("2R2", "4R7") and leading-zero ("0R47") forms
    // already parse. Rewrite the leading-R form to its "0R47" equivalent so it
    // reuses the already-correct path. Scoped to 'R' (ohms) with an immediately
    // following digit — the only unambiguous leading-letter marking — so a
    // unit-prefixed token is never mis-read.
    let rewritten;
    let s = {
        let b = s.as_bytes();
        let mut k = 0;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        if k + 1 < b.len() && (b[k] == b'R' || b[k] == b'r') && b[k + 1].is_ascii_digit() {
            rewritten = format!("{}0{}", &s[..k], &s[k..]);
            rewritten.as_str()
        } else {
            s
        }
    };
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
    // Optional scientific-notation exponent ("4.7e3", "1e-6"): e/E, optional
    // sign, one or more digits. Only consumed after a real mantissa (i > start)
    // and only when digits actually follow the 'e' — otherwise a lone 'e' is
    // left for the tail (no unit token begins with 'E', so it fails loudly
    // rather than silently eating a bad char). f64::parse handles the exponent.
    if i > start && i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_digits_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_digits_start {
            i = j;
        }
    }
    if i == start {
        // No leading digits — not a numeric value.
        return None;
    }
    let before = &s[..i];
    let rest = &s[i..];
    // A space between the number and a prefixed unit ("10 kΩ", "4.7 uF") is the
    // canonical SI typeset form and common in BOM exports; skip it so the SI
    // multiplier is still recognized (parse_suffix does not itself skip spaces,
    // so " kOhm" left the multiplier unread and emitted the value 10^n too small).
    // But do NOT skip a space that precedes another DIGIT ("10 5"): that is not a
    // single value and must stay rejected, never silently fused into "10.5".
    let rest = {
        let trimmed = rest.trim_start();
        if trimmed.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            rest
        } else {
            trimmed
        }
    };

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
        let mut unit = parse_tail(after_frac)?;
        if unit.is_none() {
            // An RKM decimal-point letter that is ITSELF a unit — H in "4H7",
            // F in "4F7" — leaves nothing after the fraction, so parse_tail
            // returns no unit and the H/F was silently dropped. The value then
            // read as unitless and downstream (parse_ohms) accepted a henry /
            // farad part as a resistance. Recover the unit from the suffix letter.
            unit = match suffix_str {
                Some("H") => Some("H".to_string()),
                Some("F") => Some("F".to_string()),
                _ => None,
            };
        }
        (combined, unit)
    } else {
        let unit = parse_tail(after_suffix)?;
        (before.to_string(), unit)
    };

    // Resolve the EIA tolerance-letter / unit collision: after a
    // resistance-scale multiplier a lone trailing 'F' is the ±1% tolerance
    // code, not the Farad unit.
    let unit = fixup_tolerance_unit(unit, suffix_str);

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
    // RKM decimal-point letters for inductance ("4H7" = 4.7 H) and large
    // capacitance ("4F7" = 4.7 F): the same middle-letter-as-decimal-point
    // convention "R" uses for resistance (IEC 60062), multiplier ×1. Gated on a
    // FOLLOWING digit so the bare UNIT forms ("1F", "10H", "F50V") still fall
    // through to parse_tail's unit recognition, and lowercase 'f' stays femto.
    let bytes = s.as_bytes();
    let next_is_digit = bytes.get(1).is_some_and(u8::is_ascii_digit);
    // For 'F' as an RKM decimal point ("4F7" = 4.7 F), make sure the digits after
    // 'F' are a genuine fractional part and not a voltage rating attached to a
    // bare-Farad value: "10F2V7" / "10F50V" are a 10 F supercap rated 2.7 V / 50 V,
    // NOT 10.2 F / 10.50 F. If a 'V' immediately follows the digit run, 'F' was the
    // Farad UNIT and the tail is a rating — fall through so parse_tail handles it,
    // matching the prefixed "10uF2V7" path (which never reaches this branch).
    let f_is_rkm_decimal = bytes[0] == b'F' && next_is_digit && {
        let mut j = 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        !matches!(bytes.get(j), Some(b'V') | Some(b'v'))
    };
    match bytes[0] {
        b'H' | b'h' if next_is_digit => return (1.0, Some("H"), &s[1..]),
        b'F' if f_is_rkm_decimal => return (1.0, Some("F"), &s[1..]),
        _ => {}
    }
    match s.as_bytes()[0] {
        // Lowercase 'f' is femto; UPPERCASE 'F' is the Farad UNIT, not a
        // multiplier. Consuming 'F' as femto turned a bare-Farad value ("1F",
        // a supercap) into 1e-15 F — off by 10^15. Femto essentially never
        // appears bare-uppercase in BOMs, so 'F' falls through to parse_tail,
        // which recognises it as the Farad unit (×1). ("100nF" is unaffected:
        // the explicit 'n' multiplier is consumed first, then 'F' is the unit.)
        b'f' => (1e-15, Some("f"), &s[1..]),
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

/// Parse the tail of a value string (everything after the numeric part and
/// SI suffix). Returns `Some(unit)` when the WHOLE tail is accounted for — a
/// unit token, an ignorable annotation (voltage rating, tolerance,
/// dielectric), or both — and `None` when unparsed garbage remains. The
/// caller must then reject the string: silently dropping the tail is exactly
/// how "0402_47k" once read as 402 Ω.
fn parse_tail(t: &str) -> Option<Option<String>> {
    if t.is_empty() {
        return Some(None);
    }
    let first = t.chars().next().unwrap();
    if first.is_whitespace() {
        // Space-separated tail: a whole unit word keeps its unit ("10k Ohm");
        // anything else is an annotation we ignore ("47k 1%", "22u X7R").
        let tt = t.trim_start();
        if let Some((unit, rest)) = unit_token(tt) {
            if tail_is_annotation(rest) {
                return Some(Some(unit));
            }
        }
        return Some(None);
    }
    if is_annotation_start(first) {
        return Some(None); // "/25V", ",25V", "@100MHz" rating-style tails
    }
    // A lone EIA tolerance-code letter directly attached after the value+
    // multiplier is a tolerance, not a unit: "4k7K" = 4.7 kΩ ±10%, "10RG" =
    // 10 Ω ±2%, "2k2J" = 2.2 kΩ ±5%, "1M0G" = 1 MΩ ±2%. G/J/K/M have no unit
    // meaning, so a lone one (optionally trailed by another annotation) is
    // ignorable. F is deliberately NOT handled here: it collides with the Farad
    // unit and is resolved by unit_token + fixup_tolerance_unit instead.
    if let Some(rest) = strip_eia_tolerance_letter(t) {
        if rest.is_empty()
            || rest.starts_with(char::is_whitespace)
            || rest.starts_with(is_annotation_start)
        {
            return Some(None);
        }
    }
    // Directly attached: must be a unit, optionally followed by an annotation
    // ("uF/25V") or a rating that starts with a digit ("F50V").
    let (unit, rest) = unit_token(t)?;
    if tail_is_annotation(rest) {
        return Some(Some(unit));
    }
    None
}

/// Extract a leading unit token. Returns the unit and the remainder after it.
fn unit_token(s: &str) -> Option<(String, &str)> {
    let upper = s.to_uppercase();
    let (unit, len) = if upper.starts_with("OHMS") {
        ("Ω", 4)
    } else if upper.starts_with("OHM") || upper.starts_with("OH") {
        ("Ω", if upper.starts_with("OHM") { 3 } else { 2 })
    } else if upper.starts_with('R') {
        ("Ω", 1)
    } else if upper.starts_with('F') {
        ("F", 1)
    } else if upper.starts_with('H') {
        ("H", 1)
    } else if upper.starts_with('V') {
        ("V", 1)
    } else if upper.starts_with('A') {
        ("A", 1)
    } else {
        return None;
    };
    Some((unit.to_string(), &s[len..]))
}

/// True when `rest` is empty or begins an ignorable trailing annotation.
fn tail_is_annotation(rest: &str) -> bool {
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || c.is_ascii_digit() || is_annotation_start(c),
    }
}

/// Characters that begin a tolerated trailing annotation (voltage rating,
/// tolerance, dielectric): the magnitude before them is already complete.
fn is_annotation_start(c: char) -> bool {
    matches!(c, '/' | ',' | ';' | '%' | '(' | '@' | '±' | '+')
}

/// If `t` begins with a lone EIA tolerance-code letter (G=±2%, J=±5%, K=±10%,
/// M=±20%), return the remainder after it; otherwise `None`. These only reach
/// [`parse_tail`] as a SECOND letter — the first SI multiplier is already
/// consumed by [`parse_suffix`] — so a leading k/m/g here is a tolerance code,
/// never a multiplier (a real "10k" never gets here with a leading 'k'). 'F' is
/// excluded: it means Farad and is handled by [`fixup_tolerance_unit`].
fn strip_eia_tolerance_letter(t: &str) -> Option<&str> {
    let mut chars = t.chars();
    match chars.next() {
        Some('G' | 'g' | 'J' | 'j' | 'K' | 'k' | 'M' | 'm') => Some(chars.as_str()),
        _ => None,
    }
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
    fn test_eia_tolerance_letters_are_not_units() {
        // RKM + EIA tolerance codes: the trailing letter is a tolerance, the
        // magnitude must still parse (regression for round-4 #1).
        check("4k7K", 4700.0); // 4.7 kΩ ±10%
        check("10RM", 10.0); // 10 Ω ±20%
        check("10RG", 10.0); // 10 Ω ±2%
        check("2k2J", 2200.0); // 2.2 kΩ ±5%
        check("1M0G", 1_000_000.0); // 1 MΩ ±2%
        check("100J", 100.0); // 100 Ω ±5%, no multiplier
        check("100RK", 100.0); // 100 Ω ±10%
        // The tolerance letter followed by a further annotation still parses.
        check("4k7K 1%", 4700.0);
        // Bare 'F' is still the Farad unit, not a tolerance code.
        check("1F", 1.0);
        check("4k7", 4700.0); // no tolerance letter: unchanged
    }

    #[test]
    fn test_rkm_henry_farad_decimal_letters() {
        // RKM middle-letter-as-decimal-point for inductors (H) and large caps
        // (F), like "2R2" for resistors (regression for round-6 F9): the
        // trailing digit is the fractional part, not dropped.
        check("4H7", 4.7); // 4.7 H
        check("4F7", 4.7); // 4.7 F
        check("1H5", 1.5); // 1.5 H
        check("2R2", 2.2); // resistor form still works
        // R36: the H/F decimal-letter IS the unit — it must not be dropped, or
        // downstream parse_ohms accepts a henry/farad part as a resistance. Only
        // the ohmic 'R' form is legitimately unitless.
        assert_eq!(parse_value("4H7").unwrap().unit.as_deref(), Some("H"));
        assert_eq!(parse_value("4F7").unwrap().unit.as_deref(), Some("F"));
        assert_eq!(parse_value("1H5").unwrap().unit.as_deref(), Some("H"));
        assert_eq!(parse_value("2R2").unwrap().unit.as_deref(), None);
        // Bare unit forms are NOT decimal letters (no following digit):
        check("10F", 10.0); // 10 farads
        check("1H", 1.0); // 1 henry
        assert_eq!(parse_value("10F").unwrap().unit.as_deref(), Some("F"));
        assert_eq!(parse_value("1H").unwrap().unit.as_deref(), Some("H"));
        // Prefix multipliers still win; lowercase 'f' stays femto:
        assert!((parse_value("100nF").unwrap().si - 1e-7).abs() < 1e-20);
        assert!((parse_value("4f7").unwrap().si - 4.7e-15).abs() < 1e-30);
    }

    #[test]
    fn bare_farad_with_attached_voltage_rating_parses_the_capacitance() {
        // Round-27: the F-as-RKM-decimal gate fired for ANY digit after 'F', so a
        // supercap written with an attached rating ("10F2V7" = 10 F / 2.7 V,
        // "10F50V" = 10 F / 50 V) mis-parsed the rating as a fractional Farad and
        // the pure-voltage filter then dropped the whole thing to None. The
        // capacitance must survive, matching the prefixed "10uF2V7" path.
        let p = parse_value("10F2V7").expect("10F2V7 is a 10 F supercap");
        assert!((p.si - 10.0).abs() < 1e-9, "si is 10 F, got {}", p.si);
        assert_eq!(p.unit.as_deref(), Some("F"));
        let p = parse_value("10F50V").expect("10F50V is a 10 F cap");
        assert!((p.si - 10.0).abs() < 1e-9, "si is 10 F, got {}", p.si);
        // The genuine RKM decimal is untouched: a digit run with no trailing 'V'.
        check("4F7", 4.7);
        check("10F", 10.0);
    }

    #[test]
    fn test_sampled_value_debug_format_round_trips_past_size_codes() {
        // Round-8 #8: a nominal like 1210 Ω formatted with `{}` becomes "1210",
        // which the parser reads as a 4-digit imperial footprint SIZE CODE and
        // rejects — so `apply_sampled_values` must serialize with `{:?}`, which
        // always emits a decimal point. Verify both halves of that reasoning.
        for si in [1210.0_f64, 1206.0, 2512.0, 2010.0, 1812.0] {
            let plain = format!("{si}");
            let dbg = format!("{si:?}");
            // `{}` collides with a size code → the parser rejects it.
            assert!(
                parse_value(&plain).is_none(),
                "format!(\"{{}}\", {si}) = {plain:?} is read as a size code (that was the bug)"
            );
            // `{:?}` carries a decimal point → parses back to the same value.
            let parsed = parse_value(&dbg)
                .unwrap_or_else(|| panic!("parse_value({dbg:?}) should round-trip"));
            assert!(
                (parsed.si - si).abs() < 1e-6,
                "{{:?}} round-trips: {dbg:?} -> {} (want {si})",
                parsed.si
            );
        }
    }

    #[test]
    fn test_european_decimal_comma_vs_thousands() {
        // A single comma with a 3-digit group is a thousands separator for a
        // grouped integer ("4,700" = 4700). What separates thousands from a
        // European decimal is the LEADING-ZERO integer part, not whether a unit
        // follows: "0,047uF" (leading zero) is 0.047 µF = 47 nF, while "4,700uF"
        // (no leading zero) is 4700 µF (round-7 #4).
        check("0,047uF", 47e-9); // 0.047 µF = 47 nF, NOT 47 µF
        check("0,022uF", 22e-9);
        check("0,1uF", 100e-9); // 1-2 digit group already worked
        check("4,7uF", 4.7e-6);
        check("5,1k", 5100.0); // 5.1 kΩ
        // Genuine thousands grouping (nonzero integer part) stays 1000x, and a
        // trailing unit does not demote it to a decimal (R34: the old
        // "group is the whole string" clause read "4,700uF" as 4.7 µF, 1000x low).
        check("4,700", 4700.0);
        check("4,700uF", 4.7e-3); // 4700 µF = 4.7 mF, NOT 4.7 µF
        check("2,200uF", 2.2e-3); // 2200 µF = 2.2 mF
        check("10,000", 10000.0);
        check("1,000,000", 1_000_000.0);
    }

    #[test]
    fn test_scientific_notation() {
        // Exponential notation is a common script/SPICE-exported numeric form
        // (regression for round-4 #2).
        check("4.7e3", 4700.0);
        check("1e-6", 1e-6);
        check("1E-6", 1e-6);
        check("2.2e-9", 2.2e-9);
        check("1e3", 1000.0);
        check("4.7e3F", 4700.0); // exponent then a unit (Farads)
        // A lone 'e' with no exponent digits is NOT a number.
        assert!(parse_value("4e").is_none());
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
    fn test_rkm_leading_letter_below_one_ohm() {
        // R18: the RKM leading-letter form (magnitude < 1) puts the decimal
        // letter first — the exact marking on a current-sense shunt. It was
        // silently rejected (None => read as an OPEN) while "0R47"/"2R2" parsed.
        check("R47", 0.47);
        check("R1", 0.1);
        check("R047", 0.047);
        check("r47", 0.47); // lowercase marking
        // The leading-zero and middle-letter equivalents still parse identically.
        check("0R47", 0.47);
        // A bare "R" with no following digit is not a value.
        assert!(parse_value("R").is_none(), "bare R is not a value");
        assert!(parse_value("R_LABEL").is_none(), "R + non-digit is not a value");
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
    fn jedec_diode_part_numbers_are_not_passive_values() {
        // R33: "1N4007" collided with the RKM nano form — parse_inner read it as
        // 1 + nano + ".4007" = ~1.4 nF, so parse_value returned Some(). That
        // defeated the binder's generic-diode fallback (which keys off
        // parse_value() == None), silently deleting a conducting rectifier /
        // Schottky / zener path. JEDEC 1N/2N part numbers must return None.
        for pn in ["1N4007", "1N5819", "1N914", "1N4733", "2N3904", "2N7000", "3N201"] {
            assert!(
                parse_value(pn).is_none(),
                "JEDEC part number {pn:?} must not parse as a passive value, got {:?}",
                parse_value(pn)
            );
        }
        // R38: short 2-digit-serial JEDEC diodes (germanium detectors) use the
        // same uppercase-N spelling and must ALSO return None — they were parsed
        // as ~1.x nF and their conducting path silently deleted.
        for pn in ["1N34", "1N60", "1N21", "1N34A"] {
            assert!(
                parse_value(pn).is_none(),
                "short JEDEC part number {pn:?} must not parse as a passive value, got {:?}",
                parse_value(pn)
            );
        }
        // The short RKM nano values it must NOT reject still parse correctly.
        // These use LOWERCASE 'n' — the discriminator that separates them from
        // the uppercase-N JEDEC parts above.
        check("4n7", 4.7e-9);
        check("1n5", 1.5e-9);
        check("2n2", 2.2e-9);
        check("100n", 100e-9);
    }

    #[test]
    fn test_edge_cases() {
        check("0", 0.0); // actually zero resistance (jumper)
        check("0R", 0.0);
        check("0R0", 0.0);
    }

    /// Bug regression: bare EIA chip-size codes must not read as magnitudes
    /// ("0402".parse::<f64>() is 402 — a 47 kΩ part bound at 402 Ω), and a
    /// leading size code is a naming prefix to strip, not the value.
    #[test]
    fn test_footprint_size_codes() {
        for c in [
            "0201", "0402", "0603", "0805", "1206", "1210", "1812", "2010", "2512",
        ] {
            assert!(
                parse_value(c).is_none(),
                "bare size code {c:?} must not parse as a magnitude"
            );
        }
        // Code + separator: the real value follows.
        check("0402_47k", 47_000.0);
        check("0603 100nF", 100e-9);
        check("0805-2k2", 2_200.0);
        // Genuine numbers that merely start like a code stay numbers.
        check("12065", 12_065.0);
        check("0402.5", 402.5);
    }

    /// Bug regression: the grammar must consume the WHOLE input. Trailing
    /// garbage silently dropped is how "0402_47k" once read as 402 Ω.
    #[test]
    fn test_trailing_garbage_rejected() {
        assert!(parse_value("10k_junk").is_none());
        assert!(parse_value("47kXYZ").is_none());
        assert!(parse_value("100n_47k").is_none());
        // Annotations (tolerance, rating, dielectric) are still tolerated.
        check("47k 1%", 47_000.0);
        check("22uF/25V", 22e-6);
        check("100nF 50V", 100e-9);
        check("22u X7R", 22e-6);
        check("10k Ohm", 10_000.0);
        check("600@100MHz", 600.0); // ferrite bead impedance@frequency
    }

    /// Round-29 (HIGH): a space BEFORE the SI multiplier ("10 kOhm") is the
    /// canonical typeset form, but parse_suffix never skipped it, so the
    /// multiplier was dropped and the value came out 10^n too small with the unit
    /// silently lost. The space-AFTER form ("10k Ohm") already worked; the two
    /// must agree.
    #[test]
    fn test_space_before_multiplier_keeps_the_scale() {
        check("10 kOhm", 10_000.0);
        check("4.7 kOhm", 4_700.0);
        check("1 MOhm", 1_000_000.0);
        check("10 uF", 10e-6);
        check("2.2 nF", 2.2e-9);
        // Space-after-multiplier still works (unchanged).
        check("10k Ohm", 10_000.0);
        // A bare unit after a space (no multiplier) is unaffected.
        check("10 Ohm", 10.0);
        // A space before another DIGIT must never be fused into a fractional value
        // ("10 5" must not become 10.5): the digit guard keeps the space, so the
        // magnitude stays 10 (the trailing token is ignored, as before the fix).
        let r = parse_value("10 5");
        assert!(
            r.map_or(true, |v| (v.si - 10.5).abs() > 1e-9),
            "'10 5' must not silently fuse into 10.5"
        );
    }

    /// Bug regression: a bare uppercase 'F' is the Farad unit, not the femto
    /// multiplier. "1F" (a supercap) must be 1 farad, not 1e-15.
    #[test]
    fn test_bare_farad_is_not_femto() {
        check("1F", 1.0);
        check("10F", 10.0);
        check("0.1F", 0.1);
        check("4.7F", 4.7);
        // Explicit-multiplier capacitances are unaffected (multiplier first).
        check("100nF", 100e-9);
        check("0.1uF", 0.1e-6);
        check("10pF", 10e-12);
    }

    /// Bug regression: a value expressed purely in volts is a zener/TVS rating,
    /// not an R/C/L magnitude. It must parse to None so the diode fallback
    /// handles it, instead of reading "5V1" as 5.0 (dropping the ".1").
    #[test]
    fn test_pure_voltage_codes_are_not_magnitudes() {
        for v in ["5V1", "3V3", "12V", "5V", "18V", "1V8"] {
            assert!(
                parse_value(v).is_none(),
                "pure-voltage value {v:?} must not parse as a magnitude"
            );
        }
        // A capacitance with a voltage RATING annotation still parses (unit F).
        check("22uF/25V", 22e-6);
        check("100nF 50V", 100e-9);
    }

    /// Bug regression: a 3-digit group after a comma is a thousands separator,
    /// not a decimal — "10,000" is 10000, not 10.0. 1-2 digits stay decimal.
    #[test]
    fn test_comma_thousands_vs_decimal() {
        check("10,000", 10_000.0);
        check("1,000,000", 1_000_000.0);
        check("5,1K", 5_100.0); // European decimal
        check("2,2uF", 2.2e-6);
    }

    /// Bug regression: an EIA tolerance letter 'F' after a resistance-scale
    /// multiplier is ±1%, not the Farad unit — "10KF" is a 10 kΩ resistor, not
    /// a 10-kilofarad capacitor.
    #[test]
    fn test_tolerance_letter_is_not_farad() {
        let r = parse_value("10KF").unwrap();
        assert_ne!(r.unit.as_deref(), Some("F"), "10KF must not be a capacitor");
        assert_eq!(r.si, 10_000.0);
        let r2 = parse_value("100RF").unwrap();
        assert_eq!(r2.unit.as_deref(), Some("Ω"), "100RF is 100 Ω 1%");
        assert_eq!(r2.si, 100.0);
        // Genuine capacitances (sub-farad prefixes, or bare) stay Farads.
        assert_eq!(parse_value("4.7uF").unwrap().unit.as_deref(), Some("F"));
        assert_eq!(parse_value("1F").unwrap().unit.as_deref(), Some("F"));
    }
}




