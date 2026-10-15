//! `--ac`: small-signal AC sweep on the bound circuit.
//!
//! Prints a Bode (magnitude dB + phase) table for the requested net(s), and,
//! with `--ac-loop`, gain crossover and phase margin. Refuses (exit 3) to
//! present a meaningless all-sentinel or no-signal-path sweep as valid data.

use std::{fmt::Write as _, path::Path};

use crate::result::{
    ac_is_all_sentinel, coverage_open_active_refs, no_signal_path_reason, AcJson, AcNetJson,
    BindSummary, CheckCoverage, JsonReport, Refusal, Validity, EXIT_INVALID_FOR_ANALYSIS,
};

/// One net's Bode rows: (net, [(`freq_hz`, `mag_db`, `phase_deg`)]).
type NetBode = (String, Vec<(f64, f64, f64)>);

/// What an AC refusal renders against: board, evidence, inputs, `--json` and `--quiet`.
type AcContext<'a> = (
    &'a crate::BoundBoard,
    &'a crate::evidence::BoardEvidence,
    &'a [crate::result::JsonInputEvidence],
    bool,
    bool,
);

#[allow(clippy::too_many_arguments, reason = "one per --ac flag")]
#[allow(clippy::too_many_lines, reason = "validate, sweep, refuse, emit")]
pub(crate) fn emit_quiet(
    bound: &crate::BoundBoard,
    evidence: &crate::evidence::BoardEvidence,
    ac_arg: &str,
    ac_nodes: &[String],
    csv: Option<&Path>,
    ac_loop: Option<&str>,
    json: bool,
    quiet: bool,
    inputs: &[crate::result::JsonInputEvidence],
) -> anyhow::Result<()> {
    use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions};

    let spec = AcSpec::parse(ac_arg).map_err(|e| anyhow::anyhow!("--ac: {e}"))?;
    let circuit = &bound.circuit;
    let ctx = (bound, evidence, inputs, json, quiet);

    // Default to every non-ground node if no --ac-node given.
    let nodes: Vec<String> = if ac_nodes.is_empty() {
        (1..u32::try_from(circuit.node_count())?)
            .map(|i| circuit.node_name(hauksbee_ir::NodeId(i)).to_string())
            .collect()
    } else {
        ac_nodes.to_vec()
    };

    // Validate explicitly requested nets BEFORE the sweep: when EVERY
    // requested node is unknown there is nothing to sweep, so this is user
    // misuse caught up front (an error with near matches, the --probe
    // contract) with the documented invalid-for-analysis exit (3), instead of
    // paying for a full sweep and then only warning about it. A PARTIAL miss
    // still sweeps: the found nets are real data and the JSON contract
    // surfaces the missing ones in `not_found_nets`.
    if !ac_nodes.is_empty()
        && nodes
            .iter()
            .all(|n| !bound.net_names.iter().any(|k| k == n))
    {
        let missing = nodes.join(", ");
        let reason = format!("no requested AC nodes found in the circuit: {missing}");
        let refusal = Refusal::new(
            format!("an AC response for requested node(s) {missing}"),
            reason.clone(),
            vec!["board extraction and component binding completed"],
            "run --list-nets, replace the missing --ac-node names, then rerun the same sweep",
        );
        refuse_ac(ctx, &refusal, &[], &nodes, || {
            eprintln!("error: AC result not valid: {reason}");
            for n in &nodes {
                let near = crate::reports::cosim::nearest_nets(n, &bound.net_names, 5);
                if !near.is_empty() {
                    eprintln!("  '{n}': did you mean {}?", near.join(", "));
                }
            }
            eprintln!("  run --list-nets to see every net name, then re-run.");
        });
    }

    let solver_options = SolverOptions::default();
    let resp = AcAnalysis::new(solver_options)
        .run(circuit, &spec)
        .map_err(|e| anyhow::anyhow!("AC analysis: {e}"))?;

    // Injection-point honesty: with no dedicated AC source the sweep drives EVERY
    // independent source (the power rails included), so the Bode is a superposition
    // of all stimuli, not a single-input transfer function. On an extracted board
    // there is usually no VINJ, so warn, a user reading a "transfer function" off
    // this would be measuring the wrong thing. (SPICE decks with explicit `AC`
    // stimulus set their own drive and are exempt: that IS a chosen injection.)
    if !json && !hauksbee_solve::has_dedicated_ac_source(circuit) {
        eprintln!(
            "NOTE: no dedicated AC injection source: the sweep drove every independent \n\
             source (power rails included), so this Bode is a superposition, not a \n\
             single-input transfer function. To measure a real transfer function, name \n\
             the drive source VINJ/VLOOP/IINJ/ILOOP (insert one at the input/loop with \n\
             board-as-code: {}), then re-run --ac.",
            hauksbee_ir::docs_url("docs/ingest/BOARD_AS_CODE.md")
        );
    }

    // Collect the bode rows once so both the validity check and the renderers
    // read the SAME data (one structured result, no forked logic).
    let summary = BindSummary::from_report(&bound.report);
    let per_net: Vec<NetBode> = nodes
        .iter()
        .map(|net| (net.clone(), resp.bode(circuit, net)))
        .collect();

    // An AC sweep where EVERY reported net is at the -6000 dB sentinel has no
    // signal path; it is a meaningless result, not data. Refuse to present it as
    // a Bode table; name the unresolved driving ICs and exit 3 ("board invalid
    // for the requested analysis"), never 0.
    let nonempty: Vec<&NetBode> = per_net.iter().filter(|(_, b)| !b.is_empty()).collect();

    // If EVERY requested net produced no data at all (none exist in the
    // circuit), `nonempty` is empty. That case slips past the all-sentinel guard
    // below (which requires `!nonempty.is_empty()`), which would let the JSON
    // path emit `ac: { valid: true, nets: [] }` with exit 0, a meaningless result
    // reported as valid. Refuse it: name the missing nodes, emit valid:false, and
    // exit 3, exactly like the all-sentinel path. Only fires when
    // the user explicitly asked for nodes; the "every node" default never lands
    // here because at least one real node exists in any bound circuit.
    if nonempty.is_empty() {
        let missing = nodes.join(", ");
        let reason = format!("no requested AC nodes found in the circuit: {missing}");
        let refusal = Refusal::new(
            format!("an AC response for requested node(s) {missing}"),
            reason.clone(),
            vec!["deck/board parsing and circuit assembly completed"],
            "run --list-nets, replace the missing --ac-node names, then rerun the same sweep",
        );
        // The requested nets are ALL missing here, surface them in the
        // structured field too, not just the prose reason, so a machine
        // consumer reading `not_found_nets` sees them (matching the
        // partial-sweep path below and this report's never-silent promise).
        refuse_ac(ctx, &refusal, &[], &nodes, || {
            eprintln!("error: AC result not valid: {reason}");
            // Did-you-mean per missing node, then the discoverability pointer,
            // matching the net-not-found pattern the co-sim / spec surfaces use.
            for n in &nodes {
                let near = crate::reports::cosim::nearest_nets(n, &bound.net_names, 5);
                if !near.is_empty() {
                    eprintln!("  '{n}': did you mean {}?", near.join(", "));
                }
            }
            eprintln!(
                "  (none of the requested --ac-node nets exist in this circuit; \
                 run `--list-nets` to see every net name, then re-run.)"
            );
        });
    }

    let all_sentinel = !nonempty.is_empty() && nonempty.iter().all(|(_, b)| ac_is_all_sentinel(b));

    if all_sentinel {
        // Name a representative net for the reason (the first reported one).
        let net = nonempty
            .first()
            .map_or("the requested node", |(n, _)| n.as_str());
        let reason = no_signal_path_reason(net, &summary);
        let refusal = Refusal::new(
            format!("an AC transfer response at {net}"),
            reason.clone(),
            vec!["deck/board parsing and circuit assembly completed"],
            "bind the unresolved driving ICs with --models-dir, then rerun the same sweep",
        );
        refuse_ac(ctx, &refusal, &[], &[], || {
            eprintln!("WARNING: AC result not valid: {reason}");
            eprintln!(
                "  (every reported net is at the {:.0} dB floor: no path to drive it. \
                 Bind the driving ICs with --models-dir, then re-run.)",
                crate::result::AC_FLOOR_DB
            );
        });
    }

    // Loop-net validity guard (degeneracy), applied to BOTH --json and text
    // BEFORE either emits. A loop/break net that is missing from the circuit OR
    // sits at the dB floor has no feedback path to measure: LoopStability would
    // yield a meaningless ~-6000 dB gain on the text path, while the --json path
    // (which returns just below) would emit valid:true and exit 0, a structured
    // false-pass. Refuse it identically on both surfaces with exit 3.
    if let Some(loop_net) = ac_loop {
        let loop_bode = resp.bode(circuit, loop_net);
        if loop_bode.is_empty() || ac_is_all_sentinel(&loop_bode) {
            let reason = if loop_bode.is_empty() {
                format!("loop/break net '{loop_net}' not found in the circuit")
            } else {
                no_signal_path_reason(loop_net, &summary)
            };
            let refusal = Refusal::new(
                format!("loop-stability margins at {loop_net}"),
                reason.clone(),
                vec!["the ordinary AC sweep completed before the loop-specific guard"],
                "choose an existing driven feedback net (use --list-nets), bind its driving ICs if open, then rerun --ac-loop",
            );
            // Structured refusal on the JSON surface too, a consumer reading
            // stdout (not the exit code) must see valid:false, not empty output.
            let loop_nets = [loop_net.to_string()];
            refuse_ac(ctx, &refusal, &loop_nets, &[], || {
                eprintln!("WARNING: --ac-loop result not valid: {reason}");
                eprintln!(
                    "  (no feedback path to measure at '{loop_net}'. Bind the driving \
                     ICs with --models-dir, then re-run.)"
                );
            });
        }
    }

    // Optional CSV of the full sweep. Written here, BEFORE the `--json` early
    // return, so the artifact is produced on BOTH surfaces: a caller that wants
    // structured JSON on stdout AND a CSV file on disk (a common CI/tooling
    // pattern) gets both from one run. Placed after the validity guards so an
    // invalid/empty sweep still refuses first and writes no misleading CSV.
    if let Some(path) = csv {
        let (out, no_path) = ac_csv_body(&per_net);
        std::fs::write(path, out)?;
        eprintln!("wrote {}", path.display());
        if !no_path.is_empty() {
            eprintln!(
                "  (omitted {} net(s) with no signal path from the CSV: {})",
                no_path.len(),
                no_path.join(", ")
            );
        }
    }

    // Causal evidence for the numbers actually emitted. A frequency-domain
    // sweep consumes the board/model path and solver tolerances, but no
    // transient time window or firmware artifact.
    let ac_budget = crate::evidence::BoardEvidence::solver_error_budget(&solver_options)?;
    let mut ac_maps = Vec::new();
    for (net, bode) in &per_net {
        if bode.is_empty() || ac_is_all_sentinel(bode) {
            continue;
        }
        // A solver node the board does not hold (blank, merged) keeps its
        // Bode rows; it just has no board location to back a map.
        ac_maps.extend(evidence.simulation_map_if_mapped(
            format!("AC response for net {net}"),
            std::slice::from_ref(net),
            &[],
            Some(ac_budget.clone()),
        )?);
    }
    let report_evidence = evidence.clone().with_maps(ac_maps);

    if json {
        // Valid sweep: emit the structured bode per net. Skip empty/not-found
        // nets AND any individual net that is all-sentinel (no path to THIS net),
        // so a JSON consumer never sees -6000 dB rows presented as real data
        // alongside valid:true. The skipped nets are listed so the omission is
        // explicit, never silent.
        let mut jr = JsonReport::new(&bound.name, summary)
            .with_inputs(inputs)
            .with_evidence(&report_evidence);
        let nets: Vec<AcNetJson> = per_net
            .iter()
            .filter(|(_, b)| !b.is_empty() && !ac_is_all_sentinel(b))
            .map(|(net, b)| AcNetJson {
                net: net.clone(),
                points: b.iter().map(|(f, db, ph)| [*f, *db, *ph]).collect(),
            })
            .collect();
        let no_path: Vec<String> = per_net
            .iter()
            .filter(|(_, b)| !b.is_empty() && ac_is_all_sentinel(b))
            .map(|(net, _)| net.clone())
            .collect();
        // A requested net whose bode is EMPTY was not found in the circuit at
        // all. It lands in neither `nets` nor `no_path`, so without surfacing it
        // here the JSON would silently drop it while the text path warns; the
        // exact "never silent" promise this report makes.
        let not_found: Vec<String> = per_net
            .iter()
            .filter(|(_, b)| b.is_empty())
            .map(|(net, _)| net.clone())
            .collect();
        // Honest coverage for a partially-valid sweep: some requested nets carry
        // signal, others sit at the floor. Non-gating; mirrors `no_signal_path_nets`.
        let requested = nets.len() + no_path.len();
        let coverage = if no_path.is_empty() {
            None
        } else {
            #[allow(clippy::cast_precision_loss, reason = "net counts never reach 2^52")]
            let frac = if requested == 0 {
                1.0
            } else {
                (nets.len() as f64 / requested as f64).clamp(0.0, 1.0)
            };
            Some(CheckCoverage {
                resolved_fraction: frac,
                dissipating_count: nets.len(),
                total_active_count: requested,
                open_active_on_live_circuit: no_path.len(),
                partial: true,
            })
        };
        jr.ac = Some(AcJson {
            validity: Validity::valid(),
            nets,
            no_signal_path_nets: no_path,
            not_found_nets: not_found,
            coverage,
        });
        jr.push_coverage_note(crate::scope::single_check_note("AC"));
        println!("{}", jr.to_json());
        return Ok(());
    }

    // Print a Bode table per requested node. Track how many of the requested
    // nets had no signal path so we can print an end-of-run summary that matches
    // the JSON surface's `no_signal_path_nets` list (text/JSON parity).
    let mut no_path_nets: Vec<String> = Vec::new();
    let mut requested_nets = 0usize;
    // Echo the parsed sweep so `1M` reading as mega (not SPICE milli) is visible.
    println!("AC sweep {spec}");
    for (net, bode) in &per_net {
        if bode.is_empty() {
            eprintln!("warning: net '{net}' not found in circuit; skipping");
            continue;
        }
        requested_nets += 1;
        // A single net at the floor amid others that carry signal is still a
        // local "no path here", caveat it rather than presenting -6000 as data.
        if ac_is_all_sentinel(bode) {
            no_path_nets.push(net.clone());
            println!(
                "\nAC sweep: net '{net}': NO SIGNAL PATH (all points at the {:.0} dB floor); result not meaningful for this net.",
                crate::result::AC_FLOOR_DB
            );
            continue;
        }
        println!("\nAC sweep: net '{net}' ({} points)", bode.len());
        println!(
            "┌────────────────┬───────────────┬───────────────┐\n\
             │ Freq (Hz)      │ Mag (dB)      │ Phase (deg)   │\n\
             ├────────────────┼───────────────┼───────────────┤"
        );
        for (f, db, ph) in bode {
            println!("│ {f:>14.4} │ {db:>13.4} │ {ph:>13.3} │");
        }
        println!("└────────────────┴───────────────┴───────────────┘");
    }

    // End-of-run partial-sentinel summary (text/JSON parity): name how many of
    // the requested nets had no signal path, matching JSON's no_signal_path_nets.
    // Non-gating: a partially-valid sweep still exits 0 on the text path.
    if !no_path_nets.is_empty() {
        println!(
            "\nAC coverage: {} of {} requested net(s) had no signal path (at the {:.0} dB floor): {}.",
            no_path_nets.len(),
            requested_nets,
            crate::result::AC_FLOOR_DB,
            no_path_nets.join(", "),
        );
    }

    // Active-circuit coverage caveat (the false-comfort case): the per-net sentinel
    // check above only catches a net AT the dB floor. A board whose active devices
    // (op-amps, drivers, MOSFETs, MCU) are UNRESOLVED -> OPEN still solves as a
    // passive shell and prints a clean, authoritative-looking Bode that is NOT the
    // real loop. The --json path carries this via the bind summary (active_path_
    // unresolved); the TEXT path must say it too, or the result reads as trustworthy.
    let ac_open = coverage_open_active_refs(&summary);
    if !ac_open.is_empty() {
        eprintln!(
            "\nINCONCLUSIVE: this AC result is NOT trustworthy: {} active IC(s) on the live circuit \
             are unresolved/open ({}), so the response/loop shown is a passive shell, not the real \
             circuit. Bind them with --models-dir, then re-run.",
            ac_open.len(),
            ac_open.join(", "),
        );
    }

    // Optional loop-stability report (text only). The loop-net validity guard
    // above already refused a missing/floored loop net (exit 3) for both surfaces,
    // so by here the net carries signal and LoopStability has a real response.
    if let Some(loop_net) = ac_loop {
        let st = LoopStability::from_response(&resp, circuit, loop_net)
            .map_err(|e| anyhow::anyhow!("--ac-loop: {e}"))?;
        let m = st.margins();
        println!("\nLoop stability at net '{loop_net}':");
        println!("  DC/low-f loop gain : {:.2} dB", m.dc_gain_db);
        match (m.gain_crossover_hz, m.phase_margin_deg) {
            (Some(fc), Some(pm)) => {
                println!("  gain crossover     : {fc:.4} Hz (|T| = 0 dB)");
                println!("  phase margin       : {pm:.2} deg");
            }
            _ => println!("  gain crossover     : none in band (loop never reaches 0 dB)"),
        }
        match (m.phase_crossover_hz, m.gain_margin_db) {
            (Some(fp), Some(gm)) => {
                println!("  phase crossover    : {fp:.4} Hz (phase = -180 deg)");
                println!("  gain margin        : {gm:.2} dB");
            }
            _ => println!("  phase crossover    : none in band (phase never reaches -180 deg)"),
        }
    }

    print!(
        "{}",
        super::render_evidence_appendix(&report_evidence, quiet, false)
    );

    Ok(())
}

