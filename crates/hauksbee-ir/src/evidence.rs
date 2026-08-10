//! The evidence spine: what a result rests on, as data.
//!
//! Four types live here, and they exist because the tree already expressed the
//! same idea eleven different ways. An unresolved part defaulted to open, an
//! MCU modelled by a substitute core, a check that could not run on a gerber
//! archive, a bus the firmware never addressed, a waived finding: each one is
//! "this answer rests on something you should know about", and each one had its
//! own struct, its own wording, and its own renderer. Four vocabularies for one
//! idea is how a report ends up telling a user two different things about the
//! same gap.
//!
//! - [`Assumption`] is one thing the run took as true, or deliberately did not
//!   examine, carrying the same four sentences every existing mechanism already
//!   carried in some costume: what is assumed, why, what it does to results,
//!   and what would close it. The sentence FRAMES are composed here, by the
//!   constructors: `statement` and `consequence` entirely, `because` and
//!   `replacement` around a clause the producer supplies where the reason
//!   genuinely varies per input. Nothing can be re-worded afterwards, by a
//!   renderer or by anyone else, which is the property the later phases need.
//! - [`ArtifactProvenance`] and [`ParameterProvenance`] are provenance at the
//!   two granularities that matter: the file the run consumed, and the value a
//!   device carried into the solve.
//! - [`ErrorBudget`] is how good a number is: integration tolerance, which
//!   method produced which time window, the accepted residual, the windows
//!   with no valid solution at all.
//! - [`EvidenceMap`] ties one assertion to the artifacts, models, parameters
//!   and assumptions on its causal path, and derives an [`EvidenceStatus`]
//!   from them.
//!
//! The one invariant the rest of the design rests on: `EvidenceMap`'s status is
//! **derived, never set**. The field is private, there is one constructor, and
//! it computes the status from the on-path assumption kinds through
//! [`EvidenceMap::derive_status`]. `Undermined` maps onto the run's existing
//! third outcome (invalid for analysis, exit code 3), which waivers already
//! refuse to flip green, so an undermined conclusion cannot be waived into a
//! pass. If any code path could hand-set `Clean` over an undermined input set,
//! the whole honesty argument would be decoration.
//!
//! Be precise about what that buys, because the precise version is the useful
//! one: **a status cannot disagree with the set of assumptions it was handed, and
//! neither the set, the status, nor the assertion they belong to can be edited
//! afterwards.** Every field those three invariants touch is private behind a
//! getter, and neither judgement type deserializes. A status is only as
//! trustworthy as the kinds it came from, so a downstream
//! `a.kind = ReducedFidelity` would demote an undermined conclusion to a
//! gradeable one without touching [`EvidenceMap`] at all, and a mutable
//! `assertion` would let a clean map be relabelled onto an undermined assertion.
//!
//! Map construction requires the opaque output of [`CausalPathIndex::traverse`].
//! Outside this module there is no empty-slice constructor that can mint a clean
//! map over an unvisited board. The traversal rejects unknown nets, preserves
//! net/time scope, and resolves every cited assumption against one registry.
//! Artifact references and invariant-bearing numeric records are validated at
//! their own construction boundaries too.
//!
//! [`CausalPathIndex`] is the validated IR boundary for causal incidence. The
//! engine binder still owns production of that incidence and remains the exact
//! integration seam; the IR contract test rejects both vacuous and saturated
//! mappings before a production consumer is wired.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/evidence.md

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

mod date;
pub use date::{parse_ymd_epoch_days, RunDate, WaiverState};

/// A structural evidence error. These errors invalidate the producing result;
/// they are never repaired by dropping the offending field.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EvidenceError {
    /// A required numeric value is NaN or infinite.
    #[error("{field} must be finite, got {value}")]
    NonFinite { field: &'static str, value: f64 },
    /// A required tolerance or scale is zero or negative.
    #[error("{field} must be greater than zero, got {value}")]
    NonPositive { field: &'static str, value: f64 },
    /// A magnitude that may be zero is negative.
    #[error("{field} must not be negative, got {value}")]
    Negative { field: &'static str, value: f64 },
    /// A time interval ends before it starts.
    #[error("time window is inverted: [{start_s}, {end_s})")]
    InvertedWindow { start_s: f64, end_s: f64 },
    /// A reported window lies outside the result span it qualifies.
    #[error(
        "{kind} window [{start_s}, {end_s}) lies outside result [{result_start_s}, {result_end_s})"
    )]
    WindowOutsideResult {
        kind: &'static str,
        start_s: f64,
        end_s: f64,
        result_start_s: f64,
        result_end_s: f64,
    },
    /// Two windows that cannot both describe the same span overlap.
    #[error("{first_kind} window overlaps {second_kind} window")]
    OverlappingWindows {
        first_kind: &'static str,
        second_kind: &'static str,
    },
    /// A method name crossed a crate boundary without a typed mapping.
    #[error("unknown integration method {method:?}")]
    UnknownIntegrationMethod { method: String },
    /// An uncertainty interval has its bounds reversed.
    #[error("uncertainty interval for {parameter} is inverted: [{low}, {high}]")]
    InvertedInterval {
        parameter: String,
        low: f64,
        high: f64,
    },
    /// A required identifier or collection is empty.
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    /// An assumption registry contains the same stable id twice.
    #[error("duplicate assumption id {id}")]
    DuplicateAssumption { id: String },
    /// An artifact cites an assumption absent from the run registry.
    #[error("artifact cites assumption {id} absent from the evidence registry")]
    MissingAssumption { id: String },
    /// An evidence map cites an artifact absent from the run inventory.
    #[error("evidence map cites artifact index {index}, but inventory length is {len}")]
    MissingArtifact { index: usize, len: usize },
    /// A supplied SHA-256 is not an empty directory marker or 64 hex digits.
    #[error("artifact sha256 must be empty or 64 hexadecimal digits")]
    InvalidSha256,
    /// An assertion named a net absent from the validated incidence index.
    #[error("causal traversal has no incidence record for net {net}")]
    UnknownNet { net: String },
    /// A constructor-produced assumption failed its structural validation.
    #[error("invalid assumption: {message}")]
    InvalidAssumption { message: String },
    /// A waiver expiry is not a real `YYYY-MM-DD` calendar date.
    #[error("{field} must be a real date as YYYY-MM-DD, got {value:?}")]
    InvalidDate { field: &'static str, value: String },
}

// ─────────────────────────────────────────────────────────────────────────────
// Assumptions
// ─────────────────────────────────────────────────────────────────────────────

/// Stable identity for one assumption within a run. Deterministic:
/// `"{kind_slug}:{subject}"`, e.g. `"open-part:R7"`,
/// `"not-exercised:i2c0/U4"`, `"not-checked:drc"`,
/// `"waived:si/controlled_impedance/DDR_CLK"`. Re-running the same board yields
/// the same ids, so an acknowledgment file can name one, a diff can track one
/// across runs, and a renderer can cross-reference one.
///
/// The subject keeps its own case (a reference designator is `R7`, not `r7`)
/// and reserved bytes are percent-encoded, so distinct subjects never collapse
/// while the kind slug remains everything before the first `:`.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct AssumptionId(String);

impl AssumptionId {
    /// The subject an id falls back to when the producer had none. A board can
    /// carry an unnamed net or a footprint with a blank designator, and a gap on
    /// one of those is still a gap worth reporting: an id naming
    /// [`Self::UNNAMED_SUBJECT`] says so, where an id naming nothing at all would
    /// be unciteable.
    pub const UNNAMED_SUBJECT: &'static str = "unnamed";

    /// Compose an id from a kind and a subject. Reserved bytes are encoded
    /// injectively; an empty subject becomes [`Self::UNNAMED_SUBJECT`].
    pub fn new(kind: AssumptionKind, subject: &str) -> Self {
        Self::disambiguated(kind, subject, "")
    }

    /// An id for a subject the producer could not name, disambiguated by the
    /// assumption's own composed statement.
    ///
    /// Two footprints with blank designators are two gaps, and giving them one id
    /// would be worse than giving them an ugly one: the evidence map dedupes by
    /// id, so the second gap would vanish from the report rather than appear
    /// twice. The statement is the only thing that distinguishes them, so its
    /// complete bytes become the disambiguator. It stays deterministic across
    /// runs and avoids a truncated-hash collision.
    fn disambiguated(kind: AssumptionKind, subject: &str, statement: &str) -> Self {
        let subject = if subject.trim().is_empty() {
            format!(
                "{}-{}",
                Self::UNNAMED_SUBJECT,
                hex_bytes(statement.as_bytes())
            )
        } else {
            subject.trim().to_string()
        };
        Self(format!("{}:{}", kind.slug(), escape_id_component(&subject)))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn escape_id_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

impl std::fmt::Display for AssumptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What kind of gap an [`Assumption`] records. The kind, not the wording, is
/// what [`EvidenceMap::derive_status`] reads, so adding a variant is a policy
/// decision about what undermines a conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
// Iterated only by the test that proves every kind has a constructor and a row
// in the status table, so it stays out of the published API.
#[cfg_attr(test, derive(strum::EnumIter))]
#[serde(rename_all = "snake_case")]
pub enum AssumptionKind {
    /// An unresolved part defaulted to an open circuit.
    OpenPart,
    /// A model stood in for the real part: an engine fallback entry, or a
    /// substitute MCU core.
    SubstituteModel,
    /// A pin's role was inferred from the pin-rule table, not read from the
    /// schematic.
    InferredPinRole,
    /// A parameter took a documented default because the source carried no
    /// value.
    DefaultParameter,
    /// Every placed part was treated as fitted because the input carries no
    /// BOM or populate flag.
    FittedByDefault,
    /// A whole check could not run on this input class and reported
    /// "not checked" instead of a vacuous green.
    NotChecked,
    /// The run happened but never exercised this element: a bus device never
    /// addressed, an ADC channel whose injections the backend dropped,
    /// firmware that never executed.
    NotExercised,
    /// The result was produced at reduced fidelity by a documented degraded
    /// mechanism: heuristic bus framing, a strapless MCU model, a net held by
    /// an ideal source.
    ReducedFidelity,
    /// The producing parser has a known limitation that can fabricate or miss
    /// findings here.
    ParserLimitation,
    /// A human overrode a finding, with a reason and an expiry. The gating
    /// machinery stays in the waiver code; this is the surfaced record.
    Waived,
}

impl AssumptionKind {
    /// The wire form, identical to what serde writes (`snake_case`). The one
    /// place this vocabulary is spelled.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenPart => "open_part",
            Self::SubstituteModel => "substitute_model",
            Self::InferredPinRole => "inferred_pin_role",
            Self::DefaultParameter => "default_parameter",
            Self::FittedByDefault => "fitted_by_default",
            Self::NotChecked => "not_checked",
            Self::NotExercised => "not_exercised",
            Self::ReducedFidelity => "reduced_fidelity",
            Self::ParserLimitation => "parser_limitation",
            Self::Waived => "waived",
        }
    }

    /// The id slug for this kind, `kebab-case`. Stable: ids are compared across
    /// runs and named in acknowledgment files.
    pub fn slug(self) -> &'static str {
        match self {
            Self::OpenPart => "open-part",
            Self::SubstituteModel => "substitute-model",
            Self::InferredPinRole => "inferred-pin-role",
            Self::DefaultParameter => "default-parameter",
            Self::FittedByDefault => "fitted-by-default",
            Self::NotChecked => "not-checked",
            Self::NotExercised => "not-exercised",
            Self::ReducedFidelity => "reduced-fidelity",
            Self::ParserLimitation => "parser-limitation",
            Self::Waived => "waived",
        }
    }
}

/// Which stage of the run raised the assumption. Mirrors the pipeline, so a
/// reader can tell an input-format limitation from a binder guess from a co-sim
/// coverage gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssumptionSource {
    /// An input reader (extract).
    Reader,
    /// The binder, turning connectivity plus models into a circuit.
    Binder,
    /// The co-sim scheduler.
    Scheduler,
    /// The analog solver.
    Solver,
    /// A check (DRC, SI, lint, ampacity, coverage).
    Check,
    /// A human, through a waiver or an override.
    User,
}

