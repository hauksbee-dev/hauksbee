//! The web front-door analysis: take an uploaded board file's bytes, run every
//! static check, and return a single JSON payload a browser can render with no
//! CLI involved.
//!
//! This is the "drop your board, get a report" backend. It reuses the exact same
//! analysis as the CLI (`ExtractedBoard` extraction + the DRC / lint / SI /
//! resource checks + the plain-language [`crate::plain`] templates), so the web
//! report and the terminal report can never disagree. The HTTP plumbing lives in
//! `hauksbee-server`; this module is pure (bytes in, JSON string out) so it has
//! no web dependency and is unit-testable.

use serde::Serialize;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::engine::HauksbeeEngine;
use crate::plain::{
    plain_drc_structured, plain_netlint, plain_si, PlainFinding, PlainLevel, PlainReport,
};
use crate::result::{BindSummary, JsonNote, JsonNoteKind};

/// A finding, in the shape the browser renders.
#[derive(Debug, Clone, Serialize)]
pub struct WebFinding {
    /// "serious" | "warning" | "note".
    pub level: String,
    pub what: String,
    pub why: String,
    pub fix: String,
}

impl From<&PlainFinding> for WebFinding {
    fn from(f: &PlainFinding) -> Self {
        WebFinding {
            level: match f.level {
                PlainLevel::Serious => "serious",
                PlainLevel::Warning => "warning",
                PlainLevel::Note => "note",
            }
            .to_string(),
            what: f.what.clone(),
            why: f.why.clone(),
            fix: f.fix.clone(),
        }
    }
}

/// One analysis section (one check family) as the browser sees it.
#[derive(Debug, Clone, Serialize)]
pub struct WebSection {
    /// "Copper spacing (DRC)", "Connectivity", "Signal integrity".
    pub title: String,
    pub verdict: String,
    pub findings: Vec<WebFinding>,
    /// Actionable info-level notes promoted from [`PlainReport::heads_up`] (the
    /// 171-ohm USB controlled-impedance note that used to disappear on the web).
    /// These never count as findings but must NEVER be silently dropped: a
    /// "Looks healthy" verdict that hides the only actionable observation is the
    /// exact false-comfort breach the plain/text/json surfaces already avoid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heads_up: Vec<String>,
}

impl WebSection {
    fn from_plain(title: &str, p: &PlainReport) -> Self {
        WebSection {
            title: title.to_string(),
            verdict: p.verdict(),
            findings: p.findings.iter().map(WebFinding::from).collect(),
            heads_up: p.heads_up.clone(),
        }
    }
}

/// The web mirror of the bind-honesty summary. Populated from [`BindSummary`]
/// (computed once on the already-bound board), NEVER recomputed, so the web
/// report carries the SAME bind-role honesty data as the CLI/JSON surfaces. The
/// browser uses `active_path_unresolved` to refuse a false "Looks healthy".
#[derive(Debug, Clone, Serialize)]
pub struct BindSummaryWeb {
    /// `"M/N"` — active ICs that bound, over the total active ICs on the board.
    pub critical_parts_bound: String,
    /// References of active ICs left open on the live circuit (unresolved active
    /// ICs + resolved-but-open active ICs). Non-empty => analog/AC/thermal on
    /// those nets is not trustworthy; the web banner must say so loudly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_path_unresolved: Vec<String>,
}

impl BindSummaryWeb {
    /// Bridge a [`BindSummary`] into the web shape: keep the critical-parts ratio
    /// and the union of open active ICs (unresolved + resolved-but-open), refs
    /// only, in report order.
    pub fn from_summary(s: &BindSummary) -> Self {
        let active_path_unresolved = s
            .active_path_unresolved
            .iter()
            .filter(|u| u.active_ic)
            .chain(s.resolved_but_open_active.iter().filter(|u| u.active_ic))
            .map(|u| u.reference.clone())
            .collect();
        BindSummaryWeb {
            critical_parts_bound: s.critical_parts_bound.clone(),
            active_path_unresolved,
        }
    }

    /// Whether an active IC is open on the live circuit — the predicate that must
    /// override a "Looks healthy" verdict on the web report.
    pub fn active_path_open(&self) -> bool {
        !self.active_path_unresolved.is_empty()
    }
}

/// One watched net's activity after a firmware co-sim run, for the web GPIO
/// table. `driven` marks a net that actually moved (toggled) during the run, so
/// the browser can show at a glance whether the firmware drove anything.
#[derive(Debug, Clone, Serialize)]
pub struct WebGpioNet {
    pub name: String,
    /// Last sampled node voltage at the end of the run.
    pub volts: f64,
    /// True if the net toggled at least once (the firmware drove it).
    pub driven: bool,
}

