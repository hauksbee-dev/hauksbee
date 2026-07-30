//! `hauksbee run <board>`: the run orchestrator. Loads/binds the board, then
//! dispatches to the right report (reports/), the interactive TUI, a headless
//! co-sim, or the websocket server. Argument parsing lives in the binary; this
//! takes a plain [`RunConfig`].

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::engine::HauksbeeEngine;
use crate::result::{
    strict_analog_exit_code, BindSummary, BootGateJson, JsonNote, JsonNoteKind, JsonReport,
    EXIT_INVALID_FOR_ANALYSIS,
};

/// Plain (non-clap) mirror of the binary's `RunArgs`, so the `run` orchestrator
/// lives in the library while argument parsing stays in `main.rs`. Field types
/// match `RunArgs` exactly; the binary builds one and hands it over.
pub struct RunConfig {
    pub board: std::path::PathBuf,
    pub firmware: Option<std::path::PathBuf>,
    pub seconds: f64,
    pub headless: bool,
    pub report: bool,
    pub drc: bool,
    pub ampacity: bool,
    pub lint: bool,
    pub si: bool,
    pub resources: bool,
    pub usb_c: bool,
    pub thermal: bool,
    pub ambient: f64,
    pub plain: bool,
    pub json: bool,
    pub strict: bool,
    pub strict_thermal: bool,
    pub strict_boot: bool,
    pub list_nets: bool,
    pub check: bool,
    pub oracle: bool,
    pub apply_shorts: bool,
    pub serve: bool,
    pub tui: bool,
    pub port: u16,
    pub models_dir: Option<std::path::PathBuf>,
    pub ac: Option<String>,
    pub ac_node: Vec<String>,
    pub ac_csv: Option<std::path::PathBuf>,
    pub ac_loop: Option<String>,
    pub probe: Vec<String>,
    pub probe_csv: Option<std::path::PathBuf>,
    /// Solver chunk width in microseconds. Narrower chunks resolve firmware
    /// pulses the default width steps straight over, at proportional cost.
    pub chunk_us: Option<f64>,
    /// References of DNP parts to simulate as fitted, whatever the policy says.
    pub fit: Vec<String>,
    /// References of DNP parts to leave open, whatever the policy says.
    pub no_fit: Vec<String>,
    /// What to do with the DNP parts neither list names.
    pub dnp_policy: hauksbee_extract::dnp::DnpPolicy,
    /// Open a host-facing serial endpoint for the co-sim, so the user's own
    /// software can talk to the emulated MCU's UART (see
    /// `commands::hostserial`).
    pub serial_attach: bool,
    /// `pty` (default: unmodified serial software works unmodified) or `tcp`.
    pub serial_transport: hauksbee_mcu::hostserial::HostSerialTransport,
    /// Hold the co-sim at t=0 until a host tool attaches, at most this long.
    pub serial_wait: Option<f64>,
    /// Let the co-sim run as fast as it can instead of pacing it to wall-clock
    /// time.
    pub serial_no_pace: bool,
    /// Which MCU reference to bridge, when a board carries more than one.
    pub serial_mcu: Option<String>,
    /// `.asbuilt.toml` overlay: the declarative physical delta (cuts, jumpers,
    /// fitted values) between the design files and the real reworked board,
    /// applied to the bound board before the engine is built.
    pub asbuilt: Option<std::path::PathBuf>,
}

