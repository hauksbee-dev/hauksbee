//! The structured-honest result layer.
//!
//! A static check (`--si`/`--drc`/`--lint`/`--report`/`--thermal`/`--ac`) that
//! renders straight to a Unicode table or to a plain-language verdict opens two
//! failure modes:
//!
//! 1. **Silent sentinels.** An AC sweep with no signal path prints 121 rows of
//!    `-6000 dB`; a thermal table with no resolved dissipating devices prints
//!    one resistor. Both *look like data* and are not. (Theme A.)
//! 2. **False-comfort metrics.** The bind summary reports a flat "83% resolved"
//!    while the entire active power circuit is open. (Theme F.)
//!
//! This module is the single structured representation that the text, plain, and
//! JSON renderers all read from, so they can never disagree (the same discipline
//! as [`crate::plain`], extended with machine-readable validity + honest bind
//! roles). It does **not** re-run or weaken any check: it consumes the existing
//! [`BindReport`](crate::BindReport), [`DrcReport`],
//! AC bode points, and thermal peaks, and adds the honesty annotations
//! (`valid`/`reason`, `critical_parts_bound`, grouped DRC) on top.
//!
//! ## Exit-code contract (see `ONBOARDING_PLAN` §4.2)
//! - `0`  clean, or findings present but not `--strict`.
//! - `1`  the run never happened: the board could not be read, or the analysis
//!   could not be set up. An input error, not a verdict about the board.
//! - `2`  `--strict` and at least one gating finding.
//! - `3`  **board invalid for the requested analysis** (AC with no signal path,
//!   thermal with no resolved dissipating devices). A run that could not
//!   produce a meaningful answer must never exit 0.
//!
//! ## Rendering paths
//!
//! One path per output surface, each reading the structured types below rather
//! than re-deriving a finding of its own:
//!
//! - **text**: `reports::<family>::emit` under <code>[crate::reports::OutputMode]::Text</code>,
//!   rendering through [`DrcStructured::render_with_clearance_rule_provenance`],
//!   [`crate::reports::bind::BindReport::render_table_compact`],
//!   [`BindSummary::render_banner`], [`Refusal::render_text`] and
//!   `reports::render_evidence_appendix`.
//! - **plain-language** (`--plain`): the same `emit`, through
//!   [`crate::plain`]'s `plain_*` builders into [`crate::plain::PlainReport::render`]
//!   (DRC condenses through `plain::render_drc_condensed*`, which prints its
//!   findings with the same `PlainReport` writers).
//! - **JSON** (`--json`): the same `emit`, through [`JsonReport::to_json`].
//! - **web**: [`crate::frontdoor::analyze`] and friends, building a `WebReport`
//!   from the same [`DrcStructured`], [`BindSummary`] and `plain_*` values as
//!   the three above, serialized in the browser's own shape.
//!
//! ## Deliberate divergences
//!
//! The one register of them. Anything else that differs between two surfaces
//! for the same board is a bug, not a variant.
//!
//! 1. **Web DRC clearance rules.** The front door receives one file's bytes and
//!    cannot see a sibling `.kicad_pro`, so its DRC uses the board's
//!    default/embedded clearance where the CLI applies per-netclass rules. On a
//!    board with non-default netclass clearances the two surfaces can report
//!    different violations; every other check is byte-for-byte the same.
//! 2. **JSON keeps the SI info notes** `--plain` suppresses
//!    ([`si_findings_json`]), so a machine consumer never loses a note the
//!    prose surface chose not to nag about.
//! 3. **`--plain` humanises strict gate ids** (`reports::strict_gate_exit`);
//!    text and JSON keep the exact rule ids, which are the grep/waiver keys.

use serde::ser::SerializeStruct;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use hauksbee_extract::{DrcReport, ViolationKind};
use hauksbee_ir::vocabulary::{DRAFT_MODELS_NEXT_STEP, NOT_JUDGED_JOINER, NOT_JUDGED_LEAD};

// The role-aware bind summary lives with the bind report in hauksbee-bind;
// it stays reachable here so `hauksbee_engine::result::BindSummary` and the
// generated `run --json` schema are unchanged.
use hauksbee_bind::bind_report::is_tool_generated_ref;
pub use hauksbee_bind::bind_report::{BindSummary, UnresolvedActive};

pub use hauksbee_bind::EXIT_INVALID_FOR_ANALYSIS;

/// The C5.3 refusal contract.
///
/// Exit 3 means the requested claim could not be made; it is neither a finding
/// nor a malformed-input error.  Every renderer carries these same four answers
/// so a refusal is actionable and does not discard work that remains valid.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Refusal {
    /// The conclusion the run declined to make.
    pub claim: String,
    /// The concrete absent or invalid prerequisite that blocked that claim.
    pub missing_prerequisite: String,
    /// Conclusions/artifacts produced by the run that remain trustworthy.
    pub valid_partial_conclusions: Vec<String>,
    /// The cheapest concrete action that can make the claim answerable.
    pub next_action: String,
}

impl Refusal {
    pub fn new(
        claim: impl Into<String>,
        missing_prerequisite: impl Into<String>,
        valid_partial_conclusions: Vec<impl Into<String>>,
        next_action: impl Into<String>,
    ) -> Self {
        Self {
            claim: claim.into(),
            missing_prerequisite: missing_prerequisite.into(),
            valid_partial_conclusions: valid_partial_conclusions
                .into_iter()
                .map(Into::into)
                .collect(),
            next_action: next_action.into(),
        }
    }

    /// Lossless human rendering used by terminal and CI-native text surfaces.
    #[must_use]
    pub fn render_text(&self) -> String {
        format!(
            "refused claim: {}\nmissing prerequisite: {}\nvalid partial conclusions: {}\nnext action: {}",
            self.claim,
            self.missing_prerequisite,
            self.valid_partial_conclusions.join("; "),
            self.next_action,
        )
    }
}

/// The main reason a run is not a pass, in the order a verdict weighs them.
///
/// [`Self::reason`] is the ONE plain sentence (what is wrong, then what to do
/// next) that `--json`, the `--check` closing line, the CI renderers and the web
/// report all carry, so no surface can word the same verdict differently.
#[derive(Debug, Clone, Copy)]
pub enum VerdictCause<'a> {
    /// Gating findings: how many, then the first one's plain words and its fix.
    Failing {
        count: usize,
        first: Option<(&'a str, &'a str)>,
        /// The other parts that same finding is raised on (a mirrored half, a
        /// repeated cell), so the count's other problems are not left unnamed;
        /// empty when it is on no other part.
        same_on: &'a str,
    },
    /// A claim the run declined to make, the prerequisite it lacks, and the action.
    Refused {
        claim: &'a str,
        missing: &'a str,
        next: &'a str,
    },
    /// Active or current-carrying parts with no usable model.
    Unmodelled(&'a [String]),
    /// An input the checks rest on is missing or unreadable.
    Coverage,
}

impl<'a> VerdictCause<'a> {
    /// The cause of a non-pass verdict: failing beats refused beats unmodelled
    /// beats coverage, exactly the order the verdict itself uses.
    #[must_use]
    pub fn pick(
        failing: Option<Self>,
        refused: Option<Self>,
        unmodelled: &'a [String],
        coverage: bool,
    ) -> Option<Self> {
        failing
            .or(refused)
            .or_else(|| (!unmodelled.is_empty()).then_some(Self::Unmodelled(unmodelled)))
            .or_else(|| coverage.then_some(Self::Coverage))
    }

    /// Whether this cause makes the verdict `invalid` (it could not be judged)
    /// rather than `fail` (it was judged and found wanting).
    #[must_use]
    pub const fn is_invalid(self) -> bool {
        !matches!(self, Self::Failing { .. })
    }

    /// One plain sentence: the main reason, then the next action.
    #[must_use]
    pub fn reason(self) -> String {
        let (why, next) = match self {
            Self::Failing {
                count,
                first,
                same_on,
            } => (
                format!(
                    "{count} blocking problem(s){}",
                    // A finding's own remedy is the next step: not said twice.
                    first.map_or_else(String::new, |(what, _)| {
                        let said = what.rsplit_once(hauksbee_extract::REMEDY_MARKER);
                        let same = if same_on.is_empty() {
                            String::new()
                        } else {
                            format!(" (the same on {same_on})")
                        };
                        format!(
                            ", first: {}{same}",
                            said.map_or(what, |(w, _)| w.trim_end())
                        )
                    })
                ),
                first
                    .map(|(_, fix)| fix)
                    .filter(|fix| !fix.is_empty())
                    .unwrap_or("fix the first listed finding and rerun")
                    .to_string(),
            ),
            Self::Refused { claim, missing, next } => (
                format!("cannot give {claim}: {missing}"),
                next.to_string(),
            ),
            Self::Unmodelled(refs) => (
                format!(
                    "{} key {} no usable model ({}), so the checks cannot vouch for this board",
                    refs.len(),
                    if refs.len() == 1 { "part has" } else { "parts have" },
                    refs_summary(refs)
                ),
                "draft their models with `hauksbee models extract --unmodelled <board>` (one part: `--pdf <datasheet.pdf> --part <MPN>`) or `hauksbee models new`, then rerun".into(),
            ),
            Self::Coverage => (
                "an input the checks rest on is missing or unreadable, so a clean result would not be trustworthy".into(),
                "read the undermined items under Evidence, supply the input they name, then rerun".into(),
            ),
        };
        format!(
            "{}; next: {}.",
            why.trim_end_matches('.'),
            next.trim_end_matches('.')
        )
    }

    /// [`Self::reason`], plus the coverage hole a failing or refused run would
    /// otherwise hide. A run that stops on a finding (or on a refusal) while
    /// key parts have no usable model still fails, but fixing that finding must
    /// not read as "the rest of the board is fine": it was never judged. The
    /// unmodelled cause already says this on its own, so it is left unchanged.
    #[must_use]
    pub fn reason_alongside(self, unmodelled: &[String]) -> String {
        self.reason_scoped(unmodelled, None)
    }

    /// [`Self::reason_alongside`], with the judgement scope's sentence in
    /// place of the blanket caveat when the verdict was scoped around them.
    #[must_use]
    pub fn reason_scoped(
        self,
        unmodelled: &[String],
        scope: Option<&crate::scope::JudgementScope>,
    ) -> String {
        self.not_judged(unmodelled, scope).map_or_else(
            || self.reason(),
            |clause| format!("{}{NOT_JUDGED_JOINER}{clause}", self.reason()),
        )
    }

    /// The one sentence naming what a failing or refused run did not judge,
    /// which [`Self::reason_scoped`] appends after the joiner: the scope's
    /// sentence when the verdict was scoped around the open parts, else the
    /// blanket caveat. `None` when every key part has a model, and for the
    /// unmodelled cause, which already says it.
    #[must_use]
    pub fn not_judged(
        self,
        unmodelled: &[String],
        scope: Option<&crate::scope::JudgementScope>,
    ) -> Option<String> {
        if let Some(scope) = scope {
            return Some(scope.condition());
        }
        if unmodelled.is_empty() || matches!(self, Self::Unmodelled(_)) {
            return None;
        }
        Some(format!(
            "{} key {} no usable model ({}), so the rest of the board is not vouched for.",
            unmodelled.len(),
            if unmodelled.len() == 1 {
                "part has"
            } else {
                "parts have"
            },
            refs_summary(unmodelled)
        ))
    }
}

/// The one machine verdict every human surface of a run must agree with.
///
/// Derived once, by [`JsonReport::run_verdict`] (the same derivation that fills
/// the `verdict` and `verdict_reason` fields of `--json`), and handed to the
/// triage headline and the closing `VERDICT:` line so neither can re-derive a
/// different answer from the findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunVerdict {
    /// `"pass"` | `"fail"` | `"invalid"`, exactly as `--json` states it.
    pub verdict: &'static str,
    /// Why it is not a pass and what to do next ([`VerdictCause::reason`], plus
    /// the unmodelled-parts caveat when the run failed over unmodelled parts).
    pub reason: Option<String>,
    /// The assumptions a pass rests on (empty unless the verdict is a pass).
    pub conditions: Vec<String>,
    /// PARTLY JUDGED: nothing was found among the checks unmodelled key parts
    /// cannot change ([`crate::scope::JudgementScope`]). Never a pass: the
    /// verdict is `invalid` and the reason starts `PARTLY JUDGED`.
    pub partial: bool,
    /// What a failing (or refused) run did not judge, when key parts have no
    /// usable model: the sentence `reason` ends with, after its own. A surface
    /// that lists the failures itself states this once, on its own line,
    /// beside them.
    pub not_judged: Option<String>,
    /// How many items the order triage lists to inspect before ordering, on a
    /// pass (0 otherwise). A pass with any is not "clean" and not plainly "OK
    /// to order": every surface tags it [`Self::inspect_tag`] from this one
    /// count, so the closing `VERDICT:` line and the web headline agree.
    pub inspect: usize,
    /// Set on a pass over an input that is not a board (a schematic): what the
    /// input is, what was checked, and what to run to judge ordering. Such a
    /// pass is never "OK TO ORDER" or "clean", because nothing about ordering
    /// was judged; every surface opens with
    /// [`hauksbee_ir::vocabulary::NOT_ORDERABLE_LEAD`] and this sentence instead.
    /// The sentence is also one of `conditions`, so the machine document
    /// carries it too.
    pub unorderable: Option<String>,
}

impl Default for RunVerdict {
    fn default() -> Self {
        Self {
            verdict: "pass",
            reason: None,
            conditions: Vec::new(),
            partial: false,
            not_judged: None,
            inspect: 0,
            unorderable: None,
        }
    }
}

impl RunVerdict {
    /// Whether the run passed (the `ok` field of `--json`).
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.verdict == "pass"
    }

    /// How bad the verdict is, for taking the worst of several: a failure
    /// outranks a run that could not be judged, which outranks a pass.
    const fn severity(&self) -> u8 {
        match self.verdict.as_bytes() {
            b"fail" => 2,
            b"pass" => 0,
            _ => 1,
        }
    }

    /// The one verdict of a run that judged the same board twice, once
    /// statically (`self`) and once by co-simulating it (`later`): the worst of
    /// the two, never softer than either.
    ///
    /// A pass is only as good as both passes, and rests on both passes'
    /// conditions. When only one is not a pass, its words stand. When both
    /// are, the static one leads and the other follows after the same joiner
    /// a failing verdict uses for what it did not judge, unless the two say
    /// the same thing (both refuse over the same unmodelled parts).
    #[must_use]
    pub fn combine(&self, later: &Self) -> Self {
        let (first, second) = if later.severity() > self.severity() {
            (later, self)
        } else {
            (self, later)
        };
        if first.severity() == 0 {
            let mut conditions = self.conditions.clone();
            for condition in &later.conditions {
                if !conditions.contains(condition) {
                    conditions.push(condition.clone());
                }
            }
            return Self {
                conditions,
                inspect: self.inspect + later.inspect,
                unorderable: self
                    .unorderable
                    .clone()
                    .or_else(|| later.unorderable.clone()),
                ..Self::default()
            };
        }
        let reason = match (&first.reason, &second.reason) {
            (Some(lead), Some(follow)) if second.severity() > 0 && lead != follow => {
                Some(format!("{lead}{NOT_JUDGED_JOINER}{follow}"))
            }
            (reason, _) => reason.clone(),
        };
        Self {
            verdict: first.verdict,
            reason,
            conditions: Vec::new(),
            partial: first.partial,
            not_judged: first.not_judged.clone(),
            inspect: 0,
            unorderable: None,
        }
    }

    /// "inspect before ordering (N items)" for a pass with items the triage
    /// lists to inspect, in the triage bucket's own words; `None` when there
    /// are none (or the run did not pass).
    #[must_use]
    pub fn inspect_tag(&self) -> Option<String> {
        (self.is_pass() && self.inspect > 0).then(|| {
            format!(
                "{} ({} item{})",
                hauksbee_ir::vocabulary::INSPECT_BEFORE_ORDERING.to_lowercase(),
                self.inspect,
                if self.inspect == 1 { "" } else { "s" }
            )
        })
    }

    /// The pass conditions other than `sentence` (the unorderable-input
    /// sentence, which a headline and the `VERDICT:` line lead with instead).
    #[must_use]
    pub fn conditions_without(&self, sentence: &str) -> Vec<&str> {
        self.conditions
            .iter()
            .map(String::as_str)
            .filter(|c| *c != sentence)
            .collect()
    }

    /// The line a failing run prints once, after its failures: the lead, what
    /// was not judged, and (when that sentence does not already) the way to
    /// judge it. `None` for any other verdict.
    #[must_use]
    pub fn not_judged_line(&self) -> Option<String> {
        let said = self
            .not_judged
            .as_deref()
            .filter(|_| self.verdict == "fail")?;
        let next = if said.contains(DRAFT_MODELS_NEXT_STEP) {
            String::new()
        } else {
            format!(" {DRAFT_MODELS_NEXT_STEP}")
        };
        Some(format!("{NOT_JUDGED_LEAD}: {said}{next}"))
    }

    /// [`Self::reason`] without the sentence naming what was not judged, for a
    /// surface that states that sentence on its own line ([`Self::not_judged`]).
    #[must_use]
    pub fn reason_without_not_judged(&self) -> Option<&str> {
        let reason = self.reason.as_deref()?;
        let Some(said) = self.not_judged.as_deref() else {
            return Some(reason);
        };
        let tail = format!("{NOT_JUDGED_JOINER}{said}");
        Some(reason.strip_suffix(tail.as_str()).unwrap_or(reason))
    }
}

impl<'a> From<&'a Refusal> for VerdictCause<'a> {
    fn from(r: &'a Refusal) -> Self {
        Self::Refused {
            claim: &r.claim,
            missing: &r.missing_prerequisite,
            next: &r.next_action,
        }
    }
}

/// [`JsonReport::verdict_with_waivers`]: `(ok, verdict, serious, actionable)`,
/// the findings a waiver covered, the reason sentence when not a pass,
/// whether the run is PARTLY JUDGED ([`RunVerdict::partial`]) and, for a
/// failing run, the sentence naming what it did not judge
/// ([`RunVerdict::not_judged`]).
type VerdictParts = (
    (bool, &'static str, usize, usize),
    Vec<crate::waiver::WaivedFinding>,
    Option<String>,
    bool,
    Option<String>,
);

/// The parts, other than the first's own, that the finding named by `first`
/// (its plain words) is repeated on: those gating findings sharing its group
/// key. Empty when `first` is none, matches no finding, or is on one part.
fn first_siblings<'a>(
    first: Option<(&str, &str)>,
    gating: impl Iterator<Item = &'a JsonFinding> + Clone,
) -> String {
    let Some((plain, _)) = first else {
        return String::new();
    };
    let Some(lead) = gating.clone().find(|f| f.plain == plain) else {
        return String::new();
    };
    let key = lead.group_key();
    let others: Vec<String> = gating
        .filter(|f| !std::ptr::eq(*f, lead) && f.group_key() == key)
        .flat_map(|f| f.refs.iter().cloned())
        .collect();
    refs_summary(&others)
}

/// The first few references, then a count of the rest (the one implementation,
/// [`hauksbee_ir::vocabulary::names_summary`], shared by every surface).
pub(crate) fn refs_summary(refs: &[String]) -> String {
    hauksbee_ir::vocabulary::names_summary(refs)
}

/// Version of the `run --json` document contract.
///
/// Covers `JsonReport` plus the `ok`/`verdict`/`serious_count`/`actionable_count`
/// rollup `to_json` prepends. Bump on a breaking change only; additive fields
/// keep it.
pub const RUN_REPORT_SCHEMA_VERSION: u32 = 3;

/// Exit code for a strict run whose analog co-sim tripped the failed-chunk abort.
///
/// A strict headless run (`--strict`) or hauksbee-ci must use it when the analog
/// co-sim tripped the consecutive-failed-chunk abort. Centralised so both entry
/// points resolve the same code ([`EXIT_INVALID_FOR_ANALYSIS`]) and a test can
/// assert the contract without spawning a process. `None` means the run is
/// analysable and the ordinary pass/fail exit codes apply.
#[must_use]
pub fn strict_analog_exit_code(analog_abort_tripped: bool) -> Option<i32> {
    analog_abort_tripped.then_some(EXIT_INVALID_FOR_ANALYSIS)
}

// ─────────────────────────────────────────────────────────────────────────────
// AC validity
// ─────────────────────────────────────────────────────────────────────────────

/// The magnitude floor in [`hauksbee_solve`]'s `bode()`.
///
/// `20*log10(1e-300)` clamps to exactly `-6000.0 dB` when a node has no signal
/// path. We treat every point being at (or below) this floor as "no path to
/// this net".
pub const AC_FLOOR_DB: f64 = -6000.0;

/// Whether an AC sweep (the `(freq, mag_db, phase)` triples for one net) is the
/// all-sentinel "no signal path" result. Empty input is also meaningless.
#[must_use]
pub fn ac_is_all_sentinel(bode: &[(f64, f64, f64)]) -> bool {
    !bode.is_empty() && bode.iter().all(|(_, db, _)| *db <= AC_FLOOR_DB + 1.0)
}

/// A validity verdict for an analysis that can be meaningless: AC and thermal.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Validity {
    pub valid: bool,
    /// Set only when `valid` is false: the named reason, listing offending refs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Present on every exit-3 validity result. `reason` remains as a compact
    /// compatibility alias for `missing_prerequisite`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

impl Validity {
    #[must_use]
    pub const fn valid() -> Self {
        Self {
            valid: true,
            reason: None,
            refusal: None,
        }
    }
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self {
            valid: false,
            reason: Some(reason.into()),
            refusal: None,
        }
    }

    #[must_use]
    pub fn refused(refusal: Refusal) -> Self {
        Self {
            valid: false,
            reason: Some(refusal.missing_prerequisite.clone()),
            refusal: Some(refusal),
        }
    }
}

/// Build the "no signal path" reason naming the unresolved driving ICs. Used by
/// both AC (no path) and the warning text.
#[must_use]
pub fn no_signal_path_reason(net: &str, summary: &BindSummary) -> String {
    let drivers: Vec<String> = summary
        .active_ic_unresolved()
        .map(|u| {
            if u.value.is_empty() {
                u.reference.clone()
            } else {
                format!("{} ({})", u.reference, u.value)
            }
        })
        .collect();
    if drivers.is_empty() {
        format!("no signal path to {net}: the driving devices on this net are open or unresolved")
    } else {
        format!(
            "no signal path to {net}; unresolved driving ICs: {}",
            hauksbee_ir::vocabulary::names_summary(&drivers)
        )
    }
}

