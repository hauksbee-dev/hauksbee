//! The TUI driver: terminal lifecycle (alt-screen + raw mode, with a panic hook
//! that always restores the terminal), the event loop, and the [`run`]
//! entrypoint that builds the [`AppState`] from the SAME analysis the
//! `--json`/text paths use, then renders it.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use super::cosim::{self, CosimHandle, CosimUpdate};
use super::render;
use super::state::{AppState, Net, Pane};
use crate::binder::bind_board;
use crate::result::{
    lint_findings_json, si_findings_json, BindSummary, DrcStructured,
};

/// Co-sim target duration (s) when launched from the TUI.
const COSIM_SECONDS: f64 = 2.0;

/// Launch the interactive TUI for a board. `board_path` is the file on disk (for
/// firmware auto-detection and naming); `board_text` is its already-read text
/// (so we reuse the exact bytes the caller validated); `models_dir` layers an
/// extra model directory exactly as the CLI does; `firmware` is an optional
/// explicit ELF/HEX for co-sim.
pub fn run(
    board_path: &Path,
    board_text: &str,
    models_dir: Option<&Path>,
    firmware: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Build the model on the SAME analysis path the --json/text surfaces use.
    let state = build_state(board_path, board_text, models_dir)?;

    // Firmware: explicit arg wins; otherwise auto-detect a sibling .elf.
    let firmware = firmware.or_else(|| cosim::autodetect_firmware(board_path));
    let chunk_ms = cosim::default_chunk_ms(state.backend.as_deref());
    let board_text = board_text.to_string();
    let board_name = state.board_name.clone();

    let mut term = setup_terminal()?;
    let res = event_loop(
        &mut term,
        state,
        board_text,
        board_name,
        firmware,
        chunk_ms,
    );
    restore_terminal(&mut term)?;
    res
}

/// Build the [`AppState`] from the structured-honest result. This calls the
/// SAME bind / SI / lint / DRC paths as the CLI (`--json`/text), then converts
/// to the TUI model. It never re-runs or re-implements a check.
pub fn build_state(
    board_path: &Path,
    board_text: &str,
    models_dir: Option<&Path>,
) -> anyhow::Result<AppState> {
    // A `.kicad_sch` references sibling sub-sheets, so it must load by path.
    let board = if board_path.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(board_path)?
    } else {
        ExtractedBoard::from_auto(board_text)?
    };
    let extra: Vec<&std::path::Path> = models_dir.into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);

    let bound = bind_board(&board, &lib);
    let summary = BindSummary::from_report(&bound.report);

    // DRC reads copper geometry from the board text (same as --drc / frontdoor).
    let drc = ExtractedBoard::drc(board_text).unwrap_or_default();
    let drc_structured = DrcStructured::from_report(&drc);

    // SI = the signal-integrity static checks (with geometry text). lint = the
    // exact `--lint` bundle via the single `engine_lint` chokepoint (net lint +
    // strap lint + MCU resource conflicts + the unchecked-strap-bearing-MCU
    // coverage note), so the TUI never prints a clean verdict over a strap-bearing
    // MCU whose BOOT0 was never examined.
    let si = board.si_checks(Some(board_text));
    let si_json = si_findings_json(&si);
    let lint = crate::checks::engine_lint(&board, &lib);
    let lint_json = lint_findings_json(&lint);

    // Per-net DC voltages: build a transient engine and step 0 to get the DC
    // operating point. This is the same engine the co-sim uses; a failure here
    // is non-fatal (we just show nets without voltages).
    let nets = dc_nets(board_text, &bound.net_names);

    // Connectivity maps for the part/net detail views: ref → connected net
    // names, and net name → connected refs. Built from the same ExtractedBoard
    // the binder used, so the detail can never disagree with the analysis.
    let (part_nets, net_parts) = connectivity(&board);

    Ok(AppState::new(
        bound.name.clone(),
        &bound.report,
        &summary,
        &drc_structured,
        &si_json,
        &lint_json,
        nets,
        part_nets,
        net_parts,
    ))
}

