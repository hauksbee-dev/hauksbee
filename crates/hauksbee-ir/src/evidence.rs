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
//!   and what would close it. The sentences are composed HERE, by the
//!   constructors, never by a renderer.
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
//! Which is why both types keep every field private behind getters. A status is
//! only as trustworthy as the kinds it was derived from, so a downstream
//! `a.kind = ReducedFidelity` would demote an undermined conclusion to a
//! gradeable one without touching [`EvidenceMap`] at all, and a mutable
//! `assertion` would let a clean map be relabelled onto an undermined assertion.
//! Renderers need to read these, never to write them.
//!
//! This module is types and rules only. It computes no causal path: building
//! the net-part incidence that decides which assumptions are on-path belongs to
//! the binder, and the discrimination test at the bottom of this file is the
//! contract that work has to satisfy.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/evidence.md

use serde::{Deserialize, Serialize};

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
pub struct AssumptionId(pub String);

impl AssumptionId {
    /// Compose an id from a kind and a subject. The subject is folded only
    /// where folding is needed to keep the id parseable: whitespace runs and
    /// `:` become `_`.
    pub fn new(kind: AssumptionKind, subject: &str) -> Self {
        let mut out = String::with_capacity(subject.len());
        let mut pending_space = false;
        for ch in subject.trim().chars() {
            if ch.is_whitespace() {
                pending_space = true;
                continue;
            }
            if pending_space {
                out.push('_');
                pending_space = false;
            }
            out.push(if ch == ':' { '_' } else { ch });
        }
        Self(format!("{}:{out}", kind.slug()))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AssumptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What kind of gap an [`Assumption`] records. The kind, not the wording, is
/// what [`EvidenceMap::derive_status`] reads, so adding a variant is a policy
/// decision about what undermines a conclusion.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumIter,
)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Scope {
    /// The whole run, e.g. every placed part treated as fitted because there
    /// was no BOM at all.
    Board,
    /// Specific parts, by reference designator.
    Parts {
        /// The reference designators.
        refs: Vec<String>,
    },
    /// Specific nets, by name.
    Nets {
        /// The net names.
        nets: Vec<String>,
    },
    /// A sim-time window, seconds.
    TimeWindow {
        /// Window start, seconds.
        start_s: f64,
        /// Window end, seconds.
        end_s: f64,
    },
    /// One named check ("drc", "si", "lint"), for `NotChecked` and `Waived`.
    Check {
        /// The check name.
        check: String,
        /// The specific rule inside the check, when the scope is that narrow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
/// Every field is private with a getter, which is what makes that structural
/// rather than conventional. Public fields would have left the wording
/// rewritable in one line from any crate, and `kind` is worse than the wording:
/// it is the input to [`EvidenceMap::derive_status`], so a downstream
/// `a.kind = ReducedFidelity` would demote an undermined conclusion to a
/// gradeable one without ever touching [`EvidenceMap`]. Read access is all a
/// renderer needs.
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
    /// every run-derived kind; a run-derived assumption carrying an expiry is
    /// malformed ([`Assumption::validate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<String>,
}

/// The deserialization shape of [`Assumption`], which exists so that reading one
/// back runs the same well-formedness rules a constructor's output satisfies.
/// A document is still its producer's word (nothing here can re-derive the
/// sentences from the id), but a malformed one is refused at the boundary rather
/// than rendered.
#[derive(Deserialize)]
struct AssumptionWire {
    id: AssumptionId,
    kind: AssumptionKind,
    source: AssumptionSource,
    scope: Scope,
    statement: String,
    because: String,
    consequence: String,
    replacement: String,
    #[serde(default)]
    expires: Option<String>,
}

impl<'de> Deserialize<'de> for Assumption {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let w = AssumptionWire::deserialize(d)?;
        let a = Self {
            id: w.id,
            kind: w.kind,
            source: w.source,
            scope: w.scope,
            statement: w.statement,
            because: w.because,
            consequence: w.consequence,
            replacement: w.replacement,
            expires: w.expires,
        };
        a.validate().map_err(serde::de::Error::custom)?;
        Ok(a)
    }
}

/// Trim a caller-supplied data fragment so the constructors, not the caller,
/// own sentence punctuation. Callers pass fragments ("no model matched"), never
/// finished sentences.
fn fragment(s: &str) -> String {
    s.trim()
        .trim_end_matches(['.', ';', ','])
        .trim()
        .to_string()
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
    ) -> Self {
        Self {
            id: AssumptionId::new(kind, subject),
            kind,
            source,
            scope,
            // Every sentence passes through `sentence` here, so capitalization
            // and the single full stop are guaranteed once rather than at ten
            // call sites, even where a composed sentence opens with a
            // caller-supplied fragment.
            statement: sentence(&statement),
            because: sentence(&because),
            consequence: sentence(&consequence),
            replacement: sentence(&replacement),
            expires: None,
        }
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
            reference,
            Scope::Parts {
                refs: vec![reference.to_string()],
            },
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
        let requested = fragment(requested);
        let stand_in = fragment(stand_in);
        Self::build(
            AssumptionKind::SubstituteModel,
            source,
            reference,
            Scope::Parts {
                refs: vec![reference.to_string()],
            },
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
        )
    }

