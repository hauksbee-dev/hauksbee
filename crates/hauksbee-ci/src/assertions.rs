//! Evaluate a spec's assertions against the per-seed run outcomes. An assertion
//! passes only if it holds on *every* seed (that is what makes initial-state
//! fuzzing meaningful: the rail must come up across all random power-up states).

use crate::runner::RunOutcome;
use crate::spec::{Assertion, Spec};

/// The result of one assertion across all seeds.
#[derive(Debug, Clone)]
pub struct AssertResult {
    pub label: String,
    pub kind: String,
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
    pub failing_seed: Option<u32>,
    /// Every ensemble member this assertion failed on (empty on a pass). For a
    /// tolerance ensemble this is the per-seed failure list the report and
    /// JUnit surface, together with the pass-rate.
    pub failing_seeds: Vec<u32>,
    /// How many ensemble members were evaluated.
    pub seeds_total: u32,
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
                evaluate_hwtrace(spec, a, outcomes)
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
fn evaluate_hwtrace(spec: &Spec, a: &Assertion, outcomes: &[RunOutcome]) -> Vec<AssertResult> {
    use crate::hwtrace;

    let hard_fail = |label: String, detail: String| AssertResult {
        label,
        kind: "hwtrace".to_string(),
        passed: false,
        invalid: false,
        detail,
        failing_seed: None,
        failing_seeds: Vec::new(),
        seeds_total: outcomes.len() as u32,
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
        " [SYNTHETIC trace — validates the harness, not the hardware]"
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
                results.push(hard_fail(
                    format!("hwtrace {}", ch.net),
                    e.to_string(),
                ));
                continue;
            }
        };
        for f in &ch.features {
            let label = format!("hwtrace {} {}", ch.net, f.kind);
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
                });
                continue;
            }

            // Per-seed comparison; the feature must hold on every seed.
            let mut last_detail = String::new();
            let mut failures: Vec<(u32, String)> = Vec::new();
            for out in outcomes {
                let Some(sim) = out.net_series.get(&ch.net) else {
                    failures.push((
                        out.seed,
                        format!(
                            "net '{}' was never sampled by the run — check the net name in \
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

            let (passed, mut detail, failing_seeds) = match failures.first() {
                None => (true, last_detail, Vec::new()),
                Some((seed, d)) => {
                    let d = if outcomes.len() > 1 {
                        format!("seed {seed}: {d}")
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
            });
        }
    }
    results
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

    // Analog-validity gate (05 §3b, "refuse rather than fake"): if this assertion
    // reads the analog transient operating point AND its evaluation window
    // overlaps a chunk the solver failed on, the samples there were held-stale.
    // We can honestly neither pass nor fail it, so report a distinct INVALID
    // outcome. This fires per-window (not per consecutive-abort), so an
    // intermittent divergence below the abort threshold still refuses instead of
    // reporting a fake result. Checked before pass/fail so a window with no valid
    // samples cannot masquerade as either.
    for out in outcomes {
        if let Some((ws, we)) = analog_eval_window(a, out) {
            if let Some(&(fs, fe)) = out
                .failed_windows
                .iter()
                .find(|&&(fs, fe)| ws < fe && fs < we)
            {
                let per_seed = if outcomes.len() > 1 {
                    format!("seed {}: ", out.seed)
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
                };
            }
        }
    }

    // Evaluate every ensemble member, so the result carries the pass-rate and
    // the full failing-seed list, not just the first red.
    let mut last_detail = String::new();
    let mut failures: Vec<(&RunOutcome, String)> = Vec::new();
    for out in outcomes {
        let (ok, detail) = check_seed(a, out);
        if ok {
            last_detail = detail;
        } else {
            failures.push((out, detail));
        }
    }

    if let Some((first, first_detail)) = failures.first() {
        // Lead with the first failing member's measurement, then the exact
        // sampled component values it ran with (the actionable artifact), then
        // the ensemble pass-rate.
        let mut detail = if outcomes.len() > 1 {
            format!("{member} {}: {first_detail}", first.seed)
        } else {
            first_detail.clone()
        };
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
                .map(|(o, _)| o.seed.to_string())
                .collect();
            let more = if failures.len() > 8 { ", …" } else { "" };
            detail.push_str(&format!(
                "; passed {}/{} {member}s (failing: {}{more})",
                outcomes.len() - failures.len(),
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
            failing_seeds: failures.iter().map(|(o, _)| o.seed).collect(),
            seeds_total: outcomes.len() as u32,
        };
    }

    // All members green. State exactly what that means: plain fuzz keeps its
    // wording; a tolerance ensemble claims sampled coverage (never proof), and
    // corners claim boundedness only for monotonic responses.
    let detail = match (outcomes.len(), mode) {
        (1, _) => last_detail,
        (n, None) => format!("{last_detail} (held across {n} seeds)"),
        (n, Some(crate::tolerance::Mode::MonteCarlo)) => format!(
            "{last_detail} (passed {n}/{n} sampled tolerance seeds — statistical \
             coverage, not worst-case proof)"
        ),
        (n, Some(crate::tolerance::Mode::Corners)) => format!(
            "{last_detail} (held on all {n} min/max tolerance corners — bounds the \
             worst case only where the response is monotonic in each value)"
        ),
    };
    AssertResult {
        label,
        kind,
        passed: true,
        invalid: false,
        detail,
        failing_seed: None,
        failing_seeds: Vec::new(),
        seeds_total: outcomes.len() as u32,
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
        "boot-coverage" => Some((0.0, a.deadline_ms.map(|d| d / 1000.0).unwrap_or(end))),
        "rail_window" => Some((0.0, end)),
        _ => None,
    }
}

/// Check one assertion against one seed's outcome. Returns (passed, detail).
fn check_seed(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    match a.kind.as_str() {
        "voltage" => check_voltage(a, out),
        "uart" => check_uart(a, out),
        "toggle" => check_toggle(a, out),
        "no_faults" => check_no_faults(out),
        "max_current" => check_max_current(a, out),
        "max_temp" => check_max_temp(a, out),
        "peripheral" => check_peripheral(a, out),
        "rail_window" => check_rail_window(a, out),
        "protection_trip" => check_protection_trip(a, out),
        "boot-coverage" => check_boot_coverage(a, out),
        "phase_margin" => check_phase_margin(a, out),
        "ac_gain" => check_ac_gain(a, out),
        other => (false, format!("unknown assertion kind '{other}'")),
    }
}

/// rail_window: judge a rail's behaviour over a scenario window: min/max bounds,
/// dip duration below a threshold, and recovery time.
fn check_rail_window(a: &crate::spec::Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let scope = a.scenario.clone().unwrap_or_default();
    let Some(win) = out.rail_windows.get(&(scope.clone(), net.clone())) else {
        return (
            false,
            format!("net '{net}' was never sampled in scenario window '{scope}'"),
        );
    };
    if win.samples.is_empty() {
        return (false, format!("net '{net}' had no samples in the window"));
    }

    let mut ok = true;
    let mut parts = Vec::new();
    if let Some(lo) = a.min {
        ok &= win.min_v >= lo - 1e-6;
        parts.push(format!("min={:.3}V (>= {lo}V)", win.min_v));
    }
    if let Some(hi) = a.max {
        ok &= win.max_v <= hi + 1e-6;
        parts.push(format!("max={:.3}V (<= {hi}V)", win.max_v));
    }
    if let (Some(d), Some(for_ms)) = (a.dip_below, a.for_max_ms) {
        let dip_ms = win.dip_duration_s(d) * 1000.0;
        ok &= dip_ms <= for_ms + 1e-6;
        parts.push(format!("dip<{d}V for {dip_ms:.2}ms (<= {for_ms}ms)"));
    }
    if let (Some(d), Some(r), Some(within_ms)) = (a.dip_below, a.recover_to, a.recover_within_ms) {
        let rec_ms = win.recovery_s(d, r) * 1000.0;
        ok &= rec_ms <= within_ms + 1e-6;
        parts.push(format!(
            "recover-to-{r}V in {rec_ms:.2}ms (<= {within_ms}ms)"
        ));
    }

    (
        ok,
        format!(
            "{net} window: {} [min={:.3}V max={:.3}V]",
            parts.join(", "),
            win.min_v,
            win.max_v
        ),
    )
}

/// protection_trip: whether a supply net's battery protection latched.
fn check_protection_trip(a: &crate::spec::Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.supply_net.clone().unwrap_or_default();
    let want = a.expect_trip.unwrap_or(false);
    let tripped = out.protection_tripped.get(&net).copied();
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

/// Boot-coverage: the control net named by `net` must reach and hold its defined
/// level (`min`, volts) by `deadline_ms` after reset, and no stress fault may
/// fire during the boot window before it does. This is the formerly-rejected
/// "Hi-Z control input" class made decidable by running the firmware: on a net
/// with no static board bias (the genuinely-undefined case this targets) only
/// the firmware can bring it to level, so this measures whether the firmware
/// drives it in time. A statically-biased net reads at level from t=0 and is out
/// of scope (it was never undefined).
fn check_boot_coverage(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let level = a.min.unwrap_or(0.0);
    let deadline = a.deadline_ms.unwrap_or(0.0);
    let key = (net.clone(), level.to_bits());

    let reached = out.first_reach_ms.get(&key).copied();
    match reached {
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
        Some(t_ms) if t_ms > deadline + 1e-9 => (
            false,
            format!(
                "control net '{net}' first reached {level} V at {t_ms:.2} ms, past the {deadline} ms boot deadline"
            ),
        ),
        Some(t_ms) => {
            // Driven in time. Now require no fault fired in the boot window
            // *before* the net was first driven (rails must hold and nothing
            // over-stresses while the control input is still undefined).
            if let Some(ft) = out.first_fault_ms {
                if ft < t_ms - 1e-9 {
                    return (
                        false,
                        format!(
                            "control net '{net}' was driven at {t_ms:.2} ms, but a stress fault fired earlier at {ft:.2} ms during the boot window"
                        ),
                    );
                }
            }
            (
                true,
                format!("control net '{net}' driven to >= {level} V at {t_ms:.2} ms (<= {deadline} ms), boot window clean"),
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
///     backend reads DDR, so a held-LOW pin is known driven): a net absent here
///     is genuinely undriven / Hi-Z;
///   - it is not driven and the backend cannot report drive direction (the
///     external Renode/QEMU backends see drive state only through observed
///     edges): absence is ambiguous, so we say only what is known rather than
///     asserting Hi-Z on what might be a held-LOW pin.
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
    let Some(snap) = out.peripherals.get(&id) else {
        return (false, format!("peripheral '{id}' not found in run"));
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
                "{id} memory {} bytes {spec_bytes} ({ascii:?})",
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
            let known: Vec<&String> = snap.fields.keys().collect();
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
        return (ok, format!("{id}.{field} = {v} ({})", parts.join(", ")));
    }

    (false, format!("peripheral '{id}' assertion incomplete"))
}

fn check_voltage(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let thr = a.after_ms.unwrap_or(0.0);
    let Some(win) = out.windows.get(&(net.clone(), thr.to_bits())) else {
        return (
            false,
            format!("net '{net}' was never sampled (no window at {thr}ms)"),
        );
    };
    if win.samples == 0 {
        return (false, format!("net '{net}' had no samples after {thr}ms"));
    }
    // For a >= bound we care about the worst (minimum) the rail dipped to in
    // the window; for a <= bound, the worst (maximum) it rose to.
    let mut ok = true;
    let mut parts = Vec::new();
    if let Some(lo) = a.min {
        let worst = win.min_v;
        ok &= worst >= lo - 1e-6;
        parts.push(format!("min={worst:.3}V (>= {lo}V)"));
    }
    if let Some(hi) = a.max {
        let worst = win.max_v;
        ok &= worst <= hi + 1e-6;
        parts.push(format!("max={worst:.3}V (<= {hi}V)"));
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
    )
}

fn check_uart(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    // Concatenate the requested MCU's UART, or all MCUs if unspecified.
    let text: String = match &a.mcu {
        Some(m) => out.uart.get(m).cloned().unwrap_or_default(),
        None => out.uart.values().cloned().collect::<Vec<_>>().join(""),
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

fn check_toggle(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let toggles = out.toggles.get(&net).copied().unwrap_or(0);

    if let Some(min) = a.min_toggles {
        let ok = toggles >= min;
        return (ok, format!("{net}: {toggles} toggles (need >= {min})"));
    }
    if let Some(freq) = a.freq_hz {
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
        return (
            ok,
            format!(
                "{net}: ~{measured:.2} Hz from {toggles} toggles (want {freq} Hz ±{:.0}%)",
                tol * 100.0
            ),
        );
    }
    (false, format!("{net}: toggle assertion incomplete"))
}

/// phase_margin: the loop's phase margin (degrees) at gain crossover must lie in
/// the requested bound. The loop gain is read at `net` from the shared AC sweep.
fn check_phase_margin(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let Some(ac) = &out.ac else {
        return (false, "no AC analysis ran (missing [ac] block)".into());
    };
    let Some(m) = ac.margins.get(&net) else {
        return (
            false,
            format!("net '{net}' produced no loop-stability margins"),
        );
    };
    let Some(pm) = m.phase_margin_deg else {
        return (
            false,
            format!(
                "loop '{net}' never crosses 0 dB in the swept band (no gain crossover; DC loop gain {:.1} dB)",
                m.dc_gain_db
            ),
        );
    };
    let fc = m.gain_crossover_hz.unwrap_or(f64::NAN);
    let mut ok = true;
    let mut parts = Vec::new();
    if let Some(lo) = a.min {
        ok &= pm >= lo - 1e-6;
        parts.push(format!(">= {lo}"));
    }
    if let Some(hi) = a.max {
        ok &= pm <= hi + 1e-6;
        parts.push(format!("<= {hi}"));
    }
    (
        ok,
        format!(
            "loop {net}: phase margin {pm:.2} deg at fc={fc:.4} Hz ({})",
            parts.join(", ")
        ),
    )
}

/// ac_gain: the magnitude (dB) at `net` must lie in the requested bound, at the
/// frequency `freq_hz` (interpolated) or, if absent, over the whole sweep.
fn check_ac_gain(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let net = a.net.clone().unwrap_or_default();
    let Some(ac) = &out.ac else {
        return (false, "no AC analysis ran (missing [ac] block)".into());
    };
    let Some(bode) = ac.bode.get(&net) else {
        return (
            false,
            format!("net '{net}' was not sampled by the AC sweep"),
        );
    };
    if bode.is_empty() {
        return (false, format!("net '{net}' has no AC data"));
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
    if let Some(lo) = a.min {
        let worst = if a.freq_hz.is_some() {
            db
        } else {
            bode.iter().map(|p| p.1).fold(f64::INFINITY, f64::min)
        };
        ok &= worst >= lo - 1e-6;
        parts.push(format!("min={worst:.3}dB (>= {lo})"));
    }
    if let Some(hi) = a.max {
        let worst = if a.freq_hz.is_some() {
            db
        } else {
            bode.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max)
        };
        ok &= worst <= hi + 1e-6;
        parts.push(format!("max={worst:.3}dB (<= {hi})"));
    }
    (ok, format!("{net} gain {where_str}: {}", parts.join(", ")))
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

fn check_max_current(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let reference = a.reference.clone().unwrap_or_default();
    let limit = a.amps.unwrap_or(0.0);
    match out.peak_current.get(&reference) {
        Some(&peak) => {
            let ok = peak <= limit + 1e-9;
            (ok, format!("I({reference}) peak {peak:.4}A (<= {limit}A)"))
        }
        None => (
            // No current data: only resistors/diodes are tracked. Treat absence
            // as a soft pass but say so, rather than failing a check we cannot
            // measure for this component kind.
            true,
            format!("I({reference}): no current data (only R/D tracked); skipped"),
        ),
    }
}

/// max_temp: the steady-state junction temperature of `ref` must stay at or
/// below `celsius` (if given) or below the device's own max junction temp (if
/// not, in which case we lean on whether an overtemperature fault fired).
fn check_max_temp(a: &Assertion, out: &RunOutcome) -> (bool, String) {
    let reference = a.reference.clone().unwrap_or_default();
    let peak = out.peak_temp_c.get(&reference).copied();

    // Explicit ceiling: compare the peak junction temperature against it.
    if let Some(limit) = a.celsius {
        return match peak {
            Some(tj) => {
                let ok = tj <= limit + 1e-6;
                (ok, format!("Tj({reference}) peak {tj:.1}C (<= {limit}C)"))
            }
            None => (
                // No thermal data: the part never dissipated measurably, so it
                // cannot have exceeded the ceiling. Pass, but say so.
                true,
                format!("Tj({reference}): no dissipation measured (idle/non-dissipating); skipped"),
            ),
        };
    }

    // No explicit ceiling: pass unless an overtemperature fault fired for this
    // component (the monitor compares Tj against the device's own max Tj).
    let over = out
        .faults
        .iter()
        .find(|f| f.component == reference && f.kind == "overtemperature");
    match over {
        Some(f) => (
            false,
            format!(
                "Tj({reference}) exceeded device max: {:.1}C > {:.1}C at {:.1}ms",
                f.value, f.limit, f.t_ms
            ),
        ),
        None => {
            let detail = match peak {
                Some(tj) => format!("Tj({reference}) peak {tj:.1}C, within device max"),
                None => format!("Tj({reference}): no dissipation measured; within device max"),
            };
            (true, detail)
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
    use super::boot_below_threshold_msg;

    // A boot-coverage net that the firmware actively drove but that never crossed
    // the threshold must NOT be reported as Hi-Z / undefined: it was driven.
    #[test]
    fn driven_but_below_threshold_says_driven_not_hi_z() {
        let m = boot_below_threshold_msg("FLAG", 2.3, true, true, Some((0.0, 0.4)));
        assert!(m.contains("was driven but never exceeded 2.3 V"), "got: {m}");
        assert!(m.contains("observed range [0.000, 0.400] V"), "got: {m}");
        assert!(!m.contains("Hi-Z"), "a driven pin is not Hi-Z, got: {m}");
    }

    // A genuinely undriven net on a backend that can report drive direction (AVR)
    // keeps the honest Hi-Z / undefined wording.
    #[test]
    fn undriven_on_observable_backend_says_hi_z() {
        let m = boot_below_threshold_msg("FLAG", 2.3, false, true, Some((0.0, 0.0)));
        assert!(m.contains("Hi-Z / undefined"), "got: {m}");
        assert!(m.contains("never driven"), "got: {m}");
    }

    // On a backend that cannot report drive direction (Renode/QEMU), absence of a
    // drive record is ambiguous, so the message must not assert Hi-Z: it names
    // both possibilities instead.
    #[test]
    fn unknown_drive_direction_does_not_assert_hi_z() {
        let m = boot_below_threshold_msg("FLAG", 2.3, false, false, Some((0.0, 0.4)));
        assert!(m.contains("cannot report pin drive direction"), "got: {m}");
        assert!(m.contains("undriven"), "got: {m}");
        assert!(m.contains("driven LOW"), "got: {m}");
        assert!(!m.contains("firmware left it Hi-Z"), "must not assert Hi-Z, got: {m}");
    }
}
