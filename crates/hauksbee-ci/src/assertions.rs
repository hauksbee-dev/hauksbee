//! Evaluate a spec's assertions against the per-seed run outcomes. An assertion
//! passes only if it holds on *every* seed (that is what makes initial-state
//! fuzzing meaningful: the rail must come up across all random power-up states).

use crate::runner::RunOutcome;
use crate::spec::{Assertion, Spec};

/// The result of one assertion across all seeds. Serialized verbatim into the
/// `results` array of the `hauksbee-ci run --json` document, so the published
/// schema is generated from this type: see
/// `crates/hauksbee-ci/tests/ci_report_schema_drift.rs`. `why` and `waived` are
/// the only two fields a consumer may find ABSENT rather than null.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct AssertResult {
    /// The assertion's label: its `label` in the spec, or a generated one
    /// naming the kind and subject.
    pub label: String,
    /// The assertion kind, as the spec's `kind` token (`voltage`, `uart`,
    /// `blink`, `no_faults`, `hwtrace`, ...).
    pub kind: String,
    /// Did the assertion hold on every ensemble member. False on an ordinary
    /// red, on a waived red, and on an INVALID result.
    pub passed: bool,
    /// A THIRD outcome distinct from pass/fail (05 §3b): the assertion could not
    /// be honestly evaluated because its analog evaluation window overlaps a
    /// chunk the solver failed on, so the samples there are held-stale. When
    /// `invalid` is true, `passed` is always false, but this is NOT an ordinary
    /// failure: the run exits 3 (invalid-for-analysis) rather than 1, even below
    /// the consecutive-abort threshold. A normal pass or fail leaves it `false`.
    pub invalid: bool,
    /// One-line detail (the measured value, the offending seed, etc).
    pub detail: String,
    /// If it failed, the first seed index that failed (for fuzzed runs).
    /// Always present in the JSON, `null` on a pass, unlike `why` and `waived`
    /// which are omitted.
    #[schemars(schema_with = "schema_nullable_seed", required)]
    pub failing_seed: Option<u32>,
    /// Every ensemble member this assertion failed on (empty on a pass). For a
    /// tolerance ensemble this is the per-seed failure list the report and
    /// JUnit surface, together with the pass-rate.
    pub failing_seeds: Vec<u32>,
    /// How many ensemble members were evaluated.
    pub seeds_total: u32,
    /// On a real red: one sentence naming the OBSERVED shortfall ("dipped to
    /// 3.300 V, 0.100 V below your 3.4 V floor"), computed from the first
    /// failing member's measurement. The human report prints it as the `why:`
    /// line in place of the generic per-kind pointer. ABSENT (not null) on
    /// pass/INVALID and on kinds whose detail already carries the diagnosis.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "schema_absent_or_string")]
    pub why: Option<String>,
    /// Set when an active waiver (hauksbee-waivers.toml beside the board)
    /// covers this failure: the reason + expiry, e.g.
    /// "fab-confirmed artifact (until 2026-09-01)". A waived failure stays
    /// visible on every surface but does not gate the exit code. ABSENT (not
    /// null) when no waiver covers this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "schema_absent_or_string")]
    pub waived: Option<String>,
    /// Net names this assertion judges, for waiver matching. Not serialized:
    /// the JSON surface already carries them in the label/detail.
    #[serde(skip)]
    pub subject_nets: Vec<String>,
    /// Component references this assertion judges, for waiver matching.
    #[serde(skip)]
    pub subject_refs: Vec<String>,
}

/// An ensemble-member index that is ALWAYS emitted and may be `null`. Hand
/// written for the same reason as `report::schema_nullable_string`: schemars'
/// `required` attribute would drop `null` from the type and promise a number
/// where a passing assertion really does carry `null`.
pub(crate) fn schema_nullable_seed(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["integer", "null"],
        "format": "uint32",
        "minimum": 0
    })
}

/// A string that is OMITTED rather than set to `null` when it has no value
/// (`skip_serializing_if`). The plain `Option<String>` schema would allow
/// `null`, which this surface never emits; a consumer checking for the key's
/// presence is doing the right thing, and the schema should say so.
pub(crate) fn schema_absent_or_string(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string" })
}

/// Evaluate every assertion in the spec; returns one result per assertion.
pub fn evaluate(spec: &Spec, outcomes: &[RunOutcome]) -> Vec<AssertResult> {
    // Ensemble flavor drives the honest wording: None = plain fuzz/single run,
    // Some(mode) = tolerance ensemble (sampled coverage vs bounded corners).
    let mode = if spec.has_tolerances() {
        spec.ensemble_mode().ok()
    } else {
        None
    };
    spec.asserts
        .iter()
        .flat_map(|a| {
            if a.kind == "hwtrace" {
                // One hwtrace assertion expands to one result per (channel,
                // feature), so the report shows the per-feature table an EE
                // can argue with, not a single opaque pass/fail.
                evaluate_hwtrace(spec, a, outcomes, mode)
            } else {
                vec![evaluate_one(a, outcomes, mode)]
            }
        })
        .collect()
}

/// Evaluate one `hwtrace` assertion: load the trace, extract each declared
/// feature from the captured waveform and from every seed's simulated
/// waveform, and emit one [`AssertResult`] per (channel, feature) with both
/// values in the detail line. A feature passes only if it holds on every seed
/// (same all-seeds rule as every other assertion kind).
fn evaluate_hwtrace(
    spec: &Spec,
    a: &Assertion,
    outcomes: &[RunOutcome],
    mode: Option<crate::tolerance::Mode>,
) -> Vec<AssertResult> {
    use crate::hwtrace;

    // In corners mode a member index is a corner number, not a random seed,
    // label it to match evaluate_one, the coverage banner, and every sibling
    // assertion.
    let member = match mode {
        Some(crate::tolerance::Mode::Corners) => "corner",
        _ => "seed",
    };

    let hard_fail = |label: String, detail: String| AssertResult {
        label,
        kind: "hwtrace".to_string(),
        passed: false,
        invalid: false,
        detail,
        failing_seed: None,
        failing_seeds: Vec::new(),
        seeds_total: outcomes.len() as u32,
        why: None,
        waived: None,
        subject_nets: Vec::new(),
        subject_refs: Vec::new(),
    };

    let path = match hwtrace::trace_path(spec, a) {
        Ok(p) => p,
        Err(e) => return vec![hard_fail(a.label(), e.to_string())],
    };
    let trace = match hwtrace::Trace::load(&path) {
        Ok(t) => t,
        Err(e) => return vec![hard_fail(a.label(), e.to_string())],
    };

    // The provenance banner: a synthetic trace must announce itself in the
    // report so a green run can never be mistaken for hardware validation.
    let provenance = if trace.trace.provenance == "synthetic" {
        " [SYNTHETIC trace: validates the harness, not the hardware]"
    } else {
        ""
    };

    // The analog-validity gate (05 §3b): the simulated waveforms are per-frame
    // analog samples over the whole run, so any failed-solve window inside the
    // run makes every feature INVALID rather than pass/fail.
    let stale = outcomes.iter().find(|o| !o.failed_windows.is_empty());

    let mut results = Vec::new();
    for ch in &trace.channels {
        let captured = match trace.load_channel(ch) {
            Ok(s) => s,
            Err(e) => {
                results.push(hard_fail(format!("hwtrace {}", ch.net), e.to_string()));
                continue;
            }
        };
        for f in &ch.features {
            let label = format!("hwtrace {} {}", ch.net, f.kind);

            // Per-seed comparison; the feature must hold on every EVALUABLE seed.
            // A member whose analog solve diverged (non-empty failed_windows) has
            // held-stale net_series, so SKIP it; its comparison is untrustworthy.
            // FAIL > INVALID (round-50): a converged member's real disagreement with
            // the capture must beat a diverged sibling's INVALID, so we only fall
            // back to INVALID (below) when no converged member fails.
            let mut last_detail = String::new();
            let mut failures: Vec<(u32, String)> = Vec::new();
            for out in outcomes {
                if !out.failed_windows.is_empty() {
                    continue;
                }
                let Some(sim) = out.net_series.get(&ch.net) else {
                    failures.push((
                        out.seed,
                        format!(
                            "net '{}' was never sampled by the run; check the net name in \
                             the trace against the board",
                            ch.net
                        ),
                    ));
                    continue;
                };
                let r = hwtrace::compare(&ch.net, f, &captured, sim);
                if r.pass {
                    last_detail = r.detail;
                } else {
                    failures.push((out.seed, r.detail));
                }
            }

            // No trustworthy (converged) failure, but a member diverged → INVALID
            // > PASS: a run with a held-stale window must not masquerade as a pass.
            if failures.is_empty() {
                if let Some(out) = stale {
                    let (fs, fe) = out.failed_windows[0];
                    results.push(AssertResult {
                        label,
                        kind: "hwtrace".to_string(),
                        passed: false,
                        invalid: true,
                        detail: format!(
                            "INVALID: the analog solve failed within {:.2}-{:.2} ms of the run; \
                             the simulated waveform contains held-stale samples, so no feature \
                             comparison against the capture can be trusted (05 §3b).",
                            fs * 1e3,
                            fe * 1e3
                        ),
                        failing_seed: Some(out.seed),
                        failing_seeds: vec![out.seed],
                        seeds_total: outcomes.len() as u32,
                        why: None,
                        waived: None,
                        subject_nets: vec![ch.net.clone()],
                        subject_refs: Vec::new(),
                    });
                    continue;
                }
            }

            let (passed, mut detail, failing_seeds) = match failures.first() {
                None => (true, last_detail, Vec::new()),
                Some((seed, d)) => {
                    let d = if outcomes.len() > 1 {
                        format!("{member} {seed}: {d}")
                    } else {
                        d.clone()
                    };
                    (false, d, failures.iter().map(|(s, _)| *s).collect())
                }
            };
            detail.push_str(provenance);
            results.push(AssertResult {
                label,
                kind: "hwtrace".to_string(),
                passed,
                invalid: false,
                detail,
                failing_seed: failing_seeds.first().copied(),
                failing_seeds,
                seeds_total: outcomes.len() as u32,
                why: None,
                waived: None,
                subject_nets: vec![ch.net.clone()],
                subject_refs: Vec::new(),
            });
        }
    }
    results
}

/// How one ensemble member is named in a detail line: `corner 3`, `seed 3`, or
/// `interior probe 12` for a corner-mode interior member.
///
/// The interior members are numbered on from the corners, so calling one
/// "corner 12" would send a reader looking for a min/max combination that does
/// not exist. `member` is the caller's already-resolved corner/seed noun.
fn member_label(member: &str, out: &RunOutcome) -> String {
    if out.interior {
        format!("interior probe {}", out.seed)
    } else {
        format!("{member} {}", out.seed)
    }
}

fn evaluate_one(
    a: &Assertion,
    outcomes: &[RunOutcome],
    mode: Option<crate::tolerance::Mode>,
) -> AssertResult {
    let label = a.label();
    let kind = a.kind.clone();
    // In corner mode a member index is a corner number, not a random seed.
    let member = match mode {
        Some(crate::tolerance::Mode::Corners) => "corner",
        _ => "seed",
    };

    // Is this member's analog evaluation window held-stale, i.e. does it overlap
    // a chunk the solver failed on? Such a member's samples cannot be trusted, so
    // its pass/fail is meaningless (05 §3b, "refuse rather than fake").
    let member_invalid = |out: &RunOutcome| -> bool {
        analog_eval_window(a, out).is_some_and(|(ws, we)| {
            out.failed_windows
                .iter()
                .any(|&(fs, fe)| ws < fe && fs < we)
        })
    };

    // Precedence is FAIL > INVALID > PASS. Evaluate every ensemble member so the
    // result carries the pass-rate and the full failing-seed list, but SKIP a
    // member whose own window is analog-invalid: its held-stale samples are not a
    // trustworthy fail or pass. A member that fails on a fully-converged solve is a
    // real, disproving failure and is decisive under the all-seeds rule, even if a
    // DIFFERENT member diverged, so we must evaluate trustworthy failures before
    // letting a diverged sibling refuse the whole assertion (otherwise a genuine
    // brownout on one corner is silently downgraded to INVALID by an unrelated
    // convergence hiccup on another).
    // The nets/refs this assertion judges, carried on the result so the waiver
    // layer can match a failure against hauksbee-waivers.toml without walking
    // back to the spec (hwtrace expansion breaks any index-based mapping).
    let subject_nets: Vec<String> = [a.net.as_ref(), a.supply_net.as_ref()]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let subject_refs: Vec<String> = a.reference.iter().cloned().collect();

    let mut last_detail = String::new();
    let mut failures: Vec<(&RunOutcome, String, Option<String>)> = Vec::new();
    for out in outcomes {
        if member_invalid(out) {
            continue;
        }
        let (ok, detail, why) = check_seed(a, out);
        if ok {
            last_detail = detail;
        } else {
            failures.push((out, detail, why));
        }
    }

    if let Some((first, first_detail, first_why)) = failures.first() {
        // Lead with the first failing member's measurement, then the exact
        // sampled component values it ran with (the actionable artifact), then
        // the ensemble pass-rate.
        let mut detail = if outcomes.len() > 1 {
            format!("{}: {first_detail}", member_label(member, first))
        } else {
            first_detail.clone()
        };
        // Non-monotonicity, caught: every corner passed and an interior probe
        // did not. The corner set's whole claim is that an extreme of the inputs
        // produces an extreme of the output, and this is that claim disproved on
        // this board, for this assertion. Say so at the point of failure, because
        // the reflex on a corner-mode red is to look at the named corner values,
        // and here the corner values are exactly what did NOT find it.
        if failures.iter().all(|(o, _, _)| o.interior)
            && outcomes.iter().any(|o| !o.interior && !member_invalid(o))
        {
            detail.push_str(
                " [NON-MONOTONIC: every min/max corner passed and this INTERIOR point \
                 failed, so the corners do not bound this response and a corner-only \
                 run would have reported green]",
            );
        }
        if !first.sampled_values.is_empty() {
            detail.push_str(&format!(
                " [{}]",
                crate::tolerance::describe_values(&first.sampled_values)
            ));
        }
        if outcomes.len() > 1 {
            let failing: Vec<String> = failures
                .iter()
                .take(8)
                .map(|(o, _, _)| o.seed.to_string())
                .collect();
            let more = if failures.len() > 8 { ", …" } else { "" };
            // The passed count must exclude held-stale (INVALID) members, not fold
            // them into "passed": those members were SKIPPED (member_invalid) and
            // never evaluated, so counting them green over-claims worst-case
            // coverage on the very surface a reviewer trusts. passed = total −
            // failed − invalid; surface the invalid count when any occurred.
            let invalid = outcomes.iter().filter(|o| member_invalid(o)).count();
            let passed = outcomes.len() - failures.len() - invalid;
            let invalid_note = if invalid > 0 {
                format!(", {invalid} invalid")
            } else {
                String::new()
            };
            // The pass-rate must not fold the interior probes into the corner
            // count. "passed 7/10 corners" on a 2-component board is a lie a
            // reader can catch with arithmetic (2 components is 4 corners), and
            // the corner total is exactly what they check the coverage banner
            // against.
            let probes = outcomes.iter().filter(|o| o.interior).count();
            let noun = if probes > 0 {
                format!("{member}s + interior probes")
            } else {
                format!("{member}s")
            };
            detail.push_str(&format!(
                "; passed {}/{} {noun} (failing: {}{more}{invalid_note})",
                passed,
                outcomes.len(),
                failing.join(", "),
            ));
        }
        return AssertResult {
            label,
            kind,
            passed: false,
            invalid: false,
            detail,
            failing_seed: Some(failures[0].0.seed),
            failing_seeds: failures.iter().map(|(o, _, _)| o.seed).collect(),
            seeds_total: outcomes.len() as u32,
            why: first_why.clone(),
            waived: None,
            subject_nets,
            subject_refs,
        };
    }

    // No trustworthy failure. INVALID > PASS: if any member's evaluation window
    // was held-stale (analog divergence), we can neither honestly pass nor fail,
    // report a distinct INVALID rather than let a window with no valid samples
    // masquerade as a pass. This fires per-window (not per consecutive-abort), so
    // an intermittent divergence below the abort threshold still refuses.
    for out in outcomes {
        if let Some((ws, we)) = analog_eval_window(a, out) {
            if let Some(&(fs, fe)) = out
                .failed_windows
                .iter()
                .find(|&&(fs, fe)| ws < fe && fs < we)
            {
                let per_seed = if outcomes.len() > 1 {
                    format!("{}: ", member_label(member, out))
                } else {
                    String::new()
                };
                return AssertResult {
                    label,
                    kind,
                    passed: false,
                    invalid: true,
                    detail: format!(
                        "{per_seed}INVALID: the analog solve failed within this \
                         assertion's window ({:.2}-{:.2} ms overlaps its \
                         {:.2}-{:.2} ms evaluation span); those voltages are \
                         held-stale, so the result cannot be trusted (05 §3b).",
                        fs * 1e3,
                        fe * 1e3,
                        ws * 1e3,
                        we * 1e3,
                    ),
                    failing_seed: Some(out.seed),
                    failing_seeds: vec![out.seed],
                    seeds_total: outcomes.len() as u32,
                    why: None,
                    waived: None,
                    subject_nets,
                    subject_refs,
                };
            }
        }
    }

    // All members green. State exactly what that means: plain fuzz keeps its
    // wording; a tolerance ensemble claims sampled coverage (never proof), and
    // corners claim boundedness only for monotonic responses.
    let detail = all_green_detail(
        outcomes.len(),
        outcomes.iter().filter(|o| o.interior).count(),
        mode,
        last_detail,
    );
    AssertResult {
        label,
        kind,
        passed: true,
        invalid: false,
        detail,
        failing_seed: None,
        failing_seeds: Vec::new(),
        seeds_total: outcomes.len() as u32,
        why: None,
        waived: None,
        subject_nets,
        subject_refs,
    }
}

