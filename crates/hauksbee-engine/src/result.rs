//! The structured-honest result layer.
//!
//! Every static check (`--si`/`--drc`/`--lint`/`--report`/`--thermal`/`--ac`)
//! historically rendered straight to a Unicode table or to a plain-language
//! verdict. That produced two failure modes the personas hit hard:
//!
//! 1. **Silent sentinels.** An AC sweep with no signal path prints 121 rows of
//!    `-6000 dB`; a thermal table with no resolved dissipating devices prints
//!    one resistor. Both *look like data* and are not. (Theme A.)
//! 2. **False-comfort metrics.** The bind summary reports a flat "83% resolved"
//!    while the entire active power circuit is open. (Theme F.)
//!
//! This module is the single structured representation that the text, plain, and
//! JSON renderers all read from, so they can never disagree (the same discipline
//! as [`crate::plain`], extended with machine-readable validity + honest bind
//! roles). It does **not** re-run or weaken any check: it consumes the existing
//! [`BindReport`](crate::report::BindReport), [`DrcReport`](hauksbee_extract::DrcReport),
//! AC bode points, and thermal peaks, and adds the honesty annotations
//! (`valid`/`reason`, `critical_parts_bound`, grouped DRC) on top.
//!
//! ## Exit-code contract (see ONBOARDING_PLAN §4.2)
//! - `0`  clean, or findings present but not `--strict`.
//! - `2`  `--strict` and at least one gating finding.
//! - `3`  **board invalid for the requested analysis** (AC with no signal path,
//!        thermal with no resolved dissipating devices). A run that could not
//!        produce a meaningful answer must never exit 0.

use serde::Serialize;

use hauksbee_extract::{DrcReport, ViolationKind};

use crate::report::{BindOutcome, BindReport};

/// Distinct process exit code for "the board is invalid for the analysis you
/// asked for" — a meaningless result, not a clean one. Kept here so the CLI and
/// any future caller share one source of truth.
pub const EXIT_INVALID_FOR_ANALYSIS: i32 = 3;

// ─────────────────────────────────────────────────────────────────────────────
// Bind summary by ROLE (Fix #5 / Theme F)
// ─────────────────────────────────────────────────────────────────────────────

/// One unresolved part that sits on a connected (active) net, with the
/// electrical consequence of defaulting it to open.
#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedActive {
    pub reference: String,
    pub value: String,
    /// Why it could not be bound (e.g. "no model; left open").
    pub reason: String,
    /// What leaving it open does to the analysis, in one plain line.
    pub consequence: String,
    /// True when this part is an active IC (reference prefix U/IC/MCU): the kind
    /// of part whose absence makes analog/AC/thermal results untrustworthy.
    pub active_ic: bool,
}

/// The bind report, summarised by role rather than a single flat percentage.
///
/// "How many parts resolved" is the wrong unit when the 17% that did not are the
/// entire active circuit. The honest metrics are: did the MCU and the active
/// power ICs bind, and which active-path parts are open?
#[derive(Debug, Clone, Serialize)]
pub struct BindSummary {
    pub resolved: usize,
    pub unresolved: usize,
    pub non_ignored: usize,
    /// `"M/N"` — active ICs (MCU + U/IC-prefixed parts) that bound, over the
    /// total active ICs on the board. The metric, not a bare percentage.
    pub critical_parts_bound: String,
    pub critical_parts_bound_n: usize,
    pub critical_parts_total: usize,
    /// True when at least one MCU bound.
    pub mcu_bound: bool,
    /// Unresolved parts on connected nets (the active path), each annotated.
    pub active_path_unresolved: Vec<UnresolvedActive>,
    /// RESOLVED parts (incl. MCUs bound as `BindOutcome::Mcu`) that nonetheless
    /// carry an open-pin warning while sitting on a live circuit. These are not
    /// in `active_path_unresolved` (their outcome is not `Unresolved`), but an
    /// active IC here still fails to drive its nets, so it must be surfaced as a
    /// coverage caveat. Never empty implies untrustworthy thermal/AC results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_but_open_active: Vec<UnresolvedActive>,
}