/// The firmware co-sim result, attached to [`WebReport::cosim`] only when a
/// firmware file was supplied. `ran` is false (with `findings`/`uart_output`
/// carrying the reason) when the board has no MCU, the backend is an external
/// emulator we skip on the web path, or the firmware failed to load. The
/// findings reuse the exact same `{what, why, fix}` cards as the static sections
/// (via [`crate::plain::plain_faults`]), so the web and CLI co-sim surfaces can
/// never disagree.
#[derive(Debug, Clone, Serialize)]
pub struct WebCosimSection {
    /// Whether a co-sim actually executed against the firmware.
    pub ran: bool,
    /// Wall-clock simulated time (seconds). Zero when `ran` is false.
    pub seconds_simulated: f64,
    /// Captured UART output (best-effort UTF-8, may be empty).
    pub uart_output: String,
    /// Electrical-stress faults from the run, rendered like static findings.
    pub findings: Vec<WebFinding>,
    /// Per-net activity table (top movers first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpio_nets: Vec<WebGpioNet>,
    /// False once any chunk's analog solve failed to converge during the co-sim:
    /// the run held stale node voltages over `failed_windows` and cannot vouch
    /// for electrical results there (05 §3b, refuse rather than fake). A clean
    /// run reports `true` with an empty `failed_windows`, so the common JSON shape
    /// is unchanged and existing consumers keep parsing. This is the STRUCTURAL
    /// mirror of the prepended analog-validity finding: a JSON consumer must be
    /// able to read invalidity as a field, not only scrape it out of prose.
    /// Always serialized (never skipped) so a consumer can rely on its presence.
    pub analog_valid: bool,
    /// Sim-time windows `[start_s, end_s)` where the analog solve failed. Empty
    /// (and omitted) on a clean run. Mirrors the CLI `--json` `failed_windows`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_windows: Vec<WebFailedWindow>,
}

/// One sim-time window `[start_s, end_s)` where the analog co-sim solve failed to
/// converge and the run held stale voltages. Reported so the browser can show the
/// exact span whose electrical results are untrustworthy. Mirrors the CLI/JSON
/// `CosimFailedWindow`.
#[derive(Debug, Clone, Serialize)]
pub struct WebFailedWindow {
    pub start_s: f64,
    pub end_s: f64,
}

/// A component for the simple 2D footprint map (positions in board mm).
#[derive(Debug, Clone, Serialize)]
pub struct WebComponent {
    pub reference: String,
    pub value: String,
    pub x: f64,
    pub y: f64,
    pub rot: f64,
}

/// The whole payload sent back to the browser after an upload.
#[derive(Debug, Clone, Serialize)]
pub struct WebReport {
    pub ok: bool,
    /// Set when extraction failed; `ok` is false and the rest is empty.
    pub error: Option<String>,
    pub board_name: String,
    pub file_name: String,
    pub num_components: usize,
    pub num_nets: usize,
    /// The single overall headline across all sections.
    pub headline: String,
    /// Total serious findings across all sections.
    pub serious: usize,
    /// Total findings across all sections.
    pub total: usize,
    pub sections: Vec<WebSection>,
    /// Components with a known position, for the 2D map (empty for netlist-only
    /// inputs that carry no layout).
    pub components: Vec<WebComponent>,
    /// Bind-role honesty summary (active ICs bound / open on the live circuit).
    /// Present whenever the board bound; the browser renders a warning banner
    /// when `active_path_unresolved` is non-empty and must NOT say "Looks
    /// healthy" while it is. Mirrors the CLI/JSON bind surface (parity fix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<BindSummaryWeb>,
    /// Info-level notes (bind roles, coverage caveats) that must never be
    /// silently absent. Additive + `skip_serializing_if` so the `/api/analyze`
    /// JSON schema stays backward-compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<JsonNote>,
    /// Firmware co-sim result, present only when [`analyze_with_firmware`] ran
    /// (a firmware file was dropped alongside the board). Additive +
    /// `skip_serializing_if` so the board-only `/api/analyze` path is byte-for-
    /// byte unchanged for existing consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosim: Option<WebCosimSection>,
}

