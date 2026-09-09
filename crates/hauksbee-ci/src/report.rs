//! Rendering the outcome of a CI run into the formats a pipeline consumes: a
//! human-readable terminal report, JUnit XML for any CI system to ingest, and
//! GitHub Actions workflow-command annotations. [`CiResult`] also owns the process
//! exit code and the honest ensemble-coverage wording, so a green run never
//! over-claims worst-case proof and a diverged co-sim refuses rather than
//! reporting a fake verdict.

use std::time::Duration;

use crate::assertions::AssertResult;
use hauksbee_engine::result::Refusal;

/// Published JSON shape for one evaluated assertion. Kept separate from the
/// runtime [`AssertResult`] because subject nets/refs are internal waiver keys,
/// while the causal evidence map is part of the external contract. The
/// committed schema is generated from this type via [`CiJsonReport`]: see
/// `crates/hauksbee-ci/tests/ci_report_schema_drift.rs`. `why`, `waived`, and
/// `evidence` are the only fields a consumer may find ABSENT rather than null.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CiJsonAssertion {
    /// The assertion's label: its `label` in the spec, or a generated one
    /// naming the kind and subject.
    pub label: String,
    /// The assertion kind, as the spec's `kind` token (`voltage`, `uart`,
    /// `blink`, `no_faults`, `hwtrace`, ...).
    pub kind: String,
    /// Did the assertion hold on every ensemble member. False on an ordinary
    /// red, on a waived red, and on an INVALID result.
    pub passed: bool,
    /// A THIRD outcome distinct from pass/fail: the assertion could
    /// not be honestly evaluated because its analog evaluation window overlaps
    /// a chunk the solver failed on. When `invalid` is true, `passed` is always
    /// false, but the run exits 3 (invalid-for-analysis) rather than 1.
    pub invalid: bool,
    /// One-line detail (the measured value, the offending seed, etc).
    pub detail: String,
    /// If it failed, the first seed index that failed (for fuzzed runs).
    /// Always present in the JSON, `null` on a pass, unlike `why` and `waived`
    /// which are omitted.
    #[schemars(schema_with = "crate::assertions::schema_nullable_seed", required)]
    pub failing_seed: Option<u32>,
    /// Every ensemble member this assertion failed on (empty on a pass).
    pub failing_seeds: Vec<u32>,
    /// How many ensemble members were evaluated.
    pub seeds_total: u32,
    /// On a real red: one sentence naming the OBSERVED shortfall. ABSENT (not
    /// null) on pass/INVALID and on kinds whose detail already carries the
    /// diagnosis.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::assertions::schema_absent_or_string")]
    pub why: Option<String>,
    /// Set when an active waiver covers this failure: the reason + expiry.
    /// ABSENT (not null) when no waiver covers this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::assertions::schema_absent_or_string")]
    pub waived: Option<String>,
    /// The causal evidence map for this assertion: its assumption ids,
    /// artifacts, models, parameters and numerical error budget. ABSENT (not
    /// null) when the run produced no map for this label.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "schema_absent_evidence")]
    pub evidence: Option<hauksbee_ir::evidence::EvidenceMap>,
}

/// The full result of a CI run: the spec name, per-assertion results, the seed
/// count, and timing.
#[derive(Debug)]
pub struct CiResult {
    pub spec_name: String,
    pub board: String,
    pub results: Vec<AssertResult>,
    pub seeds: u32,
    pub elapsed: Duration,
    /// True if any seed's analog co-sim tripped the consecutive-failed-chunk
    /// abort. Forces exit 3 on its own, even when no single assertion's
    /// window overlapped a failed span (e.g. a spec with only a UART assertion
    /// over a run whose analog side collapsed).
    pub analog_abort: bool,
    /// Present when the run was a component-tolerance ensemble: what kind of
    /// coverage the members provide. Drives the honest headline wording
    /// (sampled coverage vs monotonic-only bounds), so a green ensemble can
    /// never read as a worst-case proof.
    pub coverage: Option<EnsembleCoverage>,
    /// One message per MCU co-simulated on a SUBSTITUTE core (requested part not
    /// modelled, run on a less-specific one). Surfaced in every report format so a
    /// GREEN verdict never silently vouches for firmware on the wrong silicon.
    pub substitutions: Vec<String>,
    /// Co-sim coverage warnings (U3): dropped ADC injections (the firmware never
    /// received the solved voltage) and never-exercised bus peripherals (no
    /// matching controller modeled). Surfaced in every report format, exactly
    /// like `substitutions`, a GREEN over an un-run co-sim path must be
    /// qualified everywhere a pipeline reads.
    pub coverage_warnings: Vec<String>,
    /// Per-MCU measured edge timestamp and guaranteed-pulse limits at the
    /// actual negotiated chunk. Present even when no strict timing request was
    /// declared, so a green report states what timing it covered.
    pub timing_coverage: Vec<hauksbee_engine::scheduler::TimingCoverage>,
    /// Timing claims that could not be honored. Non-empty makes every
    /// assertion INVALID and cannot be waived.
    pub timing_refusals: Vec<String>,
    /// Nets that name themselves a supply, carry a rail's worth of parts, and
    /// nothing powers (see `runner::dead_rails`). Surfaced FIRST in every
    /// format, ahead of the assertion list: with a rail dead the operating
    /// point is fiction, so this changes how every result below it reads, and a
    /// reader who meets it after a named component fault has already believed
    /// the fault.
    pub dead_rails: Vec<String>,
    /// Waiver-file housekeeping notes (a lapsed waiver whose finding gates
    /// again, an active waiver that matched nothing, a malformed file that was
    /// ignored). Surfaced on every format like `coverage_warnings`: a board
    /// carrying waivers has to look like one.
    pub waiver_notes: Vec<String>,
    /// Exact files consumed by this run, shared with engine/web reports.
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// The one typed assumption registry every CI renderer projects.
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// One causal evidence map per evaluated assertion.
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
}

/// What a tolerance-ensemble run covered, for the report headline.
#[derive(Debug, Clone)]
pub enum EnsembleCoverage {
    /// Random sampling: statistical evidence over the tolerance space. `seeds`
    /// is the number of genuinely SAMPLED seeds, excluding the nominal baseline
    /// (member 0, which draws no random sample).
    MonteCarlo { seeds: u32, components: usize },
    /// Deterministic all-min/all-max enumeration, plus the `interior` stratified
    /// probes that test the monotonicity the corner bound rests on. The two are
    /// counted separately because only the corners carry the bounded claim.
    Corners {
        corners: u32,
        interior: u32,
        components: usize,
    },
    /// A single pinned ensemble member (`--seed N`): the runner filtered the
    /// ensemble down to exactly this one, so the nominal-baseline / sampled-count
    /// arithmetic doesn't apply, report the member honestly instead. `corners`
    /// distinguishes a deterministic corner (corners mode) from a random draw
    /// (Monte-Carlo), so the banner matches the mode-aware per-assertion wording.
    /// `interior` is set when the pinned member turned out to be a corner-mode
    /// INTERIOR probe rather than a corner: the member numbering runs on past the
    /// last corner, so mode alone cannot tell which one `--seed N` selected, and
    /// calling a probe "corner N" sends the reader looking for a min/max
    /// combination that does not exist.
    SingleMember {
        seed: u32,
        components: usize,
        corners: bool,
        interior: bool,
    },
}

