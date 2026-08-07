//! ratatui rendering of the TUI's four-pane + footer layout. This module is the
//! only place that touches ratatui widgets: it reads the pure [`AppState`] from
//! [`super::state`] and draws it, keeping rendering isolated from state so the
//! state stays testable without a terminal. Focus and selection also carry ASCII
//! markers so they read in colour-stripped captures.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::cosim::CosimUpdate;
use super::state::{
    downsample, AppState, LeftDetail, Level, Pane, PartStatus, ScopeView, Severity,
    TUI_LAUNCH_BANNER,
};

/// The ASCII marker prefixed to the focused pane's title, so focus is legible
/// without colour (the personas were navigating "blind" in colour-stripped
/// captures). Unfocused panes get a space so titles stay aligned.
fn pane_title(label: &str, pane: Pane, focus: Pane) -> String {
    if pane == focus {
        format!("▶ {label} ")
    } else {
        format!("  {label} ")
    }
}

/// The leading cursor marker for a list row: "▸ " on the selected row, two
/// spaces otherwise. Gives the selection an ASCII glyph on top of the
/// reverse-video highlight, so it's visible in a colour-stripped terminal.
fn row_marker(selected: bool) -> &'static str {
    if selected {
        "▸ "
    } else {
        "  "
    }
}

/// Wall-clock seconds of co-sim with no GPIO and no UART activity before the
/// pane shows the "may be waiting on a peripheral" stall note. Generous enough
/// to cover a slow QEMU/Renode boot, short enough that a truly stalled firmware
/// doesn't look frozen for long.
const STALL_AFTER_WALL_S: f64 = 4.0;

/// Detail-overlay size as a percentage of the frame, shared by both the
/// findings and the part/net overlays so they stay the same size.
const OVERLAY_PCT_X: u16 = 70;
const OVERLAY_PCT_Y: u16 = 60;

/// The two-line note appended under the stall-honesty headline, pointing the
/// user at the full-sensor co-sim path. Constant because it never varies with
/// run state.
const STALL_HINT_L1: &str = "TUI co-sim does not attach declarative sensor models; use";
const STALL_HINT_L2: &str = "`hauksbee-ci run <spec>` for full sensor co-sim.";

/// Colour for a severity, per the brief: serious=red, medium=amber, info=dim.
fn severity_style(sev: Severity) -> Style {
    match sev {
        Severity::Serious => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Severity::Medium => Style::default().fg(Color::Yellow),
        Severity::Info => Style::default().fg(Color::DarkGray),
    }
}

/// The selected row's highlight: one solid background band. `Modifier::REVERSED`
/// inverts each span against its OWN colour, so a multi-coloured row (or a
/// wrapped finding) breaks into per-word inverse fragments and the blank tail of
/// the row flashes white. A plain background patches only `bg`, leaving every
/// span's foreground intact, so the selection reads as one continuous block
/// across the full pane width on every wrapped line.
fn selection_style() -> Style {
    Style::default().bg(Color::Indexed(24))
}

fn focused_border(pane: Pane, focus: Pane) -> Style {
    if pane == focus {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Draw the whole UI for one frame.
pub fn draw(f: &mut Frame, state: &AppState, cosim: Option<&CosimUpdate>, cosim_running: bool) {
    // The launch banner takes a single top line until the first keypress dismisses
    // it; once dismissed it costs no rows, so the steady-state layout is unchanged.
    let banner_rows = if state.banner_dismissed { 0 } else { 1 };
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(banner_rows),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(f.area());

    draw_identity_bar(f, root[0], state);
    if banner_rows > 0 {
        draw_banner(f, root[1]);
    }

    let body = root[2];
    if state.no_mcu {
        // Nothing to co-simulate and nothing to trace, so the two live-signal
        // panes stop paying for a full box: each becomes a one-line stub and the
        // height goes to the panes that actually carry this board's content.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(body);
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(rows[0]);
        draw_parts(f, panes[0], state);
        draw_findings(f, panes[1], state);
        draw_pane_stub(
            f,
            rows[1],
            "co-sim",
            "no MCU on this board",
            Pane::Cosim,
            state.focus,
        );
        draw_pane_stub(
            f,
            rows[2],
            "scope",
            "no live signals without an MCU",
            Pane::Scope,
            state.focus,
        );
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(24),
                Constraint::Percentage(32),
                Constraint::Percentage(22),
                Constraint::Percentage(22),
            ])
            .split(body);

        draw_parts(f, panes[0], state);
        draw_findings(f, panes[1], state);
        draw_cosim(f, panes[2], state, cosim);
        draw_scope(f, panes[3], state, cosim_running);
    }
    draw_footer(f, root[3], state, cosim_running);

    if state.detail_open {
        draw_detail_overlay(f, state);
    }
    if state.left_detail_open {
        draw_left_detail_overlay(f, state);
    }
}