/// Run the full front-door analysis on an uploaded board file.
///
/// `file_name` is used only for display and to disambiguate a `.kicad_sch` (which
/// the extractor handles by content here too). `contents` is the file's text.
pub fn analyze(file_name: &str, contents: &str) -> WebReport {
    let board = match ExtractedBoard::from_auto(contents) {
        Ok(b) => b,
        Err(e) => {
            return WebReport {
                ok: false,
                error: Some(format!(
                    "Could not read this board file: {e}. Supported: KiCad .kicad_pcb / .kicad_sch, Eagle .brd, IPC-D-356 .d356, or a gerber zip."
                )),
                board_name: String::new(),
                file_name: file_name.to_string(),
                num_components: 0,
                num_nets: 0,
                headline: "Could not read the file.".to_string(),
                serious: 0,
                total: 0,
                sections: Vec::new(),
                components: Vec::new(),
                bind: None,
                notes: Vec::new(),
                cosim: None,
            };
        }
    };

    let lib = ModelLibrary::builtin_with_user_dirs(&[]);

    // DRC reads copper geometry from the raw text (gerbers/KiCad layout).
    let drc = ExtractedBoard::drc(contents).unwrap_or_default();
    // Render from the grouped structure (single source of truth shared with the
    // CLI text/plain/json surfaces): duplicates collapsed, and gap==rule labelled
    // "at minimum clearance (no margin)" rather than the wrong "below the rule".
    let drc_plain = plain_drc_structured(&crate::result::DrcStructured::from_report(&drc));

    // Lint = the same bundle the `--lint` CLI surface assembles, via the single
    // `engine_lint` chokepoint (connectivity + strap lint + MCU resource conflicts
    // + the unchecked-strap-bearing-MCU coverage note), so the web report never
    // prints "Looks healthy" over a strap-bearing MCU whose BOOT0 was unexamined.
    let lint = crate::checks::engine_lint(&board, &lib);
    let lint_plain = plain_netlint(&lint);

    // SI needs the layout text for the geometry-bearing checks.
    let si = board.si_checks(Some(contents));
    let si_plain = plain_si(&si);

    let sections = vec![
        WebSection::from_plain("Copper spacing (DRC)", &drc_plain),
        WebSection::from_plain("Connectivity & wiring", &lint_plain),
        WebSection::from_plain("Signal integrity", &si_plain),
    ];

    let serious: usize = sections
        .iter()
        .map(|s| s.findings.iter().filter(|f| f.level == "serious").count())
        .sum();
    let total: usize = sections.iter().map(|s| s.findings.len()).sum();

    // Bind to count nets/components consistently with the report panel, AND to
    // derive the same bind-role honesty data the CLI/JSON surfaces carry. The
    // web report previously dropped this entirely (a board with every active IC
    // open showed "Looks healthy"); compute it once here from the bound report.
    let bound = bind_board(&board, &lib);
    let bind_summary = BindSummary::from_report(&bound.report);
    let bind_web = BindSummaryWeb::from_summary(&bind_summary);

    // Notes: bind-role caveat (active IC open on the live circuit). These mirror
    // the CLI/JSON `notes` so the web never silently omits an honesty annotation.
    let mut notes: Vec<JsonNote> = Vec::new();
    if bind_web.active_path_open() {
        notes.push(JsonNote {
            kind: JsonNoteKind::BindRole,
            message: format!(
                "Active IC(s) left open on the live circuit: {}. Analog/AC/thermal results on their nets are NOT trustworthy.",
                bind_web.active_path_unresolved.join(", ")
            ),
        });
    }

    // Any heads-up note (e.g. the 171-ohm USB controlled-impedance note) is an
    // actionable observation, so the headline must NOT read "Looks healthy".
    let has_heads_up = sections.iter().any(|s| !s.heads_up.is_empty());
    let headline = overall_headline(total, serious, has_heads_up, bind_web.active_path_open());

    let components = board
        .components
        .iter()
        .filter_map(|c| {
            c.position.map(|(x, y, rot)| WebComponent {
                reference: c.reference.clone(),
                value: c.value.clone(),
                x,
                y,
                rot,
            })
        })
        .collect();

    WebReport {
        ok: true,
        error: None,
        board_name: board.name.clone(),
        file_name: file_name.to_string(),
        num_components: board.components.len(),
        num_nets: bound.net_names.len(),
        headline,
        serious,
        total,
        sections,
        components,
        bind: Some(bind_web),
        notes,
        cosim: None,
    }
}