pub fn run(mut cfg: RunConfig, quiet: bool) -> anyhow::Result<()> {
    // Validate `--firmware` up front, before any heavy work or the TUI takes over
    // the terminal. The native emulator loaders segfault (exit 139) on a missing
    // file instead of erroring; this turns a one-character typo into a clean,
    // actionable message naming the absolute path that was tried.
    if let Some(fw) = &cfg.firmware {
        // A PlatformIO project directory, a built .pio tree, or a zip of either
        // resolves to its compiled image first; a bare .elf/.hex passes through.
        if let Some(resolved) = crate::firmware_input::resolve_firmware_cli(fw)? {
            if !quiet {
                eprintln!("  firmware: {}", resolved.note);
            }
            cfg.firmware = Some(resolved.path);
        }
        hauksbee_mcu::validate_firmware_path(cfg.firmware.as_deref().expect("just set"))?;
    }
    // Advisory: if this board sits among sibling .kicad_pcb files (a multi-board
    // product), say so, a clean verdict on one file is misleading if the user
    // meant the whole thing. Routed through `Notes` so it stays on stderr and is
    // silenced under --quiet / --json / a piped stdout (it is helpful exactly once
    // for an interactive user, and pure noise in report and pipeline output).
    let notes = Notes::new(quiet, cfg.json);
    warn_sibling_boards(&cfg.board, notes);
    // Every input format (gerber dir/zip, binary Altium, Board-as-Code, KiCad
    // schematic hierarchy, the text layout/netlist formats) routes through the
    // ONE normalizer shared with the web front door and hauksbee-ci, including
    // the file-stem name fallback for titleless boards. `raw` keeps the file's
    // exact bytes (the Altium DRC twin reads copper from them) and `text` is
    // the KiCad-parseable layout text (empty for Altium/gerber, which have
    // none; those checks get the bytes twin or an honest "not checked").
    let norm = crate::board_input::from_path(&cfg.board)?;
    let is_altium = norm.is_binary();
    let is_board_code = norm.kind == crate::board_input::InputKind::BoardCode;
    let raw = norm.raw;
    let text = norm.layout_text.unwrap_or_default();
    let mut board = norm.board;
    // Do-not-populate policy, applied before binding because DNP decides
    // whether a part is stamped at all. The decision is printed rather than
    // assumed: a board where a part was quietly added or dropped is a board
    // whose numbers mean something other than the reader thinks.
    let dnp = board.apply_dnp_policy(cfg.dnp_policy, &cfg.fit, &cfg.no_fit)?;
    if !quiet {
        for line in dnp.lines() {
            eprintln!("{line}");
        }
    }
    let board = board;
    // Layered model library: builtin < ~/.hauksbee/models (datasheet-extracted)
    // < ~/.config/hauksbee/models (user) < --models-dir (highest). A custom
    // behavioural part dropped in any of these loads with no recompile.
    let extra: Vec<&std::path::Path> = cfg.models_dir.as_deref().into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);

    // --probe records live waveforms, which only exist during a co-sim; it is
    // meaningless for the static reports and the interactive server. Fail loudly
    // rather than silently ignore the flag.
    if !cfg.probe.is_empty() && !cfg.headless {
        anyhow::bail!("--probe records co-sim waveforms and needs --headless");
    }

    // --serial-attach bridges a host serial port to the firmware's UART, so
    // without firmware there is nothing on the far end to answer. Refuse here,
    // before any binding work, rather than open a port onto silence.
    if cfg.serial_attach && cfg.firmware.is_none() {
        anyhow::bail!(
            "--serial-attach connects your own software to the emulated MCU's UART, so it \
             needs firmware to talk to: add --firmware <FILE>"
        );
    }

    // --list-nets: print the board's net names so the user can pick one for
    // --ac-node / --ac-loop without grepping the layout. One net per line on
    // stdout (pipeable); a JSON array under --json.
    if cfg.list_nets {
        let bound = bind_board(&board, &lib);
        let mut nets: Vec<String> = bound.net_names.clone();
        nets.sort();
        if cfg.json {
            println!("{}", list_nets_json(&nets));
        } else {
            eprintln!("{} net(s):", nets.len());
            for n in &nets {
                println!("{n}");
            }
        }
        return Ok(());
    }

    // --check / --all: the whole static suite (bind + DRC + lint + SI) in ONE
    // report, so a person (or an AI) gets everything in a single command instead
    // of running one flag at a time. Honours --plain / --json / --strict.
    if cfg.check {
        return crate::reports::check::emit(
            &cfg.board,
            &board,
            &text,
            &raw,
            is_altium,
            &lib,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
        );
    }

    if cfg.report {
        return crate::reports::bind::emit(
            &board,
            &lib,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
        );
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if cfg.drc {
        return crate::reports::drc::emit(
            &cfg.board,
            &board,
            &text,
            &raw,
            is_altium,
            &lib,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.oracle,
            cfg.strict,
        );
    }

    // --ampacity: IPC-2221 capacity-only report. No current is fabricated here:
    // without a per-net current spec this tells the user the bottleneck capacity
    // and explicitly asks for a current before pass/fail.
    if cfg.ampacity {
        return crate::reports::ampacity::emit(&text, is_altium);
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if cfg.lint {
        return crate::reports::lint::emit(
            &board,
            &lib,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
        );
    }

    // --resources: run only the MCU internal resource-conflict check, print, exit.
    if cfg.resources {
        return crate::reports::lint::emit_resources(
            &board,
            &lib,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
        );
    }

    // --usb-c: run the USB-C CC attach classifier (the RPi 4 re-derivation) and
    // print the compliance report. The capability existed but was unreachable from
    // any user-facing surface; this is its CLI front door.
    if cfg.usb_c {
        return crate::reports::usb_c::emit(
            &board,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
        );
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if cfg.si {
        return crate::reports::si::emit(
            &board,
            &text,
            is_altium,
            &lib,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
        );
    }

    // --ac: small-signal AC sweep on the bound circuit, print Bode + (optional)
    // loop-stability margins, then exit. Informational like the other reports.
    if let Some(ac_arg) = &cfg.ac {
        let bound = bind_board(&board, &lib);
        return crate::reports::ac::emit(
            &bound,
            ac_arg,
            &cfg.ac_node,
            cfg.ac_csv.as_deref(),
            cfg.ac_loop.as_deref(),
            cfg.json,
        );
    }

    // Bare `--json` with no specific report selector: emit a COMBINED machine
    // report (bind + DRC + lint/straps/resources + SI) and exit. Without this,
    // `--json` alone falls through to the TUI/websocket default below and hangs a
    // piped / CI / AI caller (the regression a bare `run <board> --json` hit).
    // `--json` is an explicit machine-intent flag, so it must never launch the TUI.
    // `--thermal`/`--headless` are selectors handled further down with their OWN
    // JSON emitters (thermal coverage, co-sim notes); they must fall THROUGH this
    // combined branch or those JSON paths become unreachable dead code.
    if cfg.json && !cfg.thermal && !cfg.headless {
        return crate::reports::check::emit_combined_json(
            &cfg.board, &board, &text, &raw, is_altium, &lib, cfg.strict,
        );
    }

    // Default flow (no report/headless/ac flag). The interactive terminal UI is
    // the new human-facing default: bare `run <board>` on a TTY launches it. Any
    // explicit report flag was handled above, so reaching here means none was
    // given. `--serve` keeps the historical websocket frontend; a non-TTY stdout
    // (piped / CI) also keeps the websocket behaviour untouched, so existing
    // scripts and tests are unaffected.
    //
    // `--firmware`/`--apply-shorts` only matter for the simulating paths; the TUI
    // honours `--firmware` for its co-sim pane. We branch to the TUI before
    // building the websocket engine so we never spin up tokio for the TUI path.
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
        return crate::tui::run(
            &cfg.board,
            &text,
            cfg.models_dir.as_deref(),
            cfg.firmware.clone(),
        );
    }

    // Bind with the layered library (so a --models-dir / user-dir custom part is
    // in scope), then build the engine from the bound board.
    let mut bound = bind_board(&board, &lib);
    // --asbuilt: apply the declarative rework overlay post-bind, pre-engine
    // (the same seam the flagship prep uses). Fail-loud: an overlay that does
    // not describe this board aborts the run with its line-numbered error.
    if let Some(asbuilt_path) = &cfg.asbuilt {
        let overlay = crate::asbuilt::AsBuiltOverlay::load(asbuilt_path)?;
        let report = overlay.apply(&mut bound)?;
        if !quiet {
            println!("as-built overlay {} applied:", asbuilt_path.display());
            for line in &report.lines {
                println!("  {line}");
            }
        }
    }
    let bound = bound;
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
            eprintln!(
                "error: {}",
                crate::binder::no_processor_message(&bound.dnp_mcus, crate::binder::FitRemedy::Cli)
            );
            std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS);
        }
        let backends: Vec<String> = bound.mcus.iter().map(|m| m.backend.clone()).collect();
        crate::commands::install::offer_esp_qemu_install(&backends)?;
    }
    let mut engine = HauksbeeEngine::from_bound(
        bound,
        cfg.firmware.as_deref(),
        &format!("/boards/{}", crate::commands::common::file_name(&cfg.board)),
    )?;

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
        let applied = engine.apply_drc_shorts(&report);
        eprintln!(
            "applied {applied} copper short(s) of {} detected ({} clearance violations)",
            report.short_count(),
            report.clearance_violations().count(),
        );
        // A served live sim must also DISCLOSE the outcome on the wire
        // BoardInfo, matching the report co-sim's "ran WITH the shorts
        // bridged" note.
        if cfg.serve && report.short_count() > 0 {
            engine.set_shorts_disclosure(hauksbee_server::protocol::ShortsDisclosure {
                detected: report.short_count(),
                bridged: applied,
                unapplied_reason: (applied == 0).then(|| {
                    "the shorted nets could not be bridged into the live circuit".to_string()
                }),
            });
        }
    }

    // --thermal: run a short co-sim, then print the steady-state junction
    // temperature per dissipating device and exit. Fix #1: a thermal table that
    // covers ~no dissipating devices because the power ICs are UNRESOLVED is a
    // meaningless result, not a "runs cool" pass, flag it invalid and exit 3.
    if cfg.thermal {
        return crate::reports::thermal::emit(
            &mut engine,
            cfg.ambient,
            cfg.seconds,
            cfg.json,
            cfg.strict_thermal,
        );
    }

    // --serial-attach: a live co-sim with a host-facing serial port. Placed before
    // the headless report path because it IS a co-sim run, just one whose stimulus
    // comes from the user's own software instead of a report flag; it prints its
    // own endpoint narration and session summary.
    if cfg.serial_attach {
        let scfg = crate::commands::hostserial::SerialSessionConfig {
            transport: cfg.serial_transport,
            wait_secs: cfg.serial_wait,
            pace: !cfg.serial_no_pace,
            mcu: cfg.serial_mcu.clone(),
            chunk_us: cfg.chunk_us,
        };
        let mut say = |line: &str| eprintln!("{line}");
        let summary =
            crate::commands::hostserial::run_session(&mut engine, cfg.seconds, &scfg, &mut say)?;
        for line in crate::commands::hostserial::summary_lines(&summary) {
            eprintln!("{line}");
        }
        return Ok(());
    }

    if cfg.headless {
        // --probe preconditions, checked before the run so a typo fails fast with
        // the same near-match style the rest of the net-facing CLI uses.
        let probes = crate::reports::cosim::dedup_probes(&cfg.probe);
        crate::reports::cosim::validate_probes(
            &probes,
            cfg.probe_csv.as_deref(),
            &probe_known_nets,
        )?;

        let board_name = engine.report().board_name.clone();
        let summary = BindSummary::from_report(engine.report());
        let mut uart_seen = false;
        let headless = crate::reports::cosim::run_headless(
            &mut engine,
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
        let cosim = crate::reports::cosim::build_cosim_json(&engine, uart_seen).map(|mut c| {
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
        let analog_valid = engine.scheduler().analog_valid();
        let failed_chunk_count = engine.scheduler().failed_chunk_count();
        let analog_abort = engine.scheduler().analog_abort_tripped();
        // A co-sim that drove no GPIO, produced no net toggles, AND emitted no
        // UART did not exercise the firmware. `any_gpio_driven()` is essential:
        // a firmware that drives a control line high and HOLDS it (boot-gate style)
        // has zero net toggles yet clearly ran, so a toggles-only test would cry
        // wolf on it. Determined BEFORE emitting so the refusal reaches every
        // surface, including --json when no MCU was instantiated (cosim is None).
        let zero_activity =
            total_toggles == 0 && !uart_seen && !engine.scheduler().any_gpio_driven();

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
            cfg.firmware.is_some() && !zero_activity,
        );
        let held_high_boot_nets = &boot_advisory.held_high_control_nets;
        let has_boot_advisory = !held_high_boot_nets.is_empty();
        let gate_rows = &boot_advisory.gate_states;

        if cfg.json {
            let mut jr = JsonReport::new(&board_name, summary);
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
            // A non-convergent chunk held stale voltages: a loud coverage note so
            // a CI consumer that filters notes (not just the CosimJson body) sees
            // the analog side is not trustworthy over the failed windows (05 §3b).
            if !analog_valid {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: format!(
                        "co-sim analog solve failed to converge on {failed_chunk_count} \
                         chunk(s); those windows held stale node voltages and are \
                         reported as analog_valid:false; analog-derived findings over \
                         them are not trustworthy"
                    ),
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
            if !faults.is_empty() {
                jr.findings = Some(crate::result::fault_findings_json(&faults));
            }
            jr.cosim = cosim;
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
            if !analog_valid {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "co-sim analog solve failed to converge on {failed_chunk_count} chunk(s); \
                     those windows held stale voltages and cannot be trusted (analog_valid is \
                     false); rerun with --json to see the exact failed windows"
                )));
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
                     INCOMPLETE (the copper/DRC checks are unaffected). Add models with --models-dir.",
                    summary.critical_parts_bound_n,
                    summary.critical_parts_total,
                    open.len()
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
                     unresolved/open, so firmware/analog results on their nets are INCOMPLETE; \
                     add models with --models-dir (copper/DRC checks are unaffected).",
                    summary.critical_parts_bound_n,
                    summary.critical_parts_total,
                    open.len()
                );
            }
        }

        // 0-activity refusal (Track B): warn always; under --strict this is a hard
        // refusal (exit 3), not a clean pass. The UART-AND-toggles guard avoids
        // false positives on firmware that is busy on the bus but quiet on GPIO.
        if zero_activity {
            eprintln!(
                "WARNING: co-sim saw zero net toggles; cannot vouch for firmware \
                 behaviour (the MCU may have stalled at boot, run no I/O, or the \
                 firmware may not match this board)."
            );
            if cfg.strict {
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            }
        }

        // Refuse-rather-than-fake (05 §3b): once the analog solve was stuck for a
        // whole streak of chunks, a strict run must abort with the invalid code
        // rather than complete a fake-quiet run. Warn always so the reason is never
        // silent; only --strict turns it into a failing exit.
        if analog_abort {
            eprintln!(
                "WARNING: co-sim analog solve failed to converge for {} chunks in a row \
                 ({} failed chunks total); the run held stale voltages and cannot vouch \
                 for the analog side.",
                crate::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT,
                failed_chunk_count,
            );
            if let Some(code) = strict_analog_exit_code(cfg.strict && analog_abort) {
                std::process::exit(code);
            }
        }

        // Strict: any fault raised during the run fails the gate.
        if cfg.strict && !faults.is_empty() {
            std::process::exit(2);
        }
        // --strict-boot: opt-in escalation of the boot-safety advisory to a
        // failing gate (exit 2). The run was valid and these are real findings
        // about specific nets; default behaviour leaves them advisory-only. Print
        // the reason to stderr so the failure is never silent, including in the
        // default headless mode (neither --json nor --plain), where the advisory
        // text is not otherwise emitted.
        if cfg.strict_boot && has_boot_advisory {
            for net in held_high_boot_nets {
                eprintln!(
                    "BOOT HAZARD (--strict-boot): control net '{net}' switches a transistor/relay \
                     and is driven HIGH and held from power-up with no bias resistor; the load is \
                     energised at reset."
                );
            }
            std::process::exit(2);
        }
        return Ok(());
    }

    // Non-TTY invocation with no report flag and no explicit --serve: rather than
    // silently starting a websocket server a pipe / CI can't use (the "something
    // different" §7 warns about), print a two-line hint pointing at the report
    // surfaces and exit cleanly. A TTY would have launched the TUI above; an
    // explicit --serve keeps the historical websocket behaviour untouched.
    if !stdout_is_tty && !cfg.serve {
        eprintln!(
            "hauksbee run: stdout is not a terminal, so there is no interactive dashboard to show."
        );
        eprintln!(
            "  For a report add a flag: --check (all static checks) · --plain (prose) · --json (machine); or --serve for the browser UI."
        );
        return Ok(());
    }

    // The preloaded live session must run the board AS BUILT: run the same
    // geometric DRC the report page shows and bridge its validated shorts into
    // the live engine (KiCad-10 `version_warning` shorts refused, reason
    // disclosed), exactly like the web co-sim. Without this the live sim
    // silently streamed idealised rails right under a report whose co-sim
    // block ran WITH the shorts bridged and said so. Skipped when
    // `--apply-shorts` already bridged and disclosed them above.
    if !cfg.apply_shorts {
        let drc_report = if is_altium {
            ExtractedBoard::altium_drc(&raw).unwrap_or_default()
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                crate::reports::kicad_pro_clearance_rules(&cfg.board, &board),
            )
            .unwrap_or_default()
        };
        engine.apply_and_disclose_drc_shorts(&drc_report);
    }

    // Serve the loaded board's own file at the URL the frontend fetches it from
    // (`/boards/<name>`), so the 2D/3D viewer renders the real geometry for any
    // board, not just the demo boards baked into dist/.
    let file_name = crate::commands::common::file_name(&cfg.board);
    let board_url = format!("/boards/{file_name}");

    // `run --serve` preloads the board, so the React app lands on THIS
    // board's report (the same JSON the drop path produces) and "run it" expands
    // it into the live sim already running on `/ws`. Compute the report once here
    // and hand it to the app via `/api/startup`. Board-only unless firmware was
    // supplied (then include the in-process co-sim, matching the drop path).
    // The analyzers take the board as raw bytes (so binary formats survive)
    // and normalize by file name, exactly like the drop path. Binary (Altium)
    // and Board-as-Code inputs hand over the file's own bytes: a `.board`
    // name with the recompiled KiCad text would be re-"compiled" as DSL and
    // fail. Plain text boards hand over the layout text.
    let report_bytes: &[u8] = if is_altium || is_board_code {
        &raw
    } else {
        text.as_bytes()
    };
    let report_json = match &cfg.firmware {
        Some(fw) => {
            let fw_name = crate::commands::common::file_name(fw);
            match std::fs::read(fw) {
                Ok(bytes) => {
                    crate::analyze_with_firmware_json(&file_name, report_bytes, &fw_name, &bytes)
                }
                // Firmware was already path-validated above; a read error here is
                // unexpected, so fall back to the board-only report rather than fail.
                Err(_) => crate::analyze_json(&file_name, report_bytes),
            }
        }
        None => crate::analyze_json(&file_name, report_bytes),
    };
    let report_val: serde_json::Value =
        serde_json::from_str(&report_json).unwrap_or(serde_json::Value::Null);
    let startup_json = serde_json::json!({
        "preloaded": true,
        "board_name": file_name,
        "report": report_val,
        // This server can also launch a live session for a NEWLY uploaded
        // board (replacing the preloaded one), same as `hauksbee serve`.
        "live": true,
        // Engine version, for the Environment page's "what am I running" card.
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string();

    crate::commands::common::serve(engine, cfg.port, Some((board_url, text)), startup_json)
}

/// Warn (advisory, stderr) when the board sits among sibling `.kicad_pcb` files,
/// a multi-board product (e.g. a main board with a separate ESC/daughter board in
/// a sibling folder). A clean verdict on ONE file reads as "the product is fine"
/// when the user may have meant the whole thing. Best-effort; never fails the run.
/// Whether informational `note:` lines should be shown for this invocation.
/// They go to stderr and only for an interactive human: suppressed under
/// `--quiet`, under `--json` (machine output), and when stdout is piped or
/// redirected (not a TTY), so report and pipeline output stays clean while the
/// note stays discoverable for interactive users. Pure so it is unit-testable
/// without a real terminal.
fn notes_visible(quiet: bool, json: bool, stdout_is_tty: bool) -> bool {
    !quiet && !json && stdout_is_tty
}

/// The `--list-nets --json` array, serialized through serde_json rather than
/// Rust `Debug`. The two agree on the common escapes but diverge on control
/// characters, Debug emits variable-length brace-hex, whereas JSON mandates a
/// fixed four-hex-digit escape, so a net name carrying a control char from a
/// malformed/adversarial netlist would make the hand-`Debug`-assembled array a
/// document no JSON parser accepts. This is pure so it is unit-testable.
fn list_nets_json(nets: &[String]) -> String {
    serde_json::to_string(nets).unwrap_or_else(|_| "[]".into())
}

/// The single gate every chatty informational note routes through, so `--quiet`
/// (and JSON / non-TTY suppression) is honoured uniformly and future notes
/// inherit the behaviour by going through here instead of a bare `eprintln!`.
#[derive(Clone, Copy)]
struct Notes {
    enabled: bool,
}

impl Notes {
    fn new(quiet: bool, json: bool) -> Self {
        Notes {
            enabled: notes_visible(
                quiet,
                json,
                std::io::IsTerminal::is_terminal(&std::io::stdout()),
            ),
        }
    }

    /// Emit a single-line informational note (prefixed `note:`) on stderr, unless
    /// notes are suppressed for this invocation.
    fn say(&self, msg: impl std::fmt::Display) {
        if self.enabled {
            eprintln!("note: {msg}");
        }
    }
}

fn warn_sibling_boards(board: &std::path::Path, notes: Notes) {
    if !notes.enabled {
        return;
    }
    let Ok(abs) = std::fs::canonicalize(board) else {
        return;
    };
    let Some(dir) = abs.parent() else {
        return;
    };
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    let is_hidden = |p: &std::path::Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
    };
    let mut scan = |d: &std::path::Path| {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("kicad_pcb") && p != abs {
                    found.push(p);
                }
            }
        }
    };
    scan(dir);
    // Immediate CHILD directories (e.g. a daughter board in `KiCad/ESC_Board/`)
    // and SIBLING directories (children of the grandparent). One level only, and
    // hidden dirs (`.history`, `.git`) are skipped so we don't surface backups.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && !is_hidden(&p) {
                scan(&p);
            }
        }
    }
    if let Some(gp) = dir.parent() {
        if let Ok(rd) = std::fs::read_dir(gp) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && p != dir && !is_hidden(&p) {
                    scan(&p);
                }
            }
        }
    }
    found.sort();
    found.dedup();
    if found.is_empty() {
        return;
    }
    notes.say(format!(
        "{} other board file(s) found nearby; this run only checks '{}':",
        found.len(),
        board.display()
    ));
    for p in found.iter().take(5) {
        eprintln!("  - {}", p.display());
    }
    if found.len() > 5 {
        eprintln!("  ... and {} more", found.len() - 5);
    }
    eprintln!("  If they are part of the same product, check each one separately.");
}