/// What results the assumption taints. Scope is the input to the on-path
/// decision: the causal-path traversal compares an assertion's subject nets and
/// refs against these, and only assumptions that match land in an
/// [`EvidenceMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// A placed component or footprint.
    Part,
    /// One pin on a placed component.
    Pin,
    /// A logical bus or peripheral.
    Bus,
    /// An input artifact.
    Artifact,
    /// A board-level object that is not one of the narrower kinds.
    BoardObject,
}

/// A typed, non-empty causal subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
pub struct EntityRef {
    kind: EntityKind,
    id: String,
}

impl EntityRef {
    /// Construct a named subject.
    pub fn new(kind: EntityKind, id: impl Into<String>) -> Result<Self, EvidenceError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "entity_ref.id",
            });
        }
        Ok(Self { kind, id })
    }

    /// The subject category.
    pub fn kind(&self) -> EntityKind {
        self.kind
    }

    /// The stable subject id.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// A non-empty, deduplicated subject set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SubjectSet(Vec<EntityRef>);

impl SubjectSet {
    /// Construct a set in deterministic first-seen order.
    pub fn new<I>(subjects: I) -> Result<Self, EvidenceError>
    where
        I: IntoIterator<Item = EntityRef>,
    {
        let mut out = Vec::new();
        for subject in subjects {
            if !out.contains(&subject) {
                out.push(subject);
            }
        }
        if out.is_empty() {
            return Err(EvidenceError::Empty {
                field: "subject_set",
            });
        }
        Ok(Self(out))
    }

    /// The subjects in deterministic first-seen order.
    pub fn as_slice(&self) -> &[EntityRef] {
        &self.0
    }
}

/// One model or input parameter on a specific causal subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ParameterRef {
    subject: EntityRef,
    parameter: String,
}

impl ParameterRef {
    /// Construct a subject-qualified parameter.
    pub fn new(subject: EntityRef, parameter: impl Into<String>) -> Result<Self, EvidenceError> {
        let parameter = parameter.into();
        if parameter.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "parameter_ref.parameter",
            });
        }
        Ok(Self { subject, parameter })
    }

    /// The subject carrying the parameter.
    pub fn subject(&self) -> &EntityRef {
        &self.subject
    }

    /// The parameter's stable name.
    pub fn parameter(&self) -> &str {
        &self.parameter
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Scope {
    /// The whole run, e.g. every placed part treated as fitted because there
    /// was no BOM at all.
    Board,
    /// Specific typed subjects.
    Subjects(SubjectSet),
    /// One subject-qualified parameter.
    Parameter(ParameterRef),
    /// Specific nets, optionally restricted to an observation window.
    Nets(NetScope),
    /// One named check ("drc", "si", "lint"), for `NotChecked` and `Waived`.
    Check {
        /// The check name.
        check: String,
        /// The specific rule inside the check, when the scope is that narrow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        kind: Option<String>,
    },
}

/// One thing the run took as true, or deliberately did not examine, without
/// evidence for it.
///
/// The four sentence fields are the point. Every mechanism this type absorbs
/// already carried them in some costume, and they are composed by the
/// constructors on this type so that two surfaces cannot describe the same gap
/// differently.
///
/// Every field is private with a getter, and the type does NOT implement
/// `Deserialize`. Both halves are needed. Public fields would have left the
/// wording rewritable in one line from any crate, and `kind` is worse than the
/// wording: it is the input to [`EvidenceMap::derive_status`], so a downstream
/// `a.kind = ReducedFidelity` would demote an undermined conclusion to a
/// gradeable one without ever touching [`EvidenceMap`]. A derived `Deserialize`
/// would then hand the same power back through eight lines of JSON, minting an
/// assumption with any kind and any wording outside the constructors. An
/// assumption is produced, not parsed, which is also how the run report's own
/// types are shaped (they are serialize-only), so this costs nothing real.
/// Read access is all a renderer needs.
///
/// ```
/// use hauksbee_ir::evidence::{Assumption, AssumptionKind};
///
/// let a = Assumption::open_part("R7", "10k", "no model matched");
/// assert_eq!(a.id().as_str(), "open-part:R7");
/// assert_eq!(a.kind(), AssumptionKind::OpenPart);
/// assert!(a.statement().starts_with("R7 (10k) is treated as an open circuit"));
/// assert!(a.expires().is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[schemars(
    description = "One thing the run took as true, or deliberately did not \
                          examine, without evidence for it, with the four sentences \
                          that say what, why, what it costs, and what would close it."
)]
pub struct Assumption {
    /// Stable id, `"{kind_slug}:{subject}"`.
    id: AssumptionId,
    /// What kind of gap this is. Drives [`EvidenceStatus`].
    kind: AssumptionKind,
    /// Which pipeline stage raised it.
    source: AssumptionSource,
    /// What it taints.
    scope: Scope,
    /// What is being taken as true, one sentence, present tense.
    statement: String,
    /// Why the run had to assume it, one sentence.
    because: String,
    /// What it does to results, one sentence.
    consequence: String,
    /// The replacement path: what a user does so this stops being an
    /// assumption. Always actionable, never "n/a".
    replacement: String,
    /// Waived assumptions only: the `YYYY-MM-DD` the waiver lapses. `None` for
    /// every run-derived kind, since an expiry on a machine observation would
    /// mean nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // The schema must not permit `null`: this field is either an absent key or a
    // date string, and a published schema that allows a third encoding is a
    // schema the module's own writers would fail to satisfy. Narrowing it after
    // publication breaks every validating consumer, so it is pinned now.
    #[schemars(with = "String")]
    expires: Option<String>,
}

