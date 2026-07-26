//! User-supplied SPICE model parser.
//!
//! Parses `.model` and `.subckt` cards from a SPICE netlist file so users can
//! drop in vendor SPICE models (e.g. downloaded from a manufacturer's website).
//!
//! The parser is intentionally minimal: it stores the raw card text plus
//! structured metadata (name, kind, pin list), not a full SPICE evaluator.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// A single SPICE `.model` or `.subckt` card as loaded from a user file.
#[derive(Debug, Clone)]
pub struct SpiceCard {
    /// The model / subckt name (e.g. `"BC847"`, `"MYOPAMP"`).
    pub name: String,
    /// Card type.
    pub kind: SpiceCardKind,
    /// Raw source text (the original card including continuation lines).
    pub raw: String,
    /// For `.subckt`: port/pin names in declaration order.
    pub ports: Vec<String>,
    /// For `.model`: parsed parameter key=value pairs.
    pub params: BTreeMap<String, f64>,
    /// For `.model`: SPICE model type string (e.g. `"NPN"`, `"D"`, `"NMOS"`).
    pub model_type: Option<String>,
}

/// SPICE card type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpiceCardKind {
    Model,
    Subckt,
}

/// Parse all `.model` and `.subckt` cards from a SPICE file.
pub fn parse_spice_file(path: &Path) -> Result<Vec<SpiceCard>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading SPICE file {}", path.display()))?;
    parse_spice_text(&text)
}

/// Parse SPICE model cards from a string.
pub fn parse_spice_text(text: &str) -> Result<Vec<SpiceCard>> {
    let lines = join_continuation_lines(text);
    let mut cards = Vec::new();

    for line in &lines {
        let trimmed = line.trim();
        // Skip blank lines and comments
        if trimmed.is_empty() || trimmed.starts_with('*') || trimmed.starts_with('$') {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if lower.starts_with(".model") {
            if let Some(card) = parse_dot_model(trimmed) {
                cards.push(card);
            }
        } else if lower.starts_with(".subckt") {
            if let Some(card) = parse_dot_subckt(trimmed, &lines) {
                cards.push(card);
            }
        }
    }

    Ok(cards)
}

/// Join SPICE continuation lines (lines starting with `+`) into single logical lines.
fn join_continuation_lines(text: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.starts_with('+') {
            // Continuation: append to previous line
            if let Some(last) = result.last_mut() {
                last.push(' ');
                last.push_str(line[1..].trim_start());
            } else {
                // Continuation without a prior line, treat as new
                result.push(line[1..].trim_start().to_string());
            }
        } else {
            result.push(line.to_string());
        }
    }
    result
}

/// Parse a `.model` card.
///
/// Handles two common formats:
/// - `.MODEL name TYPE (key=val ...)`, space-separated type and params
/// - `.MODEL name TYPE(key=val ...)`, type and params fused (no space)
fn parse_dot_model(line: &str) -> Option<SpiceCard> {
    // Split on whitespace, skip the ".model" keyword
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }
    let name = tokens[1].to_uppercase();

    // The type token may be "NPN", "D", etc., or "NPN(IS=..." with a fused paren.
    let type_raw = tokens[2];
    let (model_type, extra_params) = if let Some(paren_pos) = type_raw.find('(') {
        // Fused: "D(IS=2.52N ...", split at the paren
        let typ = type_raw[..paren_pos].to_uppercase();
        let rest_of_type = &type_raw[paren_pos..]; // includes the '('
        (typ, Some(rest_of_type))
    } else {
        (type_raw.to_uppercase(), None)
    };

    // Build the param string from everything after the type token
    let rest = if let Some(extra) = extra_params {
        // extra already starts with '('; append remaining tokens
        format!("{} {}", extra, tokens[3..].join(" "))
    } else {
        tokens[3..].join(" ")
    };

    // Strip surrounding parens if present
    let param_str = rest.trim_start_matches('(').trim_end_matches(')').trim();

    let params = parse_kv_params(param_str);

    Some(SpiceCard {
        name,
        kind: SpiceCardKind::Model,
        raw: line.to_string(),
        ports: Vec::new(),
        params,
        model_type: Some(model_type),
    })
}

