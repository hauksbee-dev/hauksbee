//! Match-rule evaluation engine.
//!
//! A compiled [`CompiledEntry`] pre-compiles all regex patterns from a
//! [`ModelEntry`] so matching is fast at runtime. The [`score`] function
//! assigns a specificity score so more-specific rules win over catch-alls.

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
            .map(|p| Regex::new(p))
            .transpose()?;
        let mpn_re = entry
            .r#match
            .mpn_re
            .as_deref()
            .map(|p| Regex::new(&format!("(?i){}", p)))
            .transpose()?;
        Ok(CompiledEntry { entry, value_re, footprint_re, mpn_re })
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