impl EnsembleCoverage {
    /// The one-line coverage claim, worded so it cannot over-claim: Monte-Carlo
    /// is sampled coverage (never proof); corners bound the worst case only
    /// where the response is monotonic in each value, and the interior probes
    /// are evidence for that monotonicity rather than proof of it.
    ///
    /// This string is the banner AND the JSON `coverage` field AND what the
    /// evidence map carries, so it must never claim more than the per-assertion
    /// detail lines in `assertions::all_green_detail` do.
    pub fn describe(&self) -> String {
        match self {
            EnsembleCoverage::MonteCarlo { seeds, components } => format!(
                "tolerance ensemble: nominal baseline + {seeds} sampled seed(s) over \
                 {components} toleranced component(s): statistical coverage, not \
                 worst-case proof"
            ),
            EnsembleCoverage::Corners {
                corners,
                interior: 0,
                components,
            } => format!(
                "tolerance corners: {corners} deterministic min/max corner(s) over \
                 {components} component(s): bounds the worst case only where the \
                 response is monotonic in each value"
            ),
            EnsembleCoverage::Corners {
                corners,
                interior,
                components,
            } => format!(
                "tolerance corners: {corners} deterministic min/max corner(s) + {interior} \
                 interior probe(s) over {components} component(s): bounds the worst case \
                 only where the response is monotonic in each value, and the probes sample \
                 the interior for a point that breaks an assertion the corners passed \
                 (evidence for that monotonicity, not proof of it)"
            ),
            EnsembleCoverage::SingleMember {
                seed,
                components,
                interior: true,
                ..
            } => format!(
                "single interior probe: interior probe {seed} over {components} toleranced \
                 component(s): one pinned point strictly inside the tolerance ranges, \
                 carrying none of the corner set's bounded claim"
            ),
            EnsembleCoverage::SingleMember {
                seed,
                components,
                corners: true,
                interior: false,
            } => format!(
                "single corner: corner {seed} over {components} toleranced \
                 component(s): one pinned deterministic corner, not full corner coverage"
            ),
            EnsembleCoverage::SingleMember {
                seed,
                components,
                corners: false,
                interior: false,
            } => format!(
                "single ensemble member: seed {seed} over {components} toleranced \
                 component(s): one pinned draw, not ensemble coverage"
            ),
        }
    }
}

/// Version of the `hauksbee-ci run --json` document shape, carried in every
/// emitted line as `schema_version`. Additive fields do NOT bump it (a
/// consumer that ignores unknown keys keeps working); a removal, a rename, or
/// a changed meaning does. The published contract, including what counts as
/// additive, is `docs/ci/JSON_OUTPUT.md`.
pub const CI_REPORT_SCHEMA_VERSION: u32 = 2;

/// One line of `hauksbee-ci run --json`: either a run report or a spec/board
/// error. The two are told apart by `ok`, which is `true` on a report and
/// `false` on an error, so a consumer branches on one boolean before reading
/// anything else. A multi-spec invocation prints one of these per spec, one per
/// line (NDJSON), and the variants may be mixed within the one stream.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum CiJsonLine {
    /// A spec that ran. `ok` is `true`; the verdict is in `passed` / `exit_code`.
    Report(Box<CiJsonReport>),
    /// A spec that never ran: unreadable spec, desynced net/component
    /// reference, missing board or firmware. `ok` is `false` and the run
    /// contributes exit 2.
    Error(CiJsonError),
}

/// The machine-readable result of one spec's run: the `--json` document, and
/// what the web checks panel consumes via `/api/check`.
///
/// Every field below is ALWAYS present, including `coverage`, which is `null`
/// rather than absent when the run was not a tolerance ensemble, except the
/// top-level `refusal` (exit 3 only). Inside each result, `why`, `waived`,
/// and `evidence` are also conditionally absent (see [`CiJsonAssertion`]),
/// omitted entirely rather than set to `null`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CiJsonReport {
    /// [`CI_REPORT_SCHEMA_VERSION`]: the shape of this document.
    pub schema_version: u32,
    /// Always `true` on a report line. A `false` here is the error variant,
    /// which carries `error` and none of the fields below.
    #[schemars(schema_with = "schema_true")]
    pub ok: bool,
    /// The spec's `name`, or its file stem when the spec did not set one.
    pub spec_name: String,
    /// The board file the spec bound, as the spec wrote it.
    pub board: String,
    /// The OVERALL verdict: `exit_code == 0`. Green only when the run was
    /// trustworthy AND every assertion held. Never read this without
    /// `run_valid`: `assertions_passed` alone can be true over a run whose
    /// analog side collapsed.
    pub passed: bool,
    /// Did every assertion pass or carry an active waiver. True with
    /// `run_valid: false` means "nothing failed, but do not trust it".
    pub assertions_passed: bool,
    /// Was the run trustworthy: `false` when the analog co-sim aborted or any
    /// assertion's evaluation window overlapped a failed solve chunk.
    pub run_valid: bool,
    /// The process exit code this run contributes: 0 green, 1 red, 3 invalid
    /// for analysis. Never 2 here, a spec error is the error variant instead.
    pub exit_code: i32,
    /// Did the analog co-sim trip the consecutive-failed-chunk abort. Forces
    /// `exit_code` 3 on its own, even when no single assertion was affected.
    pub analog_abort: bool,
    /// Useful-refusal contract, present exactly when `exit_code == 3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    /// How many ensemble members ran (fuzz seeds, or tolerance members).
    pub seeds: u32,
    /// Wall-clock duration of the run, in seconds.
    pub elapsed_s: f64,
    /// The one-line tolerance-ensemble coverage claim, worded so it cannot
    /// over-claim. `null` when the run was not an ensemble. Always present:
    /// nullable, not absent.
    #[schemars(schema_with = "schema_nullable_string", required)]
    pub coverage: Option<String>,
    /// One entry per MCU co-simulated on a SUBSTITUTE core. Non-empty means a
    /// green verdict does not vouch for firmware on the requested silicon.
    pub substitutions: Vec<String>,
    /// Co-sim coverage holes: dropped ADC injections, never-exercised bus
    /// peripherals. Non-empty means part of the co-sim path never ran.
    pub coverage_warnings: Vec<String>,
    /// Measured timing coverage per live MCU backend.
    pub timing_coverage: Vec<hauksbee_engine::scheduler::TimingCoverage>,
    /// Explicit reasons timing evidence was invalidated.
    pub timing_refusals: Vec<String>,
    /// Nets that name themselves a supply but that nothing powered. Non-empty
    /// means the operating point every result below was solved around is
    /// fiction.
    pub dead_rails: Vec<String>,
    /// Waiver-file housekeeping: a lapsed waiver, an active waiver that matched
    /// nothing, a malformed file that was ignored.
    pub waiver_notes: Vec<String>,
    /// Exact files this run consumed (board, firmware, spec, models), with
    /// content hashes: the input inventory the evidence maps refer to.
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// The canonical typed assumption registry. Every honesty qualifier
    /// (substitute core, coverage hole, reduced-fidelity reader) appears here
    /// as a typed record; `substitutions` / `coverage_warnings` remain
    /// compatibility projections of the same facts.
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// One causal evidence map per evaluated assertion, board-scoped maps
    /// included. Each map's `assumptions` ids resolve in `assumptions` above.
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
    /// One entry per assertion, in spec order. An `hwtrace` assertion expands
    /// to one entry per (channel, feature), so this can be longer than the
    /// spec's `[[assert]]` list.
    pub results: Vec<CiJsonAssertion>,
}

/// The error line: a spec that could not be run at all.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CiJsonError {
    /// Always `false` on an error line.
    #[schemars(schema_with = "schema_false")]
    pub ok: bool,
    /// Why the spec did not run, the same sentence stderr carries.
    pub error: String,
}

impl CiJsonError {
    /// The error line for a spec that could not be run.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: error.into(),
        }
    }

    /// Serialize to the single line `hauksbee-ci run --json` prints.
    pub fn render_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"ok\":false,\"error\":\"could not serialize the spec error\"}".to_string()
        })
    }
}

