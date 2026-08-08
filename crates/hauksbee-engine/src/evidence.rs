//! Production adapter from a bound board to the shared IR evidence spine.
//!
//! The IR crate defines what evidence *is* (assumptions, provenance, error
//! budgets, one validated [`EvidenceMap`] per claim, with a derived status no
//! caller can forge). This module is where a real run earns one: it walks the
//! extracted board's net/component incidence, the bind report's outcomes, and
//! the reader's coverage notes, and builds a [`BoardEvidence`] holding the
//! registry plus one map per electrical net. Unresolved parts become open-part
//! assumptions; every reader limitation becomes a board-scoped
//! reduced-fidelity assumption keyed by a content hash of its text, so its
//! identity survives reordering and cannot vanish from one surface while
//! remaining on another. Both the JSON output and the human summaries render
//! this same object — there is no second bookkeeping path to drift. It also
//! folds in binder decisions, model matches, solver fallbacks, CI inputs, and
//! live waivers; report producers ask it for assertion-scoped maps, and
//! renderers only project those maps, so provenance and trust status cannot
//! drift between terminal, JSON, web, JUnit, SARIF, and GitHub output.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;

use crate::report::{BindOutcome, BindReport};
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::evidence::{
    ArtifactId, ArtifactKind, ArtifactProvenance, ArtifactRole, Assumption, AssumptionSource,
    CausalPathIndex, Contribution, CrossCheck, EntityKind, EntityRef, ErrorBudget, EvidenceError,
    EvidenceMap, EvidenceRegistry, EvidenceStatus, IgnoredInput, IntegrationMethod,
    IntegrationTolerance, MatchConfidence, ModelLayer, ModelOnPath, ModelSource, ModelSourceTier,
    ModelUncertainty, ModelValidation, NetScope, ParameterProvenance, RunDate, Scope, Subject,
    SubjectSet, TimeWindow, ValueOrigin, WindowMethod,
};
use hauksbee_models::Confidence;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
struct ModelFact {
    model_id: String,
    confidence: MatchConfidence,
    source: hauksbee_ir::evidence::ModelSource,
}

#[derive(Debug, Clone)]
struct DefaultFact {
    parameter: String,
    value: String,
    assumption: hauksbee_ir::evidence::AssumptionId,
}

fn unspecified_source(model_id: &str) -> ModelSource {
    ModelSource::new(
        ModelSourceTier::EstimatedFallback,
        ModelLayer::Unspecified,
        "legacy-bind-row",
        ModelValidation::Unvalidated,
        vec![ModelUncertainty::unknown(
            format!("{model_id}.model"),
            "the legacy bind producer did not record a numeric error interval",
        )
        .expect("static uncertainty is valid")],
    )
    .expect("static model source is valid")
}

/// The evidence registry and one validated map per electrical net in a bound
/// board. Both JSON and human surfaces render this same object.
#[derive(Debug, Clone)]
pub struct BoardEvidence {
    registry: EvidenceRegistry,
    index: CausalPathIndex,
    refs_by_net: BTreeMap<String, Vec<String>>,
    model_by_ref: BTreeMap<String, ModelFact>,
    today: RunDate,
    assumptions: Vec<Assumption>,
    maps: Vec<EvidenceMap>,
    board_artifact: Option<ArtifactId>,
    firmware_artifact: Option<ArtifactId>,
    supporting_artifacts: Vec<ArtifactId>,
    defaults_by_ref: BTreeMap<String, Vec<DefaultFact>>,
    reader_contributions: Vec<Contribution>,
    reader_ignored: Vec<IgnoredInput>,
    reader_cross_checks: Vec<CrossCheck>,
}

fn integration_method(method: hauksbee_solve::Integration) -> IntegrationMethod {
    match method {
        hauksbee_solve::Integration::Trapezoidal => IntegrationMethod::Trapezoidal,
        hauksbee_solve::Integration::Gear2 => IntegrationMethod::Gear2,
        hauksbee_solve::Integration::BackwardEuler => IntegrationMethod::BackwardEuler,
    }
}

fn checked_subwindow(
    kind: &'static str,
    start_s: f64,
    end_s: f64,
    result: TimeWindow,
) -> Result<TimeWindow, EvidenceError> {
    let window = TimeWindow::new(start_s, end_s)?;
    if start_s < result.start_s() || end_s > result.end_s() {
        return Err(EvidenceError::WindowOutsideResult {
            kind,
            start_s,
            end_s,
            result_start_s: result.start_s(),
            result_end_s: result.end_s(),
        });
    }
    Ok(window)
}

fn windows_overlap(first: TimeWindow, second: TimeWindow) -> bool {
    first.start_s() < second.end_s() && second.start_s() < first.end_s()
}