/// Parse a `.subckt` card (only the declaration line, full body is the raw text).
///
/// Format: `.SUBCKT <name> <port1> <port2> ...`
fn parse_dot_subckt(decl_line: &str, all_lines: &[String]) -> Option<SpiceCard> {
    let tokens: Vec<&str> = decl_line.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    let name = tokens[1].to_uppercase();
    // Ports are everything after the name, excluding `params:` sections
    let ports: Vec<String> = tokens[2..]
        .iter()
        .take_while(|t| !t.to_lowercase().starts_with("params"))
        .map(|t| t.to_string())
        .collect();

    // Collect the full subckt body (from .subckt to .ends)
    let mut raw_lines = Vec::new();
    let mut in_body = false;
    let lower_name = name.to_lowercase();
    for line in all_lines {
        let lower = line.to_lowercase();
        if lower.starts_with(".subckt") {
            // The declared name is the SECOND token, match it exactly, not as a
            // substring of the whole line. A substring match opened the wrong
            // body whenever this name was a substring of another subckt's name
            // (".subckt OP" vs ".subckt OPAMP") or appeared as a port/comment.
            let decl_name = line.split_whitespace().nth(1).map(|t| t.to_lowercase());
            if decl_name.as_deref() == Some(lower_name.as_str()) {
                in_body = true;
            }
        }
        if in_body {
            raw_lines.push(line.clone());
            if lower.starts_with(".ends") {
                break;
            }
        }
    }

    // Count ports for subckt (approximate; the solver may need more detail)
    let pin_count = ports.len();
    let _ = pin_count;

    Some(SpiceCard {
        name,
        kind: SpiceCardKind::Subckt,
        raw: raw_lines.join("\n"),
        ports,
        params: BTreeMap::new(),
        model_type: None,
    })
}

/// Parse `key=value` parameter pairs from a SPICE model param string.
fn parse_kv_params(s: &str) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    // Tokens may be separated by whitespace or commas
    let normalised = s.replace(',', " ");
    for token in normalised.split_whitespace() {
        if let Some((k, v)) = token.split_once('=') {
            let key = k.trim().to_uppercase();
            if let Some(val) = parse_spice_number(v.trim()) {
                map.insert(key, val);
            }
        }
    }
    map
}

/// Parse a SPICE number, which may use SPICE multiplier suffixes optionally
/// followed by a unit (`4pF`, `2.2uF`, `1MegOhm`). Returns `None` if the string
/// has no numeric mantissa.
///
/// This mirrors the authoritative loader in `hauksbee-ir::spice`: split the
/// leading numeric mantissa (with optional exponent) from the trailing suffix,
/// then resolve the scale by the LONGEST matching prefix of the suffix and
/// ignore any trailing unit. Matching on `ends_with` was wrong: a trailing unit
/// letter (`F` for farad) collided with a scale letter (`f` = femto), so `4pF`
/// took the femto branch, then failed to parse `"4p"` as a mantissa and returned
/// `None`, silently dropping the value, disagreeing with hauksbee-ir which
/// reads it as 4 pF.
fn parse_spice_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let bytes = t.as_bytes();
    let mut i = 0;
    if bytes[i] == b'+' || bytes[i] == b'-' {
        i += 1;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                seen_digit = true;
                i += 1;
            }
            b'.' if !seen_dot => {
                seen_dot = true;
                i += 1;
            }
            b'e' | b'E' => {
                // Exponent: e[+/-]digits, only when actually followed by digits.
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j + 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                break;
            }
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    let value: f64 = t[..i].parse().ok()?;
    let suffix = t[i..].to_ascii_lowercase();
    Some(value * scale_suffix(&suffix))
}

