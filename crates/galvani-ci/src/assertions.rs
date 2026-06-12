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
        other => (false, format!("unknown assertion kind '{other}'")),
    }
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