impl BindSummary {
    /// Build the role-aware summary from a raw [`BindReport`].
    ///
    /// "Active IC" = a reference whose alpha prefix is `U`/`IC`/`MCU`, matching
    /// the binder's own MCU-candidate heuristic ([`crate::binder`]). "Active
    /// path" = an unresolved part that raised the binder's connected-net warning
    /// (its pins touch a real node), so its open default actually changes the
    /// result.
    pub fn from_report(report: &BindReport) -> Self {
        let mut active_path_unresolved = Vec::new();
        let mut resolved_but_open_active = Vec::new();
        let mut critical_total = 0usize;
        let mut critical_bound = 0usize;
        let mut mcu_bound = false;

        for row in &report.rows {
            let is_mcu = matches!(row.outcome, BindOutcome::Mcu { .. });
            if is_mcu {
                mcu_bound = true;
            }
            let active_ic = is_mcu || is_active_ic_ref(&row.reference);
            let unresolved = matches!(row.outcome, BindOutcome::Unresolved { .. });

            // A part counts toward the critical denominator if it is an active IC
            // (resolved or not) or an MCU. Ignored parts (connectors, fiducials)
            // never count.
            if active_ic && !row.outcome.is_ignored() {
                critical_total += 1;
                if !unresolved {
                    critical_bound += 1;
                }
            }

            // Active-path unresolved: the binder already decided this part is on
            // a connected net (it set `warning`). Annotate the consequence.
            if unresolved && row.warning.is_some() {
                let reason = match &row.outcome {
                    BindOutcome::Unresolved { reason } => reason.clone(),
                    _ => String::new(),
                };
                let consequence = if active_ic {
                    format!(
                        "{} is an active IC left OPEN; analog/AC/thermal results on its nets are NOT trustworthy",
                        row.reference
                    )
                } else {
                    format!(
                        "{} defaults to OPEN; nets through it are isolated in simulation",
                        row.reference
                    )
                };
                active_path_unresolved.push(UnresolvedActive {
                    reference: row.reference.clone(),
                    value: row.value.clone(),
                    reason,
                    consequence,
                    active_ic,
                });
            }

            // Resolved-but-open: an MCU (BindOutcome::Mcu) or any other resolved
            // (non-ignored, non-unresolved) part that still carries a GENUINE
            // open/undriven-pin warning is on the live circuit but not driving its
            // nets. These escape the unresolved walk above, so a parallel bucket
            // records them. We only care about ACTIVE IC refs (a passive with a
            // dangling pin does not invalidate a thermal/AC sweep the way an open
            // driver does). CRUCIAL: an `[auto-bind] ... GPIO map ...` note is a
            // pin-NAME-derivation limitation, NOT an open circuit — a working MCU
            // carries it routinely. Treating it as "open/undriven" cried wolf on
            // healthy boards (df-pill, stm32-multiprotocol), so it is excluded.
            let resolved_open = !unresolved
                && !row.outcome.is_ignored()
                && active_ic
                && row.warning.as_deref().is_some_and(is_open_pin_warning);
            if resolved_open {
                resolved_but_open_active.push(UnresolvedActive {
                    reference: row.reference.clone(),
                    value: row.value.clone(),
                    reason: row.warning.clone().unwrap_or_default(),
                    consequence: format!(
                        "{} is a resolved active IC with open/undriven pins on the live circuit; analog/AC/thermal results on its nets are NOT fully trustworthy",
                        row.reference
                    ),
                    active_ic,
                });
            }
        }

        BindSummary {
            resolved: report.resolved_count(),
            unresolved: report.non_ignored_count() - report.resolved_count(),
            non_ignored: report.non_ignored_count(),
            critical_parts_bound: format!("{critical_bound}/{critical_total}"),
            critical_parts_bound_n: critical_bound,
            critical_parts_total: critical_total,
            mcu_bound,
            active_path_unresolved,
            resolved_but_open_active,
        }
    }

    /// Whether any active IC on the live circuit is RESOLVED but had an open
    /// pin warning raised against it (e.g. an MCU bound as `BindOutcome::Mcu`
    /// whose every I/O pin was `open_warning`'d, or a resolved analog part with
    /// a dangling pin). These escape [`active_ics_unresolved`] because their
    /// outcome is not `Unresolved`, yet the part still does not drive its nets —
    /// so a thermal/AC result over those nets is just as untrustworthy. We walk
    /// `Mcu`/`Resolved`-style rows that carry a `warning` and name an active IC.
    pub fn active_open_on_live_circuit(&self) -> bool {
        self.resolved_but_open_active
            .iter()
            .any(|u| u.active_ic)
    }

    /// Whether any active IC that actually sits on the live circuit is
    /// unresolved — the condition that makes analog/AC/thermal results
    /// untrustworthy and should WARN instead of reporting "ok".
    ///
    /// We deliberately key this on `active_path_unresolved` (unresolved active
    /// ICs the binder flagged as being on a CONNECTED net), NOT on the raw
    /// `critical_parts_bound_n < critical_parts_total` count. An unresolved IC
    /// whose every pin is on a floating/placeholder net cannot affect the
    /// result, so it must not trigger a false "invalid" verdict (a board that
    /// genuinely runs cool would otherwise be wrongly declared invalid). The
    /// `critical_parts_bound` ratio is still reported in the banner for audit.
    pub fn active_ics_unresolved(&self) -> bool {
        self.active_path_unresolved.iter().any(|u| u.active_ic)
    }

    /// The unresolved active ICs on the live circuit (those flagged
    /// `active_ic`), in report order. The shared driver list behind both the
    /// "no signal path" reason and the empty-thermal-table reason.
    fn active_ic_unresolved(&self) -> impl Iterator<Item = &UnresolvedActive> {
        self.active_path_unresolved.iter().filter(|u| u.active_ic)
    }

    /// Render the honest summary banner that augments the bind table. Leads with
    /// the role metric, then WARNS (loudly) when the active circuit is open, then
    /// lists each affected active-path part.
    pub fn render_banner(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let mcu_state = if self.mcu_bound {
            "MCU bound"
        } else if self.critical_parts_total == 0 {
            "no active ICs on board"
        } else {
            "no MCU bound"
        };
        let _ = writeln!(
            s,
            "\nbind summary: {} of {} non-ignored parts resolved; critical_parts_bound: {} ({})",
            self.resolved, self.non_ignored, self.critical_parts_bound, mcu_state,
        );
        if self.active_ics_unresolved() {
            let _ = writeln!(
                s,
                "WARNING: active part(s) unresolved — analog/AC/thermal results on their nets are NOT trustworthy."
            );
        }
        if !self.active_path_unresolved.is_empty() {
            let _ = writeln!(s, "active_path_unresolved ({}):", self.active_path_unresolved.len());
            for u in &self.active_path_unresolved {
                let _ = writeln!(s, "  {} ({}): {}", u.reference, u.value, u.consequence);
            }
        }
        s
    }
}

/// Whether a reference designator names an active IC (prefix U / IC / MCU),
/// matching the binder's MCU-candidate convention.
fn is_active_ic_ref(reference: &str) -> bool {
    let prefix: String = reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    matches!(prefix.as_str(), "U" | "IC" | "MCU")
}

/// Whether a resolved row's `warning` indicates a GENUINE open/undriven-pin
/// condition (which makes thermal/AC over its nets untrustworthy) as opposed to
/// an auto-bind GPIO-map note. The binder routinely attaches an
/// `[auto-bind] ... GPIO map derived/cannot be derived ...` note to a perfectly
/// working MCU when it infers (or fails to infer) the pin map from schematic pin
/// names; that is a naming limitation, not an open circuit, and must NOT mark the
/// part as untrustworthy on the live circuit.
fn is_open_pin_warning(warning: &str) -> bool {
    !warning.contains("[auto-bind]") && !warning.contains("GPIO map")
}

