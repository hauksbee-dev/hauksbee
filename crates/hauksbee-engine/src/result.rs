//! The structured-honest result layer.
//!
//! A static check (`--si`/`--drc`/`--lint`/`--report`/`--thermal`/`--ac`) that
//! renders straight to a Unicode table or to a plain-language verdict opens two
//! failure modes:
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
//! [`BindReport`], [`DrcReport`],
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
/// asked for", a meaningless result, not a clean one. Kept here so the CLI and
/// any future caller share one source of truth.
pub const EXIT_INVALID_FOR_ANALYSIS: i32 = 3;

/// Version of the `run --json` document contract (`JsonReport` plus the
/// `ok`/`verdict`/`serious_count`/`actionable_count` rollup `to_json`
/// prepends). Bump on a breaking change only; additive fields keep it.
pub const RUN_REPORT_SCHEMA_VERSION: u32 = 1;

/// Exit code a strict headless run (`--strict`) or hauksbee-ci must use when the
/// analog co-sim tripped the consecutive-failed-chunk abort. Centralised so both
/// entry points resolve the same code ([`EXIT_INVALID_FOR_ANALYSIS`]) and a test
/// can assert the contract without spawning a process. `None` means the run is
/// analysable and the ordinary pass/fail exit codes apply.
pub fn strict_analog_exit_code(analog_abort_tripped: bool) -> Option<i32> {
    analog_abort_tripped.then_some(EXIT_INVALID_FOR_ANALYSIS)
}

// ─────────────────────────────────────────────────────────────────────────────
// Bind summary by ROLE (Fix #5 / Theme F)
// ─────────────────────────────────────────────────────────────────────────────

/// One unresolved part that sits on a connected (active) net, with the
/// electrical consequence of defaulting it to open.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BindSummary {
    pub resolved: usize,
    pub unresolved: usize,
    pub non_ignored: usize,
    /// `"M/N"`, active ICs (MCU + U/IC-prefixed parts) that bound, over the
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
            // pin-NAME-derivation limitation, NOT an open circuit, a working MCU
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
    /// a dangling pin). These escape [`Self::active_path_unresolved`] because their
    /// outcome is not `Unresolved`, yet the part still does not drive its nets,
    /// so a thermal/AC result over those nets is just as untrustworthy. We walk
    /// `Mcu`/`Resolved`-style rows that carry a `warning` and name an active IC.
    pub fn active_open_on_live_circuit(&self) -> bool {
        self.resolved_but_open_active.iter().any(|u| u.active_ic)
    }

    /// Whether any active IC that actually sits on the live circuit is
    /// unresolved; the condition that makes analog/AC/thermal results
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
            "\nbind summary: {} of {} non-ignored parts resolved; Critical parts modelled: {} of {} ({})",
            self.resolved,
            self.non_ignored,
            self.critical_parts_bound_n,
            self.critical_parts_total,
            mcu_state,
        );
        // Surface the same union the web/json personas do: active ICs that are
        // unresolved OR resolved-but-open on the live circuit both make
        // analog/AC/thermal on their nets untrustworthy, so the banner must warn
        // on either (warn on one alone and a resolved MCU with all I/O pins open
        // slips through).
        if self.active_ics_unresolved() || self.active_open_on_live_circuit() {
            let _ = writeln!(
                s,
                "WARNING: active part(s) unresolved or left open on the live circuit; \
                 analog/AC/thermal results on their nets are NOT trustworthy."
            );
        }
        // ONE grouped section per bucket. The table above already marks each
        // affected row, so this section groups the refs and states the shared
        // consequence once, instead of repeating a near-identical sentence per
        // part. Natural refdes order, so the list is scannable and stable.
        if !self.active_path_unresolved.is_empty() {
            let _ = writeln!(
                s,
                "Active parts left open ({}): {}",
                self.active_path_unresolved.len(),
                grouped_refs(&self.active_path_unresolved),
            );
            let actives = self
                .active_path_unresolved
                .iter()
                .filter(|u| u.active_ic)
                .count();
            if actives > 0 {
                let _ = writeln!(
                    s,
                    "  {} of these are active ICs left OPEN: analog/AC/thermal results on \
                     their nets are NOT trustworthy.",
                    actives
                );
            }
            if actives < self.active_path_unresolved.len() {
                let _ = writeln!(
                    s,
                    "  The rest default to OPEN: nets through them are isolated in simulation."
                );
            }
        }
        let open_resolved: Vec<UnresolvedActive> = self
            .resolved_but_open_active
            .iter()
            .filter(|u| u.active_ic)
            .cloned()
            .collect();
        if !open_resolved.is_empty() {
            let _ = writeln!(
                s,
                "Modelled active parts with open/undriven pins ({}): {}",
                open_resolved.len(),
                grouped_refs(&open_resolved),
            );
            let _ = writeln!(
                s,
                "  They sit on the live circuit but do not drive their nets, so \
                 analog/AC/thermal results there are NOT fully trustworthy."
            );
        }
        s
    }
}

