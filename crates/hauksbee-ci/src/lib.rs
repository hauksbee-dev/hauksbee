//! `hauksbee-ci`: CI for hardware.
//!
//! Software got transformed by running tests on every commit. Hardware has had
//! nothing like it. `hauksbee-ci` runs the hauksbee PCB emulator headless in a
//! pipeline: on every layout change it boots the firmware on the real board,
//! and asserts the things you would otherwise only learn on the bench; the
//! rail comes up at 4.96 V, the UART says hello, the LED blinks at 5 Hz, no
//! part exceeds its rating.
//!
//! Input is a hand-written [`spec::Spec`] (TOML) checked into a hardware repo.
//! Output is a human report, a process exit code (0 green / 1 red), `JUnit` XML
//! for any CI system, and GitHub Actions annotations.
//!
//!
//! ```no_run
//! use hauksbee_ci::{run, RunConfig};
//! let cfg = RunConfig { spec: "ci/power-up.toml".into(), ..Default::default() };
//! let result = run(&cfg).unwrap();
//! std::process::exit(result.exit_code());
//! ```

use std::path::PathBuf;
use std::time::Instant;

use hauksbee_ir::eprintln;
use hauksbee_ir::evidence::{Assumption, EvidenceMap, MatchConfidence, ModelLayer, ModelOnPath};

pub mod add;
pub mod assertions;
pub mod catalog;
pub mod check;
pub mod error;
pub mod examples;
pub mod hwtrace;
pub mod init;
pub mod integrate;
pub mod lint;
pub mod measure;
pub mod progress;
pub(crate) mod rail_feed;
pub mod report;
pub mod runner;
pub mod scenarios;
pub mod search;
pub mod spec;
pub mod static_gate;
pub mod tolerance;
pub mod waveforms;

pub use error::SpecError;
pub use init::init;
pub use report::CiResult;
pub use spec::Spec;

/// The `--version` string: crate version plus the git hash of this build.
///
/// The engine's `build.rs` sets `GIT_HASH` (absent outside a git checkout, e.g.
/// a source tarball, in which case the bare crate version is all we honestly
/// have). Two consumers must agree on it byte for byte: clap's `version =` in
/// main.rs, and the `# installed by ...` line [`integrate::hook_install`] writes
/// into the pre-commit hook so the hook can warn when the binary on PATH is a
/// different build.
pub use hauksbee_engine::version_string;

/// How to run a spec.
#[derive(Debug, Clone, Default)]
pub struct RunConfig {
    /// Path to the TOML spec file.
    pub spec: PathBuf,
    /// Run only this one ensemble seed (fuzz + tolerance), for re-running a
    /// reported failing seed in isolation. Sampling is keyed by the absolute
    /// seed number, so the isolated run reproduces the full run's values
    /// exactly. `None` = run the whole ensemble.
    pub seed: Option<u32>,
    /// Extra model directory, layered above the builtin db, installed packs,
    /// and the user model dirs; the same `--models-dir` layer `hauksbee run`
    /// has, so a custom `[[models]]` routing entry binds in CI too. Replaces
    /// the spec's `models_dir` key when set.
    pub models_dir: Option<PathBuf>,
    /// Vendor SPICE model files whose `.model` / `.subckt` cards claim parts by
    /// card name, above every model directory; the same `--spice-models` flag
    /// `hauksbee run` has, so a board binds the same way in CI.
    pub spice_models: Vec<PathBuf>,
    /// Write each seed's asserted-net series under this directory
    /// (see [`waveforms`]). `None` records nothing extra.
    pub waveforms: Option<PathBuf>,
}

/// Apply the board's waiver file to the evaluated assertion results.
///
/// A real failure (never a pass, never an INVALID refusal) that an active
/// `check = "ci"` waiver matches is marked visible-but-not-gating. INVALID
/// stays unwaivable on purpose: a waiver overrules a *finding*, and an INVALID
/// is the absence of one; letting a waiver green an untrustworthy run would
/// fail open. Matching mirrors the engine's `--check` semantics exactly (same
/// [`hauksbee_engine::waiver::WaiverSet`], same expiry, same nets/refs rules),
/// so a board+waiver behaves the same under `hauksbee-ci run` as under the
/// static gates.
pub fn apply_waivers(
    results: &mut [assertions::AssertResult],
    waivers: &mut hauksbee_engine::waiver::WaiverSet,
) -> Vec<hauksbee_engine::waiver::WaivedFinding> {
    let mut applied = Vec::new();
    for r in results.iter_mut() {
        if r.passed || r.invalid {
            continue;
        }
        if let Some(w) =
            waivers.take_waiver("ci", &r.kind, &r.subject_nets, &r.subject_refs, &r.label)
        {
            r.waived = Some(format!("{} (until {})", w.reason, w.until));
            applied.push(w);
        }
    }
    applied
}

/// Every check surface a waiver's `check` field can name. A waiver naming
/// anything else can never match a finding, because nothing produces findings
/// under that name.
const WAIVER_SURFACES: &[&str] = &["ci", "drc", "lint", "si"];

/// The housekeeping notes for this run's waiver set, filtered to the `ci`
/// check.
///
/// That is a lapsed `ci` waiver whose finding gates again, and an active `ci`
/// waiver that matched nothing. Waivers for the static checks (`drc` / `lint`
/// / `si`) live in the same file but belong to `hauksbee run`'s surfaces;
/// reporting them stale HERE would tell the user to delete waivers this run
/// simply never consults.
///
/// A waiver whose `check` names NO surface is the exception: it belongs to
/// nobody, so if this filter skipped it too, a typo like `check = "cl"` would be
/// the one waiver mistake that is completely silent, while every other one
/// (missing `reason`, unparseable `until`, no `nets`/`refs`) refuses to load at
/// all. It gets folded into the matched-nothing accounting here, with the
/// surface it probably meant.
#[must_use]
pub fn waiver_notes(waivers: &hauksbee_engine::waiver::WaiverSet) -> Vec<String> {
    let mut notes = Vec::new();
    for w in waivers.expired() {
        if w.check.eq_ignore_ascii_case("ci") {
            notes.push(format!(
                "the waiver on '{}' (reason: {}) lapsed {}; its finding gates again",
                w.kind, w.reason, w.until
            ));
        }
    }
    for w in waivers.stale() {
        let known = WAIVER_SURFACES
            .iter()
            .any(|s| w.check.eq_ignore_ascii_case(s));
        if !known {
            notes.push(format!(
                "the waiver on '{}' (reason: {}) sets check = '{}', which is not a check \
                 hauksbee produces findings under{}, so it can never match anything. \
                 Valid: {}",
                w.kind,
                w.reason,
                w.check,
                error::did_you_mean_hint(&w.check, WAIVER_SURFACES),
                WAIVER_SURFACES.join(", ")
            ));
        } else if w.check.eq_ignore_ascii_case("ci") {
            notes.push(format!(
                "the waiver on '{}' (reason: {}) matched nothing this run; either the \
                 finding is fixed and the waiver can go, or it no longer describes what \
                 fires. Note a waiver's `nets` must ALL appear in one finding's net set \
                 (AND, not OR); to cover several findings, write one [[waive]] block per \
                 finding",
                w.kind, w.reason
            ));
        }
    }
    notes
}

