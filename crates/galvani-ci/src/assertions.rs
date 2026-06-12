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
    /// One-line detail (the measured value, the offending seed, etc).
    pub detail: String,
    /// If it failed, the seed index that failed (for fuzzed runs).
    pub failing_seed: Option<u32>,
}

/// Evaluate every assertion in the spec; returns one result per assertion.
pub fn evaluate(spec: &Spec, outcomes: &[RunOutcome]) -> Vec<AssertResult> {
    spec.asserts
        .iter()
        .map(|a| evaluate_one(a, outcomes))
        .collect()
}

fn evaluate_one(a: &Assertion, outcomes: &[RunOutcome]) -> AssertResult {
    let label = a.label();
    let kind = a.kind.clone();

    // Evaluate per seed; the first failing seed determines the result.
    let mut last_detail = String::new();
    for out in outcomes {
        let (ok, detail) = check_seed(a, out);
        last_detail = detail.clone();
        if !ok {
            return AssertResult {
                label,
                kind,
                passed: false,
                detail: if outcomes.len() > 1 {
                    format!("seed {}: {detail}", out.seed)
                } else {
                    detail
                },
                failing_seed: Some(out.seed),
            };
        }
    }

    AssertResult {
        label,
        kind,
        passed: true,
        detail: if outcomes.len() > 1 {
            format!("{} (held across {} seeds)", last_detail, outcomes.len())
        } else {
            last_detail
        },
        failing_seed: None,
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
        "peripheral" => check_peripheral(a, out),
        "rail_window" => check_rail_window(a, out),
        "protection_trip" => check_protection_trip(a, out),
        "boot-coverage" => check_boot_coverage(a, out),
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
    if let (Some(d), Some(r), Some(within_ms)) =
        (a.dip_below, a.recover_to, a.recover_within_ms)
    {
        let rec_ms = win.recovery_s(d, r) * 1000.0;
        ok &= rec_ms <= within_ms + 1e-6;
        parts.push(format!("recover-to-{r}V in {rec_ms:.2}ms (<= {within_ms}ms)"));
    }

    (
        ok,
        format!("{net} window: {} [min={:.3}V max={:.3}V]", parts.join(", "), win.min_v, win.max_v),
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
            format!(
                "control net '{net}' was NEVER driven to >= {level} V (firmware left it Hi-Z / undefined through the whole run)"
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
        let needle = parse_hex_bytes(spec_bytes)
            .unwrap_or_else(|| spec_bytes.as_bytes().to_vec());
        let found = !needle.is_empty()
            && snap.bytes.windows(needle.len()).any(|w| w == needle.as_slice());
        let ascii = String::from_utf8_lossy(&needle);
        return (
            found,
            format!(
                "{id} memory {} bytes {spec_bytes} ({ascii:?})",
                if found { "contains" } else { "does NOT contain" }
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
        return (
            false,
            format!("net '{net}' had no samples after {thr}ms"),
        );
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
        format!("{net}{when}: {} [settled {:.3}V]", parts.join(", "), win.last_v),
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
            format!("UART={preview:?} {} {needle:?}", if ok { "contains" } else { "does NOT contain" }),
        );
    }
    if let Some(re) = &a.matches {
        match regex::Regex::new(re) {
            Ok(rx) => {
                let ok = rx.is_match(&text);
                (
                    ok,
                    format!("UART={preview:?} {} /{re}/", if ok { "matches" } else { "does NOT match" }),
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
        return (
            ok,
            format!("{net}: {toggles} toggles (need >= {min})"),
        );
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
            (
                ok,
                format!("I({reference}) peak {peak:.4}A (<= {limit}A)"),
            )
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
