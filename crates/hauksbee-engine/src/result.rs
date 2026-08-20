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
//! - `1`  the run never happened: the board could not be read, or the analysis
//!        could not be set up. An input error, not a verdict about the board.
//! - `2`  `--strict` and at least one gating finding.
//! - `3`  **board invalid for the requested analysis** (AC with no signal path,
//!        thermal with no resolved dissipating devices). A run that could not
//!        produce a meaningful answer must never exit 0.

use serde::ser::SerializeStruct;
use serde::Serialize;
use std::collections::BTreeMap;

use hauksbee_extract::{DrcReport, ViolationKind};
use hauksbee_ir::evidence::Assumption;

use crate::report::{BindOutcome, BindReport};

/// Distinct process exit code for "the board is invalid for the analysis you
/// asked for", a meaningless result, not a clean one. Kept here so the CLI and
/// any future caller share one source of truth.
pub const EXIT_INVALID_FOR_ANALYSIS: i32 = 3;

/// The C5.3 refusal contract.  Exit 3 means the requested claim could not be
/// made; it is neither a finding nor a malformed-input error.  Every renderer
/// carries these same four answers so a refusal is actionable and does not
/// discard work that remains valid.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Refusal {
    /// The conclusion the run declined to make.
    pub claim: String,
    /// The concrete absent or invalid prerequisite that blocked that claim.
    pub missing_prerequisite: String,
    /// Conclusions/artifacts produced by the run that remain trustworthy.
    pub valid_partial_conclusions: Vec<String>,
    /// The cheapest concrete action that can make the claim answerable.
    pub next_action: String,
}

impl Refusal {
    pub fn new(
        claim: impl Into<String>,
        missing_prerequisite: impl Into<String>,
        valid_partial_conclusions: Vec<impl Into<String>>,
        next_action: impl Into<String>,
    ) -> Self {
        Self {
            claim: claim.into(),
            missing_prerequisite: missing_prerequisite.into(),
            valid_partial_conclusions: valid_partial_conclusions
                .into_iter()
                .map(Into::into)
                .collect(),
            next_action: next_action.into(),
        }
    }

    /// Lossless human rendering used by terminal and CI-native text surfaces.
    pub fn render_text(&self) -> String {
        format!(
            "refused claim: {}\nmissing prerequisite: {}\nvalid partial conclusions: {}\nnext action: {}",
            self.claim,
            self.missing_prerequisite,
            self.valid_partial_conclusions.join("; "),
            self.next_action,
        )
    }
}

/// Version of the `run --json` document contract (`JsonReport` plus the
/// `ok`/`verdict`/`serious_count`/`actionable_count` rollup `to_json`
/// prepends). Bump on a breaking change only; additive fields keep it.
pub const RUN_REPORT_SCHEMA_VERSION: u32 = 3;

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
    ///
    /// Never carries a routing marker. The binder joins a named abstention's two
    /// halves into one `reason` string with [`Assumption::UNLOCKED_BY_MARKER`], the
    /// way `Assumption::open_part` expects, and this surface splits it the same way
    /// rather than handing the reader plumbing.
    pub reason: String,
    /// For a named abstention, the input that would let the tool model this part.
    ///
    /// Its own field rather than a sentence glued onto `reason`, because this is the
    /// actionable half and `--json` is the surface a pipeline reads: a consumer that
    /// wants to tell someone what to upload should not have to parse prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unlocked_by: Option<String>,
    /// What leaving it open does to the analysis, in one plain line.
    pub consequence: String,
    /// True when this part is an active IC (reference prefix U/IC/MCU): the kind
    /// of part whose absence makes analog/AC/thermal results untrustworthy.
    pub active_ic: bool,
}

