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
use crate::result::{BindSummary, JsonNote, JsonNoteKind, Refusal};

/// A finding, in the shape the browser renders.
#[derive(Debug, Clone, Serialize)]
pub struct WebFinding {
    /// "serious" | "warning" | "note".
    pub level: String,
    pub what: String,
    pub why: String,
    pub fix: String,
    /// Board location (mm, layout space; same space as [`WebComponent`]
    /// positions) when the finding points at one physical spot. The browser
    /// renders a "show on board" affordance that pans the map there. Omitted
    /// from the JSON when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
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
        WebHeadsUp {
            what: h.what.clone(),
            why: h.why.clone(),
            fix: h.fix.clone(),
        }
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
            x: f.loc_mm.map(|l| l[0]),
            y: f.loc_mm.map(|l| l[1]),
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
    /// 171-ohm USB controlled-impedance note is the canonical example).
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
    /// `"M/N"`, active ICs that bound, over the total active ICs on the board.
    pub critical_parts_bound: String,
    /// References of verdict-critical parts left open on the live circuit:
    /// unresolved active ICs, unresolved discrete transistors (Q prefix, the
    /// power FETs; [`crate::result::is_verdict_critical`]), and
    /// resolved-but-open active ICs. Non-empty => analog/AC/thermal on those
    /// nets is not trustworthy; the web banner must say so loudly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_path_unresolved: Vec<String>,
    /// The parts behind that list, one entry each, with what went wrong and what
    /// it costs the analysis.
    ///
    /// The refs alone are enough for the banner but not for doing anything about
    /// it: the moment a user learns they need a model is the moment they are
    /// looking at this list, and offering "draft one from the datasheet" there
    /// needs the part's value (the MPN to extract) and whether it is missing a
    /// model at all (`bound`; a resolved-but-open part already has one, so
    /// drafting a second would not help).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_parts: Vec<WebOpenPart>,
}

/// One part on the open-active-path list, in the shape the browser renders.
#[derive(Debug, Clone, Serialize)]
pub struct WebOpenPart {
    pub reference: String,
    /// The board's value field: usually the manufacturer part number, and so the
    /// prefill for a datasheet extraction.
    pub value: String,
    /// Why it could not be bound, or the open-pin warning it carries.
    pub reason: String,
    /// What that does to the analysis, in one plain line.
    pub consequence: String,
    /// True for a reference the binder treats as an active IC (U/IC/MCU).
    pub active_ic: bool,
    /// True when the part DID bind and is merely open on the live circuit. Those
    /// parts need wiring or pin attention, not a model, so no extraction is
    /// offered for them.
    pub bound: bool,
}

impl BindSummaryWeb {
    /// Bridge a [`BindSummary`] into the web shape: keep the critical-parts ratio
    /// and the union of verdict-critical open parts, unresolved active ICs AND
    /// unresolved discrete transistors (the power FETs on a protection path),
    /// plus resolved-but-open active ICs. The transistor half uses the same
    /// [`crate::result::is_verdict_critical`] predicate the CLI INCONCLUSIVE
    /// verdict uses, so the browser can never print a clean bill over an
    /// unbound FET the CLI refuses to bless.
    pub fn from_summary(s: &BindSummary) -> Self {
        let mut active_path_unresolved: Vec<String> = s
            .active_path_unresolved
            .iter()
            .filter(|u| crate::result::is_verdict_critical(u))
            .chain(s.resolved_but_open_active.iter().filter(|u| u.active_ic))
            .map(|u| u.reference.clone())
            .collect();
        // Sorted, not report order: every surface that prints this list (the
        // web banner, the bind-role note) must agree on one order, and "U3,
        // U6, U2, U5, U1" reads as noise next to "U1, U2, U3, U5, U6".
        // Reference-natural order (prefix, then numeric index) so U10 does not
        // sort before U2.
        let ref_key = |r: &String| -> (String, u64) {
            let split = r.find(|c: char| c.is_ascii_digit()).unwrap_or(r.len());
            let (prefix, digits) = r.split_at(split);
            (prefix.to_string(), digits.parse().unwrap_or(0))
        };
        active_path_unresolved.sort_by_key(ref_key);
        active_path_unresolved.dedup();

        // Same two buckets, same active-IC filter, same order as the ref list
        // above, so the banner and the per-part detail can never name different
        // parts. `bound` is what separates the buckets: an unresolved part has no
        // model, a resolved-but-open one has a model and a wiring problem.
        let mut open_parts: Vec<WebOpenPart> = s
            .active_path_unresolved
            .iter()
            .filter(|u| crate::result::is_verdict_critical(u))
            .map(|u| (u, false))
            .chain(
                s.resolved_but_open_active
                    .iter()
                    .filter(|u| u.active_ic)
                    .map(|u| (u, true)),
            )
            .map(|(u, bound)| WebOpenPart {
                reference: u.reference.clone(),
                value: u.value.clone(),
                reason: u.reason.clone(),
                consequence: u.consequence.clone(),
                active_ic: u.active_ic,
                bound,
            })
            .collect();
        open_parts.sort_by_key(|p| ref_key(&p.reference));
        open_parts.dedup_by(|a, b| a.reference == b.reference);

        BindSummaryWeb {
            critical_parts_bound: s.critical_parts_bound.clone(),
            active_path_unresolved,
            open_parts,
        }
    }

    /// Whether an active IC is open on the live circuit; the predicate that must
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
    /// `boot_gates` / `--plain` gate panel so the web surface cannot give false
    /// comfort on a board that energises a switched load at power-up.
    /// Empty (and omitted) when the firmware ran no relevant gates, so the
    /// common JSON shape is unchanged for existing consumers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_gates: Vec<WebBootGate>,
    /// False when the firmware ran but produced zero GPIO toggles and no UART,
    /// it was not meaningfully exercised, so the run cannot vouch for firmware
    /// behaviour. Drives the headline demotion (a statically-clean board must not
    /// read "Looks healthy" over a co-sim that proved nothing). Always serialized
    /// (never skipped) so a consumer can rely on its presence.
    pub firmware_exercised: bool,
    /// True when the co-sim ran on a SUBSTITUTE chip (the board's real MCU was
    /// swapped for an emulatable stand-in), so its behaviour may not match the
    /// production part. Also demotes the headline. Always serialized.
    pub substituted: bool,
    /// Numerical qualification for a run that executed. Absent when no
    /// co-simulation ran or invalid solver settings caused qualification to be
    /// explicitly refused; a missing residual inside the budget means
    /// unmeasured, not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_budget: Option<hauksbee_ir::evidence::ErrorBudget>,
}

/// Additive JSON-only data for the synchronous firmware report. Keeping this
/// out of [`WebCosimSection`] preserves source compatibility for downstream
/// Rust code that constructs the planned 0.1 public struct with a literal;
/// [`analyze_with_firmware_json`] inserts these optional keys into the nested
/// `cosim` object for HTTP/MCP/frontend consumers.
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct WebFirmwareCoverage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_coverage: Vec<crate::scheduler::TimingCoverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_refusals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_windows: Vec<crate::result::CosimFallbackWindow>,
}

type WebCosimCoverage = WebFirmwareCoverage;

/// Source-compatible detailed result for embedded Rust consumers that need the
/// typed coverage fields inserted by [`analyze_with_firmware_json`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WebFirmwareAnalysis {
    pub report: WebReport,
    pub coverage: WebFirmwareCoverage,
}

impl WebFirmwareAnalysis {
    /// Serialize the detailed result in the same nested shape as the HTTP/MCP
    /// front door, including the additive coverage fields under `cosim`.
    pub fn to_json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(&self.report)?;
        if let Some(cosim) = value
            .get_mut("cosim")
            .and_then(serde_json::Value::as_object_mut)
        {
            if !self.coverage.timing_coverage.is_empty() {
                cosim.insert(
                    "timing_coverage".to_string(),
                    serde_json::to_value(&self.coverage.timing_coverage)?,
                );
            }
            if !self.coverage.timing_refusals.is_empty() {
                cosim.insert(
                    "timing_refusals".to_string(),
                    serde_json::to_value(&self.coverage.timing_refusals)?,
                );
            }
            if !self.coverage.fallback_windows.is_empty() {
                cosim.insert(
                    "fallback_windows".to_string(),
                    serde_json::to_value(&self.coverage.fallback_windows)?,
                );
            }
        }
        Ok(value)
    }
}

/// Evidence inputs captured before the co-sim scheduler is dropped. Keeping
/// these beside (but out of) the public presentation type prevents the web
/// path from trying to infer causal scope back from rendered prose.
struct WebCosimEvidence {
    faults: Vec<crate::stress::FaultEvent>,
    activity_nets: Vec<String>,
    scoped_substitutions: Vec<crate::scheduler::ScopedMcuSubstitution>,
    error_budget: Option<hauksbee_ir::evidence::ErrorBudget>,
    coverage: WebCosimCoverage,
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
    /// On the exact tier, where the chip-select came from: `"spec"`,
    /// `"model-roles"`, or `"bitbang-pins"`. Absent on the backend and heuristic
    /// tiers, where none was resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cs_provenance: Option<String>,
}