/// Trim a caller-supplied data fragment so the constructors, not the caller,
/// own sentence punctuation. Callers pass fragments ("no model matched"), never
/// finished sentences.
fn fragment(s: &str) -> String {
    // Internal whitespace runs collapse too. A reason string lifted out of a
    // real file arrives with whatever spacing the file had, and a double space
    // is not a reason to refuse a sentence, only to tidy it.
    s.trim()
        .trim_end_matches(['.', ';', ',', ':', '!', '?'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Promote a caller-supplied fragment to a sentence: one full stop, and a
/// capital where a capital belongs. The whole point of composing sentences in
/// one crate is that a producer cannot half-do it somewhere else.
///
/// The leading word is capitalized only when it is prose. Net, bus, signal and
/// property names are case-sensitive, and "spi0" or "value_unresolved" rendered
/// as "Spi0" or "Value_unresolved" would be a different identifier: the module
/// exists to stop one gap being described two ways, so it must not spell the
/// same subject two ways itself.
fn sentence(s: &str) -> String {
    let f = fragment(s);
    let Some(first_word) = f.split_whitespace().next() else {
        return String::new();
    };
    let prose = first_word
        .chars()
        .all(|c| c.is_ascii_lowercase() || c == ',' || c == '\'');
    let mut chars = f.chars();
    let first = chars.next().expect("a non-empty fragment has a first char");
    if prose {
        format!("{}{}.", first.to_uppercase(), chars.as_str())
    } else {
        format!("{f}.")
    }
}

/// A sentence from a caller's fragment, falling back to a composed one when the
/// producer had nothing to say.
///
/// Producers pass data through: a reason lifted from a file property, a bus name
/// from a descriptor. Real data is sometimes absent, and the two bad answers to
/// that are a sentence with a hole in it (release) and a panic inside a producer
/// for a data reason (debug). The fallback is the third answer: say the general
/// truth of the kind, which is always still true.
fn sentence_or(s: &str, fallback: &str) -> String {
    sentence(&or_else(s, fallback))
}

fn part_scope(reference: &str) -> Scope {
    let reference = or_else(reference, AssumptionId::UNNAMED_SUBJECT);
    Scope::Subjects(SubjectSet(vec![EntityRef {
        kind: EntityKind::Part,
        id: reference,
    }]))
}

fn parameter_scope(reference: &str, parameter: &str) -> Scope {
    let reference = or_else(reference, AssumptionId::UNNAMED_SUBJECT);
    let parameter = or_else(parameter, AssumptionId::UNNAMED_SUBJECT);
    Scope::Parameter(ParameterRef {
        subject: EntityRef {
            kind: EntityKind::Part,
            id: reference,
        },
        parameter,
    })
}

fn net_scope(net: &str) -> Scope {
    let net = or_else(net, AssumptionId::UNNAMED_SUBJECT);
    Scope::Nets(NetScope {
        nets: vec![net],
        window: None,
    })
}

/// A tidied fragment, or the fallback when the producer had nothing. Same
/// reasoning as [`sentence_or`], for a datum that sits mid-sentence.
fn or_else(s: &str, fallback: &str) -> String {
    let f = fragment(s);
    if f.is_empty() {
        fallback.to_string()
    } else {
        f
    }
}

/// What an assumption is about, in the two forms an assumption needs it: the
/// `key` that goes into the id (short, stable, slug-shaped, because ids are
/// named in acknowledgment files and compared across runs) and the `text` that
/// goes into the sentences (prose, because a reader is reading English).
///
/// They are separate parameters because collapsing them produces either an
/// unreadable sentence ("Odbpp carries no BOM") or an unusable id
/// (`fitted-by-default:the_ODB++_archive`). [`Subject::same`] is for the cases
/// where one string honestly serves both, such as a bus name.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    /// The id subject: short and stable, e.g. "odbpp", "i2c0/U4", "drc/short".
    pub key: &'a str,
    /// The prose form, e.g. "the ODB++ archive", "the i2c0 bus".
    pub text: &'a str,
}

impl<'a> Subject<'a> {
    /// Distinct id key and prose.
    pub fn new(key: &'a str, text: &'a str) -> Self {
        Self { key, text }
    }

    /// One string serving as both, for subjects that already read as prose.
    pub fn same(both: &'a str) -> Self {
        Self {
            key: both,
            text: both,
        }
    }

    /// The prose form, falling back to the key. A producer that has an id but no
    /// prose still has a real gap to report, and the key names it.
    fn text_or_key(&self) -> String {
        or_else(self.text, &or_else(self.key, AssumptionId::UNNAMED_SUBJECT))
    }
}

impl Assumption {
    /// The stable id.
    pub fn id(&self) -> &AssumptionId {
        &self.id
    }

    /// What kind of gap this is. Read-only: it is the input to the status rule.
    pub fn kind(&self) -> AssumptionKind {
        self.kind
    }

    /// Which pipeline stage raised it.
    pub fn source(&self) -> AssumptionSource {
        self.source
    }

    /// What it taints, which is what the causal-path traversal matches against.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// What is being taken as true.
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// Why the run had to assume it.
    pub fn because(&self) -> &str {
        &self.because
    }

    /// What it does to results.
    pub fn consequence(&self) -> &str {
        &self.consequence
    }

    /// What a user does so this stops being an assumption.
    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    /// The `YYYY-MM-DD` a waiver lapses, for `Waived` only.
    pub fn expires(&self) -> Option<&str> {
        self.expires.as_deref()
    }

    /// Raw constructor, private on purpose: every public constructor below goes
    /// through it, so there is exactly one place a sentence set is assembled.
    #[allow(clippy::too_many_arguments)]
    fn build(
        kind: AssumptionKind,
        source: AssumptionSource,
        subject: &str,
        scope: Scope,
        statement: String,
        because: String,
        consequence: String,
        replacement: String,
        expires: Option<String>,
    ) -> Self {
        // Every sentence passes through `sentence` here, so capitalization and
        // the single full stop are guaranteed once rather than at ten call
        // sites, even where a composed sentence opens with a caller-supplied
        // fragment.
        let statement = sentence(&statement);
        let built = Self {
            // The statement is composed FIRST because it is what disambiguates
            // an id whose subject the producer could not name.
            id: AssumptionId::disambiguated(kind, subject, &statement),
            kind,
            source,
            scope,
            statement,
            because: sentence(&because),
            consequence: sentence(&consequence),
            replacement: sentence(&replacement),
            expires,
        };
        // Every constructor's output must satisfy the rules `validate` states,
        // and a debug build is where a producer that hands over an empty datum
        // finds out. Left as a debug assert rather than a `Result` because
        // ninety call sites returning `Result` for a bug in their own arguments
        // buys nothing a test does not.
        debug_assert!(
            built.validate().is_ok(),
            "{}",
            built.validate().unwrap_err()
        );
        built
    }

    /// Separates the two halves of a named abstention inside the binder's single
    /// `reason` string: what blocks the model, and what would unlock it.
    ///
    /// Written by the binder when `db/unmodelled.toml` matches a part, read by
    /// [`Assumption::open_part`] below, and defined here so the two cannot drift.
    /// A reason that does not contain it is an ordinary one and is passed through
    /// whole.
    pub const UNLOCKED_BY_MARKER: &'static str = " Unlocked by: ";

    /// An unresolved part defaulted to an open circuit. `reason` is the
    /// binder's or extractor's data fragment for why nothing bound (for
    /// Altium boards, the `value_unresolved` property lands here).
    ///
    /// ```
    /// use hauksbee_ir::evidence::Assumption;
    /// let a = Assumption::open_part("U2", "XC6206", "no model matched");
    /// assert!(a.replacement().contains("U2"));
    /// ```
    pub fn open_part(reference: &str, value: &str, reason: &str) -> Self {
        // A NAMED ABSTENTION arrives as one string carrying both halves, joined by
        // [`UNLOCKED_BY_MARKER`]: the binder has one `reason` channel to the report
        // and the two halves belong in two different fields here, so the split
        // happens at the point of use rather than by widening every BindRow
        // constructor in the tree. The marker is a shared const precisely so the
        // producer and this consumer cannot drift.
        //
        // Everything else, including every generic reason and every extractor
        // explanation, contains no marker and takes the `None` arm untouched.
        let (reason, unlocked_by) = match reason.split_once(Self::UNLOCKED_BY_MARKER) {
            Some((because, unlock)) => (because.trim(), Some(unlock.trim())),
            None => (reason, None),
        };
        // A board can carry a footprint with a blank designator, and a gap on one
        // is still a gap. So the SENTENCES get prose ("an unnamed part") while
        // the ID keeps the raw subject and lets `AssumptionId` disambiguate:
        // prose in an id would collide two blank-designator parts onto one
        // entry, and the evidence map's dedupe would then drop one of them.
        let subject = fragment(reference);
        let reference = or_else(reference, "an unnamed part");
        let value = fragment(value);
        let named = if value.is_empty() {
            reference.to_string()
        } else {
            format!("{reference} ({value})")
        };
        // The producer's own reason IS the "why" here (for Altium boards it is
        // the extractor's `value_unresolved` property, carried through the
        // binder), so it becomes the sentence rather than being wrapped in a
        // frame that would repeat it.
        let because = if fragment(reason).is_empty() {
            "No model in the library matched this part.".to_string()
        } else {
            sentence(reason)
        };
        Self::build(
            AssumptionKind::OpenPart,
            AssumptionSource::Binder,
            &subject,
            part_scope(&subject),
            format!("{named} is treated as an open circuit."),
            because,
            format!(
                "Nets through {reference} are isolated in simulation, so any current path \
                 across it is missing from every result that depends on it."
            ),
            // The "what to do" is the generic route ONLY when nobody has looked at
            // this part. When the library has looked and named the blocker, that
            // named input IS the next step, and printing "add a model to your models
            // directory" beside it would be telling the reader to do the thing the
            // sentence above just explained is not yet possible.
            match unlocked_by {
                Some(unlock) => unlock.to_string(),
                None => format!(
                    "Add a model for {reference} to your models directory, or mark it DNP if \
                     the board does not fit it."
                ),
            },
            None,
        )
    }

    /// A model stood in for the real part: an engine fallback entry
    /// (`source: Binder`) or a substitute MCU core (`source: Scheduler`).
    /// `requested` is what the board asked for, `stand_in` what actually
    /// modelled it.
    pub fn substitute_model(
        source: AssumptionSource,
        reference: &str,
        requested: &str,
        stand_in: &str,
    ) -> Self {
        // A producer with no name for one side still has a real gap to report,
        // so the sentence names what it can rather than leaving a hole. The id
        // keeps the raw subject: see `open_part`.
        let subject = fragment(reference);
        let reference = or_else(reference, "an unnamed part");
        let requested = or_else(requested, "the part the board asks for");
        let stand_in = or_else(stand_in, "a stand-in model");
        Self::build(
            AssumptionKind::SubstituteModel,
            source,
            &subject,
            part_scope(&subject),
            format!("{reference} is modelled by {stand_in}, not {requested}."),
            format!(
                "No model matched {requested}, and {stand_in} was the closest stand-in \
                 available to this run."
            ),
            format!(
                "Every result touching {reference} describes {stand_in}'s behaviour; wherever \
                 the two parts differ, this run is describing the wrong part."
            ),
            format!(
                "Add a model for {requested} (a models directory entry or a model pack) so the \
                 run binds the part the board actually carries."
            ),
            None,
        )
    }

    /// A pin's role was inferred from the pin-rule table rather than read from
    /// the schematic.
    pub fn inferred_pin_role(reference: &str, pin: &str, role: &str) -> Self {
        let subject = if reference.trim().is_empty() && pin.trim().is_empty() {
            String::new()
        } else {
            format!("{}/{}", fragment(reference), fragment(pin))
        };
        let part_ref = fragment(reference);
        let reference = or_else(reference, "an unnamed part");
        let pin = or_else(pin, "an unnamed pin");
        let role = or_else(role, "inferred");
        Self::build(
            AssumptionKind::InferredPinRole,
            AssumptionSource::Binder,
            &subject,
            part_scope(&part_ref),
            format!("{reference} pin {pin} is taken to be its {role} pin."),
            "The inputs named no role for that pin, so the pin-rule table inferred one from \
             the part's shape."
                .to_string(),
            format!(
                "If the inference is wrong, {reference} is wired into the simulation \
                 differently from the board, and its results describe a different circuit."
            ),
            format!(
                "Name the pin roles for {reference} in its model (or give the run a schematic \
                 that names them) so nothing has to be inferred."
            ),
            None,
        )
    }

    /// A parameter took a documented default because no input carried a value.
    pub fn default_parameter(reference: &str, parameter: &str, value: &str) -> Self {
        let subject = if reference.trim().is_empty() && parameter.trim().is_empty() {
            String::new()
        } else {
            format!("{}.{}", fragment(reference), fragment(parameter))
        };
        let part_ref = fragment(reference);
        let reference = or_else(reference, "an unnamed part");
        let parameter = or_else(parameter, "a parameter");
        let value = or_else(value, "the documented default");
        Self::build(
            AssumptionKind::DefaultParameter,
            AssumptionSource::Binder,
            &subject,
            parameter_scope(&part_ref, &parameter),
            format!("{reference}.{parameter} is taken as {value}, a documented default."),
            "No input carried a value for it.".to_string(),
            format!(
                "Any result that depends on {reference}.{parameter} moves with that default \
                 rather than with the real part."
            ),
            format!(
                "Set {parameter} in {reference}'s model, or supply it in your spec, so the run \
                 reads a real value."
            ),
            None,
        )
    }

    /// Every placed part was treated as fitted, because the input carries no
    /// BOM or populate flag. `input` names the artifact that could not say: its
    /// key is the artifact kind or path, its text the prose name.
    ///
    /// `scope` is the caller's, and the choice is consequential. This kind
    /// undermines, so a `Scope::Board` entry is on the causal path of every
    /// assertion and turns a whole BOM-less run invalid for analysis. A reader
    /// that knows which placed parts are actually in question should scope it to
    /// `Scope::Parts` instead, which is the reading the status table's
    /// "FittedByDefault (subject part)" row asks for.
    pub fn fitted_by_default(source: AssumptionSource, input: Subject<'_>, scope: Scope) -> Self {
        let input_key = input.key;
        let input = or_else(&input.text_or_key(), "this input");
        Self::build(
            AssumptionKind::FittedByDefault,
            source,
            input_key,
            scope,
            "Every placed part is treated as fitted.".to_string(),
            format!(
                "{input} carries no BOM or populate flag, so nothing in the inputs \
                 distinguishes a fitted part from a DNP one."
            ),
            "Parts the board does not actually fit are simulated as present, loading their \
             nets and carrying current that is not there."
                .to_string(),
            "Supply a BOM (or an as-built overlay) marking the parts that are not fitted."
                .to_string(),
            None,
        )
    }

    /// A whole check could not run on this input class. `because` and
    /// `replacement` are data fragments, because what is missing and what would
    /// supply it differ per check and per input class; the statement and the
    /// consequence, which carry the honesty, are fixed here.
    pub fn not_checked(
        source: AssumptionSource,
        check: &str,
        rule: Option<&str>,
        because: &str,
        replacement: &str,
    ) -> Self {
        let rule = rule.map(fragment).filter(|r| !r.is_empty());
        // A check that could not run one of its rules says nothing about that
        // rule, not about the whole check, and the scope has to say which or the
        // traversal will put it on the path of every assertion that touches the
        // check.
        //
        // The ID keeps the raw check name, empty and all, so that two nameless
        // ones are disambiguated by their statements rather than colliding on a
        // sentinel and losing one to the evidence map's dedupe. The PROSE gets
        // the sentinel.
        let raw = fragment(check);
        let check = or_else(check, AssumptionId::UNNAMED_SUBJECT);
        let (subject, named) = match &rule {
            Some(r) => (format!("{raw}/{r}"), format!("{check} {r}")),
            None => (raw.clone(), check.clone()),
        };
        let subject = if raw.is_empty() && rule.is_none() {
            String::new()
        } else {
            subject
        };
        Self::build(
            AssumptionKind::NotChecked,
            source,
            &subject,
            Scope::Check {
                check: check.clone(),
                kind: rule,
            },
            format!("The {named} check did not run."),
            sentence_or(
                because,
                "the inputs this run was given do not carry what the check reads",
            ),
            format!(
                "This run says nothing about {named}: no {named} findings here is not evidence \
                 that there are none."
            ),
            sentence_or(
                replacement,
                "supply an input this check can read, then re-run",
            ),
            None,
        )
    }

    /// The run happened but never exercised `subject` (a bus, an ADC channel,
    /// firmware, a programmed current limit), so nothing here tests it.
    /// `because` and `replacement` are data fragments; this constructor owns
    /// the statement and the consequence.
    pub fn not_exercised(
        source: AssumptionSource,
        subject: Subject<'_>,
        scope: Scope,
        because: &str,
        replacement: &str,
    ) -> Self {
        let subject_key = subject.key;
        let subject_text = subject.text_or_key();
        Self::build(
            AssumptionKind::NotExercised,
            source,
            subject_key,
            scope,
            format!("This run does not cover {subject_text}."),
            sentence_or(because, "this run's stimulus never reached it"),
            format!(
                "Any conclusion that assumes {subject_text} was exercised is unsupported by \
                 this run: what it reports there is a default, not a measurement."
            ),
            sentence_or(
                replacement,
                "exercise it, from firmware or from a spec stimulus, and re-run",
            ),
            None,
        )
    }

    /// The result was produced at reduced fidelity by a documented degraded
    /// mechanism. `mechanism` names the degraded path.
    pub fn reduced_fidelity(
        source: AssumptionSource,
        subject: Subject<'_>,
        scope: Scope,
        mechanism: &str,
        replacement: &str,
    ) -> Self {
        let subject_key = subject.key;
        let subject_text = subject.text_or_key();
        let mechanism = or_else(mechanism, "a documented degraded path");
        Self::build(
            AssumptionKind::ReducedFidelity,
            source,
            subject_key,
            scope,
            format!("{subject_text} was produced at reduced fidelity."),
            format!("It came from {mechanism}, not from the primary path."),
            format!(
                "Numbers for {subject_text} are indicative: the mechanism that produced them \
                 is documented as less accurate than the primary path."
            ),
            sentence_or(
                replacement,
                "give the run what the primary path needs, so this stops being the fallback",
            ),
            None,
        )
    }

    /// A net held by an ideal source: the check cannot fail there for a board
    /// reason, so a pass on it vouches for nothing. A named `ReducedFidelity`
    /// constructor because this wording is load-bearing and appears on several
    /// surfaces.
    pub fn held_by_ideal_source(net: &str) -> Self {
        // An unnamed net is ordinary board data, not a producer bug. Prose in the
        // sentences, raw subject in the id: see `open_part`.
        let subject = fragment(net);
        let net = or_else(net, "an unnamed net");
        Self::build(
            AssumptionKind::ReducedFidelity,
            AssumptionSource::Check,
            &subject,
            net_scope(&subject),
            format!("Net {net} is held by an ideal source."),
            "Nothing on the board sets its voltage in this run: a stimulus does.".to_string(),
            format!(
                "A rail check on {net} cannot fail for a board reason, so passing it vouches \
                 for nothing about the board."
            ),
            format!(
                "Model the part that actually drives {net} (the regulator or the supply path) \
                 and re-run, so the check reads the board instead of the stimulus."
            ),
            None,
        )
    }

    /// The producing parser has a known limitation that can fabricate or miss
    /// findings about `subject`.
    pub fn parser_limitation(
        source: AssumptionSource,
        subject: Subject<'_>,
        scope: Scope,
        limitation: &str,
        replacement: &str,
    ) -> Self {
        let subject_key = subject.key;
        let subject_text = subject.text_or_key();
        Self::build(
            AssumptionKind::ParserLimitation,
            source,
            subject_key,
            scope,
            format!("Findings about {subject_text} may be wrong."),
            sentence_or(
                limitation,
                "the reader that produced them has a documented limitation here",
            ),
            format!(
                "Findings on {subject_text} can be fabricated or missed here, so treat them as \
                 unconfirmed rather than as results."
            ),
            sentence_or(
                replacement,
                "re-export the input from a version this reader models, then re-run",
            ),
            None,
        )
    }

    /// The surfaced record of an applied waiver. The gating machinery stays in
    /// the waiver code; this is what a reader sees. `until` is the waiver's
    /// `YYYY-MM-DD` expiry and lands in [`Assumption::expires`], which is what
    /// lets [`EvidenceMap::derive_status`] treat a lapsed waiver as undermining
    /// rather than qualifying.
    pub fn waived(
        check: &str,
        kind: &str,
        subject: &str,
        reason: &str,
        until: &str,
        today: RunDate,
    ) -> Result<Self, EvidenceError> {
        // The check name and rule come from the waiver file's own required
        // fields, and the subject from its nets or refs; a producer that lost one
        // on the way here still has a waiver to surface.
        // Prose in the sentences, raw fields in the id: see `open_part`.
        let id_subject = if [check, kind, subject].iter().all(|s| s.trim().is_empty()) {
            String::new()
        } else {
            format!(
                "{}/{}/{}",
                fragment(check),
                fragment(kind),
                fragment(subject)
            )
        };
        let check = or_else(check, "an unnamed check");
        let kind = or_else(kind, "an unnamed rule");
        let subject_text = or_else(subject, "an unnamed subject");
        let until = fragment(until);
        let expiry = parse_ymd_epoch_days(&until).ok_or_else(|| EvidenceError::InvalidDate {
            field: "waiver.until",
            value: until.clone(),
        })?;
        let (consequence, replacement) = if today.is_covered_by(expiry) {
            (
                format!(
                    "It does not gate this run until {until}, so this result is clean by \
                     authorization rather than by measurement."
                ),
                format!(
                    "Fix the finding, or let the waiver lapse on {until}, after which it gates \
                     again."
                ),
            )
        } else {
            (
                format!(
                    "This waiver has lapsed: it stopped covering the finding after {until}, so \
                     the finding gates this run again."
                ),
                "Fix the finding, or add a new narrowly scoped waiver with a fresh reason and \
                 expiry."
                    .to_string(),
            )
        };
        Ok(Self::build(
            AssumptionKind::Waived,
            AssumptionSource::User,
            &id_subject,
            Scope::Check {
                check: check.clone(),
                kind: Some(kind.clone()),
            },
            format!("The {check} {kind} finding on {subject_text} is waived by hand."),
            // Same reasoning for the reason: the file requires one, so an empty
            // one is worth saying rather than leaving blank.
            sentence_or(
                reason,
                "no reason reached this report, though the waiver file requires one",
            ),
            consequence,
            replacement,
            Some(until),
        ))
    }

    /// A waiver applied to one production assertion. The wording and lifecycle
    /// are identical to [`Self::waived`], while the causal scope uses the
    /// assertion's stable label so another result of the same kind does not
    /// inherit this authorization.
    pub fn waived_assertion(
        check: &str,
        kind: &str,
        assertion: &str,
        subject: &str,
        reason: &str,
        until: &str,
        today: RunDate,
    ) -> Result<Self, EvidenceError> {
        let mut assumption = Self::waived(check, kind, subject, reason, until, today)?;
        assumption.scope = Scope::Check {
            check: or_else(check, "an unnamed check"),
            kind: Some(or_else(assertion, "an unnamed assertion")),
        };
        let id_subject = format!(
            "{}/{}/{}/{}",
            fragment(check),
            fragment(kind),
            hex_bytes(subject.as_bytes()),
            hex_bytes(assertion.as_bytes())
        );
        assumption.id =
            AssumptionId::disambiguated(AssumptionKind::Waived, &id_subject, &assumption.statement);
        debug_assert!(assumption.validate().is_ok());
        Ok(assumption)
    }

    /// Structural well-formedness, which a registry asserts as it collects and
    /// which every constructor's output satisfies. The rules:
    ///
    /// - all four sentences are present, and none has a gap where a datum
    ///   belonged;
    /// - the id names a subject, and its kind slug is this assumption's kind;
    /// - only a `Waived` assumption carries an expiry (an expiry on a machine
    ///   observation is meaningless, so it is a construction error rather than a
    ///   warning), and a `Waived` one must;
    /// - a `TimeWindow` scope's bounds are real numbers, because they reach the
    ///   published JSON and NaN or infinity is not JSON.
    pub fn validate(&self) -> Result<(), String> {
        for (name, text) in [
            ("statement", &self.statement),
            ("because", &self.because),
            ("consequence", &self.consequence),
            ("replacement", &self.replacement),
        ] {
            if text.trim().is_empty() {
                return Err(format!("{}: `{name}` is empty", self.id));
            }
            // A gap in a composed sentence means a producer passed an empty
            // datum where a subject or a value belonged, which reads as a
            // half-written sentence rather than as an obvious bug.
            // A dot-prefixed artifact token ("the .SchDoc") is data, not a
            // missing interpolation. Only punctuation stranded at the end of
            // a sentence is a hole; internal doubled whitespace remains one too.
            if text.contains("  ") || text.ends_with(" .") || text.ends_with(" ,") {
                return Err(format!(
                    "{}: `{name}` has a hole where a datum belongs: {text:?}",
                    self.id
                ));
            }
        }
        // An id whose subject is empty names nothing, so an acknowledgment file
        // could not name it and a diff could not track it.
        let subject = self
            .id
            .as_str()
            .split_once(':')
            .map(|(_, s)| s)
            .unwrap_or_default();
        if subject.trim().is_empty() {
            return Err(format!("{}: the id names no subject", self.id));
        }
        let id_slug = self
            .id
            .as_str()
            .split_once(':')
            .map(|(k, _)| k)
            .unwrap_or_default();
        if id_slug != self.kind.slug() {
            return Err(format!(
                "{}: the id's kind slug is not `{}`",
                self.id,
                self.kind.slug()
            ));
        }
        if let Scope::Check { check, .. } = &self.scope {
            if check.trim().is_empty() {
                return Err(format!("{}: a check scope needs a check name", self.id));
            }
        }
        match (self.kind, self.expires.as_deref()) {
            (AssumptionKind::Waived, None) => {
                Err(format!("{}: a waived assumption needs an expiry", self.id))
            }
            // Note what is NOT checked here: whether the expiry parses. The
            // waiver file's own loader validates its `until` field and refuses a
            // malformed one, and that machinery deliberately stays where it is.
            // An unreadable date reaching this far is handled where it matters,
            // by the status rule reading it as lapsed.
            (k, Some(_)) if k != AssumptionKind::Waived => Err(format!(
                "{}: a run-derived assumption must not carry an expiry",
                self.id
            )),
            _ => Ok(()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Provenance
// ─────────────────────────────────────────────────────────────────────────────

/// What role an input artifact played in the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    /// A board layout.
    Layout,
    /// A schematic.
    Schematic,
    /// A netlist.
    Netlist,
    /// A fabrication archive (gerbers, ODB++, IPC-2581).
    FabArchive,
    /// A firmware image.
    Firmware,
    /// A bill of materials.
    Bom,
    /// A CI spec / assertion file.
    Spec,
    /// A waiver file.
    Waivers,
    /// A model pack or models directory.
    ModelPack,
    /// An MCU / SoC descriptor.
    SocDescriptor,
}

/// The normalized input format. Shared by provenance producers and renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    KiCadPcb,
    KiCadSchematic,
    KiCadNetlist,
    EagleBoard,
    AltiumPcbDoc,
    GerberArchive,
    OdbPlusPlus,
    Ipc2581,
    Ipc356,
    BoardCode,
    Bom,
    Placement,
    Elf,
    IntelHex,
    Toml,
}

/// One layer of the model-resolution ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ModelLayer {
    /// The producer recorded a resolved model but not which ladder layer won.
    Unspecified,
    Builtin,
    Pack,
    UserDir,
    UserConfigDir,
    ModelsDir,
    Spice,
    EngineFallback,
}

/// Semantic rung in the model-source policy. This is deliberately distinct
/// from [`ModelLayer`]: a layer says where bytes were loaded from, while a tier
/// says how authoritative the model is. An explicit user model remains the
/// override escape hatch; otherwise vendor SPICE and curated sources outrank an
/// extracted draft, which outranks a deliberately estimated fallback.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ModelSourceTier {
    Open,
    EstimatedFallback,
    IntervalModel,
    DatasheetDerived,
    CuratedLibrary,
    CuratedPack,
    VendorSpice,
    UserModel,
}

impl ModelSourceTier {
    /// Policy precedence. The number is internal ordering, not an accuracy
    /// percentage: tiers never invent a numeric error bound.
    pub fn priority(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::EstimatedFallback => 10,
            Self::IntervalModel => 20,
            Self::DatasheetDerived => 30,
            Self::CuratedLibrary => 40,
            Self::CuratedPack => 50,
            Self::VendorSpice => 60,
            Self::UserModel => 70,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::EstimatedFallback => "estimated-fallback",
            Self::IntervalModel => "interval-model",
            Self::DatasheetDerived => "datasheet-derived",
            Self::CuratedLibrary => "curated-library",
            Self::CuratedPack => "curated-pack",
            Self::VendorSpice => "vendor-spice",
            Self::UserModel => "user-model",
        }
    }
}

impl std::fmt::Display for ModelSourceTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ModelSourceTier {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "open" => Ok(Self::Open),
            "estimated-fallback" => Ok(Self::EstimatedFallback),
            "interval-model" => Ok(Self::IntervalModel),
            "datasheet-derived" => Ok(Self::DatasheetDerived),
            "curated-library" => Ok(Self::CuratedLibrary),
            "curated-pack" => Ok(Self::CuratedPack),
            "vendor-spice" => Ok(Self::VendorSpice),
            "user-model" => Ok(Self::UserModel),
            other => Err(format!(
                "unknown model tier {other:?}; expected open, estimated-fallback, interval-model, \
                 datasheet-derived, curated-library, curated-pack, vendor-spice, or user-model"
            )),
        }
    }
}

