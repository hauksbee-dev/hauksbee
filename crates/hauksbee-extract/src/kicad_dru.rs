//! Reader for the KiCad custom design-rules format, `.kicad_dru`.
//!
//! This module intentionally reads only the rule fields and constraint types
//! that can change a Hauksbee report. A rule it cannot interpret is never
//! applied and never silently dropped: it is kept in
//! [`KicadDruRules::not_understood`] with its name and line so the report can
//! say that rule was not checked, and the rest of the file is still read. Known
//! constraint types for checks Hauksbee does not implement are counted so the
//! engine can disclose them instead of treating their absence as coverage.
//!
//! ## The two precedence rules, and where they were established
//!
//! A LATER matching custom rule wins over an earlier one, even when the later
//! rule is looser, and a custom rule outranks the project netclass in both
//! directions. Bare-value rejection is FILE-scoped: one constraint written
//! without a unit silently deactivates every custom rule in the file, and
//! Hauksbee then falls back to project/netclass rules.
//!
//! Both were established against KiCad CLI 10.0.5 with the
//! `kicad_dru_precedence` fixtures and
//! `tests/fixtures/kicad_dru_bare_scope.kicad_pcb`; the raw outputs and hashes
//! are retained in `qc/evidence/drc-parity/kicad-dru-precedence-10.0.5.md` and
//! `qc/evidence/drc-parity/dru-bare-value-poisons-the-whole-file.md`.

use std::collections::BTreeMap;

use forge_sexpr::{Document, List, Sexpr};

use crate::drc_rules::CustomClearanceRule;

const SUPPORTED_VERSION: u64 = 1;

/// Valid KiCad constraints whose bounds are distances. Hauksbee does not check
/// them; they are recognised and disclosed.
const KNOWN_DISTANCE_CONSTRAINTS: &[&str] = &[
    "track_width",
    "via_diameter",
    "annular_width",
    "silk_clearance",
    "physical_clearance",
    "physical_hole_clearance",
    "courtyard_clearance",
    "creepage",
    "hole_size",
    "hole_to_hole",
    "text_height",
    "text_thickness",
    "track_segment_length",
    "connection_width",
    "length",
    "diff_pair_gap",
    "diff_pair_uncoupled",
    "thermal_relief_gap",
    "thermal_spoke_width",
    "solder_mask_expansion",
    "solder_mask_sliver",
    "solder_paste_abs_margin",
    // Older spellings KiCad still accepts for `physical_clearance`,
    // `physical_hole_clearance` and `hole_size`.
    "mechanical_clearance",
    "mechanical_hole_clearance",
    "hole",
];