/// Thermal validity: invalid when no resolved dissipating device made the table.
///
/// Invalid only when that holds *and* the board has unresolved active parts that
/// would have dissipated. The `dissipating_rows` is the count of devices that
/// produced a finite peak temp.
#[must_use]
pub fn thermal_validity(dissipating_rows: usize, summary: &BindSummary) -> Validity {
    if dissipating_rows > 0 {
        return Validity::valid();
    }
    // Zero rows. If there are active ICs left OPEN on the live circuit, whether
    // UNRESOLVED (no model) or RESOLVED-BUT-OPEN (bound but with an open/undriven
    // pin); the table is empty because those dissipating devices are open, not
    // because the board runs cool. Both cases make the thermal result equally
    // untrustworthy, so both must escalate to invalid (exit 3); testing only the
    // unresolved case let a resolved-but-open power IC report a false "runs cool"
    // pass. Mirrors render_banner / thermal_coverage, which OR the two.
    if summary.active_ics_unresolved() || summary.active_open_on_live_circuit() {
        let mut refs: Vec<String> = summary
            .active_ic_unresolved()
            .map(|u| u.reference.clone())
            .collect();
        refs.extend(
            summary
                .resolved_but_open_active
                .iter()
                .filter(|u| u.active_ic)
                .map(|u| u.reference.clone()),
        );
        let refs = hauksbee_ir::vocabulary::names_summary(&refs);
        Validity::refused(Refusal::new(
            "a thermal-safety conclusion for the board",
            format!(
                "no resolved dissipating devices reached the thermal table; active ICs are open on the live circuit: {refs}"
            ),
            vec!["board extraction and component binding completed"],
            format!(
                "bind {refs} with --models-dir, then rerun the same --thermal command"
            ),
        ))
    } else {
        // Genuinely no dissipating devices and nothing unresolved, a valid (if
        // boring) result, e.g. a passive-only board at ambient.
        Validity::valid()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-check COVERAGE (distinct from Validity); the honest "N of M" annotation
// ─────────────────────────────────────────────────────────────────────────────

/// Coverage is distinct from [`Validity`].
///
/// `Validity` stays BINARY and drives exit 3. `CheckCoverage` is the honest "N of
/// M active parts actually entered this check" metric: it is ALWAYS emittable and
/// NEVER changes the exit code on its own (the partial-coverage thermal
/// escalation is applied by the thermal report, by default; `--no-strict-thermal`
/// opts out). `partial == true` means a renderer should print a coverage caveat
/// even though the check itself produced rows.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CheckCoverage {
    /// Covered active ICs / total active ICs, clamped to `[0, 1]`: how much of the
    /// active circuit the check actually modelled. `1.0` when every active IC is
    /// covered (or there are none, vacuously complete); low when power ICs are
    /// open/unresolved.
    pub resolved_fraction: f64,
    /// Devices that actually produced a row in the check's table.
    pub dissipating_count: usize,
    /// Active ICs on the live circuit that the check should have covered.
    pub total_active_count: usize,
    /// Active ICs left open on the live circuit (unresolved + resolved-but-open).
    pub open_active_on_live_circuit: usize,
    /// True => the renderer should surface a coverage caveat. Set when at least
    /// one row exists but an active IC on the live circuit is open/unresolved.
    pub partial: bool,
}

/// Thermal coverage, parallel to [`thermal_validity`].
///
/// Returns `partial = true` when there ARE dissipating rows yet an active IC on
/// the live circuit is open or unresolved, i.e. the table is real but incomplete
/// (rows exist only because some passives/parts resolved while a power IC is
/// open). The thermal report escalates the partial case to exit 3 by default
/// (`--no-strict-thermal` opts out); `thermal_validity` itself is unchanged.
#[must_use]
pub fn thermal_coverage(dissipating_rows: usize, summary: &BindSummary) -> CheckCoverage {
    let open_unresolved = summary
        .active_path_unresolved
        .iter()
        .filter(|u| u.active_ic)
        .count();
    let open_resolved = summary
        .resolved_but_open_active
        .iter()
        .filter(|u| u.active_ic)
        .count();
    let open_active = open_unresolved + open_resolved;
    let total_active = summary.critical_parts_total;
    // resolved_fraction is COVERED active ICs / total active ICs, NOT dissipating
    // rows / active ICs. Dividing the (mostly-passive) dissipating-row count by the
    // active-IC count is meaningless and inverts the signal: it read 1.0 on a board
    // with every power IC open (40 passive rows / 7 ICs, clamped) and 0.0 on a clean
    // board with one IC and no separate dissipating rows. A consumer thresholding
    // resolved_fraction would then pass the worst board and flag the good one.
    let resolved_fraction = if total_active == 0 {
        1.0
    } else {
        let covered_active = total_active.saturating_sub(open_active);
        #[allow(clippy::cast_precision_loss, reason = "IC counts never reach 2^52")]
        (covered_active as f64 / total_active as f64).clamp(0.0, 1.0)
    };
    let partial = dissipating_rows > 0
        && (summary.active_ics_unresolved() || summary.active_open_on_live_circuit());
    CheckCoverage {
        resolved_fraction,
        dissipating_count: dissipating_rows,
        total_active_count: total_active,
        open_active_on_live_circuit: open_active,
        partial,
    }
}

/// The active ICs (unresolved + resolved-but-open) on the live circuit that a
/// coverage caveat should name. Shared by the CLI text/plain/json renderers so
/// every surface names the SAME parts.
#[must_use]
pub fn coverage_open_active_refs(summary: &BindSummary) -> Vec<String> {
    summary
        .active_path_unresolved
        .iter()
        .filter(|u| u.active_ic)
        .chain(
            summary
                .resolved_but_open_active
                .iter()
                .filter(|u| u.active_ic),
        )
        .map(|u| u.reference.clone())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// The INCONCLUSIVE verdict vocabulary (one dialect for every static surface)
// ─────────────────────────────────────────────────────────────────────────────

/// The current-carrying / active parts whose missing models make a static verdict vacuous.
///
/// The [`coverage_open_active_refs`] set (active ICs that are unresolved or
/// resolved-but-open on the live circuit) plus unresolved discrete transistors
/// (`Q` prefix) on the live circuit, the power FETs on a protection path. A clean
/// `--lint`/`--si` result over these parts is not a clean bill, and the verdict
/// line must say so instead of "Looks healthy" (`PROCESS_AND_UX_LOG` item 4: a
/// vacuous pass on a BMS protection path is the most dangerous UX failure here).
///
/// Passives are deliberately excluded: an unbound connector or resistor does
/// not blind these checks the way an open driver or FET does, and flagging
/// every unresolved part would cry wolf on healthy boards.
/// The single rule for whether undermined evidence invalidates a RUN-LEVEL
/// claim: an undermined map counts unless it backs an individual finding
/// (`is_finding_assertion` on its assertion text). Finding-backed maps carry
/// their own qualified/undermined badges on the report without vetoing a
/// judgement they never backed; without this split, one unparseable value
/// field anywhere on the board invalidated the whole run through whichever
/// informational note touched its net. Used by the JSON verdict and by the
/// `--strict` exit gates, so the exit code can never disagree with the
/// verdict field.
pub fn run_level_undermined(
    maps: &[hauksbee_ir::evidence::EvidenceMap],
    is_finding_assertion: impl Fn(&str) -> bool,
) -> bool {
    maps.iter()
        .any(|map| map.is_undermined() && !is_finding_assertion(map.assertion()))
}

/// One part the verdict counts as having no usable model.
#[derive(Debug, Clone, Copy)]
pub struct VerdictBlocker<'a> {
    pub part: &'a UnresolvedActive,
    /// True when the part DID resolve to a model card but sits open on the live
    /// circuit (an active IC with undriven pins); false when no card resolved.
    pub has_card: bool,
}

/// Every part the verdict counts as having no usable model, in reference order.
///
/// The single source behind the verdict's count and named parts and
/// behind `hauksbee models extract --unmodelled`, so the two cannot disagree.
#[must_use]
pub fn unmodelled_critical(summary: &BindSummary) -> Vec<VerdictBlocker<'_>> {
    let mut blockers: Vec<VerdictBlocker<'_>> = summary
        .active_path_unresolved
        .iter()
        .filter(|u| is_verdict_critical(u))
        .map(|part| VerdictBlocker {
            part,
            has_card: false,
        })
        .chain(
            summary
                .resolved_but_open_active
                .iter()
                .filter(|u| u.active_ic)
                .map(|part| VerdictBlocker {
                    part,
                    has_card: true,
                }),
        )
        .collect();
    blockers.sort_by_key(|b| crate::reports::bind::natural_ref_key(&b.part.reference));
    blockers.dedup_by(|a, b| a.part.reference == b.part.reference);
    blockers
}

#[must_use]
pub fn unmodelled_critical_refs(summary: &BindSummary) -> Vec<String> {
    unmodelled_critical(summary)
        .into_iter()
        .map(|b| b.part.reference.clone())
        .collect()
}

/// The one INCONCLUSIVE verdict sentence the lint/SI/check surfaces share.
///
/// It gives the count, the named parts, and the input that unlocks a conclusive
/// verdict. Their `--plain` verdict lines, default text summaries, the `--check`
/// closing verdict and their JSON coverage notes all render THIS string (the
/// closing verdict drops the leading tag it already states), so the vocabulary
/// cannot fork across those surfaces. Thermal and AC speak through their own
/// coverage caveats, which lead with the same INCONCLUSIVE tag. It never changes
/// an exit code on its own; the exit-code contract is documented in
/// docs/ci/CI.md.
#[must_use]
pub fn inconclusive_verdict(refs: &[String]) -> String {
    let list = hauksbee_ir::vocabulary::names_summary(refs);
    format!(
        "INCONCLUSIVE: {} current-carrying / active part(s) have no model ({list}), \
         so a clean result here is not a clean bill. Draft models from their \
         datasheets (`hauksbee models extract --unmodelled <board>`, \
         or the Extend button in `hauksbee serve`), scaffold one by hand \
         (`hauksbee models new`, then --models-dir), or supply BOM identity \
         (--bom) to make this verdict conclusive.",
        refs.len(),
    )
}

/// [`inconclusive_verdict`] for the web app, without terminal commands.
///
/// The same count and parts and the same refusal of a clean bill. The app puts
/// the ways to fix it on the section as buttons (`actions`), so the sentence
/// only says what is wrong.
#[must_use]
pub fn web_inconclusive_verdict(refs: &[String]) -> String {
    let list = hauksbee_ir::vocabulary::names_summary(refs);
    let count = refs.len();
    let (parts, have) = if count == 1 {
        ("part", "has")
    } else {
        ("parts", "have")
    };
    format!(
        "INCONCLUSIVE: {count} current-carrying or active {parts} {have} no model ({list}), \
         so a clean result here is not a clean bill."
    )
}

/// Whether an unresolved/open part on the live circuit blocks a conclusive
/// verdict: an active IC, or a discrete transistor. Shared by
/// [`unmodelled_critical_refs`] and the web front door so the CLI and browser
/// can never disagree about which parts forbid a clean bill.
pub(crate) fn is_verdict_critical(u: &UnresolvedActive) -> bool {
    u.active_ic || is_transistor_ref(&u.reference)
}

/// Whether a reference designator names a discrete transistor (prefix `Q`),
/// the current-carrying part class whose unbound absence blinds the static
/// checks without being an active IC.
fn is_transistor_ref(reference: &str) -> bool {
    if is_tool_generated_ref(reference) {
        return false;
    }
    let prefix: String = reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    prefix == "Q"
}

// ─────────────────────────────────────────────────────────────────────────────
// Info-level notes + machine-readable co-sim summary
// ─────────────────────────────────────────────────────────────────────────────

/// An always-emitted, info-level note: the structured home for honesty
/// annotations that must never be silently absent (bind roles, co-sim
/// substitution, coverage caveats, SI info, boot hazards).
///
/// Distinct from [`JsonFinding`]: a note carries no severity and no gating flag,
/// nothing counts it into `serious_count`, and no gate reads this array. But
/// "informational" is about the note, NOT about what it reports: 3 of the 5
/// [`JsonNoteKind`] variants describe a condition a gate reads from its own
/// source, so seeing one of those does not mean the run exited 0.
///
/// - `Coverage` carries the INCONCLUSIVE sentence for unbound verdict-critical
///   parts (verdict `invalid`, exit 3 under `--strict`) and the `--thermal`
///   PARTIAL-coverage caveat (exit 3 BY DEFAULT, unless `--no-strict-thermal`).
/// - `CosimSubstitution` reports a substitute MCU core, which is an
///   `Undermined`-class assumption: on a run-level evidence map it makes the
///   verdict `invalid`, and `--strict` exits 3.
/// - `BootControlNet` reports the boot hazard `--strict-boot` escalates to a
///   failing gate (verdict `fail`, exit 2).
///
/// The other 2, `BindRole` and `SiInfo`, report conditions no gate reads: an
/// inferred pin role is a `Qualified`-class assumption, which never invalidates
/// a run, and the SI gate counts real findings only, never its info notes.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonNote {
    pub kind: JsonNoteKind,
    pub message: String,
}

/// One row of the boot-state panel.
///
/// A transistor gate and what the firmware does to it at power-up
/// (`driven_high` / `driven_low` / `floating`). Informational (reported, not
/// judged), so a consumer can read it without it being a verdict.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BootGateJson {
    pub reference: String,
    pub net: String,
    pub state: String,
}

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JsonNoteKind {
    BindRole,
    CosimSubstitution,
    Coverage,
    SiInfo,
    /// A boot-sequence hazard about a specific MCU control net: driven HIGH and
    /// held from reset, or left floating the whole run, with no bias resistor.
    /// A distinct kind (not `Coverage`) so CI can filter boot hazards on their
    /// own. Serializes as `"boot_control_net"`.
    BootControlNet,
}

/// Machine-readable co-sim summary (today only emitted as CLI text).
///
/// Populated from the headless run + the MCU binding. `substituted` is true when
/// the part the user asked for was modelled by a less-specific core (Track B).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimJson {
    pub mcu_ref: String,
    /// The actual backend string from `BindOutcome::Mcu { backend }`.
    pub backend: String,
    /// The part the board asked for (threaded through `McuBinding`, Track B).
    pub requested_part: String,
    /// True when the requested part != the modelled core.
    pub substituted: bool,
    /// Wall-clock seconds the headless co-sim loop consumed. 0 when the
    /// producing path did not measure (older producers; the web mirror).
    #[serde(default)]
    pub wall_s: f64,
    /// ACHIEVED realtime factor: sim seconds delivered per wall second,
    /// measured, never the requested window over an assumed wall. 0 when
    /// unmeasured.
    #[serde(default)]
    pub realtime_factor: f64,
    /// Total net toggles seen during the run. `0` => stalled/quiet.
    pub total_toggles: u64,
    pub uart_seen: bool,
    /// Numerical qualification for this exact run. Solver tolerances and
    /// methods are inputs, the residual is measured when available, and failed
    /// windows are explicitly invalid rather than assigned invented precision.
    pub error_budget: hauksbee_ir::evidence::ErrorBudget,
    /// Top-N nets by activity: name, toggle count, observed min/max voltage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_summary: Vec<NetActivity>,
    /// Measured edge timestamp precision and guaranteed pulse floor for every
    /// live MCU backend at the actual chunk used by this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_coverage: Vec<crate::scheduler::TimingCoverage>,
    /// Per-MCU evidence that the firmware executed: entry symbol reached,
    /// instructions and PC samples, faults, UART bytes, GPIO toggles per pin,
    /// and whether reset, supply and clock let it run. An MCU with
    /// `executed: false` never counts as a firmware pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution: Vec<crate::execution::ExecutionEvidence>,
    /// Runtime timing/replay limits reached. A strict run exits INVALID when
    /// this is non-empty instead of silently trusting a collapsed waveform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_refusals: Vec<String>,
    /// False once any chunk's analog solve failed to converge: the run held stale
    /// node voltages over `failed_windows` and cannot vouch for analog-derived
    /// findings there: it refuses rather than fakes them. A fully valid run reports
    /// `true` with an empty `failed_windows`, so the common shape is unchanged and
    /// existing consumers keep parsing.
    pub analog_valid: bool,
    /// Sim-time windows `[start_s, end_s)` where the analog solve failed. Empty
    /// (and omitted from JSON) on a valid run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_windows: Vec<CosimFailedWindow>,
    /// Sim-time windows solved by a per-chunk FALLBACK integration rung after
    /// the primary solve failed there, each carrying the method that produced
    /// it and that method's qualitative fidelity note. These windows are solved (they do
    /// not make the run analog-invalid) but their numbers are second-class:
    /// a consumer reading a waveform is entitled to know which spans came from
    /// a more dissipative method. Empty (and omitted) when the primary path
    /// carried the whole run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_windows: Vec<CosimFallbackWindow>,
    /// Per-SPI-bus transaction-framing tier: "exact" (framed on real CS GPIO
    /// edges), "backend" (the emulator frames CS itself), or "heuristic" (the
    /// chunk-boundary fallback, honest about its two documented failure
    /// modes). Empty (and omitted) when the board has no SPI buses; mirrors
    /// the web co-sim section so the CLI JSON carries the same coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spi_framing: Vec<CosimSpiFraming>,
    /// ADC channels whose per-chunk injections the MCU backend DROPPED because
    /// the platform carries no injection map (U3 finding 1). Non-empty means
    /// the analog solve drove these nets but the firmware NEVER received a
    /// sample, analog readings on those pins are meaningless, and this run
    /// cannot vouch for them. Empty (and omitted) on full coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adc_dropped: Vec<CosimAdcDrop>,
    /// Bus peripherals (I2C/SPI slave models) bound on a platform whose MCU
    /// backend models no matching bus controller (U3 finding 2): the firmware
    /// never exercised them, so their state is the power-on default. Empty
    /// (and omitted) when every bound bus device sits on a modeled controller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexercised_buses: Vec<CosimUnexercisedBus>,
    /// Firmware GPIO pulses that rose and fell inside one solver chunk on a
    /// net clocking a TICK-evaluated sequential part (friction 1.16): those
    /// parts sample once per chunk against the previous solve, so the pulse
    /// was NEVER observed by them and their state in this run lags or misses
    /// events, while chain-responder parts on the same board saw every edge
    /// exactly. One entry per offending net. Empty (and omitted) on a run with
    /// no such pulses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub short_pulses: Vec<CosimShortPulse>,
    /// Runtime driver contention: nets where the firmware configured an MCU
    /// pin as a push-pull OUTPUT while an enabled modelled push-pull output
    /// was already driving (the model-vs-MCU case the static output-contention
    /// lint documents as out of its reach). Waveforms touching these nets are
    /// untrustworthy from `t_s` on. One entry per offending net. Empty (and
    /// omitted) on a healthy run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub driver_contention: Vec<CosimDriverContention>,
    /// Per-MCU statements of how this backend's watchdog fidelity falls short of
    /// the part. Non-empty means an armed, never-fed watchdog does NOT reboot
    /// the core here the way silicon does, so firmware that HANGS runs forever
    /// in simulation and every assertion about behaviour after a hang is
    /// fiction: the run reads healthy while proving nothing about the recovery
    /// path. Empty (and omitted) on a backend whose watchdog behaves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchdog_limitations: Vec<CosimWatchdogLimitation>,
    /// Per-MCU counts of reboots an unserviced watchdog actually performed
    /// during this run. Not an error, a FINDING: an assertion that passed across
    /// a reboot was measuring a rebooted core, not the run it claimed. Empty
    /// (and omitted) when nothing rebooted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchdog_resets: Vec<CosimWatchdogResets>,
    /// Per-MCU statements of how this backend's TIMING fidelity falls short of
    /// the part. Non-empty means simulated time on these cores carries a known
    /// systematic bias (wall-clock-paced virtual time on the QEMU family, the
    /// F103's deliberate TIMx-at-72MHz divergence), so time-based assertions
    /// there mean less than they look. Empty (and omitted) on cores whose
    /// timing is clock-truth gated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_limitations: Vec<CosimTimingLimitation>,
    /// What the run looked like from the bench: per-pin state (static or
    /// toggling at f with duty), where each core is parked, the serial lines,
    /// the I2C and SPI transactions with their acknowledgements, and the event
    /// timeline (resets, stalls, faults). The same view the CLI prints and the
    /// app strip draws. Absent when no MCU ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<hauksbee_ir::observations::BenchReport>,
}

/// One backend watchdog-fidelity gap (see [`CosimJson::watchdog_limitations`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimWatchdogLimitation {
    pub mcu_ref: String,
    /// The backend's whole sentence, for verbatim display. Prose for a human:
    /// do not parse it or match on it exactly.
    pub limitation: String,
}

/// One backend timing-fidelity gap (see [`CosimJson::timing_limitations`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimTimingLimitation {
    pub mcu_ref: String,
    /// The backend's whole sentence, for verbatim display. Prose for a human:
    /// do not parse it or match on it exactly.
    pub limitation: String,
}

/// One MCU's watchdog reboot count (see [`CosimJson::watchdog_resets`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimWatchdogResets {
    pub mcu_ref: String,
    /// Reboots observed during the run. Always >= 1: a zero is omitted.
    pub resets: u64,
}

/// One sub-chunk GPIO pulse warning (see [`CosimJson::short_pulses`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimShortPulse {
    pub net: String,
    pub mcu_ref: String,
    /// The driving pin, e.g. `"PB1"`.
    pub pin: String,
    /// Narrowest completed pulse observed on the net (seconds).
    pub pulse_s: f64,
    /// The solver chunk it fell inside (seconds).
    pub chunk_s: f64,
    /// Tick-evaluated sequential parts clocked by the net.
    pub parts: Vec<String>,
}

/// One runtime driver-contention finding (see [`CosimJson::driver_contention`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimDriverContention {
    pub net: String,
    pub mcu_ref: String,
    /// The firmware-driven pin, e.g. `"PB1"`.
    pub pin: String,
    /// `"REF.role"` of every enabled modelled push-pull output on the net.
    pub parts: Vec<String>,
    /// Sim time (s) at which both sides were first seen driving together.
    pub t_s: f64,
}

/// One dropped ADC injection channel (see [`CosimJson::adc_dropped`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimAdcDrop {
    pub mcu_ref: String,
    pub channel: u8,
    pub net: String,
    /// Nearby part names on the net (best-effort, may be empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<String>,
}

/// One never-exercised bus peripheral (see [`CosimJson::unexercised_buses`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimUnexercisedBus {
    pub id: String,
    /// `"I2C"` or `"SPI"`.
    pub bus: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
}

/// One SPI bus's framing tier (see [`CosimJson::spi_framing`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimSpiFraming {
    pub bus: String,
    pub mode: String,
    /// On `mode = "exact"`, where the chip-select came from: `"spec"` (the run
    /// spec's `cs_net`), `"model-roles"` (the `cs` pin role of the bound model for
    /// the board component the peripheral names), or `"bitbang-pins"` (a
    /// bit-banged SPI slave, whose CS pin comes from the GPIO wiring the responder
    /// was attached with rather than from any net lookup). Absent on the `backend`
    /// and `heuristic` tiers, where no chip-select was resolved. The tier alone
    /// does not say this, and the routes fail differently, so a consumer
    /// reproducing a result needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cs_provenance: Option<String>,
}

/// One sim-time window `[start_s, end_s)` where the analog solve failed to converge.
///
/// The co-sim held stale voltages there. Reported so a consumer knows the exact
/// span that cannot be trusted rather than inferring a quiet run.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimFailedWindow {
    pub start_s: f64,
    pub end_s: f64,
    /// The solver's own refusal message for this window: the blame clause
    /// naming the net that refused to settle, the devices on it, and any
    /// near-zero-ohm link poisoning the matrix. Defaulted (and omitted
    /// from JSON) when empty, so an older consumer keeps parsing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// One sim-time window `[start_s, end_s)` whose answer came from a fallback rung.
///
/// The answer was produced by a per-chunk fallback integration rung after the
/// primary analog solve failed there. `method` is the rung's stable name
/// (`crate::scheduler::ChunkFallbackMethod::as_str`), `fidelity_note` names the
/// known algorithmic trade-off, and `error_estimate_v` is a MEASURED estimate, in
/// volts, of the chunk-end node-voltage error each chunk's march added relative
/// to its own chunk-start state, from a companion re-solve at a 4x-shifted
/// accuracy dial (tighter first, a coarser leg when the tight companion will not
/// converge), conservatively Richardson-scaled. Worst chunk of a merged window,
/// not a sum across it; omitted when no companion converged, never invented. The
/// record travels with the number it qualifies.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimFallbackWindow {
    pub start_s: f64,
    pub end_s: f64,
    pub method: String,
    pub fidelity_note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_estimate_v: Option<f64>,
    /// For a rung that discarded the carried chunk-start state (a cold
    /// start): the largest node-voltage jump, in volts, between the state the
    /// previous chunk ended in and the fresh operating point the rung marched
    /// from. The trajectory is DISCONTINUOUS by this much at the window
    /// start. Worst chunk of a merged window; omitted for a rung that
    /// continued the carried state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_jump_v: Option<f64>,
}

