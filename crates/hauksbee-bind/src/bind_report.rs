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
use hauksbee_ir::truncate_ellipsis;
use hauksbee_ir::units::{format_percent, format_volts};
use hauksbee_models::Confidence;
use serde::Serialize;
use std::fmt::Write as _;

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

/// Whether a stamped device kind is an ACTIVE part, whose generic fallback model
/// would carry invented breakdown/current/power ratings that a stress verdict can
/// cite.
///
/// Passives (resistor, capacitor, inductor) are excluded: their fallbacks carry
/// no such ratings and bind on nearly every board.
#[must_use]
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
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Analog { device } => format!("analog {device}"),
            Self::Behavioral { device } => format!("behavioral {device}"),
            Self::Digital { kind } => format!("digital {kind}"),
            Self::Mcu { backend } => format!("mcu {backend}"),
            Self::PowerRail { volts } => format!("rail {}", format_volts(volts)),
            Self::Skipped { reason } => format!("skipped ({reason})"),
            Self::Unresolved { reason } => {
                let reason = reason
                    .split_once(hauksbee_ir::evidence::Assumption::UNLOCKED_BY_MARKER)
                    .map_or(reason.as_str(), |(because, _)| because.trim());
                format!("UNRESOLVED ({reason})")
            }
        }
    }

    /// Whether this outcome counts as a "resolved" component (for stats).
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        !matches!(self, Self::Unresolved { .. })
    }

    /// Whether this outcome was deliberately ignored (excluded from resolve %).
    #[must_use]
    pub const fn is_ignored(&self) -> bool {
        matches!(self, Self::Skipped { .. })
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
    /// the pin-rule table or a card's package standard (not an explicit pin
    /// number or pin-function). Each names the pad, role and rule or standard, so nothing is
    /// silently guessed. Empty when every role was explicit.
    pub guesses: Vec<String>,
}

impl BindRow {
    /// A row with no model, source, warning or guesses (skips, rails, refusals).
    #[must_use]
    pub const fn without_model(
        reference: String,
        value: String,
        confidence: Confidence,
        outcome: BindOutcome,
    ) -> Self {
        Self {
            reference,
            value,
            model_id: None,
            confidence,
            source: None,
            outcome,
            warning: None,
            guesses: Vec::new(),
        }
    }
}

/// The full report from one bind pass.
#[derive(Debug, Clone, Default)]
pub struct BindReport {
    pub rows: Vec<BindRow>,
    pub board_name: String,
    pub regulators: Vec<SimpleRegulator>,
    pub open_parts: Vec<OpenPart>,
    /// Linear charger pass elements whose dissipation is watched: the 0 V
    /// ammeter in series with the output and the input node it draws from.
    pub linear_heat: Vec<(hauksbee_ir::DeviceId, hauksbee_ir::NodeId)>,
    /// Power-path parts whose dissipation is watched over both paths out of
    /// IN: the system-rail source, the input node, and the charge-port ammeter
    /// (see [`crate::stress::RegulatorHeat::LinearWithPort`]).
    pub port_heat: Vec<(
        hauksbee_ir::DeviceId,
        hauksbee_ir::NodeId,
        hauksbee_ir::DeviceId,
    )>,
    /// Rails more than one source sets the voltage of (see `rail_owner`).
    pub rail_conflicts: Vec<crate::rail_owner::RailConflict>,
    /// The board's power tree by net name (see
    /// [`crate::rail_owner::RailDomains::supply_groups`]), so evidence can put a
    /// part's supply chain on its causal path without the circuit.
    pub supply_groups: Vec<crate::rail_owner::SupplyGroup>,
    /// Orderable-variant selections, keyed by the index of the row they bound.
    pub selections: std::collections::BTreeMap<usize, hauksbee_models::variants::Selection>,
    /// References of parts that are active ICs by what their record says (symbol,
    /// package, part number, pad count) though the designator does not read as
    /// one (`S1`, `SW2`). They count as active ICs wherever the designator
    /// prefix does, so an IC drawn under a switch-like designator is still
    /// offered for a model.
    pub identity_ics: std::collections::BTreeSet<String>,
    /// Every connected pin of every fitted part the pass bound or left open
    /// (not the skipped ones: connectors, test points, mechanicals), with the
    /// role its card lists for it and the card's id, so a check can name the
    /// pins on a net that the DC netlist gives no behaviour (see
    /// [`crate::control_inputs`]).
    pub part_pins: Vec<PartPin>,
}

