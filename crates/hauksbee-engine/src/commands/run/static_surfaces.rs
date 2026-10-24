//! Dispatches report-only surfaces before a live simulation engine is built.
//! Inventory, combined checks, binding, DRC, ampacity, lint, resource, USB-C,
//! signal-integrity, AC, and bare JSON reports retain their established
//! precedence while sharing normalized inputs and resolved schematic context.

use crate::binder::bind_board;
use hauksbee_ir::{eprintln, print, println};

use super::{
    list_nets_json,
    prepare::{RunArtifacts, RunInputs},
    RunConfig, SelectedSurface,
};

#[allow(clippy::too_many_lines, reason = "flat dispatch over report surfaces")]
pub fn emit_selected(
    cfg: &RunConfig,
    quiet: bool,
    surface: SelectedSurface,
    run_inputs: &RunInputs,
    artifacts: &mut RunArtifacts,
) -> anyhow::Result<bool> {
    // The as-built board when --asbuilt was given: static checks judge the
    // board on the bench, not the design it was reworked from.
    let board = artifacts
        .asbuilt_board
        .as_ref()
        .unwrap_or(&run_inputs.board);
    let text = &run_inputs.text;
    let raw = &run_inputs.raw;
    let input_kind = run_inputs.input_kind;
    let is_altium = run_inputs.is_altium;
    let lib = &run_inputs.lib;
    let reader_notes = &run_inputs.reader_notes;
    let inputs = &run_inputs.inputs;
    let schematic_ties = &artifacts.schematic_ties;
    let report_inputs = crate::reports::StaticInputs {
        board_path: &cfg.board,
        board,
        text,
        raw,
        input_kind,
        altium_present: is_altium,
        lib,
        reader_notes,
        inputs,
        firmware: cfg.firmware.as_deref(),
    };
    let mode = crate::reports::OutputMode::from_flags(cfg.json, cfg.plain);
    let prebound = &mut artifacts.prebound;
    // --list-nets: print the board's net names so the user can pick one for
    // --ac-node / --ac-loop without grepping the layout. One net per line on
    // stdout (pipeable); a JSON array under --json.
    if surface == SelectedSurface::Inventory {
        let bound = prebound.take().unwrap_or_else(|| bind_board(board, lib));
        let mut nets: Vec<String> = bound.net_names;
        nets.sort();
        if cfg.json {
            println!("{}", list_nets_json(&nets));
        } else {
            eprintln!("{} net(s):", nets.len());
            for n in &nets {
                println!("{n}");
            }
        }
        return Ok(true);
    }

    // --emit-netlist: the connectivity the extractor derived, so another tool
    // can ask hauksbee what is connected to what without re-deriving a netlist
    // from the board file itself. It answers on a board whose parts have no
    // models at all, because connectivity is settled before binding.
    if surface == SelectedSurface::Netlist {
        if cfg.json {
            println!("{}", super::netlist_json(board));
        } else {
            print!("{}", super::netlist_lines(board));
        }
        return Ok(true);
    }

    // --check / --all: the whole static suite (bind + DRC + lint + SI) in ONE
    // report, so a person (or an AI) gets everything in a single command instead
    // of running one flag at a time. Honours --plain / --json / --strict.
    if surface == SelectedSurface::Check {
        return crate::reports::check::emit_with_schematic_quiet(
            &report_inputs,
            mode,
            cfg.strict,
            cfg.verbose,
            cfg.oracle,
            quiet,
            schematic_ties.as_ref(),
        )
        .map(|()| true);
    }

    // Bare --plain with an explicit --seconds: the same combined check, but its
    // closing verdict and strict exits wait for the co-sim that follows, so the
    // run ends on one verdict covering both. Returns false: the run goes on.
    if surface == SelectedSurface::CheckThenCosim {
        artifacts.static_outcome = Some(crate::reports::check::emit_deferred_with_schematic_quiet(
            &report_inputs,
            mode,
            cfg.strict,
            cfg.verbose,
            cfg.oracle,
            quiet,
            schematic_ties.as_ref(),
        )?);
        return Ok(false);
    }

    if surface == SelectedSurface::Bind {
        return crate::reports::bind::emit_quiet(
            board,
            lib,
            reader_notes,
            mode,
            quiet,
            cfg.verbose,
            inputs,
        )
        .map(|()| true);
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if surface == SelectedSurface::Drc {
        return crate::reports::drc::emit_with_schematic_quiet(
            &report_inputs,
            mode,
            cfg.oracle,
            cfg.strict,
            cfg.verbose,
            quiet,
            schematic_ties.as_ref(),
        )
        .map(|()| true);
    }

    // --ampacity: IPC-2221 capacity-only report. No current is fabricated here:
    // without a per-net current spec this tells the user the bottleneck capacity
    // and explicitly asks for a current before pass/fail.
    if surface == SelectedSurface::Ampacity {
        let bound = bind_board(board, lib);
        let evidence = report_inputs.evidence(&bound.report)?;
        let net_names: Vec<String> = board.nets.iter().map(|net| net.name.clone()).collect();
        return crate::reports::ampacity::emit_quiet(
            text,
            is_altium,
            &evidence,
            &net_names,
            cfg.plain,
            quiet,
            cfg.verbose,
        )
        .map(|()| true);
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if surface == SelectedSurface::Lint {
        return crate::reports::lint::emit_quiet(&report_inputs, mode, cfg.strict, quiet)
            .map(|()| true);
    }

    // --resources: run only the MCU internal resource-conflict check, print, exit.
    if surface == SelectedSurface::Resources {
        return crate::reports::lint::emit_resources_quiet(&report_inputs, mode, cfg.strict, quiet)
            .map(|()| true);
    }

    // --usb-c: run the USB-C CC attach classifier (the RPi 4 re-derivation) and
    // print the compliance report. The capability existed but was unreachable from
    // any user-facing surface; this is its CLI front door.
    if surface == SelectedSurface::UsbC {
        let bound = bind_board(board, lib);
        let evidence = report_inputs.evidence(&bound.report)?;
        let bind = crate::result::BindSummary::from_report(&bound.report);
        let blockers = crate::result::unmodelled_critical_refs(&bind);
        return crate::reports::usb_c::emit_quiet(
            board, &evidence, mode, cfg.strict, quiet, inputs, &bind, &blockers,
        )
        .map(|()| true);
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if surface == SelectedSurface::Si {
        return crate::reports::si::emit_quiet(&report_inputs, mode, cfg.strict, quiet)
            .map(|()| true);
    }

    // --ac: small-signal AC sweep on the bound circuit, print Bode + (optional)
    // loop-stability margins, then exit. Informational like the other reports.
    if surface == SelectedSurface::Ac {
        let ac_arg = cfg.ac.as_ref().expect("AC surface has an --ac value");
        // The overlay-applied bound board when --asbuilt was given, so the AC
        // sweep runs on the reworked circuit.
        let bound = prebound.take().unwrap_or_else(|| bind_board(board, lib));
        let evidence = report_inputs.evidence(&bound.report)?;
        return crate::reports::ac::emit_quiet(
            &bound,
            &evidence,
            ac_arg,
            &cfg.ac_node,
            cfg.ac_csv.as_deref(),
            cfg.ac_loop.as_deref(),
            cfg.json,
            quiet,
            inputs,
        )
        .map(|()| true);
    }

    // Bare `--json` with no specific report selector: emit a COMBINED machine
    // report (bind + DRC + lint/straps/resources + SI) and exit. Without this,
    // `--json` alone falls through to the TUI/websocket default below and hangs a
    // piped / CI / AI caller (the regression a bare `run <board> --json` hit).
    // `--json` is an explicit machine-intent flag, so it must never launch the TUI.
    // `--thermal`/`--headless` are selectors handled further down with their OWN
    // JSON emitters (thermal coverage, co-sim notes); they must fall THROUGH this
    // combined branch or those JSON paths become unreachable dead code.
    if surface == SelectedSurface::BareJson {
        return crate::reports::check::emit_combined_json_with_schematic(
            &report_inputs,
            cfg.strict,
            schematic_ties.as_ref(),
        )
        .map(|()| true);
    }

    Ok(false)
}