/// Run the full static analysis, then (when the board has a bound MCU on an
/// in-process backend) a short firmware co-sim, and attach the result as
/// [`WebReport::cosim`].
///
/// `fw_bytes` is the raw uploaded firmware (ELF/HEX); it is passed as `&[u8]`
/// and written verbatim to a temp file — NEVER lossy-decoded, which would
/// corrupt an ELF. The co-sim is skipped (with a friendly `cosim.ran = false`
/// note instead of an error) when:
///   * the board has no bound MCU (nothing to run firmware on), or
///   * the only MCU(s) use an external emulator backend (Renode/QEMU): those
///     advance over a TCP control socket and can take 5-30s, well past a
///     browser/Axum request budget, so the web path stays in-process only, or
///   * the firmware fails to load (wrong architecture, corrupt file).
///
/// The static [`WebReport`] is always returned intact; only `cosim` reflects
/// whether the firmware run succeeded.
pub fn analyze_with_firmware(
    file_name: &str,
    contents: &str,
    fw_name: &str,
    fw_bytes: &[u8],
) -> WebReport {
    // Always run the static analysis first. If it failed to even read the board,
    // there is no board to co-sim against; return the static error as-is.
    let mut report = analyze(file_name, contents);
    if !report.ok {
        return report;
    }

    report.cosim = Some(run_web_cosim(contents, fw_name, fw_bytes));
    report
}

/// A skipped/failed co-sim section carrying the reason as a single note-level
/// finding, so the browser renders it with the same card markup.
fn cosim_unavailable(reason: impl Into<String>) -> WebCosimSection {
    WebCosimSection {
        ran: false,
        seconds_simulated: 0.0,
        uart_output: String::new(),
        findings: vec![WebFinding {
            level: "note".to_string(),
            what: "Co-sim not available for this board.".to_string(),
            why: reason.into(),
            fix: "Drop a board with a supported in-process MCU and a matching firmware build to run the firmware co-sim.".to_string(),
        }],
        gpio_nets: Vec::new(),
        // A co-sim that never ran cannot have failed an analog chunk: report the
        // clean, backward-compatible shape (valid, no windows).
        analog_valid: true,
        failed_windows: Vec::new(),
    }
}