/// `"U3 (XC6206), U5 (TP4056), ..."`: the refs of a banner bucket in natural
/// refdes order, one line, so the section lists WHO once and the shared
/// consequence once instead of a sentence per part.
fn grouped_refs(parts: &[UnresolvedActive]) -> String {
    let mut sorted: Vec<&UnresolvedActive> = parts.iter().collect();
    sorted.sort_by_key(|u| crate::report::natural_ref_key(&u.reference));
    sorted
        .iter()
        .map(|u| {
            if u.value.trim().is_empty() {
                u.reference.clone()
            } else {
                format!("{} ({})", u.reference, u.value)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a reference designator names an active IC (prefix U / IC / MCU),
/// matching the binder's MCU-candidate convention.
fn is_active_ic_ref(reference: &str) -> bool {
    // Eagle names any element the designer left unnamed `U$12`, and that covers
    // mounting holes, fiducials, logos, frames and test points. They take the
    // `U` prefix without being parts at all, so counting them inflated the
    // active-IC denominator by 17x on a real SparkFun board (69 "active ICs"
    // where the board has four). Any `<letters>$<digits>` reference is
    // machine-generated, never a designator someone chose.
    if is_tool_generated_ref(reference) {
        return false;
    }
    let prefix: String = reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    matches!(prefix.as_str(), "U" | "IC" | "MCU")
}

/// Whether a reference is a CAD tool's placeholder for an unnamed element
/// (Eagle's `U$12` form) rather than a designator a person assigned.
fn is_tool_generated_ref(reference: &str) -> bool {
    let Some((head, tail)) = reference.split_once('$') else {
        return false;
    };
    !head.is_empty()
        && head.chars().all(|c| c.is_ascii_alphabetic())
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
}

/// Whether a resolved row's `warning` indicates a GENUINE open/undriven-pin
/// condition (which makes thermal/AC over its nets untrustworthy) as opposed to
/// an auto-bind GPIO-map note. The binder routinely attaches an
/// `[auto-bind] ... GPIO map derived/cannot be derived ...` note to a perfectly
/// working MCU when it infers (or fails to infer) the pin map from schematic pin
/// names; that is a naming limitation, not an open circuit, and must NOT mark the
/// part as untrustworthy on the live circuit.
fn is_open_pin_warning(warning: &str) -> bool {
    // Positive match on explicit open-pin markers, NOT a negative catch-all: a
    // benign advisory on a fully-wired resolved IC (e.g. an analog switch whose
    // VCC net is non-canonically named, "...may read as open, so verify the
    // switch's actual supply") contains the bare word "open" but is not an
    // open-pin condition, and must not push the part into resolved_but_open.
    if warning.contains("[auto-bind]") || warning.contains("GPIO map") {
        return false;
    }
    let w = warning.to_ascii_lowercase();
    w.contains("undriven")
        || w.contains("left open")
        || w.contains("not connected")
        || w.contains("all i/o pins open")
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
    // Zero rows. If there are active ICs left OPEN on the live circuit, whether
    // UNRESOLVED (no model) or RESOLVED-BUT-OPEN (bound but with an open/undriven
    // pin); the table is empty because those dissipating devices are open, not
    // because the board runs cool. Both cases make the thermal result equally
    // untrustworthy, so both must escalate to invalid (exit 3); testing only the
    // unresolved case let a resolved-but-open power IC report a false "runs cool"
    // pass. Mirrors render_banner / thermal_coverage, which OR the two.
    if summary.active_ics_unresolved() || summary.active_open_on_live_circuit() {
        let mut refs: Vec<String> = summary
            .active_ic_unresolved()
            .map(|u| u.reference.clone())
            .collect();
        refs.extend(
            summary
                .resolved_but_open_active
                .iter()
                .filter(|u| u.active_ic)
                .map(|u| u.reference.clone()),
        );
        Validity::invalid(format!(
            "thermal table empty: no resolved dissipating devices; driving ICs open on the live circuit: {}",
            refs.join(", ")
        ))
    } else {
        // Genuinely no dissipating devices and nothing unresolved, a valid (if
        // boring) result, e.g. a passive-only board at ambient.
        Validity::valid()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-check COVERAGE (distinct from Validity); the honest "N of M" annotation
// ─────────────────────────────────────────────────────────────────────────────

/// Coverage is distinct from [`Validity`]. `Validity` stays BINARY and drives
/// exit 3. `CheckCoverage` is the honest "N of M active parts actually entered
/// this check" metric: it is ALWAYS emittable and NEVER changes the exit code on
/// its own (the partial-coverage thermal escalation is opt-in behind
/// `--strict-thermal`). `partial == true` means a renderer should print a
/// coverage caveat even though the check itself produced rows.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CheckCoverage {
    /// Covered active ICs / total active ICs, clamped to `[0, 1]`: how much of the
    /// active circuit the check actually modelled. `1.0` when every active IC is
    /// covered (or there are none, vacuously complete); low when power ICs are
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
/// circuit is open or unresolved, i.e. the table is real but incomplete (rows
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
    // resolved_fraction is COVERED active ICs / total active ICs, NOT dissipating
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
        .chain(
            summary
                .resolved_but_open_active
                .iter()
                .filter(|u| u.active_ic),
        )
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonNote {
    pub kind: JsonNoteKind,
    pub message: String,
}

/// One row of the boot-state panel: a transistor gate and what the firmware does
/// to it at power-up (`driven_high` / `driven_low` / `floating`). Informational
/// (reported, not judged), so a consumer can read it without it being a verdict.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BootGateJson {
    pub reference: String,
    pub net: String,
    pub state: String,
}

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JsonNoteKind {
    BindRole,
    CosimSubstitution,
    Coverage,
    SiInfo,
    /// A boot-sequence hazard about a specific MCU control net: driven HIGH and
    /// held from reset, or left floating the whole run, with no bias resistor.
    /// A distinct kind (not `Coverage`) so CI can filter boot hazards on their
    /// own. Serializes as `"boot_control_net"`.
    BootControlNet,
}

/// Machine-readable co-sim summary (today only emitted as CLI text). Populated
/// from the headless run + the MCU binding. `substituted` is true when the part
/// the user asked for was modelled by a less-specific core (Track B).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimJson {
    pub mcu_ref: String,
    /// The actual backend string from `BindOutcome::Mcu { backend }`.
    pub backend: String,
    /// The part the board asked for (threaded through `McuBinding`, Track B).
    pub requested_part: String,
    /// True when the requested part != the modelled core.
    pub substituted: bool,
    /// Wall-clock seconds the headless co-sim loop consumed. 0 when the
    /// producing path did not measure (older producers; the web mirror).
    #[serde(default)]
    pub wall_s: f64,
    /// ACHIEVED realtime factor: sim seconds delivered per wall second,
    /// measured, never the requested window over an assumed wall. 0 when
    /// unmeasured.
    #[serde(default)]
    pub realtime_factor: f64,
    /// Total net toggles seen during the run. `0` => stalled/quiet.
    pub total_toggles: u64,
    pub uart_seen: bool,
    /// Top-N nets by activity: name, toggle count, observed min/max voltage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_summary: Vec<NetActivity>,
    /// False once any chunk's analog solve failed to converge: the run held stale
    /// node voltages over `failed_windows` and cannot vouch for analog-derived
    /// findings there (05 §3b, refuse rather than fake). A fully valid run reports
    /// `true` with an empty `failed_windows`, so the common shape is unchanged and
    /// existing consumers keep parsing.
    pub analog_valid: bool,
    /// Sim-time windows `[start_s, end_s)` where the analog solve failed. Empty
    /// (and omitted from JSON) on a valid run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_windows: Vec<CosimFailedWindow>,
    /// Sim-time windows solved by a per-chunk FALLBACK integration rung after
    /// the primary solve failed there, each carrying the method that produced
    /// it and that method's accuracy cost. These windows are solved (they do
    /// not make the run analog-invalid) but their numbers are second-class:
    /// a consumer reading a waveform is entitled to know which spans came from
    /// a more dissipative method. Empty (and omitted) when the primary path
    /// carried the whole run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_windows: Vec<CosimFallbackWindow>,
    /// Per-SPI-bus transaction-framing tier: "exact" (framed on real CS GPIO
    /// edges), "backend" (the emulator frames CS itself), or "heuristic" (the
    /// chunk-boundary fallback, honest about its two documented failure
    /// modes). Empty (and omitted) when the board has no SPI buses; mirrors
    /// the web co-sim section so the CLI JSON carries the same coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spi_framing: Vec<CosimSpiFraming>,
    /// ADC channels whose per-chunk injections the MCU backend DROPPED because
    /// the platform carries no injection map (U3 finding 1). Non-empty means
    /// the analog solve drove these nets but the firmware NEVER received a
    /// sample, analog readings on those pins are meaningless, and this run
    /// cannot vouch for them. Empty (and omitted) on full coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adc_dropped: Vec<CosimAdcDrop>,
    /// Bus peripherals (I2C/SPI slave models) bound on a platform whose MCU
    /// backend models no matching bus controller (U3 finding 2): the firmware
    /// never exercised them, so their state is the power-on default. Empty
    /// (and omitted) when every bound bus device sits on a modeled controller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexercised_buses: Vec<CosimUnexercisedBus>,
    /// Firmware GPIO pulses that rose and fell inside one solver chunk on a
    /// net clocking a TICK-evaluated sequential part (friction 1.16): those
    /// parts sample once per chunk against the previous solve, so the pulse
    /// was NEVER observed by them and their state in this run lags or misses
    /// events, while chain-responder parts on the same board saw every edge
    /// exactly. One entry per offending net. Empty (and omitted) on a run with
    /// no such pulses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub short_pulses: Vec<CosimShortPulse>,
    /// Runtime driver contention: nets where the firmware configured an MCU
    /// pin as a push-pull OUTPUT while an enabled modelled push-pull output
    /// was already driving (the model-vs-MCU case the static output-contention
    /// lint documents as out of its reach). Waveforms touching these nets are
    /// untrustworthy from `t_s` on. One entry per offending net. Empty (and
    /// omitted) on a healthy run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub driver_contention: Vec<CosimDriverContention>,
    /// Per-MCU statements of how this backend's watchdog fidelity falls short of
    /// the part. Non-empty means an armed, never-fed watchdog does NOT reboot
    /// the core here the way silicon does, so firmware that HANGS runs forever
    /// in simulation and every assertion about behaviour after a hang is
    /// fiction: the run reads healthy while proving nothing about the recovery
    /// path. Empty (and omitted) on a backend whose watchdog behaves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchdog_limitations: Vec<CosimWatchdogLimitation>,
    /// Per-MCU counts of reboots an unserviced watchdog actually performed
    /// during this run. Not an error, a FINDING: an assertion that passed across
    /// a reboot was measuring a rebooted core, not the run it claimed. Empty
    /// (and omitted) when nothing rebooted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchdog_resets: Vec<CosimWatchdogResets>,
}

/// One backend watchdog-fidelity gap (see [`CosimJson::watchdog_limitations`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimWatchdogLimitation {
    pub mcu_ref: String,
    /// The backend's whole sentence, for verbatim display. Prose for a human:
    /// do not parse it or match on it exactly.
    pub limitation: String,
}

/// One MCU's watchdog reboot count (see [`CosimJson::watchdog_resets`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimWatchdogResets {
    pub mcu_ref: String,
    /// Reboots observed during the run. Always >= 1: a zero is omitted.
    pub resets: u64,
}

/// One sub-chunk GPIO pulse warning (see [`CosimJson::short_pulses`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimShortPulse {
    pub net: String,
    pub mcu_ref: String,
    /// The driving pin, e.g. `"PB1"`.
    pub pin: String,
    /// Narrowest completed pulse observed on the net (seconds).
    pub pulse_s: f64,
    /// The solver chunk it fell inside (seconds).
    pub chunk_s: f64,
    /// Tick-evaluated sequential parts clocked by the net.
    pub parts: Vec<String>,
}

/// One runtime driver-contention finding (see [`CosimJson::driver_contention`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimDriverContention {
    pub net: String,
    pub mcu_ref: String,
    /// The firmware-driven pin, e.g. `"PB1"`.
    pub pin: String,
    /// `"REF.role"` of every enabled modelled push-pull output on the net.
    pub parts: Vec<String>,
    /// Sim time (s) at which both sides were first seen driving together.
    pub t_s: f64,
}