/// Split a binder string on an in-band routing marker, returning the prose and the
/// marked tail.
///
/// The binder has one string channel per row and two things to say on some rows, so
/// it joins them with a marker constant that every consumer is meant to split on.
/// Defined here rather than reaching for `split_once` inline so that the "marker
/// never reaches a reader" rule has one implementation on this surface.
fn split_marker(raw: &str, marker: &str) -> (String, Option<String>) {
    match raw.split_once(marker) {
        Some((head, tail)) => (head.trim().to_string(), Some(tail.trim().to_string())),
        None => (raw.to_string(), None),
    }
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
    /// `"M/N"`, active ICs (MCU + U/IC-prefixed parts) with executable device
    /// behaviour bound, over the total active ICs discovered on the board.
    /// This is deliberately **not extraction coverage**: every member of the
    /// denominator was already extracted with its reference/value/pads/nets.
    /// Identity-only or static-contract-only model cards remain outside `M`
    /// until they can actually participate in the electrical simulation.
    /// Retain the existing field name for schema compatibility; renderers must
    /// label it as behavioural model coverage rather than generic "binding".
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
                let raw = match &row.outcome {
                    BindOutcome::Unresolved { reason } => reason.clone(),
                    _ => String::new(),
                };
                // SPLIT, don't pass through. `unresolved_outcome` joins the two
                // halves of a `db/unmodelled.toml` abstention with
                // `UNLOCKED_BY_MARKER` because the binder has one `reason` channel;
                // every consumer is expected to split it. This one used to copy the
                // string verbatim, so the JSON surface, which is the one a CI
                // pipeline parses, was the only reader that got the marker text in
                // its face.
                let (reason, unlocked_by) = split_marker(&raw, Assumption::UNLOCKED_BY_MARKER);
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
                    unlocked_by,
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
                    // Belt and braces: `is_open_pin_warning` gates this bucket and no
                    // partial-model text matches its tokens today, but a warning is a
                    // warning and none of them may arrive here wearing a marker.
                    reason: row
                        .warning
                        .as_deref()
                        .map(|w| {
                            w.strip_prefix(Assumption::PARTIAL_MODEL_MARKER)
                                .unwrap_or(w)
                                .to_string()
                        })
                        .unwrap_or_default(),
                    unlocked_by: None,
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
            "\nbind summary: extracted {} non-ignored parts; {} have executable/fallback models. Critical active devices discovered: {}; executable behavioural models: {} ({})",
            self.non_ignored,
            self.resolved,
            self.critical_parts_total,
            self.critical_parts_bound,
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
    /// Present on every exit-3 validity result. `reason` remains as a compact
    /// compatibility alias for `missing_prerequisite`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

impl Validity {
    pub fn valid() -> Self {
        Validity {
            valid: true,
            reason: None,
            refusal: None,
        }
    }
    pub fn invalid(reason: impl Into<String>) -> Self {
        Validity {
            valid: false,
            reason: Some(reason.into()),
            refusal: None,
        }
    }

    pub fn refused(refusal: Refusal) -> Self {
        Validity {
            valid: false,
            reason: Some(refusal.missing_prerequisite.clone()),
            refusal: Some(refusal),
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
        let refs = refs.join(", ");
        Validity::refused(Refusal::new(
            "a thermal-safety conclusion for the board",
            format!(
                "no resolved dissipating devices reached the thermal table; active ICs are open on the live circuit: {refs}"
            ),
            vec!["board extraction and component binding completed"],
            format!(
                "bind {refs} with --models-dir, then rerun the same --thermal command"
            ),
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
/// its own (the partial-coverage thermal escalation is applied by the thermal
/// report, by default; `--no-strict-thermal` opts out). `partial == true` means
/// a renderer should print a coverage caveat even though the check itself
/// produced rows.
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

/// Thermal coverage, parallel to [`thermal_validity`]. Returns `partial = true`
/// when there ARE dissipating rows yet an active IC on the live circuit is open
/// or unresolved, i.e. the table is real but incomplete (rows exist only because
/// some passives/parts resolved while a power IC is open). The thermal report
/// escalates the partial case to exit 3 by default (`--no-strict-thermal` opts
/// out); `thermal_validity` itself is unchanged.
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
// The INCONCLUSIVE verdict vocabulary (one dialect for every static surface)
// ─────────────────────────────────────────────────────────────────────────────

/// The current-carrying / active parts whose absence of a model makes a clean
/// static verdict vacuous: the [`coverage_open_active_refs`] set (active ICs
/// that are unresolved or resolved-but-open on the live circuit) plus
/// unresolved discrete transistors (`Q` prefix) on the live circuit, the power
/// FETs on a protection path. A clean `--lint`/`--si` result over these parts
/// is not a clean bill, and the verdict line must say so instead of "Looks
/// healthy" (PROCESS_AND_UX_LOG item 4: a vacuous pass on a BMS protection
/// path is the most dangerous UX failure here).
///
/// Passives are deliberately excluded: an unbound connector or resistor does
/// not blind these checks the way an open driver or FET does, and flagging
/// every unresolved part would cry wolf on healthy boards.
/// The single rule for whether undermined evidence invalidates a RUN-LEVEL
/// claim: an undermined map counts unless it backs an individual finding
/// (`is_finding_assertion` on its assertion text). Finding-backed maps carry
/// their own qualified/undermined badges on the report without vetoing a
/// judgement they never backed; without this split, one unparseable value
/// field anywhere on the board invalidated the whole run through whichever
/// informational note touched its net. Used by the JSON verdict and by the
/// `--strict` exit gates, so the exit code can never disagree with the
/// verdict field.
pub fn run_level_undermined(
    maps: &[hauksbee_ir::evidence::EvidenceMap],
    is_finding_assertion: impl Fn(&str) -> bool,
) -> bool {
    maps.iter()
        .any(|map| map.is_undermined() && !is_finding_assertion(map.assertion()))
}

pub fn unmodelled_critical_refs(summary: &BindSummary) -> Vec<String> {
    let mut refs: Vec<String> = summary
        .active_path_unresolved
        .iter()
        .filter(|u| is_verdict_critical(u))
        .chain(
            summary
                .resolved_but_open_active
                .iter()
                .filter(|u| u.active_ic),
        )
        .map(|u| u.reference.clone())
        .collect();
    refs.sort_by_key(|r| crate::report::natural_ref_key(r));
    refs.dedup();
    refs
}

/// The one INCONCLUSIVE verdict sentence the lint/SI/check surfaces share: the
/// count, the named parts, and the input that unlocks a conclusive verdict.
/// Their `--plain` verdict lines, default text summaries, the `--check`
/// closing verdict and their JSON coverage notes all render THIS string (the
/// closing verdict drops the leading tag it already states), so the vocabulary
/// cannot fork across those surfaces. Thermal and AC speak through their own
/// coverage caveats, which lead with the same INCONCLUSIVE tag. It never
/// changes an exit code on its own; the exit-code contract is documented in
/// docs/ci/CI.md.
pub fn inconclusive_verdict(refs: &[String]) -> String {
    let list = refs.join(", ");
    format!(
        "INCONCLUSIVE: {} current-carrying / active part(s) have no model ({list}), \
         so a clean result here is not a clean bill. Draft models from their \
         datasheets (`hauksbee models extract --pdf <datasheet.pdf> --part <MPN>`, \
         or the Extend button in `hauksbee serve`), scaffold one by hand \
         (`hauksbee models new`, then --models-dir), or supply BOM identity \
         (--bom) to make this verdict conclusive.",
        refs.len(),
    )
}

/// Whether an unresolved/open part on the live circuit blocks a conclusive
/// verdict: an active IC, or a discrete transistor. Shared by
/// [`unmodelled_critical_refs`] and the web front door so the CLI and browser
/// can never disagree about which parts forbid a clean bill.
pub(crate) fn is_verdict_critical(u: &UnresolvedActive) -> bool {
    u.active_ic || is_transistor_ref(&u.reference)
}

/// Whether a reference designator names a discrete transistor (prefix `Q`),
/// the current-carrying part class whose unbound absence blinds the static
/// checks without being an active IC.
fn is_transistor_ref(reference: &str) -> bool {
    if is_tool_generated_ref(reference) {
        return false;
    }
    let prefix: String = reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    prefix == "Q"
}

// ─────────────────────────────────────────────────────────────────────────────
// Info-level notes + machine-readable co-sim summary
// ─────────────────────────────────────────────────────────────────────────────

/// An always-emitted, info-level note: the structured home for honesty
/// annotations that must never be silently absent (bind roles, co-sim
/// substitution, coverage caveats, SI info, boot hazards).
///
/// Distinct from [`JsonFinding`]: a note carries no severity and no gating flag,
/// nothing counts it into `serious_count`, and no gate reads this array. But
/// "informational" is about the note, NOT about what it reports: 3 of the 5
/// [`JsonNoteKind`] variants describe a condition a gate reads from its own
/// source, so seeing one of those does not mean the run exited 0.
///
/// - `Coverage` carries the INCONCLUSIVE sentence for unbound verdict-critical
///   parts (verdict `invalid`, exit 3 under `--strict`) and the `--thermal`
///   PARTIAL-coverage caveat (exit 3 BY DEFAULT, unless `--no-strict-thermal`).
/// - `CosimSubstitution` reports a substitute MCU core, which is an
///   `Undermined`-class assumption: on a run-level evidence map it makes the
///   verdict `invalid`, and `--strict` exits 3.
/// - `BootControlNet` reports the boot hazard `--strict-boot` escalates to a
///   failing gate (verdict `fail`, exit 2).
///
/// The other 2, `BindRole` and `SiInfo`, report conditions no gate reads: an
/// inferred pin role is a `Qualified`-class assumption, which never invalidates
/// a run, and the SI gate counts real findings only, never its info notes.
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
    /// Numerical qualification for this exact run. Solver tolerances and
    /// methods are inputs, the residual is measured when available, and failed
    /// windows are explicitly invalid rather than assigned invented precision.
    pub error_budget: hauksbee_ir::evidence::ErrorBudget,
    /// Top-N nets by activity: name, toggle count, observed min/max voltage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_summary: Vec<NetActivity>,
    /// Measured edge timestamp precision and guaranteed pulse floor for every
    /// live MCU backend at the actual chunk used by this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_coverage: Vec<crate::scheduler::TimingCoverage>,
    /// Runtime timing/replay limits reached. A strict run exits INVALID when
    /// this is non-empty instead of silently trusting a collapsed waveform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_refusals: Vec<String>,
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
    /// it and that method's qualitative fidelity note. These windows are solved (they do
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
    /// Per-MCU statements of how this backend's TIMING fidelity falls short of
    /// the part. Non-empty means simulated time on these cores carries a known
    /// systematic bias (wall-clock-paced virtual time on the QEMU family, the
    /// F103's deliberate TIMx-at-72MHz divergence), so time-based assertions
    /// there mean less than they look. Empty (and omitted) on cores whose
    /// timing is clock-truth gated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_limitations: Vec<CosimTimingLimitation>,
}

/// One backend watchdog-fidelity gap (see [`CosimJson::watchdog_limitations`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimWatchdogLimitation {
    pub mcu_ref: String,
    /// The backend's whole sentence, for verbatim display. Prose for a human:
    /// do not parse it or match on it exactly.
    pub limitation: String,
}

/// One backend timing-fidelity gap (see [`CosimJson::timing_limitations`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimTimingLimitation {
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
    /// On `mode = "exact"`, where the chip-select came from: `"spec"` (the run
    /// spec's `cs_net`), `"model-roles"` (the `cs` pin role of the bound model for
    /// the board component the peripheral names), or `"bitbang-pins"` (a
    /// bit-banged SPI slave, whose CS pin comes from the GPIO wiring the responder
    /// was attached with rather than from any net lookup). Absent on the `backend`
    /// and `heuristic` tiers, where no chip-select was resolved. The tier alone
    /// does not say this, and the routes fail differently, so a consumer
    /// reproducing a result needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cs_provenance: Option<String>,
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
/// (`crate::scheduler::ChunkFallbackMethod::as_str`), `fidelity_note` names
/// the known algorithmic trade-off, and `error_estimate_v` is a MEASURED
/// estimate, in volts, of the chunk-end node-voltage error each chunk's march
/// added relative to its own chunk-start state, from a companion re-solve at
/// a 4x-shifted accuracy dial (tighter first, a coarser leg when the tight
/// companion will not converge), conservatively Richardson-scaled. Worst
/// chunk of a merged window, not a sum across it; omitted when no companion
/// converged, never invented. The record travels with the number it
/// qualifies.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CosimFallbackWindow {
    pub start_s: f64,
    pub end_s: f64,
    pub method: String,
    pub fidelity_note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_estimate_v: Option<f64>,
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

/// Where the clearance value applied by DRC came from.  A default is not a
/// design rule merely because the geometric check had to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearanceRuleSource {
    /// Netclass rules were read from the sibling KiCad project file.
    ProjectFileFound,
    /// No usable project rules were available, so the extractor's fallback was
    /// applied.  Renderers must never call this the user's rule.
    Defaulted,
    /// The importer for this format has no design-rule channel and used the
    /// tool fallback directly.
    ToolDefault,
    /// A non-KiCad layout format carried its own clearance value.
    BoardFile,
    /// A sibling KiCad custom-rules file supplied the report-wide value.
    CustomRulesFile,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CustomRuleScopeOmission {
    pub name: String,
    pub line_number: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub constraint_types: Vec<String>,
    /// Constraint types whose bounds contain a bare, unitless value. KiCad
    /// 10.0.5 deactivates the whole rules file, so this is disclosure, not a
    /// fidelity qualification on otherwise matching findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bare_value_constraint_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CustomRulesCoverage {
    pub file_name: String,
    /// KiCad 10.0.5 silently deactivates every custom rule in a file when any
    /// constraint bound omits its unit.
    pub file_inactive_due_to_bare_values: bool,
    pub unevaluated_rules: Vec<CustomRuleScopeOmission>,
    pub not_covered_rules: Vec<CustomRuleScopeOmission>,
    pub unsupported_constraint_counts: BTreeMap<String, usize>,
}

impl CustomRulesCoverage {
    pub fn from_parsed(file_name: String, parsed: &hauksbee_extract::KicadDruRules) -> Self {
        let describe = |rule: &hauksbee_extract::KicadDruRule| CustomRuleScopeOmission {
            name: rule.name.clone(),
            line_number: rule.line_number,
            condition: rule.condition.clone(),
            layer: rule.layer.clone(),
            constraint_types: rule
                .constraints
                .iter()
                .map(|constraint| constraint.kind.as_str().to_string())
                .collect(),
            bare_value_constraint_types: rule
                .bare_value_constraint_types()
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
        };
        let unevaluated_rules = parsed.unevaluated_rules().map(describe).collect();
        let not_covered_rules = parsed
            .rules
            .iter()
            .filter(|rule| {
                rule.constraints.iter().any(|constraint| {
                    constraint.kind != hauksbee_extract::KicadDruConstraintKind::Clearance
                })
            })
            .map(describe)
            .collect();
        Self {
            file_name,
            file_inactive_due_to_bare_values: parsed.has_bare_values(),
            unevaluated_rules,
            not_covered_rules,
            unsupported_constraint_counts: parsed.unsupported_constraint_counts.clone(),
        }
    }

    pub fn qualifies_clearance_findings(&self) -> bool {
        !self.file_inactive_due_to_bare_values
            && self.unevaluated_rules.iter().any(|rule| {
                rule.bare_value_constraint_types.is_empty()
                    && rule.constraint_types.iter().any(|kind| kind == "clearance")
            })
    }

    pub fn unevaluated_notice(&self) -> Option<String> {
        if self.unevaluated_rules.is_empty() {
            return None;
        }
        use std::fmt::Write as _;
        let bare_rules = self
            .unevaluated_rules
            .iter()
            .filter(|rule| !rule.bare_value_constraint_types.is_empty())
            .collect::<Vec<_>>();
        let scoped_rules = self
            .unevaluated_rules
            .iter()
            .filter(|rule| rule.bare_value_constraint_types.is_empty())
            .collect::<Vec<_>>();
        let mut notice = String::new();

        if !bare_rules.is_empty() {
            if self.file_inactive_due_to_bare_values {
                let rule_count = self.unevaluated_rules.len();
                let first = bare_rules[0];
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES DISABLED: {} is not in force. Rule {:?} on line {} has a value with no unit, and KiCad discards the ENTIRE rules file when any value lacks one, so all {rule_count} rules in it are inactive and this board is being checked against netclass defaults instead. Add the unit to restore the rest.",
                    self.file_name,
                    first.name,
                    first.line_number,
                );
                for rule in bare_rules.into_iter().skip(1) {
                    let types = rule.bare_value_constraint_types.join(", ");
                    let _ = writeln!(
                        notice,
                        "  Additional unitless rule: {:?} (line {}, {types}).",
                        rule.name, rule.line_number,
                    );
                }
            } else {
                let count = bare_rules.len();
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES NOT EVALUATED: {count} bare-value rule{} in {} {} not applied:",
                    if count == 1 { "" } else { "s" },
                    self.file_name,
                    if count == 1 { "was" } else { "were" },
                );
                for rule in bare_rules {
                    let types = rule.bare_value_constraint_types.join(", ");
                    let _ = writeln!(
                        notice,
                        "  {:?} (line {}, {types}): this bare-value rule needs an explicit unit (for example, mm).",
                        rule.name, rule.line_number,
                    );
                }
            }
        }

        if !scoped_rules.is_empty() && !self.file_inactive_due_to_bare_values {
            let conditional = scoped_rules
                .iter()
                .all(|rule| rule.condition.is_some() && rule.layer.is_none());
            let count = scoped_rules.len();
            if conditional {
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES NOT EVALUATED: {count} rule{} in {} {} conditional and hauksbee cannot evaluate {} condition{}:",
                    if count == 1 { "" } else { "s" },
                    self.file_name,
                    if count == 1 { "is" } else { "are" },
                    if count == 1 { "its" } else { "their" },
                    if count == 1 { "" } else { "s" },
                );
            } else {
                let _ = writeln!(
                    notice,
                    "CUSTOM RULES NOT EVALUATED: {count} scoped rule{} in {} cannot be applied report-wide:",
                    if count == 1 { "" } else { "s" },
                    self.file_name,
                );
            }
            for rule in scoped_rules {
                let scope = match (&rule.condition, &rule.layer) {
                    (Some(condition), Some(layer)) => format!(" when {condition} on {layer}"),
                    (Some(condition), None) => format!(" when {condition}"),
                    (None, Some(layer)) => format!(" on {layer}"),
                    (None, None) => String::new(),
                };
                let _ = writeln!(notice, "  {:?}{scope}", rule.name);
            }
        }

        if self.qualifies_clearance_findings() {
            notice.push_str(
                "Copper-clearance findings may therefore be judged against the wrong limit.",
            );
        } else if !self.file_inactive_due_to_bare_values
            && (!self.not_covered_rules.is_empty()
                || !self.unsupported_constraint_counts.is_empty())
        {
            notice.push_str(
                "These rules govern checks hauksbee does not report, so they do not qualify reported findings; they are listed in NOT COVERED.",
            );
        }
        Some(notice)
    }

    pub fn not_covered_summary(&self) -> Option<String> {
        if self.not_covered_rules.is_empty() && self.unsupported_constraint_counts.is_empty() {
            return None;
        }
        let rules = self
            .not_covered_rules
            .iter()
            .map(|rule| {
                format!(
                    "{:?} ({})",
                    rule.name,
                    rule.constraint_types
                        .iter()
                        .filter(|kind| kind.as_str() != "clearance")
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let unsupported = self
            .unsupported_constraint_counts
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Custom rules not covered in {}: {}{}{} Hauksbee has no hole-clearance, board-edge-clearance, or other listed custom-constraint checks; these rules are parsed and disclosed but do not qualify reported copper-clearance findings.",
            self.file_name,
            rules,
            if rules.is_empty() || unsupported.is_empty() { "" } else { "; " },
            unsupported,
        ))
    }
}

/// Machine-readable provenance for a reported clearance value.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ClearanceRuleProvenance {
    pub source: ClearanceRuleSource,
    /// The default/report-wide value in millimetres. Pair-specific resolved
    /// values remain on each [`DrcGroup`] as `rule_mm`.
    pub value_mm: f64,
    /// Distinguishes a missing sibling from a present file whose rules could not
    /// be read. In both cases `source` is `defaulted`: invented evidence does not
    /// become project evidence merely because a file existed.
    pub project_file_found: bool,
    /// The netclass value replaced by a report-wide custom rule, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden_project_value_mm: Option<f64>,
    /// Parsed custom-rule coverage, including scopes Hauksbee did not evaluate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_rules: Option<CustomRulesCoverage>,
}

impl Default for ClearanceRuleProvenance {
    fn default() -> Self {
        Self::defaulted(0.0, false)
    }
}

impl ClearanceRuleProvenance {
    pub fn project_file(value_mm: f64) -> Self {
        Self {
            source: ClearanceRuleSource::ProjectFileFound,
            value_mm,
            project_file_found: true,
            overridden_project_value_mm: None,
            custom_rules: None,
        }
    }

    pub fn defaulted(value_mm: f64, project_file_found: bool) -> Self {
        Self {
            source: ClearanceRuleSource::Defaulted,
            value_mm,
            project_file_found,
            overridden_project_value_mm: None,
            custom_rules: None,
        }
    }

    pub fn board_file(value_mm: f64) -> Self {
        Self {
            source: ClearanceRuleSource::BoardFile,
            value_mm,
            project_file_found: false,
            overridden_project_value_mm: None,
            custom_rules: None,
        }
    }

    pub fn tool_default(value_mm: f64) -> Self {
        Self {
            source: ClearanceRuleSource::ToolDefault,
            value_mm,
            project_file_found: false,
            overridden_project_value_mm: None,
            custom_rules: None,
        }
    }

    pub fn custom_rules_file(
        value_mm: f64,
        overridden_project_value_mm: Option<f64>,
        coverage: CustomRulesCoverage,
    ) -> Self {
        Self {
            source: ClearanceRuleSource::CustomRulesFile,
            value_mm,
            project_file_found: overridden_project_value_mm.is_some(),
            overridden_project_value_mm,
            custom_rules: Some(coverage),
        }
    }

    pub fn with_custom_rules(mut self, coverage: CustomRulesCoverage) -> Self {
        self.custom_rules = Some(coverage);
        self
    }

    pub fn is_defaulted(&self) -> bool {
        matches!(
            self.source,
            ClearanceRuleSource::Defaulted | ClearanceRuleSource::ToolDefault
        )
    }

    fn missing_reason(&self) -> &'static str {
        if self.project_file_found {
            "the sibling .kicad_pro was found, but its netclass rules were not read"
        } else {
            "no .kicad_pro was found beside this board, so your netclass rules were not read"
        }
    }

    /// Qualification appended to every human rule reference.
    pub fn rule_reference(&self, value_mm: f64) -> String {
        match self.source {
            ClearanceRuleSource::ProjectFileFound => format!("your {value_mm:.3} mm rule"),
            ClearanceRuleSource::Defaulted => format!(
                "the {value_mm:.3} mm DEFAULT clearance ({}; these are default rules, not your rules)",
                self.missing_reason()
            ),
            ClearanceRuleSource::ToolDefault => format!(
                "the {value_mm:.3} mm TOOL DEFAULT clearance (the importer did not read design rules; these are default rules, not your rules)"
            ),
            ClearanceRuleSource::BoardFile => {
                format!("the board-file {value_mm:.3} mm clearance rule")
            }
            ClearanceRuleSource::CustomRulesFile => format!(
                "the {value_mm:.3} mm custom rule from {}",
                self.custom_rules
                    .as_ref()
                    .map(|coverage| coverage.file_name.as_str())
                    .unwrap_or("the sibling .kicad_dru")
            ),
        }
    }

    /// Prominent source + unlocking-input sentence for a CLI report.
    pub fn source_notice(&self) -> String {
        match self.source {
            ClearanceRuleSource::ProjectFileFound => format!(
                "CLEARANCE RULE SOURCE: sibling .kicad_pro (report-wide value {:.3} mm).",
                self.value_mm
            ),
            ClearanceRuleSource::Defaulted => format!(
                "CLEARANCE RULE SOURCE: DEFAULT {:.3} mm; {}; these are default rules, not your rules.",
                self.value_mm,
                self.missing_reason()
            ),
            ClearanceRuleSource::ToolDefault => format!(
                "CLEARANCE RULE SOURCE: TOOL DEFAULT {:.3} mm; this importer did not read design rules, so these are default rules, not your rules.",
                self.value_mm
            ),
            ClearanceRuleSource::BoardFile => format!(
                "CLEARANCE RULE SOURCE: clearance carried by the board file (report-wide value {:.3} mm).",
                self.value_mm
            ),
            ClearanceRuleSource::CustomRulesFile => {
                let file = self
                    .custom_rules
                    .as_ref()
                    .map(|coverage| coverage.file_name.as_str())
                    .unwrap_or("sibling .kicad_dru");
                match self.overridden_project_value_mm {
                    Some(project) => format!(
                        "CLEARANCE RULE SOURCE: custom rules file {file} (report-wide value {:.3} mm), which overrides the .kicad_pro netclass value of {project:.3} mm.",
                        self.value_mm
                    ),
                    None => format!(
                        "CLEARANCE RULE SOURCE: custom rules file {file} (report-wide value {:.3} mm).",
                        self.value_mm
                    ),
                }
            }
        }
    }

    pub fn custom_rules_notice(&self) -> Option<String> {
        self.custom_rules
            .as_ref()
            .and_then(CustomRulesCoverage::unevaluated_notice)
    }

    pub fn custom_rules_not_covered(&self) -> Option<String> {
        self.custom_rules
            .as_ref()
            .and_then(CustomRulesCoverage::not_covered_summary)
    }

    /// Prominent source + unlocking-input sentence for a CLI report.
    pub fn cli_notice(&self) -> String {
        let notice = self.source_notice();
        if self.source == ClearanceRuleSource::Defaulted {
            let unlock = if self.project_file_found {
                "Repair or replace the sibling .kicad_pro with readable netclass rules and rerun to check your netclass rules."
            } else {
                "Place the matching .kicad_pro next to the board and rerun to check your netclass rules."
            };
            format!("{notice} {unlock}")
        } else {
            notice
        }
    }

    /// Web-specific unlocking instruction: this path has upload bytes, not a
    /// sibling filesystem path.
    pub fn web_notice(&self) -> String {
        match self.source {
            ClearanceRuleSource::Defaulted if self.project_file_found => format!(
                "CLEARANCE RULE SOURCE: DEFAULT {:.3} mm; the uploaded .kicad_pro was present, but its netclass rules were not read; these are default rules, not your rules. Repair or replace that .kicad_pro and upload it alongside the board to check your netclass rules.",
                self.value_mm
            ),
            ClearanceRuleSource::Defaulted => format!(
                "CLEARANCE RULE SOURCE: DEFAULT {:.3} mm; this upload did not include a .kicad_pro, so your netclass rules were not read; these are default rules, not your rules. Upload the matching .kicad_pro alongside the board to check your netclass rules.",
                self.value_mm
            ),
            _ => self.cli_notice(),
        }
    }
}

