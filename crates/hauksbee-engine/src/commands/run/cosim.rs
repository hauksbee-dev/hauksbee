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
use hauksbee_cosim::scheduler::{AdcDrop, DriverContention, ShortPulse, UnexercisedBus};
use hauksbee_ir::units::format_volts;
use hauksbee_ir::{eprintln, print, println};

pub(super) struct AnalogStatus {
    pub(super) valid: bool,
    pub(super) failed_chunk_count: u64,
    pub(super) fallback_chunk_count: u64,
    pub(super) abort: bool,
    pub(super) timing_refusals: Vec<String>,
}

pub(super) struct ActivityStatus {
    pub(super) zero: bool,
    /// No processor is bound and no firmware was supplied: there is no
    /// firmware to have executed, so firmware-execution wording is wrong.
    pub(super) no_processor: bool,
    /// Executing MCUs that still cannot count as a firmware pass, one line each.
    pub(super) pass_blockers: Vec<String>,
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

pub fn run_headless(
    cfg: &RunConfig,
    quiet: bool,
    run_inputs: &RunInputs,
    artifacts: &RunArtifacts,
    sim: &mut SimulationContext,
    static_outcome: Option<crate::reports::check::CheckOutcome>,
) -> anyhow::Result<()> {
    let observations = execute_and_collect_observations(cfg, run_inputs, sim)?;
    let evidence = assemble_evidence_and_ci_artifacts(cfg, artifacts, sim, &observations)?;
    let combined = emit_report(
        cfg,
        run_inputs,
        sim,
        &observations,
        &evidence,
        static_outcome.as_ref(),
    );
    emit_evidence(cfg, quiet, &evidence);
    // A combined run owes the static check's strict exits too. They are taken
    // first (the static pass came first), and the co-sim's gates follow, so a
    // gating board finding is never lost behind a quiet simulation.
    if let (Some(outcome), Some(verdict)) = (static_outcome, combined) {
        outcome.finish(verdict.partial || verdict.verdict == "invalid");
    }
    super::cosim_gates::enforce_gates(cfg, sim, &observations, &evidence);
    Ok(())
}

/// The strict refusal for a quiet run on a board with no processor and no
/// firmware: it names the missing processor, not a firmware that never ran.
fn no_processor_refusal() -> Refusal {
    Refusal::new(
        "a strict co-simulation verdict that depends on a processor",
        format!(
            "{}, and nothing else on the board was driven",
            crate::execution::NO_PROCESSOR_MESSAGE
        ),
        vec!["board extraction, binding, and static copper findings remain valid"],
        "bind a processor and supply firmware if the board needs one, or run without --strict",
    )
}

// One flat gather of independent run observations, each a few lines.
#[allow(clippy::too_many_lines)]
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
    // everything the user passed was empty.
    if !cfg.probe.is_empty() && probes.is_empty() {
        anyhow::bail!(
            "--probe was given only empty net name(s); pass a real net \
                 (run --list-nets to see every net name)"
        );
    }
    crate::reports::cosim::validate_probes(&probes, cfg.probe_csv.as_deref(), probe_known_nets)?;

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
        crate::reports::cosim::ObserveOutputs {
            vcd: cfg.vcd.as_deref(),
            bus_log: cfg.bus_log,
        },
    )?;
    let achieved_factor = headless.realtime_factor();
    let wall_s = headless.wall_s;
    let faults = headless.faults;
    let uart_output = headless.uart_output;

    // Co-sim honesty summary: total net toggles, UART activity, and
    // any chip substitution detected at build time. Built from the SAME run
    // stats the text table reads, so every surface agrees. The achieved
    // rate is stamped from the run's own wall-clock measurement so the
    // machine surface carries the delivered factor, not an assumed one.
    let cosim = crate::reports::cosim::build_cosim_json(engine, uart_seen)?.map(|mut c| {
        c.wall_s = wall_s;
        c.realtime_factor = achieved_factor;
        c.set_uart_output(&uart_output);
        c
    });
    let total_toggles = cosim.as_ref().map_or(0, |c| c.total_toggles);
    // Analog-fidelity honesty: once any chunk's analog solve failed,
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
    // Execution evidence (entry reached, firmware pin edges, UART) decides
    // when the run gathered it; see `Scheduler::firmware_quiet`.
    let zero = engine.scheduler().firmware_quiet(total_toggles, uart_seen);
    let no_processor = cfg.firmware.is_none() && !engine.scheduler().has_processor();
    let pass_blockers = engine.scheduler().firmware_pass_blockers();
    let strict_refusal = if cfg.strict && abort {
        Some(Refusal::new(
                "a trustworthy strict co-sim verdict for the firmware and analog circuit",
                format!(
                    "the analog solver failed for {failed_chunk_count} chunk(s), including the consecutive-failure abort; affected windows held stale voltages"
                ),
                vec!["static board/copper findings and explicitly non-analog observations remain available"],
                "inspect the first analog non-convergence diagnosis below, fix its named net/device, then rerun the same strict command",
            ))
    } else if cfg.strict && zero && no_processor {
        Some(no_processor_refusal())
    } else if cfg.strict && zero {
        Some(Refusal::new(
                "firmware behavior during this strict co-simulation",
                "the run observed no entry symbol reached, firmware GPIO drive, or UART output, so it cannot prove the firmware executed meaningfully",
                vec!["board extraction, binding, and static copper findings remain valid"],
                "verify the firmware image/MCU match and boot entry, then rerun with a probe or UART-producing fixture",
            ))
    } else if cfg.strict && !pass_blockers.is_empty() {
        Some(Refusal::new(
                "firmware behavior during this strict co-simulation",
                format!(
                    "the firmware executed but cannot count as a pass: {}",
                    pass_blockers.join(" | ")
                ),
                vec!["board extraction, binding, and static copper findings remain valid"],
                "fix the fault, reset or supply condition named in the execution evidence, then rerun the same strict command",
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
        board,
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
            no_processor,
            pass_blockers,
            strict_refusal,
            boot_advisory,
        },
    })
}