impl From<&crate::scheduler::FallbackWindow> for CosimFallbackWindow {
    fn from(window: &crate::scheduler::FallbackWindow) -> Self {
        Self {
            start_s: window.start_s,
            end_s: window.end_s,
            method: window.method.as_str().to_string(),
            fidelity_note: window.method.fidelity_note().to_string(),
            error_estimate_v: window.error_estimate_v,
            state_jump_v: window.state_jump_v,
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NetActivity {
    pub net: String,
    pub toggles: u64,
    pub v_min: f64,
    pub v_max: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// DRC grouping
// ─────────────────────────────────────────────────────────────────────────────

/// Where the clearance value applied by DRC came from.  A default is not a
/// design rule merely because the geometric check had to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearanceRuleSource {
    /// Netclass rules were read from the sibling `KiCad` project file.
    ProjectFileFound,
    /// No usable project rules were available, so the extractor's fallback was
    /// applied.  Renderers must never call this the user's rule.
    Defaulted,
    /// The importer for this format has no design-rule channel and used the
    /// tool fallback directly.
    ToolDefault,
    /// A non-KiCad layout format carried its own clearance value.
    BoardFile,
    /// A sibling `KiCad` custom-rules file supplied the report-wide value.
    CustomRulesFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CustomRuleScopeOmission {
    pub name: String,
    pub line_number: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub constraint_types: Vec<String>,
    /// Constraint types whose bounds contain a bare, unitless value. `KiCad`
    /// 10.0.5 deactivates the whole rules file, so this is disclosure, not a
    /// fidelity qualification on otherwise matching findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bare_value_constraint_types: Vec<String>,
}

// A custom rule the reader could not interpret. It was not applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CustomRuleNotUnderstood {
    pub line_number: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CustomRulesCoverage {
    pub file_name: String,
    /// `KiCad` 10.0.5 silently deactivates every custom rule in a file when any
    /// constraint bound omits its unit.
    pub file_inactive_due_to_bare_values: bool,
    pub unevaluated_rules: Vec<CustomRuleScopeOmission>,
    pub not_covered_rules: Vec<CustomRuleScopeOmission>,
    // Rules the reader could not interpret; each was left unchecked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_understood_rules: Vec<CustomRuleNotUnderstood>,
    // Set when the file as a whole could not be read, so none of it was checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable_reason: Option<String>,
    pub unsupported_constraint_counts: BTreeMap<String, usize>,
}

impl CustomRulesCoverage {
    #[must_use]
    pub fn from_parsed(file_name: String, parsed: &hauksbee_extract::KicadDruRules) -> Self {
        let describe = |rule: &hauksbee_extract::KicadDruRule| CustomRuleScopeOmission {
            name: rule.name.clone(),
            line_number: rule.line_number,
            condition: rule.condition.clone(),
            layer: rule.layer.clone(),
            constraint_types: rule
                .constraints
                .iter()
                .map(|constraint| constraint.kind.as_str().to_string())
                .collect(),
            bare_value_constraint_types: rule
                .bare_value_constraint_types()
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
        };
        let unevaluated_rules = parsed.unevaluated_rules().map(describe).collect();
        let not_covered_rules = parsed
            .rules
            .iter()
            .filter(|rule| {
                rule.constraints.iter().any(|constraint| {
                    constraint.kind != hauksbee_extract::KicadDruConstraintKind::Clearance
                })
            })
            .map(describe)
            .collect();
        let not_understood_rules = parsed
            .not_understood
            .iter()
            .map(|skipped| CustomRuleNotUnderstood {
                line_number: skipped.line_number,
                name: skipped.name.clone(),
                why: skipped.why.clone(),
            })
            .collect();
        Self {
            file_name,
            file_inactive_due_to_bare_values: parsed.has_bare_values(),
            unevaluated_rules,
            not_covered_rules,
            not_understood_rules,
            unreadable_reason: None,
            unsupported_constraint_counts: parsed.unsupported_constraint_counts.clone(),
        }
    }

    /// Coverage for a rules file that could not be read as a whole (broken
    /// syntax, an unsupported version): none of its rules were checked, but the
    /// run goes on with the project's own rules.
    #[must_use]
    pub const fn unreadable(file_name: String, why: String) -> Self {
        Self {
            file_name,
            file_inactive_due_to_bare_values: false,
            unevaluated_rules: Vec::new(),
            not_covered_rules: Vec::new(),
            not_understood_rules: Vec::new(),
            unreadable_reason: Some(why),
            unsupported_constraint_counts: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn qualifies_clearance_findings(&self) -> bool {
        // A rule that could not be read might have been a clearance rule.
        self.unreadable_reason.is_some()
            || !self.not_understood_rules.is_empty()
            || (!self.file_inactive_due_to_bare_values
                && self.unevaluated_rules.iter().any(|rule| {
                    rule.bare_value_constraint_types.is_empty()
                        && rule.constraint_types.iter().any(|kind| kind == "clearance")
                }))
    }

    #[must_use]
    #[allow(clippy::too_many_lines, reason = "one sentence per coverage state")]
    pub fn unevaluated_notice(&self) -> Option<String> {
        if self.unevaluated_rules.is_empty()
            && self.not_understood_rules.is_empty()
            && self.unreadable_reason.is_none()
        {
            return None;
        }
        let bare_rules = self
            .unevaluated_rules
            .iter()
            .filter(|rule| !rule.bare_value_constraint_types.is_empty())
            .collect::<Vec<_>>();
        let scoped_rules = self
            .unevaluated_rules
            .iter()
            .filter(|rule| rule.bare_value_constraint_types.is_empty())
            .collect::<Vec<_>>();
        let mut notice = String::new();

        if let Some(why) = &self.unreadable_reason {
            let _ = writeln!(
                notice,
                "CUSTOM RULES NOT EVALUATED: custom DRC rules file not understood; none of its rules were checked ({}: {why}).",
                self.file_name,
            );
        }
        if !self.not_understood_rules.is_empty() {
            let count = self.not_understood_rules.len();
            let _ = writeln!(
                notice,
                "CUSTOM RULES NOT EVALUATED: {count} rule{} in {} could not be read:",
                if count == 1 { "" } else { "s" },
                self.file_name,
            );
            for rule in &self.not_understood_rules {
                let name = rule
                    .name
                    .as_ref()
                    .map_or_else(String::new, |name| format!("{name:?} "));
                let _ = writeln!(
                    notice,
                    "  {name}(line {}): custom DRC rule not understood; that rule was not checked ({}).",
                    rule.line_number, rule.why,
                );
            }
        }

        if !bare_rules.is_empty() {
            if self.file_inactive_due_to_bare_values {
                let rule_count = self.unevaluated_rules.len();
                let first = bare_rules[0];
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES DISABLED: {} is not in force. Rule {:?} on line {} has a value with no unit, and KiCad discards the ENTIRE rules file when any value lacks one, so all {rule_count} rules in it are inactive and this board is being checked against netclass defaults instead. Add the unit to restore the rest.",
                    self.file_name,
                    first.name,
                    first.line_number,
                );
                for rule in bare_rules.into_iter().skip(1) {
                    let types = rule.bare_value_constraint_types.join(", ");
                    let _ = writeln!(
                        notice,
                        "  Additional unitless rule: {:?} (line {}, {types}).",
                        rule.name, rule.line_number,
                    );
                }
            } else {
                let count = bare_rules.len();
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES NOT EVALUATED: {count} bare-value rule{} in {} {} not applied:",
                    if count == 1 { "" } else { "s" },
                    self.file_name,
                    if count == 1 { "was" } else { "were" },
                );
                for rule in bare_rules {
                    let types = rule.bare_value_constraint_types.join(", ");
                    let _ = writeln!(
                        notice,
                        "  {:?} (line {}, {types}): this bare-value rule needs an explicit unit (for example, mm).",
                        rule.name, rule.line_number,
                    );
                }
            }
        }

        if !scoped_rules.is_empty() && !self.file_inactive_due_to_bare_values {
            let conditional = scoped_rules
                .iter()
                .all(|rule| rule.condition.is_some() && rule.layer.is_none());
            let count = scoped_rules.len();
            if conditional {
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES NOT EVALUATED: {count} rule{} in {} {} conditional and hauksbee cannot evaluate {} condition{} (recognised, not checked):",
                    if count == 1 { "" } else { "s" },
                    self.file_name,
                    if count == 1 { "is" } else { "are" },
                    if count == 1 { "its" } else { "their" },
                    if count == 1 { "" } else { "s" },
                );
            } else {
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES NOT EVALUATED: {count} scoped rule{} in {} cannot be applied report-wide (recognised, not checked):",
                    if count == 1 { "" } else { "s" },
                    self.file_name,
                );
            }
            for rule in scoped_rules {
                let scope = match (&rule.condition, &rule.layer) {
                    (Some(condition), Some(layer)) => format!(" when {condition} on {layer}"),
                    (Some(condition), None) => format!(" when {condition}"),
                    (None, Some(layer)) => format!(" on {layer}"),
                    (None, None) => String::new(),
                };
                let _ = writeln!(notice, "  {:?}{scope}", rule.name);
            }
        }

        if self.qualifies_clearance_findings() {
            notice.push_str(
                "Copper-clearance findings may therefore be judged against the wrong limit.",
            );
        } else if !self.file_inactive_due_to_bare_values
            && (!self.not_covered_rules.is_empty()
                || !self.unsupported_constraint_counts.is_empty())
        {
            notice.push_str(
                "These rules govern checks hauksbee does not report, so they do not qualify reported findings; they are listed in NOT COVERED.",
            );
        }
        Some(notice)
    }

    #[must_use]
    pub fn not_covered_summary(&self) -> Option<String> {
        if self.not_covered_rules.is_empty() && self.unsupported_constraint_counts.is_empty() {
            return None;
        }
        let rules = self
            .not_covered_rules
            .iter()
            .map(|rule| {
                format!(
                    "{:?} ({})",
                    rule.name,
                    rule.constraint_types
                        .iter()
                        .filter(|kind| kind.as_str() != "clearance")
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let unsupported = self
            .unsupported_constraint_counts
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Custom rules recognised, not checked in {}: {}{}{} Hauksbee has no hole-clearance, board-edge-clearance, or other listed custom-constraint checks; these valid rules are listed here, were not evaluated, and do not qualify reported copper-clearance findings.",
            self.file_name,
            rules,
            if rules.is_empty() || unsupported.is_empty() { "" } else { "; " },
            unsupported,
        ))
    }
}

/// The spacing floor (5 mil) applied when the board's own rules were not read.
///
/// A gap below a default rule but at or above this floor is no evidence of a
/// problem, so it is noted rather than raised as an issue.
pub const DEFAULT_RULE_SPACING_FLOOR_MM: f64 = 0.127;

/// Machine-readable provenance for a reported clearance value.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ClearanceRuleProvenance {
    pub source: ClearanceRuleSource,
    /// The default/report-wide value in millimetres. Pair-specific resolved
    /// values remain on each [`DrcGroup`] as `rule_mm`.
    pub value_mm: f64,
    /// Distinguishes a missing sibling from a present file whose rules could not
    /// be read. In both cases `source` is `defaulted`: invented evidence does not
    /// become project evidence merely because a file existed.
    pub project_file_found: bool,
    /// The netclass value replaced by a report-wide custom rule, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden_project_value_mm: Option<f64>,
    /// Parsed custom-rule coverage, including scopes Hauksbee did not evaluate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_rules: Option<CustomRulesCoverage>,
    /// The input is a board-file source for which no design rule was read at all
    /// (the clearance sweep never ran), as opposed to a rule that was read and
    /// happens to be small. Not part of the report document: it only picks the
    /// wording of [`Self::source_notice`].
    #[serde(skip)]
    pub no_rule_read: bool,
}

impl Default for ClearanceRuleProvenance {
    fn default() -> Self {
        Self::defaulted(0.0, false)
    }
}

impl ClearanceRuleProvenance {
    #[must_use]
    pub const fn project_file(value_mm: f64) -> Self {
        Self {
            source: ClearanceRuleSource::ProjectFileFound,
            value_mm,
            project_file_found: true,
            overridden_project_value_mm: None,
            custom_rules: None,
            no_rule_read: false,
        }
    }

    #[must_use]
    pub const fn defaulted(value_mm: f64, project_file_found: bool) -> Self {
        Self {
            source: ClearanceRuleSource::Defaulted,
            value_mm,
            project_file_found,
            overridden_project_value_mm: None,
            custom_rules: None,
            no_rule_read: false,
        }
    }

    #[must_use]
    pub const fn board_file(value_mm: f64) -> Self {
        Self {
            source: ClearanceRuleSource::BoardFile,
            value_mm,
            project_file_found: false,
            overridden_project_value_mm: None,
            custom_rules: None,
            no_rule_read: false,
        }
    }

    #[must_use]
    pub const fn tool_default(value_mm: f64) -> Self {
        Self {
            source: ClearanceRuleSource::ToolDefault,
            value_mm,
            project_file_found: false,
            overridden_project_value_mm: None,
            custom_rules: None,
            no_rule_read: false,
        }
    }

    #[must_use]
    pub const fn custom_rules_file(
        value_mm: f64,
        overridden_project_value_mm: Option<f64>,
        coverage: CustomRulesCoverage,
    ) -> Self {
        Self {
            source: ClearanceRuleSource::CustomRulesFile,
            value_mm,
            project_file_found: overridden_project_value_mm.is_some(),
            overridden_project_value_mm,
            custom_rules: Some(coverage),
            no_rule_read: false,
        }
    }

    /// Mark that no design rule was read from this input (see
    /// [`Self::no_rule_read`]). An explicit rule of any size, zero included,
    /// stays the board's own rule.
    #[must_use]
    pub const fn with_no_rule_read(mut self) -> Self {
        self.no_rule_read = true;
        self
    }

    #[must_use]
    pub fn with_custom_rules(mut self, coverage: CustomRulesCoverage) -> Self {
        self.custom_rules = Some(coverage);
        self
    }

    #[must_use]
    pub const fn is_defaulted(&self) -> bool {
        matches!(
            self.source,
            ClearanceRuleSource::Defaulted | ClearanceRuleSource::ToolDefault
        )
    }

    /// Whether the project rules file would change the answer: the check fell
    /// back to a default rule only because no `.kicad_pro` sat beside the board.
    /// A tool-default source (the importer reads no rules at all) is not helped
    /// by a project file, and nor is an input that carried no rule.
    #[must_use]
    pub const fn wants_project_rules(&self) -> bool {
        !self.no_rule_read
            && !self.project_file_found
            && matches!(self.source, ClearanceRuleSource::Defaulted)
    }

    /// A below-rule group measured only against a default rule whose tightest
    /// gap still meets [`DEFAULT_RULE_SPACING_FLOOR_MM`]: noted, not an issue.
    #[must_use]
    pub fn is_advisory(&self, group: &DrcGroup) -> bool {
        self.is_defaulted() && group.min_gap_mm >= DEFAULT_RULE_SPACING_FLOOR_MM - 1e-6
    }

    const fn missing_reason(&self) -> &'static str {
        if self.project_file_found {
            "the sibling .kicad_pro was found, but its netclass rules were not read"
        } else {
            "no .kicad_pro was found beside this board, so your netclass rules were not read"
        }
    }

    /// Qualification appended to every human rule reference.
    #[must_use]
    pub fn rule_reference(&self, value_mm: f64) -> String {
        match self.source {
            ClearanceRuleSource::ProjectFileFound => format!("your {value_mm:.3} mm rule"),
            ClearanceRuleSource::Defaulted => format!(
                "the {value_mm:.3} mm DEFAULT clearance ({}; these are default rules, not your rules)",
                self.missing_reason()
            ),
            ClearanceRuleSource::ToolDefault => format!(
                "the {value_mm:.3} mm TOOL DEFAULT clearance (the importer did not read design rules; these are default rules, not your rules)"
            ),
            ClearanceRuleSource::BoardFile => {
                format!("the board-file {value_mm:.3} mm clearance rule")
            }
            ClearanceRuleSource::CustomRulesFile => format!(
                "the {value_mm:.3} mm custom rule from {}",
                self.custom_rules
                    .as_ref()
                    .map_or("the sibling .kicad_dru", |coverage| coverage.file_name.as_str())
            ),
        }
    }

    /// Prominent source + unlocking-input sentence for a CLI report.
    #[must_use]
    pub fn source_notice(&self) -> String {
        // No rule was read at all (the sweep never ran, as for fabrication files
        // and netlists): whatever the source, nothing could be flagged, so it
        // must not read as a default or a rule the input carries.
        if self.no_rule_read {
            return "CLEARANCE RULE SOURCE: none; this input carries no clearance rule Hauksbee reads, so no copper spacing was measured.".to_string();
        }
        match self.source {
            ClearanceRuleSource::ProjectFileFound => format!(
                "CLEARANCE RULE SOURCE: sibling .kicad_pro (report-wide value {:.3} mm).",
                self.value_mm
            ),
            ClearanceRuleSource::Defaulted => format!(
                "CLEARANCE RULE SOURCE: DEFAULT {:.3} mm; {}; these are default rules, not your rules.",
                self.value_mm,
                self.missing_reason()
            ),
            ClearanceRuleSource::ToolDefault => format!(
                "CLEARANCE RULE SOURCE: TOOL DEFAULT {:.3} mm; this importer did not read design rules, so these are default rules, not your rules.",
                self.value_mm
            ),
            ClearanceRuleSource::BoardFile => format!(
                "CLEARANCE RULE SOURCE: clearance carried by the board file (report-wide value {:.3} mm).",
                self.value_mm
            ),
            ClearanceRuleSource::CustomRulesFile => {
                let file = self
                    .custom_rules
                    .as_ref()
                    .map_or("sibling .kicad_dru", |coverage| coverage.file_name.as_str());
                self.overridden_project_value_mm.map_or_else(|| format!(
                        "CLEARANCE RULE SOURCE: custom rules file {file} (report-wide value {:.3} mm).",
                        self.value_mm
                    ), |project| format!(
                        "CLEARANCE RULE SOURCE: custom rules file {file} (report-wide value {:.3} mm), which overrides the .kicad_pro netclass value of {project:.3} mm.",
                        self.value_mm
                    ))
            }
        }
    }

    pub fn custom_rules_notice(&self) -> Option<String> {
        self.custom_rules
            .as_ref()
            .and_then(CustomRulesCoverage::unevaluated_notice)
    }

    pub fn custom_rules_not_covered(&self) -> Option<String> {
        self.custom_rules
            .as_ref()
            .and_then(CustomRulesCoverage::not_covered_summary)
    }

    /// Prominent source + unlocking-input sentence for a CLI report.
    #[must_use]
    pub fn cli_notice(&self) -> String {
        let notice = self.source_notice();
        if self.source == ClearanceRuleSource::Defaulted && !self.no_rule_read {
            let unlock = if self.project_file_found {
                "Repair or replace the sibling .kicad_pro with readable netclass rules and rerun to check your netclass rules."
            } else {
                "Place the matching .kicad_pro next to the board and rerun to check your netclass rules."
            };
            format!("{notice} {unlock}")
        } else {
            notice
        }
    }

    /// Web-specific unlocking instruction: this path has upload bytes, not a
    /// sibling filesystem path.
    #[must_use]
    pub fn web_notice(&self) -> String {
        if self.no_rule_read {
            return self.cli_notice();
        }
        match self.source {
            ClearanceRuleSource::Defaulted if self.project_file_found => format!(
                "CLEARANCE RULE SOURCE: DEFAULT {:.3} mm; the uploaded .kicad_pro was present, but its netclass rules were not read; these are default rules, not your rules. Repair or replace that .kicad_pro and upload it alongside the board to check your netclass rules.",
                self.value_mm
            ),
            ClearanceRuleSource::Defaulted => format!(
                "CLEARANCE RULE SOURCE: DEFAULT {:.3} mm; this upload did not include a .kicad_pro, so your netclass rules were not read; these are default rules, not your rules. Upload the matching .kicad_pro alongside the board to check your netclass rules.",
                self.value_mm
            ),
            _ => self.cli_notice(),
        }
    }
}

/// A real short between two nets (gap <= 0: touching copper).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DrcShort {
    pub net_a: String,
    pub net_b: String,
    pub layer: String,
    pub gap_mm: f64,
    pub loc_mm: [f64; 2],
    /// "serious" for a validated-format short or an unvalidated-format short
    /// independently confirmed by `KiCad`'s own DRC. "note" remains for a
    /// tool-only unvalidated-format claim or where board-local physical
    /// authority qualifies this location. A companion Eagle schematic adds
    /// context but cannot change severity.
    pub severity: String,
    /// Human one-line description, mirroring `JsonFinding.plain` so every
    /// `--json` finding category reads uniformly (SI/lint already carry it).
    pub plain: String,
    /// Suggested remediation, mirroring `JsonFinding.fix`.
    pub fix: String,
    /// The parts whose pads form the contact, for the finding's `refs`.
    #[serde(skip)]
    pub refs: Vec<String>,
}

const ORACLE_AGREEMENT_MARKER: &str = "ORACLE AGREEMENT: ";
const TOOL_ONLY_FORMAT_MARKER: &str = "; TOOL-ONLY: ";

impl DrcShort {
    pub(crate) fn oracle_agreement(&self) -> Option<&str> {
        self.plain
            .split_once(ORACLE_AGREEMENT_MARKER)
            .map(|(_, agreement)| agreement)
    }

    /// Replace the unvalidated-format, tool-only qualification with the exact
    /// independent evidence that settled it. The caller owns pair matching;
    /// this method only keeps the finding's evidence sentence single-sourced.
    pub(crate) fn attach_oracle_agreement(&mut self, version: &str) {
        if let Some((claim, _)) = self.plain.split_once(TOOL_ONLY_FORMAT_MARKER) {
            self.plain = claim.to_string();
        }
        let _ = write!(
            self.plain,
            "; {ORACLE_AGREEMENT_MARKER}confirmed by KiCad's own DRC ({version}) for this \
             specific net pair; the coordinates shown are Hauksbee's reported contact location"
        );
    }
}

/// A group of clearance findings that share (`net_a`, `net_b`, layer, root cause).
///
/// Collapsed to one line with a count. `at_limit` separates `gap == rule` (no
/// margin, but not below) from genuine sub-clearance violations.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DrcGroup {
    pub net_a: String,
    pub net_b: String,
    pub layer: String,
    pub count: usize,
    /// How many of `count` are genuinely below the rule (gap < rule). The rest
    /// are exactly at the rule (no margin, not below).
    pub below_count: usize,
    /// True when every member sits exactly at the rule (gap == rule, no margin),
    /// false when at least one is genuinely *below* the rule.
    pub at_limit: bool,
    /// The tightest gap in the group (mm).
    pub min_gap_mm: f64,
    /// Board location (mm) of the tightest gap, so a UI can pan to the worst
    /// spot of the group.
    pub min_gap_loc_mm: [f64; 2],
    /// The clearance rule for this pair (mm).
    pub rule_mm: f64,
    /// What pair of copper objects the tightest gap is between ("via \u{2194}
    /// zone", "track \u{2194} pad"), so a reader knows what to look AT before
    /// panning to the coordinates. Diagnosability is the point: a via-related
    /// finding sends the reader to the via's layer settings, not to rerouting.
    pub between: String,
    /// Human one-line description (`self.label()`), serialized so `--json`
    /// consumers get the same text the human/`--plain` renderers show.
    pub plain: String,
    /// Suggested remediation, mirroring `JsonFinding.fix`.
    pub fix: String,
    /// The parts whose pads are in the group, for the finding's `refs`.
    #[serde(skip)]
    pub refs: Vec<String>,
}

/// `"3 locations"` / `"1 location"`: how many places one grouped clearance
/// finding covers. Shared so the expert label and the plain sentence count the
/// same way.
pub(crate) fn locations(count: usize) -> String {
    format!("{count} location{}", if count == 1 { "" } else { "s" })
}

impl DrcGroup {
    /// The honest one-line label. `gap == rule` is "exactly at minimum clearance
    /// (no margin)", NOT "below the spacing the board asks for" (which is only
    /// true when gap < rule). For a mixed group we name BOTH counts so we never
    /// overstate how many locations are actually below the rule.
    #[must_use]
    pub fn label(&self) -> String {
        self.label_with_rule_provenance(&ClearanceRuleProvenance::defaulted(self.rule_mm, false))
    }

    #[must_use]
    pub fn label_with_rule_provenance(&self, provenance: &ClearanceRuleProvenance) -> String {
        let loc = locations;
        let rule = provenance.rule_reference(self.rule_mm);
        if self.at_limit {
            format!(
                "{} vs {}: {} on {}, all exactly at minimum clearance (no margin) [{}]",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                rule
            )
        } else if self.below_count == self.count {
            format!(
                "{} vs {}: {} on {}, below {} (tightest {}, {})",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                rule,
                hauksbee_ir::units::format_gap_mm(self.min_gap_mm),
                self.between
            )
        } else {
            // Mixed: some below, the remainder exactly at the limit.
            format!(
                "{} vs {}: {} on {} ({} below {}, tightest {}; {} at the limit)",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                self.below_count,
                rule,
                hauksbee_ir::units::format_gap_mm(self.min_gap_mm),
                self.count - self.below_count,
            )
        }
    }
}

// A track of one net inside another net's stored zone fill, grouped by
// net pair and layer. Not a short: the fill is out of date and a refill clears
// it, which is the only action. Never gates.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct DrcStaleFill {
    pub zone_net: String,
    pub copper_net: String,
    pub copper_kind: String,
    pub layer: String,
    pub count: usize,
    pub loc_mm: [f64; 2],
    pub plain: String,
    pub fix: String,
}

/// (zone net, copper net, copper kind, layer): what one stale-fill group shares.
type StaleKey = (String, String, String, String);