    /// A pin's role was inferred from the pin-rule table rather than read from
    /// the schematic.
    pub fn inferred_pin_role(reference: &str, pin: &str, role: &str) -> Self {
        let pin = fragment(pin);
        let role = fragment(role);
        Self::build(
            AssumptionKind::InferredPinRole,
            AssumptionSource::Binder,
            &format!("{reference}/{pin}"),
            Scope::Parts {
                refs: vec![reference.to_string()],
            },
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
        )
    }

    /// A parameter took a documented default because no input carried a value.
    pub fn default_parameter(reference: &str, parameter: &str, value: &str) -> Self {
        let parameter = fragment(parameter);
        let value = fragment(value);
        Self::build(
            AssumptionKind::DefaultParameter,
            AssumptionSource::Binder,
            &format!("{reference}.{parameter}"),
            Scope::Parts {
                refs: vec![reference.to_string()],
            },
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
        let input = fragment(input.text);
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
        )
    }

    /// A whole check could not run on this input class. `because` and
    /// `replacement` are data fragments, because what is missing and what would
    /// supply it differ per check and per input class; the statement and the
    /// consequence, which carry the honesty, are fixed here.
    pub fn not_checked(
        source: AssumptionSource,
        check: &str,
        because: &str,
        replacement: &str,
    ) -> Self {
        let check = fragment(check);
        Self::build(
            AssumptionKind::NotChecked,
            source,
            &check,
            Scope::Check {
                check: check.clone(),
                kind: None,
            },
            format!("The {check} check did not run."),
            sentence(because),
            format!(
                "This run says nothing about {check}: no {check} findings here is not evidence \
                 that there are none."
            ),
            sentence(replacement),
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
        let subject_text = fragment(subject.text);
        Self::build(
            AssumptionKind::NotExercised,
            source,
            subject_key,
            scope,
            format!("This run does not cover {subject_text}."),
            sentence(because),
            format!(
                "Any conclusion that assumes {subject_text} was exercised is unsupported by \
                 this run: what it reports there is a default, not a measurement."
            ),
            sentence(replacement),
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
        let subject_text = fragment(subject.text);
        let mechanism = fragment(mechanism);
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
            sentence(replacement),
        )
    }

    /// A net held by an ideal source: the check cannot fail there for a board
    /// reason, so a pass on it vouches for nothing. A named `ReducedFidelity`
    /// constructor because this wording is load-bearing and appears on several
    /// surfaces.
    pub fn held_by_ideal_source(net: &str) -> Self {
        Self::build(
            AssumptionKind::ReducedFidelity,
            AssumptionSource::Check,
            net,
            Scope::Nets {
                nets: vec![net.to_string()],
            },
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
        let subject_text = fragment(subject.text);
        Self::build(
            AssumptionKind::ParserLimitation,
            source,
            subject_key,
            scope,
            format!("Findings about {subject_text} may be wrong."),
            sentence(limitation),
            format!(
                "Findings on {subject_text} can be fabricated or missed here, so treat them as \
                 unconfirmed rather than as results."
            ),
            sentence(replacement),
        )
    }

    /// The surfaced record of an applied waiver. The gating machinery stays in
    /// the waiver code; this is what a reader sees. `until` is the waiver's
    /// `YYYY-MM-DD` expiry and lands in [`Assumption::expires`], which is what
    /// lets [`EvidenceMap::derive_status`] treat a lapsed waiver as undermining
    /// rather than qualifying.
    pub fn waived(check: &str, kind: &str, subject: &str, reason: &str, until: &str) -> Self {
        let check = fragment(check);
        let kind = fragment(kind);
        let subject_text = fragment(subject);
        let until = fragment(until);
        let mut a = Self::build(
            AssumptionKind::Waived,
            AssumptionSource::User,
            &format!("{check}/{kind}/{subject_text}"),
            Scope::Check {
                check: check.clone(),
                kind: Some(kind.clone()),
            },
            format!("The {check} {kind} finding on {subject_text} is waived by hand."),
            sentence(reason),
            format!(
                "It does not gate this run until {until}, so this result is clean by \
                 authorization rather than by measurement."
            ),
            format!(
                "Fix the finding, or let the waiver lapse on {until}, after which it gates \
                 again."
            ),
        );
        a.expires = Some(until);
        a
    }

    /// Structural well-formedness, for a registry to assert as it collects.
    /// Two rules: the four sentences are all present, and only a `Waived`
    /// assumption carries an expiry (an expiry on a machine observation is
    /// meaningless, so it is a construction error rather than a warning).
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
            if text.contains("  ") || text.contains(" .") {
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
        if !self.id.as_str().starts_with(self.kind.slug()) {
            return Err(format!(
                "{}: the id's kind slug is not `{}`",
                self.id,
                self.kind.slug()
            ));
        }
        match (self.kind, self.expires.as_deref()) {
            (AssumptionKind::Waived, None) => {
                Err(format!("{}: a waived assumption needs an expiry", self.id))
            }
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
/// [`AssumptionKind::ParserLimitation`] assumption; an agreement is positive
/// evidence and belongs in the inventory in its own right.
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
/// use hauksbee_ir::evidence::{ArtifactProvenance, ArtifactRole, Contribution};
///
/// let a = ArtifactProvenance {
///     path: "boards/blinky.kicad_pcb".into(),
///     kind: "kicad_pcb".into(),
///     role: ArtifactRole::Layout,
///     sha256: "0".repeat(64),
///     contributed: vec![Contribution {
///         what: "connectivity".into(),
///         detail: "nets read from the file's net table".into(),
///     }],
///     ignored: Vec::new(),
///     cross_checks: Vec::new(),
///     assumptions: Vec::new(),
/// };
/// assert_eq!(a.role, ArtifactRole::Layout);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(
    description = "One input artifact: what it was, what it gave the run, \
                          what in it was ignored, and which cross-checks it passed."
)]
pub struct ArtifactProvenance {
    /// Path as the user gave it, not canonicalized: the report must read back
    /// in the user's own vocabulary.
    pub path: String,
    /// What the normalizer recognised it as: "kicad_pcb", "altium_pcbdoc",
    /// "gerber_archive", "odbpp", "ipc2581", "board_code", "elf", "hex",
    /// "toml", and so on.
    pub kind: String,
    /// The role it played.
    pub role: ArtifactRole,
    /// SHA-256 of the bytes read (of the archive, for archives). Empty only
    /// for a gerber DIRECTORY, where there is no single file to hash.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// What it contributed.
    pub contributed: Vec<Contribution>,
    /// What in it was deliberately not used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<IgnoredInput>,
    /// Cross-checks run on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cross_checks: Vec<CrossCheck>,
    /// Assumptions this artifact's limitations raised, by id, so the inventory
    /// row and the assumption registry cross-reference instead of duplicating
    /// text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<AssumptionId>,
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
        layer: String,
        /// How good the match was.
        confidence: String,
    },
    /// A documented default, naming the assumption that records it.
    Default {
        /// The `DefaultParameter` assumption for this value.
        assumption: AssumptionId,
    },
    /// The user overrode it (an as-built overlay, a spec field, a CLI flag).
    UserOverride {
        /// How the override arrived.
        via: String,
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TimeWindow {
    /// Window start, seconds.
    pub start_s: f64,
    /// Window end, seconds, exclusive.
    pub end_s: f64,
}

/// One window and the integration method that produced it. The primary method
/// covers most runs in a single entry; a per-chunk fallback ladder contributes
/// one entry per fallback-solved window, using that machinery's own stable
/// method names verbatim ("trapezoidal", "gear2", "backward-euler",
/// "reduced-step", "cold-start-backward-euler",
/// "subdivided-backward-euler").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WindowMethod {
    /// The span this method produced.
    pub window: TimeWindow,
    /// The method's stable name.
    pub method: String,
    /// The accuracy cost, stated: empty for the primary method, a sentence for
    /// a more dissipative rung.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accuracy: String,
}

/// The solver's convergence and truncation-error settings for this result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntegrationTolerance {
    /// Relative tolerance for Newton convergence and local-truncation-error
    /// control.
    pub reltol: f64,
    /// Absolute current tolerance, amps.
    pub abstol: f64,
    /// Charge tolerance, coulombs (capacitor local truncation error).
    pub chgtol: f64,
}

/// The final accepted Newton residual, worst node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Residual {
    /// `max |f(x)|` over the residual vector at acceptance.
    pub max_abs: f64,
    /// The node or branch carrying it, for the "who" question.
    pub at: String,
}

/// Interval bounds a model places on a derived quantity. This is the socket for
/// interval models: shaped before its producer exists so that work lands in
/// this vocabulary instead of inventing a second one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelUncertainty {
    /// Which parameter carries the interval ("Q2.beta", "D1.vf").
    pub parameter: String,
    /// Interval low bound.
    pub low: f64,
    /// Interval high bound.
    pub high: f64,
    /// Where the interval came from ("datasheet min/max", "pack tolerance").
    pub basis: String,
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
/// let b = ErrorBudget::new(IntegrationTolerance {
///     reltol: 1e-3,
///     abstol: 1e-12,
///     chgtol: 1e-14,
/// });
/// assert!(b.methods.is_empty());
/// assert!(b.failed_windows.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(
    description = "How good a number is: integration tolerances, which method \
                          produced which time window, the accepted residual, the \
                          windows with no valid solution, and model uncertainty."
)]
pub struct ErrorBudget {
    /// The solver settings the numbers were produced under.
    pub tolerance: IntegrationTolerance,
    /// Which method produced which span. One entry for a homogeneous run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<WindowMethod>,
    /// The accepted Newton residual, when the producing path measured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residual: Option<Residual>,
    /// Windows with NO valid solution: the run held stale values there. Any
    /// quantity read inside one of these is not a measurement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_windows: Vec<TimeWindow>,
    /// Worst-case timestamp error for events, seconds. Today this is the
    /// co-sim chunk quantization: an edge is observed at a chunk boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time_error_s: Option<f64>,
    /// Interval bounds models place on parameters behind these numbers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_uncertainty: Vec<ModelUncertainty>,
}