/// One dropped ADC injection channel (see [`CosimJson::adc_dropped`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimAdcDrop {
    pub mcu_ref: String,
    pub channel: u8,
    pub net: String,
    /// Nearby part names on the net (best-effort, may be empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<String>,
}

/// One never-exercised bus peripheral (see [`CosimJson::unexercised_buses`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimUnexercisedBus {
    pub id: String,
    /// `"I2C"` or `"SPI"`.
    pub bus: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
}

/// One SPI bus's framing tier (see [`CosimJson::spi_framing`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimSpiFraming {
    pub bus: String,
    pub mode: String,
}

/// One sim-time window `[start_s, end_s)` where the analog solve failed to
/// converge and the co-sim held stale voltages. Reported so a consumer knows the
/// exact span that cannot be trusted rather than inferring a quiet run.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimFailedWindow {
    pub start_s: f64,
    pub end_s: f64,
    /// The solver's own refusal message for this window: the blame clause
    /// naming the net that refused to settle, the devices on it, and any
    /// near-zero-ohm link poisoning the matrix (E29). Defaulted (and omitted
    /// from JSON) when empty, so an older consumer keeps parsing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// One sim-time window `[start_s, end_s)` whose answer was produced by a
/// per-chunk fallback integration rung after the primary analog solve failed
/// there. `method` is the rung's stable name
/// (`crate::scheduler::ChunkFallbackMethod::as_str`), `accuracy` its stated
/// cost, so the record travels with the number it qualifies. Minimal on
/// purpose: shaped so a typed error-budget/provenance spine can absorb it
/// later as one provenance tag per window.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimFallbackWindow {
    pub start_s: f64,
    pub end_s: f64,
    pub method: String,
    pub accuracy: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DrcShort {
    pub net_a: String,
    pub net_b: String,
    pub layer: String,
    pub gap_mm: f64,
    pub loc_mm: [f64; 2],
    /// Always "serious" for a short; carried for the uniform finding shape.
    pub severity: String,
    /// Human one-line description, mirroring `JsonFinding.plain` so every
    /// `--json` finding category reads uniformly (SI/lint already carry it).
    pub plain: String,
    /// Suggested remediation, mirroring `JsonFinding.fix`.
    pub fix: String,
}