/// What validation the selected model has actually passed. These names say
/// nothing about unmeasured accuracy: range checking is not curve validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ModelValidation {
    Unvalidated,
    PhysicalBoundsOnly,
    DatasheetCurves,
    VendorQualified,
}

impl ModelValidation {
    pub fn priority(self) -> u8 {
        match self {
            Self::Unvalidated => 0,
            Self::PhysicalBoundsOnly => 10,
            Self::DatasheetCurves => 20,
            Self::VendorQualified => 30,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unvalidated => "unvalidated",
            Self::PhysicalBoundsOnly => "physical-bounds-only",
            Self::DatasheetCurves => "datasheet-curves",
            Self::VendorQualified => "vendor-qualified",
        }
    }
}

impl std::str::FromStr for ModelValidation {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unvalidated" => Ok(Self::Unvalidated),
            "physical-bounds-only" => Ok(Self::PhysicalBoundsOnly),
            "datasheet-curves" => Ok(Self::DatasheetCurves),
            "vendor-qualified" => Ok(Self::VendorQualified),
            other => Err(format!(
                "unknown model validation {other:?}; expected unvalidated, \
                 physical-bounds-only, datasheet-curves, or vendor-qualified"
            )),
        }
    }
}

impl std::fmt::Display for ModelValidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Confidence in a model or parameter match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchConfidence {
    Exact,
    High,
    Heuristic,
    Guessed,
}