/// One connected pin of a fitted part, as the bind pass saw it.
#[derive(Debug, Clone)]
pub struct PartPin {
    /// The part's reference designator.
    pub reference: String,
    /// The pad number as printed.
    pub pin: String,
    /// The node the pin lands on.
    pub node: hauksbee_ir::NodeId,
    /// The id of the card the part bound to; `None` when it bound to none.
    pub card: Option<String>,
    /// The role the card's `[models.pins]` lists for this pad, if any.
    pub role: Option<String>,
    /// Whether the pin could drive its net ([`pin_can_drive`]).
    pub can_drive: bool,
}

/// One unresolved (open-circuit) part and where its pins land, so the scheduler
/// can tell a bystander from a part that would DRIVE a net if it were modelled.
///
/// `pins` pairs each connected node with whether the pin can drive: `false` only
/// when the schematic types it `input`, `power_in` or `no_connect`.
#[derive(Debug, Clone)]
pub struct OpenPart {
    pub reference: String,
    pub value: String,
    pub pins: Vec<(hauksbee_ir::NodeId, bool)>,
}

/// Whether a pin of a part with no usable model could drive its net.
///
/// Read from the schematic's electrical type: `false` only for `input`,
/// `power_in` and `no_connect`. An untyped pin (PCB-only extraction) can drive,
/// so a caller relying on this only ever over-counts drivers.
#[must_use]
pub fn pin_can_drive(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    !(k.starts_with("input") || k.starts_with("power_in") || k.contains("no_connect"))
}

/// One simple (ideal-source) regulator stamp and the input headroom it needs.
/// Output node, return node and signed `vout` live on `circuit.devices[source]`.
#[derive(Debug, Clone)]
pub struct SimpleRegulator {
    pub reference: String,
    pub source: hauksbee_ir::DeviceId,
    pub input: hauksbee_ir::NodeId,
    /// Largest |vout| endpoint plus `dropout_v` (plus nothing when it is absent).
    pub min_in_v: f64,
    /// Absolute-maximum input magnitude; infinite when the card has no rating.
    pub max_in_v: f64,
    /// Declared dropout limit from the card; `None` means headroom is unverified.
    pub dropout_v: Option<f64>,
    /// Present only on an adjustable regulator, where `source` carries Vref from
    /// OUT (to ADJ, or to a private node) rather than the rail against a return:
    /// the return the rail and the headroom are measured against. The
    /// rail is then a SOLVED value, so `min_in_v` is not knowable at bind time.
    pub adj_return: Option<hauksbee_ir::NodeId>,
    /// The pass draw, IADJ and IQ sources the stamp adds beside `source`: any
    /// pass that neutralises `source` must neutralise these with it, or the
    /// pass draw is left reading a branch that no longer exists.
    pub helpers: Vec<hauksbee_ir::DeviceId>,
    /// The hidden DC source (1 V per A) the pass draw reads in a transient:
    /// the scheduler sets it each chunk to the mean current `source` delivered
    /// over the previous one. `None` when nothing draws on the input.
    pub pass_level: Option<hauksbee_ir::DeviceId>,
    /// The board nodes on the part's enable or shutdown pins (`en`, `shdn`).
    /// Kept so a pass that asks what can switch this rail on the real board
    /// (whatever drives these pins can) does not have to re-derive the pin map.
    pub enable: Vec<hauksbee_ir::NodeId>,
    /// The switch those pins drive, when they switch the rail: `source` then
    /// returns to a private node and `gate.ret` is the part's GND (see
    /// [`crate::vreg_enable`]). `None` is an unconditional rail (no pin, or one
    /// the board ties on).
    pub gate: Option<crate::vreg_enable::EnableGate>,
}

impl BindReport {
    pub fn push(&mut self, row: BindRow) {
        self.rows.push(row);
    }

    /// The first row bound for `reference`, if the report has one.
    #[must_use]
    pub fn row(&self, reference: &str) -> Option<&BindRow> {
        self.rows.iter().find(|row| row.reference == reference)
    }