/// A group of clearance findings that share (net_a, net_b, layer, root cause),
/// collapsed to one line with a count. `at_limit` separates `gap == rule` (no
/// margin, but not below) from genuine sub-clearance violations.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
    /// Board location (mm) of the tightest gap, so a UI can pan to the worst
    /// spot of the group.
    pub min_gap_loc_mm: [f64; 2],
    /// The clearance rule for this pair (mm).
    pub rule_mm: f64,
    /// Human one-line description (`self.label()`), serialized so `--json`
    /// consumers get the same text the human/`--plain` renderers show.
    pub plain: String,
    /// Suggested remediation, mirroring `JsonFinding.fix`.
    pub fix: String,
}

impl DrcGroup {
    /// The honest one-line label. `gap == rule` is "exactly at minimum clearance
    /// (no margin)", NOT "below the spacing the board asks for" (which is only
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DrcStructured {
    pub clearance_rule_mm: f64,
    pub primitive_count: usize,
    pub shorts: Vec<DrcShort>,
    /// Grouped clearance violations that are genuinely BELOW the rule.
    pub violations: Vec<DrcGroup>,
    /// Grouped findings that sit exactly AT the rule (no margin, not below).
    pub at_limit: Vec<DrcGroup>,
    /// Set when the board's format is newer than hauksbee's validated copper
    /// extraction (KiCad 10+), making the shorts above unreliable. Surfaced by
    /// every renderer; CI gates ignore the shorts when it is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_warning: Option<String>,
}

impl DrcStructured {
    pub fn from_report(report: &DrcReport) -> Self {
        let mut shorts = Vec::new();
        // Group clearance findings by (net_a, net_b, layer). Within a group we
        // split at_limit (gap >= rule, i.e. exactly at or, defensively, above)
        // from below-rule. A KiCad clearance finding has positive gap; "at limit"
        // means gap is within a hair of the rule.
        use std::collections::BTreeMap;
        // key -> (count, below_count, min_gap, rule, min_gap_loc)
        #[allow(clippy::type_complexity)]
        let mut groups: BTreeMap<
            (String, String, String),
            (usize, usize, f64, f64, [f64; 2]),
        > = BTreeMap::new();

        // On an unvalidated board format (KiCad 10+) the shorts may be phantom, so
        // they carry "note" severity, not "serious", every structured consumer
        // (JSON, TUI) inherits the downgrade from this single source.
        let short_severity = if report.version_warning.is_some() {
            "note"
        } else {
            "serious"
        };

        for f in &report.findings {
            match f.kind {
                ViolationKind::Short => shorts.push(DrcShort {
                    net_a: f.net_a_name.clone(),
                    net_b: f.net_b_name.clone(),
                    layer: f.layer.clone(),
                    gap_mm: f.gap_mm,
                    loc_mm: [f.x, f.y],
                    severity: short_severity.to_string(),
                    plain: format!(
                        "{} shorts {} on {} at ({:.2}, {:.2}) mm (gap {:.3} mm)",
                        f.net_a_name, f.net_b_name, f.layer, f.x, f.y, f.gap_mm
                    ),
                    fix: "separate the two nets' copper: widen the gap or reroute so \
                          the trace/pad spacing clears the clearance rule"
                        .to_string(),
                }),
                ViolationKind::Clearance => {
                    // A finding is "below" the rule when its gap is under it;
                    // "at limit" when gap == rule. The 1e-4 mm (100 nm) tolerance
                    // absorbs f64 representation noise at rule values like 0.200
                    // mm without misclassifying a genuine sub-micron violation as
                    // "at limit" (a gap 0.5 um short stays "below the rule").
                    let below = f.gap_mm < f.required_clearance_mm - 1e-4;
                    let key = (f.net_a_name.clone(), f.net_b_name.clone(), f.layer.clone());
                    let e = groups.entry(key).or_insert((
                        0,
                        0,
                        f64::INFINITY,
                        f.required_clearance_mm,
                        [f.x, f.y],
                    ));
                    e.0 += 1;
                    if below {
                        e.1 += 1;
                    }
                    // Track the tightest gap AND where it is, so the group can
                    // point a UI at its worst spot.
                    if f.gap_mm < e.2 {
                        e.2 = f.gap_mm;
                        e.4 = [f.x, f.y];
                    }
                    e.3 = f.required_clearance_mm;
                }
            }
        }

        let mut violations = Vec::new();
        let mut at_limit = Vec::new();
        for ((net_a, net_b, layer), (count, below_count, min_gap, rule, min_gap_loc)) in groups {
            let any_below = below_count > 0;
            let mut group = DrcGroup {
                net_a,
                net_b,
                layer,
                count,
                below_count,
                at_limit: !any_below,
                min_gap_mm: if min_gap.is_finite() { min_gap } else { rule },
                min_gap_loc_mm: min_gap_loc,
                rule_mm: rule,
                plain: String::new(),
                fix: String::new(),
            };
            group.plain = group.label();
            group.fix = if any_below {
                "increase spacing or route clearance so the gap meets the \
                 clearance rule"
                    .to_string()
            } else {
                "no margin: widen the spacing above the rule for manufacturing \
                 tolerance"
                    .to_string()
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
            version_warning: report.version_warning.clone(),
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
        if let Some(w) = &self.version_warning {
            let _ = writeln!(s, "\n⚠ UNRELIABLE: {w}");
        }
        if self.shorts.is_empty() && self.violations.is_empty() && self.at_limit.is_empty() {
            let _ = writeln!(s, "no shorts or clearance violations.");
            return s;
        }
        if !self.shorts.is_empty() {
            let _ = writeln!(s, "\nSHORTS ({}):", self.shorts.len());
            for sh in &self.shorts {
                // Honor the (possibly downgraded) severity from the structured
                // form: on an unvalidated KiCad-10 board the shorts read "NOTE",
                // not "SERIOUS", consistent with --plain / --json / the TUI.
                let tag = if sh.severity == "serious" {
                    "SERIOUS"
                } else {
                    "NOTE"
                };
                let _ = writeln!(
                    s,
                    "  [{tag}] {} touches {} on {} (gap {:.4} mm) at x={:.1}, y={:.1}",
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
// Machine-readable JSON (Fix #6 / §4.1), one structured surface for every check
// ─────────────────────────────────────────────────────────────────────────────

use hauksbee_extract::{LintCheck, NetLintReport, Severity, SiCheck, SiReport, SiSeverity};

/// One finding in the uniform machine-readable shape (§4.1): every check's
/// findings serialize the same way, so a CI pipeline or AI never parses prose.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
        refs: report
            .receptacles
            .iter()
            .map(|r| r.reference.clone())
            .collect(),
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
            fix: lint_fix_hint(f.check, f.severity).map(str::to_string),
        })
        .collect()
}

/// Convert co-sim electrical-stress faults (overcurrent, overvoltage, a destroyed
/// MOSFET, …) to uniform JSON findings, so the machine `--json` co-sim surface
/// carries them like `--plain` (`plain_faults`), `--strict` (the exit gate), and
/// the web report already do. They were silently dropped from `--json`, so a CI
/// consumer parsing the JSON saw a clean run over a board the co-sim flagged.
/// `plain_faults` emits one finding per fault in order, so the zip is 1:1.
pub fn fault_findings_json(faults: &[crate::stress::FaultEvent]) -> Vec<JsonFinding> {
    let plain = crate::plain::plain_faults(faults);
    faults
        .iter()
        .zip(plain.findings.iter())
        .map(|(f, pf)| JsonFinding {
            check: "cosim".to_string(),
            kind: f.kind.as_str().to_string(),
            severity: match pf.level {
                crate::plain::PlainLevel::Serious => "serious",
                crate::plain::PlainLevel::Warning => "warning",
                crate::plain::PlainLevel::Note => "note",
            }
            .to_string(),
            nets: Vec::new(),
            location_mm: None,
            layer: None,
            refs: vec![f.component.clone()],
            actionable: true,
            message: pf.what.clone(),
            plain: pf.what.clone(),
            fix: if pf.fix.is_empty() {
                None
            } else {
                Some(pf.fix.clone())
            },
        })
        .collect()
}

/// The top-level `--json` document. Only the section(s) for the requested check
/// are populated; the rest stay `None` (omitted from output). `board` + `bind`
/// are always present so an AI always has the bind-role context (Theme F).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonReport {
    /// Version of the `run --json` document contract. Bumped when a field
    /// changes meaning or is removed; purely additive fields do not bump it.
    /// The generated schema lives in `crates/hauksbee-engine/schemas/`.
    pub schema_version: u32,
    pub board: String,
    pub bind: BindSummary,
    /// Every explicitly supplied input and what it contributed to this run.
    /// Empty on older/internal call paths that have no inventory context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<JsonInputEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<JsonFinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drc: Option<DrcStructured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ac: Option<AcJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalJson>,
    /// Boot-state panel: per-transistor-gate power-up state (informational).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_gates: Option<Vec<BootGateJson>>,
    /// Info-level notes (bind roles, co-sim substitution, coverage caveats, SI
    /// info) that must never be silently absent but never gate a CI pipeline.
    /// Additive + `skip_serializing_if` so the schema stays backward-compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<JsonNote>,
    /// Machine-readable co-sim summary, present only on a co-sim run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosim: Option<CosimJson>,
    /// Findings that fired and were overruled by an active waiver, with the
    /// reason and the expiry date.
    ///
    /// A green verdict that quietly dropped findings would be the worst of both
    /// worlds, so the machine surface carries them exactly as the text report
    /// prints them. A pipeline can watch this array grow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waived: Vec<JsonWaived>,
}

/// Machine-readable input inventory for BOM/placement-aware runs.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonInputEvidence {
    pub path: String,
    /// Stable broad type: `board`, `bom`, or `placement`.
    pub kind: String,
    /// Detected format within that type.
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity: Vec<String>,
}

/// One overruled finding on the machine surface.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JsonWaived {
    /// Which check produced it ("si" / "lint" / "drc").
    pub check: String,
    /// The specific rule it was matched on.
    pub kind: String,
    /// The finding's own message, so a reader can tell which one was overruled.
    pub subject: String,
    /// Why the board's owner judged it wrong or acceptable.
    pub reason: String,
    /// The date the waiver stops applying, after which this gates again.
    pub until: String,
}

impl From<crate::waiver::WaivedFinding> for JsonWaived {
    fn from(w: crate::waiver::WaivedFinding) -> Self {
        JsonWaived {
            check: w.check,
            kind: w.kind,
            subject: w.subject,
            reason: w.reason,
            until: w.until,
        }
    }
}

/// AC sweep in JSON: validity first, then the bode rows only when valid.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
    /// Requested `--ac-node` names that don't exist in the circuit at all (their
    /// bode is empty, distinct from "exists but no signal path"). Surfaced so the
    /// JSON matches the text path's "net not found" warning instead of silently
    /// dropping the net.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_found_nets: Vec<String>,
    /// Honest "N of M" coverage annotation, distinct from `validity`. Non-gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CheckCoverage>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AcNetJson {
    pub net: String,
    /// `[freq_hz, mag_db, phase_deg]` triples.
    pub points: Vec<[f64; 3]>,
}

/// Thermal in JSON: validity first, then the per-device peak temps when valid.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ThermalDeviceJson {
    pub reference: String,
    pub tj_c: f64,
    pub over_limit: bool,
}