/// `ok: true`, as a schema constant: the discriminator a consumer branches on
/// has to be pinned, or the two variants of [`CiJsonLine`] are structurally
/// ambiguous to a validator.
fn schema_true(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "boolean", "const": true })
}

/// `ok: false`, the error variant's discriminator. See [`schema_true`].
fn schema_false(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "boolean", "const": false })
}

/// A string that is ALWAYS emitted and may be `null`. Written by hand because
/// schemars' `required` attribute marks the key required by dropping `null`
/// from the type, which would promise a string where a consumer really does
/// see `null`, the opposite of the field's contract.
pub(crate) fn schema_nullable_string(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["string", "null"] })
}

/// An evidence object that is absent when no map was produced, never `null`
/// when present.
fn schema_absent_evidence(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    generator.subschema_for::<hauksbee_ir::evidence::EvidenceMap>()
}

impl CiResult {
    /// One C5.3 refusal shared verbatim by JSON, terminal, JUnit, annotations,
    /// the web checks panel, and MCP.
    pub fn refusal(&self) -> Option<Refusal> {
        if !self.analog_invalid() {
            return None;
        }
        let invalid = self.invalid_count();
        // An assertion is INVALID for one of three reasons, each with its own
        // fix: an unresolved part undermined its evidence (write or bind a
        // model), a timing measurement was refused (change the backend or the
        // assertion), or the analog solve diverged under it (fix the named
        // net). The footer must send the reader to the right one; "inspect the
        // failed window" is wrong advice when no window failed.
        let count = |prefix: &str| {
            self.results
                .iter()
                .filter(|r| r.invalid && r.detail.starts_with(prefix))
                .count()
        };
        let evidence = count("INVALID evidence:");
        let timing = count("INVALID: timing coverage refusal:");
        let missing = if self.analog_abort {
            format!(
                "a converged analog solve through the run; the consecutive-failure abort tripped and {invalid} assertion(s) were marked INVALID"
            )
        } else if evidence + timing == invalid {
            let mut parts = Vec::new();
            if evidence > 0 {
                parts.push(format!(
                    "a model for every part the assertions rest on; {evidence} assertion(s) depend on an unresolved (open) part"
                ));
            }
            if timing > 0 {
                parts.push(format!(
                    "a backend that can measure the requested timing; {timing} assertion(s) were refused"
                ));
            }
            parts.join("; ")
        } else {
            format!(
                "a converged analog solve across every assertion window; {invalid} assertion(s) overlapped held-stale analog spans"
            )
        };
        let mut partial = vec![
            "the spec and board loaded; every emitted assertion record remains available"
                .to_string(),
        ];
        for r in &self.results {
            if !r.invalid && r.kind == "uart" {
                partial.push(format!(
                    "UART assertion '{}' retained its {} result",
                    r.label,
                    if r.passed { "PASS" } else { "FAIL" }
                ));
            }
        }
        let next = self
            .results
            .iter()
            .find(|r| r.invalid)
            .map(|r| {
                if r.detail.starts_with("INVALID evidence:") {
                    format!(
                        "give the open part(s) named under INVALID assertion '{}' a model (hauksbee models new, or a [[supply]] for a battery net), then rerun the same spec",
                        r.label
                    )
                } else if r.detail.starts_with("INVALID: timing coverage refusal:") {
                    format!(
                        "read the timing refusal under INVALID assertion '{}'; run on a backend that measures it, or drop the timing claim, then rerun the same spec",
                        r.label
                    )
                } else {
                    format!(
                        "inspect INVALID assertion '{}' and its failed-window detail, fix the first named net/device, then rerun the same spec",
                        r.label
                    )
                }
            })
            .unwrap_or_else(|| {
                "inspect the first analog non-convergence diagnosis in the job log, fix its named net/device, then rerun the same spec"
                    .to_string()
            });
        Some(Refusal::new(
            "an overall CI verdict for the requested assertions",
            missing,
            partial,
            next,
        ))
    }

    /// True if every assertion passed or was waived. An INVALID assertion has
    /// `passed == false` and can never be waived, so it is not counted here.
    /// A waived failure is visible-but-not-gating (the whole point of a
    /// waiver): it stays visible as `[WAIVED]` in the terminal report, a
    /// `<skipped>` JUnit testcase, a `::warning` annotation and the `waived`
    /// JSON field, without turning the build red. The web checks panel is the
    /// exception and renders it as a plain FAIL (see docs/ci/CI.md).
    pub fn passed(&self) -> bool {
        self.results.iter().all(|r| r.passed || r.waived.is_some())
    }

    /// Results that gate the build red: real failures no active waiver covers.
    fn gating_failures(&self) -> impl Iterator<Item = &AssertResult> {
        self.results
            .iter()
            .filter(|r| !r.passed && !r.invalid && r.waived.is_none())
    }

    /// Count of failures an active waiver covers (visible, not gating).
    pub fn waived_count(&self) -> usize {
        self.results.iter().filter(|r| r.waived.is_some()).count()
    }

    /// The run result as the published JSON document. Built as a real type
    /// rather than an ad-hoc object so that the committed schema
    /// (`crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json`) is
    /// GENERATED from it and a new field cannot ship without appearing in the
    /// contract: `tests/ci_report_schema_drift.rs` fails when the two diverge.
    pub fn json_report(&self) -> CiJsonReport {
        CiJsonReport {
            schema_version: CI_REPORT_SCHEMA_VERSION,
            ok: true,
            spec_name: self.spec_name.clone(),
            board: self.board.clone(),
            // The OVERALL verdict is the process verdict: green only when the
            // run was valid AND every assertion held. `passed()` alone ignores
            // an analog abort (exit 3, every assertion left false-but-not-failed),
            // which would render a green "all passed" over an untrustworthy run.
            passed: self.exit_code() == 0,
            // The two components, so a consumer can tell "an assertion failed"
            // from "the run itself was not trustworthy".
            assertions_passed: self.passed(),
            run_valid: !self.analog_invalid(),
            exit_code: self.exit_code(),
            analog_abort: self.analog_abort,
            refusal: self.refusal(),
            seeds: self.seeds,
            elapsed_s: self.elapsed.as_secs_f64(),
            coverage: self.coverage.as_ref().map(|c| c.describe()),
            substitutions: self.substitutions.clone(),
            coverage_warnings: self.coverage_warnings.clone(),
            timing_coverage: self.timing_coverage.clone(),
            timing_refusals: self.timing_refusals.clone(),
            dead_rails: self.dead_rails.clone(),
            waiver_notes: self.waiver_notes.clone(),
            inventory: self.inventory.clone(),
            assumptions: self.assumptions.clone(),
            evidence: self.evidence.clone(),
            results: self
                .results
                .iter()
                .map(|result| CiJsonAssertion {
                    label: result.label.clone(),
                    kind: result.kind.clone(),
                    passed: result.passed,
                    invalid: result.invalid,
                    detail: result.detail.clone(),
                    failing_seed: result.failing_seed,
                    failing_seeds: result.failing_seeds.clone(),
                    seeds_total: result.seeds_total,
                    why: result.why.clone(),
                    waived: result.waived.clone(),
                    evidence: self.evidence_for(&result.label).cloned(),
                })
                .collect(),
        }
    }

    /// Machine-readable run result (the `--json` surface, and what the web
    /// checks panel consumes via `/api/check`). One stable JSON object: the
    /// overall verdict, the per-assertion results verbatim, and every honesty
    /// qualifier the human report carries (analog abort, substitutions,
    /// coverage wording), a consumer must never see a cleaner story than the
    /// terminal does.
    pub fn render_json(&self) -> String {
        serde_json::to_string(&self.json_report()).unwrap_or_else(|e| {
            CiJsonError::new(format!("could not serialize the run result: {e}")).render_json()
        })
    }

