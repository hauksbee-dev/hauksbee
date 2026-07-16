//! Match-rule evaluation engine.
//!
//! A compiled [`CompiledEntry`] pre-compiles all regex patterns from a
//! [`ModelEntry`] so matching is fast at runtime. The [`score`] function
//! assigns a specificity score so more-specific rules win over catch-alls.
//! The specificity score orders entries only *within* a resolution layer;
//! the layer itself wins first (see `crate::SourceLayer`).
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-models/schema.md (this
//! module and the schema are one story: what an entry is and how it wins).

use regex::Regex;

use crate::schema::{MatchRules, ModelEntry};

/// A model entry with pre-compiled regexes.
pub struct CompiledEntry {
    pub entry: ModelEntry,
    value_re: Option<Regex>,
    footprint_re: Option<Regex>,
    mpn_re: Option<Regex>,
}

impl CompiledEntry {
    /// Compile a [`ModelEntry`]'s match rules into regex objects.
    ///
    /// Returns an error if any regex pattern is invalid. The `id` from the
    /// entry is included in error messages.
    pub fn compile(entry: ModelEntry) -> Result<Self, regex::Error> {
        let value_re = entry
            .r#match
            .value_re
            .as_deref()
            .map(|p| Regex::new(&format!("(?i){}", p)))
            .transpose()?;
        let footprint_re = entry
            .r#match
            .footprint_re
            .as_deref()
            // Case-INSENSITIVE like value_re/mpn_re: a rule `footprint_re = "sot-23"`
            // must match a board footprint `Package_TO_SOT_SMD:SOT-23`. A
            // case-sensitive compile silently skipped the entry, dropping the part
            // to a generic fallback with no diagnostic.
            .map(|p| Regex::new(&format!("(?i){}", p)))
            .transpose()?;
        let mpn_re = entry
            .r#match
            .mpn_re
            .as_deref()
            .map(|p| Regex::new(&format!("(?i){}", p)))
            .transpose()?;
        Ok(CompiledEntry {
            entry,
            value_re,
            footprint_re,
            mpn_re,
        })
    }

    /// Whether this entry's match rules fire for the given query fields.
    ///
    /// Rules are ANDed: every populated rule must match.
    pub fn matches(&self, q: &ComponentQuery) -> bool {
        let rules = &self.entry.r#match;

        // lib_id: exact or prefix match
        if let Some(lid) = &rules.lib_id {
            let query_lid = q.lib_id.as_deref().unwrap_or("");
            let ok = if lid.ends_with(':') {
                // prefix match
                query_lid.starts_with(lid.as_str())
            } else {
                // exact match
                query_lid == lid.as_str()
            };
            if !ok {
                return false;
            }
        }

        // value regex
        if let Some(re) = &self.value_re {
            // Normalise common comma-decimal before matching
            let val = normalise_value_str(q.value.as_deref().unwrap_or(""));
            if !re.is_match(&val) {
                return false;
            }
        }

        // footprint regex
        if let Some(re) = &self.footprint_re {
            let fp = q.footprint.as_deref().unwrap_or("");
            if !re.is_match(fp) {
                return false;
            }
        }

        // MPN regex
        if let Some(re) = &self.mpn_re {
            let mpn = q.mpn.as_deref().unwrap_or("");
            if !re.is_match(mpn) {
                return false;
            }
        }

        true
    }

    /// Specificity score (higher = more specific = wins ties).
    ///
    /// Scoring rationale:
    /// - MPN match is the most specific (part number uniquely identifies a device).
    /// - Exact lib_id match is more specific than a prefix.
    /// - Value regex is fairly specific.
    /// - Footprint regex is a catch-all fallback.
    pub fn specificity_score(&self, q: &ComponentQuery) -> u32 {
        let rules = &self.entry.r#match;
        let mut score = 0u32;

        if let Some(lid) = &rules.lib_id {
            if lid.ends_with(':') {
                score += 10; // prefix
            } else {
                score += 20; // exact
            }
        }
        if rules.value_re.is_some() {
            score += 30;
        }
        if rules.mpn_re.is_some() {
            score += 40;
        }
        if rules.footprint_re.is_some() {
            // Only add footprint to score if it was actually checked
            let fp = q.footprint.as_deref().unwrap_or("");
            if self
                .footprint_re
                .as_ref()
                .map(|re| re.is_match(fp))
                .unwrap_or(false)
            {
                score += 5;
            }
        }
        score
    }

    /// Fine-grained regex constrainedness, used as a same-layer tie-break.
    ///
    /// [`specificity_score`](Self::specificity_score) only counts *which*
    /// rule fields are present, so a dedicated override (`^1N4004$`) and the
    /// family entry it carves out of (`^1N400[1-7]$`) tie when both match the
    /// same value. This score ranks the patterns themselves: an exact literal
    /// outranks a character-class/alternation family pattern, so the override
    /// wins deterministically instead of by load order.
    pub fn regex_specificity(&self) -> u32 {
        let rules = &self.entry.r#match;
        [
            rules.value_re.as_deref(),
            rules.mpn_re.as_deref(),
            rules.footprint_re.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(pattern_constrainedness)
        .sum()
    }
}

/// Heuristic constrainedness of a regex pattern (higher = more exact).
///
/// The pattern is split on top-level `|` and the *least* constrained branch
/// counts — an alternation is only as specific as its loosest arm. Within a
/// branch: literal characters score 2, a character class (`[...]`) or escaped
/// class (`\d`, `\w`, …) scores 1 (it pins one position but admits several
/// characters), anchors score 1, and wildcards / quantifiers / grouping
/// punctuation score 0. So `^1N4004$` (14) outranks `^1N400[1-7]$` (13) while
/// both keep the same field-level [`CompiledEntry::specificity_score`].
pub fn pattern_constrainedness(pattern: &str) -> u32 {
    split_top_level_alternation(pattern)
        .into_iter()
        .map(branch_constrainedness)
        .min()
        .unwrap_or(0)
}

/// Split a pattern on `|` at nesting depth zero (outside classes and groups).
fn split_top_level_alternation(pattern: &str) -> Vec<&str> {
    let mut branches = Vec::new();
    let bytes = pattern.as_bytes();
    let mut depth = 0usize;
    let mut in_class = false;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1, // skip the escaped byte
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => depth += 1,
            b')' if !in_class => depth = depth.saturating_sub(1),
            b'|' if !in_class && depth == 0 => {
                branches.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    branches.push(&pattern[start..]);
    branches
}

/// Constrainedness of a single alternation-free branch (see
/// [`pattern_constrainedness`] for the character weights).
fn branch_constrainedness(branch: &str) -> u32 {
    let mut score = 0u32;
    let mut chars = branch.chars().peekable();
    let mut in_class = false;
    // Constrainedness contributed by the most recent quantifiable atom, so a
    // following `*` / `?` (which allow ZERO occurrences) can retract it: an
    // optional token matches nothing and constrains nothing. Without this an
    // optional tail like `[A-Z0-9-]*` inflated a family pattern's score above the
    // exact `^1N4148$` override it carves out of, letting the broad entry win the
    // same-layer specificity tie and bind the wrong (generic) params.
    let mut last_atom: u32 = 0;
    while let Some(c) = chars.next() {
        if in_class {
            match c {
                '\\' => {
                    chars.next();
                }
                ']' => in_class = false,
                _ => {}
            }
            continue;
        }
        match c {
            '\\' => match chars.next() {
                // Escaped class: pins one position, admits several chars.
                Some('d' | 'D' | 'w' | 'W' | 's' | 'S') => {
                    score += 1;
                    last_atom = 1;
                }
                // Escaped metacharacter: a literal.
                Some(_) => {
                    score += 2;
                    last_atom = 2;
                }
                None => {}
            },
            '[' => {
                in_class = true;
                score += 1;
                last_atom = 1;
            }
            '(' => {
                // Skip inline flags: `(?i)` scores nothing, `(?i:` / `(?:`
                // fall through so the group body is scored normally.
                if chars.peek() == Some(&'?') {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == ':' || n == ')' {
                            break;
                        }
                    }
                }
                // A group is not a single cancellable atom.
                last_atom = 0;
            }
            '{' => {
                // Repetition count `{m,n}`: contributes nothing, and consumes the
                // atom so a following lazy `?` cannot retract it.
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == '}' {
                        break;
                    }
                }
                last_atom = 0;
            }
            '^' | '$' => {
                score += 1;
                last_atom = 0;
            }
            // `*` and `?` make the PRECEDING atom optional (zero occurrences ok):
            // retract what that atom added AND dock one looseness unit, so a
            // pattern carrying an optional region is strictly LESS constrained
            // than the same pattern without it (it matches a superset). This is
            // what lets the exact override `^1N4148$` (14) deterministically beat
            // the family `^1N4148[A-Z0-9-]*$` (13) it carves out of, rather than
            // merely tie it and fall to the id tiebreak. `+` requires at least one
            // occurrence, so it still constrains and is not retracted.
            '*' | '?' => {
                score = score.saturating_sub(last_atom + 1);
                last_atom = 0;
            }
            ')' | '.' | '+' => {
                last_atom = 0;
            }
            _ => {
                score += 2;
                last_atom = 2;
            }
        }
    }
    score
}

/// Normalise common BOM variations before regex matching.
fn normalise_value_str(s: &str) -> String {
    s.replace(',', ".") // European decimal comma → point
}

// ── ComponentQuery ────────────────────────────────────────────────────────────

/// All available metadata for a component, used as input to [`ModelLibrary::resolve`].
///
/// All fields are optional; the library matches against whatever is present.
#[derive(Debug, Clone, Default)]
pub struct ComponentQuery {
    /// KiCad lib_id, e.g. `"Device:R"` or `"Device:Q_NPN_BCE"`.
    pub lib_id: Option<String>,
    /// Component value field, e.g. `"10k"`, `"BC847"`, `"100nF"`.
    pub value: Option<String>,
    /// Footprint string, e.g. `"Resistor_THT:R_Axial_…"`.
    pub footprint: Option<String>,
    /// Manufacturer part number (from a schematic property), if present.
    pub mpn: Option<String>,
    /// Reference designator (e.g. `"R1"`), used only in diagnostics.
    pub reference: Option<String>,
}

impl ComponentQuery {
    /// Convenience constructor with the most common fields.
    pub fn new(
        lib_id: impl Into<Option<String>>,
        value: impl Into<Option<String>>,
        footprint: impl Into<Option<String>>,
    ) -> Self {
        ComponentQuery {
            lib_id: lib_id.into(),
            value: value.into(),
            footprint: footprint.into(),
            mpn: None,
            reference: None,
        }
    }
}

// ── MatchRules helpers ────────────────────────────────────────────────────────

impl MatchRules {
    /// Number of populated rule fields (for tie-breaking).
    pub fn populated(&self) -> usize {
        self.lib_id.is_some() as usize
            + self.value_re.is_some() as usize
            + self.footprint_re.is_some() as usize
            + self.mpn_re.is_some() as usize
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{pattern_constrainedness, ComponentQuery, CompiledEntry};
    use crate::schema::{ComponentKind, MatchRules, ModelEntry};

    #[test]
    fn footprint_re_matches_case_insensitively() {
        // Round-26: a rule `footprint_re = "sot-23"` must fire against a board
        // footprint `Package_TO_SOT_SMD:SOT-23`. A case-SENSITIVE compile silently
        // skipped the entry, dropping the part to a generic fallback. Author the
        // rule in lower case, query in the board's mixed case: it must still match.
        let entry = ModelEntry {
            id: "sot23-test".to_string(),
            kind: ComponentKind::BjtNpn,
            description: String::new(),
            r#match: MatchRules {
                footprint_re: Some("sot-23".to_string()),
                ..Default::default()
            },
            params: Default::default(),
            pins: Default::default(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let compiled = CompiledEntry::compile(entry).expect("compiles");
        let q = ComponentQuery {
            footprint: Some("Package_TO_SOT_SMD:SOT-23".to_string()),
            ..Default::default()
        };
        assert!(compiled.matches(&q), "footprint regex must match case-insensitively");
    }

    #[test]
    fn exact_literal_outranks_character_class_family() {
        // The 1N400x case: the dedicated override must score above the family.
        assert!(
            pattern_constrainedness("(?i)^1N4004$") > pattern_constrainedness("(?i)^1N400[1-7]$")
        );
    }

    #[test]
    fn anchored_exact_outranks_unanchored_prefix() {
        // ^INA186$ vs (?i)^INA186 — the anchored exact is more constrained.
        assert!(pattern_constrainedness("^INA186$") > pattern_constrainedness("(?i)^INA186"));
    }

    #[test]
    fn alternation_scores_its_loosest_branch() {
        // ^BAT5[0-9]$|^BAT43$ is only as specific as its looser arm.
        let alt = pattern_constrainedness("(?i)^BAT5[0-9]$|^BAT43$");
        assert_eq!(alt, pattern_constrainedness("^BAT5[0-9]$"));
        assert!(pattern_constrainedness("(?i)^BAT54$") > alt);
    }

    #[test]
    fn inline_flags_and_groups_score_nothing() {
        assert_eq!(pattern_constrainedness("(?i)AB"), pattern_constrainedness("AB"));
        assert_eq!(pattern_constrainedness("(AB)"), pattern_constrainedness("AB"));
    }

    #[test]
    fn optional_quantifier_lowers_specificity_below_the_exact_override() {
        // R32: an optional-quantified token (`[A-Z0-9-]*`, `a?`) matches zero
        // characters and constrains nothing, so it must NOT inflate the score.
        // Previously the class scored +1 and the `*` retracted nothing, so the
        // family pattern outscored the exact `^1N4148$` override it carves out of
        // and won the same-layer regex tie-break — silently binding the generic
        // params. The exact literal must now win deterministically.
        assert!(
            pattern_constrainedness("^1N4148$")
                > pattern_constrainedness("^1N4148[A-Z0-9-]*$"),
            "the exact override must out-score the optional-tail family"
        );
        // An optional region is strictly looser than the same required region.
        assert!(
            pattern_constrainedness("^1N4148[A-Z0-9-]$")
                > pattern_constrainedness("^1N4148[A-Z0-9-]*$"),
            "a required class char out-scores the same class made optional"
        );
        // `+` requires at least one occurrence, so it still constrains — a `+`
        // family is not docked below its `*` sibling to nothing.
        assert!(
            pattern_constrainedness("^1N4148[A-Z0-9-]+$")
                > pattern_constrainedness("^1N4148[A-Z0-9-]*$"),
            "`+` (one-or-more) constrains more than `*` (zero-or-more)"
        );
        // Wildcards still score nothing.
        assert!(pattern_constrainedness("^ABC$") > pattern_constrainedness("^AB.$"));
    }
}
