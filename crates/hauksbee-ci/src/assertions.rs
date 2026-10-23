//! Evaluate a spec's assertions against the per-seed run outcomes.
//!
//! An assertion passes only if it holds on *every* seed (that is what makes initial-state
//! fuzzing meaningful: the rail must come up across all random power-up states).

use crate::runner::RunOutcome;
use crate::spec::{Assertion, Spec};
use hauksbee_engine::scheduler::PeripheralPad;
use hauksbee_engine::stress::{is_far_past_rating, rating_noun, stress_clause, FaultKind};
use hauksbee_ir::truncate_ellipsis;
use hauksbee_ir::units::{
    format_amps, format_celsius, format_celsius_to, format_percent, format_volts,
};
use std::fmt::Write as _;

/// The result of one assertion across all seeds.
///
/// Serialized verbatim into the `results` array of the `hauksbee-ci run --json` document, so the published
/// schema is generated from this type: see
/// `crates/hauksbee-ci/tests/ci_report_schema_drift.rs`. `why` and `waived` are
/// the only two fields a consumer may find ABSENT rather than null.
#[derive(Debug, Clone, Default, serde::Serialize, schemars::JsonSchema)]
pub struct AssertResult {
    /// The assertion's label: its `label` in the spec, or a generated one
    /// naming the kind and subject.
    pub label: String,
    /// The assertion kind, as the spec's `kind` token (`voltage`, `uart`,
    /// `blink`, `no_faults`, `hwtrace`, ...).
    pub kind: String,
    /// Did the numbers hold on every ensemble member. False on an ordinary red
    /// and a waived red; read [`Self::invalid`] first, since it is not the verdict.
    pub passed: bool,
    /// A THIRD outcome distinct from pass/fail: the assertion could not
    /// be honestly evaluated because its analog evaluation window overlaps a
    /// chunk the solver failed on, so the samples there are held-stale. When
    /// `invalid` is true, `passed` keeps only its numeric meaning: a stale span or
    /// timing refusal forces it false, but an evidence-undermined row (an open part
    /// on its causal path) keeps `passed == true` if the numbers held. Either way
    /// this is NOT an ordinary failure: the run exits 3 (invalid-for-analysis)
    /// rather than 1, even below the consecutive-abort threshold. A normal pass or
    /// fail leaves it `false`.
    pub invalid: bool,
    /// One-line detail (the measured value, the offending seed, etc).
    pub detail: String,
    /// This assertion carried no bound, so it is a MEASUREMENT: the value is
    /// reported and nothing is judged. `passed` is true (a measurement cannot
    /// fail) but every surface renders it "measured" rather than PASS, and a run
    /// whose results are all measurements headlines as measurements reported.
    pub measured: bool,
    /// If it failed, the first seed index that failed (for fuzzed runs).
    /// Always present in the JSON, `null` on a pass, unlike `why` and `waived`
    /// which are omitted.
    #[schemars(schema_with = "schema_nullable_seed", required)]
    pub failing_seed: Option<u32>,
    /// Every ensemble member this assertion failed on (empty on a pass). For a
    /// tolerance ensemble this is the per-seed failure list the report and
    /// `JUnit` surface, together with the pass-rate.
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
    /// Every part and net the `label` stands for when it names too many to print
    /// ("... and 44 more"): the whole list, parts first, so the label can stay
    /// one readable line without dropping a name. ABSENT (not empty) when the
    /// label already names them all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "schema_absent_or_strings")]
    pub all_subjects: Vec<String>,
    /// Net names this assertion judges, for waiver matching. Not serialized:
    /// the JSON surface already carries them in the label/detail.
    #[serde(skip)]
    pub subject_nets: Vec<String>,
    /// Component references this assertion judges, for waiver matching.
    #[serde(skip)]
    pub subject_refs: Vec<String>,
    /// The `[[peripheral]]` / `[[sensor]]` id a `peripheral` assertion reads,
    /// so its evidence names that sensor's model. Kept out of `subject_refs`,
    /// which feeds waiver matching.
    #[serde(skip)]
    pub subject_peripheral: Option<String>,
    /// The time bound a host-paced MCU clock left unjudged here. Not
    /// serialized: the run turns it into a
    /// typed assumption on this assertion's evidence map, the one channel
    /// every surface (terminal, `JUnit`, annotations, JSON, web) already
    /// renders, so the condition a pass rests on is worded once.
    #[serde(skip)]
    pub host_paced: Option<HostPacedBound>,
}

/// A deadline or time bound that was NOT judged because the firmware time it
/// compares was measured on a host-paced MCU clock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPacedBound {
    /// The bound, as a noun phrase: "the 700 ms boot deadline".
    pub bound: String,
    /// The MCU references whose clocks are host-paced.
    pub mcus: Vec<String>,
}

/// The verdict conditions host-paced MCU clocks put on the passes in `results`.
///
/// One sentence per passing result whose time bound such a clock left
/// unjudged, naming the assertion, the bound and the clock. A failing or
/// INVALID result qualifies no pass, so it adds none.
#[must_use]
pub fn host_paced_conditions(results: &[AssertResult]) -> Vec<String> {
    results
        .iter()
        .filter(|r| r.passed && !r.invalid)
        .filter_map(|r| {
            r.host_paced.as_ref().map(|paced| {
                format!(
                    "{}: {} was not judged, because MCU {} runs on a host-paced clock that \
                     credits simulated time from host wall-clock spans; the rest of the \
                     assertion held",
                    r.label,
                    paced.bound,
                    paced.mcus.join(", ")
                )
            })
        })
        .collect()
}

impl AssertResult {
    /// A neutral result for one assertion: an unwaived, unexplained, non-invalid
    /// failure with no failing member and no subjects. Every construction site
    /// sets only the fields that differ, via struct-update syntax.
    pub fn shell(label: String, kind: impl Into<String>, seeds_total: usize) -> Self {
        Self {
            label,
            kind: kind.into(),
            seeds_total: u32::try_from(seeds_total).unwrap_or(u32::MAX),
            ..Default::default()
        }
    }

    /// The whole list a summarised label stands for, as one line, or `None`
    /// when the label names everything.
    #[must_use]
    pub fn all_subjects_line(&self) -> Option<String> {
        (!self.all_subjects.is_empty()).then(|| {
            format!(
                "all {}: {}",
                self.all_subjects.len(),
                self.all_subjects.join(", ")
            )
        })
    }
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

/// A list of strings that is OMITTED rather than empty when there is nothing to
/// list (`skip_serializing_if`), like [`schema_absent_or_string`].
pub(crate) fn schema_absent_or_strings(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "array", "items": { "type": "string" } })
}

/// Evaluate every assertion in the spec; returns one result per assertion.
#[must_use]
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
            let mut results = if a.kind == "hwtrace" {
                // One hwtrace assertion expands to one result per (channel,
                // feature), so the report shows the per-feature table an EE
                // can argue with, not a single opaque pass/fail.
                evaluate_hwtrace(spec, a, outcomes, mode)
            } else {
                let mut result = evaluate_one(a, outcomes, mode, spec.frame_ms);
                result.subject_peripheral = a.id.clone().filter(|_| a.kind == "peripheral");
                // An unjudged (INVALID) result has no verdict to qualify, and a
                // hard failure of a kind whose miss is otherwise unjudged (a bus
                // never exercised, a bad regex) names no time bound it left
                // unjudged.
                let qualifies = !result.invalid && (result.passed || !unjudged_when_missed(a));
                result.host_paced = qualifies
                    .then(|| {
                        outcomes.iter().find_map(|out| {
                            host_paced_time_bound(a, out).map(|bound| HostPacedBound {
                                bound,
                                mcus: out.host_paced_mcus.clone(),
                            })
                        })
                    })
                    .flatten();
                vec![result]
            };
            if a.times_startup() && spec.dc_initialised() {
                for r in &mut results {
                    r.detail.push_str(DC_INITIALISED_NOTE);
                }
            }
            results
        })
        .collect()
}

/// Appended to any result whose assertion reads a start-up time on a board
/// where nothing makes the run time-varying (see [`Spec::dc_initialised`]).
/// Cheap and narrowly aimed: on such a board the answer to a timing question is
/// a convention, not a measurement, and it must not read as one.
const DC_INITIALISED_NOTE: &str = " [supplies start settled at t=0 and nothing \
in this spec makes the run time-varying, so power-on timing is not simulated; \
set `ramp_ms` on the supply to model a cold start]";

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
    let member = member_noun(mode);

    let hard_fail = |label: String, detail: String| AssertResult {
        detail,
        ..AssertResult::shell(label, "hwtrace", outcomes.len())
    };

    let trace = match hwtrace::trace_path(spec, a).and_then(|p| hwtrace::Trace::load(&p)) {
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

    // The analog-validity gate: the simulated waveforms are per-frame
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
            // FAIL > INVALID: a converged member's real disagreement with the
            // capture must beat a diverged sibling's INVALID, so we only fall
            // back to INVALID (below) when no converged member fails.
            let mut last_detail = String::new();
            // Carry the OUTCOME, not just its index: an interior probe and a
            // corner are numbered from the same sequence, so the label needs the
            // member itself to tell them apart (see `member_label`).
            let mut failures: Vec<(&RunOutcome, String)> = Vec::new();
            for out in outcomes {
                if !out.failed_windows.is_empty() {
                    continue;
                }
                let Some(sim) = out.net_series.get(&ch.net) else {
                    failures.push((
                        out,
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
                    failures.push((out, r.detail));
                }
            }

            // No trustworthy (converged) failure, but a member diverged → INVALID
            // > PASS: a run with a held-stale window must not masquerade as a pass.
            if failures.is_empty() {
                if let Some(out) = stale {
                    let (fs, fe) = out.failed_windows[0];
                    results.push(AssertResult {
                        invalid: true,
                        detail: format!(
                            "{}INVALID: the analog solve failed within {:.2}-{:.2} ms of the \
                             run; the simulated waveform contains held-stale samples, so no \
                             feature comparison against the capture can be trusted.",
                            member_prefix(member, out, outcomes.len()),
                            fs * 1e3,
                            fe * 1e3
                        ),
                        failing_seed: Some(out.seed),
                        failing_seeds: vec![out.seed],
                        subject_nets: vec![ch.net.clone()],
                        ..AssertResult::shell(label, "hwtrace", outcomes.len())
                    });
                    continue;
                }
            }

            let (passed, mut detail, failing_seeds) = match failures.first() {
                None => (true, last_detail, Vec::new()),
                Some((out, d)) => (
                    false,
                    member_prefix(member, out, outcomes.len()) + d,
                    failures.iter().map(|(o, _)| o.seed).collect(),
                ),
            };
            detail.push_str(provenance);
            results.push(AssertResult {
                passed,
                detail,
                failing_seed: failing_seeds.first().copied(),
                failing_seeds,
                subject_nets: vec![ch.net.clone()],
                ..AssertResult::shell(label, "hwtrace", outcomes.len())
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
    let with = |(r, id): &(String, String)| format!(" with {r} as variant {id}");
    let variants: String = out.variants.iter().map(with).collect();
    if out.interior {
        format!("interior probe {}{variants}", out.seed)
    } else {
        format!("{member} {}{variants}", out.seed)
    }
}

/// [`member_label`] and `": "` to lead a detail line, or nothing for a single run,
/// where there is no member to tell apart.
fn member_prefix(member: &str, out: &RunOutcome, members: usize) -> String {
    if members > 1 {
        format!("{}: ", member_label(member, out))
    } else {
        String::new()
    }
}

/// The noun for an ensemble member index: a corner number in corner mode, else a seed.
const fn member_noun(mode: Option<crate::tolerance::Mode>) -> &'static str {
    match mode {
        Some(crate::tolerance::Mode::Corners) => "corner",
        _ => "seed",
    }
}

/// The shared regulation reason that invalidates this assertion on this member: an
/// unsupported regulator's output is an ideal source's setpoint, so nothing it
/// reaches can be judged. `model_coverage` counts bound models, not voltages, and
/// survives. A subjectless kind (`no_faults` and the other whole-run kinds) is
/// always on the tainted path, and a firmware-stateful kind is invalidated by a
/// tainted MCU (`refs[0]` is the regulator; later entries are MCUs it powers) even
/// when it names no net of its own.
fn regulation_reason<'a>(a: &Assertion, out: &'a RunOutcome) -> Option<&'a str> {
    fn named(o: [Option<&String>; 2]) -> Vec<&String> {
        o.into_iter().flatten().collect()
    }
    if a.kind == "model_coverage" {
        return None;
    }
    let nets = named([a.net.as_ref(), a.supply_net.as_ref()]);
    let refs = named([a.reference.as_ref(), a.mcu.as_ref()]);
    let stateful = matches!(
        a.kind.as_str(),
        "uart" | "toggle" | "boot_coverage" | "peripheral"
    );
    out.regulation
        .iter()
        .find(|t| {
            (nets.is_empty() && refs.is_empty())
                || nets.iter().any(|n| t.nets.contains(n))
                || refs.iter().any(|r| t.refs.contains(r))
                || (stateful && t.refs.len() > 1)
        })
        .map(|t| t.reason.as_str())
}

/// Silicon's melting point (C): no junction estimate above it is physical.
pub(crate) const TJ_IMPOSSIBLE_C: f64 = 1414.0;

/// The lead of every INVALID reason that refuses a figure which is not a
/// physical result (a junction past the melting point, a measurement that is
/// not a finite number). The report's refusal footer counts rows by it, so the
/// footer names this cause and not a solver failure.
pub(crate) const UNPHYSICAL_FIGURE: &str = "unphysical figure:";

/// Does any entry of a per-device map that belongs to `reference` hold a
/// non-finite value (the runner records a NaN or infinite sample as NaN)?
fn nonfinite_for_ref(map: &std::collections::HashMap<String, f64>, reference: &str) -> bool {
    map.iter()
        .any(|(k, v)| key_belongs_to_ref(reference, k) && !v.is_finite())
}

/// Why a measurement this assertion reads is not a number at all, if so: the
/// run recorded a NaN or infinite sample for it. `f64::max`/`min` drop a NaN,
/// so such a sample used to vanish into the running extreme and leave a peak of
/// `-inf` or `0 A` that PASSED its limit; it now refuses, with the part or net.
fn nonfinite_measurement_reason(a: &Assertion, out: &RunOutcome) -> Option<String> {
    let reference = a.reference.as_deref().unwrap_or_default();
    let (what, subject) = match a.kind.as_str() {
        "max_temp" if nonfinite_for_ref(&out.peak_temp_c, reference) => {
            ("junction estimate", reference.to_string())
        }
        "max_current" if nonfinite_for_ref(&out.peak_current, reference) => {
            ("peak current", reference.to_string())
        }
        _ => {
            let nets = [a.net.as_ref(), a.supply_net.as_ref()];
            let net = nets.into_iter().flatten().find(|n| {
                out.windows
                    .iter()
                    .any(|((w, _), win)| &w == n && win.is_poisoned())
                    || out
                        .rail_windows
                        .iter()
                        .any(|((_, w), win)| &w == n && win.is_poisoned())
            })?;
            ("voltage", net.clone())
        }
    };
    Some(format!(
        "{UNPHYSICAL_FIGURE} the {what} for {subject} is not a finite number (NaN or infinite),          so there is nothing to compare with your limit. A non-finite input is the usual cause:          check `ambient_c`, the supply voltages and the part values for a nan, inf or absurd figure."
    ))
}

/// The lead of every INVALID reason that says a kind is valid in the spec but
/// the run recorded nothing it could be judged from. The report's refusal
/// footer and the app both recognise it by this prefix.
pub(crate) const NEEDS_BENCH_DATA: &str = "needs bench data:";

/// The execution rows `reaches` reads: those of the MCU it names (all when it
/// names none) whose entry symbol is the one asked for.
fn entry_rows<'a>(
    a: &'a Assertion,
    out: &'a RunOutcome,
) -> impl Iterator<Item = &'a hauksbee_engine::execution::ExecutionEvidence> {
    let symbol = a.symbol.as_deref();
    out.execution.iter().filter(move |e| {
        a.mcu.as_deref().is_none_or(|m| m == e.mcu_ref) && e.entry_symbol.as_deref() == symbol
    })
}

