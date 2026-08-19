//! Reader for the KiCad custom design-rules format, `.kicad_dru`.
//!
//! This module intentionally reads only the rule fields and constraint types
//! that can change a Hauksbee report. Unknown grammar fails closed. Known
//! constraint types for checks Hauksbee does not implement are counted so the
//! engine can disclose them instead of treating their absence as coverage.
//!
//! ## Precedence evidence
//!
//! The retained precedence probe is
//! `tests/fixtures/kicad_dru_precedence.kicad_pcb` with its sibling `.kicad_pro`
//! and `.kicad_dru`. Its two 0.2 mm tracks have a 0.180 mm copper-edge gap and
//! the project netclass is 0.150 mm. KiCad CLI 10.0.5 reported the later
//! `restrictive 0.250 mm second` rule and an actual 0.180 mm gap, proving that
//! the later matching custom rule wins. The doorbell oracle independently
//! established that a global 0.127 mm custom clearance overrides the sibling
//! project's 0.200 mm Default netclass even though the custom rule is looser.
//!
//! The retained bare-value scope probe is
//! `tests/fixtures/kicad_dru_bare_scope.kicad_pcb`; full evidence is in
//! `qc/evidence/drc-parity/kicad-dru-bare-scope.md`. Its project rule is
//! 0.150 mm, its gap is 0.180 mm, and its custom file contains a bare 0.200
//! rule followed by an explicit 0.200mm rule. KiCad CLI 10.0.5 reported no
//! clearance violation. Removing only the bare rule made the explicit `mm
//! rule` fire at 0.2000 mm against the same 0.1800 mm gap. Bare-value rejection
//! is therefore file-scoped: one missing unit silently deactivates every custom
//! rule in the file, and Hauksbee falls back to project/netclass rules.

use std::collections::BTreeMap;

use forge_sexpr::{Document, List, Sexpr};

const SUPPORTED_VERSION: u64 = 1;

