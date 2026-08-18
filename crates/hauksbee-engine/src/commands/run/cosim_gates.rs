//! Applies the post-report exit policy for a completed headless co-simulation.
//! It preserves warning order and precedence among zero-activity refusals,
//! timing invalidity, analogue-abort invalidity, electrical faults, boot
//! hazards, and undermined run-level evidence after all report bytes are emitted.

use super::{
    cosim::{EvidenceArtifacts, RunObservations},
    simulation::SimulationContext,
    timing_refusal, RunConfig,
};
use crate::result::{strict_analog_exit_code, EXIT_INVALID_FOR_ANALYSIS};

pub(super) fn enforce_gates(
    cfg: &RunConfig,
    sim: &SimulationContext,
    observations: &RunObservations,
    evidence: &EvidenceArtifacts,
) {
    let engine = &sim.engine;
    let faults = &observations.faults;
    let failed_chunk_count = observations.analog.failed_chunk_count;
    let analog_abort = observations.analog.abort;
    let timing_refusals = &observations.analog.timing_refusals;
    let zero_activity = observations.activity.zero;
    let strict_refusal = &observations.activity.strict_refusal;
    let held_high_boot_nets = &observations.activity.boot_advisory.held_high_control_nets;
    let has_boot_advisory = !held_high_boot_nets.is_empty();
    let unresolved_active_count = observations.unresolved_active_count;
    let run_evidence = &evidence.run_evidence;
    let run_findings = &evidence.run_findings;
    let faults_gate = evidence.faults_gate;

    // 0-activity refusal (Track B): warn always; under --strict this is a hard
    // refusal (exit 3), not a clean pass. The UART-AND-toggles guard avoids
    // false positives on firmware that is busy on the bus but quiet on GPIO.
    if zero_activity {
        eprintln!(
            "WARNING: co-sim saw zero net toggles; cannot vouch for firmware \
                 behaviour (the MCU may have stalled at boot, run no I/O, or the \
                 firmware may not match this board)."
        );
        if cfg.strict && !analog_abort {
            if let Some(refusal) = &strict_refusal {
                crate::reports::ci_artifacts::set_current_refusal(refusal.clone());
            }
            if !cfg.json {
                if let Some(refusal) = &strict_refusal {
                    eprintln!("{}", refusal.render_text());
                }
            }
            // `fail` outranks `invalid`, the same precedence the verdict
            // field applies: a run that raised real electrical faults CAN
            // be judged, so the fault gate below takes the exit (2) and
            // this refusal stays on stderr and in the artifacts. Exiting 3
            // here put "invalid for analysis" beside a document reading
            // `"verdict":"fail"`. A timing refusal, if this run also had
            // one, still exits 3 from between here and that gate: that
            // exception is documented in docs/ci/CI.md.
            if !faults_gate {
                crate::reports::ci_artifacts::exit_with_refusal(
                    EXIT_INVALID_FOR_ANALYSIS,
                    strict_refusal.as_ref().expect("zero-activity refusal"),
                );
            }
        }
    }

    // A waveform that exceeded a concrete replay capability is invalid
    // evidence. Always disclose it; strict mode fails closed with the same
    // INVALID code used for other evidence gaps.
    if !timing_refusals.is_empty() {
        for refusal in timing_refusals {
            eprintln!("WARNING: timing evidence invalid: {refusal}");
        }
        if cfg.strict {
            let refusal = timing_refusal(&timing_refusals);
            crate::reports::ci_artifacts::exit_with_refusal(EXIT_INVALID_FOR_ANALYSIS, &refusal);
        }
    }

    // Refuse-rather-than-fake (05 §3b): once the analog solve was stuck for a
    // whole streak of chunks, a strict run must abort with the invalid code
    // rather than complete a fake-quiet run. Warn always so the reason is never
    // silent; only --strict turns it into a failing exit.
    if analog_abort {
        eprintln!(
                "WARNING: co-sim analog solve failed to converge for {} chunks in a row \
                 ({} failed chunks total); no fallback integration could carry them, so \
                 the run held stale voltages and cannot vouch for the analog side. The usual cause is unresolved active parts leaving \
                 nodes floating: {} active IC(s) are unresolved/open here. Review the exact \
                 gaps with `hauksbee models coverage {}`, then approve local drafts with \
                 `hauksbee models prepare {} --pack-dir <DIR>`. See {}.",
                crate::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT,
                failed_chunk_count,
                unresolved_active_count,
                cfg.board.display(),
                cfg.board.display(),
                hauksbee_ir::docs_url("docs/about/LIMITATIONS.md"),
            );
        if let Some(code) = strict_analog_exit_code(cfg.strict && analog_abort) {
            if let Some(refusal) = &strict_refusal {
                crate::reports::ci_artifacts::set_current_refusal(refusal.clone());
            }
            if !cfg.json {
                if let Some(refusal) = &strict_refusal {
                    eprintln!("{}", refusal.render_text());
                }
            }
            // NOT the zero-activity precedence: there the analog solve was
            // sound, so raised faults were a judgement the run could make.
            // Here the solve held stale voltages over the failed windows,
            // which is where these faults may come from, so the honest exit
            // is still invalid-for-analysis. The document's `fail` verdict
            // grades the faults as observed; this code says they could not
            // be trusted, and refusing outranks that.
            crate::reports::ci_artifacts::exit_with_refusal(
                code,
                strict_refusal.as_ref().expect("analog refusal"),
            );
        }
    }

    // Strict: any fault raised during the run fails the gate, and the last
    // line says so; a bare exit 2 reads as a tool crash, and --plain's
    // "worth a look" verdict used to contradict the failing code.
    if cfg.strict && faults_gate {
        let items: Vec<String> = faults
            .iter()
            .map(|f| format!("cosim-{} {}", f.kind.as_str(), f.component))
            .collect();
        crate::reports::strict_gate_exit(
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            &items,
        );
    }
    // --strict-boot: opt-in escalation of the boot-safety advisory to a
    // failing gate (exit 2). The run was valid and these are real findings
    // about specific nets; default behaviour leaves them advisory-only. Print
    // the reason to stderr so the failure is never silent, including in the
    // default headless mode (neither --json nor --plain), where the advisory
    // text is not otherwise emitted.
    if cfg.strict_boot && has_boot_advisory {
        let mut items = Vec::new();
        for net in held_high_boot_nets {
            eprintln!(
                "BOOT HAZARD (--strict-boot): control net '{net}' switches a transistor/relay \
                     and is driven HIGH and held from power-up with no bias resistor; the load is \
                     energised at reset."
            );
            items.push(format!("strict-boot {net}"));
        }
        crate::reports::strict_gate_exit(
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            &items,
        );
    }
    // Mirror of the co-sim JSON verdict: the bind contract (unbound
    // verdict-critical parts) and undermined run-level simulation maps
    // exit 3; a per-fault map is that finding's badge and does not, the
    // same exemption the verdict field applies through the matching
    // fault-finding messages.
    let strict_blockers = crate::result::unmodelled_critical_refs(
        &crate::result::BindSummary::from_report(engine.report()),
    );
    let strict_invalid = !strict_blockers.is_empty()
        || crate::result::run_level_undermined(run_evidence.maps(), |a| {
            run_findings.iter().any(|f| f.message == a)
        });
    if cfg.strict && strict_invalid {
        crate::reports::exit_invalid_for_analysis(&strict_blockers);
    }
}