impl JsonReport {
    /// Start a JSON report with just the always-present board + bind header.
    pub fn new(board_name: &str, bind: BindSummary) -> Self {
        JsonReport {
            schema_version: RUN_REPORT_SCHEMA_VERSION,
            board: board_name.to_string(),
            bind,
            inputs: Vec::new(),
            findings: None,
            drc: None,
            ac: None,
            thermal: None,
            boot_gates: None,
            notes: Vec::new(),
            cosim: None,
            waived: Vec::new(),
        }
    }

    /// Attach the CLI's complete, ordered input inventory to a result document.
    pub fn with_inputs(mut self, inputs: &[JsonInputEvidence]) -> Self {
        self.inputs = inputs.to_vec();
        self
    }

    /// A top-level machine verdict computed from the populated sections, so a CI
    /// consumer can read pass/fail without re-deriving it from every finding.
    /// Returns `(ok, verdict, serious_count, actionable_count)` where `verdict`
    /// is `"pass"` | `"fail"` | `"invalid"`:
    ///   - `fail`, at least one serious finding (a DRC short, a co-sim stress
    ///     fault, a serious lint/SI finding);
    ///   - `invalid`, nothing serious, but an analysis that ran could not be
    ///     judged (AC or thermal reported `valid:false`);
    ///   - `pass`, otherwise.
    /// Mirrors the run's own exit gate: DRC shorts are ignored when the board is
    /// newer than the validated copper extraction (`version_warning` set), the
    /// same carve-out the CI gate makes.
    pub fn verdict(&self) -> (bool, &'static str, usize, usize) {
        let (v, _) = self.verdict_with_waivers(&mut crate::waiver::WaiverSet::default());
        v
    }

