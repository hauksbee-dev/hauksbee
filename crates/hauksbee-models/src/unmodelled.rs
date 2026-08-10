//! Named abstentions: parts the library knows it cannot model, and what would
//! unlock each one.
//!
//! # Why this exists
//!
//! When no model matches, the report says the part is treated as an open circuit
//! "because: No model in the library matched this part", and tells the reader to
//! "add a model for U201 to your models directory". Both sentences are true and
//! neither is useful, because they are the same sentences for a part nobody has
//! looked at and for a part somebody has looked at closely and concluded cannot
//! honestly be modelled yet. Those are different situations and a reader deserves
//! to be able to tell them apart:
//!
//!   - Nobody has looked at it. The reader's next step is to write an entry.
//!   - It has been looked at, and the blocker is named. The reader's next step is
//!     whatever the blocker says: commit to a strap, supply a vendor SPICE card,
//!     resolve a package contradiction on their own board. Sometimes the answer is
//!     that no datasheet in the world unlocks it and the ambiguity is in the board
//!     file, which is worth knowing before spending an afternoon looking.
//!
//! This table carries the second case. An entry here changes NOTHING about
//! binding: it cannot make a part resolve, cannot stamp a device, and cannot move
//! `critical_parts_bound`. It can only replace two generic sentences in the
//! disclosure with specific ones. That containment is deliberate and is the reason
//! this is a separate table rather than a `[[models]]` entry with an escape hatch:
//! a model entry that meant "do not model this" would be one bad merge away from
//! counting as coverage.
//!
//! # Shape
//!
//! ```toml
//! [[unmodelled]]
//! id = "si53301"
//! value_re = "(?i)^Si53301"
//! # Becomes the disclosure's "because".
//! because = "its output format is strap-selected per bank ..."
//! # Becomes the disclosure's "what to do".
//! unlocked_by = "a board-level declaration of the SFOUT strap ..."
//! ```
//!
//! Loaded from any DB file carrying an `[[unmodelled]]` array, and layered the
//! same way `pin_rules` is: a user file's entries go ahead of the built-ins, so a
//! reader who disagrees with an abstention can override its text (or supply a real
//! model and never reach this table at all).

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Top-level container for a file carrying an `[[unmodelled]]` array.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UnmodelledFile {
    #[serde(default)]
    pub unmodelled: Vec<UnmodelledPart>,
}

/// One named abstention.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UnmodelledPart {
    /// Stable identifier, for diagnostics and so a user file can be seen to
    /// override a built-in.
    pub id: String,

    /// Case-insensitive regex matched against the component value / MPN string.
    /// Required: an abstention with no match rule would claim every part.
    pub value_re: String,

    /// Why the library abstains, as a sentence. Becomes the disclosure's
    /// "because", replacing "No model in the library matched this part".
    pub because: String,

    /// What would unlock it, as a sentence. Becomes the disclosure's "what to
    /// do", replacing the generic "add a model to your models directory".
    ///
    /// This is the field the whole table exists for, so it is REQUIRED. An
    /// abstention that does not name its unlocking input is the thing this table
    /// was built to stop being possible.
    pub unlocked_by: String,
}

/// A compiled abstention: its value regex pre-built once, and its note built once
/// so `note_for` can hand out a reference instead of a clone.
#[derive(Debug, Clone)]
struct Compiled {
    value_re: Regex,
    note: UnmodelledNote,
}

/// An ordered set of abstentions. Earlier entries win; user entries are inserted
/// ahead of the built-ins so they override.
#[derive(Debug, Clone, Default)]
pub struct UnmodelledTable {
    parts: Vec<Compiled>,
}

/// The disclosure text for one matched abstention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmodelledNote {
    /// The id of the entry that matched, so a reader can find it.
    pub id: String,
    /// Why the library abstains.
    pub because: String,
    /// What would unlock it.
    pub unlocked_by: String,
}

impl UnmodelledTable {
    /// An empty table.
    pub fn empty() -> Self {
        UnmodelledTable { parts: Vec::new() }
    }

    /// Parse and append abstentions from one TOML source string.
    ///
    /// `prepend` puts the new entries *ahead* of the existing ones, so a later
    /// user-supplied file overrides the built-ins. Returns the ids loaded.
    pub fn load_toml_str(&mut self, src: &str, prepend: bool) -> Result<Vec<String>, String> {
        let file: UnmodelledFile =
            toml::from_str(src).map_err(|e| format!("unmodelled: TOML parse error: {e}"))?;
        let mut compiled = Vec::new();
        let mut ids = Vec::new();
        for part in file.unmodelled {
            if part.value_re.trim().is_empty() {
                return Err(format!(
                    "unmodelled: entry '{}' has an empty value_re, which would claim every part",
                    part.id
                ));
            }
            // Both sentences are load-bearing: the table's entire purpose is to
            // replace two generic ones, and a blank field would leave the reader
            // with less than the generic text rather than more.
            if part.because.trim().is_empty() {
                return Err(format!(
                    "unmodelled: entry '{}' states no `because`",
                    part.id
                ));
            }
            if part.unlocked_by.trim().is_empty() {
                return Err(format!(
                    "unmodelled: entry '{}' states no `unlocked_by`; an abstention that \
                     does not name what would unlock it is what this table exists to prevent",
                    part.id
                ));
            }
            let value_re = Regex::new(&part.value_re)
                .map_err(|e| format!("unmodelled: entry '{}' bad value_re: {e}", part.id))?;
            ids.push(part.id.clone());
            compiled.push(Compiled {
                value_re,
                note: UnmodelledNote {
                    id: part.id,
                    because: part.because,
                    unlocked_by: part.unlocked_by,
                },
            });
        }
        if prepend {
            compiled.append(&mut self.parts);
            self.parts = compiled;
        } else {
            self.parts.extend(compiled);
        }
        Ok(ids)
    }