/// One sim-time window `[start_s, end_s)` where the analog co-sim solve failed to
/// converge and the run held stale voltages. Reported so the browser can show the
/// exact span whose electrical results are untrustworthy. Mirrors the CLI/JSON
/// `CosimFailedWindow`.
#[derive(Debug, Clone, Serialize)]
pub struct WebFailedWindow {
    pub start_s: f64,
    pub end_s: f64,
    /// The solver's own refusal message for this window, carrying the blame
    /// clause that names the net which refused to settle, the devices on it,
    /// and any near-zero-ohm link poisoning the matrix (E29). Never empty: a
    /// window with no recorded reason gets the generic march-did-not-advance
    /// line rather than a blank, because a blank reads as "no diagnosis
    /// available" when in fact one was simply dropped.
    pub reason: String,
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
    /// Whether the STATIC analysis left an undermined run-level claim (input
    /// coverage, bind completeness), computed with the same finding-backed
    /// exemption as the JSON verdict. Kept on the report so the firmware
    /// pass, which folds these maps in with its own simulation maps, can
    /// grade run-level invalidity without re-deriving the exemption from
    /// finding texts that were readable()-transformed for display.
    #[serde(skip)]
    pub run_level_undermined: bool,
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
    /// Content-addressed inputs consumed by the analysis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// First-class assumptions from the same evidence object as CLI JSON/plain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// Per-net evidence maps derived from actual board incidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
    /// Info-level notes (bind roles, coverage caveats) that must never be
    /// silently absent. Additive + `skip_serializing_if` so the `/api/analyze`
    /// JSON schema stays backward-compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<JsonNote>,
    /// Every net name on the board, sorted; the checks builder's pickers.
    /// Empty (and omitted) on an error report; additive for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nets: Vec<String>,
    /// reference -> resolved model kind ("mcu", "bjt_npn", ...), from the same
    /// bind the summary uses: what a component click on the board map reports
    /// as the part's bound model. A BTreeMap so the JSON is deterministic
    /// (the golden-parity test compares bytes). Additive; omitted when empty.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub component_kinds: std::collections::BTreeMap<String, String>,
    /// Binder-detected power supplies (rail net → nominal volts): the checks
    /// builder prefills `[[supply]]` rows from these; the same data
    /// `hauksbee-ci init` scaffolds from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supplies: Vec<WebSupply>,
    /// Firmware co-sim result, present only when [`analyze_with_firmware`] ran
    /// (a firmware file was dropped alongside the board). Additive +
    /// `skip_serializing_if` so the board-only `/api/analyze` path is byte-for-
    /// byte unchanged for existing consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosim: Option<WebCosimSection>,
    /// Present when a requested analysis could not support its intended claim.
    /// The static report remains available; this names the narrower refused
    /// claim, its prerequisite, the surviving conclusions, and the cheapest
    /// next action in the same shape as terminal and CI output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

/// A binder-detected power supply, as the checks builder consumes it.
#[derive(Debug, Clone, Serialize)]
pub struct WebSupply {
    pub net: String,
    pub volts: f64,
}

/// The "could not read the file" report shape, shared by every early-return in
/// [`analyze`] so the error surface stays consistent.
fn unreadable(file_name: &str, error: String) -> WebReport {
    WebReport {
        ok: false,
        error: Some(error),
        board_name: String::new(),
        file_name: file_name.to_string(),
        num_components: 0,
        num_nets: 0,
        headline: "Could not read the file.".to_string(),
        run_level_undermined: false,
        serious: 0,
        total: 0,
        sections: Vec::new(),
        components: Vec::new(),
        bind: None,
        inventory: Vec::new(),
        assumptions: Vec::new(),
        evidence: Vec::new(),
        notes: Vec::new(),
        nets: Vec::new(),
        component_kinds: std::collections::BTreeMap::new(),
        supplies: Vec::new(),
        cosim: None,
        refusal: None,
    }
}

/// Run the full front-door analysis on an uploaded board file.
///
/// `file_name` is used only for display and to disambiguate a `.board` (which
/// the normalizer routes to the Board-as-Code compiler). `contents` is the
/// file's RAW bytes: binary formats (Altium `.PcbDoc`, an OLE2 container) are
/// sniffed and read from the bytes first, exactly like the CLI `run` path, and
/// only a non-binary input falls back to the text sniffer over a lossy-UTF8
/// view (which is exact for the text formats). Decoding before this point
/// would corrupt a binary board before it was ever parsed. All of that routing
/// lives in [`crate::board_input::from_bytes`], the SAME normalizer the CLI
/// path uses, so the web and CLI surfaces can never disagree about what a
/// board file is.
pub fn analyze(file_name: &str, contents: &[u8]) -> WebReport {
    match crate::board_input::from_bytes(file_name, contents) {
        Ok(norm) => analyze_normalized(file_name, &norm).0,
        Err(e) => unreadable(file_name, e.web_message()),
    }
}

/// The static analysis on an already-normalized board. Split from [`analyze`]
/// so [`analyze_with_firmware`] can normalize ONCE and hand the same
/// [`crate::board_input::NormalizedBoard`] to both the static report and the
/// co-sim, instead of re-reading the original bytes with a weaker sniffer
/// (re-reading fails co-sim on any `.board` or gerber zip that has just
/// produced a clean static report).
///
/// Also returns the DRC report it computed, so the firmware path can bridge
/// the REAL copper shorts into the co-sim circuit instead of simulating a
/// board the DRC section (on the same page) says is shorted.
fn analyze_normalized(
    file_name: &str,
    norm: &crate::board_input::NormalizedBoard,
) -> (WebReport, hauksbee_extract::DrcReport) {
    analyze_normalized_with_ties(file_name, norm, None)
}

/// [`analyze_normalized`] with an optional companion schematic's declared net
/// ties.
///
/// Split out rather than folded into `analyze`, because the two entries have
/// genuinely different inputs. A file DROPPED on the web UI arrives as bytes with
/// no filesystem beside it, so there is no companion to read and the report keeps
/// the "supply the .sch" hint. A `--serve` run was pointed at a path, so the
/// schematic may be sitting right next to the board, and reading it there is what
/// carries schematic declarations as context without letting them excuse a
/// board location they cannot identify, matching the CLI contract.
fn analyze_normalized_with_ties(
    file_name: &str,
    norm: &crate::board_input::NormalizedBoard,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> (WebReport, hauksbee_extract::DrcReport) {
    let is_binary = norm.is_binary();
    let is_gerber = norm.is_gerber();
    let board = &norm.board;
    // Text view for the geometry-bearing text checks (DRC / SI). A binary or
    // gerber board has no KiCad layout text: those checks get their bytes twin
    // (Altium) or nothing, stated in the report rather than silently green.
    let text_view: Option<&str> = norm.layout_text.as_deref();

    let lib = ModelLibrary::builtin_with_user_dirs(&[]);

    // DRC reads copper geometry from the raw input: the bytes twin
    // (`altium_drc`) for a binary board, the KiCad layout text otherwise. A
    // gerber archive has neither; its DRC section says so below instead of
    // reporting a vacuous "no problems".
    let drc = if is_binary {
        ExtractedBoard::altium_drc(&norm.raw).unwrap_or_default()
    } else if is_gerber {
        Default::default()
    } else {
        ExtractedBoard::drc(text_view.unwrap_or_default()).unwrap_or_default()
    };
    let qualification = schematic_ties.map(|ties| ties.qualify(&drc));
    // Render from the grouped structure (single source of truth shared with the
    // CLI text/plain/json surfaces): duplicates collapsed, and gap==rule labelled
    // "at minimum clearance (no margin)" rather than the wrong "below the rule".
    let drc_structured = crate::result::DrcStructured::from_report_with_ties(
        &drc,
        qualification.as_ref(),
        norm.layout_text
            .as_deref()
            .is_some_and(|text| text.contains("<eagle"))
            && schematic_ties.is_none(),
    );
    let drc_plain = plain_drc_structured(&drc_structured);

    // Bind FIRST (also consumed below for the report panel and evidence): the
    // lint/SI section verdicts need the unmodelled-critical part list so the
    // web sections read INCONCLUSIVE, not "Looks healthy", over an unbound
    // main IC or power FET, the same contract as the CLI surfaces.
    let bound = bind_board(board, &lib);
    let bind_summary = BindSummary::from_report(&bound.report);
    let verdict_blockers = crate::result::unmodelled_critical_refs(&bind_summary);

    // Lint = the same bundle the `--lint` CLI surface assembles, via the single
    // `engine_lint` chokepoint (connectivity + strap lint + MCU resource conflicts
    // + the unchecked-strap-bearing-MCU coverage note), so the web report never
    // prints "Looks healthy" over a strap-bearing MCU whose BOOT0 was unexamined.
    let lint = crate::checks::engine_lint(board, &lib);
    let mut lint_plain = plain_netlint(&lint);
    lint_plain.unmodelled_critical = verdict_blockers.clone();

    // SI needs the layout text for the geometry-bearing checks; a binary board
    // has none, so it gets the netlist-only subset (`None`). Route through the SI
    // chokepoint so the web report carries the trace-ampacity + input-cap-ripple
    // findings too; the bare `si_checks` left the web "Signal integrity" section
    // silently missing an under-width power trace the CLI `--si` flags.
    let si = crate::checks::engine_si(board, &lib, text_view);
    let mut si_plain = plain_si(&si);
    si_plain.unmodelled_critical = verdict_blockers.clone();

    let mut sections = vec![
        if is_gerber {
            // An empty default DRC report would render "no copper spacing
            // problems found", a vacuous green for input we never checked.
            WebSection {
                title: "Copper spacing (DRC)".to_string(),
                verdict: format!(
                    "Not checked: clearance DRC needs the layout file \
                     (KiCad/Eagle/Altium), which {} does not carry.",
                    crate::board_input::input_kind_phrase(norm.kind)
                ),
                findings: Vec::new(),
                heads_up: Vec::new(),
            }
        } else {
            WebSection::from_plain("Copper spacing (DRC)", &drc_plain)
        },
        WebSection::from_plain("Connectivity & wiring", &lint_plain),
        WebSection::from_plain("Signal integrity", &si_plain),
    ];

    // USB-C CC compliance: the CLI text/plain/json surfaces all carry this
    // verdict (a Serious shared-CC-pulldown fault gates `--check --strict`), but
    // omitting it from the web persona lets a board with the RPi-4 fault
    // read "Looks healthy". Fold it in so all four personas agree: a Serious
    // verdict becomes a serious WebFinding (raising serious/total), an Info
    // verdict becomes a heads-up (suppressing a false "Looks healthy").
    if let Some(section) = crate::usb_c_report(board)
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

    // The bind-role honesty data the CLI/JSON surfaces carry, in the web
    // shape, from the SAME bind computed above the sections. The web dropping
    // this would let a board with every active IC open show "Looks healthy".
    let bind_web = BindSummaryWeb::from_summary(&bind_summary);
    // Reader notes plus the binder's own coverage gaps, so an overpower check
    // that never ran is visible on the human report, not only in CI.
    let mut notes = norm.notes.clone();
    notes.extend(bound.power_coverage_gaps());
    let evidence = match crate::evidence::BoardEvidence::from_bound(
        board,
        &bound.report,
        &notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )
    .and_then(|evidence| evidence.with_input_artifact(file_name, &norm.raw, norm.kind))
    {
        Ok(evidence) => evidence,
        Err(error) => {
            return (
                unreadable(file_name, format!("could not build evidence map: {error}")),
                drc,
            )
        }
    };
    let evidence = match (schematic_ties, &qualification) {
        (Some(ties), Some(qualification)) => match evidence.with_schematic_artifact(
            &ties.path,
            &ties.raw,
            ties.contribution(qualification),
        ) {
            Ok(evidence) => evidence,
            Err(error) => {
                return (
                    unreadable(
                        file_name,
                        format!("could not record schematic evidence: {error}"),
                    ),
                    drc,
                )
            }
        },
        _ => evidence,
    };
    let mut actual_findings = crate::result::lint_findings_json(&lint);
    actual_findings.extend(crate::result::si_findings_json(&si));
    actual_findings.extend(
        crate::usb_c_report(board)
            .as_ref()
            .and_then(crate::result::usbc_finding_json),
    );
    let mut actual_maps =
        match evidence.maps_for_drc_with_ties(&drc_structured, qualification.as_ref()) {
            Ok(maps) => maps,
            Err(error) => {
                return (
                    unreadable(file_name, format!("could not build DRC evidence: {error}")),
                    drc,
                )
            }
        };
    match evidence.maps_for_findings(&actual_findings) {
        Ok(maps) => actual_maps.extend(maps),
        Err(error) => {
            return (
                unreadable(
                    file_name,
                    format!("could not build finding evidence: {error}"),
                ),
                drc,
            )
        }
    }
    for (check, assertion) in [
        ("drc", "DRC input coverage"),
        ("si", "Signal-integrity input coverage"),
    ] {
        match evidence.check_coverage_map(check, assertion) {
            Ok(map) if map.status() != hauksbee_ir::evidence::EvidenceStatus::Clean => {
                actual_maps.push(map)
            }
            Ok(_) => {}
            Err(error) => {
                return (
                    unreadable(
                        file_name,
                        format!("could not build coverage evidence: {error}"),
                    ),
                    drc,
                )
            }
        }
    }
    if actual_maps.is_empty() {
        match evidence.static_coverage_map() {
            Ok(map) => actual_maps.push(map),
            Err(error) => {
                return (
                    unreadable(
                        file_name,
                        format!("could not build coverage evidence: {error}"),
                    ),
                    drc,
                )
            }
        }
    }
    let evidence = evidence.with_maps(actual_maps);

    // Notes: bind-role caveat (active IC open on the live circuit). These mirror
    // the CLI/JSON `notes` so the web never silently omits an honesty annotation.
    let mut notes: Vec<JsonNote> = Vec::new();
    // The reader's own coverage notes (an ODB++ job and an IPC-2581 document each
    // state where their connectivity came from and every cross-check inside the
    // file that disagreed), then the gerber note for the one input that really IS
    // reverse-extracted from copper. Saying "reverse-extracted from the fab
    // files' copper geometry" over an ODB++ job was simply false: that job states
    // its netlist, and the reader read it.
    for assumption in evidence
        .assumptions()
        .iter()
        .filter(|assumption| assumption.source() == hauksbee_ir::evidence::AssumptionSource::Reader)
    {
        notes.push(JsonNote {
            kind: JsonNoteKind::Coverage,
            message: assumption.because().to_string(),
        });
    }
    if norm.is_gerber_archive() {
        notes.push(JsonNote {
            kind: JsonNoteKind::Coverage,
            message: "Gerber input: the circuit was reverse-extracted from the fab \
                      files' copper geometry. Clearance DRC and trace-geometry SI need \
                      the original layout file and were not run."
                .to_string(),
        });
    }
    if bind_web.active_path_open() {
        notes.push(JsonNote {
            kind: JsonNoteKind::BindRole,
            message: format!(
                "Current-carrying / active part(s) left open on the live circuit: {}. Analog/AC/thermal results on their nets are NOT trustworthy.",
                bind_web.active_path_unresolved.join(", ")
            ),
        });
    }

    // Any heads-up note (e.g. the 171-ohm USB controlled-impedance note) is an
    // actionable observation, so the headline must NOT read "Looks healthy".
    let has_heads_up = sections.iter().any(|s| !s.heads_up.is_empty());
    let mut headline = overall_headline(total, serious, has_heads_up, bind_web.active_path_open());
    // Same run-level split as the JSON verdict: a finding-backed undermined
    // map is a badge on that finding, not run invalidity, so the headline only
    // escalates for undermined run-level claims (coverage, bind completeness).
    let run_level_undermined = crate::result::run_level_undermined(evidence.maps(), |a| {
        actual_findings.iter().any(|f| f.message == a)
    });
    // Fail beats invalid, but a non-serious warning must not: an undermined
    // run-level claim overrides the headline whenever nothing serious does.
    if serious == 0 && run_level_undermined {
        headline = "Evidence is undermined; results touching unresolved inputs are invalid for analysis. See the evidence below.".to_string();
    } else if total == 0 && evidence.has_caveats() {
        headline = "No blocking findings, but some evidence is qualified; see the evidence limitations below.".to_string();
    }

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

    // The checks builder's raw material: every net name (for its pickers) and
    // the binder-detected supplies (for its prefill); the same data
    // `hauksbee-ci init` scaffolds a spec from, so the web builder and the CLI
    // scaffold can never disagree about what powers the board.
    let mut nets: Vec<String> = board.nets.iter().map(|n| n.name.clone()).collect();
    nets.sort();
    nets.dedup();
    let mut supplies: Vec<WebSupply> = bound
        .supplies
        .iter()
        .map(|leg| WebSupply {
            net: leg.net_name.clone(),
            volts: leg.supply.nominal_volts(),
        })
        .collect();
    supplies.sort_by(|a, b| a.net.cmp(&b.net));
    supplies.dedup_by(|a, b| a.net == b.net);

    let report = WebReport {
        ok: true,
        error: None,
        board_name: board.name.clone(),
        file_name: file_name.to_string(),
        num_components: board.components.len(),
        num_nets: bound.net_names.len(),
        headline,
        run_level_undermined,
        serious,
        total,
        sections,
        components,
        bind: Some(bind_web),
        inventory: evidence.inventory().to_vec(),
        assumptions: evidence.assumptions().to_vec(),
        evidence: evidence.maps().to_vec(),
        notes,
        nets,
        component_kinds: bound
            .component_kinds
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        supplies,
        cosim: None,
        refusal: None,
    };
    (report, drc)
}

/// Run the full static analysis, then (when the board has a bound MCU on an
/// in-process backend) a short firmware co-sim, and attach the result as
/// [`WebReport::cosim`].
///
/// `fw_bytes` is the raw uploaded firmware (ELF/HEX); it is passed as `&[u8]`
/// and written verbatim to a temp file, NEVER lossy-decoded, which would
/// corrupt an ELF. The co-sim is skipped (with a friendly `cosim.ran = false`
/// note instead of an error) when:
///   * the board has no bound MCU (nothing to run firmware on), or
///   * any selected MCU uses an external emulator backend (Renode/QEMU): those
///     advance over a TCP control socket and can take 5-30s, well past a
///     browser/Axum request budget, and running only the in-process subset would
///     be false whole-board evidence, so the web path refuses the complete run, or
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
    analyze_with_firmware_detailed(file_name, contents, fw_name, fw_bytes).report
}

pub fn analyze_with_firmware_with_ties(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> WebReport {
    analyze_with_firmware_detailed_with_ties(file_name, contents, fw_name, fw_bytes, schematic_ties)
        .report
}

pub fn analyze_with_firmware_detailed(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> WebFirmwareAnalysis {
    analyze_with_firmware_detailed_with_ties(file_name, contents, fw_name, fw_bytes, None)
}

pub fn analyze_with_firmware_detailed_with_ties(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> WebFirmwareAnalysis {
    let (report, coverage) =
        analyze_with_firmware_parts(file_name, contents, fw_name, fw_bytes, schematic_ties);
    WebFirmwareAnalysis { report, coverage }
}

fn analyze_with_firmware_parts(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> (WebReport, WebCosimCoverage) {
    // Normalize ONCE; the static analysis and the co-sim share the same
    // extracted board. Re-reading the ORIGINAL bytes for co-sim with only the
    // text/binary sniffers fails with "could not re-read the board" on a
    // `.board` or gerber zip that has just produced a clean static report.
    let norm = match crate::board_input::from_bytes(file_name, contents) {
        Ok(n) => n,
        // No board to co-sim against; return the normalization error as-is.
        Err(e) => {
            return (
                unreadable(file_name, e.web_message()),
                WebCosimCoverage::default(),
            )
        }
    };
    let (mut report, drc) = analyze_normalized_with_ties(file_name, &norm, schematic_ties);
    let tie_qualification = schematic_ties.map(|ties| ties.qualify(&drc));

    // The firmware part may be a zip (a built tree, or a whole PlatformIO
    // project) rather than a bare image, resolve it first. A resolution
    // failure is a co-sim-tier problem, not a board problem: the static report
    // stands, with the reason in the co-sim card.
    let resolved = match crate::firmware_input::resolve_firmware_bytes(fw_name, fw_bytes) {
        Ok(r) => r,
        Err(msg) => {
            let cosim = cosim_unavailable(msg);
            let coverage = WebCosimCoverage::default();
            report.refusal = refusal_for_cosim(&cosim, &coverage);
            apply_refusal_headline(&mut report);
            report.cosim = Some(cosim);
            return (report, coverage);
        }
    };
    let (mut cosim, cosim_evidence) = run_web_cosim(
        &norm.board,
        file_name,
        &resolved.name,
        &resolved.bytes,
        &drc,
        tie_qualification.as_ref(),
    );
    let coverage = cosim_evidence
        .as_ref()
        .map(|captured| captured.coverage.clone())
        .unwrap_or_default();
    // Whether the evidence pass below parked the headline on run-level
    // invalidity; the fault fold must not demote that to a warning headline
    // (serious beats invalid beats warning).
    let mut evidence_invalid = false;
    if let Some(captured) = cosim_evidence {
        let lib = ModelLibrary::builtin();
        let bound = bind_board(&norm.board, &lib);
        let mut notes = norm.notes.clone();
        notes.extend(bound.power_coverage_gaps());
        let evidence_result = crate::evidence::BoardEvidence::from_bound(
            &norm.board,
            &bound.report,
            &notes,
            hauksbee_ir::evidence::RunDate::from_system_clock(),
        )
        .and_then(|evidence| evidence.with_input_artifact(file_name, &norm.raw, norm.kind))
        .and_then(|evidence| evidence.with_firmware_artifact(&resolved.name, &resolved.bytes))
        .and_then(|evidence| match (schematic_ties, &tie_qualification) {
            (Some(ties), Some(qualification)) => evidence.with_schematic_artifact(
                &ties.path,
                &ties.raw,
                ties.contribution(qualification),
            ),
            _ => Ok(evidence),
        });
        if let Ok(mut evidence) = evidence_result {
            evidence = match evidence
                .clone()
                .with_scoped_substitutions(&captured.scoped_substitutions)
            {
                Ok(evidence) => evidence,
                Err(_) => evidence,
            };
            if let Some(budget) = captured.error_budget.clone() {
                let mut maps = report.evidence.clone();
                for fault in &captured.faults {
                    if let Ok(map) = evidence.simulation_map(
                        format!(
                            "Firmware co-sim stress: {} {} = {:.6} (limit {:.6})",
                            fault.component,
                            fault.kind.as_str(),
                            fault.value,
                            fault.limit
                        ),
                        &[],
                        std::slice::from_ref(&fault.component),
                        Some(budget.clone()),
                    ) {
                        maps.push(map);
                    }
                }
                let fault_maps_end = maps.len();
                for scoped in &captured.scoped_substitutions {
                    let substitution = scoped.event();
                    if let Ok(map) = evidence.simulation_map(
                        format!(
                            "Firmware behaviour for {} executed on substitute core {}",
                            substitution.reference, substitution.modelled_core
                        ),
                        &[],
                        &[scoped.subject().to_string()],
                        Some(budget.clone()),
                    ) {
                        maps.push(map);
                    }
                }
                for net in &captured.activity_nets {
                    if let Ok(map) = evidence.simulation_map(
                        format!("Firmware co-sim activity on net {net}"),
                        std::slice::from_ref(net),
                        &[],
                        Some(budget.clone()),
                    ) {
                        maps.push(map);
                    }
                }
                evidence = evidence.with_maps(maps);
                report.inventory = evidence.inventory().to_vec();
                report.assumptions = evidence.assumptions().to_vec();
                report.evidence = evidence.maps().to_vec();
                // Same run-level split as everywhere else: the static
                // finding-backed maps folded in above must not invalidate the
                // firmware headline; the static run-level bit travels on the
                // report, and the simulation maps appended after the static
                // prefix are all run-level claims graded directly.
                // The per-fault maps are finding-backed (each fault is a
                // finding on this surface, wearing its own badge), so only
                // the substitution and activity maps after them are run-level
                // simulation claims. everything before
                // `fault_maps_end` is the static prefix plus the fault
                // segment; grade what follows it.
                let sim_undermined = evidence
                    .maps()
                    .get(fault_maps_end..)
                    .unwrap_or_default()
                    .iter()
                    .any(hauksbee_ir::evidence::EvidenceMap::is_undermined);
                if report.serious == 0 && (report.run_level_undermined || sim_undermined) {
                    report.headline = "Firmware evidence is undermined; substituted or unresolved inputs invalidate affected co-sim assertions.".to_string();
                    evidence_invalid = true;
                } else if report.total == 0 && evidence.has_caveats() {
                    report.headline = "No blocking findings, but firmware evidence is qualified; see the evidence limitations below.".to_string();
                }
            }
        }
    }
    if let Some(note) = &resolved.note {
        // Say which image actually ran when it came out of an archive/build,
        // so a wrong-env surprise is diagnosable from the report itself.
        cosim.findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: format!("Firmware resolved from '{fw_name}'."),
                why: note.clone(),
                fix: "Upload the exact .elf/.hex directly if this is not the image you meant."
                    .to_string(),
                x: None,
                y: None,
            },
        );
    }
    // Fold co-sim electrical FAULTS into the top-level verdict: analyze() computed
    // serious/total/headline from the STATIC sections only, so an electrical fault
    // the firmware co-sim produced otherwise left the badge green and the headline
    // "Looks healthy". A destructive fault (e.g. an overcurrent-killed MOSFET) is
    // SERIOUS; a non-destructive over-stress (a part carrying past its continuous
    // rating without dying) is a WARNING, but it is still an actionable issue the
    // CLI counts ("N issues found, none serious. Worth a look.") and that --strict
    // exits 2 on, so it must escalate the web headline too rather than sit silently
    // in the co-sim card under a "Looks healthy" banner. Only note-level honesty
    // caveats stay out of the count (they demote via cosim_caveat_headline below).
    if let Some((total, serious, headline)) =
        fold_cosim_faults(report.total, report.serious, &cosim)
    {
        let gained_serious = serious > report.serious;
        report.total = total;
        // Serious beats invalid beats warning: new serious faults own the
        // headline; a warning-only fold keeps an invalid-class headline in
        // place, whether it came from undermined run-level evidence or from
        // the bind contract (an unmodelled active part on the live path whose
        // INCONCLUSIVE refusal the static pass already printed).
        let bind_invalid = report
            .bind
            .as_ref()
            .is_some_and(|b| !b.active_path_unresolved.is_empty());
        if gained_serious || !(evidence_invalid || report.run_level_undermined || bind_invalid) {
            report.headline = headline;
        }
        report.serious = serious;
    } else if report.total == 0 {
        // No fault escalation and the static board is clean, but a co-sim that
        // proved nothing must not leave a bare "Looks healthy" headline.
        if let Some(demoted) = cosim_caveat_headline(&cosim, &coverage, &report.headline) {
            report.headline = demoted;
        }
    }
    report.refusal = refusal_for_cosim(&cosim, &coverage);
    apply_refusal_headline(&mut report);
    report.cosim = Some(cosim);
    (report, coverage)
}

fn apply_refusal_headline(report: &mut WebReport) {
    if report.refusal.is_some() && report.serious == 0 {
        report.headline = "Analysis invalid for the requested firmware co-simulation. Static board findings remain valid.".to_string();
    }
}

/// Translate a co-sim validity failure into the shared C5.3 refusal contract.
/// A firmware run that merely carries a substitute/exercise caveat is not an
/// exit-3-class refusal: only an unavailable run or invalid analog solve lands
/// here, matching the MCP and strict CLI semantics.
fn refusal_for_cosim(cosim: &WebCosimSection, coverage: &WebCosimCoverage) -> Option<Refusal> {
    if !cosim.ran {
        let finding = cosim.findings.first();
        let missing = finding
            .map(|f| f.why.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "the firmware co-simulation did not run".to_string());
        let next = finding
            .map(|f| f.fix.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                "provide a supported board and matching firmware, then rerun".to_string()
            });
        return Some(Refusal::new(
            "firmware and board co-simulation conclusions",
            missing,
            vec!["Static board analysis and its reported findings remain valid."],
            next,
        ));
    }

    if !cosim.analog_valid {
        let diagnosis = cosim
            .failed_windows
            .iter()
            .map(|window| window.reason.as_str())
            .find(|reason| !reason.is_empty())
            .unwrap_or("the analog solver did not converge in one or more simulation windows");
        let next = cosim
            .findings
            .iter()
            .find(|finding| finding.what.starts_with("Analog co-sim did not converge"))
            .map(|finding| finding.fix.clone())
            .unwrap_or_else(|| {
                "fix the first failed analog window, then rerun the same firmware co-simulation"
                    .to_string()
            });
        return Some(Refusal::new(
            "electrical conclusions across the complete firmware co-simulation",
            diagnosis,
            vec![
                "Static board analysis and its reported findings remain valid.",
                "Firmware loading and reported digital observations outside failed analog windows remain available.",
            ],
            next,
        ));
    }

    if let Some(diagnosis) = coverage.timing_refusals.first() {
        return Some(Refusal::new(
            "timing-sensitive firmware and electrical conclusions",
            diagnosis.clone(),
            vec![
                "Static board analysis and its reported findings remain valid.",
                "Non-timing-sensitive observations outside the refused replay remain available.",
            ],
            "reduce transitions per solver chunk (for example with a narrower --chunk-us), then rerun the same firmware",
        ));
    }

    None
}

/// Fold co-sim electrical FAULTS into the static verdict counts. A destructive
/// fault is SERIOUS; a non-destructive over-stress is a WARNING, both are
/// actionable issues (the CLI counts them and `--strict` exits 2 on them), so
/// both raise `total`; only serious ones raise `serious`. Note-level honesty
/// caveats are excluded (they demote separately via [`cosim_caveat_headline`]).
///
/// Returns the new `(total, serious, headline)` when any fault folds in, or
/// `None` when the co-sim carried no fault-level findings (caller keeps the
/// static counts). Because `total > 0` whenever this returns `Some`, the
/// heads-up / bind-open arms of [`overall_headline`] are unreachable, so passing
/// `false` for them is correct.
fn fold_cosim_faults(
    static_total: usize,
    static_serious: usize,
    cosim: &WebCosimSection,
) -> Option<(usize, usize, String)> {
    let serious = cosim
        .findings
        .iter()
        .filter(|f| f.level == "serious")
        .count();
    let warnings = cosim
        .findings
        .iter()
        .filter(|f| f.level == "warning")
        .count();
    if serious == 0 && warnings == 0 {
        return None;
    }
    let total = static_total + serious + warnings;
    let serious = static_serious + serious;
    Some((
        total,
        serious,
        overall_headline(total, serious, false, false),
    ))
}

/// The headline a statically-clean board (`total == 0`, no serious co-sim faults)
/// should carry once its firmware co-sim is known.
///
/// A co-sim that RAN yet proved nothing, firmware not meaningfully exercised,
/// ran on a SUBSTITUTE core, or an analog window failed to converge, must not
/// leave a bare "Looks healthy" verdict: that is the exact false comfort the
/// co-sim honesty notes exist to prevent, and it would contradict the CLI
/// `--plain` "worth a look" line for the same inputs. Such a run demotes to the
/// heads-up verdict (note-level only; the badge/serious count is unchanged).
///
/// Returns `None` (keep the existing headline) when no caveat applies: the gate
/// on `ran` keeps a not-run co-sim (no firmware / external backend) from
/// demoting, and only a bare "Looks healthy" line is ever overwritten.
fn cosim_caveat_headline(
    cosim: &WebCosimSection,
    coverage: &WebCosimCoverage,
    current_headline: &str,
) -> Option<String> {
    // A note-level co-sim finding (a boot held-high advisory, the analog-validity
    // caveat) is an actionable heads-up, not a counted fault: like the static
    // sections' `heads_up`, it must demote a bare "Looks healthy" to the heads-up
    // verdict without touching the serious/total badge.
    let has_note = cosim.findings.iter().any(|f| f.level == "note");
    let caveat = cosim.ran
        && (!cosim.firmware_exercised
            || cosim.substituted
            || !cosim.analog_valid
            || !coverage.timing_refusals.is_empty()
            || !coverage.fallback_windows.is_empty()
            || has_note);
    if caveat && current_headline == overall_headline(0, 0, false, false) {
        Some(overall_headline(0, 0, true, false))
    } else {
        None
    }
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
            x: None,
            y: None,
        }],
        gpio_nets: Vec::new(),
        // A co-sim that never ran cannot have failed an analog chunk: report the
        // clean, backward-compatible shape (valid, no windows).
        analog_valid: true,
        failed_windows: Vec::new(),
        spi_framing: Vec::new(),
        boot_gates: Vec::new(),
        // ran == false: the headline demotion is gated on `ran`, so these remain
        // neutral defaults inside the co-sim section. The containing WebReport's
        // refusal field, not these compatibility booleans, carries the refusal.
        firmware_exercised: true,
        substituted: false,
        error_budget: None,
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
    // The DIAGNOSIS, not just the span. "10 chunks failed" sent a user bisecting
    // a 259-part board by model class; the solver's own refusal message names the
    // net that refused to settle, the devices on it, and any element whose
    // conductance is outside the board's own distribution (E29). Deduplicated,
    // because ten failed windows on one bad jumper should say it once.
    let mut seen: Vec<&str> = Vec::new();
    for w in windows {
        if !seen.contains(&w.reason.as_str()) {
            seen.push(&w.reason);
        }
    }
    let diagnosis = if seen.is_empty() {
        String::new()
    } else {
        format!(" The solver's diagnosis: {}.", seen.join(" | "))
    };
    let chunk_word = if failed_chunks == 1 {
        "chunk"
    } else {
        "chunks"
    };
    // Note-level, NOT serious: analog non-convergence is a co-sim HONESTY caveat,
    // not a board defect. Like its sibling caveats (substitute core, firmware not
    // exercised, both note-level) it must DEMOTE the headline off "Looks healthy"
    // via `cosim_caveat_headline` (whose `!analog_valid` term relies on this) and
    // must NOT fold into the serious/total fault count in `fold_cosim_faults`.
    // Marking it "serious" made the web report a phantom board fault on a run
    // where only the solver failed to converge, contradicting both this contract
    // and the CLI `--plain` path, which surfaces it as a heads-up note. The prose
    // below stays loud (it leads the co-sim card); only the severity is honest.
    WebFinding {
        level: "note".to_string(),
        what: "Analog co-sim did not converge: electrical results are not trustworthy".to_string(),
        why: format!(
            "The analog solver failed on {failed_chunks} {chunk_word} covering {spans}, \
             and no fallback integration rung (reduced step, backward Euler, cold-start \
             continuation, subdivision) could carry them. Over those windows the co-sim \
             held stale node voltages instead of a real solve, so any voltage, current or \
             fault reading there is fiction (see {}: refuse rather than fake).{diagnosis}",
            hauksbee_ir::docs_url("docs/about/LIMITATIONS.md"),
        ),
        fix: "Treat electrical results inside those windows as unknown. A stiff or \
              structurally singular section (conflicting rails, an unconverging \
              nonlinear stage) usually causes it; simplify the offending section or \
              relax the operating point, then re-run."
            .to_string(),
        x: None,
        y: None,
    }
}