impl ErrorBudget {
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelOnPath {
    /// The part it is bound to.
    pub reference: String,
    /// The model that bound.
    pub model_id: String,
    /// Which layer of the model ladder it came from, same vocabulary as
    /// [`ValueOrigin::Model`].
    pub layer: String,
    /// How good the match was.
    pub confidence: String,
}

/// How much a conclusion is entitled to claim, derived from the assumptions on
/// its causal path. Ordered by severity: `Clean < Qualified < Undermined`.
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

/// What one assertion rests on: the artifacts, models, parameters and
/// assumptions on its causal path, its error budget, and the derived status.
///
/// `status`, the assertion it is a status OF, and the id list it was derived
/// from are all private, and there is one constructor. That is the whole honesty
/// argument: no producer, no renderer and no other crate can declare a
/// conclusion clean, because the only way to obtain an `EvidenceMap` is to hand
/// [`EvidenceMap::new`] the on-path assumptions and let it decide, and none of
/// the three can be prised apart afterwards. `Undermined` maps onto the run's
/// existing invalid-for-analysis outcome, which waivers already refuse to flip
/// green, so the only routes back to a clean verdict are closing the assumption
/// or acknowledging it in the open.
///
/// ```
/// use hauksbee_ir::evidence::{Assumption, EvidenceMap, EvidenceStatus};
///
/// let open = Assumption::open_part("U2", "XC6206", "no model matched");
/// let map = EvidenceMap::new("3V3 stays above 3.1 V", &[open], 20_000)
///     .with_artifacts(vec![0, 2]);
/// assert_eq!(map.status(), EvidenceStatus::Undermined);
/// assert_eq!(map.assumptions()[0].as_str(), "open-part:U2");
///
/// let clean = EvidenceMap::new("VBUS stays below 5.5 V", &[], 20_000);
/// assert_eq!(clean.status(), EvidenceStatus::Clean);
/// assert!(clean.assumptions().is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[schemars(description = "What one assertion rests on: the artifacts, models, \
                          parameters and assumptions on its causal path, its error \
                          budget, and the status derived from them.")]
pub struct EvidenceMap {
    /// The assertion or finding this map belongs to. Private with a getter: it
    /// is set once, and relabelling a clean map onto a different assertion is
    /// the same forgery one indirection out.
    assertion: String,
    /// Indices into the run's `inventory`, causal only: the artifacts whose
    /// contributions this assertion actually consumed.
    pub artifacts: Vec<usize>,
    /// Models bound on the causal path.
    pub models: Vec<ModelOnPath>,
    /// Parameters read on the causal path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ParameterProvenance>,
    /// On-path assumptions, by id into the run's assumption registry. Private
    /// with a getter, because it is half of the derived-status invariant:
    /// letting a caller push an id in after construction would let the id list
    /// and the status disagree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assumptions: Vec<AssumptionId>,
    /// How good the numbers behind this assertion are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_budget: Option<ErrorBudget>,
    /// Statistical coverage wording for ensemble assertions. Not an
    /// assumption: an accurate statement of method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    /// DERIVED, never set. See [`EvidenceMap::derive_status`].
    status: EvidenceStatus,
}

/// The deserialization shape of [`EvidenceMap`].
///
/// A serialized map carries assumption *ids*, not kinds, so the status cannot be
/// recomputed on the way back in: the kinds it derives from live in the run's
/// registry, a different part of the document. Two things are enforced anyway,
/// because they need no kinds: an empty id list is forced to `Clean`, and a
/// non-empty one can never read back `Clean`. Above that floor a status read from
/// a document is its producer's word, which is the honest limit of what a reader
/// can check. Nothing in the tree produces a map by deserializing one; the
/// produced-side guarantee is the private field plus the single constructor.
#[derive(Deserialize)]
struct EvidenceMapWire {
    assertion: String,
    #[serde(default)]
    artifacts: Vec<usize>,
    #[serde(default)]
    models: Vec<ModelOnPath>,
    #[serde(default)]
    parameters: Vec<ParameterProvenance>,
    #[serde(default)]
    assumptions: Vec<AssumptionId>,
    #[serde(default)]
    error_budget: Option<ErrorBudget>,
    #[serde(default)]
    coverage: Option<String>,
    #[serde(default = "wire_default_status")]
    status: EvidenceStatus,
}

fn wire_default_status() -> EvidenceStatus {
    EvidenceStatus::Undermined
}

/// Hand-written so the schema stays the schema of [`EvidenceMap`] itself (a
/// `#[serde(from = ...)]` container attribute would publish the permissive wire
/// shape instead, and the report schema has a drift test).
impl<'de> Deserialize<'de> for EvidenceMap {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(EvidenceMapWire::deserialize(d)?.into())
    }
}

