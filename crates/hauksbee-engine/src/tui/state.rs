//! The pure, terminal-free TUI state.
//!
//! Everything in this module is a plain data model plus navigation logic, with
//! **no** dependency on a terminal, ratatui, or crossterm. That keeps the
//! interesting behaviour (which finding is selected, how the verdict counts add
//! up, what the detail line says) unit-testable without a PTY.
//!
//! The model is built from the SAME structured-honest result the `--json` /
//! text paths read ([`BindSummary`], [`DrcStructured`], the [`JsonFinding`]
//! vectors from [`crate::result::si_findings_json`] /
//! [`crate::result::lint_findings_json`]). The TUI is a renderer over that
//! result; it never re-runs or re-implements a check.

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::report::{BindOutcome, BindReport};
use crate::result::{BindSummary, DrcStructured, JsonFinding};

/// A logic-level label for a control-net voltage, derived from a voltage
/// threshold rather than a GPIO drive direction. The personas were misled by a
/// 1.48 V node labelled "low" (it reads as near-0 V); this makes the label mean
/// what the user expects: HIGH near a rail, LOW near ground, MID in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Near a logic rail (≥ 2.0 V): a driven/pulled-high line.
    High,
    /// Near ground (≤ 0.8 V): a driven/pulled-low line.
    Low,
    /// Between the two thresholds (e.g. an LED forward drop, a divider tap):
    /// neither a clean high nor a clean low.
    Mid,
}

impl Level {
    /// Classify a net voltage into HIGH / LOW / MID using TTL-ish thresholds
    /// (LOW ≤ 0.8 V, HIGH ≥ 2.0 V, MID between). These are deliberately the
    /// classic logic-threshold bands so "low" can never read as 1.48 V again.
    pub fn from_volts(v: f64) -> Level {
        if v >= 2.0 {
            Level::High
        } else if v <= 0.8 {
            Level::Low
        } else {
            Level::Mid
        }
    }

    /// A short ASCII-safe word for the label column.
    pub fn word(self) -> &'static str {
        match self {
            Level::High => "HIGH",
            Level::Low => "LOW",
            Level::Mid => "MID",
        }
    }

    /// A leading glyph: full block for high, mid-dot for mid, low dot for low.
    pub fn glyph(self) -> &'static str {
        match self {
            Level::High => "▇",
            Level::Mid => "◐",
            Level::Low => "·",
        }
    }
}

/// Severity of a finding, ordered worst-first for triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Breaks the board: a copper short, a connectivity failure.
    Serious,
    /// Degrades margin / robustness: medium lint & SI findings.
    Medium,
    /// Dim, informational, but kept visible (the actionable USB-impedance note
    /// is `Info` yet must never be hidden).
    Info,
}

impl Severity {
    /// Parse the `"serious" | "warning" | "note" | "info"` strings the
    /// structured layer emits into our three triage buckets. `"medium"` is
    /// accepted as a defensive alias for `"warning"`: no current caller emits
    /// it, but it keeps the parse robust if the structured layer ever does.
    pub fn from_str(s: &str) -> Severity {
        match s {
            "serious" => Severity::Serious,
            "warning" | "medium" => Severity::Medium,
            _ => Severity::Info, // "note" | "info" | anything else
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Serious => "SERIOUS",
            Severity::Medium => "MEDIUM",
            Severity::Info => "info",
        }
    }
}

/// One triaged finding, flattened from any check family (DRC / SI / lint).
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// Which check produced it ("drc" / "si" / "lint").
    pub check: String,
    /// The specific rule, e.g. "controlled_impedance" or "short".
    pub kind: String,
    /// The one-line expert headline shown in the list.
    pub headline: String,
    /// The plain-language explanation shown in the detail view.
    pub plain: String,
    pub nets: Vec<String>,
    pub refs: Vec<String>,
    pub location_mm: Option<[f64; 2]>,
    pub layer: Option<String>,
    /// A concrete suggested fix, when we have one.
    pub fix: Option<String>,
    /// Whether the user should act on this (true for real findings and for
    /// off-target info notes; the actionable info notes are NEVER hidden).
    pub actionable: bool,
}

impl Finding {
    fn from_json(f: &JsonFinding) -> Finding {
        Finding {
            severity: Severity::from_str(&f.severity),
            check: f.check.clone(),
            kind: f.kind.clone(),
            headline: f.message.clone(),
            plain: f.plain.clone(),
            nets: f.nets.clone(),
            refs: f.refs.clone(),
            location_mm: f.location_mm,
            layer: f.layer.clone(),
            // SI / lint fix text now flows through `JsonFinding.fix` (CORE), so
            // the detail overlay shows the suggested fix on parity with --plain.
            fix: f.fix.clone(),
            actionable: f.actionable,
        }
    }
}

/// Bind status of a part, for the colour/marker in the Nets & Parts list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartStatus {
    /// Resolved to a concrete model (exact match).
    Bound,
    /// Resolved by family / heuristic (lower confidence).
    Family,
    /// Could not be resolved; defaults to OPEN.
    Unresolved,
    /// Deliberately ignored (connector, fiducial, mounting hole).
    Ignored,
}

impl PartStatus {
    /// A full-word status label for the part detail view (the list uses the
    /// compact `UNRES`/`bound`/`family`/`ignore` prefixes).
    pub fn label(self) -> &'static str {
        match self {
            PartStatus::Bound => "bound (exact match)",
            PartStatus::Family => "family / heuristic match",
            PartStatus::Unresolved => "UNRESOLVED (no model — defaults to OPEN)",
            PartStatus::Ignored => "ignored (connector / mechanical)",
        }
    }
}

/// One part in the left-pane list.
#[derive(Debug, Clone)]
pub struct Part {
    pub reference: String,
    pub value: String,
    pub status: PartStatus,
    /// What it became (the bind outcome label), for the detail line.
    pub became: String,
    /// True for an MCU or an active IC (U/IC/MCU prefix), marked in the list.
    pub active_ic: bool,
    /// True when this is an UNRESOLVED active IC sitting on the live circuit:
    /// the "critical, open" case the honesty layer cares about.
    pub critical_open: bool,
}

/// One net in the left-pane list, with a DC voltage when we have one.
#[derive(Debug, Clone)]
pub struct Net {
    pub name: String,
    /// DC operating-point voltage, when the solver produced one for this net.
    pub voltage_v: Option<f64>,
}

