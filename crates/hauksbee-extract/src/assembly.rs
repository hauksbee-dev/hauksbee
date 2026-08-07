//! The shared three-state assembled-component contract.
//!
//! Every semantic check eventually asks the same question about a component
//! record: "is this part actually on the assembled board, as the part the
//! record claims to be?" Before this module, the answer lived in two
//! vocabularies in two crates: the DNP decision (`dnp.rs`) answered the
//! present-versus-absent axis, and the engine's identity-refusal detection
//! answered the trusted-versus-ambiguous axis. A check that consulted one and
//! not the other let a part leak through as ordinary and fitted: Board-as-Code
//! recompiled an unknown part into a fitted one, and the binder, ideal-supply
//! and ampacity paths each needed an individual fix to leave refused
//! identities open. Those were point fixes; this type is the contract that
//! stops the next check from needing its own.
//!
//! [`AssemblyState::of`] classifies one component record into exactly one of
//! three states:
//!
//! - **Present**: assembled and identity-trusted. Carries the only
//!   [`FittedComponent`] witness, which is the sole ticket into model
//!   resolution (the engine's binder refuses to resolve anything else), so a
//!   check cannot obtain an electrical model without having answered the
//!   three-state question first.
//! - **DnpAbsent**: on the layout but not assembled, with the [`DnpReason`]
//!   the DNP policy recorded (or the bare board-file flag when no policy ran).
//! - **IdentityUnknown**: the record's identity is refused (a conflicting
//!   duplicate designator, or an inferred ambiguous reference without an
//!   authoritative source UID), so nothing about it, including its DNP flag,
//!   is evidence.
//!
//! Refusal is checked before DNP on purpose: a record whose identity is
//! contradictory cannot vouch for its own DNP flag, so the more conservative
//! state wins.
//!
//! Upstream machinery is deliberately NOT a consumer: the DNP policy
//! (`ExtractedBoard::apply_dnp_policy`) decides fit/no-fit and so produces
//! this state rather than asking it, and identity application
//! (BOM/placement reconciliation in the engine's binder) exists to repair
//! identity before the state is classified.

use crate::dnp::{DnpReason, DNP_REASON_KEY};
use crate::Component;

/// Why a component record's identity is refused: the record exists, but which
/// physical part it names cannot be answered from the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityRefusal {
    /// Two populated records share one reference designator with contradictory
    /// electrical identity. The detail string names the contradiction.
    DuplicateReferenceConflict(String),
    /// The reference was inferred during extraction and no authoritative
    /// source unique ID backs it, so the designator may belong to a different
    /// part than the record's value claims.
    AmbiguousReferenceWithoutUid,
}

impl IdentityRefusal {
    /// The human-readable reason a report prints.
    pub fn reason(&self) -> String {
        match self {
            Self::DuplicateReferenceConflict(detail) => {
                format!("ambiguous duplicate designator: {detail}")
            }
            Self::AmbiguousReferenceWithoutUid => {
                "ambiguous inferred reference without an authoritative source UID".to_string()
            }
        }
    }
}

/// Why the part is not on the assembled board.
///
/// `reason` is the fine-grained decision the DNP policy recorded on the
/// component ([`DNP_REASON_KEY`]); `None` means the board file marks the part
/// DNP and no policy has (re)fitted it, which is the same physical fact with
/// less history attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnpAbsence {
    pub reason: Option<DnpReason>,
}

impl DnpAbsence {
    /// The line a report prints for this absence.
    pub fn describe(&self) -> &'static str {
        match &self.reason {
            Some(reason) => reason.describe(),
            None => "DNP (not populated)",
        }
    }
}

/// The witness that a component is assembled and identity-trusted.
///
/// The only way to construct one is [`AssemblyState::of`] returning
/// [`AssemblyState::Present`]; the private field keeps every other path out.
/// APIs that hand back an electrical model (the engine binder's `resolve`,
/// its role-net reader) take this instead of a bare [`Component`], which makes
/// "you answered the three-state question" a compile-time fact rather than a
/// per-call-site discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FittedComponent<'c> {
    part: &'c Component,
}

impl<'c> FittedComponent<'c> {
    /// The underlying record, for read access with the original lifetime.
    pub fn component(self) -> &'c Component {
        self.part
    }
}

impl std::ops::Deref for FittedComponent<'_> {
    type Target = Component;

    fn deref(&self) -> &Component {
        self.part
    }
}

/// One component record, classified: present, DNP-absent, or
/// identity-unknown. See the module doc for why exactly these three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyState<'c> {
    /// Assembled and identity-trusted; carries the model-resolution witness.
    Present(FittedComponent<'c>),
    /// On the layout, not on the assembled board.
    DnpAbsent(DnpAbsence),
    /// The record's identity is refused; nothing about it is evidence.
    IdentityUnknown(IdentityRefusal),
}

impl<'c> AssemblyState<'c> {
    /// Classify one component record. This is the single entry point: every
    /// consumer that used to read `component.dnp` or probe the identity
    /// properties asks this instead, so no two checks can disagree about what
    /// "present" means.
    pub fn of(component: &'c Component) -> AssemblyState<'c> {
        if let Some(refusal) = identity_refusal(component) {
            return AssemblyState::IdentityUnknown(refusal);
        }
        if component.dnp {
            let reason = component
                .properties
                .iter()
                .find(|(key, _)| key == DNP_REASON_KEY)
                .and_then(|(_, tag)| DnpReason::from_policy_tag(tag));
            return AssemblyState::DnpAbsent(DnpAbsence { reason });
        }
        AssemblyState::Present(FittedComponent { part: component })
    }

    /// The witness, if the part is present. Consuming (the state is `Copy`able
    /// data plus a borrow) so `AssemblyState::of(c).fitted()` composes.
    pub fn fitted(self) -> Option<FittedComponent<'c>> {
        match self {
            AssemblyState::Present(part) => Some(part),
            _ => None,
        }
    }