/// A real short between two nets (gap <= 0: touching copper).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DrcShort {
    pub net_a: String,
    pub net_b: String,
    pub layer: String,
    pub gap_mm: f64,
    pub loc_mm: [f64; 2],
    /// "serious" for a validated-format short or an unvalidated-format short
    /// independently confirmed by KiCad's own DRC. "note" remains for a
    /// tool-only unvalidated-format claim or where board-local physical
    /// authority qualifies this location. A companion Eagle schematic adds
    /// context but cannot change severity.
    pub severity: String,
    /// Human one-line description, mirroring `JsonFinding.plain` so every
    /// `--json` finding category reads uniformly (SI/lint already carry it).
    pub plain: String,
    /// Suggested remediation, mirroring `JsonFinding.fix`.
    pub fix: String,
}

const ORACLE_AGREEMENT_MARKER: &str = "ORACLE AGREEMENT: ";
const TOOL_ONLY_FORMAT_MARKER: &str = "; TOOL-ONLY: ";

impl DrcShort {
    pub(crate) fn oracle_agreement(&self) -> Option<&str> {
        self.plain
            .split_once(ORACLE_AGREEMENT_MARKER)
            .map(|(_, agreement)| agreement)
    }

    /// Replace the unvalidated-format, tool-only qualification with the exact
    /// independent evidence that settled it. The caller owns pair matching;
    /// this method only keeps the finding's evidence sentence single-sourced.
    pub(crate) fn attach_oracle_agreement(&mut self, version: &str) {
        if let Some((claim, _)) = self.plain.split_once(TOOL_ONLY_FORMAT_MARKER) {
            self.plain = claim.to_string();
        }
        self.plain.push_str(&format!(
            "; {ORACLE_AGREEMENT_MARKER}confirmed by KiCad's own DRC ({version}) for this \
             specific net pair; the coordinates shown are Hauksbee's reported contact location"
        ));
    }
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
    /// What pair of copper objects the tightest gap is between ("via \u{2194}
    /// zone", "track \u{2194} pad"), so a reader knows what to look AT before
    /// panning to the coordinates. Diagnosability is the point: a via-related
    /// finding sends the reader to the via's layer settings, not to rerouting.
    pub between: String,
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
        self.label_with_rule_provenance(&ClearanceRuleProvenance::defaulted(self.rule_mm, false))
    }

    pub fn label_with_rule_provenance(&self, provenance: &ClearanceRuleProvenance) -> String {
        let loc = |n: usize| format!("{n} location{}", if n == 1 { "" } else { "s" });
        let rule = provenance.rule_reference(self.rule_mm);
        if self.at_limit {
            format!(
                "{} vs {}: {} on {}, all exactly at minimum clearance (no margin) [{}]",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                rule
            )
        } else if self.below_count == self.count {
            format!(
                "{} vs {}: {} on {}, below {} (tightest {:.3} mm, {})",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                rule,
                self.min_gap_mm,
                self.between
            )
        } else {
            // Mixed: some below, the remainder exactly at the limit.
            format!(
                "{} vs {}: {} on {} ({} below {}, tightest {:.3} mm; {} at the limit)",
                self.net_a,
                self.net_b,
                loc(self.count),
                self.layer,
                self.below_count,
                rule,
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
    /// extraction (KiCad 10+), making tool-only shorts above unreliable.
    /// Surfaced by every renderer; CI gates ignore those tool-only shorts, but
    /// an exact net-pair match from KiCad's own DRC restores a short's serious
    /// severity and gate status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_warning: Option<String>,
    /// Set when the run suppressed zone-versus-pad overlaps as antipad carves.
    /// "No shorts found" and "no shorts found, having skipped a whole class"
    /// are different claims, so every renderer states this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppression_note: Option<String>,
}

impl DrcStructured {
    pub fn from_report(report: &DrcReport) -> Self {
        Self::from_report_with_ties(report, None, false)
    }

    pub fn from_report_with_ties(
        report: &DrcReport,
        qualification: Option<&hauksbee_extract::DrcTieQualification>,
        missing_eagle_schematic: bool,
    ) -> Self {
        let mut shorts = Vec::new();
        // Group clearance findings by (net_a, net_b, layer). Within a group we
        // split at_limit (gap >= rule, i.e. exactly at or, defensively, above)
        // from below-rule. A KiCad clearance finding has positive gap; "at limit"
        // means gap is within a hair of the rule.
        use std::collections::BTreeMap;
        // key -> (count, below_count, min_gap, rule, min_gap_loc)
        let mut groups: BTreeMap<
            (String, String, String),
            (
                usize,
                usize,
                f64,
                f64,
                [f64; 2],
                (hauksbee_extract::ItemKind, hauksbee_extract::ItemKind),
            ),
        > = BTreeMap::new();

        // On an unvalidated board format (KiCad 10+) the shorts may be phantom, so
        // they carry "note" severity, not "serious", every structured consumer
        // (JSON, TUI) inherits the downgrade from this single source.
        let phantom = report.version_warning.is_some();

        for f in &report.findings {
            let authorized_tie = qualification.and_then(|ties| ties.tie_for(f));
            let declared_tie = qualification.and_then(|ties| ties.declaration_for(f));
            match f.kind {
                // A short is serious unless something specific says otherwise:
                // the format was unvalidated, or board-local authority declares
                // this exact pair of nets deliberately tied. Both are per-finding
                // here rather than per-report, because board-local authority qualifies
                // only the contacts it covers and every other short on the same
                // board stays serious.
                ViolationKind::Short => {
                    // The geometry sentence is identical at every confidence:
                    // the nets share copper. A newer-than-validated format adds
                    // an explicit tool-only evidence boundary here, at the same
                    // demotion site that changes the severity. Pair-specific
                    // oracle agreement later replaces this sentence and restores
                    // the real severity; an unmatched finding keeps both.
                    let mut plain = match declared_tie {
                        Some(tie) => format!(
                            "{} and {} are joined in copper on {} at ({:.2}, {:.2}) mm \
                             (gap {:.3} mm). The schematic names this net pair ({}; {}), but \
                             does not identify or authorize this physical location",
                            f.net_a_name,
                            f.net_b_name,
                            f.layer,
                            f.x,
                            f.y,
                            f.gap_mm,
                            tie.declaration,
                            tie.source,
                        ),
                        None => format!(
                            "{} shorts {} on {} at ({:.2}, {:.2}) mm (gap {:.3} mm)",
                            f.net_a_name, f.net_b_name, f.layer, f.x, f.y, f.gap_mm
                        ),
                    };
                    if phantom {
                        plain.push_str(
                            "; TOOL-ONLY: Hauksbee reports this contact from an unvalidated \
                             board format; no matching KiCad-oracle confirmation is attached",
                        );
                    }
                    shorts.push(DrcShort {
                        net_a: f.net_a_name.clone(),
                        net_b: f.net_b_name.clone(),
                        layer: f.layer.clone(),
                        gap_mm: f.gap_mm,
                        loc_mm: [f.x, f.y],
                        severity: if phantom || authorized_tie.is_some() {
                            "note".to_string()
                        } else {
                            "serious".to_string()
                        },
                        plain,
                        fix: match (declared_tie, missing_eagle_schematic) {
                        (Some(_), _) => "verify this exact join against board-local layout intent \
                             (for example a named net-tie footprint or reviewed coordinate), or \
                             separate the nets. The Eagle schematic names only the net pair and \
                             cannot prove where the physical join belongs."
                            .to_string(),
                        // No schematic was supplied and this format can declare
                        // ties in one. Name that upload rather than leaving the
                        // reader to guess what would settle it.
                        (None, true) => "separate the two nets' copper: widen the gap or reroute \
                             so the trace/pad spacing clears the clearance rule. If this contact \
                             is deliberate, supply the same-named Eagle .sch companion for net-pair \
                             context, then provide board-local layout authority for this location"
                            .to_string(),
                        (None, false) => "separate the two nets' copper: widen the gap or reroute \
                             so the trace/pad spacing clears the clearance rule"
                            .to_string(),
                        },
                    });
                }
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
                        (f.item_a.kind, f.item_b.kind),
                    ));
                    e.0 += 1;
                    if below {
                        e.1 += 1;
                    }
                    // Track the tightest gap AND where it is (and what pair of
                    // objects it is between), so the group can point a UI at
                    // its worst spot and the reader knows whether to look at a
                    // via, a track, or a zone fill.
                    if f.gap_mm < e.2 {
                        e.2 = f.gap_mm;
                        e.4 = [f.x, f.y];
                        e.5 = (f.item_a.kind, f.item_b.kind);
                    }
                    e.3 = f.required_clearance_mm;
                }
            }
        }

        let mut violations = Vec::new();
        let mut at_limit = Vec::new();
        for ((net_a, net_b, layer), (count, below_count, min_gap, rule, min_gap_loc, kinds)) in
            groups
        {
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
                between: format!("{} \u{2194} {}", kinds.0.as_str(), kinds.1.as_str()),
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
            suppression_note: report.suppression_note(),
        }
    }

    /// Attach provenance after the extractor has resolved its rule set. This
    /// also rebuilds every group label, preventing serialized `plain` text from
    /// retaining the constructor's conservative default qualification.
    pub fn with_clearance_rule_provenance(mut self, provenance: ClearanceRuleProvenance) -> Self {
        for group in self.violations.iter_mut().chain(&mut self.at_limit) {
            group.plain = group.label_with_rule_provenance(&provenance);
        }
        self
    }

    /// The combined-check section heading. A default must be visible before a
    /// reader reaches any individual finding.
    pub fn section_title(&self, provenance: &ClearanceRuleProvenance) -> &'static str {
        if provenance.is_defaulted() {
            "Copper spacing (DRC; DEFAULT rules, not your rules)"
        } else {
            "Copper spacing (DRC)"
        }
    }

    /// The flood warning is evidence about this run, not a guess at the correct
    /// replacement rule. "Exceeds" is strict: exactly 10x does not qualify.
    pub fn default_rule_flood_note(&self, provenance: &ClearanceRuleProvenance) -> Option<String> {
        let clearance = self.violations.len();
        let shorts = self.shorts.len();
        (provenance.is_defaulted() && clearance > shorts.saturating_mul(10))
        .then(|| {
            format!(
                "DEFAULT-RULE COUNT: {clearance} below-rule clearance group(s) versus {shorts} short(s) (ratio {clearance}:{shorts}); exceeding the short count by more than 10x is evidence that this default is probably wrong for this board."
            )
        })
    }

    /// Render the grouped DRC as text (the honest, de-duplicated view). Shorts
    /// first (the things that actually break a board), then below-rule groups,
    /// then the at-limit bucket (separated and labelled correctly).
    pub fn render(&self) -> String {
        self.render_with_clearance_rule_provenance(&ClearanceRuleProvenance::defaulted(
            self.clearance_rule_mm,
            false,
        ))
    }

    pub fn render_with_clearance_rule_provenance(
        &self,
        provenance: &ClearanceRuleProvenance,
    ) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        if provenance.is_defaulted() {
            let _ = writeln!(
                s,
                "DRC (DEFAULT rules, not your rules): {} primitive(s), DEFAULT clearance {:.3} mm",
                self.primitive_count, self.clearance_rule_mm
            );
        } else {
            let _ = writeln!(
                s,
                "DRC: {} primitive(s), clearance rule {:.3} mm",
                self.primitive_count, self.clearance_rule_mm
            );
        }
        let _ = writeln!(s, "{}", provenance.cli_notice());
        if let Some(notice) = provenance.custom_rules_notice() {
            let _ = writeln!(s, "{notice}");
        }
        if let Some(note) = self.default_rule_flood_note(provenance) {
            let _ = writeln!(s, "{note}");
        }
        if let Some(w) = &self.version_warning {
            let _ = writeln!(s, "\n⚠ UNRELIABLE: {w}");
        }
        // Before any "clean" claim: a suppressed class is not a checked class.
        if let Some(n) = &self.suppression_note {
            let _ = writeln!(s, "\nNOT CHECKED: {n}");
        }
        if self.shorts.is_empty() && self.violations.is_empty() && self.at_limit.is_empty() {
            let _ = writeln!(s, "no shorts or clearance violations.");
            return s;
        }
        if !self.shorts.is_empty() {
            let _ = writeln!(s, "\nSHORTS ({}):", self.shorts.len());
            for sh in &self.shorts {
                // Honor the final per-short severity from the structured form:
                // an unmatched unvalidated-format short reads "NOTE", while an
                // exact KiCad-oracle pair match is restored to "SERIOUS".
                let tag = if sh.severity == "serious" {
                    "SERIOUS"
                } else {
                    "NOTE"
                };
                // The measurement is printed identically whatever the severity:
                // a declared tie is still copper touching, and hiding or
                // softening the geometry would be the opposite dishonesty to
                // calling it a defect. The declaration is appended to it.
                let _ = writeln!(
                    s,
                    "  [{tag}] {} touches {} on {} (gap {:.4} mm) at x={:.1}, y={:.1}{}",
                    sh.net_a,
                    sh.net_b,
                    sh.layer,
                    sh.gap_mm,
                    sh.loc_mm[0],
                    sh.loc_mm[1],
                    sh.oracle_agreement()
                        .map(|agreement| format!("; {agreement}"))
                        .unwrap_or_default()
                );
            }
        }
        if !self.violations.is_empty() {
            let _ = writeln!(s, "\nCLEARANCE VIOLATIONS (below rule), grouped:");
            for g in &self.violations {
                let _ = writeln!(s, "  {}", g.label_with_rule_provenance(provenance));
            }
        }
        if !self.at_limit.is_empty() {
            let _ = writeln!(s, "\nAT MINIMUM CLEARANCE (no margin), grouped:");
            for g in &self.at_limit {
                let _ = writeln!(s, "  {}", g.label_with_rule_provenance(provenance));
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
#[derive(Debug, Clone)]
pub struct JsonFinding {
    /// Which check produced it ("si" / "lint" / "drc").
    pub check: String,
    /// The specific rule (e.g. "controlled_impedance", "strap_pin").
    pub kind: String,
    /// "serious" | "warning" | "note" | "info".
    pub severity: String,
    pub nets: Vec<String>,
    pub location_mm: Option<[f64; 2]>,
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
    pub fix: Option<String>,
}

impl JsonFinding {
    /// Whether this finding is a reason its report family fails its gate.
    ///
    /// This is derived from the stable pre-v2 public fields so adding the
    /// machine-readable wire property does not break downstream Rust struct
    /// literals. Every serious finding gates; lint warnings, all real SI
    /// findings and co-sim fault/strict-boot findings widen that rule.
    pub fn gates(&self) -> bool {
        self.severity == "serious"
            || (self.check == "lint" && self.severity == "warning")
            || (self.check == "si" && self.severity != "info")
            || self.check == "cosim"
    }
}

impl Serialize for JsonFinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("JsonFinding", 12)?;
        state.serialize_field("check", &self.check)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("severity", &self.severity)?;
        state.serialize_field("gating", &self.gates())?;
        state.serialize_field("nets", &self.nets)?;
        if let Some(location_mm) = &self.location_mm {
            state.serialize_field("location_mm", location_mm)?;
        }
        if let Some(layer) = &self.layer {
            state.serialize_field("layer", layer)?;
        }
        state.serialize_field("refs", &self.refs)?;
        state.serialize_field("actionable", &self.actionable)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("plain", &self.plain)?;
        if let Some(fix) = &self.fix {
            state.serialize_field("fix", fix)?;
        }
        state.end()
    }
}