// ─────────────────────────────────────────────────────────────────────────────
// AC validity (Fix #1 / Theme A)
// ─────────────────────────────────────────────────────────────────────────────

/// The magnitude floor in [`hauksbee_solve`]'s `bode()`: `20*log10(1e-300)`
/// clamps to exactly `-6000.0 dB` when a node has no signal path. We treat every
/// point being at (or below) this floor as "no path to this net".
pub const AC_FLOOR_DB: f64 = -6000.0;

/// Whether an AC sweep (the `(freq, mag_db, phase)` triples for one net) is the
/// all-sentinel "no signal path" result. Empty input is also meaningless.
pub fn ac_is_all_sentinel(bode: &[(f64, f64, f64)]) -> bool {
    !bode.is_empty() && bode.iter().all(|(_, db, _)| *db <= AC_FLOOR_DB + 1.0)
}

/// A validity verdict for an analysis that can be meaningless: AC and thermal.
#[derive(Debug, Clone, Serialize)]
pub struct Validity {
    pub valid: bool,
    /// Set only when `valid` is false: the named reason, listing offending refs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Validity {
    pub fn valid() -> Self {
        Validity {
            valid: true,
            reason: None,
        }
    }
    pub fn invalid(reason: impl Into<String>) -> Self {
        Validity {
            valid: false,
            reason: Some(reason.into()),
        }
    }
}

/// Build the "no signal path" reason naming the unresolved driving ICs. Used by
/// both AC (no path) and the warning text.
pub fn no_signal_path_reason(net: &str, summary: &BindSummary) -> String {
    let drivers: Vec<String> = summary
        .active_ic_unresolved()
        .map(|u| {
            if u.value.is_empty() {
                u.reference.clone()
            } else {
                format!("{} ({})", u.reference, u.value)
            }
        })
        .collect();
    if drivers.is_empty() {
        format!("no signal path to {net}: the driving devices on this net are open or unresolved")
    } else {
        format!(
            "no signal path to {net}; unresolved driving ICs: {}",
            drivers.join(", ")
        )
    }
}

