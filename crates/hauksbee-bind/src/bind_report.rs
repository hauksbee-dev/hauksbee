//! The bind report: one row per board component recording what model it
//! resolved to and what it became in the simulation.
//!
//! [`BindReport`] holds those rows; [`BindOutcome`] is the per-component
//! verdict (an analog or behavioral device, a digital or MCU block, a power
//! rail, a deliberately-skipped part, or an unresolved open circuit) and the
//! report aggregates them for the resolve-rate stats. [`BindSummary`] is the
//! role-aware roll-up (did the MCU and the active ICs bind, which active-path
//! parts are open) that every renderer and the JSON report share.
//!
//! [`BindReport::render_table`] and [`BindReport::render_table_compact`] are the
//! only text renderings of those rows; the compact form collapses bulk fallback
//! passives and `--verbose` restores every component row. The `--report`
//! surface that prints the table lives upstairs in
//! `hauksbee_engine::reports::bind`.

use hauksbee_ir::evidence::{Assumption, ModelSource, ModelSourceTier};
use hauksbee_models::Confidence;
use serde::Serialize;

/// What a component turned into during binding.
#[derive(Debug, Clone, PartialEq)]
pub enum BindOutcome {
    /// Stamped into the MNA circuit as one or more analog IR devices.
    Analog { device: String },
    /// A behavioral block stamped analog (opamp/comparator/analog switch/vreg).
    Behavioral { device: String },
    /// An event-driven digital component handled outside MNA.
    Digital { kind: String },
    /// An emulated microcontroller core.
    Mcu { backend: String },
    /// A power rail attached as an ideal source.
    PowerRail { volts: f64 },
    /// Deliberately ignored (mounting hole, fiducial, connector...).
    Skipped { reason: String },
    /// Could not be resolved or stamped; left as an open circuit.
    Unresolved { reason: String },
}

/// Whether a stamped device kind is an ACTIVE part, whose generic fallback
/// model would carry invented breakdown/current/power ratings that a stress
/// verdict can cite. Passives (resistor, capacitor, inductor) are excluded: their
/// fallbacks carry no such ratings and bind on nearly every board.
pub fn is_active_fallback_device(device: &str) -> bool {
    let d = device.to_ascii_lowercase();
    [
        "nmos",
        "pmos",
        "mosfet",
        "fet",
        "jfet",
        "bjt",
        "npn",
        "pnp",
        "diode",
        "zener",
        "led",
        "vreg",
        "regulator",
        "opamp",
        "comparator",
        "switch",
    ]
    .iter()
    .any(|k| d.contains(k))
}

impl BindOutcome {
    /// The one-line label the bind table and the TUI detail line show.
    ///
    /// A named abstention's `reason` arrives with its two halves joined by
    /// `UNLOCKED_BY_MARKER`; the marker is stripped here so no reader surface
    /// shows the plumbing.
    pub fn label(&self) -> String {
        match self {
            BindOutcome::Analog { device } => format!("analog {device}"),
            BindOutcome::Behavioral { device } => format!("behavioral {device}"),
            BindOutcome::Digital { kind } => format!("digital {kind}"),
            BindOutcome::Mcu { backend } => format!("mcu {backend}"),
            BindOutcome::PowerRail { volts } => format!("rail {volts:.2}V"),
            BindOutcome::Skipped { reason } => format!("skipped ({reason})"),
            BindOutcome::Unresolved { reason } => {
                let reason = reason
                    .split_once(hauksbee_ir::evidence::Assumption::UNLOCKED_BY_MARKER)
                    .map_or(reason.as_str(), |(because, _)| because.trim());
                format!("UNRESOLVED ({reason})")
            }
        }
    }

    /// Whether this outcome counts as a "resolved" component (for stats).
    pub fn is_resolved(&self) -> bool {
        !matches!(self, BindOutcome::Unresolved { .. })
    }

    /// Whether this outcome was deliberately ignored (excluded from resolve %).
    pub fn is_ignored(&self) -> bool {
        matches!(self, BindOutcome::Skipped { .. })
    }
}