/// Why a bench kind cannot be judged on this run, or `None` when it can.
///
/// Today a run records one fact of this kind: whether each core reached its
/// entry symbol (`main`). So `reaches symbol = "<entry>"` with no deadline is
/// judged from it, and every other bench assertion is valid but UNJUDGED (an
/// INVALID with this reason, never a pass) until the run records the bench
/// view it reads. This is the one seam to widen when it does.
fn bench_gate(a: &Assertion, out: &RunOutcome) -> Option<String> {
    use hauksbee_engine::execution::Observed;
    let (what, now) = match a.kind.as_str() {
        "bus" => (
            "the bus log (each I2C/SPI transaction with its ACK and bytes)",
            "",
        ),
        "resets" => ("the MCU's reset count and causes", ""),
        "uart_baud" => ("the baud rate decoded from the UART's bit timing", ""),
        "reaches" if a.not_parked_in.is_some() => {
            ("the park site (the function the core ends the run in)", "")
        }
        "reaches" if a.deadline_ms.is_some() => (
            "the time each symbol was first reached",
            " Without a deadline, `reaches` can already judge the entry symbol.",
        ),
        "reaches" => {
            let mut rows = entry_rows(a, out).peekable();
            if rows.peek().is_none() {
                (
                    "a record of which symbols the core reached; only the entry symbol is \
                     known today",
                    "",
                )
            } else if rows.any(|e| e.reached_entry == Observed::Unknown) {
                (
                    "a backend that reports whether the core reached its entry symbol \
                     (this one cannot tell)",
                    "",
                )
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(format!(
        "{NEEDS_BENCH_DATA} this {} assertion judges {what}, which this run did not record. \
         The assertion is valid and will be judged once runs record the bench view; until \
         then it is not a pass.{now}",
        a.kind
    ))
}

/// `reaches symbol = "<entry>"`: did every core with that entry symbol reach it?
fn check_reaches(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    use hauksbee_engine::execution::Observed;
    let symbol = a.symbol.clone().unwrap_or_default();
    let rows: Vec<_> = entry_rows(a, out).collect();
    let missed: Vec<&str> = rows
        .iter()
        .filter(|e| e.reached_entry != Observed::Yes)
        .map(|e| e.mcu_ref.as_str())
        .collect();
    if missed.is_empty() {
        return (true, format!("{symbol}: reached by {}", rows.len()), None);
    }
    (
        false,
        format!("{symbol}: not reached by {}", missed.join(", ")),
        Some(format!(
            "the core never got to {symbol}: it stopped earlier (a clock or supply that never \
             came up, a hang waiting on hardware, or a fault); read the execution evidence \
             line for where"
        )),
    )
}

/// Why this member cannot be judged AT ALL, if it cannot: its evaluation window
/// overlapped a chunk the solver failed on, so the samples are held-stale; a figure
/// it rests on is not physical; or an unsupported regulator's setpoint reaches
/// what it judges. Worded once, here.
fn member_invalid_reason(a: &Assertion, out: &RunOutcome, frame_ms: f64) -> Option<String> {
    if let Some(((ws, we), (fs, fe))) = stale_overlap(a, out) {
        return Some(format!(
            "the analog solve failed within this assertion's window ({:.2}-{:.2} ms              overlaps its {:.2}-{:.2} ms evaluation span); those voltages are              held-stale, so the result cannot be trusted.",
            fs * 1e3,
            fe * 1e3,
            ws * 1e3,
            we * 1e3,
        ));
    }
    if let Some(reason) = nonfinite_measurement_reason(a, out) {
        return Some(reason);
    }
    // A junction estimate past silicon's melting point is no state a part can
    // occupy, so the power behind it is not physical: it decides nothing.
    let melted = |kind: &str, tj: f64| kind == "overtemperature" && tj > TJ_IMPOSSIBLE_C;
    let tj = match a.kind.as_str() {
        "max_temp" => peak_for_ref(&out.peak_temp_c, a.reference.as_deref().unwrap_or_default()),
        "no_faults" if out.faults.iter().all(|f| melted(&f.kind, f.value)) => {
            out.faults.first().map(|f| (f.component.clone(), f.value))
        }
        _ => None,
    };
    if let Some((key, tj)) = tj.filter(|(_, tj)| melted("overtemperature", *tj)) {
        return Some(format!(
            "{UNPHYSICAL_FIGURE} the junction estimate for {key} reached {}, past silicon's              {} melting point; no part holds that temperature, so the power behind it is              not a physical result. Check that part's value, its supply voltage and `ambient_c`.",
            format_celsius_to(tj.round(), 3),
            format_celsius_to(TJ_IMPOSSIBLE_C, 3),
        ));
    }
    // A bench kind with nothing recorded to judge it from is unjudged, not failed.
    if let Some(reason) = bench_gate(a, out) {
        return Some(reason);
    }
    // Content the firmware had not produced by the end of a host-paced run is not
    // evidence against it: the same firmware may need longer in simulated time on
    // a slower host.
    if let Some(reason) =
        host_paced_unjudged_reason(a, out).or_else(|| host_paced_level_reason(a, out, frame_ms))
    {
        return Some(reason);
    }
    // An absent open driver cannot make a failure real; a pass stands on the stated
    // assumption that the part leaves the net alone (the refinement words it).
    let reason = regulation_reason(a, out)?;
    let stands =
        reason.contains(hauksbee_engine::scheduler::OPEN_DRIVER) && check_seed(a, out, frame_ms).0;
    (!stands).then(|| reason.to_string())
}

fn evaluate_one(
    a: &Assertion,
    outcomes: &[RunOutcome],
    mode: Option<crate::tolerance::Mode>,
    frame_ms: f64,
) -> AssertResult {
    let label = a.label();
    let kind = a.kind.clone();
    // In corner mode a member index is a corner number, not a random seed.
    let member = member_noun(mode);

    // Is this member's analog evaluation window held-stale, i.e. does it overlap
    // a chunk the solver failed on? Such a member's samples cannot be trusted, so
    // its pass/fail is meaningless.
    // ... or does an unsupported regulator's setpoint reach what it judges?
    let member_invalid = |out: &RunOutcome| member_invalid_reason(a, out, frame_ms).is_some();

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
    let subject_refs = subject_refs_for(a, outcomes);

    if crate::search::is_relation(a) {
        let r = crate::search::relation(a, outcomes, &member_invalid);
        return AssertResult {
            subject_nets,
            subject_refs,
            ..r
        };
    }
    let mut last_detail = String::new();
    let mut failures: Vec<(&RunOutcome, String, Option<String>)> = Vec::new();
    for out in outcomes {
        if member_invalid(out) {
            continue;
        }
        let (ok, detail, why) = check_seed(a, out, frame_ms);
        if ok {
            last_detail = detail;
        } else {
            failures.push((out, detail, why));
        }
    }

    if crate::search::is_outcome(a) {
        // An outcome reports its WORST member (least margin), the one a
        // search exists to find and the one culprits were ranked on.
        let margin = |o: &RunOutcome| crate::search::measure(a, o).margin;
        failures.sort_by(|x, y| margin(x.0).total_cmp(&margin(y.0)));
    }
    if let Some((first, first_detail, first_why)) = failures.first() {
        // Lead with the first failing member's measurement, then the exact
        // sampled component values it ran with (the actionable artifact), then
        // the ensemble pass-rate.
        let mut detail = member_prefix(member, first, outcomes.len()) + first_detail;
        // Non-monotonicity, caught: an interior probe failed where the corners
        // did not. The corner set's whole claim is that an extreme of the inputs
        // produces an extreme of the output, and this is that claim disproved on
        // this board, for this assertion. Say so at the point of failure, because
        // the reflex on a corner-mode red is to look at the named corner values,
        // and here the corner values are exactly what did NOT find it.
        //
        // The counterfactual ("a corner-only run would have reported green") is
        // only true if EVERY corner both ran trustworthily and passed. One
        // held-stale corner and a corner-only run would have refused as INVALID,
        // not passed, so the claim needs all corners valid, not merely one.
        let corners_all_valid_and_passed = outcomes.iter().any(|o| !o.interior)
            && outcomes
                .iter()
                .filter(|o| !o.interior)
                .all(|o| !member_invalid(o) && !failures.iter().any(|(f, _, _)| f.seed == o.seed));
        if failures.iter().all(|(o, _, _)| o.interior) && corners_all_valid_and_passed {
            detail.push_str(
                " [NON-MONOTONIC: every min/max corner passed and this INTERIOR point \
                 failed, so the corners do not bound this response and a corner-only \
                 run would have reported green]",
            );
        }
        if !first.sampled_values.is_empty() {
            let _ = write!(
                detail,
                " [{}]",
                crate::tolerance::describe_values(&first.sampled_values)
            );
        }
        let failing_seeds: Vec<u32> = failures.iter().map(|(o, _, _)| o.seed).collect();
        if outcomes.len() > 1 {
            // The passed count must exclude held-stale (INVALID) members, not fold
            // them into "passed": those members were SKIPPED (member_invalid) and
            // never evaluated, so counting them green over-claims worst-case
            // coverage on the very surface a reviewer trusts.
            let invalid = outcomes.iter().filter(|o| member_invalid(o)).count();
            detail.push_str(&pass_rate_note(member, outcomes, &failing_seeds, invalid));
        }
        return AssertResult {
            detail,
            failing_seed: Some(failures[0].0.seed),
            failing_seeds,
            why: first_why.clone(),
            subject_nets,
            subject_refs,
            ..AssertResult::shell(label, kind, outcomes.len())
        };
    }

    // No trustworthy failure. INVALID > PASS: with no member we can honestly
    // pass or fail, report a distinct INVALID rather than let a window with no
    // valid samples masquerade as a pass. The stale half fires per-window (not
    // per consecutive-abort), so an intermittent divergence below the abort
    // threshold still refuses.
    if let Some((out, reason)) = outcomes
        .iter()
        .find_map(|out| member_invalid_reason(a, out, frame_ms).map(|r| (out, r)))
    {
        return AssertResult {
            invalid: true,
            detail: member_prefix(member, out, outcomes.len()) + "INVALID: " + &reason,
            failing_seed: Some(out.seed),
            failing_seeds: vec![out.seed],
            subject_nets,
            subject_refs,
            ..AssertResult::shell(label, kind, outcomes.len())
        };
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
        passed: true,
        measured: a.measurement(),
        detail,
        subject_nets,
        subject_refs,
        ..AssertResult::shell(label, kind, outcomes.len())
    }
}

/// The ensemble pass-rate a failing assertion's detail line ends with:
/// `; passed 7/10 seeds (failing: 3, 5, 9)`.
///
/// `invalid` members were skipped, never evaluated, so passed = total − failed −
/// invalid, and the invalid count is surfaced when any occurred.
fn pass_rate_note(
    member: &str,
    outcomes: &[RunOutcome],
    failing: &[u32],
    invalid: usize,
) -> String {
    let shown: Vec<String> = failing.iter().take(8).map(u32::to_string).collect();
    let elided = if failing.len() > 8 { ", …" } else { "" };
    let passed = outcomes.len() - failing.len() - invalid;
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
    let probes = outcomes
        .iter()
        .any(|o| o.interior)
        .then_some(" + interior probes");
    format!(
        "; passed {passed}/{} {member}s{} (failing: {}{elided}{invalid_note})",
        outcomes.len(),
        probes.unwrap_or_default(),
        shown.join(", "),
    )
}

/// The all-members-green detail string for an assertion, worded to the ensemble
/// mode. `members` is the total member count INCLUDING the nominal baseline
/// (member 0), so a `MonteCarlo` run reports `members - 1` sampled seeds; the
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
        // The corner claim, narrowed to exactly what ran. The old wording made
        // monotonicity the reader's problem: it disclosed the assumption and gave
        // them no way to test it. The interior probes give them a way, and the
        // wording must claim no more than the probes establish.
        //
        // What a green probe set establishes is precisely "no interior point I
        // sampled broke this assertion". It is NOT "the response is monotonic",
        // and it is not even "no interior point was worse than a corner": each
        // member is judged against the assertion's own window, so a probe worse
        // than every corner still passes while it stays in band. Both remaining
        // gaps are named rather than glossed, because a reader who trusts this
        // line is deciding whether the corner bound covers their board.
        (n, Some(crate::tolerance::Mode::Corners)) => {
            let corners = n.saturating_sub(interior);
            if interior == 0 {
                format!(
                    "{last_detail} (held on all {corners} min/max tolerance corners: bounds the \
                     worst case only where the response is monotonic in each value)"
                )
            } else {
                // `last_detail` is ONE member's measurement (whichever passed
                // last), not the ensemble's extremes, and with the probes running
                // that member is usually an interior probe rather than a corner.
                // Say so, or the numbers read as the span the corners established.
                format!(
                    "{last_detail} <- one member's reading, not the ensemble span (held on all \
                     {corners} min/max tolerance corners and on {interior} interior \
                     Latin-hypercube probe(s): no interior point sampled broke this assertion, \
                     which is evidence for the monotonicity the corner bound needs, not proof \
                     of it, and a probe inside the window is not compared against the corners' \
                     own margin)"
                )
            }
        }
    }
}

/// The member's analog evaluation span for `a` and the first failed-solve
/// window overlapping it, or `None` when nothing overlaps (or the assertion is
/// not analog-derived at all). A hit means the member's samples are held-stale
/// and its pass/fail is meaningless.
fn stale_overlap(a: &Assertion, out: &RunOutcome) -> Option<((f64, f64), (f64, f64))> {
    let (ws, we) = analog_eval_window(a, out)?;
    let failed = out
        .failed_windows
        .iter()
        .find(|&&(fs, fe)| ws < fe && fs < we)?;
    Some(((ws, we), *failed))
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
pub(crate) fn analog_eval_window(a: &Assertion, out: &RunOutcome) -> Option<(f64, f64)> {
    let end = out.sim_ms / 1000.0;
    match a.kind.as_str() {
        // On a host-paced clock a firmware-driven net's level is judged over the
        // whole run (see `host_paced_level_claim`), so that is the span a failed
        // chunk must not overlap.
        "voltage" if host_paced_level_claim(a, out).is_some() => Some((0.0, end)),
        "voltage" | "pulse_width" | "logic_level" | "fires" | "increases" | "decreases" => {
            Some((a.after_ms.unwrap_or(0.0) / 1000.0, end))
        }
        // `no_faults` reads the stress monitor, which only sees converged
        // chunks: a fault inside a failed window is unseen, not absent.
        "toggle" | "max_current" | "max_temp" | "rail_window" | "no_faults" => Some((0.0, end)),
        "boot_coverage" | "boot-coverage" => Some((0.0, a.deadline_ms.map_or(end, |d| d / 1000.0))),
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
/// e.g. `boot_coverage`'s drive-state analysis).
///
/// A time bound this member's host-paced MCU clock cannot honestly judge (see
/// [`host_paced_time_bound`]) is taken out before the check runs, so only the
/// host-independent part of the assertion decides PASS or FAIL, and the detail
/// says which bound went unjudged and why. `boot_coverage` does the same inside
/// [`check_boot_coverage`], whose clauses are not separable bound fields.
fn check_seed(a: &Assertion, out: &RunOutcome, frame_ms: f64) -> (bool, String, Option<String>) {
    let boot = matches!(a.kind.as_str(), "boot_coverage" | "boot-coverage");
    match host_paced_time_bound(a, out) {
        Some(bound) if !boot => {
            let (ok, detail, why) = host_paced_level_claim(a, out).map_or_else(
                || check_kind(&without_time_bound(a), out, frame_ms),
                |level| check_level_over_run(&level, out),
            );
            // Such a kind's miss is unjudged before this point (see
            // [`host_paced_unjudged_reason`]), so a failure here is a hard one
            // and only a pass carries the note.
            if ok || !unjudged_when_missed(a) {
                (ok, detail + &host_paced_note(&bound, out), why)
            } else {
                (ok, detail, why)
            }
        }
        _ => check_kind(a, out, frame_ms),
    }
}

/// Why firmware times on a host-paced clock are approximate, in the words
/// every unjudged bound carries. The cross-host figure is measured (the Watchy
/// v1.5 RES drive: 384-496 ms on one machine, 704-760 ms on another), see
/// `hauksbee_mcu::qemu::TIMING_LIMITATION`.
const HOST_PACED_WHY: &str = "so how far its firmware gets in a simulated millisecond \
     depends on the host's speed and load (the same boot has measured about 2x apart on \
     two machines)";

/// The detail suffix naming a bound a host-paced clock left unjudged.
fn host_paced_note(bound: &str, out: &RunOutcome) -> String {
    format!(
        "; {bound} not judged: MCU {} runs on a host-paced clock, {HOST_PACED_WHY}",
        out.host_paced_mcus.join(", ")
    )
}

/// The deadline or time bound in `a` that would compare a firmware time measured
/// on a host-paced MCU clock, as a noun phrase, or `None` when every bound `a`
/// carries is host-independent on this member.
///
/// A host-paced clock (`RunOutcome::host_paced_mcus`, the ESP32 QEMU backend)
/// credits simulated time from host wall-clock spans, so the same firmware on
/// the same board reaches the same event at a different simulated time on a
/// faster or slower machine. A verdict comparing that time against a bound
/// would be one a host's speed can flip, so the bound is reported unjudged and
/// the rest of the assertion keeps its hard verdict:
///
/// - `boot_coverage`: the deadline (and the hold measured against it, and the
///   order of a stress fault against the drive). Whether the net was driven
///   to level at all stays judged.
/// - `toggle` on a firmware-driven net: a `freq_hz` band, or a `min_toggles`
///   count above one (a count over a fixed simulated window is a rate). That
///   the net toggled at all stays judged.
/// - `pulse_width` on a firmware-driven net: the `min`/`max` width bounds.
///   The width is still measured and reported.
/// - `logic_level` on a firmware-driven net: `min_width_ms`, and a
///   `min_toggles` pulse count above one. The level itself stays judged.
/// - `voltage` on a firmware-driven net: an `after_ms` window start. Where it
///   falls in the firmware's progress depends on the host, so the `min`/`max`
///   level is judged over every sample of the run instead (see
///   [`host_paced_level_claim`]).
/// - `rail_window` on a firmware-driven net: a `scenario` window and the dip,
///   spike, recovery and settling time bounds. The level is judged over the
///   whole run, as for `voltage`.
///
/// Every bound on a firmware-driven net is equally host-dependent on a net
/// downstream of one (see [`firmware_net`]): a load switch's output, a
/// regulator's rail through its enable. Such a bound names the net and the
/// driven net it follows.
///
/// Analog-only nets keep hard verdicts even on a board with a host-paced core:
/// the solver's own time base is exact. Cycle-counting backends (simavr) and
/// Renode, which advances virtual time per executed instruction, never land
/// here.
pub(crate) fn host_paced_time_bound(a: &Assertion, out: &RunOutcome) -> Option<String> {
    if out.host_paced_mcus.is_empty() {
        return None;
    }
    match a.kind.as_str() {
        "boot_coverage" | "boot-coverage" => {
            let deadline = a.deadline_ms.unwrap_or(0.0);
            let hold = match a.hold_ms {
                None => " and the hold through it".to_string(),
                Some(h) if h > 0.0 => format!(" and the {h} ms hold"),
                Some(_) => String::new(),
            };
            Some(format!("the {deadline} ms boot deadline{hold}"))
        }
        "uart" if host_paced_reads_text(a, out) => Some(format!(
            "the {} ms window the text had to appear in",
            out.sim_ms
        )),
        "peripheral" if peripheral_reads_content(a, out) => Some(format!(
            "the {} ms window the peripheral state had to be reached in",
            out.sim_ms
        )),
        _ => {
            let fw = firmware_net(a, out)?;
            let bound = net_time_bound(a)?;
            Some(match (fw.of, fw.pad) {
                (None, None) => bound,
                (Some(of), None) => {
                    format!(
                        "{bound} on '{}', downstream of firmware-driven '{of}'",
                        fw.net
                    )
                }
                (_, Some(pad)) => format!("{bound} on '{}', {}", fw.net, fw.pad_clause(pad)),
            })
        }
    }
}

/// How the firmware moves the net an assertion reads.
struct FirmwareNet<'a> {
    net: &'a str,
    /// The firmware-driven net `net` follows through the board
    /// (`RunOutcome::influenced_nets`), or `None` when an MCU pin drives
    /// `net` itself (`RunOutcome::driven_nets`).
    of: Option<&'a str>,
    /// The MCU pad whose unmodelled peripheral output drives, or may drive,
    /// the net (`RunOutcome::peripheral_pad_nets`).
    pad: Option<&'a PeripheralPad>,
}

impl FirmwareNet<'_> {
    /// How `pad` moves the net, completing "'NET' ...": the pad itself or a
    /// net downstream of it, and what drives it unmodelled.
    fn pad_clause(&self, pad: &PeripheralPad) -> String {
        let place = if pad.net == self.net {
            format!("MCU {} pad {}", pad.mcu, pad.pin)
        } else {
            format!(
                "downstream of MCU {} pad {} ('{}')",
                pad.mcu, pad.pin, pad.net
            )
        };
        pad.unobserved.as_ref().map_or_else(
            || {
                format!(
                    "{place}, driven by a peripheral output the firmware routed there, whose \
                     waveform the co-simulation does not model"
                )
            },
            |why| {
                format!(
                    "{place}, which the firmware may drive through a peripheral output (PWM, \
                     timer, RMT, a bus with no attached model): the backend could not observe \
                     peripheral routing on this pad ({why})"
                )
            },
        )
    }
}

/// Whether the firmware moves `a`'s net, directly or through the board, or,
/// for a level judged in a window, may move it through a peripheral output
/// the backend could not observe.
fn firmware_net<'a>(a: &'a Assertion, out: &'a RunOutcome) -> Option<FirmwareNet<'a>> {
    let net = a.net.as_deref()?;
    let pad_on = |n: &str| out.peripheral_pad_nets.get(n).filter(|p| p.net == n);
    if out.driven_nets.contains(net) {
        let pad = pad_on(net);
        return Some(FirmwareNet { net, of: None, pad });
    }
    if let Some(of) = out.influenced_nets.get(net) {
        let pad = pad_on(of);
        return Some(FirmwareNet {
            net,
            of: Some(of),
            pad,
        });
    }
    level_window(a)?;
    let pad = out
        .peripheral_pad_nets
        .get(net)
        .filter(|p| p.unobserved.is_some())?;
    let of = (pad.net != net).then_some(pad.net.as_str());
    Some(FirmwareNet {
        net,
        of,
        pad: Some(pad),
    })
}

/// The time bound a per-net kind carries that compares a firmware time, as a
/// noun phrase, when its net is one the firmware moves (see
/// [`host_paced_time_bound`]).
fn net_time_bound(a: &Assertion) -> Option<String> {
    match a.kind.as_str() {
        "toggle" => match (a.min_toggles, a.freq_hz) {
            (Some(n), _) if n > 1 => Some(format!("the {n}-toggle minimum over the run")),
            (None, Some(f)) => Some(format!("the {f} Hz rate band")),
            _ => None,
        },
        "pulse_width" if a.min.is_some() || a.max.is_some() => {
            Some("the pulse-width bound".to_string())
        }
        "logic_level" => {
            let width = a.min_width_ms.map(|w| format!("the {w} ms hold width"));
            let pulses = a
                .min_toggles
                .filter(|n| *n > 1)
                .map(|n| format!("the {n}-pulse minimum over the run"));
            match (width, pulses) {
                (Some(w), Some(p)) => Some(format!("{w} and {p}")),
                (w, p) => w.or(p),
            }
        }
        "voltage" => a
            .after_ms
            .filter(|t| *t > 0.0)
            .map(|t| format!("the {t} ms after_ms window")),
        "rail_window" => rail_window_time_bound(a),
        _ => None,
    }
}

/// Assertions whose verdict on a host-paced clock is about content the firmware
/// produced (what it printed, what it wrote to a bus peripheral, the level it
/// held a net at from a window start on) rather than about a measured time.
/// Their pass stands on the content and discloses the time bound; their miss is
/// unjudged (see [`host_paced_unjudged_reason`]).
fn unjudged_when_missed(a: &Assertion) -> bool {
    matches!(a.kind.as_str(), "uart" | "peripheral") || level_window(a).is_some()
}

/// The simulated-time window a `voltage` or `rail_window` level is read in, as
/// the words that complete "judged ...": `after 50 ms`, `in the scenario 'load'
/// window`. `None` when the level is read over the whole run.
fn level_window(a: &Assertion) -> Option<String> {
    match a.kind.as_str() {
        "voltage" => a
            .after_ms
            .filter(|t| *t > 0.0)
            .map(|t| format!("after {t} ms")),
        "rail_window" => a
            .scenario
            .as_ref()
            .map(|id| format!("in the scenario '{id}' window")),
        _ => None,
    }
}

/// The time bounds a `rail_window` carries, as one noun phrase: its scenario
/// window and its [`rail_budgets`].
fn rail_window_time_bound(a: &Assertion) -> Option<String> {
    let window = a
        .scenario
        .as_ref()
        .map(|id| format!("the scenario '{id}' window"));
    let budgets = match rail_budgets(a).as_slice() {
        [] => None,
        [one] => Some(format!("the {one} bound")),
        many => Some(format!("the {} bounds", many.join(" and "))),
    };
    match (window, budgets) {
        (Some(w), Some(b)) => Some(format!("{w} and {b}")),
        (w, b) => w.or(b),
    }
}

/// The `rail_window` time budgets complete enough to be checked (the same
/// pairings [`rail_durations`] reads), by name.
fn rail_budgets(a: &Assertion) -> Vec<&'static str> {
    [
        (a.dip_below.and(a.for_max_ms), "dip duration"),
        (
            a.dip_below.and(a.recover_to).and(a.recover_within_ms),
            "recovery time",
        ),
        (a.spike_above.and(a.spike_for_max_ms), "spike duration"),
        (
            a.spike_above.and(a.settle_to).and(a.settle_within_ms),
            "settling time",
        ),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.map(|_| name))
    .collect()
}

/// The level claim left to judge when a host-paced clock leaves a `voltage` or
/// `rail_window` assertion's time bound unjudged (see
/// [`host_paced_time_bound`]): its `min`/`max` as a plain `voltage` assertion
/// with no window, so it is read over every sample of the run. `None` when the
/// assertion is not one of those kinds or every bound it carries is
/// host-independent here.
///
/// The firmware's progress at any simulated time depends on the host, so a
/// window that starts at a simulated time (an `after_ms`, a scenario) may start
/// before or after the firmware event it stands for on a real chip. Only a level
/// that holds on every sample, wherever the window falls, is a pass no host speed
/// can flip.
fn host_paced_level_claim(a: &Assertion, out: &RunOutcome) -> Option<Assertion> {
    level_window(a)?;
    host_paced_time_bound(a, out)?;
    Some(Assertion {
        kind: "voltage".to_string(),
        net: a.net.clone(),
        min: a.min,
        max: a.max,
        ..Assertion::default()
    })
}

/// [`check_voltage`] on a [`host_paced_level_claim`], its detail saying the
/// level was read over the whole run rather than over the unjudged window.
fn check_level_over_run(level: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let (ok, detail, why) = check_voltage(level, out);
    let net = level.net.as_deref().unwrap_or_default();
    let detail = detail.strip_prefix(&format!("{net}: ")).map_or_else(
        || detail.clone(),
        |rest| format!("{net} over the whole {} ms run: {rest}", out.sim_ms),
    );
    (ok, detail, why)
}

/// Why a `voltage` or `rail_window` assertion on a firmware-driven net cannot be
/// judged on a host-paced member, or `None` when it can: its
/// [`host_paced_level_claim`] holds over the whole run, or it has no window and
/// its level is read over the whole run anyway (an unscoped `rail_window`, whose
/// failing level is as hard a FAIL as a `voltage` with no `after_ms`).
fn host_paced_level_reason(a: &Assertion, out: &RunOutcome, frame_ms: f64) -> Option<String> {
    if !matches!(a.kind.as_str(), "voltage" | "rail_window") {
        return None;
    }
    host_paced_time_bound(a, out)?;
    let bound = net_time_bound(a)?;
    let net = a.net.as_deref().unwrap_or_default();
    let mcus = out.host_paced_mcus.join(", ");
    let fw = firmware_net(a, out)?;
    let driver = match (fw.of, fw.pad) {
        (None, None) => format!(
            "'{net}' is driven by firmware on MCU {mcus}, which runs on a host-paced clock, \
             {HOST_PACED_WHY}"
        ),
        (Some(of), None) => format!(
            "'{net}' is downstream of firmware-driven '{of}' (driven by firmware on MCU \
             {mcus}, which runs on a host-paced clock, {HOST_PACED_WHY})"
        ),
        (_, Some(pad)) => format!(
            "'{net}' is {}, and MCU {mcus} runs on a host-paced clock, {HOST_PACED_WHY}",
            fw.pad_clause(pad)
        ),
    };
    if a.min.is_none() && a.max.is_none() && !rail_budgets(a).is_empty() {
        // Nothing but time budgets: with them unjudged nothing is left, and a
        // "measured" pass would claim what the run cannot.
        return Some(format!(
            "host-paced clock: {driver}, so {bound} cannot be judged, and that is every bound \
             this assertion carries: nothing is left to judge. It is neither a pass nor a \
             fail. Add a min or max level, or judge the timing on a cycle-exact backend \
             (simavr or Renode) or on hardware."
        ));
    }
    let cause =
        format!("{driver}, so where {bound} falls in the firmware's progress depends on the host");
    let level = host_paced_level_claim(a, out)?;
    let window = level_window(a)?;
    let (ok, over_run, _) = check_level_over_run(&level, out);
    if ok {
        return None;
    }
    let (in_window, windowed, _) = check_kind(&without_time_bound(a), out, frame_ms);
    let what = if in_window {
        format!(
            "Judged {window}, the level holds ({windowed}), but only because the window leaves \
             out an excursion earlier in the run ({over_run})"
        )
    } else {
        format!(
            "Judged {window}, the level does not hold ({windowed}), and on a real chip that \
             window may start after the firmware has brought the net into range"
        )
    };
    Some(format!(
        "host-paced clock: {cause}. {what}. That is not evidence the board or firmware is \
         wrong and it is not a pass, so it is not judged. Assert the level over the whole run \
         (drop the window), assert the drive with boot_coverage (its deadline is disclosed, \
         not judged), or judge it on a cycle-exact backend (simavr or Renode) or on hardware."
    ))
}

/// Whether a `uart` assertion reads text from an MCU on a host-paced clock: the
/// named MCU when `mcu` is set, else any MCU on the board (the assertion reads
/// every MCU's output together).
fn host_paced_reads_text(a: &Assertion, out: &RunOutcome) -> bool {
    a.mcu.as_ref().map_or_else(
        || !out.host_paced_mcus.is_empty(),
        |m| out.host_paced_mcus.contains(m),
    )
}

/// Whether a `peripheral` check reaches a content comparison (EEPROM bytes, or a
/// state field the peripheral has) rather than failing for a reason no host
/// speed changes: a bus never exercised, no such peripheral, no such field.
fn peripheral_reads_content(a: &Assertion, out: &RunOutcome) -> bool {
    let id = a.id.clone().unwrap_or_default();
    if out.host_paced_mcus.is_empty() || out.unexercised_bus_ids.contains(&id) {
        return false;
    }
    out.peripherals.get(&id).is_some_and(|snap| {
        a.bytes.is_some()
            || a.field
                .as_ref()
                .is_some_and(|f| snap.fields.contains_key(f))
    })
}

/// The reason a content assertion's MISS cannot be judged on a host-paced MCU
/// clock, or `None` when it is judged (content present, a non-host-paced run, or
/// a failure no host speed changes).
///
/// A run on such a clock covers a host-dependent amount of firmware in its
/// simulated window (the same boot has measured about 2x apart on two machines),
/// so text the firmware had not printed by the end of the run may be text a real
/// chip prints in time: neither a PASS nor a FAIL. It is INVALID, the same
/// "cannot be judged" the code already gives a held-stale analog window.
///
/// Nothing a run reports today proves the firmware had run to completion (a
/// parked program counter is also where an idle `FreeRTOS` core waiting out a
/// long `delay` sits), so no miss is promoted back to a FAIL on a host-paced
/// clock; simavr and Renode count their own time and keep the hard verdict.
fn host_paced_unjudged_reason(a: &Assertion, out: &RunOutcome) -> Option<String> {
    let (what, detail) = match a.kind.as_str() {
        "uart" if host_paced_reads_text(a, out) && uart_missing(a, out) => {
            ("the UART text", check_uart(a, out).1)
        }
        "peripheral" if peripheral_reads_content(a, out) => {
            let (ok, detail) = check_peripheral(a, out);
            if ok {
                return None;
            }
            ("the peripheral state", detail)
        }
        _ => return None,
    };
    Some(format!(
        "host-paced clock: {what} was not there at the end of the {} ms run, but MCU {} runs \
         on a host-paced clock, {HOST_PACED_WHY}, so a slower host may simply not have run \
         the firmware that far in this window. That is not evidence the firmware is wrong and \
         it is not a pass, so it is not judged ({detail}). Raise duration_ms until the \
         firmware is expected to be there with room to spare, or judge it on a cycle-exact \
         backend (simavr or Renode) or on hardware.",
        out.sim_ms,
        out.host_paced_mcus.join(", ")
    ))
}

/// `a` with the bound [`host_paced_time_bound`] names taken out, leaving the
/// host-independent claim: a toggle count becomes "toggled at all", a rate band
/// becomes a measured rate, a width bound becomes a measured width, a hold width
/// becomes a plain level, and a rail window keeps its level bounds without its
/// time budgets.
fn without_time_bound(a: &Assertion) -> Assertion {
    let mut a = a.clone();
    match a.kind.as_str() {
        "toggle" if a.min_toggles.is_some() => a.min_toggles = Some(1),
        "toggle" => a.freq_hz = None,
        "pulse_width" => (a.min, a.max) = (None, None),
        "rail_window" => {
            a.for_max_ms = None;
            a.recover_within_ms = None;
            a.spike_for_max_ms = None;
            a.settle_within_ms = None;
        }
        "logic_level" => {
            a.min_width_ms = None;
            a.min_toggles = a.min_toggles.map(|_| 1);
        }
        _ => {}
    }
    a
}

/// Dispatch one assertion kind's check (see [`check_seed`]).
fn check_kind(a: &Assertion, out: &RunOutcome, frame_ms: f64) -> (bool, String, Option<String>) {
    // Adapter for the kinds whose checks carry no separate `why`.
    fn plain((ok, detail): (bool, String)) -> (bool, String, Option<String>) {
        (ok, detail, None)
    }
    match a.kind.as_str() {
        "voltage" => check_voltage(a, out),
        "pulse_width" => check_pulse_width(a, out, frame_ms),
        "uart" => plain(check_uart(a, out)),
        "toggle" => check_toggle(a, out, frame_ms),
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
        "logic_level" | "fires" => crate::search::check(a, out),
        "reaches" => check_reaches(a, out),
        // Unreachable while `bench_gate` refuses them first; a kind that gets here
        // without a bench record must not read as judged.
        "bus" | "resets" | "uart_baud" => (
            false,
            format!(
                "{NEEDS_BENCH_DATA} the {} assertion has no bench record to read",
                a.kind
            ),
            None,
        ),
        other => (false, format!("unknown assertion kind '{other}'"), None),
    }
}

/// `model_coverage`: hold the line on how much of the board bound to a real model.
///
/// The failure text names the parts rather than only the shortfall, because the
/// list IS the user's next action: each name is either a model to write (the
/// extending guides take one TOML file) or a part whose vendor keeps its model
/// encrypted, and those two need different responses.
// Part counts are far below 2^52, so usize -> f64 is exact.
#[allow(clippy::cast_precision_loss)]
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
                    "active ICs bound {} ({}), floor {}",
                    bind.critical_parts_bound,
                    format_percent(got * 100.0, 1),
                    format_percent(floor * 100.0, 1)
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
                    "parts bound {}/{} ({}), floor {}",
                    bind.resolved,
                    bind.non_ignored,
                    format_percent(got * 100.0, 1),
                    format_percent(floor * 100.0, 1)
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

/// The pass/fail accumulator behind every multi-clause check. Each clause
/// contributes one detail fragment; a failing clause is MARKED (`<- FAILED
/// HERE`) so a two-clause failure reads at a glance, and may name the observed
/// shortfall for the report's `why:` line, the number the fix has to close.
struct Verdict {
    ok: bool,
    parts: Vec<String>,
    whys: Vec<String>,
}

impl Verdict {
    const fn new() -> Self {
        Self {
            ok: true,
            parts: Vec::new(),
            whys: Vec::new(),
        }
    }

    fn pass(&mut self, part: String) {
        self.parts.push(part);
    }

    /// A failing clause: `part` is the detail fragment as it should print,
    /// `why` the observed shortfall (`None` when the fragment already is the
    /// diagnosis).
    fn fail(&mut self, part: String, why: Option<String>) {
        self.ok = false;
        self.parts.push(part);
        self.whys.extend(why);
    }

    /// One clause judged against a bound: `pass_part` when `held`, otherwise
    /// `fail_part` marked and `why` recorded.
    fn clause(&mut self, held: bool, pass_part: String, fail_part: String, why: String) {
        if held {
            self.pass(pass_part);
        } else {
            self.fail(fail_part + " <- FAILED HERE", Some(why));
        }
    }

    /// The judged clauses, joined. With no clause at all nothing was bounded,
    /// which is the measurement case: say so where a verdict would have gone.
    fn parts(&self) -> String {
        if self.parts.is_empty() {
            return "measured".to_string();
        }
        self.parts.join(", ")
    }

    fn finish(self, detail: String) -> (bool, String, Option<String>) {
        let why = (!self.whys.is_empty()).then(|| self.whys.join("; "));
        (self.ok, detail, why)
    }
}

/// A `(false, reason, None)` verdict for a check that never got data.
const fn no_data(reason: String) -> (bool, String, Option<String>) {
    (false, reason, None)
}

/// `rail_window`: judge a rail's behaviour over a scenario window: min/max bounds,
/// dip/spike duration, and recovery/settling time.
fn check_rail_window(
    a: &crate::spec::Assertion,
    out: &RunOutcome,
) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let scope = a.scenario.clone().unwrap_or_default();
    let Some(win) = out.rail_windows.get(&(scope.clone(), net.clone())) else {
        return no_data(format!(
            "net '{net}' was never sampled in scenario window '{scope}'"
        ));
    };
    if win.samples.is_empty() {
        return no_data(format!("net '{net}' had no samples in the window"));
    }

    // Same marking discipline as check_voltage: the failing clause carries
    // `<- FAILED HERE`, the passing clauses stay un-annotated, and the why
    // names the observed excess (volts of sag/spike, ms over a duration or
    // recovery/settling budget).
    let mut v = Verdict::new();
    if let Some(lo) = a.min {
        let lo_v = format_volts(lo);
        v.clause(
            win.min_v >= lo - 1e-6,
            format!("min={} (>= {lo_v})", format_volts(win.min_v)),
            format!("min={} < required {lo_v}", format_volts(win.min_v)),
            format!(
                "{net} sagged to {} in the window, {} below your {lo_v} floor",
                format_volts(win.min_v),
                format_volts(lo - win.min_v)
            ),
        );
    }
    if let Some(hi) = a.max {
        let hi_v = format_volts(hi);
        v.clause(
            win.max_v <= hi + 1e-6,
            format!("max={} (<= {hi_v})", format_volts(win.max_v)),
            format!("max={} > allowed {hi_v}", format_volts(win.max_v)),
            format!(
                "{net} rose to {} in the window, {} above your {hi_v} ceiling",
                format_volts(win.max_v),
                format_volts(win.max_v - hi)
            ),
        );
    }
    // A duration needs at least two samples to measure (windows(2) yields
    // nothing from one point, so dip_duration_s folds to 0 and silently
    // auto-passes). A window that spans less than one frame is degenerate:
    // fail loudly rather than claim a timing spec we could not evaluate.
    for (what, ms, budget, why) in rail_durations(a, win, &net).into_iter().flatten() {
        if win.samples.len() < 2 {
            v.fail(
                format!(
                    "{what}: window has {} sample(s), too few to measure a duration",
                    win.samples.len()
                ),
                None,
            );
            continue;
        }
        v.clause(
            ms <= budget + 1e-6,
            format!("{what} {ms:.2}ms (<= {budget}ms)"),
            format!("{what} {ms:.2}ms > allowed {budget}ms"),
            why,
        );
    }
    let detail = format!(
        "{net} window: {} [min={} max={}]",
        v.parts(),
        format_volts(win.min_v),
        format_volts(win.max_v)
    );
    v.finish(detail)
}