    /// True when the part is assembled and identity-trusted.
    pub fn is_present(&self) -> bool {
        matches!(self, AssemblyState::Present(_))
    }

    /// The human-readable reason the part is NOT an ordinary fitted component,
    /// or `None` when it is present. Reports print this so a skip is never
    /// silent.
    pub fn absence(&self) -> Option<String> {
        match self {
            AssemblyState::Present(_) => None,
            AssemblyState::DnpAbsent(absence) => Some(absence.describe().to_string()),
            AssemblyState::IdentityUnknown(refusal) => Some(refusal.reason()),
        }
    }
}

/// The one detector for refused identity. Private: consumers ask
/// [`AssemblyState::of`], so presence and identity can never be consulted
/// separately again.
fn identity_refusal(component: &Component) -> Option<IdentityRefusal> {
    if let Some((_, detail)) = component
        .properties
        .iter()
        .find(|(key, _)| key == crate::DUPLICATE_REFERENCE_CONFLICT_KEY)
    {
        return Some(IdentityRefusal::DuplicateReferenceConflict(detail.clone()));
    }

    let reference_is_ambiguous = component
        .properties
        .iter()
        .any(|(key, _)| key == crate::altium::REFERENCE_AMBIGUOUS_KEY);
    if reference_is_ambiguous {
        let has_authoritative_uid = component.properties.iter().any(|(key, value)| {
            key == crate::altium::SOURCE_UNIQUE_ID_KEY && !value.trim().is_empty()
        });
        if !has_authoritative_uid {
            return Some(IdentityRefusal::AmbiguousReferenceWithoutUid);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::altium::{REFERENCE_AMBIGUOUS_KEY, SOURCE_UNIQUE_ID_KEY};
    use crate::DUPLICATE_REFERENCE_CONFLICT_KEY;

    fn component(dnp: bool, properties: &[(&str, &str)]) -> Component {
        Component {
            reference: "U1".into(),
            value: "TEST".into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: properties
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            dnp,
            pins: Vec::new(),
        }
    }

    #[test]
    fn an_ordinary_record_is_present_and_yields_the_witness() {
        let c = component(false, &[]);
        let state = AssemblyState::of(&c);
        assert!(state.is_present());
        assert_eq!(state.absence(), None);
        let part = AssemblyState::of(&c).fitted().expect("present");
        assert_eq!(part.reference, "U1");
    }

    #[test]
    fn a_dnp_record_is_absent_with_the_recorded_reason() {
        let bare = component(true, &[]);
        match AssemblyState::of(&bare) {
            AssemblyState::DnpAbsent(absence) => {
                assert_eq!(absence.reason, None);
                assert_eq!(absence.describe(), "DNP (not populated)");
            }
            other => panic!("bare DNP flag must classify DnpAbsent, got {other:?}"),
        }
        assert!(AssemblyState::of(&bare).fitted().is_none());

        let reasoned = component(true, &[(DNP_REASON_KEY, DnpReason::ZeroOhmLink.policy_tag())]);
        match AssemblyState::of(&reasoned) {
            AssemblyState::DnpAbsent(absence) => {
                assert_eq!(absence.reason, Some(DnpReason::ZeroOhmLink));
            }
            other => panic!("recorded reason must survive classification, got {other:?}"),
        }
    }

    #[test]
    fn inferred_ambiguous_identity_requires_a_nonempty_authoritative_uid() {
        let ambiguous = component(false, &[(REFERENCE_AMBIGUOUS_KEY, "true")]);
        assert_eq!(
            AssemblyState::of(&ambiguous),
            AssemblyState::IdentityUnknown(IdentityRefusal::AmbiguousReferenceWithoutUid)
        );
        let empty_uid = component(
            false,
            &[
                (REFERENCE_AMBIGUOUS_KEY, "true"),
                (SOURCE_UNIQUE_ID_KEY, "  "),
            ],
        );
        assert!(!AssemblyState::of(&empty_uid).is_present());
        let authoritative = component(
            false,
            &[
                (REFERENCE_AMBIGUOUS_KEY, "true"),
                (SOURCE_UNIQUE_ID_KEY, "ABC-123"),
            ],
        );
        assert!(AssemblyState::of(&authoritative).is_present());
    }

    #[test]
    fn duplicate_conflict_refuses_even_when_a_uid_exists() {
        let c = component(
            false,
            &[
                (DUPLICATE_REFERENCE_CONFLICT_KEY, "different values"),
                (SOURCE_UNIQUE_ID_KEY, "ABC-123"),
            ],
        );
        assert_eq!(
            AssemblyState::of(&c),
            AssemblyState::IdentityUnknown(IdentityRefusal::DuplicateReferenceConflict(
                "different values".into()
            ))
        );
    }

    #[test]
    fn refused_identity_wins_over_the_dnp_flag() {
        // A record whose identity is contradictory cannot vouch for its own
        // DNP flag, so refusal is the state even when dnp is set.
        let c = component(true, &[(DUPLICATE_REFERENCE_CONFLICT_KEY, "two boards")]);
        assert!(matches!(
            AssemblyState::of(&c),
            AssemblyState::IdentityUnknown(_)
        ));
    }
}