/// One reported component.
#[derive(Debug, Clone)]
pub struct BindRow {
    pub reference: String,
    pub value: String,
    pub model_id: Option<String>,
    pub confidence: Confidence,
    /// Canonical model source, validation and uncertainty. `None` only for a
    /// skipped/unresolved row or a synthetic test row that predates evidence.
    pub source: Option<ModelSource>,
    pub outcome: BindOutcome,
    /// Set when a connected analog part failed to resolve (loud warning).
    pub warning: Option<String>,
    /// Pin-role GUESS warnings: one per pad whose role the binder inferred from
    /// the pin-rule table (not an explicit schematic pin-function). Each names
    /// the pad, the guessed role, and the rule that matched, so nothing is
    /// silently guessed. Empty when every role was explicit.
    pub guesses: Vec<String>,
}

/// The full report from one bind pass.
#[derive(Debug, Clone, Default)]
pub struct BindReport {
    pub rows: Vec<BindRow>,
    pub board_name: String,
}

impl BindReport {
    pub fn push(&mut self, row: BindRow) {
        self.rows.push(row);
    }

    /// One warning per **active** component whose device parameters and safety
    /// ratings come from a generic estimated-fallback model rather than a real
    /// part.
    ///
    /// These entries exist so an unmodeled power FET in a recognised package
    /// binds to something and can be simulated at all, but their numbers are
    /// invented: the generic power-FET catch-alls carry a nominal 30 V / 20 A /
    /// 30 W and a 20 mOhm Rds_on that belong to no datasheet. Naming them puts
    /// the invented basis of any verdict resting on them on the record.
    ///
    /// Scoped to active devices ([`is_active_fallback_device`]) on purpose.
    /// Generic passive fallbacks (`c_fallback`, `r_fallback`) carry no invented
    /// breakdown ratings for a verdict to rest on, and they bind on nearly every
    /// board, so warning about them would bury this channel in noise. Their
    /// provenance is already on the evidence block.
    ///
    /// Deduped and order-stable, so this can be chained straight into a CI
    /// report's `coverage_warnings`. Empty on a board with no fallback binding.
    pub fn estimated_fallback_warnings(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        self.non_ignored()
            .filter(|r| {
                r.source
                    .as_ref()
                    .is_some_and(|s| s.tier() == ModelSourceTier::EstimatedFallback)
            })
            .filter(|r| match &r.outcome {
                BindOutcome::Analog { device } | BindOutcome::Behavioral { device } => {
                    is_active_fallback_device(device)
                }
                _ => false,
            })
            .map(|r| (r.model_id.as_deref().unwrap_or("(unnamed)"), &r.reference))
            .fold(
                std::collections::BTreeMap::<&str, Vec<&String>>::new(),
                |mut acc, (model, reference)| {
                    let refs = acc.entry(model).or_default();
                    if !refs.contains(&reference) {
                        refs.push(reference);
                    }
                    acc
                },
            )
            .into_iter()
            .map(|(model, mut refs)| {
                // Aggregated per MODEL, not per part: a motor driver board with
                // eight identical unmodeled FETs is one hole with eight instances,
                // and eight near-identical notes is how this channel gets ignored.
                refs.sort();
                const SHOWN: usize = 5;
                let listed = refs
                    .iter()
                    .take(SHOWN)
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let rest = refs.len().saturating_sub(SHOWN);
                let named = if rest > 0 {
                    format!("{listed} and {rest} more")
                } else {
                    listed
                };
                format!(
                    "models: {} part(s) ({}) are bound to the generic fallback model '{}', \
                     whose ratings and device parameters are estimates for the package class, \
                     not values from any datasheet. Any verdict citing them rests on invented \
                     numbers. Add a model entry matching the part (value_re or mpn_re), or put \
                     the manufacturer part number on the component, to replace them.",
                    refs.len(),
                    named,
                    model,
                )
            })
            .filter(|m| seen.insert(m.clone()))
            .collect()
    }

    /// Components that are not deliberately ignored.
    pub fn non_ignored(&self) -> impl Iterator<Item = &BindRow> {
        self.rows.iter().filter(|r| !r.outcome.is_ignored())
    }

    /// Count of non-ignored components.
    pub fn non_ignored_count(&self) -> usize {
        self.non_ignored().count()
    }

    /// Count of non-ignored components that actually resolved to a device.
    ///
    /// Keyed on the bind OUTCOME, not the model-match confidence: a part whose
    /// model matched (confidence Exact/High) but was then left `Unresolved`
    /// (e.g. an open diode whose pins are not both connected) is NOT resolved,
    /// so the "N of M resolved" headline agrees with the open parts listed
    /// below it.
    pub fn resolved_count(&self) -> usize {
        self.non_ignored()
            .filter(|r| r.outcome.is_resolved())
            .count()
    }