/// The rail window's configured duration clauses, each as `(fragment, measured
/// ms, budget ms, excess sentence)`.
///
/// The four duration clauses are one shape: a measured interval in ms, the
/// budget it must fit, the fragment that names it, and the sentence that
/// names the excess. `dip`/`spike` bound how long the rail sat out of band;
/// `recover`/`settle` bound how long it took to come back.
fn rail_durations(
    a: &crate::spec::Assertion,
    win: &crate::scenarios::RailWindow,
    net: &str,
) -> [Option<(String, f64, f64, String)>; 4] {
    [
        a.dip_below.zip(a.for_max_ms).map(|(d, budget)| {
            let ms = win.dip_duration_s(d) * 1000.0;
            let d = format_volts(d);
            (
                format!("dip<{d} for"),
                ms,
                budget,
                format!(
                    "{net} sat below {d} for {ms:.2} ms, {:.2} ms longer than your budget",
                    ms - budget
                ),
            )
        }),
        a.dip_below
            .zip(a.recover_to)
            .zip(a.recover_within_ms)
            .map(|((d, r), budget)| {
                let ms = win.recovery_s(d, r) * 1000.0;
                let (d, r) = (format_volts(d), format_volts(r));
                (
                    format!("recover-to-{r} in"),
                    ms,
                    budget,
                    format!(
                        "{net} took {ms:.2} ms to climb back to {r} after dipping below \
                 {d}, {:.2} ms past your recovery deadline",
                        ms - budget
                    ),
                )
            }),
        a.spike_above.zip(a.spike_for_max_ms).map(|(sp, budget)| {
            let ms = win.spike_duration_s(sp) * 1000.0;
            let sp = format_volts(sp);
            (
                format!("spike>{sp} for"),
                ms,
                budget,
                format!(
                    "{net} stayed above {sp} for {ms:.2} ms, {:.2} ms longer than your budget",
                    ms - budget
                ),
            )
        }),
        a.spike_above
            .zip(a.settle_to)
            .zip(a.settle_within_ms)
            .map(|((sp, settle), budget)| {
                let ms = win.settling_s(sp, settle) * 1000.0;
                let (sp, settle) = (format_volts(sp), format_volts(settle));
                (
                    format!("settle-to-{settle} in"),
                    ms,
                    budget,
                    format!(
                        "{net} took {ms:.2} ms to settle to {settle} after spiking above \
                 {sp}, {:.2} ms past your settling deadline",
                        ms - budget
                    ),
                )
            }),
    ]
}

