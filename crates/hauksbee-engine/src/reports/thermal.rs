//! `--thermal`: steady-state junction temperatures after a short headless co-sim.
//!
//! Reports the junction temperature per dissipating device
//! (Tj = Tambient + P * `theta_JA`). Text or JSON (there is no plain variant).
//! An empty table because the power ICs are open/unresolved is flagged invalid
//! (exit 3), not a false "runs cool" pass,
//! and a PARTIAL-coverage table (rows exist while an active power IC on the
//! live circuit is open/unresolved) escalates to exit 3 BY DEFAULT;
//! `--no-strict-thermal` opts out of the escalation but never of the caveat.

use super::truncate;
use crate::engine::HauksbeeEngine;
use crate::result::{
    coverage_open_active_refs, thermal_coverage, thermal_validity, BindSummary, CheckCoverage,
    JsonInputEvidence, JsonReport, Refusal, ThermalDeviceJson, ThermalJson, Validity,
    EXIT_INVALID_FOR_ANALYSIS,
};
use hauksbee_ir::evidence::Scope;

/// Run the thermal estimate and print it (`json` selects JSON over the text
/// table). Exits with the invalid-for-analysis code when the table is invalid,
/// or (under `strict`, the default; `--no-strict-thermal` opts out) when
/// coverage is only partial.
#[allow(clippy::too_many_arguments, reason = "one per clap flag")]
#[allow(clippy::fn_params_excessive_bools, reason = "independent CLI switches")]
#[allow(clippy::too_many_lines, reason = "linear run: solve, render, gate")]
pub(crate) fn emit_quiet(
    engine: &mut HauksbeeEngine,
    evidence: &crate::evidence::BoardEvidence,
    seconds: f64,
    json: bool,
    plain: bool,
    strict: bool,
    quiet: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    let ambient = engine.scheduler().ambient_c();
    let summary = BindSummary::from_report(engine.report());
    let board_name = engine.report().board_name.clone();
    let rows = collect_thermal(engine, seconds.max(0.05));
    let validity = thermal_validity(rows.len(), &summary);
    // Coverage is the honest "N of M" companion to validity. A PARTIAL table
    // escalates to exit 3 BY DEFAULT (a real-but-incomplete table understates
    // the true load, and a vacuous "runs cool" over an open power IC is the
    // most dangerous outcome); `--no-strict-thermal` opts out of the exit-code
    // escalation while keeping the INCONCLUSIVE caveat. validity is unchanged.
    let coverage = thermal_coverage(rows.len(), &summary);
    let partial_refusal = coverage.partial.then(|| {
        Refusal::new(
            "a complete thermal-safety conclusion for every active power IC",
            format!(
                "{} of {} active power IC(s) are open/unresolved and absent from the thermal result",
                coverage.open_active_on_live_circuit, coverage.total_active_count
            ),
            vec![format!(
                "the {} dissipating device row(s) shown were solved and remain valid",
                coverage.dissipating_count
            )],
            "bind the named open active ICs with --models-dir, then rerun the same --thermal command",
        )
    });
    // Under `--no-strict-thermal` the exit code stays 0, but the machine verdict
    // still refuses the complete-coverage claim (below), so the JSON can never
    // read `pass` beside the INCONCLUSIVE caveat the text surface prints.
    let strict_partial_refusal = partial_refusal.clone().filter(|_| strict);
    // The open active ICs to NAME in the caveat, computed before `summary` is
    // moved into the JSON report.
    let coverage_refs = coverage_open_active_refs(&summary);
    let budget = engine.scheduler().error_budget()?;
    // Each row's temperature on a package-guessed theta_JA qualifies that row.
    let scope = |r: &str, _| {
        vec![Scope::Check {
            check: "thermal".into(),
            kind: Some(r.into()),
        }]
    };
    let mut guessed = engine.scheduler().stress.inferred_theta_ja(scope);
    guessed.extend(engine.scheduler().stress.rating_disclosures());
    let evidence = evidence.clone().with_assumptions(guessed)?;
    let mut maps = Vec::new();
    // A dissipating device the board connects to no net keeps its row; it
    // just has no board location to back a map, as on the co-sim surface.
    let mut unmapped = Vec::new();
    for (reference, tj_c, over_limit) in &rows {
        let map = evidence.check_simulation_map_if_mapped(
            format!(
                "{reference} peak junction temperature is {tj_c:.3} C ({})",
                if *over_limit {
                    "over limit"
                } else {
                    "within limit"
                }
            ),
            &[],
            std::slice::from_ref(reference),
            Some(budget.clone()),
            Some(("thermal", reference)),
        )?;
        match map {
            Some(map) => maps.push(map),
            None => unmapped.push(reference.as_str()),
        }
    }
    let unmapped_note = (!unmapped.is_empty()).then(|| {
        format!(
            "Not mapped to the board: {} connect(s) to no board net, so those thermal rows have no board location.",
            unmapped.join(", ")
        )
    });
    let report_evidence = evidence.with_maps(maps);
    // The machine document is built in EVERY output mode: building it is what
    // tells the CI artifacts the run's verdict (see `JsonReport::to_json`). Only
    // `--json` prints it.
    let document = {
        let mut jr = JsonReport::new(&board_name, summary)
            .with_inputs(inputs)
            .with_evidence(&report_evidence);
        if !strict {
            // Opted out of the exit-code escalation only; the claim stays refused.
            jr.refusal.clone_from(&partial_refusal);
        }
        // Surface the partial-coverage caveat as an info note too, so a JSON
        // consumer that ignores `coverage` still sees the honesty annotation.
        // Same INCONCLUSIVE lead as the text caveat: one vocabulary, no
        // text-vs-JSON dialect fork.
        if coverage.partial {
            jr.push_coverage_note(format!(
                "INCONCLUSIVE: {}",
                thermal_coverage_caveat(&coverage)
            ));
        }
        if let Some(note) = &unmapped_note {
            jr.push_coverage_note(note.clone());
        }
        jr.push_coverage_note(crate::scope::single_check_note("thermal"));
        jr.thermal = Some(ThermalJson {
            validity: strict_partial_refusal
                .clone()
                .map_or_else(|| validity.clone(), Validity::refused),
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
        jr.to_json()
    };
    if json {
        println!("{document}");
    } else {
        if plain {
            println!(
                "Plain-language thermal: temperatures below cover modeled dissipating devices only; any unmodeled power parts make the overall verdict invalid."
            );
        }
        print!(
            "{}",
            render_thermal_text(&rows, ambient, &validity, &coverage)
        );
        // Partial-coverage caveat (text path): the table is real but some active
        // power IC on the live circuit is open/unresolved, so the result
        // understates the true thermal load. Naming the parts keeps this from
        // being a silent false-comfort pass.
        if coverage.partial {
            emit_thermal_coverage_caveat(&coverage, &coverage_refs);
        }
        if let Some(note) = &unmapped_note {
            println!("{note}");
        }
        print!(
            "{}",
            super::render_evidence_appendix(&report_evidence, quiet, false)
        );
    }
    if !validity.valid {
        if !json {
            if let Some(refusal) = &validity.refusal {
                eprintln!("{}", refusal.render_text());
            }
        }
        crate::reports::ci_artifacts::exit_with_refusal(
            EXIT_INVALID_FOR_ANALYSIS,
            validity.refusal.as_ref().expect("invalid thermal refusal"),
        );
    }
    // Default escalation: partial coverage fails (exit 3) unless the caller
    // opted out with --no-strict-thermal. The opt-out changes ONLY the exit
    // code; the INCONCLUSIVE coverage caveat above prints either way.
    if coverage.partial && strict {
        if !json {
            if let Some(refusal) = &strict_partial_refusal {
                eprintln!("{}", refusal.render_text());
            }
        }
        crate::reports::ci_artifacts::exit_with_refusal(
            EXIT_INVALID_FOR_ANALYSIS,
            strict_partial_refusal
                .as_ref()
                .expect("partial thermal refusal"),
        );
    }
    if report_evidence.is_undermined() && strict {
        let refusal = crate::result::Refusal::new(
            "a trustworthy thermal result",
            "the thermal evidence is undermined",
            vec!["the emitted component and coverage inventory remains available"],
            "resolve the cited evidence assumptions, then rerun the thermal analysis",
        );
        crate::reports::ci_artifacts::exit_with_refusal(EXIT_INVALID_FOR_ANALYSIS, &refusal);
    }
    Ok(())
}

/// Run a short headless co-sim and collect the steady-state junction-temperature
/// estimate per dissipating device. Returns `(reference, peak_Tj_C, over_limit)`
/// rows, sorted hottest-first.
#[allow(clippy::while_float, reason = "f64 time sum fixes the frame count")]
fn collect_thermal(engine: &mut HauksbeeEngine, seconds: f64) -> Vec<(String, f64, bool)> {
    use hauksbee_frontdoor_api::engine::Engine;
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
    // State the honest facts: how many active power ICs are OPEN (and so
    // dissipate nothing), out of the total on the live circuit. Being
    // resolved/non-open is NOT the same as producing a thermal row (a resolved
    // logic IC dissipating ~0 W yields no row), so this must never be phrased as
    // a count of ICs "in the table"; that overstates coverage in a caveat whose
    // whole point is to prevent false comfort.
    format!(
        "thermal coverage is PARTIAL: {} of {} active power IC(s) on the live circuit \
         are open/unresolved and dissipate nothing in simulation. The {} dissipating \
         part(s) shown are real, but the result UNDERSTATES the true load.",
        coverage.open_active_on_live_circuit,
        coverage.total_active_count,
        coverage.dissipating_count,
    )
}

/// Emit the partial-coverage caveat on the text path, naming the open active
/// ICs. Leads with INCONCLUSIVE, the same verdict vocabulary the lint/SI
/// surfaces use for unmodelled critical parts.
fn emit_thermal_coverage_caveat(coverage: &CheckCoverage, open_refs: &[String]) {
    eprintln!("INCONCLUSIVE: {}", thermal_coverage_caveat(coverage));
    if !open_refs.is_empty() {
        eprintln!(
            "  open/unresolved active IC(s): {}. Bind them with --models-dir, then re-run. \
             (Partial coverage FAILS with exit 3 by default; pass --no-strict-thermal to \
             keep exit 0 while this caveat still prints.)",
            open_refs.join(", ")
        );
    }
}

/// Render the thermal result as text. When the result is invalid (empty table
/// because the dissipating devices are unresolved/open), print a loud WARNING
/// naming the reason rather than a near-empty table that reads as "runs cool".
fn render_thermal_text(
    rows: &[(String, f64, bool)],
    ambient_c: f64,
    validity: &Validity,
    coverage: &CheckCoverage,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if !validity.valid {
        let reason = validity
            .reason
            .as_deref()
            .unwrap_or("no resolved dissipating devices");
        let _ = writeln!(out, "WARNING: thermal result not valid: {reason}");
        let _ = writeln!(
            out,
            "  (a thermal table covering no dissipating devices is NOT a 'runs cool' pass. \
             Bind the power ICs with --models-dir, then re-run.)"
        );
        return out;
    }
    if coverage.partial {
        let _ = writeln!(
            out,
            "INVALID: {} dissipating-class power part(s) were NOT modeled; the table below covers only {} MODELED dissipating device(s).",
            coverage.open_active_on_live_circuit, rows.len()
        );
    }
    let _ = writeln!(
        out,
        "\nsteady-state junction temperature (Tj = {ambient_c:.0} C + P * theta_JA):"
    );
    if rows.is_empty() {
        let _ = writeln!(
            out,
            "  no dissipating device reached a measurable temperature (board carries no static load)."
        );
        return out;
    }
    let _ = writeln!(
        out,
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
        let _ = writeln!(
            out,
            "│ {:<18} │ {:>7.1}   │ {:<8} │",
            truncate(reference, 18),
            tj,
            status
        );
    }
    let _ = writeln!(out, "└────────────────────┴───────────┴──────────┘");
    if n_over > 0 {
        let _ = writeln!(
            out,
            "\n{n_over} MODELED device(s) over their junction-temperature limit."
        );
    } else if coverage.partial {
        let _ = writeln!(
            out,
            "\nall {} MODELED dissipating devices are within their junction-temperature limit; {} power part(s) unmodeled, verdict INVALID.",
            rows.len(), coverage.open_active_on_live_circuit
        );
    } else {
        let _ = writeln!(
            out,
            "\nall {} modeled dissipating devices are within their junction-temperature limit.",
            rows.len()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::CheckCoverage;

    #[test]
    fn partial_coverage_caveat_does_not_overstate_ics_in_the_table() {
        // A resolved active IC that dissipates ~0 W produces no thermal row, so
        // being non-open is NOT being in the table. The caveat must report the
        // OPEN count, never "total - open" dressed up as coverage: here
        // total_active=3, open=1, and every row comes from a passive.
        let cov = CheckCoverage {
            resolved_fraction: 0.0,
            dissipating_count: 4, // passives
            total_active_count: 3,
            open_active_on_live_circuit: 1,
            partial: true,
        };
        let msg = thermal_coverage_caveat(&cov);
        assert!(
            msg.contains("1 of 3 active power IC(s)") && msg.contains("dissipate nothing"),
            "must state the open count, not the covered count: {msg}"
        );
    }

    #[test]
    fn partial_thermal_table_names_unmodelled_power_parts_inline() {
        let coverage = CheckCoverage {
            resolved_fraction: 0.5,
            dissipating_count: 2,
            total_active_count: 3,
            open_active_on_live_circuit: 2,
            partial: true,
        };
        let validity = Validity::valid();
        let text = render_thermal_text(
            &[
                ("R1".to_string(), 25.1, false),
                ("R2".to_string(), 25.0, false),
            ],
            25.0,
            &validity,
            &coverage,
        );
        assert!(
            text.starts_with("INVALID: 2 dissipating-class power part(s) were NOT modeled"),
            "coverage invalidity must lead the table:\n{text}"
        );
        assert!(
            text.contains(
                "all 2 MODELED dissipating devices are within their junction-temperature limit; \
                 2 power part(s) unmodeled, verdict INVALID"
            ),
            "the closing line must condition the clean rows on modeled-only coverage:\n{text}"
        );
    }
}