    /// One warning per **active** component whose device parameters and safety
    /// ratings come from a generic estimated-fallback model rather than a real
    /// part.
    ///
    /// These entries exist so an unmodeled power FET in a recognised package
    /// binds to something and can be simulated at all, but their numbers are
    /// invented: the generic power-FET catch-alls carry a nominal 30 V / 20 A /
    /// 30 W and a 20 mOhm `Rds_on` that belong to no datasheet. Naming them puts
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
    #[must_use]
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
                const SHOWN: usize = 5;
                // Aggregated per MODEL, not per part: a motor driver board with
                // eight identical unmodeled FETs is one hole with eight instances,
                // and eight near-identical notes is how this channel gets ignored.
                refs.sort();
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
    #[must_use]
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
    #[must_use]
    pub fn resolved_count(&self) -> usize {
        self.non_ignored()
            .filter(|r| r.outcome.is_resolved())
            .count()
    }

    /// Fraction of non-ignored components that resolved (0.0..=1.0).
    #[must_use]
    // Component counts are far below 2^53, so the ratio is exact enough.
    #[allow(clippy::cast_precision_loss)]
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

    #[must_use]
    pub fn mcu_count(&self) -> usize {
        self.count_where(|o| matches!(o, BindOutcome::Mcu { .. }))
    }

    /// Count of components bound as shift registers specifically.
    #[must_use]
    pub fn shift_register_count(&self) -> usize {
        self.count_where(|o| matches!(o, BindOutcome::Digital { kind } if kind == "shift_register"))
    }