    /// As [`Self::verdict`], but findings an active waiver covers do not gate.
    ///
    /// Returns the verdict tuple and the waived findings, which the caller
    /// reports. Waived is not hidden: a board carrying overruled findings has
    /// to look like one, or the file rots into a list nobody reads.
    #[allow(clippy::type_complexity)]
    pub fn verdict_with_waivers(
        &self,
        waivers: &mut crate::waiver::WaiverSet,
    ) -> (
        (bool, &'static str, usize, usize),
        Vec<crate::waiver::WaivedFinding>,
    ) {
        let mut serious = 0usize;
        let mut actionable = 0usize;
        let mut waived = Vec::new();
        if let Some(findings) = &self.findings {
            for f in findings {
                // Only a gating finding can be waived. Waiving a note would
                // suppress information without changing any outcome, which is
                // cost with no benefit.
                if f.severity == "serious" {
                    if let Some(w) =
                        waivers.take_waiver(&f.check, &f.kind, &f.nets, &f.refs, &f.message)
                    {
                        waived.push(w);
                        continue;
                    }
                    serious += 1;
                }
                if f.actionable {
                    actionable += 1;
                }
            }
        }
        if let Some(drc) = &self.drc {
            if drc.version_warning.is_none() {
                for s in &drc.shorts {
                    let nets = vec![s.net_a.clone(), s.net_b.clone()];
                    let subject = format!("{} to {} on {}", s.net_a, s.net_b, s.layer);
                    if let Some(w) = waivers.take_waiver("drc", "short", &nets, &[], &subject) {
                        waived.push(w);
                        continue;
                    }
                    serious += 1;
                    actionable += 1;
                }
            }
            actionable += drc.violations.len();
        }
        let invalid = self.ac.as_ref().is_some_and(|a| !a.validity.valid)
            || self.thermal.as_ref().is_some_and(|t| !t.validity.valid);
        let verdict = if serious > 0 {
            "fail"
        } else if invalid {
            "invalid"
        } else {
            "pass"
        };
        ((verdict == "pass", verdict, serious, actionable), waived)
    }

    /// Serialize to a pretty JSON string (stable, non-interactive output). The
    /// top-level object is prefixed with the machine verdict (`ok`, `verdict`,
    /// `serious_count`, `actionable_count`) computed by [`Self::verdict`], so a
    /// success document shares the `{"ok": ...}` shape with the hard-error
    /// envelope and a CI consumer never has to re-derive pass/fail.
    pub fn to_json(&self) -> String {
        let (ok, verdict, serious, actionable) = self.verdict();
        let mut value = match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => {
                return serde_json::to_string_pretty(self)
                    .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialize: {e}\"}}"))
            }
        };
        // Insert the rollup at the front so it reads first. serde_json::Map
        // preserves insertion order under the `preserve_order` feature; without
        // it the keys are present regardless of position, which is what matters.
        let mut out = serde_json::Map::new();
        out.insert("ok".into(), serde_json::json!(ok));
        out.insert("verdict".into(), serde_json::json!(verdict));
        out.insert("serious_count".into(), serde_json::json!(serious));
        out.insert("actionable_count".into(), serde_json::json!(actionable));
        out.append(&mut value);
        serde_json::to_string_pretty(&serde_json::Value::Object(out))
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
        SiCheck::TraceAmpacity => "a routed trace is too narrow for the current it carries",
        SiCheck::InputCapRipple => "the input bulk capacitor is over its ripple-current rating",
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
        SiCheck::TraceAmpacity => {
            "Widen the trace to at least the IPC-2221 minimum width for its current, pour the rail as a plane, or use heavier copper."
        }
        SiCheck::InputCapRipple => {
            "Use a capacitor with a higher ripple-current rating, split the bulk across parallel caps, or add low-ESR ceramics to take the high-frequency ripple."
        }
    })
}