/// The actual firmware co-sim for the web path. Mirrors `run_headless` in the
/// CLI (same fixed 1 kHz frame cadence, same fault dedup via `plain_faults`) but
/// runs synchronously for a short fixed window so it fits a request budget.
///
/// Takes the ALREADY-extracted board from the caller's normalization pass
/// (never re-reads the upload): every format the static analysis accepts,
/// `.board` exports and gerber zips included, reaches co-sim identically.
/// The exact terminal command that runs this co-sim outside the web report,
/// with the REAL uploaded file names, never placeholders (a `<board>`
/// placeholder is an instruction the user cannot follow). The binary is named
/// by its full running path: an app user has no `hauksbee` on PATH (it lives
/// in Hauksbee.app/Contents/Resources/bin), and the serving executable IS that
/// binary, so its own path is the one command guaranteed to exist on this
/// machine.
fn cli_cosim_command(board_file: &str, fw_file: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "hauksbee".to_string());
    format!("\"{exe}\" run \"{board_file}\" --firmware \"{fw_file}\" --headless")
}

/// Project ordinary coverage limitations/events onto the web finding cards.
/// Strict timing refusal deliberately stays out: it already has the structural
/// `timing_refusals` field and the typed [`Refusal`] contract, so projecting it
/// here would render the same invalidity again as a lower-severity note.
fn coverage_findings_for_web(
    caveats: &[crate::reports::coverage::CoverageCaveat],
) -> Vec<WebFinding> {
    use crate::reports::coverage::CoverageClass;

    caveats
        .iter()
        .filter(|c| {
            matches!(
                c.class,
                CoverageClass::AdcDropped
                    | CoverageClass::UnexercisedBus
                    | CoverageClass::WatchdogLimitation
                    | CoverageClass::WatchdogReboot
                    | CoverageClass::TimingLimitation
            )
        })
        .map(|c| WebFinding {
            level: "note".to_string(),
            what: c.headline.clone(),
            why: c.message.clone(),
            fix: c.fix.clone(),
            x: None,
            y: None,
        })
        .collect()
}