/// Valid KiCad constraints whose arguments are not distances: keywords, counts,
/// angles, times, ratios or expressions. Their values are not interpreted, and a
/// bare number in one is not a unitless distance.
const KNOWN_NON_DISTANCE_CONSTRAINTS: &[&str] = &[
    "disallow",
    "assertion",
    "zone_connection",
    "via_count",
    "skew",
    "track_angle",
    "min_resolved_spokes",
    "solder_paste_rel_margin",
    "bridged_mask",
    "via_dangling",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KicadDruConstraintKind {
    Clearance,
    HoleClearance,
    EdgeClearance,
    Unimplemented(String),
}

impl KicadDruConstraintKind {
    #[must_use]
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
    /// KiCad 10.0.5 silently deactivates the entire rules file when one of its
    /// constraint bounds is a bare number rather than a unit-suffixed distance.
    pub has_bare_value: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KicadDruRule {
    pub name: String,
    /// One-based line containing this rule's opening `(rule` form.
    pub line_number: usize,
    pub condition: Option<String>,
    pub layer: Option<String>,
    /// The `(layer ...)` value was written as a quoted string. KiCad reads the
    /// bare words `outer` and `inner` as layer groups but a quoted one as a
    /// layer name that does not exist.
    pub layer_is_quoted: bool,
    pub severity: Option<String>,
    pub constraints: Vec<KicadDruConstraint>,
}

impl KicadDruRule {
    #[must_use]
    pub fn clearance_min_mm(&self) -> Option<f64> {
        self.constraints.iter().rev().find_map(|constraint| {
            (constraint.kind == KicadDruConstraintKind::Clearance)
                .then_some(constraint.min_mm)
                .flatten()
        })
    }

    #[must_use]
    pub const fn is_global(&self) -> bool {
        self.condition.is_none() && self.layer.is_none()
    }

    /// The rule as the clearance sweep applies it: its scope compiled, with
    /// the bound it sets. `None` for a rule that sets no clearance bound.
    #[must_use]
    pub fn custom_clearance_rule(&self) -> Option<CustomClearanceRule> {
        let min_mm = self.clearance_min_mm()?;
        Some(CustomClearanceRule::compile(
            &self.name,
            self.condition.as_deref(),
            self.layer.as_deref(),
            self.layer_is_quoted,
            self.severity.as_deref(),
            min_mm,
        ))
    }

    /// True when the sweep can judge this rule's scope: it is unconditional, or
    /// it sets a clearance bound under a condition and layer the evaluator
    /// understands. A rule that is not is disclosed as not evaluated.
    #[must_use]
    pub fn is_scope_evaluated(&self) -> bool {
        self.is_global()
            || self
                .custom_clearance_rule()
                .is_some_and(|rule| rule.is_evaluable())
    }

    #[must_use]
    pub fn has_bare_value(&self) -> bool {
        self.constraints
            .iter()
            .any(|constraint| constraint.has_bare_value)
    }

    #[must_use]
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

/// A rule (or other top-level form) the reader could not interpret. It is not
/// applied; the report names it so the user knows that rule was not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KicadDruSkippedRule {
    /// The rule's name, when it could be read.
    pub name: Option<String>,
    /// One-based line of the rule's opening form.
    pub line_number: usize,
    /// What the reader could not understand, as a short phrase.
    pub why: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KicadDruRules {
    pub version: u64,
    pub rules: Vec<KicadDruRule>,
    pub unsupported_constraint_counts: BTreeMap<String, usize>,
    /// Rules the reader could not interpret, in file order.
    pub not_understood: Vec<KicadDruSkippedRule>,
}

impl KicadDruRules {
    /// KiCad gives a later matching custom rule priority over an earlier one.
    /// Only an unconditional, layer-independent rule is report-wide. KiCad
    /// 10.0.5 deactivates the entire file if any constraint bound is unitless.
    #[must_use]
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

    /// The clearance rules the sweep applies, in file order (a later matching
    /// rule wins). Empty when KiCad would discard the whole file for a bare
    /// value.
    #[must_use]
    pub fn clearance_rules(&self) -> Vec<CustomClearanceRule> {
        if self.has_bare_values() {
            return Vec::new();
        }
        self.rules
            .iter()
            .filter_map(KicadDruRule::custom_clearance_rule)
            .collect()
    }

    /// Rules the sweep cannot fully apply: everything when the file is
    /// inactive, a rule with a bare value, and a rule whose condition or layer
    /// the evaluator does not understand. A conditional rule it does understand
    /// is applied pair by pair, not listed here.
    pub fn unevaluated_rules(&self) -> impl Iterator<Item = &KicadDruRule> {
        let file_inactive = self.has_bare_values();
        self.rules.iter().filter(move |rule| {
            file_inactive || !rule.is_scope_evaluated() || rule.has_bare_value()
        })
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

impl KicadDruError {
    /// The error without its category prefix, as a short phrase.
    fn reason(&self) -> String {
        match self {
            Self::Parse(why) | Self::Malformed(why) => why.clone(),
            Self::UnsupportedVersion(version) => format!("unsupported version {version}"),
        }
    }
}

/// Parse a KiCad custom design-rules file (`.kicad_dru`).
///
/// A rule this reader cannot interpret does not fail the file: it is listed in
/// [`KicadDruRules::not_understood`] and the other rules are still read.
///
/// # Errors
/// A [`KicadDruError`] when the file as a whole is unusable: the text does not
/// parse as s-expressions, declares an unsupported version, or has no version.
pub fn parse_kicad_dru(text: &str) -> Result<KicadDruRules, KicadDruError> {
    let stripped = strip_line_comments(text)?;
    let wrapped = format!("(kicad_dru {stripped}\n)");
    let document =
        forge_sexpr::parse(&wrapped).map_err(|error| KicadDruError::Parse(error.to_string()))?;
    let root = document
        .root()
        .filter(|root| root.name() == Some("kicad_dru"))
        .ok_or_else(|| KicadDruError::Malformed("missing custom-rules root".into()))?;

    let mut version = None;
    let mut rules = Vec::new();
    let mut unsupported_constraint_counts = BTreeMap::new();
    let mut not_understood = Vec::new();
    for item in root.lists() {
        let line_number = original_line_number(&document, item).unwrap_or(1);
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
                let mut counts = BTreeMap::new();
                match parse_rule(item, line_number, &mut counts) {
                    Ok(rule) => {
                        for (kind, count) in counts {
                            *unsupported_constraint_counts.entry(kind).or_default() += count;
                        }
                        rules.push(rule);
                    }
                    Err(error) => not_understood.push(KicadDruSkippedRule {
                        name: item.arg_value(0).filter(|name| !name.trim().is_empty()),
                        line_number,
                        why: error.reason(),
                    }),
                }
            }
            Some(name) => not_understood.push(KicadDruSkippedRule {
                name: None,
                line_number,
                why: format!("unknown top-level form {name}"),
            }),
            None => not_understood.push(KicadDruSkippedRule {
                name: None,
                line_number,
                why: "top-level form has no keyword".into(),
            }),
        }
    }
    let version =
        version.ok_or_else(|| KicadDruError::Malformed("missing (version N) header".into()))?;
    Ok(KicadDruRules {
        version,
        rules,
        unsupported_constraint_counts,
        not_understood,
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
    let name = name_token.value();
    if name.trim().is_empty() {
        return Err(KicadDruError::Malformed("rule name is empty".into()));
    }

    let mut condition = None;
    let mut layer = None;
    let mut layer_is_quoted = false;
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
            Some("layer") => {
                set_once(&mut layer, one_argument(field, "layer")?, "layer", &name)?;
                layer_is_quoted = field.arg(0).is_some_and(forge_sexpr::Token::is_string);
            }
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
        layer_is_quoted,
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
        known
            if KNOWN_DISTANCE_CONSTRAINTS.contains(&known)
                || KNOWN_NON_DISTANCE_CONSTRAINTS.contains(&known) =>
        {
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
    let distances = !KNOWN_NON_DISTANCE_CONSTRAINTS.contains(&kind_name.as_str());
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
        let value = one_argument(bound, bound.name().unwrap_or("bound"))?;
        // `length` bounds may also be times (`10ps`), which are not distances.
        let is_time = kind_name == "length"
            && ["ps", "ns", "us", "ms"]
                .iter()
                .any(|unit| value.ends_with(unit));
        if distances && !is_time {
            let distance = parse_distance(&value)?;
            *target = Some(distance.millimetres);
            has_bare_value |= !distance.has_explicit_unit;
        }
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
    let (number, scale, has_explicit_unit) = [("mil", 0.0254), ("mm", 1.0), ("in", 25.4)]
        .into_iter()
        .find_map(|(unit, scale)| value.strip_suffix(unit).map(|n| (n, scale, true)))
        .unwrap_or((value, 1.0, false));
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
    let outside = || KicadDruError::Malformed("rule keyword is outside the parsed source".into());
    let source = document.src.as_deref().ok_or_else(outside)?;
    let Some(Sexpr::Token(keyword)) = list.children.first() else {
        return Err(outside());
    };
    let offset = (keyword.raw.as_ptr() as usize)
        .checked_sub(source.as_ptr() as usize)
        .filter(|offset| *offset <= source.len())
        .ok_or_else(outside)?;
    // parse_kicad_dru wraps the text without adding a line, so lines match the file.
    Ok(1 + source[..offset].matches('\n').count())
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