impl From<EvidenceMapWire> for EvidenceMap {
    fn from(w: EvidenceMapWire) -> Self {
        let status = if w.assumptions.is_empty() {
            EvidenceStatus::Clean
        } else {
            w.status.max(EvidenceStatus::Qualified)
        };
        Self {
            assertion: w.assertion,
            artifacts: w.artifacts,
            models: w.models,
            parameters: w.parameters,
            assumptions: w.assumptions,
            error_budget: w.error_budget,
            coverage: w.coverage,
            status,
        }
    }
}

impl EvidenceMap {
    /// The one constructor. `on_path` is the assumptions the causal-path
    /// traversal found for this assertion, whole rather than by id, because the
    /// status rule reads their kinds and expiries.
    ///
    /// `today_epoch_days` is the run's date as days since the Unix epoch, used
    /// only to tell a live waiver from a lapsed one. It is taken verbatim, which
    /// is deliberately the same contract as the waiver gate's own `load_at`: the
    /// clock floor that refuses an implausibly early reading belongs at the one
    /// place the clock is READ, not at every place a date is compared, and
    /// duplicating that floor here would give the repo two of them to keep in
    /// step. So callers pass a floored reading. A caller with no date at all
    /// passes `i64::MAX`, which reads every waiver as lapsed, the fail-closed
    /// direction.
    pub fn new(
        assertion: impl Into<String>,
        on_path: &[Assumption],
        today_epoch_days: i64,
    ) -> Self {
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
            status: Self::derive_status(on_path, today_epoch_days),
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
    pub fn derive_status(on_path: &[Assumption], today_epoch_days: i64) -> EvidenceStatus {
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
                        Some(day) if day >= today_epoch_days => EvidenceStatus::Qualified,
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
    pub fn with_artifacts(mut self, artifacts: Vec<usize>) -> Self {
        self.artifacts = artifacts;
        self
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

    /// Attach the error budget for this assertion's numbers.
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

    /// 2026-01-01, a fixed "today" so no test depends on the wall clock.
    const TODAY: i64 = 20_454;

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
                Scope::Parts {
                    refs: vec!["R7".into()],
                },
            ),
            Assumption::not_checked(
                AssumptionSource::Reader,
                "drc",
                "this input class carries no copper geometry",
                "supply a layout so the check has copper to read",
            ),
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::new("i2c0", "the i2c0 bus"),
                Scope::Nets {
                    nets: vec!["SDA".into(), "SCL".into()],
                },
                "the MCU backend models no I2C controller on this platform",
                "run on a platform whose backend models the controller",
            ),
            Assumption::reduced_fidelity(
                AssumptionSource::Scheduler,
                Subject::new("spi0/framing", "SPI transaction framing on spi0"),
                Scope::Nets {
                    nets: vec!["SCK".into()],
                },
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
                "2026-06-01",
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
            Assumption::not_checked(AssumptionSource::Reader, "drc", "no copper", "add a layout")
                .id
                .as_str(),
            "not-checked:drc"
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
            Assumption::waived("si", "controlled_impedance", "DDR_CLK", "why", "2026-06-01")
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
            "not-checked:spec_check__rails"
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
                AssumptionKind::Waived => assert_eq!(a.expires.as_deref(), Some("2026-06-01")),
                _ => assert!(a.expires.is_none(), "{} carries an expiry", a.id),
            }
        }
        // The two malformed shapes are construction errors, not warnings.
        let mut open = Assumption::open_part("R7", "10k", "");
        open.expires = Some("2026-06-01".into());
        assert!(open.validate().is_err());
        let mut waived = Assumption::waived("si", "k", "N1", "why", "2026-06-01");
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

