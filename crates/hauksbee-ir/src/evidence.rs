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
//! What no type can police is which assumptions a producer hands over: an
//! `EvidenceMap::new(assertion, &[], today)` over an undermined board compiles
//! from anywhere and is `Clean`. That is not a hole to be plugged here, it is the
//! reason the discrimination fixture at the bottom of this file exists, and the
//! reason it is the contract the traversal has to satisfy rather than a nicety.
//! The fields that carry no judgement (the artifact indices, the models, the
//! parameters, the budget, the coverage wording) stay public: they are records,
//! and a renderer editing one is a bug rather than a laundered verdict.
//!
//! This module is types and rules only. It computes no causal path: building
//! the net-part incidence that decides which assumptions are on-path belongs to
//! the binder, and the discrimination test at the bottom of this file states, in
//! executable form, what the test beside that traversal has to assert.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/evidence.md

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

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
/// The subject keeps its own case (a reference designator is `R7`, not `r7`);
/// only whitespace and the separator colon are folded, so the kind slug is
/// always everything before the first `:`.
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

    /// Compose an id from a kind and a subject. The subject is folded only
    /// where folding is needed to keep the id parseable: whitespace runs and
    /// `:` become `_`. An empty subject becomes [`Self::UNNAMED_SUBJECT`].
    pub fn new(kind: AssumptionKind, subject: &str) -> Self {
        Self::disambiguated(kind, subject, "")
    }

    /// An id for a subject the producer could not name, disambiguated by the
    /// assumption's own composed statement.
    ///
    /// Two footprints with blank designators are two gaps, and giving them one id
    /// would be worse than giving them an ugly one: the evidence map dedupes by
    /// id, so the second gap would vanish from the report rather than appear
    /// twice. The statement is the only thing that distinguishes them, so an
    /// 8-hex digest of it becomes the disambiguator. It stays deterministic
    /// across runs, because the statement is composed from the same inputs.
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
            format!(
                "Add a model for {reference} to your models directory, or mark it DNP if the \
                 board does not fit it."
            ),
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
    pub fn waived(check: &str, kind: &str, subject: &str, reason: &str, until: &str) -> Self {
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
        // The waiver file requires an expiry, so a missing one means a producer
        // dropped it. Composing a date clause around nothing would put two
        // half-written sentences on the report ("until , so this result is..."),
        // and the status already fails closed here, so say what happened instead.
        let (consequence, replacement) = if until.is_empty() {
            (
                "It carries no expiry, so nothing here can tell whether it is still in force, \
                 and this run treats it as lapsed."
                    .to_string(),
                "Give the waiver an expiry date in the waiver file, or fix the finding."
                    .to_string(),
            )
        } else {
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
        };
        Self::build(
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
        )
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
            if text.contains("  ") || text.contains(" .") || text.contains(" ,") {
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
    Builtin,
    Pack,
    UserDir,
    UserConfigDir,
    ModelsDir,
    Spice,
    EngineFallback,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValueOrigin {
    /// Read from an input artifact, by index into the run inventory.
    Artifact {
        /// Index into the run's `inventory`.
        index: usize,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParameterProvenance {
    /// Reference-qualified, so an evidence map can select by part:
    /// "R7.resistance", "U1.model", "Q2.beta".
    pub parameter: String,
    /// Rendered value ("10k", "TP4056", "1.2e-14 A").
    pub value: String,
    /// Where the value came from.
    pub origin: ValueOrigin,
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

    /// Whether both bounds are real numbers. See [`ErrorBudget::sanitized`] for
    /// why a budget checks this before it is published.
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
    /// The accuracy cost, stated: empty for the primary method, a sentence for
    /// a more dissipative rung.
    #[serde(default, skip_serializing_if = "is_zero")]
    accuracy_cost: f64,
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
    /// Construct one method record. `accuracy_cost` is a non-negative relative
    /// error bound; invalid costs reject the whole producing budget.
    pub fn new(
        window: TimeWindow,
        method: IntegrationMethod,
        accuracy_cost: f64,
    ) -> Result<Self, EvidenceError> {
        non_negative("window_method.accuracy_cost", accuracy_cost)?;
        Ok(Self {
            window,
            method,
            accuracy_cost,
        })
    }

    pub fn window(&self) -> TimeWindow {
        self.window
    }

    pub fn method(&self) -> IntegrationMethod {
        self.method
    }

    pub fn accuracy_cost(&self) -> f64 {
        self.accuracy_cost
    }
}

/// The solver's convergence and truncation-error settings for this result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, schemars::JsonSchema)]
pub struct IntegrationTolerance {
    /// Relative tolerance for Newton convergence and local-truncation-error
    /// control.
    reltol: f64,
    /// Absolute current tolerance, amps.
    abstol: f64,
    /// Charge tolerance, coulombs (capacitor local truncation error).
    chgtol: f64,
}

impl IntegrationTolerance {
    /// Construct strictly-positive, finite solver tolerances.
    pub fn new(reltol: f64, abstol: f64, chgtol: f64) -> Result<Self, EvidenceError> {
        positive("integration_tolerance.reltol", reltol)?;
        positive("integration_tolerance.abstol", abstol)?;
        positive("integration_tolerance.chgtol", chgtol)?;
        Ok(Self {
            reltol,
            abstol,
            chgtol,
        })
    }

    pub fn reltol(self) -> f64 {
        self.reltol
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
    /// `max |f(x)|` over the residual vector at acceptance.
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
/// Both bounds are real numbers. A one-sided datasheet interval ("beta at least
/// 100") has to be expressed with a finite other bound, because an infinity is
/// not JSON: it would serialize as `null` against a schema that says `number`
/// and no consumer could read it back. Whoever lands the producer picks the
/// physical limit and says so in `basis`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ModelUncertainty {
    /// Which parameter carries the interval ("Q2.beta", "D1.vf").
    parameter: String,
    /// Interval low bound.
    low: f64,
    /// Interval high bound.
    high: f64,
    /// Where the interval came from ("datasheet min/max", "pack tolerance").
    basis: String,
}

impl ModelUncertainty {
    /// Construct a finite, ordered uncertainty interval.
    pub fn new(
        parameter: impl Into<String>,
        low: f64,
        high: f64,
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
        Ok(Self {
            parameter,
            low,
            high,
            basis,
        })
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

fn is_zero(value: &f64) -> bool {
    *value == 0.0
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
/// let tolerance = IntegrationTolerance::new(1e-3, 1e-12, 1e-14).unwrap();
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
    /// Which layer of the model ladder it came from: "builtin", "pack",
    /// "user-dir", "user-config-dir", "models-dir", "spice",
    /// "engine-fallback".
    layer: ModelLayer,
    /// How good the match was.
    confidence: MatchConfidence,
}

impl ModelOnPath {
    /// Construct a typed model-path record.
    pub fn new(
        reference: impl Into<String>,
        model_id: impl Into<String>,
        layer: ModelLayer,
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
            layer,
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
                Scope::Check { .. } => false,
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

/// The run's date, for deciding whether a waiver is still in force.
///
/// A newtype rather than a bare `i64` because the two ways of getting it wrong
/// are not equally bad. A date read too late expires waivers early: noisy, and
/// safe. A date read too early re-arms waivers somebody deliberately let lapse,
/// silently, which is the one direction expiry must never fail in. And "too
/// early" is not exotic: it is a zero-initialised field, a container with no RTC
/// before NTP settles, a machine with a dead clock battery. A bare number makes
/// all of those look like a valid Thursday in 1970.
///
/// So a reading before [`RunDate::EARLIEST_CREDIBLE_DAY`] is not believed, and
/// [`RunDate::unknown`] is what a caller with no date passes. Either way every
/// waiver reads as lapsed, which is the fail-closed direction.
///
/// The floor is a constant, set to the day it was last raised, and it does not
/// track builds: the window between it and today is a window in which a broken
/// clock IS believed, and it widens by a day per day until someone raises the
/// constant. That is a maintenance bargain, not a proof, and it is the same
/// bargain the waiver gate's own floor makes
/// (`CLOCK_FLOOR_EPOCH_DAYS` in `crates/hauksbee-engine/src/waiver.rs`, kept at
/// the same date). What the floor does buy is the whole class of obviously wrong
/// readings, which is where broken clocks actually land: zero, a 1970 default, a
/// pre-release date.
///
/// One wiring note, because it decides whether any of this runs at all. Take the
/// clock reading RAW, or use [`RunDate::from_system_clock`]. The waiver gate's
/// `today_epoch_days` CLAMPS a sub-floor reading and returns the floor as a
/// believed date; feeding that here can never produce
/// [`RunDate::unknown`], so the fail-closed path would be dead code and a dead
/// clock battery would re-arm every waiver expiring on or after the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunDate(Option<i64>);

impl RunDate {
    /// Days since the Unix epoch for 2026-07-29, the day this floor was set. A
    /// run cannot honestly be happening before it, so a reading below it is a
    /// broken clock rather than a date.
    ///
    /// Kept equal to the waiver gate's `CLOCK_FLOOR_EPOCH_DAYS`, so the two
    /// answers about a given day agree; note that the gate CLAMPS to its floor
    /// where this REJECTS, which is the difference the type's doc comment warns
    /// about wiring past.
    pub const EARLIEST_CREDIBLE_DAY: i64 = 20_663;

    /// The run's date from the system clock, refusing a reading below
    /// [`Self::EARLIEST_CREDIBLE_DAY`].
    ///
    /// This exists so a caller cannot accidentally launder a broken clock through
    /// a clamping reader on the way here: it takes the raw reading and applies
    /// this type's own rule. A clock before the Unix epoch reads as unknown too,
    /// because a board check is not the place to crash over it.
    pub fn from_system_clock() -> Self {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Self::from_epoch_days((d.as_secs() / 86_400) as i64),
            Err(_) => Self::unknown(),
        }
    }

    /// The run's date, as days since the Unix epoch. A reading below
    /// [`Self::EARLIEST_CREDIBLE_DAY`] is treated as no date at all.
    pub fn from_epoch_days(days: i64) -> Self {
        Self((days >= Self::EARLIEST_CREDIBLE_DAY).then_some(days))
    }

    /// No trustworthy date. Every waiver reads as lapsed.
    pub fn unknown() -> Self {
        Self(None)
    }

    /// True when a waiver expiring on `expiry_epoch_days` still covers this run.
    /// Expiry is end-of-day, the same reading the waiver gate uses: a waiver
    /// dated today is still in force today, which is what someone writing "until
    /// the fab confirms on Friday" means. Unknown dates cover nothing.
    pub fn is_covered_by(self, expiry_epoch_days: i64) -> bool {
        self.0.is_some_and(|today| expiry_epoch_days >= today)
    }

    /// The date as days since the Unix epoch, or `None` when there is none to
    /// believe.
    pub fn epoch_days(self) -> Option<i64> {
        self.0
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
    pub models: Vec<ModelOnPath>,
    /// Parameters read on the causal path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ParameterProvenance>,
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

    /// Attach the models on the causal path.
    pub fn with_models(mut self, models: Vec<ModelOnPath>) -> Self {
        self.models = models;
        self
    }

    /// Attach the parameters read on the causal path.
    pub fn with_parameters(mut self, parameters: Vec<ParameterProvenance>) -> Self {
        self.parameters = parameters;
        self
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

/// Parse `YYYY-MM-DD` into days since the Unix epoch. `None` on anything else,
/// including a well-formed string naming a day that does not exist.
///
/// Public because the waiver gate needs the same reading of the same dates, and
/// two copies of calendar arithmetic in one repo is one too many.
pub fn parse_ymd_epoch_days(s: &str) -> Option<i64> {
    let mut parts = s.trim().split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    // The year bound is not pedantry: `days_from_civil` multiplies the era by
    // 146_097, and an expiry string is user input (a waiver's `until`), so an
    // absurd year would overflow rather than be refused.
    if parts.next().is_some()
        || !(1..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || d < 1
        || d > days_in_month(y, m)
    {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's days-from-civil: calendar date to days since 1970-01-01,
/// with no dependency and no drift.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-01, a fixed run date so no test depends on the wall clock.
    fn today() -> RunDate {
        RunDate::from_epoch_days(20_666)
    }

    fn all_kinds() -> Vec<Assumption> {
        vec![
            Assumption::open_part(
                "R7",
                "10k",
                "no model matched it, and its pins sit on connected nets",
            ),
            Assumption::substitute_model(
                AssumptionSource::Scheduler,
                "U1",
                "ATmega328PB",
                "atmega328p",
            ),
            Assumption::inferred_pin_role("U2", "3", "output"),
            Assumption::default_parameter("U2", "vout", "3.3 V"),
            Assumption::fitted_by_default(
                AssumptionSource::Reader,
                Subject::new("odbpp", "the ODB++ archive"),
                part_scope("R7"),
            ),
            Assumption::not_checked(
                AssumptionSource::Reader,
                "drc",
                None,
                "this input class carries no copper geometry",
                "supply a layout so the check has copper to read",
            ),
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::new("i2c0", "the i2c0 bus"),
                Scope::Nets(NetScope::new(["SDA", "SCL"], None).unwrap()),
                "the MCU backend models no I2C controller on this platform",
                "run on a platform whose backend models the controller",
            ),
            Assumption::reduced_fidelity(
                AssumptionSource::Scheduler,
                Subject::new("spi0/framing", "SPI transaction framing on spi0"),
                Scope::Nets(NetScope::new(["SCK"], None).unwrap()),
                "the chunk-boundary heuristic",
                "expose the chip-select GPIO so framing reads real edges",
            ),
            Assumption::parser_limitation(
                AssumptionSource::Reader,
                Subject::new("drc/short", "shorts on this board"),
                Scope::Check {
                    check: "drc".into(),
                    kind: Some("short".into()),
                },
                "the file was written by a newer KiCad than this reader models",
                "re-export the board from the KiCad version this build supports",
            ),
            Assumption::waived(
                "si",
                "controlled_impedance",
                "DDR_CLK",
                "the fab confirmed the stackup by email",
                "2027-06-01",
            ),
        ]
    }

    // ── ids and sentences ────────────────────────────────────────────────

    #[test]
    fn ids_are_deterministic_and_match_the_documented_shapes() {
        assert_eq!(
            Assumption::open_part("R7", "10k", "").id.as_str(),
            "open-part:R7"
        );
        assert_eq!(
            Assumption::not_checked(
                AssumptionSource::Reader,
                "drc",
                None,
                "no copper",
                "add a layout"
            )
            .id
            .as_str(),
            "not-checked:drc"
        );
        // A check that could not run ONE of its rules names the rule, so the
        // traversal can scope it to the assertions that rely on that rule.
        assert_eq!(
            Assumption::not_checked(
                AssumptionSource::Reader,
                "drc",
                Some("short"),
                "the reader models a different file version",
                "re-export the board"
            )
            .id
            .as_str(),
            "not-checked:drc/short"
        );
        assert_eq!(
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::same("i2c0/U4"),
                Scope::Board,
                "never addressed",
                "exercise it"
            )
            .id
            .as_str(),
            "not-exercised:i2c0/U4"
        );
        assert_eq!(
            Assumption::waived("si", "controlled_impedance", "DDR_CLK", "why", "2027-06-01")
                .id
                .as_str(),
            "waived:si/controlled_impedance/DDR_CLK"
        );
        // Same inputs, same id, twice: what makes an acknowledgment file and a
        // cross-run diff able to name one assumption.
        assert_eq!(
            Assumption::open_part("R7", "10k", "no model matched").id,
            Assumption::open_part("R7", "10k", "no model matched").id
        );
        // Whitespace and colons fold so the kind slug is always everything
        // before the first colon.
        assert_eq!(
            AssumptionId::new(AssumptionKind::NotChecked, "spec check: rails").as_str(),
            "not-checked:spec%20check%3A%20rails"
        );
    }

    #[test]
    fn every_constructor_composes_four_sentences_and_validates() {
        for a in all_kinds() {
            println!(
                "[{}] {}\n  because: {}\n  consequence: {}\n  fix: {}\n",
                a.id, a.statement, a.because, a.consequence, a.replacement
            );
            a.validate().unwrap_or_else(|e| panic!("{e}"));
            for (name, text) in [
                ("statement", &a.statement),
                ("because", &a.because),
                ("consequence", &a.consequence),
                ("replacement", &a.replacement),
            ] {
                assert!(
                    text.ends_with('.'),
                    "{}: {name} is not a sentence: {text:?}",
                    a.id
                );
                assert!(
                    !text.contains(".."),
                    "{}: {name} double stop: {text:?}",
                    a.id
                );
                // A sentence opens with a capital, unless it opens with a
                // case-sensitive identifier a producer handed over, which must
                // be spelled the way the board spells it.
                assert!(
                    !text.starts_with(|c: char| c.is_ascii_lowercase())
                        || !text
                            .split_whitespace()
                            .next()
                            .unwrap()
                            .chars()
                            .all(|c| c.is_ascii_lowercase()),
                    "{}: {name} does not open a sentence: {text:?}",
                    a.id
                );
                assert!(
                    !text.to_lowercase().contains("n/a"),
                    "{}: {name} is a placeholder, not an answer",
                    a.id
                );
            }
        }
    }

    #[test]
    fn only_waived_carries_an_expiry() {
        for a in all_kinds() {
            match a.kind {
                AssumptionKind::Waived => assert_eq!(a.expires.as_deref(), Some("2027-06-01")),
                _ => assert!(a.expires.is_none(), "{} carries an expiry", a.id),
            }
        }
        // The two malformed shapes are construction errors, not warnings.
        let mut open = Assumption::open_part("R7", "10k", "");
        open.expires = Some("2027-06-01".into());
        assert!(open.validate().is_err());
        let mut waived = Assumption::waived("si", "k", "N1", "why", "2027-06-01");
        waived.expires = None;
        assert!(waived.validate().is_err());
    }

    #[test]
    fn case_sensitive_subjects_are_not_recapitalized() {
        // A bus called spi0 is not called Spi0, and the Altium property is
        // `value_unresolved`, not `Value_unresolved`. Spelling the same subject
        // two ways is the exact drift this module exists to stop, so it must not
        // do it inside one assumption.
        let a = Assumption::reduced_fidelity(
            AssumptionSource::Scheduler,
            Subject::same("spi0 framing"),
            Scope::Board,
            "the chunk-boundary heuristic",
            "expose the chip-select GPIO",
        );
        assert!(a.statement.contains("spi0 framing"), "{}", a.statement);
        assert!(!a.statement.contains("Spi0"), "{}", a.statement);
        let b = Assumption::open_part("R7", "10k", "value_unresolved: no value in the source");
        assert!(b.because.starts_with("value_unresolved:"), "{}", b.because);
    }

    /// An id with no subject names nothing: an acknowledgment file could not name
    /// it and a diff could not track it. No constructor can produce one, because
    /// a board legitimately carries an unnamed net or a blank designator and a
    /// gap on one of those is still a gap: it gets named as unnamed rather than
    /// crashing a reader or shipping an unciteable id.
    #[test]
    fn no_constructor_can_produce_an_unciteable_id() {
        let from_nothing = [
            Assumption::open_part("", "", ""),
            Assumption::substitute_model(AssumptionSource::Binder, "", "", ""),
            Assumption::inferred_pin_role("", "", ""),
            Assumption::default_parameter("", "", ""),
            Assumption::not_checked(AssumptionSource::Reader, "", None, "", ""),
            Assumption::held_by_ideal_source(""),
            Assumption::fitted_by_default(
                AssumptionSource::Reader,
                Subject::same(""),
                Scope::Board,
            ),
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::same(""),
                Scope::Board,
                "",
                "",
            ),
            Assumption::waived("", "", "", "", "2027-06-01"),
        ];
        for a in from_nothing {
            a.validate().unwrap_or_else(|e| panic!("{e}"));
            let subject = a.id().as_str().split_once(':').unwrap().1;
            assert!(!subject.is_empty(), "{} names no subject", a.id());
        }
    }

    /// Missing DATA is a different thing from a producer bug, and must not be
    /// either a panic or a hole. Reasons, bus names and part numbers are lifted
    /// out of real files; real files sometimes do not carry them. Every sentence
    /// still has to be a sentence, in release as much as in debug, because in
    /// release `build`'s assertion is compiled out and nothing else is looking.
    #[test]
    fn a_missing_datum_falls_back_rather_than_leaving_a_hole() {
        let thin = [
            Assumption::open_part("R7", "", ""),
            Assumption::substitute_model(AssumptionSource::Binder, "U1", "", ""),
            Assumption::inferred_pin_role("U2", "", ""),
            Assumption::default_parameter("U2", "", ""),
            Assumption::fitted_by_default(
                AssumptionSource::Reader,
                Subject::new("odbpp", ""),
                Scope::Board,
            ),
            Assumption::not_checked(AssumptionSource::Reader, "drc", None, "", ""),
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::new("i2c0", ""),
                Scope::Board,
                "",
                "",
            ),
            Assumption::reduced_fidelity(
                AssumptionSource::Solver,
                Subject::new("spi0", ""),
                Scope::Board,
                "",
                "",
            ),
            Assumption::parser_limitation(
                AssumptionSource::Reader,
                Subject::new("drc/short", ""),
                Scope::Board,
                "",
                "",
            ),
            Assumption::waived("si", "k", "DDR_CLK", "", "2027-06-01"),
        ];
        for a in thin {
            a.validate().unwrap_or_else(|e| panic!("{e}"));
            for (name, text) in [
                ("statement", a.statement()),
                ("because", a.because()),
                ("consequence", a.consequence()),
                ("replacement", a.replacement()),
            ] {
                assert!(text.len() > 10, "{}: {name} is a stub: {text:?}", a.id());
            }
        }
        // And whitespace inside a datum is tidied rather than refused: a reason
        // lifted from a file arrives with whatever spacing the file had.
        let a = Assumption::open_part("R7", "10k", "no  model   matched");
        assert_eq!(a.because(), "No model matched.");
    }

    #[test]
    fn validate_catches_a_mismatched_id_slug() {
        // The id and the kind must name the same thing, or the status rule and
        // the rendered id describe different gaps. Reachable only by hand here,
        // since the constructors compose the id from the kind.
        let mut a = Assumption::open_part("R7", "10k", "no model matched");
        a.id = AssumptionId(format!("{}:R7", AssumptionKind::ReducedFidelity.slug()));
        assert!(a.validate().is_err());
        // And an id naming no subject: an acknowledgment file could not name it
        // and a diff could not track it.
        let mut a = Assumption::open_part("R7", "10k", "no model matched");
        a.id = AssumptionId(format!("{}:", AssumptionKind::OpenPart.slug()));
        assert!(a.validate().is_err());
    }

    #[test]
    fn an_absurd_expiry_is_refused_rather_than_overflowing() {
        // `until` is user input, and the civil-date arithmetic multiplies by
        // 146_097, so an absurd year has to be refused before it is computed.
        assert_eq!(parse_ymd_epoch_days("9223372036854775807-03-01"), None);
        assert_eq!(parse_ymd_epoch_days("99999999999999-12-31"), None);
        assert_eq!(parse_ymd_epoch_days("2026-02-30"), None);
        assert_eq!(parse_ymd_epoch_days("2026-13-01"), None);
        assert_eq!(parse_ymd_epoch_days("2027-06-01"), Some(20_970));
    }

    // ── the status rule table ────────────────────────────────────────────

    /// Every declared kind has a constructor and a row in the test table, driven
    /// off the enum itself so a new variant fails mechanically rather than
    /// waiting for someone to remember.
    #[test]
    fn every_kind_has_a_constructor() {
        use strum::IntoEnumIterator;
        let built: Vec<AssumptionKind> = all_kinds().iter().map(|a| a.kind).collect();
        for kind in AssumptionKind::iter() {
            assert!(
                built.contains(&kind),
                "{kind:?} has no constructor exercised in all_kinds()"
            );
        }
        assert_eq!(built.len(), AssumptionKind::iter().count());
    }

    #[test]
    fn duplicate_on_path_assumptions_are_listed_once() {
        // A traversal walking several nets that share a part hands the same
        // assumption over twice; rendering it twice is noise.
        let a = Assumption::open_part("R7", "10k", "no model matched");
        let map = EvidenceMap::new("A", &[a.clone(), a], today());
        assert_eq!(map.assumptions().len(), 1);
        assert_eq!(map.status(), EvidenceStatus::Undermined);
    }

    /// One row per line of the table in [`EvidenceMap::derive_status`], because
    /// the table IS the policy: nothing else in the tree decides whether a
    /// conclusion is entitled to a verdict.
    #[test]
    fn status_rule_covers_every_kind() {
        let expected = [
            // In `all_kinds()` order, which is the order the kinds are
            // declared, so a new variant with no row here fails the length
            // assertion below rather than passing silently.
            (AssumptionKind::OpenPart, EvidenceStatus::Undermined),
            (AssumptionKind::SubstituteModel, EvidenceStatus::Undermined),
            (AssumptionKind::InferredPinRole, EvidenceStatus::Qualified),
            (AssumptionKind::DefaultParameter, EvidenceStatus::Qualified),
            (AssumptionKind::FittedByDefault, EvidenceStatus::Undermined),
            (AssumptionKind::NotChecked, EvidenceStatus::Undermined),
            (AssumptionKind::NotExercised, EvidenceStatus::Undermined),
            (AssumptionKind::ReducedFidelity, EvidenceStatus::Qualified),
            (AssumptionKind::ParserLimitation, EvidenceStatus::Qualified),
            (AssumptionKind::Waived, EvidenceStatus::Qualified),
        ];
        let built = all_kinds();
        assert_eq!(built.len(), expected.len(), "a kind lost its constructor");
        for (a, (kind, want)) in built.iter().zip(expected) {
            assert_eq!(a.kind, kind, "constructor order drifted from the table");
            let map = EvidenceMap::new("A", std::slice::from_ref(a), today());
            assert_eq!(map.status(), want, "{} should be {want:?} on its own", a.id);
        }
    }

    #[test]
    fn two_gaps_on_unnameable_subjects_stay_two_gaps() {
        // Two footprints with blank designators are two gaps. Giving them one id
        // would be worse than giving them an ugly one, because the evidence map
        // dedupes by id and the second gap would vanish from the report rather
        // than appear twice. The statement is the only thing that tells them
        // apart, so it disambiguates the id, deterministically.
        let a = Assumption::open_part("", "10k", "no model matched");
        let b = Assumption::open_part("", "47k", "no model matched");
        assert_ne!(a.id(), b.id(), "{} == {}", a.id(), b.id());
        assert!(a.id().as_str().starts_with("open-part:unnamed-"));
        // Deterministic: the same board yields the same id next run, which is
        // what makes an id citeable at all.
        assert_eq!(
            a.id(),
            Assumption::open_part("", "10k", "no model matched").id()
        );
        let map = EvidenceMap::new("A", &[a, b], today());
        assert_eq!(map.assumptions().len(), 2, "a real gap went missing");
        // And a NAMED subject never carries prose: an id is a contract, and
        // "open-part:an_unnamed_part" would be neither citeable nor unique.
        assert_eq!(
            Assumption::open_part("R7", "10k", "").id().as_str(),
            "open-part:R7"
        );
        // EVERY constructor, because one that substitutes prose or a bare
        // sentinel before the id is composed is one that collides two gaps onto
        // one entry, and the dedupe then eats one. That is the failure this whole
        // scheme exists to prevent, so the coverage is exhaustive rather than
        // representative.
        let nameless: Vec<(Assumption, Assumption)> = vec![
            (
                Assumption::open_part("", "10k", "no model matched"),
                Assumption::open_part("", "47k", "no model matched"),
            ),
            (
                Assumption::substitute_model(AssumptionSource::Binder, "", "a", "b"),
                Assumption::substitute_model(AssumptionSource::Binder, "", "c", "d"),
            ),
            (
                Assumption::inferred_pin_role("", "", "output"),
                Assumption::inferred_pin_role("", "", "input"),
            ),
            (
                Assumption::default_parameter("", "", "3.3 V"),
                Assumption::default_parameter("", "", "5 V"),
            ),
            (
                Assumption::fitted_by_default(
                    AssumptionSource::Reader,
                    Subject::same(""),
                    Scope::Board,
                ),
                Assumption::fitted_by_default(
                    AssumptionSource::Reader,
                    Subject::new("", "the second archive"),
                    Scope::Board,
                ),
            ),
            (
                Assumption::not_checked(AssumptionSource::Reader, "", None, "no copper", "add one"),
                Assumption::not_checked(
                    AssumptionSource::Reader,
                    "",
                    None,
                    "no firmware",
                    "supply one",
                ),
            ),
            (
                Assumption::not_exercised(
                    AssumptionSource::Scheduler,
                    Subject::same(""),
                    Scope::Board,
                    "a",
                    "b",
                ),
                Assumption::not_exercised(
                    AssumptionSource::Scheduler,
                    Subject::new("", "the second bus"),
                    Scope::Board,
                    "c",
                    "d",
                ),
            ),
            (
                Assumption::reduced_fidelity(
                    AssumptionSource::Solver,
                    Subject::same(""),
                    Scope::Board,
                    "a",
                    "b",
                ),
                Assumption::reduced_fidelity(
                    AssumptionSource::Solver,
                    Subject::new("", "the second span"),
                    Scope::Board,
                    "c",
                    "d",
                ),
            ),
            (
                Assumption::held_by_ideal_source(""),
                Assumption::held_by_ideal_source("   "),
            ),
            (
                Assumption::parser_limitation(
                    AssumptionSource::Reader,
                    Subject::same(""),
                    Scope::Board,
                    "a",
                    "b",
                ),
                Assumption::parser_limitation(
                    AssumptionSource::Reader,
                    Subject::new("", "the second finding"),
                    Scope::Board,
                    "c",
                    "d",
                ),
            ),
            (
                Assumption::waived("", "", "", "why", "2027-06-01"),
                Assumption::waived("", "", "", "another why", "2027-06-01"),
            ),
        ];
        for (first, second) in nameless {
            let subject = first.id().as_str().split_once(':').unwrap().1;
            assert!(
                subject.starts_with("unnamed-"),
                "{} does not go through the disambiguator",
                first.id()
            );
            // Distinct statements, distinct ids, both surviving the map.
            if first.statement() != second.statement() {
                assert_ne!(
                    first.id(),
                    second.id(),
                    "two gaps collided on {}",
                    first.id()
                );
                let map = EvidenceMap::new("A", &[first, second], today());
                assert_eq!(map.assumptions().len(), 2, "a real gap went missing");
            }
        }
    }

    #[test]
    fn a_waiver_that_lost_its_expiry_says_so_instead_of_leaving_a_gap() {
        // The waiver file requires an expiry and its own loader refuses anything
        // else, so a missing one here is a producer bug. Composing a date clause
        // around nothing would put "until , so this result is..." on the report,
        // which is a half-written sentence rather than an obvious bug.
        let a = Assumption::waived("si", "controlled_impedance", "DDR_CLK", "why", "");
        assert!(
            a.consequence().contains("carries no expiry"),
            "{}",
            a.consequence()
        );
        assert!(!a.consequence().contains(" ,"));
        assert!(!a.replacement().contains(" ,"));
        // Well formed: whether the date PARSES is the waiver file loader's rule,
        // not this crate's, and the status rule is where an unreadable one is
        // handled.
        a.validate().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&a), today()),
            EvidenceStatus::Undermined
        );
    }

    #[test]
    fn a_scope_window_that_is_not_numbers_is_refused() {
        assert!(matches!(
            TimeWindow::new(0.0, f64::NAN),
            Err(EvidenceError::NonFinite { .. })
        ));
        let ok = Assumption::not_exercised(
            AssumptionSource::Solver,
            Subject::same("the settling window"),
            Scope::Nets(NetScope::new(["3V3"], Some(TimeWindow::new(0.0, 0.5).unwrap())).unwrap()),
            "the solve never reached it",
            "extend the run",
        );
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn the_wire_vocabulary_is_spelled_in_exactly_one_place() {
        // One surface downstream hand-serializes its JSON instead of going
        // through serde, so the strings have to be reachable as strings, and they
        // have to be the same strings serde writes.
        for status in [
            EvidenceStatus::Clean,
            EvidenceStatus::Qualified,
            EvidenceStatus::Undermined,
        ] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().to_string())
            );
            assert_eq!(status.to_string(), status.as_str());
        }
        use strum::IntoEnumIterator;
        for kind in AssumptionKind::iter() {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.as_str().to_string())
            );
        }
    }

    #[test]
    fn no_assumptions_is_clean() {
        let map = EvidenceMap::new("A", &[], today());
        assert_eq!(map.status(), EvidenceStatus::Clean);
        assert!(map.assumptions().is_empty());
        assert!(!map.is_undermined());
    }

    #[test]
    fn one_undermining_assumption_beats_any_number_of_qualifiers() {
        let mut set: Vec<Assumption> = all_kinds()
            .into_iter()
            .filter(|a| {
                EvidenceMap::derive_status(std::slice::from_ref(a), today())
                    == EvidenceStatus::Qualified
            })
            .collect();
        assert_eq!(
            EvidenceMap::derive_status(&set, today()),
            EvidenceStatus::Qualified
        );
        set.push(Assumption::open_part("R7", "10k", ""));
        assert_eq!(
            EvidenceMap::derive_status(&set, today()),
            EvidenceStatus::Undermined,
            "a pile of caveats must not dilute one undermining assumption"
        );
    }

    #[test]
    fn a_lapsed_waiver_stops_covering() {
        let w = Assumption::waived("si", "k", "DDR_CLK", "fab confirmed", "2027-06-01");
        let expiry = parse_ymd_epoch_days("2027-06-01").unwrap();
        // In force up to and including the expiry date (end-of-day expiry, the
        // reading the waiver gate already uses).
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&w), RunDate::from_epoch_days(expiry)),
            EvidenceStatus::Qualified
        );
        assert_eq!(
            EvidenceMap::derive_status(
                std::slice::from_ref(&w),
                RunDate::from_epoch_days(expiry + 1)
            ),
            EvidenceStatus::Undermined
        );
        // An unreadable or absent expiry fails CLOSED: a date that cannot be
        // read cannot vouch for a finding.
        let mut broken = w.clone();
        broken.expires = Some("next Friday".into());
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&broken), today()),
            EvidenceStatus::Undermined
        );
        broken.expires = None;
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&broken), today()),
            EvidenceStatus::Undermined
        );
        // A caller with no date at all gets the same fail-closed reading.
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&w), RunDate::unknown()),
            EvidenceStatus::Undermined
        );
    }

    #[test]
    fn a_clock_reading_from_before_this_build_is_not_believed() {
        // The asymmetry that makes RunDate a type: a date read LATE only expires
        // waivers early, but a date read early re-arms every lapsed one, which is
        // the direction expiry must never fail in. A zero-initialised field, a
        // container with no RTC, a dead clock battery: a bare number makes all of
        // them look like a valid Thursday in 1970.
        let lapsed = Assumption::waived("si", "k", "DDR_CLK", "fab confirmed", "2001-01-01");
        for broken in [0, 1, RunDate::EARLIEST_CREDIBLE_DAY - 1, i64::MIN] {
            assert_eq!(
                RunDate::from_epoch_days(broken).epoch_days(),
                None,
                "{broken} is not a credible run date"
            );
            assert_eq!(
                EvidenceMap::derive_status(
                    std::slice::from_ref(&lapsed),
                    RunDate::from_epoch_days(broken)
                ),
                EvidenceStatus::Undermined,
                "a broken clock must not resurrect a waiver that lapsed in 2001"
            );
        }
        // A credible reading is believed, and the floor itself is credible.
        assert_eq!(
            RunDate::from_epoch_days(RunDate::EARLIEST_CREDIBLE_DAY).epoch_days(),
            Some(RunDate::EARLIEST_CREDIBLE_DAY)
        );
        assert_eq!(
            parse_ymd_epoch_days("2026-07-29"),
            Some(RunDate::EARLIEST_CREDIBLE_DAY),
            "the floor is the date its doc comment claims"
        );
    }

    // ── forgery ─────────────────────────────────────────────────────────

    /// A judgement is produced, never parsed. `Assumption` and `EvidenceMap` are
    /// serialize-only for exactly this reason: a `Deserialize` on either is a
    /// minting route that needs no constructor. Eight lines of JSON would
    /// otherwise buy an assumption with any kind and any wording, and a map with
    /// its `assumptions` key deleted would read back `Clean` over a real gap,
    /// which is the hiding the whole spine exists to prevent.
    ///
    /// This test is the guard on that decision. `serde_json::from_value` for
    /// either type does not compile today, and if someone adds the derive to make
    /// a consumer's life easier, this stops passing.
    #[test]
    fn the_judgement_types_are_serialize_only() {
        // The record types read back: they are facts about what happened, and
        // parsing one cannot launder a verdict.
        fn round_trips<T>(value: T)
        where
            T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        {
            let text = serde_json::to_string(&value).expect("serializes");
            let back: T = serde_json::from_str(&text).expect("deserializes");
            assert_eq!(back, value);
        }
        round_trips(ParameterProvenance {
            parameter: "R7.resistance".into(),
            value: "10k".into(),
            origin: ValueOrigin::Artifact {
                index: 0,
                field: "value".into(),
            },
        });
        // The judgements do not. Written as a source-text assertion because a
        // negative trait bound is not expressible: the derives are in this file,
        // so the file can check itself.
        let src = include_str!("evidence.rs");
        for judgement in ["pub struct Assumption {", "pub struct EvidenceMap {"] {
            let at = src.find(judgement).expect("the type is declared here");
            // The whole attribute block, however long it grows: everything from
            // the last blank line before the declaration. A fixed byte window
            // would slide off the `#[derive(...)]` line the first time someone
            // lengthens a doc comment, and the guard would go quiet in the same
            // edit that made it necessary.
            let block: String = src[..at]
                .rsplit_once("\n\n")
                .map(|(_, tail)| tail)
                .unwrap_or(&src[..at])
                .lines()
                // Attribute lines only: the doc comment above these types
                // discusses `Deserialize` in prose, and a guard that reads the
                // prose is a guard that fires on its own explanation.
                .filter(|l| l.trim_start().starts_with("#["))
                .collect();
            assert!(
                block.contains("#[derive("),
                "{judgement}: the attribute block was not found, so this guard is not \
                 guarding anything"
            );
            assert!(
                !block.contains("Deserialize"),
                "{judgement} gained a Deserialize derive, which is a minting route"
            );
        }
        // A hand-written impl is the other way someone would satisfy a
        // consumer's inconvenience, and it would not touch the derive line. The
        // needle is assembled at runtime so this assertion does not trip over
        // its own source text.
        for ty in ["Assumption", "EvidenceMap"] {
            let needle = format!("Deserialize<'de> for {ty}");
            assert!(
                !src.contains(&needle),
                "a hand-written `impl {needle}` is the same minting route"
            );
        }
    }

    #[test]
    fn a_consumer_settles_a_status_by_re_deriving_it_from_the_registry() {
        // The only honest way to check a status: hand the kinds back to the rule.
        // A reader with the run's registry can always do this; a reader with a
        // map alone is holding the producer's word, which is why the map is not
        // parseable on its own.
        let registry = [Assumption::open_part("U2", "XC6206", "no model matched")];
        let map = EvidenceMap::new("3V3 stays above 3.1 V", &registry, today());
        let on_path: Vec<Assumption> = registry
            .iter()
            .filter(|a| map.assumptions().contains(a.id()))
            .cloned()
            .collect();
        assert_eq!(
            EvidenceMap::derive_status(&on_path, today()),
            map.status(),
            "re-deriving from the registry must reproduce the recorded status"
        );
    }

    // ── serde shape ─────────────────────────────────────────────────────

    #[test]
    fn assumption_json_shape_is_the_published_one() {
        let a = Assumption::open_part("R7", "10k", "no model matched");
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["id"], "open-part:R7");
        assert_eq!(v["kind"], "open_part");
        assert_eq!(v["source"], "binder");
        assert_eq!(v["scope"]["type"], "subjects");
        assert_eq!(v["scope"]["value"][0]["kind"], "part");
        assert_eq!(v["scope"]["value"][0]["id"], "R7");
        // No expiry on a run-derived assumption, and the field is absent
        // rather than null: the common shape stays small.
        assert!(v.get("expires").is_none());
        assert_eq!(
            v.as_object().unwrap().keys().count(),
            8,
            "the assumption shape gained or lost a field; the report schema \
             bumps exactly once, in the rendering phase"
        );
    }

    #[test]
    fn evidence_map_json_shape_is_the_published_one() {
        let open = Assumption::open_part("U2", "XC6206", "no model matched");
        let mut registry = EvidenceRegistry::new(vec![open.clone()]).unwrap();
        let artifacts: Vec<ArtifactId> = (0..3)
            .map(|index| {
                registry
                    .add_artifact(
                        ArtifactProvenance::new(
                            format!("board-{index}.kicad_pcb"),
                            ArtifactKind::KiCadPcb,
                            ArtifactRole::Layout,
                            String::new(),
                            Vec::new(),
                        )
                        .unwrap(),
                    )
                    .unwrap()
            })
            .collect();
        let map = EvidenceMap::new("3V3 stays above 3.1 V", &[open], today())
            .with_artifacts(&registry, [artifacts[0], artifacts[2]])
            .unwrap()
            .with_models(vec![ModelOnPath::new(
                "U2",
                "xc6206",
                ModelLayer::Pack,
                MatchConfidence::High,
            )
            .unwrap()])
            .with_error_budget(ErrorBudget {
                methods: vec![WindowMethod {
                    window: TimeWindow {
                        start_s: 0.0,
                        end_s: 0.05,
                    },
                    method: IntegrationMethod::Trapezoidal,
                    accuracy_cost: 0.0,
                }],
                ..ErrorBudget::new(IntegrationTolerance {
                    reltol: 1e-3,
                    abstol: 1e-12,
                    chgtol: 1e-14,
                })
            });
        let v = serde_json::to_value(&map).unwrap();
        assert_eq!(v["assertion"], "3V3 stays above 3.1 V");
        assert_eq!(v["artifacts"][1], 2);
        assert_eq!(v["assumptions"][0], "open-part:U2");
        assert_eq!(v["status"], "undermined");
        assert_eq!(v["error_budget"]["tolerance"]["reltol"], 1e-3);
        assert_eq!(v["error_budget"]["methods"][0]["method"], "trapezoidal");
        assert!(v["error_budget"].get("residual").is_none());
        assert!(v.get("parameters").is_none());
        assert!(v.get("coverage").is_none());
    }

    #[test]
    fn provenance_and_origin_shapes() {
        let p = ParameterProvenance {
            parameter: "U2.vout".into(),
            value: "3.3 V".into(),
            origin: ValueOrigin::Model {
                model_id: "xc6206".into(),
                layer: ModelLayer::Pack,
                confidence: MatchConfidence::Exact,
            },
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["origin"]["type"], "model");
        assert_eq!(v["origin"]["layer"], "pack");
        let art = ArtifactProvenance {
            path: "boards/blinky.kicad_pcb".into(),
            kind: ArtifactKind::KiCadPcb,
            role: ArtifactRole::Layout,
            sha256: String::new(),
            contributed: vec![Contribution {
                what: "connectivity".into(),
                detail: "nets read from the file's net table".into(),
            }],
            ignored: vec![IgnoredInput {
                what: "F.SilkS".into(),
                why: "board artwork rather than parts".into(),
            }],
            cross_checks: vec![CrossCheck {
                what: "netlist against copper".into(),
                agreed: true,
                detail: "both reported 41 nets".into(),
            }],
            assumptions: vec![AssumptionId::new(
                AssumptionKind::FittedByDefault,
                "kicad_pcb",
            )],
        };
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["role"], "layout");
        assert!(v.get("sha256").is_none(), "an empty hash is omitted");
        assert_eq!(v["assumptions"][0], "fitted-by-default:kicad_pcb");
    }

    #[test]
    fn schemas_generate_with_the_status_vocabulary() {
        let schema = schemars::schema_for!(EvidenceMap);
        let text = serde_json::to_string(&schema).unwrap();
        for want in ["clean", "qualified", "undermined", "assumptions", "status"] {
            assert!(text.contains(want), "schema is missing {want}");
        }
        // Every type that reaches a JSON surface must have a schema, because
        // the report schema has a drift test.
        serde_json::to_string(&schemars::schema_for!(Assumption)).unwrap();
        serde_json::to_string(&schemars::schema_for!(ArtifactProvenance)).unwrap();
        serde_json::to_string(&schemars::schema_for!(ParameterProvenance)).unwrap();
        serde_json::to_string(&schemars::schema_for!(ErrorBudget)).unwrap();
    }

    #[test]
    fn the_published_schema_does_not_permit_a_null_the_writers_never_write() {
        // An optional field here is an ABSENT KEY, never `null`: the whole
        // non-finite discipline exists because a `null` cannot be read back
        // against a `number`. A schema that permits a third encoding is a schema
        // this module's own writers would fail, and narrowing it after
        // publication breaks every validating consumer, so it is pinned now.
        for (name, schema) in [
            ("EvidenceMap", schemars::schema_for!(EvidenceMap)),
            ("ErrorBudget", schemars::schema_for!(ErrorBudget)),
            ("Assumption", schemars::schema_for!(Assumption)),
            ("Scope", schemars::schema_for!(Scope)),
        ] {
            let text = serde_json::to_string(&schema).unwrap();
            assert!(
                !text.contains("\"null\""),
                "{name}'s schema permits null: {text}"
            );
        }
        // Optional fields stay optional, though: absence is the encoding.
        let map = serde_json::to_value(schemars::schema_for!(EvidenceMap)).unwrap();
        let required: Vec<&str> = map["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["assertion", "status"]);
    }

    // ── the discrimination fixture ──────────────────────────────────────

    /// THE CONTRACT FOR THE CAUSAL-PATH TRAVERSAL, written before any
    /// traversal or renderer exists.
    ///
    /// The failure mode this exists to catch is the evidence map degenerating,
    /// in either of two mirror-image ways, both of which pass every
    /// type-level test in this file while reopening the honesty gap the spine
    /// was built to close:
    ///
    /// - **Saturated.** Every assumption gets attached to every assertion,
    ///   because "attach the whole registry" is the path of least resistance
    ///   once recording real net-part incidence gets awkward. Every assertion
    ///   then renders undermined, users learn to scroll past the block, and
    ///   the signal dies in noise.
    /// - **Vacuous.** The traversal quietly returns nothing, every assertion
    ///   renders `Clean`, and the new vocabulary certifies the very silence it
    ///   was built to end, now with more authority.
    ///
    /// The incidence below is built BY HAND, and `on_path_for` is a stand-in for
    /// a traversal that does not exist yet, because the real one needs the
    /// binder's net-part incidence and the binder lives in another crate that
    /// this one cannot depend on. So this test cannot itself fail when the real
    /// traversal degenerates. What it is instead is the SPECIFICATION of the test
    /// that must exist beside that traversal, in the engine, asserting the same
    /// two halves against real incidence: an unresolved part on assertion A's net
    /// undermines A and names the part, and assertion B on an unreachable net
    /// stays clean and empty. A vacuous traversal fails the first half; a
    /// saturated one fails the second. Whoever writes the traversal owes that
    /// test, and this one says exactly what it has to assert.
    #[test]
    fn an_unresolved_part_undermines_its_own_net_and_only_its_own_net() {
        // The fixture board, as the binder will eventually report it: net ->
        // the parts incident on it. X is the regulator feeding 3V3 and did not
        // resolve; the parts on VBUS are all bound.
        let incidence: &[(&str, &[&str])] =
            &[("3V3", &["X", "C1", "U5"]), ("VBUS", &["J1", "C9", "D2"])];
        let registry = vec![Assumption::open_part("X", "XC6206", "no model matched")];

        // Stand-in for the phase-4 traversal: an assumption is on-path when its
        // scope names a part incident on the assertion's subject nets.
        fn on_path_for(
            subject_nets: &[&str],
            incidence: &[(&str, &[&str])],
            registry: &[Assumption],
        ) -> Vec<Assumption> {
            let reachable: Vec<&str> = incidence
                .iter()
                .filter(|(net, _)| subject_nets.contains(net))
                .flat_map(|(_, refs)| refs.iter().copied())
                .collect();
            registry
                .iter()
                .filter(|a| match &a.scope {
                    Scope::Subjects(subjects) => subjects.as_slice().iter().any(|entity| {
                        entity.kind() == EntityKind::Part && reachable.contains(&entity.id())
                    }),
                    Scope::Parameter(parameter) => {
                        parameter.subject().kind() == EntityKind::Part
                            && reachable.contains(&parameter.subject().id())
                    }
                    Scope::Nets(nets) => nets
                        .nets()
                        .iter()
                        .any(|n| subject_nets.contains(&n.as_str())),
                    // A board-wide gap is on every assertion's path by
                    // definition, which is why a constructor that hardcodes
                    // Scope::Board for an undermining kind saturates a run.
                    Scope::Board => true,
                    // Named explicitly rather than swept into a `_` arm: these
                    // two are NOT membership-by-electrical-reachability, and the
                    // real traversal owes each its own rule and its own test. A
                    // NotChecked assumption is on-path for every assertion that
                    // relies on that check (§2.4's "covering this assertion's
                    // check"), and a TimeWindow one for every assertion whose
                    // observation window overlaps it. Dropping them here, in the
                    // one worked example, is how they would go missing there:
                    // silent, and for the kinds where silence is hardest to
                    // notice.
                    Scope::Check { .. } => false,
                })
                .cloned()
                .collect()
        }

        let a = EvidenceMap::new(
            "3V3 stays above 3.1 V",
            &on_path_for(&["3V3"], incidence, &registry),
            today(),
        );
        let b = EvidenceMap::new(
            "VBUS stays below 5.5 V",
            &on_path_for(&["VBUS"], incidence, &registry),
            today(),
        );

        // Half one: the gap cannot hide behind a board-wide percentage. A is
        // undermined, and it NAMES the part, so the report can say which.
        assert_eq!(a.status(), EvidenceStatus::Undermined);
        assert!(a.is_undermined());
        assert_eq!(a.assumptions(), [AssumptionId("open-part:X".into())]);

        // Half two: and it does not smear over the rest of the board.
        assert_eq!(b.status(), EvidenceStatus::Clean);
        assert!(b.assumptions().is_empty());

        // Half three, the one a caller controls rather than the traversal: a
        // board-scoped gap of an undermining kind is on every assertion's path by
        // definition, so scoping one that way makes a whole run invalid. That is
        // a real answer for a board with no BOM at all, and the wrong answer for
        // a reader that knows which parts are in question. Pinned here because it
        // is the saturated mode arriving as a scope choice rather than as a
        // traversal bug.
        let board_wide = vec![Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::new("odbpp", "the ODB++ archive"),
            Scope::Board,
        )];
        for nets in [&["3V3"], &["VBUS"]] {
            let map = EvidenceMap::new(
                "any assertion",
                &on_path_for(nets, incidence, &board_wide),
                today(),
            );
            assert_eq!(map.status(), EvidenceStatus::Undermined);
            assert_eq!(
                map.assumptions(),
                [AssumptionId("fitted-by-default:odbpp".into())]
            );
        }
        // Scoped to the parts actually in question, it touches only those.
        let scoped = vec![Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::new("odbpp", "the ODB++ archive"),
            part_scope("C1"),
        )];
        assert_eq!(
            EvidenceMap::new("A", &on_path_for(&["3V3"], incidence, &scoped), today()).status(),
            EvidenceStatus::Undermined
        );
        assert_eq!(
            EvidenceMap::new("B", &on_path_for(&["VBUS"], incidence, &scoped), today()).status(),
            EvidenceStatus::Clean
        );
    }

    /// The two degenerate traversals, each shown FAILING the discrimination
    /// test's assertions, so that test is demonstrably load-bearing rather than
    /// merely present.
    ///
    /// This is the part a fixture of this shape usually skips: asserting that a
    /// correct traversal passes says nothing about whether the assertions would
    /// catch a wrong one. Here a saturating traversal (everything on every path)
    /// and a vacuous one (nothing on any path) are both run against the same two
    /// halves, and each fails its own half.
    #[test]
    fn a_saturating_traversal_and_a_vacuous_one_each_fail_a_half() {
        let registry = vec![Assumption::open_part("X", "XC6206", "no model matched")];

        // Saturating: attach the whole registry to every assertion. Half one
        // (A undermined, naming X) passes, and half two (B clean and empty)
        // fails, which is the failure that keeps the spine meaningful rather
        // than merely loud.
        let a = EvidenceMap::new("3V3 stays above 3.1 V", &registry, today());
        let b = EvidenceMap::new("VBUS stays below 5.5 V", &registry, today());
        assert_eq!(a.status(), EvidenceStatus::Undermined);
        assert_eq!(a.assumptions(), [AssumptionId("open-part:X".into())]);
        assert_ne!(
            b.status(),
            EvidenceStatus::Clean,
            "a saturating traversal must fail the discrimination test's second half"
        );
        assert!(!b.assumptions().is_empty());

        // Vacuous: attach nothing to anything. Half two passes, half one fails,
        // and the failure is the new vocabulary certifying the silence it was
        // built to end.
        let a = EvidenceMap::new("3V3 stays above 3.1 V", &[], today());
        let b = EvidenceMap::new("VBUS stays below 5.5 V", &[], today());
        assert_ne!(
            a.status(),
            EvidenceStatus::Undermined,
            "a vacuous traversal must fail the discrimination test's first half"
        );
        assert!(a.assumptions().is_empty());
        assert_eq!(b.status(), EvidenceStatus::Clean);
    }

    #[test]
    fn the_ideal_source_wording_is_composed_here_too() {
        // `held_by_ideal_source` is the second `ReducedFidelity` constructor, so
        // the kind table above exercises the generic one and this covers the
        // named one. Its wording is load-bearing: it is the difference between
        // "this rail check passed" and "this rail check could not have failed".
        let a = Assumption::held_by_ideal_source("3V3");
        a.validate().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.id().as_str(), "reduced-fidelity:3V3");
        assert_eq!(a.statement(), "Net 3V3 is held by an ideal source.");
        assert!(a.consequence().contains("vouches for nothing"));
        assert_eq!(a.scope(), &net_scope("3V3"));
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&a), today()),
            EvidenceStatus::Qualified
        );
    }
}
