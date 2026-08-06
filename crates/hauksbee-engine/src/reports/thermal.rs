//! `--thermal`: a short headless co-sim, then the steady-state junction
//! temperature per dissipating device (Tj = Tambient + P * theta_JA). Text or
//! JSON (there is no plain variant). An empty table because the power ICs are
//! open/unresolved is flagged invalid (exit 3), not a false "runs cool" pass.

use crate::engine::HauksbeeEngine;
use crate::result::{
    coverage_open_active_refs, thermal_coverage, thermal_validity, BindSummary, CheckCoverage,
    JsonNote, JsonNoteKind, JsonReport, ThermalDeviceJson, ThermalJson, Validity,
    EXIT_INVALID_FOR_ANALYSIS,
};

/// Run the thermal estimate and print it (`json` selects JSON over the text
/// table). Exits with the invalid-for-analysis code when the table is invalid, or
/// (under `strict_thermal`) when coverage is only partial.
pub fn emit(
    engine: &mut HauksbeeEngine,
    evidence: &crate::evidence::BoardEvidence,
    ambient: f64,
    seconds: f64,
    json: bool,
    strict_thermal: bool,
) -> anyhow::Result<()> {
    engine.scheduler_mut().set_ambient_c(ambient);
    let summary = BindSummary::from_report(engine.report());
    let board_name = engine.report().board_name.clone();
    let rows = collect_thermal(engine, seconds.max(0.05));
    let validity = thermal_validity(rows.len(), &summary);
    // Coverage is the honest "N of M" companion to validity. It is NON-gating by
    // default (the partial case stays exit 0); only --strict-thermal escalates a
    // partial-coverage table to exit 3. validity stays unchanged.
    let coverage = thermal_coverage(rows.len(), &summary);
    // The open active ICs to NAME in the caveat, computed before `summary` is
    // moved into the JSON report.
    let coverage_refs = coverage_open_active_refs(&summary);
    if json {
        let mut jr = JsonReport::new(&board_name, summary).with_evidence(evidence);
        // Surface the partial-coverage caveat as an info note too, so a JSON
        // consumer that ignores `coverage` still sees the honesty annotation.
        if coverage.partial {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: thermal_coverage_caveat(&coverage),
            });
        }
        jr.thermal = Some(ThermalJson {
            validity: validity.clone(),
            ambient_c: ambient,
            devices: rows
                .iter()
                .map(|(r, tj, over)| ThermalDeviceJson {
                    reference: r.clone(),
                    tj_c: *tj,
                    over_limit: *over,
                })
                .collect(),
            coverage: Some(coverage.clone()),
        });
        println!("{}", jr.to_json());
    } else {
        render_thermal_text(&rows, ambient, &validity);
        // Partial-coverage caveat (text path): the table is real but some active
        // power IC on the live circuit is open/unresolved, so the result
        // understates the true thermal load. Naming the parts keeps this from
        // being a silent false-comfort pass.
        if coverage.partial {
            emit_thermal_coverage_caveat(&coverage, &coverage_refs);
        }
        print!("{}", evidence.render_plain());
    }
    if !validity.valid {
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }
    // Opt-in escalation: partial coverage fails only under --strict-thermal.
    if coverage.partial && strict_thermal {
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }
    if evidence.is_undermined() && strict_thermal {
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }
    Ok(())
}

/// Run a short headless co-sim and collect the steady-state junction-temperature
/// estimate per dissipating device. Returns `(reference, peak_Tj_C, over_limit)`
/// rows, sorted hottest-first.
fn collect_thermal(engine: &mut HauksbeeEngine, seconds: f64) -> Vec<(String, f64, bool)> {
    use hauksbee_server::engine::Engine;
    use std::collections::HashMap;

    eprintln!("thermal: {seconds:.2}s co-sim...");
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    // Peak temperature seen per device over the run (steady state is reached
    // quickly; the peak is the worst-case junction temperature).
    let mut peak_temp: HashMap<String, f64> = HashMap::new();
    let mut overtemp: HashMap<String, (f64, f64)> = HashMap::new(); // ref -> (Tj, limit)
    while t < seconds {
        let frame = engine.step(frame_dt);
        for (reference, &tj) in &engine.scheduler().temp_states() {
            let e = peak_temp
                .entry(reference.clone())
                .or_insert(f64::NEG_INFINITY);
            if tj > *e {
                *e = tj;
            }
        }
        for f in &frame.faults {
            if f.kind == "overtemperature" {
                overtemp.insert(f.component.clone(), (f.value, f.limit));
            }
        }
        t += frame_dt;
    }

    let mut rows: Vec<(String, f64, bool)> = peak_temp
        .into_iter()
        .filter(|(_, v)| v.is_finite())
        .map(|(r, tj)| {
            let over = overtemp.contains_key(&r);
            (r, tj, over)
        })
        .collect();
    rows.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Reference-name tiebreak so two devices at equal Tj (e.g. a dual
            // package's pooled siblings) always emit in a stable order.
            .then_with(|| a.0.cmp(&b.0))
    });
    rows
}

