//! Selects the interactive terminal path or constructs the live simulation
//! state used by thermal, serial, headless, and browser surfaces. Engine setup
//! binds the prepared board, authenticates firmware evidence, offers required
//! emulator installation, and applies requested copper-short bridges.

use hauksbee_extract::ExtractedBoard;

use crate::binder::{bind_board, BoundBoard};
use crate::engine::HauksbeeEngine;
use crate::evidence::BoardEvidence;
use crate::result::{BindSummary, JsonReport, Refusal};

use super::{prepare::RunInputs, RunConfig, SelectedSurface};

pub(crate) struct SimulationContext {
    pub(crate) engine: HauksbeeEngine,
    pub(crate) board_evidence: BoardEvidence,
    pub(crate) probe_known_nets: Vec<String>,
}

pub(crate) fn launch_tui_if_selected(
    cfg: &RunConfig,
    run_inputs: &RunInputs,
    schematic: Option<&std::path::Path>,
) -> anyhow::Result<bool> {
    let text = &run_inputs.text;
    let is_altium = run_inputs.is_altium;
    // Default flow (no report/headless/ac flag). The interactive terminal UI is
    // the new human-facing default: bare `run <board>` on a TTY launches it. Any
    // explicit report flag was handled above, so reaching here means none was
    // given. `--serve` keeps the historical websocket frontend; a non-TTY stdout
    // (piped / CI) also keeps the websocket behaviour untouched, so existing
    // scripts and tests are unaffected.
    //
    // `--firmware`/`--apply-shorts` only matter for the simulating paths; the TUI
    // honours `--firmware` and `--chunk-us` for its co-sim pane. We branch to the
    // TUI before building the websocket engine so we never spin up tokio for the
    // TUI path.
    let stdout_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    // Altium boards reach here with an empty `text` (binary parsed from bytes);
    // the TUI's text-based build path can't analyse those, so they keep the
    // websocket flow.
    // `--serial-attach` is a live co-sim session driven from another terminal, so
    // it must never be swallowed by the TTY-default dashboard: the whole point is
    // that this terminal narrates the serial endpoint while the user's tool talks
    // to it.
    let launch_tui = !cfg.serve
        && !cfg.headless
        && !cfg.serial_attach
        && !is_altium
        && (cfg.tui || stdout_is_tty);
    if launch_tui {
        // The TUI rebuilds its board from the layout text, so it cannot apply
        // the overlay; refuse rather than show pristine-design numbers under an
        // --asbuilt flag the user believes is in effect.
        if cfg.asbuilt.is_some() {
            return Err(anyhow::anyhow!(
                "the interactive dashboard does not apply --asbuilt; run a report \
                 (--check/--report) or a co-sim (--headless/--serve) instead"
            ));
        }
        // Forcing the TUI without a terminal on the other end fails deep inside
        // the terminal setup with a bare OS error; say what is actually wrong.
        if cfg.tui && !stdout_is_tty && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(anyhow::anyhow!(
                "the interactive dashboard needs a terminal, and neither stdin nor stdout \
                 is one (output is piped or redirected). Drop --tui and use --check/--report \
                 for a report, --json for machine output, or --serve for the browser UI"
            ));
        }
        crate::tui::run_with_schematic_and_chunk(
            &cfg.board,
            &text,
            cfg.models_dir.as_deref(),
            cfg.firmware.clone(),
            schematic,
            cfg.chunk_us,
        )?;
        return Ok(true);
    }

    Ok(false)
}