/// Build ref↔net adjacency from the extracted board: for each component, the
/// distinct names of the nets its pins sit on, and the inverse map.
fn connectivity(
    board: &ExtractedBoard,
) -> (
    std::collections::HashMap<String, Vec<String>>,
    std::collections::HashMap<String, Vec<String>>,
) {
    use std::collections::HashMap;
    // net id → name, skipping the unconnected net 0 and KiCad's per-pad
    // `unconnected-*` placeholder nets (an explicit no-connect, not real
    // connectivity — the binder filters these the same way).
    let net_name: HashMap<i64, &str> = board
        .nets
        .iter()
        .filter(|n| n.id != 0 && !is_unconnected_net(&n.name))
        .map(|n| (n.id, n.name.as_str()))
        .collect();

    let mut part_nets: HashMap<String, Vec<String>> = HashMap::new();
    let mut net_parts: HashMap<String, Vec<String>> = HashMap::new();
    for c in &board.components {
        for pin in &c.pins {
            let Some(id) = pin.net else { continue };
            let Some(&name) = net_name.get(&id) else {
                continue;
            };
            part_nets
                .entry(c.reference.clone())
                .or_default()
                .push(name.to_string());
            net_parts
                .entry(name.to_string())
                .or_default()
                .push(c.reference.clone());
        }
    }
    (part_nets, net_parts)
}

/// True for a KiCad `unconnected-*` placeholder net name (a pad's explicit
/// no-connect), matching the binder's `node_name(..).starts_with("unconnected-")`
/// convention.
fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches(['/', '+'])
        .trim_start_matches("net-(")
        .trim_start_matches("Net-(")
        .starts_with("unconnected-")
        || name.starts_with("unconnected-")
}