/// A concise suggested fix for a lint finding kind, for the machine-readable
/// `fix` field. `None` when no template applies. Severity disambiguates the
/// resource-conflict check, whose low-severity form (a single function
/// committed to a pin-locked group) has a different honest remedy than the
/// serious two-function contention.
pub fn lint_fix_hint(check: LintCheck, severity: Severity) -> Option<&'static str> {
    if check == LintCheck::McuResourceConflict && severity != Severity::High {
        return Some(
            "If deliberate (firmware drives the device another way), nothing needs to change; \
             if the pin-locked peripheral was intended, move the device to its full fixed pin set.",
        );
    }
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
        LintCheck::DeviceDecode => {
            "Re-pick the divider resistors so the config pin lands in the intended datasheet band, per the part's decode table."
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{DrcFinding, Item, ItemKind};

    #[test]
    fn eagle_auto_named_elements_are_not_active_ics() {
        // Measured on SparkFun's SAMD51 Thing Plus: 69 references start with U,
        // and 65 of them are Eagle's `U$n` placeholder for an unnamed element
        // (mounting holes, fiducials, the logo). Counting those made the board
        // report 0/69 active ICs bound when the honest ratio is 0/4, which is a
        // different and much smaller problem. A coverage gate reading the
        // inflated number would be unpassable on every Eagle board.
        for auto in ["U$1", "U$12", "IC$3", "R$7"] {
            assert!(
                !is_active_ic_ref(auto),
                "{auto} is a CAD placeholder, not a part"
            );
        }
        for real in ["U1", "U12", "IC3", "MCU1"] {
            assert!(is_active_ic_ref(real), "{real} is a designator");
        }
        // A `$` that is not the auto-name form leaves the prefix rule alone.
        assert!(
            is_active_ic_ref("U$"),
            "no digits is not the placeholder form"
        );
        assert!(!is_active_ic_ref("R1"), "passives are not active ICs");
    }

    #[test]
    fn cosim_faults_become_json_findings() {
        // R46: co-sim electrical-stress faults were dropped from the --json surface
        // (only --plain rendered them and --strict gated them), so a CI consumer
        // parsing the JSON saw a clean run over a board the co-sim flagged. The
        // fault→JsonFinding conversion must carry a destroyed part as a serious
        // finding, refs, and fix text.
        use crate::stress::{FaultEvent, FaultKind};
        let faults = vec![
            FaultEvent {
                component: "Q1".into(),
                kind: FaultKind::Overcurrent,
                value: 5.0,
                limit: 2.0,
                t: 0.01,
                destroyed: true,
            },
            FaultEvent {
                component: "C3".into(),
                kind: FaultKind::Overvoltage,
                value: 30.0,
                limit: 16.0,
                t: 0.02,
                destroyed: false,
            },
        ];
        let js = fault_findings_json(&faults);
        assert_eq!(js.len(), 2, "one JSON finding per fault");
        assert_eq!(js[0].check, "cosim");
        assert_eq!(js[0].kind, "overcurrent");
        assert_eq!(js[0].severity, "serious", "a destroyed part is serious");
        assert_eq!(js[0].refs, vec!["Q1".to_string()]);
        assert!(js[0].fix.is_some(), "the finding carries a suggested fix");
        // A non-destructive over-voltage is a warning, not serious.
        assert_eq!(js[1].severity, "warning");
        assert_eq!(js[1].refs, vec!["C3".to_string()]);
    }

    #[test]
    fn json_report_carries_a_top_level_verdict() {
        // U3: --json success had no top-level pass/fail, so a CI consumer had to
        // re-derive it from every finding (asymmetric with the {"ok":false}
        // error envelope). to_json now prefixes ok/verdict/serious_count.
        use crate::stress::{FaultEvent, FaultKind};
        let bind = summary_with(Vec::new(), Vec::new());

        // A clean report → pass.
        let clean = JsonReport::new("b", bind.clone());
        let (ok, verdict, serious, _) = clean.verdict();
        assert!(ok && verdict == "pass" && serious == 0);
        let txt = clean.to_json();
        assert!(
            txt.contains("\"ok\": true") && txt.contains("\"verdict\": \"pass\""),
            "{txt}"
        );

        // A serious co-sim fault → fail, counted.
        let mut failed = JsonReport::new("b", bind.clone());
        failed.findings = Some(fault_findings_json(&[FaultEvent {
            component: "Q1".into(),
            kind: FaultKind::Overcurrent,
            value: 5.0,
            limit: 2.0,
            t: 0.01,
            destroyed: true,
        }]));
        let (ok, verdict, serious, actionable) = failed.verdict();
        assert!(!ok && verdict == "fail" && serious == 1 && actionable >= 1);
        assert!(failed.to_json().contains("\"verdict\": \"fail\""));

        // An invalid AC sweep with nothing serious → invalid, not pass.
        let mut invalid = JsonReport::new("b", bind);
        invalid.ac = Some(AcJson {
            validity: Validity {
                valid: false,
                reason: Some("no signal path".into()),
            },
            nets: Vec::new(),
            no_signal_path_nets: Vec::new(),
            not_found_nets: Vec::new(),
            coverage: None,
        });
        assert_eq!(invalid.verdict().1, "invalid");
    }

    #[test]
    fn drc_json_findings_carry_plain_and_fix() {
        // U3: DRC serialized as DrcShort/DrcGroup with no `plain`/`fix`, unlike
        // SI/lint findings, so a --json consumer got remediation for every
        // finding category except shorts/clearance. Both now carry them.
        let short = DrcShort {
            net_a: "GND".into(),
            net_b: "VCC".into(),
            layer: "F.Cu".into(),
            gap_mm: 0.0,
            loc_mm: [1.0, 2.0],
            severity: "serious".into(),
            plain: "GND shorts VCC on F.Cu at (1.00, 2.00) mm (gap 0.000 mm)".into(),
            fix: "separate the two nets' copper".into(),
        };
        let js = serde_json::to_string(&short).unwrap();
        assert!(js.contains("\"plain\"") && js.contains("\"fix\""), "{js}");

        let mut group = DrcGroup {
            net_a: "A".into(),
            net_b: "B".into(),
            layer: "F.Cu".into(),
            count: 3,
            below_count: 3,
            at_limit: false,
            min_gap_mm: 0.1,
            min_gap_loc_mm: [0.0, 0.0],
            rule_mm: 0.2,
            plain: String::new(),
            fix: String::new(),
        };
        group.plain = group.label();
        assert!(
            !group.plain.is_empty(),
            "group plain must be the human label"
        );
    }

    fn active_ic(reference: &str) -> UnresolvedActive {
        UnresolvedActive {
            reference: reference.to_string(),
            value: "IC".to_string(),
            reason: "all I/O pins open".to_string(),
            consequence: "its nets are not driven".to_string(),
            active_ic: true,
        }
    }

    fn summary_with(
        active_path_unresolved: Vec<UnresolvedActive>,
        resolved_but_open_active: Vec<UnresolvedActive>,
    ) -> BindSummary {
        BindSummary {
            resolved: 1,
            unresolved: 0,
            non_ignored: 1,
            critical_parts_bound: "1/1".to_string(),
            critical_parts_bound_n: 1,
            critical_parts_total: 1,
            mcu_bound: true,
            active_path_unresolved,
            resolved_but_open_active,
        }
    }

    #[test]
    fn open_pin_warning_matches_only_genuine_open_conditions() {
        // R23 (is-open-pin-warning-overbroad): a benign rail-assumption advisory
        // on a fully-wired resolved IC contains the bare word "open" but is NOT
        // an open-pin condition; it must not push the part to resolved_but_open.
        let switch_advisory =
            "U3 (SN74LVC1G3157): VCC net non-canonical, may read as open, so verify the \
             switch's actual supply";
        assert!(
            !is_open_pin_warning(switch_advisory),
            "an analog-switch VCC advisory is not an open-pin warning"
        );
        // Genuine open-pin warnings still match.
        assert!(is_open_pin_warning("U1: all I/O pins open (undriven)"));
        assert!(is_open_pin_warning("U2 output pin not connected"));
        // The auto-bind GPIO-map note is still excluded.
        assert!(!is_open_pin_warning(
            "[auto-bind] U1: GPIO map cannot be derived from pin names"
        ));
    }

    #[test]
    fn banner_warns_on_resolved_but_open_active_ic() {
        // R23 (check-heads-up-drops-resolved-but-open): a resolved MCU whose I/O
        // pins are all open on the live circuit makes its nets untrustworthy, so
        // the banner must WARN and NAME it, not just for unresolved active ICs
        // (which the web/json personas already carry via resolved_but_open_active).
        let s = summary_with(Vec::new(), vec![active_ic("U1")]);
        let banner = s.render_banner();
        assert!(
            banner.contains("NOT trustworthy"),
            "a resolved-but-open active IC must trigger the WARNING: {banner}"
        );
        assert!(
            banner.contains("U1"),
            "the resolved-but-open active IC must be named: {banner}"
        );
        // And the shared union helper the personas consume counts it.
        assert_eq!(coverage_open_active_refs(&s), vec!["U1".to_string()]);

        // A fully-clean summary stays quiet.
        let clean = summary_with(Vec::new(), Vec::new());
        assert!(!clean.render_banner().contains("NOT trustworthy"));
        assert!(coverage_open_active_refs(&clean).is_empty());
    }

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

    fn short(net_a: &str, net_b: &str) -> DrcFinding {
        let mut f = clearance(net_a, net_b, 0.0, 0.2);
        f.kind = ViolationKind::Short;
        f
    }

    #[test]
    fn unvalidated_version_downgrades_shorts_and_propagates_warning() {
        // A KiCad-10 report (version_warning set) → shorts carry "note", not
        // "serious", and the warning propagates to the structured form so every
        // consumer (JSON, TUI) inherits the downgrade from one source.
        let mut report = DrcReport {
            clearance_mm: 0.2,
            findings: vec![short("GND", "+3V3")],
            primitive_count: 2,
            version_warning: Some("unreliable on this version".into()),
        };
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.shorts.len(), 1);
        assert_eq!(
            st.shorts[0].severity, "note",
            "phantom-prone short must not be 'serious'"
        );
        assert_eq!(
            st.version_warning.as_deref(),
            Some("unreliable on this version")
        );

        // The same report on a validated version keeps "serious".
        report.version_warning = None;
        let st = DrcStructured::from_report(&report);
        assert_eq!(st.shorts[0].severity, "serious");
        assert!(st.version_warning.is_none());
    }

    use crate::report::{BindOutcome, BindRow};
    use hauksbee_models::Confidence;

    fn row(reference: &str, outcome: BindOutcome, warning: Option<&str>) -> BindRow {
        BindRow {
            reference: reference.to_string(),
            value: String::new(),
            model_id: None,
            confidence: Confidence::Exact,
            outcome,
            warning: warning.map(|s| s.to_string()),
            guesses: Vec::new(),
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
        assert!(
            thermal_validity(0, &summary).valid,
            "isolated open IC -> still valid"
        );
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
        assert!(
            !summary.active_ics_unresolved(),
            "resolved MCU is not 'unresolved'"
        );
        assert!(
            summary.active_open_on_live_circuit(),
            "but it IS open on the live circuit"
        );
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
    fn thermal_invalid_when_resolved_but_open_active_ic_leaves_empty_table() {
        // R36: an empty thermal table with a RESOLVED-BUT-OPEN power IC (bound to
        // a model, but open on the live circuit) reports a false "runs cool"
        // pass if thermal_validity escalates only the UNRESOLVED case. Both
        // open cases make the table equally untrustworthy and must exit 3.
        let mut report = BindReport::default();
        report.push(row(
            "U1",
            BindOutcome::Mcu {
                backend: "renode:stm32f4".into(),
            },
            Some("U1: all I/O pins open (undriven)"),
        ));
        let summary = BindSummary::from_report(&report);
        // The distinguishing condition: unresolved is FALSE (it bound), but the
        // part is open on the live circuit.
        assert!(
            !summary.active_ics_unresolved(),
            "resolved MCU is not 'unresolved'"
        );
        assert!(summary.active_open_on_live_circuit());
        let v = thermal_validity(0, &summary);
        assert!(
            !v.valid,
            "an empty table hiding a resolved-but-open power IC must be invalid, not 'runs cool'"
        );
        assert!(
            v.reason.unwrap().contains("U1"),
            "the reason must name the open IC"
        );
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
        assert!(st.at_limit[0]
            .label()
            .contains("exactly at minimum clearance (no margin)"));
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

    #[test]
    fn boot_control_net_kind_serializes_as_snake_case() {
        // The JSON contract is "boot_control_net", a consumer filtering
        // notes[].kind must see exactly that, not "BootControlNet".
        let note = JsonNote {
            kind: JsonNoteKind::BootControlNet,
            message: "GATE left floating".to_string(),
        };
        let json = serde_json::to_string(&note).unwrap();
        assert!(
            json.contains("\"boot_control_net\""),
            "expected snake_case kind, got: {json}"
        );
    }

    #[test]
    fn every_note_kind_serializes() {
        for kind in [
            JsonNoteKind::BindRole,
            JsonNoteKind::CosimSubstitution,
            JsonNoteKind::Coverage,
            JsonNoteKind::SiInfo,
            JsonNoteKind::BootControlNet,
        ] {
            let note = JsonNote {
                kind,
                message: "x".to_string(),
            };
            assert!(serde_json::to_string(&note)
                .unwrap()
                .contains("\"message\""));
        }
    }
}