/// One finding in the uniform machine-readable shape (§4.1): every check's
/// findings serialize the same way, so a CI pipeline or AI never parses prose.
#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct JsonFindingSchema {
    /// Which check produced it ("si" / "lint" / "drc").
    check: String,
    /// The specific rule (e.g. "controlled_impedance", "strap_pin").
    kind: String,
    /// "serious" | "warning" | "note" | "info".
    severity: String,
    /// Whether THIS finding is a reason the run fails its gate.
    ///
    /// Not a restatement of `severity`. Every `serious` finding gates, and so do
    /// findings the severity word grades lower: a medium (`warning`) lint
    /// finding, every real SI finding, every co-sim fault. It runs the other way
    /// too, so a `warning` is not a gate grade on its own: a copper short on a
    /// board format the copper extraction was never validated against may be
    /// phantom and does not gate, a `note`-grade lint finding does not gate, and
    /// neither does a qualified or demoted evidence badge.
    ///
    /// Current schema-v2 serializers always emit this additive field. It stays
    /// optional in the schema so documents emitted by earlier v2 binaries
    /// remain valid.
    gating: bool,
    nets: Vec<String>,
    location_mm: Option<[f64; 2]>,
    layer: Option<String>,
    refs: Vec<String>,
    /// Whether a user should act on this (true for real findings and for
    /// off-target info notes; false for within-tolerance observations).
    actionable: bool,
    /// The expert one-line message.
    message: String,
    /// The same finding in plain language (best-effort; equals `message` when no
    /// dedicated plain template applies).
    plain: String,
    /// A concise suggested fix, when a dedicated template applies. Closes the
    /// TUI "no fix text" gap: once `JsonFinding` carries it, `Finding::from_json`
    /// can read it instead of hard-coding `None`. Omitted (None) when no fix
    /// template applies to this finding kind.
    fix: Option<String>,
}

