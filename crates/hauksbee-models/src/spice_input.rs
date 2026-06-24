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
                // Continuation without a prior line — treat as new
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
/// - `.MODEL name TYPE (key=val ...)` — space-separated type and params
/// - `.MODEL name TYPE(key=val ...)`  — type and params fused (no space)
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
        // Fused: "D(IS=2.52N ..." — split at the paren
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

/// Parse a `.subckt` card (only the declaration line — full body is the raw text).
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
        if lower.starts_with(".subckt") && lower.contains(&lower_name) {
            in_body = true;
        }
        if in_body {
            raw_lines.push(line.clone());
            if lower.starts_with(".ends") {
                break;
            }
        }
    }

    // Count ports for subckt (approximate — the solver may need more detail)
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

/// Parse a SPICE number, which may use SPICE multiplier suffixes.
/// Returns `None` if the string cannot be parsed.
fn parse_spice_number(s: &str) -> Option<f64> {
    // Try plain f64 first
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    // Try with SPICE engineering suffixes (case-insensitive)
    let upper = s.to_uppercase();
    let (mantissa_str, multiplier) = if upper.ends_with("MEG") {
        (&s[..s.len() - 3], 1e6)
    } else if upper.ends_with('G') {
        (&s[..s.len() - 1], 1e9)
    } else if upper.ends_with('T') {
        (&s[..s.len() - 1], 1e12)
    } else if upper.ends_with('K') {
        (&s[..s.len() - 1], 1e3)
    } else if upper.ends_with('M') {
        (&s[..s.len() - 1], 1e-3)
    } else if upper.ends_with('U') {
        (&s[..s.len() - 1], 1e-6)
    } else if upper.ends_with('N') {
        (&s[..s.len() - 1], 1e-9)
    } else if upper.ends_with('P') {
        (&s[..s.len() - 1], 1e-12)
    } else if upper.ends_with('F') {
        (&s[..s.len() - 1], 1e-15)
    } else {
        return None;
    };
    mantissa_str.parse::<f64>().ok().map(|v| v * multiplier)
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
    fn continuation_lines() {
        let src = ".MODEL BIGDEV NPN(\n+ IS=1E-14 BF=200\n+ VAF=80)";
        let cards = parse_spice_text(src).unwrap();
        assert_eq!(cards.len(), 1);
        let is = cards[0].params.get("IS").copied().unwrap_or(0.0);
        assert!((is - 1e-14).abs() < 1e-20);
    }
}