/// How a user-authored override entered the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverrideSource {
    AsBuilt,
    Spec,
    Cli,
}

/// One concrete thing an artifact contributed to the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Contribution {
    /// What, as a stable slug: "connectivity", "copper_geometry",
    /// "placements", "component_values", "dnp_flags", "firmware_image",
    /// "net_names", "clearance_rules".
    pub what: String,
    /// The specifics, one sentence, e.g. "netlist read from the document's
    /// LogicalNet section, not reverse-engineered from copper".
    pub detail: String,
}

/// One member or section of an artifact that was deliberately not used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IgnoredInput {
    /// The member: a file inside an archive, a section of a document, or a
    /// sibling file the reader saw and skipped.
    pub what: String,
    /// Why, one sentence.
    pub why: String,
}

/// A cross-check the reader ran between two sources inside (or across)
/// artifacts, and whether they agreed. A disagreement additionally raises a
/// parser-limitation assumption; an agreement is positive evidence and belongs
/// in the inventory in its own right.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CrossCheck {
    /// What was cross-checked.
    pub what: String,
    /// Whether the two sources agreed.
    pub agreed: bool,
    /// The specifics, one sentence.
    pub detail: String,
}

/// One input artifact: what it was, what it gave the run, what was ignored.
/// The run report carries `inventory: Vec<ArtifactProvenance>`, and an
/// [`EvidenceMap`] names artifacts by index into it.
///
/// ```
/// use hauksbee_ir::evidence::{ArtifactKind, ArtifactProvenance, ArtifactRole, Contribution};
///
/// let a = ArtifactProvenance::new(
///     "boards/blinky.kicad_pcb", ArtifactKind::KiCadPcb,
///     ArtifactRole::Layout, "0".repeat(64), Vec::new()
/// ).unwrap().with_contributions(vec![Contribution {
///         what: "connectivity".into(),
///         detail: "nets read from the file's net table".into(),
///     }]);
/// assert_eq!(a.role(), ArtifactRole::Layout);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[schemars(
    description = "One input artifact: what it was, what it gave the run, \
                          what in it was ignored, and which cross-checks it passed."
)]
pub struct ArtifactProvenance {
    /// Path as the user gave it, not canonicalized: the report must read back
    /// in the user's own vocabulary.
    path: String,
    /// What the normalizer recognised it as: "kicad_pcb", "altium_pcbdoc",
    /// "gerber_archive", "odbpp", "ipc2581", "board_code", "elf", "hex",
    /// "toml", and so on.
    kind: ArtifactKind,
    /// The role it played.
    role: ArtifactRole,
    /// SHA-256 of the bytes read (of the archive, for archives). Empty only
    /// for a gerber DIRECTORY, where there is no single file to hash.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    sha256: String,
    /// What it contributed.
    // Skipped when empty like every other vec in this module: an artifact that
    // contributed nothing says so by carrying no key, and adding this skip after
    // the schema publishes would remove a key a consumer may index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    contributed: Vec<Contribution>,
    /// What in it was deliberately not used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignored: Vec<IgnoredInput>,
    /// Cross-checks run on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cross_checks: Vec<CrossCheck>,
    /// Assumptions this artifact's limitations raised, by id, so the inventory
    /// row and the assumption registry cross-reference instead of duplicating
    /// text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assumptions: Vec<AssumptionId>,
}

impl ArtifactProvenance {
    /// Construct one artifact record. Referential integrity for `assumptions`
    /// is checked when the artifact enters an [`EvidenceRegistry`].
    pub fn new(
        path: impl Into<String>,
        kind: ArtifactKind,
        role: ArtifactRole,
        sha256: impl Into<String>,
        assumptions: Vec<AssumptionId>,
    ) -> Result<Self, EvidenceError> {
        let path = path.into();
        let sha256 = sha256.into();
        if path.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "artifact.path",
            });
        }
        if !sha256.is_empty()
            && (sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(EvidenceError::InvalidSha256);
        }
        Ok(Self {
            path,
            kind,
            role,
            sha256,
            contributed: Vec::new(),
            ignored: Vec::new(),
            cross_checks: Vec::new(),
            assumptions,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn kind(&self) -> ArtifactKind {
        self.kind
    }

    pub fn role(&self) -> ArtifactRole {
        self.role
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn assumptions(&self) -> &[AssumptionId] {
        &self.assumptions
    }

    pub fn with_contributions(mut self, contributions: Vec<Contribution>) -> Self {
        self.contributed = contributions;
        self
    }

    pub fn with_ignored(mut self, ignored: Vec<IgnoredInput>) -> Self {
        self.ignored = ignored;
        self
    }

    pub fn with_cross_checks(mut self, cross_checks: Vec<CrossCheck>) -> Self {
        self.cross_checks = cross_checks;
        self
    }
}

/// Where a bound parameter's value came from.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValueOrigin {
    /// Read from an input artifact, by index into the run inventory.
    Artifact {
        /// Index into the run's `inventory`.
        index: ArtifactId,
        /// Which field of the artifact carried it.
        field: String,
    },
    /// From a resolved model. `layer` is the model source layer ("builtin",
    /// "pack", "user-dir", "user-config-dir", "models-dir", "spice",
    /// "engine-fallback"); `confidence` is the match confidence's display form
    /// ("exact" through "guessed").
    Model {
        /// The model that supplied it.
        model_id: String,
        /// Which layer of the model ladder it came from.
        layer: ModelLayer,
        /// How good the match was.
        confidence: MatchConfidence,
    },
    /// A documented default, naming the assumption that records it.
    Default {
        /// The `DefaultParameter` assumption for this value.
        assumption: AssumptionId,
    },
    /// The user overrode it (an as-built overlay, a spec field, a CLI flag).
    UserOverride {
        /// How the override arrived.
        via: OverrideSource,
    },
}

/// Provenance for one bound parameter of one device.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ParameterProvenance {
    /// Reference-qualified, so an evidence map can select by part:
    /// "R7.resistance", "U1.model", "Q2.beta".
    parameter: String,
    /// Rendered value ("10k", "TP4056", "1.2e-14 A").
    value: String,
    /// Where the value came from.
    origin: ValueOrigin,
}

impl ParameterProvenance {
    pub fn new(
        parameter: impl Into<String>,
        value: impl Into<String>,
        origin: ValueOrigin,
    ) -> Result<Self, EvidenceError> {
        let parameter = parameter.into();
        let value = value.into();
        if parameter.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "parameter_provenance.parameter",
            });
        }
        if value.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "parameter_provenance.value",
            });
        }
        Ok(Self {
            parameter,
            value,
            origin,
        })
    }

    pub fn parameter(&self) -> &str {
        &self.parameter
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn origin(&self) -> &ValueOrigin {
        &self.origin
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error budget
// ─────────────────────────────────────────────────────────────────────────────

/// A sim-time window, seconds, half-open `[start_s, end_s)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TimeWindow {
    /// Window start, seconds.
    start_s: f64,
    /// Window end, seconds, exclusive.
    end_s: f64,
}

impl TimeWindow {
    /// Construct a finite, ordered half-open time window.
    pub fn new(start_s: f64, end_s: f64) -> Result<Self, EvidenceError> {
        finite("time_window.start_s", start_s)?;
        finite("time_window.end_s", end_s)?;
        if end_s < start_s {
            return Err(EvidenceError::InvertedWindow { start_s, end_s });
        }
        Ok(Self { start_s, end_s })
    }

    /// Whether both bounds are real numbers.
    pub fn is_finite(&self) -> bool {
        self.start_s.is_finite() && self.end_s.is_finite()
    }

    pub fn start_s(self) -> f64 {
        self.start_s
    }

    pub fn end_s(self) -> f64 {
        self.end_s
    }
}

/// One window and the integration method that produced it. The primary method
/// covers most runs in a single entry; a per-chunk fallback ladder contributes
/// one entry per fallback-solved window, using that machinery's own stable
/// method names verbatim ("trapezoidal", "gear2", "backward-euler",
/// "reduced-step", "cold-start-backward-euler",
/// "subdivided-backward-euler").
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct WindowMethod {
    /// The span this method produced.
    window: TimeWindow,
    /// The method's stable name.
    method: IntegrationMethod,
}

/// The integration algorithm used for a solved window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationMethod {
    /// Trapezoidal integration.
    Trapezoidal,
    /// Second-order Gear integration.
    Gear2,
    /// Backward Euler integration.
    BackwardEuler,
    /// Backward Euler after reducing the step.
    ReducedStep,
    /// Backward Euler after rebuilding the local state.
    ColdStartBackwardEuler,
    /// Backward Euler over recursively subdivided chunks.
    SubdividedBackwardEuler,
}

impl WindowMethod {
    /// Construct one method record.
    ///
    /// The record deliberately carries no made-up percentage "accuracy cost".
    /// An integration algorithm and timestep are provenance, not an empirical
    /// error bound. A producer may report a measured comparison elsewhere, but
    /// it must not translate "first order" into an unsupported relative error.
    pub fn new(window: TimeWindow, method: IntegrationMethod) -> Result<Self, EvidenceError> {
        Ok(Self { window, method })
    }

    pub fn window(&self) -> TimeWindow {
        self.window
    }

    pub fn method(&self) -> IntegrationMethod {
        self.method
    }
}

/// The solver's convergence and truncation-error settings for this result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, schemars::JsonSchema)]
pub struct IntegrationTolerance {
    /// Relative tolerance for Newton convergence and local-truncation-error
    /// control.
    reltol: f64,
    /// Absolute voltage tolerance, volts.
    vntol: f64,
    /// Absolute current tolerance, amps.
    abstol: f64,
    /// Charge tolerance, coulombs (capacitor local truncation error).
    chgtol: f64,
}

impl IntegrationTolerance {
    /// Construct strictly-positive, finite solver tolerances.
    pub fn new(reltol: f64, vntol: f64, abstol: f64, chgtol: f64) -> Result<Self, EvidenceError> {
        positive("integration_tolerance.reltol", reltol)?;
        positive("integration_tolerance.vntol", vntol)?;
        positive("integration_tolerance.abstol", abstol)?;
        positive("integration_tolerance.chgtol", chgtol)?;
        Ok(Self {
            reltol,
            vntol,
            abstol,
            chgtol,
        })
    }