#[cfg(test)]
mod list_nets_json_tests {
    use super::list_nets_json;

    #[test]
    fn control_char_net_names_emit_valid_json() {
        // R51: the array was hand-assembled with `format!("{n:?}")`, so a net name
        // carrying a control char (e.g. U+0007 from a malformed netlist) produced
        // `\u{7}`, valid Rust Debug but invalid JSON. serde_json must emit a
        // document every JSON parser accepts.
        let nets = vec![
            "GND".to_string(),
            "bell\u{7}x".to_string(),
            "esc\u{1b}end".to_string(),
            "nét".to_string(),
        ];
        let out = list_nets_json(&nets);
        // The output must round-trip through a strict JSON parser back to the
        // original names; the base bug's `\u{7}` form fails to parse here.
        let parsed: Vec<String> =
            serde_json::from_str(&out).expect("list-nets --json must be valid JSON");
        assert_eq!(parsed, nets);
    }
}

#[cfg(test)]
mod notes_gate_tests {
    use super::notes_visible;

    #[test]
    fn shown_only_for_interactive_non_json_non_quiet() {
        // Default interactive terminal: the note is discoverable.
        assert!(notes_visible(false, false, true));
    }

    #[test]
    fn quiet_suppresses_notes() {
        assert!(!notes_visible(true, false, true));
        assert!(!notes_visible(true, false, false));
        assert!(!notes_visible(true, true, true));
    }

    #[test]
    fn json_never_emits_notes() {
        // --json is machine output: no note regardless of TTY or quiet.
        assert!(!notes_visible(false, true, true));
        assert!(!notes_visible(false, true, false));
    }

    #[test]
    fn piped_stdout_suppresses_notes() {
        // Non-TTY stdout (piped / redirected / CI): keep report output clean.
        assert!(!notes_visible(false, false, false));
    }
}