/// The DRC report, restructured into honest buckets: real shorts kept verbatim,
/// clearance findings grouped, and `gap == rule` separated from `gap < rule`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DrcStructured {
    pub clearance_rule_mm: f64,
    pub primitive_count: usize,
    pub shorts: Vec<DrcShort>,
    // Tracks and vias inside another net's out-of-date zone fill; refilling
    // the zones clears them. Reported beside the shorts, never as one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_fills: Vec<DrcStaleFill>,
    /// Grouped clearance violations that are genuinely BELOW the rule.
    pub violations: Vec<DrcGroup>,
    /// Grouped findings that sit exactly AT the rule (no margin, not below).
    pub at_limit: Vec<DrcGroup>,
    /// Set when the board's format is newer than hauksbee's validated copper
    /// extraction (`KiCad` 10+), making tool-only shorts above unreliable.
    /// Surfaced by every renderer; CI gates ignore those tool-only shorts, but
    /// an exact net-pair match from `KiCad`'s own DRC restores a short's serious
    /// severity and gate status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_warning: Option<String>,
    /// Set when the run suppressed zone-versus-pad overlaps as antipad carves.
    /// "No shorts found" and "no shorts found, having skipped a whole class"
    /// are different claims, so every renderer states this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppression_note: Option<String>,
}

impl DrcStructured {
    #[must_use]
    pub fn from_report(report: &DrcReport) -> Self {
        Self::from_report_with_ties(report, None, false)
    }

    #[must_use]
    #[allow(clippy::too_many_lines, reason = "one grouping pass over DRC findings")]
    pub fn from_report_with_ties(
        report: &DrcReport,
        qualification: Option<&hauksbee_extract::DrcTieQualification>,
        missing_eagle_schematic: bool,
    ) -> Self {
        // Group clearance findings by (net_a, net_b, layer). Within a group we
        // split at_limit (gap >= rule, i.e. exactly at or, defensively, above)
        // from below-rule. A KiCad clearance finding has positive gap; "at limit"
        // means gap is within a hair of the rule.
        // (count, below_count, min_gap, rule, min_gap_loc, item kinds)
        type Group = (
            usize,
            usize,
            f64,
            f64,
            [f64; 2],
            (hauksbee_extract::ItemKind, hauksbee_extract::ItemKind),
            std::collections::BTreeSet<String>,
        );
        // The pad owners at one contact; tracks, vias and zones own none.
        let owners = |f: &hauksbee_extract::DrcFinding| {
            [&f.item_a.owner, &f.item_b.owner]
                .into_iter()
                .filter(|o| !o.is_empty())
                .cloned()
                .collect::<std::collections::BTreeSet<String>>()
        };
        let mut shorts = Vec::new();
        let mut groups: BTreeMap<(String, String, String), Group> = BTreeMap::new();
        let mut stale: BTreeMap<StaleKey, (usize, [f64; 2])> = BTreeMap::new();

        // On an unvalidated board format (KiCad 10+) the shorts may be phantom, so
        // they carry "note" severity, not "serious", every structured consumer
        // (JSON, TUI) inherits the downgrade from this single source.
        let phantom = report.version_warning.is_some();

        for f in &report.findings {
            let authorized_tie = qualification.and_then(|ties| ties.tie_for(f));
            let declared_tie = qualification.and_then(|ties| ties.declaration_for(f));
            match f.kind {
                // A short is serious unless something specific says otherwise:
                // the format was unvalidated, or board-local authority declares
                // this exact pair of nets deliberately tied. Both are per-finding
                // here rather than per-report, because board-local authority qualifies
                // only the contacts it covers and every other short on the same
                // board stays serious.
                ViolationKind::Short => {
                    // The geometry sentence is identical at every confidence:
                    // the nets share copper. A newer-than-validated format adds
                    // an explicit tool-only evidence boundary here, at the same
                    // demotion site that changes the severity. Pair-specific
                    // oracle agreement later replaces this sentence and restores
                    // the real severity; an unmatched finding keeps both.
                    let mut plain = declared_tie.map_or_else(
                        || {
                            format!(
                                "{} shorts {} on {} at ({:.2}, {:.2}) mm (gap {:.3} mm)",
                                f.net_a_name, f.net_b_name, f.layer, f.x, f.y, f.gap_mm
                            )
                        },
                        |tie| {
                            format!(
                                "{} and {} are joined in copper on {} at ({:.2}, {:.2}) mm \
                             (gap {:.3} mm). The schematic names this net pair ({}; {}), but \
                             does not identify or authorize this physical location",
                                f.net_a_name,
                                f.net_b_name,
                                f.layer,
                                f.x,
                                f.y,
                                f.gap_mm,
                                tie.declaration,
                                tie.source,
                            )
                        },
                    );
                    if phantom {
                        plain.push_str(
                            "; TOOL-ONLY: Hauksbee reports this contact from an unvalidated \
                             board format; KiCad's own check has not confirmed it",
                        );
                    }
                    shorts.push(DrcShort {
                        net_a: f.net_a_name.clone(),
                        net_b: f.net_b_name.clone(),
                        layer: f.layer.clone(),
                        gap_mm: f.gap_mm,
                        loc_mm: [f.x, f.y],
                        severity: if phantom || authorized_tie.is_some() {
                            "note".to_string()
                        } else {
                            "serious".to_string()
                        },
                        plain,
                        fix: match (declared_tie, missing_eagle_schematic) {
                        (Some(_), _) => "verify this exact join against board-local layout intent \
                             (for example a named net-tie footprint or reviewed coordinate), or \
                             separate the nets. The Eagle schematic names only the net pair and \
                             cannot prove where the physical join belongs."
                            .to_string(),
                        // No schematic was supplied and this format can declare
                        // ties in one. Name that upload rather than leaving the
                        // reader to guess what would settle it.
                        (None, true) => "separate the two nets' copper: widen the gap or reroute \
                             so the trace/pad spacing clears the clearance rule. If this contact \
                             is deliberate, supply the same-named Eagle .sch companion for net-pair \
                             context, then provide board-local layout authority for this location"
                            .to_string(),
                        (None, false) => "separate the two nets' copper: widen the gap or reroute \
                             so the trace/pad spacing clears the clearance rule"
                            .to_string(),
                        },
                        refs: owners(f).into_iter().collect(),
                    });
                }
                ViolationKind::StaleFill => {
                    // The zone is whichever item is the pour; the other is the
                    // track, via or pad that lies inside its stored fill.
                    let (zone_net, copper_net, copper) =
                        if f.item_a.kind == hauksbee_extract::ItemKind::Zone {
                            (&f.net_a_name, &f.net_b_name, &f.item_b)
                        } else {
                            (&f.net_b_name, &f.net_a_name, &f.item_a)
                        };
                    let copper_kind = match copper.kind {
                        hauksbee_extract::ItemKind::Via => "via",
                        hauksbee_extract::ItemKind::Pad => "pad",
                        _ => "track",
                    };
                    let e = stale
                        .entry((
                            zone_net.clone(),
                            copper_net.clone(),
                            copper_kind.to_string(),
                            f.layer.clone(),
                        ))
                        .or_insert((0, [f.x, f.y]));
                    e.0 += 1;
                }
                ViolationKind::Clearance => {
                    // A finding is "below" the rule when its gap is under it;
                    // "at limit" when gap == rule. The 1e-4 mm (100 nm) tolerance
                    // absorbs f64 representation noise at rule values like 0.200
                    // mm without misclassifying a genuine sub-micron violation as
                    // "at limit" (a gap 0.5 um short stays "below the rule").
                    let below = f.gap_mm < f.required_clearance_mm - 1e-4;
                    let key = (f.net_a_name.clone(), f.net_b_name.clone(), f.layer.clone());
                    let e = groups.entry(key).or_insert((
                        0,
                        0,
                        f64::INFINITY,
                        f.required_clearance_mm,
                        [f.x, f.y],
                        (f.item_a.kind, f.item_b.kind),
                        std::collections::BTreeSet::new(),
                    ));
                    e.0 += 1;
                    e.6.extend(owners(f));
                    if below {
                        e.1 += 1;
                    }
                    // Track the tightest gap AND where it is (and what pair of
                    // objects it is between), so the group can point a UI at
                    // its worst spot and the reader knows whether to look at a
                    // via, a track, or a zone fill.
                    if f.gap_mm < e.2 {
                        e.2 = f.gap_mm;
                        e.4 = [f.x, f.y];
                        e.5 = (f.item_a.kind, f.item_b.kind);
                    }
                    e.3 = f.required_clearance_mm;
                }
            }
        }

        let mut violations = Vec::new();
        let mut at_limit = Vec::new();
        for (
            (net_a, net_b, layer),
            (count, below_count, min_gap, rule, min_gap_loc, kinds, refs),
        ) in groups
        {
            let any_below = below_count > 0;
            let mut group = DrcGroup {
                net_a,
                net_b,
                layer,
                count,
                below_count,
                at_limit: !any_below,
                min_gap_mm: if min_gap.is_finite() { min_gap } else { rule },
                min_gap_loc_mm: min_gap_loc,
                rule_mm: rule,
                between: format!("{} \u{2194} {}", kinds.0.as_str(), kinds.1.as_str()),
                plain: String::new(),
                fix: String::new(),
                refs: refs.into_iter().collect(),
            };
            group.plain = group.label();
            group.fix = if any_below {
                "increase spacing or route clearance so the gap meets the \
                 clearance rule"
                    .to_string()
            } else {
                "no margin: widen the spacing above the rule for manufacturing \
                 tolerance"
                    .to_string()
            };
            if any_below {
                violations.push(group);
            } else {
                at_limit.push(group);
            }
        }

        let stale_fills = stale
            .into_iter()
            .map(
                |((zone_net, copper_net, copper_kind, layer), (count, loc_mm))| DrcStaleFill {
                    plain: hauksbee_ir::vocabulary::stale_zone_fill(
                        &zone_net,
                        &copper_net,
                        &copper_kind,
                    ),
                    fix: "refill all zones (Edit > Fill All Zones, key B) in KiCad, save the \
                          board and re-run; the stored fill was computed before this copper \
                          was routed or moved"
                        .to_string(),
                    zone_net,
                    copper_net,
                    copper_kind,
                    layer,
                    count,
                    loc_mm,
                },
            )
            .collect();
        Self {
            clearance_rule_mm: report.clearance_mm,
            primitive_count: report.primitive_count,
            shorts,
            stale_fills,
            violations,
            at_limit,
            version_warning: report.version_warning.clone(),
            suppression_note: report.suppression_note(),
        }
    }

    /// Attach provenance after the extractor has resolved its rule set. This
    /// also rebuilds every group label, preventing serialized `plain` text from
    /// retaining the constructor's conservative default qualification.
    #[must_use]
    pub fn with_clearance_rule_provenance(mut self, provenance: &ClearanceRuleProvenance) -> Self {
        for group in self.violations.iter_mut().chain(&mut self.at_limit) {
            group.plain = group.label_with_rule_provenance(provenance);
        }
        self
    }

    /// The combined-check section heading. A default must be visible before a
    /// reader reaches any individual finding.
    #[must_use]
    pub const fn section_title(&self, provenance: &ClearanceRuleProvenance) -> &'static str {
        if provenance.is_defaulted() {
            "Copper spacing (DRC; DEFAULT rules, not your rules)"
        } else {
            "Copper spacing (DRC)"
        }
    }

    /// The groups a reader should act on: advisory default-rule groups (see
    /// [`ClearanceRuleProvenance::is_advisory`]) are left out. The JSON keeps
    /// every group; the human surfaces and the actionable count read this.
    #[must_use]
    pub fn actionable_view(&self, provenance: &ClearanceRuleProvenance) -> Self {
        let mut view = self.clone();
        view.violations.retain(|g| !provenance.is_advisory(g));
        view
    }

    /// Below-rule groups that count toward `actionable_count`. Without a
    /// recorded rule source every group counts.
    #[must_use]
    pub fn actionable_group_count(&self, provenance: Option<&ClearanceRuleProvenance>) -> usize {
        self.violations
            .iter()
            .filter(|g| !provenance.is_some_and(|p| p.is_advisory(g)))
            .count()
    }

    /// The default-rule lines a renderer prints before any finding.
    #[must_use]
    pub fn default_rule_notes(&self, provenance: &ClearanceRuleProvenance) -> Vec<String> {
        [
            self.default_rule_flood_note(provenance),
            self.default_rule_advisory_note(provenance),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// One line accounting for the advisory groups, so leaving them out of
    /// the issue list is stated rather than silent.
    #[must_use]
    pub fn default_rule_advisory_note(
        &self,
        provenance: &ClearanceRuleProvenance,
    ) -> Option<String> {
        let advisory: Vec<&DrcGroup> = self
            .violations
            .iter()
            .filter(|g| provenance.is_advisory(g))
            .collect();
        let tightest = advisory.iter().map(|g| g.min_gap_mm).reduce(f64::min)?;
        Some(format!(
            "DEFAULT-RULE NOTE: {} net pair(s) are closer than the {:.3} mm default (tightest {}) but not closer than {DEFAULT_RULE_SPACING_FLOOR_MM:.3} mm (5 mil); without your rules that is no evidence of a problem, so they are not listed as issues.",
            advisory.len(),
            self.clearance_rule_mm,
            hauksbee_ir::units::format_gap_mm(tightest),
        ))
    }

    /// The flood warning is evidence about this run, not a guess at the correct
    /// replacement rule. "Exceeds" is strict: exactly 10x does not qualify.
    #[must_use]
    pub fn default_rule_flood_note(&self, provenance: &ClearanceRuleProvenance) -> Option<String> {
        let clearance = self.violations.len();
        let shorts = self.shorts.len();
        (provenance.is_defaulted() && clearance > shorts.saturating_mul(10))
        .then(|| {
            format!(
                "DEFAULT-RULE COUNT: {clearance} below-rule clearance group(s) versus {shorts} short(s) (ratio {clearance}:{shorts}); exceeding the short count by more than 10x is evidence that this default is probably wrong for this board."
            )
        })
    }

    /// Render the grouped DRC as text (the honest, de-duplicated view). Shorts
    /// first (the things that actually break a board), then below-rule groups,
    /// then the at-limit bucket (separated and labelled correctly). The only
    /// text rendering of a DRC result; `provenance` decides whether the header
    /// announces the clearance as the project's rule or as a tool default.
    #[must_use]
    pub fn render_with_clearance_rule_provenance(
        &self,
        provenance: &ClearanceRuleProvenance,
    ) -> String {
        let mut s = String::new();
        if provenance.is_defaulted() {
            let _ = writeln!(
                s,
                "DRC (DEFAULT rules, not your rules): {} primitive(s), DEFAULT clearance {:.3} mm",
                self.primitive_count, self.clearance_rule_mm
            );
        } else {
            let _ = writeln!(
                s,
                "DRC: {} primitive(s), clearance rule {:.3} mm",
                self.primitive_count, self.clearance_rule_mm
            );
        }
        let _ = writeln!(s, "{}", provenance.cli_notice());
        if let Some(notice) = provenance.custom_rules_notice() {
            let _ = writeln!(s, "{notice}");
        }
        for note in self.default_rule_notes(provenance) {
            let _ = writeln!(s, "{note}");
        }
        if let Some(w) = &self.version_warning {
            let _ = writeln!(s, "\n⚠ UNRELIABLE: {w}");
        }
        // Before any "clean" claim: a suppressed class is not a checked class.
        if let Some(n) = &self.suppression_note {
            let _ = writeln!(s, "\nNOT CHECKED: {n}");
        }
        if self.shorts.is_empty()
            && self.stale_fills.is_empty()
            && self.violations.is_empty()
            && self.at_limit.is_empty()
        {
            let _ = writeln!(s, "no shorts or clearance violations.");
            return s;
        }
        if !self.shorts.is_empty() {
            let _ = writeln!(s, "\nSHORTS ({}):", self.shorts.len());
            for sh in &self.shorts {
                // Honor the final per-short severity from the structured form:
                // an unmatched unvalidated-format short reads "NOTE", while an
                // exact KiCad-oracle pair match is restored to "SERIOUS".
                let tag = if sh.severity == "serious" {
                    "SERIOUS"
                } else {
                    "NOTE"
                };
                // The measurement is printed identically whatever the severity:
                // a declared tie is still copper touching, and hiding or
                // softening the geometry would be the opposite dishonesty to
                // calling it a defect. The declaration is appended to it.
                let _ = writeln!(
                    s,
                    "  [{tag}] {} touches {} on {} (gap {:.4} mm) at x={:.1}, y={:.1}{}",
                    sh.net_a,
                    sh.net_b,
                    sh.layer,
                    sh.gap_mm,
                    sh.loc_mm[0],
                    sh.loc_mm[1],
                    sh.oracle_agreement()
                        .map(|agreement| format!("; {agreement}"))
                        .unwrap_or_default()
                );
            }
        }
        if !self.stale_fills.is_empty() {
            let _ = writeln!(s, "\nOUT-OF-DATE ZONE FILL ({}):", self.stale_fills.len());
            for sf in &self.stale_fills {
                let _ = writeln!(
                    s,
                    "  [WARNING] {} on {} at {} place(s), first at x={:.1}, y={:.1}",
                    sf.plain, sf.layer, sf.count, sf.loc_mm[0], sf.loc_mm[1],
                );
            }
        }
        if !self.violations.is_empty() {
            let _ = writeln!(s, "\nCLEARANCE VIOLATIONS (below rule), grouped:");
            for g in &self.violations {
                let _ = writeln!(s, "  {}", g.label_with_rule_provenance(provenance));
            }
        }
        if !self.at_limit.is_empty() {
            let _ = writeln!(s, "\nAT MINIMUM CLEARANCE (no margin), grouped:");
            for g in &self.at_limit {
                let _ = writeln!(s, "  {}", g.label_with_rule_provenance(provenance));
            }
        }
        let _ = writeln!(
            s,
            "\n{} short(s), {} below-rule group(s), {} at-limit group(s).",
            self.shorts.len(),
            self.violations.len(),
            self.at_limit.len()
        );
        s
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Machine-readable JSON: one structured surface for every check
// ─────────────────────────────────────────────────────────────────────────────

use hauksbee_extract::{LintCheck, NetLintReport, Severity, SiCheck, SiReport, SiSeverity};

/// One finding in the uniform machine-readable shape: every check's
/// findings serialize the same way, so a CI pipeline or AI never parses prose.
#[derive(Debug, Clone)]
pub struct JsonFinding {
    /// Which check produced it ("si" / "lint" / "drc").
    pub check: String,
    /// The specific rule (e.g. `"controlled_impedance"`, `"strap_pin"`).
    pub kind: String,
    /// "serious" | "warning" | "note" | "info".
    pub severity: String,
    pub nets: Vec<String>,
    pub location_mm: Option<[f64; 2]>,
    pub layer: Option<String>,
    pub refs: Vec<String>,
    /// Whether a user should act on this (true for real findings and for
    /// off-target info notes; false for within-tolerance observations).
    pub actionable: bool,
    /// The expert one-line message.
    pub message: String,
    /// The same finding in plain language (best-effort; equals `message` when no
    /// dedicated plain template applies).
    pub plain: String,
    /// A concise suggested fix, when a dedicated template applies. Closes the
    /// TUI "no fix text" gap: once `JsonFinding` carries it, `Finding::from_json`
    /// can read it instead of hard-coding `None`. Omitted (None) when no fix
    /// template applies to this finding kind.
    pub fix: Option<String>,
}

impl JsonFinding {
    /// Whether this finding is a reason its report family fails its gate.
    ///
    /// This is derived from the stable pre-v2 public fields so adding the
    /// machine-readable wire property does not break downstream Rust struct
    /// literals. Every serious finding gates; lint warnings, all real SI
    /// findings and co-sim fault/strict-boot findings widen that rule.
    #[must_use]
    pub fn gates(&self) -> bool {
        self.severity == "serious"
            || (self.check == "lint" && self.severity == "warning")
            || (self.check == "si" && self.severity != "info")
            || self.check == "cosim"
    }
}

/// The fix for an analysis that did not run to a result.
const NOT_JUDGED_FIX: &str = "Declare the level of each input the board takes power from (a \
     net_drive for the connector's rail), or give the part the note blames a working model, so \
     the operating point solves with its rails up; then re-check.";

/// Why a verdict with no cause is partly judged, if it is: the judgement
/// scope's sentence and the clause [`JsonReport::stated_condition`] gives,
/// when there is one.
fn partly_judged_reason(
    scope: Option<&crate::scope::JudgementScope>,
    stated: Option<String>,
) -> Option<String> {
    match (scope, stated) {
        (Some(s), Some(c)) => Some(format!("{} {c}", s.partly_judged_reason())),
        (Some(s), None) => Some(s.partly_judged_reason()),
        (None, Some(c)) => Some(format!(
            "{}: nothing found blocks ordering, but {c}",
            hauksbee_ir::vocabulary::PARTLY_JUDGED_LEAD
        )),
        (None, None) => None,
    }
}

/// The sentence a finding carries when its subject resolves to no board net.
pub const NOT_MAPPED_TO_BOARD_NOTE: &str = "Not mapped to the board: the affected component connects to no board net, so this finding has no board location.";

impl JsonFinding {
    /// Say, in the finding's own words, that it has no board location. The
    /// finding keeps its severity and gating; only its text gains the note.
    /// Idempotent.
    pub fn mark_not_mapped_to_board(&mut self) {
        for text in [&mut self.message, &mut self.plain] {
            if !text.contains(NOT_MAPPED_TO_BOARD_NOTE) {
                text.push(' ');
                text.push_str(NOT_MAPPED_TO_BOARD_NOTE);
            }
        }
    }

    /// A key shared by findings that say the same thing about different parts
    /// (check, kind, severity and the message with its one part's designator
    /// masked). The human reports print such findings once; the JSON keeps each
    /// finding and lets a consumer group them on this.
    #[must_use]
    pub fn group_key(&self) -> String {
        use sha2::{Digest as _, Sha256};
        use std::fmt::Write as _;
        let shape = hauksbee_extract::repeat::repeat_shape(&self.message, &self.refs);
        let digest = Sha256::digest(shape.as_bytes());
        let mut tag = String::new();
        for byte in digest.iter().take(6) {
            let _ = write!(tag, "{byte:02x}");
        }
        format!("{}/{}/{}/{tag}", self.check, self.kind, self.severity)
    }
}

impl Serialize for JsonFinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("JsonFinding", 13)?;
        state.serialize_field("check", &self.check)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("severity", &self.severity)?;
        state.serialize_field("gating", &self.gates())?;
        state.serialize_field("group", &self.group_key())?;
        state.serialize_field("nets", &self.nets)?;
        if let Some(location_mm) = &self.location_mm {
            state.serialize_field("location_mm", location_mm)?;
        }
        if let Some(layer) = &self.layer {
            state.serialize_field("layer", layer)?;
        }
        // The implicated parts are a set: print them in natural designator
        // order so a file that lists its parts differently reports the same.
        state.serialize_field("refs", &hauksbee_ir::order::sorted_refs(&self.refs))?;
        state.serialize_field("actionable", &self.actionable)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("plain", &self.plain)?;
        if let Some(fix) = &self.fix {
            state.serialize_field("fix", fix)?;
        }
        state.end()
    }
}

/// One finding in the uniform machine-readable shape: every check's
/// findings serialize the same way, so a CI pipeline or AI never parses prose.
#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct JsonFindingSchema {
    /// Which check produced it ("si" / "lint" / "drc").
    check: String,
    /// The specific rule (e.g. `"controlled_impedance"`, `"strap_pin"`).
    kind: String,
    /// "serious" | "warning" | "note" | "info".
    severity: String,
    /// Whether THIS finding is a reason the run fails its gate.
    ///
    /// Not a restatement of `severity`. Every `serious` finding gates, and so do
    /// findings the severity word grades lower: a medium (`warning`) lint
    /// finding, every real SI finding, every co-sim fault. It runs the other way
    /// too, so a `warning` is not a gate grade on its own: a copper short on a
    /// board format the copper extraction was never validated against may be
    /// phantom and does not gate, a `note`-grade lint finding does not gate, and
    /// neither does a qualified or demoted evidence badge.
    ///
    /// Current schema-v2 serializers always emit this additive field. It stays
    /// optional in the schema so documents emitted by earlier v2 binaries
    /// remain valid.
    gating: bool,
    /// Findings with the same `group` say the same thing about different parts
    /// (the message differs only in the part's designator). The human reports
    /// print each group once; this array keeps every finding. Additive and
    /// optional in the schema, like `gating`.
    group: String,
    nets: Vec<String>,
    location_mm: Option<[f64; 2]>,
    layer: Option<String>,
    refs: Vec<String>,
    /// Whether a user should act on this (true for real findings and for
    /// off-target info notes; false for within-tolerance observations).
    actionable: bool,
    /// The expert one-line message.
    message: String,
    /// The same finding in plain language (best-effort; equals `message` when no
    /// dedicated plain template applies).
    plain: String,
    /// A concise suggested fix, when a dedicated template applies. Closes the
    /// TUI "no fix text" gap: once `JsonFinding` carries it, `Finding::from_json`
    /// can read it instead of hard-coding `None`. Omitted (None) when no fix
    /// template applies to this finding kind.
    fix: Option<String>,
}