/// A detail view for the selected left-pane row, either a part or a net. This
/// is what the Nets&Parts pane opens on Enter, mirroring the Findings detail so
/// Enter is consistent across panes (the personas hit a dead Enter here).
#[derive(Debug, Clone, PartialEq)]
pub enum LeftDetail {
    Part {
        reference: String,
        value: String,
        /// "UNRESOLVED" / "bound (exact)" / "family" / "ignored".
        status: String,
        /// The bind outcome / model it resolved to.
        became: String,
        active_ic: bool,
        critical_open: bool,
        /// The nets this part's pins connect to (sorted, de-duped).
        nets: Vec<String>,
    },
    Net {
        name: String,
        voltage_v: Option<f64>,
        /// The parts whose pins sit on this net (sorted, de-duped).
        parts: Vec<String>,
    },
}

/// Resolves the flat left-pane selection index into the region it lands in.
/// The list is `parts` followed by `nets`, so an index below `parts.len()` is a
/// part and the rest map onto nets. Keeping this split in one place means the
/// detail line and the detail view can never disagree about where the cursor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftPaneIndex {
    Part(usize),
    Net(usize),
    /// Out of range (no selectable row under the cursor).
    None,
}

/// The maximum number of nets on the scope at once. Probing a fifth net evicts
/// the oldest-probed one (FIFO), so the scope never grows unbounded and stays
/// readable at small terminal heights. A deliberate UI constraint; the
/// `scope_probe_caps_and_evicts_oldest` test mirrors it.
pub const SCOPE_MAX_PROBES: usize = 4;

/// Per-probed-net ring-buffer capacity, in samples. One co-sim chunk = one
/// sample, so at the 5 ms QEMU/Renode cadence this holds ~5 s of history and at
/// the 1 ms in-process AVR cadence ~1 s; older samples are dropped from the
/// front. The pane downsamples this to the (narrow) pane width for display, so a
/// generous buffer costs a little memory but never a wide render.
pub const SCOPE_CAPACITY: usize = 1024;

/// A per-net rolling voltage history for the scope: `(sim_ms, volts)` samples,
/// oldest at the front, capped at [`SCOPE_CAPACITY`]. Pure and terminal-free so
/// the ring-buffer behaviour is unit-testable without a PTY.
#[derive(Debug, Clone, Default)]
pub struct ScopeSeries {
    samples: VecDeque<(f64, f64)>,
}

impl ScopeSeries {
    /// Append one `(sim_ms, volts)` sample, evicting the oldest once the buffer
    /// is at capacity so it never grows past [`SCOPE_CAPACITY`].
    pub fn push(&mut self, sim_ms: f64, volts: f64) {
        self.samples.push_back((sim_ms, volts));
        while self.samples.len() > SCOPE_CAPACITY {
            self.samples.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The most recent voltage, if any sample has been recorded.
    pub fn latest(&self) -> Option<f64> {
        self.samples.back().map(|&(_, v)| v)
    }

    /// The (min, max) voltage across the buffered window, if non-empty.
    pub fn min_max(&self) -> Option<(f64, f64)> {
        let mut it = self.samples.iter().map(|&(_, v)| v);
        let first = it.next()?;
        let (mut lo, mut hi) = (first, first);
        for v in it {
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        Some((lo, hi))
    }

    /// The buffered voltages, oldest→newest, for downsampling / plotting.
    pub fn voltages(&self) -> Vec<f64> {
        self.samples.iter().map(|&(_, v)| v).collect()
    }
}

/// The scope's probe state: which nets are on the scope (in probe order, front =
/// oldest) and their rolling voltage histories. Fed from the existing co-sim
/// stream via [`ScopeState::record`]; entirely terminal-free.
#[derive(Debug, Clone, Default)]
pub struct ScopeState {
    /// Probed net names in probe order, front is the oldest, evicted first when
    /// the cap is hit.
    order: Vec<String>,
    /// Per-net rolling history, keyed by net name. Always in sync with `order`.
    series: HashMap<String, ScopeSeries>,
}

impl ScopeState {
    /// Toggle a net's probe: if already probed, remove it (and drop its history);
    /// otherwise add it, evicting the oldest-probed net when at [`SCOPE_MAX_PROBES`].
    /// Returns `true` if the net is probed after the call, `false` if it was removed.
    pub fn toggle(&mut self, net: &str) -> bool {
        if let Some(pos) = self.order.iter().position(|n| n == net) {
            self.order.remove(pos);
            self.series.remove(net);
            false
        } else {
            if self.order.len() >= SCOPE_MAX_PROBES {
                let evicted = self.order.remove(0);
                self.series.remove(&evicted);
            }
            self.order.push(net.to_string());
            self.series.insert(net.to_string(), ScopeSeries::default());
            true
        }
    }

    pub fn is_probed(&self, net: &str) -> bool {
        self.series.contains_key(net)
    }

    /// The probed net names, in probe order (oldest first).
    pub fn probed(&self) -> &[String] {
        &self.order
    }

    /// The rolling history for a probed net, if any.
    pub fn series(&self, net: &str) -> Option<&ScopeSeries> {
        self.series.get(net)
    }

    /// Record one co-sim sample: for each probed net present in `voltages`, push
    /// `(sim_ms, v)` onto its ring buffer. Nets not currently probed are ignored,
    /// so the scope only ever buffers what the user asked to watch.
    pub fn record(&mut self, sim_ms: f64, voltages: &HashMap<String, f64>) {
        for name in &self.order {
            if let Some(&v) = voltages.get(name) {
                if let Some(s) = self.series.get_mut(name) {
                    s.push(sim_ms, v);
                }
            }
        }
    }

    /// Drop every buffered sample but keep the probe selection, used when a new
    /// co-sim run starts (sim time resets to 0) so an old trace doesn't splice
    /// onto the new one.
    pub fn clear_samples(&mut self) {
        for s in self.series.values_mut() {
            *s = ScopeSeries::default();
        }
    }

    /// True once any probed net has at least one recorded sample.
    pub fn has_any_samples(&self) -> bool {
        self.series.values().any(|s| !s.is_empty())
    }
}

/// Which top-level state the scope pane should render, decided from pure state so
/// the render path is a trivial (and testable) dispatch. The distinction between
/// "waiting for the first sample" and "press r to run" is left to the renderer,
/// which knows whether a co-sim is currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeView {
    /// No MCU/firmware on this board; there are no live signals to scope.
    NoMcu,
    /// An MCU exists but no net is probed yet.
    NoProbes,
    /// Nets are probed but no samples have arrived (run not started, or just begun).
    NoData,
    /// At least one probed net has samples, draw the sparklines.
    Live,
}

/// Reduce `values` to at most `target` points by bucket-averaging, so a long
/// history renders in a narrow pane without dropping the overall shape. Values
/// already within `target` (or a `target` of 0/empty input) are returned as-is.
/// Pure and terminal-free for unit testing the narrow-width downsample.
pub fn downsample(values: &[f64], target: usize) -> Vec<f64> {
    if target == 0 || values.len() <= target {
        return values.to_vec();
    }
    let n = values.len();
    (0..target)
        .map(|b| {
            // Bucket b covers [start, end) of the source, averaged.
            let start = b * n / target;
            let end = ((b + 1) * n / target).max(start + 1).min(n);
            let slice = &values[start..end];
            slice.iter().sum::<f64>() / slice.len() as f64
        })
        .collect()
}

/// Which pane currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Parts,
    Findings,
    Cosim,
    Scope,
}

impl Pane {
    /// Cycle to the next pane (Tab / →).
    pub fn next(self) -> Pane {
        match self {
            Pane::Parts => Pane::Findings,
            Pane::Findings => Pane::Cosim,
            Pane::Cosim => Pane::Scope,
            Pane::Scope => Pane::Parts,
        }
    }

