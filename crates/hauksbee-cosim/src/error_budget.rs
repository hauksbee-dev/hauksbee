//! The numerical error budget for a transient co-simulation.
//!
//! [`transient_error_budget`] turns the solver options actually used plus the
//! run's measured failed and fallback windows into an
//! [`ErrorBudget`](hauksbee_ir::evidence::ErrorBudget): the machine-readable
//! qualification of every transient / thermal / co-sim numeric claim. The
//! scheduler calls it directly; `hauksbee_engine::evidence::BoardEvidence`
//! forwards to it so the evidence spine carries the same budget.

use hauksbee_ir::evidence::{
    ErrorBudget, EvidenceError, IntegrationMethod, IntegrationTolerance, TimeWindow, WindowMethod,
};

fn integration_method(method: hauksbee_solve::Integration) -> IntegrationMethod {
    match method {
        hauksbee_solve::Integration::Trapezoidal => IntegrationMethod::Trapezoidal,
        hauksbee_solve::Integration::Gear2 => IntegrationMethod::Gear2,
        hauksbee_solve::Integration::BackwardEuler => IntegrationMethod::BackwardEuler,
    }
}

fn checked_subwindow(
    kind: &'static str,
    start_s: f64,
    end_s: f64,
    result: TimeWindow,
) -> Result<TimeWindow, EvidenceError> {
    let window = TimeWindow::new(start_s, end_s)?;
    if start_s < result.start_s() || end_s > result.end_s() {
        return Err(EvidenceError::WindowOutsideResult {
            kind,
            start_s,
            end_s,
            result_start_s: result.start_s(),
            result_end_s: result.end_s(),
        });
    }
    Ok(window)
}

fn windows_overlap(first: TimeWindow, second: TimeWindow) -> bool {
    first.start_s() < second.end_s() && second.start_s() < first.end_s()
}

/// Partition the solved span into primary-method windows after removing failed
/// and fallback spans. This avoids an overlapping "primary over the whole run"
/// entry that would falsely claim the primary method produced fallback data.
fn uncovered_windows(
    result: TimeWindow,
    blocked: impl IntoIterator<Item = TimeWindow>,
) -> Result<Vec<TimeWindow>, EvidenceError> {
    let mut blocked: Vec<_> = blocked.into_iter().collect();
    blocked.sort_by(|a, b| a.start_s().total_cmp(&b.start_s()));
    let mut cursor = result.start_s();
    let mut out = Vec::new();
    for window in blocked {
        if cursor < window.start_s() {
            out.push(TimeWindow::new(cursor, window.start_s())?);
        }
        cursor = cursor.max(window.end_s());
    }
    if cursor < result.end_s() {
        out.push(TimeWindow::new(cursor, result.end_s())?);
    }
    Ok(out)
}

/// Error budget for transient/thermal/co-sim numeric claims, populated from
/// the solver options actually used plus the run's measured windows.
pub fn transient_error_budget(
    options: &hauksbee_solve::SolverOptions,
    start_s: f64,
    end_s: f64,
    event_time_error_s: f64,
    failed_windows: &[(f64, f64)],
    fallback_windows: &[(f64, f64, &str)],
) -> Result<ErrorBudget, EvidenceError> {
    let result_window = TimeWindow::new(start_s, end_s)?;
    let tolerance = IntegrationTolerance::new(
        options.reltol,
        options.vntol,
        options.abstol,
        options.chgtol,
    )?;
    let mut blocked = Vec::with_capacity(failed_windows.len() + fallback_windows.len());
    let mut failed_typed = Vec::with_capacity(failed_windows.len());
    for &(start, end) in failed_windows {
        let window = checked_subwindow("failed", start, end, result_window)?;
        blocked.push((window, "failed"));
        failed_typed.push(window);
    }
    let mut fallback_typed = Vec::with_capacity(fallback_windows.len());
    for &(start, end, method) in fallback_windows {
        let window = checked_subwindow("fallback", start, end, result_window)?;
        let method = match method {
            "reduced-step" => IntegrationMethod::ReducedStep,
            "backward-euler" => IntegrationMethod::BackwardEuler,
            "cold-start-backward-euler" => IntegrationMethod::ColdStartBackwardEuler,
            "subdivided-backward-euler" => IntegrationMethod::SubdividedBackwardEuler,
            unknown => {
                return Err(EvidenceError::UnknownIntegrationMethod {
                    method: unknown.to_string(),
                })
            }
        };
        blocked.push((window, "fallback"));
        fallback_typed.push((window, method));
    }
    for (i, (first, first_kind)) in blocked.iter().enumerate() {
        for (second, second_kind) in blocked.iter().skip(i + 1) {
            if windows_overlap(*first, *second) {
                return Err(EvidenceError::OverlappingWindows {
                    first_kind,
                    second_kind,
                });
            }
        }
    }

    let mut budget = ErrorBudget::new(tolerance).with_event_time_error(event_time_error_s)?;
    let primary = integration_method(options.integration);
    for window in uncovered_windows(result_window, blocked.iter().map(|(window, _)| *window))? {
        budget = budget.with_method(WindowMethod::new(window, primary)?);
    }
    for (window, method) in fallback_typed {
        budget = budget.with_method(WindowMethod::new(window, method)?);
    }
    for window in failed_typed {
        budget = budget.with_failed_window(window);
    }
    Ok(budget)
}