/// A `(start_s, end_s, method)` span a member solved on a fallback rung.
type FallbackWindow = (f64, f64, String);

/// Every member's failed and fallback windows on the ensemble's one timeline.
/// Two seeds failing over overlapping spans is one failed span, not a
/// contradiction, so the failed windows become their union; a fallback span
/// keeps only what no failure covers, and where members fell back differently
/// it carries the latest rung of the scheduler's ladder any of them needed.
/// An unknown method outranks them all, so the budget still refuses it.
/// The condition a host-paced timing verdict rests on, as the existing
/// reduced-fidelity assumption (the "conditional pass" channel), scoped to the
/// one assertion it qualifies.
fn host_paced_assumption(label: &str, paced: &assertions::HostPacedBound) -> Assumption {
    let key = format!("host-paced/{label}");
    let text = format!("the time measured against {}", paced.bound);
    let mechanism = format!(
        "the host-paced clock of MCU {}, which credits simulated time from host \
         wall-clock spans (the same boot has measured about 2x apart on two machines), \
         so the bound was left unjudged",
        paced.mcus.join(", ")
    );
    Assumption::reduced_fidelity(
        hauksbee_ir::evidence::AssumptionSource::Scheduler,
        hauksbee_ir::evidence::Subject::new(&key, &text),
        hauksbee_ir::evidence::Scope::Check {
            check: "ci".into(),
            kind: Some(label.to_string()),
        },
        &mechanism,
        "judge this bound on a cycle-exact backend (simavr) or on hardware; the \
         host-independent part of the assertion is still judged here",
    )
}

fn ensemble_windows(outcomes: &[runner::RunOutcome]) -> (Vec<(f64, f64)>, Vec<FallbackWindow>) {
    use hauksbee_engine::scheduler::ChunkFallbackMethod as M;
    let ladder = [
        M::ReducedStep,
        M::BackwardEuler,
        M::ColdStartBackwardEuler,
        M::SubdividedBackwardEuler,
    ]
    .map(|m| m.as_str());
    let rank = |m: &str| ladder.iter().position(|l| *l == m).unwrap_or(usize::MAX);
    let mut failed: Vec<(f64, f64)> = outcomes
        .iter()
        .flat_map(|o| o.failed_windows.clone())
        .collect();
    failed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut union: Vec<(f64, f64)> = Vec::new();
    for (start, end) in failed {
        match union.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => union.push((start, end)),
        }
    }
    let fallback: Vec<&FallbackWindow> =
        outcomes.iter().flat_map(|o| &o.fallback_windows).collect();
    let mut cuts: Vec<f64> = fallback.iter().flat_map(|w| [w.0, w.1]).collect();
    cuts.extend(union.iter().flat_map(|w| [w.0, w.1]));
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();
    let mut merged: Vec<FallbackWindow> = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let mid = 0.5 * (a + b);
        let inside = |w: &(f64, f64)| w.0 <= mid && mid < w.1;
        let method = fallback
            .iter()
            .filter(|w| inside(&(w.0, w.1)))
            .map(|w| &w.2);
        let Some(method) = method.max_by_key(|m| rank(m)) else {
            continue;
        };
        if union.iter().any(inside) {
            continue;
        }
        match merged.last_mut() {
            // Adjacent spans share the very cut value, so exact equality is meant.
            Some(last) if last.1.total_cmp(&a).is_eq() && last.2 == *method => last.1 = b,
            _ => merged.push((a, b, method.clone())),
        }
    }
    (union, merged)
}

fn refuse_timing_claims(results: &mut [assertions::AssertResult], refusals: &[String]) {
    if refusals.is_empty() {
        return;
    }
    let refusal = refusals.join("; ");
    for result in results {
        result.passed = false;
        result.invalid = true;
        result.detail = format!("INVALID: timing coverage refusal: {refusal}");
        // A capability refusal is not a board finding and cannot be waived into
        // a verdict: the requested measurement simply did not happen.
        result.waived = None;
    }
}

/// The conditions the assertions' evidence implies, worded as every engine
/// surface words them: the assumptions cited by the evidence map of an
/// assertion not in `waived`, minus the member-sweep records built from
/// `stated` (already said, in its own words).
fn evidence_conditions(
    evidence: &hauksbee_engine::BoardEvidence,
    waived: &std::collections::BTreeSet<&str>,
    stated: &[String],
) -> Vec<String> {
    let norm = |s: &str| s.trim().trim_end_matches('.').to_ascii_lowercase();
    let stated: std::collections::BTreeSet<String> = stated.iter().map(|c| norm(c)).collect();
    let cited: std::collections::BTreeSet<&hauksbee_ir::evidence::AssumptionId> = evidence
        .maps()
        .iter()
        .filter(|m| !waived.contains(m.assertion()))
        .flat_map(|m| m.assumptions().iter())
        .collect();
    let on_path: Vec<Assumption> = evidence
        .assumptions()
        .iter()
        .filter(|a| cited.contains(a.id()) && !stated.contains(&norm(a.statement())))
        .cloned()
        .collect();
    hauksbee_engine::verdict_conditions::assumption_conditions(&on_path)
        .into_iter()
        .filter(|c| !stated.contains(&norm(c)))
        .collect()
}