fn run_web_cosim(
    board: &ExtractedBoard,
    board_file_name: &str,
    fw_name: &str,
    fw_bytes: &[u8],
    drc: &hauksbee_extract::DrcReport,
    tie_qualification: Option<&hauksbee_extract::DrcTieQualification>,
) -> (WebCosimSection, Option<WebCosimEvidence>) {
    use crate::plain::plain_faults;
    use crate::stress::{FaultEvent, FaultKind};
    use hauksbee_server::engine::Engine;
    use std::io::Write;

    /// Simulated window this synchronous report runs, mirroring run_headless's
    /// short fixed window. Named here because the external-emulator refusal
    /// below quotes it: the honest reason that path is skipped is that 0.2 s
    /// of simulated time is less than such a chip's boot ROM needs, so the
    /// user would be shown a processor that has not started.
    const SECONDS: f64 = 0.2;

    // No MCU => firmware drives nothing. Inspect the bound board before paying
    // for a temp file / engine build, and say so plainly.
    let lib = ModelLibrary::builtin();
    let bound = bind_board(board, &lib);
    if bound.mcus.is_empty() {
        return (cosim_unavailable(
            "No microcontroller was found on this board; the firmware co-sim needs an MCU to run on.",
        ), None);
    }
    // Web path is in-process (simavr) only: external emulator backends
    // (Renode/QEMU) advance over a TCP socket, and their boot alone takes tens
    // of seconds, past a browser request budget. Skip them here with an honest
    // note that names the working alternatives WITH the real file names (the
    // live sim runs these backends fine; only this synchronous quick report
    // cannot wait for them).
    let external_mcus: Vec<_> = bound
        .mcus
        .iter()
        .filter(|m| m.backend.starts_with("renode:") || m.backend.starts_with("qemu:"))
        .collect();
    if !external_mcus.is_empty() {
        let selected = external_mcus
            .iter()
            .map(|m| format!("{} ({})", m.reference, m.backend))
            .collect::<Vec<_>>()
            .join(", ");
        let mut section = cosim_unavailable(format!(
            "This board selects at least one MCU that requires an external emulator: \
             {selected}. The synchronous web run cannot execute only a subset of the \
             selected cores and present it as whole-board firmware evidence. This quick \
             report simulates only a {SECONDS:.1} s window, and a chip like this one \
             spends several simulated seconds in its boot ROM before your code starts, \
             so the report would show a processor that has not booted yet. Running it \
             long enough is a job of tens of wall-clock seconds, which is why it is not \
             done inside a page load. The complete co-sim works, two ways: open the live \
             sim (\"Drive it live\") to boot this firmware here in the app, or run it \
             from a terminal, from the folder holding your files: {}",
            cli_cosim_command(board_file_name, fw_name)
        ));
        // The generic fix line ("drop a board with an in-process MCU") would be
        // wrong advice here: the board is fine, only the quick report cannot
        // host the emulator.
        if let Some(f) = section.findings.first_mut() {
            f.fix = "If the emulator is not installed yet, the Environment page installs \
                     it with one click."
                .to_string();
        }
        return (section, None);
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
        Err(e) => {
            return (
                cosim_unavailable(format!("Could not stage the firmware for co-sim: {e}.")),
                None,
            )
        }
    };
    if let Err(e) = tmp.write_all(fw_bytes).and_then(|_| tmp.flush()) {
        return (
            cosim_unavailable(format!("Could not write the firmware for co-sim: {e}.")),
            None,
        );
    }

    // Build the engine from the board we already extracted and bound above
    // (rather than re-extracting from text via `from_board_file`), so a binary
    // board, which has no text form to re-parse, co-sims like any other.
    let mut engine = match HauksbeeEngine::from_bound(bound, Some(tmp.path()), "web-firmware") {
        Ok(e) => e,
        // Architecture mismatch / corrupt firmware: the static analysis still
        // succeeded, so report co-sim failure as a note, not a hard error.
        Err(e) => {
            return (
                cosim_unavailable(format!(
                    "The firmware could not be loaded onto this board's MCU: {e}. \
                 (Check the firmware matches the MCU's architecture.)"
                )),
                None,
            )
        }
    };

    // Bridge the DRC's REAL copper shorts into the circuit before stepping.
    // The static report on the same page names these shorts; a co-sim that
    // quietly simulated the un-shorted board would then show healthy rails
    // right under a "GND and +5V are touching" finding, a silent
    // contradiction. Applying the bridge makes the rails below reflect the
    // board as it would actually be built. Skipped on an unvalidated format
    // (KiCad 10+ `version_warning`): those shorts may be phantom, and bridging
    // a phantom short would corrupt an otherwise-honest co-sim (CI gates skip
    // them for the same reason).
    let shorts_applied = if drc.version_warning.is_none() {
        engine.apply_drc_shorts_with_qualification(drc, tie_qualification)
    } else {
        0
    };

    // Short fixed window mirroring run_headless: 1 kHz frame cadence.
    let seconds = SECONDS;
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    let mut last_uart: Vec<u8> = Vec::new();
    let mut faults: Vec<FaultEvent> = Vec::new();
    while t < seconds {
        let frame = engine.step(frame_dt);
        // Concatenate per-MCU UART in a STABLE (sorted-by-MCU-key) order, plain
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
    // Say plainly that the shorted copper was simulated as shorted, so the
    // rails/GPIO table below cannot be read as "the shorted board is fine".
    if shorts_applied > 0 {
        let connection_word = if shorts_applied == 1 {
            "connection"
        } else {
            "connections"
        };
        findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: format!(
                    "This co-sim ran WITH the board's {shorts_applied} physical copper {connection_word} \
                     bridged into the circuit."
                ),
                why: "The DRC section above determines whether a contact has board-local physical \
                      authorization. The co-sim applies every validated-format copper contact \
                      before simulating, so the rail and GPIO voltages reflect the board as built; \
                      schematic net names alone do not suppress short faults."
                    .to_string(),
                fix: "Follow the DRC section: repair serious contacts, or provide board-local \
                      authority for the exact intended join."
                    .to_string(),
                x: None,
                y: None,
            },
        );
    }
    // A net whose voltage is decided by something other than what the user
    // asked for: two ideal sources pinning one net, or a post-solve override on
    // top of a stamped source. Loud, and it names both contenders and the winner
    // (E30). Note-level for the same reason as the substitution caveat: it is an
    // honesty caveat about what the run means, not a board defect.
    for msg in sched.drive_conflicts() {
        findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: "A requested drive was overridden on its net".to_string(),
                why: msg,
                fix: "Remove the losing source, or suppress the rail that pins the net, so                       the drive you asked for is the one that takes effect."
                    .to_string(),
                x: None,
                y: None,
            },
        );
    }
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
                x: None,
                y: None,
            },
        );
    }
    // Runtime driver contention (the model-vs-MCU case the static lint
    // documents as out of its reach): a real electrical fight, so it is a
    // SERIOUS finding that folds into the fault counts, not a caveat.
    for c in sched.driver_contentions() {
        findings.insert(
            0,
            WebFinding {
                level: "serious".to_string(),
                what: format!("Driver contention on net '{}'", c.net),
                why: c.message(),
                fix: "Check the model pin mapping (`hauksbee models resolve`) and the \
                      firmware's pin-direction writes; two push-pull drivers must never \
                      share a net without a series element."
                    .to_string(),
                x: None,
                y: None,
            },
        );
    }
    // Sub-chunk pulses swallowed by tick-evaluated sequential parts (friction
    // 1.16): a co-sim FIDELITY caveat (the board may be fine; the result is
    // not trustworthy), so note-level, which demotes the headline the same way
    // the other honesty caveats do.
    for p in sched.short_pulses() {
        findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: format!(
                    "A GPIO pulse on net '{}' is too short for the co-sim to observe",
                    p.net
                ),
                why: p.message(),
                fix: "Rerun from the command line with --chunk-us at or below half the \
                      pulse width, or widen the pulse in firmware."
                    .to_string(),
                x: None,
                y: None,
            },
        );
    }
    // The co-sim coverage caveats this surface did not carry, on the same
    // note-level `WebFinding` mechanism as the ones it already did (short
    // pulses, driver contention, drive conflicts).
    //
    // Watchdog reboots were reachable here and silent: `simavr`'s watchdog does
    // bite and `Mcu::watchdog_resets` counts the reboots, so a run whose
    // firmware was rebooted mid-window read quiet on the web while `hauksbee
    // run` warned about it. Behaviour after a reboot belongs to a rebooted core,
    // so it demotes the headline like every other honesty caveat here.
    //
    // Dropped ADC injections, unexercised buses, watchdog limitations and timing
    // limitations are structurally empty on this path, which co-sims AVR in
    // process: `simavr` has an exact ADC injection map, decodes TWI and SPI
    // natively, and reports neither limitation. They are wired anyway rather
    // than skipped, so a backend added to this path later cannot make them
    // silent again.
    //
    // Sourced from `reports::coverage`, the one enumeration the batch surfaces'
    // wording comes from, so a sentence here is the sentence `--json` carries.
    // Per-core timing coverage is deliberately NOT in this list: it is a
    // resolution statement present on every run with a live core, so a finding
    // would demote every healthy report's headline. It rides the structural
    // `timing_coverage` field below instead, the tier `--json` gives it and the
    // mechanism `spi_framing` already uses on this surface.
    {
        use crate::reports::coverage::CoverageInputs;
        let caveats = CoverageInputs::from_scheduler(sched).caveats();
        for finding in coverage_findings_for_web(&caveats) {
            // Note-level for the same reason as the substitution and short-pulse
            // caveats: these qualify what the run means rather than inventing a
            // board defect. Strict-invalid timing refusals are not in this list.
            findings.insert(0, finding);
        }
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
                fix: "Confirm the firmware matches this MCU and actually drives I/O.".to_string(),
                x: None,
                y: None,
            },
        );
    }
    // Analog-validity refusal (05 §3b): if any chunk's analog solve failed, the
    // web report is the surface most likely to give false comfort (an empty
    // findings list reads as "no faults"). Surface it BOTH as a loud prepended
    // finding (so it leads the prose) and as the structural `analog_valid` /
    // `failed_windows` fields below (so a JSON consumer reads it as data). This
    // parallels the CLI `--json` and the TUI pane; a web path that consults
    // neither `analog_valid()` nor `failed_windows()` shows a diverged run as
    // quiet.
    let analog_valid = sched.analog_valid();
    let reasons = sched.failed_window_reasons();
    let failed_windows: Vec<WebFailedWindow> = sched
        .failed_windows()
        .iter()
        .enumerate()
        .map(|(i, &(start_s, end_s))| WebFailedWindow {
            start_s,
            end_s,
            reason: reasons
                .get(i)
                .cloned()
                .unwrap_or_else(|| "analog march did not advance".to_string()),
        })
        .collect();
    if !analog_valid {
        findings.insert(
            0,
            analog_invalid_finding(sched.failed_chunk_count(), &failed_windows),
        );
    }
    let net_volts = sched.net_voltages();
    // Rank by TOGGLE COUNT descending (name tiebreak), matching the CLI toggle
    // table and the JSON activity_summary; this field is documented as "top
    // movers first". Sorting by the driven flag then alphabetically (the old
    // order) dropped the genuinely most-active nets whenever more than 15 nets
    // were driven and kept quiet, alphabetically-early ones instead.
    let gpio_nets = top_gpio_nets(&sched.stats, &net_volts, 15);

    let uart_output = String::from_utf8_lossy(&last_uart).trim_end().to_string();

    // Boot power-up advisory, mirroring the CLI `run` (--json/--plain) so the web
    // surface carries the SAME safety warning. Without this, a board that drives a
    // MOSFET-gate/relay/igniter net HIGH and holds it from reset (no bias
    // resistor) reads as "no faults" on the web while the CLI warns it is
    // energised at power-up; the exact false-comfort divergence this path's own
    // honesty notes exist to prevent. The advisory is firmware-derived, so it is
    // computed from the finished co-sim's drive sets; `firmware_ran` reuses the
    // same zero-activity gate the notes above use.
    let firmware_ran = !(total_toggles == 0 && uart_empty && !any_gpio_driven);
    let boot_advisory = crate::checks::boot::analyze(
        board,
        &sched.firmware_held_high_nets(),
        &sched.firmware_output_configured_nets(),
        &sched.firmware_driven_nets(),
        firmware_ran,
    );
    // Each held-high control net is an ADVISORY (note-level) heads-up; the same
    // status every CLI surface gives it: `--plain` renders it as a "worth a look"
    // note, `--json` as a boot_control_net note, and `--strict` does not fail on
    // it. It is a real, actionable observation (a switched load possibly energised
    // at power-up) but not a confirmed board fault, so it must NOT fold into the
    // web serious/total count and flip the headline to "fix the serious ones"; it
    // demotes a bare "Looks healthy" to the heads-up verdict via
    // `cosim_caveat_headline`. Pushed BEFORE the electrical faults so it leads.
    for net in &boot_advisory.held_high_control_nets {
        findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
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
                x: None,
                y: None,
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
            cs_provenance: mode.cs_provenance().map(|p| p.as_str().to_string()),
        })
        .collect();

    // Per-core timing coverage, read from the same accessor the CLI `--json`
    // field reads (see the field's doc comment for why it is a field and not a
    // finding).
    let timing_coverage = sched.timing_coverage();
    let timing_refusals = sched.timing_refusals().to_vec();
    let fallback_windows = sched
        .fallback_windows()
        .iter()
        .map(|window| crate::result::CosimFallbackWindow {
            start_s: window.start_s,
            end_s: window.end_s,
            method: window.method.as_str().to_string(),
            fidelity_note: window.method.fidelity_note().to_string(),
            error_estimate_v: window.error_estimate_v,
        })
        .collect();

    let error_budget = match sched.error_budget() {
        Ok(budget) => Some(budget),
        Err(error) => {
            findings.insert(
                0,
                WebFinding {
                    level: "serious".to_string(),
                    what: "Co-sim result has no valid numerical qualification.".to_string(),
                    why: error.to_string(),
                    fix: "Use finite, positive solver tolerances and run the co-simulation again."
                        .to_string(),
                    x: None,
                    y: None,
                },
            );
            None
        }
    };
    let captured = WebCosimEvidence {
        faults,
        activity_nets: gpio_nets.iter().map(|net| net.name.clone()).collect(),
        scoped_substitutions: sched.scoped_substitutions().to_vec(),
        error_budget: error_budget.clone(),
        coverage: WebCosimCoverage {
            timing_coverage,
            timing_refusals,
            fallback_windows,
        },
    };
    let substituted = !captured.scoped_substitutions.is_empty();
    (
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
            firmware_exercised: firmware_ran,
            substituted,
            error_budget,
        },
        Some(captured),
    )
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
    // The verdict must be a full sentence, matching every other section (which is
    // built via `WebSection::from_plain`, whose verdict is `PlainReport::verdict()`
    // prose). Bare tokens like "problem"/"note" made this one folded-in section
    // carry an incompatible string format in the same `/api/analyze` payload and
    // rendered a lone word under the USB-C card while siblings showed sentences.
    if usbc.is_serious() {
        section.verdict = "1 issue found, 1 serious.".to_string();
        section.findings.push(WebFinding {
            level: "serious".to_string(),
            what,
            why,
            fix,
            x: None,
            y: None,
        });
    } else {
        section.verdict =
            "No usb-c cc compliance failures, but 1 thing worth a look (see below).".to_string();
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
            return "No blocking issues, but active parts are unresolved; analog/AC/thermal results are not trustworthy. See the notes.".to_string();
        }
        if has_heads_up {
            return "No problems found, but there is something worth knowing; see the heads-up note below.".to_string();
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
    analyze_json_with_ties(file_name, contents, None)
}

