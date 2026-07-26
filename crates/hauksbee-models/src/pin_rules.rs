//! Configurable pin-role inference rules.
//!
//! Layout-only board sources (a `.kicad_pcb`, or Board-as-Code decompiled from
//! one) carry pads with bare *numbers* and no electrode role: a diode footprint
//! has pads `1`/`2`, never `anode`/`cathode`. A netlist *does* carry the role
//! (`pinfunction "A"`/`"K"`), but the moment a board round-trips through a layout
//! the role is gone, and a role-dependent binder (diode, BJT, MOSFET) can no
//! longer tell which pad is which, so the part fails to bind.
//!
//! This module is the inference layer that closes that gap. A [`PinRuleTable`]
//! holds an ordered list of [`PinRule`]s; each rule matches on a footprint regex
//! and/or a part kind plus an exact pad count, and maps pad numbers to roles.
//! When the binder finds a pad with no explicit role it consults the table; the
//! first matching rule wins. Every role assigned this way is a *guess* and is
//! reported as such (the binder emits a warning naming the component, pad, role,
//! and the rule id), so nothing is silently inferred.
//!
//! The table is seeded from the built-in `db/pin_rules.toml` and is
//! user-extensible: drop a `pin_rules.toml` (any file with a `[[pin_rules]]`
//! array) into a model directory (`~/.config/hauksbee/models`, `--models-dir`,
//! …) and those rules are layered on top, highest priority first, so a user can
//! override or extend the built-ins without recompiling.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use regex::Regex;

use crate::ComponentKind;

/// Top-level container for a `pin_rules.toml` file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PinRuleFile {
    #[serde(default)]
    pub pin_rules: Vec<PinRule>,
}

/// One pad-number → role inference rule.
///
/// A rule matches a component when *every* populated condition holds:
/// `footprint_re` (if set) matches the component footprint, `kind` (if set)
/// equals the resolved part kind, and `pad_count` (if set) equals the number of
/// pads on the component. At least one condition must be present.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PinRule {
    /// Stable identifier, surfaced in the guess-warning so a reader can find the
    /// rule that fired.
    pub id: String,

    /// Human-readable description for documentation / diagnostics.
    #[serde(default)]
    pub description: String,

    /// Case-insensitive regex matched against the component footprint string
    /// (e.g. `"^Diode_SMD:|SOD-|SMA|SMB"`).
    #[serde(default)]
    pub footprint_re: Option<String>,

    /// Resolved component kind this rule applies to (`diode`, `bjt_npn`, …).
    #[serde(default)]
    pub kind: Option<ComponentKind>,

    /// Exact pad count the component must have for this rule to fire.
    #[serde(default)]
    pub pad_count: Option<usize>,

    /// Pad number → role map (e.g. `{ "1" = "cathode", "2" = "anode" }`).
    pub roles: BTreeMap<String, String>,
}

/// A compiled rule: its footprint regex pre-built once.
#[derive(Debug, Clone)]
struct CompiledRule {
    rule: PinRule,
    footprint_re: Option<Regex>,
}

/// An ordered set of pin-role rules. Earlier rules win; user rules are inserted
/// ahead of the built-ins so they override.
#[derive(Debug, Clone, Default)]
pub struct PinRuleTable {
    rules: Vec<CompiledRule>,
}

/// The outcome of consulting the table for one pad: the inferred role plus the
/// id of the rule that supplied it (for the guess-warning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredRole {
    pub role: String,
    pub rule_id: String,
}

impl PinRuleTable {
    /// An empty table (no rules).
    pub fn empty() -> Self {
        PinRuleTable { rules: Vec::new() }
    }