/// Build the net list with DC voltages from the operating point, falling back to
/// voltage-less nets if the engine can't be built.
fn dc_nets(board_text: &str, net_names: &[String]) -> Vec<Net> {
    use hauksbee_server::engine::Engine;
    let voltages = crate::engine::HauksbeeEngine::from_board_file(board_text, None, "/dc")
        .ok()
        .map(|mut e| {
            // One zero-length step settles the DC operating point.
            let frame = e.step(0.0);
            frame.net_voltages
        })
        .unwrap_or_default();
    net_names
        .iter()
        .map(|n| Net {
            name: n.clone(),
            voltage_v: voltages.get(n).copied(),
        })
        .collect()
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Drain any key-press events already sitting in the input queue without acting
/// on them. Used right after a modal close so a rapidly-queued trailing key
/// (the `r` in an `Esc r` burst) can't leak through to a top-level action.
fn drain_pending_keys() -> anyhow::Result<()> {
    while event::poll(Duration::from_millis(0))? {
        // Consume whatever is queued (key or otherwise) and discard it.
        let _ = event::read()?;
    }
    Ok(())
}

/// Enter raw mode + alt-screen and install a panic hook that ALWAYS restores the
/// terminal first, so a panic never leaves the user in a broken raw terminal.
fn setup_terminal() -> anyhow::Result<Term> {
    // Install the panic hook before we touch the terminal so an early panic is
    // covered too.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let term = Terminal::new(backend)?;
    Ok(term)
}

/// Restore the terminal: leave alt-screen, disable raw mode, show the cursor.
fn restore_terminal(term: &mut Term) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn event_loop(
    term: &mut Term,
    mut state: AppState,
    board_text: String,
    board_name: String,
    firmware: Option<PathBuf>,
    chunk_ms: f64,
) -> anyhow::Result<()> {
    let mut cosim: Option<CosimHandle> = None;
    let mut last_update: Option<CosimUpdate> = None;

    loop {
        // Drain any pending co-sim updates (non-blocking).
        if let Some(h) = &cosim {
            while let Ok(u) = h.rx.try_recv() {
                let done = u.done;
                // Surface the chip-substitution caveat in AppState so both the
                // idle and live cosim views show it (parity with CLI/web).
                if let Some(sub) = &u.substitution {
                    state.set_chip_substitution(sub.clone());
                }
                last_update = Some(u);
                if done {
                    cosim = None;
                    break;
                }
            }
        }

        let running = cosim.is_some();
        term.draw(|f| render::draw(f, &state, last_update.as_ref(), running))?;

        // Poll for input with a short timeout so the co-sim stream animates.
        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // ── Overlay-first input handling ─────────────────────────────────────
        // When a detail overlay is open it is MODAL: it must consume every key,
        // so a rapidly-queued key can never leak through to an action (e.g. `r`
        // starting a co-sim under the modal). Close on Esc/Enter/q; swallow the
        // rest. This is also the fix for the `Esc`-then-`r` keystroke-queuing
        // exit: many terminals encode a quick `Esc r` as a single Alt+r event
        // (Esc as the meta prefix), so an Alt-modified key while a modal is open
        // is treated as the Esc that was meant to close it — not the action.
        if state.any_overlay_open() {
            // Two ways to close: an explicit close key (Esc/Enter/q), or an
            // Alt-modified key — many terminals encode a quick `Esc <key>` burst
            // as a single Alt+<key> event (Esc as the meta prefix), so an
            // Alt-modified key while a modal is up IS the Esc that was meant to
            // close it, not the action the trailing byte would otherwise run.
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let is_close_key =
                matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'));
            if is_close_key || alt {
                state.close_overlays();
                // A quick `Esc <key>` can also arrive as TWO queued events (not
                // one Alt-modified event) on some terminals. If we just
                // `continue`d here, the trailing `r`/`q`/`Tab` would reach the
                // top-level dispatch on the next loop and fire an action (or
                // quit) the instant the modal closed. Drain any already-queued
                // key presses so the close consumes the whole burst.
                drain_pending_keys()?;
            }
            // Anything else is swallowed: the overlay is modal.
            continue;
        }

        // ── Top-level input ──────────────────────────────────────────────────
        // Only `q` (and Ctrl-C) ever quit. Esc at the top level is a no-op (it
        // must NEVER quit the app).
        match key.code {
            KeyCode::Esc => {} // explicit: top-level Esc does nothing
            KeyCode::Char('q') => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Tab | KeyCode::Right => state.focus_next(),
            KeyCode::BackTab | KeyCode::Left => state.focus_prev(),
            KeyCode::Down | KeyCode::Char('j') => state.select_down(),
            KeyCode::Up | KeyCode::Char('k') => state.select_up(),
            KeyCode::Enter => state.activate(),
            KeyCode::Char('r') => {
                if state.no_mcu {
                    // No firmware to co-simulate — `r` just surfaces the pane's
                    // static-analysis message rather than faking a run.
                    state.focus = Pane::Cosim;
                } else if let Some(h) = &cosim {
                    h.stop();
                    cosim = None;
                } else {
                    // Start the co-sim worker.
                    state.focus = Pane::Cosim;
                    last_update = Some(CosimUpdate {
                        chunk_ms,
                        ..Default::default()
                    });
                    cosim = Some(cosim::spawn(
                        board_text.clone(),
                        firmware.clone(),
                        board_name.clone(),
                        COSIM_SECONDS,
                        chunk_ms,
                    ));
                }
            }
            _ => {}
        }

        if state.should_quit {
            break;
        }
    }
    // Stop any running co-sim before we tear down the terminal.
    drop(cosim);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Component, Net as ExtractNet, Pin};

    fn pin(number: &str, net: Option<i64>) -> Pin {
        Pin {
            number: number.to_string(),
            net,
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }

    #[test]
    fn unconnected_placeholder_nets_are_recognised() {
        assert!(is_unconnected_net("unconnected-(U1-PadA)"));
        assert!(is_unconnected_net("/unconnected-(U1-PadA)"));
        assert!(is_unconnected_net("Net-(unconnected-U1-1)"));
        assert!(!is_unconnected_net("/VBUS"));
        assert!(!is_unconnected_net("GND"));
        assert!(!is_unconnected_net("Net-(U1-REXT)"));
    }

    #[test]
    fn connectivity_skips_net0_and_unconnected_placeholders() {
        let board = ExtractedBoard {
            name: "t".into(),
            nets: vec![
                ExtractNet { id: 0, name: "".into() },
                ExtractNet { id: 1, name: "GND".into() },
                ExtractNet { id: 2, name: "VBUS".into() },
                ExtractNet { id: 9, name: "unconnected-(U1-NC)".into() },
            ],
            components: vec![Component {
                reference: "U1".into(),
                value: "BQ".into(),
                lib_id: String::new(),
                footprint: String::new(),
                position: None,
                layer: String::new(),
                properties: vec![],
                dnp: false,
                pins: vec![
                    pin("1", Some(2)),    // VBUS
                    pin("2", Some(1)),    // GND
                    pin("3", Some(9)),    // unconnected placeholder -> dropped
                    pin("4", Some(0)),    // net 0 -> dropped
                    pin("5", None),       // no net -> dropped
                ],
            }],
        };
        let (part_nets, net_parts) = connectivity(&board);
        let mut u1 = part_nets.get("U1").cloned().unwrap_or_default();
        u1.sort();
        assert_eq!(u1, vec!["GND".to_string(), "VBUS".to_string()]);
        assert!(net_parts.contains_key("VBUS"));
        assert!(!net_parts.contains_key("unconnected-(U1-NC)"));
    }
}
