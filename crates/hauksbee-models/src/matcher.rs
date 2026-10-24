//! Match-rule evaluation engine.
//!
//! A compiled [`CompiledEntry`] pre-compiles all regex patterns from a
//! [`ModelEntry`] so matching is fast at runtime.
//! [`CompiledEntry::specificity_score`] assigns a specificity score so more-specific rules win over catch-alls.
//! The specificity score orders entries only *within* a resolution layer;
//! the layer itself wins first (see `crate::SourceLayer`).
//!
//! module and the schema are one story: what an entry is and how it wins).

use std::sync::OnceLock;

use regex::Regex;

use crate::schema::{ComponentKind, ModelEntry};

/// A model entry with pre-compiled regexes.
pub struct CompiledEntry {
    pub entry: ModelEntry,
    value_re: Option<Regex>,
    footprint_re: Option<Regex>,
    mpn_re: Option<Regex>,
    property_res: Vec<(String, Regex)>,
}

/// A query's value as the value rules read it, worked out once per library scan.
pub struct PreparedValue {
    value: String,
    /// Length of the value without a trailing parenthesised package.
    bare: Option<usize>,
}

impl PreparedValue {
    /// A comma is normalised before the value regex runs, by the same rule
    /// the value parser uses: "4,7k" is 4.7k, "10,000" and "4,700uF" are
    /// thousands groupings.
    ///
    /// A part value often carries its package as a trailing parenthesis
    /// ("BAT54A(SOT23-3)"). The rule may name the whole string or only the
    /// part, so it matches if either spelling does.
    #[must_use]
    pub fn of(q: &ComponentQuery) -> Self {
        let value = crate::value::normalise_comma_decimal(q.value.as_deref().unwrap_or(""));
        // The suffix pattern is anchored, so the bare part is a prefix of the value.
        let bare = strip_package_suffix(&value).map(str::len);
        Self { value, bare }
    }

    fn bare(&self) -> Option<&str> {
        self.bare.map(|end| &self.value[..end])
    }
}

impl CompiledEntry {
    /// Compile a [`ModelEntry`]'s match rules into regex objects.
    ///
    /// Returns an error if any regex pattern is invalid. The `id` from the
    /// entry is included in error messages.
    ///
    /// # Errors
    ///
    /// The [`regex::Error`] of the first `value_re`, `footprint_re`, `mpn_re` or
    /// property pattern that does not compile.
    pub fn compile(entry: ModelEntry) -> Result<Self, regex::Error> {
        // Every pattern is case-INSENSITIVE, footprints included: a rule
        // `footprint_re = "sot-23"` must match `Package_TO_SOT_SMD:SOT-23`. A
        // case-sensitive compile silently skipped the entry, dropping the part
        // to a generic fallback with no diagnostic.
        let ci = |p: &str| Regex::new(&format!("(?i){p}"));
        let rules = &entry.r#match;
        let value_re = rules.value_re.as_deref().map(ci).transpose()?;
        let footprint_re = rules.footprint_re.as_deref().map(ci).transpose()?;
        let mpn_re = rules.mpn_re.as_deref().map(ci).transpose()?;
        let property_res = rules
            .properties
            .iter()
            .map(|(key, pattern)| ci(pattern).map(|re| (normalise_key(key), re)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            entry,
            value_re,
            footprint_re,
            mpn_re,
            property_res,
        })
    }

    /// Whether this entry's match rules fire for the given query fields.
    ///
    /// Rules are AND-ed: every populated rule must match.
    #[must_use]
    pub fn matches(&self, q: &ComponentQuery) -> bool {
        self.matches_prepared(q, &PreparedValue::of(q))
    }

