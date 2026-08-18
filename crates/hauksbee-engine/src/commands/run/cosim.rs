//! Executes the headless co-simulation surface after the live engine is built.
//! It validates waveform probes, runs firmware and analogue windows, assembles
//! evidence and findings, emits JSON or text reports, and applies strict exit
//! gates without changing their established precedence.

use super::{
    prepare::{RunArtifacts, RunInputs},
    simulation::SimulationContext,
    RunConfig,
};
use crate::result::{BindSummary, BootGateJson, JsonNote, JsonNoteKind, JsonReport, Refusal};

pub(super) struct AnalogStatus {
    pub(super) valid: bool,
    pub(super) failed_chunk_count: u64,
    pub(super) fallback_chunk_count: u64,
    pub(super) abort: bool,
    pub(super) timing_refusals: Vec<String>,
}

pub(super) struct ActivityStatus {
    pub(super) zero: bool,
    pub(super) strict_refusal: Option<Refusal>,
    pub(super) boot_advisory: crate::checks::boot::BootAdvisory,
}

pub(super) struct RunObservations {
    pub(super) board_name: String,
    pub(super) summary: BindSummary,
    pub(super) unresolved_active_count: usize,
    pub(super) faults: Vec<crate::FaultEvent>,
    pub(super) cosim: Option<crate::result::CosimJson>,
    pub(super) analog: AnalogStatus,
    pub(super) activity: ActivityStatus,
}

pub(super) struct EvidenceArtifacts {
    pub(super) run_evidence: crate::evidence::BoardEvidence,
    pub(super) run_findings: Vec<crate::result::JsonFinding>,
    pub(super) faults_gate: bool,
}

pub(crate) fn run_headless(
    cfg: &RunConfig,
    quiet: bool,
    run_inputs: &RunInputs,
    artifacts: &RunArtifacts,
    sim: &mut SimulationContext,
) -> anyhow::Result<()> {
    let observations = execute_and_collect_observations(cfg, run_inputs, sim)?;
    let evidence = assemble_evidence_and_ci_artifacts(cfg, artifacts, sim, &observations)?;
    emit_report(cfg, run_inputs, sim, &observations, &evidence);
    emit_evidence(cfg, quiet, &evidence);
    super::cosim_gates::enforce_gates(cfg, sim, &observations, &evidence);
    Ok(())
}

