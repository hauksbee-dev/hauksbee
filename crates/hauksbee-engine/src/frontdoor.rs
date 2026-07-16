//! The web front-door analysis: take an uploaded board file's bytes, run every
//! static check, and return a single JSON payload a browser can render with no
//! CLI involved.
//!
//! This is the "drop your board, get a report" backend. It reuses the exact same
//! analysis as the CLI (`ExtractedBoard` extraction + the DRC / lint / SI /
//! resource checks + the plain-language [`crate::plain`] templates).
//!
//! One documented divergence: the web path receives a single file's bytes and so
//! cannot see a sibling `.kicad_pro` project file. Its DRC therefore uses the
//! board's default/embedded clearances, whereas the CLI (`hauksbee run --drc`,
//! given the `.kicad_pro` next to the board) applies per-netclass clearances. On
//! a KiCad board with non-default netclass clearances the two DRC surfaces can
//! report different violations. Everything else is byte-for-byte the same. The
//! HTTP plumbing lives in `hauksbee-server`; this module is pure (bytes in, JSON
//! string out) so it has no web dependency and is unit-testable.

use serde::Serialize;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::engine::HauksbeeEngine;
use crate::plain::{
    plain_drc_structured, plain_netlint, plain_si, HeadsUp, PlainFinding, PlainLevel, PlainReport,
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

/// A heads-up note in the shape the browser renders. Carries the same
/// what / why / what-to-do gloss a finding does (persona-panel fix: a bare
/// "Zdiff 173 vs 90" jargon line got the finding treatment). `why` / `fix` are
/// omitted from the JSON when empty (a self-contained note), so the browser
/// renders only the lines that exist.
#[derive(Debug, Clone, Serialize)]
pub struct WebHeadsUp {
    pub what: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fix: String,
}

impl From<&HeadsUp> for WebHeadsUp {
    fn from(h: &HeadsUp) -> Self {
        WebHeadsUp { what: h.what.clone(), why: h.why.clone(), fix: h.fix.clone() }
    }
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
    pub heads_up: Vec<WebHeadsUp>,
}

impl WebSection {
    fn from_plain(title: &str, p: &PlainReport) -> Self {
        WebSection {
            title: title.to_string(),
            verdict: p.verdict(),
            findings: p.findings.iter().map(WebFinding::from).collect(),
            heads_up: p.heads_up.iter().map(WebHeadsUp::from).collect(),
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
    /// Per-SPI-slave chip-select framing tier (05 §2): `exact` (framed from the
    /// resolved CS pin's real GPIO edges), `backend` (the backend surfaced CS
    /// itself, e.g. Renode hardware-NSS FinishTransmission), or `heuristic` (no
    /// resolved CS, the chunk-boundary fallback, whose two failure modes are
    /// surfaced here rather than hidden). Empty (and omitted) when the board has
    /// no SPI slaves, so the common JSON shape is unchanged for existing consumers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spi_framing: Vec<WebSpiFraming>,
    /// Per-gate power-up state panel (what the firmware does to each
    /// transistor-gate control net at boot). Mirrors the CLI `--json`
    /// `boot_gates` / `--plain` gate panel so the web surface can no longer give
    /// false comfort on a board that energises a switched load at power-up.
    /// Empty (and omitted) when the firmware ran no relevant gates, so the
    /// common JSON shape is unchanged for existing consumers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_gates: Vec<WebBootGate>,
}

/// One row of the web boot-state panel: a transistor gate control net and what
/// the firmware does to it at power-up. Mirrors the CLI `BootGateJson`.
#[derive(Debug, Clone, Serialize)]
pub struct WebBootGate {
    pub reference: String,
    pub net: String,
    /// `"driven_high"` | `"driven_low"` | `"floating"`.
    pub state: String,
}

/// One SPI slave's chip-select framing tier, for the co-sim coverage (05 §2).
#[derive(Debug, Clone, Serialize)]
pub struct WebSpiFraming {
    /// The bus / slave id (its reference designator).
    pub bus: String,
    /// `"exact"` | `"backend"` | `"heuristic"`.
    pub mode: String,
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
/// the extractor handles by content here too). `contents` is the file's RAW
/// bytes: binary formats (Altium `.PcbDoc`, an OLE2 container) are sniffed and
/// read from the bytes first, exactly like the CLI `run` path, and only a
/// non-binary input falls back to the text sniffer over a lossy-UTF8 view
/// (which is exact for the text formats). Decoding before this point would
/// corrupt a binary board before it was ever parsed.
pub fn analyze(file_name: &str, contents: &[u8]) -> WebReport {
    // Binary-first, mirroring `run`: `from_auto_bytes` claims a recognised
    // binary board (OLE2 magic + Altium streams) or returns None so text
    // formats keep their exact behaviour through `from_auto`.
    let binary = ExtractedBoard::from_auto_bytes(contents);
    let is_binary = binary.is_some();
    let text = String::from_utf8_lossy(contents);
    // Text view for the geometry-bearing text checks (DRC / SI). A binary board
    // has no meaningful text: those checks get their bytes twin / None instead.
    let text_view: Option<&str> = (!is_binary).then_some(&text);
    let board = match binary.unwrap_or_else(|| ExtractedBoard::from_auto(&text)) {
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

    // DRC reads copper geometry from the raw input: the bytes twin
    // (`altium_drc`) for a binary board, the text path (gerbers/KiCad layout)
    // for everything else.
    let drc = if is_binary {
        ExtractedBoard::altium_drc(contents).unwrap_or_default()
    } else {
        ExtractedBoard::drc(&text).unwrap_or_default()
    };
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

    // SI needs the layout text for the geometry-bearing checks; a binary board
    // has none, so it gets the netlist-only subset (`None`). Route through the SI
    // chokepoint so the web report carries the trace-ampacity + input-cap-ripple
    // findings too — the bare `si_checks` left the web "Signal integrity" section
    // silently missing an under-width power trace the CLI `--si` flags.
    let si = crate::checks::engine_si(&board, &lib, text_view);
    let si_plain = plain_si(&si);

    let mut sections = vec![
        WebSection::from_plain("Copper spacing (DRC)", &drc_plain),
        WebSection::from_plain("Connectivity & wiring", &lint_plain),
        WebSection::from_plain("Signal integrity", &si_plain),
    ];

    // USB-C CC compliance: the CLI text/plain/json surfaces all carry this
    // verdict (a Serious shared-CC-pulldown fault gates `--check --strict`), but
    // the web persona used to omit it entirely — a board with the RPi-4 fault
    // read "Looks healthy". Fold it in so all four personas agree: a Serious
    // verdict becomes a serious WebFinding (raising serious/total), an Info
    // verdict becomes a heads-up (suppressing a false "Looks healthy").
    if let Some(section) = crate::usb_c_report(&board)
        .as_ref()
        .and_then(usbc_web_section)
    {
        sections.push(section);
    }

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
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> WebReport {
    // Always run the static analysis first. If it failed to even read the board,
    // there is no board to co-sim against; return the static error as-is.
    let mut report = analyze(file_name, contents);
    if !report.ok {
        return report;
    }

    let cosim = run_web_cosim(contents, fw_name, fw_bytes);
    // Fold SERIOUS co-sim faults into the top-level verdict: analyze() computed
    // serious/total/headline from the STATIC sections only, so a destructive
    // electrical fault the firmware co-sim produced (e.g. an overcurrent-killed
    // MOSFET) otherwise left the badge green and the headline "Looks healthy".
    // Only serious faults escalate the headline; benign co-sim notes stay in the
    // co-sim card without flipping the top-line verdict.
    let cosim_serious = cosim.findings.iter().filter(|f| f.level == "serious").count();
    if cosim_serious > 0 {
        report.serious += cosim_serious;
        report.total += cosim_serious;
        // total > 0 now, so the heads-up / bind-open arms of overall_headline are
        // not reached; passing false for them is correct.
        report.headline = overall_headline(report.total, report.serious, false, false);
    }
    report.cosim = Some(cosim);
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
        spi_framing: Vec::new(),
        boot_gates: Vec::new(),
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
fn run_web_cosim(contents: &[u8], fw_name: &str, fw_bytes: &[u8]) -> WebCosimSection {
    use crate::plain::plain_faults;
    use crate::stress::{FaultEvent, FaultKind};
    use hauksbee_server::engine::Engine;
    use std::io::Write;

    // No MCU => firmware drives nothing. Inspect the bound board before paying for
    // a temp file / engine build, and say so plainly. Same binary-first routing as
    // [`analyze`], so a binary board reaches the co-sim path uncorrupted too.
    let board = match ExtractedBoard::from_auto_bytes(contents)
        .unwrap_or_else(|| ExtractedBoard::from_auto(&String::from_utf8_lossy(contents)))
    {
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

    // Build the engine from the board we already extracted and bound above
    // (rather than re-extracting from text via `from_board_file`), so a binary
    // board — which has no text form to re-parse — co-sims like any other.
    let mut engine = match HauksbeeEngine::from_bound(bound, Some(tmp.path()), "web-firmware") {
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
        // Concatenate per-MCU UART in a STABLE (sorted-by-MCU-key) order — plain
        // `values()` is HashMap iteration order, so a multi-MCU board's merged
        // uart_output would interleave nondeterministically run-to-run. Mirrors
        // the CI runner's sorted-by-key concatenation.
        let mut uart_entries: Vec<_> = frame.uart.iter().collect();
        uart_entries.sort_by(|a, b| a.0.cmp(b.0));
        for (_, bytes) in uart_entries {
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
    // Rank by TOGGLE COUNT descending (name tiebreak), matching the CLI toggle
    // table and the JSON activity_summary — this field is documented as "top
    // movers first". Sorting by the driven flag then alphabetically (the old
    // order) dropped the genuinely most-active nets whenever more than 15 nets
    // were driven and kept quiet, alphabetically-early ones instead.
    let gpio_nets = top_gpio_nets(&sched.stats, &net_volts, 15);

    let uart_output = String::from_utf8_lossy(&last_uart).trim_end().to_string();

    // Boot power-up advisory, mirroring the CLI `run` (--json/--plain) so the web
    // surface carries the SAME safety warning. Without this, a board that drives a
    // MOSFET-gate/relay/igniter net HIGH and holds it from reset (no bias
    // resistor) reads as "no faults" on the web while the CLI warns it is
    // energised at power-up — the exact false-comfort divergence this path's own
    // honesty notes exist to prevent. The advisory is firmware-derived, so it is
    // computed from the finished co-sim's drive sets; `firmware_ran` reuses the
    // same zero-activity gate the notes above use.
    let firmware_ran = !(total_toggles == 0 && uart_empty && !any_gpio_driven);
    let boot_advisory = crate::checks::boot::analyze(
        &board,
        &sched.firmware_held_high_nets(),
        &sched.firmware_output_configured_nets(),
        &sched.firmware_driven_nets(),
        firmware_ran,
    );
    // Each held-high control net is a serious finding (a switched load possibly
    // energised at power-up), pushed BEFORE the electrical faults so it leads.
    for net in &boot_advisory.held_high_control_nets {
        findings.insert(
            0,
            WebFinding {
                level: "serious".to_string(),
                what: format!("Control net '{net}' may be energised at power-up"),
                why: format!(
                    "'{net}' drives a transistor/relay, is driven HIGH and held from power-up, \
                     and has no resistor setting a safe default. If a HIGH turns the switched \
                     load ON when it must stay OFF until firmware enables it, it is energised at \
                     power-up."
                ),
                fix: "Confirm the polarity is intended, or add a pull resistor that holds the \
                      gate at its safe (OFF) level until the firmware drives it."
                    .to_string(),
            },
        );
    }
    let boot_gates: Vec<WebBootGate> = boot_advisory
        .gate_states
        .iter()
        .map(|(reference, net, state)| WebBootGate {
            reference: reference.clone(),
            net: net.clone(),
            state: state.json().to_string(),
        })
        .collect();

    // Per-slave CS framing tier: exact | backend | heuristic (05 §2). Surfaced so
    // a JSON consumer can tell which slaves' transaction boundaries are real and
    // which are the chunk-boundary guess.
    let spi_framing: Vec<WebSpiFraming> = sched
        .spi_framing_modes()
        .into_iter()
        .map(|(bus, mode)| WebSpiFraming {
            bus,
            mode: mode.as_str().to_string(),
        })
        .collect();

    WebCosimSection {
        ran: true,
        seconds_simulated,
        uart_output,
        findings,
        gpio_nets,
        analog_valid,
        failed_windows,
        spi_framing,
        boot_gates,
    }
}

/// The single line at the top of the web report.
///
/// The web mirror of the USB-C CC compliance verdict, so the web persona carries
/// the same finding the CLI text/plain/json surfaces do (a Serious shared-CC
/// board must not read "Looks healthy" on the web). A Serious verdict becomes a
/// serious [`WebFinding`] (which raises the section's serious/total counts); an
/// Info verdict becomes a heads-up (which suppresses a false "Looks healthy"
/// without gating). `None` when there is no receptacle or the verdict is Ok.
fn usbc_web_section(usbc: &crate::checks::usb_c::UsbcReport) -> Option<WebSection> {
    let (what, why, fix) = usbc.web_gloss()?;
    let mut section = WebSection {
        title: "USB-C CC compliance".to_string(),
        verdict: String::new(),
        findings: Vec::new(),
        heads_up: Vec::new(),
    };
    if usbc.is_serious() {
        section.verdict = "problem".to_string();
        section.findings.push(WebFinding {
            level: "serious".to_string(),
            what,
            why,
            fix,
        });
    } else {
        section.verdict = "note".to_string();
        section.heads_up.push(WebHeadsUp { what, why, fix });
    }
    Some(section)
}

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

/// Serialize an [`analyze`] result to a JSON string for the HTTP layer. Board
/// bytes are passed raw so binary formats (Altium `.PcbDoc`) survive intact.
pub fn analyze_json(file_name: &str, contents: &[u8]) -> String {
    let report = analyze(file_name, contents);
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

/// Serialize an [`analyze_with_firmware`] result to a JSON string for the HTTP
/// layer (the `/api/analyze-with-firmware` endpoint). Board AND firmware bytes
/// are passed as `&[u8]` end-to-end — never lossy-decoded — so an uploaded
/// binary board or ELF stays intact.
pub fn analyze_with_firmware_json(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> String {
    let report = analyze_with_firmware(file_name, contents, fw_name, fw_bytes);
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

/// The web co-sim GPIO activity table: the `limit` most-active nets, ranked by
/// TOGGLE COUNT descending (name tiebreak) — the "top movers first" contract the
/// CLI toggle table and JSON activity_summary also honour. Ranking by the driven
/// flag then alphabetically (the old order) truncated away the genuinely
/// most-active nets whenever more than `limit` nets were driven, keeping quiet
/// alphabetically-early ones instead.
fn top_gpio_nets(
    stats: &std::collections::HashMap<String, crate::scheduler::NetStat>,
    net_volts: &std::collections::HashMap<String, f64>,
    limit: usize,
) -> Vec<WebGpioNet> {
    let mut ranked: Vec<_> = stats.iter().collect();
    ranked.sort_by(|a, b| b.1.toggles.cmp(&a.1.toggles).then(a.0.cmp(b.0)));
    ranked.truncate(limit);
    ranked
        .into_iter()
        .map(|(name, st)| WebGpioNet {
            name: name.clone(),
            volts: net_volts.get(name).copied().unwrap_or(0.0),
            driven: st.toggles > 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R15: the web GPIO table must keep the highest-TOGGLE nets and present them
    /// activity-first, matching the CLI/JSON surfaces — not the 15 alphabetically-
    /// earliest driven nets. A most-active net with a late-sorting name must
    /// survive truncation.
    #[test]
    fn gpio_nets_ranked_by_activity_not_name() {
        use crate::scheduler::NetStat;
        use std::collections::HashMap;
        let mut stats: HashMap<String, NetStat> = HashMap::new();
        // 'ZZ_CLK' is the most active; 16 quiet-but-driven 'A##' nets sort earlier.
        stats.insert("ZZ_CLK".to_string(), NetStat::with_toggles(9999));
        for i in 0..16 {
            stats.insert(format!("A{i:02}"), NetStat::with_toggles(1));
        }
        let net_volts: HashMap<String, f64> = HashMap::new();
        let top = top_gpio_nets(&stats, &net_volts, 15);
        assert_eq!(top.len(), 15);
        assert_eq!(top[0].name, "ZZ_CLK", "the top mover must lead, not be dropped");
        // Toggle counts are non-increasing down the ranked list.
        // (ZZ_CLK first, then the 1-toggle nets.)
        assert!(top.iter().all(|n| n.driven), "all 15 kept nets were driven");
    }

    const SHORTED: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    const BLUEPILL: &[u8] = include_bytes!("../../../testdata/boards/stm32_bluepill_demo.kicad_pcb");

    #[test]
    fn from_plain_carries_heads_up_and_serializes() {
        // Parity fix (the 171-ohm USB controlled-impedance case): an actionable
        // info note stored on PlainReport.heads_up must reach the web section,
        // even when the section verdict reads "healthy" (zero findings). This is
        // the exact note that used to vanish on the web while showing on --plain.
        let mut p = PlainReport::default();
        p.subject = "signal-integrity".to_string();
        p.heads_up.push(HeadsUp::glossed(
            "USB trace impedance is 171 ohm, +71% from target (90 ohm).",
            "reflections can make the link marginal",
            "match trace width and spacing to the stackup",
        ));
        let sect = WebSection::from_plain("Signal integrity", &p);
        assert!(sect.findings.is_empty(), "no findings, only a heads-up note");
        assert_eq!(sect.heads_up.len(), 1, "the heads-up note must survive");
        // The three-part gloss survives into the web shape.
        assert!(!sect.heads_up[0].why.is_empty(), "why must survive");
        assert!(!sect.heads_up[0].fix.is_empty(), "fix must survive");
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
    fn web_persona_carries_usbc_verdict_like_the_cli() {
        // R23 (web-drops-usbc-verdict): the web report used to omit the USB-C CC
        // compliance verdict entirely, so a board with the RPi-4 shared-CC-
        // pulldown fault (which the CLI text/plain/json all flag SERIOUS) could
        // read "Looks healthy" on the web. A Serious verdict must become a
        // serious section finding that raises the counts; an Info verdict must
        // become a heads-up that suppresses a false "Looks healthy".
        use crate::checks::usb_c::{Attach, UsbcLevel, UsbcReport};

        let serious = UsbcReport {
            receptacles: Vec::new(),
            shared_net: true,
            cc1_rd_ohms: Some(5100.0),
            cc2_rd_ohms: Some(5100.0),
            attach: Attach::AudioAccessory,
            powers_vbus: false,
            has_discrete_rd: true,
            level: UsbcLevel::Serious,
            headline: "CC1 and CC2 are the SAME net (RPi-4 fault).".to_string(),
        };
        let sect = super::usbc_web_section(&serious).expect("serious verdict yields a section");
        assert_eq!(sect.title, "USB-C CC compliance");
        assert!(
            sect.findings.iter().any(|f| f.level == "serious"),
            "a Serious verdict must be a serious finding"
        );
        // Folded into a sections vec, it raises serious/total and denies "healthy".
        let sections = vec![sect];
        let serious_n: usize = sections
            .iter()
            .map(|s| s.findings.iter().filter(|f| f.level == "serious").count())
            .sum();
        let total: usize = sections.iter().map(|s| s.findings.len()).sum();
        assert_eq!((serious_n, total), (1, 1));
        let headline = overall_headline(total, serious_n, false, false);
        assert!(
            headline.to_lowercase().contains("serious")
                && !headline.to_lowercase().contains("looks healthy"),
            "a serious USB-C fault must reach the headline: {headline}"
        );

        // Info verdict → a heads-up, still enough to suppress "Looks healthy".
        let info = UsbcReport {
            level: UsbcLevel::Info,
            has_discrete_rd: false,
            headline: "No discrete CC pulldown visible.".to_string(),
            ..serious
        };
        let isect = super::usbc_web_section(&info).expect("info verdict yields a section");
        assert!(isect.findings.is_empty(), "Info is not a finding");
        assert_eq!(isect.heads_up.len(), 1, "Info becomes a heads-up");
        let has_heads_up = [isect].iter().any(|s| !s.heads_up.is_empty());
        let h = overall_headline(0, 0, has_heads_up, false);
        assert!(
            !h.to_lowercase().contains("looks healthy"),
            "an Info USB-C note must deny a false healthy verdict: {h}"
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
        let r = analyze("nope.txt", b"this is not a board file at all");
        assert!(!r.ok);
        assert!(r.error.is_some());
        // The JSON wrapper still produces valid JSON.
        let json = analyze_json("nope.txt", b"garbage");
        assert!(json.contains("\"ok\":false"));
    }

    #[test]
    fn analyze_json_is_valid_json() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["sections"].as_array().unwrap().len() >= 3);
    }

    // A minimal binary Altium .PcbDoc (OLE2 container, two resistors sharing a
    // MID net), the deterministic fixture the extract crate's altium tests
    // synthesise. Binary on purpose: its bytes do NOT survive a lossy UTF-8
    // round-trip, which is exactly what the old web path performed.
    const ALTIUM: &[u8] = include_bytes!("../../../testdata/boards/altium_two_resistor.PcbDoc");

    #[test]
    fn binary_altium_board_survives_the_web_path() {
        // Regression (web bytes fix): the analyze path used to lossy-UTF8-decode
        // the upload and only ever try the TEXT sniffer, so a binary Altium
        // board was corrupted before parse AND never routed to its reader. Raw
        // bytes must now extract and report like any text board.
        let r = analyze("two_resistor.PcbDoc", ALTIUM);
        assert!(r.ok, "binary board must extract from raw bytes: {:?}", r.error);
        assert_eq!(r.num_components, 2, "R1 and R2 survive");
        assert!(r.num_nets > 0, "nets survive: {}", r.num_nets);
        // The guard that proves the bytes path is what makes it work: the SAME
        // board pushed through the lossy round-trip the old path performed is
        // mangled (the OLE2 magic is not valid UTF-8) and fails to read.
        let lossy = String::from_utf8_lossy(ALTIUM).into_owned();
        assert_ne!(lossy.as_bytes(), ALTIUM, "lossy decode must corrupt the container");
        let r2 = analyze("two_resistor.PcbDoc", lossy.as_bytes());
        assert!(!r2.ok, "the lossy view must NOT extract — bytes-first routing is load-bearing");
    }

    // Track D: web firmware drop zone.
    const NO_MCU: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/power_resistor.kicad_pcb");
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
            spi_framing: Vec::new(),
            boot_gates: Vec::new(),
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

    /// R18: the web co-sim section carries the boot-state panel STRUCTURALLY
    /// (mirroring the CLI `--json` `boot_gates`). A populated panel serializes;
    /// an empty one is omitted so the common JSON shape is backward-compatible.
    #[test]
    fn cosim_section_serializes_boot_gates_when_present() {
        let mut section = WebCosimSection {
            ran: true,
            seconds_simulated: 0.1,
            uart_output: String::new(),
            findings: Vec::new(),
            gpio_nets: Vec::new(),
            analog_valid: true,
            failed_windows: Vec::new(),
            spi_framing: Vec::new(),
            boot_gates: vec![WebBootGate {
                reference: "Q1".to_string(),
                net: "GATE_CTRL".to_string(),
                state: "driven_high".to_string(),
            }],
        };
        let json = serde_json::to_string(&section).unwrap();
        assert!(json.contains("\"boot_gates\""), "populated boot_gates serializes: {json}");
        assert!(json.contains("GATE_CTRL") && json.contains("driven_high"), "panel row present: {json}");
        // Empty => omitted (backward-compatible schema).
        section.boot_gates.clear();
        let json2 = serde_json::to_string(&section).unwrap();
        assert!(!json2.contains("\"boot_gates\""), "empty boot_gates omitted: {json2}");
    }

    /// R18 parity: the web firmware co-sim must surface the SAME boot power-up
    /// advisory as the CLI. boot_gate + variant-A firmware drives GATE_CTRL HIGH
    /// and holds it from reset with no bias resistor; the CLI emits a
    /// boot_control_net note + boot_gates panel (see cli_strict_plain.rs). The
    /// web section must carry both. Gated on `avr` because it boots AVR firmware
    /// on the in-process simavr backend (the renode/qemu build won't run it).
    #[cfg(feature = "avr")]
    #[test]
    fn web_cosim_carries_the_boot_advisory() {
        let json =
            analyze_with_firmware_json("boot_gate.kicad_pcb", SHORTED, "boot_gate.elf", BOOT_GATE_FW);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cosim = &v["cosim"];
        if cosim["ran"] != serde_json::json!(true) {
            eprintln!("skipping: boot_gate co-sim did not run in this build");
            return;
        }
        // The gate panel is present and names the driven-high gate.
        let gates = cosim.get("boot_gates").and_then(|g| g.as_array());
        assert!(
            gates.is_some_and(|g| !g.is_empty()),
            "boot_gates panel must be present on the web co-sim: {json:.400}"
        );
        // The held-high hazard leads the findings as a serious item.
        let findings = cosim["findings"].as_array().expect("findings array");
        assert!(
            findings
                .iter()
                .any(|f| f["what"].as_str().unwrap_or("").contains("energised at power-up")),
            "the held-high control net must surface as a serious finding: {json:.600}"
        );
    }
}