impl schemars::JsonSchema for JsonFinding {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JsonFinding".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = <JsonFindingSchema as schemars::JsonSchema>::json_schema(generator);
        if let Some(required) = schema
            .ensure_object()
            .get_mut("required")
            .and_then(serde_json::Value::as_array_mut)
        {
            required.retain(|field| field != "gating");
        }
        schema
    }
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
    /// changes meaning, is removed, or a closed enum gains a value; purely
    /// additive optional fields do not bump it.
    /// The generated schema lives in `crates/hauksbee-engine/schemas/`.
    pub schema_version: u32,
    pub board: String,
    pub bind: BindSummary,
    /// Whether unbound verdict-critical parts gate THIS report's verdict.
    /// True on the surfaces that make model-dependent claims (--check, --lint,
    /// --si and the default machine report), where a clean bill over an
    /// unbound critical part is vacuous and every text rendering already
    /// refuses it. False on the copper-only (--drc) and descriptive
    /// (--report) surfaces, which deliberately do not refuse: copper reads
    /// the layout, and the bind table is not a pass/fail claim. Not
    /// serialized: the gate's OUTCOME is the verdict field itself.
    #[serde(skip)]
    pub bind_gates_verdict: bool,
    /// Whether THIS surface's own `--strict` gate fails the run on findings the
    /// shared `serious` severity does not carry. Those gates are deliberately
    /// wider than `serious`: `--lint` gates on medium-severity findings, `--si`
    /// on every real finding (its informational computed-value notes excluded),
    /// and the co-sim gate on any raised fault. The findings those gates catch
    /// beyond the shared grade serialize as `warning`/`note`, which is exactly
    /// why the verdict cannot derive them from severity. Without this the same
    /// invocation printed `"verdict":"pass","ok":true` and exited 2, so a
    /// consumer gating on the document disagreed with a consumer gating on the
    /// exit code. On the routes that load waivers it is set AFTER the partition,
    /// so an overruled finding neither gates nor flips the verdict; the bare
    /// machine report and the co-sim surface load none and gate on everything
    /// they found. Not serialized: the gate's OUTCOME is the verdict field.
    #[serde(skip)]
    pub surface_gate_fails: bool,
    /// Whether this document only DESCRIBES the run (the `--report` bind table)
    /// instead of making a pass/fail claim about it. Such a surface has no
    /// `--strict` gate at all, and its subject IS the binding it prints in
    /// full, so undermined binding/coverage evidence must not become a refusal
    /// verdict here: binding completeness reaches a verdict only through the
    /// verdict-critical bind gate, which this surface is deliberately exempt
    /// from (see [`Self::bind_gates_verdict`]), and the specialist surfaces
    /// trim those per-net maps out of their verdicts for the same reason.
    /// Without this the descriptive document read `"ok":false` while the
    /// command always exited 0, so the two ways to gate on it disagreed. With
    /// no findings, no analysis section and no refusal, `--report`'s only
    /// caller leaves `verdict` constant at `pass`: on a surface that makes no
    /// pass/fail claim the rollup carries no information, and the bind summary
    /// and the evidence array (both rendered in full) are what a consumer reads
    /// instead. Not serialized: the gate's outcome is the verdict field.
    #[serde(skip)]
    pub descriptive_only: bool,
    /// Every explicitly supplied input and what it contributed to this run.
    /// Empty on older/internal call paths that have no inventory context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<JsonInputEvidence>,
    /// Exact inputs consumed by this run, content-addressed for reproducibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// First-class assumptions collected from the real reader/bind path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// Per-net assertions derived from actual board incidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
    /// Top-level refusal for a whole-run exit 3 (for example strict co-sim).
    /// AC/thermal refusals additionally live in their analysis section so a
    /// section-only consumer sees the same contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    /// The compact order-decision surface as data. The `--check --plain` text is
    /// rendered from this same object, so CI/web consumers and a terminal reader
    /// cannot receive different bucket assignments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<crate::plain::OrderTriage>,
    /// Source and report-wide value for DRC clearance rules. Pair-specific
    /// values remain in `drc.violations[*].rule_mm`; this field says whose rule
    /// those numbers are (or are not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clearance_rule_source: Option<ClearanceRuleProvenance>,
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
            bind_gates_verdict: false,
            surface_gate_fails: false,
            descriptive_only: false,
            inputs: Vec::new(),
            inventory: Vec::new(),
            assumptions: Vec::new(),
            evidence: Vec::new(),
            refusal: None,
            triage: None,
            clearance_rule_source: None,
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
    /// Mark this report as a model-dependent-claim surface: unbound
    /// verdict-critical parts flip its verdict to `invalid` (the machine
    /// mirror of the INCONCLUSIVE refusal the text surfaces print).
    pub fn with_bind_verdict_gate(mut self) -> Self {
        self.bind_gates_verdict = true;
        self
    }

    /// Tell this report that the emitting surface's own `--strict` gate fails
    /// the run, so its verdict reads `fail` instead of `pass` beside an exit 2
    /// (see [`Self::surface_gate_fails`]). Pass the surface's post-waiver gate
    /// predicate: `lint_fails`, `si_fails`, the combined `--check` gate, or the
    /// co-sim fault gate.
    pub fn with_surface_gate(mut self, fails: bool) -> Self {
        self.surface_gate_fails = fails;
        self
    }

    /// Mark this report as descriptive only, so undermined run-level evidence
    /// does not become a refusal verdict on a surface that never gates (see
    /// [`Self::descriptive_only`]).
    pub fn with_descriptive_only(mut self) -> Self {
        self.descriptive_only = true;
        self
    }

    pub fn with_inputs(mut self, inputs: &[JsonInputEvidence]) -> Self {
        self.inputs = inputs.to_vec();
        self
    }

    /// Attach the one evidence object used by every report renderer.
    pub fn with_evidence(mut self, evidence: &crate::evidence::BoardEvidence) -> Self {
        self.inventory = evidence.inventory().to_vec();
        self.assumptions = evidence.assumptions().to_vec();
        self.evidence = evidence.maps().to_vec();
        self
    }

    /// Replace the board-wide binding maps with maps for the actual findings
    /// already attached to this report, KEEPING the run-level maps in
    /// `run_level` (input coverage and its kin). The replacement must not
    /// silently drop them: the verdict rests on run-level maps, and a
    /// specialist surface that swapped them out for finding-backed maps alone
    /// would read `pass` over an undermined coverage claim, because
    /// finding-backed maps carry their own badges without vetoing the run.
    pub fn attach_finding_evidence(
        &mut self,
        evidence: &crate::evidence::BoardEvidence,
        run_level: Vec<hauksbee_ir::evidence::EvidenceMap>,
    ) -> Result<(), hauksbee_ir::evidence::EvidenceError> {
        if let Some(findings) = &self.findings {
            let mut maps = evidence.maps_for_findings(findings)?;
            maps.extend(run_level);
            // Always replace, even with an empty result: leaving the per-net
            // binding-completeness maps in place would let ANY unresolved
            // passive invalidate a clean specialist report, when binding
            // completeness enters these verdicts only through the
            // verdict-critical bind gate.
            self.evidence = maps;
        }
        Ok(())
    }

    /// A top-level machine verdict computed from the populated sections, so a CI
    /// consumer can read pass/fail without re-deriving it from every finding.
    /// Returns `(ok, verdict, serious_count, actionable_count)` where `verdict`
    /// is `"pass"` | `"fail"` | `"invalid"`:
    ///   - `fail`, at least one serious finding (a DRC short, a destroyed part
    ///     in co-sim, a high-severity lint/SI finding), or a finding the
    ///     emitting surface's own `--strict` gate fails on where that gate is
    ///     wider than `serious` (see [`Self::surface_gate_fails`]);
    ///   - `invalid`, nothing gates, but the run-level claim could not be
    ///     judged: a refusal, AC or thermal reported `valid:false`, or a
    ///     run-level evidence map (input coverage, bind completeness) is
    ///     undermined. Undermined maps backing individual findings do NOT
    ///     invalidate the run; the finding stays on the report wearing its own
    ///     badge;
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
                // Waiving a note would suppress information without changing
                // any outcome, which is cost with no benefit, so only the
                // `serious` grade is offered a waiver here. That is narrower
                // than `JsonFinding::gates`: this path counts `serious_count`,
                // and the surfaces whose gate is wider apply their waivers
                // upstream, before a finding reaches this document (see
                // `reports::check::gather_findings`).
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
                    // A board-authorized contact or an unvalidated-format phantom
                    // may be non-serious. Keep the aggregate verdict aligned with
                    // the per-finding and artifact severities rather than
                    // re-deriving intent here.
                    if s.severity != "serious" {
                        continue;
                    }
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
        // Which undermined evidence flips the verdict to `invalid`: the maps
        // the RUN-LEVEL claim rests on (input coverage, bind completeness,
        // analysis validity), not the maps backing individual findings. A
        // finding is an observation the report already surfaces with its own
        // qualified/undermined badge; an undermined heads-up note means THAT
        // note's magnitude is uncertain, and it stays on the report saying so.
        // It does not mean the run could not be judged: "0 serious" rests on
        // the coverage maps and the serious-finding set, both of which keep
        // their own undermined routes to `invalid`. Without this split, one
        // unparseable value field anywhere on the board invalidated the whole
        // run through whichever informational note touched its net.
        let finding_assertions: std::collections::HashSet<&str> = self
            .findings
            .iter()
            .flatten()
            .map(|f| f.message.as_str())
            .collect();
        let evidence_undermined = !self.descriptive_only
            && run_level_undermined(&self.evidence, |a| finding_assertions.contains(a));
        // The INCONCLUSIVE bind contract on the machine surface: a clean
        // result over an unbound verdict-critical part is vacuous, and the
        // model-dependent-claim surfaces' text renderings already refuse the
        // clean bill for it, so their JSON verdict must refuse it too rather
        // than reading "pass" beside an INCONCLUSIVE coverage note. Gated by
        // `bind_gates_verdict`: the copper-only and descriptive surfaces
        // deliberately do not refuse (see the field's doc).
        let bind_blockers =
            self.bind_gates_verdict && !unmodelled_critical_refs(&self.bind).is_empty();
        let invalid = self.refusal.is_some()
            || bind_blockers
            || self.ac.as_ref().is_some_and(|a| !a.validity.valid)
            || self.thermal.as_ref().is_some_and(|t| !t.validity.valid)
            || evidence_undermined;
        // `fail` covers both routes to a failing gate: a serious finding, and a
        // finding this surface's own (wider) strict gate fails on. The second
        // route is what keeps the verdict field from reading `pass` in the very
        // document a `--strict` exit 2 was printed beside.
        let verdict = if serious > 0 || self.surface_gate_fails {
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
        LintCheck::MissingSdPullup => {
            "Add a 10k-100k pull-up from the SD line to the card's supply rail (the SD spec asks for one on CMD and each DAT line)."
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
        LintCheck::OperatingEnvelope => {
            "Move the pin to a compatible rail, or change the rail so its full operating range satisfies the quoted datasheet condition."
        }
        LintCheck::BackPower => {
            "Pull the pin up to the part's own supply rail instead, or put a proper level shifter between the domains; if the pin is documented higher-voltage-tolerant, note that in the design."
        }
        LintCheck::I2cBusLoading => {
            "Re-pick the pull-ups so the sink current stays under 3 mA (R > (Vrail - 0.4 V) / 3 mA) and the rise time fits the bus speed; 2.2k-4.7k suits most 3.3 V buses."
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{DrcFinding, Item, ItemKind};

    #[test]
    fn doorbell_custom_rule_provenance_names_override_and_coverage_gap() {
        let rules = r#"(version 1)
(rule "fine-pitch routing clearance"
  (constraint clearance (min 0.127mm)))
(rule "via/NPTH hole-to-copper clearance"
  (constraint hole_clearance (min 0.2mm)))
(rule "PTH hole-to-copper clearance"
  (condition "A.Pad_Type == 'Through Hole Pad'")
  (constraint hole_clearance (min 0.28mm)))
(rule "board-edge copper clearance"
  (constraint edge_clearance (min 0.3mm)))
"#;
        let parsed = hauksbee_extract::parse_kicad_dru(rules).unwrap();
        let coverage = CustomRulesCoverage::from_parsed("doorbell.kicad_dru".into(), &parsed);
        let provenance = ClearanceRuleProvenance::custom_rules_file(0.127, Some(0.2), coverage);
        assert_eq!(
            provenance.source_notice(),
            "CLEARANCE RULE SOURCE: custom rules file doorbell.kicad_dru (report-wide value 0.127 mm), which overrides the .kicad_pro netclass value of 0.200 mm."
        );
        let notice = provenance.custom_rules_notice().unwrap();
        assert!(notice.contains("CUSTOM RULES NOT EVALUATED: 1 rule"));
        assert!(notice.contains("PTH hole-to-copper clearance"));
        assert!(notice.contains("do not qualify reported findings"));
        let not_covered = provenance.custom_rules_not_covered().unwrap();
        assert!(not_covered.contains("via/NPTH hole-to-copper clearance"));
        assert!(not_covered.contains("board-edge copper clearance"));
    }

    #[test]
    fn bare_value_rule_uses_custom_rule_disclosure_without_qualifying_findings() {
        let parsed = hauksbee_extract::parse_kicad_dru(include_str!(
            "../../hauksbee-extract/tests/fixtures/kicad_dru_bare_scope.kicad_dru"
        ))
        .unwrap();
        let coverage = CustomRulesCoverage::from_parsed("scope.kicad_dru".into(), &parsed);
        assert!(coverage.file_inactive_due_to_bare_values);
        assert_eq!(coverage.unevaluated_rules.len(), 2);
        let bare = &coverage.unevaluated_rules[0];
        assert_eq!(bare.name, "bare rule");
        assert_eq!(bare.line_number, 3);
        assert_eq!(bare.bare_value_constraint_types, ["clearance"]);
        assert!(!coverage.qualifies_clearance_findings());

        let notice = coverage.unevaluated_notice().unwrap();
        assert!(notice.starts_with(
            "CUSTOM RULES DISABLED: scope.kicad_dru is not in force. Rule \"bare rule\" on line 3 has a value with no unit"
        ));
        assert!(notice.contains("all 2 rules in it are inactive"));
        assert!(notice.contains("being checked against netclass defaults instead"));
        assert!(notice.contains("Add the unit to restore the rest."));
    }

    /// No routing marker may reach the JSON surface.
    ///
    /// The binder has one `reason` channel per row and two things to say when
    /// `db/unmodelled.toml` names an abstention: what blocks the model, and what
    /// would unlock it. It joins them with `UNLOCKED_BY_MARKER`, and every consumer
    /// is expected to split. `Assumption::open_part` does. This surface used to copy
    /// the string verbatim, so `--json`, the one a CI pipeline parses, was the only
    /// reader handed the marker text and the two halves re-merged into one field.
    ///
    /// Both markers are checked, because there are two and the rule is the same for
    /// each: they are plumbing between the binder and the report builders.
    #[test]
    fn no_routing_marker_reaches_the_json_surface() {
        let unlocked = "the strap state for this board, or a schematic naming the pin";
        let mut report = BindReport::default();
        report.push(BindRow {
            reference: "U201".to_string(),
            value: "Si53301".to_string(),
            model_id: None,
            confidence: Confidence::Unresolved,
            source: None,
            outcome: BindOutcome::Unresolved {
                reason: format!(
                    "the output format is strap-selected and it is the driven level \
                     that is not known{}{unlocked}",
                    Assumption::UNLOCKED_BY_MARKER
                ),
            },
            // A `warning` is what puts the row on the active path at all.
            warning: Some("U201 (Si53301): active part left open".to_string()),
            guesses: Vec::new(),
        });

        let summary = BindSummary::from_report(&report);
        let row = summary
            .active_path_unresolved
            .first()
            .expect("an unresolved active part is reported");

        assert!(
            !row.reason.contains(Assumption::UNLOCKED_BY_MARKER.trim()),
            "the marker is plumbing and must not reach the reader: {}",
            row.reason
        );
        assert!(
            !row.reason.contains(unlocked),
            "the unlocking input belongs in its own field, not glued onto the \
             reason: {}",
            row.reason
        );
        assert_eq!(
            row.unlocked_by.as_deref(),
            Some(unlocked),
            "the actionable half must survive the split, in its own field"
        );

        // And an ordinary reason with no marker passes through whole.
        let mut plain = BindReport::default();
        plain.push(BindRow {
            reference: "U9".to_string(),
            value: "MYSTERY".to_string(),
            model_id: None,
            confidence: Confidence::Unresolved,
            source: None,
            outcome: BindOutcome::Unresolved {
                reason: "no model matched".to_string(),
            },
            warning: Some("U9 (MYSTERY): active part left open".to_string()),
            guesses: Vec::new(),
        });
        let plain = BindSummary::from_report(&plain);
        let row = plain.active_path_unresolved.first().expect("reported");
        assert_eq!(row.reason, "no model matched");
        assert!(row.unlocked_by.is_none(), "nothing to unlock, nothing said");
    }

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
                refusal: None,
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
            between: "track \u{2194} via".into(),
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
            unlocked_by: None,
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
        assert!(
            banner.contains(
                "Critical active devices discovered: 1; executable behavioural models: 1/1"
            ),
            "the banner must distinguish CAD discovery from behavioural coverage: {banner}"
        );
        assert!(
            !banner.contains("Critical parts modelled"),
            "the old label made behavioural coverage look like extraction coverage: {banner}"
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
            zone_pad_overlaps_suppressed: Some(0),
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

    fn declared_qualification(report: &DrcReport) -> hauksbee_extract::DrcTieQualification {
        report.qualify_with_declared_ties(
            "emonTx V3.4.5.sch",
            &[hauksbee_extract::DeclaredNetTie {
                net: "GND".into(),
                tied_net: "AGND".into(),
                symbol: "AGND7".into(),
                tied_to: vec!["SUPPLY6".into()],
            }],
        )
    }

    fn eagle_report(findings: Vec<DrcFinding>) -> DrcReport {
        DrcReport {
            clearance_mm: 0.2,
            findings,
            primitive_count: 2,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
        }
    }

    #[test]
    fn a_schematic_only_tie_stays_serious_and_states_the_context() {
        let report = eagle_report(vec![short("GND", "AGND")]);
        let qualification = declared_qualification(&report);

        let st = DrcStructured::from_report_with_ties(&report, Some(&qualification), false);
        assert_eq!(st.shorts.len(), 1, "the finding is not deleted");
        assert_eq!(st.shorts[0].severity, "serious");

        // The geometry claim must survive into the text a user reads. Both nets
        // named, and the word that says they are connected.
        let plain = &st.shorts[0].plain;
        assert!(plain.contains("GND") && plain.contains("AGND"), "{plain}");
        assert!(
            plain.contains("joined in copper"),
            "the contact must still be stated: {plain}"
        );
        assert!(
            plain.contains("AGND7 wired to SUPPLY6 in net GND"),
            "and the declaration must name the symbols: {plain}"
        );
        assert!(
            plain.contains("emonTx V3.4.5.sch"),
            "and cite the file: {plain}"
        );
        // The schematic lacks a coordinate, so the fix must request board-local
        // authority or separation rather than silently excusing the contact.
        assert!(
            st.shorts[0].fix.contains("separate"),
            "a schematic-only declaration must retain an actionable fix: {}",
            st.shorts[0].fix
        );

        // The rendered report keeps the measurement line verbatim and appends the
        // declaration, so the copper is visible on the text surface too.
        let rendered = st.render();
        assert!(
            rendered.contains("[SERIOUS] GND touches AGND on F.Cu"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[SERIOUS] GND touches AGND"),
            "{rendered}"
        );
    }

    #[test]
    fn without_a_schematic_the_short_stays_serious_and_names_the_upload() {
        let report = eagle_report(vec![short("GND", "AGND")]);

        let st = DrcStructured::from_report_with_ties(&report, None, true);
        assert_eq!(st.shorts[0].severity, "serious");
        // The abstention rule: a finding that cannot be settled from this input
        // must name the input that settles it.
        assert!(
            st.shorts[0].fix.contains(".sch"),
            "the fix must name the unlocking upload: {}",
            st.shorts[0].fix
        );

        // And the plain-language surface carries it too.
        let plain = crate::plain::plain_drc_structured(&st).render();
        assert!(plain.contains(".sch"), "{plain}");
    }

    #[test]
    fn schematic_context_does_not_downgrade_either_short() {
        let report = eagle_report(vec![short("GND", "AGND"), short("+5V", "VBAT")]);
        let qualification = declared_qualification(&report);

        let st = DrcStructured::from_report_with_ties(&report, Some(&qualification), false);
        assert_eq!(st.shorts.len(), 2);
        let by_pair: std::collections::BTreeMap<&str, &str> = st
            .shorts
            .iter()
            .map(|s| (s.net_b.as_str(), s.severity.as_str()))
            .collect();
        assert_eq!(by_pair["AGND"], "serious");
        assert_eq!(by_pair["VBAT"], "serious");
    }

    use crate::report::{BindOutcome, BindRow};
    use hauksbee_models::Confidence;

    fn row(reference: &str, outcome: BindOutcome, warning: Option<&str>) -> BindRow {
        BindRow {
            reference: reference.to_string(),
            value: String::new(),
            model_id: None,
            confidence: Confidence::Exact,
            source: None,
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