const KNOWN_UNIMPLEMENTED_CONSTRAINTS: &[&str] = &[
    "track_width",
    "via_diameter",
    "annular_width",
    "silk_clearance",
    "physical_clearance",
    "courtyard_clearance",
    "hole_size",
    "text_height",
    "text_thickness",
    "disallow",
    "length",
    "skew",
    "diff_pair_gap",
    "diff_pair_uncoupled",
    "via_count",
    "zone_connection",
    "thermal_relief_gap",
    "assertion",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KicadDruConstraintKind {
    Clearance,
    HoleClearance,
    EdgeClearance,
    Unimplemented(String),
}

impl KicadDruConstraintKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Clearance => "clearance",
            Self::HoleClearance => "hole_clearance",
            Self::EdgeClearance => "edge_clearance",
            Self::Unimplemented(name) => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KicadDruConstraint {
    pub kind: KicadDruConstraintKind,
    pub min_mm: Option<f64>,
    pub max_mm: Option<f64>,
    pub opt_mm: Option<f64>,
    /// KiCad 10.0.5 silently ignores a rule when one of its constraint bounds
    /// is a bare number rather than an explicitly unit-suffixed distance.
    pub has_bare_value: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KicadDruRule {
    pub name: String,
    /// One-based line containing this rule's opening `(rule` form.
    pub line_number: usize,
    pub condition: Option<String>,
    pub layer: Option<String>,
    pub severity: Option<String>,
    pub constraints: Vec<KicadDruConstraint>,
}

impl KicadDruRule {
    pub fn has_constraint(&self, kind: KicadDruConstraintKind) -> bool {
        self.constraints
            .iter()
            .any(|constraint| constraint.kind == kind)
    }

    pub fn clearance_min_mm(&self) -> Option<f64> {
        self.constraints.iter().rev().find_map(|constraint| {
            (constraint.kind == KicadDruConstraintKind::Clearance)
                .then_some(constraint.min_mm)
                .flatten()
        })
    }

    pub fn is_global(&self) -> bool {
        self.condition.is_none() && self.layer.is_none()
    }

    pub fn has_bare_value(&self) -> bool {
        self.constraints
            .iter()
            .any(|constraint| constraint.has_bare_value)
    }

    pub fn bare_value_constraint_types(&self) -> Vec<KicadDruConstraintKind> {
        let mut kinds = Vec::new();
        for constraint in self
            .constraints
            .iter()
            .filter(|constraint| constraint.has_bare_value)
        {
            if !kinds.contains(&constraint.kind) {
                kinds.push(constraint.kind.clone());
            }
        }
        kinds
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KicadDruRules {
    pub version: u64,
    pub rules: Vec<KicadDruRule>,
    pub unsupported_constraint_counts: BTreeMap<String, usize>,
}

impl KicadDruRules {
    /// KiCad gives a later matching custom rule priority over an earlier one.
    /// Only an unconditional, layer-independent rule is report-wide. KiCad
    /// 10.0.5 deactivates the entire file if any constraint bound is unitless.
    pub fn global_clearance_mm(&self) -> Option<f64> {
        if self.has_bare_values() {
            return None;
        }
        self.rules.iter().rev().find_map(|rule| {
            rule.is_global()
                .then(|| rule.clearance_min_mm())
                .flatten()
                .filter(|value| *value > 0.0)
        })
    }

    pub fn unevaluated_rules(&self) -> impl Iterator<Item = &KicadDruRule> {
        self.rules
            .iter()
            .filter(|rule| !rule.is_global() || rule.has_bare_value())
    }

    pub fn has_bare_values(&self) -> bool {
        self.rules.iter().any(KicadDruRule::has_bare_value)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KicadDruError {
    #[error(".kicad_dru parse failed: {0}")]
    Parse(String),
    #[error("unsupported .kicad_dru version {0}; supported version is 1")]
    UnsupportedVersion(u64),
    #[error("malformed .kicad_dru: {0}")]
    Malformed(String),
}

pub fn parse_kicad_dru(text: &str) -> Result<KicadDruRules, KicadDruError> {
    let stripped = strip_line_comments(text)?;
    let wrapped = format!("(kicad_dru\n{stripped}\n)");
    let document =
        forge_sexpr::parse(&wrapped).map_err(|error| KicadDruError::Parse(error.to_string()))?;
    let root = document
        .root()
        .filter(|root| root.name() == Some("kicad_dru"))
        .ok_or_else(|| KicadDruError::Malformed("missing custom-rules root".into()))?;

    let mut version = None;
    let mut rules = Vec::new();
    let mut unsupported_constraint_counts = BTreeMap::new();
    for item in root.lists() {
        match item.name() {
            Some("version") => {
                if version.is_some() {
                    return Err(KicadDruError::Malformed(
                        "more than one version header".into(),
                    ));
                }
                let value = one_argument(item, "version")?.parse::<u64>().map_err(|_| {
                    KicadDruError::Malformed("version must be a whole number".into())
                })?;
                if value != SUPPORTED_VERSION {
                    return Err(KicadDruError::UnsupportedVersion(value));
                }
                version = Some(value);
            }
            Some("rule") => {
                let line_number = original_line_number(&document, item)?;
                rules.push(parse_rule(
                    item,
                    line_number,
                    &mut unsupported_constraint_counts,
                )?);
            }
            Some(name) => {
                return Err(KicadDruError::Malformed(format!(
                    "unknown top-level form {name}"
                )));
            }
            None => {
                return Err(KicadDruError::Malformed(
                    "top-level form has no keyword".into(),
                ));
            }
        }
    }
    let version =
        version.ok_or_else(|| KicadDruError::Malformed("missing (version N) header".into()))?;
    Ok(KicadDruRules {
        version,
        rules,
        unsupported_constraint_counts,
    })
}

fn parse_rule(
    rule: &List,
    line_number: usize,
    unsupported_constraint_counts: &mut BTreeMap<String, usize>,
) -> Result<KicadDruRule, KicadDruError> {
    let name_token = rule
        .arg(0)
        .ok_or_else(|| KicadDruError::Malformed("rule is missing its name".into()))?;
    if !name_token.is_string() {
        return Err(KicadDruError::Malformed(
            "rule name must be a quoted string".into(),
        ));
    }
    let name = name_token.value();
    if name.trim().is_empty() {
        return Err(KicadDruError::Malformed("rule name is empty".into()));
    }

    let mut condition = None;
    let mut layer = None;
    let mut severity = None;
    let mut constraints = Vec::new();
    for field in rule.lists() {
        match field.name() {
            Some("condition") => set_once(
                &mut condition,
                one_argument(field, "condition")?,
                "condition",
                &name,
            )?,
            Some("layer") => set_once(&mut layer, one_argument(field, "layer")?, "layer", &name)?,
            Some("severity") => set_once(
                &mut severity,
                one_argument(field, "severity")?,
                "severity",
                &name,
            )?,
            Some("constraint") => {
                let constraint = parse_constraint(field)?;
                if let KicadDruConstraintKind::Unimplemented(kind) = &constraint.kind {
                    *unsupported_constraint_counts
                        .entry(kind.clone())
                        .or_default() += 1;
                }
                constraints.push(constraint);
            }
            Some(field_name) => {
                return Err(KicadDruError::Malformed(format!(
                    "rule {name:?} has unknown field {field_name}"
                )));
            }
            None => {
                return Err(KicadDruError::Malformed(format!(
                    "rule {name:?} has a field without a keyword"
                )));
            }
        }
    }
    if constraints.is_empty() {
        return Err(KicadDruError::Malformed(format!(
            "rule {name:?} has no constraint"
        )));
    }
    Ok(KicadDruRule {
        name,
        line_number,
        condition,
        layer,
        severity,
        constraints,
    })
}

fn parse_constraint(constraint: &List) -> Result<KicadDruConstraint, KicadDruError> {
    let kind_name = constraint
        .arg_value(0)
        .ok_or_else(|| KicadDruError::Malformed("constraint is missing its type".into()))?;
    let kind = match kind_name.as_str() {
        "clearance" => KicadDruConstraintKind::Clearance,
        "hole_clearance" => KicadDruConstraintKind::HoleClearance,
        "edge_clearance" => KicadDruConstraintKind::EdgeClearance,
        known if KNOWN_UNIMPLEMENTED_CONSTRAINTS.contains(&known) => {
            KicadDruConstraintKind::Unimplemented(known.to_string())
        }
        unknown => {
            return Err(KicadDruError::Malformed(format!(
                "unknown constraint type {unknown}"
            )));
        }
    };

    let mut min_mm = None;
    let mut max_mm = None;
    let mut opt_mm = None;
    let mut has_bare_value = false;
    for bound in constraint.lists() {
        let target = match bound.name() {
            Some("min") => &mut min_mm,
            Some("max") => &mut max_mm,
            Some("opt") => &mut opt_mm,
            Some(name) => {
                return Err(KicadDruError::Malformed(format!(
                    "constraint {kind_name} has unknown bound {name}"
                )));
            }
            None => {
                return Err(KicadDruError::Malformed(format!(
                    "constraint {kind_name} has a bound without a keyword"
                )));
            }
        };
        if target.is_some() {
            return Err(KicadDruError::Malformed(format!(
                "constraint {kind_name} repeats its {} bound",
                bound.name().unwrap_or("unknown")
            )));
        }
        let distance = parse_distance(&one_argument(bound, bound.name().unwrap_or("bound"))?)?;
        *target = Some(distance.millimetres);
        has_bare_value |= !distance.has_explicit_unit;
    }
    if matches!(
        kind,
        KicadDruConstraintKind::Clearance
            | KicadDruConstraintKind::HoleClearance
            | KicadDruConstraintKind::EdgeClearance
    ) && min_mm.is_none()
    {
        return Err(KicadDruError::Malformed(format!(
            "constraint {kind_name} has no minimum"
        )));
    }
    Ok(KicadDruConstraint {
        kind,
        min_mm,
        max_mm,
        opt_mm,
        has_bare_value,
    })
}

struct ParsedDistance {
    millimetres: f64,
    has_explicit_unit: bool,
}

fn parse_distance(value: &str) -> Result<ParsedDistance, KicadDruError> {
    let (number, scale, has_explicit_unit) = if let Some(number) = value.strip_suffix("mil") {
        (number, 0.0254, true)
    } else if let Some(number) = value.strip_suffix("mm") {
        (number, 1.0, true)
    } else if let Some(number) = value.strip_suffix("in") {
        (number, 25.4, true)
    } else {
        (value, 1.0, false)
    };
    let number = number.parse::<f64>().map_err(|_| {
        KicadDruError::Malformed(format!(
            "distance {value:?} is not a number with mm, mil, or in units"
        ))
    })?;
    let millimetres = number * scale;
    if !millimetres.is_finite() || millimetres < 0.0 {
        return Err(KicadDruError::Malformed(format!(
            "distance {value:?} must be finite and non-negative"
        )));
    }
    Ok(ParsedDistance {
        millimetres,
        has_explicit_unit,
    })
}

fn original_line_number(document: &Document, list: &List) -> Result<usize, KicadDruError> {
    let source = document
        .src
        .as_deref()
        .ok_or_else(|| KicadDruError::Malformed("parsed document has no source buffer".into()))?;
    let keyword = match list.children.first() {
        Some(Sexpr::Token(token)) => token.raw.as_str(),
        _ => {
            return Err(KicadDruError::Malformed(
                "rule form has no keyword token".into(),
            ));
        }
    };
    let source_start = source.as_ptr() as usize;
    let keyword_start = keyword.as_ptr() as usize;
    let offset = keyword_start.checked_sub(source_start).ok_or_else(|| {
        KicadDruError::Malformed("rule keyword is outside the parsed source buffer".into())
    })?;
    if offset > source.len() {
        return Err(KicadDruError::Malformed(
            "rule keyword is outside the parsed source buffer".into(),
        ));
    }
    // parse_kicad_dru wraps the original text in one synthetic root line.
    let wrapped_line = 1 + source[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    Ok(wrapped_line.saturating_sub(1).max(1))
}

fn one_argument(list: &List, form: &str) -> Result<String, KicadDruError> {
    let value = list
        .arg_value(0)
        .ok_or_else(|| KicadDruError::Malformed(format!("{form} is missing its value")))?;
    if list.arg(1).is_some() || list.lists().next().is_some() {
        return Err(KicadDruError::Malformed(format!(
            "{form} must have exactly one value"
        )));
    }
    Ok(value)
}

fn set_once(
    target: &mut Option<String>,
    value: String,
    field: &str,
    rule: &str,
) -> Result<(), KicadDruError> {
    if target.replace(value).is_some() {
        return Err(KicadDruError::Malformed(format!(
            "rule {rule:?} repeats its {field} field"
        )));
    }
    Ok(())
}

fn strip_line_comments(text: &str) -> Result<String, KicadDruError> {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;
    for character in text.chars() {
        if in_comment {
            if character == '\n' {
                in_comment = false;
                out.push(character);
            } else {
                out.push(' ');
            }
            continue;
        }
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '#' => {
                in_comment = true;
                out.push(' ');
            }
            '"' => {
                in_string = true;
                out.push(character);
            }
            _ => out.push(character),
        }
    }
    if in_string {
        return Err(KicadDruError::Parse(
            "unterminated string while reading line comments".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_kicad_dru;

    #[test]
    fn hash_inside_a_string_is_not_a_comment() {
        let parsed = parse_kicad_dru(
            r#"(version 1)
            (rule "hash # name" (constraint clearance (min 0.2mm))) # comment"#,
        )
        .unwrap();
        assert_eq!(parsed.rules[0].name, "hash # name");
    }
}
