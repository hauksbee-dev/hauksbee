//! Production adapter from a bound board to the shared IR evidence spine.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use crate::report::{BindOutcome, BindReport};
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::evidence::{
    Assumption, AssumptionSource, CausalPathIndex, EvidenceError, EvidenceMap, EvidenceRegistry,
    EvidenceStatus, MatchConfidence, ModelLayer, ModelOnPath, NetScope, ParameterProvenance,
    RunDate, Scope, Subject, ValueOrigin,
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
        })
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
            maps.push(self.map_for_nets(finding.message.clone(), nets.into_iter().collect())?);
        }
        Ok(maps)
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