impl schemars::JsonSchema for JsonFinding {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JsonFinding".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = <JsonFindingSchema as schemars::JsonSchema>::json_schema(generator);
        if let Some(required) = schema
            .ensure_object()
            .get_mut("required")
            .and_then(serde_json::Value::as_array_mut)
        {
            required.retain(|field| field != "gating" && field != "group");
        }
        schema
    }
}

const fn si_sev_str(s: SiSeverity) -> &'static str {
    match s {
        SiSeverity::High => "serious",
        SiSeverity::Medium => "warning",
        SiSeverity::Low => "note",
        SiSeverity::Info => "info",
    }
}

const fn lint_sev_str(s: Severity) -> &'static str {
    match s {
        Severity::High => "serious",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// Whether an SI info note is actionable (off-target deviation, not "ok"/"no
/// judgement"). Mirrors the plain-mode promotion rule so JSON and `--plain`
/// agree on what counts as actionable.
fn si_info_actionable(message: &str) -> bool {
    message.contains("from target") && !message.contains("within")
}

/// Convert an [`SiReport`] (findings AND info notes) to uniform JSON findings.
///
/// Info notes are INCLUDED in JSON even though `--plain` suppresses most of them,
/// so a machine consumer sees every note the text surface hides.
#[must_use]
pub fn si_findings_json(report: &SiReport) -> Vec<JsonFinding> {
    report
        .findings
        .iter()
        .map(|f| {
            let actionable = if f.severity == SiSeverity::Info {
                si_info_actionable(&f.message)
            } else {
                true
            };
            JsonFinding {
                check: "si".to_string(),
                kind: f.check.as_str().to_string(),
                severity: si_sev_str(f.severity).to_string(),
                nets: f.nets.clone(),
                location_mm: None,
                layer: None,
                refs: f.refs.clone(),
                actionable,
                message: f.message.clone(),
                plain: si_plain_hint(f.check).to_string(),
                fix: si_fix_hint(f.check).map(str::to_string),
            }
        })
        .collect()
}

/// Convert a USB-C CC compliance report to a uniform JSON finding.
///
/// Used by the `--check` aggregate, so the aggregate stays a single valid JSON
/// document (the standalone `--usb-c --json` keeps the richer dedicated shape).
/// `None` for an `Ok` verdict (nothing to report).
#[must_use]
pub fn usbc_finding_json(report: &crate::checks::usb_c::UsbcReport) -> Option<JsonFinding> {
    use crate::checks::usb_c::UsbcLevel;
    let (severity, actionable) = match report.level {
        UsbcLevel::Ok => return None,
        UsbcLevel::Serious => ("serious", true),
        UsbcLevel::Info => ("info", false),
    };
    Some(JsonFinding {
        check: "usb_c".to_string(),
        kind: "cc_compliance".to_string(),
        severity: severity.to_string(),
        nets: Vec::new(),
        location_mm: None,
        layer: None,
        refs: report
            .receptacles
            .iter()
            .map(|r| r.reference.clone())
            .collect(),
        actionable,
        message: report.headline.clone(),
        plain: report.headline.clone(),
        fix: None,
    })
}

/// Convert a [`NetLintReport`] to uniform JSON findings.
#[must_use]
pub fn lint_findings_json(report: &NetLintReport) -> Vec<JsonFinding> {
    report
        .findings
        .iter()
        .map(|f| JsonFinding {
            check: "lint".to_string(),
            kind: f.check.as_str().to_string(),
            severity: lint_sev_str(f.severity).to_string(),
            nets: f.nets.clone(),
            location_mm: None,
            layer: None,
            refs: f.refs.clone(),
            // An abstention says what was not judged and an informational note
            // reports evidence with no verdict; both stay on the report but are
            // not findings to act on (see `LintFinding::is_actionable`).
            actionable: f.is_actionable(),
            message: f.message.clone(),
            plain: f.message.clone(),
            // The rule that raised a finding states its fix when it can
            // (`own_remedy`); the check kind's advice is the fallback.
            fix: if hauksbee_models::rating_basis::is_not_judged(&f.message) {
                // A check that names its own remedy keeps it; the rail-level
                // advice is for the analyses that need a rail declared.
                Some(f.own_remedy().unwrap_or(NOT_JUDGED_FIX).to_string())
            } else if f.is_abstention() {
                f.own_remedy()
                    .or_else(|| lint_abstention_hint(f.check))
                    .map(str::to_string)
            } else if f
                .message
                .starts_with(hauksbee_extract::NOT_ROUTED_MESSAGE_PREFIX)
            {
                Some(hauksbee_extract::NOT_ROUTED_FIX.to_string())
            } else if let Some(own) = f.own_remedy() {
                Some(own.to_string())
            } else {
                lint_fix_hint(f.check, f.severity).map(str::to_string)
            },
        })
        .collect()
}

/// Convert co-sim electrical-stress faults to uniform JSON findings.
///
/// Faults (overcurrent, overvoltage, a destroyed MOSFET, …) convert so the
/// machine `--json` co-sim surface carries them like `--plain` (`plain_faults`),
/// `--strict` (the exit gate), and the web report already do. They were silently
/// dropped from `--json`, so a CI consumer parsing the JSON saw a clean run over
/// a board the co-sim flagged. `plain_faults` emits one finding per fault in
/// order, so the zip is 1:1.
#[must_use]
pub fn fault_findings_json(faults: &[crate::stress::FaultEvent]) -> Vec<JsonFinding> {
    let plain = crate::plain::plain_faults(faults);
    faults
        .iter()
        .zip(plain.findings.iter())
        .map(|(f, pf)| JsonFinding {
            check: "cosim".to_string(),
            kind: f.kind.as_str().to_string(),
            severity: match pf.level {
                crate::plain::PlainLevel::Serious => "serious",
                crate::plain::PlainLevel::Warning => "warning",
                crate::plain::PlainLevel::Note => "note",
            }
            .to_string(),
            nets: Vec::new(),
            location_mm: None,
            layer: None,
            refs: vec![f.component.clone()],
            actionable: true,
            message: pf.what.clone(),
            plain: pf.what.clone(),
            fix: (!pf.fix.is_empty()).then(|| pf.fix.clone()),
        })
        .collect()
}

/// The top-level `--json` document.
///
/// Only the section(s) for the requested check are populated; the rest stay
/// `None` (omitted from output). `board` + `bind` are always present so an AI
/// always has the bind-role context.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonReport {
    /// Version of the `run --json` document contract. Bumped when a field
    /// changes meaning, is removed, or a closed enum gains a value; purely
    /// additive optional fields do not bump it.
    /// The generated schema lives in `crates/hauksbee-engine/schemas/`.
    pub schema_version: u32,
    pub board: String,
    pub bind: BindSummary,
    /// Whether unbound verdict-critical parts gate THIS report's verdict.
    /// True on the surfaces that make model-dependent claims (--check, --lint,
    /// --si and the default machine report), where a clean bill over an
    /// unbound critical part is vacuous and every text rendering already
    /// refuses it. False on the copper-only (--drc) and descriptive
    /// (--report) surfaces, which deliberately do not refuse: copper reads
    /// the layout, and the bind table is not a pass/fail claim. Not
    /// serialized: the gate's OUTCOME is the verdict field itself.
    #[serde(skip)]
    pub bind_gates_verdict: bool,
    /// Whether THIS surface's own `--strict` gate fails the run on findings the
    /// shared `serious` severity does not carry. Those gates are deliberately
    /// wider than `serious`: `--lint` gates on medium-severity findings, `--si`
    /// on every real finding (its informational computed-value notes excluded),
    /// and the co-sim gate on any raised fault. The findings those gates catch
    /// beyond the shared grade serialize as `warning`/`note`, which is exactly
    /// why the verdict cannot derive them from severity. Without this the same
    /// invocation printed `"verdict":"pass","ok":true` and exited 2, so a
    /// consumer gating on the document disagreed with a consumer gating on the
    /// exit code. On the routes that load waivers it is set AFTER the partition,
    /// so an overruled finding neither gates nor flips the verdict; the bare
    /// machine report and the co-sim surface load none and gate on everything
    /// they found. Not serialized: the gate's OUTCOME is the verdict field.
    #[serde(skip)]
    pub surface_gate_fails: bool,
    /// Whether this document only DESCRIBES the run (the `--report` bind table)
    /// instead of making a pass/fail claim about it. Such a surface has no
    /// `--strict` gate at all, and its subject IS the binding it prints in
    /// full, so undermined binding/coverage evidence must not become a refusal
    /// verdict here: binding completeness reaches a verdict only through the
    /// verdict-critical bind gate, which this surface is deliberately exempt
    /// from (see [`Self::bind_gates_verdict`]), and the specialist surfaces
    /// trim those per-net maps out of their verdicts for the same reason.
    /// Without this the descriptive document read `"ok":false` while the
    /// command always exited 0, so the two ways to gate on it disagreed. With
    /// no findings, no analysis section and no refusal, `--report`'s only
    /// caller leaves `verdict` constant at `pass`: on a surface that makes no
    /// pass/fail claim the rollup carries no information, and the bind summary
    /// and the evidence array (both rendered in full) are what a consumer reads
    /// instead. Not serialized: the gate's outcome is the verdict field.
    #[serde(skip)]
    pub descriptive_only: bool,
    /// What the input is when it is not a board and so cannot be ordered (a
    /// schematic), and what to run to judge ordering; see
    /// [`RunVerdict::unorderable`]. Set with [`Self::with_unorderable_input`],
    /// which also adds it to the verdict's conditions. Not serialized: the
    /// condition carries it.
    #[serde(skip)]
    pub unorderable: Option<String>,
    /// Every explicitly supplied input and what it contributed to this run.
    /// Empty on older/internal call paths that have no inventory context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<JsonInputEvidence>,
    /// Exact inputs consumed by this run, content-addressed for reproducibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// First-class assumptions collected from the real reader/bind path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// Per-net assertions derived from actual board incidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
    /// Top-level refusal for a whole-run exit 3 (for example strict co-sim).
    /// AC/thermal refusals additionally live in their analysis section so a
    /// section-only consumer sees the same contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    /// The compact order-decision surface as data. The `--check --plain` text is
    /// rendered from this same object, so CI/web consumers and a terminal reader
    /// cannot receive different bucket assignments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<crate::plain::OrderTriage>,
    /// Source and report-wide value for DRC clearance rules. Pair-specific
    /// values remain in `drc.violations[*].rule_mm`; this field says whose rule
    /// those numbers are (or are not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clearance_rule_source: Option<ClearanceRuleProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<JsonFinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drc: Option<DrcStructured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ac: Option<AcJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalJson>,
    /// Boot-state panel: per-transistor-gate power-up state (informational).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_gates: Option<Vec<BootGateJson>>,
    /// Info-level notes (bind roles, co-sim substitution, coverage caveats, SI
    /// info) that must never be silently absent but never gate a CI pipeline.
    /// Additive + `skip_serializing_if` so the schema stays backward-compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<JsonNote>,
    /// Machine-readable co-sim summary, present only on a co-sim run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosim: Option<CosimJson>,
    /// Findings that fired and were overruled by an active waiver, with the
    /// reason and the expiry date.
    ///
    /// A green verdict that quietly dropped findings would be the worst of both
    /// worlds, so the machine surface carries them exactly as the text report
    /// prints them. A pipeline can watch this array grow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waived: Vec<JsonWaived>,
    /// The assumptions a `pass` rests on beyond the checks, one sentence each:
    /// each part the board names only by class, whose assumed corners the run
    /// swept ("Q1 passes across assumed corners (...); put the MPN on Q1 to
    /// remove this assumption."), and, once per area, intent the tool cannot
    /// know but the hardware fixes (which level lights an indicator, when no
    /// stated outcome says). The serialized list also names each check that
    /// abstained on a part or net, and the modelling assumptions a
    /// model-dependent surface judged on (`JsonReport::pass_conditions`). A pass
    /// with any is conditional on them, never unconditional. Additive; omitted
    /// when empty or when the verdict is not a pass (`verdict_reason` then says
    /// why).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdict_conditions: Vec<String>,
    /// Which net checks the run judged when key parts have no usable model:
    /// the open parts, and the nets whose wiring/value or electrical checks
    /// they could change (not judged). With some electrical check judged, a
    /// finding on the rest fails the board, and nothing found is PARTLY
    /// JUDGED (`partly_judged: true`, verdict `invalid`, `ok` false); the
    /// reason names what was not judged. Additive; omitted when every key
    /// part is modelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgement_scope: Option<crate::scope::JudgementScope>,
    /// How many items the surface's order triage lists to inspect, set before
    /// the verdict is derived ([`RunVerdict::inspect`]). Not serialized: the
    /// triage itself is the machine record of them.
    #[serde(skip)]
    #[schemars(skip)]
    pub inspect_items: usize,
}

/// Machine-readable input inventory for BOM/placement-aware runs.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonInputEvidence {
    pub path: String,
    /// Stable broad type: `board`, `bom`, or `placement`.
    pub kind: String,
    /// Detected format within that type.
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity: Vec<String>,
}

/// One overruled finding on the machine surface.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonWaived {
    /// Which check produced it ("si" / "lint" / "drc").
    pub check: String,
    /// The specific rule it was matched on.
    pub kind: String,
    /// The finding's own message, so a reader can tell which one was overruled.
    pub subject: String,
    /// Why the board's owner judged it wrong or acceptable.
    pub reason: String,
    /// The date the waiver stops applying, after which this gates again.
    pub until: String,
}

impl From<crate::waiver::WaivedFinding> for JsonWaived {
    fn from(w: crate::waiver::WaivedFinding) -> Self {
        Self {
            check: w.check,
            kind: w.kind,
            subject: w.subject,
            reason: w.reason,
            until: w.until,
        }
    }
}

/// AC sweep in JSON: validity first, then the bode rows only when valid.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AcJson {
    #[serde(flatten)]
    pub validity: Validity,
    /// Per-net bode rows, present only when valid AND the net has a signal path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nets: Vec<AcNetJson>,
    /// Nets that were requested but had no signal path (all points at the floor)
    /// and so were omitted from `nets`. Empty when the whole sweep is invalid
    /// (the `valid:false` + `reason` carries that case). Listed so a partially
    /// valid sweep never silently drops a net.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub no_signal_path_nets: Vec<String>,
    /// Requested `--ac-node` names that don't exist in the circuit at all (their
    /// bode is empty, distinct from "exists but no signal path"). Surfaced so the
    /// JSON matches the text path's "net not found" warning instead of silently
    /// dropping the net.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_found_nets: Vec<String>,
    /// Honest "N of M" coverage annotation, distinct from `validity`. Non-gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CheckCoverage>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AcNetJson {
    pub net: String,
    /// `[freq_hz, mag_db, phase_deg]` triples.
    pub points: Vec<[f64; 3]>,
}

/// Thermal in JSON: validity first, then the per-device peak temps when valid.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ThermalJson {
    #[serde(flatten)]
    pub validity: Validity,
    pub ambient_c: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<ThermalDeviceJson>,
    /// Honest "N of M" coverage annotation, distinct from `validity`. Non-gating
    /// by default (`partial:true` => the table is real but some power IC is open).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CheckCoverage>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ThermalDeviceJson {
    pub reference: String,
    pub tj_c: f64,
    pub over_limit: bool,
}

impl JsonReport {
    /// Start a JSON report with just the always-present board + bind header.
    #[must_use]
    pub fn new(board_name: &str, bind: BindSummary) -> Self {
        Self {
            schema_version: RUN_REPORT_SCHEMA_VERSION,
            board: board_name.to_string(),
            bind,
            bind_gates_verdict: false,
            surface_gate_fails: false,
            descriptive_only: false,
            unorderable: None,
            inputs: Vec::new(),
            inventory: Vec::new(),
            assumptions: Vec::new(),
            evidence: Vec::new(),
            refusal: None,
            triage: None,
            clearance_rule_source: None,
            findings: None,
            drc: None,
            ac: None,
            thermal: None,
            boot_gates: None,
            notes: Vec::new(),
            cosim: None,
            waived: Vec::new(),
            verdict_conditions: Vec::new(),
            judgement_scope: None,
            inspect_items: 0,
        }
    }

    /// Append a `coverage` note: a caveat on what this result can vouch for.
    pub fn push_coverage_note(&mut self, message: impl Into<String>) {
        self.notes.push(JsonNote {
            kind: JsonNoteKind::Coverage,
            message: message.into(),
        });
    }

    /// Attach the CLI's complete, ordered input inventory to a result document.
    /// Mark this report as a model-dependent-claim surface: unbound
    /// verdict-critical parts flip its verdict to `invalid` (the machine
    /// mirror of the INCONCLUSIVE refusal the text surfaces print).
    #[must_use]
    pub const fn with_bind_verdict_gate(mut self) -> Self {
        self.bind_gates_verdict = true;
        self
    }

    /// Tell this report that the emitting surface's own `--strict` gate fails
    /// the run, so its verdict reads `fail` instead of `pass` beside an exit 2
    /// (see [`Self::surface_gate_fails`]). Pass the surface's post-waiver gate
    /// predicate: `lint_fails`, `si_fails`, the combined `--check` gate, or the
    /// co-sim fault gate.
    #[must_use]
    pub const fn with_surface_gate(mut self, fails: bool) -> Self {
        self.surface_gate_fails = fails;
        self
    }

    /// Mark this report as descriptive only, so undermined run-level evidence
    /// does not become a refusal verdict on a surface that never gates (see
    /// [`Self::descriptive_only`]).
    #[must_use]
    pub const fn with_descriptive_only(mut self) -> Self {
        self.descriptive_only = true;
        self
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: &[JsonInputEvidence]) -> Self {
        self.inputs = inputs.to_vec();
        self
    }

    /// Attach the one evidence object used by every report renderer.
    #[must_use]
    pub fn with_evidence(mut self, evidence: &crate::evidence::BoardEvidence) -> Self {
        self.inventory = evidence.inventory().to_vec();
        self.assumptions = evidence.assumptions().to_vec();
        self.evidence = evidence.maps().to_vec();
        self.verdict_conditions = evidence.conditions().to_vec();
        // Parts whose rated voltage nothing states, seen past the floor
        // assumed for them: the pass rests on their real ratings, so it
        // names them (`rating_basis::UNSTATED_RATING_MARKER`).
        let unstated: Vec<String> = (self.findings.iter().flatten())
            .filter(|f| hauksbee_models::rating_basis::is_unstated_rating(&f.message))
            .map(|f| {
                f.message
                    .replace(hauksbee_extract::INFORMATIONAL_MARKER, "")
                    .trim()
                    .to_string()
            })
            .collect();
        for c in unstated {
            if !self.verdict_conditions.contains(&c) {
                self.verdict_conditions.push(c);
            }
        }
        self
    }

    /// Add one assumption a pass rests on beyond the checks (nothing for
    /// `None`): an outcome the tool cannot know, such as what an indicator
    /// is meant to show. It qualifies a pass and never changes the verdict.
    #[must_use]
    pub fn with_condition(mut self, condition: Option<String>) -> Self {
        self.verdict_conditions.extend(condition);
        self
    }

    /// Mark the input as one that is not a board (nothing for `None`): the
    /// sentence becomes a condition of any pass and the words a human surface
    /// opens a pass with (see [`RunVerdict::unorderable`]).
    #[must_use]
    pub fn with_unorderable_input(mut self, why: Option<String>) -> Self {
        if let Some(why) = &why {
            if !self.verdict_conditions.contains(why) {
                self.verdict_conditions.push(why.clone());
            }
        }
        self.unorderable = why;
        self
    }

    /// Scope the verdict around unmodelled key parts (nothing for `None`).
    /// The verdict reason names what the scope did not judge.
    #[must_use]
    pub fn with_judgement_scope(mut self, scope: Option<crate::scope::JudgementScope>) -> Self {
        self.judgement_scope = scope;
        self
    }

    /// What keeps a clean run from passing, as a clause: an analysis that did
    /// not run to a result (it judged nothing), else the first stated
    /// condition (a part held past a placeholder or guessed card's limit, not
    /// its own); see [`hauksbee_models::rating_basis`]. Neither is a fail.
    fn stated_condition(&self) -> Option<String> {
        use hauksbee_models::rating_basis::{is_condition, is_not_judged, not_judged};
        let first = |pick: fn(&str) -> bool| {
            (self.findings.iter().flatten())
                .find(|f| pick(&f.message))
                .map(|f| f.message.as_str())
        };
        // A co-sim whose analog solve failed held stale voltages over those
        // windows: no fault seen there is no fault absent.
        let unsolved = (self.cosim.as_ref()).filter(|c| !c.analog_valid).map(|c| {
            let what = format!(
                "the co-sim's analog solve did not converge over {} window(s) and held stale \
                 voltages there",
                c.failed_windows.len().max(1)
            );
            format!(
                "an analysis did not run to a result: {}.",
                not_judged(&what)
            )
        });
        (unsolved.or_else(|| {
            first(is_not_judged).map(|m| format!("an analysis did not run to a result: {m}."))
        }))
        .or_else(|| {
            first(is_condition).map(|c| {
                format!(
                    "a part's limit was judged against a stand-in card's rating, not its \
                         own: {c}."
                )
            })
        })
    }

    /// The scope, when it lets the verdict judge around exactly `blockers`.
    fn active_scope(&self, blockers: &[String]) -> Option<&crate::scope::JudgementScope> {
        self.judgement_scope.as_ref().filter(|s| s.scopes(blockers))
    }

