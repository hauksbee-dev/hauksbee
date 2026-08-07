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
//! Output is a human report, a process exit code (0 green / 1 red), JUnit XML
//! for any CI system, and GitHub Actions annotations.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-ci/README.md.
//!
//! ```no_run
//! use hauksbee_ci::{run, RunConfig};
//! let cfg = RunConfig { spec: "ci/power-up.toml".into(), ..Default::default() };
//! let result = run(&cfg).unwrap();
//! std::process::exit(result.exit_code());
//! ```

use std::path::PathBuf;
use std::time::Instant;

pub mod assertions;
pub mod check;
pub mod error;
pub mod examples;
pub mod hwtrace;
pub mod init;
pub mod integrate;
pub mod progress;
pub mod report;
pub mod runner;
pub mod scenarios;
pub mod spec;
pub mod tolerance;

pub use error::SpecError;
pub use init::init;
pub use report::CiResult;
pub use spec::Spec;

/// The `--version` string: crate version plus the git hash this binary was
/// built from (`build.rs` sets `GIT_HASH`; absent outside a git checkout, e.g.
/// a source tarball, in which case the bare crate version is all we honestly
/// have). Two consumers must agree on it byte for byte: clap's `version =` in
/// main.rs, and the `# installed by ...` line [`integrate::hook_install`]
/// writes into the pre-commit hook so the hook can warn when the binary on
/// PATH is a different build. Returned as `&'static str` because that is what
/// clap's `version` wants; the one-time `OnceLock` init is the cheapest way to
/// a static composed string.
pub fn version_string() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| match option_env!("GIT_HASH") {
        Some(hash) => format!("{} (git {hash})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    })
}

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
    /// has, so a custom `[[models]]` routing entry binds in CI too.
    pub models_dir: Option<PathBuf>,
}

/// Apply the board's waiver file to the evaluated assertion results: a real
/// failure (never a pass, never an INVALID refusal) that an active `check =
/// "ci"` waiver matches is marked visible-but-not-gating. INVALID stays
/// unwaivable on purpose: a waiver overrules a *finding*, and an INVALID is
/// the absence of one; letting a waiver green an untrustworthy run would fail
/// open. Matching mirrors the engine's `--check` semantics exactly (same
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
/// check: a lapsed `ci` waiver whose finding gates again, and an active `ci`
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