/// The all-members-green detail string for an assertion, worded to the ensemble
/// mode. `members` is the total member count INCLUDING the nominal baseline
/// (member 0), so a MonteCarlo run reports `members - 1` sampled seeds; the
/// nominal draws no random sample and must not be counted as one (mirrors the
/// run banner in `lib.rs`, which also subtracts the nominal).
fn all_green_detail(
    members: usize,
    interior: usize,
    mode: Option<crate::tolerance::Mode>,
    last_detail: String,
) -> String {
    match (members, mode) {
        (1, _) => last_detail,
        (n, None) => format!("{last_detail} (held across {n} seeds)"),
        (n, Some(crate::tolerance::Mode::MonteCarlo)) => {
            let sampled = n.saturating_sub(1);
            format!(
                "{last_detail} (passed all {n} members: {sampled} sampled tolerance \
                 seed(s) + nominal: statistical coverage, not worst-case proof)"
            )
        }
        // The corner claim, narrowed to what was actually checked. The old
        // wording made monotonicity the reader's problem: it disclosed the
        // assumption and left them no way to test it. The interior probes test
        // it, so the disclosure now reports a search that came back empty rather
        // than an assumption nobody looked at, and it still stops short of proof,
        // because a sampled interior is a sample.
        (n, Some(crate::tolerance::Mode::Corners)) => {
            let corners = n.saturating_sub(interior);
            if interior == 0 {
                format!(
                    "{last_detail} (held on all {corners} min/max tolerance corners: bounds the \
                     worst case only where the response is monotonic in each value)"
                )
            } else {
                format!(
                    "{last_detail} (held on all {corners} min/max tolerance corners and on \
                     {interior} interior Latin-hypercube probe(s), which found no \
                     non-monotonic response: the corners bound the worst case unless a \
                     non-monotonicity sits between the probes)"
                )
            }
        }
    }
}

/// The sim-time window (seconds) an assertion evaluates the analog transient
/// operating point over, or `None` when the assertion is not analog-transient-
/// derived and a failed chunk therefore does not invalidate it.
///
/// `uart` (digital MCU output), `peripheral` (bus-slave state), `protection_trip`
/// (a latch in the supply model) and the small-signal `ac_gain` / `phase_margin`
/// (a separate DC-linearised sweep, not the transient march) are NOT invalidated
/// by a failed transient chunk, so they return `None`. Everything that reads a
/// per-frame node voltage / current / temperature returns its span; the run's
/// total sim time is the open end. `rail_window` uses the whole run conservatively
/// (the scoped scenario start is not reachable here); this only ever bites a run
/// that actually diverged, which must be INVALID regardless, so the conservatism
/// is in the honest direction.
fn analog_eval_window(a: &Assertion, out: &RunOutcome) -> Option<(f64, f64)> {
    let end = out.sim_ms / 1000.0;
    match a.kind.as_str() {
        "voltage" => Some((a.after_ms.unwrap_or(0.0) / 1000.0, end)),
        "toggle" => Some((0.0, end)),
        "max_current" | "max_temp" => Some((0.0, end)),
        "boot_coverage" | "boot-coverage" => {
            Some((0.0, a.deadline_ms.map(|d| d / 1000.0).unwrap_or(end)))
        }
        "rail_window" => Some((0.0, end)),
        // A vcd_sink's `transitions` field counts analog per-frame threshold
        // crossings (exactly like `toggle`), so a chunk the solver failed on
        // corrupts it and it must be INVALID, not a definite pass/fail. Digital
        // bus-slave peripheral state (i2c/spi) is NOT analog-derived and stays
        // un-gated; keying on the field is fail-safe (worst case an unrelated
        // field is refused on divergence, the honest direction).
        "peripheral" if a.field.as_deref() == Some("transitions") => Some((0.0, end)),
        _ => None,
    }
}

/// Check one assertion against one seed's outcome. Returns (passed, detail,
/// why): `why` is the observed-shortfall sentence a failing bound computes
/// (`None` on a pass, and on kinds whose detail already IS the diagnosis,
/// e.g. boot_coverage's drive-state analysis).
fn check_seed(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    // Adapter for the kinds whose checks carry no separate `why`.
    fn plain((ok, detail): (bool, String)) -> (bool, String, Option<String>) {
        (ok, detail, None)
    }
    match a.kind.as_str() {
        "voltage" => check_voltage(a, out),
        "uart" => plain(check_uart(a, out)),
        "toggle" => check_toggle(a, out),
        "no_faults" => plain(check_no_faults(out)),
        "max_current" => check_max_current(a, out),
        "max_temp" => check_max_temp(a, out),
        "peripheral" => plain(check_peripheral(a, out)),
        "rail_window" => check_rail_window(a, out),
        "protection_trip" => plain(check_protection_trip(a, out)),
        "boot_coverage" | "boot-coverage" => plain(check_boot_coverage(a, out)),
        "model_coverage" => plain(check_model_coverage(a, out)),
        "phase_margin" => check_phase_margin(a, out),
        "ac_gain" => check_ac_gain(a, out),
        other => (false, format!("unknown assertion kind '{other}'"), None),
    }
}

/// model_coverage: hold the line on how much of the board bound to a real model.
///
/// The failure text names the parts rather than only the shortfall, because the
/// list IS the user's next action: each name is either a model to write (the
/// extending guides take one TOML file) or a part whose vendor keeps its model
/// encrypted, and those two need different responses.
fn check_model_coverage(a: &crate::spec::Assertion, out: &RunOutcome) -> (bool, String) {
    let Some(bind) = out.bind.as_ref() else {
        return (
            false,
            "no bind summary was captured for this run, so coverage cannot be judged".to_string(),
        );
    };

    let mut checks: Vec<(bool, String)> = Vec::new();

    if let Some(floor) = a.min_critical {
        // A board with no active ICs has nothing to bind. Reading that as 100%
        // would let the assertion pass on a board it never looked at, so it
        // fails and says which case it is.
        if bind.critical_parts_total == 0 {
            checks.push((
                false,
                "min_critical was set, but the board has no active ICs to bind".to_string(),
            ));
        } else {
            let got = bind.critical_parts_bound_n as f64 / bind.critical_parts_total as f64;
            checks.push((
                got >= floor,
                format!(
                    "active ICs bound {} ({:.1}%), floor {:.1}%",
                    bind.critical_parts_bound,
                    got * 100.0,
                    floor * 100.0
                ),
            ));
        }
    }

    if let Some(floor) = a.min_resolved {
        if bind.non_ignored == 0 {
            checks.push((
                false,
                "min_resolved was set, but the board has no simulatable parts".to_string(),
            ));
        } else {
            let got = bind.resolved as f64 / bind.non_ignored as f64;
            checks.push((
                got >= floor,
                format!(
                    "parts bound {}/{} ({:.1}%), floor {:.1}%",
                    bind.resolved,
                    bind.non_ignored,
                    got * 100.0,
                    floor * 100.0
                ),
            ));
        }
    }

    if let Some(ceiling) = a.max_active_unresolved {
        let n = bind.active_path_unresolved.len();
        let named: Vec<&str> = bind
            .active_path_unresolved
            .iter()
            .take(8)
            .map(|u| u.reference.as_str())
            .collect();
        let tail = if n > named.len() {
            format!(" and {} more", n - named.len())
        } else {
            String::new()
        };
        checks.push((
            n <= ceiling,
            if n == 0 {
                "no unresolved parts on connected nets".to_string()
            } else {
                format!(
                    "{n} unresolved on connected nets (limit {ceiling}): {}{tail}",
                    named.join(", ")
                )
            },
        ));
    }

    if checks.is_empty() {
        return (
            false,
            "model_coverage needs at least one of min_critical, min_resolved or \
             max_active_unresolved; an assertion with no threshold checks nothing"
                .to_string(),
        );
    }

    let passed = checks.iter().all(|(ok, _)| *ok);
    let detail = checks
        .into_iter()
        .map(|(_, d)| d)
        .collect::<Vec<_>>()
        .join("; ");
    (passed, detail)
}

/// rail_window: judge a rail's behaviour over a scenario window: min/max bounds,
/// dip duration below a threshold, and recovery time.
fn check_rail_window(
    a: &crate::spec::Assertion,
    out: &RunOutcome,
) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let scope = a.scenario.clone().unwrap_or_default();
    let Some(win) = out.rail_windows.get(&(scope.clone(), net.clone())) else {
        return (
            false,
            format!("net '{net}' was never sampled in scenario window '{scope}'"),
            None,
        );
    };
    if win.samples.is_empty() {
        return (
            false,
            format!("net '{net}' had no samples in the window"),
            None,
        );
    }

    // Same marking discipline as check_voltage: the failing clause carries
    // `<- FAILED HERE`, the passing clauses stay un-annotated, and the why
    // names the observed excess (volts of sag, ms over the dip budget).
    let mut ok = true;
    let mut parts = Vec::new();
    let mut whys = Vec::new();
    if let Some(lo) = a.min {
        if win.min_v >= lo - 1e-6 {
            parts.push(format!("min={:.3}V (>= {lo}V)", win.min_v));
        } else {
            ok = false;
            parts.push(format!(
                "min={:.3}V < required {lo}V <- FAILED HERE",
                win.min_v
            ));
            whys.push(format!(
                "{net} sagged to {:.3} V in the window, {:.3} V below your {lo} V floor",
                win.min_v,
                lo - win.min_v
            ));
        }
    }
    if let Some(hi) = a.max {
        if win.max_v <= hi + 1e-6 {
            parts.push(format!("max={:.3}V (<= {hi}V)", win.max_v));
        } else {
            ok = false;
            parts.push(format!(
                "max={:.3}V > allowed {hi}V <- FAILED HERE",
                win.max_v
            ));
            whys.push(format!(
                "{net} rose to {:.3} V in the window, {:.3} V above your {hi} V ceiling",
                win.max_v,
                win.max_v - hi
            ));
        }
    }
    if let (Some(d), Some(for_ms)) = (a.dip_below, a.for_max_ms) {
        // A duration needs at least two samples to measure (windows(2) yields
        // nothing from one point, so dip_duration_s folds to 0 and silently
        // auto-passes). A window that spans less than one frame is degenerate,
        // fail loudly rather than claim a timing spec we could not evaluate.
        if win.samples.len() < 2 {
            ok = false;
            parts.push(format!(
                "dip<{d}V: window has {} sample(s), too few to measure a duration",
                win.samples.len()
            ));
        } else {
            let dip_ms = win.dip_duration_s(d) * 1000.0;
            if dip_ms <= for_ms + 1e-6 {
                parts.push(format!("dip<{d}V for {dip_ms:.2}ms (<= {for_ms}ms)"));
            } else {
                ok = false;
                parts.push(format!(
                    "dip<{d}V for {dip_ms:.2}ms > allowed {for_ms}ms <- FAILED HERE"
                ));
                whys.push(format!(
                    "{net} sat below {d} V for {dip_ms:.2} ms, {:.2} ms longer than your budget",
                    dip_ms - for_ms
                ));
            }
        }
    }
    if let (Some(d), Some(r), Some(within_ms)) = (a.dip_below, a.recover_to, a.recover_within_ms) {
        if win.samples.len() < 2 {
            ok = false;
            parts.push(format!(
                "recover-to-{r}V: window has {} sample(s), too few to measure a duration",
                win.samples.len()
            ));
        } else {
            let rec_ms = win.recovery_s(d, r) * 1000.0;
            if rec_ms <= within_ms + 1e-6 {
                parts.push(format!(
                    "recover-to-{r}V in {rec_ms:.2}ms (<= {within_ms}ms)"
                ));
            } else {
                ok = false;
                parts.push(format!(
                    "recover-to-{r}V in {rec_ms:.2}ms > allowed {within_ms}ms <- FAILED HERE"
                ));
                whys.push(format!(
                    "{net} took {rec_ms:.2} ms to climb back to {r} V after dipping below \
                     {d} V, {:.2} ms past your recovery deadline",
                    rec_ms - within_ms
                ));
            }
        }
    }

    (
        ok,
        format!(
            "{net} window: {} [min={:.3}V max={:.3}V]",
            parts.join(", "),
            win.min_v,
            win.max_v
        ),
        if whys.is_empty() {
            None
        } else {
            Some(whys.join("; "))
        },
    )
}

/// protection_trip: whether a supply net's battery protection latched.
fn check_protection_trip(a: &crate::spec::Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.supply_net.clone().unwrap_or_default();
    let want = a.expect_trip.unwrap_or(false);
    // When the assertion names a scenario, only count trips that latched inside
    // that scenario's window, a trip from an earlier scenario must not satisfy
    // (or violate) an assertion scoped to a later one. Unscoped assertions fall
    // back to the run-wide "ever tripped" flag. An explicit empty scenario ("")
    // means the run-wide window, same as leaving it unset (Spec::validate's
    // documented contract), filter it out so it takes the run-wide branch, not a
    // scoped lookup that misses and returns a false RED. Mirrors check_rail_window.
    let tripped = match a.scenario.as_deref().filter(|s| !s.is_empty()) {
        Some(scope) => {
            let scope = scope.to_string();
            match out
                .protection_tripped_scoped
                .get(&(scope.clone(), net.clone()))
                .copied()
            {
                Some(t) => Some(t),
                None => {
                    return (
                        false,
                        format!(
                            "{net} was not a supply net in scenario window '{scope}' (nothing to trip)"
                        ),
                    );
                }
            }
        }
        None => out.protection_tripped.get(&net).copied(),
    };
    match tripped {
        Some(t) => {
            let ok = t == want;
            (
                ok,
                format!(
                    "{net} protection {} (expected {})",
                    if t { "TRIPPED" } else { "held" },
                    if want { "trip" } else { "no trip" }
                ),
            )
        }
        None => (
            false,
            format!("{net} has no battery protection supply leg (nothing to trip)"),
        ),
    }
}