/// The loud, plain-language analog-validity finding, prepended to a co-sim
/// section when the run held stale voltages over one or more failed windows
/// (05 §3b). Factored out (a) so `run_web_cosim` reads cleanly and (b) so its
/// exact wording is unit-testable without staging a diverging board through the
/// whole engine. `failed_chunks` is the count and `windows` the merged spans.
fn analog_invalid_finding(failed_chunks: u64, windows: &[WebFailedWindow]) -> WebFinding {
    // Human-readable span list in milliseconds: "1.20-3.40 ms". Empty windows
    // (should not happen when failed_chunks > 0) degrade to "unknown span".
    let spans = if windows.is_empty() {
        "an unrecorded span".to_string()
    } else {
        windows
            .iter()
            .map(|w| format!("{:.2}-{:.2} ms", w.start_s * 1e3, w.end_s * 1e3))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let chunk_word = if failed_chunks == 1 { "chunk" } else { "chunks" };
    WebFinding {
        level: "serious".to_string(),
        what: "Analog co-sim did not converge: electrical results are not trustworthy".to_string(),
        why: format!(
            "The analog solver failed on {failed_chunks} {chunk_word} covering {spans}. \
             Over those windows the co-sim held stale node voltages instead of a real \
             solve, so any voltage, current or fault reading there is fiction (05 §3b, \
             refuse rather than fake)."
        ),
        fix: "Treat electrical results inside those windows as unknown. A stiff or \
              structurally singular section (conflicting rails, an unconverging \
              nonlinear stage) usually causes it; simplify the offending section or \
              relax the operating point, then re-run."
            .to_string(),
    }
}

/// The actual firmware co-sim for the web path. Mirrors `run_headless` in the
/// CLI (same fixed 1 kHz frame cadence, same fault dedup via `plain_faults`) but
/// runs synchronously for a short fixed window so it fits a request budget.
fn run_web_cosim(contents: &str, fw_name: &str, fw_bytes: &[u8]) -> WebCosimSection {
    use crate::plain::plain_faults;
    use crate::stress::{FaultEvent, FaultKind};
    use hauksbee_server::engine::Engine;
    use std::io::Write;

    // No MCU => firmware drives nothing. Inspect the bound board before paying for
    // a temp file / engine build, and say so plainly.
    let board = match ExtractedBoard::from_auto(contents) {
        Ok(b) => b,
        // Should not happen (the static analyze already succeeded), but be safe.
        Err(e) => return cosim_unavailable(format!("Could not re-read the board for co-sim: {e}.")),
    };
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    if bound.mcus.is_empty() {
        return cosim_unavailable(
            "No microcontroller was found on this board; the firmware co-sim needs an MCU to run on.",
        );
    }
    // Web path is in-process (simavr) only: external emulator backends
    // (Renode/QEMU) advance over a TCP socket and can take many seconds, past a
    // browser request budget. Skip them here with a clear note (the CLI co-sim
    // still runs them).
    if bound
        .mcus
        .iter()
        .all(|m| m.backend.starts_with("renode:") || m.backend.starts_with("qemu:"))
    {
        return cosim_unavailable(
            "This board's MCU uses an external emulator (Renode/QEMU) that is too slow for the web path. Run it from the command line: hauksbee run <board> --firmware <fw> --headless.",
        );
    }

    // Write the firmware bytes verbatim to a temp file (the MCU loader is
    // path-based). NEVER lossy-decode: an ELF has null bytes and arbitrary
    // patterns that UTF-8 round-tripping would corrupt. The loader dispatches on
    // the file EXTENSION (.elf vs .hex), so preserve the uploaded name's suffix
    // (default .elf for an unknown/extensionless upload).
    let suffix = match std::path::Path::new(fw_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        Some(e) if e == "hex" => ".hex",
        _ => ".elf",
    };
    let mut tmp = match tempfile::Builder::new()
        .prefix("hauksbee-fw-")
        .suffix(suffix)
        .tempfile()
    {
        Ok(t) => t,
        Err(e) => return cosim_unavailable(format!("Could not stage the firmware for co-sim: {e}.")),
    };
    if let Err(e) = tmp.write_all(fw_bytes).and_then(|_| tmp.flush()) {
        return cosim_unavailable(format!("Could not write the firmware for co-sim: {e}."));
    }

    let mut engine =
        match HauksbeeEngine::from_board_file(contents, Some(tmp.path()), "web-firmware") {
            Ok(e) => e,
            // Architecture mismatch / corrupt firmware: the static analysis still
            // succeeded, so report co-sim failure as a note, not a hard error.
            Err(e) => {
                return cosim_unavailable(format!(
                    "The firmware could not be loaded onto this board's MCU: {e}. \
                     (Check the firmware matches the MCU's architecture.)"
                ))
            }
        };

    // Short fixed window mirroring run_headless: 1 kHz frame cadence for ~0.2s.
    let seconds = 0.2;
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    let mut last_uart: Vec<u8> = Vec::new();
    let mut faults: Vec<FaultEvent> = Vec::new();
    while t < seconds {
        let frame = engine.step(frame_dt);
        for bytes in frame.uart.values() {
            last_uart.extend_from_slice(bytes);
        }
        for f in frame.faults {
            faults.push(FaultEvent {
                component: f.component,
                kind: FaultKind::from_str(&f.kind),
                value: f.value,
                limit: f.limit,
                t: f.t,
                destroyed: f.destroyed,
            });
        }
        t += frame_dt;
    }

    // De-duplicate faults by (component, kind), keeping the worst value, exactly
    // like the CLI headless path, so the two surfaces produce identical findings.
    faults.sort_by(|a, b| {
        a.component
            .cmp(&b.component)
            .then(a.kind.as_str().cmp(b.kind.as_str()))
            .then(
                b.value
                    .partial_cmp(&a.value)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    faults.dedup_by(|a, b| a.component == b.component && a.kind.as_str() == b.kind.as_str());

    let plain = plain_faults(&faults);
    let mut findings: Vec<WebFinding> = plain.findings.iter().map(WebFinding::from).collect();

    // Per-net activity table (top movers first), mirroring the CLI toggle table.
    let sched = engine.scheduler();
    let seconds_simulated = sched.sim_time;
    // Co-sim honesty parity with the CLI/TUI surfaces. Without these two notes the
    // web path is the ONE surface that gives false comfort: an empty `findings`
    // list renders as "No electrical-stress faults during the run." even when the
    // firmware never executed or ran on a substitute chip. Prepended so they lead.
    let total_toggles: u64 = sched.stats.values().map(|s| s.toggles).sum();
    let uart_empty = last_uart.is_empty();
    // A pin driven high and HELD shows zero net toggles yet clearly ran, so the
    // refusal must also consult whether the firmware drove any GPIO at all.
    let any_gpio_driven = sched.any_gpio_driven();
    // Chip-substitution: the firmware was emulated on a less-specific core.
    for sub in sched.substitutions() {
        findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: "MCU modelled on a substitute core".to_string(),
                why: sub.message(),
                fix: "Peripherals/clock/flash may differ on the real part; treat \
                      the co-sim as approximate for this MCU."
                    .to_string(),
            },
        );
    }
    // Zero-activity refusal: a run that drove nothing proves nothing.
    if total_toggles == 0 && uart_empty && !any_gpio_driven {
        findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: "Firmware was not exercised".to_string(),
                why: "Co-sim saw zero net toggles and no UART output; this result \
                      cannot vouch for firmware behaviour (the MCU may have stalled \
                      at boot, run no I/O, or the firmware may not match this board)."
                    .to_string(),
                fix: "Confirm the firmware matches this MCU and actually drives I/O."
                    .to_string(),
            },
        );
    }
    // Analog-validity refusal (05 §3b): if any chunk's analog solve failed, the
    // web report is the surface most likely to give false comfort (an empty
    // findings list reads as "no faults"). Surface it BOTH as a loud prepended
    // finding (so it leads the prose) and as the structural `analog_valid` /
    // `failed_windows` fields below (so a JSON consumer reads it as data). This
    // parallels the CLI `--json` and the TUI pane; the web path used to consult
    // neither `analog_valid()` nor `failed_windows()`, so a diverged run showed
    // as quiet.
    let analog_valid = sched.analog_valid();
    let failed_windows: Vec<WebFailedWindow> = sched
        .failed_windows()
        .iter()
        .map(|&(start_s, end_s)| WebFailedWindow { start_s, end_s })
        .collect();
    if !analog_valid {
        findings.insert(
            0,
            analog_invalid_finding(sched.failed_chunk_count(), &failed_windows),
        );
    }
    let net_volts = sched.net_voltages();
    let mut gpio_nets: Vec<WebGpioNet> = sched
        .stats
        .iter()
        .map(|(name, st)| WebGpioNet {
            name: name.clone(),
            volts: net_volts.get(name).copied().unwrap_or(0.0),
            driven: st.toggles > 0,
        })
        .collect();
    gpio_nets.sort_by(|a, b| {
        b.driven
            .cmp(&a.driven)
            .then(a.name.cmp(&b.name))
    });
    gpio_nets.truncate(15);

    let uart_output = String::from_utf8_lossy(&last_uart).trim_end().to_string();

    WebCosimSection {
        ran: true,
        seconds_simulated,
        uart_output,
        findings,
        gpio_nets,
        analog_valid,
        failed_windows,
    }
}