/// Load the spec, run the co-sim across its seeds, and evaluate its assertions.
pub fn run(cfg: &RunConfig) -> Result<CiResult, SpecError> {
    let started = Instant::now();
    let spec = Spec::load(&cfg.spec)?;
    let extra: Vec<&std::path::Path> = cfg.models_dir.as_deref().into_iter().collect();
    let lib = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&extra);
    let outcomes = runner::run_spec_with_lib(&spec, cfg.seed, &lib)?;
    let mut results = assertions::evaluate(&spec, &outcomes);

    // Timing coverage is a validity contract, not an advisory note. One
    // unrepresentable request or one pulse the tick path demonstrably missed
    // makes every assertion INVALID, preventing a plausible false green.
    let mut timing_refusals: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        outcomes
            .iter()
            .flat_map(|o| o.timing_refusals.iter().cloned())
            .filter(|m| seen.insert(m.clone()))
            .collect()
    };
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
    // if no assertion's window happened to overlap the failed span (05 §3b).
    let analog_abort = outcomes.iter().any(|o| o.analog_abort);
    // Union of substitution messages across members (an MCU substituted once is
    // substituted for the whole ensemble), deduped and order-stable.
    let substitutions: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        outcomes
            .iter()
            .flat_map(|o| o.substitutions.iter().cloned())
            .filter(|m| seen.insert(m.clone()))
            .collect()
    };
    // Union of co-sim coverage warnings (dropped ADC channels, unexercised bus
    // peripherals) across members, deduped and order-stable, same discipline
    // as substitutions: a hole in ONE member is a hole in the ensemble.
    let coverage_warnings: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        outcomes
            .iter()
            .flat_map(|o| o.coverage_warnings.iter().cloned())
            .filter(|m| seen.insert(m.clone()))
            .collect()
    };
    // Coverage banner data for a tolerance ensemble: how many members ran and
    // how many components were sampled (from any outcome; the set is fixed).
    let coverage = if spec.has_tolerances() {
        let components = outcomes
            .first()
            .map(|o| o.sampled_values.len())
            .unwrap_or(0);
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
            Some(report::EnsembleCoverage::SingleMember {
                seed,
                components,
                corners,
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
                tolerance::Mode::Corners => report::EnsembleCoverage::Corners {
                    corners: members,
                    components,
                },
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
            .map_err(|error| SpecError::Invalid(format!("merging run evidence: {error}")))?;
    }
    let spec_bytes = std::fs::read(&cfg.spec).map_err(|error| {
        SpecError::Io(format!(
            "reading spec evidence '{}': {error}",
            cfg.spec.display()
        ))
    })?;
    evidence = evidence
        .with_toml_artifact(
            &cfg.spec,
            &spec_bytes,
            hauksbee_ir::evidence::ArtifactRole::Spec,
            hauksbee_ir::evidence::Contribution {
                what: "assertions".into(),
                detail: "run configuration, stimuli, supplies, and assertion thresholds".into(),
            },
        )
        .map_err(|error| SpecError::Invalid(format!("building spec evidence: {error}")))?;

    let waiver_path = spec
        .board_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(hauksbee_engine::waiver::DEFAULT_WAIVER_FILE);
    if waiver_path.is_file() {
        let bytes = std::fs::read(&waiver_path).map_err(|error| {
            SpecError::Io(format!(
                "reading waiver evidence '{}': {error}",
                waiver_path.display()
            ))
        })?;
        evidence = evidence
            .with_toml_artifact(
                &waiver_path,
                &bytes,
                hauksbee_ir::evidence::ArtifactRole::Waivers,
                hauksbee_ir::evidence::Contribution {
                    what: "authorizations".into(),
                    detail: "time-bounded human authorizations applied to matching CI findings"
                        .into(),
                },
            )
            .map_err(|error| SpecError::Invalid(format!("building waiver evidence: {error}")))?;
    }
    let today = hauksbee_ir::evidence::RunDate::from_system_clock();
    let waiver_assumptions = applied_waivers.iter().map(|waiver| {
        hauksbee_ir::evidence::Assumption::waived_assertion(
            &waiver.check,
            &waiver.kind,
            &waiver.subject,
            &waiver.subject,
            &waiver.reason,
            &waiver.until,
            today,
        )
    });
    for assumption in waiver_assumptions {
        evidence = evidence
            .with_assumptions([assumption.map_err(|error| {
                SpecError::Invalid(format!("building live-waiver evidence: {error}"))
            })?])
            .map_err(|error| {
                SpecError::Invalid(format!("building live-waiver evidence: {error}"))
            })?;
    }

    let failed_windows: Vec<(f64, f64)> = outcomes
        .iter()
        .flat_map(|outcome| outcome.failed_windows.iter().copied())
        .collect();
    let fallback_owned: Vec<(f64, f64, String)> = outcomes
        .iter()
        .flat_map(|outcome| outcome.fallback_windows.iter().cloned())
        .collect();
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
    .map_err(|error| SpecError::Invalid(format!("building CI error budget: {error}")))?;
    if let Some(residual) = outcomes
        .iter()
        .filter_map(|outcome| outcome.error_budget.as_ref()?.residual())
        .max_by(|left, right| left.max_abs().total_cmp(&right.max_abs()))
    {
        transient_budget = transient_budget.with_residual(residual.clone());
    }
    let coverage_description = coverage.as_ref().map(report::EnsembleCoverage::describe);
    let mut maps = Vec::with_capacity(results.len());
    for result in &results {
        let budget = if matches!(result.kind.as_str(), "phase_margin" | "ac_gain") {
            Some(
                hauksbee_engine::BoardEvidence::solver_error_budget(
                    &hauksbee_solve::SolverOptions::default(),
                )
                .map_err(|error| {
                    SpecError::Invalid(format!("building AC assertion budget: {error}"))
                })?,
            )
        } else if assertion_has_numeric_result(&spec, result) {
            Some(transient_budget.clone())
        } else {
            None
        };
        maps.push(
            evidence
                .ci_assertion_map(
                    &result.label,
                    &result.subject_nets,
                    &result.subject_refs,
                    budget,
                    coverage_description.as_deref(),
                )
                .map_err(|error| {
                    SpecError::Invalid(format!(
                        "building evidence for assertion '{}': {error}",
                        result.label
                    ))
                })?,
        );
    }
    evidence = evidence.with_maps(maps);
    for result in &mut results {
        if evidence
            .maps()
            .iter()
            .find(|map| map.assertion() == result.label)
            .is_some_and(hauksbee_ir::evidence::EvidenceMap::is_undermined)
        {
            result.invalid = true;
            result.waived = None;
            if !result.detail.starts_with("INVALID evidence:") {
                result.detail = format!(
                    "INVALID evidence: this assertion rests on a critical unresolved assumption. {}",
                    result.detail
                );
            }
        }
    }
    Ok(CiResult {
        spec_name: spec.name.clone(),
        board: spec.board.display().to_string(),
        results,
        seeds: outcomes.len() as u32,
        elapsed: started.elapsed(),
        analog_abort,
        coverage,
        substitutions,
        // A rail dead in one member is dead in every member (nothing per-seed
        // powers a rail), but union rather than assume, on the same discipline
        // as substitutions: a hole in ONE member is a hole in the ensemble.
        dead_rails: {
            let mut seen = std::collections::BTreeSet::new();
            outcomes
                .iter()
                .flat_map(|o| o.dead_rails.iter().cloned())
                .filter(|n| seen.insert(n.clone()))
                .collect()
        },
        coverage_warnings,
        timing_coverage,
        timing_refusals,
        waiver_notes: notes,
        inventory: evidence.inventory().to_vec(),
        assumptions: evidence.assumptions().to_vec(),
        evidence: evidence.maps().to_vec(),
    })
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

#[cfg(test)]
mod evidence_budget_tests {
    use super::result_kind_is_numeric;

    #[test]
    fn every_numeric_assertion_vocabulary_gets_a_budget() {
        for kind in [
            "voltage",
            "toggle",
            "max_current",
            "max_temp",
            "rail_window",
            "boot_coverage",
            "boot-coverage",
            "phase_margin",
            "ac_gain",
            "hwtrace",
            "model_coverage",
        ] {
            assert!(result_kind_is_numeric(kind, false), "{kind}");
        }
        assert!(result_kind_is_numeric("peripheral", true));
        for kind in ["uart", "no_faults", "protection_trip"] {
            assert!(!result_kind_is_numeric(kind, false), "{kind}");
        }
        assert!(!result_kind_is_numeric("peripheral", false));
    }
}

#[cfg(test)]
mod timing_validity_tests {
    use super::*;

    #[test]
    fn unmet_timing_contract_cannot_green_or_be_waived() {
        let mut results = vec![assertions::AssertResult {
            label: "short strobe captured".into(),
            kind: "toggle".into(),
            passed: true,
            invalid: false,
            detail: "observed 2 toggles".into(),
            failing_seed: None,
            failing_seeds: Vec::new(),
            seeds_total: 1,
            why: None,
            waived: Some("legacy waiver".into()),
            subject_nets: vec!["STROBE".into()],
            subject_refs: Vec::new(),
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