/// The persistent identity bar: which board this is, what it is bound to, and
/// how much it found. It never scrolls away and never dismisses, so the answer to
/// "what am I looking at" is always on screen rather than only in the bottom bar
/// (k9s keeps context at the top for the same reason).
fn draw_identity_bar(f: &mut Frame, area: Rect, state: &AppState) {
    let mut spans = vec![Span::styled(
        truncate(&state.board_label, 32),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    match (state.mcu.as_deref(), state.backend.as_deref()) {
        (Some(mcu), Some(backend)) => {
            spans.push(Span::styled(
                format!(" · {mcu}"),
                Style::default().fg(Color::Magenta),
            ));
            spans.push(Span::styled(
                format!(" · {backend}"),
                Style::default().fg(Color::Cyan),
            ));
        }
        (Some(mcu), None) => spans.push(Span::styled(
            format!(" · {mcu}"),
            Style::default().fg(Color::Magenta),
        )),
        _ => spans.push(Span::styled(" · no MCU", Style::default().fg(Color::Gray))),
    }

    let v = &state.verdict;
    spans.push(Span::styled(
        format!(" · {} worth attention", v.worth_attention),
        if v.worth_attention > 0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        },
    ));
    spans.push(Span::styled(
        format!(" · {} notes", v.grouped_notes),
        Style::default().fg(Color::Gray),
    ));
    spans.push(Span::styled(
        format!(" · {} serious", v.serious),
        if v.serious > 0 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        },
    ));

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Indexed(236))),
        area,
    );
}