    /// Total components bound into the event-driven digital layer.
    #[must_use]
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
            let w = w
                .strip_prefix(Assumption::PARTIAL_MODEL_MARKER)
                .unwrap_or(w);
            Some((
                r.reference.as_str(),
                w.strip_prefix(Assumption::OPERATING_LIMIT_MARKER)
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
    #[must_use]
    pub fn render_table(&self) -> String {
        self.render_table_with_bulk_passives(true)
    }

    /// Render the bind table for a person asking what still needs attention.
    /// Generic R/C/L fallbacks are sound enough to account for in the summary,
    /// but printing hundreds of them ahead of the active devices hides the
    /// answer. [`Self::render_table`] remains the full form.
    #[must_use]
    pub fn render_table_compact(&self) -> String {
        self.render_table_with_bulk_passives(false)
    }

    #[allow(clippy::too_many_lines)] // one table: widths, rows, summary, warnings, legend
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
                let val = truncate_ellipsis(&r.value, 18);
                let out = truncate_ellipsis(&r.outcome.label(), 34);
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

        let pad = |s: &str, w: usize| format!("{s:<w$}");

        let widths = [w_ref, w_val, w_mod, w_conf, w_out];
        let rule = |left: &str, mid: &str, right: &str| {
            let bars: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            format!("{left}─{}─{right}\n", bars.join(&format!("─{mid}─")))
        };

        let mut out = String::new();
        out.push_str(&rule("┌", "┬", "┐"));
        // Header.
        refs = pad(&refs, w_ref);
        vals = pad(&vals, w_val);
        models = pad(&models, w_mod);
        confs = pad(&confs, w_conf);
        outs = pad(&outs, w_out);
        let _ = writeln!(out, "│ {refs} │ {vals} │ {models} │ {confs} │ {outs} │");
        out.push_str(&rule("├", "┼", "┤"));
        for (r, v, m, c, o) in &cells {
            let _ = writeln!(
                out,
                "│ {} │ {} │ {} │ {} │ {} │",
                pad(r, w_ref),
                pad(v, w_val),
                pad(m, w_mod),
                pad(c, w_conf),
                pad(o, w_out),
            );
        }
        out.push_str(&rule("└", "┴", "┘"));

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
                for (n, kind) in [
                    (capacitors, "capacitor"),
                    (resistors, "resistor"),
                    (inductors, "inductor"),
                ] {
                    if n > 0 {
                        kinds.push(format!("{n} {kind}{}", if n == 1 { "" } else { "s" }));
                    }
                }
                let _ = writeln!(
                    out,
                    "{total} passives bound by footprint/value fallback: {}. Use --verbose to show every bind row.",
                    kinds.join(", ")
                );
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
        let _ = writeln!(
            out,
            "\n{} of {} non-ignored components resolved ({}); {}, {} digital, {}, {}",
            self.resolved_count(),
            self.non_ignored_count(),
            format_percent(self.resolved_fraction() * 100.0, 0),
            plural(self.mcu_count(), "MCU", "MCUs"),
            self.count_where(|o| matches!(o, BindOutcome::Digital { .. })),
            plural(self.warnings().count(), "warning", "warnings"),
            plural(guess_count, "pin-role guess", "pin-role guesses")
        );
        for (r, w) in self.warnings() {
            let _ = writeln!(out, "  ⚠ {r}: {w}");
        }
        if include_bulk_passives {
            // The full form: one line per guessed pad.
            for (r, g) in self.guess_warnings() {
                let _ = writeln!(out, "  ? {r}: {g}");
            }
        } else {
            // Identical guesses across parts read as one line; --verbose has
            // every pad.
            let grouped = group_pin_role_guesses(self.guess_warnings());
            for line in &grouped {
                let _ = writeln!(out, "{line}");
            }
            if grouped.len() < guess_count {
                let _ = writeln!(
                    out,
                    "  (identical guesses are grouped above; --verbose lists every pad)"
                );
            }
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

/// Natural sort key for a reference designator.
///
/// The key is the alpha prefix (case-folded), then the numeric part as a
/// NUMBER (so R2 sorts before R10), then any remaining suffix. The one
/// definition lives in [`hauksbee_ir::order`].
///
/// Shared by every user-facing table that lists parts.
#[must_use]
pub fn natural_ref_key(reference: &str) -> (String, u64, String) {
    hauksbee_ir::order::natural_ref_key(reference)
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
            let active_ic = is_mcu
                || is_active_ic_ref(&row.reference)
                || report.identity_ics.contains(&row.reference);
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

        // The rows come in the order the board file listed its parts; the
        // summary is what reaches JSON, so it reads the same whatever that was.
        let by_part = |a: &UnresolvedActive, b: &UnresolvedActive| {
            hauksbee_ir::order::cmp_refs(&a.reference, &b.reference)
                .then_with(|| a.value.cmp(&b.value))
                .then_with(|| a.reason.cmp(&b.reason))
                .then_with(|| a.consequence.cmp(&b.consequence))
        };
        active_path_unresolved.sort_by(by_part);
        resolved_but_open_active.sort_by(by_part);

        Self {
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn render_banner(&self) -> String {
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
                    "  {actives} of these are active ICs left OPEN: analog/AC/thermal results on \
                     their nets are NOT trustworthy."
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

pub use hauksbee_models::matcher::{is_active_ic_ref, is_tool_generated_ref};

/// Whether a resolved row's `warning` indicates a GENUINE open/undriven-pin
/// condition (which makes thermal/AC over its nets untrustworthy) as opposed to
/// an auto-bind GPIO-map note.
///
/// The binder routinely attaches an `[auto-bind] ... GPIO map derived/cannot be
/// derived ...` note to a perfectly working MCU when it infers (or fails to
/// infer) the pin map from schematic pin names; that is a naming limitation, not
/// an open circuit, and must NOT mark the part as untrustworthy on the live
/// circuit.
#[must_use]
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

/// The text of one pin-rule role guess, as stored in [`BindRow::guesses`].
///
/// It is carried unchanged into `--json`. The binder writes it here and
/// [`group_pin_role_guesses`] reads it back through `parse_rule_guess`, so the
/// wording has one definition.
#[must_use]
pub fn rule_guess_text(
    pad: &str,
    role: &str,
    rule_id: &str,
    footprint: &str,
    pad_count: usize,
) -> String {
    format!(
        "pad {pad} role '{role}' guessed from rule '{rule_id}' (no explicit pin-function; \
         footprint \"{footprint}\", {pad_count} pads)"
    )
}

/// The parts of a [`rule_guess_text`] string: one pad's role, the rule id that
/// gave it, and everything after the rule (the footprint and pad count it
/// matched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuleGuess<'a> {
    pad: &'a str,
    role: &'a str,
    rule_id: &'a str,
    tail: &'a str,
}

fn parse_rule_guess(guess: &str) -> Option<RuleGuess<'_>> {
    let rest = guess.strip_prefix("pad ")?;
    let (pad, rest) = rest.split_once(" role '")?;
    let (role, rest) = rest.split_once("' guessed from rule '")?;
    let (rule_id, tail) = rest.split_once("' (")?;
    (!pad.contains(' ')).then_some(RuleGuess {
        pad,
        role,
        rule_id,
        tail,
    })
}

/// Parts named on a grouped guess line before the rest are counted.
const GROUPED_REFS_SHOWN: usize = 3;

/// One output line of [`group_pin_role_guesses`]: either parts sharing a
/// pad-to-role map, or a guess printed as it stands.
enum GuessEntry<'a> {
    Group {
        pads: Vec<RuleGuess<'a>>,
        refs: Vec<&'a str>,
    },
    Verbatim(String),
}

/// Collapse pin-role guesses into one line per distinct guess.
///
/// The same rule, footprint and pad-to-role map across parts is one line naming
/// the parts compactly (the first few, then a count) and the pad map once. A
/// part whose guesses do not all come from a pin rule (a package-standard
/// numbering, say) keeps its own per-guess lines, unchanged. Groups keep the
/// order their first part appears in, so the output is stable for a board.
///
/// `guesses` is `(reference, guess text)` in report order. The per-pad lines
/// remain available through `--verbose` and the `--json` document, which this
/// does not touch.
#[must_use]
pub fn group_pin_role_guesses<'a>(
    guesses: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<String> {
    let mut per_part: Vec<(&str, Vec<&str>)> = Vec::new();
    for (reference, guess) in guesses {
        match per_part.last_mut() {
            Some((previous, texts)) if *previous == reference => texts.push(guess),
            _ => per_part.push((reference, vec![guess])),
        }
    }
    let mut entries: Vec<GuessEntry<'_>> = Vec::new();
    for (reference, texts) in &per_part {
        let parsed: Option<Vec<RuleGuess<'_>>> =
            texts.iter().map(|text| parse_rule_guess(text)).collect();
        let Some(pads) = parsed else {
            entries.extend(
                texts
                    .iter()
                    .map(|text| GuessEntry::Verbatim(format!("  ? {reference}: {text}"))),
            );
            continue;
        };
        let same_map = entries.iter_mut().find_map(|entry| match entry {
            GuessEntry::Group { pads: seen, refs } if *seen == pads => Some(refs),
            _ => None,
        });
        match same_map {
            Some(refs) => refs.push(reference),
            None => entries.push(GuessEntry::Group {
                pads,
                refs: vec![reference],
            }),
        }
    }
    entries
        .into_iter()
        .map(|entry| match entry {
            GuessEntry::Verbatim(line) => line,
            GuessEntry::Group { pads, refs } => grouped_guess_line(&pads, &refs),
        })
        .collect()
}

fn grouped_guess_line(pads: &[RuleGuess<'_>], refs: &[&str]) -> String {
    let shown = refs
        .iter()
        .take(GROUPED_REFS_SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let parts = if refs.len() > GROUPED_REFS_SHOWN {
        format!(
            "{shown} and {} more ({} parts)",
            refs.len() - GROUPED_REFS_SHOWN,
            refs.len()
        )
    } else {
        shown
    };
    let map = pads
        .iter()
        .map(|g| format!("pad {} role '{}'", g.pad, g.role))
        .collect::<Vec<_>>()
        .join(", ");
    let mut ids: Vec<&str> = Vec::new();
    for g in pads {
        if !ids.contains(&g.rule_id) {
            ids.push(g.rule_id);
        }
    }
    let noun = if ids.len() > 1 { "rules" } else { "rule" };
    let ids = ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let tail = pads.first().map_or("", |g| g.tail);
    format!("  ? {parts}: {map} guessed from {noun} {ids} ({tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::evidence::{ModelSource, ModelSourceTier};

    fn row(reference: &str, confidence: Confidence, outcome: BindOutcome) -> BindRow {
        BindRow::without_model(reference.to_string(), String::new(), confidence, outcome)
    }

    /// A bind row carrying a model source at `tier`.
    fn sourced_row(reference: &str, model_id: &str, tier: ModelSourceTier) -> BindRow {
        use hauksbee_ir::evidence::{ModelLayer, ModelUncertainty, ModelValidation};
        let outcome = BindOutcome::Analog {
            device: "nmos".into(),
        };
        BindRow {
            value: "NMOS".to_string(),
            model_id: Some(model_id.to_string()),
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
            ..row(reference, Confidence::Exact, outcome)
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

    /// C1 and R1 bound to the generic `c_fallback` / `r_fallback` passives.
    fn fallback_passives() -> BindReport {
        let mut report = BindReport::default();
        for (reference, id, device) in [
            ("C1", "c_fallback", "capacitor"),
            ("R1", "r_fallback", "resistor"),
        ] {
            let mut row = sourced_row(reference, id, ModelSourceTier::EstimatedFallback);
            row.outcome = BindOutcome::Analog {
                device: device.into(),
            };
            report.push(row);
        }
        report
    }

    #[test]
    fn generic_passive_fallbacks_do_not_flood_the_channel() {
        // c_fallback / r_fallback bind on nearly every board and carry no
        // invented breakdown ratings for a verdict to rest on, so warning about
        // them would bury the FET case this exists for.
        let report = fallback_passives();
        assert!(report.estimated_fallback_warnings().is_empty());
    }

    #[test]
    fn compact_bind_table_rolls_up_only_bulk_fallback_passives() {
        let mut report = fallback_passives();
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

    fn guessed_fet(reference: &str, rule: &str, footprint: &str, roles: &[&str]) -> BindRow {
        BindRow {
            guesses: roles
                .iter()
                .enumerate()
                .map(|(i, role)| {
                    rule_guess_text(&(i + 1).to_string(), role, rule, footprint, roles.len())
                })
                .collect(),
            ..sourced_row(reference, "nmos", ModelSourceTier::CuratedLibrary)
        }
    }

    #[test]
    fn rule_guess_text_round_trips_through_the_parser() {
        let text = rule_guess_text("2", "source", "nmos_sot23_g1_s2_d3", "SOT-23", 3);
        let parsed = parse_rule_guess(&text).expect("parses");
        assert_eq!(
            (parsed.pad, parsed.role, parsed.rule_id),
            ("2", "source", "nmos_sot23_g1_s2_d3")
        );
        assert_eq!(
            parsed.tail,
            "no explicit pin-function; footprint \"SOT-23\", 3 pads)"
        );
        assert_eq!(
            parse_rule_guess("pad 1 role 'a' numbered from package standard \"x\""),
            None
        );
    }

    #[test]
    fn identical_pin_role_guesses_print_as_one_line_and_verbose_keeps_every_pad() {
        let mut report = BindReport::default();
        let roles = ["gate", "source", "drain"];
        for i in 0..22 {
            report.push(guessed_fet(
                &format!("Q{i}"),
                "nmos_g1_s2_d3",
                "SOT-23",
                &roles,
            ));
        }
        // Same rule, different footprint: its own line.
        report.push(guessed_fet("Q99", "nmos_g1_s2_d3", "SOT-523", &roles));
        // A different pad map under another rule: its own line.
        report.push(guessed_fet(
            "Q100",
            "nmos_g1_d2_s3",
            "SOT-23",
            &["gate", "drain", "source"],
        ));

        let compact = report.render_table_compact();
        let lines: Vec<&str> = compact.lines().filter(|l| l.starts_with("  ? ")).collect();
        assert_eq!(lines.len(), 3, "{compact}");
        assert_eq!(
            lines[0],
            "  ? Q0, Q1, Q2 and 19 more (22 parts): pad 1 role 'gate', pad 2 role 'source', \
             pad 3 role 'drain' guessed from rule 'nmos_g1_s2_d3' (no explicit pin-function; \
             footprint \"SOT-23\", 3 pads)"
        );
        assert!(
            lines[1].starts_with("  ? Q99: pad 1 role 'gate'"),
            "{compact}"
        );
        assert!(lines[2].contains("rule 'nmos_g1_d2_s3'"), "{compact}");
        // The summary still counts every guessed pad.
        assert!(compact.contains("72 pin-role guesses"), "{compact}");

        let verbose = report.render_table();
        let per_pad = verbose.lines().filter(|l| l.starts_with("  ? ")).count();
        assert_eq!(per_pad, 72, "{verbose}");
    }

    #[test]
    fn a_single_part_guess_keeps_its_original_wording() {
        let mut report = BindReport::default();
        report.push(guessed_fet("Q1", "nmos_g1_s2_d3", "SOT-23", &["gate"]));
        let compact = report.render_table_compact();
        assert!(
            compact.contains(
                "  ? Q1: pad 1 role 'gate' guessed from rule 'nmos_g1_s2_d3' (no explicit \
                 pin-function; footprint \"SOT-23\", 1 pads)\n"
            ),
            "{compact}"
        );
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