/// Engineering-suffix multiplier. Order matters: `meg`/`mil` before `m`. Any
/// trailing unit after the scale letter (e.g. the `f` in `pf`, the `ohm` in
/// `kohm`) is ignored.
fn scale_suffix(suffix: &str) -> f64 {
    let s = suffix;
    if s.is_empty() {
        1.0
    } else if s.starts_with("meg") {
        1e6
    } else if s.starts_with("mil") {
        25.4e-6
    } else if s.starts_with('t') {
        1e12
    } else if s.starts_with('g') {
        1e9
    } else if s.starts_with('k') {
        1e3
    } else if s.starts_with('m') {
        1e-3
    } else if s.starts_with('u') {
        1e-6
    } else if s.starts_with('n') {
        1e-9
    } else if s.starts_with('p') {
        1e-12
    } else if s.starts_with('f') {
        1e-15
    } else {
        // No `a`=atto branch: atto is NOT in the SPICE3/ngspice scale set
        // (T/G/Meg/K/mil/m/u/n/p/f) and 'a' collides with the ampere unit, a
        // current-valued model param "IBV=5A" must read 5 A, not 5e-18. This
        // matches hauksbee-ir::spice::scale_suffix exactly (the authoritative
        // loader this function mirrors). An unrecognised trailing unit with no
        // scale letter (bare "V"/"A"/"Ohm") stands as written.
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MODEL: &str = r"
* 1N4148 SPICE model
.MODEL D1N4148 D(IS=2.52N N=1.752 RS=0.568 CJO=4P VJ=0.8 M=0.4 TT=6N)
";

    const SAMPLE_SUBCKT: &str = r"
* Simple 2-transistor subckt
.SUBCKT MATCHPAIR IN1 IN2 OUT1 OUT2 VCC GND
Q1 OUT1 IN1 GND NPN_MODEL
Q2 OUT2 IN2 GND NPN_MODEL
.ENDS MATCHPAIR
";

    #[test]
    fn parse_model_card() {
        let cards = parse_spice_text(SAMPLE_MODEL).unwrap();
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.name, "D1N4148");
        assert_eq!(card.kind, SpiceCardKind::Model);
        assert_eq!(card.model_type.as_deref(), Some("D"));
        let is = *card.params.get("IS").unwrap();
        assert!((is - 2.52e-9).abs() < 1e-12, "IS={is}");
        let n = *card.params.get("N").unwrap();
        assert!((n - 1.752).abs() < 1e-6, "N={n}");
    }

    #[test]
    fn parse_subckt_card() {
        let cards = parse_spice_text(SAMPLE_SUBCKT).unwrap();
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.name, "MATCHPAIR");
        assert_eq!(card.kind, SpiceCardKind::Subckt);
        assert_eq!(card.ports.len(), 6);
        assert!(card.raw.contains(".ENDS"));
    }

    #[test]
    fn subckt_body_matches_name_exactly_not_as_substring() {
        // R11: two subckts whose names share a prefix ("OP" ⊂ "OPAMP"). The old
        // substring match opened OP's body at the first `.subckt` line
        // containing "op", which was OPAMP, so OP captured OPAMP's body.
        let src = "\
.SUBCKT OPAMP INP INN OUT
R1 INP OUT 1k
.ENDS OPAMP
.SUBCKT OP A B
R2 A B 2k
.ENDS OP
";
        let cards = parse_spice_text(src).unwrap();
        let op = cards.iter().find(|c| c.name == "OP").expect("OP present");
        assert!(
            op.raw.contains("R2") && !op.raw.contains("R1"),
            "OP must capture its own body, not OPAMP's: {:?}",
            op.raw
        );
        let opamp = cards.iter().find(|c| c.name == "OPAMP").unwrap();
        assert!(opamp.raw.contains("R1") && !opamp.raw.contains("R2"));
    }

    #[test]
    fn continuation_lines() {
        let src = ".MODEL BIGDEV NPN(\n+ IS=1E-14 BF=200\n+ VAF=80)";
        let cards = parse_spice_text(src).unwrap();
        assert_eq!(cards.len(), 1);
        let is = cards[0].params.get("IS").copied().unwrap_or(0.0);
        assert!((is - 1e-14).abs() < 1e-20);
    }

    #[test]
    fn scale_suffix_with_trailing_unit_is_not_dropped() {
        // The old `ends_with` parser collided the femto scale letter with a
        // trailing farad unit: "4pF" took the femto branch and then failed to
        // parse "4p" as a mantissa, returning None and dropping CJO entirely.
        // The scale must be read from the FRONT of the suffix, the unit ignored.
        let cases: [(&str, f64); 13] = [
            ("4pF", 4e-12),
            ("2.2uF", 2.2e-6),
            ("100nF", 100e-9),
            ("1MegOhm", 1e6),
            ("4.7kOhm", 4.7e3),
            ("15mV", 15e-3),
            ("4F", 4e-15),   // bare femto, no unit — unchanged from before
            ("2.52N", 2.52e-9),
            ("80", 80.0),
            ("1e-14", 1e-14),
            // R17: a trailing ampere unit must NOT be read as the atto scale.
            // Atto is not in the SPICE3/ngspice scale set and 'a' collides with
            // amperes; a current param "IBV=5A" is 5 A, not 5e-18. Matches the
            // authoritative hauksbee-ir loader this function mirrors.
            ("5A", 5.0),
            ("2.2A", 2.2),
            ("100mA", 100e-3), // m=milli, A ignored
        ];
        for (input, expected) in cases {
            let got = parse_spice_number(input).unwrap_or_else(|| panic!("{input} parsed to None"));
            let tol = (expected.abs() * 1e-9).max(1e-24);
            assert!(
                (got - expected).abs() <= tol,
                "{input}: got {got}, expected {expected}"
            );
        }
        // And end-to-end through a model card: CJO=4P survives.
        let card = &parse_spice_text(SAMPLE_MODEL).unwrap()[0];
        let cjo = *card.params.get("CJO").expect("CJO must be parsed, not dropped");
        assert!((cjo - 4e-12).abs() < 1e-18, "CJO={cjo}");
    }
}