/// The one-line partial-coverage caveat text (shared by JSON note + stderr).
fn thermal_coverage_caveat(coverage: &CheckCoverage) -> String {
    // State the honest facts: how many active power ICs are OPEN (and so dissipate
    // nothing), out of the total on the live circuit. The earlier wording claimed
    // "{total - open} active IC(s) are in the table", but being resolved/non-open
    // is NOT the same as producing a thermal row, a resolved logic IC that
    // dissipates ~0 W yields no row yet was counted as "in the table", overstating
    // coverage in a caveat whose whole point is to prevent false comfort.
    format!(
        "thermal coverage is PARTIAL: {} of {} active power IC(s) on the live circuit \
         are open/unresolved and dissipate nothing in simulation. The {} dissipating \
         part(s) shown are real, but the result UNDERSTATES the true load.",
        coverage.open_active_on_live_circuit,
        coverage.total_active_count,
        coverage.dissipating_count,
    )
}

/// Emit the partial-coverage caveat on the text path, naming the open active ICs.
fn emit_thermal_coverage_caveat(coverage: &CheckCoverage, open_refs: &[String]) {
    eprintln!("CAVEAT: {}", thermal_coverage_caveat(coverage));
    if !open_refs.is_empty() {
        eprintln!(
            "  open/unresolved active IC(s): {}. Bind them with --models-dir, then re-run \
             (or pass --strict-thermal to FAIL on partial coverage).",
            open_refs.join(", ")
        );
    }
}

/// Render the thermal result as text. When the result is invalid (empty table
/// because the dissipating devices are unresolved/open), print a loud WARNING
/// naming the reason rather than a near-empty table that reads as "runs cool".
fn render_thermal_text(rows: &[(String, f64, bool)], ambient_c: f64, validity: &Validity) {
    if !validity.valid {
        let reason = validity
            .reason
            .as_deref()
            .unwrap_or("no resolved dissipating devices");
        eprintln!("WARNING: thermal result not valid: {reason}");
        eprintln!(
            "  (a thermal table covering no dissipating devices is NOT a 'runs cool' pass. \
             Bind the power ICs with --models-dir, then re-run.)"
        );
        return;
    }
    println!("\nsteady-state junction temperature (Tj = {ambient_c:.0} C + P * theta_JA):");
    if rows.is_empty() {
        println!("  no dissipating device reached a measurable temperature (board carries no static load).");
        return;
    }
    println!(
        "┌────────────────────┬───────────┬──────────┐\n\
         │ Component          │  Tj (C)   │  status  │\n\
         ├────────────────────┼───────────┼──────────┤"
    );
    let mut n_over = 0;
    for (reference, tj, over) in rows {
        let status = if *over {
            n_over += 1;
            "OVER".to_string()
        } else {
            "ok".to_string()
        };
        println!(
            "│ {:<18} │ {:>7.1}   │ {:<8} │",
            truncate(reference, 18),
            tj,
            status
        );
    }
    println!("└────────────────────┴───────────┴──────────┘");
    if n_over > 0 {
        println!("\n{n_over} device(s) over their junction-temperature limit.");
    } else {
        println!("\nall dissipating devices within their junction-temperature limit.");
    }
}

/// Truncate to at most `max` chars (byte-for-byte the binary's helper).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::CheckCoverage;

    #[test]
    fn partial_coverage_caveat_does_not_overstate_ics_in_the_table() {
        // R42: the caveat computed "covered = total - open" and claimed that many
        // active ICs were "in the table". But a resolved active IC that dissipates
        // ~0 W produces no thermal row, so being non-open is NOT being in the
        // table; the wording overstated coverage in a message whose whole purpose
        // is to prevent false comfort. total_active=3, open=1, and the rows come
        // only from passives (dissipating_count counts them): the caveat must not
        // claim "2 of 3 active power IC(s) ... are in the table".
        let cov = CheckCoverage {
            resolved_fraction: 0.0,
            dissipating_count: 4, // passives
            total_active_count: 3,
            open_active_on_live_circuit: 1,
            partial: true,
        };
        let msg = thermal_coverage_caveat(&cov);
        assert!(
            !msg.contains("are in the table"),
            "must not claim non-open active ICs are in the table: {msg}"
        );
        // It states the honest fact: how many active ICs are open/dissipate nothing.
        assert!(
            msg.contains("1 of 3 active power IC(s)") && msg.contains("dissipate nothing"),
            "must state the open count honestly: {msg}"
        );
    }
}
