//! The web front-door analysis: take an uploaded board file's bytes, run every
//! static check, and return a single JSON payload a browser can render with no
//! CLI involved.
//!
//! This is the "drop your board, get a report" backend. It reuses the exact same
//! analysis as the CLI (`ExtractedBoard` extraction + the DRC / lint / SI /
//! resource checks + the plain-language [`crate::plain`] templates).
//!
//! One divergence from the CLI: the web path receives a single file's bytes and
//! so cannot see a sibling `.kicad_pro` project file, leaving its DRC on the
//! board's default/embedded clearances where `hauksbee run --drc` applies
//! per-netclass ones. That is entry 1 of the divergence register in
//! [`crate::result`], which is where any other deliberate difference between two
//! surfaces must be recorded. The HTTP plumbing lives in `hauksbee-server`; this
//! module is pure (bytes in, JSON string out) so it has no web dependency and is
//! unit-testable.

use serde::Serialize;

use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

use crate::binder::{bind_board, BoundBoard};
use crate::engine::HauksbeeEngine;
use crate::plain::{
    plain_drc_structured_with_rule_source, plain_netlint, plain_si, HeadsUp, PlainFinding,
    PlainLevel, PlainReport,
};
use crate::result::{BindSummary, JsonNote, JsonNoteKind, Refusal};
use hauksbee_frontdoor_api::actions::{push_unique, WebAction};

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

/// A heads-up note in the shape the browser renders.
///
/// Carries the same what / why / what-to-do gloss a finding does. `why` / `fix` are
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
        Self {
            what: h.what.clone(),
            why: h.why.clone(),
            fix: h.fix.clone(),
        }
    }
}

impl WebFinding {
    /// A finding that points at no single spot on the board.
    fn unplaced(
        level: &str,
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            level: level.to_string(),
            what: what.into(),
            why: why.into(),
            fix: fix.into(),
            x: None,
            y: None,
        }
    }
}

impl From<&PlainFinding> for WebFinding {
    fn from(f: &PlainFinding) -> Self {
        Self {
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
    /// What the app can offer as buttons to move this section forward (give
    /// the open parts models, add the project rules file). The section's text
    /// stays free of terminal commands; this is the structured form of them.
    /// Omitted when there is nothing to offer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<WebAction>,
}

impl WebSection {
    fn from_plain(title: &str, p: &PlainReport) -> Self {
        Self {
            title: title.to_string(),
            verdict: p.web_verdict(),
            findings: p.findings.iter().map(WebFinding::from).collect(),
            heads_up: p.heads_up.iter().map(WebHeadsUp::from).collect(),
            actions: Vec::new(),
        }
    }
}

/// The web mirror of the bind-honesty summary.
///
/// Populated from [`BindSummary`] (computed once on the already-bound board), NEVER
/// recomputed, so the web report carries the SAME bind-role honesty data as the
/// CLI/JSON surfaces. The browser uses `active_path_unresolved` to refuse a false
/// "Looks healthy".
#[derive(Debug, Clone, Serialize)]
pub struct BindSummaryWeb {
    /// `"M/N"`, active ICs that bound, over the total active ICs on the board.
    pub critical_parts_bound: String,
    /// References of verdict-critical parts left open on the live circuit:
    /// unresolved active ICs, unresolved discrete transistors (Q prefix, the
    /// power FETs; `crate::result::is_verdict_critical`), and
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
    /// Orderable-variant sets the board does not settle, for the app's picker:
    /// the part and every member with its selector, source and S/C/U reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_selections: Vec<hauksbee_models::variants::Selection>,
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
    /// Set when the part is open because two parts share its designator: the
    /// fix is to rename one, so the page offers no model drafting for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_fix: Option<String>,
}

impl BindSummaryWeb {
    /// Bridge a [`BindSummary`] into the web shape: keep the critical-parts ratio
    /// and the union of verdict-critical open parts, unresolved active ICs AND
    /// unresolved discrete transistors (the power FETs on a protection path),
    /// plus resolved-but-open active ICs. The transistor half uses the same
    /// `crate::result::is_verdict_critical` predicate the CLI INCONCLUSIVE
    /// verdict uses, so the browser can never print a clean bill over an
    /// unbound FET the CLI refuses to bless.
    #[must_use]
    pub fn from_summary(s: &BindSummary) -> Self {
        // The two buckets, one entry each. `bound` separates them: an
        // unresolved part has no model, a resolved-but-open one has a model and
        // a wiring problem.
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
                rename_fix: hauksbee_ir::evidence::Assumption::duplicate_fix(
                    &u.reference,
                    &u.reason,
                ),
            })
            .collect();
        // Sorted, not report order: every surface that prints this list (the
        // web banner, the bind-role note) must agree on one order, and "U3, U6,
        // U2, U5, U1" reads as noise next to "U1, U2, U3, U5, U6". The shared
        // reference-natural key, so U10 does not sort before U2.
        open_parts.sort_by_key(|p| crate::reports::bind::natural_ref_key(&p.reference));
        open_parts.dedup_by(|a, b| a.reference == b.reference);

        Self {
            critical_parts_bound: s.critical_parts_bound.clone(),
            // Projected from the detail list, so the banner and the per-part
            // detail can never name different parts in a different order.
            active_path_unresolved: open_parts.iter().map(|p| p.reference.clone()).collect(),
            open_parts,
            open_selections: Vec::new(),
        }
    }

    /// Whether an active IC is open on the live circuit; the predicate that must
    /// override a "Looks healthy" verdict on the web report.
    #[must_use]
    pub const fn active_path_open(&self) -> bool {
        !self.active_path_unresolved.is_empty()
    }
}

/// One watched net's activity after a firmware co-sim run, for the web GPIO table.
///
/// `driven` marks a net that actually moved (toggled) during the run, so
/// the browser can show at a glance whether the firmware drove anything.
#[derive(Debug, Clone, Serialize)]
pub struct WebGpioNet {
    pub name: String,
    /// Last sampled node voltage at the end of the run.
    pub volts: f64,
    /// True if the net toggled at least once (the firmware drove it).
    pub driven: bool,
    /// Level changes seen over the run; with `seconds_simulated`, a measured rate.
    #[serde(default)]
    pub toggles: u64,
}

/// The firmware co-sim result, attached to [`WebReport::cosim`].
///
/// It is attached only when a firmware file was supplied. `ran` is false (with
/// `findings`/`uart_output` carrying the reason) when the board has no MCU, the backend
/// is an external emulator we skip on the web path, or the firmware failed to load. The
/// findings reuse the exact same `{what, why, fix}` cards as the static sections (via
/// [`crate::plain::plain_faults`]), so the web and CLI co-sim surfaces can never
/// disagree.
#[derive(Debug, Clone, Default, Serialize)]
#[allow(clippy::struct_excessive_bools, reason = "each bool is a wire field")]
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
    /// for electrical results there: it refuses rather than fakes them. A clean
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
    /// itself, e.g. Renode hardware-NSS `FinishTransmission`), or `heuristic` (no
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

/// Additive JSON-only data for the synchronous firmware report.
///
/// Keeping this out of [`WebCosimSection`] preserves source compatibility for downstream
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
    /// Per-MCU execution evidence, the same rows the CLI `--json` carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution: Vec<crate::execution::ExecutionEvidence>,
    /// What the run looked like from the bench: per-pin state, where each core
    /// is parked, serial lines, bus transactions and the event timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<hauksbee_ir::observations::BenchReport>,
    /// True when the board's MCU needs an emulator (Renode, QEMU) that the
    /// synchronous analysis never runs: the page offers to run it as a job.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external_backend: bool,
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
    ///
    /// # Errors
    ///
    /// Returns an error if the report or a coverage field cannot be serialised to
    /// JSON.
    pub fn to_json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(&self.report)?;
        if let Some(cosim) = value
            .get_mut("cosim")
            .and_then(serde_json::Value::as_object_mut)
        {
            insert_coverage_fields(cosim, &self.coverage)?;
        }
        Ok(value)
    }
}