    /// [`Self::matches`] with the query's value already normalised: a library
    /// scan asks every entry about the same query, so the work on the value
    /// (a copy, a regex for a package suffix) is done once, not once per entry.
    #[must_use]
    pub fn matches_prepared(&self, q: &ComponentQuery, prepared: &PreparedValue) -> bool {
        fn field(v: Option<&String>) -> &str {
            v.map_or("", String::as_str)
        }
        // lib_id: a trailing ':' makes it a library prefix, else an exact id.
        let lib_ok = self.entry.r#match.lib_id.as_deref().is_none_or(|lid| {
            let query_lid = field(q.lib_id.as_ref());
            if lid.ends_with(':') {
                query_lid.starts_with(lid)
            } else {
                query_lid == lid
            }
        });
        let value = prepared.value.as_str();
        let bare = prepared.bare();
        let re_ok = |re: &Option<Regex>, text: &str| re.as_ref().is_none_or(|re| re.is_match(text));
        let value_ok = |re: &Option<Regex>| re_ok(re, value) || bare.is_some_and(|b| re_ok(re, b));
        // Named source properties: every authored rule is required, and a
        // missing property is not evidence, so it cannot match.
        let properties_ok = self.property_res.iter().all(|(expected_key, regex)| {
            q.properties
                .iter()
                .find(|(key, _)| normalise_key(key) == *expected_key)
                .is_some_and(|(_, value)| regex.is_match(value))
        });
        // A footprint-only card for a discrete semiconductor guesses the part from
        // its package alone. It never speaks for a U/IC reference: a regulator in
        // a DPAK is not a MOSFET, and a wrong model there is worse than none.
        let rules = &self.entry.r#match;
        let package_only = rules.footprint_re.is_some()
            && rules.lib_id.is_none()
            && rules.value_re.is_none()
            && rules.mpn_re.is_none()
            && rules.properties.is_empty();
        let discrete = matches!(
            self.entry.kind,
            ComponentKind::Diode
                | ComponentKind::BjtNpn
                | ComponentKind::BjtPnp
                | ComponentKind::Nmos
                | ComponentKind::Pmos
        );
        if package_only && discrete && q.is_active_ic() {
            return false;
        }
        lib_ok
            && value_ok(&self.value_re)
            && re_ok(&self.footprint_re, field(q.footprint.as_ref()))
            && re_ok(&self.mpn_re, field(q.mpn.as_ref()))
            && properties_ok
    }

    /// Specificity score (higher = more specific = wins ties).
    ///
    /// Scoring rationale:
    /// - MPN match is the most specific (part number uniquely identifies a device).
    /// - Exact `lib_id` match is more specific than a prefix.
    /// - Value regex is fairly specific.
    /// - Footprint regex is a catch-all fallback.
    #[must_use]
    pub fn specificity_score(&self, q: &ComponentQuery) -> u32 {
        let rules = &self.entry.r#match;
        let lib = match &rules.lib_id {
            Some(lid) if lid.ends_with(':') => 10,
            Some(_) => 20,
            None => 0,
        };
        // The footprint counts only when it actually matched the query.
        let footprint = self
            .footprint_re
            .as_ref()
            .is_some_and(|re| re.is_match(q.footprint.as_deref().unwrap_or("")));
        // A named property that carries an exact order code is at least as
        // discriminating as an MPN field. Count each required property so an
        // exact source-qualified entry wins over the same generic family rule.
        lib + 30 * u32::from(rules.value_re.is_some())
            + 40 * u32::from(rules.mpn_re.is_some())
            + 5 * u32::from(footprint)
            + u32::try_from(rules.properties.len()).map_or(u32::MAX, |n| n.saturating_mul(50))
    }

    /// Confidence points for a value rule that lists the matched value
    /// outright: an anchored, finite set of spellings (case and separators,
    /// ordering codes), not a family pattern, is an exact match (50) whatever
    /// other fields the rule has.
    pub fn outright_bonus(&self) -> u32 {
        let rule = self.entry.r#match.value_re.as_deref();
        50 * u32::from(rule.and_then(crate::extract_output::literal_set).is_some())
    }

    /// Fine-grained regex constrainedness, used as a same-layer tie-break.
    ///
    /// [`specificity_score`](Self::specificity_score) only counts *which*
    /// rule fields are present, so a dedicated override (`^1N4004$`) and the
    /// family entry it carves out of (`^1N400[1-7]$`) tie when both match the
    /// same value. This score ranks the patterns themselves: an exact literal
    /// outranks a character-class/alternation family pattern, so the override
    /// wins deterministically instead of by load order.
    #[must_use]
    pub fn regex_specificity(&self) -> u32 {
        let rules = &self.entry.r#match;
        [&rules.value_re, &rules.mpn_re, &rules.footprint_re]
            .into_iter()
            .flatten()
            .chain(rules.properties.values())
            .map(|pattern| pattern_constrainedness(pattern))
            .sum()
    }
}