    /// Why the run refused a verdict, if it did: the run-level refusal first,
    /// then an AC or thermal result that could not be judged (with its own
    /// refusal when it carries one).
    fn refused_cause(&self) -> Option<VerdictCause<'_>> {
        self.refusal.as_ref().map(VerdictCause::from).or_else(|| {
            let validities = [
                self.ac.as_ref().map(|a| &a.validity),
                self.thermal.as_ref().map(|t| &t.validity),
            ];
            let invalid = validities.into_iter().flatten().find(|v| !v.valid)?;
            Some(invalid.refusal.as_ref().map_or_else(
                || {
                    VerdictCause::Refused {
                        claim: "a trustworthy analysis result",
                        missing: invalid
                            .reason
                            .as_deref()
                            .unwrap_or("its inputs were not valid"),
                        next: "read the reason in that analysis section",
                    }
                },
                VerdictCause::from,
            ))
        })
    }

    /// Replace the board-wide binding maps with maps for the actual findings
    /// already attached to this report, KEEPING the run-level maps in
    /// `run_level` (input coverage and its kin). The replacement must not
    /// silently drop them: the verdict rests on run-level maps, and a
    /// specialist surface that swapped them out for finding-backed maps alone
    /// would read `pass` over an undermined coverage claim, because
    /// finding-backed maps carry their own badges without vetoing the run.
    ///
    /// # Errors
    ///
    /// Returns an [`hauksbee_ir::evidence::EvidenceError`] if building a
    /// finding's evidence map fails (a blank or unknown net, or a map that fails
    /// validation against the evidence registry).
    pub fn attach_finding_evidence(
        &mut self,
        evidence: &crate::evidence::BoardEvidence,
        run_level: Vec<hauksbee_ir::evidence::EvidenceMap>,
    ) -> Result<(), hauksbee_ir::evidence::EvidenceError> {
        if let Some(findings) = &self.findings {
            let mut maps = evidence.maps_for_findings(findings)?;
            maps.extend(run_level);
            // Always replace, even with an empty result: leaving the per-net
            // binding-completeness maps in place would let ANY unresolved
            // passive invalidate a clean specialist report, when binding
            // completeness enters these verdicts only through the
            // verdict-critical bind gate.
            self.evidence = maps;
        }
        Ok(())
    }

    /// A top-level machine verdict computed from the populated sections, so a CI
    /// consumer can read pass/fail without re-deriving it from every finding.
    /// Returns `(ok, verdict, serious_count, actionable_count)` where `verdict`
    /// is `"pass"` | `"fail"` | `"invalid"`:
    ///   - `fail`, at least one serious finding (a DRC short, a destroyed part
    ///     in co-sim, a high-severity lint/SI finding), or a finding the
    ///     emitting surface's own `--strict` gate fails on where that gate is
    ///     wider than `serious` (see [`Self::surface_gate_fails`]);
    ///   - `invalid`, nothing gates, but the run-level claim could not be
    ///     judged: a refusal, AC or thermal reported `valid:false`, or a
    ///     run-level evidence map (input coverage, bind completeness) is
    ///     undermined. Undermined maps backing individual findings do NOT
    ///     invalidate the run; the finding stays on the report wearing its own
    ///     badge;
    ///   - `pass`, otherwise.
    ///
    /// Mirrors the run's own exit gate: DRC shorts are ignored when the board is
    /// newer than the validated copper extraction (`version_warning` set), the
    /// same carve-out the CI gate makes.
    #[must_use]
    pub fn verdict(&self) -> (bool, &'static str, usize, usize) {
        self.verdict_with_waivers(&mut crate::waiver::WaiverSet::default())
            .0
    }

    /// Whether undermined evidence flips the verdict to `invalid`: the maps
    /// the RUN-LEVEL claim rests on (input coverage, bind completeness,
    /// analysis validity), not the maps backing individual findings. A
    /// finding is an observation the report already surfaces with its own
    /// qualified/undermined badge; an undermined heads-up note means THAT
    /// note's magnitude is uncertain, and it stays on the report saying so.
    /// It does not mean the run could not be judged: "0 serious" rests on
    /// the coverage maps and the serious-finding set, both of which keep
    /// their own undermined routes to `invalid`. Without this split, one
    /// unparseable value field anywhere on the board invalidated the whole
    /// run through whichever informational note touched its net.
    fn run_level_evidence_undermined(&self) -> bool {
        let finding_assertions: std::collections::HashSet<&str> = self
            .findings
            .iter()
            .flatten()
            .map(|f| f.message.as_str())
            .collect();
        !self.descriptive_only
            && run_level_undermined(&self.evidence, |a| finding_assertions.contains(a))
    }

    /// As [`Self::verdict`], but findings an active waiver covers do not gate.
    ///
    /// Returns the verdict tuple and the waived findings, which the caller
    /// reports. Waived is not hidden: a board carrying overruled findings has
    /// to look like one, or the file rots into a list nobody reads.
    pub fn verdict_with_waivers(&self, waivers: &mut crate::waiver::WaiverSet) -> VerdictParts {
        let mut serious = 0usize;
        let mut actionable = 0usize;
        let mut waived = Vec::new();
        // The first gating finding's plain words and fix, for the reason sentence.
        let mut first: Option<(&str, &str)> = None;
        if let Some(findings) = &self.findings {
            // DRC findings mirror `self.drc`, which is counted below.
            for f in findings
                .iter()
                .filter(|f| self.drc.is_none() || f.check != "drc")
            {
                // Waiving a note would suppress information without changing
                // any outcome, which is cost with no benefit, so only the
                // `serious` grade is offered a waiver here. That is narrower
                // than `JsonFinding::gates`: this path counts `serious_count`,
                // and the surfaces whose gate is wider apply their waivers
                // upstream, before a finding reaches this document (see
                // `reports::check::gather_findings_with_schematic`).
                if f.severity == "serious" {
                    if let Some(w) =
                        waivers.take_waiver(&f.check, &f.kind, &f.nets, &f.refs, &f.message)
                    {
                        waived.push(w);
                        continue;
                    }
                    serious += 1;
                    first.get_or_insert_with(|| (&f.plain, f.fix.as_deref().unwrap_or_default()));
                }
                if f.actionable {
                    actionable += 1;
                }
            }
        }
        if let Some(drc) = &self.drc {
            if drc.version_warning.is_none() {
                for s in &drc.shorts {
                    // A board-authorized contact or an unvalidated-format phantom
                    // may be non-serious. Keep the aggregate verdict aligned with
                    // the per-finding and artifact severities rather than
                    // re-deriving intent here.
                    if s.severity != "serious" {
                        continue;
                    }
                    let nets = vec![s.net_a.clone(), s.net_b.clone()];
                    let subject = format!("{} to {} on {}", s.net_a, s.net_b, s.layer);
                    if let Some(w) = waivers.take_waiver("drc", "short", &nets, &[], &subject) {
                        waived.push(w);
                        continue;
                    }
                    serious += 1;
                    actionable += 1;
                    first.get_or_insert((&s.plain, &s.fix));
                }
            }
            actionable += drc.actionable_group_count(self.clearance_rule_source.as_ref());
            // One per out-of-date zone fill, the unit of `drc.stale_fills`: each
            // is something to refill and sits on the inspect list, but it never
            // gates (it is not `serious`).
            actionable += drc.stale_fills.len();
        }
        let evidence_undermined = self.run_level_evidence_undermined();
        // The INCONCLUSIVE bind contract on the machine surface: a clean
        // result over an unbound verdict-critical part is vacuous, and the
        // model-dependent-claim surfaces' text renderings already refuse the
        // clean bill for it, so their JSON verdict must refuse it too rather
        // than reading "pass" beside an INCONCLUSIVE coverage note. Gated by
        // `bind_gates_verdict`: the copper-only and descriptive surfaces
        // deliberately do not refuse (see the field's doc).
        let blockers = self.verdict_open_parts();
        let refused = self.refused_cause();
        // `fail` covers both routes to a failing gate: a serious finding, and a
        // finding this surface's own (wider) strict gate fails on. The second
        // route is what keeps the verdict field from reading `pass` in the very
        // document a `--strict` exit 2 was printed beside.
        let gating = || self.findings.iter().flatten().filter(|f| f.gates());
        let first = first.or_else(|| {
            gating()
                .next()
                .map(|f| (f.plain.as_str(), f.fix.as_deref().unwrap_or_default()))
        });
        // The other parts the first finding is repeated on.
        let same_on = first_siblings(first, gating());
        let failing = (serious > 0 || self.surface_gate_fails).then(|| {
            let count = if serious > 0 {
                serious
            } else {
                gating().count().max(1)
            };
            VerdictCause::Failing {
                count,
                first,
                same_on: &same_on,
            }
        });
        // Scoped judgement: blockers whose dependent checks the scope set aside
        // no longer refuse the rest; its sentence says what was not judged. A
        // finding still fails the board, and nothing found is PARTLY JUDGED,
        // which is `invalid`, never a pass.
        let scope = self.active_scope(&blockers);
        let unjudged: &[String] = if scope.is_some() { &[] } else { &blockers };
        let cause = VerdictCause::pick(failing, refused, unjudged, evidence_undermined);
        // A limit passed that was a placeholder or guessed card's, not the
        // part's own, is a stated condition, and an analysis that did not run
        // to a result judged nothing: never a fail, never a pass either.
        let stated = self.stated_condition();
        let partial = cause.is_none() && (scope.is_some() || stated.is_some());
        let verdict = match cause {
            None if partial => "invalid",
            None => "pass",
            Some(c) if c.is_invalid() => "invalid",
            Some(_) => "fail",
        };
        let not_judged = cause.and_then(|c| c.not_judged(&blockers, scope));
        let reason = match (cause, scope) {
            (Some(c @ VerdictCause::Unmodelled(_)), None) => {
                // Too narrow a scope to judge an electrical check: the refusal
                // stands, with the clean wiring/value checks as information.
                let note = (self.judgement_scope.as_ref())
                    .filter(|s| s.open_parts == blockers)
                    .and_then(crate::scope::JudgementScope::wiring_note);
                Some(note.map_or_else(|| c.reason(), |n| format!("{} {n}", c.reason())))
            }
            (Some(c), _) => Some(c.reason_scoped(&blockers, scope)),
            (None, s) => partly_judged_reason(s, stated),
        };
        (
            (verdict == "pass", verdict, serious, actionable),
            waived,
            reason,
            partial,
            not_judged,
        )
    }

    /// Everything a pass rests on: the conditions the surface stated, then the
    /// ones its findings and assumptions imply (a check that abstained on a
    /// part, the modelling assumptions a model-dependent surface judged on; see
    /// [`crate::verdict_conditions`]), each once. Every surface reads this one
    /// list, so `--json`, `--plain`, the closing `VERDICT:` line and the web
    /// headline name the same conditions.
    #[must_use]
    pub fn pass_conditions(&self) -> Vec<String> {
        // A waived finding is listed with its waiver's reason and date, never
        // as something the pass rests on.
        let findings: Vec<JsonFinding> = (self.findings.iter().flatten())
            .filter(|f| {
                !self
                    .waived
                    .iter()
                    .any(|w| [&w.check, &w.kind, &w.subject] == [&f.check, &f.kind, &f.message])
            })
            .cloned()
            .collect();
        let model_dependent = self.bind_gates_verdict && !self.descriptive_only;
        let derived =
            crate::verdict_conditions::derived(&findings, &self.assumptions, model_dependent);
        let mut seen = std::collections::HashSet::new();
        (self.verdict_conditions.iter().cloned().chain(derived))
            .map(|c| hauksbee_ir::vocabulary::sentence_case(&c))
            .filter(|c| seen.insert(c.clone()))
            .collect()
    }

    /// The parts whose missing models make the verdict not a pass: the
    /// verdict-critical unmodelled parts, on a surface whose verdict they
    /// gate. The CI artifacts name them on the not-a-pass case, so the result
    /// stays linked to the parts it is about.
    fn verdict_open_parts(&self) -> Vec<String> {
        if self.bind_gates_verdict {
            unmodelled_critical_refs(&self.bind)
        } else {
            Vec::new()
        }
    }

    /// The sentence a CI test case carries for a partly judged run: the
    /// partly judged label, then the judgement scope's one sentence naming the
    /// open parts and what was not judged. `None` when the run is not judged
    /// around a scope (the verdict reason itself is then the case's words).
    fn partly_judged_case(&self) -> Option<String> {
        let scope = self.active_scope(&self.verdict_open_parts())?;
        Some(format!(
            "{}: {}",
            hauksbee_ir::vocabulary::PARTLY_JUDGED_LEAD,
            scope.condition()
        ))
    }

    /// The machine verdict as the human surfaces carry it: the SAME derivation
    /// [`Self::to_json`] prints, so a triage headline or a closing `VERDICT:`
    /// line built from this can never disagree with the `verdict` field.
    #[must_use]
    pub fn run_verdict(&self) -> RunVerdict {
        let ((_, verdict, _, _), _, reason, partial, not_judged) =
            self.verdict_with_waivers(&mut crate::waiver::WaiverSet::default());
        // The one place a surface's verdict reaches the CI artifacts: whoever
        // asks this machine document for its verdict, in whatever output mode,
        // tells JUnit and SARIF the same answer (a no-op outside a `--junit` /
        // `--sarif` run).
        crate::reports::ci_artifacts::note_verdict(
            verdict,
            partial,
            reason.as_deref(),
            &self.verdict_open_parts(),
            self.partly_judged_case().as_deref(),
        );
        RunVerdict {
            verdict,
            reason,
            // A condition qualifies a pass; a verdict that is not one says why instead.
            conditions: if verdict == "pass" {
                self.pass_conditions()
            } else {
                Vec::new()
            },
            partial,
            not_judged,
            inspect: if verdict == "pass" {
                self.inspect_items
            } else {
                0
            },
            // Only a pass is ever mistaken for "OK TO ORDER".
            unorderable: self
                .unorderable
                .clone()
                .filter(|_| verdict == "pass" && !partial),
        }
    }

    /// Serialize to a pretty JSON string (stable, non-interactive output). The
    /// top-level object is prefixed with the machine verdict (`ok`, `verdict`,
    /// `serious_count`, `actionable_count`) computed by [`Self::verdict`], so a
    /// success document shares the `{"ok": ...}` shape with the hard-error
    /// envelope and a CI consumer never has to re-derive pass/fail.
    #[must_use]
    pub fn to_json(&self) -> String {
        let ((ok, verdict, serious, actionable), _, reason, partial, _) =
            self.verdict_with_waivers(&mut crate::waiver::WaiverSet::default());
        // Printing the document tells the CI artifacts the verdict it states
        // (see [`Self::run_verdict`]).
        crate::reports::ci_artifacts::note_verdict(
            verdict,
            partial,
            reason.as_deref(),
            &self.verdict_open_parts(),
            self.partly_judged_case().as_deref(),
        );
        let Ok(serde_json::Value::Object(mut value)) = serde_json::to_value(self) else {
            return serde_json::to_string_pretty(self)
                .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialize: {e}\"}}"));
        };
        // Insert the rollup at the front so it reads first. serde_json::Map
        // preserves insertion order under the `preserve_order` feature; without
        // it the keys are present regardless of position, which is what matters.
        let mut out = serde_json::Map::new();
        out.insert("ok".into(), serde_json::json!(ok));
        out.insert("verdict".into(), serde_json::json!(verdict));
        // For consumers that want the partly judged answer: `ok` stays false.
        if partial {
            out.insert("partly_judged".into(), serde_json::json!(true));
        }
        if let Some(reason) = reason {
            out.insert("verdict_reason".into(), serde_json::json!(reason));
        }
        out.insert("serious_count".into(), serde_json::json!(serious));
        out.insert("actionable_count".into(), serde_json::json!(actionable));
        // A condition qualifies a pass; a verdict that is not one says why instead.
        let conditions = if verdict == "pass" {
            self.pass_conditions()
        } else {
            Vec::new()
        };
        if conditions.is_empty() {
            value.remove("verdict_conditions");
        } else {
            value.insert("verdict_conditions".into(), serde_json::json!(conditions));
        }
        out.append(&mut value);
        serde_json::to_string_pretty(&serde_json::Value::Object(out))
            .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialize: {e}\"}}"))
    }
}

/// Best-effort plain text for an SI finding kind.
///
/// So JSON `plain` is not just a copy of `message` for the kinds we have
/// templates for. Kept tiny on purpose; the full plain templates live in
/// [`crate::plain`].
#[must_use]
pub const fn si_plain_hint(check: SiCheck) -> &'static str {
    match check {
        SiCheck::ControlledImpedance => "trace impedance is off the target value",
        SiCheck::UsbDiffPair => "USB data pair is mismatched",
        SiCheck::CrystalLoadCap => "crystal load capacitors may be the wrong value",
        SiCheck::I2cRiseTime => "I2C bus rise time is slow",
        SiCheck::AntennaKeepout => "copper intrudes into the antenna keep-out",
        SiCheck::TraceAmpacity => "a routed trace is too narrow for the current it carries",
        SiCheck::InputCapRipple => "the input bulk capacitor is over its ripple-current rating",
    }
}

/// A concise suggested fix for an SI finding kind, for the machine-readable `fix` field.
///
/// Kept to a single sentence; the full plain remediation prose lives in
/// [`crate::plain::plain_si`]. `None` when no template applies.
#[must_use]
pub const fn si_fix_hint(check: SiCheck) -> Option<&'static str> {
    Some(match check {
        SiCheck::ControlledImpedance => {
            "Adjust trace width/spacing for the actual stackup to hit the target impedance, or have the fab build a controlled-impedance stackup."
        }
        SiCheck::UsbDiffPair => {
            "Length-match D+/D- and route them as a tight, parallel, equal-width pair over a continuous reference ground."
        }
        SiCheck::CrystalLoadCap => {
            "Set each load cap to ~2*(crystal load capacitance) minus board stray (a few pF), per the crystal datasheet."
        }
        SiCheck::I2cRiseTime => {
            "Use a stronger (smaller) pull-up, reduce bus capacitance/length, or lower the I2C clock."
        }
        SiCheck::AntennaKeepout => {
            "Clear all copper and ground fill out of the antenna keep-out region and re-route any tracks that cross it."
        }
        SiCheck::TraceAmpacity => {
            "Widen the trace to at least the IPC-2221 minimum width for its current, pour the rail as a plane, or use heavier copper."
        }
        SiCheck::InputCapRipple => {
            "Use a capacitor with a higher ripple-current rating, split the bulk across parallel caps, or add low-ESR ceramics to take the high-frequency ripple."
        }
    })
}

/// What unlocks a lint abstention. `None` where the finding's own message
/// already names the unlock (the decode abstention); the strap and unchecked-MCU
/// abstentions use the shared [`lint_fix_hint`], which words them correctly.
fn lint_abstention_hint(check: LintCheck) -> Option<&'static str> {
    match check {
        LintCheck::OperatingEnvelope => Some(
            "Declare the rail's level with a net_drive, or bind the regulator that defines the net, so its operating envelope can be judged.",
        ),
        LintCheck::UnroutedPad => Some("Refill the zones (KiCad: B), save the board, and re-check so connectivity through the pours can be judged."),
        LintCheck::DeviceDecode => None,
        LintCheck::DcOverstress => Some("Give the part the finding names a model or a usable value (a card, a numeric resistance, the trimmer's setting) so the rating can be judged, then re-check."),
        _ => lint_fix_hint(check, Severity::Low),
    }
}

/// The fix hints that depend on severity as well as the check kind.
fn severity_specific_fix_hint(check: LintCheck, severity: Severity) -> Option<&'static str> {
    if check == LintCheck::McuResourceConflict && severity != Severity::High {
        return Some(if severity == Severity::Medium {
            "Generate one of the two signals with the MCU's programmable engine (e.g. RP2040 PIO) in firmware; \
             otherwise move one function to a pin on a free hardware channel."
        } else {
            "If deliberate (firmware drives the device another way), nothing needs to change; \
             if the pin-locked peripheral was intended, move the device to its full fixed pin set."
        });
    }
    // A Low strap finding is the lint's abstention, worded as unchecked (as in plain.rs).
    (check == LintCheck::StrapPin && severity == Severity::Low).then_some(
        "Confirm the strap divider gives a clear reset level per the datasheet, or supply the missing model data the message names so the check can judge it.",
    )
}

/// The suggested fix of a schematic/PCB disagreement: below High it is a
/// part-identity disagreement, not moved wiring.
fn pcb_schematic_fix_hint(severity: Severity) -> &'static str {
    if severity == Severity::High {
        "Update the PCB from the schematic, re-route the changed pins, and re-check."
    } else {
        "Confirm which part is intended and, if the schematic's is right, update the PCB from it, then re-check."
    }
}

/// The suggested fix of a polarity finding (a swapped supply and ground pin, a diode or a
/// polarised capacitor fitted backwards).
const fn polarity_fix_hint(check: LintCheck) -> &'static str {
    match check {
        LintCheck::SupplyGroundSwapped => {
            "Swap the nets of the connector's supply and ground pins so each pin reaches the rail its name says."
        }
        LintCheck::DiodeReversed => {
            "Turn the diode round so its cathode is on the more positive net and its anode on the more negative one."
        }
        _ => {
            "Turn the capacitor round so its + terminal is on the more positive net and its - terminal on the more negative one."
        }
    }
}

/// The machine-readable fix for a back-powering finding. The tolerance wording
/// is the shared vocabulary's, the same sentence the plain report ends on.
fn back_power_fix_hint() -> &'static str {
    static HINT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "Pull the pin up to the part's own supply rail instead, or put a proper level shifter between the domains. {}",
            hauksbee_ir::vocabulary::back_power_tolerance_fix(None)
        )
    });
    HINT.as_str()
}

/// A concise suggested fix for a lint finding kind, for the machine-readable `fix` field.
///
/// `None` when no template applies. Severity disambiguates the resource-conflict
/// check, whose low-severity form (a single function committed to a pin-locked
/// group) has a different honest remedy than the serious two-function contention.
#[must_use]
pub fn lint_fix_hint(check: LintCheck, severity: Severity) -> Option<&'static str> {
    if let Some(hint) = severity_specific_fix_hint(check, severity) {
        return Some(hint);
    }
    Some(match check {
        LintCheck::MissingI2cPullup => {
            "Add a 2.2k-10k pull-up from the I2C line to its power rail (one per SDA and SCL)."
        }
        LintCheck::MissingSdPullup => {
            "Add a 10k-100k pull-up from the SD line to the card's supply rail (the SD spec asks for one on CMD and each DAT line)."
        }
        LintCheck::FloatingControlPin => {
            "Tie the pin to a defined level with a pull-up/pull-down per the datasheet, or drive it from a known output."
        }
        LintCheck::LedCurrentSanity => "Re-pick the series resistor for ~1-20 mA: R = (Vsupply - Vf) / I_target.",
        LintCheck::OutputContention => {
            "Leave one push-pull driver on the net, or switch to open-drain with a single pull-up."
        }
        LintCheck::PinFunction => "Rewire the named pins as the finding describes, then re-run the lint.",
        LintCheck::UnroutedPad => {
            "Route the unconnected pads to their net (or refill the zones) until KiCad DRC reports no unconnected items."
        }
        LintCheck::PcbSchematicMismatch => pcb_schematic_fix_hint(severity),
        LintCheck::StrapPin => {
            "Hold the strap net at the datasheet's required reset level with a pull resistor; keep active signals off it until after boot."
        }
        LintCheck::McuResourceConflict => {
            "Move one function to a pin mapping to a free internal block, or drop one of the two features."
        }
        LintCheck::DesignatorFootprintMismatch => {
            "Make the reference/value and footprint agree, then regenerate the BOM."
        }
        LintCheck::ValuePackageSanity => {
            "Check the value against the package: lower the value, pick a larger package, or correct the value field."
        }
        LintCheck::DatasheetMismatch => {
            "Replace the Datasheet field with the datasheet for this part's own part number."
        }
        LintCheck::PlaceholderValue => {
            "Replace the placeholder with the actual passive value before ordering or trusting simulation."
        }
        LintCheck::UncheckedMcu => {
            "Verify the boot/strap pins and pin-mux by hand against the datasheet, or supply a model with --models-dir so the strap and resource-conflict checks can run."
        }
        LintCheck::DeviceDecode => {
            "Re-pick the divider resistors so the config pin lands in the intended datasheet band, per the part's decode table."
        }
        LintCheck::OperatingEnvelope => {
            "Move the pin to a compatible rail, or change the rail so its full operating range satisfies the quoted datasheet condition."
        }
        LintCheck::BackPower => back_power_fix_hint(),
        LintCheck::RailConflict => {
            "Leave one source on the rail: unfit the extra regulator or jumper, or split the rail so each source feeds its own net."
        }
        LintCheck::MissingDecoupling => {
            "Add ~100 nF from the rail to ground at each IC's supply pin, plus a bulk capacitor (several uF) on the rail."
        }
        LintCheck::DcOverstress => {
            "Limit the current into the named part: add or enlarge the series resistance that feeds it (a base resistor of a few kilohms for a small transistor) so it and its driver stay inside their ratings."
        }
        LintCheck::GroundIsland => {
            "Join the net to the board ground (same ground symbol, a net tie, or a 0 ohm link), or name it as a deliberately separate domain."
        }
        LintCheck::FloatingInput => {
            "Tie each unused CMOS input high or low (directly or through a resistor), or drive it; a no-connect flag does not define its level."
        }
        LintCheck::SiblingMismatch => {
            "Compare the flagged part pin by pin with a neighbouring copy of the cell and correct the schematic (and footprint) so it matches."
        }
        LintCheck::UnlimitedBaseDrive => {
            "Add a series resistor (a few kilohms) between the driver and the transistor base."
        }
        LintCheck::FloatingGate => {
            "Add a pull-up or pull-down resistor (about 100 kilohms) on the gate or select, to the level that is safe at power-up."
        }
        LintCheck::GateUnderdrive => {
            "Put the transistor's part number on it so its real threshold is checked, or drive the gate past the class's maximum threshold (a level shifter or a logic-level part)."
        }
        LintCheck::SupplyGroundSwapped | LintCheck::DiodeReversed | LintCheck::PolarisedCapReversed => {
            polarity_fix_hint(check)
        }
        LintCheck::RegulatorHeldOff
        | LintCheck::PowerPinWiring
        | LintCheck::RailUnpowered
        | LintCheck::ControlHeldOff
        | LintCheck::I2cBusLoading
        | LintCheck::PullupIneffective
        | LintCheck::AudioBusWiring
        | LintCheck::PowerDomainBackfeed
        | LintCheck::PowerDomainBusLockup => supply_fix_hint(check),
        LintCheck::SignalDirection
        | LintCheck::DebugHeaderPinout
        | LintCheck::SwitchShort
        | LintCheck::PinMuxCapability
        | LintCheck::SymbolCardPinMismatch
        | LintCheck::LedReversed
        | LintCheck::UnwiredConnector
        | LintCheck::CapNoDcPath => wiring_fix_hint(check),
    })
}