/// Boot-coverage: the control net named by `net` must reach its defined level
/// (`min`, volts) by `deadline_ms` after reset, hold it per `hold_ms`, and no
/// stress fault may fire during the boot window before it does. This makes the
/// "Hi-Z control input" class decidable by running the firmware: on a net with
/// no static board bias (the genuinely-undefined case this targets) only the
/// firmware can bring it to level, so this measures whether the firmware drives
/// it in time. A statically-biased net reads at level from t=0 and is out of
/// scope (it is never undefined).
///
/// The hold requirement has three shapes, selected by `hold_ms`:
///   - absent: hold continuously through the whole boot deadline (the strict
///     legacy default, right for a set-and-hold control net);
///   - `0`: the level only needs to be REACHED by the deadline (a heartbeat /
///     toggling net can never hold to the deadline, so this is its honest form);
///   - `h > 0`: hold continuously for `h` ms after the FIRST reach; a hold
///     window the simulation did not fully observe fails as unconfirmed rather
///     than passing on hope.
fn check_boot_coverage(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let level = a.min.unwrap_or(0.0);
    let deadline = a.deadline_ms.unwrap_or(0.0);
    let key = (net.clone(), level.to_bits());

    // The boot window is [0, deadline], but the firmware run only produced data
    // out to sim_ms. In the hold-to-deadline form (hold_ms absent), a deadline
    // past the end of the simulation means the tail of the window was never
    // observed, so "boot window clean" would be asserting coverage over an
    // unsimulated interval, a false green. Report it as unmet (mirrors
    // check_voltage's "never sampled" failure) rather than trusting a
    // first-cross with no chance of a later drop being seen. With hold_ms set,
    // an in-window reach can be confirmed without simulating out to the
    // deadline, so the guard only applies when nothing was ever observed to
    // reach (below).
    let sim_too_short = || {
        (
            false,
            format!(
                "boot deadline {deadline} ms is past the end of the {:.2} ms simulation, \
                 so boot coverage for control net '{net}' cannot be confirmed; extend the run duration",
                out.sim_ms
            ),
        )
    };
    if a.hold_ms.is_none() && deadline > out.sim_ms + 1e-9 {
        return sim_too_short();
    }

    let first_cross = out.boot_first_cross_ms.get(&key).copied();
    let drop_after = out.boot_drop_after_cross_ms.get(&key).copied();
    match first_cross {
        // Never reached its level at all. If the simulation also ended before
        // the deadline, the un-simulated tail could still have held a reach, so
        // that case is "cannot be confirmed", not "never reached".
        None if deadline > out.sim_ms + 1e-9 => sim_too_short(),
        // Genuinely undriven or driven-but-low over the whole observed window.
        None => (
            false,
            boot_below_threshold_msg(
                &net,
                level,
                out.driven_nets.contains(&net),
                out.drive_direction_observable,
                boot_observed_range(&net, out),
            ),
        ),
        // Reached, but only after the deadline.
        Some(tc) if tc > deadline + 1e-9 => (
            false,
            format!(
                "control net '{net}' first reached {level} V at {tc:.2} ms, past the {deadline} ms boot deadline"
            ),
        ),
        Some(tc) => {
            // Reached in time. Now the hold requirement, per hold_ms.
            match a.hold_ms {
                // Absent: hold continuously through the deadline. A drop back
                // below level at or before the deadline breaks coverage (a bare
                // end-of-run latch would conflate this case with "never
                // reached"). A drop AFTER the deadline is a legitimate release
                // and does not fail, boot coverage is about the boot window only.
                None => {
                    if let Some(td) = drop_after {
                        if td <= deadline + 1e-9 {
                            return (
                                false,
                                format!(
                                    "control net '{net}' reached {level} V at {tc:.2} ms but fell back below it at {td:.2} ms, before the {deadline} ms boot deadline"
                                ),
                            );
                        }
                    }
                }
                // hold_ms > 0: hold continuously for h ms after the first reach.
                Some(h) if h > 0.0 => {
                    if let Some(td) = drop_after {
                        let held = td - tc;
                        if held + 1e-9 < h {
                            return (
                                false,
                                format!(
                                    "control net '{net}' reached {level} V at {tc:.2} ms but held it only \
                                     {held:.2} ms (fell back below at {td:.2} ms), shorter than the required \
                                     hold_ms = {h}"
                                ),
                            );
                        }
                    } else if tc + h > out.sim_ms + 1e-9 {
                        // Never observed to drop, but the hold window runs past
                        // the end of the simulation: the tail was never seen, so
                        // the hold cannot be confirmed (same honesty rule as the
                        // deadline-past-sim-end guard above).
                        return (
                            false,
                            format!(
                                "control net '{net}' reached {level} V at {tc:.2} ms, but its {h} ms hold \
                                 window runs past the end of the {:.2} ms simulation, so the hold cannot \
                                 be confirmed; extend the run duration",
                                out.sim_ms
                            ),
                        );
                    }
                }
                // hold_ms = 0: reached by the deadline is enough; any later
                // drop (a heartbeat's low phase) is expected, not a failure.
                Some(_) => {}
            }
            // Hold satisfied. Now require no stress fault fired in the boot
            // window *before* the net was first driven (rails must hold and
            // nothing over-stresses while the control input is still undefined).
            if let Some(ft) = out.first_fault_ms {
                if ft < tc - 1e-9 {
                    return (
                        false,
                        format!(
                            "control net '{net}' was driven at {tc:.2} ms, but a stress fault fired earlier at {ft:.2} ms during the boot window"
                        ),
                    );
                }
            }
            let hold_note = match a.hold_ms {
                None => "boot window clean".to_string(),
                Some(h) if h > 0.0 => format!("held {h} ms, boot window clean"),
                Some(_) => "reach only (hold_ms = 0), boot window clean".to_string(),
            };
            (
                true,
                format!("control net '{net}' driven to >= {level} V at {tc:.2} ms (<= {deadline} ms), {hold_note}"),
            )
        }
    }
}

/// Diagnosis for a boot-coverage net that never reached its required level.
///
/// The engine tracks pin *drive state* separately from *voltage*, so this must
/// not blame "Hi-Z / undefined" on a pin the firmware actively drove. Three
/// honest cases, keyed off what the run actually knows:
///   - the net is in `driven_nets` (the firmware drove it to a defined level):
///     it was driven, the voltage just never crossed the threshold;
///   - it is not driven and the backend can report drive direction (the AVR
///     backend reads DDR; a Renode part with a direction-register map in its
///     SoC descriptor reads MODER/CRL+CRH/DIR, either way a held-LOW pin is
///     known driven): a net absent here is genuinely undriven / Hi-Z;
///   - it is not driven and the backend cannot report drive direction (QEMU,
///     or a Renode part without a dir map, those see drive state only through
///     observed edges): absence is ambiguous, so we say only what is known
///     rather than asserting Hi-Z on what might be a held-LOW pin.
fn boot_below_threshold_msg(
    net: &str,
    level: f64,
    driven: bool,
    drive_direction_observable: bool,
    observed: Option<(f64, f64)>,
) -> String {
    let range = observed
        .map(|(lo, hi)| format!("; observed range [{lo:.3}, {hi:.3}] V"))
        .unwrap_or_default();
    if driven {
        format!("control net '{net}' was driven but never exceeded {level} V{range}")
    } else if drive_direction_observable {
        format!(
            "control net '{net}' was never driven to >= {level} V (firmware left it Hi-Z / undefined through the whole run){range}"
        )
    } else {
        format!(
            "control net '{net}' never reached {level} V{range}; the backend cannot report pin drive direction, so the pin may be undriven (Hi-Z / undefined) or driven LOW"
        )
    }
}

/// Full-run observed voltage range for `net`, read from the always-present
/// threshold-0 window the runner keeps. `None` if the net was never sampled.
fn boot_observed_range(net: &str, out: &RunOutcome) -> Option<(f64, f64)> {
    let w = out.windows.get(&(net.to_string(), 0.0_f64.to_bits()))?;
    (w.samples > 0).then_some((w.min_v, w.max_v))
}

/// Parse a hex byte string like "48 69" / "4869" / "0x48,0x69" into bytes.
fn parse_hex_bytes(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s
        .replace("0x", " ")
        .replace("0X", " ")
        .replace([',', ':'], " ");
    let toks: Vec<&str> = cleaned.split_whitespace().collect();
    if toks.len() > 1 && toks.iter().all(|t| t.len() <= 2 && !t.is_empty()) {
        toks.iter()
            .map(|t| u8::from_str_radix(t, 16).ok())
            .collect()
    } else {
        let h: String = cleaned.split_whitespace().collect();
        if h.len() % 2 != 0 || h.is_empty() {
            return None;
        }
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok())
            .collect()
    }
}

fn check_peripheral(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let id = a.id.clone().unwrap_or_default();
    // Unexercised-bus refusal (U3 finding 2): this peripheral was bound on a
    // platform that models no matching bus controller, so the firmware never
    // sent it a single transaction. Its snapshot is the slave's POWER-ON
    // DEFAULT state, asserting on that could green-pass (an LM75 default
    // temp is in-range for most specs), which is exactly the false green this
    // check exists to prevent. FAIL loudly before any field/bytes check.
    if out.unexercised_bus_ids.contains(&id) {
        return (
            false,
            format!(
                "peripheral '{id}' was bound but NEVER exercised: this MCU platform \
                 models no matching bus controller, so the firmware's bus traffic \
                 never reached it and its state is the power-on default. A pass \
                 here would vouch for a co-sim path that never ran; add the bus \
                 controller to the SoC descriptor ({}) or drop this \
                 assertion.",
                hauksbee_ir::docs_url("docs/cosim/MCU.md")
            ),
        );
    }
    let Some(snap) = out.peripherals.get(&id) else {
        return (false, format!("peripheral '{id}' not found in run"));
    };
    // SPI framing-tier flag (U3 finding 3): a heuristic-framed bus guesses
    // transaction boundaries at chunk edges (merges/truncates transactions),
    // so any verdict about this peripheral carries that caveat in its detail,
    // in the report itself, not a code comment.
    let framing_flag = match out.spi_framing.get(&id).map(String::as_str) {
        Some("heuristic") => {
            " [SPI framing: HEURISTIC; transaction boundaries guessed at chunk \
             edges; two transactions in one chunk merge and a boundary-spanning \
             one is truncated. Wire cs_net for exact framing]"
        }
        _ => "",
    };

    // Byte-contains check (EEPROM contents).
    if let Some(spec_bytes) = &a.bytes {
        let needle = parse_hex_bytes(spec_bytes).unwrap_or_else(|| spec_bytes.as_bytes().to_vec());
        let found = !needle.is_empty()
            && snap
                .bytes
                .windows(needle.len())
                .any(|w| w == needle.as_slice());
        let ascii = String::from_utf8_lossy(&needle);
        return (
            found,
            format!(
                "{id} memory {} bytes {spec_bytes} ({ascii:?}){framing_flag}",
                if found {
                    "contains"
                } else {
                    "does NOT contain"
                }
            ),
        );
    }

    // Field range check.
    if let Some(field) = &a.field {
        let Some(&v) = snap.fields.get(field) else {
            // Sort the known-field list: `snap.fields` is a HashMap, so its key
            // order varies run to run and would leak into the report/JUnit/GitHub
            // annotation bytes, making two identical runs differ. (Mirrors the
            // sorted-key fix on the UART concatenation below.)
            let mut known: Vec<&String> = snap.fields.keys().collect();
            known.sort();
            return (
                false,
                format!("{id} has no state field '{field}' (have: {known:?})"),
            );
        };
        let mut ok = true;
        let mut parts = Vec::new();
        if let Some(lo) = a.min {
            ok &= v >= lo - 1e-9;
            parts.push(format!(">= {lo}"));
        }
        if let Some(hi) = a.max {
            ok &= v <= hi + 1e-9;
            parts.push(format!("<= {hi}"));
        }
        return (
            ok,
            format!("{id}.{field} = {v} ({}){framing_flag}", parts.join(", ")),
        );
    }

    (false, format!("peripheral '{id}' assertion incomplete"))
}

fn check_voltage(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let thr = a.after_ms.unwrap_or(0.0);
    let Some(win) = out.windows.get(&(net.clone(), thr.to_bits())) else {
        return (
            false,
            format!("net '{net}' was never sampled (no window at {thr}ms)"),
            None,
        );
    };
    if win.samples == 0 {
        return (
            false,
            format!("net '{net}' had no samples after {thr}ms"),
            None,
        );
    }
    // For a >= bound we care about the worst (minimum) the rail dipped to in
    // the window; for a <= bound, the worst (maximum) it rose to. A failing
    // bound is MARKED in the detail and the passing one left un-annotated, so
    // a two-bound failure reads at a glance; the `why` names the observed
    // shortfall in volts, which is the number the fix has to close.
    let mut ok = true;
    let mut parts = Vec::new();
    let mut whys = Vec::new();
    if let Some(lo) = a.min {
        let worst = win.min_v;
        if worst >= lo - 1e-6 {
            parts.push(format!("min={worst:.3}V (>= {lo}V)"));
        } else {
            ok = false;
            parts.push(format!("min={worst:.3}V < required {lo}V <- FAILED HERE"));
            whys.push(if win.last_v >= lo - 1e-6 {
                format!(
                    "{net} dipped to {worst:.3} V, {:.3} V below your {lo} V floor, \
                     before settling back to {:.3} V",
                    lo - worst,
                    win.last_v
                )
            } else {
                format!(
                    "{net} settled {:.3} V below your floor ({:.3} V vs min {lo} V)",
                    lo - win.last_v,
                    win.last_v
                )
            });
        }
    }
    if let Some(hi) = a.max {
        let worst = win.max_v;
        if worst <= hi + 1e-6 {
            parts.push(format!("max={worst:.3}V (<= {hi}V)"));
        } else {
            ok = false;
            parts.push(format!("max={worst:.3}V > allowed {hi}V <- FAILED HERE"));
            whys.push(if win.last_v <= hi + 1e-6 {
                format!(
                    "{net} rose to {worst:.3} V, {:.3} V above your {hi} V ceiling, \
                     before settling back to {:.3} V",
                    worst - hi,
                    win.last_v
                )
            } else {
                format!(
                    "{net} settled {:.3} V above your ceiling ({:.3} V vs max {hi} V)",
                    win.last_v - hi,
                    win.last_v
                )
            });
        }
    }
    let when = if thr > 0.0 {
        format!(" after {thr}ms")
    } else {
        String::new()
    };
    (
        ok,
        format!(
            "{net}{when}: {} [settled {:.3}V]",
            parts.join(", "),
            win.last_v
        ),
        if whys.is_empty() {
            None
        } else {
            Some(whys.join("; "))
        },
    )
}