    pub fn pass_count(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }

    /// Count of assertions that could not be honestly evaluated (their window
    /// overlapped a failed analog chunk).
    pub fn invalid_count(&self) -> usize {
        self.results.iter().filter(|r| r.invalid).count()
    }

    /// True when the run is invalid-for-analysis: at least one assertion is
    /// INVALID, or the analog co-sim tripped the strict abort.
    pub fn analog_invalid(&self) -> bool {
        self.analog_abort || self.results.iter().any(|r| r.invalid)
    }

    /// Process exit code: 3 invalid-for-analysis (any INVALID assertion or a
    /// tripped analog abort), else 1 any ordinary red, else 0 all green.
    /// The invalid path is checked first so a diverged co-sim refuses rather than
    /// reports a fake pass/fail.
    pub fn exit_code(&self) -> i32 {
        if self.analog_invalid() {
            hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
        } else if self.passed() {
            0
        } else {
            1
        }
    }

    fn evidence_for(&self, label: &str) -> Option<&hauksbee_ir::evidence::EvidenceMap> {
        self.evidence.iter().find(|map| map.assertion() == label)
    }

    fn evidence_text(&self, label: &str) -> String {
        let Some(map) = self.evidence_for(label) else {
            return String::new();
        };
        let mut out = format!("evidence: {}", map.status());
        for id in map.assumptions() {
            if let Some(assumption) = self.assumptions.iter().find(|a| a.id() == id) {
                out.push_str(&format!(
                    "\n[{}] {} Why: {} Effect: {} Fix: {}",
                    id,
                    assumption.statement(),
                    assumption.because(),
                    assumption.consequence(),
                    assumption.replacement()
                ));
            }
        }
        if let Some(budget) = map.error_budget() {
            out.push_str("\nerror budget: ");
            out.push_str(&budget.plain_summary());
        }
        for model in map.models() {
            let source = model.source();
            let accuracy = if source.uncertainty().iter().any(|value| {
                matches!(
                    value,
                    hauksbee_ir::evidence::ModelUncertainty::Unknown { .. }
                )
            }) {
                "uncertainty unknown"
            } else if source
                .uncertainty()
                .iter()
                .any(|value| !value.is_strict_bound())
            {
                "non-guaranteed typical/estimated range"
            } else {
                "validated two-sided bound"
            };
            out.push_str(&format!(
                "\nmodel {}={} source={} validation={} {accuracy}",
                model.cited_reference(),
                model.model_id(),
                source.tier(),
                source.validation(),
            ));
        }
        out
    }

    /// The unpowered-rail warning, or empty when every rail is fed.
    ///
    /// Printed BEFORE the assertions, because it changes what they mean rather
    /// than adding to them. A rail sitting at 0 V because nobody could work out
    /// its voltage makes the operating point around it fiction, and the stress
    /// monitor will report on that fiction as confidently as on a real
    /// overload. Someone who reads "R_Shunt15301 overpower" first and the
    /// caveat afterwards has already believed the accusation.
    fn dead_rail_banner(&self) -> String {
        if self.dead_rails.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n  UNPOWERED RAIL: ");
        s.push_str(&self.dead_rails.join(", "));
        s.push('\n');
        s.push_str(
            "        These nets name a supply but not a voltage, so nothing powered\n\
             \x20       them and they sat at 0 V. Every analog result below was solved\n\
             \x20       around that, so a fault it names may be an artifact rather than\n\
             \x20       a finding about your board. Add a [[supply]] for each, then run\n\
             \x20       again before acting on anything here.\n",
        );
        s
    }