    /// Number of loaded abstentions.
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The note for a component, if one is declared.
    ///
    /// `value` and `mpn` are both consulted because a board may carry the part
    /// number in either; the first entry matching either wins.
    pub fn note_for(&self, value: &str, mpn: &str) -> Option<&UnmodelledNote> {
        self.parts
            .iter()
            .find(|c| c.value_re.is_match(value) || (!mpn.is_empty() && c.value_re.is_match(mpn)))
            .map(|c| &c.note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(src: &str) -> UnmodelledTable {
        let mut t = UnmodelledTable::empty();
        t.load_toml_str(src, false).expect("loads");
        t
    }

    /// The guard the whole table exists for: an abstention that does not name what
    /// would unlock it must not load. Without this the table would be a place to
    /// write "we don't model this" at length and call it a disclosure.
    #[test]
    fn an_abstention_with_no_unlocking_input_is_refused() {
        let mut t = UnmodelledTable::empty();
        let err = t
            .load_toml_str(
                r#"
                [[unmodelled]]
                id = "x"
                value_re = "^X$"
                because = "it is hard"
                unlocked_by = "   "
                "#,
                false,
            )
            .expect_err("a blank unlocked_by must refuse");
        assert!(
            err.contains("unlocked_by"),
            "the error must name the missing field: {err}"
        );
        assert!(t.is_empty(), "nothing may load from a rejected file");

        // And the other two fields are load-bearing too.
        for (field, src) in [
            (
                "because",
                r#"[[unmodelled]]
                   id = "x"
                   value_re = "^X$"
                   because = ""
                   unlocked_by = "a datasheet""#,
            ),
            (
                "value_re",
                r#"[[unmodelled]]
                   id = "x"
                   value_re = ""
                   because = "reasons"
                   unlocked_by = "a datasheet""#,
            ),
        ] {
            let mut t = UnmodelledTable::empty();
            let err = match t.load_toml_str(src, false) {
                Err(e) => e,
                Ok(ids) => panic!("a blank {field} must refuse, but loaded {ids:?}"),
            };
            assert!(
                err.contains(field),
                "the error for a blank {field} must name it: {err}"
            );
            assert!(t.is_empty());
        }
    }

    /// A note is found by value OR by MPN, because a board may carry the part
    /// number in either field.
    #[test]
    fn a_note_matches_on_value_or_on_mpn() {
        let t = table(
            r#"
            [[unmodelled]]
            id = "widget"
            value_re = "(?i)^WIDGET99"
            because = "no vendor card exists"
            unlocked_by = "a Gummel-Poon card from the vendor"
            "#,
        );
        assert_eq!(t.len(), 1);
        assert_eq!(
            t.note_for("WIDGET99", "").map(|n| n.id.as_str()),
            Some("widget")
        );
        assert_eq!(
            t.note_for("widget99x", "").map(|n| n.id.as_str()),
            Some("widget")
        );
        // Value says nothing, MPN carries it.
        assert_eq!(
            t.note_for("U5", "WIDGET99A").map(|n| n.id.as_str()),
            Some("widget")
        );
        // Neither matches.
        assert!(t.note_for("WIDGET98", "").is_none());
        assert!(t.note_for("", "").is_none());
    }

    /// An earlier entry wins, and `prepend` is what lets a user file override a
    /// built-in abstention's text rather than being appended behind it.
    #[test]
    fn a_prepended_entry_overrides_an_earlier_one() {
        let mut t = table(
            r#"
            [[unmodelled]]
            id = "builtin"
            value_re = "^PART$"
            because = "the built-in reason"
            unlocked_by = "the built-in unlock"
            "#,
        );
        t.load_toml_str(
            r#"
            [[unmodelled]]
            id = "user"
            value_re = "^PART$"
            because = "the user's reason"
            unlocked_by = "the user's unlock"
            "#,
            true,
        )
        .expect("loads");
        assert_eq!(t.len(), 2);
        let n = t.note_for("PART", "").expect("matches");
        assert_eq!(n.id, "user", "a prepended entry must win");
        assert_eq!(n.unlocked_by, "the user's unlock");
    }
}