/// Heuristic constrainedness of a regex pattern (higher = more exact).
///
/// The pattern is split on top-level `|` and the *least* constrained branch
/// counts, an alternation is only as specific as its loosest arm. Within a
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
    let cs: Vec<char> = branch.chars().collect();
    let mut score = 0u32;
    let mut last_atom: u32 = 0;
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\\' => {
                i += 1;
                // A class escape is one unit, any other escaped char two.
                last_atom = match cs.get(i) {
                    Some('d' | 'D' | 'w' | 'W' | 's' | 'S') => 1,
                    Some(_) => 2,
                    None => 0,
                };
                score += last_atom;
            }
            '[' => {
                score += 1;
                last_atom = 1;
                i += 1;
                while i < cs.len() {
                    match cs[i] {
                        '\\' => i += 1,
                        ']' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            '(' => {
                // Inline flags `(?i)` score nothing; a capturing `(...)` or
                // non-capturing `(?:...)` group is scored by RECURSING on its body
                // via [`pattern_constrainedness`], which splits the body's
                // top-level `|` and takes the LOOSEST (min) arm. This is the whole
                // point of the docstring ("an alternation is only as specific as
                // its loosest arm"): the earlier char scanner never split a
                // group-nested alternation, so each internal `|` and every arm's
                // literals were summed, inflating a grouped-alternation family far
                // above the exact override it carves out of and inverting the
                // same-layer tie-break.
                let mut body_start = i + 1;
                let mut flags_only = false;
                if cs.get(body_start) == Some(&'?') {
                    let mut j = body_start + 1;
                    while j < cs.len() && cs[j] != ':' && cs[j] != ')' {
                        j += 1;
                    }
                    if cs.get(j) == Some(&')') {
                        flags_only = true;
                    }
                    body_start = j + 1; // past the ':' (or past ')' when flags-only)
                }
                // Find the matching ')' (skip classes and escapes).
                let mut depth = 1i32;
                let mut k = i + 1;
                let mut in_cls = false;
                while k < cs.len() {
                    match cs[k] {
                        '\\' => k += 1,
                        '[' if !in_cls => in_cls = true,
                        ']' if in_cls => in_cls = false,
                        '(' if !in_cls => depth += 1,
                        ')' if !in_cls => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                if !flags_only && body_start < k {
                    let body: String = cs[body_start..k].iter().collect();
                    let body_score = pattern_constrainedness(&body);
                    // A grouped ALTERNATION matches a superset of any single arm,
                    // so it is strictly LOOSER than the exact override that pins one
                    // arm, dock one looseness unit below the loosest arm, exactly
                    // as `*`/`?` dock an optional region. This lets a dedicated
                    // exact override (`^ZZ904$`) deterministically beat the family
                    // (`^(ZZ901|…|ZZ905)$`) it carves out of, instead of merely tying
                    // and falling to the id order. A single-branch group (`(AB)`) is
                    // NOT an alternation and is not docked, so `(AB)` == `AB`.
                    let is_alt = split_top_level_alternation(&body).len() > 1;
                    let group_add = body_score.saturating_sub(u32::from(is_alt));
                    score += group_add;
                    // Make the group's WHOLE contribution the retractable atom, so a
                    // following `*`/`?` cancels the entire group (plus the usual dock
                    // unit), not just one unit. An optional group `(W)?` matches zero
                    // chars; leaving last_atom=0 let `^1N4148(W)?$` (group +2, `?`
                    // docked only 1 → net +1) out-score the exact `^1N4148$` override;
                    // the same inversion R32/R42 close for `[..]*` and `(A|B)`.
                    last_atom = group_add;
                } else {
                    last_atom = 0;
                }
                i = k; // the loop's `i += 1` steps past the matching ')'
            }
            '{' => {
                i += 1;
                while i < cs.len() && cs[i] != '}' {
                    i += 1;
                }
                last_atom = 0;
            }
            '^' | '$' => {
                score += 1;
                last_atom = 0;
            }
            '*' | '?' => {
                score = score.saturating_sub(last_atom + 1);
                last_atom = 0;
            }
            ')' | '.' | '+' => last_atom = 0,
            _ => {
                score += 2;
                last_atom = 2;
            }
        }
        i += 1;
    }
    score
}

/// The value without a trailing parenthesised package, when that is what the
/// parenthesis is: a package family followed by a digit ("(SOT23-3)",
/// "(QFN32)", "(SOIC-8_150mil)"), or SMA/SMB/SMC. Anything else in
/// parentheses (a value, "DNP", a manufacturer) is left alone, so a suffix can
/// never turn an unrelated string into a part code.
fn strip_package_suffix(value: &str) -> Option<&str> {
    static PACKAGE: OnceLock<Regex> = OnceLock::new();
    let re = PACKAGE.get_or_init(|| {
        Regex::new(
            r"(?i)^(.*[a-z0-9])\s*\(\s*(?:(?:SOT|SOD|SOIC|SOP|SSOP|TSSOP|MSOP|VSSOP|SO|SC|QFN|DFN|BGA|LGA|DIP|TO|DO|LQFP|TQFP|QFP)[-_ ]?\d[-_ a-z0-9.]*|SM[abc])\s*\)$",
        )
        .expect("static package regex")
    });
    re.captures(value)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// The one key normaliser for property names and table headers.
///
/// Lowercase, every run of non-alphanumerics folded to one `_`, trimmed. `"Mid X"`,
/// `"mid-x"` and `"MID_X"` are all `mid_x`; `"Reference(s)"` is `reference_s`.
#[must_use]
pub fn normalise_key(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.truncate(out.trim_end_matches('_').len());
    out
}

// ── ComponentQuery ────────────────────────────────────────────────────────────

/// All available metadata for a component, used as input to [`crate::ModelLibrary::resolve`].
///
/// All fields are optional; the library matches against whatever is present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ComponentQuery {
    /// KiCad `lib_id`, e.g. `"Device:R"` or `"Device:Q_NPN_BCE"`.
    pub lib_id: Option<String>,
    /// Component value field, e.g. `"10k"`, `"BC847"`, `"100nF"`.
    pub value: Option<String>,
    /// Footprint string, e.g. `"Resistor_THT:R_Axial_…"`.
    pub footprint: Option<String>,
    /// Manufacturer part number (from a schematic property), if present.
    pub mpn: Option<String>,
    /// Source properties retained by the board extractor (datasheet URL,
    /// supplier/order-code metadata, BOM fields, and similar evidence).
    pub properties: Vec<(String, String)>,
    /// Numbered electrical pads on the footprint, for the variant pad-set selector.
    pub pads: Vec<String>,
    /// Reference designator (e.g. `"R1"`), used only in diagnostics.
    pub reference: Option<String>,
}

impl ComponentQuery {
    /// Whether the part is an active IC: by designator (`U`, `IC`, `MCU`), or by
    /// what its symbol, footprint, part number and pad count say when the
    /// designator reads as something else (`S1`, `SW2`).
    #[must_use]
    pub fn is_active_ic(&self) -> bool {
        let text = |f: &Option<String>| f.as_deref().unwrap_or("").to_string();
        let (lib_id, value, footprint) =
            (text(&self.lib_id), text(&self.value), text(&self.footprint));
        crate::refdes::is_active_ic_part(&crate::refdes::PartIdentity {
            reference: self.reference.as_deref().unwrap_or(""),
            lib_id: &lib_id,
            value: &value,
            footprint: &footprint,
            mpn: self.mpn.as_deref(),
            pads: self
                .pads
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
        })
    }
}

pub use crate::refdes::{is_active_ic_ref, is_tool_generated_ref};

/// Longest text field a library match reads.
///
/// A real value, footprint or part number is a few dozen bytes; every library
/// entry is matched against each field in full, so a megabyte-long field would
/// cost a megabyte times every entry. Past this length nothing more can
/// identify a part.
pub const MAX_MATCH_FIELD_BYTES: usize = 1024;

fn clamp_field(field: Option<&str>) -> Option<String> {
    let text = field?;
    if text.len() <= MAX_MATCH_FIELD_BYTES {
        return None;
    }
    let mut end = MAX_MATCH_FIELD_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_string())
}

impl ComponentQuery {
    /// This query with every over-long text field cut to
    /// [`MAX_MATCH_FIELD_BYTES`], or `None` when none is over it (the common
    /// case, which clones nothing).
    #[must_use]
    pub fn bounded_for_matching(&self) -> Option<Self> {
        let (lib_id, value) = (
            clamp_field(self.lib_id.as_deref()),
            clamp_field(self.value.as_deref()),
        );
        let (footprint, mpn) = (
            clamp_field(self.footprint.as_deref()),
            clamp_field(self.mpn.as_deref()),
        );
        if lib_id.is_none() && value.is_none() && footprint.is_none() && mpn.is_none() {
            return None;
        }
        let mut bounded = self.clone();
        for (slot, cut) in [
            (&mut bounded.lib_id, lib_id),
            (&mut bounded.value, value),
            (&mut bounded.footprint, footprint),
            (&mut bounded.mpn, mpn),
        ] {
            if cut.is_some() {
                *slot = cut;
            }
        }
        Some(bounded)
    }

    /// Convenience constructor with the most common fields.
    pub fn new(
        lib_id: impl Into<Option<String>>,
        value: impl Into<Option<String>>,
        footprint: impl Into<Option<String>>,
    ) -> Self {
        Self {
            lib_id: lib_id.into(),
            value: value.into(),
            footprint: footprint.into(),
            ..Default::default()
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{is_active_ic_ref, pattern_constrainedness as c, CompiledEntry, ComponentQuery};
    use crate::schema::{ComponentKind, MatchRules, ModelEntry};

    fn entry(kind: ComponentKind, rules: MatchRules) -> CompiledEntry {
        CompiledEntry::compile(ModelEntry {
            id: "t".to_string(),
            kind,
            r#match: rules,
            ..Default::default()
        })
        .expect("compiles")
    }

    #[test]
    fn one_key_normaliser_for_properties_and_headers() {
        for (raw, want) in [
            ("Mid X", "mid_x"),
            ("mid-x", "mid_x"),
            (" MID_X ", "mid_x"),
            ("Reference(s)", "reference_s"),
            ("Center-X(mm)", "center_x_mm"),
            ("\u{feff}Designator", "designator"),
        ] {
            assert_eq!(super::normalise_key(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn value_commas_follow_the_value_parser() {
        let value_re = |re: &str, value: &str| {
            let compiled = entry(
                ComponentKind::Passive,
                MatchRules {
                    value_re: Some(re.to_string()),
                    ..Default::default()
                },
            );
            compiled.matches(&ComponentQuery {
                value: Some(value.to_string()),
                ..Default::default()
            })
        };
        assert!(value_re("^4\\.7k$", "4,7k"), "European decimal comma");
        assert!(value_re("^4700uF$", "4,700uF"), "thousands grouping");
        assert!(!value_re("^4\\.700uF$", "4,700uF"), "never 1000x small");
    }

    #[test]
    fn footprint_re_matches_case_insensitively() {
        let compiled = entry(
            ComponentKind::BjtNpn,
            MatchRules {
                footprint_re: Some("sot-23".to_string()),
                ..Default::default()
            },
        );
        assert!(compiled.matches(&ComponentQuery {
            footprint: Some("Package_TO_SOT_SMD:SOT-23".to_string()),
            ..Default::default()
        }));
    }

    #[test]
    fn named_source_property_can_disambiguate_a_generic_board_value() {
        let properties = [("KiLib Generator".to_string(), "1812L150-24MR".to_string())].into();
        let compiled = entry(
            ComponentKind::Passive,
            MatchRules {
                value_re: Some("^Polyfuse 1\\.8A$".to_string()),
                properties,
                ..Default::default()
            },
        );
        let with = |url: &str| ComponentQuery {
            value: Some("Polyfuse 1.8A".to_string()),
            properties: vec![("KiLib_Generator".to_string(), url.to_string())],
            ..Default::default()
        };
        assert!(compiled.matches(&with("https://supplier.invalid/1812L150-24MR/123")));
        assert!(!compiled.matches(&with("https://supplier.invalid/1812L110-24MR/456")));
        assert!(!compiled.matches(&ComponentQuery {
            properties: Vec::new(),
            ..with("")
        }));
    }

    #[test]
    fn exact_overrides_outrank_the_families_they_carve_out_of() {
        assert!(c("(?i)^1N4004$") > c("(?i)^1N400[1-7]$"));
        assert!(c("^INA186$") > c("(?i)^INA186"));
        assert!(c("^1N4148$") > c("^1N4148[A-Z0-9-]*$"));
        assert!(c("^ZZ904$") > c("^(ZZ901|ZZ902|ZZ903|ZZ904|ZZ905)$"));
        assert!(c("^BC847$") > c("^(BC846|BC847|BC848)$"));
        assert!(c("^1N4148$") > c("^1N4148(W)?$"));
        assert!(c("^ABC$") > c("^ABC(D|E)?$"));
        assert!(c("^ABC$") > c("^AB.$"));
    }

    #[test]
    fn quantifiers_alternations_and_groups_score_by_what_they_constrain() {
        assert!(c("^1N4148[A-Z0-9-]$") > c("^1N4148[A-Z0-9-]*$"));
        assert!(c("^1N4148[A-Z0-9-]+$") > c("^1N4148[A-Z0-9-]*$"));
        let alt = c("(?i)^BAT5[0-9]$|^BAT43$");
        assert_eq!(alt, c("^BAT5[0-9]$"));
        assert!(c("(?i)^BAT54$") > alt);
        assert_eq!(c("^AB$|^ABCD$"), c("^AB$"));
        assert_eq!(c("(?i)AB"), c("AB"));
        assert_eq!(c("(AB)"), c("AB"));
        assert_eq!(c("^1N4148W$"), c("^1N4148(W)$"));
    }

    #[test]
    fn a_trailing_package_parenthesis_does_not_hide_the_part_code() {
        let card = entry(
            ComponentKind::Diode,
            MatchRules {
                value_re: Some("^BAT54[AC]?$".to_string()),
                ..Default::default()
            },
        );
        let value = |v: &str| ComponentQuery {
            value: Some(v.to_string()),
            ..Default::default()
        };
        // Legal control: the package suffix is dropped before matching.
        assert!(card.matches(&value("BAT54A(SOT23-3)")));
        assert!(card.matches(&value("BAT54C (SOT-23)")));
        assert!(card.matches(&value("BAT54")));
        // Not a package: a note, a value or another part stays unmatched.
        assert!(!card.matches(&value("BAT54A(DNP)")));
        assert!(!card.matches(&value("BAT54A(SUNHOLD)")));
        // The stripped part must still be the part: an unrelated code with a
        // package suffix does not match, and a bare package is not a part.
        assert!(!card.matches(&value("BAT46(SOT23-3)")));
        assert!(!card.matches(&value("(SOT23-3)")));
        assert!(!card.matches(&value("XBAT54A(SOT23-3)")));
    }

    #[test]
    fn eagle_auto_named_elements_are_not_active_ics() {
        for auto in ["U$1", "U$12", "IC$3", "R$7", "R1"] {
            assert!(!is_active_ic_ref(auto), "{auto}");
        }
        for real in ["U1", "U12", "IC3", "MCU1", "U$"] {
            assert!(is_active_ic_ref(real), "{real}");
        }
    }

    /// A footprint-only card for a discrete semiconductor never speaks for a
    /// chip, whatever the chip's designator: an Eagle auto-named `U$1` that
    /// lands a QFP is an IC by what its record says, so the MOSFET card for a
    /// generic power package is refused for it, while a real transistor under the
    /// same kind of designator keeps matching.
    #[test]
    fn a_footprint_only_discrete_card_is_refused_for_a_chip_under_any_designator() {
        let card = entry(
            ComponentKind::Nmos,
            MatchRules {
                footprint_re: Some("sot-23|qfp".to_string()),
                ..Default::default()
            },
        );
        let chip = ComponentQuery {
            reference: Some("U$1".to_string()),
            value: Some("ATSAMD51J".to_string()),
            footprint: Some("Package_QFP:TQFP-64_10x10mm_P0.5mm".to_string()),
            pads: (1..=64).map(|n| n.to_string()).collect(),
            ..Default::default()
        };
        assert!(chip.is_active_ic());
        assert!(!card.matches(&chip), "a 64-pad QFP is not a MOSFET");
        let fet = ComponentQuery {
            reference: Some("Q$3".to_string()),
            value: Some("BSS138".to_string()),
            footprint: Some("Package_TO_SOT_SMD:SOT-23".to_string()),
            pads: ["1", "2", "3"].map(String::from).to_vec(),
            ..Default::default()
        };
        assert!(!fet.is_active_ic());
        assert!(card.matches(&fet));
    }
}