pub(crate) fn build_live_simulation(
    cfg: &RunConfig,
    run_inputs: &RunInputs,
    mut prebound: Option<BoundBoard>,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<SimulationContext> {
    let board = &run_inputs.board;
    let text = &run_inputs.text;
    let raw = &run_inputs.raw;
    let input_kind = run_inputs.input_kind;
    let is_altium = run_inputs.is_altium;
    let lib = &run_inputs.lib;
    let reader_notes = &run_inputs.reader_notes;
    // Bind with the layered library (so a --models-dir / user-dir custom part is
    // in scope), then build the engine from the bound board. When --asbuilt was
    // given, the overlay was already validated, applied and narrated up front;
    // reuse that bound board rather than re-binding and re-applying.
    let bound = match prebound.take() {
        Some(b) => b,
        None => bind_board(&board, &lib),
    };
    let mut board_evidence = crate::evidence::BoardEvidence::from_bound(
        &board,
        &bound.report,
        &reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?
    .with_input_artifact(&cfg.board, &raw, input_kind)?;
    if let Some(firmware) = &cfg.firmware {
        board_evidence = board_evidence.with_firmware_artifact(
            firmware,
            &std::fs::read(firmware).map_err(|error| {
                anyhow::anyhow!(
                    "reading firmware evidence '{}': {error}",
                    firmware.display()
                )
            })?,
        )?;
    }
    // Net names captured before `bound` is consumed, for --probe validation.
    let probe_known_nets: Vec<String> = if cfg.probe.is_empty() {
        Vec::new()
    } else {
        bound.net_names.clone()
    };
    // Pre-flight: firmware on a `qemu:` backend needs the Espressif QEMU fork.
    // On an interactive terminal, offer to fetch it inline (official prebuilt,
    // into ~/.hauksbee-qemu-esp) so the run continues; declined or
    // non-interactive paths keep the loud install-guidance error the scheduler
    // raises. CLI-layer on purpose: the library/server must never prompt.
    if cfg.firmware.is_some() {
        // Firmware on a board with no processor cannot produce an answer: every
        // "the firmware must ..." assertion would pass because nothing ever ran.
        // That is invalid-for-analysis, not a warning, so it exits 3 like the
        // other unanswerable runs rather than reporting a vacuous success.
        if bound.mcus.is_empty() {
            let missing =
                crate::binder::no_processor_message(&bound.dnp_mcus, crate::binder::FitRemedy::Cli);
            let refusal = Refusal::new(
                "firmware behavior on this board",
                missing.clone(),
                vec!["board extraction, binding, and static copper checks remain available"],
                "fit/select a supported MCU on the board or remove --firmware and rerun the static analysis",
            );
            if cfg.json {
                let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
                jr.refusal = Some(refusal.clone());
                println!("{}", jr.to_json());
            } else {
                eprintln!("error: {missing}");
                eprintln!("{}", refusal.render_text());
            }
            crate::reports::ci_artifacts::exit_with_refusal(
                crate::result::EXIT_INVALID_FOR_ANALYSIS,
                &refusal,
            );
        }
        let backends: Vec<String> = bound.mcus.iter().map(|m| m.backend.clone()).collect();
        crate::commands::install::offer_esp_qemu_install(&backends)?;
    }
    let mut engine = HauksbeeEngine::from_bound(
        bound,
        cfg.firmware.as_deref(),
        &format!("/boards/{}", crate::commands::common::file_name(&cfg.board)),
    )?;
    board_evidence =
        board_evidence.with_scoped_substitutions(engine.scheduler().scoped_substitutions())?;

    // --apply-shorts: bridge every detected copper short before simulating.
    if cfg.apply_shorts {
        let report = if is_altium {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                crate::reports::kicad_pro_clearance_rules(&cfg.board, &board),
            )?
        };
        let qualification = schematic_ties.map(|ties| ties.qualify(&report));
        let applied = engine.apply_drc_shorts_with_qualification(&report, qualification.as_ref());
        eprintln!(
            "applied {applied} copper short(s) of {} detected ({} clearance violations)",
            report.short_count(),
            report.clearance_violations().count(),
        );
        // A served live sim must also DISCLOSE the outcome on the wire
        // BoardInfo, matching the report co-sim's "ran WITH the shorts
        // bridged" note.
        if cfg.serve && report.short_count() > 0 {
            engine.set_shorts_disclosure(hauksbee_frontdoor_api::protocol::ShortsDisclosure {
                detected: report.short_count(),
                bridged: applied,
                unapplied_reason: (applied == 0).then(|| {
                    "the shorted nets could not be bridged into the live circuit".to_string()
                }),
            });
        }
    }

    Ok(SimulationContext {
        engine,
        board_evidence,
        probe_known_nets,
    })
}

pub(crate) fn run_selected_simulation_surface(
    cfg: &RunConfig,
    surface: SelectedSurface,
    run_inputs: &RunInputs,
    artifacts: &super::prepare::RunArtifacts,
    sim: &mut SimulationContext,
) -> anyhow::Result<bool> {
    // --thermal: run a short co-sim, then print the steady-state junction
    // temperature per dissipating device and exit. Fix #1: a thermal table that
    // covers ~no dissipating devices because the power ICs are UNRESOLVED is a
    // meaningless result, not a "runs cool" pass, flag it invalid and exit 3.
    // Strict is the DEFAULT: a PARTIAL-coverage table escalates to exit 3
    // unless --no-strict-thermal opts out (--strict-thermal is accepted as a
    // quiet no-op so existing CI invocations keep working).
    if surface == SelectedSurface::Thermal {
        crate::reports::thermal::emit(
            &mut sim.engine,
            &sim.board_evidence,
            cfg.ambient,
            cfg.seconds,
            cfg.json,
            !cfg.no_strict_thermal,
            &run_inputs.inputs,
        )?;
        return Ok(true);
    }
    // --serial-attach: a live co-sim with a host-facing serial port. Placed before
    // the headless report path because it IS a co-sim run, just one whose stimulus
    // comes from the user's own software instead of a report flag; it prints its
    // own endpoint narration and session summary.
    if surface == SelectedSurface::Serial {
        let scfg = crate::commands::hostserial::SerialSessionConfig {
            transport: cfg.serial_transport,
            wait_secs: cfg.serial_wait,
            pace: !cfg.serial_no_pace,
            mcu: cfg.serial_mcu.clone(),
            chunk_us: cfg.chunk_us,
            ..Default::default()
        };
        let mut say = |line: &str| eprintln!("{line}");
        let summary = crate::commands::hostserial::run_session(
            &mut sim.engine,
            cfg.seconds,
            &scfg,
            &mut say,
        )?;
        for line in crate::commands::hostserial::summary_lines(&summary) {
            eprintln!("{line}");
        }
        return Ok(true);
    }
    if surface == SelectedSurface::Headless {
        super::cosim::run_headless(cfg, run_inputs, artifacts, sim)?;
        return Ok(true);
    }
    Ok(false)
}
