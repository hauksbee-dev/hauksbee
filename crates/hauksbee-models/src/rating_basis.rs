//! Where a part's limits come from, and what passing one may say.
//!
//! A rating is evidence about a board only when it is the fitted part's own:
//! the part's card, matched to it and sourced from its datasheet, or a limit
//! the board itself states (a capacitor value's "16V", a fuse value's hold
//! current, a resistor footprint's package wattage). A card the resolver fell
//! back to (an estimated-fallback placeholder, such as the generic power FET's
//! nominal 30 V) or guessed (matched on the part's class, colour or footprint,
//! not on the part) carries a stand-in limit. The real part may be rated far
//! past it, so passing it says nothing about the board: it is a stated
//! condition ("judged against a generic 30 V rating; give Q1 its own model"),
//! never a failing finding. A tool or model limit is never a board fault.
//!
//! Every check that compares a value with a card's limit asks
//! [`RatingBasis::judges`] first, and words an exceedance of a stand-in limit
//! with [`condition`]; the verdict reads such a finding back with
//! [`is_condition`]. The basis rides on [`crate::schema::Ratings::basis`], so
//! it travels wherever the ratings do.

use hauksbee_ir::evidence::ModelSourceTier;

use crate::{Confidence, Resolution};

/// Which of a part's limits a comparison is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Continuous or surge current through the part.
    Current,
    /// Dissipation, as the card rates it.
    Power,
    /// Blocking, working or supply voltage.
    Voltage,
    /// Whether the part is polarized, so reverse bias harms it.
    Polarity,
    /// Per-pin source or sink current.
    PinCurrent,
    /// A capacitor's rated ripple current.
    RippleCurrent,
    /// Junction temperature (and the dissipation that sets it).
    JunctionTemp,
    /// A card's operating envelope (supply range, input common mode).
    Envelope,
}

/// Why a card's limits are not the part's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandInKind {
    /// A generic estimated-fallback card: its numbers belong to no datasheet.
    Placeholder,
    /// A real card the resolver guessed from the part's class, value or
    /// footprint, not one matched to this part.
    Guessed,
    /// The part's own card and the board state no such limit at all (a
    /// capacitor whose value names no rated voltage), so the lowest value its
    /// class is sold with is assumed: a floor, not a rating.
    Unstated,
}

/// The card a part's limits came from when they are not its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandIn {
    /// The card's model id.
    pub card: String,
    pub kind: StandInKind,
}

impl StandIn {
    /// "generic" or "guessed", the adjective the condition gives the rating.
    #[must_use]
    pub const fn adjective(&self) -> &'static str {
        match self.kind {
            StandInKind::Placeholder => "generic",
            StandInKind::Guessed => "guessed",
            StandInKind::Unstated => "assumed",
        }
    }

    /// The provenance of the rating, in words.
    #[must_use]
    pub fn provenance(&self) -> String {
        let why = match self.kind {
            StandInKind::Placeholder => "a placeholder card, not from any datasheet",
            StandInKind::Guessed => {
                "a card guessed from the part's class, value or footprint, not matched to its part number"
            }
            StandInKind::Unstated => {
                "the value and fields state no such rating, so the lowest value the part's class \
                 is sold with is assumed"
            }
        };
        format!("card '{}', {why}", self.card)
    }

    /// What the board's author can do to have `part` judged on its own limit.
    #[must_use]
    pub fn remedy(&self, part: &str) -> String {
        match self.kind {
            StandInKind::Unstated => format!(
                "state {part}'s rated voltage in its value (for example 10uF 25V) or give it \
                 its manufacturer part number"
            ),
            _ => format!(
                "give {part} its own model (its manufacturer part number, or a card from its \
                 datasheet)"
            ),
        }
    }
}

/// Where one bound part's limits come from. The default is the part's own
/// card: a rating with no stated basis is judged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RatingBasis {
    /// The card the limits came from, when it is not the part's own.
    pub stand_in: Option<StandIn>,
    /// Limits the board states itself (a value's rated voltage or hold
    /// current), which stay the part's own whatever card it bound.
    pub board_stated: Vec<Limit>,
    /// Stand-in limits for single limits of an otherwise own card: a floor
    /// assumed where neither the card nor the board states one
    /// ([`StandInKind::Unstated`]). Other limits of the part stay judged.
    pub assumed: Vec<(Limit, StandIn)>,
}