    #[test]
    fn a_sentence_with_a_hole_in_it_is_a_construction_error() {
        // An empty datum leaves a gap where a subject or a value belonged, and a
        // half-written sentence on a report is worse than a loud failure.
        let holed = Assumption::not_checked(AssumptionSource::Reader, "", "no copper", "add one");
        assert!(holed.validate().is_err(), "{:?}", holed.statement);
        let holed = Assumption::substitute_model(AssumptionSource::Binder, "U1", "", "atmega328p");
        assert!(holed.validate().is_err(), "{:?}", holed.statement);
    }

    #[test]
    fn a_deserialized_assumption_runs_the_same_well_formedness_rules() {
        // Reading a report back cannot re-derive the sentences, but a malformed
        // assumption is refused at the boundary rather than rendered.
        let bad = serde_json::json!({
            "id": "open-part:R7",
            "kind": "open_part",
            "source": "binder",
            "scope": { "type": "parts", "refs": ["R7"] },
            "statement": "R7 is treated as an open circuit.",
            "because": "No model matched.",
            "consequence": "Nets through R7 are isolated.",
            "replacement": "",
        });
        assert!(serde_json::from_value::<Assumption>(bad).is_err());
        // And an id whose slug disagrees with the kind, which would make the
        // status rule and the rendered id describe different things.
        let mismatched = serde_json::json!({
            "id": "reduced-fidelity:R7",
            "kind": "open_part",
            "source": "binder",
            "scope": { "type": "parts", "refs": ["R7"] },
            "statement": "R7 is treated as an open circuit.",
            "because": "No model matched.",
            "consequence": "Nets through R7 are isolated.",
            "replacement": "Add a model.",
        });
        assert!(serde_json::from_value::<Assumption>(mismatched).is_err());
    }