/// Thermal validity: invalid when no resolved dissipating device made the table
/// *and* the board has unresolved active parts that would have dissipated. The
/// `dissipating_rows` is the count of devices that produced a finite peak temp.
pub fn thermal_validity(dissipating_rows: usize, summary: &BindSummary) -> Validity {
    if dissipating_rows > 0 {
        return Validity::valid();
    }
    // Zero rows. If there are unresolved active ICs ON THE LIVE CIRCUIT, the
    // table is empty because the dissipating devices are open, not because the
    // board runs cool. (active_ics_unresolved is keyed on connected-net ICs, so
    // refs is always non-empty here.)
    if summary.active_ics_unresolved() {
        let refs: Vec<String> = summary
            .active_ic_unresolved()
            .map(|u| u.reference.clone())
            .collect();
        Validity::invalid(format!(
            "thermal table empty: no resolved dissipating devices; unresolved driving ICs: {}",
            refs.join(", ")
        ))
    } else {
        // Genuinely no dissipating devices and nothing unresolved — a valid (if
        // boring) result, e.g. a passive-only board at ambient.
        Validity::valid()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-check COVERAGE (distinct from Validity) — the honest "N of M" annotation
// ─────────────────────────────────────────────────────────────────────────────

/// Coverage is distinct from [`Validity`]. `Validity` stays BINARY and drives
/// exit 3. `CheckCoverage` is the honest "N of M active parts actually entered
/// this check" metric: it is ALWAYS emittable and NEVER changes the exit code on
/// its own (the partial-coverage thermal escalation is opt-in behind
/// `--strict-thermal`). `partial == true` means a renderer should print a
/// coverage caveat even though the check itself produced rows.
#[derive(Debug, Clone, Serialize)]
pub struct CheckCoverage {
    /// Covered active ICs / total active ICs, clamped to `[0, 1]`: how much of the
    /// active circuit the check actually modelled. `1.0` when every active IC is
    /// covered (or there are none — vacuously complete); low when power ICs are
    /// open/unresolved.
    pub resolved_fraction: f64,
    /// Devices that actually produced a row in the check's table.
    pub dissipating_count: usize,
    /// Active ICs on the live circuit that the check should have covered.
    pub total_active_count: usize,
    /// Active ICs left open on the live circuit (unresolved + resolved-but-open).
    pub open_active_on_live_circuit: usize,
    /// True => the renderer should surface a coverage caveat. Set when at least
    /// one row exists but an active IC on the live circuit is open/unresolved.
    pub partial: bool,
}

/// Thermal coverage, parallel to [`thermal_validity`] but NON-gating. Returns
/// `partial = true` when there ARE dissipating rows yet an active IC on the live
/// circuit is open or unresolved — i.e. the table is real but incomplete (rows
/// exist only because some passives/parts resolved while a power IC is open).
/// `thermal_validity` itself is unchanged; this is the honest companion metric.
pub fn thermal_coverage(dissipating_rows: usize, summary: &BindSummary) -> CheckCoverage {
    let open_unresolved = summary
        .active_path_unresolved
        .iter()
        .filter(|u| u.active_ic)
        .count();
    let open_resolved = summary
        .resolved_but_open_active
        .iter()
        .filter(|u| u.active_ic)
        .count();
    let open_active = open_unresolved + open_resolved;
    let total_active = summary.critical_parts_total;
    // resolved_fraction is COVERED active ICs / total active ICs — NOT dissipating
    // rows / active ICs. Dividing the (mostly-passive) dissipating-row count by the
    // active-IC count is meaningless and inverts the signal: it read 1.0 on a board
    // with every power IC open (40 passive rows / 7 ICs, clamped) and 0.0 on a clean
    // board with one IC and no separate dissipating rows. A consumer thresholding
    // resolved_fraction would then pass the worst board and flag the good one.
    let resolved_fraction = if total_active == 0 {
        1.0
    } else {
        let covered_active = total_active.saturating_sub(open_active);
        (covered_active as f64 / total_active as f64).clamp(0.0, 1.0)
    };
    let partial = dissipating_rows > 0
        && (summary.active_ics_unresolved() || summary.active_open_on_live_circuit());
    CheckCoverage {
        resolved_fraction,
        dissipating_count: dissipating_rows,
        total_active_count: total_active,
        open_active_on_live_circuit: open_active,
        partial,
    }
}

/// The active ICs (unresolved + resolved-but-open) on the live circuit that a
/// coverage caveat should name. Shared by the CLI text/plain/json renderers so
/// every surface names the SAME parts.
pub fn coverage_open_active_refs(summary: &BindSummary) -> Vec<String> {
    summary
        .active_path_unresolved
        .iter()
        .filter(|u| u.active_ic)
        .chain(summary.resolved_but_open_active.iter().filter(|u| u.active_ic))
        .map(|u| u.reference.clone())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Info-level notes + machine-readable co-sim summary
// ─────────────────────────────────────────────────────────────────────────────

/// An info-level, never-`--strict`, always-emitted note. Distinct from
/// [`JsonFinding`]: a note never gates a CI pipeline and never changes an exit
/// code; it is the structured home for honesty annotations that must never be
/// silently absent (bind roles, co-sim substitution, coverage caveats, SI info).
#[derive(Debug, Clone, Serialize)]
pub struct JsonNote {
    pub kind: JsonNoteKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonNoteKind {
    BindRole,
    CosimSubstitution,
    Coverage,
    SiInfo,
}

/// Machine-readable co-sim summary (today only emitted as CLI text). Populated
/// from the headless run + the MCU binding. `substituted` is true when the part
/// the user asked for was modelled by a less-specific core (Track B).
#[derive(Debug, Clone, Serialize)]
pub struct CosimJson {
    pub mcu_ref: String,
    /// The actual backend string from `BindOutcome::Mcu { backend }`.
    pub backend: String,
    /// The part the board asked for (threaded through `McuBinding`, Track B).
    pub requested_part: String,
    /// True when the requested part != the modelled core.
    pub substituted: bool,
    /// Total net toggles seen during the run. `0` => stalled/quiet.
    pub total_toggles: u64,
    pub uart_seen: bool,
    /// Top-N nets by activity: name, toggle count, observed min/max voltage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_summary: Vec<NetActivity>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetActivity {
    pub net: String,
    pub toggles: u64,
    pub v_min: f64,
    pub v_max: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// DRC grouping (Fix #8 / Theme D)
// ─────────────────────────────────────────────────────────────────────────────

/// A real short between two nets (gap <= 0: touching copper).
#[derive(Debug, Clone, Serialize)]
pub struct DrcShort {
    pub net_a: String,
    pub net_b: String,
    pub layer: String,
    pub gap_mm: f64,
    pub loc_mm: [f64; 2],
    /// Always "serious" for a short; carried for the uniform finding shape.
    pub severity: String,
}

/// A group of clearance findings that share (net_a, net_b, layer, root cause),
/// collapsed to one line with a count. `at_limit` separates `gap == rule` (no
/// margin, but not below) from genuine sub-clearance violations.
#[derive(Debug, Clone, Serialize)]
pub struct DrcGroup {
    pub net_a: String,
    pub net_b: String,
    pub layer: String,
    pub count: usize,
    /// How many of `count` are genuinely below the rule (gap < rule). The rest
    /// are exactly at the rule (no margin, not below).
    pub below_count: usize,
    /// True when every member sits exactly at the rule (gap == rule, no margin),
    /// false when at least one is genuinely *below* the rule.
    pub at_limit: bool,
    /// The tightest gap in the group (mm).
    pub min_gap_mm: f64,
    /// The clearance rule for this pair (mm).
    pub rule_mm: f64,
}

impl DrcGroup {
    /// The honest one-line label. `gap == rule` is "exactly at minimum clearance
    /// (no margin)" — NOT "below the spacing the board asks for" (which is only
    /// true when gap < rule). For a mixed group we name BOTH counts so we never
    /// overstate how many locations are actually below the rule.
    pub fn label(&self) -> String {
        let loc = |n: usize| format!("{n} location{}", if n == 1 { "" } else { "s" });
        if self.at_limit {
            format!(
                "{} vs {}: {} on {}, all exactly at minimum clearance (no margin) [{:.3} mm]",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                self.rule_mm
            )
        } else if self.below_count == self.count {
            format!(
                "{} vs {}: {} on {}, below the {:.3} mm clearance rule (tightest {:.3} mm)",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                self.rule_mm,
                self.min_gap_mm
            )
        } else {
            // Mixed: some below, the remainder exactly at the limit.
            format!(
                "{} vs {}: {} on {} ({} below the {:.3} mm rule, tightest {:.3} mm; {} at the limit)",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                self.below_count,
                self.rule_mm,
                self.min_gap_mm,
                self.count - self.below_count,
            )
        }
    }
}

/// The DRC report, restructured into honest buckets: real shorts kept verbatim,
/// clearance findings grouped, and `gap == rule` separated from `gap < rule`.
#[derive(Debug, Clone, Serialize)]
pub struct DrcStructured {
    pub clearance_rule_mm: f64,
    pub primitive_count: usize,
    pub shorts: Vec<DrcShort>,
    /// Grouped clearance violations that are genuinely BELOW the rule.
    pub violations: Vec<DrcGroup>,
    /// Grouped findings that sit exactly AT the rule (no margin, not below).
    pub at_limit: Vec<DrcGroup>,
}

impl DrcStructured {
    pub fn from_report(report: &DrcReport) -> Self {
        let mut shorts = Vec::new();
        // Group clearance findings by (net_a, net_b, layer). Within a group we
        // split at_limit (gap >= rule, i.e. exactly at or, defensively, above)
        // from below-rule. A KiCad clearance finding has positive gap; "at limit"
        // means gap is within a hair of the rule.
        use std::collections::BTreeMap;
        // key -> (count, below_count, min_gap, rule)
        let mut groups: BTreeMap<(String, String, String), (usize, usize, f64, f64)> =
            BTreeMap::new();

        for f in &report.findings {
            match f.kind {
                ViolationKind::Short => shorts.push(DrcShort {
                    net_a: f.net_a_name.clone(),
                    net_b: f.net_b_name.clone(),
                    layer: f.layer.clone(),
                    gap_mm: f.gap_mm,
                    loc_mm: [f.x, f.y],
                    severity: "serious".to_string(),
                }),
                ViolationKind::Clearance => {
                    // A finding is "below" the rule when its gap is under it;
                    // "at limit" when gap == rule. The 1e-4 mm (100 nm) tolerance
                    // absorbs f64 representation noise at rule values like 0.200
                    // mm without misclassifying a genuine sub-micron violation as
                    // "at limit" (a gap 0.5 um short stays "below the rule").
                    let below = f.gap_mm < f.required_clearance_mm - 1e-4;
                    let key = (
                        f.net_a_name.clone(),
                        f.net_b_name.clone(),
                        f.layer.clone(),
                    );
                    let e = groups
                        .entry(key)
                        .or_insert((0, 0, f64::INFINITY, f.required_clearance_mm));
                    e.0 += 1;
                    if below {
                        e.1 += 1;
                    }
                    e.2 = e.2.min(f.gap_mm);
                    e.3 = f.required_clearance_mm;
                }
            }
        }

        let mut violations = Vec::new();
        let mut at_limit = Vec::new();
        for ((net_a, net_b, layer), (count, below_count, min_gap, rule)) in groups {
            let any_below = below_count > 0;
            let group = DrcGroup {
                net_a,
                net_b,
                layer,
                count,
                below_count,
                at_limit: !any_below,
                min_gap_mm: if min_gap.is_finite() { min_gap } else { rule },
                rule_mm: rule,
            };
            if any_below {
                violations.push(group);
            } else {
                at_limit.push(group);
            }
        }

        DrcStructured {
            clearance_rule_mm: report.clearance_mm,
            primitive_count: report.primitive_count,
            shorts,
            violations,
            at_limit,
        }
    }

    /// Render the grouped DRC as text (the honest, de-duplicated view). Shorts
    /// first (the things that actually break a board), then below-rule groups,
    /// then the at-limit bucket (separated and labelled correctly).
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "DRC: {} primitive(s), clearance rule {:.3} mm",
            self.primitive_count, self.clearance_rule_mm
        );
        if self.shorts.is_empty() && self.violations.is_empty() && self.at_limit.is_empty() {
            let _ = writeln!(s, "no shorts or clearance violations.");
            return s;
        }
        if !self.shorts.is_empty() {
            let _ = writeln!(s, "\nSHORTS ({}):", self.shorts.len());
            for sh in &self.shorts {
                let _ = writeln!(
                    s,
                    "  [SERIOUS] {} touches {} on {} (gap {:.4} mm) at x={:.1}, y={:.1}",
                    sh.net_a, sh.net_b, sh.layer, sh.gap_mm, sh.loc_mm[0], sh.loc_mm[1]
                );
            }
        }
        if !self.violations.is_empty() {
            let _ = writeln!(s, "\nCLEARANCE VIOLATIONS (below rule), grouped:");
            for g in &self.violations {
                let _ = writeln!(s, "  {}", g.label());
            }
        }
        if !self.at_limit.is_empty() {
            let _ = writeln!(s, "\nAT MINIMUM CLEARANCE (no margin), grouped:");
            for g in &self.at_limit {
                let _ = writeln!(s, "  {}", g.label());
            }
        }
        let _ = writeln!(
            s,
            "\n{} short(s), {} below-rule group(s), {} at-limit group(s).",
            self.shorts.len(),
            self.violations.len(),
            self.at_limit.len()
        );
        s
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Machine-readable JSON (Fix #6 / §4.1) — one structured surface for every check
// ─────────────────────────────────────────────────────────────────────────────

use hauksbee_extract::{LintCheck, NetLintReport, Severity, SiCheck, SiReport, SiSeverity};

/// One finding in the uniform machine-readable shape (§4.1): every check's
/// findings serialize the same way, so a CI pipeline or AI never parses prose.
#[derive(Debug, Clone, Serialize)]
pub struct JsonFinding {
    /// Which check produced it ("si" / "lint" / "drc").
    pub check: String,
    /// The specific rule (e.g. "controlled_impedance", "strap_pin").
    pub kind: String,
    /// "serious" | "warning" | "note" | "info".
    pub severity: String,
    pub nets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_mm: Option<[f64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub refs: Vec<String>,
    /// Whether a user should act on this (true for real findings and for
    /// off-target info notes; false for within-tolerance observations).
    pub actionable: bool,
    /// The expert one-line message.
    pub message: String,
    /// The same finding in plain language (best-effort; equals `message` when no
    /// dedicated plain template applies).
    pub plain: String,
    /// A concise suggested fix, when a dedicated template applies. Closes the
    /// TUI "no fix text" gap: once `JsonFinding` carries it, `Finding::from_json`
    /// can read it instead of hard-coding `None`. Omitted (None) when no fix
    /// template applies to this finding kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn si_sev_str(s: SiSeverity) -> &'static str {
    match s {
        SiSeverity::High => "serious",
        SiSeverity::Medium => "warning",
        SiSeverity::Low => "note",
        SiSeverity::Info => "info",
    }
}

fn lint_sev_str(s: Severity) -> &'static str {
    match s {
        Severity::High => "serious",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// Whether an SI info note is actionable (off-target deviation, not "ok"/"no
/// judgement"). Mirrors the plain-mode promotion rule so JSON and `--plain`
/// agree on what counts as actionable.
fn si_info_actionable(message: &str) -> bool {
    message.contains("from target") && !message.contains("within")
}

/// Convert an [`SiReport`] (findings AND info notes) to uniform JSON findings.
/// Info notes are INCLUDED in JSON even though `--plain` suppresses most of them
/// (§4.1: "Info notes appear in JSON even when suppressed from --plain text").
pub fn si_findings_json(report: &SiReport) -> Vec<JsonFinding> {
    report
        .findings
        .iter()
        .map(|f| {
            let actionable = if f.severity == SiSeverity::Info {
                si_info_actionable(&f.message)
            } else {
                true
            };
            JsonFinding {
                check: "si".to_string(),
                kind: f.check.as_str().to_string(),
                severity: si_sev_str(f.severity).to_string(),
                nets: f.nets.clone(),
                location_mm: None,
                layer: None,
                refs: f.refs.clone(),
                actionable,
                message: f.message.clone(),
                plain: si_plain_hint(f.check).to_string(),
                fix: si_fix_hint(f.check).map(str::to_string),
            }
        })
        .collect()
}

/// Convert a USB-C CC compliance report to a uniform JSON finding for the
/// `--check` aggregate, so the aggregate stays a single valid JSON document
/// (the standalone `--usb-c --json` keeps the richer dedicated shape). `None`
/// for an `Ok` verdict (nothing to report).
pub fn usbc_finding_json(report: &crate::checks::usb_c::UsbcReport) -> Option<JsonFinding> {
    use crate::checks::usb_c::UsbcLevel;
    let (severity, actionable) = match report.level {
        UsbcLevel::Ok => return None,
        UsbcLevel::Serious => ("serious", true),
        UsbcLevel::Info => ("info", false),
    };
    Some(JsonFinding {
        check: "usb_c".to_string(),
        kind: "cc_compliance".to_string(),
        severity: severity.to_string(),
        nets: Vec::new(),
        location_mm: None,
        layer: None,
        refs: report.receptacles.iter().map(|r| r.reference.clone()).collect(),
        actionable,
        message: report.headline.clone(),
        plain: report.headline.clone(),
        fix: None,
    })
}

/// Convert a [`NetLintReport`] to uniform JSON findings.
pub fn lint_findings_json(report: &NetLintReport) -> Vec<JsonFinding> {
    report
        .findings
        .iter()
        .map(|f| JsonFinding {
            check: "lint".to_string(),
            kind: f.check.as_str().to_string(),
            severity: lint_sev_str(f.severity).to_string(),
            nets: f.nets.clone(),
            location_mm: None,
            layer: None,
            refs: f.refs.clone(),
            actionable: true,
            message: f.message.clone(),
            plain: f.message.clone(),
            fix: lint_fix_hint(f.check).map(str::to_string),
        })
        .collect()
}

/// The top-level `--json` document. Only the section(s) for the requested check
/// are populated; the rest stay `None` (omitted from output). `board` + `bind`
/// are always present so an AI always has the bind-role context (Theme F).
#[derive(Debug, Clone, Serialize)]
pub struct JsonReport {
    pub board: String,
    pub bind: BindSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<JsonFinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drc: Option<DrcStructured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ac: Option<AcJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalJson>,
    /// Info-level notes (bind roles, co-sim substitution, coverage caveats, SI
    /// info) that must never be silently absent but never gate a CI pipeline.
    /// Additive + `skip_serializing_if` so the schema stays backward-compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<JsonNote>,
    /// Machine-readable co-sim summary, present only on a co-sim run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosim: Option<CosimJson>,
}

/// AC sweep in JSON: validity first, then the bode rows only when valid.
#[derive(Debug, Clone, Serialize)]
pub struct AcJson {
    #[serde(flatten)]
    pub validity: Validity,
    /// Per-net bode rows, present only when valid AND the net has a signal path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nets: Vec<AcNetJson>,
    /// Nets that were requested but had no signal path (all points at the floor)
    /// and so were omitted from `nets`. Empty when the whole sweep is invalid
    /// (the `valid:false` + `reason` carries that case). Listed so a partially
    /// valid sweep never silently drops a net.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub no_signal_path_nets: Vec<String>,
    /// Honest "N of M" coverage annotation, distinct from `validity`. Non-gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CheckCoverage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AcNetJson {
    pub net: String,
    /// `[freq_hz, mag_db, phase_deg]` triples.
    pub points: Vec<[f64; 3]>,
}

/// Thermal in JSON: validity first, then the per-device peak temps when valid.
#[derive(Debug, Clone, Serialize)]
pub struct ThermalJson {
    #[serde(flatten)]
    pub validity: Validity,
    pub ambient_c: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<ThermalDeviceJson>,
    /// Honest "N of M" coverage annotation, distinct from `validity`. Non-gating
    /// by default (`partial:true` => the table is real but some power IC is open).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CheckCoverage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThermalDeviceJson {
    pub reference: String,
    pub tj_c: f64,
    pub over_limit: bool,
}

impl JsonReport {
    /// Start a JSON report with just the always-present board + bind header.
    pub fn new(board_name: &str, bind: BindSummary) -> Self {
        JsonReport {
            board: board_name.to_string(),
            bind,
            findings: None,
            drc: None,
            ac: None,
            thermal: None,
            notes: Vec::new(),
            cosim: None,
        }
    }

    /// Serialize to a pretty JSON string (stable, non-interactive output).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialize: {e}\"}}"))
    }
}

/// Best-effort plain text for an SI finding kind (so JSON `plain` is not just a
/// copy of `message` for the kinds we have templates for). Kept tiny on purpose;
/// the full plain templates live in [`crate::plain`].
pub fn si_plain_hint(check: SiCheck) -> &'static str {
    match check {
        SiCheck::ControlledImpedance => "trace impedance is off the target value",
        SiCheck::UsbDiffPair => "USB data pair is mismatched",
        SiCheck::CrystalLoadCap => "crystal load capacitors may be the wrong value",
        SiCheck::I2cRiseTime => "I2C bus rise time is slow",
        SiCheck::AntennaKeepout => "copper intrudes into the antenna keep-out",
    }
}

/// A concise suggested fix for an SI finding kind, for the machine-readable
/// `fix` field. Kept to a single sentence; the full plain remediation prose
/// lives in [`crate::plain::plain_si`]. `None` when no template applies.
pub fn si_fix_hint(check: SiCheck) -> Option<&'static str> {
    Some(match check {
        SiCheck::ControlledImpedance => {
            "Adjust trace width/spacing for the actual stackup to hit the target impedance, or have the fab build a controlled-impedance stackup."
        }
        SiCheck::UsbDiffPair => {
            "Length-match D+/D- and route them as a tight, parallel, equal-width pair over a continuous reference ground."
        }
        SiCheck::CrystalLoadCap => {
            "Set each load cap to ~2*(crystal load capacitance) minus board stray (a few pF), per the crystal datasheet."
        }
        SiCheck::I2cRiseTime => {
            "Use a stronger (smaller) pull-up, reduce bus capacitance/length, or lower the I2C clock."
        }
        SiCheck::AntennaKeepout => {
            "Clear all copper and ground fill out of the antenna keep-out region and re-route any tracks that cross it."
        }
    })
}

/// A concise suggested fix for a lint finding kind, for the machine-readable
/// `fix` field. `None` when no template applies.
pub fn lint_fix_hint(check: LintCheck) -> Option<&'static str> {
    Some(match check {
        LintCheck::MissingI2cPullup => {
            "Add a 2.2k-10k pull-up from the I2C line to its power rail (one per SDA and SCL)."
        }
        LintCheck::FloatingControlPin => {
            "Tie the pin to a defined level with a pull-up/pull-down per the datasheet, or drive it from a known output."
        }
        LintCheck::LedCurrentSanity => {
            "Re-pick the series resistor for ~1-20 mA: R = (Vsupply - Vf) / I_target."
        }
        LintCheck::OutputContention => {
            "Leave one push-pull driver on the net, or switch to open-drain with a single pull-up."
        }
        LintCheck::StrapPin => {
            "Hold the strap net at the datasheet's required reset level with a pull resistor; keep active signals off it until after boot."
        }
        LintCheck::McuResourceConflict => {
            "Move one function to a pin mapping to a free internal block, or drop one of the two features."
        }
        LintCheck::DesignatorFootprintMismatch => {
            "Make the reference/value and footprint agree, then regenerate the BOM."
        }
        LintCheck::ValuePackageSanity => {
            "Check the value against the package: lower the value, pick a larger package, or correct the value field."
        }
        LintCheck::PlaceholderValue => {
            "Replace the placeholder with the actual passive value before ordering or trusting simulation."
        }
        LintCheck::UncheckedMcu => {
            "Verify the boot/strap pins and pin-mux by hand against the datasheet, or supply a model with --models-dir so the strap and resource-conflict checks can run."
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{DrcFinding, Item, ItemKind};

    fn clearance(net_a: &str, net_b: &str, gap: f64, rule: f64) -> DrcFinding {
        DrcFinding {
            kind: ViolationKind::Clearance,
            net_a: 1,
            net_b: 2,
            net_a_name: net_a.to_string(),
            net_b_name: net_b.to_string(),
            layer: "F.Cu".to_string(),
            x: 10.0,
            y: 20.0,
            gap_mm: gap,
            required_clearance_mm: rule,
            item_a: Item {
                kind: ItemKind::Track,
                net: 1,
                owner: String::new(),
            },
            item_b: Item {
                kind: ItemKind::Track,
                net: 2,
                owner: String::new(),
            },
        }
    }

    use crate::report::{BindRow, BindOutcome};
    use hauksbee_models::Confidence;

    fn row(reference: &str, outcome: BindOutcome, warning: Option<&str>) -> BindRow {
        BindRow {
            reference: reference.to_string(),
            value: String::new(),
            model_id: None,
            confidence: Confidence::Exact,
            outcome,
            warning: warning.map(|s| s.to_string()),
        }
    }

    #[test]
    fn thermal_valid_when_no_connected_active_ic_is_open() {
        // A board with an unresolved IC whose pins are NOT on connected nets
        // (binder set no warning) must NOT be declared thermally invalid: it
        // cannot dissipate, so an empty table is a real "runs cool", not a lie.
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, None));
        report.push(row(
            "U9",
            BindOutcome::Unresolved {
                reason: "no model".into(),
            },
            None, // no warning => not on a connected net
        ));
        let summary = BindSummary::from_report(&report);
        // U9 counts toward critical_total (it's an active IC) but is NOT in the
        // active path, so validity must stay TRUE with an empty table.
        assert!(!summary.active_ics_unresolved());
        assert!(thermal_validity(0, &summary).valid, "isolated open IC -> still valid");
    }

    #[test]
    fn thermal_coverage_partial_when_passive_resolved_but_active_ic_open() {
        // One resolved passive (gives a row) + one unresolved active IC on the
        // live circuit. validity stays VALID (rows > 0) but coverage is PARTIAL:
        // the table understates the load because the power IC is open.
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, None));
        report.push(row(
            "U7",
            BindOutcome::Unresolved {
                reason: "no model".into(),
            },
            Some("unresolved part 'U7' on connected net(s): defaulting to OPEN circuit"),
        ));
        let summary = BindSummary::from_report(&report);
        // validity is unchanged/binary: rows>0 => valid.
        assert!(thermal_validity(1, &summary).valid);
        let cov = thermal_coverage(1, &summary);
        assert!(cov.partial, "one row + open active IC => partial coverage");
        assert_eq!(cov.dissipating_count, 1);
        assert_eq!(cov.open_active_on_live_circuit, 1);
        assert!(coverage_open_active_refs(&summary).contains(&"U7".to_string()));
    }

    #[test]
    fn thermal_coverage_partial_when_resolved_mcu_has_open_pins() {
        // A RESOLVED MCU (BindOutcome::Mcu) whose I/O pins were all open_warning'd
        // escapes active_ics_unresolved, but active_open_on_live_circuit catches
        // it, so coverage is still PARTIAL.
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, None));
        report.push(row(
            "U1",
            BindOutcome::Mcu {
                backend: "renode:stm32f4".into(),
            },
            Some("U1: all I/O pins open (undriven)"),
        ));
        let summary = BindSummary::from_report(&report);
        assert!(!summary.active_ics_unresolved(), "resolved MCU is not 'unresolved'");
        assert!(summary.active_open_on_live_circuit(), "but it IS open on the live circuit");
        let cov = thermal_coverage(1, &summary);
        assert!(cov.partial);
        assert!(coverage_open_active_refs(&summary).contains(&"U1".to_string()));
    }

    #[test]
    fn thermal_coverage_not_partial_for_clean_board() {
        // Passives only, no open active ICs => coverage is complete, not partial.
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, None));
        let summary = BindSummary::from_report(&report);
        let cov = thermal_coverage(1, &summary);
        assert!(!cov.partial);
        assert_eq!(cov.open_active_on_live_circuit, 0);
        assert!(coverage_open_active_refs(&summary).is_empty());
    }

    #[test]
    fn thermal_invalid_when_connected_active_ic_is_open() {
        let mut report = BindReport::default();
        report.push(row(
            "U2",
            BindOutcome::Unresolved {
                reason: "no model".into(),
            },
            Some("unresolved part 'U2' on connected net(s): defaulting to OPEN circuit"),
        ));
        let summary = BindSummary::from_report(&report);
        assert!(summary.active_ics_unresolved());
        let v = thermal_validity(0, &summary);
        assert!(!v.valid);
        assert!(v.reason.unwrap().contains("U2"));
    }

    #[test]
    fn passive_only_board_thermal_is_valid() {
        let mut report = BindReport::default();
        report.push(row("R1", BindOutcome::Analog { device: "R".into() }, None));
        let summary = BindSummary::from_report(&report);
        assert_eq!(summary.critical_parts_total, 0);
        assert!(thermal_validity(0, &summary).valid);
        // Banner names the no-IC case honestly rather than "MCU UNRESOLVED".
        assert!(summary.render_banner().contains("no active ICs on board"));
    }

    #[test]
    fn ac_all_floor_is_sentinel() {
        let bode: Vec<(f64, f64, f64)> = (0..10).map(|i| (i as f64, -6000.0, 0.0)).collect();
        assert!(ac_is_all_sentinel(&bode));
        let real: Vec<(f64, f64, f64)> = vec![(1.0, -3.0, -45.0), (2.0, -6000.0, 0.0)];
        assert!(!ac_is_all_sentinel(&real));
        assert!(!ac_is_all_sentinel(&[]));
    }

    #[test]
    fn drc_at_limit_is_separated_and_labelled_correctly() {
        let mut report = DrcReport {
            clearance_mm: 0.2,
            ..Default::default()
        };
        // 3 findings exactly at the rule (gap == rule) for one pair.
        for _ in 0..3 {
            report.findings.push(clearance("GND", "+3.3V", 0.2, 0.2));
        }
        // 1 genuinely below for another pair.
        report.findings.push(clearance("GND", "+5V", 0.1, 0.2));
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.at_limit.len(), 1, "at-limit group separated");
        assert_eq!(st.at_limit[0].count, 3);
        assert!(st.at_limit[0].label().contains("exactly at minimum clearance (no margin)"));
        assert!(!st.at_limit[0].label().contains("below the"));
        assert_eq!(st.violations.len(), 1, "below-rule group separated");
        assert!(st.violations[0].label().contains("below the"));
    }

    #[test]
    fn drc_mixed_group_does_not_overcount_below() {
        // A group with 2 below-rule + 3 at-limit members must report "2 below"
        // and "3 at the limit", never "5 below" (no crying wolf on count).
        let mut report = DrcReport {
            clearance_mm: 0.2,
            ..Default::default()
        };
        for _ in 0..2 {
            report.findings.push(clearance("GND", "+3.3V", 0.10, 0.2)); // below
        }
        for _ in 0..3 {
            report.findings.push(clearance("GND", "+3.3V", 0.20, 0.2)); // at limit
        }
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.violations.len(), 1, "mixed group lands in violations");
        let g = &st.violations[0];
        assert_eq!(g.count, 5);
        assert_eq!(g.below_count, 2);
        let label = g.label();
        assert!(label.contains("2 below"), "label: {label}");
        assert!(label.contains("3 at the limit"), "label: {label}");
        assert!(!label.contains("5 below"), "must not overcount: {label}");
    }

    #[test]
    fn drc_groups_repeated_findings_with_count() {
        let mut report = DrcReport {
            clearance_mm: 0.2,
            ..Default::default()
        };
        for _ in 0..9 {
            report.findings.push(clearance("GND", "+3.3V", 0.2, 0.2));
        }
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.at_limit.len(), 1, "9 identical findings -> 1 group");
        assert_eq!(st.at_limit[0].count, 9);
        assert!(st.at_limit[0].label().contains("9 locations"));
    }
}