    pub fn reltol(self) -> f64 {
        self.reltol
    }

    pub fn vntol(self) -> f64 {
        self.vntol
    }

    pub fn abstol(self) -> f64 {
        self.abstol
    }

    pub fn chgtol(self) -> f64 {
        self.chgtol
    }
}

/// The final accepted Newton residual, worst node.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Residual {
    /// `max |f(x)|` over node KCL rows at acceptance, amperes.
    max_abs: f64,
    /// The node or branch carrying it, for the "who" question.
    at: String,
}

impl Residual {
    pub fn new(max_abs: f64, at: impl Into<String>) -> Result<Self, EvidenceError> {
        non_negative("residual.max_abs", max_abs)?;
        let at = at.into();
        if at.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "residual.at",
            });
        }
        Ok(Self { max_abs, at })
    }

    pub fn max_abs(&self) -> f64 {
        self.max_abs
    }

    pub fn at(&self) -> &str {
        &self.at
    }
}

/// Interval bounds a model places on a derived quantity. This is the socket for
/// interval models: shaped before its producer exists so that work lands in
/// this vocabulary instead of inventing a second one.
///
/// Both bounds are real numbers. A one-sided datasheet limit ("at least 100")
/// is [`ModelUncertainty::Unknown`] until a defensible other bound exists. A
/// producer must never turn a typical value or an invented physical cap into a
/// guaranteed two-sided accuracy interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ModelIntervalKind {
    /// Published finite minimum and maximum specification limits.
    SpecificationLimits,
    /// Finite error bounds established by empirical validation.
    EmpiricalError,
    /// A published typical spread; informative, but not guaranteed.
    TypicalRange,
    /// A finite engineering estimate; informative, but not validated.
    EstimatedRange,
}

impl ModelIntervalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpecificationLimits => "specification-limits",
            Self::EmpiricalError => "empirical-error",
            Self::TypicalRange => "typical-range",
            Self::EstimatedRange => "estimated-range",
        }
    }
}

impl std::fmt::Display for ModelIntervalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelUncertainty {
    /// A measured or source-published finite interval.
    Interval {
        parameter: String,
        low: f64,
        high: f64,
        unit: String,
        kind: ModelIntervalKind,
        basis: String,
    },
    /// No defensible numeric interval is available. Unknown is data, never an
    /// omitted field and never a made-up percentage.
    Unknown { parameter: String, reason: String },
}

impl ModelUncertainty {
    /// Construct a finite, ordered uncertainty interval.
    pub fn new(
        parameter: impl Into<String>,
        low: f64,
        high: f64,
        basis: impl Into<String>,
    ) -> Result<Self, EvidenceError> {
        Self::interval(parameter, low, high, "", basis)
    }

    pub fn interval(
        parameter: impl Into<String>,
        low: f64,
        high: f64,
        unit: impl Into<String>,
        basis: impl Into<String>,
    ) -> Result<Self, EvidenceError> {
        Self::interval_with_kind(
            parameter,
            low,
            high,
            unit,
            ModelIntervalKind::SpecificationLimits,
            basis,
        )
    }

    pub fn interval_with_kind(
        parameter: impl Into<String>,
        low: f64,
        high: f64,
        unit: impl Into<String>,
        kind: ModelIntervalKind,
        basis: impl Into<String>,
    ) -> Result<Self, EvidenceError> {
        finite("model_uncertainty.low", low)?;
        finite("model_uncertainty.high", high)?;
        let parameter = parameter.into();
        if high < low {
            return Err(EvidenceError::InvertedInterval {
                parameter,
                low,
                high,
            });
        }
        let basis = basis.into();
        if parameter.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_uncertainty.parameter",
            });
        }
        if basis.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_uncertainty.basis",
            });
        }
        Ok(Self::Interval {
            parameter,
            low,
            high,
            unit: unit.into(),
            kind,
            basis,
        })
    }

    pub fn unknown(
        parameter: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self, EvidenceError> {
        let parameter = parameter.into();
        let reason = reason.into();
        if parameter.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_uncertainty.parameter",
            });
        }
        if reason.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_uncertainty.reason",
            });
        }
        Ok(Self::Unknown { parameter, reason })
    }

    pub fn validate(&self) -> Result<(), EvidenceError> {
        match self {
            Self::Interval {
                parameter,
                low,
                high,
                basis,
                ..
            } => {
                finite("model_uncertainty.low", *low)?;
                finite("model_uncertainty.high", *high)?;
                if high < low {
                    return Err(EvidenceError::InvertedInterval {
                        parameter: parameter.clone(),
                        low: *low,
                        high: *high,
                    });
                }
                if parameter.trim().is_empty() {
                    return Err(EvidenceError::Empty {
                        field: "model_uncertainty.parameter",
                    });
                }
                if basis.trim().is_empty() {
                    return Err(EvidenceError::Empty {
                        field: "model_uncertainty.basis",
                    });
                }
            }
            Self::Unknown { parameter, reason } => {
                if parameter.trim().is_empty() {
                    return Err(EvidenceError::Empty {
                        field: "model_uncertainty.parameter",
                    });
                }
                if reason.trim().is_empty() {
                    return Err(EvidenceError::Empty {
                        field: "model_uncertainty.reason",
                    });
                }
            }
        }
        Ok(())
    }

    /// Whether this interval can satisfy a fail-closed accuracy requirement.
    /// Typical and estimated ranges stay visible but never become guarantees.
    pub fn is_strict_bound(&self) -> bool {
        matches!(
            self,
            Self::Interval {
                kind: ModelIntervalKind::SpecificationLimits | ModelIntervalKind::EmpiricalError,
                ..
            }
        )
    }

    pub fn interval_kind(&self) -> Option<ModelIntervalKind> {
        match self {
            Self::Interval { kind, .. } => Some(*kind),
            Self::Unknown { .. } => None,
        }
    }
}

/// Canonical provenance and accuracy record for one selected model. Every
/// report surface carries this object verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelSource {
    tier: ModelSourceTier,
    layer: ModelLayer,
    origin: String,
    validation: ModelValidation,
    uncertainty: Vec<ModelUncertainty>,
}

impl ModelSource {
    pub fn new(
        tier: ModelSourceTier,
        layer: ModelLayer,
        origin: impl Into<String>,
        validation: ModelValidation,
        uncertainty: Vec<ModelUncertainty>,
    ) -> Result<Self, EvidenceError> {
        let origin = origin.into();
        if origin.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_source.origin",
            });
        }
        if uncertainty.is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_source.uncertainty",
            });
        }
        for value in &uncertainty {
            value.validate()?;
        }
        Ok(Self {
            tier,
            layer,
            origin,
            validation,
            uncertainty,
        })
    }

    pub fn tier(&self) -> ModelSourceTier {
        self.tier
    }
    pub fn layer(&self) -> ModelLayer {
        self.layer
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn validation(&self) -> ModelValidation {
        self.validation
    }
    pub fn uncertainty(&self) -> &[ModelUncertainty] {
        &self.uncertainty
    }
}

fn finite(field: &'static str, value: f64) -> Result<(), EvidenceError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(EvidenceError::NonFinite { field, value })
    }
}

fn positive(field: &'static str, value: f64) -> Result<(), EvidenceError> {
    finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(EvidenceError::NonPositive { field, value })
    }
}

fn non_negative(field: &'static str, value: f64) -> Result<(), EvidenceError> {
    finite(field, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(EvidenceError::Negative { field, value })
    }
}

/// Everything a consumer needs to know about how good a number is.
///
/// Rides on the things that carry numbers: one per transient/co-sim run at run
/// level, one per assertion inside its [`EvidenceMap`] with windows clipped to
/// that assertion's observation window. A DC-only path carries no methods and
/// no event-time error.
///
/// ```
/// use hauksbee_ir::evidence::{ErrorBudget, IntegrationTolerance};
///
/// let tolerance = IntegrationTolerance::new(1e-3, 1e-6, 1e-12, 1e-14).unwrap();
/// let b = ErrorBudget::new(tolerance);
/// assert!(b.methods().is_empty());
/// assert!(b.failed_windows().is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[schemars(
    description = "How good a number is: integration tolerances, which method \
                          produced which time window, the accepted residual, the \
                          windows with no valid solution, and model uncertainty."
)]
pub struct ErrorBudget {
    /// The solver settings the numbers were produced under.
    tolerance: IntegrationTolerance,
    /// Which method produced which span. One entry for a homogeneous run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    methods: Vec<WindowMethod>,
    /// The accepted Newton residual, when the producing path measured one.
    /// Present only when the producer measured a finite, non-negative residual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Residual")]
    residual: Option<Residual>,
    /// Windows with NO valid solution: the run held stale values there. Any
    /// quantity read inside one of these is not a measurement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    failed_windows: Vec<TimeWindow>,
    /// Worst-case timestamp error for events, seconds. Today this is the
    /// co-sim chunk quantization: an edge is observed at a chunk boundary.
    /// Constructed only from a finite, non-negative value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    event_time_error_s: Option<f64>,
    /// Interval bounds models place on parameters behind these numbers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_uncertainty: Vec<ModelUncertainty>,
}