    #[test]
    fn an_absurd_expiry_is_refused_rather_than_overflowing() {
        // `until` is user input, and the civil-date arithmetic multiplies by
        // 146_097, so an absurd year has to be refused before it is computed.
        assert_eq!(parse_ymd_epoch_days("9223372036854775807-03-01"), None);
        assert_eq!(parse_ymd_epoch_days("99999999999999-12-31"), None);
        assert_eq!(parse_ymd_epoch_days("2026-02-30"), None);
        assert_eq!(parse_ymd_epoch_days("2026-13-01"), None);
        assert_eq!(parse_ymd_epoch_days("2026-06-01"), Some(20_605));
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
        let map = EvidenceMap::new("A", &[a.clone(), a], TODAY);
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
            let map = EvidenceMap::new("A", std::slice::from_ref(a), TODAY);
            assert_eq!(map.status(), want, "{} should be {want:?} on its own", a.id);
        }
    }

    #[test]
    fn no_assumptions_is_clean() {
        let map = EvidenceMap::new("A", &[], TODAY);
        assert_eq!(map.status(), EvidenceStatus::Clean);
        assert!(map.assumptions().is_empty());
        assert!(!map.is_undermined());
    }

    #[test]
    fn one_undermining_assumption_beats_any_number_of_qualifiers() {
        let mut set: Vec<Assumption> = all_kinds()
            .into_iter()
            .filter(|a| {
                EvidenceMap::derive_status(std::slice::from_ref(a), TODAY)
                    == EvidenceStatus::Qualified
            })
            .collect();
        assert_eq!(
            EvidenceMap::derive_status(&set, TODAY),
            EvidenceStatus::Qualified
        );
        set.push(Assumption::open_part("R7", "10k", ""));
        assert_eq!(
            EvidenceMap::derive_status(&set, TODAY),
            EvidenceStatus::Undermined,
            "a pile of caveats must not dilute one undermining assumption"
        );
    }

    #[test]
    fn a_lapsed_waiver_stops_covering() {
        let w = Assumption::waived("si", "k", "DDR_CLK", "fab confirmed", "2026-06-01");
        let expiry = parse_ymd_epoch_days("2026-06-01").unwrap();
        // In force up to and including the expiry date (end-of-day expiry, the
        // reading the waiver gate already uses).
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&w), expiry),
            EvidenceStatus::Qualified
        );
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&w), expiry + 1),
            EvidenceStatus::Undermined
        );
        // An unreadable or absent expiry fails CLOSED: a date that cannot be
        // read cannot vouch for a finding.
        let mut broken = w.clone();
        broken.expires = Some("next Friday".into());
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&broken), TODAY),
            EvidenceStatus::Undermined
        );
        broken.expires = None;
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&broken), TODAY),
            EvidenceStatus::Undermined
        );
        // And a caller with no date at all gets the same fail-closed reading.
        assert_eq!(
            EvidenceMap::derive_status(std::slice::from_ref(&w), i64::MAX),
            EvidenceStatus::Undermined
        );
    }

    // ── forgery ─────────────────────────────────────────────────────────

    #[test]
    fn a_hand_edited_report_cannot_launder_a_status() {
        // The only in-tree route to an EvidenceMap that did not go through
        // `new` is deserialization, so that is where the laundering attempt
        // has to be blocked.
        let forged = serde_json::json!({
            "assertion": "3V3 stays above 3.1 V",
            "artifacts": [0],
            "models": [],
            "assumptions": ["open-part:U2"],
            "status": "clean"
        });
        let map: EvidenceMap = serde_json::from_value(forged).unwrap();
        assert_ne!(
            map.status(),
            EvidenceStatus::Clean,
            "an assertion with on-path assumptions must never read back clean"
        );
        // Nor the mirror trick: scaring a reader with a status the id list does
        // not support.
        let empty = serde_json::json!({
            "assertion": "VBUS stays below 5.5 V",
            "artifacts": [],
            "models": [],
            "status": "undermined"
        });
        let map: EvidenceMap = serde_json::from_value(empty).unwrap();
        assert_eq!(map.status(), EvidenceStatus::Clean);
    }

    #[test]
    fn a_status_read_back_above_the_floor_is_the_producers_word() {
        // The honest limit of what a reader can check: the kinds a status derives
        // from live in the run's assumption registry, not in the map, so a
        // document claiming `qualified` over an undermining id list reads back as
        // it was written. What cannot be forged is a clean verdict, and nothing
        // in the tree builds a map by deserializing one. A consumer that wants
        // certainty re-derives from the registry with `derive_status`.
        let wire = serde_json::json!({
            "assertion": "3V3 stays above 3.1 V",
            "artifacts": [0],
            "models": [],
            "assumptions": ["open-part:U2"],
            "status": "qualified"
        });
        let map: EvidenceMap = serde_json::from_value(wire).unwrap();
        assert_eq!(map.status(), EvidenceStatus::Qualified);
        let registry = [Assumption::open_part("U2", "XC6206", "no model matched")];
        assert_eq!(
            EvidenceMap::derive_status(&registry, TODAY),
            EvidenceStatus::Undermined,
            "re-deriving from the registry is what settles it"
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
        assert_eq!(v["scope"]["type"], "parts");
        assert_eq!(v["scope"]["refs"][0], "R7");
        // No expiry on a run-derived assumption, and the field is absent
        // rather than null: the common shape stays small.
        assert!(v.get("expires").is_none());
        assert_eq!(
            v.as_object().unwrap().keys().count(),
            8,
            "the assumption shape gained or lost a field; the report schema \
             bumps exactly once, in the rendering phase"
        );
        let back: Assumption = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn evidence_map_json_shape_is_the_published_one() {
        let open = Assumption::open_part("U2", "XC6206", "no model matched");
        let map = EvidenceMap::new("3V3 stays above 3.1 V", &[open], TODAY)
            .with_artifacts(vec![0, 2])
            .with_models(vec![ModelOnPath {
                reference: "U2".into(),
                model_id: "xc6206".into(),
                layer: "pack".into(),
                confidence: "high".into(),
            }])
            .with_error_budget(ErrorBudget {
                methods: vec![WindowMethod {
                    window: TimeWindow {
                        start_s: 0.0,
                        end_s: 0.05,
                    },
                    method: "trapezoidal".into(),
                    accuracy: String::new(),
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
        // Round-trip keeps the status: a consumer reads what the producer said.
        let back: EvidenceMap = serde_json::from_value(v).unwrap();
        assert_eq!(back.status(), EvidenceStatus::Undermined);
        assert_eq!(back, map);
    }

    #[test]
    fn provenance_and_origin_shapes() {
        let p = ParameterProvenance {
            parameter: "U2.vout".into(),
            value: "3.3 V".into(),
            origin: ValueOrigin::Model {
                model_id: "xc6206".into(),
                layer: "pack".into(),
                confidence: "exact".into(),
            },
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["origin"]["type"], "model");
        assert_eq!(v["origin"]["layer"], "pack");
        let art = ArtifactProvenance {
            path: "boards/blinky.kicad_pcb".into(),
            kind: "kicad_pcb".into(),
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
        let back: ArtifactProvenance = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
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
    /// The incidence below is built BY HAND, because the real one comes from
    /// the binder (which records net-part incidence it currently discards) and
    /// that is a later phase. When that traversal lands, it replaces
    /// `on_path_for` here and this test must pass unchanged: an unresolved part
    /// on assertion A's net undermines A and names the part, and assertion B on
    /// an unreachable net stays clean and empty. A vacuous traversal fails the
    /// first half; a saturated one fails the second.
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
                    Scope::Parts { refs } => refs.iter().any(|r| reachable.contains(&r.as_str())),
                    Scope::Nets { nets } => nets.iter().any(|n| subject_nets.contains(&n.as_str())),
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
                    Scope::Check { .. } | Scope::TimeWindow { .. } => false,
                })
                .cloned()
                .collect()
        }

        let a = EvidenceMap::new(
            "3V3 stays above 3.1 V",
            &on_path_for(&["3V3"], incidence, &registry),
            TODAY,
        );
        let b = EvidenceMap::new(
            "VBUS stays below 5.5 V",
            &on_path_for(&["VBUS"], incidence, &registry),
            TODAY,
        );

        // Half one: the gap cannot hide behind a board-wide percentage. A is
        // undermined, and it NAMES the part, so the report can say which.
        assert_eq!(a.status(), EvidenceStatus::Undermined);
        assert!(a.is_undermined());
        assert_eq!(a.assumptions(), [AssumptionId("open-part:X".into())]);

        // Half two: and it does not smear over the rest of the board.
        assert_eq!(b.status(), EvidenceStatus::Clean);
        assert!(b.assumptions().is_empty());
    }

    /// The saturated map, shown failing, so the discrimination test above is
    /// demonstrably load-bearing rather than merely present: attaching the
    /// whole registry to the unrelated assertion is exactly what turns it red.
    #[test]
    fn attaching_the_whole_registry_makes_an_unrelated_assertion_undermined() {
        let registry = vec![Assumption::open_part("X", "XC6206", "no model matched")];
        let saturated = EvidenceMap::new("VBUS stays below 5.5 V", &registry, TODAY);
        assert_eq!(saturated.status(), EvidenceStatus::Undermined);
        assert!(!saturated.assumptions().is_empty());
    }
}