impl RatingBasis {
    /// The basis of `res`'s card: a placeholder when its source tier is an
    /// estimated fallback (or open), guessed when the resolver matched it on
    /// no more than the part's class, value or footprint, else its own.
    #[must_use]
    pub fn of(res: &Resolution) -> Self {
        let Some(model) = res.model.as_ref() else {
            return Self::default();
        };
        let tier = (res
            .provenance
            .as_ref()
            .map(hauksbee_ir::evidence::ModelSource::tier))
        .or_else(|| model.source.as_ref().map(|s| s.tier));
        let kind = if tier.is_some_and(|t| t <= ModelSourceTier::EstimatedFallback) {
            Some(StandInKind::Placeholder)
        } else if res.confidence == Confidence::Guessed {
            Some(StandInKind::Guessed)
        } else {
            None
        };
        Self {
            stand_in: kind.map(|kind| StandIn {
                card: model.id.clone(),
                kind,
            }),
            board_stated: Vec::new(),
            assumed: Vec::new(),
        }
    }

    /// Whether passing `limit` is a finding about the board: the limit is the
    /// part's own card's, or one the board states.
    #[must_use]
    pub fn judges(&self, limit: Limit) -> bool {
        self.stand_in_for(limit).is_none()
    }

    /// The stand-in card `limit` came from, when it is not the part's own.
    #[must_use]
    pub fn stand_in_for(&self, limit: Limit) -> Option<&StandIn> {
        if self.board_stated.contains(&limit) {
            return None;
        }
        // A limit named as an assumed floor says so, whatever the rest of the card is.
        (self.assumed.iter())
            .find(|(l, _)| *l == limit)
            .map(|(_, s)| s)
            .or(self.stand_in.as_ref())
    }

    /// Record that `limit` is an assumed floor, not the part's rating.
    pub fn assume(&mut self, limit: Limit, stand_in: StandIn) {
        if !self.assumed.iter().any(|(l, _)| *l == limit) {
            self.assumed.push((limit, stand_in));
        }
    }

    /// Record that the board itself states `limit`.
    pub fn stated_on_board(&mut self, limit: Limit) {
        if !self.board_stated.contains(&limit) {
            self.board_stated.push(limit);
        }
    }

    /// `value` when `limit` is judged, else `None`: for a check that abstains
    /// without a part-specific limit rather than stating a condition.
    #[must_use]
    pub fn judged<T>(&self, limit: Limit, value: Option<T>) -> Option<T> {
        value.filter(|_| self.judges(limit))
    }
}

/// What passing a limit says about the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// The part's own limit: a finding about the board.
    Fails,
    /// A stand-in limit: the stated condition, never a failing finding.
    Condition(String),
}

/// Judge `subject` (what was seen, e.g. "Q1 sees 30.00 V, over its 30.00 V
/// rating") against `limit` of `part`, whose `rating` (e.g. "30 V") came from
/// `basis`.
#[must_use]
pub fn judge(
    basis: &RatingBasis,
    limit: Limit,
    part: &str,
    subject: &str,
    rating: &str,
) -> Judgement {
    basis.stand_in_for(limit).map_or(Judgement::Fails, |s| {
        Judgement::Condition(condition(part, s, subject, rating))
    })
}

/// The words every surface reads a stand-in exceedance by; only
/// [`condition`] writes them.
pub const CONDITION_MARKER: &str = "so this is a condition, not a fault";

/// The stated condition for `subject` passing `part`'s `rating` from the
/// stand-in card `s`: what was seen, what it was judged against, and that the
/// part needs its own model.
#[must_use]
pub fn condition(part: &str, s: &StandIn, subject: &str, rating: &str) -> String {
    let adjective = s.adjective();
    let article = if adjective.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!(
        "{subject}: judged against {article} {adjective} {rating} rating ({}), not {part}'s own \
         datasheet, {CONDITION_MARKER}; {} to judge it",
        s.provenance(),
        s.remedy(part),
    )
}

/// The words the note for parts whose rated voltage nothing states carries
/// ([`StandInKind::Unstated`] past its assumed floor).
///
/// That is a condition the pass rests on, not a partial judgement: the part
/// is its own card, only the rating is unknown, so the verdict stays a pass
/// and names the parts among its conditions.
pub const UNSTATED_RATING_MARKER: &str = "rated voltage not stated";

/// Whether `message` is the unstated-rating note ([`UNSTATED_RATING_MARKER`]).
#[must_use]
pub fn is_unstated_rating(message: &str) -> bool {
    message.contains(UNSTATED_RATING_MARKER)
}

/// Whether `message` states a stand-in exceedance ([`condition`]).
#[must_use]
pub fn is_condition(message: &str) -> bool {
    message.contains(CONDITION_MARKER)
}

/// The words every surface reads an analysis that did not run by.
///
/// Only [`not_judged`] writes them. A failed or abstaining analysis judged
/// nothing, so the verdict built over it can never be a pass.
pub const NOT_JUDGED_MARKER: &str = "not judged, which is not a pass";

/// `what` (the analysis that did not run to a result, and the limits it
/// therefore did not judge), marked so the verdict reads it as partly judged.
#[must_use]
pub fn not_judged(what: &str) -> String {
    format!("{what}: {NOT_JUDGED_MARKER}")
}