impl ErrorBudget {
    /// Compact human rendering of the same typed data carried in JSON. This
    /// deliberately labels tolerances as solver settings and an absent
    /// residual as unmeasured; it never turns method order into a percentage.
    pub fn plain_summary(&self) -> String {
        let tolerance = self.tolerance;
        let residual = self
            .residual
            .as_ref()
            .map(|residual| format!("{:.3e}A at {}", residual.max_abs, residual.at))
            .unwrap_or_else(|| "unmeasured".to_string());
        let failed = match self.failed_windows.len() {
            1 => "1 window".to_string(),
            count => format!("{count} windows"),
        };
        let methods = self
            .methods
            .iter()
            .map(|entry| match entry.method {
                IntegrationMethod::Trapezoidal => "trapezoidal",
                IntegrationMethod::Gear2 => "gear2",
                IntegrationMethod::BackwardEuler => "backward-euler",
                IntegrationMethod::ReducedStep => "reduced-step",
                IntegrationMethod::ColdStartBackwardEuler => "cold-start-backward-euler",
                IntegrationMethod::SubdividedBackwardEuler => "subdivided-backward-euler",
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(",");
        let event_time = self
            .event_time_error_s
            .map(|seconds| format!("<={seconds:.3e}s"))
            .unwrap_or_else(|| "unmeasured".to_string());
        format!(
            "settings reltol={:.3e} vntol={:.3e}V abstol={:.3e}A chgtol={:.3e}C; \
             residual={residual}; methods={}; failed={failed}; event_time_error={event_time}; \
             model_intervals={}",
            tolerance.reltol,
            tolerance.vntol,
            tolerance.abstol,
            tolerance.chgtol,
            if methods.is_empty() { "none" } else { &methods },
            self.model_uncertainty.len(),
        )
    }

    pub fn with_method(mut self, method: WindowMethod) -> Self {
        self.methods.push(method);
        self
    }

    pub fn with_residual(mut self, residual: Residual) -> Self {
        self.residual = Some(residual);
        self
    }

    pub fn with_failed_window(mut self, window: TimeWindow) -> Self {
        self.failed_windows.push(window);
        self
    }

    pub fn with_uncertainty(mut self, uncertainty: ModelUncertainty) -> Self {
        self.model_uncertainty.push(uncertainty);
        self
    }

    pub fn tolerance(&self) -> IntegrationTolerance {
        self.tolerance
    }

    pub fn methods(&self) -> &[WindowMethod] {
        &self.methods
    }

    pub fn residual(&self) -> Option<&Residual> {
        self.residual.as_ref()
    }

    pub fn failed_windows(&self) -> &[TimeWindow] {
        &self.failed_windows
    }

    pub fn model_uncertainty(&self) -> &[ModelUncertainty] {
        &self.model_uncertainty
    }

    pub fn event_time_error_s(&self) -> Option<f64> {
        self.event_time_error_s
    }

    /// Attach a finite, non-negative event timestamp error. Invalid values are
    /// errors, not absent fields.
    pub fn with_event_time_error(mut self, seconds: f64) -> Result<Self, EvidenceError> {
        non_negative("error_budget.event_time_error_s", seconds)?;
        self.event_time_error_s = Some(seconds);
        Ok(self)
    }

    /// The common shape: known tolerances, nothing else measured yet.
    pub fn new(tolerance: IntegrationTolerance) -> Self {
        Self {
            tolerance,
            methods: Vec::new(),
            residual: None,
            failed_windows: Vec::new(),
            event_time_error_s: None,
            model_uncertainty: Vec::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Evidence map
// ─────────────────────────────────────────────────────────────────────────────

/// A model on the causal path.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ModelOnPath {
    /// The part it is bound to.
    reference: String,
    /// The model that bound.
    model_id: String,
    /// Compatibility projection of [`Self::source`]'s storage layer.
    layer: ModelLayer,
    /// The canonical source, validation and uncertainty record.
    source: ModelSource,
    /// How good the match was.
    confidence: MatchConfidence,
}

impl ModelOnPath {
    /// Construct a typed model-path record.
    pub fn new(
        reference: impl Into<String>,
        model_id: impl Into<String>,
        source: ModelSource,
        confidence: MatchConfidence,
    ) -> Result<Self, EvidenceError> {
        let reference = reference.into();
        let model_id = model_id.into();
        if reference.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_on_path.reference",
            });
        }
        if model_id.trim().is_empty() {
            return Err(EvidenceError::Empty {
                field: "model_on_path.model_id",
            });
        }
        Ok(Self {
            reference,
            model_id,
            layer: source.layer(),
            source,
            confidence,
        })
    }

    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn layer(&self) -> ModelLayer {
        self.layer
    }

    pub fn source(&self) -> &ModelSource {
        &self.source
    }

    pub fn confidence(&self) -> MatchConfidence {
        self.confidence
    }
}

/// Stable index into one validated run inventory. The inner index is private;
/// only [`EvidenceRegistry::add_artifact`] can mint one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ArtifactId(usize);

/// A registry whose stable ids are unique and whose assumptions have passed
/// structural validation. Traversal and map construction share this instance,
/// so an evidence map cannot cite an assumption absent from the run registry.
#[derive(Debug, Clone)]
pub struct EvidenceRegistry {
    assumptions: Vec<Assumption>,
    artifacts: Vec<ArtifactProvenance>,
}

impl EvidenceRegistry {
    /// Validate and index a run's assumption registry.
    pub fn new(assumptions: Vec<Assumption>) -> Result<Self, EvidenceError> {
        let mut ids = HashSet::with_capacity(assumptions.len());
        for assumption in &assumptions {
            assumption
                .validate()
                .map_err(|message| EvidenceError::InvalidAssumption { message })?;
            if !ids.insert(assumption.id().clone()) {
                return Err(EvidenceError::DuplicateAssumption {
                    id: assumption.id().to_string(),
                });
            }
        }
        Ok(Self {
            assumptions,
            artifacts: Vec::new(),
        })
    }

    /// The validated assumptions in deterministic registry order.
    pub fn assumptions(&self) -> &[Assumption] {
        &self.assumptions
    }

    /// Add an artifact after resolving all assumption references.
    pub fn add_artifact(
        &mut self,
        artifact: ArtifactProvenance,
    ) -> Result<ArtifactId, EvidenceError> {
        let known: HashSet<&AssumptionId> = self.assumptions.iter().map(Assumption::id).collect();
        for id in artifact.assumptions() {
            if !known.contains(id) {
                return Err(EvidenceError::MissingAssumption { id: id.to_string() });
            }
        }
        let id = ArtifactId(self.artifacts.len());
        self.artifacts.push(artifact);
        Ok(id)
    }

    pub fn artifacts(&self) -> &[ArtifactProvenance] {
        &self.artifacts
    }
}

/// The net and optional observation interval an assertion causally reads.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetScope {
    nets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "TimeWindow")]
    window: Option<TimeWindow>,
}

impl NetScope {
    /// Construct a non-empty net scope.
    pub fn new<I, S>(nets: I, window: Option<TimeWindow>) -> Result<Self, EvidenceError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut out = Vec::new();
        for net in nets {
            let net = net.into();
            if net.trim().is_empty() {
                return Err(EvidenceError::Empty {
                    field: "net_scope.net",
                });
            }
            if !out.contains(&net) {
                out.push(net);
            }
        }
        if out.is_empty() {
            return Err(EvidenceError::Empty {
                field: "net_scope.nets",
            });
        }
        Ok(Self { nets: out, window })
    }

    /// The named nets in deterministic input order.
    pub fn nets(&self) -> &[String] {
        &self.nets
    }

    /// The assertion observation window, when it has one.
    pub fn window(&self) -> Option<TimeWindow> {
        self.window
    }
}

/// Validated net-to-part incidence used by the first real causal traversal.
#[derive(Debug, Clone)]
pub struct CausalPathIndex {
    net_parts: BTreeMap<String, BTreeSet<String>>,
}

impl CausalPathIndex {
    /// Build deterministic incidence from the binder-shaped `net -> refs`
    /// boundary. Empty names are rejected rather than becoming unreachable.
    pub fn from_net_parts<'a, I, R>(incidence: I) -> Result<Self, EvidenceError>
    where
        I: IntoIterator<Item = (&'a str, &'a [R])>,
        R: AsRef<str> + 'a,
    {
        let mut net_parts = BTreeMap::new();
        for (net, refs) in incidence {
            if net.trim().is_empty() {
                return Err(EvidenceError::Empty {
                    field: "causal_path.net",
                });
            }
            let entry = net_parts
                .entry(net.to_string())
                .or_insert_with(BTreeSet::new);
            for reference in refs {
                let reference = reference.as_ref();
                if reference.trim().is_empty() {
                    return Err(EvidenceError::Empty {
                        field: "causal_path.reference",
                    });
                }
                entry.insert(reference.to_string());
            }
        }
        Ok(Self { net_parts })
    }

    /// Traverse one assertion's net scope against a validated registry.
    pub fn traverse(
        &self,
        subject: &NetScope,
        registry: &EvidenceRegistry,
    ) -> Result<TraversalResult, EvidenceError> {
        self.traverse_inner(subject, None, registry)
    }

    /// Traverse an assertion and also admit a check-scoped fact only when its
    /// check and stable assertion key both match. This is the non-saturating
    /// path for facts such as a live waiver or a peripheral that never ran:
    /// attaching every `ci` fact to every CI assertion would make the evidence
    /// registry loud but causally useless.
    pub fn traverse_assertion(
        &self,
        subject: &NetScope,
        check: &str,
        assertion_key: &str,
        registry: &EvidenceRegistry,
    ) -> Result<TraversalResult, EvidenceError> {
        self.traverse_inner(subject, Some((check, assertion_key)), registry)
    }

    /// Traversal for a geometry-and-stated-value claim: one computed from
    /// copper, stackup, and the value fields of the parts on its nets, never
    /// from a bound simulation model. Model-class part assumptions
    /// (`OpenPart`, `SubstituteModel`) are off such a claim's causal path: a
    /// part being open means it has no simulation model, and nothing here was
    /// simulated. Everything else stays on: presence-class part assumptions
    /// (`FittedByDefault`; the claim read the part's value, so whether the
    /// part is really fitted IS its evidence), every net-scoped and
    /// board-scoped limitation, and the check's own scoped assumptions.
    pub fn traverse_value_claim(
        &self,
        subject: &NetScope,
        check: &str,
        assertion_key: &str,
        registry: &EvidenceRegistry,
    ) -> Result<TraversalResult, EvidenceError> {
        let mut result = self.traverse_inner(subject, Some((check, assertion_key)), registry)?;
        result.on_path.retain(|a| {
            let part_scoped = match a.scope() {
                Scope::Subjects(subjects) => subjects
                    .as_slice()
                    .iter()
                    .any(|entity| entity.kind() == EntityKind::Part),
                Scope::Parameter(parameter) => parameter.subject().kind() == EntityKind::Part,
                Scope::Board | Scope::Nets(_) | Scope::Check { .. } => false,
            };
            !(part_scoped
                && matches!(
                    a.kind,
                    AssumptionKind::OpenPart | AssumptionKind::SubstituteModel
                ))
        });
        Ok(result)
    }

    fn traverse_inner(
        &self,
        subject: &NetScope,
        check_scope: Option<(&str, &str)>,
        registry: &EvidenceRegistry,
    ) -> Result<TraversalResult, EvidenceError> {
        let mut reachable_parts = BTreeSet::new();
        for net in subject.nets() {
            let parts = self
                .net_parts
                .get(net)
                .ok_or_else(|| EvidenceError::UnknownNet { net: net.clone() })?;
            reachable_parts.extend(parts.iter().cloned());
        }

        let on_path = registry
            .assumptions()
            .iter()
            .filter(|assumption| match assumption.scope() {
                Scope::Board => true,
                Scope::Subjects(subjects) => subjects.as_slice().iter().any(|entity| {
                    entity.kind() == EntityKind::Part && reachable_parts.contains(entity.id())
                }),
                Scope::Parameter(parameter) => {
                    parameter.subject().kind() == EntityKind::Part
                        && reachable_parts.contains(parameter.subject().id())
                }
                Scope::Nets(nets) => {
                    let overlaps_net = nets.nets().iter().any(|n| subject.nets().contains(n));
                    let overlaps_time = match (nets.window(), subject.window()) {
                        (Some(a), Some(b)) => a.start_s < b.end_s && b.start_s < a.end_s,
                        _ => true,
                    };
                    overlaps_net && overlaps_time
                }
                Scope::Check { check, kind } => {
                    check_scope.is_some_and(|(expected_check, expected_key)| {
                        check.eq_ignore_ascii_case(expected_check)
                            && kind.as_deref().is_none_or(|actual| actual == expected_key)
                    })
                }
            })
            .cloned()
            .collect();
        Ok(TraversalResult { on_path })
    }
}

/// An opaque, validated causal traversal. Its field is private so only a
/// traversal adapter in this module can authorize evidence-map construction.
#[derive(Debug, Clone)]
pub struct TraversalResult {
    on_path: Vec<Assumption>,
}

/// How much a conclusion is entitled to claim, derived from the assumptions on
/// its causal path. Ordered by severity: `Clean < Qualified < Undermined`.
///
/// Use [`EvidenceStatus::as_str`] on every surface. One of the surfaces this
/// feeds is hand-serialized JSON rather than serde output, and a module whose
/// purpose is that surfaces cannot drift should not have five call sites
/// spelling `"undermined"` themselves.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// No on-path assumptions. The conclusion stands on evidence alone.
    Clean,
    /// On-path assumptions exist but none is critical: the conclusion holds
    /// with named caveats.
    Qualified,
    /// A critical assumption sits on the causal path. The conclusion is not
    /// entitled to a verdict: invalid for analysis, exit-code-3 semantics.
    Undermined,
}

impl EvidenceStatus {
    /// The wire form, identical to what serde writes. The one place this
    /// vocabulary is spelled, so a hand-written surface cannot spell it
    /// differently.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Qualified => "qualified",
            Self::Undermined => "undermined",
        }
    }
}