    /// Fraction of non-ignored components that resolved (0.0..=1.0).
    pub fn resolved_fraction(&self) -> f64 {
        let n = self.non_ignored_count();
        if n == 0 {
            return 1.0;
        }
        self.resolved_count() as f64 / n as f64
    }

    /// Count outcomes of a given variant discriminant via a predicate.
    pub fn count_where(&self, pred: impl Fn(&BindOutcome) -> bool) -> usize {
        self.rows.iter().filter(|r| pred(&r.outcome)).count()
    }

    pub fn mcu_count(&self) -> usize {
        self.count_where(|o| matches!(o, BindOutcome::Mcu { .. }))
    }

    /// Count of components bound as shift registers specifically.
    pub fn shift_register_count(&self) -> usize {
        self.count_where(|o| matches!(o, BindOutcome::Digital { kind } if kind == "shift_register"))
    }

    /// Total components bound into the event-driven digital layer.
    pub fn digital_count(&self) -> usize {
        self.count_where(|o| matches!(o, BindOutcome::Digital { .. }))
    }

    /// All warnings raised during binding.
    ///
    /// `Assumption::PARTIAL_MODEL_MARKER` is stripped here: it is routing for
    /// `BoardEvidence::from_bound`, which lifts a partial-model warning into an
    /// assumption so it reaches `--plain` and `--json` too, and it has no business
    /// in the printed line.
    pub fn warnings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rows.iter().filter_map(|r| {
            let w = r.warning.as_deref()?;
            Some((
                r.reference.as_str(),
                w.strip_prefix(hauksbee_ir::evidence::Assumption::PARTIAL_MODEL_MARKER)
                    .unwrap_or(w),
            ))
        })
    }

    /// Every pin-role GUESS warning: `(reference, message)` for each pad whose
    /// role was inferred from a pin-rule rather than an explicit pin-function.
    pub fn guess_warnings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rows.iter().flat_map(|r| {
            r.guesses
                .iter()
                .map(move |g| (r.reference.as_str(), g.as_str()))
        })
    }

    /// Render a Unicode box-drawing table of every row.
    pub fn render_table(&self) -> String {
        self.render_table_with_bulk_passives(true)
    }

    /// Render the bind table for a person asking what still needs attention.
    /// Generic R/C/L fallbacks are sound enough to account for in the summary,
    /// but printing hundreds of them ahead of the active devices hides the
    /// answer. [`Self::render_table`] remains the full form.
    pub fn render_table_compact(&self) -> String {
        self.render_table_with_bulk_passives(false)
    }

    fn render_table_with_bulk_passives(&self, include_bulk_passives: bool) -> String {
        let mut refs = "Ref".to_string();
        let mut vals = "Value".to_string();
        let mut models = "Model".to_string();
        let mut confs = "Conf".to_string();
        let mut outs = "Became".to_string();

        // Compute column widths.
        let mut w_ref = refs.len();
        let mut w_val = vals.len();
        let mut w_mod = models.len();
        let mut w_conf = confs.len();
        let mut w_out = outs.len();

        // Deterministic, human-scannable order: natural sort on the reference
        // (R2 before R10), whatever order the board file listed the parts in.
        let mut sorted_rows: Vec<&BindRow> = self
            .rows
            .iter()
            .filter(|row| include_bulk_passives || bulk_fallback_passive_kind(row).is_none())
            .collect();
        if include_bulk_passives {
            sorted_rows.sort_by_key(|r| natural_ref_key(&r.reference));
        } else {
            sorted_rows.sort_by_key(|row| {
                (
                    !matches!(&row.outcome, BindOutcome::Unresolved { .. }),
                    natural_ref_key(&row.reference),
                )
            });
        }
        let cells: Vec<(String, String, String, String, String)> = sorted_rows
            .iter()
            .map(|r| {
                let model = r.model_id.clone().unwrap_or_else(|| "-".to_string());
                let val = truncate(&r.value, 18);
                let out = truncate(&r.outcome.label(), 34);
                w_ref = w_ref.max(r.reference.len());
                w_val = w_val.max(val.len());
                w_mod = w_mod.max(model.len());
                w_conf = w_conf.max(r.confidence.to_string().len());
                w_out = w_out.max(out.len());
                (
                    r.reference.clone(),
                    val,
                    model,
                    r.confidence.to_string(),
                    out,
                )
            })
            .collect();

        let pad = |s: &str, w: usize| format!("{:<width$}", s, width = w);

        let mut out = String::new();
        let top = format!(
            "┌─{}─┬─{}─┬─{}─┬─{}─┬─{}─┐\n",
            "─".repeat(w_ref),
            "─".repeat(w_val),
            "─".repeat(w_mod),
            "─".repeat(w_conf),
            "─".repeat(w_out),
        );
        out.push_str(&top);
        // Header.
        refs = pad(&refs, w_ref);
        vals = pad(&vals, w_val);
        models = pad(&models, w_mod);
        confs = pad(&confs, w_conf);
        outs = pad(&outs, w_out);
        out.push_str(&format!(
            "│ {refs} │ {vals} │ {models} │ {confs} │ {outs} │\n"
        ));
        out.push_str(&format!(
            "├─{}─┼─{}─┼─{}─┼─{}─┼─{}─┤\n",
            "─".repeat(w_ref),
            "─".repeat(w_val),
            "─".repeat(w_mod),
            "─".repeat(w_conf),
            "─".repeat(w_out),
        ));
        for (r, v, m, c, o) in &cells {
            out.push_str(&format!(
                "│ {} │ {} │ {} │ {} │ {} │\n",
                pad(r, w_ref),
                pad(v, w_val),
                pad(m, w_mod),
                pad(c, w_conf),
                pad(o, w_out),
            ));
        }
        out.push_str(&format!(
            "└─{}─┴─{}─┴─{}─┴─{}─┴─{}─┘\n",
            "─".repeat(w_ref),
            "─".repeat(w_val),
            "─".repeat(w_mod),
            "─".repeat(w_conf),
            "─".repeat(w_out),
        ));

        if !include_bulk_passives {
            let mut capacitors = 0usize;
            let mut resistors = 0usize;
            let mut inductors = 0usize;
            for row in &self.rows {
                match bulk_fallback_passive_kind(row) {
                    Some("capacitor") => capacitors += 1,
                    Some("resistor") => resistors += 1,
                    Some("inductor") => inductors += 1,
                    Some(_) | None => {}
                }
            }
            let total = capacitors + resistors + inductors;
            if total > 0 {
                let mut kinds = Vec::new();
                if capacitors > 0 {
                    kinds.push(format!(
                        "{capacitors} capacitor{}",
                        if capacitors == 1 { "" } else { "s" }
                    ));
                }
                if resistors > 0 {
                    kinds.push(format!(
                        "{resistors} resistor{}",
                        if resistors == 1 { "" } else { "s" }
                    ));
                }
                if inductors > 0 {
                    kinds.push(format!(
                        "{inductors} inductor{}",
                        if inductors == 1 { "" } else { "s" }
                    ));
                }
                out.push_str(&format!(
                    "{total} passives bound by footprint/value fallback: {}. Use --verbose to show every bind row.\n",
                    kinds.join(", ")
                ));
            }
        }

        // Summary line.
        let guess_count = self.guess_warnings().count();
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("{n} {one}")
            } else {
                format!("{n} {many}")
            }
        };
        out.push_str(&format!(
            "\n{} of {} non-ignored components resolved ({:.0}%); {}, {} digital, {}, {}\n",
            self.resolved_count(),
            self.non_ignored_count(),
            self.resolved_fraction() * 100.0,
            plural(self.mcu_count(), "MCU", "MCUs"),
            self.count_where(|o| matches!(o, BindOutcome::Digital { .. })),
            plural(self.warnings().count(), "warning", "warnings"),
            plural(guess_count, "pin-role guess", "pin-role guesses"),
        ));
        for (r, w) in self.warnings() {
            out.push_str(&format!("  ⚠ {r}: {w}\n"));
        }
        for (r, g) in self.guess_warnings() {
            out.push_str(&format!("  ? {r}: {g}\n"));
        }
        // Legend for the Conf column, shown only when a row is not `exact` so a
        // first-timer knows whether to worry about a `guessed`/`family` row.
        let any_inexact = self
            .rows
            .iter()
            .any(|r| r.confidence.to_string() != "exact");
        if any_inexact {
            out.push_str(
                "Conf: exact = matched a specific model; family = matched its part \
                 family; guessed = value/kind inferred (usually fine for passives).\n",
            );
        }
        out
    }
}

