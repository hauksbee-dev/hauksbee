//! Production adapter from a bound board to the shared IR evidence spine.
//!
//! [`BoardEvidence`] is built once per run from the extracted board and its
//! [`BindReport`]: it registers the input/firmware artifacts with their
//! hashes, records bind-time assumptions (unresolved parts, substitutions,
//! guessed roles), and derives per-assertion [`EvidenceMap`]s from the board's
//! net-part incidence, so every reported finding can say which artifacts,
//! models and assumptions its claim actually rests on, and an undermined
//! input makes the dependent claims invalid rather than silently green.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;

use crate::report::{BindOutcome, BindReport};
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::evidence::{
    ArtifactId, ArtifactKind, ArtifactProvenance, ArtifactRole, Assumption, AssumptionSource,
    CausalPathIndex, Contribution, ErrorBudget, EvidenceError, EvidenceMap, EvidenceRegistry,
    EvidenceStatus, IntegrationMethod, IntegrationTolerance, MatchConfidence, ModelLayer,
    ModelOnPath, NetScope, ParameterProvenance, RunDate, Scope, Subject, TimeWindow, ValueOrigin,
    WindowMethod,
};
use hauksbee_models::Confidence;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
struct ModelFact {
    model_id: String,
    confidence: MatchConfidence,
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
        let mut seen_reader_notes = BTreeSet::new();
        for note in reader_notes {
            let normalized = note.trim();
            if !seen_reader_notes.insert(normalized) {
                continue;
            }
            // The id belongs to the limitation, not its position in the reader's
            // note vector. Positional ids silently changed identity whenever a
            // reader learned one additional limitation or reordered its notes.
            // A full SHA-256 keeps the id bounded when it is repeated on every
            // affected map while making collisions computationally infeasible.
            let digest = Sha256::digest(normalized.as_bytes());
            let key = format!("reader-note/{}", hex_digest(&digest));
            assumptions.push(Assumption::reduced_fidelity(
                AssumptionSource::Reader,
                Subject::new(&key, "the input reader's coverage"),
                Scope::Board,
                note,
                "supply the original native layout and BOM, or correct the source export, then re-run",
            ));
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
                let Some(row) = rows.get(reference.as_str()) else {
                    continue;
                };
                let Some(model_id) = row.model_id.as_deref() else {
                    continue;
                };
                let confidence = match row.confidence {
                    Confidence::Exact => MatchConfidence::Exact,
                    Confidence::Family => MatchConfidence::High,
                    Confidence::Guessed | Confidence::Unresolved => MatchConfidence::Guessed,
                };
                models.push(ModelOnPath::new(
                    reference,
                    model_id,
                    ModelLayer::Unspecified,
                    confidence,
                )?);
                parameters.push(ParameterProvenance::new(
                    format!("{reference}.model"),
                    model_id,
                    ValueOrigin::Model {
                        model_id: model_id.to_string(),
                        layer: ModelLayer::Unspecified,
                        confidence,
                    },
                )?);
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
        let artifact = ArtifactProvenance::new(
            path.to_string_lossy(),
            artifact_kind,
            role,
            digest,
            reader_assumptions,
        )?
        .with_contributions(vec![
            Contribution {
                what: "connectivity".into(),
                detail: "component and net incidence consumed by binding and electrical checks"
                    .into(),
            },
            Contribution {
                what: "copper_geometry".into(),
                detail: "layout geometry consumed by DRC when this input format carries it".into(),
            },
        ]);
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
        let empty: Vec<String> = Vec::new();
        let incidence: Vec<(&str, &[String])> = nets
            .iter()
            .map(|net| (net.as_str(), empty.as_slice()))
            .collect();
        let index = CausalPathIndex::from_net_parts(incidence)?;
        let scope = NetScope::new(nets.iter().map(String::as_str), None)?;
        let traversal = index.traverse(&scope, &self.registry)?;
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
            .flatten();
        map = map.with_artifacts(&self.registry, artifacts)?;
        if let Some(budget) = budget {
            map = map.with_error_budget(budget);
        }
        Ok(map)
    }

    /// Error budget for transient/thermal/co-sim numeric claims, populated from
    /// the solver's actual default tolerances plus the run's measured windows.
    pub fn transient_error_budget(
        start_s: f64,
        end_s: f64,
        event_time_error_s: f64,
        failed_windows: &[(f64, f64)],
        fallback_windows: &[(f64, f64, &str)],
    ) -> Result<ErrorBudget, EvidenceError> {
        let options = hauksbee_solve::SolverOptions::default();
        let tolerance = IntegrationTolerance::new(options.reltol, options.abstol, options.chgtol)?;
        let mut budget = ErrorBudget::new(tolerance)
            .with_method(WindowMethod::new(
                TimeWindow::new(start_s, end_s)?,
                IntegrationMethod::Trapezoidal,
                0.0,
            )?)
            .with_event_time_error(event_time_error_s)?;
        for &(start, end) in failed_windows {
            budget = budget.with_failed_window(TimeWindow::new(start, end)?);
        }
        for &(start, end, method) in fallback_windows {
            let (method, cost) = match method {
                "reduced-step" => (IntegrationMethod::ReducedStep, 0.001),
                "backward-euler" => (IntegrationMethod::BackwardEuler, 0.1),
                "cold-start-backward-euler" => (IntegrationMethod::ColdStartBackwardEuler, 0.1),
                "subdivided-backward-euler" => (IntegrationMethod::SubdividedBackwardEuler, 0.1),
                _ => (IntegrationMethod::BackwardEuler, 0.1),
            };
            budget = budget.with_method(WindowMethod::new(
                TimeWindow::new(start, end)?,
                method,
                cost,
            )?);
        }
        Ok(budget)
    }

    /// Numerical settings behind a non-transient solver result such as an AC
    /// sweep. No time method is claimed because frequency-domain solves do not
    /// integrate a time window.
    pub fn solver_error_budget() -> Result<ErrorBudget, EvidenceError> {
        let options = hauksbee_solve::SolverOptions::default();
        Ok(ErrorBudget::new(IntegrationTolerance::new(
            options.reltol,
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
        let scope = NetScope::new(nets.clone(), None)?;
        let traversal = self.index.traverse(&scope, &self.registry)?;
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
                continue;
            };
            models.push(ModelOnPath::new(
                reference,
                &fact.model_id,
                ModelLayer::Unspecified,
                fact.confidence,
            )?);
            parameters.push(ParameterProvenance::new(
                format!("{reference}.model"),
                &fact.model_id,
                ValueOrigin::Model {
                    model_id: fact.model_id.clone(),
                    layer: ModelLayer::Unspecified,
                    confidence: fact.confidence,
                },
            )?);
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