/// Put the additive coverage keys into a serialised `cosim` object.
fn insert_coverage_fields(
    cosim: &mut serde_json::Map<String, serde_json::Value>,
    coverage: &WebFirmwareCoverage,
) -> Result<(), serde_json::Error> {
    if !coverage.timing_coverage.is_empty() {
        cosim.insert(
            "timing_coverage".to_string(),
            serde_json::to_value(&coverage.timing_coverage)?,
        );
    }
    if !coverage.timing_refusals.is_empty() {
        cosim.insert(
            "timing_refusals".to_string(),
            serde_json::to_value(&coverage.timing_refusals)?,
        );
    }
    if !coverage.fallback_windows.is_empty() {
        cosim.insert(
            "fallback_windows".to_string(),
            serde_json::to_value(&coverage.fallback_windows)?,
        );
    }
    if !coverage.execution.is_empty() {
        cosim.insert(
            "execution".to_string(),
            serde_json::to_value(&coverage.execution)?,
        );
    }
    if let Some(bench) = &coverage.bench {
        cosim.insert("bench".to_string(), serde_json::to_value(bench)?);
    }
    if coverage.external_backend {
        cosim.insert(
            "external_backend".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    Ok(())
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

/// One sim-time window `[start_s, end_s)` where the analog co-sim solve failed.
///
/// The solve failed to converge and the run held stale voltages. Reported so the
/// browser can show the exact span whose electrical results are untrustworthy. Mirrors
/// the CLI/JSON `CosimFailedWindow`.
#[derive(Debug, Clone, Serialize)]
pub struct WebFailedWindow {
    pub start_s: f64,
    pub end_s: f64,
    /// The solver's own refusal message for this window, carrying the blame
    /// clause that names the net which refused to settle, the devices on it,
    /// and any near-zero-ohm link poisoning the matrix. Never empty: a
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

/// One imported object and the exact completeness of what the reader recovered.
///
/// Coordinates are optional because a netlist/schematic can recover the
/// electrical object without carrying a board location; the browser lists those
/// rows but cannot honestly paint them on the board overlay.
#[derive(Debug, Clone, Serialize)]
pub struct WebImportObject {
    pub id: String,
    /// `"recovered"` when reference, pins and (for the overlay) location are
    /// all present; `"partial"` when at least one named field is absent.
    pub status: String,
    /// `"high"`, `"medium"` or `"low"`, derived only from fields actually
    /// present and whether connectivity was declared or reconstructed.
    pub confidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    pub explanation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nets: Vec<String>,
}

/// One import limitation that needs explanation rather than a false object marker.
///
/// A `net` is carried only when the reader named one exact reconstructed
/// net, which lets the browser make the explanation clickable.
#[derive(Debug, Clone, Serialize)]
pub struct WebImportIssue {
    pub kind: String,
    pub title: String,
    pub explanation: String,
    pub suggested_fix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
}

/// Typed import coverage for the browser.
///
/// Counts are over actual recovered
/// component objects; `missing_or_refused` counts reader limitations whose
/// absent object cannot be enumerated or located without fabricating it.
#[derive(Debug, Clone, Serialize)]
pub struct WebImportDiagnostics {
    pub format: String,
    pub recovered: usize,
    pub partial: usize,
    pub unplaced: usize,
    pub missing_or_refused: usize,
    pub objects: Vec<WebImportObject>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<WebImportIssue>,
}

pub use crate::import_failure::{ImportFailureKind, WebImportFailure, WebSupportedFormat};

/// What one bound component's card says it can take, and the logic levels it reads.
/// Every field is omitted when the model does not state it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ComponentRatings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_current_a: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pin_current_a: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_power_w: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_voltage_v: Option<f64>,
    /// Lowest voltage the part reads as a high.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vih: Option<f64>,
    /// Highest voltage the part reads as a low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vil: Option<f64>,
}

impl ComponentRatings {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The largest `run --check --json` document sent inside a web report. A real
/// board's is well under 1 MiB; past this the document only repeats what the
/// report already carries (its evidence and findings), so it is left out and
/// `check_report_omitted_bytes` says how big it is.
const WEB_CHECK_REPORT_MAX_BYTES: usize = 1024 * 1024;

/// The whole payload sent back to the browser after an upload.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WebReport {
    pub ok: bool,
    /// Set when extraction failed; `ok` is false and the rest is empty.
    pub error: Option<String>,
    pub board_name: String,
    pub file_name: String,
    pub num_components: usize,
    pub num_nets: usize,
    /// The rule source used by the copper section. A browser must not infer that
    /// a fallback value came from the uploaded design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clearance_rule_source: Option<crate::result::ClearanceRuleProvenance>,
    /// The same structured order-decision buckets emitted by CLI report JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<crate::plain::OrderTriage>,
    /// The first few serious causes to fix, from the same ordering the CLI's
    /// plain check report prints.
    #[serde(default, skip_serializing_if = "crate::plain::FixFirst::is_empty")]
    pub fix_first: crate::plain::FixFirst,
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
    /// What the importer recovered per object, plus every named reader limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_diagnostics: Option<WebImportDiagnostics>,
    /// Parser-stage diagnosis for an unreadable input. Mutually exclusive with
    /// `import_diagnostics` on reports produced by this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_failure: Option<WebImportFailure>,
    /// Bind-role honesty summary (active ICs bound / open on the live circuit).
    /// Present whenever the board bound; the browser renders a warning banner
    /// when `active_path_unresolved` is non-empty and must NOT say "Looks
    /// healthy" while it is. Mirrors the CLI/JSON bind surface (parity fix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<BindSummaryWeb>,
    /// Per-component model identity and executable-behaviour coverage from
    /// the same resolver as `hauksbee models coverage`. A bound part may still
    /// be partial; the browser must show `missing` rather than collapsing this
    /// to the older bound/open bit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_coverage: Option<crate::commands::models::ModelCoverageSnapshot>,
    /// Content-addressed inputs consumed by the analysis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// First-class assumptions from the same evidence object as CLI JSON/plain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// Plain-English explanations of every term the page can show a popover for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub glossary: Vec<hauksbee_ir::glossary::TermEntry>,
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
    /// reference -> resolved model kind ("mcu", `"bjt_npn"`, ...), from the same
    /// bind the summary uses: what a component click on the board map reports
    /// as the part's bound model. A `BTreeMap` so the JSON is deterministic
    /// (the golden-parity test compares bytes). Additive; omitted when empty.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub component_kinds: std::collections::BTreeMap<String, String>,
    /// reference -> assertion kinds the bound circuit can actually evaluate.
    /// This is deliberately derived from the same stamped devices and stress
    /// metadata as the CI runner's fail-closed trackability check. The browser
    /// must not offer a component assertion merely because a reference exists.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub component_assertions: std::collections::BTreeMap<String, Vec<String>>,
    /// reference -> the limits the bound card states (current, pin current, power,
    /// voltage) and the logic input thresholds it drives with. The checks builder
    /// pre-fills a ceiling from these ("80 % of the datasheet's maximum"). Only
    /// numbers the model carries are present, and a part with none is omitted;
    /// there is no source page here, so the UI must not cite one. Additive.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub component_ratings: std::collections::BTreeMap<String, ComponentRatings>,
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
    /// Why the verdict is not a pass, and what to do next, in one plain
    /// sentence: the SAME [`crate::result::VerdictCause::reason`] text `--json`, `--check` and
    /// the CI report carry. Absent when nothing blocks the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_reason: Option<String>,
    /// The assumptions a pass rests on beyond the checks (each class-only
    /// part whose assumed corners were swept), one sentence each: the same
    /// [`crate::evidence::BoardEvidence::conditions`] `--json` carries. A
    /// pass with any is conditional on them. Absent when there are none or
    /// the verdict is not a pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdict_conditions: Vec<String>,
    /// [`Self::verdict_reason`] without its `next:` clause. That clause names
    /// terminal commands (`hauksbee models extract ...`); the app shows this
    /// sentence and offers [`Self::next_actions`] as buttons instead. Absent
    /// when `verdict_reason` is. Additive: `verdict_reason` is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_why: Option<String>,
    /// The steps that would move this verdict forward, as buttons: give the
    /// open parts models, add a BOM, declare a supply, add the project rules
    /// file. Each comes from typed data (the open-part list, an assumption's
    /// remedy), never from parsing prose. Empty (and omitted) when nothing is
    /// open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<WebAction>,
    /// The machine verdict, `"pass"` | `"fail"` | `"invalid"`: the same
    /// derivation `run --check --json` prints for this board, so the browser
    /// can never read a pass the CLI fails. Absent on a report that could not
    /// read the board.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verdict: String,
    /// On a pass with items the order triage lists to inspect, the verdict's
    /// tag for it ("inspect before ordering (3 items)",
    /// `RunVerdict::inspect_tag`): the words the CLI's closing `VERDICT:` line
    /// uses, so the browser never calls that pass clean. Absent otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verdict_inspect: String,
    /// True when the verdict is PARTLY JUDGED (`invalid`, never a pass): the
    /// same flag `run --check --json` prints. Absent otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partly_judged: bool,
    /// What a PARTLY JUDGED run judged and what it did not (the open parts and
    /// the nets whose checks they could change): the same object `run --check
    /// --json` prints, so the browser names the judged scope beside the
    /// verdict. Absent when every key part is modelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgement_scope: Option<crate::scope::JudgementScope>,
    /// The `run --check --json` document this verdict was derived from (with
    /// any firmware co-sim faults folded in), rendered by the same
    /// `JsonReport::to_json` the CLI prints, so the browser's "CI verdict as
    /// JSON" export is the CI's own document rather than a re-derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_report: Option<serde_json::Value>,
    /// Set (to the document's size in bytes) instead of `check_report` when the
    /// document is too large to send to a browser. Its content is already in
    /// the report's own sections, evidence and triage; the full document is
    /// what `hauksbee run --check --json` prints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_report_omitted_bytes: Option<usize>,
    /// The `--check` machine document for the static board, which the verdict
    /// is derived from. Firmware co-sim faults are folded into a copy of it
    /// when the verdict is derived, never into this one.
    #[serde(skip)]
    machine: Option<crate::result::JsonReport>,
    /// The headline as the sections and co-sim left it, before the verdict
    /// set its class. The firmware pass restarts from it.
    #[serde(skip)]
    pre_verdict_headline: String,
}

impl WebReport {
    /// Derive [`Self::verdict`], [`Self::verdict_reason`],
    /// [`Self::verdict_conditions`] and the verdict class of the headline from
    /// the machine document, plus any firmware co-sim faults and refusal.
    /// Called after the last mutation of the verdict inputs (findings, bind,
    /// refusal, co-sim).
    fn explain_verdict(&mut self) {
        let Some(mut machine) = self.machine.clone() else {
            return;
        };
        machine.refusal.clone_from(&self.refusal);
        // The verdict carries the triage's inspect count, as `--check` does.
        machine.inspect_items = self.triage.as_ref().map_or(0, |t| t.inspect.len());
        // A co-sim fault gates like the CLI's co-sim surface: a destroyed part
        // is serious, and any raised over-stress fails the gate.
        let faults: Vec<crate::result::JsonFinding> = self
            .cosim
            .iter()
            .flat_map(|c| &c.findings)
            .filter(|f| f.level == "serious" || f.level == "warning")
            .map(|f| crate::result::JsonFinding {
                check: "cosim".into(),
                kind: "fault".into(),
                severity: f.level.clone(),
                nets: Vec::new(),
                location_mm: None,
                layer: None,
                refs: Vec::new(),
                actionable: true,
                message: f.what.clone(),
                plain: f.what.clone(),
                fix: (!f.fix.is_empty()).then(|| f.fix.clone()),
            })
            .collect();
        if !faults.is_empty() {
            machine.surface_gate_fails = true;
            machine.findings.get_or_insert_with(Vec::new).extend(faults);
        }
        let verdict = machine.run_verdict();
        let document = machine.to_json();
        if document.len() > WEB_CHECK_REPORT_MAX_BYTES {
            // A very large board's document repeats the report's evidence,
            // findings and copper detail; the browser already has those.
            self.check_report = None;
            self.check_report_omitted_bytes = Some(document.len());
        } else {
            self.check_report = serde_json::from_str(&document).ok();
            self.check_report_omitted_bytes = None;
        }
        self.pre_verdict_headline.clone_from(&self.headline);
        self.headline = verdict_headline(&verdict, &self.headline, self.serious);
        self.verdict = verdict.verdict.to_string();
        self.verdict_inspect = verdict.inspect_tag().unwrap_or_default();
        self.partly_judged = verdict.partial;
        self.judgement_scope.clone_from(&machine.judgement_scope);
        self.verdict_reason = verdict.reason;
        self.next_actions = self.derive_next_actions();
        // Routing is the only blocking problem: the headline frames the state and
        // the strip says what ran, so the reason names the blocker once, briefly.
        let only_unrouted = format!(
            "1 blocking problem(s), first: {}",
            hauksbee_extract::NOT_ROUTED_MESSAGE_PREFIX
        );
        if self.verdict == "fail"
            && self
                .verdict_reason
                .as_ref()
                .is_some_and(|reason| reason.starts_with(&only_unrouted))
        {
            self.verdict_reason = Some(crate::plain::PRE_LAYOUT_REASON.to_string());
        }
        self.verdict_why = self
            .verdict_reason
            .as_deref()
            .map(crate::web_actions::reason_without_next);
        self.verdict_conditions = verdict.conditions;
    }

    /// The buttons for [`Self::next_actions`]: the open key parts first (the
    /// thing the verdict is waiting on), then whatever the assumptions' typed
    /// remedies ask for, then the project rules file when the copper check ran
    /// on default rules for want of it.
    fn derive_next_actions(&self) -> Vec<WebAction> {
        let open = self
            .bind
            .as_ref()
            .map(|b| b.active_path_unresolved.as_slice())
            .unwrap_or_default();
        let mut out = crate::web_actions::model_actions(open);
        for action in crate::web_actions::from_remedies(&self.assumptions) {
            // The grouped AddModel remedy repeats the open-part button.
            if action.id == hauksbee_frontdoor_api::actions::ActionId::ModelParts && !out.is_empty()
            {
                continue;
            }
            push_unique(&mut out, action);
        }
        if self
            .clearance_rule_source
            .as_ref()
            .is_some_and(crate::result::ClearanceRuleProvenance::wants_project_rules)
        {
            push_unique(&mut out, WebAction::add_project_rules(".kicad_pro"));
        }
        out
    }
}

/// How a pre-layout headline starts (see `plain::pre_layout_headline`).
const PRE_LAYOUT_HEADLINE_LEAD: &str = "Not routed yet:";

/// Headlines that already say the run could not be judged.
const BIND_OPEN_HEADLINE: &str = "No blocking issues, but active parts are unresolved; analog/AC/thermal results are not trustworthy. See the notes.";
const UNDERMINED_HEADLINE: &str = "Evidence is undermined; results touching unresolved inputs are invalid for analysis. See the evidence below.";
const FIRMWARE_UNDERMINED_HEADLINE: &str = "Firmware evidence is undermined; substituted or unresolved inputs invalidate affected co-sim assertions.";
const REFUSAL_HEADLINE: &str =
    "Analysis invalid for the requested firmware co-simulation. Static board findings remain valid.";

/// The headline in the class the machine verdict states.
///
/// The sections and the co-sim pass describe what they found; this makes sure
/// the words never claim a different outcome than the verdict. A failing run
/// never reads as fine, an inconclusive one never as a pass, and a pass that
/// rests on conditions names them (the browser lists them under the lead-in).
fn verdict_headline(verdict: &crate::result::RunVerdict, current: &str, serious: usize) -> String {
    let inconclusive = [
        BIND_OPEN_HEADLINE,
        UNDERMINED_HEADLINE,
        FIRMWARE_UNDERMINED_HEADLINE,
        REFUSAL_HEADLINE,
    ];
    match verdict.verdict {
        // A serious finding's headline already says to fix it before ordering.
        "fail" if serious > 0 || current.starts_with(PRE_LAYOUT_HEADLINE_LEAD) => current.to_string(),
        "fail" => "Not OK to order: a finding fails the check gate; see the reason below.".to_string(),
        _ if verdict.partial => "Partly judged, not a pass: nothing found blocks ordering among \
             the checks this run could judge, but parts with no usable model leave the rest \
             unjudged; see the reason below."
            .to_string(),
        "invalid" if inconclusive.contains(&current) => current.to_string(),
        "invalid" => "Inconclusive, not a pass: nothing found blocks ordering, but this run cannot vouch for the board; see the reason below.".to_string(),
        _ if verdict.unorderable.is_some() => {
            // A schematic has no board to order: say what was judged instead.
            let why = verdict.unorderable.as_deref().unwrap_or_default();
            let mut lead = hauksbee_ir::vocabulary::NOT_ORDERABLE_TAG.to_string();
            if let Some(first) = lead.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            format!("{lead}: {why}")
        }
        _ => {
            // A pass with items the triage lists to inspect is not plainly OK
            // to order: it takes the tag the closing `VERDICT:` line prints.
            let ok = verdict.inspect_tag().map_or_else(
                || "OK to order".to_string(),
                |tag| {
                    let mut chars = tag.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_uppercase().chain(chars).collect()
                    })
                },
            );
            let healthy = current == overall_headline(0, 0, false, false);
            let heads_up = current == overall_headline(0, 0, true, false);
            if verdict.conditions.is_empty() {
                return match verdict.inspect_tag() {
                    // A headline that calls the board healthy would contradict
                    // the items to inspect; any other already says to look.
                    Some(_) if healthy || heads_up => format!(
                        "{ok}: nothing found blocks ordering, but the items listed to \
                         inspect need checking by hand first."
                    ),
                    _ => current.to_string(),
                };
            }
            let conditions = verdict.conditions.join(" ");
            if healthy {
                format!(
                    "{ok}, conditionally: the static checks found nothing that blocks \
                     ordering; this rests on: {conditions}"
                )
            } else if heads_up {
                format!(
                    "{ok}, conditionally: nothing found blocks ordering (see the \
                     heads-up note below); this rests on: {conditions}"
                )
            } else {
                format!("{current} {ok}, conditionally; this rests on: {conditions}")
            }
        }
    }
}

/// A binder-detected power supply, as the checks builder consumes it.
#[derive(Debug, Clone, Serialize)]
pub struct WebSupply {
    pub net: String,
    pub volts: f64,
}

/// The "could not read the file" report shape, shared by every early-return in
/// [`analyze`] so the error surface stays consistent.
#[allow(clippy::too_many_lines, reason = "one pass: objects, then note issues")]
fn web_import_diagnostics(norm: &crate::board_input::NormalizedBoard) -> WebImportDiagnostics {
    use crate::board_input::InputKind;

    let format = match norm.kind {
        InputKind::Text => "text board / netlist",
        InputKind::Schematic => "KiCad schematic",
        InputKind::Altium => "Altium PcbDoc",
        InputKind::Gerber => "Gerber / Excellon reconstruction",
        InputKind::Ipc356Archive => "fab archive with IPC-D-356 connectivity",
        InputKind::Odb => "ODB++",
        InputKind::Ipc2581 => "IPC-2581",
        InputKind::Easyeda => "EasyEDA",
    }
    .to_string();

    let net_names: std::collections::BTreeMap<i64, String> = norm
        .board
        .nets
        .iter()
        .map(|net| (net.id, net.name.clone()))
        .collect();
    let reconstructed = norm.kind == InputKind::Gerber;
    let mut recovered = 0usize;
    let mut partial = 0usize;
    let mut unplaced = 0usize;
    let mut objects = Vec::with_capacity(norm.board.components.len());

    for (component_index, component) in norm.board.components.iter().enumerate() {
        let mut gaps = Vec::new();
        if component.reference.trim().is_empty() {
            gaps.push("reference was not recovered");
        }
        if component.pins.is_empty() {
            gaps.push("no pins were recovered");
        }
        if component.position.is_none() {
            gaps.push("the source supplied no board coordinate");
            unplaced += 1;
        }
        if component.value.trim().is_empty() {
            gaps.push("value / part identity was not recovered");
        }

        let status = if gaps.is_empty() {
            recovered += 1;
            "recovered"
        } else {
            partial += 1;
            "partial"
        };
        let confidence = if component.reference.trim().is_empty() || component.pins.is_empty() {
            "low"
        } else if reconstructed || component.position.is_none() || component.value.trim().is_empty()
        {
            "medium"
        } else {
            "high"
        };
        let explanation = if gaps.is_empty() {
            if reconstructed {
                "Reference, value, pins and location were recovered; connectivity is reconstructed from manufacturing copper rather than declared by CAD."
                    .to_string()
            } else {
                "Reference, value, pins and board location were recovered from the input."
                    .to_string()
            }
        } else {
            gaps.join("; ")
        };
        let mut nets: Vec<String> = component
            .pins
            .iter()
            .filter_map(|pin| pin.net.and_then(|id| net_names.get(&id).cloned()))
            .collect();
        nets.sort();
        nets.dedup();
        let (x, y) = component
            .position
            .map_or((None, None), |(x, y, _)| (Some(x), Some(y)));
        objects.push(WebImportObject {
            id: if component.reference.trim().is_empty() {
                format!("unnamed component {}", component_index + 1)
            } else {
                component.reference.clone()
            },
            status: status.to_string(),
            confidence: confidence.to_string(),
            x,
            y,
            explanation,
            nets,
        });
    }
    objects.sort_by(|a, b| a.id.cmp(&b.id));

    let mut issues = Vec::new();
    let mut missing_or_refused = 0usize;
    for note in &norm.notes {
        let lower = note.to_ascii_lowercase();
        // "split" alone is not a net diagnosis: a plating refusal can tell
        // the exporter to split PTH and NPTH drill files. Only phrases emitted
        // for reconstructed-connectivity uncertainty belong in the split-net
        // bucket.
        let is_split = lower.contains("under-connected") || lower.contains("synthetic name");
        let is_missing = lower.contains("were not")
            || lower.contains("missing")
            || lower.contains("refused")
            || lower.contains("dropped")
            || lower.contains("could not")
            || lower.contains("stitch no layers")
            || lower.contains("nothing in this job says whether");
        if !is_split && !is_missing {
            continue;
        }
        if is_missing {
            missing_or_refused += 1;
        }
        let linked_net = norm
            .board
            .nets
            .iter()
            .filter(|net| !net.name.is_empty() && note.contains(&net.name))
            .max_by_key(|net| net.name.len())
            .map(|net| net.name.clone());
        let (title, suggested_fix) = if is_split {
            (
                "Possible split-net boundary",
                "Supply the original CAD layout or an authoritative IPC-D-356 / ODB++ netlist so the connection can be declared instead of inferred.",
            )
        } else if reconstructed {
            (
                "Manufacturing object was not fully recovered",
                "Add the missing pick-and-place or drill metadata, or analyze the original CAD layout alongside this fab package.",
            )
        } else {
            (
                "Reader refused incomplete evidence",
                "Correct or re-export the named source record, then analyze the new export; Hauksbee will not invent the missing object.",
            )
        };
        issues.push(WebImportIssue {
            kind: if is_split { "split_net" } else { "missing" }.to_string(),
            title: title.to_string(),
            explanation: note.clone(),
            suggested_fix: suggested_fix.to_string(),
            net: linked_net,
        });
    }

    if reconstructed {
        let synthetic: Vec<String> = norm
            .board
            .nets
            .iter()
            .filter(|net| net.name.starts_with("NET_"))
            .map(|net| net.name.clone())
            .collect();
        if !synthetic.is_empty() && !issues.iter().any(|issue| issue.kind == "split_net") {
            issues.push(WebImportIssue {
                kind: "split_net".to_string(),
                title: "Reconstructed net boundaries".to_string(),
                explanation: format!(
                    "{} net(s) keep synthetic names because the fab files did not declare their electrical identity. Copper connectivity was recovered, but a missing plane or drill declaration can leave one physical net split into several reconstructed nets.",
                    synthetic.len()
                ),
                suggested_fix: "Supply the original CAD layout or an authoritative IPC-D-356 / ODB++ netlist to replace geometric inference.".to_string(),
                net: synthetic.first().cloned(),
            });
        }
    }

    WebImportDiagnostics {
        format,
        recovered,
        partial,
        unplaced,
        missing_or_refused,
        objects,
        issues,
    }
}

fn error_line_number(message: &str) -> Option<usize> {
    let lower = message.to_ascii_lowercase();
    for (index, _) in lower.match_indices("line") {
        if index > 0 && lower.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail = &message[index + "line".len()..];
        let Some(tail) = tail.strip_prefix(':').or_else(|| tail.strip_prefix(' ')) else {
            continue;
        };
        let digits: String = tail
            .chars()
            .skip_while(char::is_ascii_whitespace)
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(line) = digits.parse::<usize>() {
            if line > 0 {
                return Some(line);
            }
        }
    }
    None
}

fn web_import_failure(
    error: &crate::board_input::BoardInputError,
    file_name: &str,
    contents: &[u8],
) -> WebImportFailure {
    let message = error.to_string();
    let excerpt = std::str::from_utf8(contents).ok().and_then(|text| {
        let line = error_line_number(&message)?;
        text.lines().nth(line.saturating_sub(1)).map(|body| {
            format!(
                "line {line}: {}",
                body.chars().take(300).collect::<String>()
            )
        })
    });
    crate::import_failure::classify(error, file_name, contents, excerpt)
}

/// The board name a report shows: the board's own title, else the uploaded
/// file's stem (directory part and one extension removed, as the CLI's
/// file-stem fallback does), so `board_name` is never blank beside a set
/// `file_name`. A name with no usable stem stays empty rather than invented.
fn display_board_name(board_name: &str, file_name: &str) -> String {
    if !board_name.trim().is_empty() {
        return board_name.to_string();
    }
    let leaf = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = match leaf.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => leaf,
    };
    stem.trim().to_string()
}

fn unreadable_with_import_failure(
    file_name: &str,
    error: String,
    import_failure: Option<WebImportFailure>,
) -> WebReport {
    WebReport {
        ok: false,
        error: Some(error),
        file_name: file_name.to_string(),
        headline: "Could not read the file.".to_string(),
        import_failure,
        ..WebReport::default()
    }
}

fn unreadable(file_name: &str, error: String) -> WebReport {
    unreadable_with_import_failure(file_name, error, None)
}

/// Whether a bound device name belongs to the component with this reference:
/// the reference itself, or a multi-unit/section suffix of it (`_q`, `_s`, `_e`).
/// The reference definition; [`unit_index`] answers the same question for every
/// component in one pass (a test keeps the two equal).
#[cfg(test)]
fn ref_or_unit_matches(reference: &str, device_name: &str) -> bool {
    device_name == reference
        || device_name.strip_prefix(reference).is_some_and(|suffix| {
            suffix.starts_with("_q") || suffix.starts_with("_s") || suffix.starts_with("_e")
        })
}

/// Values indexed by every reference their name matches under
/// [`ref_or_unit_matches`]: the whole name, and each prefix that ends where a
/// `_q`/`_s`/`_e` unit suffix starts. One pass over the devices, so looking a
/// component up costs its name length instead of a scan of every device.
fn unit_index<'a, T: Copy>(
    items: impl Iterator<Item = (&'a str, T)>,
) -> std::collections::HashMap<&'a str, Vec<T>> {
    let mut index: std::collections::HashMap<&'a str, Vec<T>> = std::collections::HashMap::new();
    for (name, value) in items {
        index.entry(name).or_default().push(value);
        for (at, _) in name.match_indices('_') {
            if matches!(name.as_bytes().get(at + 1), Some(b'q' | b's' | b'e')) {
                index.entry(&name[..at]).or_default().push(value);
            }
        }
    }
    index
}

/// Capabilities for component-scoped assertions, derived from the bound
/// circuit rather than refdes/value heuristics. Keep this aligned with
/// `hauksbee-ci::runner::check_trackable_assert_refs`.
fn component_assertion_capabilities(
    board: &ExtractedBoard,
    bound: &BoundBoard,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let current_devices = unit_index(bound.circuit.devices.iter().filter_map(
        |device| match device {
            Device::Resistor { name, .. } | Device::Diode { name, .. } => Some((name.as_str(), ())),
            _ => None,
        },
    ));
    let metas = unit_index(
        bound
            .device_meta
            .iter()
            .map(|meta| (meta.reference.as_str(), ())),
    );

    board
        .components
        .iter()
        .filter_map(|component| {
            let mut kinds = Vec::new();
            if current_devices.contains_key(component.reference.as_str()) {
                kinds.push("max_current".to_string());
            }
            if metas.contains_key(component.reference.as_str()) {
                kinds.push("max_temp".to_string());
            }
            (!kinds.is_empty()).then(|| (component.reference.clone(), kinds))
        })
        .collect()
}