/// [`analyze_json`] for a board read from a PATH, where a companion Eagle `.sch`
/// may sit beside it. Used by `run --serve`, whose preloaded report must agree
/// with what `--drc` and `--check` say about the same board on disk.
pub fn analyze_json_with_ties(
    file_name: &str,
    contents: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> String {
    let report = match crate::board_input::from_bytes(file_name, contents) {
        Ok(norm) => analyze_normalized_with_ties(file_name, &norm, schematic_ties).0,
        Err(e) => unreadable(file_name, e.web_message()),
    };
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

/// Serialize an [`analyze_with_firmware`] result to a JSON string for the HTTP
/// layer (the `/api/analyze-with-firmware` endpoint). Board AND firmware bytes
/// are passed as `&[u8]` end-to-end, never lossy-decoded, so an uploaded
/// binary board or ELF stays intact.
pub fn analyze_with_firmware_json(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> String {
    analyze_with_firmware_json_with_ties(file_name, contents, fw_name, fw_bytes, None)
}

pub fn analyze_with_firmware_json_with_ties(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> String {
    let analysis = analyze_with_firmware_detailed_with_ties(
        file_name,
        contents,
        fw_name,
        fw_bytes,
        schematic_ties,
    );
    let value = match analysis.to_json_value() {
        Ok(value) => value,
        Err(e) => return format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}"),
    };
    serde_json::to_string(&value).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

/// The web co-sim GPIO activity table: the `limit` most-active nets, ranked by
/// TOGGLE COUNT descending, then VOLTAGE RANGE descending, then name; the exact
/// three-key "top movers first" contract the CLI toggle table and JSON
/// activity_summary use (see `reports::cosim`). Ranking by the driven flag then
/// alphabetically truncates away the genuinely most-active nets whenever more
/// than `limit` nets are driven; dropping the voltage-range middle key makes the
/// web keep a DIFFERENT subset than the CLI/JSON at the truncation boundary when
/// nets tie on toggle count.
fn top_gpio_nets(
    stats: &std::collections::HashMap<String, crate::scheduler::NetStat>,
    net_volts: &std::collections::HashMap<String, f64>,
    limit: usize,
) -> Vec<WebGpioNet> {
    let mut ranked: Vec<_> = stats.iter().collect();
    ranked.sort_by(|a, b| {
        b.1.toggles
            .cmp(&a.1.toggles)
            .then(
                (b.1.max_v - b.1.min_v)
                    .partial_cmp(&(a.1.max_v - a.1.min_v))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.0.cmp(b.0))
    });
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

    #[cfg(feature = "avr")]
    #[test]
    fn web_cosim_keeps_a_schematic_only_contact_as_a_short_fault() {
        let board_text = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "+5V")
  (net 2 "GND")
  (net 3 "D13")
  (footprint "Package_QFP:TQFP-32_7x7mm_P0.8mm" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1" (at 0 0))
    (property "Value" "ATmega328P" (at 0 0))
    (pad "7" smd rect (at -3 0) (size 0.5 0.5) (layers "F.Cu") (net 1))
    (pad "8" smd rect (at -3 1) (size 0.5 0.5) (layers "F.Cu") (net 2))
    (pad "19" smd rect (at 3 0) (size 0.5 0.5) (layers "F.Cu") (net 3)))
  (segment (start 100 100) (end 110 100) (width 0.5) (layer "F.Cu") (net 2))
  (segment (start 105 95) (end 105 105) (width 0.5) (layer "F.Cu") (net 3))
)"#;
        let board = ExtractedBoard::from_kicad_pcb(board_text).expect("board parses");
        let drc = ExtractedBoard::drc(board_text).expect("DRC runs");
        assert_eq!(drc.short_count(), 1, "the fixture adds one GND/D13 contact");
        let declarations = vec![hauksbee_extract::DeclaredNetTie {
            net: "GND".into(),
            tied_net: "D13".into(),
            symbol: "GND1".into(),
            tied_to: vec!["D13_TIE".into()],
        }];
        let qualification = drc.qualify_with_declared_ties("blinky.sch", &declarations);
        assert_eq!(qualification.qualified_count(), 0);

        let (cosim, captured) = run_web_cosim(
            &board,
            "blinky.kicad_pcb",
            "nowdt.elf",
            include_bytes!("../../../testdata/firmware/avr_watchdog/nowdt.elf"),
            &drc,
            Some(&qualification),
        );
        assert!(cosim.ran, "the tracked AVR firmware must execute");
        assert!(cosim.findings.iter().any(|finding| {
            finding.level == "note" && finding.what.contains("physical copper connection")
        }));
        assert!(
            captured
                .expect("a successful run captures evidence")
                .faults
                .iter()
                .any(|fault| fault.kind == crate::stress::FaultKind::Short),
            "the physical bridge remains in the runtime fault stream"
        );

        let ties = crate::schematic_ties::SchematicTies {
            path: "blinky.sch".into(),
            raw: b"<eagle><drawing><schematic/></drawing></eagle>".to_vec(),
            ties: declarations,
            auto_discovered: false,
        };
        let report = analyze_with_firmware_with_ties(
            "blinky.kicad_pcb",
            board_text.as_bytes(),
            "nowdt.elf",
            include_bytes!("../../../testdata/firmware/avr_watchdog/nowdt.elf"),
            Some(&ties),
        );
        let report_json = serde_json::to_value(&report).expect("web report serializes");
        let inventory = report_json["inventory"]
            .as_array()
            .expect("successful firmware evidence inventory");
        let schematic = inventory
            .iter()
            .find(|artifact| artifact["role"] == "schematic")
            .expect("successful firmware evidence retains schematic context");
        assert_eq!(schematic["kind"], "eagle_board");
        assert_eq!(schematic["format"], "eagle_schematic");
        let firmware = inventory
            .iter()
            .find(|artifact| artifact["role"] == "firmware")
            .expect("firmware inventory row");
        assert_eq!(firmware["kind"], "elf");
    }

    #[test]
    fn unused_schematic_declarations_do_not_enter_web_causal_provenance() {
        const BOARD: &[u8] =
            include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd");
        let ties = crate::schematic_ties::SchematicTies {
            path: "declared.sch".into(),
            raw: b"<eagle><drawing><schematic/></drawing></eagle>".to_vec(),
            ties: vec![hauksbee_extract::DeclaredNetTie {
                net: "VCC".into(),
                tied_net: "3V3".into(),
                symbol: "VCC1".into(),
                tied_to: vec!["3V3_1".into()],
            }],
            auto_discovered: false,
        };
        let report: serde_json::Value =
            serde_json::from_str(&analyze_json_with_ties("declared.brd", BOARD, Some(&ties)))
                .expect("web JSON");
        let inventory = report["inventory"].as_array().expect("inventory");
        let (schematic_id, schematic) = inventory
            .iter()
            .enumerate()
            .find(|(_, artifact)| artifact["role"] == "schematic")
            .expect("every supplied input belongs in the inventory");
        assert_eq!(schematic["kind"], "eagle_board");
        assert_eq!(schematic["format"], "eagle_schematic");
        assert!(
            report["evidence"]
                .as_array()
                .expect("evidence maps")
                .iter()
                .all(|map| map["artifacts"].as_array().is_none_or(|ids| ids
                    .iter()
                    .all(|id| id.as_u64() != Some(schematic_id as u64)))),
            "an unused declaration is inventory, not causal evidence for an unrelated finding"
        );
    }

    /// The web report is the surface a user actually reads, so the exchange
    /// readers' honesty has to be visible ON it, not merely computed.
    ///
    /// Two things are checked, and both were wrong before: the DRC section must
    /// not claim an ODB++ job is a gerber archive, and the coverage note must not
    /// say its circuit was reverse-extracted from copper geometry — the job states
    /// its netlist, and the reader read it.
    #[test]
    fn an_exchange_board_gets_honest_coverage_notes_and_a_correct_not_checked_verdict() {
        const ODB_ZIP: &[u8] =
            include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.odb.zip");
        const IPC2581: &[u8] =
            include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml");

        for (label, name, bytes, phrase) in [
            ("ODB++", "b.odb.zip", ODB_ZIP, "an ODB++ job"),
            ("IPC-2581", "b.xml", IPC2581, "an IPC-2581 document"),
        ] {
            let report = analyze(name, bytes);
            let drc = report
                .sections
                .iter()
                .find(|s| s.title == "Copper spacing (DRC)")
                .unwrap_or_else(|| panic!("{label}: a DRC section"));
            assert!(
                drc.verdict.contains("Not checked"),
                "{label}: clearance DRC must be reported as not run, not as clean: {}",
                drc.verdict
            );
            assert!(
                drc.verdict.contains(phrase),
                "{label}: the verdict must name the input correctly: {}",
                drc.verdict
            );
            assert!(
                !drc.verdict.contains("gerber"),
                "{label}: and must not call it a gerber archive: {}",
                drc.verdict
            );
            let notes: Vec<&str> = report.notes.iter().map(|n| n.message.as_str()).collect();
            assert!(
                notes.iter().any(|n| n.contains("native layout")),
                "{label}: the typed not-checked reason must reach the report: {notes:?}"
            );
            let inventory = serde_json::to_string(&report.inventory).unwrap();
            assert!(
                inventory.contains("not reverse-engineered from copper"),
                "{label}: the reader's positive accounting belongs to the input artifact: {inventory}"
            );
            assert!(
                !notes
                    .iter()
                    .any(|n| n.contains("reverse-extracted from the fab")),
                "{label}: and must not claim the circuit came from copper: {notes:?}"
            );
        }
    }

    /// R15: the web GPIO table must keep the highest-TOGGLE nets and present them
    /// activity-first, matching the CLI/JSON surfaces, not the 15 alphabetically-
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
        assert_eq!(
            top[0].name, "ZZ_CLK",
            "the top mover must lead, not be dropped"
        );
        // Toggle counts are non-increasing down the ranked list.
        // (ZZ_CLK first, then the 1-toggle nets.)
        assert!(top.iter().all(|n| n.driven), "all 15 kept nets were driven");
    }

    #[test]
    fn gpio_nets_tiebreak_on_voltage_range_like_cli_and_json() {
        // Round-29: the web table dropped the voltage-range secondary sort key the
        // CLI toggle table and JSON activity_summary use, so at the truncation
        // boundary it kept a DIFFERENT subset of equal-toggle nets. 16 nets all
        // toggle once; the one with the LARGEST swing must survive the take(15) and
        // the smallest-swing (alphabetically-early) one must be the one dropped.
        use crate::scheduler::NetStat;
        use std::collections::HashMap;
        let mut stats: HashMap<String, NetStat> = HashMap::new();
        // "AAA_TINY" sorts first alphabetically but has the smallest swing.
        stats.insert(
            "AAA_TINY".to_string(),
            NetStat::with_toggles_and_range(1, 0.0, 0.1),
        );
        // "ZZ_BIG" sorts last alphabetically but has the largest swing.
        stats.insert(
            "ZZ_BIG".to_string(),
            NetStat::with_toggles_and_range(1, 0.0, 5.0),
        );
        for i in 0..14 {
            stats.insert(
                format!("M{i:02}"),
                NetStat::with_toggles_and_range(1, 0.0, 1.0),
            );
        }
        let net_volts: HashMap<String, f64> = HashMap::new();
        let top = top_gpio_nets(&stats, &net_volts, 15);
        assert_eq!(top.len(), 15);
        assert_eq!(
            top[0].name, "ZZ_BIG",
            "largest voltage swing leads on a toggle tie"
        );
        assert!(
            top.iter().any(|n| n.name == "ZZ_BIG"),
            "the widest-swing net must survive truncation"
        );
        assert!(
            !top.iter().any(|n| n.name == "AAA_TINY"),
            "the smallest-swing net is the one dropped, not a big mover"
        );
    }

    const SHORTED: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    const BLUEPILL: &[u8] =
        include_bytes!("../../../testdata/boards/stm32_bluepill_demo.kicad_pcb");

    #[test]
    fn from_plain_carries_heads_up_and_serializes() {
        // Parity fix (the 171-ohm USB controlled-impedance case): an actionable
        // info note stored on PlainReport.heads_up must reach the web section,
        // even when the section verdict reads "healthy" (zero findings). This is
        // the note that must not vanish on the web while showing on --plain.
        let mut p = PlainReport::default();
        p.subject = "signal-integrity".to_string();
        p.heads_up.push(HeadsUp::glossed(
            "USB trace impedance is 171 ohm, +71% from target (90 ohm).",
            "reflections can make the link marginal",
            "match trace width and spacing to the stackup",
        ));
        let sect = WebSection::from_plain("Signal integrity", &p);
        assert!(
            sect.findings.is_empty(),
            "no findings, only a heads-up note"
        );
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
            !h2.to_lowercase().contains("looks healthy")
                && h2.to_lowercase().contains("trustworthy"),
            "bind-open headline must warn: {h2}"
        );
        // A genuinely clean board still reads healthy.
        let h3 = overall_headline(0, 0, false, false);
        assert!(
            h3.to_lowercase().contains("looks healthy"),
            "clean board: {h3}"
        );
    }

    #[test]
    fn web_report_carries_bind_summary() {
        // The web report must include the bind-role honesty summary, never drop
        // it. Even a healthy board carries the critical-parts
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
        // R23 (web-drops-usbc-verdict): a web report that omits the USB-C CC
        // compliance verdict lets a board with the RPi-4 shared-CC-pulldown
        // fault (which the CLI text/plain/json all flag SERIOUS) read
        // "Looks healthy" on the web. A Serious verdict must become a
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
        // R43: the verdict must be a full sentence like every other section (built
        // via WebSection::from_plain → PlainReport::verdict()), not a bare token,
        // a uniform web consumer renders `section.verdict` directly, so "problem"
        // read as a lone word under the USB-C card while siblings showed prose.
        assert!(
            sect.verdict.ends_with('.') && sect.verdict.contains(' '),
            "the USB-C verdict must be a full sentence, got {:?}",
            sect.verdict
        );
        assert!(
            sect.verdict != "problem" && sect.verdict != "note",
            "the verdict must not be a bare status token: {:?}",
            sect.verdict
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
        assert!(
            isect.verdict.ends_with('.') && isect.verdict.contains(' ') && isect.verdict != "note",
            "the Info verdict must also be a full sentence, got {:?}",
            isect.verdict
        );
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
    fn analyze_board_as_code_compiles_and_binds() {
        // The web drop zone must accept every file `run` does: a `.board`
        // Board-as-Code source is compiled to KiCad board text before the
        // normal text path, instead of dying with "unrecognized board format".
        let dsl = br#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "ANODE_NET"
    net "CATHODE_NET"
    comp D1 lib "Diode_SMD:D_SOD-323" val "1N4148" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "CATHODE_NET"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "ANODE_NET"
    }
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 5 0 rot 0 {
        pad "1" smd rect at 5 0 size 1 1 layers [F.Cu] net "ANODE_NET"
        pad "2" smd rect at 6 0 size 1 1 layers [F.Cu] net "CATHODE_NET"
    }
}
"#;
        let r = analyze("tarski.board", dsl);
        assert!(r.ok, "a .board upload must analyze: {:?}", r.error);
        assert_eq!(r.num_components, 2, "D1 and R1 survive the compile");
        assert!(r.num_nets >= 2, "both nets survive: {}", r.num_nets);
        // Sniffed by header too: the extension is not load-bearing.
        let r2 = analyze("exported.txt", dsl);
        assert!(
            r2.ok,
            "header sniff works without the extension: {:?}",
            r2.error
        );
        // A broken DSL fails with the compile error, not the format-sniffer one.
        let r3 = analyze(
            "broken.board",
            b"# Board-as-Code (hauksbee board DSL v1)\nfn main { comp }",
        );
        assert!(!r3.ok);
        assert!(
            r3.error.as_deref().unwrap_or("").contains("Board-as-Code"),
            "the error names the DSL compile step: {:?}",
            r3.error
        );
    }

    #[test]
    fn analyze_zip_of_a_board_code_export_works() {
        // "Zip it and we figure it out" must hold for the board slot too: a
        // zipped .board export analyzes like the bare file would.
        use std::io::Write;
        let dsl = br#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "B"
    }
}
"#;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file(
            "export/tarski.board",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        w.write_all(dsl).unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let r = analyze("tarski-export.zip", &bytes);
        assert!(r.ok, "zipped .board must analyze: {:?}", r.error);
        assert_eq!(r.num_components, 1, "R1 survives the zip + compile");
        // A zip with neither gerbers nor a .board says what it looked for.
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"not a board").unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let r2 = analyze("junk.zip", &bytes);
        assert!(!r2.ok);
        let err = r2.error.unwrap_or_default();
        assert!(
            err.contains("gerber") && err.contains(".board"),
            "error names both zip forms: {err}"
        );
    }

    /// Real gerber archive through the WEB path, which is what makes the
    /// drop-zone claim "gerber zip" true: the reader registry knows no zips on
    /// its own, so without it an upload dies with "unrecognized board format".
    /// Corpus-gated like the
    /// extract crate's gerber tests: skips when board-corpus is absent.
    #[test]
    fn analyze_gerber_zip_reverse_extracts() {
        use std::io::Write;
        let dir = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or_default()
            .join("famous/uconsole_cm4_adapter_gerber");
        if !dir.exists() {
            if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
                panic!("corpus required but uconsole_cm4_adapter_gerber missing");
            }
            eprintln!("skipping gerber-zip web test (corpus absent)");
            return;
        }
        // Zip the fab dir the way a user would.
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                w.start_file(
                    format!("gerbers/{}", p.file_name().unwrap().to_str().unwrap()),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
                w.write_all(&std::fs::read(&p).unwrap()).unwrap();
            }
        }
        let bytes = w.finish().unwrap().into_inner();
        let r = analyze("cm4_adapter_gerbers.zip", &bytes);
        assert!(r.ok, "gerber zip must reverse-extract: {:?}", r.error);
        // The CM4 adapter fixture ships no pick-and-place file, so components
        // cannot be named; copper nets are the reverse-extraction signal.
        assert!(r.num_nets > 0, "nets recovered from copper: {}", r.num_nets);
        // The unchecked layers are stated, not silently green.
        let drc = r.sections.iter().find(|s| s.title.contains("DRC")).unwrap();
        assert!(
            drc.verdict.contains("Not checked"),
            "gerber DRC section is honest: {}",
            drc.verdict
        );
        assert!(
            r.notes
                .iter()
                .any(|n| n.message.contains("reverse-extracted")),
            "coverage note present"
        );
    }

    #[test]
    fn evidence_fields_are_additive_to_the_plain_kicad_web_golden() {
        // The whole point of routing analyze() through the board_input
        // normalizer is that NOTHING moves for the common case. This golden was
        // captured from the pre-normalizer analyze() on boot_gate.kicad_pcb;
        // byte-for-byte equality proves the plain .kicad_pcb web report did not
        // change shape, counts, wording, or ordering under the refactor.
        // (Regenerated once when findings gained their optional x/y board
        // location: the two DRC shorts now carry x=112.0, y=100.0.)
        let golden = include_str!("../../../testdata/golden/boot_gate_web_report.json");
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("assumptions");
        object.remove("evidence");
        object.remove("inventory");
        let golden_value: serde_json::Value = serde_json::from_str(golden).unwrap();
        assert_eq!(
            value, golden_value,
            "adding evidence must leave every pre-existing web-report field unchanged"
        );
    }

    #[test]
    fn zipped_board_export_with_firmware_reaches_cosim() {
        // B6 regression: RE-READING the original upload bytes with only the
        // text/binary sniffers fails co-sim with "could not re-read the board"
        // on a zipped .board export that produced a clean static report.
        // Normalizing once must give a REAL co-sim outcome: here
        // the DSL board has no MCU, so the honest "no microcontroller" note.
        use std::io::Write;
        let dsl = br#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "B"
    }
}
"#;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file(
            "export/tarski.board",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        w.write_all(dsl).unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let r = analyze_with_firmware("tarski-export.zip", &bytes, "fw.elf", BOOT_GATE_FW);
        assert!(r.ok, "static analysis succeeds: {:?}", r.error);
        let cosim = r
            .cosim
            .expect("cosim section present once firmware was supplied");
        assert!(
            !cosim
                .findings
                .iter()
                .any(|f| f.why.contains("re-read") || f.why.contains("Could not re-read")),
            "the re-read failure mode must be gone: {:?}",
            cosim.findings
        );
        assert!(
            !cosim.ran,
            "the DSL board has no MCU, so the co-sim cannot run"
        );
        assert!(
            cosim
                .findings
                .iter()
                .any(|f| f.why.to_lowercase().contains("microcontroller")),
            "the honest no-MCU note must be the reason: {:?}",
            cosim.findings
        );
    }

    /// B6, gerber arm: a gerber zip plus firmware must reach a real co-sim
    /// outcome too (usually the honest "no MCU found", since a fab archive
    /// names no parts). Corpus-gated like [`analyze_gerber_zip_reverse_extracts`].
    #[test]
    fn gerber_zip_with_firmware_reaches_cosim() {
        use std::io::Write;
        let dir = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or_default()
            .join("famous/uconsole_cm4_adapter_gerber");
        if !dir.exists() {
            if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
                panic!("corpus required but uconsole_cm4_adapter_gerber missing");
            }
            eprintln!("skipping gerber-zip cosim test (corpus absent)");
            return;
        }
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                w.start_file(
                    format!("gerbers/{}", p.file_name().unwrap().to_str().unwrap()),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
                w.write_all(&std::fs::read(&p).unwrap()).unwrap();
            }
        }
        let bytes = w.finish().unwrap().into_inner();
        let r = analyze_with_firmware("cm4_adapter_gerbers.zip", &bytes, "fw.elf", BOOT_GATE_FW);
        assert!(r.ok, "gerber zip static analysis succeeds: {:?}", r.error);
        let cosim = r
            .cosim
            .expect("cosim section present once firmware was supplied");
        assert!(
            !cosim
                .findings
                .iter()
                .any(|f| f.why.contains("re-read") || f.why.contains("Could not re-read")),
            "the re-read failure mode must be gone: {:?}",
            cosim.findings
        );
        // A fab archive carries no part identities, so the expected honest
        // outcome is "no MCU"; a run would also be acceptable if reverse
        // extraction ever learns to name one.
        if !cosim.ran {
            assert!(
                cosim
                    .findings
                    .iter()
                    .any(|f| f.why.to_lowercase().contains("microcontroller")),
                "a not-run co-sim must carry the honest reason: {:?}",
                cosim.findings
            );
        }
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
    // round-trip, so it catches any slip back to text-first routing on the web.
    const ALTIUM: &[u8] = include_bytes!("../../../testdata/boards/altium_two_resistor.PcbDoc");

    #[test]
    fn binary_altium_board_survives_the_web_path() {
        // Regression (web bytes fix): lossy-UTF8-decoding the upload and only
        // ever trying the TEXT sniffer corrupts a binary Altium board before
        // parse AND never routes it to its reader. Raw bytes must extract and
        // report like any text board.
        let r = analyze("two_resistor.PcbDoc", ALTIUM);
        assert!(
            r.ok,
            "binary board must extract from raw bytes: {:?}",
            r.error
        );
        assert_eq!(r.num_components, 2, "R1 and R2 survive");
        assert!(r.num_nets > 0, "nets survive: {}", r.num_nets);
        // The guard that proves the bytes path is what makes it work: the SAME
        // board pushed through a lossy UTF-8 round-trip is mangled (the OLE2
        // magic is not valid UTF-8) and fails to read.
        let lossy = String::from_utf8_lossy(ALTIUM).into_owned();
        assert_ne!(
            lossy.as_bytes(),
            ALTIUM,
            "lossy decode must corrupt the container"
        );
        let r2 = analyze("two_resistor.PcbDoc", lossy.as_bytes());
        assert!(
            !r2.ok,
            "the lossy view must NOT extract; bytes-first routing is load-bearing"
        );
    }

    // Track D: web firmware drop zone.
    const NO_MCU: &[u8] =
        include_bytes!("../../hauksbee-ci/examples/boards/power_resistor.kicad_pcb");
    const BOOT_GATE_FW: &[u8] =
        include_bytes!("../../../testdata/firmware/boot_gate_a/boot_gate.elf");

    #[test]
    fn board_only_path_leaves_cosim_absent() {
        // The plain board-only analyze() must NOT carry a cosim field, so the
        // /api/analyze JSON schema is byte-for-byte unchanged (skip_serializing_if).
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(r.cosim.is_none(), "board-only analyze must not set cosim");
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        assert!(
            !json.contains("\"cosim\""),
            "board-only JSON must omit cosim: {json:.200}"
        );
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
            cosim
                .findings
                .iter()
                .any(|f| f.why.to_lowercase().contains("microcontroller")),
            "should name the missing MCU as the reason: {:?}",
            cosim.findings
        );
        let refusal = r
            .refusal
            .expect("unavailable co-sim must carry a refusal contract");
        assert!(!refusal.claim.is_empty());
        assert!(
            !r.headline.contains("Looks healthy") && r.headline.contains("invalid"),
            "a refusal cannot retain a healthy primary verdict: {}",
            r.headline
        );
        assert!(refusal
            .missing_prerequisite
            .to_lowercase()
            .contains("microcontroller"));
        assert!(
            refusal
                .valid_partial_conclusions
                .iter()
                .any(|line| line.to_lowercase().contains("static")),
            "static conclusions remain useful: {refusal:?}"
        );
        assert!(!refusal.next_action.is_empty());
    }

    #[test]
    fn firmware_serve_analysis_preserves_the_companion_tie() {
        const BOARD: &[u8] =
            include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd");
        const SCHEMATIC: &[u8] =
            include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch");
        let ties = crate::schematic_ties::SchematicTies {
            path: "declared.sch".into(),
            raw: SCHEMATIC.to_vec(),
            ties: hauksbee_extract::declared_net_ties(
                std::str::from_utf8(SCHEMATIC).expect("text fixture"),
            )
            .expect("schematic parses"),
            auto_discovered: true,
        };

        let board_only = analyze_json_with_ties("declared.brd", BOARD, Some(&ties));
        let with_firmware = analyze_with_firmware_json_with_ties(
            "declared.brd",
            BOARD,
            "fw.elf",
            BOOT_GATE_FW,
            Some(&ties),
        );
        for output in [&board_only, &with_firmware] {
            assert!(
                output.contains("schematic names this net pair"),
                "companion intent must survive every serve path: {output:.500}"
            );
            assert!(!output.contains("GND shorts AGND"), "{output:.500}");
        }
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
        assert!(
            v["cosim"].is_object(),
            "cosim object must be present: {json:.200}"
        );
        assert_eq!(v["cosim"]["ran"], false);
        assert_eq!(
            v["refusal"]["claim"],
            "firmware and board co-simulation conclusions"
        );
        assert!(v["refusal"]["missing_prerequisite"].is_string());
        assert!(v["refusal"]["valid_partial_conclusions"].is_array());
        assert!(v["refusal"]["next_action"].is_string());
    }

    #[test]
    fn real_firmware_on_in_process_mcu_runs() {
        // boot_gate is an ATmega328 (in-process simavr backend) and boot_gate.elf
        // is its matching AVR firmware: the co-sim should actually run. If the
        // build was made without the AVR backend the load fails gracefully, so we
        // accept either a real run OR a friendly ran:false note (never ok:false).
        let r = analyze_with_firmware(
            "boot_gate.kicad_pcb",
            SHORTED,
            "boot_gate.elf",
            BOOT_GATE_FW,
        );
        assert!(r.ok, "static analysis still succeeds: {:?}", r.error);
        let cosim = r
            .cosim
            .as_ref()
            .expect("cosim present once firmware was supplied");
        if cosim.ran {
            assert!(
                cosim.seconds_simulated > 0.0,
                "a run that ran must have advanced time"
            );
            assert_eq!(r.inventory.len(), 2, "board and firmware are both cited");
            assert!(r
                .inventory
                .iter()
                .all(|artifact| artifact.sha256().len() == 64));
            assert!(
                r.evidence.iter().any(|map| map.error_budget().is_some()),
                "web co-sim assertions carry the same numerical budget as CLI assertions"
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
        let json = analyze_with_firmware_json(
            "boot_gate.kicad_pcb",
            SHORTED,
            "boot_gate.elf",
            BOOT_GATE_FW,
        );
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
            reason: "DC Newton did not converge in 100 iters".to_string(),
        }];
        let f = analog_invalid_finding(2, &windows);
        assert_eq!(
            f.level, "note",
            "analog invalidity is a note-level honesty caveat (it demotes the \
             headline), not a serious board fault that folds into the count"
        );
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

    /// A clean co-sim section that RAN and vouched for the firmware; the
    /// baseline the caveat logic must NOT demote.
    fn clean_ran_section() -> WebCosimSection {
        WebCosimSection {
            ran: true,
            seconds_simulated: 0.1,
            uart_output: String::new(),
            findings: Vec::new(),
            gpio_nets: Vec::new(),
            analog_valid: true,
            failed_windows: Vec::new(),
            spi_framing: Vec::new(),
            boot_gates: Vec::new(),
            firmware_exercised: true,
            substituted: false,
            error_budget: None,
        }
    }

    #[test]
    fn empty_cosim_demotes_looks_healthy_but_a_real_run_does_not() {
        // Round-26: a statically-clean board whose firmware co-sim RAN yet proved
        // nothing (no GPIO/UART, a substitute core, or a failed analog window)
        // must not read "Looks healthy", that is false comfort, and it disagrees
        // with the CLI --plain "worth a look" verdict for the same inputs.
        let healthy = overall_headline(0, 0, false, false);
        let demoted = overall_headline(0, 0, true, false);
        assert_ne!(healthy, demoted, "the two verdicts must be distinct");

        // A clean, exercised run keeps the healthy headline.
        assert_eq!(
            cosim_caveat_headline(&clean_ran_section(), &WebCosimCoverage::default(), &healthy),
            None,
            "a real firmware run must not be demoted"
        );

        // Each caveat on its own demotes the bare healthy line.
        for mutate in [
            |s: &mut WebCosimSection| s.firmware_exercised = false,
            |s: &mut WebCosimSection| s.substituted = true,
            |s: &mut WebCosimSection| s.analog_valid = false,
        ] {
            let mut s = clean_ran_section();
            mutate(&mut s);
            assert_eq!(
                cosim_caveat_headline(&s, &WebCosimCoverage::default(), &healthy),
                Some(demoted.clone()),
                "a co-sim that proved nothing must demote Looks healthy"
            );
        }

        // This helper handles caveats from completed runs. A co-sim that did not
        // run is labeled invalid by `apply_refusal_headline` after its typed
        // refusal is built, not by this note-level helper.
        let mut not_run = clean_ran_section();
        not_run.ran = false;
        not_run.firmware_exercised = false;
        assert_eq!(
            cosim_caveat_headline(&not_run, &WebCosimCoverage::default(), &healthy),
            None,
            "not-run handling belongs to the typed refusal path"
        );

        // Only the bare healthy line is overwritten; an already-demoted or
        // findings-bearing headline is left untouched.
        let mut caveated = clean_ran_section();
        caveated.substituted = true;
        assert_eq!(
            cosim_caveat_headline(&caveated, &WebCosimCoverage::default(), &demoted),
            None,
            "an already-heads-up headline is not rewritten"
        );
    }

    #[test]
    fn timing_refusal_is_structured_and_refuses_while_fallback_is_qualified() {
        let window = crate::result::CosimFallbackWindow {
            start_s: 0.001,
            end_s: 0.002,
            method: "backward-euler".to_string(),
            fidelity_note: "first-order and numerically dissipative".to_string(),
            error_estimate_v: Some(0.012),
        };
        let fallback_only = clean_ran_section();
        let mut coverage = WebCosimCoverage {
            fallback_windows: vec![window.clone()],
            ..Default::default()
        };
        assert!(
            refusal_for_cosim(&fallback_only, &coverage).is_none(),
            "a converged second-class span is qualified, not refused"
        );
        assert!(
            cosim_caveat_headline(
                &fallback_only,
                &coverage,
                &overall_headline(0, 0, false, false)
            )
            .is_some(),
            "second-class evidence cannot leave a bare healthy headline"
        );

        let section = fallback_only;
        coverage.timing_refusals =
            vec!["PWL replay refused on net /CLK: transition budget exceeded".to_string()];

        let json = serde_json::to_value(&coverage).unwrap();
        assert_eq!(json["timing_refusals"][0], coverage.timing_refusals[0]);
        assert_eq!(json["fallback_windows"][0]["method"], "backward-euler");
        assert_eq!(json["fallback_windows"][0]["error_estimate_v"], 0.012);

        let refusal = refusal_for_cosim(&section, &coverage).expect("timing collapse is invalid");
        assert!(
            refusal.missing_prerequisite.contains("PWL replay refused"),
            "{refusal:?}"
        );
    }

    #[test]
    fn strict_timing_refusal_is_not_projected_as_an_ordinary_note_finding() {
        use crate::reports::coverage::CoverageInputs;

        let caveats = CoverageInputs {
            watchdog_resets: vec![("U1".to_string(), 1)],
            timing_refusals: vec![
                "PWL replay refused on net /CLK: transition budget exceeded".to_string()
            ],
            ..Default::default()
        }
        .caveats();
        let findings = coverage_findings_for_web(&caveats);

        assert_eq!(findings.len(), 1, "only the reboot is an ordinary note");
        assert!(findings[0].why.contains("watchdog rebooted"));
        assert!(
            findings
                .iter()
                .all(|finding| !finding.why.contains("TIMING INVALID")),
            "strict invalidity belongs only in timing_refusals + Refusal: {findings:?}"
        );
    }

    #[test]
    fn synchronous_web_cosim_refuses_when_any_selected_mcu_needs_an_external_backend() {
        use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};

        let pin = |number: &str, net: i64, function: &str| Pin {
            number: number.to_string(),
            net: Some(net),
            function: function.to_string(),
            kind: String::new(),
            position: None,
        };
        let component = |reference: &str, value: &str, lib_id: &str, pins: Vec<Pin>| Component {
            reference: reference.to_string(),
            value: value.to_string(),
            lib_id: lib_id.to_string(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins,
        };
        let board = ExtractedBoard {
            name: "mixed-backend-board".to_string(),
            nets: vec![
                Net {
                    id: 1,
                    name: "+5V".to_string(),
                },
                Net {
                    id: 2,
                    name: "GND".to_string(),
                },
            ],
            components: vec![
                component(
                    "U1",
                    "ATmega328P",
                    "MCU_Microchip_ATmega:ATmega328P-AU",
                    vec![pin("7", 1, "VCC"), pin("8", 2, "GND")],
                ),
                component(
                    "U2",
                    "STM32F411CEU6",
                    "MCU_ST_STM32F4:STM32F411CEUx",
                    vec![pin("24", 1, "VDD"), pin("23", 2, "VSS")],
                ),
            ],
        };

        let (section, evidence) = run_web_cosim(
            &board,
            "mixed.kicad_pcb",
            "firmware.elf",
            &[0u8; 4],
            &hauksbee_extract::DrcReport::default(),
            None,
        );
        let reason = section
            .findings
            .first()
            .map(|finding| finding.why.as_str())
            .unwrap_or_default();
        assert!(!section.ran, "mixed external/in-process runs must refuse");
        assert!(
            evidence.is_none(),
            "a refused run produces no co-sim evidence"
        );
        assert!(
            reason.contains("U2") && reason.contains("external emulator"),
            "the refusal must name the selected external MCU, got: {reason}"
        );
    }

    #[test]
    fn warning_cosim_fault_escalates_the_headline_off_looks_healthy() {
        // Round-27: only SERIOUS co-sim faults folded into the verdict, so a
        // non-destructive over-stress WARNING (a part carrying past its continuous
        // rating without dying) sat silently in the co-sim card under a bare
        // "Looks healthy" banner, while the CLI --plain counts it ("1 issue found,
        // none serious. Worth a look.") and --strict exits 2. A warning must
        // escalate total (not serious), matching the CLI verdict.
        let mut warned = clean_ran_section();
        warned.findings.push(WebFinding {
            level: "warning".to_string(),
            what: "R1 carries ~200 mA past its 100 mA continuous rating.".to_string(),
            why: "sustained over-current cooks the part over time".to_string(),
            fix: "raise the resistor's power/current rating or reduce the load".to_string(),
            x: None,
            y: None,
        });
        // Statically clean board (total 0, serious 0) + one warning fault.
        let folded = fold_cosim_faults(0, 0, &warned).expect("a warning fault folds in");
        assert_eq!((folded.0, folded.1), (1, 0), "1 issue, none serious");
        assert!(
            folded.2.contains("none serious") && folded.2.contains("Worth a look"),
            "headline matches the CLI 'worth a look' verdict, got: {}",
            folded.2
        );
        assert!(
            !folded.2.contains("Looks healthy"),
            "must not read Looks healthy"
        );

        // A destructive SERIOUS fault still escalates serious, not just total.
        let mut killed = clean_ran_section();
        killed.findings.push(WebFinding {
            level: "serious".to_string(),
            what: "Q1 destroyed by over-current.".to_string(),
            why: "the MOSFET exceeded its absolute-max drain current".to_string(),
            fix: "add gate/current limiting".to_string(),
            x: None,
            y: None,
        });
        let folded = fold_cosim_faults(0, 0, &killed).expect("a serious fault folds in");
        assert_eq!((folded.0, folded.1), (1, 1), "1 issue, 1 serious");
        assert!(
            folded.2.contains("1 serious"),
            "serious headline, got: {}",
            folded.2
        );

        // A clean run with only note-level caveats does NOT fold (returns None),
        // notes demote via cosim_caveat_headline, not the fault count.
        assert!(
            fold_cosim_faults(0, 0, &clean_ran_section()).is_none(),
            "a fault-free co-sim must not fold into the issue count"
        );
    }

    #[test]
    fn boot_held_high_advisory_demotes_the_headline_not_folds_as_a_serious_fault() {
        // R32: the web boot held-high advisory was inserted as a SERIOUS finding,
        // so fold_cosim_faults counted it and rewrote a statically-clean board's
        // headline to "fix the serious ones before ordering boards", while every
        // CLI surface treats a driven-high control net as an advisory note that
        // exits 0. It is a real, actionable observation but not a confirmed board
        // fault, so it must be note-level: it demotes a bare "Looks healthy" to the
        // heads-up verdict without folding into the serious/total count.
        let mut sect = clean_ran_section();
        sect.findings.insert(
            0,
            WebFinding {
                level: "note".to_string(),
                what: "Control net 'GATE_CTRL' may be energised at power-up".to_string(),
                why: "driven HIGH and held from power-up with no safe-default resistor".to_string(),
                fix: "confirm the polarity or add a pull to the safe level".to_string(),
                x: None,
                y: None,
            },
        );

        // (1) The advisory does not fold as an electrical fault.
        assert!(
            fold_cosim_faults(0, 0, &sect).is_none(),
            "a boot held-high advisory must not fold into the serious/total count"
        );

        // (2) It demotes the bare healthy headline to the heads-up verdict.
        let healthy = overall_headline(0, 0, false, false);
        assert_eq!(
            cosim_caveat_headline(&sect, &WebCosimCoverage::default(), &healthy),
            Some(overall_headline(0, 0, true, false)),
            "a boot held-high advisory must demote Looks healthy, not escalate serious"
        );
    }

    #[test]
    fn analog_invalidity_demotes_the_headline_not_folds_as_a_serious_fault() {
        // Round-30 CLI/web parity: an analog solve that failed to converge is a
        // co-sim HONESTY caveat, not a board defect. The web prepends the loud
        // analog_invalid_finding exactly as run_web_cosim does. It must NOT fold
        // into the serious/total fault count (fold returns None), and the bare
        // "Looks healthy" headline must instead DEMOTE to the heads-up verdict,
        // matching the CLI --plain "worth a look" note. Before the fix the finding
        // was level "serious", so fold_cosim_faults returned Some((1,1,..)) and the
        // web told the user to "fix the serious ones" for a phantom hardware fault.
        let mut diverged = clean_ran_section();
        diverged.analog_valid = false;
        diverged.failed_windows = vec![WebFailedWindow {
            start_s: 0.0012,
            end_s: 0.0034,
            reason: "DC Newton did not converge in 100 iters".to_string(),
        }];
        diverged.findings.insert(
            0,
            analog_invalid_finding(2, &diverged.failed_windows.clone()),
        );

        // (1) The caveat does not fold as an electrical fault.
        assert!(
            fold_cosim_faults(0, 0, &diverged).is_none(),
            "analog non-convergence must not fold into the serious/total count"
        );

        // (2) It demotes the bare healthy headline to the heads-up verdict.
        let healthy = overall_headline(0, 0, false, false);
        assert_eq!(
            cosim_caveat_headline(&diverged, &WebCosimCoverage::default(), &healthy),
            Some(overall_headline(0, 0, true, false)),
            "a diverged analog solve must demote Looks healthy, not escalate serious"
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
                &[WebFailedWindow {
                    start_s: 0.0,
                    end_s: 0.0001,
                    reason: "DC Newton did not converge in 100 iters".to_string(),
                }],
            )],
            gpio_nets: Vec::new(),
            analog_valid: false,
            failed_windows: vec![WebFailedWindow {
                start_s: 0.0,
                end_s: 0.0001,
                reason: "DC Newton did not converge in 100 iters".to_string(),
            }],
            spi_framing: Vec::new(),
            boot_gates: Vec::new(),
            firmware_exercised: true,
            substituted: false,
            error_budget: None,
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
        let refusal = refusal_for_cosim(&section, &WebCosimCoverage::default())
            .expect("invalid analog run refuses");
        assert!(
            refusal
                .missing_prerequisite
                .contains("DC Newton did not converge"),
            "refusal keeps the solver's specific diagnosis: {refusal:?}"
        );
        assert!(
            refusal
                .valid_partial_conclusions
                .iter()
                .any(|line| line.contains("Static board analysis")),
            "refusal preserves static partial conclusions: {refusal:?}"
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
            firmware_exercised: true,
            substituted: false,
            error_budget: None,
        };
        let json = serde_json::to_string(&section).unwrap();
        assert!(
            json.contains("\"boot_gates\""),
            "populated boot_gates serializes: {json}"
        );
        assert!(
            json.contains("GATE_CTRL") && json.contains("driven_high"),
            "panel row present: {json}"
        );
        // Empty => omitted (backward-compatible schema).
        section.boot_gates.clear();
        let json2 = serde_json::to_string(&section).unwrap();
        assert!(
            !json2.contains("\"boot_gates\""),
            "empty boot_gates omitted: {json2}"
        );
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
        let json = analyze_with_firmware_json(
            "boot_gate.kicad_pcb",
            SHORTED,
            "boot_gate.elf",
            BOOT_GATE_FW,
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cosim = &v["cosim"];
        assert_eq!(
            cosim["ran"],
            serde_json::json!(true),
            "tracked AVR firmware must execute in this required parity test: {json:.400}"
        );
        // The gate panel is present and names the driven-high gate.
        let gates = cosim.get("boot_gates").and_then(|g| g.as_array());
        assert!(
            gates.is_some_and(|g| !g.is_empty()),
            "boot_gates panel must be present on the web co-sim: {json:.400}"
        );
        // The held-high hazard leads the findings as a serious item.
        let findings = cosim["findings"].as_array().expect("findings array");
        assert!(
            findings.iter().any(|f| f["what"]
                .as_str()
                .unwrap_or("")
                .contains("energised at power-up")),
            "the held-high control net must surface as a serious finding: {json:.600}"
        );
    }
}