/// Partition the solved span into primary-method windows after removing failed
/// and fallback spans. This avoids an overlapping "primary over the whole run"
/// entry that would falsely claim the primary method produced fallback data.
fn uncovered_windows(
    result: TimeWindow,
    blocked: impl IntoIterator<Item = TimeWindow>,
) -> Result<Vec<TimeWindow>, EvidenceError> {
    let mut blocked: Vec<_> = blocked.into_iter().collect();
    blocked.sort_by(|a, b| a.start_s().total_cmp(&b.start_s()));
    let mut cursor = result.start_s();
    let mut out = Vec::new();
    for window in blocked {
        if cursor < window.start_s() {
            out.push(TimeWindow::new(cursor, window.start_s())?);
        }
        cursor = cursor.max(window.end_s());
    }
    if cursor < result.end_s() {
        out.push(TimeWindow::new(cursor, result.end_s())?);
    }
    Ok(out)
}

impl BoardEvidence {
    /// Build from the actual board incidence and the bind report that produced
    /// the live circuit. Reader coverage notes enter as board-scoped
    /// reduced-fidelity assumptions, so they cannot disappear on one surface.
    pub fn from_bound(
        board: &ExtractedBoard,
        report: &BindReport,
        reader_notes: &[String],
        today: RunDate,
    ) -> Result<Self, EvidenceError> {
        let mut assumptions = Vec::new();
        for row in &report.rows {
            if let BindOutcome::Unresolved { reason } = &row.outcome {
                assumptions.push(Assumption::open_part(&row.reference, &row.value, reason));
            }
        }
        // A part bound to a generic estimated-fallback model is running on
        // invented ratings. Recorded here rather than only in the CI report's
        // coverage_warnings, so it reaches every surface that renders the evidence
        // map (`--plain`, `--json`, the web front door) and not just `hauksbee ci`.
        // The TUI does not build a BoardEvidence, so it takes the same warnings
        // directly through `AppState::new`'s coverage notes.
        for row in report.non_ignored() {
            if row
                .source
                .as_ref()
                .is_none_or(|s| s.tier() != ModelSourceTier::EstimatedFallback)
            {
                continue;
            }
            if !matches!(
                &row.outcome,
                BindOutcome::Analog { device } | BindOutcome::Behavioral { device }
                    if crate::report::is_active_fallback_device(device)
            ) {
                continue;
            }
            let key = format!("model/{}", row.reference);
            let subject_text = format!("{} ({})", row.reference, row.value);
            assumptions.push(Assumption::reduced_fidelity(
                AssumptionSource::Binder,
                Subject::new(&key, &subject_text),
                Scope::Board,
                &format!(
                    "the generic fallback model '{}', whose ratings and device parameters are \
                     estimates for the package class rather than values from any datasheet",
                    row.model_id.as_deref().unwrap_or("(unnamed)"),
                ),
                "add a model entry matching the part (value_re or mpn_re), or put the \
                 manufacturer part number on the component",
            ));
        }
        let mut reader_contributions = Vec::new();
        let mut reader_ignored = Vec::new();
        let mut reader_cross_checks = Vec::new();
        let mut seen_reader_notes = BTreeSet::new();
        for note in reader_notes {
            let normalized = note.trim();
            if !seen_reader_notes.insert(normalized) {
                continue;
            }
            classify_reader_note(
                normalized,
                board,
                &mut assumptions,
                &mut reader_contributions,
                &mut reader_ignored,
                &mut reader_cross_checks,
            )?;
        }

        let mut defaults_by_ref: BTreeMap<String, Vec<DefaultFact>> = BTreeMap::new();
        for row in &report.rows {
            for guess in &row.guesses {
                let (pin, role) = guessed_pin_role(guess);
                assumptions.push(Assumption::inferred_pin_role(&row.reference, &pin, &role));
            }
            if let Some((parameter, value)) = documented_default(row.warning.as_deref()) {
                let assumption = Assumption::default_parameter(&row.reference, &parameter, &value);
                defaults_by_ref
                    .entry(row.reference.clone())
                    .or_default()
                    .push(DefaultFact {
                        parameter,
                        value,
                        assumption: assumption.id().clone(),
                    });
                assumptions.push(assumption);
            }
        }

        let registry = EvidenceRegistry::new(assumptions)?;
        let net_names: BTreeMap<i64, &str> = board
            .nets
            .iter()
            .filter(|net| net.id != 0 && !net.name.trim().is_empty())
            .map(|net| (net.id, net.name.as_str()))
            .collect();
        let mut incidence: BTreeMap<String, BTreeSet<String>> = net_names
            .values()
            .map(|name| ((*name).to_string(), BTreeSet::new()))
            .collect();
        for component in &board.components {
            if component.reference.trim().is_empty() {
                continue;
            }
            for pin in &component.pins {
                if let Some(name) = pin.net.and_then(|id| net_names.get(&id)).copied() {
                    incidence
                        .entry(name.to_string())
                        .or_default()
                        .insert(component.reference.clone());
                }
            }
        }
        let owned: Vec<(String, Vec<String>)> = incidence
            .into_iter()
            .map(|(net, refs)| (net, refs.into_iter().collect()))
            .collect();
        let index = CausalPathIndex::from_net_parts(
            owned
                .iter()
                .map(|(net, refs)| (net.as_str(), refs.as_slice())),
        )?;

        let rows: BTreeMap<&str, _> = report
            .rows
            .iter()
            .map(|row| (row.reference.as_str(), row))
            .collect();
        let mut maps = Vec::with_capacity(owned.len());
        for (net, refs) in &owned {
            let scope = NetScope::new([net.as_str()], None)?;
            let traversal = index.traverse(&scope, &registry)?;
            let mut map = EvidenceMap::from_traversal(
                format!("Binding completeness for net {net}"),
                traversal,
                &registry,
                today,
            )?;
            let mut models = Vec::new();
            let mut parameters = Vec::new();
            for reference in refs {
                if let Some(row) = rows.get(reference.as_str()) {
                    if let Some(model_id) = row.model_id.as_deref() {
                        let confidence = match row.confidence {
                            Confidence::Exact => MatchConfidence::Exact,
                            Confidence::Family => MatchConfidence::High,
                            Confidence::Guessed | Confidence::Unresolved => {
                                MatchConfidence::Guessed
                            }
                        };
                        let source = row
                            .source
                            .clone()
                            .unwrap_or_else(|| unspecified_source(model_id));
                        models.push(ModelOnPath::new(
                            reference,
                            model_id,
                            source.clone(),
                            confidence,
                        )?);
                        parameters.push(ParameterProvenance::new(
                            format!("{reference}.model"),
                            model_id,
                            ValueOrigin::Model {
                                model_id: model_id.to_string(),
                                layer: source.layer(),
                                confidence,
                            },
                        )?);
                    }
                }
                if let Some(defaults) = defaults_by_ref.get(reference) {
                    for default in defaults {
                        parameters.push(ParameterProvenance::new(
                            format!("{reference}.{}", default.parameter),
                            &default.value,
                            ValueOrigin::Default {
                                assumption: default.assumption.clone(),
                            },
                        )?);
                    }
                }
            }
            map = map.with_models(models);
            map = map.with_parameters(&registry, parameters)?;
            maps.push(map);
        }

        let model_by_ref = rows
            .values()
            .filter_map(|row| {
                let model_id = row.model_id.as_ref()?;
                Some((
                    row.reference.clone(),
                    ModelFact {
                        model_id: model_id.clone(),
                        confidence: confidence(row.confidence),
                        source: row
                            .source
                            .clone()
                            .unwrap_or_else(|| unspecified_source(model_id)),
                    },
                ))
            })
            .collect();
        let refs_by_net = owned.iter().cloned().collect();
        Ok(Self {
            registry: registry.clone(),
            index,
            refs_by_net,
            model_by_ref,
            today,
            assumptions: registry.assumptions().to_vec(),
            maps,
            board_artifact: None,
            firmware_artifact: None,
            supporting_artifacts: Vec::new(),
            defaults_by_ref,
            reader_contributions,
            reader_ignored,
            reader_cross_checks,
        })
    }