/// Fix hints for the wiring checks: a net against its pins' directions, a debug
/// header out of order, a switch across a supply, a bus on a pad that cannot carry
/// it, a symbol that disagrees with its card, a reversed LED, an unwired
/// connector and a capacitor with no DC path.
const fn wiring_fix_hint(check: LintCheck) -> &'static str {
    match check {
        LintCheck::SignalDirection => {
            "Rewire so each net has one driver and a listener (TX to RX, an output to the other part's input), or add the missing driver."
        }
        LintCheck::DebugHeaderPinout => {
            "Reorder the header to the standard pinout the finding names, or label it as a crossover that needs a matching cable."
        }
        LintCheck::SwitchShort => {
            "Put a resistor in series with the switch, or switch a signal pin that has a pull resistor, not a supply."
        }
        LintCheck::PinMuxCapability => {
            "Move the net to a pin the card lists for that function, or confirm the firmware bit-bangs it."
        }
        LintCheck::LedReversed => {
            "Turn the LED round so its anode faces the positive rail and its cathode ground."
        }
        LintCheck::UnwiredConnector => {
            "Wire the connector to the circuit it serves (check for a mismatched net label), or remove it."
        }
        LintCheck::CapNoDcPath => {
            "Add a bias or bleed resistor from the node to a rail or ground, or remove the capacitor."
        }
        // Only the wiring checks are routed here.
        _ => {
            "Compare the symbol and the card against the datasheet pin table for this package and correct whichever is wrong."
        }
    }
}

/// Fix hints for the supply-side checks: a held-off regulator, a miswired IC
/// power pin, an unpowered rail, a reset or enable held off by its pull and an
/// overloaded I2C pull-up.
const fn supply_fix_hint(check: LintCheck) -> &'static str {
    match check {
        LintCheck::RegulatorHeldOff => {
            "Pull the regulator's enable pin to its input through 10k-100k, or tie it to the input; keep that pull even when a GPIO switches the rail."
        }
        LintCheck::PowerPinWiring => {
            "Connect every supply pin of the chip to its power rail and every ground pin to ground, as its datasheet pin table shows."
        }
        LintCheck::RailUnpowered => {
            "Fit the rail's regulator or link, or bring the rail in on a connector or test point; name each supply net with its voltage so the rail is judged."
        }
        LintCheck::ControlHeldOff => {
            "Return the pull resistor to the part's supply rail instead of ground, or drive the pin from a GPIO or a supervisor."
        }
        LintCheck::PullupIneffective => {
            "Pull the line up to the receiver's own supply rail (at least its VIH), use a smaller resistor if its input leakage drops the level, or add a level shifter."
        }
        LintCheck::PowerDomainBackfeed => {
            "Pull the line up to the switched rail instead (so it goes off with the chip), add a buffer or switch with a power-down rating, or park the driving pin low in firmware before the rail turns off."
        }
        LintCheck::PowerDomainBusLockup => {
            "Put the I2C pull-ups on a rail that stays on while any chip on the bus does, and power every chip on the bus together, or isolate the switched chip with a bus switch or buffer."
        }
        LintCheck::AudioBusWiring => {
            "Wire each audio pin to the net carrying its own role (BCLK to BCLK, LRCLK to LRCLK, one part's data out to the other's data in), keep exactly one clock master, and connect MCLK where the codec needs it."
        }
        _ => {
            "Re-pick the pull-ups so the sink current stays under 3 mA (R > (Vrail - 0.4 V) / 3 mA) and the rise time fits the bus speed; 2.2k-4.7k suits most 3.3 V buses."
        }
    }
}

