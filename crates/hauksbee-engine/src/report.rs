//! The bind report: one row per component describing what it resolved to and
//! what it became in the simulation (or why it was skipped).

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

impl BindOutcome {
    pub fn label(&self) -> String {
        match self {
            BindOutcome::Analog { device } => format!("analog {device}"),
            BindOutcome::Behavioral { device } => format!("behavioral {device}"),
            BindOutcome::Digital { kind } => format!("digital {kind}"),
            BindOutcome::Mcu { backend } => format!("mcu {backend}"),
            BindOutcome::PowerRail { volts } => format!("rail {volts:.2}V"),
            BindOutcome::Skipped { reason } => format!("skipped ({reason})"),
            BindOutcome::Unresolved { reason } => format!("UNRESOLVED ({reason})"),
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
    pub outcome: BindOutcome,
    /// Set when a connected analog part failed to resolve (loud warning).
    pub warning: Option<String>,
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

    /// Components that are not deliberately ignored.
    pub fn non_ignored(&self) -> impl Iterator<Item = &BindRow> {
        self.rows.iter().filter(|r| !r.outcome.is_ignored())
    }

    /// Count of non-ignored components.
    pub fn non_ignored_count(&self) -> usize {
        self.non_ignored().count()
    }

    /// Count of non-ignored components that resolved better than Unresolved.
    pub fn resolved_count(&self) -> usize {
        self.non_ignored()
            .filter(|r| r.confidence != Confidence::Unresolved)
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
    pub fn warnings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rows
            .iter()
            .filter_map(|r| r.warning.as_deref().map(|w| (r.reference.as_str(), w)))
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

        let cells: Vec<(String, String, String, String, String)> = self
            .rows
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
        out.push_str(&format!(
            "\n{} of {} non-ignored components resolved ({:.0}%); {} mcu(s), {} digital, {} warnings\n",
            self.resolved_count(),
            self.non_ignored_count(),
            self.resolved_fraction() * 100.0,
            self.mcu_count(),
            self.count_where(|o| matches!(o, BindOutcome::Digital { .. })),
            self.warnings().count(),
        ));
        for (r, w) in self.warnings() {
            out.push_str(&format!("  ⚠ {r}: {w}\n"));
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