/// The card limits and logic thresholds of every bound component that states any.
/// A multi-unit package merges its units' numbers by keeping the smallest limit.
fn component_ratings(
    board: &ExtractedBoard,
    bound: &BoundBoard,
) -> std::collections::BTreeMap<String, ComponentRatings> {
    fn tighter(slot: &mut Option<f64>, new: Option<f64>) {
        *slot = match (*slot, new) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    let metas = unit_index(
        bound
            .device_meta
            .iter()
            .map(|meta| (meta.reference.as_str(), meta)),
    );
    let mut digital = std::collections::HashMap::new();
    for d in &bound.digital {
        digital.entry(d.reference.as_str()).or_insert(d);
    }
    board
        .components
        .iter()
        .filter_map(|component| {
            let reference = &component.reference;
            let mut out = ComponentRatings::default();
            for meta in metas.get(reference.as_str()).into_iter().flatten().copied() {
                tighter(&mut out.max_current_a, meta.ratings.max_current_a);
                tighter(&mut out.max_pin_current_a, meta.ratings.max_pin_current_a);
                tighter(&mut out.max_power_w, meta.ratings.max_power_w);
                tighter(&mut out.max_voltage_v, meta.ratings.max_voltage_v);
            }
            if let Some(d) = digital.get(reference.as_str()) {
                out.vih = Some(d.levels.vih);
                out.vil = Some(d.levels.vil);
            }
            (!out.is_empty()).then(|| (reference.clone(), out))
        })
        .collect()
}

/// Serialize one normalizer refusal with the same localized import diagnosis
/// as [`analyze_json_with_ties`]. The shipped schematic-aware HTTP callback
/// has to normalize once before resolving companion ties; without this helper
/// its early return collapsed the response back to `{ok,error}` and silently
/// discarded the parser excerpt and suggested fix.
pub(crate) fn unreadable_input_json(
    file_name: &str,
    contents: &[u8],
    error: &crate::board_input::BoardInputError,
) -> String {
    to_json_or_failure(&unreadable_input(file_name, contents, error))
}

fn unreadable_input(
    file_name: &str,
    contents: &[u8],
    error: &crate::board_input::BoardInputError,
) -> WebReport {
    let failure = web_import_failure(error, file_name, contents);
    unreadable_with_import_failure(file_name, error.web_message(), Some(failure))
}

/// The minimal `{ok,error}` document for a report that failed to serialize.
fn serialize_failure(error: &serde_json::Error) -> String {
    format!("{{\"ok\":false,\"error\":\"failed to serialize report: {error}\"}}")
}

fn to_json_or_failure(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| serialize_failure(&e))
}

/// Run the full front-door analysis on an uploaded board file.
///
/// `file_name` is used only for display and to pick the reader for an
/// extension-routed format. `contents` is the
/// file's RAW bytes: binary formats (Altium `.PcbDoc`, an OLE2 container) are
/// sniffed and read from the bytes first, exactly like the CLI `run` path, and
/// only a non-binary input falls back to the text sniffer over a lossy-UTF8
/// view (which is exact for the text formats). Decoding before this point
/// would corrupt a binary board before it was ever parsed. All of that routing
/// lives in [`crate::board_input::from_bytes`], the SAME normalizer the CLI
/// path uses, so the web and CLI surfaces can never disagree about what a
/// board file is.
#[must_use]
pub fn analyze(file_name: &str, contents: &[u8]) -> WebReport {
    match crate::board_input::from_bytes(file_name, contents) {
        Ok(norm) => analyze_normalized_with_ties(file_name, &norm, None).0,
        Err(e) => unreadable_input(file_name, contents, &e),
    }
}

/// The static analysis on an already-normalized board, with an optional
/// companion schematic's declared net ties. Split from [`analyze`] so
/// [`analyze_with_firmware`] can normalize ONCE and hand the same
/// [`crate::board_input::NormalizedBoard`] to both the static report and the
/// co-sim, instead of re-reading the original bytes with a weaker sniffer.
/// Also returns the DRC report it computed, so the firmware path can bridge
/// the REAL copper shorts into the co-sim circuit instead of simulating a
/// board the DRC section (on the same page) says is shorted.
///
/// The ties are a parameter rather than read inside `analyze`, because the two entries have
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
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    analyze_normalized_with_context(file_name, norm, schematic_ties, &lib, None, &[])
}