    /// Cycle to the previous pane (Shift-Tab / ←).
    pub fn prev(self) -> Pane {
        match self {
            Pane::Parts => Pane::Scope,
            Pane::Findings => Pane::Parts,
            Pane::Cosim => Pane::Findings,
            Pane::Scope => Pane::Cosim,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Pane::Parts => "Nets & Parts",
            Pane::Findings => "Findings",
            Pane::Cosim => "Co-sim",
            Pane::Scope => "Scope",
        }
    }
}

/// The triage verdict, computed once from the findings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Findings worth attention: serious + medium + actionable info.
    pub worth_attention: usize,
    /// Grouped notes: non-actionable info notes (at-limit clearance etc.).
    pub grouped_notes: usize,
    /// Serious findings (the catastrophic ones).
    pub serious: usize,
}

impl Verdict {
    pub fn from_findings(findings: &[Finding]) -> Verdict {
        let mut v = Verdict::default();
        for f in findings {
            match f.severity {
                Severity::Serious => {
                    v.serious += 1;
                    v.worth_attention += 1;
                }
                Severity::Medium => v.worth_attention += 1,
                Severity::Info => {
                    if f.actionable {
                        v.worth_attention += 1;
                    } else {
                        v.grouped_notes += 1;
                    }
                }
            }
        }
        v
    }

    /// The verdict-first headline for the top of the Findings pane.
    pub fn headline(&self) -> String {
        format!(
            "VERDICT: {} worth attention · {} grouped notes · {} serious",
            self.worth_attention, self.grouped_notes, self.serious
        )
    }
}

/// The whole TUI app state, terminal-free.
#[derive(Debug, Clone)]
pub struct AppState {
    pub board_name: String,
    pub mcu: Option<String>,
    pub backend: Option<String>,
    pub critical_parts_bound: String,

    pub parts: Vec<Part>,
    pub nets: Vec<Net>,
    pub findings: Vec<Finding>,
    pub verdict: Verdict,

    /// Number of UNRESOLVED *active* ICs sitting on the live circuit. When > 0,
    /// the analog results are not trustworthy and the verdict must say so. Taken
    /// straight from the [`BindSummary`] honesty data, never recomputed.
    pub active_unresolved: usize,
    /// The refs of those unresolved active ICs (for the verdict warning line).
    pub active_unresolved_refs: Vec<String>,
    /// True when the board has no MCU/firmware target; the co-sim pane is a
    /// static-analysis-only surface, not a firmware co-sim.
    pub no_mcu: bool,
    /// Set when the requested MCU part was modelled by a less-specific core
    /// (co-sim chip substitution, e.g. an STM32F411 run on the F407 model). The
    /// string is the human-readable substitution note (parallel to the JSON
    /// `cosim.substituted` honesty annotation). `None` => no substitution, or
    /// the substitution detection (Track B `requested_part`) did not fire. The
    /// co-sim pane renders this as a yellow caveat so the user is never silently
    /// shown results from a different chip than the one on the board.
    pub backend_substituted: Option<String>,

    /// ref → connected net names (for the part detail view).
    part_nets: HashMap<String, Vec<String>>,
    /// net name → connected refs (for the net detail view).
    net_parts: HashMap<String, Vec<String>>,

    pub focus: Pane,
    pub parts_sel: usize,
    pub findings_sel: usize,
    /// When true, the detail overlay for the selected finding is open.
    pub detail_open: bool,
    /// When true, the detail overlay for the selected left-pane part/net is open.
    pub left_detail_open: bool,
    /// Set when the user has pressed `q` and the event loop should exit.
    pub should_quit: bool,
    /// The one-line launch banner pointing at the other report surfaces is shown
    /// at the top until the first keypress dismisses it (see [`TUI_LAUNCH_BANNER`]).
    pub banner_dismissed: bool,
    /// The scope: which nets are probed (`p` from the Nets & Parts list) and
    /// their rolling voltage history, fed from the co-sim stream. Read-mostly,
    /// the only mutation is the probe toggle plus sample recording.
    pub scope: ScopeState,
}

/// The TUI launch banner: one line pointing a first-time user at the non-TUI
/// report surfaces they might not know exist. Shown until any keypress dismisses
/// it. Kept as a const so the first-run text is testable without a PTY.
pub const TUI_LAUNCH_BANNER: &str =
    "Report modes: --check for everything static · --plain for prose · serve for the browser  (any key dismisses)";