/// A copper finding between two track items on `F.Cu`, shared by the engine's
/// report tests; callers adjust the fields a case cares about.
#[cfg(test)]
pub(crate) fn test_finding(
    kind: hauksbee_extract::ViolationKind,
    (net_a, net_b): (&str, &str),
    (x, y): (f64, f64),
    (gap_mm, required_clearance_mm): (f64, f64),
) -> hauksbee_extract::DrcFinding {
    let item = |net| hauksbee_extract::Item {
        kind: hauksbee_extract::ItemKind::Track,
        net,
        owner: String::new(),
        uuid: String::new(),
    };
    hauksbee_extract::DrcFinding {
        kind,
        net_a: 1,
        net_b: 2,
        net_a_name: net_a.to_string(),
        net_b_name: net_b.to_string(),
        layer: "F.Cu".to_string(),
        x,
        y,
        gap_mm,
        required_clearance_mm,
        item_a: item(1),
        item_b: item(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_board_file_rule_source_says_none_only_when_no_rule_was_read() {
        // An explicit rule of zero is still the board's own rule.
        let explicit = ClearanceRuleProvenance::board_file(0.0);
        assert!(explicit
            .source_notice()
            .contains("clearance carried by the board file"));
        let unread = ClearanceRuleProvenance::board_file(0.0).with_no_rule_read();
        let notice = unread.source_notice();
        assert!(
            notice.contains("none") && notice.contains("no copper spacing was measured"),
            "{notice}"
        );
        // The marker is not part of the report document.
        assert_eq!(
            serde_json::to_value(&explicit).unwrap(),
            serde_json::to_value(&unread).unwrap()
        );
    }
    use crate::reports::bind::{BindOutcome, BindReport, BindRow};
    use hauksbee_extract::DrcFinding;
    use hauksbee_ir::evidence::Assumption;
    use hauksbee_models::Confidence;

    fn row(reference: &str, outcome: BindOutcome, warning: Option<&str>) -> BindRow {
        BindRow {
            reference: reference.to_string(),
            value: String::new(),
            model_id: None,
            confidence: Confidence::Exact,
            source: None,
            outcome,
            warning: warning.map(ToString::to_string),
            guesses: Vec::new(),
        }
    }

    fn unresolved(reason: &str) -> BindOutcome {
        BindOutcome::Unresolved {
            reason: reason.to_string(),
        }
    }

    fn analog_r() -> BindOutcome {
        BindOutcome::Analog { device: "R".into() }
    }

    fn mcu() -> BindOutcome {
        BindOutcome::Mcu {
            backend: "renode:stm32f4".into(),
        }
    }

    fn summary_of(rows: Vec<BindRow>) -> BindSummary {
        let mut report = BindReport::default();
        for r in rows {
            report.push(r);
        }
        BindSummary::from_report(&report)
    }

    #[test]
    fn doorbell_custom_rule_provenance_names_override_and_coverage_gap() {
        let rules = r#"(version 1)
(rule "fine-pitch routing clearance"
  (constraint clearance (min 0.127mm)))
(rule "via/NPTH hole-to-copper clearance"
  (constraint hole_clearance (min 0.2mm)))
(rule "PTH hole-to-copper clearance"
  (condition "A.Pad_Type == 'Through Hole Pad'")
  (constraint hole_clearance (min 0.28mm)))
(rule "board-edge copper clearance"
  (constraint edge_clearance (min 0.3mm)))
"#;
        let parsed = hauksbee_extract::parse_kicad_dru(rules).unwrap();
        let coverage = CustomRulesCoverage::from_parsed("doorbell.kicad_dru".into(), &parsed);
        let provenance = ClearanceRuleProvenance::custom_rules_file(0.127, Some(0.2), coverage);
        assert_eq!(
            provenance.source_notice(),
            "CLEARANCE RULE SOURCE: custom rules file doorbell.kicad_dru (report-wide value 0.127 mm), which overrides the .kicad_pro netclass value of 0.200 mm."
        );
        let notice = provenance.custom_rules_notice().unwrap();
        assert!(notice.contains("CUSTOM RULES NOT EVALUATED: 1 rule"));
        assert!(notice.contains("PTH hole-to-copper clearance"));
        assert!(notice.contains("do not qualify reported findings"));
        let not_covered = provenance.custom_rules_not_covered().unwrap();
        assert!(not_covered.contains("via/NPTH hole-to-copper clearance"));
        assert!(not_covered.contains("board-edge copper clearance"));
    }

    #[test]
    fn bare_value_rule_uses_custom_rule_disclosure_without_qualifying_findings() {
        let parsed = hauksbee_extract::parse_kicad_dru(include_str!(
            "../../hauksbee-extract/tests/fixtures/kicad_dru_bare_scope.kicad_dru"
        ))
        .unwrap();
        let coverage = CustomRulesCoverage::from_parsed("scope.kicad_dru".into(), &parsed);
        assert!(coverage.file_inactive_due_to_bare_values);
        assert_eq!(coverage.unevaluated_rules.len(), 2);
        let bare = &coverage.unevaluated_rules[0];
        assert_eq!(bare.name, "bare rule");
        assert_eq!(bare.line_number, 3);
        assert_eq!(bare.bare_value_constraint_types, ["clearance"]);
        assert!(!coverage.qualifies_clearance_findings());
        let notice = coverage.unevaluated_notice().unwrap();
        assert!(notice.starts_with("CUSTOM RULES DISABLED: scope.kicad_dru is not in force."));
        assert!(notice.contains("line 3"));
    }

    /// The binder joins an abstention's reason and its unlocking input with
    /// `UNLOCKED_BY_MARKER`; the JSON surface must split them into two
    /// fields and never leak the marker.
    #[test]
    fn no_routing_marker_reaches_the_json_surface() {
        let unlocked = "the strap state for this board, or a schematic naming the pin";
        let reason = format!(
            "the output format is strap-selected{}{unlocked}",
            Assumption::UNLOCKED_BY_MARKER
        );
        let summary = summary_of(vec![row(
            "U201",
            unresolved(&reason),
            Some("U201 (Si53301): active part left open"),
        )]);
        let r = summary.active_path_unresolved.first().expect("reported");
        assert!(
            !r.reason.contains(Assumption::UNLOCKED_BY_MARKER.trim()),
            "{}",
            r.reason
        );
        assert!(!r.reason.contains(unlocked), "{}", r.reason);
        assert_eq!(r.unlocked_by.as_deref(), Some(unlocked));

        let summary = summary_of(vec![row(
            "U9",
            unresolved("no model matched"),
            Some("U9 (MYSTERY): active part left open"),
        )]);
        let r = summary.active_path_unresolved.first().expect("reported");
        assert_eq!(r.reason, "no model matched");
        assert!(r.unlocked_by.is_none());
    }

    fn fault(
        component: &str,
        kind: crate::stress::FaultKind,
        destroyed: bool,
    ) -> crate::stress::FaultEvent {
        crate::stress::FaultEvent {
            component: component.into(),
            kind,
            value: 5.0,
            limit: 2.0,
            t: 0.01,
            destroyed,
        }
    }

    fn dc_overstress(part: &str, ma: &str) -> hauksbee_extract::LintFinding {
        hauksbee_extract::LintFinding {
            check: hauksbee_extract::LintCheck::DcOverstress,
            severity: hauksbee_extract::Severity::High,
            message: format!("{part} is overstressed at power-up: {part}_q2 passes {ma} mA"),
            refs: vec![part.to_string()],
            nets: Vec::new(),
        }
    }

    #[test]
    fn repeated_findings_share_a_group_key_and_all_stay_listed() {
        let report = hauksbee_extract::NetLintReport {
            findings: vec![
                dc_overstress("IC1", "234"),
                dc_overstress("IC2", "234"),
                dc_overstress("IC3", "99"),
            ],
        };
        let json = lint_findings_json(&report);
        assert_eq!(json.len(), 3, "the JSON keeps every finding");
        assert_eq!(json[0].group_key(), json[1].group_key());
        assert_ne!(
            json[0].group_key(),
            json[2].group_key(),
            "another current is another finding, not a repeat"
        );
        let value = serde_json::to_value(&json[0]).unwrap();
        assert_eq!(value["group"], json[0].group_key());
        assert!(json[0]
            .group_key()
            .starts_with("lint/dc_overstress/serious/"));
    }

    /// A clean board that only carries abstentions must report zero actionable
    /// findings: the notes stay listed, flagged non-actionable, and a real
    /// low-severity finding of the same check kind still counts.
    #[test]
    fn abstentions_are_listed_but_not_actionable() {
        use hauksbee_extract::{LintCheck, LintFinding, Severity};
        let low = |check, message: &str| LintFinding {
            check,
            severity: Severity::Low,
            message: message.to_string(),
            refs: vec!["U1".to_string()],
            nets: vec!["N1".to_string()],
        };
        let abstentions = [
            low(
                LintCheck::OperatingEnvelope,
                "operating envelope on net 'N1' abstained: declare the level with a net_drive, or bind the regulator that defines this net",
            ),
            low(
                LintCheck::DeviceDecode,
                "U1 TS thermistor network abstained on 'N1': unreadable; unlock: identify the part",
            ),
            low(LintCheck::StrapPin, "cannot judge divider (R1, R2): no thresholds, so its boot level is unchecked"),
            low(LintCheck::UncheckedMcu, "U1 looks like a strap-bearing MCU but has no table"),
            low(
                LintCheck::UnroutedPad,
                "zones not filled: the copper pours on GND have no computed fill in the file, so whether their pads are connected was not checked",
            ),
        ];
        let real = [
            low(
                LintCheck::OperatingEnvelope,
                "U1 role vdd on 'N1' presents 3.0..3.6 V, within the envelope but near its bound",
            ),
            low(
                LintCheck::UnroutedPad,
                "2 piece(s) of track or via copper join nothing else on their net",
            ),
            low(
                LintCheck::MissingI2cPullup,
                "I2C SDA net 'N1' has no pull-up to a rail: bus breaks out to a header",
            ),
        ];
        let json = lint_findings_json(&hauksbee_extract::NetLintReport {
            findings: abstentions.iter().chain(&real).cloned().collect(),
        });
        assert_eq!(json.len(), 8, "abstentions stay on the report");
        assert!(json[..5].iter().all(|f| !f.actionable), "{json:?}");
        assert!(json[5..].iter().all(|f| f.actionable), "{json:?}");
        assert!(json[..5].iter().all(|f| f.severity == "note"));
        assert!(
            json[0]
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("net_drive")),
            "an envelope abstention says how to unlock it, not 'move the pin'"
        );
    }

    #[test]
    fn informational_evidence_notes_are_not_actionable() {
        use hauksbee_extract::{
            LintCheck, LintFinding, Severity, SiCheck, SiFinding, SiReport, SiSeverity,
        };
        let lint = |severity, message: &str| LintFinding {
            check: LintCheck::DeviceDecode,
            severity,
            message: message.to_string(),
            refs: vec!["U1".to_string(), "R1".to_string()],
            nets: vec!["ILIM".to_string()],
        };
        let json = lint_findings_json(&hauksbee_extract::NetLintReport {
            findings: vec![
                // Legal: a decoded band reported for audit, no verdict.
                lint(Severity::Low, "U1 eFuse current-limit decode (informational): R1 = 127 ohm decodes to 10/11/12 A"),
                // Faulty: the verdict drawn from it stays actionable.
                lint(Severity::Medium, "U1 eFuse connector budget: R1 = 127 ohm decodes to 10/11/12 A, while J1 is rated 2 A"),
                // The marker never demotes a finding above note severity.
                lint(Severity::Medium, "U1 decode (informational) out of range"),
            ],
        });
        assert!(!json[0].actionable, "{json:?}");
        assert!(json[1].actionable && json[2].actionable, "{json:?}");

        let si = |message: &str| SiFinding {
            check: SiCheck::ControlledImpedance,
            severity: SiSeverity::Info,
            message: message.to_string(),
            refs: vec![],
            nets: vec!["DP".to_string(), "DN".to_string()],
        };
        let json = si_findings_json(&SiReport {
            findings: vec![
                si("DP / DN [target 90 ohm, USB differential]: estimate +1% from target, within +-15% - info only (undeclared)"),
                si("DP / DN [target 90 ohm, USB differential]: estimate +90% from target - info only (undeclared)"),
            ],
        });
        assert!(!json[0].actionable, "an in-band estimate is not off target");
        assert!(
            json[1].actionable,
            "an off-target estimate stays actionable"
        );
    }

    #[test]
    fn cosim_faults_become_json_findings() {
        use crate::stress::FaultKind;
        let js = fault_findings_json(&[
            fault("Q1", FaultKind::Overcurrent, true),
            fault("C3", FaultKind::Overvoltage, false),
        ]);
        assert_eq!(js.len(), 2);
        assert_eq!(js[0].check, "cosim");
        assert_eq!(js[0].kind, "overcurrent");
        assert_eq!(js[0].severity, "serious", "a destroyed part is serious");
        assert_eq!(js[0].refs, vec!["Q1".to_string()]);
        assert!(js[0].fix.is_some());
        assert_eq!(js[1].severity, "warning");
        assert_eq!(js[1].refs, vec!["C3".to_string()]);
    }

    #[test]
    fn default_rule_gaps_at_or_above_the_spacing_floor_are_noted_not_issues() {
        // 0.130 mm under a 0.200 mm DEFAULT rule meets the 0.127 mm floor: a
        // note, not an issue. 0.100 mm stays a warning, and against the
        // project's own 0.200 mm rule 0.130 mm stays a warning too.
        let defaulted = ClearanceRuleProvenance::defaulted(0.2, false);
        let project = ClearanceRuleProvenance::project_file(0.2);
        for (gap, provenance, issues) in [
            (0.130, &defaulted, 0),
            (0.100, &defaulted, 1),
            (0.130, &project, 1),
        ] {
            let group = DrcGroup {
                net_a: "SDA".into(),
                net_b: "SCL".into(),
                layer: "F.Cu".into(),
                count: 1,
                below_count: 1,
                at_limit: false,
                min_gap_mm: gap,
                min_gap_loc_mm: [3.0, 4.0],
                rule_mm: 0.2,
                between: "track \u{2194} track".into(),
                plain: String::new(),
                fix: "increase spacing".into(),
                refs: Vec::new(),
            };
            let st = DrcStructured {
                clearance_rule_mm: 0.2,
                primitive_count: 20,
                stale_fills: Vec::new(),
                shorts: Vec::new(),
                violations: vec![group],
                at_limit: Vec::new(),
                version_warning: None,
                suppression_note: None,
            };
            let plain = crate::plain::plain_drc_structured_with_rule_source(&st, provenance);
            assert_eq!(
                plain.findings.len(),
                issues,
                "{gap} mm: {:?}",
                plain.findings
            );
            let noted = st.default_rule_advisory_note(provenance).is_some();
            assert_eq!(noted, issues == 0, "{gap} mm advisory note");
            let mut jr = JsonReport::new("b", summary_with(Vec::new(), Vec::new()));
            jr.drc = Some(st);
            jr.clearance_rule_source = Some(provenance.clone());
            assert_eq!(jr.verdict().3, issues, "{gap} mm actionable count");
        }
    }

    #[test]
    fn a_sub_micron_clearance_gap_never_prints_as_zero_mm() {
        // A gap above the 1e-9 mm touch band is NOT contact, but it rounds to
        // `0.000` at three decimals, which reads as a short. Every gap sentence
        // (label, JSON message, plain finding) must say `< 0.001 mm` instead.
        let provenance = ClearanceRuleProvenance::project_file(0.2);
        for gap in [2e-9, 1e-8, 1e-7, 4.9e-7, 4.9e-4] {
            let group = DrcGroup {
                net_a: "SDA".into(),
                net_b: "SCL".into(),
                layer: "F.Cu".into(),
                count: 1,
                below_count: 1,
                at_limit: false,
                min_gap_mm: gap,
                min_gap_loc_mm: [3.0, 4.0],
                rule_mm: 0.2,
                between: "track \u{2194} track".into(),
                plain: String::new(),
                fix: "increase spacing".into(),
                refs: Vec::new(),
            };
            let label = group.label();
            assert!(label.contains("< 0.001 mm"), "{gap}: {label}");
            assert!(!label.contains("0.000 mm"), "{gap}: {label}");
            let st = DrcStructured {
                clearance_rule_mm: 0.2,
                primitive_count: 20,
                stale_fills: Vec::new(),
                shorts: Vec::new(),
                violations: vec![group],
                at_limit: Vec::new(),
                version_warning: None,
                suppression_note: None,
            };
            let plain = crate::plain::plain_drc_structured_with_rule_source(&st, &provenance);
            let what = &plain.findings[0].what;
            assert!(what.contains("< 0.001 mm"), "{gap}: {what}");
            assert!(!what.contains("0.000 mm"), "{gap}: {what}");
        }
    }

    #[test]
    fn json_report_carries_a_top_level_verdict() {
        let bind = summary_with(Vec::new(), Vec::new());
        let clean = JsonReport::new("b", bind.clone());
        let (ok, verdict, serious, _) = clean.verdict();
        assert!(ok && verdict == "pass" && serious == 0);
        let txt = clean.to_json();
        assert!(
            txt.contains("\"ok\": true") && txt.contains("\"verdict\": \"pass\""),
            "{txt}"
        );

        let mut failed = JsonReport::new("b", bind.clone());
        failed.findings = Some(fault_findings_json(&[fault(
            "Q1",
            crate::stress::FaultKind::Overcurrent,
            true,
        )]));
        let (ok, verdict, serious, actionable) = failed.verdict();
        assert!(!ok && verdict == "fail" && serious == 1 && actionable >= 1);
        assert!(failed.to_json().contains("\"verdict\": \"fail\""));

        let mut invalid = JsonReport::new("b", bind);
        invalid.ac = Some(AcJson {
            validity: Validity {
                valid: false,
                reason: Some("no signal path".into()),
                refusal: None,
            },
            nets: Vec::new(),
            no_signal_path_nets: Vec::new(),
            not_found_nets: Vec::new(),
            coverage: None,
        });
        assert_eq!(invalid.verdict().1, "invalid");
    }

    #[test]
    fn a_failure_over_unmodelled_key_parts_keeps_the_fail_and_says_so() {
        let mut report = JsonReport::new("b", summary_with(Vec::new(), vec![active_ic("U1")]))
            .with_bind_verdict_gate();
        report.findings = Some(fault_findings_json(&[fault(
            "Q1",
            crate::stress::FaultKind::Overcurrent,
            true,
        )]));
        let run = report.run_verdict();
        assert_eq!(
            run.verdict, "fail",
            "unmodelled parts must not demote a fail"
        );
        let why = run.reason.expect("fail carries a reason");
        assert!(
            why.starts_with("1 blocking problem(s)")
                && why.contains("Also: 1 key part has no usable model (U1)")
                && why.ends_with("so the rest of the board is not vouched for."),
            "{why}"
        );
        let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(json["verdict"], "fail");
        assert_eq!(json["verdict_reason"].as_str(), Some(why.as_str()));
        // The unmodelled-only cause already says it; it is not said twice.
        let only = JsonReport::new("b", summary_with(Vec::new(), vec![active_ic("U1")]))
            .with_bind_verdict_gate()
            .run_verdict();
        assert_eq!(only.verdict, "invalid");
        assert!(!only.reason.unwrap().contains("Also:"));
    }

    /// A part held past a placeholder card's rating is a stated condition: the
    /// verdict is partly judged and says so, never a fail and never a pass.
    #[test]
    fn a_stand_in_rating_passed_is_partly_judged_never_a_fail() {
        use hauksbee_models::rating_basis::{condition, StandIn, StandInKind};
        let stand_in = StandIn {
            card: "generic_nmos_power_pkg".into(),
            kind: StandInKind::Placeholder,
        };
        let stated = condition(
            "Q1",
            &stand_in,
            "Q1 sees 48 V, past its 30 V voltage limit",
            "30 V",
        );
        let mut report = JsonReport::new("b", summary_with(Vec::new(), Vec::new()));
        report.findings = Some(lint_findings_json(&NetLintReport {
            findings: vec![hauksbee_extract::LintFinding {
                check: hauksbee_extract::LintCheck::DcOverstress,
                severity: hauksbee_extract::Severity::Low,
                message: stated.clone(),
                refs: vec!["Q1".into()],
                nets: Vec::new(),
            }],
        }));
        let run = report.run_verdict();
        assert_eq!(
            run.verdict, "invalid",
            "a stand-in limit is never a fail or a pass"
        );
        assert!(run.partial);
        let why = run.reason.expect("a partly judged verdict says why");
        assert!(
            why.starts_with("PARTLY JUDGED") && why.contains(&stated),
            "{why}"
        );
        // The part's own rating passed still fails, condition or not.
        let mut own = report;
        own.findings
            .as_mut()
            .unwrap()
            .extend(fault_findings_json(&[fault(
                "Q2",
                crate::stress::FaultKind::Overcurrent,
                true,
            )]));
        assert_eq!(own.run_verdict().verdict, "fail");
    }

    /// An analysis that did not run to a result (a DC solve that failed, a
    /// rail that never came up in it) judged nothing: the clean run is partly
    /// judged, names what was not judged, and is never a pass. A real fault
    /// found elsewhere still fails the board.
    #[test]
    fn an_analysis_that_did_not_run_is_partly_judged_never_a_pass() {
        use hauksbee_models::rating_basis::not_judged;
        let unsolved = not_judged(
            "the DC ratings check abstained at power-up (registers cleared): the board's DC \
             operating point did not solve, so no part was checked against its ratings there",
        );
        let mut report = JsonReport::new("b", summary_with(Vec::new(), Vec::new()));
        report.findings = Some(lint_findings_json(&NetLintReport {
            findings: vec![hauksbee_extract::LintFinding {
                check: hauksbee_extract::LintCheck::DcOverstress,
                severity: hauksbee_extract::Severity::Low,
                message: unsolved.clone(),
                refs: Vec::new(),
                nets: Vec::new(),
            }],
        }));
        let run = report.run_verdict();
        assert_eq!(run.verdict, "invalid", "a failed solve is never a pass");
        assert!(run.partial);
        let why = run.reason.expect("a partly judged verdict says why");
        assert!(
            why.starts_with("PARTLY JUDGED") && why.contains(&unsolved),
            "{why}"
        );
        let finding = &report.findings.as_ref().unwrap()[0];
        assert!(
            !finding.actionable,
            "the note is coverage, not a fix to make"
        );
        assert!(finding
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("net_drive")));
        let mut own = report;
        own.findings
            .as_mut()
            .unwrap()
            .extend(fault_findings_json(&[fault(
                "Q2",
                crate::stress::FaultKind::Overcurrent,
                true,
            )]));
        assert_eq!(own.run_verdict().verdict, "fail");
    }

    #[test]
    fn every_non_pass_verdict_states_its_reason_and_next_action() {
        let sentence = |report: &JsonReport| {
            let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
            json["verdict_reason"].as_str().map(str::to_string)
        };
        let clean = JsonReport::new("b", summary_with(Vec::new(), Vec::new()));
        assert_eq!(sentence(&clean), None, "a pass has nothing to explain");

        let unmodelled = JsonReport::new("b", summary_with(Vec::new(), vec![active_ic("U1")]))
            .with_bind_verdict_gate();
        assert_eq!(unmodelled.verdict().1, "invalid");
        let why = sentence(&unmodelled).expect("invalid carries a reason");
        assert!(why.contains("U1") && why.contains("; next: "), "{why}");

        let mut failed = JsonReport::new("b", summary_with(Vec::new(), Vec::new()));
        failed.findings = Some(fault_findings_json(&[fault(
            "Q1",
            crate::stress::FaultKind::Overcurrent,
            true,
        )]));
        let why = sentence(&failed).expect("fail carries a reason");
        assert!(
            why.starts_with("1 blocking problem(s)") && why.contains("; next: "),
            "{why}"
        );

        let refused = Refusal::new(
            "an AC response",
            "a driving source",
            vec!["the board loaded"],
            "add a source",
        );
        assert_eq!(
            VerdictCause::from(&refused).reason(),
            "cannot give an AC response: a driving source; next: add a source."
        );
        let mut order = JsonReport::new("b", summary_with(Vec::new(), vec![active_ic("U1")]))
            .with_bind_verdict_gate();
        order.refusal = Some(refused);
        assert!(
            sentence(&order).unwrap().starts_with("cannot give"),
            "a refusal outranks unmodelled parts"
        );
    }

    #[test]
    fn drc_json_findings_carry_plain_and_fix() {
        let short = DrcShort {
            net_a: "GND".into(),
            net_b: "VCC".into(),
            layer: "F.Cu".into(),
            gap_mm: 0.0,
            loc_mm: [1.0, 2.0],
            severity: "serious".into(),
            plain: "GND shorts VCC on F.Cu at (1.00, 2.00) mm (gap 0.000 mm)".into(),
            fix: "separate the two nets' copper".into(),
            refs: Vec::new(),
        };
        let js = serde_json::to_string(&short).unwrap();
        assert!(js.contains("\"plain\"") && js.contains("\"fix\""), "{js}");
        let group = DrcGroup {
            net_a: "A".into(),
            net_b: "B".into(),
            layer: "F.Cu".into(),
            between: "track \u{2194} via".into(),
            count: 3,
            below_count: 3,
            at_limit: false,
            min_gap_mm: 0.1,
            min_gap_loc_mm: [0.0, 0.0],
            rule_mm: 0.2,
            plain: String::new(),
            fix: String::new(),
            refs: Vec::new(),
        };
        assert!(!group.label().is_empty());
    }

    fn active_ic(reference: &str) -> UnresolvedActive {
        UnresolvedActive {
            unlocked_by: None,
            reference: reference.to_string(),
            value: "IC".to_string(),
            reason: "all I/O pins open".to_string(),
            consequence: "its nets are not driven".to_string(),
            active_ic: true,
        }
    }

    fn summary_with(
        active_path_unresolved: Vec<UnresolvedActive>,
        resolved_but_open_active: Vec<UnresolvedActive>,
    ) -> BindSummary {
        BindSummary {
            resolved: 1,
            unresolved: 0,
            non_ignored: 1,
            critical_parts_bound: "1/1".to_string(),
            critical_parts_bound_n: 1,
            critical_parts_total: 1,
            mcu_bound: true,
            active_path_unresolved,
            resolved_but_open_active,
        }
    }

    #[test]
    fn banner_warns_on_resolved_but_open_active_ic() {
        let s = summary_with(Vec::new(), vec![active_ic("U1")]);
        let banner = s.render_banner();
        assert!(
            banner.contains("NOT trustworthy") && banner.contains("U1"),
            "{banner}"
        );
        assert_eq!(coverage_open_active_refs(&s), vec!["U1".to_string()]);
        let clean = summary_with(Vec::new(), Vec::new());
        assert!(!clean.render_banner().contains("NOT trustworthy"));
        assert!(coverage_open_active_refs(&clean).is_empty());
    }

    fn clearance(net_a: &str, net_b: &str, gap: f64, rule: f64) -> DrcFinding {
        super::test_finding(
            ViolationKind::Clearance,
            (net_a, net_b),
            (10.0, 20.0),
            (gap, rule),
        )
    }

    fn short(net_a: &str, net_b: &str) -> DrcFinding {
        let mut f = clearance(net_a, net_b, 0.0, 0.2);
        f.kind = ViolationKind::Short;
        f
    }

    fn drc_report(findings: Vec<DrcFinding>) -> DrcReport {
        DrcReport {
            clearance_mm: 0.2,
            findings,
            primitive_count: 2,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
            excluded: Vec::new(),
            clearance_unjudged: hauksbee_extract::UnjudgedClearance::default(),
        }
    }

    #[test]
    fn unvalidated_version_downgrades_shorts_and_propagates_warning() {
        let mut report = drc_report(vec![short("GND", "+3V3")]);
        report.version_warning = Some("unreliable on this version".into());
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.shorts.len(), 1);
        assert_eq!(st.shorts[0].severity, "note");
        assert_eq!(
            st.version_warning.as_deref(),
            Some("unreliable on this version")
        );

        report.version_warning = None;
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.shorts[0].severity, "serious");
        assert!(st.version_warning.is_none());
    }

    fn declared_qualification(report: &DrcReport) -> hauksbee_extract::DrcTieQualification {
        report.qualify_with_declared_ties(
            "emonTx V3.4.5.sch",
            &[hauksbee_extract::DeclaredNetTie {
                net: "GND".into(),
                tied_net: "AGND".into(),
                symbol: "AGND7".into(),
                tied_to: vec!["SUPPLY6".into()],
            }],
        )
    }

    /// A schematic-only tie declaration keeps the short serious, states the
    /// copper contact and the declaration, and keeps an actionable fix;
    /// with no schematic the fix names the unlocking upload.
    #[test]
    fn a_schematic_only_tie_stays_serious_and_states_the_context() {
        let report = drc_report(vec![short("GND", "AGND"), short("+5V", "VBAT")]);
        let qualification = declared_qualification(&report);
        let st = DrcStructured::from_report_with_ties(&report, Some(&qualification), false);
        assert_eq!(st.shorts.len(), 2);
        assert!(st.shorts.iter().all(|s| s.severity == "serious"));
        let plain = &st.shorts[0].plain;
        assert!(plain.contains("GND") && plain.contains("AGND"), "{plain}");
        assert!(
            plain.contains("AGND7 wired to SUPPLY6") && plain.contains("emonTx V3.4.5.sch"),
            "{plain}"
        );
        assert!(
            st.shorts[0].fix.contains("separate"),
            "{}",
            st.shorts[0].fix
        );
        assert!(st
            .render_with_clearance_rule_provenance(&ClearanceRuleProvenance::defaulted(
                st.clearance_rule_mm,
                false
            ))
            .contains("[SERIOUS] GND touches AGND on F.Cu"));

        let st = DrcStructured::from_report_with_ties(&report, None, true);
        assert_eq!(st.shorts[0].severity, "serious");
        assert!(st.shorts[0].fix.contains(".sch"), "{}", st.shorts[0].fix);
        assert!(crate::plain::plain_drc_structured(&st)
            .render()
            .contains(".sch"));
    }

    const OPEN_WARNING: &str =
        "unresolved part 'U7' on connected net(s): defaulting to OPEN circuit";
    const OPEN_PINS: &str = "U1: all I/O pins open (undriven)";

    /// Thermal validity is binary on rows > 0 plus the open-active-IC rule:
    /// an isolated open IC keeps an empty table valid, a connected open IC
    /// (unresolved OR resolved-but-open) invalidates an empty table and makes
    /// a populated one PARTIAL, and a passive-only board is simply valid.
    #[test]
    fn thermal_validity_and_coverage_follow_open_active_ics() {
        let isolated = summary_of(vec![
            row("R1", analog_r(), None),
            row("U9", unresolved("no model"), None),
        ]);
        assert!(!isolated.active_ics_unresolved());
        assert!(thermal_validity(0, &isolated).valid);

        let open_unresolved = summary_of(vec![
            row("R1", analog_r(), None),
            row("U7", unresolved("no model"), Some(OPEN_WARNING)),
        ]);
        assert!(thermal_validity(1, &open_unresolved).valid);
        let cov = thermal_coverage(1, &open_unresolved);
        assert!(cov.partial);
        assert_eq!(cov.dissipating_count, 1);
        assert_eq!(cov.open_active_on_live_circuit, 1);
        assert!(coverage_open_active_refs(&open_unresolved).contains(&"U7".to_string()));
        let v = thermal_validity(0, &open_unresolved);
        assert!(!v.valid);
        assert!(v.reason.unwrap().contains("U7"));

        let open_mcu = summary_of(vec![
            row("R1", analog_r(), None),
            row("U1", mcu(), Some(OPEN_PINS)),
        ]);
        assert!(!open_mcu.active_ics_unresolved());
        assert!(open_mcu.active_open_on_live_circuit());
        assert!(thermal_coverage(1, &open_mcu).partial);
        assert!(coverage_open_active_refs(&open_mcu).contains(&"U1".to_string()));
        let v = thermal_validity(0, &open_mcu);
        assert!(
            !v.valid,
            "an empty table hiding a resolved-but-open IC is not 'runs cool'"
        );
        assert!(v.reason.unwrap().contains("U1"));

        let passive = summary_of(vec![row("R1", analog_r(), None)]);
        assert_eq!(passive.critical_parts_total, 0);
        assert!(thermal_validity(0, &passive).valid);
        assert!(!thermal_coverage(1, &passive).partial);
        assert!(coverage_open_active_refs(&passive).is_empty());
        assert!(passive.render_banner().contains("no active ICs on board"));
    }

    #[test]
    fn ac_all_floor_is_sentinel() {
        let bode: Vec<(f64, f64, f64)> = (0..10).map(|i| (f64::from(i), -6000.0, 0.0)).collect();
        assert!(ac_is_all_sentinel(&bode));
        assert!(!ac_is_all_sentinel(&[
            (1.0, -3.0, -45.0),
            (2.0, -6000.0, 0.0)
        ]));
        assert!(!ac_is_all_sentinel(&[]));
    }

    /// Repeated findings group with a count; at-limit (gap == rule) groups are
    /// separated from below-rule ones; a mixed group never overcounts "below".
    #[test]
    fn drc_groups_separate_at_limit_from_below_rule_without_overcounting() {
        let mut report = drc_report(Vec::new());
        for _ in 0..3 {
            report.findings.push(clearance("GND", "+3.3V", 0.2, 0.2));
        }
        report.findings.push(clearance("GND", "+5V", 0.1, 0.2));
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.at_limit.len(), 1);
        assert_eq!(st.at_limit[0].count, 3);
        assert!(st.at_limit[0]
            .label()
            .contains("exactly at minimum clearance (no margin)"));
        assert!(st.at_limit[0].label().contains("3 locations"));
        assert_eq!(st.violations.len(), 1);
        assert!(st.violations[0].label().contains("below the"));

        let mut report = drc_report(Vec::new());
        for _ in 0..2 {
            report.findings.push(clearance("GND", "+3.3V", 0.10, 0.2));
        }
        for _ in 0..3 {
            report.findings.push(clearance("GND", "+3.3V", 0.20, 0.2));
        }
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.violations.len(), 1);
        let g = &st.violations[0];
        assert_eq!((g.count, g.below_count), (5, 2));
        let label = g.label();
        assert!(
            label.contains("2 below") && label.contains("3 at the limit"),
            "{label}"
        );
    }

    /// A GND pour (stored fill) with a net-A track inside it, as the extractor
    /// reports it.
    fn stale(zone_net: &str, copper_net: &str) -> DrcFinding {
        let mut f = super::test_finding(
            ViolationKind::StaleFill,
            (zone_net, copper_net),
            (10.0, 20.0),
            (0.0, 0.2),
        );
        f.item_a.kind = hauksbee_extract::ItemKind::Zone;
        f
    }

    #[test]
    fn a_track_in_a_stored_fill_is_one_non_gating_refill_warning_not_a_short() {
        let mut report = drc_report(Vec::new());
        for _ in 0..3 {
            report.findings.push(stale("GND", "A"));
        }
        let st = DrcStructured::from_report(&report);
        assert!(st.shorts.is_empty(), "never a short");
        assert!(st.violations.is_empty() && st.at_limit.is_empty());
        assert_eq!(st.stale_fills.len(), 1, "grouped by net pair and layer");
        let sf = &st.stale_fills[0];
        assert_eq!((sf.zone_net.as_str(), sf.copper_net.as_str()), ("GND", "A"));
        assert_eq!((sf.count, sf.copper_kind.as_str()), (3, "track"));
        assert_eq!(
            sf.plain,
            "zone fill is out of date (a A track lies inside the stored GND fill); \
             refill zones (B) in KiCad and re-run"
        );

        // Plain report: a warning that names the refill, not a serious finding.
        let plain = crate::plain::plain_drc_structured(&st);
        assert_eq!(plain.findings.len(), 1);
        assert_eq!(plain.findings[0].level, crate::plain::PlainLevel::Warning);
        assert!(plain.findings[0].what.contains("out of date"), "{plain:?}");
        assert!(plain.findings[0].fix.contains("refill"));

        // `--json`: a warning finding that does not gate.
        let json = crate::reports::drc::drc_findings(&st, None);
        assert_eq!(json.len(), 1);
        assert_eq!(
            (json[0].kind.as_str(), json[0].severity.as_str()),
            ("stale_fill", "warning")
        );
        assert!(!json[0].gates());
        let text = st
            .render_with_clearance_rule_provenance(&ClearanceRuleProvenance::defaulted(0.2, false));
        assert!(text.contains("OUT-OF-DATE ZONE FILL"), "{text}");

        // The same pair as a real short stays serious and gating.
        let st = DrcStructured::from_report(&drc_report(vec![short("GND", "A")]));
        assert!(st.stale_fills.is_empty());
        assert_eq!(st.shorts[0].severity, "serious");
    }

    #[test]
    fn a_stale_fill_on_a_via_or_pad_names_it() {
        for (kind, word) in [
            (hauksbee_extract::ItemKind::Via, "via"),
            (hauksbee_extract::ItemKind::Pad, "pad"),
        ] {
            let mut f = stale("GND", "A");
            f.item_b.kind = kind;
            let st = DrcStructured::from_report(&drc_report(vec![f]));
            assert!(st.shorts.is_empty(), "{word}: never a short");
            assert_eq!(st.stale_fills[0].copper_kind, word);
            assert!(
                st.stale_fills[0]
                    .plain
                    .contains(&format!("a A {word} lies inside")),
                "{word}"
            );
        }
    }

    #[test]
    fn note_kinds_serialize_as_snake_case() {
        let note = JsonNote {
            kind: JsonNoteKind::BootControlNet,
            message: "GATE left floating".to_string(),
        };
        assert!(serde_json::to_string(&note)
            .unwrap()
            .contains("\"boot_control_net\""));
    }

    /// Faulty: a starved-rail finding that worked out its own remedy (a feed
    /// resistance bound) is acted on by that remedy, on the JSON fix and on
    /// the verdict's next step, never by the check kind's "move the pin"
    /// advice; and the verdict does not say the remedy twice.
    #[test]
    fn a_findings_own_remedy_is_its_fix_and_the_verdicts_next_step() {
        let message = "'ANALOG_VDD' is starved: it reaches only 0.119 V. Remedy: Feed the rail \
                       through no more than 122 ohm in total, or directly from '+5V'.";
        let json = lint_findings_json(&hauksbee_extract::NetLintReport {
            findings: vec![hauksbee_extract::LintFinding {
                check: LintCheck::OperatingEnvelope,
                severity: Severity::High,
                message: message.to_string(),
                refs: vec!["R_SHUNT".to_string()],
                nets: vec!["ANALOG_VDD".to_string()],
            }],
        });
        let fix = json[0].fix.as_deref().expect("a fix");
        assert!(
            fix.starts_with("Feed the rail through no more than 122 ohm"),
            "{fix}"
        );
        assert!(!fix.contains("compatible rail"), "{fix}");
        let reason = VerdictCause::Failing {
            count: 1,
            first: Some((json[0].plain.as_str(), fix)),
            same_on: "",
        }
        .reason();
        assert_eq!(
            reason.matches("no more than 122 ohm").count(),
            1,
            "{reason}"
        );
        assert!(
            reason.contains("first: 'ANALOG_VDD' is starved"),
            "{reason}"
        );
    }

    /// Faulty: two mirrored parts (D1 and its right-hand copy rD1) fail alike.
    /// The verdict says "2 blocking problem(s)", so its first-finding clause
    /// names the other part it is repeated on instead of leaving it unnamed.
    /// Legal control: a finding on one part names no other.
    #[test]
    fn the_verdict_names_the_other_parts_its_first_finding_is_repeated_on() {
        let on = |part: &str| hauksbee_extract::LintFinding {
            check: LintCheck::DcOverstress,
            severity: Severity::High,
            message: format!("{part} carries 851.0 mA, over its 125.0 mA rating"),
            refs: vec![part.to_string()],
            nets: Vec::new(),
        };
        let json = lint_findings_json(&hauksbee_extract::NetLintReport {
            findings: vec![on("D1"), on("rD1"), on("D7")],
        });
        let gating = || json.iter().filter(|f| f.gates());
        let lead = Some((json[0].plain.as_str(), ""));
        assert_eq!(first_siblings(lead, gating()), "rD1, D7");
        let alone = lint_findings_json(&hauksbee_extract::NetLintReport {
            findings: vec![on("D1")],
        });
        let lead = Some((alone[0].plain.as_str(), ""));
        assert_eq!(first_siblings(lead, alone.iter().filter(|f| f.gates())), "");
        let reason = VerdictCause::Failing {
            count: 2,
            first: Some((json[0].plain.as_str(), "")),
            same_on: "rD1",
        }
        .reason();
        assert!(reason.contains("(the same on rD1)"), "{reason}");
    }

    /// Legal control: a finding with no remedy of its own keeps the check
    /// kind's advice.
    #[test]
    fn a_finding_without_its_own_remedy_keeps_the_kind_advice() {
        let json = lint_findings_json(&hauksbee_extract::NetLintReport {
            findings: vec![hauksbee_extract::LintFinding {
                check: LintCheck::OperatingEnvelope,
                severity: Severity::High,
                message: "U1 VIN at 6.2 V is above its 5.5 V maximum".to_string(),
                refs: vec!["U1".to_string()],
                nets: vec!["VIN".to_string()],
            }],
        });
        assert_eq!(
            json[0].fix.as_deref(),
            lint_fix_hint(LintCheck::OperatingEnvelope, Severity::High)
        );
    }

    /// Every surface lists the pass conditions one under another and joins
    /// them into the headline, so each starts like a sentence whatever the
    /// source worded it as.
    #[test]
    fn pass_conditions_start_with_a_capital_letter() {
        let mut jr = JsonReport::new("b", summary_with(Vec::new(), Vec::new()));
        jr.verdict_conditions = vec![
            "copper spacing was judged against a DEFAULT 0.200 mm clearance.".to_string(),
            "Rails held at their named voltage.".to_string(),
            "copper spacing was judged against a DEFAULT 0.200 mm clearance.".to_string(),
        ];
        assert_eq!(
            jr.pass_conditions(),
            [
                "Copper spacing was judged against a DEFAULT 0.200 mm clearance.",
                "Rails held at their named voltage.",
            ]
        );
    }

    fn verdict(
        verdict: &'static str,
        reason: Option<&str>,
        partial: bool,
        conditions: &[&str],
        inspect: usize,
    ) -> RunVerdict {
        RunVerdict {
            verdict,
            reason: reason.map(str::to_string),
            conditions: conditions.iter().map(ToString::to_string).collect(),
            partial,
            not_judged: None,
            inspect,
            unorderable: None,
        }
    }

    /// A run that judged the board twice is only as good as its worse pass,
    /// whichever pass was worse, and never reads softer than either.
    #[test]
    fn the_combined_verdict_is_the_worse_of_the_two_passes() {
        let pass = verdict("pass", None, false, &[], 0);
        let invalid = verdict("invalid", Some("cannot vouch."), false, &[], 0);
        let fail = verdict("fail", Some("1 blocking problem(s)."), false, &[], 0);
        for (stat, cosim, want) in [
            (&pass, &pass, "pass"),
            (&pass, &invalid, "invalid"),
            (&invalid, &pass, "invalid"),
            (&pass, &fail, "fail"),
            (&fail, &pass, "fail"),
            (&invalid, &fail, "fail"),
            (&fail, &invalid, "fail"),
            (&invalid, &invalid, "invalid"),
            (&fail, &fail, "fail"),
        ] {
            assert_eq!(stat.combine(cosim).verdict, want, "{stat:?} + {cosim:?}");
        }
    }

    /// Words of a non-pass stand as they are when only one pass is one; two
    /// that say different things are both kept, the static one leading, and two
    /// that say the same thing (the same unmodelled parts) are said once.
    #[test]
    fn the_combined_reason_keeps_what_each_pass_found_and_says_a_shared_one_once() {
        let pass = verdict("pass", None, false, &[], 0);
        let fail = verdict("fail", Some("1 blocking problem(s)."), false, &[], 0);
        let stress = verdict("fail", Some("a MOSFET is overstressed."), false, &[], 0);
        let same = verdict("invalid", Some("2 key parts have no model."), false, &[], 0);
        assert_eq!(
            pass.combine(&fail).reason.as_deref(),
            Some("1 blocking problem(s).")
        );
        assert_eq!(
            fail.combine(&stress).reason.as_deref(),
            Some(&*format!(
                "1 blocking problem(s).{NOT_JUDGED_JOINER}a MOSFET is overstressed."
            ))
        );
        assert_eq!(
            same.combine(&same).reason.as_deref(),
            Some("2 key parts have no model.")
        );
        // A partly judged pass stays partly judged when nothing worse is found.
        let partly = verdict("invalid", Some("PARTLY JUDGED: x."), true, &[], 0);
        let combined = partly.combine(&pass);
        assert!(
            combined.partial && combined.verdict == "invalid",
            "{combined:?}"
        );
        // A failure outranks it, and is not called partly judged.
        assert!(!partly.combine(&fail).partial);
    }

    /// A pass rests on both passes' assumptions, each once, and on every item
    /// either pass asked to be inspected.
    #[test]
    fn a_combined_pass_carries_both_passes_conditions_and_inspect_counts() {
        let stat = verdict("pass", None, false, &["rail held", "default rules"], 2);
        let cosim = verdict("pass", None, false, &["rail held", "assumed clock"], 1);
        let combined = stat.combine(&cosim);
        assert_eq!(combined.verdict, "pass");
        assert_eq!(
            combined.conditions,
            ["rail held", "default rules", "assumed clock"]
        );
        assert_eq!(combined.inspect, 3);
        assert_eq!(combined.reason, None);
    }
}