/// `protection_trip`: whether a supply net's battery protection latched.
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
    let Some(t) = tripped else {
        let why = format!("{net} has no battery protection supply leg (nothing to trip)");
        return (false, why);
    };
    (
        t == want,
        format!(
            "{net} protection {} (expected {})",
            if t { "TRIPPED" } else { "held" },
            if want { "trip" } else { "no trip" }
        ),
    )
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
///
/// On a host-paced MCU clock the deadline is not judged at all (see
/// [`boot_coverage_host_paced`]).
fn check_boot_coverage(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    host_paced_time_bound(a, out).map_or_else(
        || boot_coverage_against_deadline(a, out),
        |bound| boot_coverage_host_paced(a, out, &bound),
    )
}

/// [`check_boot_coverage`] on a clock whose times are the same on every host.
fn boot_coverage_against_deadline(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let level = a.min.unwrap_or(0.0);
    let level_v = format_volts(level);
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
                "control net '{net}' first reached {level_v} at {tc:.2} ms, past the {deadline} ms boot deadline"
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
                                    "control net '{net}' reached {level_v} at {tc:.2} ms but fell back below it at {td:.2} ms, before the {deadline} ms boot deadline"
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
                                    "control net '{net}' reached {level_v} at {tc:.2} ms but held it only \
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
                                "control net '{net}' reached {level_v} at {tc:.2} ms, but its {h} ms hold \
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
                format!("control net '{net}' driven to >= {level_v} at {tc:.2} ms (<= {deadline} ms), {hold_note}"),
            )
        }
    }
}

/// Boot coverage when the firmware's time is measured on a host-paced MCU
/// clock (see [`host_paced_time_bound`]).
///
/// Only the host-independent half is judged: whether the firmware drove the
/// net to its level at all during the run. That is what the undriven negative
/// control rests on, and no host speed turns a firmware that never drives the
/// pin into one that does. The deadline, the hold measured against it, and the
/// order of an early stress fault against the drive all compare a host-paced
/// time, so they are reported with the measured time marked approximate and
/// left to the evidence condition, never issued as a PASS or FAIL a faster or
/// slower machine would flip.
fn boot_coverage_host_paced(a: &Assertion, out: &RunOutcome, bound: &str) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let level = a.min.unwrap_or(0.0);
    let level_v = format_volts(level);
    let key = (net.clone(), level.to_bits());
    let mcus = out.host_paced_mcus.join(", ");
    let Some(tc) = out.boot_first_cross_ms.get(&key).copied() else {
        let never = boot_below_threshold_msg(
            &net,
            level,
            out.driven_nets.contains(&net),
            out.drive_direction_observable,
            boot_observed_range(&net, out),
        );
        return (
            false,
            format!(
                "{never} (the {:.0} ms run is host-paced time on MCU {mcus}: a slower host \
                 runs less of the firmware in it, so a drive that comes late may need a \
                 longer duration_ms)",
                out.sim_ms
            ),
        );
    };
    let mut detail = format!(
        "control net '{net}' driven to >= {level_v} at ~{tc:.0} ms (approximate){}",
        host_paced_note(bound, out)
    );
    if let Some(td) = out.boot_drop_after_cross_ms.get(&key) {
        let _ = write!(detail, "; it fell back below {level_v} at ~{td:.0} ms");
    }
    if let Some(ft) = out.first_fault_ms.filter(|ft| *ft < tc - 1e-9) {
        let _ = write!(
            detail,
            "; a stress fault fired at {ft:.2} ms, before the drive (their order is not \
             judged on a host-paced clock)"
        );
    }
    (true, detail)
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
///     `SoC` descriptor reads MODER/CRL+CRH/DIR, either way a held-LOW pin is
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
    let level_v = format_volts(level);
    let range = observed
        .map(|(lo, hi)| {
            format!(
                "; observed range [{}, {}]",
                format_volts(lo),
                format_volts(hi)
            )
        })
        .unwrap_or_default();
    if driven {
        format!("control net '{net}' was driven but never exceeded {level_v}{range}")
    } else if drive_direction_observable {
        format!(
            "control net '{net}' was never driven to >= {level_v} (firmware left it Hi-Z / undefined through the whole run){range}"
        )
    } else {
        format!(
            "control net '{net}' never reached {level_v}{range}; the backend cannot report pin drive direction, so the pin may be undriven (Hi-Z / undefined) or driven LOW"
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
        if !h.len().is_multiple_of(2) || h.is_empty() {
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
    // Unexercised-bus refusal: this peripheral was bound on a
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
                 controller to the SoC descriptor or drop this assertion."
            ),
        );
    }
    let Some(snap) = out.peripherals.get(&id) else {
        return (false, format!("peripheral '{id}' not found in run"));
    };
    // SPI framing-tier flag: a heuristic-framed bus guesses
    // transaction boundaries at chunk edges (merges/truncates transactions),
    // so any verdict about this peripheral carries that caveat in its detail,
    // in the report itself, not a code comment.
    let framing_flag = match out.spi_framing.get(&id).map(String::as_str) {
        Some("heuristic") => {
            " [SPI framing: HEURISTIC; transaction boundaries guessed at chunk \
             edges; two transactions in one chunk merge and a boundary-spanning \
             one is truncated. Declare cs_net, or point `ref` at a modelled part, \
             for exact framing]"
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
        return no_data(format!(
            "net '{net}' was never sampled (no window at {thr}ms)"
        ));
    };
    if win.samples == 0 {
        return no_data(format!("net '{net}' had no samples after {thr}ms"));
    }
    // For a >= bound the worst case is the minimum the rail dipped to in the
    // window; for a <= bound, the maximum it rose to. The `why` names the
    // observed shortfall in volts, which is the number the fix has to close.
    let mut v = Verdict::new();
    // The two bounds differ only in the direction they are breached and the
    // words that name that direction, so `sign` picks both and one signed
    // distance drives the arithmetic: positive means the sample is past the
    // bound, whichever bound it is.
    for (limit, worst, sign) in [(a.min, win.min_v, -1.0_f64), (a.max, win.max_v, 1.0)] {
        let Some(limit) = limit else { continue };
        // (label, comparison, breach, excursion verb, side, noun)
        let (label, cmp, breach, verb, side, noun) = if sign < 0.0 {
            (
                "min",
                ">=",
                "< required",
                "dipped to",
                "below your",
                "floor",
            )
        } else {
            ("max", "<=", "> allowed", "rose to", "above your", "ceiling")
        };
        let signed = |v: f64| sign * (v - limit);
        let limit_v = format_volts(limit);
        let why = if signed(win.last_v) <= 1e-6 {
            format!(
                "{net} {verb} {}, {} {side} {limit_v} {noun}, \
                 before settling back to {}",
                format_volts(worst),
                format_volts(signed(worst)),
                format_volts(win.last_v)
            )
        } else {
            format!(
                "{net} settled {} {side} {noun} ({} vs {label} {limit_v})",
                format_volts(signed(win.last_v)),
                format_volts(win.last_v)
            )
        };
        v.clause(
            signed(worst) <= 1e-6,
            format!("{label}={} ({cmp} {limit_v})", format_volts(worst)),
            format!("{label}={} {breach} {limit_v}", format_volts(worst)),
            why,
        );
    }
    if a.min.is_none() && a.max.is_none() {
        // A measurement has no clause to word, and the run-final `settled`
        // level answers a different question than `after_ms` asked, so the
        // window's own span IS the number.
        v.pass(format!(
            "min={} max={} measured",
            format_volts(win.min_v),
            format_volts(win.max_v)
        ));
    }
    let when = if thr > 0.0 {
        format!(" after {thr}ms")
    } else {
        String::new()
    };
    let detail = format!(
        "{net}{when}: {} [settled {}]",
        v.parts(),
        format_volts(win.last_v)
    );
    v.finish(detail)
}

/// The component references an assertion judges. A `uart` claim is about what
/// one MCU printed, so its subject is that MCU (the spec's `mcu`, or every MCU
/// that produced UART output when none is named, the same streams
/// [`check_uart`] reads). Its evidence then covers the MCU's own nets (supply,
/// reset, clock and every pin it drives or reads) and an unmodelled part on any
/// of them still undermines it, instead of every unrelated open part on the
/// board doing so.
fn subject_refs_for(a: &Assertion, outcomes: &[RunOutcome]) -> Vec<String> {
    let mut refs: Vec<String> = a.reference.iter().cloned().collect();
    if a.kind == "uart" {
        let mcus: std::collections::BTreeSet<&String> = a.mcu.as_ref().map_or_else(
            || outcomes.iter().flat_map(|out| out.uart.keys()).collect(),
            |m| std::iter::once(m).collect(),
        );
        for mcu in mcus {
            if !refs.contains(mcu) {
                refs.push(mcu.clone());
            }
        }
    }
    refs
}

/// The text a `uart` assertion reads: the requested MCU's UART, or every MCU's
/// concatenated in a STABLE (sorted-by-key) order. Iterating a `HashMap`'s values
/// put the streams in nondeterministic order, so an anchored/boundary-spanning
/// match (`^BOOT`) flaked run to run.
fn uart_text(a: &Assertion, out: &RunOutcome) -> String {
    a.mcu.as_ref().map_or_else(
        || {
            let sorted: std::collections::BTreeMap<_, _> = out.uart.iter().collect();
            sorted.into_values().map(String::as_str).collect()
        },
        |m| out.uart.get(m).cloned().unwrap_or_default(),
    )
}

/// True when a well-formed `uart` assertion's text is not in the output (a bad
/// regex or an assertion with neither field is a spec error, not a miss).
fn uart_missing(a: &Assertion, out: &RunOutcome) -> bool {
    let text = uart_text(a, out);
    if let Some(needle) = &a.contains {
        return !text.contains(needle);
    }
    a.matches
        .as_ref()
        .and_then(|re| regex::Regex::new(re).ok())
        .is_some_and(|rx| !rx.is_match(&text))
}

fn check_uart(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let text = uart_text(a, out);
    let preview = truncate_ellipsis(&text.replace(['\r', '\n'], "·"), 60);

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
    let Some(re) = &a.matches else {
        return (false, "uart assertion had no contains/matches".into());
    };
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
}

fn check_toggle(a: &Assertion, out: &RunOutcome, frame_ms: f64) -> (bool, String, Option<String>) {
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
        // The grid the run sampled on: the runner floors `frame_ms` at 1 us.
        let dt_s = frame_ms.max(1e-3) / 1000.0;
        let series = out.net_series.get(&net).map_or(&[][..], Vec::as_slice);
        let (hz, measured) = match edge_rate(series, dt_s) {
            Ok(r) => r,
            Err(e) => {
                return (
                    false,
                    format!("{net}: {e}; no frequency measured"),
                    Some(format!(
                        "a rate is TIMED between two same-way edges. Either {net} barely \
                         switches, or duration_ms is too short for {freq} Hz periods"
                    )),
                );
            }
        };
        // Under ten samples a period, the grid is reporting itself, not a rate.
        let samples_per_period = 1.0 / (hz * dt_s);
        if samples_per_period < 10.0 {
            return (
                false,
                format!(
                    "{net}: {measured} is under-sampled ({samples_per_period:.1} samples \
                     per period); no frequency measured"
                ),
                Some(format!(
                    "{net} changes level every {samples_per_period:.1} samples, so the run \
                     cannot separate its rate from the sampling grid. Reduce frame_ms to at \
                     most {:.3e} (a tenth of the {freq} Hz period) and re-run",
                    100.0 / freq
                )),
            );
        }
        let tol = a.tolerance.unwrap_or(0.25);
        let (lo, hi) = (freq * (1.0 - tol), freq * (1.0 + tol));
        let ok = hz >= lo && hz <= hi;
        let why = (!ok).then(|| {
            format!(
                "{net} ran at {measured}, outside your {lo:.2}-{hi:.2} Hz band \
                 ({freq} Hz ±{})",
                format_percent(tol * 100.0, 0)
            )
        });
        return (
            ok,
            format!(
                "{net}: {measured} (want {freq} Hz ±{})",
                format_percent(tol * 100.0, 0)
            ),
            why,
        );
    }
    // No bound at all: report the net's own rate as a MEASUREMENT. The refusal
    // when there are too few edges to time still holds -- an unmeasurable rate
    // must not read as a measured one.
    let dt_s = frame_ms.max(1e-3) / 1000.0;
    let series = out.net_series.get(&net).map_or(&[][..], Vec::as_slice);
    match edge_rate(series, dt_s) {
        Ok((_, measured)) => (true, format!("{net}: {measured}"), None),
        Err(e) => no_data(format!("{net}: {e}; no frequency measured")),
    }
}

/// `pulse_width`: the width of the first COMPLETE pulse on `net` at or after
/// `after_ms`, high by default and low under `level = "low"`, in milliseconds.
///
/// The threshold and the interpolation are the frequency path's
/// ([`crate::hwtrace::detect_edges`] with the default rule), so a width and a
/// rate read off the same net agree, and the reported +- is the same grid
/// residual [`edge_rate`] quotes: one sample step at each end of the interval.
/// An incomplete pulse (no opening edge, or none closing it before the run
/// ends) is REFUSED, never truncated to the end of the run: a width the run did
/// not contain must not be reported, and a refusal cannot pass.
fn check_pulse_width(
    a: &Assertion,
    out: &RunOutcome,
    frame_ms: f64,
) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let dt_s = frame_ms.max(1e-3) / 1000.0;
    let t0 = a.after_ms.unwrap_or(0.0) / 1000.0;
    let series: Vec<(f64, f64)> = out
        .net_series
        .get(&net)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .copied()
        .filter(|&(t, _)| t >= t0)
        .collect();
    let opens = a.level.as_deref() != Some("low");
    let level = if opens { "high" } else { "low" };
    let edges = crate::hwtrace::detect_edges(&series, None);
    let width = edges
        .iter()
        .position(|e| e.rising == opens)
        .and_then(|i| {
            edges[i + 1..]
                .iter()
                .find(|e| e.rising != opens)
                .map(|e| e.t - edges[i].t)
        })
        .filter(|w| *w > 0.0);
    let Some(w) = width else {
        return no_data(format!(
            "{net}: no complete {level} pulse after {:.3} ms (edges seen: {}); \
             nothing to measure, so no width is reported",
            t0 * 1e3,
            edges.len()
        ));
    };
    let (ms, err) = (w * 1e3, 200.0 * dt_s / w);
    let mut v = Verdict::new();
    bound_clauses(
        &mut v,
        (a.min, a.max),
        ms,
        "ms",
        &format!("{net}'s {level} pulse"),
    );
    let detail = format!(
        "{net}: {level} pulse {ms:.4} ms +-{} (edge timing): {}",
        format_percent(err, 1),
        v.parts()
    );
    v.finish(detail)
}

/// Judge `value` against an optional `(min, max)` window, adding one clause per
/// bound present. With neither bound the [`Verdict`] keeps no clause at all,
/// which is exactly the measurement case its `parts()` words. The two sides
/// differ only in sign and wording, so both are written once.
fn bound_clauses(
    v: &mut Verdict,
    (lo, hi): (Option<f64>, Option<f64>),
    value: f64,
    unit: &str,
    subject: &str,
) {
    for (limit, label, cmp, sign, side) in [
        (lo, "min", ">=", -1.0_f64, "short of your"),
        (hi, "max", "<=", 1.0, "over your"),
    ] {
        let Some(limit) = limit else { continue };
        let past = sign * (value - limit);
        v.clause(
            past <= 1e-6,
            format!("{label}={limit}{unit} ({cmp})"),
            format!("{value:.4}{unit} {cmp} {limit}{unit} does NOT hold"),
            format!("{subject} measured {value:.4} {unit}, {past:.4} {unit} {side} {limit} {unit} bound"),
        );
    }
}

/// Time a net's rising threshold crossings and divide the whole periods by the
/// span from the first such edge to the last. Returns the rate in Hz AND the
/// one phrase every surface prints for it, resolution included, so text, JSON
/// and the pass/fail comparison agree. It deliberately does NOT count level
/// changes and divide by the run: that charged the rate for the partial periods
/// at BOTH ends and for a start-up cycle unlike the rest (a 555's first charge
/// runs to ln 3, not ln 2: 4% low on a board right to 0.3%, needing 7% to pass).
// A period count is far below 2^52, so usize -> f64 is exact.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn edge_rate(series: &[(f64, f64)], dt_s: f64) -> Result<(f64, String), String> {
    let edges = crate::hwtrace::detect_edges(series, None);
    let rising: Vec<f64> = edges.iter().filter(|e| e.rising).map(|e| e.t).collect();
    let periods = rising.len().saturating_sub(1);
    let span = rising.last().copied().unwrap_or(0.0) - rising.first().copied().unwrap_or(0.0);
    if span <= 0.0 {
        return Err(format!(
            "{} rising edge(s), too few to measure a frequency from (2 make a period)",
            rising.len()
        ));
    }
    // The +- is the grid's, not a guess: one sample step of residual timing
    // error at each end of the span, which interpolation only ever beats.
    let (hz, err) = (periods as f64 / span, 200.0 * dt_s / span);
    Ok((
        hz,
        format!(
            "{hz:.1} Hz ±{} (edge timing, {periods} periods)",
            format_percent(err, 1)
        ),
    ))
}

/// `phase_margin`: the loop's phase margin (degrees) at gain crossover must lie in
/// the requested bound. The loop gain is read at `net` from the shared AC sweep.
fn check_phase_margin(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let Some(ac) = &out.ac else {
        return no_data("no AC analysis ran (missing [ac] block)".into());
    };
    let Some(m) = ac.margins.get(&net) else {
        return no_data(format!("net '{net}' produced no loop-stability margins"));
    };
    let Some(pm) = m.phase_margin_deg else {
        return no_data(format!(
            "loop '{net}' never crosses 0 dB in the swept band (no gain crossover; DC loop gain {:.1} dB)",
            m.dc_gain_db
        ));
    };
    let fc = m.gain_crossover_hz.unwrap_or(f64::NAN);
    let mut v = Verdict::new();
    bound_clauses(
        &mut v,
        (a.min, a.max),
        pm,
        "deg",
        &format!("loop {net}'s phase margin"),
    );
    let detail = format!(
        "loop {net}: phase margin {pm:.2} deg at fc={fc:.4} Hz ({})",
        v.parts()
    );
    v.finish(detail)
}