/// The order-stable, deduped union of one per-member message list across the
/// ensemble. One discipline for every honesty channel: a substitution, a
/// coverage hole, a dead rail or a timing refusal in ONE member is one in the
/// ensemble (an MCU substituted once is substituted for every member; nothing
/// per-seed powers a rail).
fn union_across(
    outcomes: &[runner::RunOutcome],
    f: impl Fn(&runner::RunOutcome) -> &[String],
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    outcomes
        .iter()
        .flat_map(|o| f(o).iter().cloned())
        .filter(|m| seen.insert(m.clone()))
        .collect()
}

/// The `why:` for an assertion that FAILED on a board that runs parts outside
/// their datasheet limits, or `None` when the failure is not entitled to stand.
///
/// A datasheet limit the run MEASURED as violated undermines a PASS: the card
/// guarantees nothing there, so a green number is not evidence. It does not
/// undermine a FAIL the same way. The outcome was asked of the board, and a
/// board that leaves a part outside its card (a rail starved through its feed
/// resistor, a comparator input above its common-mode range, a logic input
/// parked between VIL and VIH) has not delivered it whatever the part would
/// have done there: the violation is the board fault, and refusing with "not
/// a failure of your board" hides it. So a failure stands as a failure when
/// measured violations are the ONLY thing undermining it; any other
/// undermining gap (an open part, a substitute model, firmware that never ran)
/// still makes it INVALID, as does a run with a rail nothing powered (the
/// caller's guard), where the violations are artifacts of the spec.
///
/// A supply-envelope violation leads the sentence: a rail outside what its
/// parts need is upstream of every input-range and logic-level violation that
/// rail causes, so it is the one to fix first.
fn board_limit_failure(
    result: &assertions::AssertResult,
    map: &EvidenceMap,
    all: &[Assumption],
    today: hauksbee_ir::evidence::RunDate,
) -> Option<String> {
    use hauksbee_ir::evidence::{AssumptionKind, EvidenceStatus};
    if result.passed || result.invalid || result.measured {
        return None;
    }
    let on_path = map
        .assumptions()
        .iter()
        .filter_map(|id| all.iter().find(|a| a.id() == id));
    let (violated, rest): (Vec<&Assumption>, Vec<&Assumption>) =
        on_path.partition(|a| matches!(a.kind(), AssumptionKind::OperatingLimitViolated));
    let rest: Vec<Assumption> = rest.into_iter().cloned().collect();
    if EvidenceMap::derive_status(&rest, today) == EvidenceStatus::Undermined {
        return None;
    }
    let root = violated
        .iter()
        .find(|a| a.id().as_str().contains(BOARD_LIMIT_ROOT))
        .or_else(|| violated.first())?;
    let others = violated.len() - 1;
    let more = if others == 0 {
        String::new()
    } else {
        format!(
            " ({others} more violated limit(s) on this outcome's path are listed under evidence.)"
        )
    };
    // Worded as a finding on the outcome's path, not as THE cause: the
    // measured shortfall (when there is one) leads the line, and a limit
    // broken elsewhere on the path may or may not be what produced it.
    Some(format!(
        "this outcome does not hold as designed; its path also runs a part outside its \
         datasheet limit. {} {}{more}",
        root.statement(),
        root.because(),
    ))
}

/// The limit tag the supply-envelope run check gives its violations: a rail
/// outside its parts' supply range, the root of the violations it causes.
const BOARD_LIMIT_ROOT: &str = "-supply-envelope-";

/// Load the spec, run the co-sim across its seeds, and evaluate its assertions.
///
/// # Errors
///
/// [`SpecError`] when the spec, a model directory, a SPICE model file or an
/// evidence input cannot be loaded, when the co-sim cannot be set up, or when
/// the run's evidence cannot be assembled.
pub fn run(cfg: &RunConfig) -> Result<CiResult, SpecError> {
    progress::observed(
        || progress::plan_for_spec(&cfg.spec),
        &[hauksbee_ir::progress::Stage::Models],
        || run_unobserved(cfg),
    )
}