fn execute_and_collect_observations(
    cfg: &RunConfig,
    run_inputs: &RunInputs,
    sim: &mut SimulationContext,
) -> anyhow::Result<RunObservations> {
    let board = &run_inputs.board;
    let probe_known_nets = &sim.probe_known_nets;
    let engine = &mut sim.engine;
    // --probe preconditions, checked before the run so a typo fails fast with
    // the same near-match style the rest of the net-facing CLI uses.
    let probes = crate::reports::cosim::dedup_probes(&cfg.probe);
    // `--probe ''` (an empty shell expansion, usually) would silently
    // record nothing: the dedup drops empties, so catch the case where
    // everything the user passed was empty (M8).
    if !cfg.probe.is_empty() && probes.is_empty() {
        anyhow::bail!(
            "--probe was given only empty net name(s); pass a real net \
                 (run --list-nets to see every net name)"
        );
    }
    crate::reports::cosim::validate_probes(&probes, cfg.probe_csv.as_deref(), &probe_known_nets)?;

    let board_name = engine.report().board_name.clone();
    let summary = BindSummary::from_report(engine.report());
    // Captured before `summary` is consumed by the JSON branch; the
    // convergence-abort diagnosis below names this count.
    let unresolved_active_count = crate::result::coverage_open_active_refs(&summary).len();
    let mut uart_seen = false;
    let headless = crate::reports::cosim::run_headless(
        engine,
        cfg.seconds,
        &mut uart_seen,
        cfg.json,
        cfg.strict,
        &probes,
        cfg.probe_csv.as_deref(),
        cfg.chunk_us,
    )?;
    let achieved_factor = headless.realtime_factor();
    let wall_s = headless.wall_s;
    let faults = headless.faults;

    // Co-sim honesty summary (Track B): total net toggles, UART activity, and
    // any chip substitution detected at build time. Built from the SAME run
    // stats the text table reads, so every surface agrees. The achieved
    // rate is stamped from the run's own wall-clock measurement so the
    // machine surface carries the delivered factor, not an assumed one.
    let cosim = crate::reports::cosim::build_cosim_json(&engine, uart_seen)?.map(|mut c| {
        c.wall_s = wall_s;
        c.realtime_factor = achieved_factor;
        c
    });
    let total_toggles = cosim.as_ref().map(|c| c.total_toggles).unwrap_or(0);
    // Analog-fidelity honesty (05 §3b): once any chunk's analog solve failed,
    // the run held stale voltages and cannot vouch for analog-derived findings
    // over the failed windows. `analog_abort` is the stricter condition: the
    // solve was stuck for a whole streak of chunks, so a strict run must
    // refuse (exit 3) rather than complete a fake-quiet run.
    let valid = engine.scheduler().analog_valid();
    let failed_chunk_count = engine.scheduler().failed_chunk_count();
    // Chunks the primary solve failed but a fallback rung carried. These are
    // solved windows, so they are NOT failures, but the number in them came
    // from a more dissipative method and the reader is entitled to know
    // which. Disclosed independently of `analog_valid` for that reason.
    let fallback_chunk_count = engine.scheduler().fallback_chunk_count();
    let abort = engine.scheduler().analog_abort_tripped();
    let timing_refusals = engine.scheduler().timing_refusals().to_vec();
    // A co-sim that drove no GPIO, produced no net toggles, AND emitted no
    // UART did not exercise the firmware. `any_gpio_driven()` is essential:
    // a firmware that drives a control line high and HOLDS it (boot-gate style)
    // has zero net toggles yet clearly ran, so a toggles-only test would cry
    // wolf on it. Determined BEFORE emitting so the refusal reaches every
    // surface, including --json when no MCU was instantiated (cosim is None).
    let zero = total_toggles == 0 && !uart_seen && !engine.scheduler().any_gpio_driven();
    let strict_refusal = if cfg.strict && abort {
        Some(Refusal::new(
                "a trustworthy strict co-sim verdict for the firmware and analog circuit",
                format!(
                    "the analog solver failed for {failed_chunk_count} chunk(s), including the consecutive-failure abort; affected windows held stale voltages"
                ),
                vec!["static board/copper findings and explicitly non-analog observations remain available"],
                "inspect the first analog non-convergence diagnosis below, fix its named net/device, then rerun the same strict command",
            ))
    } else if cfg.strict && zero {
        Some(Refusal::new(
                "firmware behavior during this strict co-simulation",
                "the run observed no GPIO drive, net toggles, or UART output, so it cannot prove the firmware executed meaningfully",
                vec!["board extraction, binding, and static copper findings remain valid"],
                "verify the firmware image/MCU match and boot entry, then rerun with a probe or UART-producing fixture",
            ))
    } else {
        None
    };

    // Boot-safety advisory, derived once (so --json and --plain agree) from
    // the library so the TUI and web get the same advisory from the same call.
    // `held_high_control_nets` are the heads-up hazards (a control net driven/
    // pulled HIGH and held from reset that switches a transistor/relay and has
    // no bias resistor, a MOSFET gate / relay / igniter energised at power-up;
    // the switch requirement is the zero-FP guard). `gate_states` is the
    // informational per-gate panel, populated only when firmware actually ran
    // (with no --firmware or a stalled one, every gate would read "floating").
    let boot_advisory = crate::checks::boot::analyze(
        &board,
        &engine.scheduler().firmware_held_high_nets(),
        &engine.scheduler().firmware_output_configured_nets(),
        &engine.scheduler().firmware_driven_nets(),
        cfg.firmware.is_some() && !zero,
    );

    Ok(RunObservations {
        board_name,
        summary,
        unresolved_active_count,
        faults,
        cosim,
        analog: AnalogStatus {
            valid,
            failed_chunk_count,
            fallback_chunk_count,
            abort,
            timing_refusals,
        },
        activity: ActivityStatus {
            zero,
            strict_refusal,
            boot_advisory,
        },
    })
}