/// `ac_gain`: the magnitude (dB) at `net` must lie in the requested bound, at the
/// frequency `freq_hz` (interpolated) or, if absent, over the whole sweep.
fn check_ac_gain(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let net = a.net.clone().unwrap_or_default();
    let Some(ac) = &out.ac else {
        return no_data("no AC analysis ran (missing [ac] block)".into());
    };
    let Some(bode) = ac.bode.get(&net) else {
        return no_data(format!("net '{net}' was not sampled by the AC sweep"));
    };
    if bode.is_empty() {
        return no_data(format!("net '{net}' has no AC data"));
    }
    let band_min = bode.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let band_max = bode.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);

    // Pick the gain to test: at a specific frequency (log-interpolated) or the
    // worst case over the whole band for the bound being checked. A requested
    // frequency outside the swept band cannot be measured: interp_db would
    // silently clamp to the nearest endpoint gain and report it AS IF taken at
    // the requested frequency, so it is refused instead. A non-finite freq_hz
    // (a `nan`/`inf` TOML literal) slips past the band bounds because every
    // comparison against NaN is false, so it gets its own refusal first.
    let (worst_lo, worst_hi, where_str) = match a.freq_hz {
        Some(f) if !f.is_finite() => {
            return no_data(format!(
                "ac_gain for '{net}' has a non-finite freq_hz ({f}); set a real frequency"
            ))
        }
        Some(f) => {
            let (f_lo, f_hi) = (bode[0].0, bode[bode.len() - 1].0);
            if f < f_lo * (1.0 - 1e-9) || f > f_hi * (1.0 + 1e-9) {
                return no_data(format!(
                    "ac_gain for '{net}' requested {f} Hz is outside the swept band \
                     {f_lo}-{f_hi} Hz; widen [ac] fstart/fstop or move the check in-band"
                ));
            }
            let db = interp_db(bode, f);
            (db, db, format!("at {f} Hz"))
        }
        None => (band_min, band_max, "over band".to_string()),
    };

    // The min is judged against the band's worst LOW and the max against its
    // worst HIGH, so each bound sees the sample that could break it.
    let mut v = Verdict::new();
    let subject = format!("{net} {where_str}");
    bound_clauses(&mut v, (a.min, None), worst_lo, "dB", &subject);
    bound_clauses(&mut v, (None, a.max), worst_hi, "dB", &subject);
    let detail = format!("{net} gain {where_str}: {}", v.parts());
    v.finish(detail)
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
        // Value and limit are in the fault kind's unit (W, C, A or V); a bare
        // "120.480 > 1.000" says neither what was measured nor in what.
        let kind = FaultKind::from_str(&f.kind);
        let unit = kind.unit();
        // Far past its rating the figure is an idealised model's number, not
        // the part's: the engine's one stress wording states it without one.
        let first = if is_far_past_rating(kind, f.value, f.limit) {
            stress_clause(&f.component, kind, f.value, f.limit, rating_noun(kind), "")
        } else {
            format!(
                "{} {} {:.3} {unit} > {:.3} {unit}",
                f.component, f.kind, f.value, f.limit
            )
        };
        (
            false,
            format!(
                "{} fault(s); first: {first} at {:.1}ms",
                out.faults.len(),
                f.t_ms
            ),
        )
    }
}

fn check_max_current(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let reference = a.reference.clone().unwrap_or_default();
    // No current data: the runner rejects a max_current on an untracked
    // component kind at bind time (`check_trackable_assert_refs`), so reaching
    // this branch means the tracked device never produced a sample; fail loud
    // rather than report a guard that was never evaluated as green.
    let Some((key, peak)) = peak_for_ref(&out.peak_current, &reference) else {
        return no_data(format!(
            "I({reference}): no current data was recorded for this component; \
             the guard was never evaluated, so it cannot be reported green"
        ));
    };
    let unit = unit_note("peak unit", &key, &reference);
    let peak_s = format_amps(peak);
    let Some(limit) = a.amps else {
        return (
            true,
            format!("I({reference}) peak {peak_s} measured{unit}"),
            None,
        );
    };
    let limit_s = format_amps(limit);
    if peak <= limit + 1e-9 {
        (
            true,
            format!("I({reference}) peak {peak_s} (<= {limit_s}){unit}"),
            None,
        )
    } else {
        (
            false,
            format!("I({reference}) peak {peak_s} > limit {limit_s} <- FAILED HERE{unit}"),
            Some(format!(
                "{reference} drew {peak_s} at peak, {} over your {limit_s} limit",
                format_amps(peak - limit)
            )),
        )
    }
}

/// The highest-valued entry of a per-device map that belongs to `reference`,
/// as `(key, value)`.
///
/// A multi-unit package stamps one device per unit, so a bare `.get(reference)`
/// would always miss and silently drop a safety assert into its no-data branch.
/// Ties break on the unit key so the reported unit is stable across `HashMap`
/// iteration order.
pub(crate) fn peak_for_ref(
    map: &std::collections::HashMap<String, f64>,
    reference: &str,
) -> Option<(String, f64)> {
    map.iter()
        .filter(|(k, _)| key_belongs_to_ref(reference, k))
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, v)| (k.clone(), *v))
}

/// `" (<noun> <key>)"` when the peak came from a package unit rather than the
/// named reference itself, and empty otherwise.
fn unit_note(noun: &str, key: &str, reference: &str) -> String {
    if key == reference {
        String::new()
    } else {
        format!(" ({noun} {key})")
    }
}