impl AppState {
    /// Build the model from the structured-honest result. The caller passes the
    /// SAME values the `--json` paths produce, so the TUI can never disagree
    /// with the machine surface.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        board_name: String,
        report: &BindReport,
        summary: &BindSummary,
        drc: &DrcStructured,
        si: &[JsonFinding],
        lint: &[JsonFinding],
        nets: Vec<Net>,
        part_nets: HashMap<String, Vec<String>>,
        net_parts: HashMap<String, Vec<String>>,
    ) -> AppState {
        let mut findings = Vec::new();
        // On an unvalidated KiCad-10 board the shorts may be phantom; surface the
        // caveat as an info finding so the (downgraded) shorts are not read as real.
        if let Some(w) = &drc.version_warning {
            findings.push(Finding {
                severity: Severity::Info,
                check: "drc".to_string(),
                kind: "unreliable_version".to_string(),
                headline: "Copper short results are UNRELIABLE on this KiCad version".to_string(),
                plain: w.clone(),
                nets: Vec::new(),
                refs: Vec::new(),
                location_mm: None,
                layer: None,
                actionable: true,
                fix: Some(
                    "Cross-check the copper with KiCad's own DRC; hauksbee does not yet read this \
                     KiCad version's zone fills."
                        .to_string(),
                ),
            });
        }
        // DRC: shorts are serious; below-rule clearance groups are medium;
        // at-limit groups are info (grouped, not 42 identical rows).
        for s in &drc.shorts {
            findings.push(Finding {
                // Reads the structured severity (downgraded to a note on an
                // unvalidated KiCad-10 board where the short may be phantom).
                severity: Severity::from_str(&s.severity),
                check: "drc".to_string(),
                kind: "short".to_string(),
                headline: format!(
                    "{} touches {} on {} (gap {:.4} mm)",
                    s.net_a, s.net_b, s.layer, s.gap_mm
                ),
                plain: format!(
                    "Copper of net {} is touching net {} on layer {}. This is a dead short \
                     — the two nets are electrically joined where they should be separate.",
                    s.net_a, s.net_b, s.layer
                ),
                nets: vec![s.net_a.clone(), s.net_b.clone()],
                refs: Vec::new(),
                location_mm: Some(s.loc_mm),
                layer: Some(s.layer.clone()),
                fix: Some(
                    "Separate the two nets at this location (re-route or widen the gap)."
                        .to_string(),
                ),
                actionable: true,
            });
        }
        for g in &drc.violations {
            findings.push(Finding {
                severity: Severity::Medium,
                check: "drc".to_string(),
                kind: "clearance".to_string(),
                headline: g.label(),
                plain: format!(
                    "{} location(s) where {} and {} sit closer than the {:.3} mm clearance \
                     rule on {}.",
                    g.count, g.net_a, g.net_b, g.rule_mm, g.layer
                ),
                nets: vec![g.net_a.clone(), g.net_b.clone()],
                refs: Vec::new(),
                location_mm: None,
                layer: Some(g.layer.clone()),
                fix: Some("Widen the spacing or move the traces apart.".to_string()),
                actionable: true,
            });
        }
        for g in &drc.at_limit {
            findings.push(Finding {
                severity: Severity::Info,
                check: "drc".to_string(),
                kind: "at_limit".to_string(),
                headline: g.label(),
                plain: format!(
                    "{} location(s) where {} and {} sit exactly at the {:.3} mm minimum \
                     clearance on {} — no margin, but not below the rule.",
                    g.count, g.net_a, g.net_b, g.rule_mm, g.layer
                ),
                nets: vec![g.net_a.clone(), g.net_b.clone()],
                refs: Vec::new(),
                location_mm: None,
                layer: Some(g.layer.clone()),
                fix: None,
                // Grouped, non-actionable noise, collapsed into the notes count.
                actionable: false,
            });
        }
        for f in si {
            findings.push(Finding::from_json(f));
        }
        for f in lint {
            findings.push(Finding::from_json(f));
        }

        // Sort worst-first (serious, then medium, then actionable info, then the
        // grouped notes), so the triage is verdict-first.
        findings.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then(b.actionable.cmp(&a.actionable))
        });

        let verdict = Verdict::from_findings(&findings);
        let parts = parts_from_report(report, summary);

        // MCU / backend from the first MCU row in the report.
        let mcu = report
            .rows
            .iter()
            .find(|r| matches!(r.outcome, BindOutcome::Mcu { .. }))
            .map(|r| {
                if r.value.is_empty() {
                    r.reference.clone()
                } else {
                    format!("{} ({})", r.reference, r.value)
                }
            });
        let backend = report.rows.iter().find_map(|r| match &r.outcome {
            BindOutcome::Mcu { backend } => Some(backend.clone()),
            _ => None,
        });

        // Honesty data: unresolved *active* ICs on the live circuit. Reused from
        // the BindSummary, never recomputed, so the TUI verdict can't diverge
        // from the machine surface.
        let active_unresolved_refs: Vec<String> = summary
            .active_path_unresolved
            .iter()
            .filter(|u| u.active_ic)
            .map(|u| u.reference.clone())
            .collect();
        let active_unresolved = active_unresolved_refs.len();
        let no_mcu = mcu.is_none();

        AppState {
            board_name,
            mcu,
            backend,
            critical_parts_bound: summary.critical_parts_bound.clone(),
            parts,
            nets,
            findings,
            verdict,
            active_unresolved,
            active_unresolved_refs,
            no_mcu,
            // Populated by the caller (Track B) once the requested-vs-modelled
            // part comparison is available; no substitution known at build time.
            backend_substituted: None,
            part_nets,
            net_parts,
            focus: Pane::Findings,
            parts_sel: 0,
            findings_sel: 0,
            detail_open: false,
            left_detail_open: false,
            should_quit: false,
            banner_dismissed: false,
            scope: ScopeState::default(),
        }
    }

    /// Dismiss the launch banner (idempotent). Called on the first keypress so the
    /// hint clears the moment the user starts interacting, without stealing the key.
    pub fn dismiss_banner(&mut self) {
        self.banner_dismissed = true;
    }

    /// Record that the requested MCU part was modelled by a less-specific core
    /// (co-sim chip substitution). `message` is the human-readable note the
    /// co-sim pane renders as a yellow caveat. Mirrors the JSON
    /// `cosim.substituted` annotation so the TUI never silently presents results
    /// from a different chip than the board's.
    pub fn set_chip_substitution(&mut self, message: impl Into<String>) {
        self.backend_substituted = Some(message.into());
    }

    // ── Navigation (the unit-testable core) ──────────────────────────────────

    /// Number of selectable rows in the left (Parts+Nets) list.
    pub fn left_len(&self) -> usize {
        self.parts.len() + self.nets.len()
    }

    /// Move the selection down within the focused pane.
    pub fn select_down(&mut self) {
        match self.focus {
            Pane::Parts => {
                let n = self.left_len();
                if n > 0 && self.parts_sel + 1 < n {
                    self.parts_sel += 1;
                }
            }
            Pane::Findings => {
                if !self.findings.is_empty() && self.findings_sel + 1 < self.findings.len() {
                    self.findings_sel += 1;
                }
            }
            // Co-sim and Scope are display-only: nothing to select-navigate.
            Pane::Cosim | Pane::Scope => {}
        }
    }

    /// Move the selection up within the focused pane.
    pub fn select_up(&mut self) {
        match self.focus {
            Pane::Parts => self.parts_sel = self.parts_sel.saturating_sub(1),
            Pane::Findings => self.findings_sel = self.findings_sel.saturating_sub(1),
            Pane::Cosim | Pane::Scope => {}
        }
    }

    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }
    pub fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
    }

    /// The currently-selected finding, if the Findings pane has a selection.
    pub fn selected_finding(&self) -> Option<&Finding> {
        self.findings.get(self.findings_sel)
    }

    /// Resolve the flat left-pane selection (`parts_sel`) into the part or net
    /// region it points at. The single source of truth for the parts/nets split.
    fn left_pane_index(&self) -> LeftPaneIndex {
        if self.parts_sel < self.parts.len() {
            LeftPaneIndex::Part(self.parts_sel)
        } else {
            let net_idx = self.parts_sel - self.parts.len();
            if net_idx < self.nets.len() {
                LeftPaneIndex::Net(net_idx)
            } else {
                LeftPaneIndex::None
            }
        }
    }

    /// The detail line for the currently-selected left-pane row (a Part or a
    /// Net). Returns an empty string when nothing is selected.
    pub fn left_detail(&self) -> String {
        match self.left_pane_index() {
            LeftPaneIndex::Part(i) => {
                let p = &self.parts[i];
                let mark = if p.critical_open {
                    " [CRITICAL, OPEN]"
                } else if p.active_ic {
                    " [active IC]"
                } else {
                    ""
                };
                format!("{} {} — {}{}", p.reference, p.value, p.became, mark)
            }
            LeftPaneIndex::Net(i) => {
                let n = &self.nets[i];
                match n.voltage_v {
                    Some(v) => format!("net {} — DC {v:.3} V", n.name),
                    None => format!("net {} — (no DC voltage)", n.name),
                }
            }
            LeftPaneIndex::None => String::new(),
        }
    }

    /// Toggle the finding detail overlay (Enter on the Findings pane).
    pub fn toggle_detail(&mut self) {
        if self.focus == Pane::Findings && !self.findings.is_empty() {
            self.detail_open = !self.detail_open;
        }
    }

    /// True when any detail overlay is open (finding OR part/net).
    pub fn any_overlay_open(&self) -> bool {
        self.detail_open || self.left_detail_open
    }

    /// Close every open overlay. Returns true if one was actually open (so the
    /// caller can swallow the key that triggered the close).
    pub fn close_overlays(&mut self) -> bool {
        let was_open = self.any_overlay_open();
        self.detail_open = false;
        self.left_detail_open = false;
        was_open
    }

    /// Handle Enter: open the right detail for the focused pane. On Findings it
    /// toggles the finding detail; on Nets&Parts it opens the part/net detail
    /// (the previously-dead Enter the personas reported). Co-sim has no detail.
    pub fn activate(&mut self) {
        match self.focus {
            Pane::Findings => self.toggle_detail(),
            Pane::Parts => self.toggle_left_detail(),
            Pane::Cosim | Pane::Scope => {}
        }
    }

    /// The net name the Nets & Parts cursor is currently on, but only when the
    /// Parts pane is focused and the selection sits on a net row (not a part).
    /// This is what `p` probes, probing is scoped to "a net from the parts/nets
    /// list", so a part row or any other focused pane yields `None`.
    pub fn selected_probe_net(&self) -> Option<&str> {
        if self.focus != Pane::Parts {
            return None;
        }
        match self.left_pane_index() {
            LeftPaneIndex::Net(i) => Some(self.nets[i].name.as_str()),
            _ => None,
        }
    }

    /// Handle `p`: toggle the highlighted net onto/off the scope. No-op (returns
    /// `None`) unless the Parts pane is focused with a net row selected. Returns
    /// the toggled net name so the caller/tests can confirm what happened.
    pub fn toggle_probe_selected(&mut self) -> Option<String> {
        let name = self.selected_probe_net()?.to_string();
        self.scope.toggle(&name);
        Some(name)
    }

    /// Which top-level state the scope pane is in, decided from pure state. See
    /// [`ScopeView`].
    pub fn scope_view(&self) -> ScopeView {
        if self.no_mcu {
            ScopeView::NoMcu
        } else if self.scope.probed().is_empty() {
            ScopeView::NoProbes
        } else if !self.scope.has_any_samples() {
            ScopeView::NoData
        } else {
            ScopeView::Live
        }
    }

    /// Toggle the part/net detail overlay (Enter on the Nets&Parts pane). Only
    /// opens when there is a real selection to describe.
    pub fn toggle_left_detail(&mut self) {
        if self.focus == Pane::Parts && self.left_detail_view().is_some() {
            self.left_detail_open = !self.left_detail_open;
        }
    }

    /// Build the structured detail for the currently-selected left-pane row.
    /// Returns `None` when the selection is on the "── nets ──" boundary or out
    /// of range.
    pub fn left_detail_view(&self) -> Option<LeftDetail> {
        match self.left_pane_index() {
            LeftPaneIndex::Part(i) => {
                let p = &self.parts[i];
                let mut nets = self
                    .part_nets
                    .get(&p.reference)
                    .cloned()
                    .unwrap_or_default();
                nets.sort();
                nets.dedup();
                Some(LeftDetail::Part {
                    reference: p.reference.clone(),
                    value: p.value.clone(),
                    status: p.status.label().to_string(),
                    became: p.became.clone(),
                    active_ic: p.active_ic,
                    critical_open: p.critical_open,
                    nets,
                })
            }
            LeftPaneIndex::Net(i) => {
                let n = &self.nets[i];
                let mut parts = self.net_parts.get(&n.name).cloned().unwrap_or_default();
                parts.sort();
                parts.dedup();
                Some(LeftDetail::Net {
                    name: n.name.clone(),
                    voltage_v: n.voltage_v,
                    parts,
                })
            }
            LeftPaneIndex::None => None,
        }
    }

    /// The honesty warning for the verdict pane when active ICs are unresolved.
    /// `None` when every active IC is resolved. Reuses the BindSummary count,
    /// never recomputed.
    pub fn unresolved_warning(&self) -> Option<String> {
        if self.active_unresolved == 0 {
            return None;
        }
        // `active_unresolved == active_unresolved_refs.len()`, so the refs list
        // is always non-empty in this branch.
        Some(format!(
            "⚠ {} active part(s) unresolved ({}) → analog results NOT trustworthy",
            self.active_unresolved,
            self.active_unresolved_refs.join(", ")
        ))
    }
}