/// Emit an AC refusal as structured JSON, or as the text lead plus refusal and evidence
/// appendix, then exit with the invalid-for-analysis code.
fn refuse_ac(
    (bound, evidence, inputs, json, quiet): AcContext,
    refusal: &Refusal,
    no_signal_path_nets: &[String],
    not_found_nets: &[String],
    text_lead: impl FnOnce(),
) -> ! {
    if json {
        let summary = BindSummary::from_report(&bound.report);
        let mut jr = JsonReport::new(&bound.name, summary)
            .with_inputs(inputs)
            .with_evidence(evidence);
        jr.ac = Some(AcJson {
            validity: Validity::refused(refusal.clone()),
            nets: Vec::new(),
            no_signal_path_nets: no_signal_path_nets.to_vec(),
            not_found_nets: not_found_nets.to_vec(),
            coverage: None,
        });
        println!("{}", jr.to_json());
    } else {
        text_lead();
        eprintln!("{}", refusal.render_text());
        print!(
            "{}",
            super::render_evidence_appendix(evidence, quiet, false)
        );
    }
    crate::reports::ci_artifacts::exit_with_refusal(EXIT_INVALID_FOR_ANALYSIS, refusal)
}

/// Build the `--ac-csv` body from the per-net bode data. Returns the CSV text and
/// the list of nets omitted because they have no signal path.
///
/// A net with no signal path is a full series of `AC_FLOOR_DB` (-6000 dB)
/// sentinel rows. The JSON and text surfaces deliberately never present that
/// floor as real data (they list such nets under `no_signal_path_nets`), so the
/// CSV must not either, otherwise a CI tool ingesting `mag_db` reads -6000 dB as
/// a genuine measurement. Such nets are dropped from the rows and returned so the
/// caller can report the omission explicitly (never silent). An empty bode (a net
/// not found in the circuit) contributes no rows, as before.
fn ac_csv_body(per_net: &[NetBode]) -> (String, Vec<String>) {
    let mut out = String::from("net,freq_hz,mag_db,phase_deg\n");
    let mut no_path: Vec<String> = Vec::new();
    for (net, bode) in per_net {
        if !bode.is_empty() && ac_is_all_sentinel(bode) {
            no_path.push(net.clone());
            continue;
        }
        // A net name can carry a comma; RFC-4180 escape it so the column count
        // stays fixed (mirrors sim.rs::csv_escape).
        let net_cell = crate::commands::sim::csv_escape(net);
        for (f, db, ph) in bode {
            let _ = writeln!(out, "{net_cell},{f},{db},{ph}");
        }
    }
    (out, no_path)
}