fn assemble_evidence_and_ci_artifacts(
    cfg: &RunConfig,
    artifacts: &RunArtifacts,
    sim: &mut SimulationContext,
    observations: &RunObservations,
) -> anyhow::Result<EvidenceArtifacts> {
    let ci_findings = &artifacts.ci_findings;
    let board_evidence = &sim.board_evidence;
    let engine = &mut sim.engine;
    let held_high_boot_nets = &observations.activity.boot_advisory.held_high_control_nets;
    let budget = engine.scheduler().error_budget()?;
    let fault_findings = crate::result::fault_findings_json(&observations.faults);
    // The co-sim fault gate, asked of the findings the CI artifacts carry
    // rather than of the fault list beside them: `fault_findings_json`
    // emits one finding per fault, so this is the same set the JUnit
    // `<failure>` count and the SARIF error levels are built from, and the
    // exit code cannot grade the run differently from the archived file.
    let faults_gate = fault_findings.iter().any(|f| f.gates());
    let mut run_findings = fault_findings.clone();
    if cfg.strict_boot {
        run_findings.extend(held_high_boot_nets.iter().map(|net| crate::result::JsonFinding {
                check: "cosim".into(),
                kind: "boot_control_net".into(),
                severity: "warning".into(),
                nets: vec![net.clone()],
                location_mm: None,
                layer: None,
                refs: Vec::new(),
                actionable: true,
                message: format!(
                    "--strict-boot: control net '{net}' is driven HIGH and held from power-up with no bias resistor"
                ),
                plain: format!(
                    "--strict-boot: control net '{net}' can energise its load at reset"
                ),
                fix: Some("add a hardware bias that keeps the load off through reset, or confirm and document the polarity".into()),
            }));
    }
    let mut cosim_maps = Vec::new();
    for finding in &fault_findings {
        cosim_maps.push(board_evidence.simulation_map(
            finding.message.clone(),
            &finding.nets,
            &finding.refs,
            Some(budget.clone()),
        )?);
    }
    if let Some(summary) = &observations.cosim {
        cosim_maps.push(board_evidence.simulation_map(
            format!(
                "Firmware behaviour for {} executed on {}",
                summary.mcu_ref, summary.requested_part
            ),
            &[],
            std::slice::from_ref(&summary.mcu_ref),
            Some(budget.clone()),
        )?);
        for activity in &summary.activity_summary {
            cosim_maps.push(board_evidence.simulation_map(
                format!(
                    "{} toggled {} times between {:.6} V and {:.6} V",
                    activity.net, activity.toggles, activity.v_min, activity.v_max
                ),
                std::slice::from_ref(&activity.net),
                &[],
                Some(budget.clone()),
            )?);
        }
    }
    // A forced run declares itself: without this, the only trace of a
    // post-solve override in the published evidence is a missing
    // residual, which reads as "unmeasured" rather than "overridden".
    let board_evidence = board_evidence
        .clone()
        .with_assumptions(engine.scheduler().forced_voltage_assumptions()?)?;
    let run_evidence = board_evidence.clone().with_maps(cosim_maps);

    // Rewrite CI artifacts from the final co-sim evidence object before a
    // strict gate can exit. Invalid evidence is a failure in JUnit/SARIF and
    // a GitHub error annotation; qualified evidence remains visible.
    if let Some(base) = &ci_findings {
        let mut all = base.clone();
        all.extend(run_findings.clone());
        // The same run-level split as the static pass: an undermined map
        // backing one of the findings already in the file is that
        // finding's badge, not a second gate-grade failure. Co-sim
        // simulation maps and binding-completeness maps are run-level and
        // keep gating.
        let rewrite_messages: std::collections::HashSet<&str> = base
            .iter()
            .chain(run_findings.iter())
            .map(|f| f.message.as_str())
            .collect();
        all.extend(crate::reports::ci_artifacts::evidence_findings_with_gate(
            run_evidence.maps(),
            |m| !rewrite_messages.contains(m.assertion()),
        ));
        crate::reports::ci_artifacts::set_current_findings(all.clone());
        crate::reports::ci_artifacts::github_evidence_annotations_with_gate(
            run_evidence.maps(),
            |m| !rewrite_messages.contains(m.assertion()),
        );
    } else {
        let fault_messages: std::collections::HashSet<&str> =
            run_findings.iter().map(|f| f.message.as_str()).collect();
        crate::reports::ci_artifacts::github_evidence_annotations_with_gate(
            run_evidence.maps(),
            |m| !fault_messages.contains(m.assertion()),
        );
    }

    Ok(EvidenceArtifacts {
        run_evidence,
        run_findings,
        faults_gate,
    })
}

