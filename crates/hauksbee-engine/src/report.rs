//! The bind report: one row per board component recording what model it resolved
//! to and what it became in the simulation, an analog or behavioral device, a
//! digital or MCU block, a power rail, a deliberately-skipped part, or an
//! unresolved open circuit. [`BindOutcome`] is that per-component verdict; the
//! surrounding report aggregates them for the CLI and the resolve-rate stats.

use hauksbee_ir::evidence::{ModelSource, ModelSourceTier};
use hauksbee_models::Confidence;

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
pub(crate) fn is_active_fallback_device(device: &str) -> bool {
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
    pub fn label(&self) -> String {
        match self {
            BindOutcome::Analog { device } => format!("analog {device}"),
            BindOutcome::Behavioral { device } => format!("behavioral {device}"),
            BindOutcome::Digital { kind } => format!("digital {kind}"),
            BindOutcome::Mcu { backend } => format!("mcu {backend}"),
            BindOutcome::PowerRail { volts } => format!("rail {volts:.2}V"),
            BindOutcome::Skipped { reason } => format!("skipped ({reason})"),
            // The marker is stripped here too, and this was the last surface carrying
            // it. A named abstention's `reason` arrives with its two halves joined by
            // `UNLOCKED_BY_MARKER`; the bind table truncates before reaching it, but
            // the TUI puts this whole label into a part's detail line, so it showed
            // the reader the plumbing. `result.rs`'s own test asserts the marker never
            // reaches a reader, and this label is a reader's.
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
    /// 30 W and a 20 mOhm Rds_on that belong to no datasheet. A verdict resting
    /// on them looked exactly like a verdict resting on a vendor SPICE model,
    /// with nothing at runtime to tell the two apart. Naming them puts the
    /// invented basis on the record.
    ///
    /// Scoped to active devices (`is_active_fallback_device`) on purpose.
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
    /// counting it as such overstated device coverage and made the "N of M
    /// resolved" headline disagree with the open parts listed below it.
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
        let mut sorted_rows: Vec<&BindRow> = self.rows.iter().collect();
        sorted_rows.sort_by_key(|r| natural_ref_key(&r.reference));
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod resolved_count_tests {
    use super::*;
    use hauksbee_models::Confidence;

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
        // The generic power-FET catch-alls carry a 30 V / 20 A / 30 W rating and
        // a 20 mOhm Rds_on that belong to no datasheet. A verdict resting on
        // them used to be indistinguishable at runtime from one resting on a
        // vendor SPICE model.
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
        assert!(
            w.contains("not") && w.contains("datasheet"),
            "says the numbers are invented: {w}"
        );
        assert!(
            w.contains("mpn_re") || w.contains("part number"),
            "names the unlock: {w}"
        );
    }

    #[test]
    fn generic_passive_fallbacks_do_not_flood_the_channel() {
        // c_fallback / r_fallback bind on nearly every board and carry no
        // invented breakdown ratings for a verdict to rest on. Warning about
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
    fn an_estimated_fallback_binding_reaches_the_evidence_map() {
        // The CI report is not the only surface. A verdict resting on invented
        // ratings must be visible on `run` / `--plain` / `--json` / the TUI too,
        // which all render the evidence map.
        use hauksbee_extract::ExtractedBoard;
        let board = ExtractedBoard {
            name: "fallback".into(),
            nets: Vec::new(),
            components: Vec::new(),
        };
        let mut report = BindReport::default();
        report.push(sourced_row(
            "Q3",
            "generic_nmos_power_pkg",
            ModelSourceTier::EstimatedFallback,
        ));
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &report,
            &[],
            hauksbee_ir::evidence::RunDate::from_epoch_days(20_666),
        )
        .expect("evidence builds");
        let rendered = evidence.render_plain();
        assert!(
            rendered.contains("Q3"),
            "the evidence map must name the part: {rendered}"
        );
        assert!(
            rendered.contains("generic_nmos_power_pkg"),
            "and the fallback model it rests on: {rendered}"
        );
    }

    #[test]
    fn a_binder_coverage_note_is_not_attributed_to_the_input_reader() {
        // These notes travel the reader-notes channel but are the binder's.
        // Blaming the input reader for "R7's package is unreadable" misstates
        // which stage hit the limit, on a surface whose job is saying who assumed
        // what.
        use hauksbee_extract::ExtractedBoard;
        let board = ExtractedBoard {
            name: "gaps".into(),
            nets: Vec::new(),
            components: Vec::new(),
        };
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &BindReport::default(),
            &["stress: R7 has no power rating and no readable package".to_string()],
            hauksbee_ir::evidence::RunDate::from_epoch_days(20_666),
        )
        .expect("evidence builds");
        let assumption = evidence
            .assumptions()
            .iter()
            .find(|a: &&hauksbee_ir::evidence::Assumption| a.because().contains("R7"))
            .expect("the note becomes an assumption");
        assert_eq!(
            assumption.source(),
            hauksbee_ir::evidence::AssumptionSource::Binder,
            "a stress-monitor limit is the binder's, not the reader's"
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
        // The warning must stay off boards whose parts resolved properly, or it
        // is noise on every report.
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
        // R24: a part whose model MATCHED (confidence Exact) but was then left
        // Unresolved (e.g. an open diode) must NOT be counted resolved; the
        // headline "N of M resolved" otherwise overstated coverage and disagreed
        // with the open part listed below it.
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
            Confidence::Exact, // model matched...
            BindOutcome::Unresolved {
                reason: "left open".into(),
            }, // ...but left open
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

#[cfg(test)]
mod natural_sort_tests {
    use super::natural_ref_key;

    #[test]
    fn numeric_parts_sort_numerically() {
        let mut refs = vec!["R10", "R2", "C1", "U1", "R2B", "R2A"];
        refs.sort_by_key(|r| natural_ref_key(r));
        assert_eq!(refs, vec!["C1", "R2", "R2A", "R2B", "R10", "U1"]);
    }
}