/// "Active IC" = a reference whose alpha prefix is U / IC / MCU, matching the
/// binder's MCU-candidate convention (and `result::is_active_ic_ref`).
fn is_active_ic_ref(reference: &str) -> bool {
    // The alpha prefix, ASCII-uppercased, must equal one of the active-IC words.
    // Compare against each candidate by byte length + uppercased-byte equality
    // so we never allocate a `String` (this runs once per report row).
    let prefix: &[u8] = {
        let bytes = reference.as_bytes();
        let len = bytes.iter().take_while(|b| b.is_ascii_alphabetic()).count();
        &bytes[..len]
    };
    ["U", "IC", "MCU"].iter().any(|word| {
        let word = word.as_bytes();
        prefix.len() == word.len()
            && prefix
                .iter()
                .zip(word)
                .all(|(p, w)| p.to_ascii_uppercase() == *w)
    })
}

/// Flatten the bind report into the left-pane part list, marking active ICs and
/// the critical-open ones the honesty layer flagged.
fn parts_from_report(report: &BindReport, summary: &BindSummary) -> Vec<Part> {
    use hauksbee_models::Confidence;
    // Refs the binder flagged as unresolved active ICs on the live circuit.
    let critical_open: std::collections::HashSet<&str> = summary
        .active_path_unresolved
        .iter()
        .filter(|u| u.active_ic)
        .map(|u| u.reference.as_str())
        .collect();

    let mut parts: Vec<Part> = report
        .rows
        .iter()
        .map(|r| {
            let status = match &r.outcome {
                BindOutcome::Unresolved { .. } => PartStatus::Unresolved,
                BindOutcome::Skipped { .. } => PartStatus::Ignored,
                _ if r.confidence == Confidence::Exact => PartStatus::Bound,
                _ => PartStatus::Family,
            };
            let active_ic = matches!(r.outcome, BindOutcome::Mcu { .. })
                || is_active_ic_ref(&r.reference);
            Part {
                reference: r.reference.clone(),
                value: r.value.clone(),
                status,
                became: r.outcome.label(),
                active_ic,
                critical_open: critical_open.contains(r.reference.as_str()),
            }
        })
        .collect();

    // Sort the most important parts to the top: critical-open first, then other
    // unresolved, then active ICs, then everything else, then ignored, so the
    // engineer's eye lands on what matters. Lower rank sorts higher.
    const RANK_CRITICAL_OPEN: u8 = 0;
    const RANK_UNRESOLVED: u8 = 1;
    const RANK_ACTIVE_IC: u8 = 2;
    const RANK_NORMAL: u8 = 3;
    const RANK_IGNORED: u8 = 4;
    parts.sort_by_key(|p| {
        if p.critical_open {
            RANK_CRITICAL_OPEN
        } else if p.status == PartStatus::Unresolved {
            RANK_UNRESOLVED
        } else if p.active_ic {
            RANK_ACTIVE_IC
        } else if p.status == PartStatus::Ignored {
            RANK_IGNORED
        } else {
            RANK_NORMAL
        }
    });
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{BindOutcome, BindReport, BindRow};
    use hauksbee_models::Confidence;

    fn row(reference: &str, outcome: BindOutcome, conf: Confidence, warning: Option<&str>) -> BindRow {
        BindRow {
            reference: reference.to_string(),
            value: String::new(),
            model_id: None,
            confidence: conf,
            outcome,
            warning: warning.map(|s| s.to_string()),
            guesses: Vec::new(),
        }
    }

    fn jf(severity: &str, check: &str, actionable: bool, msg: &str) -> JsonFinding {
        JsonFinding {
            check: check.to_string(),
            kind: "k".to_string(),
            severity: severity.to_string(),
            nets: vec!["/N".to_string()],
            location_mm: None,
            layer: None,
            refs: Vec::new(),
            actionable,
            message: msg.to_string(),
            plain: msg.to_string(),
            fix: None,
        }
    }

    fn empty_drc() -> DrcStructured {
        DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 0,
            shorts: Vec::new(),
            violations: Vec::new(),
            at_limit: Vec::new(),
            version_warning: None,
        }
    }

    #[test]
    fn severity_parses_into_three_buckets() {
        assert_eq!(Severity::from_str("serious"), Severity::Serious);
        assert_eq!(Severity::from_str("warning"), Severity::Medium);
        assert_eq!(Severity::from_str("note"), Severity::Info);
        assert_eq!(Severity::from_str("info"), Severity::Info);
    }

    #[test]
    fn verdict_counts_actionable_info_as_worth_attention() {
        let findings = vec![
            Finding {
                severity: Severity::Serious,
                check: "drc".into(),
                kind: "short".into(),
                headline: "short".into(),
                plain: String::new(),
                nets: vec![],
                refs: vec![],
                location_mm: None,
                layer: None,
                fix: None,
                actionable: true,
            },
            Finding {
                severity: Severity::Info,
                check: "si".into(),
                kind: "controlled_impedance".into(),
                headline: "171 ohm".into(),
                plain: String::new(),
                nets: vec![],
                refs: vec![],
                location_mm: None,
                layer: None,
                fix: None,
                actionable: true, // the USB-impedance note
            },
            Finding {
                severity: Severity::Info,
                check: "drc".into(),
                kind: "at_limit".into(),
                headline: "at limit".into(),
                plain: String::new(),
                nets: vec![],
                refs: vec![],
                location_mm: None,
                layer: None,
                fix: None,
                actionable: false, // grouped note
            },
        ];
        let v = Verdict::from_findings(&findings);
        assert_eq!(v.serious, 1);
        assert_eq!(v.worth_attention, 2, "serious + actionable info");
        assert_eq!(v.grouped_notes, 1, "non-actionable info is a grouped note");
        assert!(v.headline().contains("2 worth attention"));
        assert!(v.headline().contains("1 serious"));
    }

    fn sample_state() -> AppState {
        let mut report = BindReport::default();
        report.push(row(
            "U2",
            BindOutcome::Mcu { backend: "renode:stm32f103".into() },
            Confidence::Exact,
            None,
        ));
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, Confidence::Exact, None));
        report.push(row(
            "U7",
            BindOutcome::Unresolved { reason: "no model".into() },
            Confidence::Unresolved,
            Some("on connected net(s)"),
        ));
        let summary = BindSummary::from_report(&report);
        let si = vec![jf("info", "si", true, "USB pair ~171 ohm from target")];
        let lint = vec![jf("serious", "lint", true, "floating EN")];
        let nets = vec![
            Net { name: "/3V3".into(), voltage_v: Some(3.3) },
            Net { name: "/USB_D+".into(), voltage_v: None },
        ];
        // U7 connects /3V3 and /USB_D+; U2 connects /3V3.
        let mut part_nets: HashMap<String, Vec<String>> = HashMap::new();
        part_nets.insert("U7".into(), vec!["/3V3".into(), "/USB_D+".into()]);
        part_nets.insert("U2".into(), vec!["/3V3".into()]);
        let mut net_parts: HashMap<String, Vec<String>> = HashMap::new();
        net_parts.insert("/3V3".into(), vec!["U2".into(), "U7".into()]);
        net_parts.insert("/USB_D+".into(), vec!["U7".into()]);
        AppState::new(
            "Demo".into(),
            &report,
            &summary,
            &empty_drc(),
            &si,
            &lint,
            nets,
            part_nets,
            net_parts,
        )
    }

    #[test]
    fn model_pulls_mcu_backend_and_critical_metric() {
        let st = sample_state();
        assert_eq!(st.mcu.as_deref(), Some("U2"));
        assert_eq!(st.backend.as_deref(), Some("renode:stm32f103"));
        // U2 is the only active IC and it bound; U7 counts toward total -> 1/2.
        assert_eq!(st.critical_parts_bound, "1/2");
    }

    #[test]
    fn launch_banner_shows_then_dismisses() {
        let mut st = sample_state();
        // Fresh state shows the banner; the first keypress dismisses it for good.
        assert!(!st.banner_dismissed, "banner is shown on launch");
        st.dismiss_banner();
        assert!(st.banner_dismissed, "first keypress dismisses the banner");
        st.dismiss_banner(); // idempotent
        assert!(st.banner_dismissed);
        // The banner points at all three non-TUI report surfaces.
        assert!(TUI_LAUNCH_BANNER.contains("--check"));
        assert!(TUI_LAUNCH_BANNER.contains("--plain"));
        assert!(TUI_LAUNCH_BANNER.contains("serve"));
    }

    #[test]
    fn findings_sorted_serious_first() {
        let st = sample_state();
        assert!(!st.findings.is_empty());
        assert_eq!(st.findings[0].severity, Severity::Serious);
    }

    #[test]
    fn critical_open_part_sorts_to_top_and_is_marked() {
        let st = sample_state();
        assert_eq!(st.parts[0].reference, "U7", "unresolved active IC first");
        assert!(st.parts[0].critical_open);
        assert_eq!(st.parts[0].status, PartStatus::Unresolved);
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let mut st = sample_state();
        st.focus = Pane::Findings;
        // Up at the top is a no-op.
        st.select_up();
        assert_eq!(st.findings_sel, 0);
        // Down walks but never past the end.
        for _ in 0..100 {
            st.select_down();
        }
        assert_eq!(st.findings_sel, st.findings.len() - 1);
    }

    #[test]
    fn left_pane_navigates_parts_then_nets_with_dc_detail() {
        let mut st = sample_state();
        st.focus = Pane::Parts;
        // First row is a part.
        assert!(st.left_detail().starts_with("U7"));
        // Walk to the first net row.
        for _ in 0..st.parts.len() {
            st.select_down();
        }
        let detail = st.left_detail();
        assert!(detail.contains("net /3V3"), "detail: {detail}");
        assert!(detail.contains("3.300 V"), "DC voltage shown: {detail}");
    }

    #[test]
    fn pane_focus_cycles_both_directions() {
        let mut st = sample_state();
        st.focus = Pane::Parts;
        st.focus_next();
        assert_eq!(st.focus, Pane::Findings);
        st.focus_next();
        assert_eq!(st.focus, Pane::Cosim);
        st.focus_next();
        assert_eq!(st.focus, Pane::Scope);
        st.focus_next();
        assert_eq!(st.focus, Pane::Parts, "Scope wraps back to Parts");
        // Backwards: Parts → Scope → Cosim.
        st.focus_prev();
        assert_eq!(st.focus, Pane::Scope);
        st.focus_prev();
        assert_eq!(st.focus, Pane::Cosim);
    }

    // ── W6 §2: scope probe + ring buffer + downsample ────────────────────────

    #[test]
    fn scope_probe_caps_and_evicts_oldest() {
        let mut sc = ScopeState::default();
        // Probe five nets; the cap is four, so the first-probed is evicted.
        for n in ["/A", "/B", "/C", "/D"] {
            assert!(sc.toggle(n), "{n} is now probed");
        }
        assert_eq!(sc.probed(), &["/A", "/B", "/C", "/D"]);
        assert!(sc.toggle("/E"), "/E is now probed");
        assert_eq!(sc.probed().len(), SCOPE_MAX_PROBES);
        assert_eq!(sc.probed(), &["/B", "/C", "/D", "/E"], "oldest (/A) evicted");
        assert!(!sc.is_probed("/A"));
        // Toggling a probed net off removes it (and frees a slot).
        assert!(!sc.toggle("/C"), "/C toggled off");
        assert!(!sc.is_probed("/C"));
        assert_eq!(sc.probed(), &["/B", "/D", "/E"]);
    }

    #[test]
    fn scope_series_ring_buffer_caps_and_tracks_stats() {
        let mut s = ScopeSeries::default();
        // Push more than capacity; the oldest samples drop off the front.
        for i in 0..(SCOPE_CAPACITY + 10) {
            s.push(i as f64, i as f64);
        }
        assert_eq!(s.len(), SCOPE_CAPACITY, "buffer never exceeds capacity");
        // Latest is the last pushed; the window is the most-recent CAPACITY samples.
        assert_eq!(s.latest(), Some((SCOPE_CAPACITY + 9) as f64));
        let (lo, hi) = s.min_max().unwrap();
        assert_eq!(hi, (SCOPE_CAPACITY + 9) as f64);
        assert_eq!(lo, 10.0, "the first ten samples were evicted");
    }

    #[test]
    fn scope_records_only_probed_nets() {
        let mut sc = ScopeState::default();
        sc.toggle("/LED");
        // A sample carrying several nets; only the probed one is buffered.
        let mut v = HashMap::new();
        v.insert("/LED".to_string(), 3.3);
        v.insert("/GND".to_string(), 0.0);
        v.insert("/UNPROBED".to_string(), 1.0);
        sc.record(0.0, &v);
        v.insert("/LED".to_string(), 0.0);
        sc.record(5.0, &v);
        assert!(sc.has_any_samples());
        let led = sc.series("/LED").unwrap();
        assert_eq!(led.len(), 2);
        assert_eq!(led.latest(), Some(0.0));
        assert_eq!(led.min_max(), Some((0.0, 3.3)));
        assert!(sc.series("/GND").is_none(), "unprobed nets are never buffered");
        // clear_samples keeps the probe but drops the history.
        sc.clear_samples();
        assert!(sc.is_probed("/LED"));
        assert!(!sc.has_any_samples());
    }

    #[test]
    fn downsample_narrows_and_preserves_range() {
        // Fewer-or-equal-than-target passes through unchanged.
        assert_eq!(downsample(&[1.0, 2.0, 3.0], 5), vec![1.0, 2.0, 3.0]);
        assert_eq!(downsample(&[], 4), Vec::<f64>::new());
        assert_eq!(downsample(&[1.0, 2.0], 0), vec![1.0, 2.0]);
        // A ramp of 100 samples into 10 buckets: monotonic, spanning the range.
        let ramp: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let ds = downsample(&ramp, 10);
        assert_eq!(ds.len(), 10);
        assert!(ds.windows(2).all(|w| w[0] < w[1]), "shape preserved (monotonic)");
        assert!(ds[0] < 10.0 && *ds.last().unwrap() > 89.0, "spans the range");
    }

    #[test]
    fn toggle_probe_selected_requires_parts_focus_and_a_net_row() {
        let mut st = sample_state();
        // Findings focus: `p` is a no-op even though a net is under the cursor.
        st.focus = Pane::Findings;
        assert_eq!(st.toggle_probe_selected(), None);
        // Parts focus, cursor on a PART row (first row is the active IC U7): no-op.
        st.focus = Pane::Parts;
        st.parts_sel = 0;
        assert_eq!(st.toggle_probe_selected(), None, "a part row is not probeable");
        // Move onto the first net row and probe it.
        st.parts_sel = st.parts.len();
        let net = st.toggle_probe_selected().expect("net row is probeable");
        assert_eq!(net, "/3V3");
        assert!(st.scope.is_probed("/3V3"));
        // Toggling again removes it.
        assert_eq!(st.toggle_probe_selected().as_deref(), Some("/3V3"));
        assert!(!st.scope.is_probed("/3V3"));
    }

    #[test]
    fn scope_view_transitions_across_placeholder_states() {
        // No-MCU board → NoMcu, regardless of probes.
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, Confidence::Exact, None));
        let summary = BindSummary::from_report(&report);
        let no_mcu = AppState::new(
            "Analog".into(), &report, &summary, &empty_drc(), &[], &[], vec![],
            HashMap::new(), HashMap::new(),
        );
        assert_eq!(no_mcu.scope_view(), ScopeView::NoMcu);

        // MCU board: NoProbes → NoData → Live as we probe then feed samples.
        let mut st = sample_state();
        assert!(!st.no_mcu);
        assert_eq!(st.scope_view(), ScopeView::NoProbes);
        st.scope.toggle("/3V3");
        assert_eq!(st.scope_view(), ScopeView::NoData, "probed but no samples yet");
        let mut v = HashMap::new();
        v.insert("/3V3".to_string(), 3.3);
        st.scope.record(0.0, &v);
        assert_eq!(st.scope_view(), ScopeView::Live);
    }

    #[test]
    fn detail_overlay_only_toggles_on_findings_pane() {
        let mut st = sample_state();
        st.focus = Pane::Parts;
        st.toggle_detail();
        assert!(!st.detail_open, "no overlay from the parts pane");
        st.focus = Pane::Findings;
        st.toggle_detail();
        assert!(st.detail_open);
        st.toggle_detail();
        assert!(!st.detail_open);
    }

    // ── Bug #1: Enter on a Nets&Parts item opens a part/net detail ───────────

    #[test]
    fn enter_on_parts_pane_opens_part_detail_with_nets() {
        let mut st = sample_state();
        st.focus = Pane::Parts;
        // First row is the unresolved active IC U7.
        st.activate();
        assert!(st.left_detail_open, "Enter on Parts opens the part detail");
        let detail = st.left_detail_view().expect("a detail for the selection");
        match detail {
            LeftDetail::Part { reference, critical_open, nets, .. } => {
                assert_eq!(reference, "U7");
                assert!(critical_open, "U7 is an unresolved active IC");
                assert_eq!(nets, vec!["/3V3".to_string(), "/USB_D+".to_string()]);
            }
            other => panic!("expected a Part detail, got {other:?}"),
        }
        // Enter again toggles it closed.
        st.activate();
        assert!(!st.left_detail_open);
    }

    #[test]
    fn enter_on_a_net_row_opens_net_detail_with_parts_and_voltage() {
        let mut st = sample_state();
        st.focus = Pane::Parts;
        // Walk past the parts into the nets region; first net is /3V3.
        for _ in 0..st.parts.len() {
            st.select_down();
        }
        st.activate();
        assert!(st.left_detail_open);
        match st.left_detail_view().unwrap() {
            LeftDetail::Net { name, voltage_v, parts } => {
                assert_eq!(name, "/3V3");
                assert_eq!(voltage_v, Some(3.3));
                assert_eq!(parts, vec!["U2".to_string(), "U7".to_string()]);
            }
            other => panic!("expected a Net detail, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_findings_pane_still_opens_finding_detail_not_left() {
        let mut st = sample_state();
        st.focus = Pane::Findings;
        st.activate();
        assert!(st.detail_open, "Findings Enter opens the finding detail");
        assert!(!st.left_detail_open, "not the left detail");
    }

    // ── Bug #2: overlay close / Escape handling ──────────────────────────────

    #[test]
    fn close_overlays_clears_both_and_reports_open() {
        let mut st = sample_state();
        st.focus = Pane::Findings;
        st.toggle_detail();
        assert!(st.any_overlay_open());
        assert!(st.close_overlays(), "reports that an overlay was open");
        assert!(!st.any_overlay_open());
        // Closing again reports nothing was open (so a top-level Esc is a no-op).
        assert!(!st.close_overlays());
    }

    // ── #6: level-label thresholds ───────────────────────────────────────────

    #[test]
    fn level_thresholds_are_voltage_based_not_drive_based() {
        // 1.48 V (the persona's /PWR_LED_K) must NOT read as low.
        assert_eq!(Level::from_volts(1.48), Level::Mid);
        assert_eq!(Level::from_volts(0.0), Level::Low);
        assert_eq!(Level::from_volts(0.8), Level::Low);
        assert_eq!(Level::from_volts(0.81), Level::Mid);
        assert_eq!(Level::from_volts(1.99), Level::Mid);
        assert_eq!(Level::from_volts(2.0), Level::High);
        assert_eq!(Level::from_volts(3.3), Level::High);
        assert_eq!(Level::from_volts(1.48).word(), "MID");
    }

    // ── #7: unresolved-active-IC warning surfaces in the verdict ─────────────

    #[test]
    fn unresolved_active_ic_surfaces_a_verdict_warning() {
        let st = sample_state();
        // U7 is the unresolved active IC in the sample.
        assert_eq!(st.active_unresolved, 1);
        let w = st.unresolved_warning().expect("a warning for the open IC");
        assert!(w.contains("1 active part"), "{w}");
        assert!(w.contains("U7"), "{w}");
        assert!(w.to_lowercase().contains("not trustworthy"), "{w}");
    }

    #[test]
    fn no_warning_when_all_active_ics_resolved() {
        let mut report = BindReport::default();
        report.push(row(
            "U1",
            BindOutcome::Mcu { backend: "renode:stm32f103".into() },
            Confidence::Exact,
            None,
        ));
        let summary = BindSummary::from_report(&report);
        let st = AppState::new(
            "Clean".into(),
            &report,
            &summary,
            &empty_drc(),
            &[],
            &[],
            vec![],
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(st.active_unresolved, 0);
        assert!(st.unresolved_warning().is_none());
    }

    // ── #8: no-MCU board is flagged for the co-sim pane ──────────────────────

    #[test]
    fn no_mcu_board_sets_the_no_mcu_flag() {
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, Confidence::Exact, None));
        let summary = BindSummary::from_report(&report);
        let st = AppState::new(
            "Analog".into(),
            &report,
            &summary,
            &empty_drc(),
            &[],
            &[],
            vec![],
            HashMap::new(),
            HashMap::new(),
        );
        assert!(st.no_mcu, "a board with no MCU row sets no_mcu");
        assert!(st.mcu.is_none());
    }
}