    /// Human-readable terminal report.
    /// Every qualification a green verdict would otherwise hide, as
    /// `(tag, message)` pairs.
    ///
    /// One producer feeds all three human-facing formats, so a note cannot
    /// reach the log and quietly miss the JUnit tab a dashboard reader lives
    /// in: a substitute MCU core, a co-sim path that never ran (dropped ADC
    /// injection, unexercised bus device), the per-MCU timing resolution, a
    /// timing claim the backend could not represent, and waiver housekeeping.
    fn honesty_notes(&self) -> Vec<(&'static str, String)> {
        fn tagged<'m>(
            tag: &'static str,
            msgs: &'m [String],
        ) -> impl Iterator<Item = (&'static str, String)> + 'm {
            msgs.iter().map(move |m| (tag, m.clone()))
        }
        tagged("co-sim ran on a SUBSTITUTE chip", &self.substitutions)
            .chain(tagged("co-sim COVERAGE HOLE", &self.coverage_warnings))
            .chain(
                self.timing_coverage
                    .iter()
                    .map(|t| ("TIMING COVERAGE", timing_coverage_note(t))),
            )
            .chain(tagged("TIMING INVALID", &self.timing_refusals))
            .chain(tagged("waivers", &self.waiver_notes))
            .collect()
    }

    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "hauksbee-ci: {}\n  board: {}\n  seeds: {}\n",
            self.spec_name, self.board, self.seeds
        ));
        if let Some(cov) = &self.coverage {
            out.push_str(&format!("  {}\n", cov.describe()));
        }
        out.push_str(&self.dead_rail_banner());
        out.push('\n');
        for r in &self.results {
            let mark = if r.invalid {
                "INVALID"
            } else if r.passed {
                "PASS"
            } else if r.waived.is_some() {
                "WAIVED"
            } else {
                "FAIL"
            };
            out.push_str(&format!("  [{mark}] {}\n        {}\n", r.label, r.detail));
            for line in self.evidence_text(&r.label).lines() {
                out.push_str(&format!("        {line}\n"));
            }
            // On a real red (not a pass, not an INVALID refusal), add one
            // actionable "why" line.
            if !r.passed && !r.invalid {
                if let Some(why) = why_line(r) {
                    out.push_str(&format!("        why: {why}\n"));
                }
                if let Some(w) = &r.waived {
                    out.push_str(&format!(
                        "        waived: {w}; visible here, not gating the build\n"
                    ));
                }
            }
        }
        let total = self.results.len();
        let passed = self.pass_count();
        // An analog abort that no assertion happened to cover still forces a
        // refusal, so surface it plainly rather than printing a misleading GREEN.
        if self.analog_abort && self.invalid_count() == 0 {
            out.push_str(
                "  analog co-sim aborted (solve failed on too many chunks in a row); \
                 the run is INVALID for analysis\n",
            );
        }
        if let Some(refusal) = self.refusal() {
            out.push('\n');
            out.push_str(&refusal.render_text());
            out.push('\n');
        }
        for (tag, msg) in self.honesty_notes() {
            out.push_str(&format!("  {tag}: {msg}\n"));
        }
        let verdict = if self.analog_invalid() {
            "INVALID (run or assertion evidence is not trustworthy)"
        } else if self.passed() {
            "GREEN"
        } else {
            "RED"
        };
        let waived = self.waived_count();
        let waived_note = if waived > 0 {
            format!(" ({waived} failure(s) waived, visible above)")
        } else {
            String::new()
        };
        // Verdict FIRST: in a CI log or a chat notification the leading word
        // is what a skimming reader keeps, so an untrustworthy run must not
        // lead with "passed", the word every green run leads with.
        if self.analog_invalid() {
            out.push_str(&format!(
                "\n{} - {}/{} assertion(s) evaluated in {:.2}s; the counts are not a verdict{}\n",
                verdict,
                total,
                total,
                self.elapsed.as_secs_f64(),
                waived_note
            ));
        } else {
            out.push_str(&format!(
                "\n{} - {}/{} assertions passed in {:.2}s{}\n",
                verdict,
                passed,
                total,
                self.elapsed.as_secs_f64(),
                waived_note
            ));
        }
        // A RED report ends with where to read about the failing check: the
        // assertion catalog's section for the first gating failure's kind.
        if verdict == "RED" {
            if let Some(r) = self.gating_failures().next() {
                out.push_str(&format!(
                    "next: the \"{}\" section of {} explains this check and its knobs\n",
                    r.kind,
                    hauksbee_ir::docs_url("docs/ci/CI.md")
                ));
            }
        }
        out
    }

    /// JUnit XML: each assertion is a `<testcase>`, failures carry a
    /// `<failure>` with the detail. Any CI (GitLab, Jenkins, GitHub, Buildkite)
    /// ingests this.
    pub fn render_junit(&self) -> String {
        render_junit_document(std::slice::from_ref(&self.junit_suite()))
    }

    /// This run as ONE `<testsuite>` fragment plus its counters, so a
    /// multi-spec invocation can merge several runs (and spec errors) into a
    /// single `<testsuites>` document with honest aggregate counts.
    pub fn junit_suite(&self) -> JunitSuite {
        // INVALID assertions map to JUnit `<error>` (the test could not run to a
        // verdict), ordinary reds to `<failure>`. Keep the two counts distinct so
        // a CI dashboard shows "errored" apart from "failed".
        //
        // An analog abort that no assertion happened to cover still
        // forces exit 3, so it must surface here too: `render_human` and
        // `render_github_annotations` both special-case that state, and a JUnit
        // that said `failures="0" errors="0"` would be a false ALL-GREEN on the
        // one surface most CI dashboards actually read. Emit one synthetic
        // errored testcase (the same `<error>` shape `render_junit_error` uses)
        // and count it in tests/errors.
        let synthetic_abort = self.analog_abort && self.invalid_count() == 0;
        let errors = self.invalid_count() + usize::from(synthetic_abort);
        let tests = self.results.len() + usize::from(synthetic_abort);
        // Waived failures are JUnit `<skipped>`: visible in every dashboard's
        // test list (with the waiver reason as the message) without counting
        // as a failure, which is exactly the visible-but-not-gating contract.
        let failures = self.gating_failures().count();
        let skipped = self.waived_count();
        let mut out = String::new();
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">\n",
            xml_escape(&self.spec_name),
            tests,
            failures,
            errors,
            skipped,
            self.elapsed.as_secs_f64()
        ));
        for r in &self.results {
            let evidence = self.evidence_text(&r.label);
            let gating = !r.passed && !r.invalid && r.waived.is_none();
            // The body is the measured detail, then (on a gating red) the
            // `why:` line, then the evidence. Plenty of people only ever read
            // the Tests tab of a CI run, and the why is the actionable half:
            // without it the failure says what the number was and nothing
            // about what to do next.
            let body = [
                Some(r.detail.clone()),
                gating
                    .then(|| why_line(r))
                    .flatten()
                    .map(|w| format!("why: {w}")),
                (!evidence.is_empty()).then(|| evidence.clone()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
            let (message, body) = (xml_escape(&r.detail), xml_escape(&body));
            out.push_str(&format!(
                "    <testcase classname=\"{}\" name=\"{}\">\n",
                xml_escape(&r.kind),
                xml_escape(&r.label)
            ));
            if r.invalid {
                out.push_str(&format!(
                    "      <error message=\"{message}\">{body}</error>\n"
                ));
            } else if let (false, Some(w)) = (r.passed, &r.waived) {
                out.push_str(&format!(
                    "      <skipped message=\"waived FAIL: {message} ({})\"/>\n",
                    xml_escape(w)
                ));
                if !evidence.is_empty() {
                    out.push_str(&format!(
                        "      <system-out>{}</system-out>\n",
                        xml_escape(&evidence)
                    ));
                }
            } else if !r.passed {
                out.push_str(&format!(
                    "      <failure message=\"{message}\">{body}</failure>\n"
                ));
            } else {
                out.push_str(&format!("      <system-out>{body}</system-out>\n"));
            }
            out.push_str("    </testcase>\n");
        }
        if synthetic_abort {
            let msg = self
                .refusal()
                .map(|r| r.render_text())
                .unwrap_or_else(|| "the run is INVALID for analysis".to_string());
            out.push_str("    <testcase classname=\"analog\" name=\"analog co-sim converged\">\n");
            out.push_str(&format!(
                "      <error message=\"{}\">{}</error>\n",
                xml_escape(&msg),
                xml_escape(&msg)
            ));
            out.push_str("    </testcase>\n");
        }
        if let Some(refusal) = self.refusal() {
            out.push_str(&format!(
                "    <system-out>{}</system-out>\n",
                xml_escape(&refusal.render_text())
            ));
        }
        // A dead rail rides the same suite-level channel: a dashboard reader who
        // only ever sees the JUnit tab must not read a fault as a finding when
        // the operating point it came from had a rail at 0 V.
        if !self.dead_rails.is_empty() {
            out.push_str(&format!(
                "    <system-out>UNPOWERED RAIL: {} sat at 0 V; \
                 any analog result here may be an artifact</system-out>\n",
                xml_escape(&self.dead_rails.join(", "))
            ));
        }
        // The honesty notes ride the suite-level channel, so a dashboard-only
        // reader sees the same qualifications the log carries.
        for (tag, msg) in self.honesty_notes() {
            out.push_str(&format!(
                "    <system-out>{tag}: {}</system-out>\n",
                xml_escape(&msg)
            ));
        }
        out.push_str("  </testsuite>\n");
        JunitSuite {
            xml: out,
            tests,
            failures,
            errors,
            time_s: self.elapsed.as_secs_f64(),
        }
    }

    /// GitHub Actions annotations: `::error` / `::warning` / `::notice`
    /// workflow commands so failures surface inline in the Checks UI. The CLI
    /// writes them to stderr when `GITHUB_ACTIONS` is set so `--json` stdout
    /// remains pure NDJSON.
    ///
    /// The budget matters: GitHub shows at most 10 annotations per type per
    /// step and silently drops the rest, so this surface spends them on
    /// verdicts only. Passing assertions get NO per-assertion `::notice` (the
    /// log and JUnit carry them; a 12-assertion green spec must not burn the
    /// whole notice budget). Timing coverage gets one aggregated notice;
    /// failures/INVALIDs get at most
    /// [`Self::MAX_ERROR_ANNOTATIONS`] `::error`s plus one overflow line and
    /// the rollup, and warnings are capped at
    /// [`Self::MAX_WARNING_ANNOTATIONS`] plus one overflow line.
    pub fn render_github_annotations(&self) -> String {
        let mut out = String::new();
        // Per-assertion verdict errors, capped to leave room for the overflow
        // line and the rollup inside GitHub's 10-per-type truncation.
        let bad: Vec<&AssertResult> = self
            .results
            .iter()
            .filter(|r| !r.passed && r.waived.is_none())
            .collect();
        for r in bad.iter().take(Self::MAX_ERROR_ANNOTATIONS) {
            let title = if r.invalid { "INVALID" } else { "FAIL" };
            out.push_str(&format!(
                "::error title=hauksbee-ci {title}::{} - {}\n",
                gh_escape(&r.label),
                gh_escape(&r.detail)
            ));
        }
        if bad.len() > Self::MAX_ERROR_ANNOTATIONS {
            out.push_str(&format!(
                "::error title=hauksbee-ci::...and {} more failing assertion(s); see the job log or the JUnit report for the full list\n",
                bad.len() - Self::MAX_ERROR_ANNOTATIONS
            ));
        }

        // Warnings: dead rails first (they change what every other line
        // means), then waived failures, unclean evidence assumptions (each
        // once), substitutions, coverage holes and waiver notes, all through
        // one capped channel.
        let mut surfaced = std::collections::HashSet::new();
        let assumptions = self
            .evidence
            .iter()
            .filter(|map| map.status() != hauksbee_ir::evidence::EvidenceStatus::Clean)
            .flat_map(|map| map.assumptions())
            .filter(|id| surfaced.insert((*id).clone()))
            .filter_map(|id| Some((id, self.assumptions.iter().find(|a| a.id() == id)?)))
            .map(|(id, a)| {
                format!(
                    "ASSUMPTION {}::{} ({})",
                    gh_escape(&id.to_string()),
                    gh_escape(a.statement()),
                    gh_escape(a.replacement())
                )
            });
        fn tagged<'m>(tag: &'static str, msgs: &'m [String]) -> impl Iterator<Item = String> + 'm {
            msgs.iter().map(move |m| format!("{tag}::{}", gh_escape(m)))
        }
        let warnings: Vec<String> = (!self.dead_rails.is_empty())
            .then(|| {
                format!(
                    "UNPOWERED RAIL::{} sat at 0 V (no voltage could be read from the name and no [[supply]] fed it); any analog result above may be an artifact",
                    gh_escape(&self.dead_rails.join(", "))
                )
            })
            .into_iter()
            .chain(self.results.iter().filter(|r| r.waived.is_some()).map(|r| {
                format!(
                    "WAIVED FAIL::{} - {} (waived: {})",
                    gh_escape(&r.label),
                    gh_escape(&r.detail),
                    gh_escape(r.waived.as_deref().unwrap_or(""))
                )
            }))
            .chain(assumptions)
            .chain(tagged("SUBSTITUTE MCU", &self.substitutions))
            .chain(tagged("COSIM COVERAGE HOLE", &self.coverage_warnings))
            .chain(tagged("WAIVERS", &self.waiver_notes))
            .collect();
        for w in warnings.iter().take(Self::MAX_WARNING_ANNOTATIONS) {
            out.push_str(&format!("::warning title=hauksbee-ci {w}\n"));
        }
        if warnings.len() > Self::MAX_WARNING_ANNOTATIONS {
            out.push_str(&format!(
                "::warning title=hauksbee-ci::...and {} more warning(s); see the job log for the full list\n",
                warnings.len() - Self::MAX_WARNING_ANNOTATIONS
            ));
        }

        if !self.timing_coverage.is_empty() {
            let coverage = self
                .timing_coverage
                .iter()
                .map(timing_coverage_note)
                .collect::<Vec<_>>()
                .join("; ");
            out.push_str(&format!(
                "::notice title=hauksbee-ci TIMING COVERAGE::{}\n",
                gh_escape(&coverage)
            ));
        }

        // The rollup: exactly one summary annotation, always emitted.
        if self.analog_invalid() {
            let refusal = self.refusal().expect("analog-invalid has refusal");
            out.push_str(&format!(
                "::error title=hauksbee-ci refusal::{}\n",
                gh_escape(&refusal.render_text().replace('\n', "; "))
            ));
        } else if self.passed() {
            out.push_str(&format!(
                "::notice title=hauksbee-ci::{}/{} assertions passed\n",
                self.pass_count(),
                self.results.len()
            ));
        } else {
            out.push_str(&format!(
                "::error title=hauksbee-ci::{}/{} assertions passed - hardware check RED\n",
                self.pass_count(),
                self.results.len()
            ));
        }
        out
    }

    /// Per-assertion `::error` cap: 8 verdicts + 1 overflow + 1 rollup fits
    /// exactly inside GitHub's 10-errors-per-step truncation.
    pub const MAX_ERROR_ANNOTATIONS: usize = 8;
    /// `::warning` cap: 9 + 1 overflow fits the 10-warnings-per-step budget.
    pub const MAX_WARNING_ANNOTATIONS: usize = 9;
}