#[cfg(test)]
mod ac_csv_tests {
    use super::ac_csv_body;
    use crate::result::AC_FLOOR_DB;

    #[test]
    fn csv_omits_no_signal_path_nets_instead_of_writing_the_floor() {
        // A floor-only net (no drive path) must be omitted from the CSV and
        // reported, not written out as if it were real -6000 dB data: the same
        // "never present the floor as data" contract the JSON/text paths keep.
        let live: Vec<(f64, f64, f64)> = vec![(1.0, -3.0, -45.0), (10.0, -20.0, -90.0)];
        let dead: Vec<(f64, f64, f64)> = vec![(1.0, AC_FLOOR_DB, 0.0), (10.0, AC_FLOOR_DB, 0.0)];
        let per_net = vec![("OUT".to_string(), live), ("DEAD".to_string(), dead)];

        let (csv, no_path) = ac_csv_body(&per_net);

        assert_eq!(
            no_path,
            vec!["DEAD".to_string()],
            "the dead net must be reported as omitted"
        );
        assert!(
            csv.contains("OUT,1,-3,-45"),
            "the live net's real rows must be present: {csv}"
        );
        assert!(
            !csv.contains("DEAD"),
            "the no-signal-path net must not appear in the CSV at all: {csv}"
        );
        assert!(
            !csv.contains("-6000"),
            "the -6000 dB floor must never be written as CSV data: {csv}"
        );
    }
}