fn emit_report(
    cfg: &RunConfig,
    run_inputs: &RunInputs,
    sim: &SimulationContext,
    observations: &RunObservations,
    evidence: &EvidenceArtifacts,
) {
    let inputs = &run_inputs.inputs;
    let engine = &sim.engine;
    let board_name = &observations.board_name;
    let summary = &observations.summary;
    let faults = &observations.faults;
    let cosim = &observations.cosim;
    let analog_valid = observations.analog.valid;
    let failed_chunk_count = observations.analog.failed_chunk_count;
    let fallback_chunk_count = observations.analog.fallback_chunk_count;
    let zero_activity = observations.activity.zero;
    let strict_refusal = &observations.activity.strict_refusal;
    let held_high_boot_nets = &observations.activity.boot_advisory.held_high_control_nets;
    let has_boot_advisory = !held_high_boot_nets.is_empty();
    let gate_rows = &observations.activity.boot_advisory.gate_states;
    let run_evidence = &evidence.run_evidence;
    let run_findings = &evidence.run_findings;
    let faults_gate = evidence.faults_gate;

    if cfg.json {
        // The co-sim machine report is a model-dependent-claim surface
        // like the static one: an unbound verdict-critical part gates its
        // verdict too, or a firmware run could read pass where the static
        // JSON for the same board says invalid.
        let mut jr = JsonReport::new(board_name, summary.clone())
            .with_bind_verdict_gate()
            // The co-sim exit gate fails on ANY raised fault, and the
            // plain-language classifier grades most of them `warning`, so
            // without this the co-sim document read `pass` beside its own
            // exit 2. `--strict-boot` is the same story one flag over: it
            // turns the boot advisory into an exit 2, so under that flag
            // the advisory is gate-grade for this document too.
            .with_surface_gate(faults_gate || (cfg.strict_boot && has_boot_advisory))
            .with_inputs(&inputs)
            .with_evidence(&run_evidence);
        // A substitution is an info-level note that must never be silently
        // absent (it changes how much the co-sim result can be trusted).
        for sub in engine.scheduler().substitutions() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::CosimSubstitution,
                message: sub.message(),
            });
        }
        if zero_activity {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: "co-sim saw zero net toggles and no UART output; the \
                              firmware was not exercised; this result cannot vouch \
                              for firmware behaviour"
                    .to_string(),
            });
        }
        // Independent of analog_valid: a run can be fully valid AND contain
        // windows a fallback rung produced. Silence here would let a
        // first-order, dissipative window read as a first-class one.
        if fallback_chunk_count > 0 {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: format!(
                    "co-sim analog solve fell back on {fallback_chunk_count} chunk(s); \
                         those windows are converged but were produced by a fallback \
                         integration path (see fallback_windows in the co-sim JSON \
                         for the method, its known numerical trade-off, and the \
                         measured error_estimate_v per window)"
                ),
            });
        }
        // A non-convergent chunk held stale voltages: a loud coverage note so
        // a CI consumer that filters notes (not just the CosimJson body) sees
        // the analog side is not trustworthy over the failed windows (05 §3b).
        if !analog_valid {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: format!(
                    "co-sim analog solve failed on {failed_chunk_count} chunk(s) \
                         that no fallback integration could carry; those windows held \
                         stale node voltages and are reported as analog_valid:false; \
                         analog-derived findings over them are not trustworthy"
                ),
            });
            // One note PER failed window naming the interval and the
            // solver's diagnosis, so a JSON consumer gets the offending net
            // and element without re-running the board (E29).
            for d in engine.scheduler().failed_window_diagnoses() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: format!("analog non-convergence at {d}"),
                });
            }
        }
        // A drive that lost to a co-located source, named on both sides
        // (E30). Never silent: a run that reports 3.300 V on a net the user
        // asked to force to 20 V has to say why.
        for msg in engine.scheduler().drive_conflicts() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: msg,
            });
        }
        // Co-sim coverage honesty (U3): dropped ADC injections and
        // never-exercised bus peripherals are silent-garbage modes; they
        // ride the same Coverage note channel analog_valid uses, in
        // addition to the structured CosimJson fields, so a consumer that
        // only filters notes still sees them.
        for d in engine.scheduler().adc_dropped() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: d.message(),
            });
        }
        for b in engine.scheduler().unexercised_buses() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: b.message(),
            });
        }
        // Watchdog coverage, same channel and the same reason: a backend
        // whose armed watchdog never fires lets hung firmware run forever,
        // so a consumer that only filters notes must still learn that this
        // run cannot vouch for the recovery path.
        for (mcu_ref, limitation) in engine.scheduler().watchdog_limitations() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: crate::scheduler::watchdog_limitation_message(&mcu_ref, &limitation),
            });
        }
        for (mcu_ref, resets) in engine.scheduler().watchdog_resets() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: crate::scheduler::watchdog_reset_message(&mcu_ref, resets),
            });
        }
        // Timing coverage: a known systematic time bias on a core makes a
        // time-based assertion there mean less than it looks, and a
        // consumer that only filters notes must still learn it.
        for (mcu_ref, limitation) in engine.scheduler().timing_limitations() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: crate::scheduler::timing_limitation_message(&mcu_ref, &limitation),
            });
        }
        for w in crate::reports::cosim::heuristic_framing_warnings(
            &engine.scheduler().spi_framing_modes(),
        ) {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: format!("co-sim: {w}"),
            });
        }
        // Sub-chunk pulses invisible to tick-evaluated sequential parts
        // (friction 1.16) and runtime driver contention: same Coverage
        // note channel, in addition to the structured CosimJson fields,
        // so a consumer that only filters notes still sees them.
        for p in engine.scheduler().short_pulses() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: p.message(),
            });
        }
        for c in engine.scheduler().driver_contentions() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::Coverage,
                message: c.message(),
            });
        }
        for net in held_high_boot_nets {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::BootControlNet,
                message: format!(
                    "control net '{net}' drives a transistor/relay, is driven HIGH and held \
                         from power-up, and has no resistor setting a safe default. If a HIGH on \
                         it turns the switched load ON when it must stay OFF until firmware \
                         enables it, it is energised at power-up; confirm the polarity and that \
                         this is intended."
                ),
            });
        }
        if !gate_rows.is_empty() {
            jr.boot_gates = Some(
                gate_rows
                    .iter()
                    .map(|(reference, net, state)| BootGateJson {
                        reference: reference.clone(),
                        net: net.clone(),
                        state: state.json().to_string(),
                    })
                    .collect(),
            );
        }
        // The electrical-stress faults must reach the machine surface too: the
        // --plain path renders them and --strict gates on them, but --json used
        // to omit them entirely, so a CI consumer parsing the JSON saw a clean
        // run over a board the co-sim flagged (a destroyed MOSFET, overcurrent…).
        if !run_findings.is_empty() {
            jr.findings = Some(run_findings.clone());
        }
        jr.cosim = cosim.clone();
        jr.refusal = strict_refusal.clone();
        println!("{}", jr.to_json());
    } else if cfg.plain {
        // A co-sim with no stress faults is NOT plainly "healthy" if it ran on
        // a substitute chip or never exercised the firmware. Surface those as
        // heads-up notes so the verdict reads "no failures, but N worth a look"
        // (via PlainReport::verdict) instead of a bare "Looks healthy".
        let mut report = crate::plain_faults(&faults);
        for sub in engine.scheduler().substitutions() {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "co-sim ran on a SUBSTITUTE chip: {}",
                sub.message()
            )));
        }
        if zero_activity {
            report.heads_up.push(crate::plain::HeadsUp::note(
                "co-sim saw zero net toggles and no UART output; the firmware was not \
                     exercised, so this result cannot vouch for firmware behaviour",
            ));
        }
        if fallback_chunk_count > 0 {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "co-sim analog solve fell back on {fallback_chunk_count} chunk(s): \
                     those windows are converged, but a more robust and less accurate \
                     method produced them, so fast transients and ringing inside them are \
                     damped. Rerun with --json for the method and window of each"
            )));
        }
        if !analog_valid {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "co-sim analog solve failed on {failed_chunk_count} chunk(s) that no \
                     fallback integration could carry; those windows held stale voltages \
                     and cannot be trusted (analog_valid is false)"
            )));
            // The interval AND the diagnosis, inline. "Rerun with --json to
            // see the windows" was the whole defect: the one surface a
            // person actually reads named nothing (E29).
            for d in engine.scheduler().failed_window_diagnoses() {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "analog non-convergence at {d}"
                )));
            }
        }
        for msg in engine.scheduler().drive_conflicts() {
            report.heads_up.push(crate::plain::HeadsUp::note(msg));
        }
        // Co-sim coverage honesty (U3): the same dropped-ADC / unexercised-bus
        // / heuristic-framing warnings the JSON notes carry, as plain
        // heads-ups so the verdict reads "no failures, but N worth a look".
        for d in engine.scheduler().adc_dropped() {
            report
                .heads_up
                .push(crate::plain::HeadsUp::note(d.message()));
        }
        for b in engine.scheduler().unexercised_buses() {
            report
                .heads_up
                .push(crate::plain::HeadsUp::note(b.message()));
        }
        // Watchdog coverage, worded identically to the JSON notes and the
        // default text summary.
        for (mcu_ref, limitation) in engine.scheduler().watchdog_limitations() {
            report.heads_up.push(crate::plain::HeadsUp::note(
                crate::scheduler::watchdog_limitation_message(&mcu_ref, &limitation),
            ));
        }
        for (mcu_ref, resets) in engine.scheduler().watchdog_resets() {
            report.heads_up.push(crate::plain::HeadsUp::note(
                crate::scheduler::watchdog_reset_message(&mcu_ref, resets),
            ));
        }
        // Timing coverage, worded identically to the JSON notes and the
        // default text summary.
        for (mcu_ref, limitation) in engine.scheduler().timing_limitations() {
            report.heads_up.push(crate::plain::HeadsUp::note(
                crate::scheduler::timing_limitation_message(&mcu_ref, &limitation),
            ));
        }
        for w in crate::reports::cosim::heuristic_framing_warnings(
            &engine.scheduler().spi_framing_modes(),
        ) {
            report.heads_up.push(crate::plain::HeadsUp::note(w));
        }
        // Sub-chunk pulse and driver-contention findings, same wording as
        // the JSON notes and the default text summary.
        for p in engine.scheduler().short_pulses() {
            report
                .heads_up
                .push(crate::plain::HeadsUp::note(p.message()));
        }
        for c in engine.scheduler().driver_contentions() {
            report
                .heads_up
                .push(crate::plain::HeadsUp::note(c.message()));
        }
        // Boot-safety heads-up: control nets the firmware switches ON and
        // holds from power-up, with no resistor setting a safe default. The
        // netlist alone cannot tell whether a power-up HIGH is intended;
        // running the firmware can. This is what surfaces, e.g., a MOSFET /
        // relay / igniter that energises at reset because firmware drove its
        // gate high (or enabled a pull-up on it) before anything else ran.
        for net in held_high_boot_nets {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "control net '{net}' switches a transistor/relay and is driven HIGH and held \
                     from the moment the board powers up, with no resistor setting a safe default \
                     level. If a HIGH on this net turns the load ON when it must stay OFF until \
                     the firmware deliberately enables it (a MOSFET, relay, motor driver, or \
                     igniter), it is energised at power-up; confirm the polarity and that this \
                     is intended."
            )));
        }
        // Bind-coverage caveat: a co-sim over a board with unmodeled/open
        // active ICs cannot vouch for the firmware/analog behaviour on their
        // nets. --report and the web/JSON surfaces already say this; the
        // headless text/plain path must too, or a clean-looking co-sim silently
        // hides that half the board was never modelled.
        let open = crate::result::coverage_open_active_refs(&summary);
        if !open.is_empty() {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "co-sim coverage: {} of {} critical parts modelled: {} active IC(s) are \
                     unresolved or open, so firmware/analog/thermal results on their nets are \
                     INCOMPLETE (the copper/DRC checks are unaffected). Review every model gap \
                     and approve a local draft pack with:  hauksbee models prepare {} \
                     --pack-dir <DIR>",
                summary.critical_parts_bound_n,
                summary.critical_parts_total,
                open.len(),
                cfg.board.display(),
            )));
        }
        println!();
        print!("{}", report.render());
        if !gate_rows.is_empty() {
            print!("{}", crate::checks::boot::render_boot_gate_panel(gate_rows));
        }
    } else {
        // Default text headless mode: the co-sim activity table is printed
        // elsewhere, but the boot power-up hazard and the gate panel must
        // surface here too, otherwise the plainest persona is the ONLY one
        // that hides a switched load energised at reset (the --json/--plain/
        // web surfaces all carry it). Advisory-only (no exit-code change);
        // --strict-boot still escalates below.
        for net in held_high_boot_nets {
            println!(
                "BOOT HAZARD: control net '{net}' switches a transistor/relay and is driven \
                     HIGH and held from the moment the board powers up, with no resistor setting \
                     a safe default level; if a HIGH turns the load ON when it must stay OFF \
                     until firmware enables it (a MOSFET, relay, motor driver, or igniter), it is \
                     energised at power-up. Confirm the polarity and that this is intended."
            );
        }
        if !gate_rows.is_empty() {
            print!("{}", crate::checks::boot::render_boot_gate_panel(gate_rows));
        }
        let open = crate::result::coverage_open_active_refs(&summary);
        if !open.is_empty() {
            println!(
                "co-sim coverage: {} of {} critical parts modelled: {} active IC(s) \
                     unresolved/open, so firmware/analog results on their nets are INCOMPLETE \
                     (copper/DRC checks are unaffected). Review every model gap and approve a \
                     local draft pack with:  hauksbee models prepare {} --pack-dir <DIR>",
                summary.critical_parts_bound_n,
                summary.critical_parts_total,
                open.len(),
                cfg.board.display(),
            );
        }
    }
}

fn emit_evidence(cfg: &RunConfig, quiet: bool, evidence: &EvidenceArtifacts) {
    if !cfg.json {
        print!(
            "{}",
            crate::reports::render_evidence_appendix(&evidence.run_evidence, quiet, false)
        );
    }
}