/// One rendered `<testsuite>` fragment plus the counters the `<testsuites>`
/// envelope aggregates. A multi-spec `hauksbee-ci run a.toml b.toml --junit`
/// merges one of these per spec (a spec that failed to LOAD contributes the
/// [`junit_error_suite`] shape) into a single document.
#[derive(Debug, Clone)]
pub struct JunitSuite {
    /// The `  <testsuite ...>...</testsuite>\n` fragment, indented for the
    /// envelope.
    pub xml: String,
    pub tests: usize,
    pub failures: usize,
    pub errors: usize,
    pub time_s: f64,
}

/// Wrap one or more suites in the `<testsuites>` envelope with honest
/// aggregate counts. Every JUnit document this crate emits goes through here,
/// so the single-spec and merged multi-spec shapes cannot drift.
pub fn render_junit_document(suites: &[JunitSuite]) -> String {
    let tests: usize = suites.iter().map(|s| s.tests).sum();
    let failures: usize = suites.iter().map(|s| s.failures).sum();
    let errors: usize = suites.iter().map(|s| s.errors).sum();
    let time: f64 = suites.iter().map(|s| s.time_s).sum();
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuites name=\"hauksbee-ci\" tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\" time=\"{time:.3}\">\n"
    ));
    for s in suites {
        out.push_str(&s.xml);
    }
    out.push_str("</testsuites>\n");
    out
}

/// The suite for a spec/board error (exit 2): a single errored testcase
/// carrying the message, so a CI that only reads the JUnit/Checks tab still
/// sees *something*. Reuses the `<error>` shape the per-assertion INVALID path
/// emits, so downstream ingestors (GitLab, Jenkins, GitHub) render it as an
/// errored test, distinct from a failure. `name` labels the suite (the spec
/// path in a multi-spec run, "spec error" for one).
pub fn junit_error_suite(name: &str, message: &str) -> JunitSuite {
    let message = xml_escape(message);
    JunitSuite {
        xml: format!(
            "  <testsuite name=\"{}\" tests=\"1\" failures=\"0\" errors=\"1\" time=\"0.000\">\n    \
             <testcase classname=\"spec\" name=\"spec/board loads\">\n      \
             <error message=\"{message}\">{message}</error>\n    \
             </testcase>\n  </testsuite>\n",
            xml_escape(name)
        ),
        tests: 1,
        failures: 0,
        errors: 1,
        time_s: 0.0,
    }
}

/// A synthetic JUnit document for a single spec/board error (exit 2); the
/// one-spec convenience over [`junit_error_suite`] + [`render_junit_document`].
pub fn render_junit_error(message: &str) -> String {
    render_junit_document(std::slice::from_ref(&junit_error_suite(
        "spec error",
        message,
    )))
}

/// The one `why:` line a failed assertion gets, from the best source available:
///
/// 1. the shortfall the check MEASURED (`why`), always preferred,
/// 2. guidance specific to a degenerate outcome the detail describes
///    (`degenerate_hint`),
/// 3. the generic per-kind pointer at the likely physical cause
///    (`failure_hint`).
///
/// Every surface that prints a `why:` goes through here, so the terminal and the
/// JUnit body cannot end up saying different things about the same failure.
pub fn why_line(r: &AssertResult) -> Option<String> {
    r.why
        .clone()
        .or_else(|| degenerate_hint(&r.detail))
        .or_else(|| failure_hint(&r.kind))
}