/// The single line at the top of the web report.
///
/// `has_heads_up` / `bind_open` make the headline honest: an actionable info
/// note (e.g. the 171-ohm USB note) or an active IC open on the live circuit
/// must override a bare "Looks healthy" even when no findings were raised.
fn overall_headline(total: usize, serious: usize, has_heads_up: bool, bind_open: bool) -> String {
    if total == 0 {
        if bind_open {
            return "No blocking issues, but active parts are unresolved — analog/AC/thermal results are not trustworthy. See the notes.".to_string();
        }
        if has_heads_up {
            return "No problems found, but there is something worth knowing — see the heads-up note below.".to_string();
        }
        return "Looks healthy: the static checks found no problems.".to_string();
    }
    let issues = if total == 1 { "issue" } else { "issues" };
    if serious == 0 {
        format!("{total} {issues} found, none serious. Worth a look before you order boards.")
    } else {
        format!("{total} {issues} found, {serious} serious. Fix the serious ones before ordering boards.")
    }
}

/// Serialize an [`analyze`] result to a JSON string for the HTTP layer.
pub fn analyze_json(file_name: &str, contents: &str) -> String {
    let report = analyze(file_name, contents);
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

/// Serialize an [`analyze_with_firmware`] result to a JSON string for the HTTP
/// layer (the `/api/analyze-with-firmware` endpoint). Firmware bytes are passed
/// as `&[u8]` end-to-end — never lossy-decoded — so an uploaded ELF stays intact.
pub fn analyze_with_firmware_json(
    file_name: &str,
    contents: &str,
    fw_name: &str,
    fw_bytes: &[u8],
) -> String {
    let report = analyze_with_firmware(file_name, contents, fw_name, fw_bytes);
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORTED: &str = include_str!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    const BLUEPILL: &str = include_str!("../../../testdata/boards/stm32_bluepill_demo.kicad_pcb");

    #[test]
    fn from_plain_carries_heads_up_and_serializes() {
        // Parity fix (the 171-ohm USB controlled-impedance case): an actionable
        // info note stored on PlainReport.heads_up must reach the web section,
        // even when the section verdict reads "healthy" (zero findings). This is
        // the exact note that used to vanish on the web while showing on --plain.
        let mut p = PlainReport::default();
        p.subject = "signal-integrity".to_string();
        p.heads_up
            .push("USB trace impedance is 171 ohm, +71% from target (90 ohm).".to_string());
        let sect = WebSection::from_plain("Signal integrity", &p);
        assert!(sect.findings.is_empty(), "no findings, only a heads-up note");
        assert_eq!(sect.heads_up.len(), 1, "the heads-up note must survive");
        // And it serializes into the JSON payload the browser reads.
        let json = serde_json::to_string(&sect).unwrap();
        assert!(
            json.contains("heads_up") && json.contains("171 ohm"),
            "heads_up must be present in the section JSON: {json}"
        );
    }

    #[test]
    fn healthy_board_with_heads_up_never_says_looks_healthy() {
        // The headline must not give false comfort when a heads-up note exists.
        let h = overall_headline(0, 0, true, false);
        assert!(
            !h.to_lowercase().contains("looks healthy"),
            "headline must flag the heads-up: {h}"
        );
        // And a bind-open board likewise.
        let h2 = overall_headline(0, 0, false, true);
        assert!(
            !h2.to_lowercase().contains("looks healthy") && h2.to_lowercase().contains("trustworthy"),
            "bind-open headline must warn: {h2}"
        );
        // A genuinely clean board still reads healthy.
        let h3 = overall_headline(0, 0, false, false);
        assert!(h3.to_lowercase().contains("looks healthy"), "clean board: {h3}");
    }

    #[test]
    fn web_report_carries_bind_summary() {
        // The web report must include the bind-role honesty summary (it used to
        // drop it entirely). Even a healthy board carries the critical-parts
        // ratio so the surface matches the CLI/JSON bind section.
        let r = analyze("bp.kicad_pcb", BLUEPILL);
        let bind = r.bind.expect("bind summary present on a bound board");
        assert!(
            bind.critical_parts_bound.contains('/'),
            "critical_parts_bound is an M/N ratio: {}",
            bind.critical_parts_bound
        );
    }

    #[test]
    fn analyze_shorted_board_reports_serious() {
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(r.ok, "extraction should succeed: {:?}", r.error);
        assert!(
            r.serious > 0,
            "boot_gate has copper shorts -> serious findings"
        );
        assert!(r.headline.to_lowercase().contains("serious"));
        // The DRC section specifically should carry the shorts.
        let drc = r.sections.iter().find(|s| s.title.contains("DRC")).unwrap();
        assert!(drc.findings.iter().any(|f| f.level == "serious"));
        // Every finding has all three plain fields.
        for s in &r.sections {
            for f in &s.findings {
                assert!(!f.what.is_empty() && !f.why.is_empty() && !f.fix.is_empty());
            }
        }
    }

    #[test]
    fn analyze_emits_component_positions_for_a_layout() {
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(!r.components.is_empty(), "a KiCad layout has placed parts");
    }

    #[test]
    fn analyze_garbage_returns_a_friendly_error() {
        let r = analyze("nope.txt", "this is not a board file at all");
        assert!(!r.ok);
        assert!(r.error.is_some());
        // The JSON wrapper still produces valid JSON.
        let json = analyze_json("nope.txt", "garbage");
        assert!(json.contains("\"ok\":false"));
    }

    #[test]
    fn analyze_json_is_valid_json() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["sections"].as_array().unwrap().len() >= 3);
    }

    // Track D: web firmware drop zone.
    const NO_MCU: &str = include_str!("../../hauksbee-ci/examples/boards/power_resistor.kicad_pcb");
    const BOOT_GATE_FW: &[u8] = include_bytes!("../../../testdata/firmware/boot_gate_a/boot_gate.elf");

    #[test]
    fn board_only_path_leaves_cosim_absent() {
        // The plain board-only analyze() must NOT carry a cosim field, so the
        // /api/analyze JSON schema is byte-for-byte unchanged (skip_serializing_if).
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(r.cosim.is_none(), "board-only analyze must not set cosim");
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        assert!(!json.contains("\"cosim\""), "board-only JSON must omit cosim: {json:.200}");
    }

    #[test]
    fn firmware_on_board_with_no_mcu_says_unavailable() {
        // A board with no microcontroller: static analysis still succeeds, and
        // cosim.ran is false with a friendly note (NOT a hard ok:false error).
        let r = analyze_with_firmware("pr.kicad_pcb", NO_MCU, "fw.elf", BOOT_GATE_FW);
        assert!(r.ok, "static analysis still succeeds: {:?}", r.error);
        let cosim = r.cosim.expect("cosim present once firmware was supplied");
        assert!(!cosim.ran, "no MCU => co-sim cannot run");
        assert!(
            cosim.findings.iter().any(|f| f.why.to_lowercase().contains("microcontroller")),
            "should name the missing MCU as the reason: {:?}",
            cosim.findings
        );
    }

    #[test]
    fn empty_firmware_fails_to_load_gracefully() {
        // Empty firmware bytes on an MCU board: load fails, but the static report
        // stays ok:true and cosim.ran:false carries the load error as a note.
        let r = analyze_with_firmware("boot_gate.kicad_pcb", SHORTED, "fw.elf", &[]);
        assert!(r.ok, "static analysis still succeeds: {:?}", r.error);
        let cosim = r.cosim.expect("cosim present once firmware was supplied");
        assert!(!cosim.ran, "empty firmware cannot load => ran:false");
    }

    #[test]
    fn firmware_json_serializes_cosim_field() {
        // The JSON wrapper for the firmware path includes the cosim object.
        let json = analyze_with_firmware_json("pr.kicad_pcb", NO_MCU, "fw.elf", BOOT_GATE_FW);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["cosim"].is_object(), "cosim object must be present: {json:.200}");
        assert_eq!(v["cosim"]["ran"], false);
    }

    #[test]
    fn real_firmware_on_in_process_mcu_runs() {
        // boot_gate is an ATmega328 (in-process simavr backend) and boot_gate.elf
        // is its matching AVR firmware: the co-sim should actually run. If the
        // build was made without the AVR backend the load fails gracefully, so we
        // accept either a real run OR a friendly ran:false note (never ok:false).
        let r = analyze_with_firmware("boot_gate.kicad_pcb", SHORTED, "boot_gate.elf", BOOT_GATE_FW);
        assert!(r.ok, "static analysis still succeeds: {:?}", r.error);
        let cosim = r.cosim.expect("cosim present once firmware was supplied");
        if cosim.ran {
            assert!(
                cosim.seconds_simulated > 0.0,
                "a run that ran must have advanced time"
            );
        } else {
            assert!(
                !cosim.findings.is_empty(),
                "a skipped co-sim must explain why"
            );
        }
    }

    #[test]
    fn cosim_section_always_carries_analog_valid_field() {
        // Finding 1 (05 §3b): the web co-sim section must expose analog validity
        // STRUCTURALLY, not only as prose. On a converging board it is true and
        // present; failed_windows is omitted when empty (backward-compatible).
        let json = analyze_with_firmware_json("boot_gate.kicad_pcb", SHORTED, "boot_gate.elf", BOOT_GATE_FW);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cosim = &v["cosim"];
        assert!(cosim.is_object(), "cosim object present: {json:.200}");
        assert!(
            cosim.get("analog_valid").is_some(),
            "analog_valid must always be present on the cosim section: {json:.300}"
        );
        assert_eq!(
            cosim["analog_valid"], true,
            "a converging (or skipped) run reports analog_valid:true"
        );
        assert!(
            cosim.get("failed_windows").is_none(),
            "failed_windows is omitted when empty: {json:.300}"
        );
    }

    #[test]
    fn analog_invalid_finding_is_loud_and_plain() {
        // The prepended honesty note names the failed-chunk count, the affected
        // millisecond span, and refuses to vouch for electrical results there.
        let windows = vec![WebFailedWindow {
            start_s: 0.0012,
            end_s: 0.0034,
        }];
        let f = analog_invalid_finding(2, &windows);
        assert_eq!(f.level, "serious", "analog invalidity is serious, not a note");
        assert!(
            f.what.to_lowercase().contains("not trustworthy"),
            "headline must refuse trust: {}",
            f.what
        );
        assert!(
            f.why.contains("2 chunks") && f.why.contains("1.20-3.40 ms"),
            "why must name the count and the span: {}",
            f.why
        );
        assert!(
            f.why.to_lowercase().contains("held stale"),
            "why must explain the held-stale mechanism: {}",
            f.why
        );
    }

    #[test]
    fn invalid_cosim_section_serializes_field_and_windows() {
        // A section built for a diverged run serializes analog_valid:false plus
        // the failed windows, so the browser/JSON consumer reads it as data.
        let section = WebCosimSection {
            ran: true,
            seconds_simulated: 0.1,
            uart_output: String::new(),
            findings: vec![analog_invalid_finding(
                1,
                &[WebFailedWindow { start_s: 0.0, end_s: 0.0001 }],
            )],
            gpio_nets: Vec::new(),
            analog_valid: false,
            failed_windows: vec![WebFailedWindow { start_s: 0.0, end_s: 0.0001 }],
        };
        let json = serde_json::to_string(&section).unwrap();
        assert!(
            json.contains("\"analog_valid\":false"),
            "invalid run serializes analog_valid:false: {json}"
        );
        assert!(
            json.contains("\"failed_windows\""),
            "invalid run lists failed_windows: {json}"
        );
    }
}