    /// Attach the exact board bytes the reader consumed. The SHA belongs to the
    /// original upload/path, not the normalized in-memory board, so a report can
    /// be reproduced and audited without guessing which source revision ran.
    pub fn with_input_artifact(
        mut self,
        path: impl AsRef<Path>,
        raw: &[u8],
        kind: crate::board_input::InputKind,
    ) -> Result<Self, EvidenceError> {
        let path = path.as_ref();
        let (artifact_kind, role) = input_artifact_kind(path, kind);
        let digest = hex_digest(&Sha256::digest(raw));
        let reader_assumptions = self
            .registry
            .assumptions()
            .iter()
            .filter(|a| a.source() == AssumptionSource::Reader)
            .map(|a| a.id().clone())
            .collect();
        let mut contributions = input_contributions(kind);
        contributions.extend(self.reader_contributions.clone());
        let artifact = ArtifactProvenance::new(
            path.to_string_lossy(),
            artifact_kind,
            role,
            digest,
            reader_assumptions,
        )?
        .with_contributions(contributions)
        .with_ignored(self.reader_ignored.clone())
        .with_cross_checks(self.reader_cross_checks.clone());
        self.board_artifact = Some(self.registry.add_artifact(artifact)?);
        Ok(self)
    }

    /// Attach the exact firmware image consumed by a co-simulation.
    pub fn with_firmware_artifact(
        mut self,
        path: impl AsRef<Path>,
        raw: &[u8],
    ) -> Result<Self, EvidenceError> {
        let path = path.as_ref();
        let kind = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("hex") => ArtifactKind::IntelHex,
            _ => ArtifactKind::Elf,
        };
        let artifact = ArtifactProvenance::new(
            path.to_string_lossy(),
            kind,
            ArtifactRole::Firmware,
            hex_digest(&Sha256::digest(raw)),
            Vec::new(),
        )?
        .with_contributions(vec![Contribution {
            what: "firmware_image".into(),
            detail: "instructions executed by the MCU co-simulation backend".into(),
        }]);
        self.firmware_artifact = Some(self.registry.add_artifact(artifact)?);
        Ok(self)
    }

    /// Attach a TOML input that directly shaped the run (CI spec or waivers).
    pub fn with_toml_artifact(
        mut self,
        path: impl AsRef<Path>,
        raw: &[u8],
        role: ArtifactRole,
        contribution: Contribution,
    ) -> Result<Self, EvidenceError> {
        let artifact = ArtifactProvenance::new(
            path.as_ref().to_string_lossy(),
            ArtifactKind::Toml,
            role,
            hex_digest(&Sha256::digest(raw)),
            Vec::new(),
        )?
        .with_contributions(vec![contribution]);
        let id = self.registry.add_artifact(artifact)?;
        self.supporting_artifacts.push(id);
        Ok(self)
    }

    /// Merge facts produced after binding (scheduler, solver, live waiver).
    /// Existing artifact indices remain stable and every map is rebuilt later.
    pub fn with_assumptions(
        mut self,
        new_assumptions: impl IntoIterator<Item = Assumption>,
    ) -> Result<Self, EvidenceError> {
        let artifacts = self.registry.artifacts().to_vec();
        let mut assumptions = self.registry.assumptions().to_vec();
        for assumption in new_assumptions {
            if !assumptions
                .iter()
                .any(|known| known.id() == assumption.id())
            {
                assumptions.push(assumption);
            }
        }
        let mut registry = EvidenceRegistry::new(assumptions)?;
        for artifact in artifacts {
            registry.add_artifact(artifact)?;
        }
        self.assumptions = registry.assumptions().to_vec();
        self.registry = registry;
        Ok(self)
    }

    /// Add only substitutions the scheduler actually recorded. Binder family
    /// matching is provenance, not automatically a semantic stand-in; it does
    /// not enter this assumption registry.
    pub fn with_substitutions(
        mut self,
        substitutions: &[crate::scheduler::McuSubstitution],
    ) -> Result<Self, EvidenceError> {
        if substitutions.is_empty() {
            return Ok(self);
        }
        let artifacts = self.registry.artifacts().to_vec();
        let mut assumptions = self.registry.assumptions().to_vec();
        for sub in substitutions {
            let assumption = Assumption::substitute_model(
                AssumptionSource::Scheduler,
                &sub.reference,
                &sub.requested_part,
                &sub.modelled_core,
            );
            if !assumptions.iter().any(|a| a.id() == assumption.id()) {
                assumptions.push(assumption);
            }
        }
        let mut registry = EvidenceRegistry::new(assumptions)?;
        for artifact in artifacts {
            registry.add_artifact(artifact)?;
        }
        self.assumptions = registry.assumptions().to_vec();
        self.registry = registry;
        Ok(self)
    }

    pub fn inventory(&self) -> &[ArtifactProvenance] {
        self.registry.artifacts()
    }

    pub fn assumptions(&self) -> &[Assumption] {
        &self.assumptions
    }

    pub fn maps(&self) -> &[EvidenceMap] {
        &self.maps
    }

    pub fn is_undermined(&self) -> bool {
        self.maps
            .iter()
            .any(|map| map.status() == EvidenceStatus::Undermined)
    }

    /// Whether at least one published assertion carries any evidence caveat.
    /// Qualified evidence is still useful, but it must not sit under an
    /// unqualified "Looks healthy" headline.
    pub fn has_caveats(&self) -> bool {
        self.maps
            .iter()
            .any(|map| map.status() != EvidenceStatus::Clean)
    }

    /// A real report-coverage assertion for an otherwise finding-free static
    /// analysis. This is not a synthetic pass result: it states exactly which
    /// extracted nets the front door inspected and therefore carries any
    /// reader limitations that constrain that coverage claim.
    pub fn static_coverage_map(&self) -> Result<EvidenceMap, EvidenceError> {
        let nets: Vec<String> = self.refs_by_net.keys().cloned().collect();
        self.map_for_nets(
            format!("Static analysis covered {} extracted nets", nets.len()),
            nets,
        )
    }

    /// Evidence for a check-level coverage claim. This is what keeps a
    /// geometry-less input from turning an empty DRC/SI finding list into a
    /// synthetic clean pass: `NotChecked` is check-scoped, so it belongs on an
    /// explicit coverage assertion even when there are no findings to map.
    pub fn check_coverage_map(
        &self,
        check: &str,
        assertion: impl Into<String>,
    ) -> Result<EvidenceMap, EvidenceError> {
        let assertion = assertion.into();
        let nets: Vec<String> = self.refs_by_net.keys().cloned().collect();
        let mut map = self.map_for_nets_with_check(
            assertion.clone(),
            nets,
            Some((check, assertion.as_str())),
        )?;
        if let Some(artifact) = self.board_artifact {
            map = map.with_artifacts(&self.registry, [artifact])?;
        }
        Ok(map)
    }

    /// Build maps for the report's actual assertions. Nets are authoritative;
    /// a refs-only finding resolves to every net touching those refs.
    pub fn maps_for_findings(
        &self,
        findings: &[crate::result::JsonFinding],
    ) -> Result<Vec<EvidenceMap>, EvidenceError> {
        let mut maps = Vec::new();
        for finding in findings {
            let mut nets: BTreeSet<String> = finding.nets.iter().cloned().collect();
            if nets.is_empty() {
                for (net, references) in &self.refs_by_net {
                    if finding.refs.iter().any(|r| references.contains(r)) {
                        nets.insert(net.clone());
                    }
                }
            }
            if nets.is_empty() {
                continue;
            }
            let nets: Vec<String> = nets.into_iter().collect();
            maps.push(if finding.check == "drc" {
                self.geometry_map(finding.message.clone(), &nets)?
            } else {
                self.map_for_nets(finding.message.clone(), nets)?
            });
        }
        Ok(maps)
    }

    /// One causal assertion per real DRC result. Geometry maps deliberately
    /// traverse no component models: an open op-amp cannot invalidate the fact
    /// that two pieces of copper touch.
    pub fn maps_for_drc(
        &self,
        drc: &crate::result::DrcStructured,
    ) -> Result<Vec<EvidenceMap>, EvidenceError> {
        let mut maps = Vec::new();
        for short in &drc.shorts {
            maps.push(self.geometry_map(
                short.plain.clone(),
                &[short.net_a.clone(), short.net_b.clone()],
            )?);
        }
        for group in drc.violations.iter().chain(&drc.at_limit) {
            maps.push(self.geometry_map(
                group.plain.clone(),
                &[group.net_a.clone(), group.net_b.clone()],
            )?);
        }
        Ok(maps)
    }

    pub fn geometry_map(
        &self,
        assertion: impl Into<String>,
        nets: &[String],
    ) -> Result<EvidenceMap, EvidenceError> {
        let assertion = assertion.into();
        let empty: Vec<String> = Vec::new();
        let incidence: Vec<(&str, &[String])> = nets
            .iter()
            .map(|net| (net.as_str(), empty.as_slice()))
            .collect();
        let index = CausalPathIndex::from_net_parts(incidence)?;
        let scope = NetScope::new(nets.iter().map(String::as_str), None)?;
        let traversal = index.traverse_assertion(&scope, "drc", &assertion, &self.registry)?;
        let mut map =
            EvidenceMap::from_traversal(assertion, traversal, &self.registry, self.today)?;
        if let Some(artifact) = self.board_artifact {
            map = map.with_artifacts(&self.registry, [artifact])?;
        }
        Ok(map)
    }

    /// A numeric simulation assertion over nets and/or component references.
    /// It consumes the board and (when present) firmware artifacts and carries
    /// the producer's validated numerical budget.
    pub fn simulation_map(
        &self,
        assertion: impl Into<String>,
        nets: &[String],
        references: &[String],
        budget: Option<ErrorBudget>,
    ) -> Result<EvidenceMap, EvidenceError> {
        let mut scoped_nets: BTreeSet<String> = nets.iter().cloned().collect();
        for (net, refs) in &self.refs_by_net {
            if references.iter().any(|reference| refs.contains(reference)) {
                scoped_nets.insert(net.clone());
            }
        }
        if scoped_nets.is_empty() {
            return Err(EvidenceError::Empty {
                field: "simulation_map.nets",
            });
        }
        let mut map = self.map_for_nets(assertion.into(), scoped_nets.into_iter().collect())?;
        let artifacts = [self.board_artifact, self.firmware_artifact]
            .into_iter()
            .flatten()
            .chain(self.supporting_artifacts.iter().copied());
        map = map.with_artifacts(&self.registry, artifacts)?;
        if let Some(budget) = budget {
            map = map.with_error_budget(budget);
        }
        Ok(map)
    }

    /// Build the map the production CI runner attaches to one evaluated
    /// assertion. Check-scoped facts use the stable result label so a waiver or
    /// an unexercised peripheral cannot leak onto a sibling assertion.
    pub fn ci_assertion_map(
        &self,
        assertion: impl Into<String>,
        nets: &[String],
        references: &[String],
        budget: Option<ErrorBudget>,
        coverage: Option<&str>,
    ) -> Result<EvidenceMap, EvidenceError> {
        let assertion = assertion.into();
        let mut scoped_nets: BTreeSet<String> = nets.iter().cloned().collect();
        for (net, refs) in &self.refs_by_net {
            if references.iter().any(|reference| refs.contains(reference)) {
                scoped_nets.insert(net.clone());
            }
        }
        // Board-wide assertions (`no_faults`, model coverage) have no explicit
        // subject. Their honest causal path is the whole extracted board.
        if scoped_nets.is_empty() {
            scoped_nets.extend(self.refs_by_net.keys().cloned());
        }
        let mut map = self.map_for_nets_with_check(
            assertion.clone(),
            scoped_nets.into_iter().collect(),
            Some(("ci", assertion.as_str())),
        )?;
        let artifacts = [self.board_artifact, self.firmware_artifact]
            .into_iter()
            .flatten()
            .chain(self.supporting_artifacts.iter().copied());
        map = map.with_artifacts(&self.registry, artifacts)?;
        if let Some(budget) = budget {
            map = map.with_error_budget(budget);
        }
        if let Some(coverage) = coverage {
            map = map.with_coverage(coverage);
        }
        Ok(map)
    }

    /// Error budget for transient/thermal/co-sim numeric claims, populated from
    /// the solver options actually used plus the run's measured windows.
    pub fn transient_error_budget(
        options: &hauksbee_solve::SolverOptions,
        start_s: f64,
        end_s: f64,
        event_time_error_s: f64,
        failed_windows: &[(f64, f64)],
        fallback_windows: &[(f64, f64, &str)],
    ) -> Result<ErrorBudget, EvidenceError> {
        let result_window = TimeWindow::new(start_s, end_s)?;
        let tolerance = IntegrationTolerance::new(
            options.reltol,
            options.vntol,
            options.abstol,
            options.chgtol,
        )?;
        let mut blocked = Vec::with_capacity(failed_windows.len() + fallback_windows.len());
        let mut failed_typed = Vec::with_capacity(failed_windows.len());
        for &(start, end) in failed_windows {
            let window = checked_subwindow("failed", start, end, result_window)?;
            blocked.push((window, "failed"));
            failed_typed.push(window);
        }
        let mut fallback_typed = Vec::with_capacity(fallback_windows.len());
        for &(start, end, method) in fallback_windows {
            let window = checked_subwindow("fallback", start, end, result_window)?;
            let method = match method {
                "reduced-step" => IntegrationMethod::ReducedStep,
                "backward-euler" => IntegrationMethod::BackwardEuler,
                "cold-start-backward-euler" => IntegrationMethod::ColdStartBackwardEuler,
                "subdivided-backward-euler" => IntegrationMethod::SubdividedBackwardEuler,
                unknown => {
                    return Err(EvidenceError::UnknownIntegrationMethod {
                        method: unknown.to_string(),
                    })
                }
            };
            blocked.push((window, "fallback"));
            fallback_typed.push((window, method));
        }
        for (i, (first, first_kind)) in blocked.iter().enumerate() {
            for (second, second_kind) in blocked.iter().skip(i + 1) {
                if windows_overlap(*first, *second) {
                    return Err(EvidenceError::OverlappingWindows {
                        first_kind,
                        second_kind,
                    });
                }
            }
        }

        let mut budget = ErrorBudget::new(tolerance).with_event_time_error(event_time_error_s)?;
        let primary = integration_method(options.integration);
        for window in uncovered_windows(result_window, blocked.iter().map(|(window, _)| *window))? {
            budget = budget.with_method(WindowMethod::new(window, primary)?);
        }
        for (window, method) in fallback_typed {
            budget = budget.with_method(WindowMethod::new(window, method)?);
        }
        for window in failed_typed {
            budget = budget.with_failed_window(window);
        }
        Ok(budget)
    }

    /// Numerical settings behind a non-transient solver result such as an AC
    /// sweep. No time method is claimed because frequency-domain solves do not
    /// integrate a time window.
    pub fn solver_error_budget(
        options: &hauksbee_solve::SolverOptions,
    ) -> Result<ErrorBudget, EvidenceError> {
        Ok(ErrorBudget::new(IntegrationTolerance::new(
            options.reltol,
            options.vntol,
            options.abstol,
            options.chgtol,
        )?))
    }

    pub fn with_maps(mut self, maps: Vec<EvidenceMap>) -> Self {
        self.maps = maps;
        self
    }

    fn map_for_nets(
        &self,
        assertion: String,
        nets: Vec<String>,
    ) -> Result<EvidenceMap, EvidenceError> {
        self.map_for_nets_with_check(assertion, nets, None)
    }

    fn map_for_nets_with_check(
        &self,
        assertion: String,
        nets: Vec<String>,
        check_scope: Option<(&str, &str)>,
    ) -> Result<EvidenceMap, EvidenceError> {
        let scope = NetScope::new(nets.clone(), None)?;
        let traversal = match check_scope {
            Some((check, key)) => {
                self.index
                    .traverse_assertion(&scope, check, key, &self.registry)?
            }
            None => self.index.traverse(&scope, &self.registry)?,
        };
        let mut map =
            EvidenceMap::from_traversal(assertion, traversal, &self.registry, self.today)?;
        let references: BTreeSet<&str> = nets
            .iter()
            .filter_map(|net| self.refs_by_net.get(net))
            .flatten()
            .map(String::as_str)
            .collect();
        let mut models = Vec::new();
        let mut parameters = Vec::new();
        for reference in references {
            let Some(fact) = self.model_by_ref.get(reference) else {
                if let Some(defaults) = self.defaults_by_ref.get(reference) {
                    for default in defaults {
                        parameters.push(ParameterProvenance::new(
                            format!("{reference}.{}", default.parameter),
                            &default.value,
                            ValueOrigin::Default {
                                assumption: default.assumption.clone(),
                            },
                        )?);
                    }
                }
                continue;
            };
            models.push(ModelOnPath::new(
                reference,
                &fact.model_id,
                fact.source.clone(),
                fact.confidence,
            )?);
            parameters.push(ParameterProvenance::new(
                format!("{reference}.model"),
                &fact.model_id,
                ValueOrigin::Model {
                    model_id: fact.model_id.clone(),
                    layer: fact.source.layer(),
                    confidence: fact.confidence,
                },
            )?);
            if let Some(defaults) = self.defaults_by_ref.get(reference) {
                for default in defaults {
                    parameters.push(ParameterProvenance::new(
                        format!("{reference}.{}", default.parameter),
                        &default.value,
                        ValueOrigin::Default {
                            assumption: default.assumption.clone(),
                        },
                    )?);
                }
            }
        }
        map = map.with_models(models);
        map.with_parameters(&self.registry, parameters)
    }

    /// Shared human rendering used by CLI and available to web presentation.
    pub fn render_plain(&self) -> String {
        if self.assumptions.is_empty() && self.maps.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n== Evidence ==\n");
        for assumption in &self.assumptions {
            let _ = writeln!(
                out,
                "[{}] {} Why: {} Effect: {} Fix: {}",
                assumption.id(),
                assumption.statement(),
                assumption.because(),
                assumption.consequence(),
                assumption.replacement()
            );
        }
        for map in &self.maps {
            let ids = map
                .assumptions()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "{}: {}{}",
                map.status(),
                map.assertion(),
                if ids.is_empty() {
                    String::new()
                } else {
                    format!(" (rests on {ids})")
                }
            );
            for model in map.models() {
                let source = model.source();
                let accuracy = if source
                    .uncertainty()
                    .iter()
                    .any(|value| matches!(value, ModelUncertainty::Unknown { .. }))
                {
                    "uncertainty unknown".to_string()
                } else {
                    source
                        .uncertainty()
                        .iter()
                        .filter_map(|value| match value {
                            ModelUncertainty::Interval {
                                low,
                                high,
                                unit,
                                kind,
                                basis,
                                ..
                            } => Some(format!("interval [{low}, {high}] {unit} {kind} ({basis})")),
                            ModelUncertainty::Unknown { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(
                    out,
                    "  model {}={} source={} layer={:?} origin={} validation={} {accuracy}",
                    model.reference(),
                    model.model_id(),
                    source.tier(),
                    source.layer(),
                    source.origin(),
                    source.validation(),
                );
            }
        }
        out
    }

    /// Add the canonical evidence fields to a legacy structured report without
    /// inventing a second JSON vocabulary. New reports should prefer
    /// `JsonReport::with_evidence`; this bridge keeps small specialist reports
    /// on the exact same `assumptions` and `evidence` field shapes.
    pub fn enrich_json(&self, mut value: serde_json::Value) -> serde_json::Value {
        let serde_json::Value::Object(object) = &mut value else {
            return value;
        };
        object.insert(
            "assumptions".to_string(),
            serde_json::to_value(&self.assumptions)
                .expect("IR assumptions have an infallible JSON representation"),
        );
        object.insert(
            "evidence".to_string(),
            serde_json::to_value(&self.maps)
                .expect("IR evidence maps have an infallible JSON representation"),
        );
        value
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn confidence(value: Confidence) -> MatchConfidence {
    match value {
        Confidence::Exact => MatchConfidence::Exact,
        Confidence::Family => MatchConfidence::High,
        Confidence::Guessed | Confidence::Unresolved => MatchConfidence::Guessed,
    }
}

fn input_artifact_kind(
    path: &Path,
    kind: crate::board_input::InputKind,
) -> (ArtifactKind, ArtifactRole) {
    use crate::board_input::InputKind;
    match kind {
        InputKind::Schematic => (ArtifactKind::KiCadSchematic, ArtifactRole::Schematic),
        InputKind::Altium => (ArtifactKind::AltiumPcbDoc, ArtifactRole::Layout),
        InputKind::Gerber => (ArtifactKind::GerberArchive, ArtifactRole::FabArchive),
        InputKind::Odb => (ArtifactKind::OdbPlusPlus, ArtifactRole::FabArchive),
        InputKind::Ipc2581 => (ArtifactKind::Ipc2581, ArtifactRole::FabArchive),
        InputKind::BoardCode => (ArtifactKind::BoardCode, ArtifactRole::Layout),
        InputKind::Text => match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("brd") => (ArtifactKind::EagleBoard, ArtifactRole::Layout),
            Some("d356") => (ArtifactKind::Ipc356, ArtifactRole::Netlist),
            Some("net") => (ArtifactKind::KiCadNetlist, ArtifactRole::Netlist),
            _ => (ArtifactKind::KiCadPcb, ArtifactRole::Layout),
        },
    }
}

fn input_contributions(kind: crate::board_input::InputKind) -> Vec<Contribution> {
    use crate::board_input::InputKind;
    let mut out = vec![Contribution {
        what: "connectivity".into(),
        detail: match kind {
            InputKind::Gerber => "connectivity reconstructed from the fabrication copper".into(),
            InputKind::Odb => "connectivity read from the ODB++ job's EDA data".into(),
            InputKind::Ipc2581 => "connectivity read from the IPC-2581 logical netlist".into(),
            _ => "component and net incidence consumed by binding and electrical checks".into(),
        },
    }];
    if matches!(
        kind,
        InputKind::Text | InputKind::Altium | InputKind::Gerber | InputKind::BoardCode
    ) {
        out.push(Contribution {
            what: "copper_geometry".into(),
            detail: match kind {
                InputKind::Gerber => {
                    "fabrication copper consumed by connectivity reconstruction".into()
                }
                _ => "layout geometry consumed by geometry-bearing checks".into(),
            },
        });
    }
    out
}

fn classify_reader_note(
    note: &str,
    board: &ExtractedBoard,
    assumptions: &mut Vec<Assumption>,
    contributions: &mut Vec<Contribution>,
    ignored: &mut Vec<IgnoredInput>,
    cross_checks: &mut Vec<CrossCheck>,
) -> Result<(), EvidenceError> {
    let lower = note.to_ascii_lowercase();
    if lower.contains("not reverse-engineered from copper") {
        contributions.push(Contribution {
            what: "connectivity".into(),
            detail: note.to_string(),
        });
        if lower.contains("clearance drc") && lower.contains("were not run") {
            assumptions.push(Assumption::not_checked(
                AssumptionSource::Reader,
                "drc",
                None,
                "the exchange input carries no native layout representation the clearance checker consumes",
                "supply the original native layout file alongside this exchange artifact, then re-run",
            ));
        }
        if lower.contains("trace-geometry si") && lower.contains("were not run") {
            assumptions.push(Assumption::not_checked(
                AssumptionSource::Reader,
                "si",
                None,
                "the exchange input carries no native routed-trace representation the SI checker consumes",
                "supply the original native layout file alongside this exchange artifact, then re-run",
            ));
        }
        return Ok(());
    }
    if lower.contains("treated every placed part as fitted")
        || lower.contains("every placed part has been treated as fitted")
    {
        let subjects = board
            .components
            .iter()
            .filter(|component| !component.reference.trim().is_empty())
            .map(|component| EntityRef::new(EntityKind::Part, component.reference.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let scope = if subjects.is_empty() {
            Scope::Board
        } else {
            Scope::Subjects(SubjectSet::new(subjects)?)
        };
        assumptions.push(Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::same("the exchange input"),
            scope,
        ));
        return Ok(());
    }
    if lower.contains("disagree") || lower.contains("mismatch") || lower.contains("contradict") {
        cross_checks.push(CrossCheck {
            what: "reader source agreement".into(),
            agreed: false,
            detail: note.to_string(),
        });
        let digest = Sha256::digest(note.as_bytes());
        let key = format!("reader-cross-check/{}", hex_digest(&digest));
        assumptions.push(Assumption::parser_limitation(
            AssumptionSource::Reader,
            Subject::new(&key, "the reader's disagreeing sources"),
            Scope::Board,
            note,
            "correct or re-export the source data so its internal connectivity descriptions agree, then re-run",
        ));
        return Ok(());
    }
    if lower.contains("board artwork") || lower.contains("nothing downstream can see") {
        ignored.push(IgnoredInput {
            what: "reader-excluded input content".into(),
            why: note.to_string(),
        });
        return Ok(());
    }

    // Notes the BINDER and stress monitor raise travel the same channel but are
    // not the reader's: attributing "R7's package is unreadable" to the input
    // reader misstates which stage of the pipeline hit the limit, on a surface
    // whose whole job is saying who assumed what. Keyed on the prefixes those
    // producers emit.
    if let Some(source) = note
        .split_once(':')
        .and_then(|(prefix, _)| match prefix.trim() {
            "stress" => Some(AssumptionSource::Binder),
            "models" => Some(AssumptionSource::Binder),
            _ => None,
        })
    {
        let digest = Sha256::digest(note.as_bytes());
        let key = format!("binder-note/{}", hex_digest(&digest));
        assumptions.push(Assumption::reduced_fidelity(
            source,
            Subject::new(&key, "the binder's model and rating coverage"),
            Scope::Board,
            note,
            "give the part a model carrying the missing rating, or a footprint / BOM line              naming its package, then re-run",
        ));
        return Ok(());
    }

    // Unknown notes remain visible and stable; only known positive accounting is
    // promoted to provenance instead of becoming a caveat.
    let digest = Sha256::digest(note.as_bytes());
    let key = format!("reader-note/{}", hex_digest(&digest));
    assumptions.push(Assumption::reduced_fidelity(
        AssumptionSource::Reader,
        Subject::new(&key, "the input reader's coverage"),
        Scope::Board,
        note,
        "supply the original native layout and BOM, or correct the source export, then re-run",
    ));
    Ok(())
}

fn guessed_pin_role(message: &str) -> (String, String) {
    let pin = message
        .strip_prefix("pad ")
        .and_then(|rest| rest.split_once(" role "))
        .map(|(pin, _)| pin.trim().to_string())
        .unwrap_or_else(|| "an unnamed pin".into());
    let role = message
        .split("role '")
        .nth(1)
        .and_then(|rest| rest.split_once('\'').map(|(role, _)| role.to_string()))
        .unwrap_or_else(|| "inferred".into());
    (pin, role)
}

fn documented_default(warning: Option<&str>) -> Option<(String, String)> {
    let warning = warning?;
    if warning.contains("vreg model has no `vout` param") {
        let value = warning
            .split("assumed ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("5.0")
            .to_string();
        return Some(("vout".into(), value));
    }
    None
}