/// The only rows the compact table hides. Model id and source tier must both
/// agree: an explicitly supplied capacitor model is non-trivial and remains
/// visible, as does every active/current-program device even when guessed.
fn bulk_fallback_passive_kind(row: &BindRow) -> Option<&'static str> {
    if row
        .source
        .as_ref()
        .is_none_or(|source| source.tier() != ModelSourceTier::EstimatedFallback)
    {
        return None;
    }
    match row.model_id.as_deref() {
        Some("c_fallback") => Some("capacitor"),
        Some("r_fallback") => Some("resistor"),
        Some("l_fallback") => Some("inductor"),
        _ => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Natural sort key for a reference designator: alpha prefix (case-folded),
/// then the numeric part as a NUMBER (so R2 sorts before R10), then any
/// remaining suffix. Shared by every user-facing table that lists parts.
pub fn natural_ref_key(reference: &str) -> (String, u64, String) {
    let prefix: String = reference
        .chars()
        .take_while(|c| !c.is_ascii_digit())
        .collect();
    let rest = &reference[prefix.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let suffix = rest[digits.len()..].to_string();
    (
        prefix.to_ascii_uppercase(),
        digits.parse().unwrap_or(0),
        suffix,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Bind summary by ROLE
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
                // every consumer splits it, so no reader (least of all the JSON a
                // CI pipeline parses) ever sees the marker text.
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
    pub fn active_ic_unresolved(&self) -> impl Iterator<Item = &UnresolvedActive> {
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
    sorted.sort_by_key(|u| natural_ref_key(&u.reference));
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
pub fn is_active_ic_ref(reference: &str) -> bool {
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
pub fn is_tool_generated_ref(reference: &str) -> bool {
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
pub fn is_open_pin_warning(warning: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::evidence::{ModelSource, ModelSourceTier};

    fn row(reference: &str, confidence: Confidence, outcome: BindOutcome) -> BindRow {
        BindRow {
            reference: reference.to_string(),
            value: String::new(),
            model_id: None,
            confidence,
            source: None,
            outcome,
            warning: None,
            guesses: Vec::new(),
        }
    }

    /// A bind row carrying a model source at `tier`.
    fn sourced_row(reference: &str, model_id: &str, tier: ModelSourceTier) -> BindRow {
        use hauksbee_ir::evidence::{ModelLayer, ModelUncertainty, ModelValidation};
        BindRow {
            reference: reference.to_string(),
            value: "NMOS".to_string(),
            model_id: Some(model_id.to_string()),
            confidence: Confidence::Exact,
            source: Some(
                ModelSource::new(
                    tier,
                    ModelLayer::Builtin,
                    "mosfet.toml",
                    ModelValidation::Unvalidated,
                    vec![ModelUncertainty::Unknown {
                        parameter: "rds_on".to_string(),
                        reason: "generic package-class estimate".to_string(),
                    }],
                )
                .expect("valid source"),
            ),
            outcome: BindOutcome::Analog {
                device: "nmos".into(),
            },
            warning: None,
            guesses: Vec::new(),
        }
    }

    #[test]
    fn an_estimated_fallback_binding_raises_a_named_warning() {
        let mut report = BindReport::default();
        report.push(sourced_row(
            "Q3",
            "generic_nmos_power_pkg",
            ModelSourceTier::EstimatedFallback,
        ));
        let warnings = report.estimated_fallback_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let w = &warnings[0];
        assert!(w.contains("Q3"), "names the part: {w}");
        assert!(w.contains("generic_nmos_power_pkg"), "names the model: {w}");
    }

    #[test]
    fn generic_passive_fallbacks_do_not_flood_the_channel() {
        // c_fallback / r_fallback bind on nearly every board and carry no
        // invented breakdown ratings for a verdict to rest on, so warning about
        // them would bury the FET case this exists for.
        let mut report = BindReport::default();
        let mut cap = sourced_row("C1", "c_fallback", ModelSourceTier::EstimatedFallback);
        cap.outcome = BindOutcome::Analog {
            device: "capacitor".into(),
        };
        report.push(cap);
        let mut res = sourced_row("R1", "r_fallback", ModelSourceTier::EstimatedFallback);
        res.outcome = BindOutcome::Analog {
            device: "resistor".into(),
        };
        report.push(res);
        assert!(report.estimated_fallback_warnings().is_empty());
    }

    #[test]
    fn compact_bind_table_rolls_up_only_bulk_fallback_passives() {
        let mut report = BindReport::default();
        let mut cap = sourced_row("C1", "c_fallback", ModelSourceTier::EstimatedFallback);
        cap.outcome = BindOutcome::Analog {
            device: "capacitor".into(),
        };
        report.push(cap);
        let mut res = sourced_row("R1", "r_fallback", ModelSourceTier::EstimatedFallback);
        res.outcome = BindOutcome::Analog {
            device: "resistor".into(),
        };
        report.push(res);
        report.push(row(
            "U1",
            Confidence::Unresolved,
            BindOutcome::Unresolved {
                reason: "no model".into(),
            },
        ));

        let compact = report.render_table_compact();
        assert!(
            compact.contains("U1") && compact.contains("UNRESOLVED"),
            "{compact}"
        );
        assert!(
            !compact.contains("│ C1 ") && !compact.contains("│ R1 "),
            "{compact}"
        );
        assert!(
            compact
                .contains("2 passives bound by footprint/value fallback: 1 capacitor, 1 resistor"),
            "{compact}"
        );
        let full = report.render_table();
        assert!(full.contains("│ C1 ") && full.contains("│ R1 "), "{full}");
    }

    #[test]
    fn many_parts_on_one_fallback_model_make_one_warning() {
        // Eight identical unmodeled FETs is one coverage hole with eight
        // instances, not eight holes.
        let mut report = BindReport::default();
        for i in 1..=8 {
            report.push(sourced_row(
                &format!("Q{i}"),
                "generic_nmos_power_pkg",
                ModelSourceTier::EstimatedFallback,
            ));
        }
        let w = report.estimated_fallback_warnings();
        assert_eq!(w.len(), 1, "one model, one warning: {w:?}");
        assert!(w[0].contains("8 part(s)"), "states the count: {}", w[0]);
        assert!(w[0].contains("and 3 more"), "{}", w[0]);
    }

    #[test]
    fn a_real_model_binding_raises_no_warning() {
        let mut report = BindReport::default();
        report.push(sourced_row("Q1", "irlml6344", ModelSourceTier::VendorSpice));
        report.push(sourced_row(
            "Q2",
            "bss138",
            ModelSourceTier::DatasheetDerived,
        ));
        assert!(report.estimated_fallback_warnings().is_empty());
    }

    #[test]
    fn resolved_count_keys_on_outcome_not_model_confidence() {
        // A part whose model MATCHED (confidence Exact) but was then left
        // Unresolved (e.g. an open diode) must NOT be counted resolved, or the
        // headline "N of M resolved" overstates coverage and disagrees with the
        // open part listed below it.
        let mut report = BindReport::default();
        report.push(row(
            "D1",
            Confidence::Exact,
            BindOutcome::Analog {
                device: "diode".into(),
            },
        ));
        report.push(row(
            "D2",
            Confidence::Exact,
            BindOutcome::Unresolved {
                reason: "left open".into(),
            },
        ));
        report.push(row(
            "H1",
            Confidence::Unresolved,
            BindOutcome::Skipped {
                reason: "hole".into(),
            },
        ));

        assert_eq!(
            report.non_ignored_count(),
            2,
            "the skipped mounting hole is ignored"
        );
        assert_eq!(
            report.resolved_count(),
            1,
            "only the genuinely-stamped D1 counts; the open D2 does not despite its Exact match"
        );
    }

    #[test]
    fn numeric_parts_sort_numerically() {
        let mut refs = vec!["R10", "R2", "C1", "U1", "R2B", "R2A"];
        refs.sort_by_key(|r| natural_ref_key(r));
        assert_eq!(refs, vec!["C1", "R2", "R2A", "R2B", "R10", "U1"]);
    }

    #[test]
    fn eagle_auto_named_elements_are_not_active_ics() {
        for auto in ["U$1", "U$12", "IC$3", "R$7", "R1"] {
            assert!(!is_active_ic_ref(auto), "{auto}");
        }
        for real in ["U1", "U12", "IC3", "MCU1", "U$"] {
            assert!(is_active_ic_ref(real), "{real}");
        }
    }

    #[test]
    fn open_pin_warning_matches_only_genuine_open_conditions() {
        assert!(!is_open_pin_warning(
            "U3 (SN74LVC1G3157): VCC net non-canonical, may read as open, so verify the switch's actual supply"
        ));
        assert!(is_open_pin_warning("U1: all I/O pins open (undriven)"));
        assert!(is_open_pin_warning("U2 output pin not connected"));
        assert!(!is_open_pin_warning(
            "[auto-bind] U1: GPIO map cannot be derived from pin names"
        ));
    }
}