// One pipeline read top to bottom; its counts are ensemble sizes, bounded by the u32 seed space.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn run_unobserved(cfg: &RunConfig) -> Result<CiResult, SpecError> {
    let started = Instant::now();
    let mut spec = Spec::load(&cfg.spec)?;
    spec.record_waveforms = cfg.waveforms.is_some();
    let mut lib = runner::spec_model_library(&spec, cfg.models_dir.as_deref())?;
    for path in &cfg.spice_models {
        lib.load_spice_models(path)
            .map_err(|e| SpecError::Invalid(format!("{e:#}")))?;
    }
    let outcomes = runner::run_spec_with_lib(&spec, cfg.seed, &lib)?;
    let mut results = assertions::evaluate(&spec, &outcomes);
    if let Some(dir) = &cfg.waveforms {
        waveforms::export(dir, &cfg.spec, &spec, &outcomes).map_err(|e| {
            SpecError::Invalid(format!("writing waveforms to {}: {e}", dir.display()))
        })?;
    }

    // Timing coverage is a validity contract, not an advisory note. One
    // unrepresentable request or one pulse the tick path demonstrably missed
    // makes every assertion INVALID, preventing a plausible false green.
    let mut timing_refusals = union_across(&outcomes, |o| &o.timing_refusals);
    let timing_coverage = outcomes
        .first()
        .map(|o| o.timing_coverage.clone())
        .unwrap_or_default();
    if outcomes
        .iter()
        .skip(1)
        .any(|o| o.timing_coverage != timing_coverage)
    {
        timing_refusals.push(
            "timing coverage changed between deterministic ensemble members; no single timing claim describes this run"
                .into(),
        );
    }
    refuse_timing_claims(&mut results, &timing_refusals);

    // Waivers: hauksbee-waivers.toml beside the BOARD (the same file and the
    // same discovery `hauksbee run --check` uses), so one staged-rollout
    // mechanism covers the whole pipeline. A malformed file is a warning and
    // every finding it would have covered gates: failing closed, identical to
    // the engine's behavior.
    let mut notes = Vec::new();
    let mut waivers = match hauksbee_engine::waiver::WaiverSet::discover(&spec.board_path()) {
        Ok(set) => set,
        Err(e) => {
            let msg = format!(
                "ignoring the waiver file ({e}); every finding it would have covered gates this run"
            );
            eprintln!("WARNING: {msg}");
            notes.push(msg);
            hauksbee_engine::waiver::WaiverSet::default()
        }
    };
    let applied_waivers = apply_waivers(&mut results, &mut waivers);
    notes.extend(waiver_notes(&waivers));
    // A strict analog abort on ANY seed forces the invalid-for-analysis exit even
    // if no assertion's window happened to overlap the failed span.
    let analog_abort = outcomes.iter().any(|o| o.analog_abort);
    // Coverage banner data for a tolerance ensemble: how many members ran and
    // how many components were sampled (from any outcome; the set is fixed).
    let coverage = if spec.has_tolerances() {
        let components = outcomes.first().map_or(0, |o| o.sampled_values.len());
        let members = outcomes.len() as u32;
        // A pinned `--seed N` filters the ensemble down to that one member, so the
        // "nominal baseline + members-1 sampled" (Monte-Carlo) and "N corners"
        // (Corners) arithmetic no longer holds: members == 1 would otherwise
        // claim "nominal + 0 sampled" (the nominal did NOT run) or "1 corner" out
        // of 2^n. Report the single pinned member honestly instead.
        if let Some(seed) = cfg.seed {
            // A corners member is a deterministic min/max corner, not a random
            // draw, carry the mode so the banner says "corner N", matching the
            // mode-aware per-assertion INVALID/FAIL wording.
            let corners = matches!(spec.ensemble_mode()?, tolerance::Mode::Corners);
            // Which member it turned out to be: corner mode numbers the interior
            // probes on past the last corner, so mode alone cannot say whether
            // `--seed N` pinned a corner or a probe. Read it off the outcome the
            // runner actually produced.
            let interior = outcomes.first().is_some_and(|o| o.interior);
            Some(report::EnsembleCoverage::SingleMember {
                seed,
                components,
                corners,
                interior,
            })
        } else {
            Some(match spec.ensemble_mode()? {
                // Member 0 is the nominal baseline (it draws no random sample), so
                // the number of genuinely SAMPLED seeds is members - 1. Reporting
                // the full member count claimed `seeds=1` sampled something when
                // it only ran the nominal.
                tolerance::Mode::MonteCarlo => report::EnsembleCoverage::MonteCarlo {
                    seeds: members.saturating_sub(1),
                    components,
                },
                // The interior probes ran alongside the corners but are not
                // corners: counting them in would inflate the "2^n corners"
                // arithmetic a reader checks against the component count.
                tolerance::Mode::Corners => {
                    let interior = outcomes.iter().filter(|o| o.interior).count() as u32;
                    report::EnsembleCoverage::Corners {
                        corners: members.saturating_sub(interior),
                        interior,
                        components,
                    }
                }
            })
        }
    } else {
        None
    };
    let mut evidence = outcomes
        .first()
        .and_then(|outcome| outcome.evidence.clone())
        .ok_or_else(|| {
            SpecError::Invalid(
                "the production runner returned no evidence object; refusing to grade assertions"
                    .into(),
            )
        })?;
    for assumption in outcomes
        .iter()
        .skip(1)
        .filter_map(|outcome| outcome.evidence.as_ref())
        .flat_map(|evidence| evidence.assumptions().iter().cloned())
    {
        evidence = evidence
            .with_assumptions([assumption])
            .map_err(SpecError::invalid("merging run evidence"))?;
    }
    // Open variant and corner sets the member sweep could not cross in full
    // qualify every assertion (board scope), and ride the verdict as its
    // conditions, so a pass that leaned on a stand-in is never a clean one.
    let verdict_conditions = union_across(&outcomes, |o| &o.verdict_conditions);
    for condition in &verdict_conditions {
        evidence = evidence
            .with_assumptions([hauksbee_ir::evidence::Assumption::partial_member_sweep(
                condition,
            )])
            .map_err(SpecError::invalid("recording the member-sweep coverage"))?;
    }
    // The spec, and the waiver file beside the board when there is one, are
    // inputs this run consumed: inventory their exact bytes.
    let waiver_path = spec
        .board_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(hauksbee_engine::waiver::DEFAULT_WAIVER_FILE);
    let toml_inputs = [
        (
            &cfg.spec,
            "spec",
            hauksbee_ir::evidence::ArtifactRole::Spec,
            "assertions",
            "run configuration, stimuli, supplies, and assertion thresholds",
        ),
        (
            &waiver_path,
            "waiver",
            hauksbee_ir::evidence::ArtifactRole::Waivers,
            "authorizations",
            "time-bounded human authorizations applied to matching CI findings",
        ),
    ];
    for (path, what, role, contributed, detail) in toml_inputs {
        if what == "waiver" && !waiver_path.is_file() {
            continue;
        }
        let bytes = std::fs::read(path).map_err(SpecError::io(format!(
            "reading {what} evidence '{}'",
            path.display()
        )))?;
        evidence = evidence
            .with_toml_artifact(
                path,
                &bytes,
                role,
                hauksbee_ir::evidence::Contribution {
                    what: contributed.into(),
                    detail: detail.into(),
                },
            )
            .map_err(SpecError::invalid(format!("building {what} evidence")))?;
    }
    let today = hauksbee_ir::evidence::RunDate::from_system_clock();
    for waiver in &applied_waivers {
        let assumption = hauksbee_ir::evidence::Assumption::waived_assertion(
            &waiver.check,
            &waiver.kind,
            &waiver.subject,
            &waiver.subject,
            &waiver.reason,
            &waiver.until,
            today,
        )
        .map_err(SpecError::invalid("building live-waiver evidence"))?;
        evidence = evidence
            .with_assumptions([assumption])
            .map_err(SpecError::invalid("building live-waiver evidence"))?;
    }
    // A time bound a host-paced MCU clock left unjudged is the condition the
    // assertion's verdict rests on: a typed reduced-fidelity assumption scoped
    // to that one assertion, so its evidence reads `qualified` on every
    // surface rather than a bare PASS a faster or slower host could flip.
    let mut seen_host_paced = std::collections::BTreeSet::new();
    for result in &results {
        let Some(paced) = &result.host_paced else {
            continue;
        };
        if !seen_host_paced.insert(result.label.clone()) {
            continue;
        }
        evidence = evidence
            .with_assumptions([host_paced_assumption(&result.label, paced)])
            .map_err(SpecError::invalid("building host-paced timing evidence"))?;
    }

    let (failed_windows, fallback_owned) = ensemble_windows(&outcomes);
    let fallback_windows: Vec<(f64, f64, &str)> = fallback_owned
        .iter()
        .map(|(start, end, method)| (*start, *end, method.as_str()))
        .collect();
    let sim_end_s = outcomes
        .iter()
        .map(|outcome| outcome.sim_ms / 1000.0)
        .fold(0.0_f64, f64::max);
    let mut transient_budget = hauksbee_engine::BoardEvidence::transient_error_budget(
        &hauksbee_solve::SolverOptions::default(),
        0.0,
        sim_end_s,
        spec.frame_ms / 1000.0,
        &failed_windows,
        &fallback_windows,
    )
    .map_err(SpecError::invalid("building CI error budget"))?;
    if let Some(residual) = outcomes
        .iter()
        .filter_map(|outcome| outcome.error_budget.as_ref()?.residual())
        .max_by(|left, right| left.max_abs().total_cmp(&right.max_abs()))
    {
        transient_budget = transient_budget.with_residual(residual.clone());
    }
    let coverage_description = coverage.as_ref().map(report::EnsembleCoverage::describe);
    let sensor_models = spec
        .sensors
        .iter()
        .map(|sa| spec_sensor_model(&sa.id, &sa.toml_source(&spec.base_dir)?))
        .collect::<Result<Vec<_>, SpecError>>()?;
    let mut maps = Vec::with_capacity(results.len());
    for result in &results {
        let budget = if matches!(result.kind.as_str(), "phase_margin" | "ac_gain") {
            Some(
                hauksbee_engine::BoardEvidence::solver_error_budget(
                    &hauksbee_solve::SolverOptions::default(),
                )
                .map_err(SpecError::invalid("building AC assertion budget"))?,
            )
        } else if assertion_has_numeric_result(&spec, result) {
            Some(transient_budget.clone())
        } else {
            None
        };
        // A part blamed for refusing this assertion is on its causal path even
        // when it sits a component away from the judged net. Naming it lets its
        // own open-part assumption undermine the map, so an INVALID verdict
        // stops reading as qualified.
        let mut subject_refs = result.subject_refs.clone();
        // A board-wide stress check that found faults is about the parts it
        // found them on: its causal path is theirs (their nets and the supply
        // chain behind them), so an open part elsewhere on the board cannot
        // turn a found overstress into "not judged". With no fault found it
        // keeps no subject and the whole board stays its path: a clean result
        // over open parts is never a pass. Waivers still match the spec's own
        // subject (`result.subject_refs`), never the faulting parts.
        if result.kind == "no_faults" && subject_refs.is_empty() && !result.passed {
            for fault in outcomes.iter().flat_map(|outcome| &outcome.faults) {
                if !subject_refs.contains(&fault.component) {
                    subject_refs.push(fault.component.clone());
                }
            }
        }
        if result.invalid {
            subject_refs.extend(
                outcomes
                    .iter()
                    .flat_map(|outcome| &outcome.regulation)
                    .filter(|taint| result.detail.contains(&taint.reason))
                    .flat_map(|taint| taint.refs.iter().cloned()),
            );
        }
        let map = evidence
            .ci_assertion_map(
                &result.label,
                &result.subject_nets,
                &subject_refs,
                budget,
                coverage_description.as_deref(),
            )
            .map_err(SpecError::invalid(format!(
                "building evidence for assertion '{}'",
                result.label
            )))?;
        let peripheral = result.subject_peripheral.as_deref();
        maps.push(with_sensor_model(map, &sensor_models, peripheral));
    }
    // Circuit-aware refinement for voltage assertions (see
    // hauksbee_engine::dcpath): a numeric band claim on a net NOTHING
    // modeled defines is a false green waiting to happen (GMIN pins a
    // floating node near zero, which reads as a healthy logic low), so it
    // becomes an ordinary RED traced to the missing element; while a net a
    // modeled passive path ties to a reference is defined even if every
    // open part is high-impedance, so the open-part assumptions downgrade
    // from verdict-blocking to a stated caveat. Both directions use the
    // run's own stated assumption (opens are absent from the circuit);
    // neither touches an assertion already invalidated for a different
    // reason (a held-stale analog span keeps its exit-3 semantics).
    let dc_definitions = outcomes
        .first()
        .map(|o| o.dc_definitions.clone())
        .unwrap_or_default();
    let mut floating_reds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (result, map) in results.iter_mut().zip(maps.iter_mut()) {
        use hauksbee_engine::dcpath::NetDcDefinition;
        if result.kind != "voltage" || result.invalid {
            continue;
        }
        let statuses: Vec<&runner::DcNetStatus> = result
            .subject_nets
            .iter()
            .filter_map(|net| dc_definitions.get(net))
            .collect();
        if statuses.is_empty() {
            continue;
        }
        if statuses
            .iter()
            .all(|s| matches!(s.definition, NetDcDefinition::Floating))
        {
            let refs = statuses
                .iter()
                .flat_map(|s| s.member_refs.iter().cloned())
                .collect::<Vec<_>>()
                .join(", ");
            result.passed = false;
            result.invalid = false;
            result.waived = None;
            result.why = Some(format!(
                "the net is floating: no modeled element defines its level (parts on it: {refs})"
            ));
            result.detail = format!(
                "RED: {} is floating. No modeled DC path defines this net; the solver's \
                 number here is a numerical convention, not a level the board holds. Every \
                 part on it ({refs}) is unmodelled or open, so if those parts are \
                 high-impedance (an input pin, an unplugged connector), nothing sets this \
                 line. Fix: add a pull resistor to the intended rest rail, or supply a \
                 model for a part that drives it. [{}]",
                result
                    .subject_nets
                    .first()
                    .map_or("the net", String::as_str),
                result.detail
            );
            floating_reds.insert(result.label.clone());
        } else if statuses.iter().all(|s| {
            matches!(
                s.definition,
                NetDcDefinition::DefinedThroughBoard { .. } | NetDcDefinition::Indeterminate
            )
        }) && statuses
            .iter()
            .any(|s| matches!(s.definition, NetDcDefinition::DefinedThroughBoard { .. }))
        {
            let via = statuses
                .iter()
                .find_map(|s| match &s.definition {
                    NetDcDefinition::DefinedThroughBoard { via } => Some(via.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            // ... UNLESS the run already proved an open part can DRIVE this net:
            // the downgrade's premise is that an unmodelled part could only
            // matter by driving it, and the co-sim taint refutes exactly that.
            if let Some(taint) = outcomes.iter().flat_map(|o| &o.regulation).find(|t| {
                t.reason.contains(hauksbee_engine::scheduler::OPEN_DRIVER)
                    && result.subject_nets.iter().any(|n| t.nets.contains(n))
            }) {
                result.detail = format!("{} {}", result.detail, taint.reason);
                continue;
            }
            *map = evidence
                .map_assuming_open_parts_high_impedance(map.clone())
                .map_err(SpecError::invalid(format!(
                    "recomputing evidence for assertion '{}'",
                    result.label
                )))?;
            result.detail = format!(
                "{} (level defined through {via} by modeled board elements; holds unless \
                 an unmodelled part on the net drives or loads it)",
                result.detail
            );
        }
    }
    // A board-wide stress claim is the claim `hauksbee check` makes about the
    // same board, so it judges open parts by the check's rule, defined once
    // (`unmodelled_critical_refs`): an open active IC or transistor still
    // refuses the verdict, while an open connector or passive becomes a stated
    // condition on it, exactly as the check states it. Without the bind
    // summary the rule cannot be applied, so the map keeps its status.
    let mut critical_open: Vec<String> = Vec::new();
    for bind in outcomes.iter().filter_map(|o| o.bind.as_ref()) {
        for r in hauksbee_engine::result::unmodelled_critical_refs(bind) {
            if !critical_open.contains(&r) {
                critical_open.push(r);
            }
        }
    }
    let bound_any = outcomes.iter().any(|o| o.bind.is_some());
    for (result, map) in results.iter().zip(maps.iter_mut()) {
        if !bound_any || result.kind != "no_faults" || !map.is_undermined() {
            continue;
        }
        *map = evidence
            .map_with_noncritical_open_parts_qualified(map.clone(), &critical_open)
            .map_err(SpecError::invalid(format!(
                "recomputing evidence for assertion '{}'",
                result.label
            )))?;
    }
    evidence = evidence.with_maps(maps);
    // A rail only a do-not-populate part would power is dead because of the
    // assembly the run was asked to judge: a failure there is a true answer
    // about that assembly, and the banner names the fit that changes it. Only
    // a rail whose voltage is simply unknown makes the operating point fiction.
    let any_dead_rail = outcomes.iter().any(|o| {
        let dnp = |net: &String| {
            let opening = format!("{net}: {}", hauksbee_engine::DNP_CAUSE_OPENING);
            o.dead_rail_causes.iter().any(|c| c.starts_with(&opening))
        };
        o.dead_rails.iter().any(|net| !dnp(net))
    });
    for result in &mut results {
        if floating_reds.contains(&result.label) {
            // A floating-net RED already carries its own diagnosis; the
            // undermined map stays attached for transparency, but the verdict
            // is an ordinary failure, not invalid-for-analysis.
            continue;
        }
        let map = evidence
            .maps()
            .iter()
            .find(|map| map.assertion() == result.label);
        if let Some(cause) = map
            .filter(|map| map.is_undermined() && !any_dead_rail)
            .and_then(|map| board_limit_failure(result, map, evidence.assumptions(), today))
        {
            // The measured shortfall stays first; the broken limit follows.
            result.why = Some(match result.why.take() {
                Some(measured) => format!("{measured}; {cause}"),
                None => cause,
            });
            continue;
        }
        if let Some(map) = map.filter(|map| map.is_undermined()) {
            result.invalid = true;
            result.waived = None;
            // An unsettled variant set names its members and the spec line that settles it.
            let cited = |id| evidence.assumptions().iter().find(|a| a.id() == id);
            let listed = |a: &&Assumption| a.because().contains(hauksbee_models::variants::LISTED);
            let variant = map.assumptions().iter().filter_map(cited).find(listed);
            let why = variant.map(|a| format!(" {} Unlocked by: {}", a.because(), a.replacement()));
            let no_firmware = map.assumptions().iter().filter_map(cited).any(|a| {
                // The assumption capitalises its reason when it renders it.
                a.because()
                    .to_ascii_lowercase()
                    .contains(&runner::NO_FIRMWARE_BECAUSE.to_ascii_lowercase())
            });
            if no_firmware {
                // The MCU never ran, so what was read off it (an empty UART, zero
                // edges) is the idle core, not a verdict on any firmware. State
                // the fix instead of sending the reader to debug firmware.
                if !result.detail.starts_with(runner::NO_FIRMWARE_DETAIL) {
                    result.detail = format!(
                        "{}{}; {}. (idle-MCU reading: {})",
                        runner::NO_FIRMWARE_DETAIL,
                        runner::NO_FIRMWARE_BECAUSE,
                        runner::NO_FIRMWARE_FIX,
                        result.detail
                    );
                }
            } else if !result.detail.starts_with("INVALID evidence:") {
                result.detail = format!(
                    "INVALID evidence: this assertion rests on a critical unresolved assumption.{} {}",
                    why.unwrap_or_default(),
                    result.detail
                );
            }
        }
    }
    // The board's own static verdict: a finding that fails `hauksbee check`
    // fails this run too, as its own named row, so a simulation-only spec can
    // never read GREEN over a board its static check fails. Judged with the
    // run's model library and the board's waiver file, like the check.
    // A finding the board's waiver file overrules keeps its row, marked waived.
    // With an `asbuilt` overlay the check judges the reworked board the
    // simulation ran: the same overlay, applied to the static board.
    let asbuilt_path = spec.asbuilt_path();
    // The same image the run loaded: the check reads what the program means to
    // use (`hauksbee check --firmware`), so a spec naming firmware gets those
    // findings too.
    let static_firmware = runner::static_check_firmware(&spec);
    let gate = (asbuilt_path.as_deref())
        .map(hauksbee_engine::asbuilt::AsBuiltOverlay::load)
        .transpose()
        .map_err(anyhow::Error::from)
        .and_then(|overlay| {
            hauksbee_engine::reports::check::static_gate_as_built(
                &spec.board_path(),
                &lib,
                overlay.as_ref(),
                static_firmware.as_deref(),
            )
        });
    let mut static_conditions: Vec<String> = Vec::new();
    match gate {
        Ok(gate) => {
            if let (Some(report), Some(path)) = (&gate.as_built, &asbuilt_path) {
                // Narrated beside the run (stderr), as `hauksbee --asbuilt` does.
                if !report.lines.is_empty() {
                    eprintln!(
                        "static check judged the as-built board ({}):",
                        path.display()
                    );
                    for line in &report.lines {
                        eprintln!("  {line}");
                    }
                }
                // An entry the static board could not carry leaves its parts
                // judged as designed: the run says so in its verdict.
                static_conditions.extend(report.unapplied.iter().map(|u| {
                    format!(
                        "the static check judged the as-built board without overlay entry {} \
                         ({}); findings on {} are the design's",
                        u.entry,
                        u.why,
                        u.refs
                            .iter()
                            .chain(&u.nets)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }));
            }
            results.extend(static_gate::gate_rows(&gate, outcomes.len()));
        }
        Err(e) => {
            let msg = format!(
                "the board's static check (hauksbee check) could not run ({e:#}), so this run \
                 does not cover what it judges"
            );
            eprintln!("WARNING: {msg}");
            notes.push(msg);
        }
    }
    // A pass that left a host-paced time bound unjudged is a conditional pass
    // of the whole run, not only of its own evidence row: the condition rides
    // the verdict every surface leads with ("GREEN, conditionally", the JSON
    // `verdict_conditions`). Added after the member-sweep assumptions above,
    // which are built from the sweep's own sentences only.
    hauksbee_ir::progress::begin_untracked(hauksbee_ir::progress::Stage::Report);
    let mut verdict_conditions = verdict_conditions;
    verdict_conditions.extend(static_conditions);
    verdict_conditions.extend(assertions::host_paced_conditions(&results));
    // A guessed AVR core clock puts every delay, baud rate and PWM period in
    // the run at risk of being off by the clock ratio: never a clean pass.
    verdict_conditions.extend(union_across(&outcomes, |o| &o.clock_assumptions));
    // What the assertions' evidence rests on, in the words every engine
    // surface gives it (`verdict_conditions::assumption_conditions`): parts
    // with no model, model-fidelity assumptions, checks not run. Only the
    // assumptions the evidence of some unwaived assertion cites (a waived
    // one is listed with its waiver), and not the member-sweep records made
    // above from the conditions already stated.
    let waived: std::collections::BTreeSet<&str> = (results.iter())
        .filter(|r| r.waived.is_some())
        .map(|r| r.label.as_str())
        .collect();
    verdict_conditions.extend(evidence_conditions(&evidence, &waived, &verdict_conditions));
    verdict_conditions.dedup();
    Ok(CiResult {
        spec_name: spec.name.clone(),
        board: spec.board.display().to_string(),
        results,
        seeds: outcomes.len() as u32,
        elapsed: started.elapsed(),
        analog_abort,
        coverage,
        substitutions: union_across(&outcomes, |o| &o.substitutions),
        dead_rails: union_across(&outcomes, |o| &o.dead_rails),
        dead_rail_causes: union_across(&outcomes, |o| &o.dead_rail_causes),
        coverage_warnings: union_across(&outcomes, |o| &o.coverage_warnings),
        verdict_conditions,
        timing_coverage,
        execution: execution_across(&outcomes),
        timing_refusals,
        waiver_notes: notes,
        inventory: evidence.inventory().to_vec(),
        assumptions: evidence.assumptions().to_vec(),
        evidence: evidence.maps().to_vec(),
    })
}

/// One execution-evidence row per MCU across the ensemble: the first member's
/// row, unless some member's row for that MCU is not a firmware pass, in which
/// case that one (a blocked member must not hide behind a clean one).
fn execution_across(
    outcomes: &[runner::RunOutcome],
) -> Vec<hauksbee_engine::execution::ExecutionEvidence> {
    let mut rows: Vec<hauksbee_engine::execution::ExecutionEvidence> = Vec::new();
    for row in outcomes.iter().flat_map(|o| &o.execution) {
        match rows.iter_mut().find(|r| r.mcu_ref == row.mcu_ref) {
            Some(kept) if kept.firmware_pass_eligible && !row.firmware_pass_eligible => {
                *kept = row.clone();
            }
            Some(_) => {}
            None => rows.push(row.clone()),
        }
    }
    rows
}

fn assertion_has_numeric_result(spec: &Spec, result: &assertions::AssertResult) -> bool {
    let peripheral_has_field = spec
        .asserts
        .iter()
        .any(|assertion| assertion.label() == result.label && assertion.field.is_some());
    result_kind_is_numeric(&result.kind, peripheral_has_field)
}

fn result_kind_is_numeric(kind: &str, peripheral_has_field: bool) -> bool {
    match kind {
        "uart" | "no_faults" | "protection_trip" => false,
        "peripheral" => peripheral_has_field,
        _ => true,
    }
}

/// The model record for one CI-spec `[[sensor]]`: its `[sensor.source]`, or
/// absent that, what a user-supplied card gets (user-model, unvalidated,
/// UNKNOWN uncertainty).
fn spec_sensor_model(id: &str, toml_src: &str) -> Result<ModelOnPath, SpecError> {
    use hauksbee_ir::evidence::{ModelSourceTier, ModelValidation};
    let invalid = |m: String| SpecError::Invalid(format!("sensor '{id}': {m}"));
    let sensor = hauksbee_models::SensorSpec::from_toml(toml_src)
        .map_err(|e| invalid(format!("failed to parse sensor spec: {e}")))?
        .sensor;
    let source = hauksbee_models::model_source(
        sensor.source.as_ref(),
        None,
        ModelSourceTier::UserModel,
        ModelValidation::Unvalidated,
        ModelLayer::Unspecified,
        &format!("ci spec [[sensor]] {id}"),
        id,
    )
    .map_err(|e| invalid(format!("invalid [sensor.source]: {e}")))?;
    let subject = format!("sensor:{id}");
    ModelOnPath::for_subject(subject, id, sensor.name, source, MatchConfidence::Exact)
        .map_err(|e| invalid(e.to_string()))
}

/// A `peripheral` assertion reads its sensor's register map, so that model is
/// on its causal path. A UART line the firmware may or may not derive from a
/// reading is not credited with a sensor it cannot be shown to depend on.
fn with_sensor_model(map: EvidenceMap, sensors: &[ModelOnPath], id: Option<&str>) -> EvidenceMap {
    match sensors.iter().find(|m| Some(m.reference()) == id) {
        Some(model) => {
            let models = [map.models(), std::slice::from_ref(model)].concat();
            map.with_models(models)
        }
        None => map,
    }
}

#[cfg(test)]
mod evidence_budget_tests {
    use super::result_kind_is_numeric;

    #[test]
    fn numeric_assertion_kinds_get_a_budget_and_textual_ones_do_not() {
        for kind in ["voltage", "rail_window", "boot-coverage", "hwtrace"] {
            assert!(result_kind_is_numeric(kind, false), "{kind}");
        }
        for kind in ["uart", "no_faults", "protection_trip"] {
            assert!(!result_kind_is_numeric(kind, false), "{kind}");
        }
        // A peripheral assertion is numeric only when it reads a field.
        assert!(result_kind_is_numeric("peripheral", true));
        assert!(!result_kind_is_numeric("peripheral", false));
    }
}

#[cfg(test)]
mod sensor_provenance_tests {
    use super::*;
    use hauksbee_ir::evidence::{CausalPathIndex, EvidenceRegistry, NetScope, RunDate};
    use hauksbee_ir::evidence::{ModelSourceTier, ModelUncertainty};

    const SENSOR: &str = "[sensor]\nname = \"LM75\"\nbus = \"i2c\"\ni2c_address = 0x48\n\
        [[sensor.register]]\naddr = 0x01\nconst = [0x00]\n[sensor.protocol]\nstyle = \"i2c_pointer\"\n";

    #[test]
    fn spec_sensor_provenance_reaches_peripheral_assertion_map() {
        let registry = EvidenceRegistry::new(Vec::new()).unwrap();
        let paths = CausalPathIndex::from_net_parts([("SDA", ["U2"].as_slice())]).unwrap();
        let scope = NetScope::new(["SDA"], None).unwrap();
        let traversal = paths.traverse(&scope, &registry).unwrap();
        let map =
            EvidenceMap::from_traversal("t", traversal, &registry, RunDate::unknown()).unwrap();
        let sensors = [spec_sensor_model("U2_temp", SENSOR).unwrap()];
        let unrelated = with_sensor_model(map.clone(), &sensors, None);
        assert!(unrelated.models().is_empty());
        let map = with_sensor_model(map, &sensors, Some("U2_temp"));
        let model = &map.models()[0];
        assert_eq!(map.models().len(), 1);
        assert_eq!(model.subject(), "sensor:U2_temp");
        assert_eq!(model.source().tier(), ModelSourceTier::UserModel);
        assert!(matches!(model.source().uncertainty(),
            [ModelUncertainty::Unknown { parameter, .. }] if parameter == "U2_temp.model"));
        let sourced = format!("{SENSOR}[sensor.source]\ntier = \"datasheet-derived\"\n");
        let model = spec_sensor_model("U2_temp", &sourced).unwrap();
        assert_eq!(model.source().tier(), ModelSourceTier::DatasheetDerived);
        assert!(spec_sensor_model("U2_temp", "[sensor]\n").is_err());
    }
}

#[cfg(test)]
mod timing_validity_tests {
    use super::*;

    #[test]
    fn unmet_timing_contract_cannot_green_or_be_waived() {
        let mut results = vec![assertions::AssertResult {
            passed: true,
            detail: "observed 2 toggles".into(),
            waived: Some("legacy waiver".into()),
            subject_nets: vec!["STROBE".into()],
            ..assertions::AssertResult::shell("short strobe captured".into(), "toggle", 1)
        }];
        refuse_timing_claims(
            &mut results,
            &["minimum pulse 1.000 us is unrepresentable".into()],
        );
        assert!(!results[0].passed);
        assert!(results[0].invalid);
        assert!(results[0].waived.is_none());
        assert!(results[0].detail.contains("minimum pulse 1.000 us"));
    }
}

#[cfg(test)]
mod ensemble_window_tests {
    use super::*;

    /// Seeds failing over overlapping spans make one failed span, and a
    /// fallback keeps only what no failure covers, at the latest rung any
    /// seed needed there.
    #[test]
    fn member_windows_merge_on_one_timeline() {
        let member =
            |failed: Vec<(f64, f64)>, fallback: Vec<(f64, f64, &str)>| runner::RunOutcome {
                failed_windows: failed,
                fallback_windows: fallback
                    .into_iter()
                    .map(|(a, b, m)| (a, b, m.to_string()))
                    .collect(),
                ..Default::default()
            };
        let outcomes = [
            member(vec![(1.0, 3.0)], vec![(0.0, 2.0, "reduced-step")]),
            member(
                vec![(2.0, 4.0), (6.0, 7.0)],
                vec![(5.0, 5.5, "reduced-step")],
            ),
            member(vec![], vec![(4.5, 5.2, "backward-euler")]),
        ];
        let (failed, fallback) = ensemble_windows(&outcomes);
        assert_eq!(failed, vec![(1.0, 4.0), (6.0, 7.0)]);
        let want = [
            (0.0, 1.0, "reduced-step"),
            (4.5, 5.2, "backward-euler"),
            (5.2, 5.5, "reduced-step"),
        ];
        let want: Vec<_> = want
            .iter()
            .map(|&(a, b, m)| (a, b, m.to_string()))
            .collect();
        assert_eq!(fallback, want);
    }
}