fn check_uart(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    // Concatenate the requested MCU's UART, or all MCUs if unspecified.
    let text: String = match &a.mcu {
        Some(m) => out.uart.get(m).cloned().unwrap_or_default(),
        None => {
            // Concatenate all MCUs in a STABLE (sorted-by-key) order, iterating
            // a HashMap's values put the streams in nondeterministic order, so an
            // anchored/boundary-spanning match (`^BOOT`) flaked run to run.
            let mut items: Vec<(&String, &String)> = out.uart.iter().collect();
            items.sort_by(|x, y| x.0.cmp(y.0));
            items
                .into_iter()
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
                .join("")
        }
    };
    let preview = text.replace(['\r', '\n'], "·");
    let preview = truncate(&preview, 60);

    if let Some(needle) = &a.contains {
        let ok = text.contains(needle);
        return (
            ok,
            format!(
                "UART={preview:?} {} {needle:?}",
                if ok { "contains" } else { "does NOT contain" }
            ),
        );
    }
    if let Some(re) = &a.matches {
        match regex::Regex::new(re) {
            Ok(rx) => {
                let ok = rx.is_match(&text);
                (
                    ok,
                    format!(
                        "UART={preview:?} {} /{re}/",
                        if ok { "matches" } else { "does NOT match" }
                    ),
                )
            }
            Err(e) => (false, format!("bad regex /{re}/: {e}")),
        }
    } else {
        (false, "uart assertion had no contains/matches".into())
    }
}

fn check_toggle(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let toggles = out.toggles.get(&net).copied().unwrap_or(0);

    if let Some(min) = a.min_toggles {
        let ok = toggles >= min;
        let why = (!ok).then(|| {
            format!(
                "{net} toggled {toggles} time(s) over the run, {} short of your {min} minimum",
                min - toggles
            )
        });
        return (ok, format!("{net}: {toggles} toggles (need >= {min})"), why);
    }
    if let Some(freq) = a.freq_hz {
        // A frequency needs a period, and a period needs at least one full
        // cycle observed: two toggles. Below that there is nothing to divide.
        // The arithmetic below happily produced "~0.50 Hz from 1 toggles" from a
        // single edge, a number that reads as a measurement and is not one: it
        // says only "something happened once", and whether the net is running at
        // 0.5 Hz or 50 Hz with a stalled firmware cannot be told apart from it.
        // Three toggles is the floor for a cycle plus the edge that confirms it
        // repeats, which is what makes a rate a rate.
        const MIN_TOGGLES_FOR_A_RATE: u64 = 3;
        if toggles < MIN_TOGGLES_FOR_A_RATE {
            let plural = if toggles == 1 { "" } else { "s" };
            return (
                false,
                format!(
                    "{net}: {toggles} toggle{plural} over the run, too few to measure a \
                     frequency from (need at least {MIN_TOGGLES_FOR_A_RATE})"
                ),
                Some(format!(
                    "a rate needs a repeated cycle, and {toggles} edge(s) is not one. \
                     Either the net is barely switching (check the firmware drives it) \
                     or the run is too short to contain a few periods of {freq} Hz: \
                     duration_ms must cover at least two, ideally several"
                )),
            );
        }
        // A full square-wave period is two toggles, so frequency = toggles /
        // (2 * duration_s). Compare against tolerance (fractional, default 25%).
        let dur_s = out.sim_ms / 1000.0;
        let measured = if dur_s > 0.0 {
            toggles as f64 / (2.0 * dur_s)
        } else {
            0.0
        };
        let tol = a.tolerance.unwrap_or(0.25);
        let lo = freq * (1.0 - tol);
        let hi = freq * (1.0 + tol);
        let ok = measured >= lo && measured <= hi;
        let why = (!ok).then(|| {
            format!(
                "{net} ran at ~{measured:.2} Hz, outside your {lo:.2}-{hi:.2} Hz band \
                 ({freq} Hz ±{:.0}%)",
                tol * 100.0
            )
        });
        return (
            ok,
            format!(
                "{net}: ~{measured:.2} Hz from {toggles} toggles (want {freq} Hz ±{:.0}%)",
                tol * 100.0
            ),
            why,
        );
    }
    (false, format!("{net}: toggle assertion incomplete"), None)
}

/// phase_margin: the loop's phase margin (degrees) at gain crossover must lie in
/// the requested bound. The loop gain is read at `net` from the shared AC sweep.
fn check_phase_margin(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let Some(ac) = &out.ac else {
        return (
            false,
            "no AC analysis ran (missing [ac] block)".into(),
            None,
        );
    };
    let Some(m) = ac.margins.get(&net) else {
        return (
            false,
            format!("net '{net}' produced no loop-stability margins"),
            None,
        );
    };
    let Some(pm) = m.phase_margin_deg else {
        return (
            false,
            format!(
                "loop '{net}' never crosses 0 dB in the swept band (no gain crossover; DC loop gain {:.1} dB)",
                m.dc_gain_db
            ),
            None,
        );
    };
    let fc = m.gain_crossover_hz.unwrap_or(f64::NAN);
    let mut ok = true;
    let mut parts = Vec::new();
    let mut whys = Vec::new();
    if let Some(lo) = a.min {
        if pm >= lo - 1e-6 {
            parts.push(format!(">= {lo}"));
        } else {
            ok = false;
            parts.push(format!("{pm:.2} deg < required {lo} deg <- FAILED HERE"));
            whys.push(format!(
                "the loop crossed 0 dB with {pm:.2} deg of phase in hand, {:.2} deg \
                 less than your {lo} deg floor",
                lo - pm
            ));
        }
    }
    if let Some(hi) = a.max {
        if pm <= hi + 1e-6 {
            parts.push(format!("<= {hi}"));
        } else {
            ok = false;
            parts.push(format!("{pm:.2} deg > allowed {hi} deg <- FAILED HERE"));
            whys.push(format!(
                "the loop's phase margin came out {:.2} deg above your {hi} deg ceiling",
                pm - hi
            ));
        }
    }
    (
        ok,
        format!(
            "loop {net}: phase margin {pm:.2} deg at fc={fc:.4} Hz ({})",
            parts.join(", ")
        ),
        if whys.is_empty() {
            None
        } else {
            Some(whys.join("; "))
        },
    )
}

/// ac_gain: the magnitude (dB) at `net` must lie in the requested bound, at the
/// frequency `freq_hz` (interpolated) or, if absent, over the whole sweep.
fn check_ac_gain(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let Some(ac) = &out.ac else {
        return (
            false,
            "no AC analysis ran (missing [ac] block)".into(),
            None,
        );
    };
    let Some(bode) = ac.bode.get(&net) else {
        return (
            false,
            format!("net '{net}' was not sampled by the AC sweep"),
            None,
        );
    };
    if bode.is_empty() {
        return (false, format!("net '{net}' has no AC data"), None);
    }

    // A requested frequency outside the swept band cannot be measured, interp_db
    // would silently clamp to the nearest endpoint gain and report it AS IF taken
    // at the requested frequency. Fail loudly instead of comparing an endpoint
    // gain against the bound. (R7 #13)
    if let Some(f) = a.freq_hz {
        // A non-finite freq_hz (a `nan`/`inf` TOML literal) slips past the band
        // bounds because every comparison against NaN is false, so the guard
        // below would be skipped and interp_db would clamp to the top-of-band
        // gain and report it as if measured "at NaN Hz". Refuse up front.
        if !f.is_finite() {
            return (
                false,
                format!("ac_gain for '{net}' has a non-finite freq_hz ({f}); set a real frequency"),
                None,
            );
        }
        let (f_lo, f_hi) = (bode[0].0, bode[bode.len() - 1].0);
        if f < f_lo * (1.0 - 1e-9) || f > f_hi * (1.0 + 1e-9) {
            return (
                false,
                format!(
                    "ac_gain for '{net}' requested {f} Hz is outside the swept band \
                     {f_lo}-{f_hi} Hz; widen [ac] fstart/fstop or move the check in-band"
                ),
                None,
            );
        }
    }

    // Pick the gain to test: at a specific frequency (log-interpolated) or the
    // worst case over the whole band.
    let (db, where_str) = if let Some(f) = a.freq_hz {
        (interp_db(bode, f), format!("at {f} Hz"))
    } else {
        // Worst case for the bound being checked.
        let min_db = bode.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_db = bode.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        // Report whichever extreme the bound cares about (default min).
        let v = if a.max.is_some() && a.min.is_none() {
            max_db
        } else {
            min_db
        };
        (v, "over band".to_string())
    };

    let mut ok = true;
    let mut parts = Vec::new();
    let mut whys = Vec::new();
    if let Some(lo) = a.min {
        let worst = if a.freq_hz.is_some() {
            db
        } else {
            bode.iter().map(|p| p.1).fold(f64::INFINITY, f64::min)
        };
        if worst >= lo - 1e-6 {
            parts.push(format!("min={worst:.3}dB (>= {lo})"));
        } else {
            ok = false;
            parts.push(format!("min={worst:.3}dB < required {lo}dB <- FAILED HERE"));
            whys.push(format!(
                "{net} measured {worst:.3} dB {where_str}, {:.3} dB below your {lo} dB floor",
                lo - worst
            ));
        }
    }
    if let Some(hi) = a.max {
        let worst = if a.freq_hz.is_some() {
            db
        } else {
            bode.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max)
        };
        if worst <= hi + 1e-6 {
            parts.push(format!("max={worst:.3}dB (<= {hi})"));
        } else {
            ok = false;
            parts.push(format!("max={worst:.3}dB > allowed {hi}dB <- FAILED HERE"));
            whys.push(format!(
                "{net} measured {worst:.3} dB {where_str}, {:.3} dB above your {hi} dB ceiling",
                worst - hi
            ));
        }
    }
    (
        ok,
        format!("{net} gain {where_str}: {}", parts.join(", ")),
        if whys.is_empty() {
            None
        } else {
            Some(whys.join("; "))
        },
    )
}

/// Linear interpolation (in log-frequency) of magnitude dB at frequency `f`.
fn interp_db(bode: &[(f64, f64, f64)], f: f64) -> f64 {
    if f <= bode[0].0 {
        return bode[0].1;
    }
    if f >= bode[bode.len() - 1].0 {
        return bode[bode.len() - 1].1;
    }
    for w in bode.windows(2) {
        let (f0, d0) = (w[0].0, w[0].1);
        let (f1, d1) = (w[1].0, w[1].1);
        if f >= f0 && f <= f1 {
            let frac = (f.log10() - f0.log10()) / (f1.log10() - f0.log10()).max(1e-30);
            return d0 + frac * (d1 - d0);
        }
    }
    bode[bode.len() - 1].1
}

fn check_no_faults(out: &RunOutcome) -> (bool, String) {
    if out.faults.is_empty() {
        (true, "no stress faults".into())
    } else {
        let f = &out.faults[0];
        (
            false,
            format!(
                "{} fault(s); first: {} {} {:.3} > {:.3} at {:.1}ms",
                out.faults.len(),
                f.component,
                f.kind,
                f.value,
                f.limit,
                f.t_ms
            ),
        )
    }
}

fn check_max_current(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let reference = a.reference.clone().unwrap_or_default();
    let limit = a.amps.unwrap_or(0.0);
    // Aggregate over the package's units, exactly like `check_max_temp`:
    // `peak_current` is keyed by the stamped device name, which for a multi-unit
    // package (a resistor array `RN1` stamps `RN1_e1..RN1_e4`) is the per-unit
    // form, never the bare `RN1` the assertion names. A bare exact `.get(&ref)`
    // would always miss and drop into the None branch, so a max_current safety
    // assert on any multi-unit package could NEVER pass. `check_trackable_
    // assert_refs` already greenlights a bare ref whose units are tracked, so
    // the consumer here MUST match those unit keys too (`key_belongs_to_ref`).
    let peak = out
        .peak_current
        .iter()
        .filter(|(k, _)| key_belongs_to_ref(&reference, k))
        // Highest-current entry; ties broken on the unit key so the reported
        // unit is stable across HashMap iteration order (reproducibility).
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, v)| (k.clone(), *v));
    match peak {
        Some((key, peak)) => {
            let ok = peak <= limit + 1e-9;
            let unit = if key != reference {
                format!(" (peak unit {key})")
            } else {
                String::new()
            };
            let peak_s = format_amps(peak);
            if ok {
                (
                    true,
                    format!("I({reference}) peak {peak_s} (<= {limit}A){unit}"),
                    None,
                )
            } else {
                (
                    false,
                    format!("I({reference}) peak {peak_s} > limit {limit}A <- FAILED HERE{unit}"),
                    Some(format!(
                        "{reference} drew {peak_s} at peak, {} over your {limit} A limit",
                        format_amps(peak - limit)
                    )),
                )
            }
        }
        None => (
            // No current data. The runner rejects a max_current on an untracked
            // component kind at bind time (`check_trackable_assert_refs`), so
            // reaching this branch means the tracked device never produced a
            // sample, fail loud rather than report a guard that was never
            // evaluated as green.
            false,
            format!(
                "I({reference}): no current data was recorded for this component; \
                 the guard was never evaluated, so it cannot be reported green"
            ),
            None,
        ),
    }
}