/// A collapsed pane: one dim line where a full bordered box would otherwise sit
/// empty. Still focusable, so Tab order and the key hints are unchanged; it just
/// costs one row instead of a quarter of the screen.
fn draw_pane_stub(f: &mut Frame, area: Rect, label: &str, text: &str, pane: Pane, focus: Pane) {
    let focused = pane == focus;
    let line = Line::from(vec![
        Span::raw(if focused { "▶ " } else { "  " }),
        Span::styled(
            format!("{label}: "),
            if focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        ),
        Span::styled(text.to_string(), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// The dismissible one-line launch banner. Dim, borderless, and truncated to the
/// terminal width; it points a first-time user at the non-TUI report surfaces
/// and disappears on the first keypress.
fn draw_banner(f: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        truncate(TUI_LAUNCH_BANNER, area.width as usize),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(line), area);
}

fn draw_parts(f: &mut Frame, area: Rect, state: &AppState) {
    let mut items: Vec<ListItem> = Vec::new();
    // Parts section.
    for (i, p) in state.parts.iter().enumerate() {
        let selected = state.focus == Pane::Parts && state.parts_sel == i;
        let (mark, style) = match p.status {
            PartStatus::Bound => ("bound ", Style::default().fg(Color::Green)),
            PartStatus::Family => ("family", Style::default().fg(Color::Cyan)),
            // Reserve alarming bold-red for genuinely-unmodelled SILICON on the live
            // circuit (critical_open). A part that is merely unresolved but not a
            // live active IC (a passive, a part off the live net, open-by-design) is
            // calmed to a dim "open" so the pane is not a wall of red on first sight.
            PartStatus::Unresolved if p.critical_open => (
                "UNRES ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            PartStatus::Unresolved => ("open  ", Style::default().fg(Color::Yellow)),
            PartStatus::Ignored => ("ignore", Style::default().fg(Color::DarkGray)),
        };
        let crit = if p.critical_open {
            Span::styled(
                " ‼",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        } else if p.active_ic {
            Span::styled(" ●", Style::default().fg(Color::Magenta))
        } else {
            Span::raw("")
        };
        let line = Line::from(vec![
            Span::raw(row_marker(selected)),
            Span::styled(format!("{mark} "), style),
            Span::raw(format!("{:<6}", p.reference)),
            Span::styled(
                format!(" {}", truncate(&p.value, 12)),
                Style::default().fg(Color::Gray),
            ),
            crit,
        ]);
        items.push(ListItem::new(line));
    }
    // Nets section header + rows.
    items.push(ListItem::new(Line::from(Span::styled(
        "── nets ──",
        Style::default().fg(Color::DarkGray),
    ))));
    for (i, n) in state.nets.iter().enumerate() {
        let selected = state.focus == Pane::Parts && state.parts_sel == state.parts.len() + i;
        let v = match n.voltage_v {
            Some(v) => Span::styled(format!("{v:>7.3} V"), Style::default().fg(Color::LightBlue)),
            None => Span::styled("    -   ", Style::default().fg(Color::DarkGray)),
        };
        // Probed-net indicator: nets on the scope get a "◉" in their series
        // colour, so the list shows what the scope is tracing (and which trace
        // is whose). Unprobed nets get a space to keep the columns aligned.
        let probe_mark = match state.scope.probed().iter().position(|p| p == &n.name) {
            Some(idx) => Span::styled("◉ ", Style::default().fg(series_color(idx))),
            None => Span::raw("  "),
        };
        items.push(ListItem::new(Line::from(vec![
            Span::raw(row_marker(selected)),
            probe_mark,
            Span::raw(format!("{:<14}", truncate(&n.name, 14))),
            v,
        ])));
    }

    let block = Block::default()
        .title(pane_title(
            &format!("{} parts · {} nets", state.parts.len(), state.nets.len()),
            Pane::Parts,
            state.focus,
        ))
        .borders(Borders::ALL)
        .border_style(focused_border(Pane::Parts, state.focus));

    let mut ls = ListState::default();
    // The net-section header sits between parts and nets, so a selection in the
    // nets region is offset by one for the header row.
    let display_sel = if state.parts_sel < state.parts.len() {
        state.parts_sel
    } else {
        state.parts_sel + 1
    };
    ls.select(Some(display_sel));

    let list = List::new(items)
        .block(block)
        .highlight_style(selection_style());
    f.render_stateful_widget(list, area, &mut ls);
}

fn draw_findings(f: &mut Frame, area: Rect, state: &AppState) {
    // Verdict line (rendered inside the bordered area's top).
    let block = Block::default()
        .title(pane_title(
            "Findings (triaged)",
            Pane::Findings,
            state.focus,
        ))
        .borders(Borders::ALL)
        .border_style(focused_border(Pane::Findings, state.focus));
    let inner = block.inner(area);
    let inner_w = inner.width as usize;
    f.render_widget(block, area);

    // The verdict band needs an extra row when the unresolved-IC honesty warning
    // is present (so a falsely-reassuring "0 worth attention" can't stand alone
    // when active ICs are open).
    let warning = state.unresolved_warning();
    let verdict_rows = if warning.is_some() { 3 } else { 2 };
    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(verdict_rows), Constraint::Min(1)])
        .split(inner);

    let v = &state.verdict;
    let mut verdict_lines = vec![Line::from(Span::styled(
        v.headline(),
        Style::default()
            .fg(if v.serious > 0 {
                Color::Red
            } else if v.worth_attention > 0 {
                Color::Yellow
            } else {
                Color::Green
            })
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(w) = &warning {
        verdict_lines.push(Line::from(Span::styled(
            w.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let verdict = Paragraph::new(verdict_lines).wrap(Wrap { trim: true });
    f.render_widget(verdict, inner_chunks[0]);

    let mut items: Vec<ListItem> = Vec::new();
    for (i, fdg) in state.findings.iter().enumerate() {
        let selected = state.focus == Pane::Findings && state.findings_sel == i;
        let st = severity_style(fdg.severity);
        let marker_txt = row_marker(selected);
        let sev_txt = format!("[{}] ", fdg.severity.label());
        let kind_txt = format!("{}/{} ", fdg.check, fdg.kind);
        let prefix_w =
            marker_txt.chars().count() + sev_txt.chars().count() + kind_txt.chars().count();
        let avail = inner_w.saturating_sub(prefix_w).max(1);
        let head = |s: &str| {
            Line::from(vec![
                Span::raw(marker_txt),
                Span::styled(sev_txt.clone(), st),
                Span::styled(kind_txt.clone(), Style::default().fg(Color::DarkGray)),
                Span::styled(s.to_string(), st),
            ])
        };
        if selected {
            // The highlighted finding EXPANDS to its full headline, wrapped, so the
            // key numbers (e.g. "Zdiff ~171 ohm vs 90 ohm") are visible in place
            // without opening the modal. Others stay truncated to one row.
            let wrapped = wrap_words(&fdg.headline, avail);
            let mut lines: Vec<Line> = Vec::new();
            lines.push(head(wrapped.first().map(String::as_str).unwrap_or("")));
            let indent = " ".repeat(prefix_w);
            for cont in wrapped.iter().skip(1) {
                lines.push(Line::from(Span::styled(format!("{indent}{cont}"), st)));
            }
            items.push(ListItem::new(lines));
        } else {
            items.push(ListItem::new(head(&truncate(&fdg.headline, avail))));
        }
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No findings: the static checks found nothing worth flagging.",
            Style::default().fg(Color::Green),
        ))));
    }

    let mut ls = ListState::default();
    ls.select(Some(
        state
            .findings_sel
            .min(state.findings.len().saturating_sub(1)),
    ));
    let list = List::new(items).highlight_style(selection_style());
    f.render_stateful_widget(list, inner_chunks[1], &mut ls);
}

fn draw_cosim(f: &mut Frame, area: Rect, state: &AppState, cosim: Option<&CosimUpdate>) {
    let block = Block::default()
        .title(pane_title("Co-sim", Pane::Cosim, state.focus))
        .borders(Borders::ALL)
        .border_style(focused_border(Pane::Cosim, state.focus));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    // A board with no MCU / no firmware target has no co-sim to run; the pane
    // is a static-analysis surface, not a firmware co-sim. Say that plainly in
    // every state instead of the MCU-centric "press [r] to run" framing.
    if state.no_mcu {
        lines.push(Line::from(Span::styled(
            "No MCU / firmware on this board.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Static analysis only; there is no firmware to co-simulate.",
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(Span::styled(
            "DC operating-point voltages are in the Nets & Parts pane.",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    match cosim {
        None => {
            // The no-MCU case returned early above, so here there IS an MCU to
            // co-simulate, show the idle "press [r]" prompt.
            lines.push(Line::from(vec![
                Span::styled("idle: press ", Style::default().fg(Color::Gray)),
                Span::styled(
                    "[r]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to run co-sim", Style::default().fg(Color::Gray)),
            ]));
            lines.push(Line::from(Span::styled(
                "co-sim runs the supplied --firmware, else auto-detects from the board",
                Style::default().fg(Color::DarkGray),
            )));
            if let Some(b) = &state.backend {
                lines.push(Line::from(Span::styled(
                    format!("backend {b}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if let Some(sub) = &state.backend_substituted {
                lines.push(Line::from(Span::styled(
                    format!("chip substitution: {sub}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }
        Some(u) => {
            if let Some(e) = &u.error {
                lines.push(Line::from(Span::styled(
                    format!("error: {e}"),
                    Style::default().fg(Color::Red),
                )));
            } else {
                let status = if u.done {
                    Span::styled("done", Style::default().fg(Color::Green))
                } else {
                    Span::styled(
                        format!("running {}", spinner(u.wall_s)),
                        Style::default().fg(Color::Yellow),
                    )
                };
                let backend = state.backend.as_deref().unwrap_or("backend");
                lines.push(Line::from(vec![
                    Span::styled(format!("{backend}  "), Style::default().fg(Color::Cyan)),
                    status,
                ]));
                lines.push(Line::from(Span::styled(
                    format!(
                        "{:.0} sim-ms / {:.1} wall-s (chunk {:.0} ms)",
                        u.sim_ms, u.wall_s, u.chunk_ms
                    ),
                    Style::default().fg(Color::Gray),
                )));

                // ── GPIO / control-net state (the "LED") ──────────────────────
                // The width budget inside the pane (border already removed).
                let w = inner.width as usize;
                lines.push(Line::from(Span::styled(
                    "GPIO / control nets:",
                    Style::default().fg(Color::DarkGray),
                )));
                if u.gpio_nets.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  (no GPIO/output nets on this board)",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    // Name column scales with the pane; leave room for "  ", the
                    // voltage ("  0.00 V "), the level glyph and the drive tag.
                    let name_w = w.saturating_sub(22).clamp(6, 18);
                    for g in &u.gpio_nets {
                        let level = Level::from_volts(g.volts);
                        // Voltage-threshold label (not a drive direction): HIGH
                        // near a rail, LOW near 0 V, MID in between, so a 1.48 V
                        // node can never read as "low".
                        let glyph = format!("{} {:<4}", level.glyph(), level.word());
                        let lvl_color = match level {
                            Level::High => Color::Green,
                            Level::Mid => Color::Yellow,
                            Level::Low => Color::DarkGray,
                        };
                        // Driven vs undriven: a net that has moved off its boot
                        // baseline is one the MCU actively drove.
                        let (drv, drv_color) = if g.driven {
                            ("drv", Color::Cyan)
                        } else {
                            ("-  ", Color::DarkGray)
                        };
                        lines.push(Line::from(vec![
                            Span::raw(format!(
                                "  {:<width$}",
                                truncate(&g.name, name_w),
                                width = name_w
                            )),
                            Span::styled(
                                format!("{:>6.2} V {glyph} ", g.volts),
                                Style::default().fg(lvl_color),
                            ),
                            Span::styled(drv.to_string(), Style::default().fg(drv_color)),
                        ]));
                    }
                }

                // ── Chip-substitution honesty ─────────────────────────────────
                // If the board's MCU was modelled by a less-specific core, the
                // GPIO/UART results are from a substitute chip, say so in yellow
                // (parallel to the stall note) so they are never read as exact.
                if let Some(sub) = &state.backend_substituted {
                    lines.push(Line::from(Span::styled(
                        format!("chip substitution: {sub}"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )));
                }

                // ── Analog-validity honesty (05 §3b) ──────────────────────────
                // If the analog solve failed on any chunk, the GPIO/net levels
                // above were read off held-stale voltages. Say so LOUDLY in red so
                // they are never trusted, mirroring the CLI --json analog_valid
                // and the web report's prepended finding. Gated on the failed-
                // chunk count (0 on a clean run) so a default/idle snapshot never
                // trips it.
                if u.failed_chunk_count > 0 {
                    let chunk_word = if u.failed_chunk_count == 1 {
                        "chunk"
                    } else {
                        "chunks"
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "analog solve FAILED on {} {chunk_word}: net levels above are held-stale, not trustworthy",
                            u.failed_chunk_count
                        ),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::styled(
                        "electrical results in the failed windows are fiction (analog_valid=false); see --json for the exact spans",
                        Style::default().fg(Color::Red),
                    )));
                }

                // SPI framing honesty: a bus whose transaction boundaries are
                // guessed at chunk edges (no CS pin resolved, backend does not
                // frame) can merge or truncate transactions; say so instead of
                // presenting its traffic as exactly framed.
                if !u.heuristic_spi_buses.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "SPI framing is heuristic on [{}]: transaction boundaries are guessed at chunk edges (wire cs_net in the spec for exact framing)",
                            u.heuristic_spi_buses.join(", ")
                        ),
                        Style::default().fg(Color::Yellow),
                    )));
                }

                // ── Stall honesty ─────────────────────────────────────────────
                // If the firmware never drove any GPIO and never printed
                // anything, say so plainly rather than freezing silently. While
                // running we wait out a boot window; on completion we show it
                // unconditionally (a finished run that produced nothing is the
                // clearest stall of all, keep the pane honest, don't blank it).
                let quiet = !u.gpio_active && !u.gpio_driven && !u.uart_seen;
                if quiet && (u.done || u.wall_s > STALL_AFTER_WALL_S) {
                    let head = if u.done {
                        "no GPIO/UART activity; firmware never drove a peripheral."
                    } else {
                        "no GPIO/UART activity yet; firmware may be waiting on a peripheral."
                    };
                    lines.push(Line::from(Span::styled(
                        head,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::styled(
                        STALL_HINT_L1,
                        Style::default().fg(Color::Yellow),
                    )));
                    lines.push(Line::from(Span::styled(
                        STALL_HINT_L2,
                        Style::default().fg(Color::Yellow),
                    )));
                }

                // ── UART tail ─────────────────────────────────────────────────
                lines.push(Line::from(Span::styled(
                    "UART:",
                    Style::default().fg(Color::DarkGray),
                )));
                if u.uart_lines.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  (no UART output)",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    // Wrap each UART line to the pane width at a fixed 2-space
                    // indent, then show the last N wrapped rows that fit. This is
                    // what fixes the garble: no hard mid-codepoint truncation, no
                    // overflow past the pane, clean wrapping. The width is the
                    // pane minus the indent (min 1) so a wrapped row can never
                    // exceed the pane and trigger a second ratatui wrap (which
                    // would break the height math below).
                    let text_w = w.saturating_sub(2).max(1);
                    let mut wrapped: Vec<String> = Vec::new();
                    for l in &u.uart_lines {
                        wrap_into(l, text_w, &mut wrapped);
                    }
                    let avail = (inner.height as usize).saturating_sub(lines.len()).max(1);
                    let start = wrapped.len().saturating_sub(avail);
                    for row in &wrapped[start..] {
                        lines.push(Line::from(Span::styled(
                            format!("  {row}"),
                            Style::default().fg(Color::White),
                        )));
                    }
                }
            }
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The per-series trace colour, by probe order. Four distinguishable hues for
/// the four probe slots; the same colour marks the net's "◉" in the list, so
/// list and trace are visually paired.
fn series_color(idx: usize) -> Color {
    const COLORS: [Color; 4] = [Color::Cyan, Color::Yellow, Color::Green, Color::Magenta];
    COLORS[idx % COLORS.len()]
}

/// The scope pane: probed nets' recent voltage history as stacked braille
/// sparklines (one mini-chart per net, at these pane widths separate y-scales
/// with a per-net label read far better than one shared multi-series chart).
/// Display-only: the sole interaction is the `p` probe-toggle in the list.
fn draw_scope(f: &mut Frame, area: Rect, state: &AppState, cosim_running: bool) {
    let probed = state.scope.probed().len();
    let title = if probed > 0 {
        format!("Scope ({probed} probed)")
    } else {
        "Scope".to_string()
    };
    let block = Block::default()
        .title(pane_title(&title, Pane::Scope, state.focus))
        .borders(Borders::ALL)
        .border_style(focused_border(Pane::Scope, state.focus));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Placeholder states: say plainly what the pane needs, never an empty box.
    let placeholder = match state.scope_view() {
        ScopeView::NoMcu => Some(vec![
            Line::from(Span::styled(
                "no live signals; run with firmware/co-sim",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "this board has no MCU: static analysis only.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "DC voltages are in the Nets & Parts pane.",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        ScopeView::NoProbes => Some(vec![
            Line::from(Span::styled(
                "no nets probed",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "press [p] on a net in Nets & Parts",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                format!(
                    "(up to {} nets; oldest is dropped)",
                    super::state::SCOPE_MAX_PROBES
                ),
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        ScopeView::NoData => {
            let hint = if cosim_running {
                "co-sim running, waiting for the first samples…"
            } else {
                "no live signals; press [r] to run co-sim"
            };
            let mut lines = vec![Line::from(Span::styled(
                hint,
                Style::default().fg(Color::Gray),
            ))];
            for (i, name) in state.scope.probed().iter().enumerate() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "◉ {}",
                        truncate(name, inner.width.saturating_sub(2) as usize)
                    ),
                    Style::default().fg(series_color(i)),
                )));
            }
            Some(lines)
        }
        ScopeView::Live => None,
    };
    if let Some(lines) = placeholder {
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    // Live traces: stack one mini-chart per probed net, splitting the pane
    // height evenly. Each mini = a one-line header (name · latest · min..max in
    // the series colour) over a braille canvas of the recent history.
    let probed_nets: Vec<&String> = state.scope.probed().iter().collect();
    let constraints: Vec<Constraint> = probed_nets
        .iter()
        .map(|_| Constraint::Ratio(1, probed_nets.len() as u32))
        .collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, name) in probed_nets.iter().enumerate() {
        let color = series_color(i);
        let cell = rows[i];
        if cell.height == 0 {
            continue;
        }
        let Some(series) = state.scope.series(name) else {
            continue;
        };

        // Header: "◉ NET 1.23V 0.00..3.30", compact so latest + min..max
        // survive a ~24-column pane; the net name yields first.
        let header = match (series.latest(), series.min_max()) {
            (Some(last), Some((lo, hi))) => {
                let stats = format!(" {last:.2}V {lo:.2}..{hi:.2}");
                let name_w = (cell.width as usize)
                    .saturating_sub(2 + stats.chars().count())
                    .max(4);
                format!("◉ {}{stats}", truncate(name, name_w))
            }
            _ => format!("◉ {} (no samples)", truncate(name, cell.width as usize)),
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(&header, cell.width as usize),
                Style::default().fg(color),
            ))),
            Rect { height: 1, ..cell },
        );

        let chart = Rect {
            y: cell.y + 1,
            height: cell.height.saturating_sub(1),
            ..cell
        };
        if chart.height == 0 || chart.width == 0 {
            continue;
        }

        // Downsample the history to the braille x-resolution (2 dots per cell)
        // and draw line segments between consecutive points. Y bounds pad the
        // observed range so a flat trace still sits mid-canvas instead of on an
        // edge.
        let volts = downsample(&series.voltages(), (chart.width as usize) * 2);
        let (lo, hi) = series.min_max().unwrap_or((0.0, 1.0));
        let pad = ((hi - lo) * 0.1).max(0.05);
        let (y_lo, y_hi) = (lo - pad, hi + pad);
        let n = volts.len();
        f.render_widget(
            Canvas::default()
                .marker(Marker::Braille)
                .x_bounds([0.0, (n.saturating_sub(1)).max(1) as f64])
                .y_bounds([y_lo, y_hi])
                .paint(move |ctx| {
                    if n == 1 {
                        // A single sample: draw a short flat tick so it's visible.
                        ctx.draw(&CanvasLine::new(0.0, volts[0], 1.0, volts[0], color));
                        return;
                    }
                    for w in volts.windows(2).enumerate() {
                        let (x, pair) = w;
                        ctx.draw(&CanvasLine::new(
                            x as f64,
                            pair[0],
                            (x + 1) as f64,
                            pair[1],
                            color,
                        ));
                    }
                }),
            chart,
        );
    }
}

fn draw_footer(f: &mut Frame, area: Rect, state: &AppState, running: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mcu = state.mcu.as_deref().unwrap_or("no MCU");
    let backend = state.backend.as_deref().unwrap_or("-");
    // On a no-MCU board there's nothing to co-sim, so drop the `r` hint rather
    // than advertise an action that can't run.
    let run_hint = if state.no_mcu {
        String::new()
    } else if running {
        "  r stop".to_string()
    } else {
        "  r run".to_string()
    };

    let line = Line::from(vec![
        Span::styled(
            format!("{} ", truncate(&state.board_name, 24)),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("· {mcu} "), Style::default().fg(Color::Magenta)),
        Span::styled(format!("· {backend} "), Style::default().fg(Color::Cyan)),
        Span::styled(
            format!("· bound {} ", state.critical_parts_bound),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("│ Tab/←→ pane  ↑↓ nav  Enter detail  p probe{run_hint}  q quit"),
            Style::default().fg(Color::Gray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), inner);
}

fn draw_detail_overlay(f: &mut Frame, state: &AppState) {
    let Some(fdg) = state.selected_finding() else {
        return;
    };
    let area = centered_rect(OVERLAY_PCT_X, OVERLAY_PCT_Y, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(
            " {} / {}: detail (Enter/Esc to close) ",
            fdg.check, fdg.kind
        ))
        .borders(Borders::ALL)
        .border_style(severity_style(fdg.severity).add_modifier(Modifier::BOLD));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("[{}] {}", fdg.severity.label(), fdg.headline),
        severity_style(fdg.severity),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "What it means:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(fdg.plain.clone()));
    if !fdg.nets.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Nets: {}", fdg.nets.join(", "))));
    }
    if !fdg.refs.is_empty() {
        lines.push(Line::from(format!("Refs: {}", fdg.refs.join(", "))));
    }
    if let Some(loc) = fdg.location_mm {
        lines.push(Line::from(format!(
            "Location: x={:.1} mm, y={:.1} mm{}",
            loc[0],
            loc[1],
            fdg.layer
                .as_ref()
                .map(|l| format!("  ({l})"))
                .unwrap_or_default()
        )));
    } else if let Some(layer) = &fdg.layer {
        lines.push(Line::from(format!("Layer: {layer}")));
    }
    if let Some(fix) = &fdg.fix {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "Suggested fix: ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(fix.clone()),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// The part/net detail overlay opened by Enter on the Nets&Parts pane; the
/// counterpart to the findings detail, so Enter is consistent across panes.
fn draw_left_detail_overlay(f: &mut Frame, state: &AppState) {
    let Some(detail) = state.left_detail_view() else {
        return;
    };
    let area = centered_rect(OVERLAY_PCT_X, OVERLAY_PCT_Y, f.area());
    f.render_widget(Clear, area);

    let (title, border_style, lines) = match detail {
        LeftDetail::Part {
            reference,
            value,
            status,
            became,
            active_ic,
            critical_open,
            nets,
        } => {
            let border = if critical_open {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            };
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{reference}  "),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(value.clone(), Style::default().fg(Color::Gray)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(status),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Model:  ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(became),
            ]));
            if critical_open {
                lines.push(Line::from(Span::styled(
                    "‼ Unresolved active IC on the live circuit; analog results on its nets are NOT trustworthy.",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
            } else if active_ic {
                lines.push(Line::from(Span::styled(
                    "● active IC",
                    Style::default().fg(Color::Magenta),
                )));
            }
            lines.push(Line::from(""));
            if nets.is_empty() {
                lines.push(Line::from(Span::styled(
                    "Nets: (no connected nets)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                lines.push(Line::from(format!("Connects {} net(s):", nets.len())));
                lines.push(Line::from(Span::styled(
                    nets.join(", "),
                    Style::default().fg(Color::LightBlue),
                )));
            }
            (
                format!(" part {reference}: detail (Enter/Esc to close) "),
                border,
                lines,
            )
        }
        LeftDetail::Net {
            name,
            voltage_v,
            parts,
        } => {
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            match voltage_v {
                Some(v) => {
                    let level = Level::from_volts(v);
                    lines.push(Line::from(vec![
                        Span::styled(
                            "DC voltage: ",
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{v:.3} V  {} {}", level.glyph(), level.word()),
                            Style::default().fg(Color::LightBlue),
                        ),
                    ]));
                }
                None => lines.push(Line::from(Span::styled(
                    "DC voltage: (none; solver produced no operating point)",
                    Style::default().fg(Color::DarkGray),
                ))),
            }
            lines.push(Line::from(""));
            if parts.is_empty() {
                lines.push(Line::from(Span::styled(
                    "Parts: (no connected parts)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                lines.push(Line::from(format!("Connects {} part(s):", parts.len())));
                lines.push(Line::from(Span::styled(
                    parts.join(", "),
                    Style::default().fg(Color::Cyan),
                )));
            }
            (
                format!(" net {name}: detail (Enter/Esc to close) "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                lines,
            )
        }
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn spinner(t: f64) -> char {
    const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    FRAMES[((t * 8.0) as usize) % FRAMES.len()]
}

/// Hard-wrap `s` into chunks of at most `width` characters, pushing each chunk
/// into `out`. An empty input yields a single empty row (so a blank UART line
/// still occupies a row). Counts by character, never splitting a codepoint,
/// which is what keeps multibyte UART bytes from garbling the pane.
fn wrap_into(s: &str, width: usize, out: &mut Vec<String>) {
    let width = width.max(1);
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        out.push(String::new());
        return;
    }
    let mut i = 0;
    while i < chars.len() {
        let end = (i + width).min(chars.len());
        out.push(chars[i..end].iter().collect());
        i = end;
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Word-wrap `s` into lines no wider than `width`. A single word longer than
/// `width` is hard-split so it never overflows the pane. Used to expand the
/// highlighted finding to its full text in place.
fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let mut word = word.to_string();
        // Hard-split an over-long token.
        while word.chars().count() > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let head: String = word.chars().take(width).collect();
            lines.push(head);
            word = word.chars().skip(width).collect();
        }
        let sep = usize::from(!cur.is_empty());
        if cur.chars().count() + sep + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{BindOutcome, BindReport, BindRow};
    use crate::result::{BindSummary, DrcStructured};
    use hauksbee_models::Confidence;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashMap;

    /// A minimal MCU board state with two nets, for driving the real `draw`.
    fn scope_sample_state() -> AppState {
        let mut report = BindReport::default();
        report.push(BindRow {
            reference: "U1".into(),
            value: "STM32".into(),
            model_id: None,
            confidence: Confidence::Exact,
            source: None,
            outcome: BindOutcome::Mcu {
                backend: "renode:stm32f103".into(),
            },
            warning: None,
            guesses: Vec::new(),
        });
        let summary = BindSummary::from_report(&report);
        let drc = DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 0,
            shorts: Vec::new(),
            violations: Vec::new(),
            at_limit: Vec::new(),
            version_warning: None,
        };
        AppState::new(
            "ScopeDemo".into(),
            &report,
            &summary,
            &drc,
            &[],
            &[],
            vec![
                crate::tui::state::Net {
                    name: "/LED".into(),
                    voltage_v: Some(0.0),
                },
                crate::tui::state::Net {
                    name: "/PA5".into(),
                    voltage_v: Some(3.3),
                },
            ],
            HashMap::new(),
            HashMap::new(),
        )
    }

    /// Render one frame of the full TUI into a TestBackend and return the
    /// buffer's rows as strings (the poor man's snapshot).
    fn render_rows(state: &AppState, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, state, None, false)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn scope_pane_renders_braille_traces_for_probed_nets() {
        let mut st = scope_sample_state();
        st.dismiss_banner();
        // Probe both nets and feed a blink-ish history through the SAME path the
        // event loop uses (ScopeState::record on a per-net voltage snapshot).
        st.scope.toggle("/LED");
        st.scope.toggle("/PA5");
        for i in 0..200 {
            let mut v = HashMap::new();
            v.insert(
                "/LED".to_string(),
                if (i / 20) % 2 == 0 { 0.0 } else { 3.3 },
            );
            v.insert("/PA5".to_string(), 3.3);
            st.scope.record(i as f64 * 5.0, &v);
        }
        assert_eq!(st.scope_view(), ScopeView::Live);

        let rows = render_rows(&st, 120, 30);
        let all = rows.join("\n");
        // The pane exists, titled with the probe count.
        assert!(all.contains("Scope (2 probed)"), "scope pane title:\n{all}");
        // Both series headers: name, latest value, min..max window.
        assert!(all.contains("◉ /LED"), "LED header:\n{all}");
        assert!(all.contains("◉ /PA5"), "PA5 header:\n{all}");
        assert!(all.contains("0.00..3.30"), "LED min..max window:\n{all}");
        assert!(all.contains("3.30V"), "latest value shown:\n{all}");
        // Braille dots actually drawn (U+2800..U+28FF); the trace itself.
        assert!(
            all.chars()
                .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c) && c != '\u{2800}'),
            "braille trace glyphs present:\n{all}"
        );
        // The probed nets are marked in the Nets & Parts list too.
        let list_row = rows
            .iter()
            .find(|r| r.contains("/LED") && r.contains("◉"))
            .unwrap();
        assert!(
            list_row.contains("◉ /LED"),
            "probe marker in the list: {list_row}"
        );
        // The footer advertises the probe key.
        assert!(all.contains("p probe"), "footer lists the p key:\n{all}");
    }

    /// Not an assertion, a viewer. `cargo test -p hauksbee-engine --lib
    /// scope_pane_snapshot -- --ignored --nocapture` prints the rendered frame
    /// so a human can eyeball the scope pane without a PTY.
    #[test]
    #[ignore = "visual snapshot dump; run with --ignored --nocapture to view"]
    fn scope_pane_snapshot_dump() {
        let mut st = scope_sample_state();
        st.dismiss_banner();
        st.scope.toggle("/LED");
        st.scope.toggle("/PA5");
        for i in 0..200 {
            let mut v = HashMap::new();
            v.insert(
                "/LED".to_string(),
                if (i / 20) % 2 == 0 { 0.0 } else { 3.3 },
            );
            v.insert("/PA5".to_string(), 3.3);
            st.scope.record(i as f64 * 5.0, &v);
        }
        for row in render_rows(&st, 120, 24) {
            eprintln!("{row}");
        }
    }

    #[test]
    fn scope_pane_placeholders_never_render_an_empty_box() {
        // No probes yet: the pane says how to probe.
        let mut st = scope_sample_state();
        st.dismiss_banner();
        let all = render_rows(&st, 120, 30).join("\n");
        assert!(all.contains("no nets probed"), "{all}");
        assert!(all.contains("press [p]"), "{all}");

        // Probed but no samples and not running: points at `r`.
        st.scope.toggle("/LED");
        let all = render_rows(&st, 120, 30).join("\n");
        assert!(all.contains("no live signals"), "{all}");
        assert!(all.contains("[r]"), "{all}");
    }

    #[test]
    fn no_mcu_board_collapses_the_live_panes_to_stubs() {
        let mut st = scope_sample_state();
        st.dismiss_banner();
        st.no_mcu = true;
        let rows = render_rows(&st, 120, 30);
        let all = rows.join("\n");

        // Both live panes are one line each, saying why, not empty boxes.
        assert!(all.contains("co-sim: no MCU on this board"), "{all}");
        assert!(
            all.contains("scope: no live signals without an MCU"),
            "{all}"
        );
        // Their boxes are gone: no bordered Co-sim / Scope pane titles remain.
        assert!(!all.contains("Co-sim "), "co-sim box collapsed:\n{all}");
        assert!(!all.contains("Scope "), "scope box collapsed:\n{all}");

        // The reclaimed width goes to the two panes that carry content: the
        // findings pane now runs to within a couple of columns of the edge.
        let findings_row = rows
            .iter()
            .find(|r| r.contains("Findings (triaged)"))
            .expect("findings pane present");
        assert!(
            findings_row.trim_end().len() >= 116,
            "findings pane spans the reclaimed width: {findings_row}"
        );
    }

    #[test]
    fn identity_bar_is_always_on_screen() {
        let mut st = scope_sample_state();
        st.dismiss_banner();
        // Board, bound MCU + backend, and the finding counts, on row 0.
        let rows = render_rows(&st, 120, 30);
        assert!(rows[0].contains("ScopeDemo"), "{}", rows[0]);
        assert!(rows[0].contains("STM32"), "{}", rows[0]);
        assert!(rows[0].contains("renode:stm32f103"), "{}", rows[0]);
        assert!(rows[0].contains("worth attention"), "{}", rows[0]);
        assert!(rows[0].contains("serious"), "{}", rows[0]);
    }

    #[test]
    fn parts_pane_title_reads_as_two_counts() {
        let mut st = scope_sample_state();
        st.dismiss_banner();
        let all = render_rows(&st, 120, 30).join("\n");
        assert!(all.contains("1 parts · 2 nets"), "{all}");
    }

    #[test]
    fn selected_finding_highlight_is_one_continuous_block() {
        let mut st = scope_sample_state();
        st.dismiss_banner();
        st.findings.push(crate::tui::state::Finding {
            severity: Severity::Medium,
            check: "si".into(),
            kind: "impedance".into(),
            headline: "a deliberately long headline that has to wrap across more \
                       than one row inside the findings pane so the selection \
                       band can be checked on every wrapped line"
                .into(),
            plain: String::new(),
            nets: Vec::new(),
            refs: Vec::new(),
            location_mm: None,
            layer: None,
            fix: None,
            actionable: true,
        });
        st.verdict = crate::tui::state::Verdict::from_findings(&st.findings);
        st.focus = Pane::Findings;
        st.findings_sel = 0;

        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| draw(f, &st, None, false)).unwrap();
        let buf = term.backend().buffer().clone();

        // Find the rows carrying the selection background, then assert the band
        // is unbroken from the first to the last column of the pane on each of
        // them (a REVERSED highlight leaves per-word gaps and colour fragments).
        let band = selection_style().bg.unwrap();
        let rows: Vec<u16> = (0..buf.area.height)
            .filter(|y| (0..buf.area.width).any(|x| buf[(x, *y)].bg == band))
            .collect();
        assert!(rows.len() >= 2, "the selected finding wrapped: {rows:?}");
        for y in rows {
            let xs: Vec<u16> = (0..buf.area.width)
                .filter(|x| buf[(*x, y)].bg == band)
                .collect();
            let (first, last) = (xs[0], *xs.last().unwrap());
            assert_eq!(
                xs.len() as u16,
                last - first + 1,
                "row {y} has gaps in the selection band"
            );
        }
    }
}

/// A centered rect `pct_x` % wide and `pct_y` % tall, for the overlay.
fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}
