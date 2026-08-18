//! Completes the browser-serving path after live-engine construction. It emits
//! the non-terminal fallback, applies undisclosed copper shorts, builds the
//! preloaded startup report, preserves the original board bytes for resumable
//! sessions, and hands the configured engine to the shared server.

use super::{
    prepare::{RunArtifacts, RunInputs},
    simulation::SimulationContext,
    RunConfig,
};

use crate::commands::run::preloaded_board_file;
use hauksbee_extract::ExtractedBoard;

pub(crate) fn emit_or_serve(
    cfg: &RunConfig,
    run_inputs: &RunInputs,
    artifacts: &RunArtifacts,
    sim: SimulationContext,
    stdout_is_tty: bool,
) -> anyhow::Result<()> {
    let board = &run_inputs.board;
    let raw = &run_inputs.raw;
    let text = &run_inputs.text;
    let input_kind = run_inputs.input_kind;
    let is_altium = run_inputs.is_altium;
    let is_board_code = run_inputs.is_board_code;
    let schematic_ties = &artifacts.schematic_ties;
    let mut engine = sim.engine;
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
                "  For a report add a flag: --check (all static checks) · --report (what was modelled) · --check --plain (prose) · --json (machine); or --serve for the browser UI."
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
        let qualification = schematic_ties
            .as_ref()
            .map(|ties| ties.qualify(&drc_report));
        engine
            .apply_and_disclose_drc_shorts_with_qualification(&drc_report, qualification.as_ref());
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
                Ok(bytes) => crate::frontdoor::analyze_with_firmware_json_with_ties(
                    &file_name,
                    report_bytes,
                    &fw_name,
                    &bytes,
                    schematic_ties.as_ref(),
                ),
                // Firmware was already path-validated above; a read error here is
                // unexpected, so fall back to the board-only report rather than fail.
                Err(_) => crate::frontdoor::analyze_json_with_ties(
                    &file_name,
                    report_bytes,
                    schematic_ties.as_ref(),
                ),
            }
        }
        // The preloaded browser report must read the same as `--drc` on the same
        // path, so it gets the companion schematic this run already resolved.
        None => crate::frontdoor::analyze_json_with_ties(
            &file_name,
            report_bytes,
            schematic_ties.as_ref(),
        ),
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
        "avr": cfg!(feature = "avr"),
        // Engine version, for the Environment page's "what am I running" card.
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string();

    let served_board_file = preloaded_board_file(&board_url, &raw, input_kind, &text);
    crate::commands::common::serve(
        engine,
        cfg.port,
        Some(served_board_file),
        startup_json,
        cfg.open,
        cfg.no_open,
    )
}