/// Whether `message` says an analysis did not run to a result ([`not_judged`]).
#[must_use]
pub fn is_not_judged(message: &str) -> bool {
    message.contains(NOT_JUDGED_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ModelEntry, ModelSourceSpec};
    use crate::ComponentQuery;
    use hauksbee_ir::evidence::ModelValidation;

    fn resolution(tier: ModelSourceTier, confidence: Confidence) -> Resolution {
        Resolution {
            model: Some(ModelEntry {
                id: "card".into(),
                source: Some(ModelSourceSpec {
                    tier,
                    validation: ModelValidation::Unvalidated,
                    references: Vec::new(),
                    uncertainty: Vec::new(),
                    characteristic: Vec::new(),
                    pinout: Vec::new(),
                    doubt: Vec::new(),
                }),
                ..ModelEntry::default()
            }),
            confidence,
            ..Resolution::unresolved(ComponentQuery::default())
        }
    }

    #[test]
    fn a_placeholder_or_guessed_card_is_a_stand_in_and_the_own_card_is_judged() {
        let own = RatingBasis::of(&resolution(
            ModelSourceTier::CuratedLibrary,
            Confidence::Exact,
        ));
        assert!(own.judges(Limit::Voltage) && own.stand_in.is_none());
        let placeholder = RatingBasis::of(&resolution(
            ModelSourceTier::EstimatedFallback,
            Confidence::Exact,
        ));
        assert_eq!(
            placeholder.stand_in.as_ref().map(|s| s.kind),
            Some(StandInKind::Placeholder)
        );
        assert!(!placeholder.judges(Limit::Voltage));
        let guessed = RatingBasis::of(&resolution(
            ModelSourceTier::CuratedLibrary,
            Confidence::Guessed,
        ));
        assert_eq!(
            guessed.stand_in.as_ref().map(|s| s.kind),
            Some(StandInKind::Guessed)
        );
    }

    #[test]
    fn a_limit_the_board_states_stays_judged_on_a_stand_in_card() {
        let mut basis = RatingBasis::of(&resolution(
            ModelSourceTier::EstimatedFallback,
            Confidence::Guessed,
        ));
        basis.stated_on_board(Limit::Voltage);
        assert!(basis.judges(Limit::Voltage));
        assert!(!basis.judges(Limit::Current));
        assert_eq!(basis.judged(Limit::Current, Some(1.0)), None);
        assert_eq!(basis.judged(Limit::Voltage, Some(16.0)), Some(16.0));
    }

    #[test]
    fn a_stand_in_exceedance_is_a_stated_condition_and_an_own_one_fails() {
        let generic = RatingBasis::of(&resolution(
            ModelSourceTier::EstimatedFallback,
            Confidence::Exact,
        ));
        let seen = "Q1 sees 31.00 V, over its 30.00 V rating";
        let Judgement::Condition(text) = judge(&generic, Limit::Voltage, "Q1", seen, "30 V") else {
            panic!("a placeholder rating must not fail");
        };
        assert!(
            text.contains("judged against a generic 30 V rating") && text.contains("card 'card'")
        );
        assert!(text.contains("give Q1 its own model") && is_condition(&text));
        let own = RatingBasis::of(&resolution(
            ModelSourceTier::DatasheetDerived,
            Confidence::Exact,
        ));
        assert_eq!(
            judge(&own, Limit::Voltage, "Q1", seen, "30 V"),
            Judgement::Fails
        );
    }

    #[test]
    fn an_assumed_floor_is_a_condition_for_that_limit_only() {
        let mut basis = RatingBasis::of(&resolution(
            ModelSourceTier::CuratedLibrary,
            Confidence::Exact,
        ));
        basis.assume(
            Limit::Voltage,
            StandIn {
                card: "card".into(),
                kind: StandInKind::Unstated,
            },
        );
        // The card's other limits stay judged; the floored one does not.
        assert!(!basis.judges(Limit::Voltage));
        assert!(basis.judges(Limit::Current));
        let seen = "C1 sees 24.00 V, over its 6.30 V rating";
        let Judgement::Condition(text) = judge(&basis, Limit::Voltage, "C1", seen, "6.3 V") else {
            panic!("an assumed floor must not fail");
        };
        assert!(
            text.contains("judged against an assumed 6.3 V rating"),
            "{text}"
        );
        assert!(text.contains("state C1's rated voltage") && is_condition(&text));
        assert_eq!(
            judge(&basis, Limit::Current, "C1", "C1 sees 2 A", "1 A"),
            Judgement::Fails
        );
        // A rating the board then states replaces the floor.
        basis.stated_on_board(Limit::Voltage);
        assert!(basis.judges(Limit::Voltage));
    }
}