    /// Parse and append rules from one `pin_rules.toml` source string.
    ///
    /// `prepend` puts the new rules *ahead* of the existing ones (so a later,
    /// user-supplied file overrides the built-ins). Returns the rule ids loaded.
    pub fn load_toml_str(&mut self, src: &str, prepend: bool) -> Result<Vec<String>, String> {
        let file: PinRuleFile =
            toml::from_str(src).map_err(|e| format!("pin_rules: TOML parse error: {e}"))?;
        let mut compiled = Vec::new();
        let mut ids = Vec::new();
        for rule in file.pin_rules {
            if rule.footprint_re.is_none() && rule.kind.is_none() && rule.pad_count.is_none() {
                return Err(format!(
                    "pin_rules: rule '{}' has no match condition (need footprint_re, kind, or pad_count)",
                    rule.id
                ));
            }
            if rule.roles.is_empty() {
                return Err(format!(
                    "pin_rules: rule '{}' maps no pad to a role",
                    rule.id
                ));
            }
            let footprint_re =
                match &rule.footprint_re {
                    Some(p) => Some(Regex::new(&format!("(?i){p}")).map_err(|e| {
                        format!("pin_rules: rule '{}' bad footprint_re: {e}", rule.id)
                    })?),
                    None => None,
                };
            ids.push(rule.id.clone());
            compiled.push(CompiledRule { rule, footprint_re });
        }
        if prepend {
            // Keep the new file's internal order, but ahead of everything loaded
            // before it.
            compiled.append(&mut self.rules);
            self.rules = compiled;
        } else {
            self.rules.extend(compiled);
        }
        Ok(ids)
    }

    /// Number of loaded rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the table has no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Resolve the role for one pad of a component, if any rule matches.
    ///
    /// `footprint` and `kind` describe the component; `pad_count` is its number
    /// of pads; `pad_number` is the pad we want a role for. The first rule whose
    /// conditions all hold *and* that maps this pad number wins.
    pub fn role_for_pad(
        &self,
        footprint: &str,
        kind: Option<ComponentKind>,
        pad_count: usize,
        pad_number: &str,
    ) -> Option<InferredRole> {
        for cr in &self.rules {
            if !cr.matches(footprint, kind, pad_count) {
                continue;
            }
            if let Some(role) = cr.rule.roles.get(pad_number) {
                return Some(InferredRole {
                    role: role.clone(),
                    rule_id: cr.rule.id.clone(),
                });
            }
        }
        None
    }
}

impl CompiledRule {
    fn matches(&self, footprint: &str, kind: Option<ComponentKind>, pad_count: usize) -> bool {
        if let Some(re) = &self.footprint_re {
            if !re.is_match(footprint) {
                return false;
            }
        }
        if let Some(k) = self.rule.kind {
            if Some(k) != kind {
                return false;
            }
        }
        if let Some(n) = self.rule.pad_count {
            if n != pad_count {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diode_table() -> PinRuleTable {
        let mut t = PinRuleTable::empty();
        t.load_toml_str(
            r#"
            [[pin_rules]]
            id = "diode_2pin"
            footprint_re = "SOD-|SMA|SMB|^D_"
            kind = "diode"
            pad_count = 2
            roles = { "1" = "cathode", "2" = "anode" }
            "#,
            false,
        )
        .unwrap();
        t
    }

    #[test]
    fn diode_rule_maps_pads() {
        let t = diode_table();
        let r = t
            .role_for_pad("Diode_SMD:D_SOD-323", Some(ComponentKind::Diode), 2, "1")
            .unwrap();
        assert_eq!(r.role, "cathode");
        assert_eq!(r.rule_id, "diode_2pin");
        assert_eq!(
            t.role_for_pad("Diode_SMD:D_SOD-323", Some(ComponentKind::Diode), 2, "2")
                .unwrap()
                .role,
            "anode"
        );
    }

    #[test]
    fn wrong_kind_or_count_does_not_match() {
        let t = diode_table();
        assert!(t
            .role_for_pad("Diode_SMD:D_SOD-323", Some(ComponentKind::Passive), 2, "1")
            .is_none());
        assert!(t
            .role_for_pad("Diode_SMD:D_SOD-323", Some(ComponentKind::Diode), 3, "1")
            .is_none());
    }

    #[test]
    fn user_rule_prepended_overrides_builtin() {
        let mut t = diode_table();
        // A user file that flips the polarity for the same footprint family.
        t.load_toml_str(
            r#"
            [[pin_rules]]
            id = "user_flip"
            footprint_re = "SOD-"
            kind = "diode"
            pad_count = 2
            roles = { "1" = "anode", "2" = "cathode" }
            "#,
            true,
        )
        .unwrap();
        let r = t
            .role_for_pad("Diode_SMD:D_SOD-323", Some(ComponentKind::Diode), 2, "1")
            .unwrap();
        assert_eq!(r.role, "anode", "user rule must win");
        assert_eq!(r.rule_id, "user_flip");
    }
}