impl std::fmt::Display for EvidenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one assertion rests on: the artifacts, models, parameters and
/// assumptions on its causal path, its error budget, and the derived status.
///
/// `status`, the assertion it is a status OF, and the id list it was derived
/// from are all private, and there is one constructor. What that guarantees,
/// exactly: a status cannot disagree with the assumptions it was handed, and no
/// producer, renderer or sibling crate can move the three apart afterwards.
/// `Undermined` maps onto the run's existing invalid-for-analysis outcome, which
/// waivers already refuse to flip green, so the only routes back to a clean
/// verdict are closing the assumption or acknowledging it in the open.
///
/// Construction requires the opaque result of [`CausalPathIndex::traverse`]. A
/// caller cannot hand over an arbitrary empty slice and mint `Clean`; an unknown
/// net is an error, while a known net with no on-path assumptions is clean.
///
/// Like [`Assumption`], and for the same reason, the type does not implement
/// `Deserialize`. A serialized map carries assumption *ids*, not kinds, so a
/// status could not be re-derived on the way back in even in principle: the kinds
/// live in the run's registry, a different part of the document. A permissive
/// `Deserialize` would therefore be a laundering route with no compensating
/// check, and deleting the `assumptions` key from a hand-edited report would read
/// back `Clean` over a real gap. A consumer that has the registry and wants the
/// answer calls [`EvidenceMap::derive_status`] on it, which is the only honest
/// way to get one.
///
/// ```
/// use hauksbee_ir::evidence::{Assumption, CausalPathIndex, EvidenceMap,
///     EvidenceRegistry, EvidenceStatus, NetScope, RunDate};
///
/// let today = RunDate::from_epoch_days(20_666); // 2026-08-01
/// let open = Assumption::open_part("U2", "XC6206", "no model matched");
/// let registry = EvidenceRegistry::new(vec![open]).unwrap();
/// let graph = CausalPathIndex::from_net_parts([("3V3", ["U2"].as_slice()),
///     ("VBUS", ["J1"].as_slice())]).unwrap();
/// let on_3v3 = graph.traverse(&NetScope::new(["3V3"], None).unwrap(), &registry).unwrap();
/// let map = EvidenceMap::from_traversal("3V3 stays above 3.1 V", on_3v3,
///     &registry, today).unwrap();
/// assert_eq!(map.status(), EvidenceStatus::Undermined);
/// assert_eq!(map.assumptions()[0].as_str(), "open-part:U2");
///
/// let on_vbus = graph.traverse(&NetScope::new(["VBUS"], None).unwrap(), &registry).unwrap();
/// let clean = EvidenceMap::from_traversal("VBUS stays below 5.5 V", on_vbus,
///     &registry, today).unwrap();
/// assert_eq!(clean.status(), EvidenceStatus::Clean);
/// assert!(clean.assumptions().is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[schemars(description = "What one assertion rests on: the artifacts, models, \
                          parameters and assumptions on its causal path, its error \
                          budget, and the status derived from them.")]
pub struct EvidenceMap {
    /// The assertion or finding this map belongs to.
    // Private with a getter: it is set once, and relabelling a clean map onto a
    // different assertion is the same forgery one indirection out.
    assertion: String,
    /// Indices into the run's `inventory`, causal only: the artifacts whose
    /// contributions this assertion actually consumed. Omitted when empty, per
    /// the module's serde convention: a check that reads no artifact says so by
    /// carrying no key, and adding this skip after the schema publishes would be
    /// the breaking change the once-only version bump is being saved for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    artifacts: Vec<ArtifactId>,
    /// Models bound on the causal path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<ModelOnPath>,
    /// Parameters read on the causal path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parameters: Vec<ParameterProvenance>,
    /// On-path assumptions, by id into the run's assumption registry.
    // Private with a getter, because it is half of the derived-status
    // invariant: an id pushed in after construction would let the list and the
    // status disagree. Kept as a `//` comment so the published schema carries
    // the description a consumer needs, not this crate's internal reasoning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assumptions: Vec<AssumptionId>,
    /// How good the numbers behind this assertion are.
    // Private with `with_error_budget` as the only way in: a failed window is a
    // claim that a quantity read inside it is not a measurement, so a mutable
    // budget would let a later stage delete that claim rather than a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ErrorBudget")]
    error_budget: Option<ErrorBudget>,
    /// Statistical coverage wording for ensemble assertions. Not an
    /// assumption: an accurate statement of method.
    // Private with `with_coverage` as the only way in. It is prose that renders,
    // which puts it in the same class as the sentence fields rather than in the
    // class of index lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    coverage: Option<String>,
    /// How much this conclusion is entitled to claim, derived from the
    /// assumptions on its causal path.
    // DERIVED, never set: see `EvidenceMap::derive_status`.
    status: EvidenceStatus,
}

impl EvidenceMap {
    /// Construct from an opaque result emitted by the validated causal
    /// traversal. The registry is repeated here deliberately: it is the
    /// referential-integrity boundary and will grow the artifact checks too.
    pub fn from_traversal(
        assertion: impl Into<String>,
        traversal: TraversalResult,
        registry: &EvidenceRegistry,
        today: RunDate,
    ) -> Result<Self, EvidenceError> {
        let known: HashSet<&AssumptionId> =
            registry.assumptions().iter().map(Assumption::id).collect();
        for assumption in &traversal.on_path {
            if !known.contains(assumption.id()) {
                return Err(EvidenceError::InvalidAssumption {
                    message: format!(
                        "traversal cited assumption {} absent from its registry",
                        assumption.id()
                    ),
                });
            }
        }
        Ok(Self::new(assertion, &traversal.on_path, today))
    }

    /// The one constructor. `on_path` is the assumptions the causal-path
    /// traversal found for this assertion, whole rather than by id, because the
    /// status rule reads their kinds and expiries.
    ///
    /// `today` is the run's date, used only to tell a live waiver from a lapsed
    /// one. It is a [`RunDate`] rather than a bare number because the failure
    /// directions are not symmetric: a date read too LATE only expires waivers
    /// early, while a date read too early re-arms every lapsed one, silently, in
    /// the one direction this must never fail in.
    fn new(assertion: impl Into<String>, on_path: &[Assumption], today: RunDate) -> Self {
        // Deduped, first occurrence winning. A traversal walking several nets
        // that share a part hands the same assumption over more than once, and
        // rendering it twice is noise; the status is a max, so duplicates never
        // changed it.
        let mut assumptions: Vec<AssumptionId> = Vec::with_capacity(on_path.len());
        for a in on_path {
            if !assumptions.contains(&a.id) {
                assumptions.push(a.id.clone());
            }
        }
        Self {
            assertion: assertion.into(),
            artifacts: Vec::new(),
            models: Vec::new(),
            parameters: Vec::new(),
            assumptions,
            error_budget: None,
            coverage: None,
            status: Self::derive_status(on_path, today),
        }
    }

    /// The assertion this map belongs to.
    pub fn assertion(&self) -> &str {
        &self.assertion
    }

    /// The status rule, the whole policy in one function.
    ///
    /// | On-path assumption kind | Effect |
    /// |---|---|
    /// | `OpenPart`, `SubstituteModel`, `NotChecked`, `NotExercised`, `FittedByDefault` | `Undermined` |
    /// | `InferredPinRole`, `DefaultParameter`, `ReducedFidelity`, `ParserLimitation` | `Qualified` |
    /// | `Waived`, expiry not yet passed | `Qualified` |
    /// | `Waived`, expired or with an unreadable expiry | `Undermined` |
    /// | none | `Clean` |
    ///
    /// Membership does the scoping: only assumptions the traversal found on
    /// this assertion's causal path are passed in, which is why the table needs
    /// no per-kind "if it covers this assertion" clause. Severity is the max
    /// over the set, so one undermining assumption is enough.
    pub fn derive_status(on_path: &[Assumption], today: RunDate) -> EvidenceStatus {
        let mut status = EvidenceStatus::Clean;
        for a in on_path {
            let effect = match a.kind {
                AssumptionKind::OpenPart
                | AssumptionKind::SubstituteModel
                | AssumptionKind::NotChecked
                | AssumptionKind::NotExercised
                | AssumptionKind::FittedByDefault => EvidenceStatus::Undermined,
                AssumptionKind::InferredPinRole
                | AssumptionKind::DefaultParameter
                | AssumptionKind::ReducedFidelity
                | AssumptionKind::ParserLimitation => EvidenceStatus::Qualified,
                AssumptionKind::Waived => {
                    // A waiver in force qualifies the conclusion; a lapsed one
                    // stops covering anything, matching what the waiver gate
                    // already does with an expired entry. An expiry that will
                    // not parse counts as lapsed: an unreadable date cannot
                    // vouch for a finding.
                    match a.expires.as_deref().and_then(parse_ymd_epoch_days) {
                        Some(day) if today.is_covered_by(day) => EvidenceStatus::Qualified,
                        _ => EvidenceStatus::Undermined,
                    }
                }
            };
            status = status.max(effect);
        }
        status
    }

    /// The derived status. There is no setter, by design.
    pub fn status(&self) -> EvidenceStatus {
        self.status
    }

    /// How good the numbers behind this assertion are, when the producing path
    /// measured them.
    pub fn error_budget(&self) -> Option<&ErrorBudget> {
        self.error_budget.as_ref()
    }

    /// Statistical coverage wording, for an ensemble assertion.
    pub fn coverage(&self) -> Option<&str> {
        self.coverage.as_deref()
    }

    /// The on-path assumption ids, deduped, in the order the traversal found
    /// them. The traversal owes that order determinism: a set-iteration order
    /// here would make the JSON, the human output and every golden file
    /// nondeterministic.
    pub fn assumptions(&self) -> &[AssumptionId] {
        &self.assumptions
    }

    /// True when this assertion is not entitled to a verdict: the caller maps
    /// this onto the run's invalid-for-analysis outcome.
    pub fn is_undermined(&self) -> bool {
        self.status == EvidenceStatus::Undermined
    }

    /// Attach the causal artifact indices.
    pub fn with_artifacts<I>(
        mut self,
        registry: &EvidenceRegistry,
        artifacts: I,
    ) -> Result<Self, EvidenceError>
    where
        I: IntoIterator<Item = ArtifactId>,
    {
        let mut validated = Vec::new();
        for artifact in artifacts {
            if artifact.0 >= registry.artifacts().len() {
                return Err(EvidenceError::MissingArtifact {
                    index: artifact.0,
                    len: registry.artifacts().len(),
                });
            }
            if !validated.contains(&artifact) {
                validated.push(artifact);
            }
        }
        self.artifacts = validated;
        Ok(self)
    }

    pub fn artifacts(&self) -> &[ArtifactId] {
        &self.artifacts
    }

    pub fn models(&self) -> &[ModelOnPath] {
        &self.models
    }

    pub fn parameters(&self) -> &[ParameterProvenance] {
        &self.parameters
    }

    /// Attach the models on the causal path.
    pub fn with_models(mut self, models: Vec<ModelOnPath>) -> Self {
        self.models = models;
        self
    }

    /// Attach the parameters read on the causal path.
    pub fn with_parameters(
        mut self,
        registry: &EvidenceRegistry,
        parameters: Vec<ParameterProvenance>,
    ) -> Result<Self, EvidenceError> {
        let known_assumptions: HashSet<&AssumptionId> =
            registry.assumptions().iter().map(Assumption::id).collect();
        for parameter in &parameters {
            match parameter.origin() {
                ValueOrigin::Artifact { index, .. } if index.0 >= registry.artifacts().len() => {
                    return Err(EvidenceError::MissingArtifact {
                        index: index.0,
                        len: registry.artifacts().len(),
                    });
                }
                ValueOrigin::Default { assumption } if !known_assumptions.contains(assumption) => {
                    return Err(EvidenceError::MissingAssumption {
                        id: assumption.to_string(),
                    });
                }
                _ => {}
            }
        }
        self.parameters = parameters;
        Ok(self)
    }

    /// Attach the validated error budget for this assertion's numbers. Every
    /// invariant-bearing member has private fields and a fallible constructor,
    /// so malformed numeric evidence cannot reach this method or disappear on
    /// the way to JSON.
    pub fn with_error_budget(mut self, budget: ErrorBudget) -> Self {
        self.error_budget = Some(budget);
        self
    }

    /// Attach ensemble coverage wording.
    pub fn with_coverage(mut self, coverage: impl Into<String>) -> Self {
        self.coverage = Some(coverage.into());
        self
    }
}

#[cfg(test)]
#[path = "evidence/tests.rs"]
mod tests;