/// Does a per-device map key (or fault component name) belong to `reference`?
/// True for the bare ref itself and for any of its per-unit keys: a multi-unit
/// package stamps one device per unit with a `_q<N>` / `_s<N>` suffix
/// ("`U3_q2`", "`SW1_s0`"), so per-unit producers (`peak_temp_c`, faults) never
/// record the bare package ref. Mirrors the binder's suffix rule; the runner's
/// trackability gate uses this same test to accept a bare ref whose units are
/// monitored, so the consumers here match exactly the keys the gate admitted,
/// or a safety assert on a multi-unit package could never fail.
pub(crate) fn key_belongs_to_ref(reference: &str, key: &str) -> bool {
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

/// The `max_temp` result for an overtemperature fault `f` past the device's
/// own junction limit. Far past it the figure is no result: the engine's one
/// stress wording states it without the number.
fn over_device_max(f: &crate::runner::RunFault) -> (bool, String, Option<String>) {
    let kind = FaultKind::Overtemperature;
    if is_far_past_rating(kind, f.value, f.limit) {
        let said = stress_clause(&f.component, kind, f.value, f.limit, rating_noun(kind), "");
        return (
            false,
            format!(
                "Tj({}) exceeded device max at {:.1}ms: {said}",
                f.component, f.t_ms
            ),
            Some(said),
        );
    }
    (
        false,
        format!(
            "Tj({}) exceeded device max: {} > {} at {:.1}ms",
            f.component,
            format_celsius(f.value),
            format_celsius(f.limit),
            f.t_ms
        ),
        Some(format!(
            "{} ran {} past its own {} max junction temperature at {:.1} ms",
            f.component,
            format_celsius(f.value - f.limit),
            format_celsius(f.limit),
            f.t_ms
        )),
    )
}

/// `max_temp`: the steady-state junction temperature of `ref` must stay at or
/// below `celsius` (if given) or below the device's own max junction temp (if
/// not, in which case we lean on whether an overtemperature fault fired).
///
/// Both the peak lookup and the fault match aggregate over the package's units
/// (`key_belongs_to_ref`): `peak_temp_c` and the fault list are keyed by the
/// stamped device name, which for a multi-unit package is the per-unit
/// `SW1_q1` / `SW1_s0` form, never the bare `SW1` the assertion names.
fn check_max_temp(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let reference = a.reference.clone().unwrap_or_default();
    let peak = peak_for_ref(&out.peak_temp_c, &reference);

    // Explicit ceiling: compare the peak junction temperature against it.
    if let Some(limit) = a.celsius {
        return if let Some((key, tj)) = peak {
            let ok = tj <= limit + 1e-6;
            let unit = unit_note("hottest unit", &key, &reference);
            let (tj_s, limit_s) = (format_celsius(tj), format_celsius_to(limit, 3));
            if ok {
                (
                    true,
                    format!("Tj({reference}) peak {tj_s}{unit} (<= {limit_s})"),
                    None,
                )
            } else {
                (
                    false,
                    format!("Tj({reference}) peak {tj_s} > ceiling {limit_s} <- FAILED HERE{unit}"),
                    Some(format!(
                        "{reference} ran {} hotter than your {limit_s} ceiling \
                             (peak {tj_s}{unit})",
                        format_celsius(tj - limit)
                    )),
                )
            }
        } else {
            // No thermal data. The runner rejects a max_temp on a component
            // with no thermal model at bind time (`check_trackable_assert_refs`),
            // so here the part IS stress-monitored and simply never dissipated
            // measurably: its junction sat at AMBIENT. That is only a pass if
            // ambient itself is within the ceiling, in a hot-ambient spec a
            // ceiling below ambient is violated by the idle part sitting at
            // ambient, not silently skipped.
            let (ambient_s, limit_s) = (format_celsius(out.ambient_c), format_celsius_to(limit, 3));
            if out.ambient_c <= limit + 1e-6 {
                (
                    true,
                    format!(
                        "Tj({reference}): no dissipation measured (idle at ambient \
                             {ambient_s} <= {limit_s}); skipped"
                    ),
                    None,
                )
            } else {
                (
                    false,
                    format!(
                        "Tj({reference}): idle at ambient {ambient_s} exceeds ceiling \
                             {limit_s} (no dissipation, but ambient alone is over-limit)"
                    ),
                    Some(format!(
                        "{reference} sat idle at the {ambient_s} ambient, {} over your \
                             {limit_s} ceiling; the ceiling is below the spec's ambient_c",
                        format_celsius(out.ambient_c - limit)
                    )),
                )
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
    if let Some(f) = over {
        return over_device_max(f);
    }
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
    let detail = format!(
        "Tj({reference}) peak {}, within device max",
        format_celsius(tj)
    );
    (true, detail, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{NetWindow, PeripheralSnapshot, RunOutcome};
    use crate::scenarios::RailWindow;
    use crate::spec::Assertion;
    use crate::tolerance::Mode;
    use std::collections::HashMap;

    /// Assert one row is GREEN as a user sees it: it passed AND is not INVALID
    /// (`passed` alone is the numeric judgement; a row can be both `passed`
    /// and `invalid`). Prints the row's detail, which carries the INVALID reason.
    #[track_caller]
    fn assert_row_green(r: &AssertResult) {
        assert!(
            r.passed && !r.invalid,
            "row '{}' not green (passed={}, invalid={}): {}",
            r.label,
            r.passed,
            r.invalid,
            r.detail
        );
    }

    fn assertion(src: &str) -> Assertion {
        toml::from_str(src).expect("assertion shape")
    }

    /// `n` samples at `dt` of a 0-5 V `P`-long wave: `duty` high, `rise` edges.
    fn wave(duty: f64, phase: f64, rise: f64, dt: f64, n: u32) -> Vec<(f64, f64)> {
        const P: f64 = 1e-3;
        let ramp = |x: f64| 5.0 * (x / rise.max(1e-12)).min(1.0);
        (0..n)
            .map(|i| {
                let (t, hi) = (f64::from(i) * dt, duty * P);
                let u = (t / P + phase).fract() * P;
                let v = if u < hi { ramp(u) } else { 5.0 - ramp(u - hi) };
                (t, v)
            })
            .collect()
    }

    /// Partial periods at either end, a mid-period start, a lopsided duty and
    /// an edge between samples all leave an EDGE-TIMED rate alone; one edge is
    /// refused, not divided.
    #[test]
    fn edge_rate_times_edges_instead_of_counting_them() {
        let dt = 1e-5; // case, duty, phase, rise_s, samples, Hz (None = refused)
        for (case, duty, phase, rise, n, want) in [
            ("partial periods both ends", 0.5, 0.0, 0.0, 1070, Some(1e3)),
            ("edge between samples", 0.5, 0.0, 7e-5, 1070, Some(1e3)),
            ("a single edge is no rate", 0.5, 0.0, 0.0, 60, None),
            ("15% duty must not bias", 0.15, 0.0, 0.0, 1070, Some(1e3)),
            ("window opens mid-period", 0.5, 0.63, 0.0, 1070, Some(1e3)),
        ] {
            let got = edge_rate(&wave(duty, phase, rise, dt, n), dt);
            match (want, &got) {
                (Some(hz), Ok((measured, text))) => {
                    assert!((measured - hz).abs() / hz < 1e-3, "{case}: {text}");
                    let seen = "±0.2% (edge timing, 10 periods)";
                    assert!(text.contains(seen), "{case}: {text}");
                }
                (None, Err(e)) => assert!(e.contains("rising edge"), "{case}: {e}"),
                _ => panic!("{case}: wanted {want:?}, got {got:?}"),
            }
        }
    }

    /// One ensemble member holding `VOUT` flat at `v` over the whole run.
    fn vout_member(seed: u32, v: f64, interior: bool) -> RunOutcome {
        let mut windows = HashMap::new();
        windows.insert(
            ("VOUT".to_string(), 0.0f64.to_bits()),
            NetWindow {
                min_v: v,
                max_v: v,
                last_v: v,
                samples: 10,
            },
        );
        RunOutcome {
            seed,
            windows,
            interior,
            ambient_c: 25.0,
            sim_ms: 100.0,
            analog_valid: true,
            ..Default::default()
        }
    }

    mod model_coverage {
        use super::*;
        use hauksbee_engine::result::{BindSummary, UnresolvedActive};

        /// 3 of 4 active ICs bound and one unresolved part on a connected net.
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
                    unlocked_by: None,
                    reference: "U7".to_string(),
                    value: "LTC4020".to_string(),
                    reason: "no model".to_string(),
                    consequence: "open circuit".to_string(),
                    active_ic: true,
                }],
                resolved_but_open_active: vec![],
            }
        }

        fn coverage(f: impl FnOnce(&mut Assertion), bind: Option<BindSummary>) -> (bool, String) {
            let mut a = Assertion {
                kind: "model_coverage".to_string(),
                ..Default::default()
            };
            f(&mut a);
            check_model_coverage(
                &a,
                &RunOutcome {
                    bind,
                    ..Default::default()
                },
            )
        }

        #[test]
        fn thresholds_are_inclusive_and_every_one_must_hold() {
            let (passed, detail) = coverage(|a| a.min_critical = Some(0.9), Some(partly_bound()));
            assert!(
                !passed && detail.contains("3/4"),
                "3/4 is below a 90% floor: {detail}"
            );
            assert!(coverage(|a| a.min_critical = Some(0.75), Some(partly_bound())).0);
            // min_resolved passes (40/50) while min_critical fails.
            let both = |a: &mut Assertion| {
                a.min_resolved = Some(0.75);
                a.min_critical = Some(0.99);
            };
            assert!(!coverage(both, Some(partly_bound())).0);
            // The unresolved part is named: it is the user's next action.
            let (passed, detail) =
                coverage(|a| a.max_active_unresolved = Some(0), Some(partly_bound()));
            assert!(!passed && detail.contains("U7"), "{detail}");
        }

        #[test]
        fn a_board_with_nothing_to_measure_cannot_pass() {
            // 0 of 0 is not 100%, and no bind data is no answer.
            let mut bind = partly_bound();
            bind.critical_parts_total = 0;
            bind.critical_parts_bound_n = 0;
            assert!(!coverage(|a| a.min_critical = Some(0.9), Some(bind)).0);
            assert!(!coverage(|a| a.min_critical = Some(0.5), None).0);
        }
    }

    #[test]
    fn protection_trip_judges_only_its_scenario_window() {
        // BATT latched inside "inrush" and not inside "steady".
        let mut out = RunOutcome::default();
        out.protection_tripped.insert("BATT".into(), true);
        out.protection_tripped_scoped
            .insert(("inrush".into(), "BATT".into()), true);
        out.protection_tripped_scoped
            .insert(("steady".into(), "BATT".into()), false);
        let expect_trip = |scope: &str| {
            let a = assertion(&format!(
                "kind = \"protection_trip\"\nsupply_net = \"BATT\"\nexpect_trip = true\n{scope}"
            ));
            check_protection_trip(&a, &out).0
        };
        assert!(expect_trip(""), "unscoped reads the run-wide flag");
        assert!(expect_trip("scenario = \"inrush\"\n"));
        assert!(
            !expect_trip("scenario = \"steady\"\n"),
            "a trip in an earlier window does not satisfy a later one"
        );

        // An explicit `scenario = ""` is the run-wide window, not a scoped miss.
        let mut never = RunOutcome::default();
        never.protection_tripped.insert("BATT".into(), false);
        let no_trip = |scope: &str| {
            let a = assertion(&format!(
                "kind = \"protection_trip\"\nsupply_net = \"BATT\"\nexpect_trip = false\n{scope}"
            ));
            check_protection_trip(&a, &never).0
        };
        assert!(no_trip(""));
        assert!(no_trip("scenario = \"\"\n"));
    }

    /// Two corners plus one interior probe holding `VOUT` at the given volts.
    fn corners_and_probe(c0: f64, c1: f64, probe: f64) -> [RunOutcome; 3] {
        [
            vout_member(0, c0, false),
            vout_member(1, c1, false),
            vout_member(2, probe, true),
        ]
    }

    #[test]
    fn an_interior_probe_failure_escalates_but_a_corner_failure_does_not() {
        let a = assertion("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.0\n");
        let eval = |members: &[RunOutcome]| evaluate_one(&a, members, Some(Mode::Corners), 1.0);
        // Corners in band, the interior probe sags out: non-monotonic.
        let r = eval(&corners_and_probe(3.30, 3.28, 2.40));
        assert!(!r.passed && r.failing_seed == Some(2), "{}", r.detail);
        assert!(
            r.detail.contains("NON-MONOTONIC") && r.detail.contains("interior probe 2"),
            "{}",
            r.detail
        );
        // The corners found it themselves: an ordinary corner red.
        let r = eval(&corners_and_probe(2.50, 3.30, 3.20));
        assert!(
            !r.passed && !r.detail.contains("NON-MONOTONIC") && r.detail.contains("corner 0"),
            "{}",
            r.detail
        );
        // A held-stale corner never produced a trustworthy pass, so the
        // "a corner-only run would have been green" counterfactual is unknowable.
        let mut members = corners_and_probe(3.30, 3.28, 2.40);
        members[0].failed_windows = vec![(0.0, 0.100)];
        let r = eval(&members);
        assert!(
            !r.passed && !r.detail.contains("NON-MONOTONIC"),
            "{}",
            r.detail
        );
    }

    #[test]
    fn a_monotonic_response_passes_and_counts_corners_and_probes_apart() {
        let a = assertion("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.0\n");
        let outcomes = [
            vout_member(0, 3.10, false),
            vout_member(1, 3.50, false),
            vout_member(2, 3.20, true),
            vout_member(3, 3.30, true),
            vout_member(4, 3.40, true),
        ];
        let r = evaluate_one(&a, &outcomes, Some(Mode::Corners), 1.0);
        assert_row_green(&r);
        assert!(!r.detail.contains("NON-MONOTONIC"));
        assert!(
            r.detail.contains("2 min/max tolerance corners")
                && r.detail.contains("3 interior Latin-hypercube probe(s)"),
            "{}",
            r.detail
        );
        // Sampling is evidence, never a claim that the response IS monotonic.
        assert!(!r.detail.contains("is monotonic"), "{}", r.detail);
    }

    #[test]
    fn a_converged_failure_beats_a_diverged_siblings_invalid() {
        let a = assertion("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.2\n");
        let diverged = |seed: u32| {
            let mut m = vout_member(seed, 3.3, false);
            m.failed_windows = vec![(0.010, 0.020)];
            m
        };
        let res = evaluate_one(
            &a,
            &[vout_member(1, 2.0, false), diverged(2)],
            Some(Mode::Corners),
            1.0,
        );
        assert!(
            !res.invalid && !res.passed && res.failing_seed == Some(1),
            "{}",
            res.detail
        );
        // The held-stale member does not inflate the passed count.
        assert!(
            res.detail.contains("passed 0/2") && res.detail.contains("1 invalid"),
            "{}",
            res.detail
        );
        // With no converged failure the held-stale member makes the result
        // INVALID, and in corners mode the member is a corner, not a seed.
        let res = evaluate_one(
            &a,
            &[vout_member(0, 3.3, false), diverged(3)],
            Some(Mode::Corners),
            1.0,
        );
        assert!(res.invalid, "{}", res.detail);
        assert!(
            res.detail.contains("corner 3") && !res.detail.contains("seed 3"),
            "{}",
            res.detail
        );
    }

    // The stress monitor only sees converged chunks, so a run whose solve
    // failed over a window has not shown that nothing was over-stressed: the
    // result is INVALID, never a pass. The same run with every chunk
    // converged passes.
    #[test]
    fn no_faults_over_a_failed_solve_window_is_invalid_not_a_pass() {
        let a = assertion("kind = \"no_faults\"\n");
        let mut diverged = vout_member(0, 3.3, false);
        diverged.failed_windows = vec![(0.0, 0.010)];
        let res = evaluate_one(&a, &[diverged], Some(Mode::Corners), 1.0);
        assert!(res.invalid && !res.passed, "{}", res.detail);
        let res = evaluate_one(&a, &[vout_member(0, 3.3, false)], Some(Mode::Corners), 1.0);
        assert!(res.passed && !res.invalid, "{}", res.detail);
    }

    // A package ref owns itself and its `_q<N>` / `_s<N>` / `_e<N>` unit keys,
    // and nothing else: no SW1/SW10 prefix bleed, no arbitrary suffixes.
    #[test]
    fn unit_key_matching_is_exact_or_unit_suffixed() {
        assert!(key_belongs_to_ref("SW1", "SW1"));
        assert!(key_belongs_to_ref("SW1", "SW1_q12"));
        assert!(key_belongs_to_ref("SW1", "SW1_s0"));
        assert!(key_belongs_to_ref("RN1", "RN1_e1"));
        assert!(!key_belongs_to_ref("RN1", "RN1_e1a"));
        assert!(!key_belongs_to_ref("SW1", "SW10"));
        assert!(!key_belongs_to_ref("SW1", "SW1_heater"));
    }

    #[test]
    fn peripheral_verdicts_refuse_unexercised_buses_and_flag_heuristic_framing() {
        let a = assertion(
            "kind = \"peripheral\"\nid = \"TEMP1\"\nfield = \"temp_c\"\nmin = 20.0\nmax = 30.0\n",
        );
        let mut snap = PeripheralSnapshot::default();
        snap.fields.insert("temp_c".to_string(), 25.0);
        let mut out = RunOutcome::default();
        out.peripherals.insert("TEMP1".to_string(), snap);
        assert!(check_peripheral(&a, &out).0);

        // Heuristic SPI framing qualifies the verdict without failing it.
        out.spi_framing
            .insert("TEMP1".to_string(), "heuristic".to_string());
        let (ok, msg) = check_peripheral(&a, &out);
        assert!(ok && msg.contains("HEURISTIC"), "{msg}");

        // The slave's power-on default would pass the window, which is
        // exactly why an unexercised bus must fail instead.
        out.unexercised_bus_ids.insert("TEMP1".to_string());
        let (ok, msg) = check_peripheral(&a, &out);
        assert!(!ok && msg.contains("NEVER exercised"), "{msg}");
    }

    /// A run of `sim_ms` in which `EN` first crossed 3 V at `cross` ms and
    /// first fell back at `drop` ms.
    fn boot_run(sim_ms: f64, cross: Option<f64>, drop: Option<f64>) -> RunOutcome {
        let key = ("EN".to_string(), 3.0_f64.to_bits());
        let mut out = RunOutcome {
            sim_ms,
            ..Default::default()
        };
        if let Some(t) = cross {
            out.boot_first_cross_ms.insert(key.clone(), t);
        }
        if let Some(t) = drop {
            out.boot_drop_after_cross_ms.insert(key, t);
        }
        out
    }

    fn boot_assert(deadline_ms: f64, extra: &str) -> Assertion {
        assertion(&format!(
            "kind = \"boot_coverage\"\nnet = \"EN\"\nmin = 3.0\ndeadline_ms = {deadline_ms}\n{extra}"
        ))
    }

    #[test]
    fn boot_coverage_holds_through_the_deadline_and_distinguishes_drop_from_never() {
        let a = boot_assert(10.0, "");
        // Reached at 5 ms, held through 10 ms, released at 50 ms.
        assert!(check_boot_coverage(&a, &boot_run(100.0, Some(5.0), Some(50.0))).0);
        // Fell back at 8 ms, before the deadline.
        let (ok, msg) = check_boot_coverage(&a, &boot_run(100.0, Some(5.0), Some(8.0)));
        assert!(!ok && msg.contains("fell back below"), "{msg}");
        // Never reached at all: a different diagnosis.
        let (ok, msg) = check_boot_coverage(&a, &boot_run(100.0, None, None));
        assert!(!ok && !msg.contains("fell back below"), "{msg}");
        // A deadline past the end of the run cannot be confirmed.
        let (ok, msg) =
            check_boot_coverage(&boot_assert(50.0, ""), &boot_run(20.0, Some(5.0), None));
        assert!(!ok && msg.contains("cannot be confirmed"), "{msg}");
        assert!(check_boot_coverage(&boot_assert(10.0, ""), &boot_run(20.0, Some(5.0), None)).0);
    }

    #[test]
    fn boot_coverage_hold_ms_is_two_sided() {
        // A toggling net: reached at 2 ms, dropped at 7 ms (a 5 ms high phase).
        let toggling = boot_run(100.0, Some(2.0), Some(7.0));
        assert!(check_boot_coverage(&boot_assert(10.0, "hold_ms = 0.0"), &toggling).0);
        let (ok, msg) = check_boot_coverage(&boot_assert(10.0, "hold_ms = 8.0"), &toggling);
        assert!(!ok && msg.contains("held it only 5.00 ms"), "{msg}");
        assert!(
            !check_boot_coverage(&boot_assert(10.0, ""), &toggling).0,
            "the hold-to-deadline default fails a mid-window drop"
        );
        // A solid net passes both forms.
        let solid = boot_run(100.0, Some(2.0), None);
        assert!(check_boot_coverage(&boot_assert(10.0, "hold_ms = 0.0"), &solid).0);
        assert!(check_boot_coverage(&boot_assert(10.0, "hold_ms = 8.0"), &solid).0);
        // An unobserved hold tail (the sim ended at 6 ms) is not a pass.
        let short = boot_run(6.0, Some(2.0), None);
        let (ok, msg) = check_boot_coverage(&boot_assert(10.0, "hold_ms = 8.0"), &short);
        assert!(!ok && msg.contains("cannot be confirmed"), "{msg}");
    }

    /// `boot_run` on a board whose firmware runs on a host-paced clock (the
    /// ESP32 QEMU backend), with the firmware driving `EN`.
    fn host_paced(mut out: RunOutcome) -> RunOutcome {
        out.host_paced_mcus = vec!["U1".to_string()];
        out.driven_nets.insert("EN".to_string());
        out
    }

    #[test]
    fn host_paced_boot_coverage_never_judges_the_deadline_but_still_fails_undriven() {
        // The same drive, early or late against the deadline: a host-paced time
        // cannot decide either way, so both are the same conditional pass.
        for cross in [5.0, 15.0] {
            let out = host_paced(boot_run(100.0, Some(cross), None));
            let (ok, msg) = check_boot_coverage(&boot_assert(10.0, ""), &out);
            assert!(ok, "{msg}");
            assert!(
                msg.contains(&format!("at ~{cross:.0} ms (approximate)")),
                "{msg}"
            );
            assert!(
                msg.contains("the 10 ms boot deadline and the hold through it not judged"),
                "{msg}"
            );
            assert!(msg.contains("MCU U1 runs on a host-paced clock"), "{msg}");
            assert!(msg.contains("about 2x apart"), "{msg}");
        }
        // A drop inside the window is reported, not judged.
        let out = host_paced(boot_run(100.0, Some(2.0), Some(7.0)));
        let (ok, msg) = check_boot_coverage(&boot_assert(10.0, "hold_ms = 8.0"), &out);
        assert!(
            ok && msg.contains("the 10 ms boot deadline and the 8 ms hold"),
            "{msg}"
        );
        assert!(msg.contains("fell back below 3 V at ~7 ms"), "{msg}");
        // Never driven is host-independent: still a hard FAIL, with the hint
        // that a slower host covers less firmware in the same run.
        let out = host_paced(boot_run(100.0, None, None));
        let (ok, msg) = check_boot_coverage(&boot_assert(10.0, ""), &out);
        assert!(!ok, "{msg}");
        assert!(msg.contains("host-paced time on MCU U1"), "{msg}");
        assert!(!msg.contains("not judged"), "{msg}");
        // The bound is named for the evidence condition.
        let a = boot_assert(10.0, "hold_ms = 0.0");
        assert_eq!(
            host_paced_time_bound(&a, &out).as_deref(),
            Some("the 10 ms boot deadline")
        );
    }

    #[test]
    fn a_clock_exact_backend_keeps_hard_boot_deadlines() {
        // No host-paced MCU (simavr counts cycles, Renode advances virtual time
        // per instruction): a late drive is a hard FAIL, as before.
        let mut out = boot_run(100.0, Some(15.0), None);
        out.driven_nets.insert("EN".to_string());
        assert_eq!(host_paced_time_bound(&boot_assert(10.0, ""), &out), None);
        let (ok, msg) = check_boot_coverage(&boot_assert(10.0, ""), &out);
        assert!(!ok && !msg.contains("not judged"), "{msg}");
        let (ok, msg, _) = check_seed(&boot_assert(20.0, ""), &out, 1.0);
        assert!(ok && msg.contains("(<= 20 ms)"), "{msg}");
    }

    #[test]
    fn host_paced_toggle_bounds_judge_only_that_the_net_switched() {
        let toggle = |extra: &str| assertion(&format!("kind = \"toggle\"\nnet = \"EN\"\n{extra}"));
        let mut out = host_paced(boot_run(100.0, None, None));
        out.toggles.insert("EN".to_string(), 3);
        // Three toggles against a minimum of ten: a rate over a host-paced
        // window, so only "it toggled" is judged.
        let (ok, msg, _) = check_seed(&toggle("min_toggles = 10"), &out, 1.0);
        assert!(ok, "{msg}");
        assert!(
            msg.contains("the 10-toggle minimum over the run not judged"),
            "{msg}"
        );
        // A net that never toggled still fails hard.
        out.toggles.insert("EN".to_string(), 0);
        let (ok, msg, _) = check_seed(&toggle("min_toggles = 10"), &out, 1.0);
        assert!(!ok, "{msg}");
        // A single-toggle claim has no time in it and stays exactly as before.
        assert_eq!(
            host_paced_time_bound(&toggle("min_toggles = 1"), &out),
            None
        );
        // An analog-only net keeps its hard bound: the solver's time is exact.
        out.driven_nets.clear();
        assert_eq!(
            host_paced_time_bound(&toggle("min_toggles = 10"), &out),
            None
        );
        let (ok, _, _) = check_seed(&toggle("min_toggles = 10"), &out, 1.0);
        assert!(!ok);
    }

    /// A 200 ms run whose MCU `U1` printed `text`, on a host-paced clock or not.
    fn uart_run(text: &str, host_paced: bool) -> RunOutcome {
        let mut out = RunOutcome {
            sim_ms: 200.0,
            ..Default::default()
        };
        out.uart.insert("U1".to_string(), text.to_string());
        if host_paced {
            out.host_paced_mcus = vec!["U1".to_string()];
        }
        out
    }

    fn uart_assertions() -> [Assertion; 2] {
        [
            assertion("kind = \"uart\"\ncontains = \"boot ok\""),
            assertion("kind = \"uart\"\nmatches = \"^boot (ok|fine)\""),
        ]
    }

    #[test]
    fn host_paced_uart_text_that_appeared_passes_and_names_the_window_unjudged() {
        let out = uart_run("boot ok\r\n", true);
        for a in uart_assertions() {
            assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
            let (ok, msg, _) = check_seed(&a, &out, 1.0);
            assert!(ok, "{msg}");
            assert!(msg.contains("UART=\"boot ok"), "{msg}");
            assert!(
                msg.contains("the 200 ms window the text had to appear in not judged"),
                "{msg}"
            );
            assert!(msg.contains("MCU U1 runs on a host-paced clock"), "{msg}");
            assert!(msg.contains("about 2x apart"), "{msg}");
            let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
            assert!(r.passed && !r.invalid, "{}", r.detail);
        }
    }

    #[test]
    fn host_paced_uart_text_that_never_appeared_is_unjudged_not_failed() {
        // The firmware may only be slow on this host: neither FAIL nor PASS.
        let out = uart_run("ROM banner only\r\n", true);
        for a in uart_assertions() {
            let reason = member_invalid_reason(&a, &out, 1.0).expect("a host-paced miss");
            assert!(
                reason.starts_with("host-paced clock: the UART text"),
                "{reason}"
            );
            assert!(
                reason.contains("MCU U1 runs on a host-paced clock"),
                "{reason}"
            );
            assert!(
                reason.contains("not evidence the firmware is wrong"),
                "{reason}"
            );
            let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
            assert!(r.invalid && !r.passed, "{}", r.detail);
            assert!(
                r.detail.contains("INVALID: host-paced clock:"),
                "{}",
                r.detail
            );
            // What was printed stays in the detail, so the reader sees the output.
            assert!(r.detail.contains("ROM banner only"), "{}", r.detail);
        }
        // A named MCU that is not host-paced keeps its hard verdict, whatever
        // else on the board is.
        let mut out = uart_run("", false);
        out.uart.insert("U2".to_string(), "idle".to_string());
        out.host_paced_mcus = vec!["U1".to_string()];
        let a = assertion("kind = \"uart\"\nmcu = \"U2\"\ncontains = \"boot ok\"");
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        assert_eq!(host_paced_time_bound(&a, &out), None);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.invalid && !r.passed, "{}", r.detail);
    }

    #[test]
    fn a_clock_exact_backend_keeps_hard_uart_verdicts() {
        for a in uart_assertions() {
            let hit = uart_run("boot ok", false);
            assert_eq!(host_paced_time_bound(&a, &hit), None);
            let r = evaluate_one(&a, std::slice::from_ref(&hit), None, 1.0);
            assert_row_green(&r);
            assert!(!r.detail.contains("not judged"), "{}", r.detail);
            let miss = uart_run("nothing yet", false);
            assert_eq!(member_invalid_reason(&a, &miss, 1.0), None);
            let r = evaluate_one(&a, std::slice::from_ref(&miss), None, 1.0);
            assert!(!r.passed && !r.invalid, "{}", r.detail);
            assert!(r.detail.contains("does NOT"), "{}", r.detail);
        }
    }

    /// `evaluate`'s record of the bound a host-paced clock left unjudged: it
    /// qualifies a UART pass, and it is absent from an unjudged miss.
    #[test]
    fn host_paced_uart_pass_records_the_bound_and_a_miss_does_not() {
        let spec: Spec = toml::from_str(
            "board = \"b.kicad_pcb\"\nduration_ms = 200\n[[assert]]\nkind = \"uart\"\ncontains = \"boot ok\"\n",
        )
        .expect("spec shape");
        let hit = evaluate(&spec, &[uart_run("boot ok", true)]);
        assert_row_green(&hit[0]);
        let paced = hit[0].host_paced.as_ref().expect("the pass is qualified");
        assert_eq!(paced.mcus, ["U1"]);
        assert!(paced.bound.contains("200 ms window"), "{}", paced.bound);
        let miss = evaluate(&spec, &[uart_run("nope", true)]);
        assert!(miss[0].invalid && miss[0].host_paced.is_none());
        let control = evaluate(&spec, &[uart_run("boot ok", false)]);
        assert_row_green(&control[0]);
        assert!(control[0].host_paced.is_none());
    }

    /// A 200 ms run with an EEPROM `ee` holding "Hi" and a sensor `s` reading
    /// `temp_c` 31, on a host-paced clock or not.
    fn peripheral_run(host_paced: bool) -> RunOutcome {
        let mut out = RunOutcome {
            sim_ms: 200.0,
            ..Default::default()
        };
        out.peripherals.insert(
            "ee".to_string(),
            PeripheralSnapshot {
                bytes: b"Hi".to_vec(),
                ..Default::default()
            },
        );
        out.peripherals.insert(
            "s".to_string(),
            PeripheralSnapshot {
                fields: HashMap::from([("temp_c".to_string(), 31.0)]),
                ..Default::default()
            },
        );
        if host_paced {
            out.host_paced_mcus = vec!["U1".to_string()];
        }
        out
    }

    #[test]
    fn host_paced_peripheral_content_passes_and_names_the_window_unjudged() {
        let out = peripheral_run(true);
        for src in [
            "kind = \"peripheral\"\nid = \"ee\"\nbytes = \"48 69\"",
            "kind = \"peripheral\"\nid = \"s\"\nfield = \"temp_c\"\nmin = 30.0\nmax = 32.0",
        ] {
            let a = assertion(src);
            assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
            let (ok, msg, _) = check_seed(&a, &out, 1.0);
            assert!(ok, "{msg}");
            assert!(
                msg.contains(
                    "the 200 ms window the peripheral state had to be reached in not judged"
                ),
                "{msg}"
            );
            assert!(msg.contains("MCU U1 runs on a host-paced clock"), "{msg}");
        }
    }

    #[test]
    fn host_paced_peripheral_content_that_is_missing_is_unjudged_not_failed() {
        let out = peripheral_run(true);
        for src in [
            "kind = \"peripheral\"\nid = \"ee\"\nbytes = \"de ad\"",
            "kind = \"peripheral\"\nid = \"s\"\nfield = \"temp_c\"\nmin = 40.0",
        ] {
            let a = assertion(src);
            let reason = member_invalid_reason(&a, &out, 1.0).expect("a host-paced miss");
            assert!(
                reason.starts_with("host-paced clock: the peripheral state"),
                "{reason}"
            );
            let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
            assert!(r.invalid && !r.passed, "{}", r.detail);
        }
        // Failures no host speed changes stay hard: a bus never exercised, no
        // such peripheral, no such field.
        let mut out = peripheral_run(true);
        out.unexercised_bus_ids.insert("ee".to_string());
        for src in [
            "kind = \"peripheral\"\nid = \"ee\"\nbytes = \"48 69\"",
            "kind = \"peripheral\"\nid = \"gone\"\nbytes = \"48 69\"",
            "kind = \"peripheral\"\nid = \"s\"\nfield = \"nope\"\nmin = 1.0",
        ] {
            let a = assertion(src);
            assert_eq!(member_invalid_reason(&a, &out, 1.0), None, "{src}");
            assert_eq!(host_paced_time_bound(&a, &out), None, "{src}");
            let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
            assert!(!r.invalid && !r.passed, "{src}: {}", r.detail);
        }
    }

    #[test]
    fn a_clock_exact_backend_keeps_hard_peripheral_verdicts() {
        let out = peripheral_run(false);
        let hit = assertion("kind = \"peripheral\"\nid = \"ee\"\nbytes = \"48 69\"");
        assert_eq!(host_paced_time_bound(&hit, &out), None);
        let r = evaluate_one(&hit, std::slice::from_ref(&out), None, 1.0);
        assert_row_green(&r);
        assert!(!r.detail.contains("not judged"), "{}", r.detail);
        let miss = assertion("kind = \"peripheral\"\nid = \"ee\"\nbytes = \"de ad\"");
        let r = evaluate_one(&miss, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.passed && !r.invalid, "{}", r.detail);
    }

    /// A 1 kHz, 50% duty square wave on `EN` sampled every 10 us for 10.7 ms,
    /// driven by firmware on a host-paced clock or on an exact one.
    fn square_run(host_paced_clock: bool) -> RunOutcome {
        let mut out = RunOutcome {
            sim_ms: 10.7,
            ..Default::default()
        };
        out.net_series
            .insert("EN".to_string(), wave(0.5, 0.0, 0.0, 1e-5, 1070));
        out.toggles.insert("EN".to_string(), 21);
        if host_paced_clock {
            host_paced(out)
        } else {
            out.driven_nets.insert("EN".to_string());
            out
        }
    }

    #[test]
    fn host_paced_frequency_and_pulse_width_bounds_are_measured_not_judged() {
        let out = square_run(true);
        // 2 kHz wanted, 1 kHz measured: a hard FAIL on an exact clock, but the
        // rate comes off a host-paced clock, so it is reported and not judged.
        let freq = assertion("kind = \"toggle\"\nnet = \"EN\"\nfreq_hz = 2000.0");
        let (ok, msg, _) = check_seed(&freq, &out, 0.01);
        assert!(ok, "{msg}");
        assert!(msg.contains("the 2000 Hz rate band not judged"), "{msg}");
        // A 0.5 ms pulse against a 0.3 ms ceiling: the width is still measured.
        let width = assertion("kind = \"pulse_width\"\nnet = \"EN\"\nmax = 0.3");
        let (ok, msg, _) = check_seed(&width, &out, 0.01);
        assert!(ok, "{msg}");
        assert!(msg.contains("0.5000 ms"), "{msg}");
        assert!(msg.contains("the pulse-width bound not judged"), "{msg}");
    }

    #[test]
    fn a_clock_exact_backend_keeps_hard_frequency_and_pulse_width_verdicts() {
        let out = square_run(false);
        let freq = assertion("kind = \"toggle\"\nnet = \"EN\"\nfreq_hz = 2000.0");
        assert_eq!(host_paced_time_bound(&freq, &out), None);
        let (ok, msg, _) = check_seed(&freq, &out, 0.01);
        assert!(!ok && !msg.contains("not judged"), "{msg}");
        let width = assertion("kind = \"pulse_width\"\nnet = \"EN\"\nmax = 0.3");
        let (ok, msg, _) = check_seed(&width, &out, 0.01);
        assert!(!ok && !msg.contains("not judged"), "{msg}");
        // Inside the band it passes, as before.
        let in_band = assertion("kind = \"toggle\"\nnet = \"EN\"\nfreq_hz = 1000.0");
        assert!(check_seed(&in_band, &out, 0.01).0);
    }

    /// A 200 ms run whose net `net` spanned `whole` = (min, max, last) volts
    /// over the run and `after` from 50 ms on, on a host-paced clock or a
    /// clock-exact one, with the firmware driving `net` or not.
    fn level_run(
        net: &str,
        whole: (f64, f64, f64),
        after: (f64, f64, f64),
        host_paced: bool,
        driven: bool,
    ) -> RunOutcome {
        let mut out = RunOutcome {
            sim_ms: 200.0,
            ..Default::default()
        };
        for (thr, (min_v, max_v, last_v)) in [(0.0_f64, whole), (50.0, after)] {
            out.windows.insert(
                (net.to_string(), thr.to_bits()),
                NetWindow {
                    min_v,
                    max_v,
                    last_v,
                    samples: 10,
                },
            );
        }
        if host_paced {
            out.host_paced_mcus = vec!["U1".to_string()];
        }
        if driven {
            out.driven_nets.insert(net.to_string());
        }
        out
    }

    fn en_after_50ms() -> Assertion {
        assertion("kind = \"voltage\"\nnet = \"EN\"\nmin = 3.0\nafter_ms = 50.0")
    }

    const HIGH: (f64, f64, f64) = (3.3, 3.3, 3.3);
    /// Low until the firmware drove it, then high to the end of the run.
    const RISES: (f64, f64, f64) = (0.0, 3.3, 3.3);
    /// High for a while, then low at the end of the run.
    const FALLS: (f64, f64, f64) = (0.0, 3.3, 0.0);

    #[test]
    fn host_paced_firmware_level_that_holds_all_run_passes_with_the_window_unjudged() {
        let out = level_run("EN", HIGH, HIGH, true, true);
        let a = en_after_50ms();
        assert_eq!(
            host_paced_time_bound(&a, &out).as_deref(),
            Some("the 50 ms after_ms window")
        );
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        let (ok, msg, _) = check_seed(&a, &out, 1.0);
        assert!(ok, "{msg}");
        assert!(
            msg.starts_with("EN over the whole 200 ms run: min=3.3 V (>= 3 V)"),
            "{msg}"
        );
        assert!(
            msg.contains("the 50 ms after_ms window not judged: MCU U1 runs on a host-paced clock"),
            "{msg}"
        );
        // The pass is conditional, and the condition reaches the run verdict.
        let spec: Spec = toml::from_str(
            "board = \"b.kicad_pcb\"\nduration_ms = 200\n[[assert]]\nkind = \"voltage\"\n\
             net = \"EN\"\nmin = 3.0\nafter_ms = 50.0\n",
        )
        .expect("spec shape");
        let results = evaluate(&spec, std::slice::from_ref(&out));
        assert!(
            results[0].passed && !results[0].invalid,
            "{}",
            results[0].detail
        );
        let paced = results[0].host_paced.as_ref().expect("a conditional pass");
        assert_eq!(paced.mcus, ["U1"]);
        let conditions = host_paced_conditions(&results);
        assert_eq!(conditions.len(), 1, "{conditions:?}");
        assert!(
            conditions[0].contains("the 50 ms after_ms window was not judged")
                && conditions[0].contains("MCU U1 runs on a host-paced clock"),
            "{conditions:?}"
        );
    }

    #[test]
    fn host_paced_firmware_level_that_holds_only_inside_the_window_is_invalid() {
        // High from 50 ms on, low before: the pass would rest on where 50 ms
        // falls in the firmware's progress, which the host decides.
        let out = level_run("EN", RISES, HIGH, true, true);
        let a = en_after_50ms();
        let reason = member_invalid_reason(&a, &out, 1.0).expect("unjudged");
        assert!(
            reason.starts_with("host-paced clock: 'EN' is driven by firmware on MCU U1"),
            "{reason}"
        );
        assert!(
            reason.contains("Judged after 50 ms, the level holds")
                && reason.contains("leaves out an excursion earlier in the run")
                && reason.contains("min=0 V < required 3 V"),
            "{reason}"
        );
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.invalid && !r.passed, "{}", r.detail);
        assert!(
            r.detail.contains("INVALID: host-paced clock:"),
            "{}",
            r.detail
        );
        // An INVALID result names no condition: it qualifies no pass.
        assert!(host_paced_conditions(std::slice::from_ref(&r)).is_empty());

        // Out of range inside the window too: still not a FAIL, the window may
        // start before the firmware gets there on a slower host.
        let out = level_run("EN", FALLS, FALLS, true, true);
        let reason = member_invalid_reason(&a, &out, 1.0).expect("unjudged");
        assert!(
            reason.contains("Judged after 50 ms, the level does not hold"),
            "{reason}"
        );
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.invalid && !r.passed, "{}", r.detail);
    }

    #[test]
    fn host_paced_net_the_firmware_never_drove_keeps_its_hard_fail() {
        // Never driven is host-independent, as for boot_coverage.
        let low = (0.0, 0.0, 0.0);
        let out = level_run("EN", low, low, true, false);
        let a = en_after_50ms();
        assert_eq!(host_paced_time_bound(&a, &out), None);
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.passed && !r.invalid, "{}", r.detail);
        assert!(!r.detail.contains("not judged"), "{}", r.detail);
        assert!(r.detail.contains("EN after 50ms"), "{}", r.detail);
    }

    #[test]
    fn host_paced_pure_analog_rail_keeps_its_windowed_verdict() {
        // A regulator output no MCU pin drives: the solver's time base is exact,
        // so `after_ms` skips its ramp exactly as on any backend.
        let a = assertion("kind = \"voltage\"\nnet = \"VREG\"\nmin = 3.2\nafter_ms = 50.0");
        let out = level_run("VREG", RISES, HIGH, true, false);
        assert_eq!(host_paced_time_bound(&a, &out), None);
        assert_eq!(analog_eval_window(&a, &out), Some((0.05, 0.2)));
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.passed && !r.invalid, "{}", r.detail);
        assert!(
            r.detail.starts_with("VREG after 50ms: min=3.3 V"),
            "{}",
            r.detail
        );
        assert!(!r.detail.contains("not judged"), "{}", r.detail);
        let out = level_run("VREG", FALLS, FALLS, true, false);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.passed && !r.invalid, "{}", r.detail);
    }

    #[test]
    fn a_clock_exact_backend_keeps_hard_windowed_level_verdicts() {
        // simavr and Renode count their own time: the same firmware-driven net
        // passes on its window and fails out of it, with nothing unjudged.
        let a = en_after_50ms();
        let out = level_run("EN", RISES, HIGH, false, true);
        assert_eq!(host_paced_time_bound(&a, &out), None);
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert_row_green(&r);
        assert!(!r.detail.contains("not judged"), "{}", r.detail);
        assert!(r.detail.starts_with("EN after 50ms:"), "{}", r.detail);
        let out = level_run("EN", FALLS, FALLS, false, true);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.passed && !r.invalid, "{}", r.detail);
    }

    /// A host-paced run whose firmware drives `EN`, with a `load` scenario
    /// window on it that saw `scoped` volts and a whole run spanning `whole`.
    fn en_rail_run(scoped: &[(f64, f64)], whole: (f64, f64, f64)) -> RunOutcome {
        let mut out = level_run("EN", whole, whole, true, true);
        let mut win = RailWindow::new();
        for &(t, v) in scoped {
            win.observe(t, v);
        }
        out.rail_windows
            .insert(("load".to_string(), "EN".to_string()), win);
        out
    }

    #[test]
    fn host_paced_rail_window_on_a_firmware_net_follows_the_level_rule() {
        let a = assertion(
            "kind = \"rail_window\"\nnet = \"EN\"\nscenario = \"load\"\nmin = 3.0\n\
             dip_below = 3.1\nfor_max_ms = 1.0",
        );
        let in_band = [(0.10, 3.3), (0.11, 3.3)];
        // In range all run: PASS, the scenario window and the dip budget unjudged.
        let out = en_rail_run(&in_band, HIGH);
        assert_eq!(
            host_paced_time_bound(&a, &out).as_deref(),
            Some("the scenario 'load' window and the dip duration bound")
        );
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        let (ok, msg, _) = check_seed(&a, &out, 1.0);
        assert!(ok, "{msg}");
        assert!(msg.starts_with("EN over the whole 200 ms run:"), "{msg}");
        assert!(
            msg.contains("the scenario 'load' window and the dip duration bound not judged"),
            "{msg}"
        );
        // In range only inside the scenario window: INVALID.
        let out = en_rail_run(&in_band, RISES);
        let reason = member_invalid_reason(&a, &out, 1.0).expect("unjudged");
        assert!(
            reason.contains("Judged in the scenario 'load' window, the level holds"),
            "{reason}"
        );
        // Nothing but a time budget on a firmware net: nothing is left to judge.
        let budget_only =
            assertion("kind = \"rail_window\"\nnet = \"EN\"\ndip_below = 3.1\nfor_max_ms = 1.0");
        let reason = member_invalid_reason(&budget_only, &en_rail_run(&in_band, HIGH), 1.0)
            .expect("nothing left to judge");
        assert!(reason.contains("nothing is left to judge"), "{reason}");
        // With no window, the level is read over the whole run anyway: a hard
        // FAIL that names the budget it left unjudged.
        let unscoped = assertion(
            "kind = \"rail_window\"\nnet = \"EN\"\nmin = 3.0\ndip_below = 3.1\nfor_max_ms = 1.0",
        );
        let mut out = en_rail_run(&in_band, FALLS);
        out.rail_windows.insert(
            (String::new(), "EN".to_string()),
            out.rail_windows[&("load".to_string(), "EN".to_string())].clone(),
        );
        out.rail_windows
            .get_mut(&(String::new(), "EN".to_string()))
            .expect("inserted")
            .observe(0.19, 0.0);
        assert_eq!(member_invalid_reason(&unscoped, &out, 1.0), None);
        let r = evaluate_one(&unscoped, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.passed && !r.invalid, "{}", r.detail);
        assert!(
            r.detail.contains("the dip duration bound not judged"),
            "{}",
            r.detail
        );
    }

    /// [`level_run`] on `LOAD`, a load-switch output no MCU pin drives but
    /// that follows firmware-driven `GATE`.
    fn load_switch_run(
        whole: (f64, f64, f64),
        after: (f64, f64, f64),
        host_paced: bool,
    ) -> RunOutcome {
        let mut out = level_run("LOAD", whole, after, host_paced, false);
        out.driven_nets.insert("GATE".to_string());
        out.influenced_nets
            .insert("LOAD".to_string(), "GATE".to_string());
        out
    }

    fn load_after_50ms() -> Assertion {
        assertion("kind = \"voltage\"\nnet = \"LOAD\"\nmin = 3.0\nafter_ms = 50.0")
    }

    #[test]
    fn host_paced_level_downstream_of_a_firmware_net_passes_only_conditionally() {
        let out = load_switch_run(HIGH, HIGH, true);
        let a = load_after_50ms();
        assert_eq!(
            host_paced_time_bound(&a, &out).as_deref(),
            Some("the 50 ms after_ms window on 'LOAD', downstream of firmware-driven 'GATE'")
        );
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        let (ok, msg, _) = check_seed(&a, &out, 1.0);
        assert!(ok, "{msg}");
        assert!(
            msg.starts_with("LOAD over the whole 200 ms run: min=3.3 V (>= 3 V)"),
            "{msg}"
        );
        let spec: Spec = toml::from_str(
            "board = \"b.kicad_pcb\"\nduration_ms = 200\n[[assert]]\nkind = \"voltage\"\n\
             net = \"LOAD\"\nmin = 3.0\nafter_ms = 50.0\n",
        )
        .expect("spec shape");
        let results = evaluate(&spec, std::slice::from_ref(&out));
        assert!(
            results[0].passed && !results[0].invalid,
            "{}",
            results[0].detail
        );
        let conditions = host_paced_conditions(&results);
        assert_eq!(conditions.len(), 1, "{conditions:?}");
        assert!(
            conditions[0].contains(
                "the 50 ms after_ms window on 'LOAD', downstream of firmware-driven 'GATE' \
                 was not judged"
            ) && conditions[0].contains("MCU U1 runs on a host-paced clock"),
            "{conditions:?}"
        );
    }

    #[test]
    fn host_paced_level_downstream_of_a_firmware_net_is_invalid_when_only_the_window_passes() {
        let a = load_after_50ms();
        // High from 50 ms on, low before: the window decides the pass.
        let out = load_switch_run(RISES, HIGH, true);
        let reason = member_invalid_reason(&a, &out, 1.0).expect("unjudged");
        assert!(
            reason.starts_with(
                "host-paced clock: 'LOAD' is downstream of firmware-driven 'GATE' (driven by \
                 firmware on MCU U1, which runs on a host-paced clock"
            ),
            "{reason}"
        );
        assert!(
            reason.contains("so where the 50 ms after_ms window falls in the firmware's progress")
                && reason.contains("Judged after 50 ms, the level holds"),
            "{reason}"
        );
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.invalid && !r.passed, "{}", r.detail);
        // Out of range in the window too: not a FAIL either.
        let out = load_switch_run(FALLS, FALLS, true);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.invalid && !r.passed, "{}", r.detail);
    }

    #[test]
    fn a_clock_exact_backend_keeps_hard_verdicts_downstream_of_firmware_too() {
        let a = load_after_50ms();
        let out = load_switch_run(RISES, HIGH, false);
        assert_eq!(host_paced_time_bound(&a, &out), None);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.passed && !r.invalid, "{}", r.detail);
        assert!(r.detail.starts_with("LOAD after 50ms:"), "{}", r.detail);
        assert!(!r.detail.contains("not judged"), "{}", r.detail);
        let out = load_switch_run(FALLS, FALLS, false);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(!r.passed && !r.invalid, "{}", r.detail);
    }

    #[test]
    fn host_paced_rail_window_on_a_regulator_rail_the_firmware_enables_follows_the_level_rule() {
        // VSW is a regulator output whose enable EN the firmware drives: its
        // scenario window is placed in host-paced firmware progress.
        let a = assertion(
            "kind = \"rail_window\"\nnet = \"VSW\"\nscenario = \"load\"\nmin = 3.0\n\
             dip_below = 3.1\nfor_max_ms = 1.0",
        );
        let in_band = [(0.10, 3.3), (0.11, 3.3)];
        let mut out = level_run("VSW", RISES, RISES, true, false);
        out.driven_nets.insert("EN".to_string());
        out.influenced_nets
            .insert("VSW".to_string(), "EN".to_string());
        let mut win = RailWindow::new();
        for &(t, v) in &in_band {
            win.observe(t, v);
        }
        out.rail_windows
            .insert(("load".to_string(), "VSW".to_string()), win);
        assert_eq!(
            host_paced_time_bound(&a, &out).as_deref(),
            Some(
                "the scenario 'load' window and the dip duration bound on 'VSW', downstream of \
                 firmware-driven 'EN'"
            )
        );
        let reason = member_invalid_reason(&a, &out, 1.0).expect("unjudged");
        assert!(
            reason.contains("'VSW' is downstream of firmware-driven 'EN'")
                && reason.contains("Judged in the scenario 'load' window, the level holds"),
            "{reason}"
        );
        // The same rail with no firmware upstream is a held rail: hard verdict.
        out.influenced_nets.clear();
        assert_eq!(host_paced_time_bound(&a, &out), None);
        assert_eq!(member_invalid_reason(&a, &out, 1.0), None);
        let r = evaluate_one(&a, std::slice::from_ref(&out), None, 1.0);
        assert!(r.passed && !r.invalid, "{}", r.detail);
        assert!(!r.detail.contains("not judged"), "{}", r.detail);
    }

    #[test]
    fn host_paced_rate_band_downstream_of_a_firmware_net_is_unjudged() {
        // An LED cathode switched by a transistor the firmware toggles blinks
        // at the firmware's host-paced rate, like the GPIO itself.
        let a = assertion("kind = \"toggle\"\nnet = \"LED_K\"\nfreq_hz = 2.0");
        let mut out = level_run("LED_K", FALLS, FALLS, true, false);
        out.influenced_nets
            .insert("LED_K".to_string(), "LED_CTL".to_string());
        assert_eq!(
            host_paced_time_bound(&a, &out).as_deref(),
            Some("the 2 Hz rate band on 'LED_K', downstream of firmware-driven 'LED_CTL'")
        );
        out.host_paced_mcus.clear();
        assert_eq!(host_paced_time_bound(&a, &out), None);
    }

    /// A run whose `load` scenario window on `VBUS` saw `samples`, with the
    /// scheduler's intra-frame minimum `fold`ed into the envelope.
    fn rail_run(samples: &[(f64, f64)], fold: Option<f64>) -> RunOutcome {
        let mut win = RailWindow::new();
        for &(t, v) in samples {
            win.observe(t, v);
        }
        if let Some(v) = fold {
            win.fold(v);
        }
        let mut out = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        out.rail_windows
            .insert(("load".to_string(), "VBUS".to_string()), win);
        out
    }

    #[test]
    fn rail_window_sees_folded_intraframe_sags_and_refuses_one_sample_dips() {
        // Settled 3.3 V samples with a 2.9 V intra-frame sag folded in.
        let floor =
            assertion("kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\nmin = 3.0\n");
        assert!(!check_rail_window(&floor, &rail_run(&[(0.000, 3.3), (0.001, 3.3)], Some(2.9))).0);
        assert!(check_rail_window(&floor, &rail_run(&[(0.000, 3.3), (0.001, 3.25)], Some(3.1))).0);

        // A dip duration cannot be measured from one sample: fail, not 0 ms.
        let dip = assertion(
            "kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\n\
             dip_below = 3.0\nfor_max_ms = 1.0\n",
        );
        let (ok, msg, _) = check_rail_window(&dip, &rail_run(&[(0.099, 2.5)], None));
        assert!(!ok && msg.contains("sample"), "{msg}");
        assert!(check_rail_window(&dip, &rail_run(&[(0.000, 5.0), (0.010, 5.0)], None)).0);
    }

    #[test]
    fn rail_window_spike_with_one_sample_does_not_auto_pass() {
        let a = assertion(
            "kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\n\
             spike_above = 3.5\nspike_for_max_ms = 1.0\n",
        );
        let (ok, msg, _why) = check_rail_window(&a, &rail_run(&[(0.099, 4.2)], None));
        assert!(!ok, "a 1-sample spike window must not auto-pass: {msg}");
        assert!(
            msg.contains("too few"),
            "message should explain the degenerate window: {msg}"
        );
    }

    // R24: on a multi-unit package with TIED max temperatures, the reported
    // hottest unit must be deterministic (lowest key), not whatever HashMap
    // iteration order happened to surface, or two identical runs emit different
    // report bytes (reproducibility doctrine). Verdict is unaffected.
    #[test]
    fn max_temp_covers_idle_parts_tied_units_and_missing_samples() {
        let ceiling = |c: f64| {
            assertion(&format!(
                "kind = \"max_temp\"\nref = \"U3\"\ncelsius = {c}\n"
            ))
        };
        // An idle part sits at ambient; ambient above the ceiling is a failure.
        let hot = RunOutcome {
            ambient_c: 85.0,
            ..Default::default()
        };
        assert!(!check_max_temp(&ceiling(70.0), &hot).0);
        assert!(check_max_temp(&ceiling(105.0), &hot).0);

        // Tied unit temperatures resolve to the lowest key, deterministically.
        let mut tied = RunOutcome {
            ambient_c: 25.0,
            ..Default::default()
        };
        for unit in ["SW1_q2", "SW1_q1", "SW1_q3"] {
            tied.peak_temp_c.insert(unit.to_string(), 100.0);
        }
        let a = assertion("kind = \"max_temp\"\nref = \"SW1\"\ncelsius = 150.0\n");
        let (ok, msg, _) = check_max_temp(&a, &tied);
        assert!(
            ok && msg.contains("SW1_q1") && !msg.contains("SW1_q2"),
            "{msg}"
        );

        // The celsius-less form with no sample at all was never evaluated.
        let bare = assertion("kind = \"max_temp\"\nref = \"U2\"\n");
        let (ok, msg, _) = check_max_temp(&bare, &RunOutcome::default());
        assert!(!ok && msg.contains("never evaluated"), "{msg}");
        let mut warm = RunOutcome::default();
        warm.peak_temp_c.insert("U2".to_string(), 61.0);
        assert!(check_max_temp(&bare, &warm).0);
    }

    /// A junction past silicon's melting point is no measurement: the member
    /// is INVALID, not a thermal FAIL, while a hot but possible part is judged.
    #[test]
    fn an_impossible_junction_temperature_is_refused_not_judged() {
        let a = assertion("kind = \"max_temp\"\nref = \"Q1\"\ncelsius = 150.0\n");
        let mut out = RunOutcome::default();
        out.peak_temp_c.insert("Q1".to_string(), 5.2e6);
        let reason = member_invalid_reason(&a, &out, 1.0).expect("refused");
        assert!(
            reason.contains("Q1") && reason.contains("melting"),
            "{reason}"
        );
        out.peak_temp_c.insert("Q1".to_string(), 240.0);
        assert!(member_invalid_reason(&a, &out, 1.0).is_none());
    }

    /// One member, judged the way `evaluate` judges it.
    fn judge(a: &Assertion, out: RunOutcome) -> AssertResult {
        let out = RunOutcome {
            analog_valid: true,
            ambient_c: 25.0,
            sim_ms: 100.0,
            ..out
        };
        evaluate_one(a, &[out], None, 1.0)
    }

    /// A NaN measurement is no number: it must refuse (INVALID, with the cause
    /// named), never fold into a running extreme and pass. The finite controls
    /// are judged normally, with `91.7 C` spacing.
    #[test]
    fn a_non_finite_measurement_is_invalid_with_its_cause_and_a_finite_one_is_judged() {
        let temp = assertion("kind = \"max_temp\"\nref = \"R1\"\ncelsius = 125.0\n");
        let amps = assertion("kind = \"max_current\"\nref = \"R1\"\namps = 1.0\n");
        for (a, is_temp) in [(&temp, true), (&amps, false)] {
            let mut out = RunOutcome::default();
            // The runner records any non-finite sample as NaN (`fold_peak`).
            let map = if is_temp {
                &mut out.peak_temp_c
            } else {
                &mut out.peak_current
            };
            map.insert("R1".to_string(), f64::NAN);
            let r = judge(a, out);
            assert!(r.invalid && !r.passed, "{}", r.detail);
            assert!(
                r.detail.contains(UNPHYSICAL_FIGURE)
                    && r.detail.contains("R1")
                    && r.detail.contains("not a finite number"),
                "{}",
                r.detail
            );
            assert!(!r.detail.contains("converge"), "{}", r.detail);
        }
        let mut ok = RunOutcome::default();
        ok.peak_temp_c.insert("R1".to_string(), 91.7);
        ok.peak_current.insert("R1".to_string(), 0.2);
        let (t, i) = (judge(&temp, ok.clone()), judge(&amps, ok));
        assert!(t.passed && !t.invalid, "{}", t.detail);
        assert_eq!(t.detail, "Tj(R1) peak 91.7 C (<= 125 C)");
        assert!(i.passed && !i.invalid, "{}", i.detail);
    }

    /// A window that saw a non-finite voltage holds no measurement: the bound
    /// check refuses instead of reading an untouched window as a pass.
    #[test]
    fn a_poisoned_voltage_window_is_invalid_and_a_clean_one_is_judged() {
        let a = assertion("kind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.0\nmax = 3.6\n");
        let with = |min_v: f64, max_v: f64| {
            let mut out = RunOutcome::default();
            out.windows.insert(
                ("VOUT".to_string(), 0.0f64.to_bits()),
                NetWindow {
                    min_v,
                    max_v,
                    last_v: min_v,
                    samples: 10,
                },
            );
            judge(&a, out)
        };
        let bad = with(f64::NAN, f64::NAN);
        assert!(bad.invalid && !bad.passed, "{}", bad.detail);
        assert!(bad.detail.contains(UNPHYSICAL_FIGURE) && bad.detail.contains("VOUT"));
        let good = with(3.3, 3.3);
        assert!(good.passed && !good.invalid, "{}", good.detail);
    }

    /// The fault line carries the unit of each figure (W, C, A or V).
    #[test]
    fn a_stress_fault_detail_states_units() {
        let fault = |kind: &str, value: f64, limit: f64| RunOutcome {
            faults: vec![crate::runner::RunFault {
                component: "R1".into(),
                kind: kind.into(),
                value,
                limit,
                t_ms: 0.4,
            }],
            ..Default::default()
        };
        for (kind, unit) in [
            ("overpower", "W"),
            ("overtemperature", "C"),
            ("overcurrent", "A"),
            ("surge_current", "A"),
            ("pin_overcurrent", "A"),
            ("overvoltage", "V"),
            ("reverse_bias", "V"),
            ("gate_overvoltage", "V"),
        ] {
            let (ok, detail) = check_no_faults(&fault(kind, 1.48, 1.0));
            assert!(!ok);
            assert!(
                detail.contains(&format!("{kind} 1.480 {unit} > 1.000 {unit} at 0.4ms")),
                "{detail}"
            );
        }
        // Over 100x its rating the figure is no result: the fault is stated
        // far past it, in the engine's one wording, with no number.
        let (ok, detail) = check_no_faults(&fault("overpower", 120.48, 1.0));
        assert!(!ok);
        assert!(
            detail.contains("first: R1 dissipates far more than its 1 W power rating")
                && !detail.contains("120.48")
                && detail.ends_with("at 0.4ms"),
            "{detail}"
        );
        assert!(check_no_faults(&RunOutcome::default()).0);
    }

    /// Temperatures read `91.7 C` everywhere, never `91.7C`, and never `-0`.
    #[test]
    fn temperatures_use_one_spelling() {
        let a = assertion("kind = \"max_temp\"\nref = \"R1\"\ncelsius = -0.0\n");
        assert_eq!(a.label(), "Tj(R1) <= 0 C");
        let mut hot = RunOutcome::default();
        hot.peak_temp_c.insert("R1".to_string(), 156.7);
        let ceiling = assertion("kind = \"max_temp\"\nref = \"R1\"\ncelsius = 125.0\n");
        let r = judge(&ceiling, hot);
        assert!(!r.passed && !r.invalid);
        assert_eq!(
            r.detail,
            "Tj(R1) peak 156.7 C > ceiling 125 C <- FAILED HERE"
        );
        assert_eq!(
            r.why.as_deref(),
            Some("R1 ran 31.7 C hotter than your 125 C ceiling (peak 156.7 C)")
        );
    }

    #[test]
    fn max_current_aggregates_over_multiunit_package_keys() {
        let limit = |amps: f64| {
            assertion(&format!(
                "kind = \"max_current\"\nref = \"RN1\"\namps = {amps}\n"
            ))
        };
        let mut out = RunOutcome::default();
        for (unit, amps) in [
            ("RN1_e1", 0.10),
            ("RN1_e2", 0.12),
            ("RN1_e3", 0.08),
            ("RN1_e4", 0.11),
        ] {
            out.peak_current.insert(unit.to_string(), amps);
        }
        let (ok, msg, _) = check_max_current(&limit(0.5), &out);
        assert!(
            ok && msg.contains("RN1_e2"),
            "the peak unit is named: {msg}"
        );
        assert!(!check_max_current(&limit(0.10), &out).0);
    }

    // A rail_window brownout floor must see an intra-frame sag that recovers by
    // the frame's last chunk; the runner folds the scheduler's per-frame extremes
    // into RailWindow.min_v/max_v, exactly like the plain voltage path. Without the
    // fold (base bug), min_v is the settled 3.3 V and a min=3.0 floor false-passes
    // the very sag it exists to catch (R49). The ceiling is the mirror case, and
    // the SAMPLED spike-duration path must stay settled-frame based: a folded
    // extreme is an envelope, not a sampled interval, and must not invent one.
    #[test]
    fn rail_window_bounds_see_folded_intraframe_extremes_without_inventing_durations() {
        let settled = [(0.000, 3.3), (0.001, 3.3)];
        let floor =
            assertion("kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\nmin = 3.0\n");
        let (ok, msg, _why) = check_rail_window(&floor, &rail_run(&settled, Some(2.9)));
        assert!(
            !ok,
            "a rail that sagged to 2.9V mid-frame must FAIL a 3.0V floor, not pass on the settled 3.3V: {msg}"
        );
        assert!(
            check_rail_window(&floor, &rail_run(&[(0.000, 3.3), (0.001, 3.25)], Some(3.1))).0,
            "a rail that stayed above 3.0V must pass"
        );

        let ceiling =
            assertion("kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\nmax = 4.0\n");
        let (ok, msg, _why) = check_rail_window(&ceiling, &rail_run(&settled, Some(4.2)));
        assert!(!ok, "a folded 4.2V spike must fail a 4.0V ceiling: {msg}");

        let sampled = assertion(
            "kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\n\
             spike_above = 3.5\nspike_for_max_ms = 1.0\n",
        );
        let (ok, msg, _why) = check_rail_window(&sampled, &rail_run(&settled, Some(4.2)));
        assert!(
            ok,
            "folded intra-frame extrema must not invent sampled spike duration: {msg}"
        );
    }

    #[test]
    fn rail_window_spike_and_settling_assertions_render_and_fail_loud() {
        let spec = |settle_within_ms: f64| {
            assertion(&format!(
                "kind = \"rail_window\"\nnet = \"VBUS\"\nscenario = \"load\"\n\
                 spike_above = 3.5\nspike_for_max_ms = 2.5\nsettle_to = 3.4\n\
                 settle_within_ms = {settle_within_ms}\n"
            ))
        };
        let points = [(0.000, 3.3), (0.001, 3.8), (0.002, 3.8), (0.003, 3.3)];
        let (ok, msg, why) = check_rail_window(&spec(2.5), &rail_run(&points, None));
        assert!(ok, "a finite spike and settling window should pass: {msg}");
        assert!(
            msg.contains("spike>") && msg.contains("settle-to-"),
            "rendered detail: {msg}"
        );
        assert!(
            why.is_none(),
            "passing overvoltage checks should have no why: {why:?}"
        );

        // The same waveform against a 1 ms settling bound: the 2 ms it really
        // took must fail loud, and a spike that never settles must read +inf
        // rather than quietly truncating to the end of the window.
        let tight = spec(1.0);
        let (ok_tight, msg_tight, why_tight) = check_rail_window(&tight, &rail_run(&points, None));
        assert!(
            !ok_tight,
            "a 2 ms settling time must fail a 1 ms bound: {msg_tight}"
        );
        assert!(msg_tight.contains("FAILED HERE"));
        assert!(why_tight.unwrap_or_default().contains("settle"));

        let never_settles = [(0.000, 3.3), (0.001, 3.8), (0.002, 3.6)];
        let (ok_never, msg_never, why_never) =
            check_rail_window(&tight, &rail_run(&never_settles, None));
        assert!(
            !ok_never,
            "a spike that never settles must fail: {msg_never}"
        );
        assert!(
            msg_never.contains("inf"),
            "never-settles detail should expose +inf: {msg_never}"
        );
        assert!(why_never.unwrap_or_default().contains("settle"));
    }

    // hwtrace ensemble: a converged member's real feature mismatch must beat a
    // diverged sibling's INVALID (the R50 FAIL>INVALID doctrine, applied to the
    // hwtrace path). Base bug: any diverged member forced every feature to INVALID.
    #[test]
    fn hwtrace_converged_mismatch_beats_a_diverged_sibling_invalid() {
        // A square wave 0<->5V of the given period over 1 s at 1 kSa/s.
        fn square(period_s: f64) -> Vec<(f64, f64)> {
            (0..=1000)
                .map(|i| {
                    let t = f64::from(i) * 0.001;
                    let v = if (t / period_s).fract() >= 0.5 {
                        4.9
                    } else {
                        0.05
                    };
                    (t, v)
                })
                .collect()
        }
        let dir = std::env::temp_dir().join(format!("hb_hwtrace_prec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let csv = square(0.2)
            .iter()
            .map(|(t, v)| format!("{t:.4},{v:.3}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("d.csv"), format!("time_s,volts\n{csv}")).unwrap();
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

        // Corner 0 converged with a 500 ms period against the 200 ms capture;
        // corner 1 matches but diverged.
        let member = |seed: u32, period_s: f64, failed: Vec<(f64, f64)>| RunOutcome {
            seed,
            net_series: HashMap::from([("D".to_string(), square(period_s))]),
            sim_ms: 1000.0,
            failed_windows: failed,
            ..Default::default()
        };
        let results = evaluate_hwtrace(
            &spec,
            &a,
            &[member(0, 0.5, Vec::new()), member(1, 0.2, vec![(0.4, 0.5)])],
            Some(Mode::Corners),
        );
        let _ = std::fs::remove_dir_all(&dir);
        let r = results
            .iter()
            .find(|r| r.kind == "hwtrace")
            .expect("a hwtrace result");
        assert!(!r.invalid && !r.passed, "{}", r.detail);
        assert_eq!(r.failing_seed, Some(0), "{}", r.detail);
    }

    #[test]
    fn peripheral_transitions_field_is_analog_gated_but_digital_state_is_not() {
        let out = RunOutcome {
            sim_ms: 100.0,
            ..Default::default()
        };
        let transitions =
            assertion("kind = \"peripheral\"\nid = \"SINK\"\nfield = \"transitions\"\nmin = 100\n");
        assert_eq!(analog_eval_window(&transitions, &out), Some((0.0, 0.1)));
        let digital =
            assertion("kind = \"peripheral\"\nid = \"EE\"\nfield = \"last_addr\"\nmin = 0\n");
        assert_eq!(analog_eval_window(&digital, &out), None);
    }

    #[test]
    fn uart_all_mcu_concatenation_is_sorted_by_mcu() {
        let a = assertion("kind = \"uart\"\nmatches = \"^BOOT\"\n");
        let matches = |a_out: &str, b_out: &str| {
            let mut out = RunOutcome::default();
            out.uart.insert("A".to_string(), a_out.to_string());
            out.uart.insert("B".to_string(), b_out.to_string());
            check_uart(&a, &out).0
        };
        assert!(
            matches("BOOT_OK\n", "READY\n"),
            "A's output leads the haystack"
        );
        assert!(
            !matches("READY\n", "BOOT_OK\n"),
            "B's output never leads it"
        );
    }

    #[test]
    fn a_uart_claim_is_about_the_mcu_that_printed_it() {
        let mut out = RunOutcome::default();
        out.uart.insert("U1".to_string(), "hello".to_string());
        out.uart.insert("U7".to_string(), String::new());
        let outcomes = [out];
        // Unnamed: every MCU whose stream check_uart reads.
        let any = assertion("kind = \"uart\"\ncontains = \"hello\"\n");
        assert_eq!(subject_refs_for(&any, &outcomes), ["U1", "U7"]);
        // Named: just that MCU.
        let one = assertion("kind = \"uart\"\nmcu = \"U1\"\ncontains = \"hello\"\n");
        assert_eq!(subject_refs_for(&one, &outcomes), ["U1"]);
        // Other kinds keep only their own reference (an empty scope is the
        // whole board, as before).
        let faults = assertion("kind = \"no_faults\"\n");
        assert!(subject_refs_for(&faults, &outcomes).is_empty());
    }

    #[test]
    fn ac_gain_refuses_frequencies_it_did_not_sweep() {
        use crate::runner::AcOutcome;
        // Swept 10 Hz .. 100 kHz at -5 dB throughout.
        let bode = HashMap::from([(
            "OUT".to_string(),
            vec![
                (10.0, -5.0, 0.0),
                (1_000.0, -5.0, 0.0),
                (100_000.0, -5.0, 0.0),
            ],
        )]);
        let out = RunOutcome {
            ac: Some(AcOutcome {
                bode,
                ..Default::default()
            }),
            ..Default::default()
        };
        let gain = |freq: &str| {
            let a = assertion(&format!(
                "kind = \"ac_gain\"\nnet = \"OUT\"\nfreq_hz = {freq}\nmax = -20.0\n"
            ));
            check_ac_gain(&a, &out)
        };
        // Out of band: refused rather than clamped to the endpoint gain.
        let (ok, msg, _) = gain("1e6");
        assert!(!ok && msg.contains("outside the swept band"), "{msg}");
        // In band: measured (and failing the bound for a real reason).
        let (ok, msg, _) = gain("1000.0");
        assert!(!ok && !msg.contains("outside the swept band"), "{msg}");
        // NaN compares false against every bound, so it needs its own refusal.
        let (ok, msg, _) = gain("nan");
        assert!(!ok && msg.contains("non-finite"), "{msg}");
    }
}

#[cfg(test)]
mod format_amps_tests {
    use hauksbee_ir::units::format_amps;

    /// Currents state their unit by magnitude, in the one shared form.
    #[test]
    fn small_currents_get_legible_units() {
        assert_eq!(format_amps(0.0), "0 A");
        assert_eq!(format_amps(1.2345), "1.234 A");
        assert_eq!(format_amps(0.0123), "12.3 mA");
        // The old rendering of this value was "0.0000A".
        assert_eq!(format_amps(4.2e-5), "42 uA");
        assert_eq!(format_amps(-0.0), "0 A");
    }
}