/// A run the tool cannot qualify numerically is not judged: exit 3 with the
/// refusal, as the web report refuses it, never an error or a failure.
fn refuse_unqualified(
    cfg: &RunConfig,
    observations: &RunObservations,
    error: &hauksbee_ir::evidence::EvidenceError,
) -> ! {
    let refusal = Refusal::new(
        "numerically qualified firmware co-simulation conclusions",
        error.to_string(),
        vec!["Firmware loading and the printed digital observations remain available."],
        "use finite, positive solver tolerances and run the co-simulation again",
    );
    if cfg.json {
        let mut jr = JsonReport::new(&observations.board_name, observations.summary.clone());
        jr.refusal = Some(refusal.clone());
        println!("{}", jr.to_json());
    } else {
        eprintln!("{}", refusal.render_text());
    }
    crate::reports::ci_artifacts::exit_with_refusal(
        crate::result::EXIT_INVALID_FOR_ANALYSIS,
        &refusal,
    )
}

fn assemble_evidence_and_ci_artifacts(
    cfg: &RunConfig,
    artifacts: &RunArtifacts,
    sim: &mut SimulationContext,
    observations: &RunObservations,
) -> anyhow::Result<EvidenceArtifacts> {
    let board_evidence = &sim.board_evidence;
    let engine = &mut sim.engine;
    let held_high_boot_nets = &observations.activity.boot_advisory.held_high_control_nets;
    let budget = engine
        .scheduler()
        .error_budget()
        .unwrap_or_else(|error| refuse_unqualified(cfg, observations, &error));
    let fault_findings = crate::result::fault_findings_json(&observations.faults);
    // The co-sim fault gate, asked of the findings the CI artifacts carry
    // rather than of the fault list beside them: `fault_findings_json`
    // emits one finding per fault, so this is the same set the JUnit
    // `<failure>` count and the SARIF error levels are built from, and the
    // exit code cannot grade the run differently from the archived file.
    let faults_gate = fault_findings.iter().any(crate::result::JsonFinding::gates);
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
    // A forced run declares itself, as do a regulator whose input headroom was
    // never established and a rating read at a defaulted ambient; else a
    // post-solve override leaves only a missing residual. BEFORE any map is
    // built: a map resolves against the assumptions its board had at birth.
    let board_evidence = &board_evidence
        .clone()
        .with_assumptions(engine.scheduler().forced_voltage_assumptions()?)?
        .with_assumptions(engine.scheduler().stress.rating_disclosures())?;
    let mut cosim_maps = Vec::new();
    // One map per unsupported regulator, over everything its setpoint reaches:
    // the Undermined assumption beneath it makes the verdict invalid and
    // `--strict` exit 3 through the existing gates, with no new gate code.
    for taint in engine.scheduler().regulation_taint() {
        cosim_maps.extend(board_evidence.simulation_map_if_mapped(
            taint.reason.clone(),
            &taint.nets,
            &taint.refs,
            Some(budget.clone()),
        )?);
    }
    // `run_findings` opens with the fault findings, one per fault in order;
    // the boot-advisory findings follow them.
    cosim_maps.extend(
        board_evidence.fault_finding_maps(&mut run_findings[..fault_findings.len()], &budget)?,
    );
    if let Some(summary) = &observations.cosim {
        cosim_maps.extend(board_evidence.simulation_map_if_mapped(
            format!(
                "Firmware behaviour for {} executed on {}",
                summary.mcu_ref, summary.requested_part
            ),
            &[],
            std::slice::from_ref(&summary.mcu_ref),
            Some(budget.clone()),
        )?);
        for activity in &summary.activity_summary {
            // A net the board's evidence does not hold (blank, merged) has no
            // causal path to map: its activity stays in the summary only.
            let (lo, hi) = (format_volts(activity.v_min), format_volts(activity.v_max));
            cosim_maps.extend(board_evidence.simulation_map_if_mapped(
                format!(
                    "{} toggled {} times between {lo} and {hi}",
                    activity.net, activity.toggles
                ),
                std::slice::from_ref(&activity.net),
                &[],
                Some(budget.clone()),
            )?);
        }
    }
    let run_evidence = board_evidence.clone().with_maps(cosim_maps);

    // Rewrite CI artifacts from the final co-sim evidence object before a
    // strict gate can exit. Invalid evidence is a failure in JUnit/SARIF and
    // a GitHub error annotation; qualified evidence remains visible.
    if let Some(base) = &artifacts.ci_findings {
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

/// The co-sim coverage messages EVERY surface carries, in one order and one
/// wording so the `--json` notes and the `--plain` heads-ups cannot drift
/// apart: a drive that lost to a co-located source, dropped ADC injections,
/// never-exercised bus peripherals, watchdog and timing limitations, watchdog
/// reboots, regulators with no proven input headroom, heuristic SPI framing,
/// sub-chunk pulses invisible to tick-evaluated
/// sequential parts, and runtime driver contention. Each is a silent-garbage
/// mode: they ride this channel in ADDITION to the structured `CosimJson`
/// fields, so a consumer that only filters notes still sees them.
///
/// `framing_prefix` is the only difference between the surfaces: every other
/// message already opens with `co-sim:`, and the shared framing warning
/// (rendered identically in the coverage report) does not.
fn shared_coverage_messages(
    scheduler: &crate::scheduler::Scheduler,
    framing_prefix: &str,
) -> Vec<String> {
    let mut out = scheduler.drive_conflicts();
    out.extend(scheduler.adc_dropped().iter().map(AdcDrop::message));
    out.extend(
        scheduler
            .unexercised_buses()
            .iter()
            .map(UnexercisedBus::message),
    );
    out.extend(crate::scheduler::coverage_limitation_messages(scheduler));
    out.extend(
        crate::reports::cosim::heuristic_framing_warnings(&scheduler.spi_framing_modes())
            .into_iter()
            .map(|w| format!("{framing_prefix}{w}")),
    );
    out.extend(scheduler.short_pulses().iter().map(ShortPulse::message));
    out.extend(
        scheduler
            .driver_contentions()
            .iter()
            .map(DriverContention::message),
    );
    out
}

#[allow(clippy::too_many_lines, reason = "three output modes, shared bindings")]
fn emit_report(
    cfg: &RunConfig,
    run_inputs: &RunInputs,
    sim: &SimulationContext,
    observations: &RunObservations,
    evidence: &EvidenceArtifacts,
    static_outcome: Option<&crate::reports::check::CheckOutcome>,
) -> Option<crate::result::RunVerdict> {
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
    // A core parked at start-up (before main, or waiting on a clock) is
    // reported with where, beside the generic no-execution sentence.
    let stall_notes = crate::execution::stall_notes(engine.scheduler().execution_evidence());
    let strict_refusal = &observations.activity.strict_refusal;
    let held_high_boot_nets = &observations.activity.boot_advisory.held_high_control_nets;
    let has_boot_advisory = !held_high_boot_nets.is_empty();
    let gate_rows = &observations.activity.boot_advisory.gate_states;
    let run_evidence = &evidence.run_evidence;
    let run_findings = &evidence.run_findings;
    let faults_gate = evidence.faults_gate;
    let machine;

    // The machine report is built on every surface: its verdict is the one
    // the manifest records and the plain closing line states, so no surface
    // can read pass over a run the JSON calls partly judged.
    {
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
            .with_inputs(inputs)
            .with_evidence(run_evidence);
        // A substitution is an info-level note that must never be silently
        // absent (it changes how much the co-sim result can be trusted).
        for sub in engine.scheduler().substitutions() {
            jr.notes.push(JsonNote {
                kind: JsonNoteKind::CosimSubstitution,
                message: sub.message(),
            });
        }
        // A guessed AVR clock puts every delay, baud rate and PWM period in the
        // run at risk of being off by the ratio: never silent.
        for guess in engine.scheduler().clock_assumptions() {
            jr.push_coverage_note(format!("assumed AVR clock: {guess}"));
        }
        if zero_activity && observations.activity.no_processor {
            jr.push_coverage_note(crate::execution::NO_PROCESSOR_MESSAGE);
        } else if zero_activity {
            jr.push_coverage_note(crate::execution::NO_EXECUTION_MESSAGE);
        }
        for stall in &stall_notes {
            jr.push_coverage_note(format!("co-sim: {stall}"));
        }
        for blocker in &observations.activity.pass_blockers {
            jr.push_coverage_note(format!("firmware executed but is not a pass: {blocker}"));
        }
        // Independent of analog_valid: a run can be fully valid AND contain
        // windows a fallback rung produced. Silence here would let a
        // first-order, dissipative window read as a first-class one.
        if fallback_chunk_count > 0 {
            jr.push_coverage_note(format!(
                "co-sim analog solve fell back on {fallback_chunk_count} chunk(s); \
                         those windows are converged but were produced by a fallback \
                         integration path (see fallback_windows in the co-sim JSON \
                         for the method, its known numerical trade-off, and the \
                         measured error_estimate_v per window)"
            ));
        }
        // A non-convergent chunk held stale voltages: a loud coverage note so
        // a CI consumer that filters notes (not just the CosimJson body) sees
        // the analog side is not trustworthy over the failed windows.
        if !analog_valid {
            jr.push_coverage_note(format!(
                "co-sim analog solve failed on {failed_chunk_count} chunk(s) \
                         that no fallback integration could carry; those windows held \
                         stale node voltages and are reported as analog_valid:false; \
                         analog-derived findings over them are not trustworthy"
            ));
            // One note PER failed window naming the interval and the
            // solver's diagnosis, so a JSON consumer gets the offending net
            // and element without re-running the board.
            for d in engine.scheduler().failed_window_diagnoses() {
                jr.push_coverage_note(format!("analog non-convergence at {d}"));
            }
        }
        for message in shared_coverage_messages(engine.scheduler(), "co-sim: ") {
            jr.push_coverage_note(message);
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
        jr.cosim.clone_from(cosim);
        jr.refusal.clone_from(strict_refusal);
        machine = jr;
    }
    let verdict = machine.run_verdict();
    // A run that also printed the static check ends on the worse of the two
    // verdicts, which is what the manifest records.
    let combined = static_outcome.map(|stat| stat.verdict.combine(&verdict));
    crate::run_manifest::note_run_verdict(combined.as_ref().unwrap_or(&verdict).verdict);
    if cfg.json {
        println!("{}", machine.to_json());
    } else if cfg.plain {
        // A co-sim with no stress faults is NOT plainly "healthy" if it ran on
        // a substitute chip or never exercised the firmware. Surface those as
        // heads-up notes so the verdict reads "no failures, but N worth a look"
        // (via PlainReport::verdict) instead of a bare "Looks healthy".
        let mut report = crate::plain_faults(faults);
        for sub in engine.scheduler().substitutions() {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "co-sim ran on a SUBSTITUTE chip: {}",
                sub.message()
            )));
        }
        for guess in engine.scheduler().clock_assumptions() {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "co-sim assumed an AVR clock: {guess}"
            )));
        }
        // A board with no processor and no firmware has nothing to say about
        // firmware execution; the neutral line rides the JSON coverage notes.
        if zero_activity && !observations.activity.no_processor {
            report.heads_up.push(crate::plain::HeadsUp::note(
                crate::execution::NO_EXECUTION_MESSAGE,
            ));
        }
        for stall in &stall_notes {
            report
                .heads_up
                .push(crate::plain::HeadsUp::note(format!("co-sim: {stall}")));
        }
        for blocker in &observations.activity.pass_blockers {
            report.heads_up.push(crate::plain::HeadsUp::note(format!(
                "firmware executed but is not a pass: {blocker}"
            )));
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
            // person actually reads named nothing.
            for d in engine.scheduler().failed_window_diagnoses() {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "analog non-convergence at {d}"
                )));
            }
        }
        for message in shared_coverage_messages(engine.scheduler(), "") {
            report.heads_up.push(crate::plain::HeadsUp::note(message));
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
        let open = crate::result::coverage_open_active_refs(summary);
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
        if let Some((combined, stat)) = combined.as_ref().zip(static_outcome) {
            println!("\n{}", combined_closing_line(cfg, combined, stat, &report));
        } else if let Some(line) = closing_verdict_line(&verdict, &report) {
            println!("{line}");
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
        if let Some(line) = closing_verdict_line(&verdict, &crate::plain_faults(faults)) {
            println!("{line}");
        }
        let open = crate::result::coverage_open_active_refs(summary);
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
    combined
}

/// The `(serious, worth a look)` counts the closing line of a co-sim reads,
/// from the plain report it ends.
fn cosim_counts(report: &crate::plain::PlainReport) -> (usize, usize) {
    let serious = report.serious_count();
    (
        serious,
        (report.findings.len() - serious) + report.heads_up.len(),
    )
}

/// The one closing line of a run that printed the static check and then
/// co-simulated: the worse of the two verdicts, over both passes' counts, in
/// the words `--check` alone closes on. Always printed, a pass included: the
/// static pass's own closing line was held back for this one.
fn combined_closing_line(
    cfg: &RunConfig,
    combined: &crate::result::RunVerdict,
    stat: &crate::reports::check::CheckOutcome,
    report: &crate::plain::PlainReport,
) -> String {
    let (serious, worth) = cosim_counts(report);
    let counts = (stat.counts.0 + serious, stat.counts.1 + worth);
    let report_only = if !cfg.strict && combined.verdict == "fail" {
        crate::reports::check::report_only_suffix()
    } else {
        String::new()
    };
    format!(
        "{}{report_only}",
        crate::reports::check::verdict_line(combined, counts, stat.copper_unvalidated())
    )
}

/// The closing `VERDICT:` line a text surface prints when the run is not a
/// pass: the machine verdict's tag and reason (the line `--check` prints), so
/// a failed solve with no fault reads PARTLY JUDGED, never healthy. A pass
/// prints none: the surface's own summary already says it.
fn closing_verdict_line(
    verdict: &crate::result::RunVerdict,
    report: &crate::plain::PlainReport,
) -> Option<String> {
    (verdict.verdict != "pass")
        .then(|| crate::reports::check::verdict_line(verdict, cosim_counts(report), None))
}

fn emit_evidence(cfg: &RunConfig, quiet: bool, evidence: &EvidenceArtifacts) {
    if !cfg.json {
        print!(
            "{}",
            crate::reports::render_evidence_appendix(&evidence.run_evidence, quiet, false)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::closing_verdict_line;
    use crate::plain::PlainReport;
    use crate::result::RunVerdict;

    /// A run whose solve failed with no fault closes on the machine
    /// verdict's PARTLY JUDGED line and its reason; a converged pass prints
    /// no closing line, its summary already says it.
    #[test]
    fn a_failed_solve_closes_partly_judged_and_a_pass_prints_no_verdict_line() {
        let partial = RunVerdict {
            verdict: "invalid",
            reason: Some(
                "PARTLY JUDGED (not a pass): an analysis did not run to a result: the co-sim's analog \
                 solve did not converge over 1 window(s)"
                    .into(),
            ),
            conditions: Vec::new(),
            partial: true,
            not_judged: None,
            inspect: 0,
            unorderable: None,
        };
        let line = closing_verdict_line(&partial, &PlainReport::default()).unwrap();
        assert!(
            line.starts_with(&format!(
                "VERDICT: {}: 0 serious, 0 worth a look; an analysis",
                hauksbee_ir::vocabulary::PARTLY_JUDGED_LEAD
            )) && line.contains("did not converge")
                && !line.contains("PARTLY JUDGED: "),
            "{line}"
        );
        assert_eq!(
            closing_verdict_line(&RunVerdict::default(), &PlainReport::default()),
            None
        );
    }
}