/// Does a per-device map key (or fault component name) belong to `reference`?
/// True for the bare ref itself and for any of its per-unit keys: a multi-unit
/// package stamps one device per unit with a `_q<N>` / `_s<N>` suffix
/// ("IC3906_q2", "SW1_s0"), so per-unit producers (`peak_temp_c`, faults) never
/// record the bare package ref. Mirrors the binder's suffix rule and the
/// runner's `thermally_tracked` gate; the gate accepts a bare ref whose units
/// are monitored, so the consumers here MUST match those unit keys too, or a
/// safety assert on a multi-unit package could never fail.
fn key_belongs_to_ref(reference: &str, key: &str) -> bool {
    key == reference
        || key.strip_prefix(reference).is_some_and(|s| {
            // `_q`/`_s`/`_e` are the binder's multi-unit suffixes (transistor,
            // switch, and resistor/passive arrays). `_e` was omitted, so a
            // package-level safety assert on a resistor array (`ref="RN1"`) could
            // never catch an overheating `RN1_e2` element.
            s.strip_prefix("_q")
                .or_else(|| s.strip_prefix("_s"))
                .or_else(|| s.strip_prefix("_e"))
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
}

/// max_temp: the steady-state junction temperature of `ref` must stay at or
/// below `celsius` (if given) or below the device's own max junction temp (if
/// not, in which case we lean on whether an overtemperature fault fired).
///
/// Both the peak lookup and the fault match aggregate over the package's units
/// (`key_belongs_to_ref`): `peak_temp_c` and the fault list are keyed by the
/// stamped device name, which for a multi-unit package is the per-unit
/// `SW1_q1` / `SW1_s0` form, never the bare `SW1` the assertion names.
fn check_max_temp(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let reference = a.reference.clone().unwrap_or_default();
    // Hottest matching entry: the bare ref for a single device, or the hottest
    // unit of a multi-unit package.
    let peak = out
        .peak_temp_c
        .iter()
        .filter(|(k, _)| key_belongs_to_ref(&reference, k))
        // Break temperature ties on the unit key so the reported hottest unit is
        // stable across HashMap iteration order (reproducibility doctrine): among
        // tied-max units the lowest key name wins deterministically.
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, v)| (k.clone(), *v));

    // Explicit ceiling: compare the peak junction temperature against it.
    if let Some(limit) = a.celsius {
        return match peak {
            Some((key, tj)) => {
                let ok = tj <= limit + 1e-6;
                let unit = if key != reference {
                    format!(" (hottest unit {key})")
                } else {
                    String::new()
                };
                if ok {
                    (
                        true,
                        format!("Tj({reference}) peak {tj:.1}C{unit} (<= {limit}C)"),
                        None,
                    )
                } else {
                    (
                        false,
                        format!(
                            "Tj({reference}) peak {tj:.1}C > ceiling {limit}C <- FAILED HERE{unit}"
                        ),
                        Some(format!(
                            "{reference} ran {:.1} C hotter than your {limit} C ceiling \
                             (peak {tj:.1} C{unit})",
                            tj - limit
                        )),
                    )
                }
            }
            None => {
                // No thermal data. The runner rejects a max_temp on a component
                // with no thermal model at bind time (`check_trackable_assert_refs`),
                // so here the part IS stress-monitored and simply never dissipated
                // measurably: its junction sat at AMBIENT. That is only a pass if
                // ambient itself is within the ceiling, in a hot-ambient spec a
                // ceiling below ambient is violated by the idle part sitting at
                // ambient, not silently skipped.
                if out.ambient_c <= limit + 1e-6 {
                    (
                        true,
                        format!(
                            "Tj({reference}): no dissipation measured (idle at ambient \
                             {:.1}C <= {limit}C); skipped",
                            out.ambient_c
                        ),
                        None,
                    )
                } else {
                    (
                        false,
                        format!(
                            "Tj({reference}): idle at ambient {:.1}C exceeds ceiling {limit}C \
                             (no dissipation, but ambient alone is over-limit)",
                            out.ambient_c
                        ),
                        Some(format!(
                            "{reference} sat idle at the {:.1} C ambient, {:.1} C over your \
                             {limit} C ceiling; the ceiling is below the spec's ambient_c",
                            out.ambient_c,
                            out.ambient_c - limit
                        )),
                    )
                }
            }
        };
    }

    // No explicit ceiling: pass unless an overtemperature fault fired for this
    // component or any of its units (the monitor compares Tj against the
    // device's own max Tj).
    let over = out
        .faults
        .iter()
        .find(|f| key_belongs_to_ref(&reference, &f.component) && f.kind == "overtemperature");
    match over {
        Some(f) => (
            false,
            format!(
                "Tj({}) exceeded device max: {:.1}C > {:.1}C at {:.1}ms",
                f.component, f.value, f.limit, f.t_ms
            ),
            Some(format!(
                "{} ran {:.1} C past its own {:.1} C max junction temperature at {:.1} ms",
                f.component,
                f.value - f.limit,
                f.limit,
                f.t_ms
            )),
        ),
        None => {
            // No sample at all: the junction was never estimated, so this
            // guard was never evaluated. Reporting it green would be the
            // same vacuous pass the load-time untracked-ref refusal exists to
            // prevent; fail with the reason instead (max_current's no-data
            // branch is the same discipline).
            let Some((_, tj)) = peak else {
                return (
                    false,
                    format!(
                        "Tj({reference}): no dissipation was ever measured, so its junction \
                         temperature was never estimated; the guard was never evaluated and \
                         cannot be reported green (give the assert an explicit `celsius`, \
                         or point it at a part this run actually stresses)"
                    ),
                    None,
                );
            };
            let detail = format!("Tj({reference}) peak {tj:.1}C, within device max");
            (true, detail, None)
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::{
        all_green_detail, boot_below_threshold_msg, check_model_coverage, check_protection_trip,
        key_belongs_to_ref,
    };
    use crate::tolerance::Mode;

    mod model_coverage {
        use super::check_model_coverage;
        use crate::runner::RunOutcome;
        use crate::spec::Assertion;
        use hauksbee_engine::result::{BindSummary, UnresolvedActive};

        /// A board where 3 of 4 active ICs bound and one unresolved part sits
        /// on a connected net: the shape a real partly-modelled board has.
        fn partly_bound() -> BindSummary {
            BindSummary {
                resolved: 40,
                unresolved: 10,
                non_ignored: 50,
                critical_parts_bound: "3/4".to_string(),
                critical_parts_bound_n: 3,
                critical_parts_total: 4,
                mcu_bound: true,
                active_path_unresolved: vec![UnresolvedActive {
                    reference: "U7".to_string(),
                    value: "LTC4020".to_string(),
                    reason: "no model".to_string(),
                    consequence: "open circuit".to_string(),
                    active_ic: true,
                }],
                resolved_but_open_active: vec![],
            }
        }

        fn assertion(f: impl FnOnce(&mut Assertion)) -> Assertion {
            let mut a = Assertion {
                kind: "model_coverage".to_string(),
                ..Default::default()
            };
            f(&mut a);
            a
        }

        fn outcome(bind: Option<BindSummary>) -> RunOutcome {
            RunOutcome {
                bind,
                ..Default::default()
            }
        }

        #[test]
        fn critical_floor_fails_when_an_active_ic_is_missing() {
            let (passed, detail) = check_model_coverage(
                &assertion(|a| a.min_critical = Some(0.9)),
                &outcome(Some(partly_bound())),
            );
            assert!(!passed, "3 of 4 active ICs is below a 90% floor: {detail}");
            assert!(
                detail.contains("3/4"),
                "detail must show the ratio: {detail}"
            );
        }

        #[test]
        fn critical_floor_passes_at_the_boundary() {
            let (passed, detail) = check_model_coverage(
                &assertion(|a| a.min_critical = Some(0.75)),
                &outcome(Some(partly_bound())),
            );
            assert!(passed, "3/4 meets a 75% floor exactly: {detail}");
        }

        #[test]
        fn unresolved_on_a_connected_net_is_named_not_just_counted() {
            let (passed, detail) = check_model_coverage(
                &assertion(|a| a.max_active_unresolved = Some(0)),
                &outcome(Some(partly_bound())),
            );
            assert!(!passed, "one unresolved part exceeds a limit of 0");
            assert!(
                detail.contains("U7"),
                "the part list is the user's next action: {detail}"
            );
        }

        #[test]
        fn an_empty_board_cannot_pass_a_critical_floor() {
            // 0 of 0 is not 100%. Reading it that way would let the assertion
            // vouch for a board it never looked at.
            let mut bind = partly_bound();
            bind.critical_parts_total = 0;
            bind.critical_parts_bound_n = 0;
            let (passed, detail) = check_model_coverage(
                &assertion(|a| a.min_critical = Some(0.9)),
                &outcome(Some(bind)),
            );
            assert!(
                !passed,
                "a board with no active ICs must not pass: {detail}"
            );
        }

        #[test]
        fn a_run_without_bind_data_fails_rather_than_passing_blind() {
            let (passed, detail) =
                check_model_coverage(&assertion(|a| a.min_critical = Some(0.5)), &outcome(None));
            assert!(
                !passed,
                "no bind data means no answer, not a green: {detail}"
            );
        }

        #[test]
        fn every_threshold_must_hold_not_just_one() {
            // min_resolved passes (40/50 = 80%) while min_critical fails, so the
            // assertion as a whole must fail.
            let (passed, detail) = check_model_coverage(
                &assertion(|a| {
                    a.min_resolved = Some(0.75);
                    a.min_critical = Some(0.99);
                }),
                &outcome(Some(partly_bound())),
            );
            assert!(
                !passed,
                "one failing threshold fails the assertion: {detail}"
            );
        }
    }

    #[test]
    fn protection_trip_empty_scenario_is_run_wide_like_unset() {
        // Round-29: Spec::validate documents an explicit scenario = "" as identical
        // to unset (the run-wide window). check_protection_trip matched the raw
        // Option, so Some("") took the scoped branch and missed the ("", net) key
        // that only rail_window/declared-scenario windows populate, yielding a false
        // RED. It must take the run-wide branch, matching the omitted-scenario form.
        use crate::runner::RunOutcome;
        use std::collections::HashMap;
        fn outcome_batt_never_tripped() -> RunOutcome {
            let mut protection_tripped = HashMap::new();
            protection_tripped.insert("BATT".to_string(), false);
            RunOutcome {
                bind: None,
                evidence: None,
                seed: 0,
                windows: HashMap::new(),
                uart: HashMap::new(),
                faults: Vec::new(),
                toggles: HashMap::new(),
                peak_current: HashMap::new(),
                peak_temp_c: HashMap::new(),
                peripherals: HashMap::new(),
                rail_windows: HashMap::new(),
                protection_tripped,
                protection_tripped_scoped: HashMap::new(),
                ambient_c: 25.0,
                sim_ms: 100.0,
                boot_first_cross_ms: HashMap::new(),
                boot_drop_after_cross_ms: HashMap::new(),
                driven_nets: Default::default(),
                drive_direction_observable: false,
                first_fault_ms: None,
                ac: None,
                analog_valid: true,
                failed_windows: Vec::new(),
                fallback_windows: Vec::new(),
                error_budget: None,
                analog_abort: false,
                sampled_values: Vec::new(),
                interior: false,
                net_series: HashMap::new(),
                substitutions: Vec::new(),
                coverage_warnings: Vec::new(),
                timing_coverage: Vec::new(),
                timing_refusals: Vec::new(),
                dead_rails: Vec::new(),
                unexercised_bus_ids: std::collections::HashSet::new(),
                spi_framing: HashMap::new(),
            }
        }
        let out = outcome_batt_never_tripped();
        let parse = |scope_line: &str| -> crate::spec::Assertion {
            toml::from_str(&format!(
                "kind = \"protection_trip\"\nsupply_net = \"BATT\"\nexpect_trip = false\n{scope_line}"
            ))
            .unwrap()
        };
        // Omitted scenario: reads the run-wide flag, passes (BATT never tripped).
        let (ok_unset, _) = check_protection_trip(&parse(""), &out);
        assert!(ok_unset, "unset scenario reads the run-wide no-trip flag");
        // Explicit "" must behave identically, not a scoped-miss false RED.
        let (ok_empty, msg) = check_protection_trip(&parse("scenario = \"\"\n"), &out);
        assert!(
            ok_empty,
            "explicit empty scenario must equal unset, got: {msg}"
        );
    }

    /// The non-monotonic case, which is the whole reason the interior probes
    /// exist: a VOUT that stays in band at both tolerance extremes and sags out
    /// of it somewhere in between (a regulator dropping out at an interior load,
    /// a resonance, a threshold crossed mid-range). Before the probes ran, this
    /// board reported green with a "bounds the worst case" banner over it.
    ///
    /// Two things must happen: the assertion must FAIL, and it must say the
    /// corners were the wrong place to look, because a reader's reflex on a
    /// corner-mode red is to inspect the named corner values.
    #[test]
    fn an_interior_probe_that_beats_every_corner_escalates_to_a_failure() {
        let a: crate::spec::Assertion =
            toml::from_str("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.0\n").unwrap();

        // Corners 0 and 1 in band; the interior probe sags to 2.4 V.
        let outcomes = vec![
            vout_member(0, 3.30, false),
            vout_member(1, 3.28, false),
            vout_member(2, 2.40, true),
        ];
        let r = super::evaluate_one(&a, &outcomes, Some(crate::tolerance::Mode::Corners));

        assert!(!r.passed, "an interior failure must fail: {}", r.detail);
        assert_eq!(r.failing_seed, Some(2));
        assert!(
            r.detail.contains("NON-MONOTONIC"),
            "the failure must name the disproved assumption: {}",
            r.detail
        );
        assert!(
            r.detail.contains("interior probe 2"),
            "an interior member must not be labelled a corner: {}",
            r.detail
        );
        assert!(
            r.detail
                .contains("corner-only run would have reported green"),
            "say what a corner-only run would have done: {}",
            r.detail
        );
    }

    /// The other side: a monotonic response must NOT be escalated. The probes
    /// find nothing, the assertion still passes, and the disclosure narrows from
    /// an unchecked assumption to a search that came back empty, without
    /// claiming proof.
    #[test]
    fn a_monotonic_response_still_passes_and_narrows_the_disclosure() {
        let a: crate::spec::Assertion =
            toml::from_str("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.0\n").unwrap();

        // Two corners bracketing three interior points, all in band and ordered:
        // the response is monotonic in the swept value.
        let outcomes = vec![
            vout_member(0, 3.10, false),
            vout_member(1, 3.50, false),
            vout_member(2, 3.20, true),
            vout_member(3, 3.30, true),
            vout_member(4, 3.40, true),
        ];
        let r = super::evaluate_one(&a, &outcomes, Some(crate::tolerance::Mode::Corners));

        assert!(r.passed, "a monotonic response must pass: {}", r.detail);
        assert!(
            !r.detail.contains("NON-MONOTONIC"),
            "nothing to escalate: {}",
            r.detail
        );
        assert!(
            r.detail.contains("2 min/max tolerance corners")
                && r.detail.contains("3 interior Latin-hypercube probe(s)"),
            "the corners and the probes are counted separately: {}",
            r.detail
        );
        assert!(
            r.detail.contains("found no non-monotonic response"),
            "the disclosure reports the search, not the assumption: {}",
            r.detail
        );
        assert!(
            r.detail
                .contains("unless a non-monotonicity sits between the probes"),
            "sampling is not proof, and the wording must still say so: {}",
            r.detail
        );
    }

    /// A corner failure keeps its old wording. The interior probes must not
    /// relabel an ordinary corner red as a monotonicity discovery.
    #[test]
    fn a_corner_failure_is_not_reported_as_non_monotonic() {
        let a: crate::spec::Assertion =
            toml::from_str("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.0\n").unwrap();
        let outcomes = vec![
            vout_member(0, 2.50, false),
            vout_member(1, 3.30, false),
            vout_member(2, 3.20, true),
        ];
        let r = super::evaluate_one(&a, &outcomes, Some(crate::tolerance::Mode::Corners));
        assert!(!r.passed);
        assert!(
            !r.detail.contains("NON-MONOTONIC"),
            "the corners DID find it: {}",
            r.detail
        );
        assert!(
            r.detail.contains("corner 0"),
            "the failing corner is named as a corner: {}",
            r.detail
        );
    }

    /// One ensemble member holding `VOUT` flat at `v` over the whole run.
    fn vout_member(seed: u32, v: f64, interior: bool) -> crate::runner::RunOutcome {
        let mut windows = std::collections::HashMap::new();
        windows.insert(
            ("VOUT".to_string(), 0.0f64.to_bits()),
            crate::runner::NetWindow {
                min_v: v,
                max_v: v,
                last_v: v,
                samples: 10,
            },
        );
        crate::runner::RunOutcome {
            seed,
            windows,
            interior,
            ambient_c: 25.0,
            sim_ms: 100.0,
            analog_valid: true,
            ..Default::default()
        }
    }

    #[test]
    fn corner_mode_invalid_detail_labels_the_member_a_corner_not_a_seed() {
        // Round-30: in corners mode a member index is a corner number (a specific
        // min/max combination), not a random fuzz seed. evaluate_one's INVALID
        // branch hardcoded "seed {n}:" instead of the mode-aware `member`, so a
        // held-stale corner was mislabeled, an EE told to re-run "seed 3" when the
        // coverage banner and every other line call it "corner 3". The prefix must
        // track the mode, matching the FAIL path.
        use crate::runner::RunOutcome;
        use std::collections::HashMap;
        fn outcome(seed: u32, failed: Vec<(f64, f64)>) -> RunOutcome {
            // A clean, in-band VOUT window so a converged corner PASSES this
            // assertion. (R50: FAIL now beats INVALID, so the converged member must
            // not itself fail; the INVALID must come purely from the diverged
            // corner's held-stale window, which is what this test exercises.)
            let mut windows = HashMap::new();
            windows.insert(
                ("VOUT".to_string(), 0.0f64.to_bits()),
                crate::runner::NetWindow {
                    min_v: 3.3,
                    max_v: 3.35,
                    last_v: 3.32,
                    samples: 10,
                },
            );
            RunOutcome {
                bind: None,
                evidence: None,
                seed,
                windows,
                uart: HashMap::new(),
                faults: Vec::new(),
                toggles: HashMap::new(),
                peak_current: HashMap::new(),
                peak_temp_c: HashMap::new(),
                peripherals: HashMap::new(),
                rail_windows: HashMap::new(),
                protection_tripped: HashMap::new(),
                protection_tripped_scoped: HashMap::new(),
                ambient_c: 25.0,
                sim_ms: 100.0,
                boot_first_cross_ms: HashMap::new(),
                boot_drop_after_cross_ms: HashMap::new(),
                driven_nets: Default::default(),
                drive_direction_observable: false,
                first_fault_ms: None,
                ac: None,
                analog_valid: failed.is_empty(),
                failed_windows: failed,
                fallback_windows: Vec::new(),
                error_budget: None,
                analog_abort: false,
                sampled_values: Vec::new(),
                interior: false,
                net_series: HashMap::new(),
                substitutions: Vec::new(),
                coverage_warnings: Vec::new(),
                timing_coverage: Vec::new(),
                timing_refusals: Vec::new(),
                dead_rails: Vec::new(),
                unexercised_bus_ids: std::collections::HashSet::new(),
                spi_framing: HashMap::new(),
            }
        }
        // A voltage assertion reads the analog window (0..sim). Corner 3 diverged.
        let a: crate::spec::Assertion =
            toml::from_str("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.2\nmax = 3.4\n").unwrap();
        let outcomes = vec![outcome(0, Vec::new()), outcome(3, vec![(0.0012, 0.0034)])];
        let res = super::evaluate_one(&a, &outcomes, Some(Mode::Corners));
        assert!(res.invalid, "the held-stale corner must be INVALID");
        assert!(
            res.detail.contains("corner 3:"),
            "the INVALID member must be labeled a corner: {}",
            res.detail
        );
        assert!(
            !res.detail.contains("seed 3:"),
            "corner-mode member must not be called a seed: {}",
            res.detail
        );
    }

    #[test]
    fn montecarlo_detail_excludes_the_nominal_from_the_sampled_count() {
        // Round-27: the all-green MonteCarlo detail said "passed {n}/{n} sampled
        // tolerance seeds" with n = total members, counting the nominal baseline
        // (member 0, which draws no random sample) as a sampled seed. It must
        // report n-1 sampled seeds, agreeing with the run banner, rather than
        // over-claiming statistical coverage by one draw.
        let d = all_green_detail(16, 0, Some(Mode::MonteCarlo), "voltage in range".into());
        assert!(
            d.contains("15 sampled tolerance seed(s) + nominal"),
            "16 members => 15 sampled seeds, not 16: {d}"
        );
        assert!(
            !d.contains("16/16"),
            "must not label the nominal a sampled seed: {d}"
        );
        // A single-member run (no ensemble) keeps the bare detail unchanged.
        assert_eq!(
            all_green_detail(1, 0, Some(Mode::MonteCarlo), "ok".into()),
            "ok"
        );
    }

    // The base-ref vs per-unit-key rule: a package ref owns itself and its
    // `_q<N>` / `_s<N>` unit keys, and nothing else (no SW1/SW10 prefix bleed,
    // no arbitrary underscore suffixes).
    #[test]
    fn unit_key_matching_is_exact_or_unit_suffixed() {
        assert!(key_belongs_to_ref("SW1", "SW1"));
        assert!(key_belongs_to_ref("SW1", "SW1_q1"));
        assert!(key_belongs_to_ref("SW1", "SW1_q12"));
        assert!(key_belongs_to_ref("SW1", "SW1_s0"));
        // R44: `_e` is the binder's suffix for resistor/passive ARRAY units (RN1_e1),
        // and it was omitted, so a package-level safety assert on an array's bare
        // ref could never match its overheating element.
        assert!(key_belongs_to_ref("RN1", "RN1_e1"));
        assert!(key_belongs_to_ref("RN1", "RN1_e12"));
        assert!(!key_belongs_to_ref("RN1", "RN1_e1a"));
        assert!(!key_belongs_to_ref("RN1", "RN10_e1"));
        assert!(!key_belongs_to_ref("SW1", "SW10"));
        assert!(!key_belongs_to_ref("SW1", "SW10_q1"));
        assert!(!key_belongs_to_ref("SW1", "SW1_heater"));
        assert!(!key_belongs_to_ref("SW1", "SW1_q1a"));
        assert!(!key_belongs_to_ref("SW1", "SW2_q1"));
    }

    // R14: a `peripheral` assertion naming a missing field lists the known
    // fields in the FAIL detail. That list must be sorted, not in HashMap
    // iteration order, or two identical runs emit different report/JUnit bytes.
    #[test]
    fn missing_peripheral_field_lists_known_fields_sorted() {
        use super::check_peripheral;
        use crate::runner::{PeripheralSnapshot, RunOutcome};
        use crate::spec::Assertion;

        let assertion: Assertion = toml::from_str(
            "kind = \"peripheral\"\nid = \"HTR1\"\nfield = \"nonesuch\"\nmin = 0.0\n",
        )
        .unwrap();

        // A snapshot with several fields inserted in non-alphabetical order.
        let mut snap = PeripheralSnapshot::default();
        for (k, v) in [
            ("transitions", 3.0),
            ("temp_c", 42.0),
            ("position", 1.0),
            ("duty", 0.5),
        ] {
            snap.fields.insert(k.to_string(), v);
        }
        let mut out = RunOutcome::default();
        out.peripherals.insert("HTR1".to_string(), snap);

        let (ok, msg) = check_peripheral(&assertion, &out);
        assert!(!ok, "a missing field must fail");
        // The formatted list must be the sorted order, deterministically.
        assert!(
            msg.contains("[\"duty\", \"position\", \"temp_c\", \"transitions\"]"),
            "known-field list must be sorted for reproducible report bytes: {msg}"
        );
    }

    // U3 finding 2: a `peripheral` assertion against a bus device the platform
    // never exercised (no matching controller modeled) must FAIL loudly; the
    // snapshot is the slave's power-on default, and a default LM75 temperature
    // sits inside most spec windows, so evaluating it would be a false green.
    #[test]
    fn peripheral_assertion_on_an_unexercised_bus_fails_loudly() {
        use super::check_peripheral;
        use crate::runner::{PeripheralSnapshot, RunOutcome};
        use crate::spec::Assertion;

        let assertion: Assertion = toml::from_str(
            "kind = \"peripheral\"\nid = \"TEMP1\"\nfield = \"temp_c\"\nmin = 20.0\nmax = 30.0\n",
        )
        .unwrap();

        // The slave's default state WOULD pass the window, that is the trap.
        let mut snap = PeripheralSnapshot::default();
        snap.fields.insert("temp_c".to_string(), 25.0);
        let mut out = RunOutcome::default();
        out.peripherals.insert("TEMP1".to_string(), snap);

        // Exercised bus: passes normally.
        let (ok, _) = check_peripheral(&assertion, &out);
        assert!(ok, "sanity: the default state passes when the bus ran");

        // Unexercised bus: the same snapshot must now FAIL with the honest
        // never-exercised wording, not green-pass on the default.
        out.unexercised_bus_ids.insert("TEMP1".to_string());
        let (ok, msg) = check_peripheral(&assertion, &out);
        assert!(!ok, "an unexercised bus peripheral must fail: {msg}");
        assert!(
            msg.contains("NEVER exercised") && msg.contains("power-on default"),
            "the failure must say WHY the verdict is refused: {msg}"
        );
    }

    // U3 finding 3: a heuristic-framed SPI bus must be flagged in the
    // assertion's own detail line (the surface a reviewer reads), pass or fail.
    #[test]
    fn peripheral_assertion_on_a_heuristic_framed_bus_is_flagged() {
        use super::check_peripheral;
        use crate::runner::{PeripheralSnapshot, RunOutcome};
        use crate::spec::Assertion;

        let assertion: Assertion = toml::from_str(
            "kind = \"peripheral\"\nid = \"ADC1\"\nfield = \"transitions\"\nmin = 1.0\n",
        )
        .unwrap();
        let mut snap = PeripheralSnapshot::default();
        snap.fields.insert("transitions".to_string(), 5.0);
        let mut out = RunOutcome::default();
        out.peripherals.insert("ADC1".to_string(), snap);

        // Exact framing: no flag.
        out.spi_framing
            .insert("ADC1".to_string(), "exact".to_string());
        let (ok, msg) = check_peripheral(&assertion, &out);
        assert!(ok);
        assert!(
            !msg.contains("HEURISTIC"),
            "exact framing must not be flagged: {msg}"
        );

        // Heuristic framing: the detail carries the caveat even on a pass.
        out.spi_framing
            .insert("ADC1".to_string(), "heuristic".to_string());
        let (ok, msg) = check_peripheral(&assertion, &out);
        assert!(ok, "framing tier qualifies, it does not fail: {msg}");
        assert!(
            msg.contains("HEURISTIC") && msg.contains("cs_net"),
            "a heuristic-framed assertion must be flagged in its detail: {msg}"
        );
    }

    // A boot-coverage net that the firmware actively drove but that never crossed
    // the threshold must NOT be reported as Hi-Z / undefined: it was driven.
    #[test]
    fn driven_but_below_threshold_says_driven_not_hi_z() {
        let m = boot_below_threshold_msg("FLAG", 2.3, true, true, Some((0.0, 0.4)));
        assert!(
            m.contains("was driven but never exceeded 2.3 V"),
            "got: {m}"
        );
        assert!(m.contains("observed range [0.000, 0.400] V"), "got: {m}");
        assert!(!m.contains("Hi-Z"), "a driven pin is not Hi-Z, got: {m}");
    }

    // Boot coverage must (1) pass a net that reaches its level by the deadline
    // and holds THROUGH the deadline even if it is later released, (2) fail a net
    // that reached but fell back before the deadline, with a message distinct
    // from (3) a net that never reached at all. An end-of-run latch alone
    // conflates (2) and (3) and wrongly fails (1).
    #[test]
    fn boot_coverage_honours_deadline_hold_and_distinguishes_drop_from_never() {
        use super::check_boot_coverage;
        use crate::runner::RunOutcome;
        use crate::spec::Assertion;

        let net = "EN".to_string();
        let level = 3.0_f64;
        let key = (net.clone(), level.to_bits());
        let assertion: Assertion = toml::from_str(
            "kind = \"boot-coverage\"\nnet = \"EN\"\nmin = 3.0\ndeadline_ms = 10.0\n",
        )
        .unwrap();

        // (1) reached at 5 ms, held through the 10 ms deadline, released at 50 ms.
        // The 100 ms sim covers the whole boot window.
        let mut out = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        out.boot_first_cross_ms.insert(key.clone(), 5.0);
        out.boot_drop_after_cross_ms.insert(key.clone(), 50.0);
        let (ok, msg) = check_boot_coverage(&assertion, &out);
        assert!(ok, "held-through-deadline then released must pass: {msg}");

        // (2) reached at 5 ms but fell back at 8 ms, before the deadline.
        let mut out = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        out.boot_first_cross_ms.insert(key.clone(), 5.0);
        out.boot_drop_after_cross_ms.insert(key.clone(), 8.0);
        let (ok, msg) = check_boot_coverage(&assertion, &out);
        assert!(!ok, "a drop before the deadline must fail");
        assert!(
            msg.contains("fell back below"),
            "distinct dropped message: {msg}"
        );

        // (3) never reached: fails with the below-threshold diagnosis, not a drop.
        let out = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        let (ok, msg) = check_boot_coverage(&assertion, &out);
        assert!(!ok, "never-reached must fail");
        assert!(
            !msg.contains("fell back below"),
            "never-reached must not read as a reached-then-dropped: {msg}"
        );
    }

    // E32: hold_ms makes boot_coverage decidable on heartbeat / toggling nets,
    // where hold-to-the-deadline can never pass. Two-sided on a toggling net
    // (reached at 2 ms, dropped at 7 ms, i.e. a 5 ms high phase): hold_ms = 0
    // passes (reach only), hold_ms longer than the high phase fails with the
    // hold shortfall named. A solid net (never drops) passes both, and an
    // unobserved hold tail fails as unconfirmed instead of passing on hope.
    #[test]
    fn boot_coverage_hold_ms_is_two_sided_on_toggling_and_solid_nets() {
        use super::check_boot_coverage;
        use crate::runner::RunOutcome;
        use crate::spec::Assertion;

        let key = ("HB".to_string(), 3.0_f64.to_bits());
        let assertion = |hold: &str| -> Assertion {
            toml::from_str(&format!(
                "kind = \"boot_coverage\"\nnet = \"HB\"\nmin = 3.0\ndeadline_ms = 10.0\n{hold}"
            ))
            .unwrap()
        };

        // A toggling net: first reach 2 ms, first drop 7 ms (5 ms high phase).
        let mut toggling = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        toggling.boot_first_cross_ms.insert(key.clone(), 2.0);
        toggling.boot_drop_after_cross_ms.insert(key.clone(), 7.0);

        let (ok, msg) = check_boot_coverage(&assertion("hold_ms = 0.0"), &toggling);
        assert!(
            ok,
            "hold_ms = 0 must pass a toggling net that reached: {msg}"
        );
        assert!(msg.contains("reach only"), "the pass names its form: {msg}");

        let (ok, msg) = check_boot_coverage(&assertion("hold_ms = 8.0"), &toggling);
        assert!(
            !ok,
            "a hold longer than the high phase must fail the toggling net"
        );
        assert!(
            msg.contains("held it only 5.00 ms") && msg.contains("hold_ms = 8"),
            "the failure names the observed hold and the requirement: {msg}"
        );

        // The strict default (absent hold_ms) also fails it: dropped at 7 ms,
        // before the 10 ms deadline.
        let (ok, _msg) = check_boot_coverage(&assertion(""), &toggling);
        assert!(
            !ok,
            "the hold-to-deadline default still fails a mid-window drop"
        );

        // A solid net: reached at 2 ms, never dropped, sim covers everything.
        let mut solid = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        solid.boot_first_cross_ms.insert(key.clone(), 2.0);

        let (ok, msg) = check_boot_coverage(&assertion("hold_ms = 0.0"), &solid);
        assert!(ok, "a solid net passes hold_ms = 0: {msg}");
        let (ok, msg) = check_boot_coverage(&assertion("hold_ms = 8.0"), &solid);
        assert!(ok, "a solid net passes hold_ms = 8: {msg}");
        assert!(msg.contains("held 8 ms"), "the pass names the hold: {msg}");

        // An unobserved hold tail must fail loud: reached at 2 ms, sim ended at
        // 6 ms, so an 8 ms hold window was never fully watched.
        let mut short = RunOutcome {
            sim_ms: 6.0,
            ..Default::default()
        };
        short.boot_first_cross_ms.insert(key.clone(), 2.0);
        let (ok, msg) = check_boot_coverage(&assertion("hold_ms = 8.0"), &short);
        assert!(!ok, "an unobserved hold window must not pass: {msg}");
        assert!(
            msg.contains("cannot be confirmed"),
            "it names the sim-too-short cause: {msg}"
        );
    }

    #[test]
    fn boot_coverage_deadline_past_sim_end_is_not_a_false_green() {
        // R34: the run stops at duration_ms, so RunOutcome only carries data out
        // to sim_ms. A first-cross before a deadline that lands PAST sim_ms left
        // drop_after = None (a later drop could never be observed), and the old
        // code returned GREEN "boot window clean", asserting coverage over an
        // unsimulated tail. The deadline outrunning the sim must FAIL, not pass.
        use super::check_boot_coverage;
        use crate::runner::RunOutcome;
        use crate::spec::Assertion;

        let net = "EN".to_string();
        let level = 3.0_f64;
        let key = (net.clone(), level.to_bits());
        // deadline 50 ms, but the firmware only ran 20 ms.
        let assertion: Assertion = toml::from_str(
            "kind = \"boot-coverage\"\nnet = \"EN\"\nmin = 3.0\ndeadline_ms = 50.0\n",
        )
        .unwrap();

        let mut out = RunOutcome {
            sim_ms: 20.0,
            ..Default::default()
        };
        out.boot_first_cross_ms.insert(key.clone(), 5.0); // crossed early, never dropped in-window
        let (ok, msg) = check_boot_coverage(&assertion, &out);
        assert!(
            !ok,
            "deadline past the sim end must not pass on an unobserved window: {msg}"
        );
        assert!(
            msg.contains("past the end") && msg.contains("cannot be confirmed"),
            "must name the sim-too-short cause, not a spurious clean pass: {msg}"
        );

        // Control: the same crossing with a deadline INSIDE the sim still passes.
        let inside: Assertion = toml::from_str(
            "kind = \"boot-coverage\"\nnet = \"EN\"\nmin = 3.0\ndeadline_ms = 10.0\n",
        )
        .unwrap();
        let (ok, _msg) = check_boot_coverage(&inside, &out);
        assert!(ok, "a deadline within the simulated window still passes");
    }

    // A genuinely undriven net on a backend that can report drive direction
    // (AVR DDR, or a dir-mapped Renode part) keeps the honest Hi-Z / undefined
    // wording.
    #[test]
    fn undriven_on_observable_backend_says_hi_z() {
        let m = boot_below_threshold_msg("FLAG", 2.3, false, true, Some((0.0, 0.0)));
        assert!(m.contains("Hi-Z / undefined"), "got: {m}");
        assert!(m.contains("never driven"), "got: {m}");
    }

    // On a backend that cannot report drive direction (QEMU, or a Renode part
    // whose descriptor has no dir map), absence of a drive record is ambiguous,
    // so the message must not assert Hi-Z: it names both possibilities instead.
    #[test]
    fn unknown_drive_direction_does_not_assert_hi_z() {
        let m = boot_below_threshold_msg("FLAG", 2.3, false, false, Some((0.0, 0.4)));
        assert!(m.contains("cannot report pin drive direction"), "got: {m}");
        assert!(m.contains("undriven"), "got: {m}");
        assert!(m.contains("driven LOW"), "got: {m}");
        assert!(
            !m.contains("firmware left it Hi-Z"),
            "must not assert Hi-Z, got: {m}"
        );
    }

    // A scenario window shorter than one frame yields a single rail sample; a
    // dip/recovery duration cannot be measured from one point, so the check
    // must FAIL loudly rather than silently report 0 ms and auto-pass
    // (round-4 #16).
    #[test]
    fn rail_window_dip_with_one_sample_does_not_auto_pass() {
        use super::check_rail_window;
        use crate::runner::RunOutcome;
        use crate::scenarios::RailWindow;
        use std::collections::HashMap;

        // Build a RunOutcome carrying only one (scenario, net) rail window.
        fn outcome_with_window(win: RailWindow) -> RunOutcome {
            let mut rail_windows = HashMap::new();
            rail_windows.insert(("load".to_string(), "VBUS".to_string()), win);
            RunOutcome {
                bind: None,
                evidence: None,
                seed: 0,
                windows: HashMap::new(),
                uart: HashMap::new(),
                faults: Vec::new(),
                toggles: HashMap::new(),
                peak_current: HashMap::new(),
                peak_temp_c: HashMap::new(),
                peripherals: HashMap::new(),
                rail_windows,
                protection_tripped: HashMap::new(),
                protection_tripped_scoped: HashMap::new(),
                ambient_c: 25.0,
                sim_ms: 100.0,
                boot_first_cross_ms: HashMap::new(),
                boot_drop_after_cross_ms: HashMap::new(),
                driven_nets: Default::default(),
                drive_direction_observable: false,
                first_fault_ms: None,
                ac: None,
                analog_valid: true,
                failed_windows: Vec::new(),
                fallback_windows: Vec::new(),
                error_budget: None,
                analog_abort: false,
                sampled_values: Vec::new(),
                interior: false,
                net_series: HashMap::new(),
                substitutions: Vec::new(),
                coverage_warnings: Vec::new(),
                timing_coverage: Vec::new(),
                timing_refusals: Vec::new(),
                dead_rails: Vec::new(),
                unexercised_bus_ids: std::collections::HashSet::new(),
                spi_framing: HashMap::new(),
            }
        }

        let a: crate::spec::Assertion = toml::from_str(
            "kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\n\
             dip_below = 3.0\nfor_max_ms = 1.0\n",
        )
        .unwrap();

        // One sample, sitting BELOW the dip threshold. A duration measured from
        // consecutive sample pairs is 0 ms here (windows(2) is empty), which
        // would auto-pass a rail that is in fact under the threshold.
        let mut win = RailWindow::new();
        win.observe(0.099, 2.5);
        let (ok, msg, _why) = check_rail_window(&a, &outcome_with_window(win));
        assert!(
            !ok,
            "a 1-sample dip window must not auto-pass; got pass: {msg}"
        );
        assert!(
            msg.contains("sample"),
            "message should explain the degenerate window: {msg}"
        );

        // Sanity: a proper 2-sample window that never dips below 3 V passes.
        let mut good = RailWindow::new();
        good.observe(0.000, 5.0);
        good.observe(0.010, 5.0);
        let (ok2, msg2, _why) = check_rail_window(&a, &outcome_with_window(good));
        assert!(ok2, "a 2-sample window that never dips must pass: {msg2}");
    }

    // R24: on a multi-unit package with TIED max temperatures, the reported
    // hottest unit must be deterministic (lowest key), not whatever HashMap
    // iteration order happened to surface, or two identical runs emit different
    // report bytes (reproducibility doctrine). Verdict is unaffected.
    #[test]
    fn max_temp_tie_break_is_deterministic() {
        use super::check_max_temp;
        use crate::runner::RunOutcome;
        use std::collections::HashMap;

        fn outcome_with_tied_temps() -> RunOutcome {
            let mut peak_temp_c = HashMap::new();
            // Two units of SW1 at the SAME peak temperature.
            peak_temp_c.insert("SW1_q2".to_string(), 100.0);
            peak_temp_c.insert("SW1_q1".to_string(), 100.0);
            peak_temp_c.insert("SW1_q3".to_string(), 100.0);
            RunOutcome {
                bind: None,
                evidence: None,
                seed: 0,
                windows: HashMap::new(),
                uart: HashMap::new(),
                faults: Vec::new(),
                toggles: HashMap::new(),
                peak_current: HashMap::new(),
                peak_temp_c,
                peripherals: HashMap::new(),
                rail_windows: HashMap::new(),
                protection_tripped: HashMap::new(),
                protection_tripped_scoped: HashMap::new(),
                ambient_c: 25.0,
                sim_ms: 100.0,
                boot_first_cross_ms: HashMap::new(),
                boot_drop_after_cross_ms: HashMap::new(),
                driven_nets: Default::default(),
                drive_direction_observable: false,
                first_fault_ms: None,
                ac: None,
                analog_valid: true,
                failed_windows: Vec::new(),
                fallback_windows: Vec::new(),
                error_budget: None,
                analog_abort: false,
                sampled_values: Vec::new(),
                interior: false,
                net_series: HashMap::new(),
                substitutions: Vec::new(),
                coverage_warnings: Vec::new(),
                timing_coverage: Vec::new(),
                timing_refusals: Vec::new(),
                dead_rails: Vec::new(),
                unexercised_bus_ids: std::collections::HashSet::new(),
                spi_framing: HashMap::new(),
            }
        }

        let a: crate::spec::Assertion =
            toml::from_str("kind = \"max_temp\"\nref = \"SW1\"\ncelsius = 150.0\n").unwrap();
        // Many independent builds must all name the SAME (lowest-key) unit.
        for _ in 0..16 {
            let (ok, msg, _why) = check_max_temp(&a, &outcome_with_tied_temps());
            assert!(ok, "100 C is under the 150 C ceiling: {msg}");
            assert!(
                msg.contains("SW1_q1"),
                "the tie must resolve to the lowest key deterministically; got: {msg}"
            );
            assert!(
                !msg.contains("SW1_q2") && !msg.contains("SW1_q3"),
                "only one unit reported: {msg}"
            );
        }
    }

    // A `protection_trip` assertion scoped to a scenario must judge only trips
    // that latched *inside* that scenario's window. A trip that happened during
    // an earlier scenario (still visible in the run-wide `protection_tripped`
    // flag) must not satisfy an assertion scoped to a later window (round-6 #8).
    #[test]
    fn protection_trip_respects_scenario_scope() {
        use super::check_protection_trip;
        use crate::runner::RunOutcome;
        use std::collections::HashMap;

        // Net BATT latched at some point in the run, but only within the
        // "inrush" window, NOT within the later "steady" window.
        fn outcome() -> RunOutcome {
            let mut protection_tripped = HashMap::new();
            protection_tripped.insert("BATT".to_string(), true);
            let mut protection_tripped_scoped = HashMap::new();
            protection_tripped_scoped.insert(("inrush".to_string(), "BATT".to_string()), true);
            protection_tripped_scoped.insert(("steady".to_string(), "BATT".to_string()), false);
            RunOutcome {
                bind: None,
                evidence: None,
                seed: 0,
                windows: HashMap::new(),
                uart: HashMap::new(),
                faults: Vec::new(),
                toggles: HashMap::new(),
                peak_current: HashMap::new(),
                peak_temp_c: HashMap::new(),
                peripherals: HashMap::new(),
                rail_windows: HashMap::new(),
                protection_tripped,
                protection_tripped_scoped,
                ambient_c: 25.0,
                sim_ms: 100.0,
                boot_first_cross_ms: HashMap::new(),
                boot_drop_after_cross_ms: HashMap::new(),
                driven_nets: Default::default(),
                drive_direction_observable: false,
                first_fault_ms: None,
                ac: None,
                analog_valid: true,
                failed_windows: Vec::new(),
                fallback_windows: Vec::new(),
                error_budget: None,
                analog_abort: false,
                sampled_values: Vec::new(),
                interior: false,
                net_series: HashMap::new(),
                substitutions: Vec::new(),
                coverage_warnings: Vec::new(),
                timing_coverage: Vec::new(),
                timing_refusals: Vec::new(),
                dead_rails: Vec::new(),
                unexercised_bus_ids: std::collections::HashSet::new(),
                spi_framing: HashMap::new(),
            }
        }

        let expect_trip = |scenario: Option<&str>| -> (bool, String) {
            let scen = scenario
                .map(|s| format!("scenario = \"{s}\"\n"))
                .unwrap_or_default();
            let a: crate::spec::Assertion = toml::from_str(&format!(
                "kind = \"protection_trip\"\nsupply_net = \"BATT\"\nexpect_trip = true\n{scen}"
            ))
            .unwrap();
            check_protection_trip(&a, &outcome())
        };

        // Unscoped: the run-wide flag shows a trip → an expect-trip passes.
        let (ok, _) = expect_trip(None);
        assert!(
            ok,
            "unscoped expect_trip should pass; BATT did trip run-wide"
        );

        // Scoped to the window where the trip happened → still passes.
        let (ok_inrush, _) = expect_trip(Some("inrush"));
        assert!(
            ok_inrush,
            "expect_trip scoped to 'inrush' should pass; that is where it latched"
        );

        // Scoped to a LATER window where no trip occurred → must FAIL, even
        // though the run-wide flag is set. This is the round-6 #8 bug.
        let (ok_steady, msg) = expect_trip(Some("steady"));
        assert!(
            !ok_steady,
            "expect_trip scoped to 'steady' must FAIL; no trip in that window; got pass: {msg}"
        );
    }

    // A max_temp ceiling BELOW ambient must fail for an idle (non-dissipating)
    // device; its junction sits at ambient, which already exceeds the ceiling.
    // Auto-passing on "no dissipation measured" hid a hot-ambient violation
    // (round-7 #8).
    #[test]
    fn max_temp_idle_device_fails_when_ambient_exceeds_ceiling() {
        use super::check_max_temp;
        use crate::runner::RunOutcome;

        let assertion = |ceiling: f64| -> crate::spec::Assertion {
            toml::from_str(&format!(
                "kind = \"max_temp\"\nref = \"U3\"\ncelsius = {ceiling}\n"
            ))
            .unwrap()
        };
        // Idle device: no entry in peak_temp_c, ambient 85 C.
        let hot = RunOutcome {
            ambient_c: 85.0,
            ..Default::default()
        };

        let (ok, msg, _why) = check_max_temp(&assertion(70.0), &hot);
        assert!(
            !ok,
            "idle U3 at ambient 85C must fail a 70C ceiling, not auto-pass: {msg}"
        );
        // A ceiling above ambient still passes the idle part.
        let (ok2, msg2, _why) = check_max_temp(&assertion(105.0), &hot);
        assert!(
            ok2,
            "idle U3 at ambient 85C is within a 105C ceiling: {msg2}"
        );
    }

    // The celsius-less form on a part with NO temperature sample must FAIL
    // ("guard never evaluated"), not report "within device max": the junction
    // was never estimated, so a green there vouches for nothing (the same
    // discipline as max_current's no-data branch).
    #[test]
    fn max_temp_without_ceiling_and_without_any_sample_fails_not_passes() {
        use super::check_max_temp;
        use crate::runner::RunOutcome;

        let a: crate::spec::Assertion =
            toml::from_str("kind = \"max_temp\"\nref = \"U2\"\n").unwrap();
        let idle = RunOutcome::default(); // no peak_temp_c entry, no faults
        let (ok, msg, _why) = check_max_temp(&a, &idle);
        assert!(
            !ok,
            "no sample + no ceiling must fail loud, not pass vacuously: {msg}"
        );
        assert!(
            msg.contains("never evaluated"),
            "the reason names the vacuousness: {msg}"
        );
        // With a real sample under the device max, the form still passes.
        let mut peak_temp_c = std::collections::HashMap::new();
        peak_temp_c.insert("U2".to_string(), 61.0);
        let warm = RunOutcome {
            peak_temp_c,
            ..Default::default()
        };
        let (ok2, msg2, _why) = check_max_temp(&a, &warm);
        assert!(ok2, "a measured junction below device max passes: {msg2}");
    }

    // A max_current assert on a multi-unit package (resistor/diode array) names
    // the bare ref `RN1`, but peak_current is keyed by the per-unit device names
    // `RN1_e1..RN1_e4`. An exact `.get("RN1")` always misses and drops into the
    // "no current data" fail branch, so the assert could NEVER pass no matter
    // how low the real current is (R48). The consumer must aggregate over units
    // via key_belongs_to_ref, exactly like check_max_temp.
    #[test]
    fn max_current_aggregates_over_multiunit_package_keys() {
        use super::check_max_current;
        use crate::runner::RunOutcome;

        let assertion = |amps: f64| -> crate::spec::Assertion {
            toml::from_str(&format!(
                "kind = \"max_current\"\nref = \"RN1\"\namps = {amps}\n"
            ))
            .unwrap()
        };
        // A 4-element resistor array: no bare "RN1" key, only per-unit `_e` keys.
        let mut peak_current = std::collections::HashMap::new();
        peak_current.insert("RN1_e1".to_string(), 0.10);
        peak_current.insert("RN1_e2".to_string(), 0.12);
        peak_current.insert("RN1_e3".to_string(), 0.08);
        peak_current.insert("RN1_e4".to_string(), 0.11);
        let out = RunOutcome {
            peak_current,
            ..Default::default()
        };

        // Within a 0.5 A limit (peak unit is 0.12 A): must PASS. Base bug always
        // reported "no current data … cannot be reported green" here.
        let (ok, msg, _why) = check_max_current(&assertion(0.5), &out);
        assert!(
            ok,
            "a resistor array within its current limit must pass, not miss its unit keys: {msg}"
        );
        assert!(
            msg.contains("RN1_e2"),
            "message must name the peak unit: {msg}"
        );
        // And it must still be able to FAIL when a unit exceeds the limit.
        let (ok_over, msg_over, _why_over) = check_max_current(&assertion(0.10), &out);
        assert!(
            !ok_over,
            "the hottest-current unit (0.12 A) must trip a 0.10 A limit: {msg_over}"
        );
    }

    // A rail_window brownout floor must see an intra-frame sag that recovers by
    // the frame's last chunk; the runner folds the scheduler's per-frame extremes
    // into RailWindow.min_v/max_v, exactly like the plain voltage path. Without the
    // fold (base bug), min_v is the settled 3.3 V and a min=3.0 floor false-passes
    // the very sag it exists to catch (R49).
    #[test]
    fn rail_window_min_reflects_folded_intraframe_sag() {
        use super::check_rail_window;
        use crate::runner::RunOutcome;
        use crate::scenarios::RailWindow;

        // Reconstruct the window the runner builds: settled 3.3 V samples plus the
        // scheduler's intra-frame minimum (2.9 V) folded into the envelope.
        let mut win = RailWindow::new();
        win.observe(0.000, 3.3);
        win.observe(0.001, 3.3);
        win.fold(2.9); // intra-frame sag from the load step, recovered by last chunk

        let mut rail_windows = std::collections::HashMap::new();
        rail_windows.insert(("load".to_string(), "VBUS".to_string()), win);
        let out = RunOutcome {
            rail_windows,
            ..Default::default()
        };

        let a: crate::spec::Assertion = toml::from_str(
            "kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\nmin = 3.0\n",
        )
        .unwrap();
        let (ok, msg, _why) = check_rail_window(&a, &out);
        assert!(
            !ok,
            "a rail that sagged to 2.9V mid-frame must FAIL a 3.0V floor, not pass on the settled 3.3V: {msg}"
        );

        // Sanity: a window that never dipped below the floor still passes.
        let mut win_ok = RailWindow::new();
        win_ok.observe(0.000, 3.3);
        win_ok.observe(0.001, 3.25);
        win_ok.fold(3.1);
        let mut rw2 = std::collections::HashMap::new();
        rw2.insert(("load".to_string(), "VBUS".to_string()), win_ok);
        let out_ok = RunOutcome {
            rail_windows: rw2,
            ..Default::default()
        };
        let (ok2, _msg2, _why) = check_rail_window(&a, &out_ok);
        assert!(ok2, "a rail that stayed above 3.0V must pass");
    }

    // hwtrace ensemble: a converged member's real feature mismatch must beat a
    // diverged sibling's INVALID (the R50 FAIL>INVALID doctrine, applied to the
    // hwtrace path). Base bug: any diverged member forced every feature to INVALID.
    #[test]
    fn hwtrace_converged_mismatch_beats_a_diverged_sibling_invalid() {
        use super::evaluate_hwtrace;
        use crate::runner::RunOutcome;

        // A square wave 0<->5V of the given period over 1 s at 1 kSa/s.
        fn square(period_s: f64) -> Vec<(f64, f64)> {
            let mut s = Vec::new();
            let mut t = 0.0_f64;
            while t <= 1.0 {
                let v = if (t / period_s).fract() >= 0.5 {
                    4.9
                } else {
                    0.05
                };
                s.push((t, v));
                t += 0.001;
            }
            s
        }

        let dir = std::env::temp_dir().join(format!("hb_hwtrace_prec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Captured waveform: 200 ms period.
        let mut csv = String::from("time_s,volts\n");
        for (t, v) in square(0.2) {
            csv.push_str(&format!("{t:.4},{v:.3}\n"));
        }
        std::fs::write(dir.join("d.csv"), csv).unwrap();
        std::fs::write(
            dir.join("trace.toml"),
            "[trace]\nboard = \"x\"\nscenario = \"prec\"\nprovenance = \"synthetic\"\n\
             instrument = \"in-test\"\n[[channel]]\nnet = \"D\"\nfile = \"d.csv\"\n\
             [[channel.feature]]\nkind = \"period\"\nreltol = 0.10\n",
        )
        .unwrap();

        let mut spec: crate::spec::Spec = toml::from_str(
            "board = \"x.kicad_pcb\"\nduration_ms = 1000\n\
             [[assert]]\nkind = \"hwtrace\"\ntrace = \"trace.toml\"\n",
        )
        .unwrap();
        spec.base_dir = dir.clone();
        let a = spec.asserts[0].clone();

        // Corner 0 converged, but its sim period (500 ms) disagrees with the 200 ms
        // capture, a trustworthy FAIL. Corner 1 diverged (a failed window).
        let mut ns0 = std::collections::HashMap::new();
        ns0.insert("D".to_string(), square(0.5));
        let corner0 = RunOutcome {
            seed: 0,
            net_series: ns0,
            sim_ms: 1000.0,
            ..Default::default()
        };
        let mut ns1 = std::collections::HashMap::new();
        ns1.insert("D".to_string(), square(0.2));
        let corner1 = RunOutcome {
            seed: 1,
            net_series: ns1,
            sim_ms: 1000.0,
            failed_windows: vec![(0.4, 0.5)],
            ..Default::default()
        };

        let results = evaluate_hwtrace(
            &spec,
            &a,
            &[corner0, corner1],
            Some(crate::tolerance::Mode::Corners),
        );
        let _ = std::fs::remove_dir_all(&dir);

        let r = results
            .iter()
            .find(|r| r.kind == "hwtrace")
            .expect("a hwtrace result");
        assert!(
            !r.invalid,
            "a converged corner's real mismatch must FAIL, not be masked INVALID: {}",
            r.detail
        );
        assert!(!r.passed, "the feature genuinely disagrees: {}", r.detail);
        assert_eq!(
            r.failing_seed,
            Some(0),
            "corner 0 (the converged failure) must be named: {}",
            r.detail
        );
    }

    // A vcd_sink `transitions` field is analog-derived (per-frame threshold
    // crossings), so its assertion window must be gated by analog validity like
    // `toggle`, otherwise a diverged chunk silently yields a definite PASS/FAIL on
    // held-stale edge counts instead of INVALID (R51). Digital bus-slave fields
    // stay un-gated.
    #[test]
    fn peripheral_transitions_field_is_analog_gated_but_digital_state_is_not() {
        use super::analog_eval_window;
        use crate::runner::RunOutcome;

        let out = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        let transitions: crate::spec::Assertion = toml::from_str(
            "kind = \"peripheral\"\nid = \"SINK\"\nfield = \"transitions\"\nmin = 100\n",
        )
        .unwrap();
        assert_eq!(
            analog_eval_window(&transitions, &out),
            Some((0.0, 0.1)),
            "a vcd_sink transitions assertion must expose an analog window so it can be INVALIDated"
        );
        // A digital bus-slave peripheral field is NOT analog-derived → no window.
        let digital: crate::spec::Assertion =
            toml::from_str("kind = \"peripheral\"\nid = \"EE\"\nfield = \"last_addr\"\nmin = 0\n")
                .unwrap();
        assert_eq!(
            analog_eval_window(&digital, &out),
            None,
            "a digital peripheral field must not be analog-gated"
        );
    }

    // A trustworthy definite failure on one converged ensemble member must WIN
    // over an unrelated analog divergence on another member (FAIL > INVALID).
    // Base bug: the INVALID gate ran first, so a diverged sibling silently
    // downgraded a real brownout on a fully-converged corner to INVALID (R50).
    #[test]
    fn definite_failure_on_a_converged_member_beats_invalid_on_a_diverged_one() {
        use super::evaluate_one;
        use crate::runner::{NetWindow, RunOutcome};

        let win = |min_v: f64| {
            let mut w = std::collections::HashMap::new();
            w.insert(
                ("VOUT".to_string(), 0.0f64.to_bits()),
                NetWindow {
                    min_v,
                    max_v: 3.4,
                    last_v: 3.3,
                    samples: 10,
                },
            );
            w
        };
        // Corner 1: converged cleanly (no failed windows), VOUT sagged to 2.0 V,
        // a real, trustworthy brownout (RED).
        let corner1 = RunOutcome {
            seed: 1,
            windows: win(2.0),
            sim_ms: 100.0,
            failed_windows: Vec::new(),
            ..Default::default()
        };
        // Corner 2: a stiff transient diverged, leaving an overlapping failed
        // window; its samples are held-stale (not trustworthy either way).
        let corner2 = RunOutcome {
            seed: 2,
            windows: win(3.3),
            sim_ms: 100.0,
            failed_windows: vec![(0.010, 0.020)],
            ..Default::default()
        };

        let a: crate::spec::Assertion =
            toml::from_str("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.2\n").unwrap();
        let res = evaluate_one(
            &a,
            &[corner1, corner2],
            Some(crate::tolerance::Mode::Corners),
        );
        assert!(
            !res.invalid,
            "a real brownout on the converged corner must be reported, not masked as INVALID: {}",
            res.detail
        );
        assert!(
            !res.passed,
            "the assertion is definitively false: {}",
            res.detail
        );
        assert_eq!(
            res.failing_seed,
            Some(1),
            "corner 1 (the converged, trustworthy failure) must be named: {}",
            res.detail
        );
        // R52: the pass-rate must NOT count the held-stale (INVALID) corner 2 as
        // passed. Of 2 corners: 0 passed, 1 failed, 1 invalid; the base bug
        // reported "passed 1/2" (folding the skipped INVALID member into passed).
        assert!(
            res.detail.contains("passed 0/2") && res.detail.contains("1 invalid"),
            "held-stale members must not inflate the passed count: {}",
            res.detail
        );
    }

    // An unscoped uart assertion concatenates every MCU's output; the order must
    // be stable (sorted by MCU key), not HashMap iteration order, or an anchored
    // match flakes run to run (round-7 #9).
    #[test]
    fn uart_all_mcu_concatenation_is_order_stable() {
        use super::check_uart;
        use crate::runner::RunOutcome;

        let a: crate::spec::Assertion =
            toml::from_str("kind = \"uart\"\nmatches = \"^BOOT\"\n").unwrap();

        // Two MCUs; whichever HashMap order they land in, the sorted-by-key
        // concatenation is deterministic ("A" before "B"), so "^BOOT" (A's
        // output) anchors the same way every time.
        let mk = |first: &str, second: &str| -> bool {
            let mut uart = std::collections::HashMap::new();
            uart.insert(first.to_string(), "BOOT_OK\n".to_string());
            uart.insert(second.to_string(), "READY\n".to_string());
            let out = RunOutcome {
                uart,
                ..Default::default()
            };
            check_uart(&a, &out).0
        };
        // "A" holds BOOT_OK regardless of insertion order → ^BOOT always matches.
        assert!(
            mk("A", "B"),
            "A-first: sorted haystack starts with A's BOOT_OK"
        );
        assert!(mk("A", "B") == mk("A", "B"), "deterministic");
        // If B held BOOT and A held READY, sorted order puts READY first, so
        // ^BOOT must NOT match, and must not match by luck either.
        let mut uart2 = std::collections::HashMap::new();
        uart2.insert("A".to_string(), "READY\n".to_string());
        uart2.insert("B".to_string(), "BOOT_OK\n".to_string());
        let out2 = RunOutcome {
            uart: uart2,
            ..Default::default()
        };
        assert!(
            !check_uart(&a, &out2).0,
            "sorted order puts A's READY first, so ^BOOT anchored at start must not match"
        );
    }

    // ac_gain at a frequency outside the swept band must fail loudly, not
    // silently clamp to the nearest endpoint gain and report it as measured at
    // the requested frequency (round-7 #13).
    #[test]
    fn ac_gain_out_of_band_frequency_is_refused() {
        use super::check_ac_gain;
        use crate::runner::{AcOutcome, RunOutcome};

        let mut bode = std::collections::HashMap::new();
        // Swept 10 Hz .. 100 kHz; gain -5 dB everywhere for simplicity.
        bode.insert(
            "OUT".to_string(),
            vec![
                (10.0, -5.0, 0.0),
                (1_000.0, -5.0, 0.0),
                (100_000.0, -5.0, 0.0),
            ],
        );
        let out = RunOutcome {
            ac: Some(AcOutcome {
                bode,
                ..Default::default()
            }),
            ..Default::default()
        };

        // 1 MHz is above the band: interp_db would clamp to -5 dB and pass the
        // max=-20 bound falsely. Must fail with an out-of-band message.
        let above: crate::spec::Assertion =
            toml::from_str("kind = \"ac_gain\"\nnet = \"OUT\"\nfreq_hz = 1e6\nmax = -20.0\n")
                .unwrap();
        let (ok, msg, _why) = check_ac_gain(&above, &out);
        assert!(
            !ok,
            "out-of-band 1 MHz must fail, not clamp-and-pass: {msg}"
        );
        assert!(
            msg.contains("outside the swept band"),
            "msg names the cause: {msg}"
        );

        // An in-band frequency evaluates normally (−5 dB is within max=−20? no,
        // −5 > −20 so it fails the bound, but for a real measured reason, not
        // out-of-band).
        let inband: crate::spec::Assertion =
            toml::from_str("kind = \"ac_gain\"\nnet = \"OUT\"\nfreq_hz = 1000.0\nmax = -20.0\n")
                .unwrap();
        let (_, msg2, _why2) = check_ac_gain(&inband, &out);
        assert!(
            !msg2.contains("outside the swept band"),
            "in-band is measured normally: {msg2}"
        );

        // R38: a non-finite freq_hz slips past the band bounds (every NaN compare
        // is false), so interp_db would clamp to the top-of-band gain and report
        // it "at NaN Hz". Must refuse, not measure a frequency the author never
        // chose.
        let nan_freq: crate::spec::Assertion =
            toml::from_str("kind = \"ac_gain\"\nnet = \"OUT\"\nfreq_hz = nan\nmax = -20.0\n")
                .unwrap();
        let (ok, msg3, _why) = check_ac_gain(&nan_freq, &out);
        assert!(!ok, "a NaN freq_hz must fail, not clamp-and-report: {msg3}");
        assert!(
            msg3.contains("non-finite"),
            "msg names the non-finite freq cause: {msg3}"
        );
    }
}

/// Adaptive current formatting: a sub-100 uA peak printed as "0.0000A" is
/// illegible, so the unit follows the magnitude (A / mA / uA; exact zero stays
/// "0 A").
pub(crate) fn format_amps(amps: f64) -> String {
    let mag = amps.abs();
    if amps == 0.0 {
        "0 A".to_string()
    } else if mag >= 0.1 {
        format!("{amps:.4} A")
    } else if mag >= 1e-4 {
        format!("{:.3} mA", amps * 1e3)
    } else {
        format!("{:.3} uA", amps * 1e6)
    }
}

#[cfg(test)]
mod format_amps_tests {
    use super::format_amps;

    #[test]
    fn small_currents_get_legible_units() {
        assert_eq!(format_amps(0.0), "0 A");
        assert_eq!(format_amps(1.2345), "1.2345 A");
        assert_eq!(format_amps(0.0123), "12.300 mA");
        // The old rendering of this value was "0.0000A".
        assert_eq!(format_amps(4.2e-5), "42.000 uA");
    }
}