/// Guidance for a failure whose measured line says the check never got data.
///
/// The per-kind hint below assumes the check RAN and disagreed with the board,
/// and on a degenerate outcome it contradicts the line above it: "net 'VCC' was
/// never sampled (no window at 500ms)" followed by "the rail left its window;
/// check the supply feeding this net" sends someone to the bench over a spec
/// whose sample window starts after the run ends. These variants get their own
/// line naming the knob that is wrong instead.
fn degenerate_hint(detail: &str) -> Option<String> {
    // boot_coverage: the whole boot window sat past the end of the run.
    if let (Some(deadline), Some(sim)) = (
        number_after(detail, "boot deadline "),
        number_after(detail, "past the end of the "),
    ) {
        return Some(format!(
            "the boot deadline ({deadline} ms) is past the end of the run ({sim} ms), so \
             the window was never observed; raise duration_ms above {deadline} or lower \
             deadline_ms below {sim}."
        ));
    }
    // voltage: no sample window exists at the assertion's threshold at all.
    if let Some(ms) = number_after(detail, "no window at ") {
        if !is_zero(&ms) {
            return Some(format!(
                "the sample window starts at {ms} ms, at or after the end of the run, so \
                 nothing was measured; raise duration_ms above {ms} or lower after_ms \
                 below it."
            ));
        }
    }
    // voltage: the window exists but no frame landed inside it.
    if let Some(ms) = number_after(detail, "had no samples after ") {
        return Some(format!(
            "no frame landed after {ms} ms, so the window is empty; raise duration_ms \
             above {ms}, lower after_ms, or shorten frame_ms so a frame falls inside it."
        ));
    }
    if detail.contains("was never sampled in scenario window") {
        return Some(
            "the scenario window produced no samples for this net; check the net name and \
             that the [[scenario]]'s `start_ms` falls inside duration_ms."
                .to_string(),
        );
    }
    if detail.contains("was never sampled") || detail.contains("had no samples") {
        return Some(
            "nothing was measured, so there is no board finding here yet; this is the spec \
             or the run's coverage, not the hardware."
                .to_string(),
        );
    }
    None
}

/// The number immediately following `prefix` in `text`, verbatim, so a hint can
/// quote the value the spec actually carries. `None` when the prefix is absent
/// or is not followed by a number.
fn number_after(text: &str, prefix: &str) -> Option<String> {
    let rest = text.split_once(prefix)?.1;
    let n: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if n.chars().any(|c| c.is_ascii_digit()) {
        Some(n)
    } else {
        None
    }
}

/// A threshold of zero means the window was never past the end of anything, so
/// "raise duration_ms above 0" would be nonsense advice; those fall through to
/// the generic no-data line.
fn is_zero(n: &str) -> bool {
    n.parse::<f64>().map(|v| v == 0.0).unwrap_or(false)
}

/// One actionable "why / where to look" line for a failed assertion of the
/// given `kind`. Points at the likely physical cause and the assertion-catalog
/// section to read, one line, per kind, not a full explanation engine. `None`
/// for kinds that have no useful generic pointer.
///
/// Only reached once [`degenerate_hint`] has ruled out the outcomes where the
/// check never measured anything: these lines all presume a real measurement.
fn failure_hint(kind: &str) -> Option<String> {
    let url = hauksbee_ir::docs_url("docs/ci/CI.md");
    Some(match kind {
        "voltage" => format!(
            "the rail left its window; check the supply feeding this net \
            and the load pulling it down ({url}, \"voltage\")."
        ),
        "rail_window" => format!(
            "the rail dipped/spiked or recovered/settled outside the window; check the \
            scenario's load step and the decoupling on this net ({url}, \
            \"rail_window\")."
        ),
        "boot_coverage" | "boot-coverage" => format!(
            "the firmware never drove the control net in time; check \
            `firmware = ...` points at the right image and the net is a GPIO the \
            firmware actually drives ({url}, \"boot_coverage\" caveat)."
        ),
        "no_faults" => format!(
            "the stress monitor tripped; the named component exceeded a \
            rating (over-current / -voltage / -power / -temp / reverse-bias); check \
            its part value and supply ({url}, \"no_faults\")."
        ),
        "max_current" => format!(
            "the part drew more than its limit; check its load and the \
            override value ({url}, \"max_current\")."
        ),
        "max_temp" => format!(
            "the junction ran hotter than the limit; check dissipation and \
            the `ambient_c` assumption ({url}, \"max_temp\")."
        ),
        "uart" => format!(
            "the expected UART text never appeared; check the firmware image \
            and baud, and that the MCU booted ({url}, \"uart\")."
        ),
        "toggle" => format!(
            "the net toggled the wrong number of times; check the firmware's \
            drive rate and the deadline window ({url}, \"toggle\")."
        ),
        "protection_trip" => format!(
            "the supply's protection did/did not latch as asserted; \
            check the supply's limits and the load that triggers it ({url}, \
            \"protection_trip\")."
        ),
        "phase_margin" => format!(
            "the loop's phase margin missed the bound; check the \
            compensation network and the `[ac]` sweep range ({url}, \
            \"phase_margin\")."
        ),
        "ac_gain" => format!(
            "the small-signal gain missed the band; check the AC stimulus \
            net and the sweep points ({url}, \"ac_gain\")."
        ),
        _ => return None,
    })
}