#[allow(clippy::too_many_lines, reason = "linear: DRC, bind, evidence, report")]
fn analyze_normalized_with_context(
    file_name: &str,
    norm: &crate::board_input::NormalizedBoard,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
    lib: &ModelLibrary,
    prebound: Option<&crate::binder::BoundBoard>,
    supporting_artifacts: &[hauksbee_ir::evidence::ArtifactProvenance],
) -> (WebReport, hauksbee_extract::DrcReport) {
    let is_binary = norm.is_binary();
    let is_gerber = norm.is_gerber();
    let board = &norm.board;
    // Text view for the geometry-bearing text checks (DRC / SI). A binary or
    // gerber board has no KiCad layout text: those checks get their bytes twin
    // (Altium) or nothing, stated in the report rather than silently green.
    let text_view: Option<&str> = norm.layout_text.as_deref();

    // DRC reads copper geometry from the raw input: the bytes twin
    // (`altium_drc`) for a binary board, the KiCad layout text otherwise. A
    // gerber archive has neither; its DRC section says so below instead of
    // reporting a vacuous "no problems".
    hauksbee_ir::progress::begin_copper();
    let drc = if is_binary {
        ExtractedBoard::altium_drc(&norm.raw).unwrap_or_default()
    } else if is_gerber {
        hauksbee_extract::DrcReport::default()
    } else {
        ExtractedBoard::drc(text_view.unwrap_or_default()).unwrap_or_default()
    };
    let qualification = schematic_ties.map(|ties| ties.qualify(&drc));
    // Render from the grouped structure (single source of truth shared with the
    // CLI text/plain/json surfaces): duplicates collapsed, and gap==rule labelled
    // "at minimum clearance (no margin)" rather than the wrong "below the rule".
    let clearance_rule_source = if is_gerber {
        None
    } else if is_binary {
        Some(crate::result::ClearanceRuleProvenance::tool_default(
            drc.clearance_mm,
        ))
    } else if text_view.is_some_and(|text| !text.contains("(kicad_pcb")) {
        let provenance = crate::result::ClearanceRuleProvenance::board_file(drc.clearance_mm);
        // A sweep that never ran (an empty default report) read no rule at all;
        // a rule that was read keeps its own wording even when it is small.
        Some(if drc.primitive_count == 0 && drc.clearance_mm <= 0.0 {
            provenance.with_no_rule_read()
        } else {
            provenance
        })
    } else {
        Some(crate::result::ClearanceRuleProvenance::defaulted(
            drc.clearance_mm,
            false,
        ))
    };
    let drc_structured = crate::result::DrcStructured::from_report_with_ties(
        &drc,
        qualification.as_ref(),
        norm.layout_text
            .as_deref()
            .is_some_and(|text| text.contains("<eagle"))
            && schematic_ties.is_none(),
    )
    .with_clearance_rule_provenance(
        &clearance_rule_source
            .clone()
            .unwrap_or_else(|| crate::result::ClearanceRuleProvenance::defaulted(0.0, false)),
    );
    // A gerber archive reads no clearance rule: say so, not a default value
    // nobody measured against.
    let gerber_default_source =
        crate::result::ClearanceRuleProvenance::defaulted(drc_structured.clearance_rule_mm, false)
            .with_no_rule_read();
    let drc_rule_source = clearance_rule_source
        .as_ref()
        .unwrap_or(&gerber_default_source);
    let drc_plain = plain_drc_structured_with_rule_source(&drc_structured, drc_rule_source);

    // Bind FIRST (also consumed below for the report panel and evidence): the
    // lint/SI section verdicts need the unmodelled-critical part list so the
    // web sections read INCONCLUSIVE, not "Looks healthy", over an unbound
    // main IC or power FET, the same contract as the CLI surfaces.
    let fallback_bound;
    #[allow(
        clippy::option_if_let_else,
        clippy::single_match_else,
        reason = "a closure cannot init the deferred borrow"
    )]
    let bound = match prebound {
        Some(bound) => bound,
        None => {
            hauksbee_ir::progress::begin_bind(board.components.len(), 1);
            fallback_bound = bind_board(board, lib);
            &fallback_bound
        }
    };
    let bind_summary = BindSummary::from_report(&bound.report);
    let verdict_blockers = crate::result::unmodelled_critical_refs(&bind_summary);

    // Lint = the same bundle the `--lint` CLI surface assembles, via the single
    // `engine_lint` chokepoint (connectivity + strap lint + MCU resource conflicts
    // + the unchecked-strap-bearing-MCU coverage note), so the web report never
    // prints "Looks healthy" over a strap-bearing MCU whose BOOT0 was unexamined.
    // Swept across the assumed corners of class-only parts, as `--check` is.
    hauksbee_ir::progress::begin_lint();
    let (mut lint, corner_plan) = crate::checks::engine_lint_swept(board, lib);
    lint.findings
        .extend(crate::checks::layout_lint(board, text_view, None).findings);
    let mut lint_plain = plain_netlint(&lint);
    lint_plain.unmodelled_critical.clone_from(&verdict_blockers);

    // SI needs the layout text for the geometry-bearing checks; a binary board
    // has none, so it gets the netlist-only subset (`None`). Route through the SI
    // chokepoint so the web report carries the trace-ampacity + input-cap-ripple
    // findings too; the bare `si_checks` left the web "Signal integrity" section
    // silently missing an under-width power trace the CLI `--si` flags.
    hauksbee_ir::progress::begin_untracked(hauksbee_ir::progress::Stage::Signals);
    let si = crate::checks::engine_si(board, lib, text_view);
    let mut si_plain = plain_si(&si);
    si_plain.unmodelled_critical.clone_from(&verdict_blockers);
    // The open parts are named once. Connectivity comes first, so when its
    // verdict states them in full (no findings of its own to say instead) the
    // signal-integrity verdict points back rather than repeating the paragraph.
    let lint_states_open_parts = lint_plain.findings.is_empty() && !verdict_blockers.is_empty();
    si_plain.inconclusive_said_above = lint_states_open_parts;

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
                actions: Vec::new(),
            }
        } else {
            let provenance = clearance_rule_source
                .as_ref()
                .expect("non-gerber DRC has clearance provenance");
            let mut section =
                WebSection::from_plain(drc_structured.section_title(provenance), &drc_plain);
            let mut lead = provenance.web_notice();
            if let Some(note) = drc_structured.default_rule_flood_note(provenance) {
                lead.push(' ');
                lead.push_str(&note);
            }
            section.verdict = format!("{lead} {}", section.verdict);
            section
        },
        WebSection::from_plain("Connectivity & wiring", &lint_plain),
        WebSection::from_plain("Signal integrity", &si_plain),
    ];

    // Buttons for the section that states the open parts in full: the same
    // fix the CLI prints as commands, as actions the app can run.
    let open_part_actions = crate::web_actions::model_actions(&verdict_blockers);
    if lint_states_open_parts {
        sections[1].actions.clone_from(&open_part_actions);
    } else if si_plain.findings.is_empty() && !verdict_blockers.is_empty() {
        sections[2].actions.clone_from(&open_part_actions);
    }
    if clearance_rule_source
        .as_ref()
        .is_some_and(crate::result::ClearanceRuleProvenance::wants_project_rules)
        && !is_gerber
    {
        push_unique(
            &mut sections[0].actions,
            WebAction::add_project_rules(".kicad_pro"),
        );
    }

    // USB-C CC compliance: the CLI text/plain/json surfaces all carry this
    // verdict (a Serious shared-CC-pulldown fault gates `--check --strict`), but
    // omitting it from the web persona lets a board with the RPi-4 fault
    // read "Looks healthy". Fold it in so all four personas agree: a Serious
    // verdict becomes a serious WebFinding (raising serious/total), an Info
    // verdict becomes a heads-up (suppressing a false "Looks healthy").
    let usbc = crate::usb_c_report(board);
    if let Some(section) = usbc.as_ref().and_then(usbc_web_section) {
        sections.push(section);
    }

    let usbc_reliable = crate::reports::usb_c::scoped_blockers(board, &verdict_blockers).is_empty();
    // What an indicator MEANS is not judged without a stated outcome; the
    // triage and the browser's notes both say so.
    let indicators = crate::checks::indicators::indicator_traces(board, lib);
    let indicators_not_judged = crate::checks::indicators::not_judged_note(&indicators);
    let triage = (!is_gerber).then(|| {
        let mut triage = crate::plain::order_triage_with_rule_source(
            &drc_structured,
            clearance_rule_source
                .as_ref()
                .expect("non-gerber DRC has clearance provenance"),
            &lint,
            &si,
            usbc.as_ref(),
            usbc_reliable,
            bind_summary.unresolved,
            false,
        );
        triage.add_not_covered(indicators_not_judged.clone());
        triage.add_not_covered(crate::plain::values_not_carried_note(board));
        triage
    });

    let serious: usize = sections
        .iter()
        .map(|s| s.findings.iter().filter(|f| f.level == "serious").count())
        .sum();
    let total: usize = sections.iter().map(|s| s.findings.len()).sum();
    // An unrouted board holds its copper findings back (listed apart, labelled
    // held back), so those do not count. The not-routed state is listed and
    // tagged serious, so it DOES count: every number on the page is this one.
    hauksbee_ir::progress::begin(hauksbee_ir::progress::Stage::Report, 0);
    let fix_first = crate::plain::fix_first(&drc_plain, &lint_plain, usbc.as_ref(), &si_plain);
    let (total, serious) = if fix_first.notice.is_some() {
        crate::plain::without_held_copper(&drc_plain, total, serious)
    } else {
        (total, serious)
    };

    // The bind-role honesty data the CLI/JSON surfaces carry, in the web
    // shape, from the SAME bind computed above the sections. The web dropping
    // this would let a board with every active IC open show "Looks healthy".
    let mut bind_web = BindSummaryWeb::from_summary(&bind_summary);
    let all = bound.report.selections.values();
    bind_web.open_selections = all.filter(|s| s.determined.is_none()).cloned().collect();
    let mut actual_findings = crate::result::lint_findings_json(&lint);
    actual_findings.extend(crate::result::si_findings_json(&si));
    actual_findings.extend(usbc.as_ref().and_then(crate::result::usbc_finding_json));
    let qualification = qualification.as_ref();
    let evidence = match web_board_evidence(
        file_name,
        norm,
        bound,
        corner_plan.as_ref(),
        schematic_ties,
        qualification,
        supporting_artifacts,
    )
    .and_then(|evidence| {
        with_web_evidence_maps(evidence, &drc_structured, qualification, &actual_findings)
    }) {
        Ok(evidence) => evidence,
        Err(error) => return (unreadable(file_name, error), drc),
    };
    let (evidence, display_only_coverage) = evidence;

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
    if let Some(message) = indicators_not_judged {
        notes.push(JsonNote {
            kind: JsonNoteKind::Coverage,
            message,
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
    // On an unrouted board the frame leads; what the run could not judge is
    // told by the bind banner and the evidence, not by a different headline.
    let pre_layout = fix_first.notice.is_some();
    if pre_layout {
        headline = crate::plain::pre_layout_headline(total, serious, fix_first.copper_held);
    }
    // Same run-level split as the JSON verdict: a finding-backed undermined
    // map is a badge on that finding, not run invalidity, so the headline only
    // escalates for undermined run-level claims (coverage, bind completeness).
    let run_level_undermined = crate::result::run_level_undermined(evidence.maps(), |a| {
        actual_findings.iter().any(|f| f.message == a)
    });
    // Fail beats invalid, but a non-serious warning must not: an undermined
    // run-level claim overrides the headline whenever nothing serious does.
    // A pass that rests on conditions is headlined by the verdict, which names
    // them (see `verdict_headline`); assumed corners are not "qualified" news.
    if !pre_layout {
        if serious == 0 && run_level_undermined {
            headline = UNDERMINED_HEADLINE.to_string();
        } else if total == 0 && evidence.conditions().is_empty() && evidence.has_caveats() {
            headline = "No blocking findings, but some evidence is qualified; see the evidence limitations below.".to_string();
        }
    }
    // The `--check` machine document for this board: the web verdict is ITS
    // verdict, so the browser and `run --check --json` cannot disagree.
    let [rails_entered, rails_held] = crate::checks::rail_basis::rail_conditions(board, lib, bound);
    let mut machine = crate::reports::check::machine_report(
        &board.name,
        &bind_summary,
        &drc_structured,
        clearance_rule_source.as_ref(),
        &actual_findings,
        crate::reports::check::check_gate_fails(&drc_structured, &lint, &si, usbc.as_ref()),
    )
    .with_evidence(&evidence)
    .with_condition(crate::checks::indicators::intent_condition(&indicators))
    .with_condition(
        clearance_rule_source
            .as_ref()
            .and_then(|p| crate::reports::drc::default_rules_condition(&drc_structured, p)),
    )
    .with_condition(crate::reports::drc::unmeasured_spacing_condition(norm.kind))
    .with_unorderable_input(crate::reports::drc::unorderable_input(
        norm.kind,
        norm.layout_text.as_deref(),
    ))
    .with_condition(rails_entered)
    .with_condition(rails_held)
    .with_judgement_scope(crate::scope::JudgementScope::of(
        board,
        bound,
        &verdict_blockers,
    ));
    if display_only_coverage {
        machine.evidence.clear();
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

    let mut report = WebReport {
        ok: true,
        board_name: display_board_name(&board.name, file_name),
        file_name: file_name.to_string(),
        num_components: board.components.len(),
        num_nets: bound.net_names.len(),
        clearance_rule_source,
        triage,
        fix_first,
        headline,
        run_level_undermined,
        serious,
        total,
        sections,
        components,
        import_diagnostics: Some(web_import_diagnostics(norm)),
        bind: Some(bind_web),
        model_coverage: Some(crate::commands::models::coverage_snapshot(board, lib)),
        inventory: evidence.inventory().to_vec(),
        assumptions: evidence.assumptions().to_vec(),
        // Every term: the page also explains verdict words (INVALID, FAIL) and chips.
        glossary: hauksbee_ir::glossary::all(),
        evidence: evidence.maps().to_vec(),
        notes,
        nets,
        component_kinds: bound
            .component_kinds
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        component_assertions: component_assertion_capabilities(board, bound),
        component_ratings: component_ratings(board, bound),
        supplies,
        machine: Some(machine),
        ..WebReport::default()
    };
    report.explain_verdict();
    (report, drc)
}

/// The run's evidence: bind provenance plus the input, schematic and supporting artifacts.
fn web_board_evidence(
    file_name: &str,
    norm: &crate::board_input::NormalizedBoard,
    bound: &BoundBoard,
    corner_plan: Option<&crate::variant_sweep::CornerPlan>,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
    qualification: Option<&hauksbee_extract::DrcTieQualification>,
    supporting_artifacts: &[hauksbee_ir::evidence::ArtifactProvenance],
) -> Result<crate::evidence::BoardEvidence, String> {
    // Reader notes plus the binder's own coverage gaps, so an overpower check
    // that never ran is visible on the human report, not only in CI.
    let mut notes = norm.notes.clone();
    notes.extend(bound.power_coverage_gaps());
    // The lint swept the corner sets: say so, not that one corner stood in.
    let mut report = std::borrow::Cow::Borrowed(&bound.report);
    if let Some(plan) = corner_plan {
        plan.mark(report.to_mut());
    }
    let evidence = crate::evidence::BoardEvidence::from_bound(
        &norm.board,
        &report,
        &notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )
    .and_then(|evidence| evidence.with_input_artifact(file_name, &norm.raw, norm.kind))
    .map_err(|error| format!("could not build evidence map: {error}"))?;
    let evidence = match (schematic_ties, qualification) {
        (Some(ties), Some(qualification)) => evidence
            .with_schematic_artifact(&ties.path, &ties.raw, ties.contribution(qualification))
            .map_err(|error| format!("could not record schematic evidence: {error}"))?,
        _ => evidence,
    };
    supporting_artifacts
        .iter()
        .try_fold(evidence, |evidence, artifact| {
            evidence.with_supporting_artifact(artifact.clone())
        })
        .map_err(|error| format!("could not record supplemental evidence: {error}"))
}

/// Attach the web report's evidence maps: DRC, findings, non-clean coverage, else static coverage.
fn with_web_evidence_maps(
    evidence: crate::evidence::BoardEvidence,
    drc_structured: &crate::result::DrcStructured,
    qualification: Option<&hauksbee_extract::DrcTieQualification>,
    actual_findings: &[crate::result::JsonFinding],
) -> Result<(crate::evidence::BoardEvidence, bool), String> {
    let coverage_error = |error| format!("could not build coverage evidence: {error}");
    let mut actual_maps = evidence
        .maps_for_drc_with_ties(drc_structured, qualification)
        .map_err(|error| format!("could not build DRC evidence: {error}"))?;
    actual_maps.extend(
        evidence
            .maps_for_findings(actual_findings)
            .map_err(|error| format!("could not build finding evidence: {error}"))?,
    );
    for (check, assertion) in [
        ("drc", "DRC input coverage"),
        ("si", "Signal-integrity input coverage"),
    ] {
        let map = evidence
            .check_coverage_map(check, assertion)
            .map_err(coverage_error)?;
        if map.status() != hauksbee_ir::evidence::EvidenceStatus::Clean {
            actual_maps.push(map);
        }
    }
    // The static coverage map is there so the evidence list is never empty; the
    // `--check` verdict has no such map, so it must not count toward the web's.
    let display_only = actual_maps.is_empty();
    if display_only {
        actual_maps.push(evidence.static_coverage_map().map_err(coverage_error)?);
    }
    Ok((evidence.with_maps(actual_maps), display_only))
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
#[must_use]
pub fn analyze_with_firmware(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> WebReport {
    analyze_with_firmware_detailed(file_name, contents, fw_name, fw_bytes).report
}

#[must_use]
pub fn analyze_with_firmware_detailed(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> WebFirmwareAnalysis {
    analyze_with_firmware_detailed_with_ties(file_name, contents, fw_name, fw_bytes, None)
}

#[must_use]
pub fn analyze_with_firmware_detailed_with_ties(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> WebFirmwareAnalysis {
    // Normalize ONCE; the static analysis and the co-sim share the same
    // extracted board. Re-reading the ORIGINAL bytes for co-sim with only the
    // text/binary sniffers fails with "could not re-read the board" on a
    // gerber zip that has just produced a clean static report.
    let norm = match crate::board_input::from_bytes(file_name, contents) {
        Ok(n) => n,
        // No board to co-sim against; return the normalization error as-is.
        Err(e) => {
            return WebFirmwareAnalysis {
                report: unreadable_input(file_name, contents, &e),
                coverage: WebCosimCoverage::default(),
            }
        }
    };
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    let static_bound = bind_board(&norm.board, &lib);
    let cosim_bound = bind_board(&norm.board, &lib);
    let evidence_bound = bind_board(&norm.board, &lib);
    let (report, coverage) = analyze_with_firmware_context(
        file_name,
        &norm,
        fw_name,
        fw_bytes,
        schematic_ties,
        &lib,
        &static_bound,
        cosim_bound,
        &evidence_bound,
        &[],
    );
    WebFirmwareAnalysis { report, coverage }
}

#[allow(clippy::too_many_lines, reason = "linear: static report, co-sim, fold")]
fn analyze_with_firmware_context(
    file_name: &str,
    norm: &crate::board_input::NormalizedBoard,
    fw_name: &str,
    fw_bytes: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
    lib: &ModelLibrary,
    static_bound: &crate::binder::BoundBoard,
    cosim_bound: crate::binder::BoundBoard,
    evidence_bound: &crate::binder::BoundBoard,
    supporting_artifacts: &[hauksbee_ir::evidence::ArtifactProvenance],
) -> (WebReport, WebCosimCoverage) {
    let (mut report, drc) = analyze_normalized_with_context(
        file_name,
        norm,
        schematic_ties,
        lib,
        Some(static_bound),
        supporting_artifacts,
    );
    // The static pass already set the headline's verdict class; the co-sim
    // below works on what the sections said, and the verdict is derived again
    // at the end over both.
    report.headline = std::mem::take(&mut report.pre_verdict_headline);
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
            report.explain_verdict();
            return (report, coverage);
        }
    };
    let external_backend = cosim_bound
        .mcus
        .iter()
        .any(|m| is_external_backend(&m.backend));
    let (mut cosim, cosim_evidence) = run_web_cosim_with_bound(
        &norm.board,
        cosim_bound,
        file_name,
        &resolved.name,
        &resolved.bytes,
        &drc,
        tie_qualification.as_ref(),
        &WebCosimWindow::QUICK,
    );
    let mut coverage = cosim_evidence
        .as_ref()
        .map(|captured| captured.coverage.clone())
        .unwrap_or_default();
    // The quick report never runs an emulator; say so as data, so the page can
    // offer to run it as a job instead of parsing the explanation.
    coverage.external_backend = external_backend && cosim_evidence.is_none();
    // Whether the evidence pass below parked the headline on run-level
    // invalidity; the fault fold must not demote that to a warning headline
    // (serious beats invalid beats warning).
    let mut evidence_invalid = false;
    if let Some(captured) = cosim_evidence {
        let mut notes = norm.notes.clone();
        notes.extend(evidence_bound.power_coverage_gaps());
        let evidence_result = crate::evidence::BoardEvidence::from_bound(
            &norm.board,
            &evidence_bound.report,
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
        })
        .and_then(|evidence| {
            supporting_artifacts
                .iter()
                .try_fold(evidence, |evidence, artifact| {
                    evidence.with_supporting_artifact(artifact.clone())
                })
        });
        if let Ok(mut evidence) = evidence_result {
            evidence = evidence
                .clone()
                .with_scoped_substitutions(&captured.scoped_substitutions)
                .unwrap_or(evidence);
            if let Some(budget) = captured.error_budget.clone() {
                let mut maps = report.evidence.clone();
                for fault in &captured.faults {
                    // Far past its rating the figure is no result: the one
                    // stress wording states it without the number.
                    let claim = if fault.is_far_past_rating() {
                        format!("Firmware co-sim stress: {}", fault.statement())
                    } else {
                        format!(
                            "Firmware co-sim stress: {} {} = {:.6} (limit {:.6})",
                            fault.component,
                            fault.kind.as_str(),
                            fault.value,
                            fault.limit
                        )
                    };
                    if let Ok(map) = evidence.simulation_map(
                        claim,
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
                // Same run-level split as everywhere else. Finding-backed
                // maps wear their badge on the finding and must not invalidate
                // the headline; the static run-level bit already travels on the
                // report. Everything up to `fault_maps_end` is the static
                // prefix plus the per-fault (finding-backed) maps, so only what
                // follows it is a run-level simulation claim to grade here.
                let sim_maps = evidence.maps().get(fault_maps_end..).unwrap_or_default();
                let sim_undermined = sim_maps
                    .iter()
                    .any(hauksbee_ir::evidence::EvidenceMap::is_undermined);
                // The verdict grades the same run-level simulation claims.
                if let Some(machine) = report.machine.as_mut() {
                    machine.evidence.extend(sim_maps.iter().cloned());
                }
                if report.serious == 0 && (report.run_level_undermined || sim_undermined) {
                    report.headline = FIRMWARE_UNDERMINED_HEADLINE.to_string();
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
            WebFinding::unplaced(
                "note",
                format!("Firmware resolved from '{fw_name}'."),
                note.clone(),
                "Upload the exact .elf/.hex directly if this is not the image you meant.",
            ),
        );
    }
    // Fold co-sim electrical FAULTS into the top-level verdict: analyze()
    // counted the STATIC sections only, so a fault the firmware co-sim produced
    // would otherwise sit in the co-sim card under a green badge. A destructive
    // fault (e.g. an overcurrent-killed MOSFET) is SERIOUS; a non-destructive
    // over-stress is a WARNING, still an actionable issue the CLI counts and
    // --strict exits 2 on, so it escalates the web headline too. Only
    // note-level honesty caveats stay out of the count (they demote via
    // cosim_caveat_headline below).
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
            report.headline = if report.fix_first.notice.is_some() {
                crate::plain::pre_layout_headline(total, serious, report.fix_first.copper_held)
            } else {
                headline
            };
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
    report.explain_verdict();
    (report, coverage)
}

/// A refusal says what this run could not judge; it is never a board fault.
/// It takes the headline only when nothing else decides the verdict: a serious
/// finding keeps its own headline, and an unrouted board keeps its pre-layout
/// frame. Overwriting that frame used to make the verdict sentence fall back
/// to "a finding fails the check gate", so attaching firmware that the quick
/// report cannot run read as a new board fault.
fn apply_refusal_headline(report: &mut WebReport) {
    if report.refusal.is_some() && report.serious == 0 && report.fix_first.notice.is_none() {
        report.headline = REFUSAL_HEADLINE.to_string();
    }
}

/// The co-sim note for a run with no valid numerical qualification.
const UNQUALIFIED_COSIM: &str = "Co-sim result has no valid numerical qualification.";

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
            .map_or_else(
                || {
                    "fix the first failed analog window, then rerun the same firmware co-simulation"
                        .to_string()
                },
                |finding| finding.fix.clone(),
            );
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

    if let Some(finding) = cosim.findings.iter().find(|f| f.what == UNQUALIFIED_COSIM) {
        return Some(Refusal::new(
            "numerically qualified firmware co-simulation conclusions",
            finding.why.clone(),
            vec![
                "Static board analysis and its reported findings remain valid.",
                "Firmware loading and reported digital observations remain available.",
            ],
            finding.fix.clone(),
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
    (caveat && current_headline == overall_headline(0, 0, false, false))
        .then(|| overall_headline(0, 0, true, false))
}

/// A skipped/failed co-sim section carrying the reason as a single note-level
/// finding, so the browser renders it with the same card markup.
fn cosim_unavailable(reason: impl Into<String>) -> WebCosimSection {
    WebCosimSection {
        ran: false,
        findings: vec![WebFinding::unplaced(
            "note",
            "Co-sim not available for this board.",
            reason,
            "Drop a board with a supported in-process MCU and a matching firmware build to run the firmware co-sim.",
        )],
        // A co-sim that never ran cannot have failed an analog chunk: report the
        // clean, backward-compatible shape (valid, no windows).
        analog_valid: true,
        // ran == false: the headline demotion is gated on `ran`, so these remain
        // neutral defaults inside the co-sim section. The containing WebReport's
        // refusal field, not these compatibility booleans, carries the refusal.
        firmware_exercised: true,
        substituted: false,
        ..WebCosimSection::default()
    }
}

/// The loud, plain-language analog-validity finding, prepended to a co-sim
/// section when the run held stale voltages over one or more failed windows
/// rather than faking a result over them. `failed_chunks` is the count and
/// `windows` the merged spans.
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
    // conductance is outside the board's own distribution. Deduplicated,
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
    WebFinding::unplaced(
        "note",
        "Analog co-sim did not converge: electrical results are not trustworthy",
        format!(
            "The analog solver failed on {failed_chunks} {chunk_word} covering {spans}, \
             and no fallback integration rung (reduced step, backward Euler, cold-start \
             continuation, subdivision) could carry them. Over those windows the co-sim \
             held stale node voltages instead of a real solve, so any voltage, current or \
             fault reading there is fiction (see {}: refuse rather than fake).{diagnosis}",
            hauksbee_ir::docs_url("docs/about/LIMITATIONS.md"),
        ),
        "Treat electrical results inside those windows as unknown. A stiff or \
         structurally singular section (conflicting rails, an unconverging \
         nonlinear stage) usually causes it; simplify the offending section or \
         relax the operating point, then re-run.",
    )
}

/// The actual firmware co-sim for the web path. Mirrors `run_headless` in the
/// CLI (same fixed 1 kHz frame cadence, same fault dedup via `plain_faults`) but
/// runs synchronously for a short fixed window so it fits a request budget.
///
/// Takes the ALREADY-extracted board from the caller's normalization pass
/// (never re-reads the upload): every format the static analysis accepts,
/// gerber zips included, reaches co-sim identically.
/// The exact terminal command that runs this co-sim outside the web report,
/// with the REAL uploaded file names, never placeholders (a `<board>`
/// placeholder is an instruction the user cannot follow). An installed app
/// names its binary by full running path: an app user has no `hauksbee` on
/// PATH (it lives in Hauksbee.app/Contents/Resources/bin), and the serving
/// executable IS that binary, so its own path is the one command guaranteed to
/// exist on this machine. A development build runs from a cargo `target/`
/// directory whose absolute path means nothing to a reader, so it is shown as
/// plain `hauksbee` and the report says which version that is.
fn cli_cosim_command(board_file: &str, fw_file: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map_or_else(|| "hauksbee".to_string(), |p| display_executable(&p));
    format!("\"{exe}\" run \"{board_file}\" --firmware \"{fw_file}\" --headless")
}

/// How a running binary's path reads in report text: the path itself for an
/// installed binary, `hauksbee` for a cargo build artifact (anything up to
/// three levels under a `target` directory: `target/<profile>/hauksbee`,
/// `target/<triple>/<profile>/hauksbee`).
fn display_executable(path: &std::path::Path) -> String {
    let parts: Vec<_> = path.components().collect();
    let dev_build = parts
        .iter()
        .rposition(|c| c.as_os_str() == "target")
        .is_some_and(|i| parts.len() - i <= 4);
    if dev_build {
        "hauksbee".to_string()
    } else {
        path.display().to_string()
    }
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
                    | CoverageClass::ExecutionBlocked
            )
        })
        .map(|c| WebFinding::unplaced("note", c.headline.clone(), c.message.clone(), c.fix.clone()))
        .collect()
}

#[cfg(test)]
fn run_web_cosim(
    board: &ExtractedBoard,
    board_file_name: &str,
    fw_name: &str,
    fw_bytes: &[u8],
    drc: &hauksbee_extract::DrcReport,
    tie_qualification: Option<&hauksbee_extract::DrcTieQualification>,
) -> (WebCosimSection, Option<WebCosimEvidence>) {
    let lib = ModelLibrary::builtin();
    let bound = bind_board(board, &lib);
    run_web_cosim_with_bound(
        board,
        bound,
        board_file_name,
        fw_name,
        fw_bytes,
        drc,
        tie_qualification,
        &WebCosimWindow::QUICK,
    )
}

/// Whether a bound MCU's backend is an external emulator.
fn is_external_backend(backend: &str) -> bool {
    backend.starts_with("renode:") || backend.starts_with("qemu:")
}

/// How long, and how finely, a web co-sim runs.
struct WebCosimWindow<'a> {
    /// Simulated seconds to run.
    seconds: f64,
    /// The stepping frame, in seconds.
    frame_dt: f64,
    /// Whether a board whose MCU runs in Renode or QEMU is run, rather than
    /// refused with the reason the quick report cannot host an emulator.
    external_ok: bool,
    /// Called after each frame with the simulated seconds so far.
    on_frame: Option<&'a (dyn Fn(f64) + Sync)>,
}

impl WebCosimWindow<'static> {
    /// The synchronous report's window: 200 one-millisecond frames, in-process
    /// cores only, so it fits a request budget.
    const QUICK: Self = Self {
        seconds: 0.2,
        frame_dt: 0.001,
        external_ok: false,
        on_frame: None,
    };
}

#[allow(clippy::too_many_lines, reason = "linear: stage, step, honesty notes")]
#[allow(
    clippy::too_many_arguments,
    reason = "the stages of one run, plus its window"
)]
fn run_web_cosim_with_bound(
    board: &ExtractedBoard,
    bound: crate::binder::BoundBoard,
    board_file_name: &str,
    fw_name: &str,
    fw_bytes: &[u8],
    drc: &hauksbee_extract::DrcReport,
    tie_qualification: Option<&hauksbee_extract::DrcTieQualification>,
    window: &WebCosimWindow<'_>,
) -> (WebCosimSection, Option<WebCosimEvidence>) {
    use crate::plain::plain_faults;
    use crate::stress::FaultEvent;
    use hauksbee_frontdoor_api::engine::Engine;
    use std::io::Write;

    // The window's length. The external-emulator refusal below quotes it: the
    // honest reason that path is skipped is that 0.2 s of simulated time is
    // less than such a chip's boot ROM needs, so the user would be shown a
    // processor that has not started.
    let seconds = window.seconds;
    // The window in whole frames. An integer count, so the step count does not
    // hinge on float accumulation.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a frame count: positive, and far below u32::MAX"
    )]
    let frames = (window.seconds / window.frame_dt).round() as u32;

    // No MCU => firmware drives nothing. Inspect the bound board before paying
    // for a temp file / engine build, and say so plainly.
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
        .filter(|m| is_external_backend(&m.backend))
        .collect();
    let has_external = !external_mcus.is_empty();
    if has_external && !window.external_ok {
        let selected = external_mcus
            .iter()
            .map(|m| {
                format!(
                    "{}: {}",
                    m.reference,
                    hauksbee_ir::labels::label_for(&m.backend)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut section = cosim_unavailable(format!(
            "This board selects at least one MCU that requires an external emulator: \
             {selected}. The synchronous web run cannot execute only a subset of the \
             selected cores and present it as whole-board firmware evidence. This quick \
             report simulates only a {seconds:.1} s window, and a chip like this one \
             spends several simulated seconds in its boot ROM before your code starts, \
             so the report would show a processor that has not booted yet. Running it \
             long enough is a job of tens of wall-clock seconds, which is why it is not \
             done inside a page load. The complete co-sim works, two ways: open the live \
             sim (\"Drive it live\") to boot this firmware here in the app, or run it \
             from a terminal (hauksbee {}), from the folder holding your files: {}",
            env!("CARGO_PKG_VERSION"),
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
        .map(str::to_ascii_lowercase)
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
    if let Err(e) = tmp.write_all(fw_bytes).and_then(|()| tmp.flush()) {
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

    // The quick window mirrors run_headless: 1 kHz frames. An emulator job runs
    // the CLI's coarse frame, with the scheduler's chunk matched to it.
    let frame_dt = window.frame_dt;
    if has_external {
        engine.scheduler_mut().chunk_s = frame_dt;
    }
    let mut last_uart: Vec<u8> = Vec::new();
    let mut faults: Vec<FaultEvent> = Vec::new();
    hauksbee_ir::progress::will_simulate(f64::from(frames) * frame_dt);
    hauksbee_ir::progress::begin(hauksbee_ir::progress::Stage::Simulate, u64::from(frames));
    for i in 0..frames {
        hauksbee_ir::progress::advance_in(hauksbee_ir::progress::Stage::Simulate, 1);
        let frame = engine.step(frame_dt);
        if let Some(on_frame) = window.on_frame {
            on_frame(f64::from(i + 1) * frame_dt);
        }
        // Concatenate per-MCU UART in a STABLE (sorted-by-MCU-key) order, plain
        // `values()` is HashMap iteration order, so a multi-MCU board's merged
        // uart_output would interleave nondeterministically run-to-run. Mirrors
        // the CI runner's sorted-by-key concatenation.
        let mut uart_entries: Vec<_> = frame.uart.iter().collect();
        uart_entries.sort_by(|a, b| a.0.cmp(b.0));
        for (_, bytes) in uart_entries {
            last_uart.extend_from_slice(bytes);
        }
        faults.extend(
            frame
                .faults
                .into_iter()
                .map(crate::reports::cosim::fault_event),
        );
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

    // Execution evidence, read once after the last frame like `run_headless`.
    engine.scheduler_mut().refresh_execution_evidence();
    // Per-net activity table (top movers first), mirroring the CLI toggle table.
    let sched = engine.scheduler();
    let seconds_simulated = sched.sim_time;
    // Co-sim honesty parity with the CLI/TUI surfaces. Without these two notes the
    // web path is the ONE surface that gives false comfort: an empty `findings`
    // list renders as "No electrical-stress faults during the run." even when the
    // firmware never executed or ran on a substitute chip. Prepended so they lead.
    let total_toggles: u64 = sched.stats.values().map(|s| s.toggles).sum();
    let uart_empty = last_uart.is_empty();
    // The one "did the firmware do nothing?" predicate the CLI also asks: a
    // pin driven high and HELD still counts, and with execution evidence net
    // toggles alone (analog-side) do not.
    let firmware_quiet = sched.firmware_quiet(total_toggles, !uart_empty);
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
            WebFinding::unplaced(
                "note",
                format!(
                    "This co-sim ran WITH the board's {shorts_applied} physical copper {connection_word} \
                     bridged into the circuit."
                ),
                "The DRC section above determines whether a contact has board-local physical \
                 authorization. The co-sim applies every validated-format copper contact \
                 before simulating, so the rail and GPIO voltages reflect the board as built; \
                 schematic net names alone do not suppress short faults.",
                "Follow the DRC section: repair serious contacts, or provide board-local \
                 authority for the exact intended join.",
            ),
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
            WebFinding::unplaced(
                "note",
                "A requested drive was overridden on its net",
                msg,
                "Remove the losing source, or suppress the rail that pins the net, so                       the drive you asked for is the one that takes effect.",
            ),
        );
    }
    // A temperature-dependent rating decided by the ambient nobody stated, or
    // read past its hottest row on a package-guessed Tj,max.
    for a in sched.stress.rating_disclosures() {
        findings.insert(
            0,
            WebFinding::unplaced("note", a.statement(), a.because(), a.replacement()),
        );
    }
    // Chip-substitution: the firmware was emulated on a less-specific core.
    for sub in sched.substitutions() {
        findings.insert(
            0,
            WebFinding::unplaced(
                "note",
                "MCU modelled on a substitute core",
                sub.message(),
                "Peripherals/clock/flash may differ on the real part; treat \
                 the co-sim as approximate for this MCU.",
            ),
        );
    }
    // Runtime driver contention (the model-vs-MCU case the static lint
    // documents as out of its reach): a real electrical fight, so it is a
    // SERIOUS finding that folds into the fault counts, not a caveat.
    for c in sched.driver_contentions() {
        findings.insert(
            0,
            WebFinding::unplaced(
                "serious",
                format!("Driver contention on net '{}'", c.net),
                c.message(),
                "Check the model pin mapping (`hauksbee models resolve`) and the \
                 firmware's pin-direction writes; two push-pull drivers must never \
                 share a net without a series element.",
            ),
        );
    }
    // Sub-chunk pulses swallowed by tick-evaluated sequential parts (friction
    // 1.16): a co-sim FIDELITY caveat (the board may be fine; the result is
    // not trustworthy), so note-level, which demotes the headline the same way
    // the other honesty caveats do.
    for p in sched.short_pulses() {
        findings.insert(
            0,
            WebFinding::unplaced(
                "note",
                format!(
                    "A GPIO pulse on net '{}' is too short for the co-sim to observe",
                    p.net
                ),
                p.message(),
                "Rerun from the command line with --chunk-us at or below half the \
                 pulse width, or widen the pulse in firmware.",
            ),
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
    if firmware_quiet {
        findings.insert(
            0,
            WebFinding::unplaced(
                "note",
                "Firmware was not exercised",
                format!(
                    "{} (the MCU may have stalled at boot, run no I/O, or the firmware \
                     may not match this board).",
                    crate::execution::NO_EXECUTION_MESSAGE
                ),
                "Confirm the firmware matches this MCU and actually drives I/O.",
            ),
        );
    }
    // Analog-validity refusal: if any chunk's analog solve failed, the
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
    // One note per regulator with no established input headroom, in the shared
    // wording. Note-level like its sibling honesty caveats, which is enough for
    // `cosim_caveat_headline` to demote "Looks healthy".
    for taint in sched.regulation_taint() {
        let (what, fix) = if taint.reason.contains(crate::scheduler::OPEN_DRIVER) {
            (
                " has no model, so the nets it drives are",
                "Give it a model, then re-run.",
            )
        } else {
            let fix = "Declare or fix the supply on this regulator's input, then re-run.";
            ("'s regulation is", fix)
        };
        findings.insert(
            0,
            WebFinding::unplaced(
                "note",
                format!(
                    "{}{what} not a simulated result",
                    taint.refs.first().map_or("A regulator", String::as_str)
                ),
                taint.reason.clone(),
                fix,
            ),
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
    let firmware_ran = !firmware_quiet;
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
            WebFinding::unplaced(
                "note",
                format!("Control net '{net}' may be energised at power-up"),
                format!(
                    "'{net}' drives a transistor/relay, is driven HIGH and held from power-up, \
                     and has no resistor setting a safe default. If a HIGH turns the switched \
                     load ON when it must stay OFF until firmware enables it, it is energised at \
                     power-up."
                ),
                "Confirm the polarity is intended, or add a pull resistor that holds the \
                 gate at its safe (OFF) level until the firmware drives it.",
            ),
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
    let fallback_windows = sched.fallback_windows().iter().map(Into::into).collect();

    let error_budget = match sched.error_budget() {
        Ok(budget) => Some(budget),
        Err(error) => {
            // Note-level, then a refusal (see `refusal_for_cosim`): a run the
            // tool cannot qualify numerically is not judged. Marking it
            // serious failed a board over the solver's own settings.
            findings.insert(
                0,
                WebFinding::unplaced(
                    "note",
                    UNQUALIFIED_COSIM,
                    error.to_string(),
                    "Use finite, positive solver tolerances and run the co-simulation again.",
                ),
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
            execution: sched.execution_evidence().to_vec(),
            bench: Some(sched.bench_report()),
            external_backend: false,
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

/// Simulated seconds an emulator co-sim job runs. The synchronous report's
/// 0.2 s is shorter than such a chip's boot ROM; two seconds lets the core boot
/// and run several periods of an ordinary blink or heartbeat loop.
const COSIM_JOB_SECONDS: f64 = 2.0;
/// The emulator job's stepping frame, the CLI's coarse external-emulator frame.
const COSIM_JOB_FRAME_S: f64 = 0.01;

/// Run the firmware co-sim for a design upload on whatever backend its MCU needs.
///
/// That includes Renode and QEMU, which the synchronous analysis refuses.
/// Answers the finished `cosim` section as JSON (the same shape the report's
/// `cosim` carries, with the bench view), reporting progress as it goes.
///
/// # Errors
///
/// Returns a user-facing sentence when the upload has no firmware, cannot be
/// read or bound, or the firmware cannot be loaded.
pub fn run_cosim_job(
    upload: hauksbee_frontdoor_api::frontdoor::DesignUpload,
    progress: hauksbee_frontdoor_api::frontdoor::CosimProgress,
) -> Result<String, String> {
    let Some(firmware) = upload.firmware.clone() else {
        return Err("the co-sim needs a firmware image alongside the board".to_string());
    };
    let name = upload.board.name.clone();
    let prepared = crate::web_design::prepare_typed(upload)
        .map_err(crate::web_design::PrepareFailure::into_message)?;
    let resolved = crate::firmware_input::resolve_firmware_bytes(&firmware.name, &firmware.bytes)?;
    let static_bound = prepared.bind()?;
    let cosim_bound = prepared.bind()?;
    // The static pass supplies the DRC shorts the co-sim bridges in, as the
    // synchronous report does.
    let (_, drc) = analyze_normalized_with_context(
        &name,
        &prepared.norm,
        prepared.ties.as_ref(),
        &prepared.lib,
        Some(&static_bound),
        &prepared.supporting_artifacts,
    );
    let tie_qualification = prepared.ties.as_ref().map(|ties| ties.qualify(&drc));
    let on_frame = move |done: f64| progress(done, COSIM_JOB_SECONDS);
    let window = WebCosimWindow {
        seconds: COSIM_JOB_SECONDS,
        frame_dt: COSIM_JOB_FRAME_S,
        external_ok: true,
        on_frame: Some(&on_frame),
    };
    on_frame(0.0);
    let (section, evidence) = run_web_cosim_with_bound(
        &prepared.norm.board,
        cosim_bound,
        &name,
        &resolved.name,
        &resolved.bytes,
        &drc,
        tie_qualification.as_ref(),
        &window,
    );
    let coverage = evidence.map(|e| e.coverage).unwrap_or_default();
    let mut value = serde_json::to_value(&section).map_err(|e| e.to_string())?;
    if let Some(cosim) = value.as_object_mut() {
        insert_coverage_fields(cosim, &coverage).map_err(|e| e.to_string())?;
    }
    serde_json::to_string(&value).map_err(|e| e.to_string())
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
        actions: Vec::new(),
    };
    // The verdict must be a full sentence, matching every other section (which is
    // built via `WebSection::from_plain`, whose verdict is `PlainReport::verdict()`
    // prose). Bare tokens like "problem"/"note" made this one folded-in section
    // carry an incompatible string format in the same `/api/analyze` payload and
    // rendered a lone word under the USB-C card while siblings showed sentences.
    if usbc.is_serious() {
        section.verdict = "1 issue found, 1 serious.".to_string();
        section
            .findings
            .push(WebFinding::unplaced("serious", what, why, fix));
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
            return BIND_OPEN_HEADLINE.to_string();
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
#[must_use]
pub fn analyze_json(file_name: &str, contents: &[u8]) -> String {
    analyze_json_with_ties(file_name, contents, None)
}

/// [`analyze_json`] for a board read from a PATH.
///
/// A companion Eagle `.sch` may sit beside it. Used by `run --serve`, whose preloaded
/// report must agree with what `--drc` and `--check` say about the same board on disk.
#[must_use]
pub fn analyze_json_with_ties(
    file_name: &str,
    contents: &[u8],
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> String {
    let report = match crate::board_input::from_bytes(file_name, contents) {
        Ok(norm) => analyze_normalized_with_ties(file_name, &norm, schematic_ties).0,
        Err(e) => return unreadable_input_json(file_name, contents, &e),
    };
    to_json_or_failure(&report)
}

/// Serialize an [`analyze_with_firmware`] result to a JSON string for the HTTP layer.
///
/// This backs the `/api/analyze-with-firmware` endpoint. Board AND firmware bytes
/// are passed as `&[u8]` end-to-end, never lossy-decoded, so an uploaded
/// binary board or ELF stays intact.
#[must_use]
pub fn analyze_with_firmware_json(
    file_name: &str,
    contents: &[u8],
    fw_name: &str,
    fw_bytes: &[u8],
) -> String {
    analyze_with_firmware_json_with_ties(file_name, contents, fw_name, fw_bytes, None)
}

#[must_use]
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
    analysis.to_json_value().map_or_else(
        |e| serialize_failure(&e),
        |value| to_json_or_failure(&value),
    )
}

/// Analyze the complete browser design bundle.
///
/// It uses the same manufacturing and model context as Live Sim. Supplemental
/// parse/apply errors are returned as the ordinary web error document, never silently
/// ignored.
#[must_use]
pub fn analyze_design_json(upload: hauksbee_frontdoor_api::frontdoor::DesignUpload) -> String {
    let error_json = |name: &str, message: String| to_json_or_failure(&unreadable(name, message));
    let name = upload.board.name.clone();
    if upload.firmware.is_some() {
        // The firmware co-sim covers 200 one-millisecond frames (`FRAMES`).
        hauksbee_ir::progress::will_simulate(0.2);
    }
    // A refused board keeps its typed error, so this route answers with the same
    // structured `import_failure` as the single-file route instead of prose alone.
    let prepared = match crate::web_design::prepare_typed(upload) {
        Ok(prepared) => prepared,
        Err(crate::web_design::PrepareFailure::Board(refusal, bytes)) => {
            return unreadable_input_json(&name, &bytes, &refusal);
        }
        Err(crate::web_design::PrepareFailure::Other(error)) => return error_json(&name, error),
    };
    // Firmware makes two more binds (the co-sim's and the evidence pass's).
    hauksbee_ir::progress::begin_bind(
        prepared.norm.board.components.len(),
        if prepared.upload.firmware.is_some() {
            3
        } else {
            1
        },
    );
    let static_bound = match prepared.bind() {
        Ok(bound) => bound,
        Err(error) => return error_json(&name, error),
    };
    let report = match prepared.upload.firmware.as_ref() {
        Some(firmware) => {
            let cosim_bound = match prepared.bind() {
                Ok(bound) => bound,
                Err(error) => return error_json(&name, error),
            };
            let evidence_bound = match prepared.bind() {
                Ok(bound) => bound,
                Err(error) => return error_json(&name, error),
            };
            analyze_with_firmware_context(
                &name,
                &prepared.norm,
                &firmware.name,
                &firmware.bytes,
                prepared.ties.as_ref(),
                &prepared.lib,
                &static_bound,
                cosim_bound,
                &evidence_bound,
                &prepared.supporting_artifacts,
            )
            .0
        }
        None => {
            analyze_normalized_with_context(
                &name,
                &prepared.norm,
                prepared.ties.as_ref(),
                &prepared.lib,
                Some(&static_bound),
                &prepared.supporting_artifacts,
            )
            .0
        }
    };
    serde_json::to_string(&report)
        .unwrap_or_else(|error| error_json(&name, format!("failed to serialize report: {error}")))
}

/// The web co-sim GPIO activity table: the `limit` most-active nets, ranked by
/// TOGGLE COUNT descending, then VOLTAGE RANGE descending, then name; the exact
/// three-key "top movers first" contract the CLI toggle table and JSON
/// `activity_summary` use (see `reports::cosim`). Ranking by the driven flag then
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
            toggles: st.toggles,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_index_answers_exactly_what_the_scan_did() {
        let names = [
            "R1", "R1_q1", "R1_s", "R10", "U3_e2", "U3", "U3_x", "Q_q", "_q", "_s_q", "A_b_q9", "",
            "R1_Q1",
        ];
        let index = unit_index(names.iter().map(|n| (*n, ())));
        let references = [
            "R1", "R10", "U3", "Q", "", "_s", "A", "A_b", "R", "R1_", "R1_Q1", "U3_x",
        ];
        for reference in references {
            let scanned = names.iter().any(|n| ref_or_unit_matches(reference, n));
            assert_eq!(
                index.contains_key(reference),
                scanned,
                "reference {reference:?}"
            );
            let scanned_count = names
                .iter()
                .filter(|n| ref_or_unit_matches(reference, n))
                .count();
            assert_eq!(
                index.get(reference).map_or(0, Vec::len),
                scanned_count,
                "reference {reference:?}"
            );
        }
    }

    #[test]
    fn a_blank_board_name_falls_back_to_the_file_stem() {
        assert_eq!(
            display_board_name("", "power_resistor.kicad_pcb"),
            "power_resistor"
        );
        assert_eq!(
            display_board_name("  ", "C:\\fakepath\\my.board.zip"),
            "my.board"
        );
        assert_eq!(display_board_name("", "dir/noext"), "noext");
        assert_eq!(display_board_name("", ".hidden"), ".hidden");
        assert_eq!(display_board_name("Rev B", "x.kicad_pcb"), "Rev B");
    }

    #[test]
    fn a_dev_build_binary_reads_as_hauksbee_but_an_installed_one_keeps_its_path() {
        use std::path::Path;
        for dev in [
            "/home/u/repo/target/debug/hauksbee",
            "/home/u/repo/target/release/hauksbee",
            "/home/u/repo/target/aarch64-apple-darwin/release/hauksbee",
            "/home/u/repo/target/debug/deps/frontdoor-0123abcd",
        ] {
            assert_eq!(display_executable(Path::new(dev)), "hauksbee", "{dev}");
        }
        let app = "/Applications/Hauksbee.app/Contents/Resources/bin/hauksbee";
        assert_eq!(display_executable(Path::new(app)), app);
    }

    const SHORTED: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    const BLUEPILL: &[u8] =
        include_bytes!("../../../testdata/boards/stm32_bluepill_demo.kicad_pcb");
    const NO_MCU: &[u8] =
        include_bytes!("../../hauksbee-ci/examples/boards/power_resistor.kicad_pcb");
    const BOOT_GATE_FW: &[u8] =
        include_bytes!("../../../testdata/firmware/boot_gate_a/boot_gate.elf");
    /// A minimal binary Altium .`PcbDoc` (OLE2 container, two resistors). Its
    /// bytes do not survive a lossy UTF-8 round-trip.
    const ALTIUM: &[u8] = include_bytes!("../../../testdata/boards/altium_two_resistor.PcbDoc");
    const EAGLE_TIES_BRD: &[u8] =
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd");

    /// A titleless one-resistor `KiCad` board.
    const TINY_BOARD: &[u8] = br#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "A")
  (net 2 "B")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 0 0)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 1 "A"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 2 "B"))
  )
)
"#;

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, contents) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(contents).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// The corpus CM4-adapter fab directory zipped the way a user would, or
    /// `None` when the corpus is absent (and not required).
    fn corpus_gerber_zip() -> Option<Vec<u8>> {
        let Some(dir) = hauksbee_testkit::corpus_board(
            env!("CARGO_MANIFEST_DIR"),
            "famous/uconsole_cm4_adapter_gerber",
        ) else {
            // Corpus-gated like the extract crate's gerber tests, including
            // their licence rule: ClockworkPi publishes this adapter with no
            // licence statement, so corpus.toml marks it
            // `license_confirmed = false` and the default fetch skips it.
            // HAUKSBEE_REQUIRE_CORPUS (which the nightly gate sets) therefore
            // must not demand it; only the explicit uConsole opt-in does.
            assert!(
                std::env::var("HAUKSBEE_REQUIRE_UCONSOLE_CORPUS").is_err(),
                "HAUKSBEE_REQUIRE_UCONSOLE_CORPUS set but uconsole_cm4_adapter_gerber is absent"
            );
            eprintln!(
                "NOT RUN  gerber-zip web test: uconsole_cm4_adapter_gerber is \
                 not in the default fetch (licence unconfirmed)"
            );
            return None;
        };
        let files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.is_file())
            .map(|p| {
                (
                    format!("gerbers/{}", p.file_name().unwrap().to_str().unwrap()),
                    std::fs::read(&p).unwrap(),
                )
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        Some(zip_of(&refs))
    }

    fn drc_section(report: &WebReport) -> &WebSection {
        report
            .sections
            .iter()
            .find(|s| s.title.starts_with("Copper spacing"))
            .expect("a DRC section")
    }

    fn cosim_says_no_mcu(cosim: &WebCosimSection) -> bool {
        cosim
            .findings
            .iter()
            .any(|f| f.why.to_lowercase().contains("microcontroller"))
    }

    /// The co-sim section of a report whose static analysis succeeded with
    /// firmware supplied, asserting it never hit the B6 "could not re-read the
    /// board" failure mode.
    fn cosim_without_reread(r: WebReport) -> WebCosimSection {
        assert!(r.ok, "static analysis succeeds: {:?}", r.error);
        let cosim = r
            .cosim
            .expect("cosim section present once firmware was supplied");
        assert!(
            !cosim.findings.iter().any(|f| f.why.contains("re-read")),
            "the re-read failure mode must be gone: {:?}",
            cosim.findings
        );
        cosim
    }

    /// Ratings are listed only for parts that state some, and serialize without the
    /// keys the card leaves out.
    #[test]
    fn web_report_lists_component_ratings_only_where_the_card_states_them() {
        let report = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(report.ok, "analysis failed: {:?}", report.error);
        assert!(
            report
                .component_ratings
                .values()
                .all(|r| *r != ComponentRatings::default()),
            "{:?}",
            report.component_ratings
        );
        let json = serde_json::to_value(&report).unwrap();
        if let Some(all) = json["component_ratings"].as_object() {
            assert!(all
                .values()
                .all(|r| r.as_object().is_some_and(|o| !o.is_empty())));
        }
        let one = ComponentRatings {
            max_current_a: Some(0.5),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&one).unwrap(),
            serde_json::json!({ "max_current_a": 0.5 })
        );
        assert!(!one.is_empty() && ComponentRatings::default().is_empty());
    }

    #[test]
    fn web_report_exposes_only_bound_component_assertion_capabilities() {
        let report = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(report.ok, "analysis failed: {:?}", report.error);
        assert!(
            report
                .component_assertions
                .get("R1")
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "max_current")),
            "a stamped resistor must expose measurable current: {:?}",
            report.component_assertions
        );
        assert!(
            !report
                .component_assertions
                .get("U1")
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "max_current")),
            "an MCU must not be offered a through-current assertion: {:?}",
            report.component_assertions
        );
    }

    #[test]
    fn complete_browser_bundle_is_recorded_in_report_inventory() {
        use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};

        let named = |name: &str, bytes: &[u8]| NamedUpload {
            name: name.into(),
            bytes: bytes.to_vec(),
        };
        let json = analyze_design_json(DesignUpload {
            board: named(
                "button_pullup.kicad_pcb",
                include_bytes!("../../../testdata/boards/button_pullup.kicad_pcb"),
            ),
            firmware: None,
            schematic: None,
            bom: Some(named("bom.csv", b"Designator,Value\nR1,10k\n")),
            placement: Some(named(
                "placement.csv",
                b"Designator,Mid X,Mid Y,Rotation,Layer\nR1,100,100,0,top\n",
            )),
            variant: Some(named("production.variant.toml", b"name = \"production\"\n")),
            asbuilt: Some(named("unit.asbuilt.toml", b"")),
            models: vec![named(
                "passives.toml",
                include_bytes!("../../hauksbee-models/db/passives.toml"),
            )],
        });
        let value: serde_json::Value = serde_json::from_str(&json).expect("report JSON");
        assert_eq!(value["ok"], true, "{json}");
        let roles: std::collections::BTreeSet<_> = value["inventory"]
            .as_array()
            .expect("inventory")
            .iter()
            .filter_map(|artifact| artifact["role"].as_str())
            .collect();
        for role in ["bom", "placement", "variant", "as_built", "model_pack"] {
            assert!(roles.contains(role), "missing {role} in {roles:?}: {json}");
        }
    }

    #[test]
    fn web_report_carries_per_object_import_coverage() {
        let report = analyze("boot_gate.kicad_pcb", SHORTED);
        let diagnostics = report
            .import_diagnostics
            .expect("a successful import carries typed diagnostics");
        assert_eq!(diagnostics.recovered, 3);
        assert_eq!(diagnostics.partial, 0);
        assert_eq!(diagnostics.unplaced, 0);
        assert_eq!(diagnostics.missing_or_refused, 0);
        let u1 = diagnostics
            .objects
            .iter()
            .find(|object| object.id == "U1")
            .expect("U1 import row");
        assert_eq!(u1.status, "recovered");
        assert_eq!(u1.confidence, "high");
        assert!(u1.x.is_some() && u1.y.is_some());
        assert!(u1.nets.iter().any(|net| net == "+5V"));
    }

    #[test]
    fn gerber_import_diagnostics_link_a_real_reconstructed_net_to_located_objects() {
        let bytes = zip_of(&[
            (
                "board-F_Cu.gbr",
                include_bytes!(
                    "../../../testdata/ingest-robustness/gerber_kicad_pos/board-F_Cu.gbr"
                ),
            ),
            (
                "board-B_Cu.gbr",
                include_bytes!(
                    "../../../testdata/ingest-robustness/gerber_kicad_pos/board-B_Cu.gbr"
                ),
            ),
            (
                "board-PTH.drl",
                include_bytes!(
                    "../../../testdata/ingest-robustness/gerber_kicad_pos/board-PTH.drl"
                ),
            ),
            (
                "board-all.pos",
                include_bytes!(
                    "../../../testdata/ingest-robustness/gerber_kicad_pos/board-all.pos"
                ),
            ),
        ]);
        let report = analyze("fab.zip", &bytes);
        assert!(report.ok, "fixture must import: {:?}", report.error);
        let diagnostics = report.import_diagnostics.expect("typed diagnostics");
        assert_eq!(diagnostics.format, "Gerber / Excellon reconstruction");
        assert_eq!(diagnostics.recovered, 2);
        assert_eq!(diagnostics.partial, 1);
        let issue = diagnostics
            .issues
            .iter()
            .find(|issue| issue.kind == "split_net")
            .expect("synthetic reconstructed boundary is explained");
        assert_eq!(issue.net.as_deref(), Some("NET_1"));
        let located_on_net = diagnostics
            .objects
            .iter()
            .filter(|o| o.x.is_some() && o.y.is_some() && o.nets.iter().any(|n| n == "NET_1"))
            .count();
        assert_eq!(located_on_net, 2);
    }

    #[test]
    fn a_plating_refusal_is_not_mislabeled_as_a_split_net() {
        macro_rules! bare {
            ($f:literal) => {
                (
                    $f,
                    include_bytes!(concat!(
                        "../../../testdata/ingest-robustness/gerber_bare_role_names/",
                        $f
                    )) as &[u8],
                )
            };
        }
        // One copper layer only: with a second layer the reader would infer
        // plating from pad rings on both sides, and the refusal under test
        // would not arise.
        let bytes = zip_of(&[
            bare!("BoardOutline.gbr"),
            bare!("Through.drl"),
            bare!("Top.gbr"),
            bare!("TopAssy.gbr"),
            bare!("TopDimension.gbr"),
            bare!("TopMask.gbr"),
            bare!("TopSilk.gbr"),
        ]);
        let report = analyze("bare-role-names.zip", &bytes);
        assert!(report.ok, "fixture must import: {:?}", report.error);
        let diagnostics = report.import_diagnostics.expect("typed diagnostics");
        let plating = diagnostics
            .issues
            .iter()
            .find(|issue| issue.explanation.contains("stitch no layers"))
            .expect("plating authority refusal stays visible");
        assert_eq!(plating.kind, "missing");
        assert!(!diagnostics
            .issues
            .iter()
            .any(|issue| issue.kind == "split_net" && issue.net.is_none()));
        assert!(diagnostics.missing_or_refused >= 1);
    }

    #[test]
    fn parser_failures_carry_the_named_line_and_a_fix() {
        assert_eq!(error_line_number("pipeline 2 failed"), None);
        assert_eq!(error_line_number("lineage 7 is invalid"), None);
        assert_eq!(error_line_number("parse failed at line: 19"), Some(19));

        let json = unreadable_input_json(
            "design.brd",
            b"<eagle>\nmalformed schematic record\n",
            &crate::board_input::BoardInputError::Schematic(
                "schematic parser failed at line 2".to_string(),
            ),
        );
        let report: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["import_failure"]["stage"], "schematic reader");
        assert_eq!(
            report["import_failure"]["excerpt"],
            "line 2: malformed schematic record"
        );
    }

    /// `/api/analyze` goes through the design route. A refused board there used
    /// to come back as prose only (with the "Supported:" list inside it); it now
    /// carries the same structured refusal as the single-file route.
    #[test]
    fn the_design_route_refuses_a_board_with_a_structured_import_failure() {
        use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};

        let refuse = |name: &str, bytes: &[u8]| -> serde_json::Value {
            let json = analyze_design_json(DesignUpload {
                board: NamedUpload {
                    name: name.into(),
                    bytes: bytes.to_vec(),
                },
                firmware: None,
                schematic: None,
                bom: None,
                placement: None,
                variant: None,
                asbuilt: None,
                models: Vec::new(),
            });
            serde_json::from_str(&json).expect("report JSON")
        };
        let cut = refuse(
            "cut.kicad_pcb",
            b"(kicad_pcb (version 20240108)\n  (footprint \"R\"",
        );
        assert_eq!(cut["ok"], false);
        let failure = &cut["import_failure"];
        assert_eq!(failure["kind"], "truncated", "{cut}");
        assert_eq!(failure["file"], "cut.kicad_pcb");
        assert_eq!(failure["line"], 2);
        assert!(failure["suggested_fix"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        assert!(failure["supported_formats"]
            .as_array()
            .is_some_and(|l| !l.is_empty()));

        let wrong = refuse("notes.kicad_pcb", b"hello, not a board");
        assert_eq!(wrong["import_failure"]["kind"], "wrong_content", "{wrong}");
        let odd = refuse("design.xyz", b"hello, not a board");
        assert_eq!(
            odd["import_failure"]["kind"], "unsupported_extension",
            "{odd}"
        );
    }

    #[cfg(feature = "avr")]
    #[test]
    fn web_cosim_keeps_a_schematic_only_contact_as_a_short_fault() {
        let board_text = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "+5V")
  (net 2 "GND")
  (net 3 "D13")
  (footprint "Package_DIP:DIP-28_W7.62mm" (layer "F.Cu") (at 0 0)
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

        let firmware = include_bytes!("../../../testdata/firmware/avr_watchdog/nowdt.elf");
        let (cosim, captured) = run_web_cosim(
            &board,
            "blinky.kicad_pcb",
            "nowdt.elf",
            firmware,
            &drc,
            Some(&qualification),
        );
        assert!(cosim.ran, "the tracked AVR firmware must execute");
        assert!(cosim.findings.iter().any(|finding| {
            finding.level == "note" && finding.what.contains("physical copper connection")
        }));
        assert!(captured
            .expect("a successful run captures evidence")
            .faults
            .iter()
            .any(|fault| fault.kind == crate::stress::FaultKind::Short));

        let ties = crate::schematic_ties::SchematicTies {
            path: "blinky.sch".into(),
            raw: b"<eagle><drawing><schematic/></drawing></eagle>".to_vec(),
            ties: declarations,
            auto_discovered: false,
        };
        let report = analyze_with_firmware_detailed_with_ties(
            "blinky.kicad_pcb",
            board_text.as_bytes(),
            "nowdt.elf",
            firmware,
            Some(&ties),
        )
        .report;
        let report_json = serde_json::to_value(&report).expect("web report serializes");
        let inventory = report_json["inventory"].as_array().expect("inventory");
        let schematic = inventory
            .iter()
            .find(|artifact| artifact["role"] == "schematic")
            .expect("successful firmware evidence retains schematic context");
        assert_eq!(schematic["kind"], "eagle_board");
        assert_eq!(schematic["format"], "eagle_schematic");
        assert!(inventory
            .iter()
            .any(|artifact| artifact["role"] == "firmware" && artifact["kind"] == "elf"));
    }

    #[test]
    fn unused_schematic_declarations_do_not_enter_web_causal_provenance() {
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
        let report: serde_json::Value = serde_json::from_str(&analyze_json_with_ties(
            "declared.brd",
            EAGLE_TIES_BRD,
            Some(&ties),
        ))
        .expect("web JSON");
        let inventory = report["inventory"].as_array().expect("inventory");
        let (schematic_id, schematic) = inventory
            .iter()
            .enumerate()
            .find(|(_, artifact)| artifact["role"] == "schematic")
            .expect("every supplied input belongs in the inventory");
        assert_eq!(schematic["kind"], "eagle_board");
        assert!(
            report["evidence"]
                .as_array()
                .expect("evidence maps")
                .iter()
                .all(|map| map["artifacts"].as_array().is_none_or(|ids| ids
                    .iter()
                    .all(|id| id.as_u64() != Some(schematic_id as u64)))),
            "an unused declaration is inventory, not causal evidence"
        );
    }

    /// An ODB++ job or IPC-2581 document gets a "Not checked" DRC verdict that
    /// names the input correctly and a coverage note that does not claim the
    /// circuit was reverse-extracted from copper.
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
            let drc = drc_section(&report);
            assert!(
                drc.verdict.contains("Not checked"),
                "{label}: {}",
                drc.verdict
            );
            assert!(drc.verdict.contains(phrase), "{label}: {}", drc.verdict);
            assert!(!drc.verdict.contains("gerber"), "{label}: {}", drc.verdict);
            let notes: Vec<&str> = report.notes.iter().map(|n| n.message.as_str()).collect();
            assert!(
                notes.iter().any(|n| n.contains("native layout")),
                "{label}: {notes:?}"
            );
            assert!(
                !notes
                    .iter()
                    .any(|n| n.contains("reverse-extracted from the fab")),
                "{label}: {notes:?}"
            );
        }
    }

    /// The web GPIO table ranks by toggles, then voltage swing, matching the
    /// CLI/JSON surfaces; the most active net survives truncation regardless
    /// of its name.
    #[test]
    fn gpio_nets_rank_by_activity_then_voltage_swing_not_name() {
        use crate::scheduler::NetStat;
        use std::collections::HashMap;
        let net_volts: HashMap<String, f64> = HashMap::new();

        let mut stats: HashMap<String, NetStat> = HashMap::new();
        stats.insert("ZZ_CLK".to_string(), NetStat::with_toggles(9999));
        for i in 0..16 {
            stats.insert(format!("A{i:02}"), NetStat::with_toggles(1));
        }
        let top = top_gpio_nets(&stats, &net_volts, 15);
        assert_eq!(top.len(), 15);
        assert_eq!(top[0].name, "ZZ_CLK");
        assert!(top.iter().all(|n| n.driven));

        let mut stats: HashMap<String, NetStat> = HashMap::new();
        stats.insert(
            "AAA_TINY".to_string(),
            NetStat::with_toggles_and_range(1, 0.0, 0.1),
        );
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
        let top = top_gpio_nets(&stats, &net_volts, 15);
        assert_eq!(top.len(), 15);
        assert_eq!(top[0].name, "ZZ_BIG");
        assert!(!top.iter().any(|n| n.name == "AAA_TINY"));
    }

    #[test]
    fn from_plain_carries_heads_up_and_serializes() {
        let mut p = PlainReport {
            subject: "signal-integrity".to_string(),
            ..PlainReport::default()
        };
        p.heads_up.push(HeadsUp::glossed(
            "USB trace impedance is 171 ohm, +71% from target (90 ohm).",
            "reflections can make the link marginal",
            "match trace width and spacing to the stackup",
        ));
        let sect = WebSection::from_plain("Signal integrity", &p);
        assert!(sect.findings.is_empty());
        assert_eq!(sect.heads_up.len(), 1);
        assert!(!sect.heads_up[0].why.is_empty() && !sect.heads_up[0].fix.is_empty());
        let json = serde_json::to_string(&sect).unwrap();
        assert!(
            json.contains("heads_up") && json.contains("171 ohm"),
            "{json}"
        );
    }

    #[test]
    fn healthy_board_with_heads_up_or_open_bind_never_says_looks_healthy() {
        let healthy = |h: &str| h.to_lowercase().contains("looks healthy");
        assert!(!healthy(&overall_headline(0, 0, true, false)));
        let bind_open = overall_headline(0, 0, false, true);
        assert!(!healthy(&bind_open) && bind_open.to_lowercase().contains("trustworthy"));
        assert!(healthy(&overall_headline(0, 0, false, false)));
    }

    /// A lint finding at `severity`, for the verdict table below.
    fn verdict_finding(severity: &str) -> crate::result::JsonFinding {
        crate::result::JsonFinding {
            check: "lint".into(),
            kind: "pcb_schematic_mismatch".into(),
            severity: severity.into(),
            nets: Vec::new(),
            location_mm: None,
            layer: None,
            refs: vec!["C4".into()],
            actionable: true,
            message: "C4 has value 100n in the schematic but 10n on the PCB".into(),
            plain: "C4's value differs between schematic and PCB".into(),
            fix: Some("make the two values agree".into()),
        }
    }

    /// A web report over `findings` whose machine verdict is built exactly as
    /// the static path builds it, then explained.
    fn verdict_web(
        findings: &[crate::result::JsonFinding],
        gate: bool,
        conditions: &[&str],
        refusal: Option<Refusal>,
    ) -> WebReport {
        let bind = crate::result::BindSummary {
            resolved: 1,
            unresolved: 0,
            non_ignored: 1,
            critical_parts_bound: "1/1".into(),
            critical_parts_bound_n: 1,
            critical_parts_total: 1,
            mcu_bound: false,
            active_path_unresolved: Vec::new(),
            resolved_but_open_active: Vec::new(),
        };
        let drc = crate::result::DrcStructured::from_report(&hauksbee_extract::DrcReport {
            clearance_mm: 0.2,
            findings: Vec::new(),
            primitive_count: 2,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
            excluded: Vec::new(),
        });
        let (total, serious) = (
            findings.len(),
            findings.iter().filter(|f| f.severity == "serious").count(),
        );
        let mut machine =
            crate::reports::check::machine_report("board", &bind, &drc, None, findings, gate);
        machine.verdict_conditions = conditions.iter().map(ToString::to_string).collect();
        let mut report = WebReport {
            ok: true,
            headline: overall_headline(total, serious, false, false),
            serious,
            total,
            refusal,
            machine: Some(machine),
            ..WebReport::default()
        };
        report.explain_verdict();
        report
    }

    /// The web headline takes its class from the `--check` machine verdict:
    /// one table of the same cases the plain triage agrees with.
    #[test]
    fn the_web_headline_follows_the_machine_verdict() {
        let finding = verdict_finding;
        let web = |findings: Vec<_>, gate, conditions: &[&str], refusal| {
            verdict_web(&findings, gate, conditions, refusal)
        };
        let refusal = || {
            Some(Refusal::new(
                "firmware co-simulation",
                "a firmware image",
                Vec::<String>::new(),
                "upload one",
            ))
        };
        let clean = web(Vec::new(), false, &[], None);
        assert_eq!(
            (clean.verdict.as_str(), clean.headline.as_str()),
            ("pass", overall_headline(0, 0, false, false).as_str())
        );

        // A gating warning that is not serious: the old headline read "worth
        // a look" beside a failed JSON verdict.
        let gating = web(vec![finding("warning")], true, &[], None);
        assert_eq!(gating.verdict, "fail");
        assert!(
            gating.headline.starts_with("Not OK to order: "),
            "{}",
            gating.headline
        );
        assert!(gating
            .verdict_reason
            .as_deref()
            .is_some_and(|r| r.contains("C4's value")));

        let serious = web(vec![finding("serious")], true, &[], None);
        assert_eq!(serious.verdict, "fail");
        assert!(
            serious.headline.contains("1 serious"),
            "{}",
            serious.headline
        );

        // Inconclusive: never a pass-shaped headline, and an inconclusive
        // headline already set by the run is kept.
        let refused = web(Vec::new(), false, &[], refusal());
        assert_eq!(refused.verdict, "invalid");
        assert!(
            refused.headline.starts_with("Inconclusive, not a pass: "),
            "{}",
            refused.headline
        );
        let mut kept = web(Vec::new(), false, &[], refusal());
        kept.headline = REFUSAL_HEADLINE.to_string();
        kept.explain_verdict();
        assert_eq!(kept.headline, REFUSAL_HEADLINE);

        // A conditional pass names its conditions after the lead-in.
        let condition = "Indicator polarity versus intent was not checked for D1.";
        let conditional = web(Vec::new(), false, &[condition], None);
        assert_eq!(conditional.verdict, "pass");
        assert_eq!(conditional.verdict_conditions, [condition]);
        assert!(
            conditional
                .headline
                .starts_with("OK to order, conditionally: ")
                && conditional
                    .headline
                    .ends_with(&format!("this rests on: {condition}")),
            "{}",
            conditional.headline
        );
        // A failing run states no conditions: the reason says what to do.
        let failing = web(vec![finding("warning")], true, &[condition], None);
        assert!(failing.verdict_conditions.is_empty() && !failing.headline.contains(condition));

        // A pass with items the triage lists to inspect takes the tag the
        // closing VERDICT line prints, never "OK to order".
        let entry = |statement: &str| crate::plain::TriageEntry {
            id: "lint".into(),
            kind: "unchecked_mcu".into(),
            statement: statement.to_string(),
            location_mm: None,
        };
        let mut inspect = web(Vec::new(), false, &[condition], None);
        inspect.triage = Some(crate::plain::OrderTriage {
            inspect: vec![entry("Check C4 by hand."), entry("Check C5 by hand.")],
            ..crate::plain::OrderTriage::default()
        });
        // Derived afresh from the run's own headline, as the firmware pass does.
        inspect.headline = std::mem::take(&mut inspect.pre_verdict_headline);
        inspect.explain_verdict();
        assert_eq!(inspect.verdict, "pass");
        assert_eq!(inspect.verdict_inspect, "inspect before ordering (2 items)");
        assert!(
            inspect
                .headline
                .starts_with("Inspect before ordering (2 items), conditionally: ")
                && !inspect.headline.contains("OK to order"),
            "{}",
            inspect.headline
        );
        // Control: an empty inspect list keeps the plain conditional pass.
        assert!(conditional.verdict_inspect.is_empty());
    }

    /// A co-sim that could not run, or ran without a numerical qualification,
    /// is not judged (`invalid`, with the refusal naming why and what next);
    /// a fault the co-sim did observe still fails the board.
    #[test]
    fn an_unrun_cosim_is_not_judged_but_an_observed_fault_fails() {
        let with_cosim = |cosim: WebCosimSection| {
            let mut report = verdict_web(&[], false, &[], None);
            report.refusal = refusal_for_cosim(&cosim, &WebCosimCoverage::default());
            apply_refusal_headline(&mut report);
            report.cosim = Some(cosim);
            report.explain_verdict();
            report
        };

        let not_run = with_cosim(cosim_unavailable("the emulator is not installed"));
        assert_eq!(not_run.verdict, "invalid");
        assert_eq!(not_run.headline, REFUSAL_HEADLINE);
        let refusal = not_run.refusal.as_ref().expect("named reason");
        assert_eq!(
            refusal.missing_prerequisite,
            "the emulator is not installed"
        );
        assert!(!refusal.next_action.is_empty());

        let mut unqualified = clean_ran_section();
        unqualified.findings.push(WebFinding::unplaced(
            "note",
            UNQUALIFIED_COSIM,
            "relative tolerance must be finite and positive",
            "Use finite, positive solver tolerances and run the co-simulation again.",
        ));
        let unqualified = with_cosim(unqualified);
        assert_eq!(unqualified.verdict, "invalid");
        assert!(unqualified
            .refusal
            .as_ref()
            .is_some_and(|r| r.missing_prerequisite.contains("tolerance")));

        for level in ["serious", "warning"] {
            let mut observed = clean_ran_section();
            observed
                .findings
                .push(finding(level, "Q1 destroyed by over-current."));
            let observed = with_cosim(observed);
            assert!(observed.refusal.is_none());
            assert_eq!(observed.verdict, "fail", "{level}");
            assert!(observed
                .verdict_reason
                .as_deref()
                .is_some_and(|r| r.contains("Q1")));
        }
    }

    /// The pre-layout frame decides an unrouted board's headline; a co-sim
    /// refusal beside it must not replace it with a generic failure sentence.
    #[test]
    fn a_cosim_refusal_keeps_the_pre_layout_headline() {
        let mut report = verdict_web(&[], false, &[], None);
        report.fix_first.notice = Some("Route the board");
        report.headline =
            format!("{PRE_LAYOUT_HEADLINE_LEAD} the circuit checks found no problems.");
        let before = report.headline.clone();
        let cosim = cosim_unavailable("the emulator is not installed");
        report.refusal = refusal_for_cosim(&cosim, &WebCosimCoverage::default());
        apply_refusal_headline(&mut report);
        assert_eq!(report.headline, before);
    }

    #[test]
    fn web_report_carries_bind_summary() {
        let r = analyze("bp.kicad_pcb", BLUEPILL);
        let bind = r.bind.expect("bind summary present on a bound board");
        assert!(
            bind.critical_parts_bound.contains('/'),
            "{}",
            bind.critical_parts_bound
        );
    }

    /// A Serious USB-C verdict becomes a serious section finding that reaches
    /// the headline; an Info verdict becomes a heads-up that still denies a
    /// false "Looks healthy". Both verdicts are full sentences.
    #[test]
    fn web_persona_carries_usbc_verdict_like_the_cli() {
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
        assert!(sect.findings.iter().any(|f| f.level == "serious"));
        assert!(
            sect.verdict.ends_with('.') && sect.verdict.contains(' '),
            "{:?}",
            sect.verdict
        );
        let serious_n = sect
            .findings
            .iter()
            .filter(|f| f.level == "serious")
            .count();
        let headline = overall_headline(sect.findings.len(), serious_n, false, false);
        assert!(
            headline.to_lowercase().contains("serious")
                && !headline.to_lowercase().contains("looks healthy"),
            "{headline}"
        );

        let info = UsbcReport {
            level: UsbcLevel::Info,
            has_discrete_rd: false,
            headline: "No discrete CC pulldown visible.".to_string(),
            ..serious
        };
        let isect = super::usbc_web_section(&info).expect("info verdict yields a section");
        assert!(isect.findings.is_empty());
        assert_eq!(isect.heads_up.len(), 1);
        assert!(
            isect.verdict.ends_with('.') && isect.verdict.contains(' '),
            "{:?}",
            isect.verdict
        );
        let h = overall_headline(0, 0, !isect.heads_up.is_empty(), false);
        assert!(!h.to_lowercase().contains("looks healthy"), "{h}");
    }

    /// `SHORTED` with one track on it: no longer an unrouted layout, so its
    /// copper findings count instead of being held back.
    fn shorted_and_routed() -> Vec<u8> {
        let text = String::from_utf8(SHORTED.to_vec()).unwrap();
        let track = "  (segment (start 50 50) (end 51 50) (width 0.25) (layer F.Cu) (net 1))\n)";
        text.trim_end()
            .strip_suffix(')')
            .map(|head| format!("{head}{track}\n").into_bytes())
            .unwrap()
    }

    #[test]
    fn an_unrouted_shorted_board_holds_its_copper_findings_back_and_says_so() {
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(r.fix_first.notice.is_some());
        assert_eq!(r.fix_first.copper_held, 2, "the two copper shorts");
        // The held copper shorts are not counted; the not-routed state is
        // listed as serious, so it is. The counters, the headline and the
        // section verdicts all quote that one number.
        let listed_serious: usize = r
            .sections
            .iter()
            .filter(|s| !s.title.starts_with("Copper spacing"))
            .map(|s| s.findings.iter().filter(|f| f.level == "serious").count())
            .sum();
        assert!(
            r.serious >= 1,
            "the unrouted state is a counted serious finding"
        );
        assert_eq!(
            r.serious, listed_serious,
            "counter == serious findings listed"
        );
        let listed: usize = r
            .sections
            .iter()
            .filter(|s| !s.title.starts_with("Copper spacing"))
            .map(|s| s.findings.len())
            .sum();
        assert_eq!(
            r.total, listed,
            "counter == findings listed outside the held copper"
        );
        assert!(r.headline.starts_with("Not routed yet: "), "{}", r.headline);
        assert!(
            r.headline.contains("2 copper finding(s) are held back"),
            "{}",
            r.headline
        );
        // They are held back, not dropped: the copper section still lists them.
        assert_eq!(drc_section(&r).findings.len(), 2);
        assert!(
            r.verdict == "fail",
            "an unrouted board never reads as ready"
        );
    }

    /// The pre-layout board whose counters used to read "0 serious" beside a
    /// finding tagged SERIOUS: the counters, the headline and each section's
    /// verdict line quote the findings actually listed, never a private tally.
    #[test]
    fn every_count_on_the_report_derives_from_the_listed_findings() {
        const UNROUTED: &[u8] =
            include_bytes!("../../../testdata/boards/stm32_adc_divider_demo.kicad_pcb");
        for (name, bytes) in [
            ("stm32.kicad_pcb", UNROUTED),
            ("boot_gate.kicad_pcb", SHORTED),
        ] {
            let r = analyze(name, bytes);
            let counted = |s: &&WebSection| {
                !s.title.starts_with("Copper spacing") || r.fix_first.notice.is_none()
            };
            let listed: usize = r
                .sections
                .iter()
                .filter(counted)
                .map(|s| s.findings.len())
                .sum();
            let listed_serious: usize = r
                .sections
                .iter()
                .filter(counted)
                .map(|s| s.findings.iter().filter(|f| f.level == "serious").count())
                .sum();
            assert_eq!(
                (r.total, r.serious),
                (listed, listed_serious),
                "{name}: counters"
            );
            assert!(r.serious > 0, "{name}: a serious finding is listed");
            assert!(
                !r.headline.contains("none serious") && !r.headline.contains("no problems"),
                "{name}: the headline denies a listed serious finding: {}",
                r.headline
            );
            for s in r.sections.iter().filter(counted) {
                let n = s.findings.len();
                let serious = s.findings.iter().filter(|f| f.level == "serious").count();
                if n > 0 && s.verdict.contains(" found,") {
                    let expect = if serious == 0 {
                        "none serious".to_string()
                    } else {
                        format!("{serious} serious")
                    };
                    assert!(
                        s.verdict.contains(&format!("{n} issue")) && s.verdict.contains(&expect),
                        "{name}: section '{}' says '{}' over {n} findings ({serious} serious)",
                        s.title,
                        s.verdict
                    );
                }
            }
        }
    }

    #[test]
    fn analyze_shorted_board_reports_serious_with_positions_and_valid_json() {
        let r = analyze("boot_gate.kicad_pcb", &shorted_and_routed());
        assert!(r.ok, "extraction should succeed: {:?}", r.error);
        assert!(
            r.fix_first.notice.is_none(),
            "a routed board has no pre-layout state"
        );
        assert!(r.serious > 0, "boot_gate has copper shorts");
        assert!(r.headline.to_lowercase().contains("serious"));
        assert!(drc_section(&r)
            .findings
            .iter()
            .any(|f| f.level == "serious"));
        for s in &r.sections {
            for f in &s.findings {
                assert!(!f.what.is_empty() && !f.why.is_empty() && !f.fix.is_empty());
            }
        }
        assert!(!r.components.is_empty(), "a KiCad layout has placed parts");
        assert!(r.cosim.is_none(), "board-only analyze must not set cosim");

        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["sections"].as_array().unwrap().len() >= 3);
        assert!(v.get("cosim").is_none(), "board-only JSON must omit cosim");
        assert_eq!(v["clearance_rule_source"]["source"], "defaulted");
        assert!(v["clearance_rule_source"]["value_mm"].is_number());
        assert!(v["triage"]["do_not_order"].is_array());
        assert!(v["triage"]["inspect"].is_array());
        assert!(v["triage"]["checked_ok"].is_array());
        assert!(v["triage"]["not_covered"].is_string());
    }

    #[test]
    fn analyze_garbage_returns_a_friendly_error() {
        let r = analyze("nope.txt", b"this is not a board file at all");
        assert!(!r.ok);
        assert!(r.error.is_some());
        assert!(analyze_json("nope.txt", b"garbage").contains("\"ok\":false"));
    }

    /// The retired Board-as-Code text format is refused like any other
    /// unrecognised file, by extension or by content, and the refusal lists
    /// what is accepted instead of advertising `.board`.
    #[test]
    fn a_dot_board_upload_is_refused_like_any_unrecognised_file() {
        let dsl =
            b"# Board-as-Code (hauksbee board DSL v1)\nboard version 20241229\n\nfn main {\n}\n";
        for name in ["tarski.board", "exported.txt"] {
            let r = analyze(name, dsl);
            assert!(!r.ok, "{name} must be refused");
            let err = r.error.unwrap_or_default();
            assert!(err.contains(".kicad_pcb"), "{name}: {err}");
            let listed = err.split_once("Supported:").map_or("", |(_, rest)| rest);
            assert!(!listed.contains(".board"), "{name}: {err}");
        }
    }

    /// A junk zip says what a board zip holds.
    #[test]
    fn analyze_junk_zip_says_what_it_looked_for() {
        let r2 = analyze("junk.zip", &zip_of(&[("README.md", b"not a board")]));
        assert!(!r2.ok);
        let err = r2.error.unwrap_or_default();
        assert!(err.contains("gerber") && !err.contains(".board"), "{err}");
    }

    /// A real gerber archive through the web path reverse-extracts nets, keeps
    /// the DRC section honest, and with firmware reaches a real co-sim outcome.
    /// Corpus-gated.
    #[test]
    fn analyze_gerber_zip_reverse_extracts() {
        let Some(bytes) = corpus_gerber_zip() else {
            return;
        };
        let r = analyze("cm4_adapter_gerbers.zip", &bytes);
        assert!(r.ok, "gerber zip must reverse-extract: {:?}", r.error);
        assert!(r.num_nets > 0, "nets recovered from copper: {}", r.num_nets);
        assert!(drc_section(&r).verdict.contains("Not checked"));
        assert!(r
            .notes
            .iter()
            .any(|n| n.message.contains("reverse-extracted")));

        let cosim = cosim_without_reread(analyze_with_firmware(
            "cm4_adapter_gerbers.zip",
            &bytes,
            "fw.elf",
            BOOT_GATE_FW,
        ));
        if !cosim.ran {
            assert!(cosim_says_no_mcu(&cosim), "{:?}", cosim.findings);
        }
    }

    #[test]
    fn web_report_embeds_the_whole_glossary() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rows = value["glossary"].as_array().unwrap();
        assert_eq!(rows.len(), hauksbee_ir::glossary::all().len());
        assert!(rows.iter().any(|r| r["term"] == "invalid"));
    }

    /// Byte-for-byte golden of the plain .`kicad_pcb` web report; additive
    /// evidence / diagnostic fields and the two DRC wording changes are
    /// normalised before comparing.
    #[test]
    fn evidence_fields_are_additive_to_the_plain_kicad_web_golden() {
        let golden = include_str!("../../../testdata/golden/boot_gate_web_report.json");
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let object = value.as_object_mut().unwrap();
        for key in [
            "assumptions",
            "evidence",
            "fix_first",
            "glossary",
            // A pre-layout board frames its headline and leaves the copper
            // findings and the not-routed state out of the counts.
            "headline",
            "serious",
            "total",
            "inventory",
            "import_diagnostics",
            "import_failure",
            "model_coverage",
            "component_assertions",
            "component_ratings",
            "clearance_rule_source",
            "triage",
            "verdict",
            "verdict_reason",
            // Structured next steps and the command-free reason: additive
            // fields the golden predates.
            "verdict_why",
            "next_actions",
            // The CLI's own `run --check --json` document, sent alongside.
            "check_report",
        ] {
            object.remove(key);
        }
        // Per-section buttons are additive too.
        for section in object
            .get_mut("sections")
            .and_then(serde_json::Value::as_array_mut)
            .unwrap()
        {
            section.as_object_mut().unwrap().remove("actions");
        }
        let mut golden_value: serde_json::Value = serde_json::from_str(golden).unwrap();
        for key in ["headline", "serious", "total"] {
            golden_value.as_object_mut().unwrap().remove(key);
        }
        let golden_drc = golden_value["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|section| section["title"] == "Copper spacing (DRC)")
            .unwrap();
        let drc = object
            .get_mut("sections")
            .and_then(serde_json::Value::as_array_mut)
            .unwrap()
            .iter_mut()
            .find(|section| {
                section["title"]
                    .as_str()
                    .is_some_and(|title| title.starts_with("Copper spacing"))
            })
            .unwrap();
        drc["title"] = golden_drc["title"].clone();
        drc["verdict"] = golden_drc["verdict"].clone();
        assert_eq!(value, golden_value);
    }

    #[test]
    fn a_failing_web_report_states_its_reason_and_next_action() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let reason = value["verdict_reason"]
            .as_str()
            .expect("a serious report carries a reason");
        // Two shorts, and the layout is not routed at all.
        assert!(
            reason.starts_with("3 blocking problem(s), first: ") && reason.contains("; next: "),
            "{reason}"
        );
    }

    #[test]
    fn the_web_reason_has_no_terminal_command_and_the_steps_are_buttons() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        // `verdict_reason` is unchanged (CLI wording); `verdict_why` is the
        // same sentence without its `next:` clause.
        let reason = value["verdict_reason"].as_str().unwrap();
        let why = value["verdict_why"].as_str().expect("verdict_why");
        assert!(
            reason.contains("; next: ") && !why.contains("next:"),
            "{why}"
        );
        assert!(why.starts_with("3 blocking problem(s), first: "), "{why}");
        // The board ran on default clearance rules (no .kicad_pro was sent), so
        // the typed provenance asks for the file: once on the report, once on
        // the copper section it concerns.
        let ids = |actions: &serde_json::Value| -> Vec<String> {
            actions
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| x["id"].as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(ids(&value["next_actions"]), ["add_project_rules"]);
        let drc = value["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"].as_str().unwrap().starts_with("Copper spacing"))
            .unwrap();
        assert_eq!(ids(&drc["actions"]), ["add_project_rules"]);
        // No section prose carries a terminal command.
        for section in value["sections"].as_array().unwrap() {
            let verdict = section["verdict"].as_str().unwrap();
            assert!(!verdict.contains("hauksbee "), "{verdict}");
        }
    }

    #[test]
    fn a_board_with_no_mcu_and_firmware_reaches_the_honest_no_mcu_note() {
        // B6 regression: co-sim must run from the ALREADY-normalized board and
        // give a REAL outcome. Here the board has no MCU, so the honest "no
        // microcontroller" note is the reason it cannot run.
        let cosim = cosim_without_reread(analyze_with_firmware(
            "tiny.kicad_pcb",
            TINY_BOARD,
            "fw.elf",
            BOOT_GATE_FW,
        ));
        assert!(!cosim.ran, "the board has no MCU, so the co-sim cannot run");
        assert!(
            cosim_says_no_mcu(&cosim),
            "the honest no-MCU note must be the reason: {:?}",
            cosim.findings
        );
    }

    /// B6, gerber arm: a gerber zip plus firmware must reach a real co-sim
    /// outcome too (usually the honest "no MCU found", since a fab archive
    /// names no parts). Corpus-gated like [`analyze_gerber_zip_reverse_extracts`].
    #[test]
    fn gerber_zip_with_firmware_reaches_cosim() {
        let Some(bytes) = corpus_gerber_zip() else {
            return;
        };
        let cosim = cosim_without_reread(analyze_with_firmware(
            "cm4_adapter_gerbers.zip",
            &bytes,
            "fw.elf",
            BOOT_GATE_FW,
        ));
        // A fab archive carries no part identities, so the expected honest
        // outcome is "no MCU"; a run would also be acceptable if reverse
        // extraction ever learns to name one.
        if !cosim.ran {
            assert!(
                cosim_says_no_mcu(&cosim),
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

    #[test]
    fn frontdoor_json_carries_defaulted_clearance_source_and_qualified_text() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["clearance_rule_source"]["source"], "defaulted",
            "the single-file web path cannot silently imply project rules: {json}"
        );
        assert!(value["clearance_rule_source"]["value_mm"].is_number());
        let drc = value["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|section| {
                section["title"]
                    .as_str()
                    .is_some_and(|title| title.starts_with("Copper spacing"))
            })
            .unwrap();
        assert!(drc["title"].as_str().unwrap().contains("not your rules"));
        let verdict = drc["verdict"].as_str().unwrap();
        assert!(verdict.contains("not your rules"), "{verdict}");
        assert!(verdict.contains("Upload the matching .kicad_pro alongside"));
        assert!(value["triage"]["do_not_order"].is_array());
        assert!(value["triage"]["inspect"].is_array());
        assert!(value["triage"]["checked_ok"].is_array());
        assert!(value["triage"]["not_covered"]
            .as_str()
            .unwrap()
            .contains("DEFAULT rules, not your rules"));
    }

    #[test]
    fn binary_altium_board_survives_the_web_path() {
        let r = analyze("two_resistor.PcbDoc", ALTIUM);
        assert!(
            r.ok,
            "binary board must extract from raw bytes: {:?}",
            r.error
        );
        assert_eq!(r.num_components, 2);
        assert!(r.num_nets > 0);
        let lossy = String::from_utf8_lossy(ALTIUM).into_owned();
        assert_ne!(
            lossy.as_bytes(),
            ALTIUM,
            "lossy decode must corrupt the container"
        );
        assert!(!analyze("two_resistor.PcbDoc", lossy.as_bytes()).ok);
    }

    /// A board with no microcontroller: static analysis succeeds, cosim.ran is
    /// false with the reason, and a typed refusal replaces the healthy verdict.
    #[test]
    fn firmware_on_board_with_no_mcu_says_unavailable() {
        let r = analyze_with_firmware("pr.kicad_pcb", NO_MCU, "fw.elf", BOOT_GATE_FW);
        assert!(r.ok, "static analysis still succeeds: {:?}", r.error);
        let cosim = r.cosim.expect("cosim present once firmware was supplied");
        assert!(!cosim.ran);
        assert!(cosim_says_no_mcu(&cosim), "{:?}", cosim.findings);
        let refusal = r
            .refusal
            .expect("unavailable co-sim must carry a refusal contract");
        assert!(!refusal.claim.is_empty());
        assert!(!r.headline.contains("Looks healthy") && r.headline.contains("invalid"));
        assert!(refusal
            .missing_prerequisite
            .to_lowercase()
            .contains("microcontroller"));
        assert!(refusal
            .valid_partial_conclusions
            .iter()
            .any(|line| line.to_lowercase().contains("static")));
        assert!(!refusal.next_action.is_empty());

        let json = analyze_with_firmware_json("pr.kicad_pcb", NO_MCU, "fw.elf", BOOT_GATE_FW);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
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
    fn firmware_serve_analysis_preserves_the_companion_tie() {
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
        let board_only = analyze_json_with_ties("declared.brd", EAGLE_TIES_BRD, Some(&ties));
        let with_firmware = analyze_with_firmware_json_with_ties(
            "declared.brd",
            EAGLE_TIES_BRD,
            "fw.elf",
            BOOT_GATE_FW,
            Some(&ties),
        );
        for output in [&board_only, &with_firmware] {
            assert!(
                output.contains("schematic names this net pair"),
                "{output:.500}"
            );
            assert!(!output.contains("GND shorts AGND"), "{output:.500}");
        }
    }

    #[test]
    fn empty_firmware_fails_to_load_gracefully() {
        let r = analyze_with_firmware("boot_gate.kicad_pcb", SHORTED, "fw.elf", &[]);
        assert!(r.ok, "static analysis still succeeds: {:?}", r.error);
        assert!(
            !r.cosim
                .expect("cosim present once firmware was supplied")
                .ran
        );
    }

    /// `boot_gate` is an `ATmega328` and `boot_gate.elf` its firmware: the co-sim
    /// either really runs (with cited inventory and an error budget) or, on a
    /// build without the AVR backend, explains why not.
    #[test]
    fn real_firmware_on_in_process_mcu_runs() {
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
            assert!(cosim.seconds_simulated > 0.0);
            assert_eq!(r.inventory.len(), 2, "board and firmware are both cited");
            assert!(r
                .inventory
                .iter()
                .all(|artifact| artifact.sha256().len() == 64));
            assert!(r.evidence.iter().any(|map| map.error_budget().is_some()));
        } else {
            assert!(
                !cosim.findings.is_empty(),
                "a skipped co-sim must explain why"
            );
        }

        let json = analyze_with_firmware_json(
            "boot_gate.kicad_pcb",
            SHORTED,
            "boot_gate.elf",
            BOOT_GATE_FW,
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cosim"]["analog_valid"], true);
        assert!(
            v["cosim"].get("failed_windows").is_none(),
            "omitted when empty"
        );
    }

    fn clean_ran_section() -> WebCosimSection {
        WebCosimSection {
            ran: true,
            seconds_simulated: 0.1,
            analog_valid: true,
            firmware_exercised: true,
            ..WebCosimSection::default()
        }
    }

    fn finding(level: &str, what: &str) -> WebFinding {
        WebFinding::unplaced(level, what, "why", "fix")
    }

    fn diverged_window() -> WebFailedWindow {
        WebFailedWindow {
            start_s: 0.0012,
            end_s: 0.0034,
            reason: "DC Newton did not converge in 100 iters".to_string(),
        }
    }

    /// A co-sim that RAN yet proved nothing (unexercised firmware, a
    /// substitute core, a failed analog window, a note-level advisory) demotes
    /// the bare "Looks healthy" line; a clean exercised run does not, and an
    /// already-demoted headline is left alone.
    #[test]
    fn empty_cosim_demotes_looks_healthy_but_a_real_run_does_not() {
        let healthy = overall_headline(0, 0, false, false);
        let demoted = overall_headline(0, 0, true, false);
        assert_ne!(healthy, demoted);
        let caveat = |s: &WebCosimSection, headline: &String| {
            cosim_caveat_headline(s, &WebCosimCoverage::default(), headline)
        };
        assert_eq!(caveat(&clean_ran_section(), &healthy), None);

        let mutations: [fn(&mut WebCosimSection); 5] = [
            |s| s.firmware_exercised = false,
            |s| s.substituted = true,
            |s| s.analog_valid = false,
            |s| {
                s.findings.insert(
                    0,
                    finding(
                        "note",
                        "Control net 'GATE_CTRL' may be energised at power-up",
                    ),
                );
            },
            |s| {
                s.analog_valid = false;
                s.failed_windows = vec![diverged_window()];
                s.findings
                    .insert(0, analog_invalid_finding(2, &s.failed_windows.clone()));
            },
        ];
        for mutate in mutations {
            let mut s = clean_ran_section();
            mutate(&mut s);
            assert_eq!(caveat(&s, &healthy), Some(demoted.clone()));
            assert!(
                fold_cosim_faults(0, 0, &s).is_none(),
                "caveats do not fold as faults"
            );
        }

        // Not-run handling belongs to the typed refusal path, not this helper.
        let mut not_run = clean_ran_section();
        not_run.ran = false;
        not_run.firmware_exercised = false;
        assert_eq!(caveat(&not_run, &healthy), None);

        let mut caveated = clean_ran_section();
        caveated.substituted = true;
        assert_eq!(caveat(&caveated, &demoted), None);
    }

    #[test]
    fn timing_refusal_is_structured_and_refuses_while_fallback_is_qualified() {
        let window = crate::result::CosimFallbackWindow {
            start_s: 0.001,
            end_s: 0.002,
            method: "backward-euler".to_string(),
            fidelity_note: "first-order and numerically dissipative".to_string(),
            error_estimate_v: Some(0.012),
            state_jump_v: None,
        };
        let section = clean_ran_section();
        let mut coverage = WebCosimCoverage {
            fallback_windows: vec![window],
            ..Default::default()
        };
        assert!(refusal_for_cosim(&section, &coverage).is_none());
        assert!(
            cosim_caveat_headline(&section, &coverage, &overall_headline(0, 0, false, false))
                .is_some()
        );

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
            findings.iter().all(|f| !f.why.contains("TIMING INVALID")),
            "{findings:?}"
        );
    }

    #[test]
    fn synchronous_web_cosim_refuses_when_any_selected_mcu_needs_an_external_backend() {
        use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
        let pin = |number: &str, net: i64, function: &str| Pin {
            number: number.to_string(),
            net: Some(net),
            function: function.to_string(),
            ..Default::default()
        };
        let component = |reference: &str, value: &str, lib_id: &str, pins: Vec<Pin>| Component {
            reference: reference.to_string(),
            value: value.to_string(),
            lib_id: lib_id.to_string(),
            pins,
            ..Default::default()
        };
        let board = ExtractedBoard {
            name: "mixed-backend-board".to_string(),
            nets: vec![Net::new(1, "+5V"), Net::new(2, "GND")],
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
            .map(|f| f.why.as_str())
            .unwrap_or_default();
        assert!(!section.ran, "mixed external/in-process runs must refuse");
        assert!(
            evidence.is_none(),
            "a refused run produces no co-sim evidence"
        );
        assert!(
            reason.contains("U2") && reason.contains("external emulator"),
            "{reason}"
        );
    }

    /// A non-destructive over-stress WARNING escalates total (not serious);
    /// a destructive SERIOUS fault escalates serious; a fault-free run does
    /// not fold at all.
    #[test]
    fn cosim_faults_fold_into_the_headline_by_level() {
        let mut warned = clean_ran_section();
        warned.findings.push(finding(
            "warning",
            "R1 carries ~200 mA past its 100 mA continuous rating.",
        ));
        let folded = fold_cosim_faults(0, 0, &warned).expect("a warning fault folds in");
        assert_eq!((folded.0, folded.1), (1, 0));
        assert!(
            folded.2.contains("none serious") && !folded.2.contains("Looks healthy"),
            "{}",
            folded.2
        );

        let mut killed = clean_ran_section();
        killed
            .findings
            .push(finding("serious", "Q1 destroyed by over-current."));
        let folded = fold_cosim_faults(0, 0, &killed).expect("a serious fault folds in");
        assert_eq!((folded.0, folded.1), (1, 1));
        assert!(folded.2.contains("1 serious"), "{}", folded.2);

        assert!(fold_cosim_faults(0, 0, &clean_ran_section()).is_none());
    }

    #[test]
    fn invalid_cosim_section_serializes_field_and_windows_and_refuses() {
        let mut section = clean_ran_section();
        section.analog_valid = false;
        section.failed_windows = vec![diverged_window()];
        section.findings = vec![analog_invalid_finding(1, &section.failed_windows.clone())];
        let json = serde_json::to_string(&section).unwrap();
        assert!(json.contains("\"analog_valid\":false"), "{json}");
        assert!(json.contains("\"failed_windows\""), "{json}");
        let refusal = refusal_for_cosim(&section, &WebCosimCoverage::default())
            .expect("invalid analog run refuses");
        assert!(
            refusal
                .missing_prerequisite
                .contains("DC Newton did not converge"),
            "{refusal:?}"
        );
        assert!(refusal
            .valid_partial_conclusions
            .iter()
            .any(|line| line.contains("Static board analysis")));
    }

    #[test]
    fn cosim_section_serializes_boot_gates_only_when_present() {
        let mut section = clean_ran_section();
        section.boot_gates = vec![WebBootGate {
            reference: "Q1".to_string(),
            net: "GATE_CTRL".to_string(),
            state: "driven_high".to_string(),
        }];
        let json = serde_json::to_string(&section).unwrap();
        assert!(
            json.contains("\"boot_gates\"") && json.contains("GATE_CTRL"),
            "{json}"
        );
        section.boot_gates.clear();
        assert!(!serde_json::to_string(&section)
            .unwrap()
            .contains("\"boot_gates\""));
    }

    /// The web firmware co-sim surfaces the same boot power-up advisory and
    /// `boot_gates` panel as the CLI for `boot_gate` + variant-A firmware.
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
        assert_eq!(cosim["ran"], serde_json::json!(true), "{json:.400}");
        let gates = cosim.get("boot_gates").and_then(|g| g.as_array());
        assert!(gates.is_some_and(|g| !g.is_empty()), "{json:.400}");
        let findings = cosim["findings"].as_array().expect("findings array");
        assert!(
            findings.iter().any(|f| f["what"]
                .as_str()
                .unwrap_or("")
                .contains("energised at power-up")),
            "{json:.600}"
        );
    }
}