/// One MCU's timing resolution, in the wording every report format uses.
fn timing_coverage_note(t: &hauksbee_engine::scheduler::TimingCoverage) -> String {
    format!(
        "{} ({}): edge timestamps +/-{:.3} us; pulses >= {:.3} us guaranteed; \
         {:.3} us chunk; {} stamps",
        t.mcu_ref,
        t.backend,
        t.timestamp_precision_s * 1e6,
        t.minimum_guaranteed_pulse_s * 1e6,
        t.chunk_s * 1e6,
        if t.cycle_exact {
            "cycle-exact"
        } else {
            "poll-boundary"
        },
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Escape the characters that are special in GitHub workflow-command data.
/// Percent first, then the control characters, else the `%0A`/`%0D` inserted
/// for them would get their own `%` re-encoded to `%25`.
pub fn gh_escape(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertions::AssertResult;
    use std::time::Duration;

    fn result(label: &str, kind: &str, passed: bool, detail: &str) -> AssertResult {
        AssertResult {
            passed,
            detail: detail.to_string(),
            ..AssertResult::shell(label.to_string(), kind, 1)
        }
    }

    fn failing(kind: &str, detail: &str) -> AssertResult {
        result(&format!("{kind} check"), kind, false, detail)
    }

    fn ci_result(results: Vec<AssertResult>) -> CiResult {
        CiResult {
            spec_name: "t".into(),
            board: "b.kicad_pcb".into(),
            results,
            seeds: 1,
            elapsed: Duration::ZERO,
            analog_abort: false,
            coverage: None,
            substitutions: Vec::new(),
            coverage_warnings: Vec::new(),
            timing_coverage: Vec::new(),
            timing_refusals: Vec::new(),
            dead_rails: Vec::new(),
            waiver_notes: Vec::new(),
            inventory: Vec::new(),
            assumptions: Vec::new(),
            evidence: Vec::new(),
        }
    }

    /// The coverage banner is also the JSON `coverage` field, so it must not
    /// out-claim the per-assertion detail: probes are evidence for the
    /// monotonicity the corner bound needs, never proof of it.
    #[test]
    fn corner_coverage_describes_probes_as_evidence_not_proof() {
        let with = EnsembleCoverage::Corners {
            corners: 4,
            interior: 6,
            components: 2,
        }
        .describe();
        assert!(with.contains("4 deterministic min/max corner(s)"), "{with}");
        assert!(with.contains("6 interior probe(s)"), "{with}");
        assert!(with.contains("not proof"), "{with}");
        assert!(
            !with.contains("10 deterministic"),
            "corners stay 2^n: {with}"
        );

        let without = EnsembleCoverage::Corners {
            corners: 4,
            interior: 0,
            components: 2,
        }
        .describe();
        assert!(
            !without.contains("interior"),
            "no probes ran, so claim none: {without}"
        );
    }

    #[test]
    fn single_member_and_monte_carlo_banners_name_exactly_what_ran() {
        // `seeds` is the sampled count; the nominal baseline is excluded.
        let mc = EnsembleCoverage::MonteCarlo {
            seeds: 0,
            components: 3,
        }
        .describe();
        assert!(mc.contains("nominal baseline + 0 sampled seed(s)"), "{mc}");
        // `--seed N` runs one member: neither a nominal run nor corner coverage is claimed.
        let member = |seed, corners, interior| {
            EnsembleCoverage::SingleMember {
                seed,
                components: 2,
                corners,
                interior,
            }
            .describe()
        };
        let draw = member(7, false, false);
        assert!(
            draw.contains("seed 7")
                && !draw.contains("nominal baseline")
                && !draw.contains("corner"),
            "{draw}"
        );
        // In corners mode a member is a deterministic corner or an interior
        // probe, never a seed or a draw.
        let corner = member(2, true, false);
        assert!(
            corner.contains("corner 2") && !corner.contains("seed") && !corner.contains("draw"),
            "{corner}"
        );
        let probe = member(4, true, true);
        assert!(
            probe.contains("interior probe 4") && !probe.contains("corner 4"),
            "{probe}"
        );
    }

    // The canned per-kind hint must not contradict the measured line when
    // the check never got data: a window that starts after the run ends is
    // a spec problem, not a supply problem.
    #[test]
    fn why_line_prefers_measured_reasons_over_canned_hints() {
        let r = failing(
            "voltage",
            "net 'VCC' was never sampled (no window at 500ms)",
        );
        let why = why_line(&r).expect("a degenerate failure still gets a why");
        assert!(
            why.contains("duration_ms") && why.contains("after_ms") && why.contains("500"),
            "{why}"
        );
        assert!(!why.contains("check the supply"), "{why}");

        // A real measurement keeps the per-kind hint.
        let r = failing("voltage", "+5V: min=3.100V < required 4.75V <- FAILED HERE");
        assert!(why_line(&r)
            .unwrap()
            .contains("check the supply feeding this net"));

        // A measured `why` outranks every hint, and both the terminal and the
        // JUnit failure body carry it (the message attribute stays the
        // one-line measured detail).
        let mut r = failing("voltage", "+5V: min=3.100V < required 4.75V");
        r.why = Some("+5V settled 1.650 V below your floor".to_string());
        assert_eq!(
            why_line(&r).as_deref(),
            Some("+5V settled 1.650 V below your floor")
        );
        let result = ci_result(vec![r]);
        let junit = result.render_junit();
        assert!(
            junit.contains("<failure") && junit.contains("why: +5V settled 1.650 V"),
            "{junit}"
        );
        assert!(junit.contains("message=\"+5V: min=3.100V"), "{junit}");
        assert!(result.render_human().contains("why: +5V settled 1.650 V"));
    }

    /// Honesty notes (an MCU substitution, a co-sim coverage hole, measured
    /// timing coverage and refusals) reach every report format, and are
    /// absent from every format when there is nothing to note.
    #[test]
    fn honesty_notes_reach_every_report_format() {
        let mut result = ci_result(Vec::new());
        result.substitutions = vec![
            "co-sim: U1 requested STM32F411RET6 but it is modelled as an STM32F407 core"
                .to_string(),
        ];
        result.coverage_warnings = vec![
            "co-sim: ADC channel 0 on U1 (net 'TEMP_SENSE') was driven by the analog solve but \
             this platform has no ADC injection map"
                .to_string(),
        ];
        result.timing_coverage = vec![hauksbee_engine::scheduler::TimingCoverage {
            mcu_ref: "U1".into(),
            backend: "renode:stm32f103".into(),
            cycle_exact: false,
            timestamp_precision_s: 4e-6,
            minimum_guaranteed_pulse_s: 8e-6,
            chunk_s: 4e-6,
        }];
        result.timing_refusals = vec!["poll backend could not represent 0.5 us".into()];

        let human = result.render_human();
        let junit = result.render_junit();
        let gh = result.render_github_annotations();
        for needle in [
            "SUBSTITUTE",
            "COVERAGE HOLE",
            "TIMING COVERAGE",
            "TIMING INVALID",
        ] {
            assert!(
                human.contains(needle),
                "human report lacks {needle}: {human}"
            );
            assert!(junit.contains(needle), "junit lacks {needle}: {junit}");
        }
        assert!(
            human.contains("STM32F411RET6") && human.contains("TEMP_SENSE"),
            "{human}"
        );
        assert!(junit.contains("system-out"), "{junit}");
        assert!(
            gh.contains("SUBSTITUTE MCU")
                && gh.contains("COSIM COVERAGE HOLE")
                && gh.contains("::warning"),
            "{gh}"
        );
        assert!(gh.contains("TIMING COVERAGE"), "{gh}");
        let json: serde_json::Value = serde_json::from_str(&result.render_json()).unwrap();
        assert_eq!(
            json["timing_coverage"][0]["minimum_guaranteed_pulse_s"],
            8e-6
        );
        assert_eq!(json["timing_refusals"].as_array().unwrap().len(), 1);

        let clean = ci_result(Vec::new()).render_human();
        assert!(
            !clean.contains("SUBSTITUTE") && !clean.contains("COVERAGE HOLE"),
            "{clean}"
        );
    }

    #[test]
    fn duplicate_model_occurrence_identity_reaches_human_and_junit_surfaces() {
        use hauksbee_ir::evidence::{
            CausalPathIndex, EvidenceMap, EvidenceRegistry, MatchConfidence, ModelLayer,
            ModelOnPath, ModelSource, ModelSourceTier, ModelUncertainty, ModelValidation, NetScope,
            RunDate,
        };
        let label = "Via model is on the SENSE path";
        let registry = EvidenceRegistry::new(Vec::new()).expect("empty registry is valid");
        let paths = CausalPathIndex::from_net_parts([("SENSE", ["Via"].as_slice())])
            .expect("fixture incidence is valid");
        let scope = NetScope::new(["SENSE"], None).expect("fixture scope is valid");
        let traversal = paths
            .traverse(&scope, &registry)
            .expect("fixture traversal is valid");
        let source = ModelSource::new(
            ModelSourceTier::CuratedPack,
            ModelLayer::Pack,
            "fixture pack",
            ModelValidation::PhysicalBoundsOnly,
            vec![
                ModelUncertainty::unknown("all parameters", "fixture has no bounds")
                    .expect("fixture uncertainty is valid"),
            ],
        )
        .expect("fixture model source is valid");
        let map = EvidenceMap::from_traversal(label, traversal, &registry, RunDate::unknown())
            .expect("fixture map is valid")
            .with_models(vec![ModelOnPath::for_subject(
                "component:v1:Via:2",
                "Via",
                "via_model_b",
                source,
                MatchConfidence::Exact,
            )
            .expect("fixture model is valid")]);

        let mut r = result(label, "voltage", true, "held");
        r.subject_nets = vec!["SENSE".into()];
        r.subject_refs = vec!["Via".into()];
        let mut result = ci_result(vec![r]);
        result.evidence = vec![map];

        let human = result.render_human();
        assert!(
            human.contains("model Via [subject=\"component:v1:Via:2\"]=via_model_b"),
            "human report lost identity: {human}"
        );
        let junit = result.render_junit();
        assert!(
            junit.contains("model Via [subject=&quot;component:v1:Via:2&quot;]=via_model_b"),
            "JUnit report lost identity: {junit}"
        );
    }
}
