//! `hauksbee.run-record/1`: the one document a run produces.
//!
//! Every surface renders from a [`RunRecord`]: the terminal, `--json`, `JUnit`,
//! SARIF, the KiCad report, the BOM CSV, the Markdown/HTML document and the
//! app. It wraps what already exists instead of replacing it:
//!
//! * `manifest` is the engine's `RunManifest` v1, verbatim (the reproduction
//!   identity). It lives in `hauksbee-engine`, which depends on this crate, so
//!   it is carried as JSON; the schema pins only its identity keys.
//! * `evidence` is the existing evidence spine (`inventory`, `assumptions`,
//!   `maps`) from [`crate::evidence`], serialized by its own types. Those types
//!   deliberately do not implement `Deserialize` (a status must be re-derived,
//!   never read back), so a record read from disk keeps them as JSON; the
//!   schema is generated from the typed spine all the same.
//! * every verdict, grade, severity, check name and exit code is a
//!   [`crate::vocabulary`] type, so the record cannot spell one differently.
//!
//! `record_id` is `sha256:` over the canonical JSON of the record without
//! `record_id`, `artifacts` and `signoffs`: the citable run identifier.
//! Artifacts and sign-offs are attached after sealing (a `JUnit` file or an
//! HTML document carries the id, so it cannot also be inside it).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use schemars::{json_schema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::evidence::{
    ArtifactProvenance, Assumption, EvidenceMap, IntegrationMethod, MatchConfidence, ModelSource,
};
use crate::vocabulary::{
    self, Check, EvidenceGrade, ExitClass, Severity, VerdictState, EXIT_CONTRACT,
};

/// The `schema` value of every record this crate writes.
pub const RUN_RECORD_SCHEMA: &str = "hauksbee.run-record/1";

/// One run: identity, method, scope, verdict, parts, findings, assertions and
/// the evidence they rest on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(
    description = "hauksbee.run-record/1: one run of hauksbee or hauksbee-ci, \
                          the single document every report renders from."
)]
pub struct RunRecord {
    /// Always [`RUN_RECORD_SCHEMA`].
    pub schema: String,
    /// `sha256:<hex>` of the canonical record without `record_id`,
    /// `artifacts` and `signoffs`. Empty until [`RunRecord::seal`].
    pub record_id: String,
    pub tool: Tool,
    /// RFC 3339 UTC start time.
    pub started_utc: String,
    /// Wall-clock duration, seconds.
    pub wall_s: f64,
    /// The engine's `RunManifest` v1 verbatim; null when none was captured.
    #[schemars(schema_with = "manifest_schema")]
    pub manifest: Option<Value>,
    #[serde(default)]
    pub backends: Vec<Backend>,
    pub methods: Methods,
    pub scope: RunScope,
    pub verdict: Verdict,
    #[serde(default)]
    pub parts: Vec<Part>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// `hauksbee-ci` spec assertions; empty for `hauksbee run`.
    #[serde(default)]
    pub assertions: Vec<Assertion>,
    #[serde(default)]
    pub evidence: EvidenceSpine,
    /// Files rendered or written by the run. Not covered by `record_id`.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    /// Approvals appended after the run. Not covered by `record_id`.
    #[serde(default)]
    pub signoffs: Vec<Signoff>,
}

/// The program that produced the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Tool {
    /// `hauksbee`, `hauksbee-ci` or `hauksbee-server`.
    pub name: String,
    pub version: String,
    /// Git revision the binary was built from, when known.
    pub git: Option<String>,
    /// Target OS, as Rust names it (`linux`, `macos`, `windows`).
    pub os: String,
    /// Target architecture (`x86_64`, `aarch64`).
    pub arch: String,
    /// Compiled cargo features (which backends this binary can run).
    #[serde(default)]
    pub features: Vec<String>,
}

/// An external simulator or oracle the run used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Backend {
    /// `ngspice`, `simavr`, `qemu`, `renode`, `kicad-cli`, ...
    pub name: String,
    pub version: Option<String>,
    /// SHA-256 of the binary, when it was hashed.
    pub sha256: Option<String>,
}

/// How the numbers were produced: the record's Methods section.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Methods {
    /// e.g. `MNA/Newton`.
    pub solver: Option<String>,
    /// Integration methods used (the primary first, then any fallbacks).
    #[serde(default)]
    pub integration: Vec<IntegrationMethod>,
    /// Solver tolerances by name (`reltol`, `vntol`, `abstol`, `chgtol`, ...).
    #[serde(default)]
    pub tolerances: BTreeMap<String, f64>,
    #[serde(default)]
    pub seeds: Vec<u64>,
    #[serde(default)]
    pub corners: Vec<String>,
    pub clearance_rules: Option<ClearanceRules>,
}

/// Where the DRC clearance came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClearanceRules {
    /// `board`, `project`, `cli` or `defaulted`.
    pub source: String,
    pub value_mm: Option<f64>,
}

/// What the verdict covers, and what it does not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunScope {
    #[serde(default)]
    pub checks_run: Vec<Check>,
    #[serde(default)]
    pub checks_not_run: Vec<Check>,
    /// True when a single-check mode ran: the verdict covers `checks_run` only.
    pub partial: bool,
    /// Key parts with no usable model.
    #[serde(default)]
    pub unjudged_refs: Vec<String>,
    #[serde(default)]
    pub not_covered: Vec<Condition>,
}

/// A named condition or gap, with how to lift it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Condition {
    /// Stable id, e.g. `default-clearance-rules`.
    pub id: String,
    pub text: String,
    pub how_to_lift: Option<String>,
}

/// The run's outcome in the [`crate::vocabulary`] words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Verdict {
    pub state: VerdictState,
    /// [`VerdictState::label`], stated so a reader needs no table.
    pub label: String,
    /// Why it is not a pass, and what to do next.
    pub reason: Option<String>,
    /// What a pass rests on. Non-empty exactly when `state` is
    /// `pass_conditional`.
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub exit_code: u8,
    /// The exit table `exit_code` belongs to.
    pub exit_contract: String,
}

impl Verdict {
    /// Build a verdict with its label and exit code filled from the state. A
    /// pass with conditions becomes `pass_conditional`, and a conditional pass
    /// with none becomes `pass`, so the two can never disagree.
    #[must_use]
    pub fn new(state: VerdictState, reason: Option<String>, conditions: Vec<Condition>) -> Self {
        let state = match state {
            VerdictState::Pass | VerdictState::PassConditional if conditions.is_empty() => {
                VerdictState::Pass
            }
            VerdictState::Pass | VerdictState::PassConditional => VerdictState::PassConditional,
            other => other,
        };
        Self {
            state,
            label: state.label().to_string(),
            reason,
            conditions,
            exit_code: state.exit_class().code(),
            exit_contract: EXIT_CONTRACT.to_string(),
        }
    }

    /// The exit class of this verdict.
    #[must_use]
    pub const fn exit_class(&self) -> ExitClass {
        self.state.exit_class()
    }
}

/// One placed part and the model that stood in for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Part {
    #[serde(rename = "ref")]
    pub reference: String,
    pub value: Option<String>,
    pub mpn: Option<String>,
    pub footprint: Option<String>,
    /// The footprint UUID in the source CAD file.
    pub uuid: Option<String>,
    /// None for a part with no usable model (it is in `scope.unjudged_refs`).
    pub model: Option<PartModel>,
    #[serde(default)]
    pub ratings: Vec<Rating>,
}

/// The model bound to a part, with the existing canonical provenance record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PartModel {
    pub id: String,
    pub confidence: Option<MatchConfidence>,
    /// The evidence spine's model provenance, verbatim.
    pub source: ModelSource,
    /// Datasheets and documents the model's numbers came from.
    #[serde(default)]
    pub references: Vec<SourceReference>,
}

/// A cited source: the model card's `[[models.source.references]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceReference {
    pub url: String,
    pub title: String,
    /// Page, table or figure, e.g. `p. 7, Table 3`.
    pub locator: String,
    pub sha256: Option<String>,
}

/// One rating checked on a part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rating {
    /// e.g. `Vds`, `Pd`, `Tj`.
    pub param: String,
    pub unit: Option<String>,
    pub limit: f64,
    /// Worst value the run saw; None when it was not judged.
    pub worst: Option<f64>,
    /// `limit - worst` in the rating's direction; negative is a violation.
    pub margin: Option<f64>,
    #[serde(default)]
    pub finding_ids: Vec<String>,
}

/// One finding, normalised across checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Finding {
    /// The stable group fingerprint (today's `JsonFinding.group`), e.g.
    /// `lint/unrouted_pad/serious/711e36fbcb84`. SARIF `partialFingerprints`,
    /// GitLab `fingerprint` and `hauksbee diff` all key on it.
    pub id: String,
    pub check: Check,
    pub kind: String,
    pub severity: Severity,
    /// The gate fails the run on it. A waived finding keeps the flag and
    /// carries `waiver`; it does not fail the run.
    pub blocking: bool,
    pub message: String,
    /// The plain-language version, when it differs.
    pub plain: Option<String>,
    pub fix: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub nets: Vec<String>,
    /// Board coordinates, millimetres.
    pub location_mm: Option<[f64; 2]>,
    pub layer: Option<String>,
    /// CAD UUIDs of the footprints or pads involved.
    #[serde(default)]
    pub uuid_refs: Vec<String>,
    pub evidence: Option<EvidenceLink>,
    pub waiver: Option<Waiver>,
}

impl Finding {
    /// Whether this finding fails the run: blocking and not waived.
    #[must_use]
    pub const fn blocks(&self) -> bool {
        self.blocking && self.waiver.is_none()
    }

    /// SARIF `result.level`.
    #[must_use]
    pub const fn sarif_level(&self) -> &'static str {
        vocabulary::sarif_level(self.severity, self.blocking)
    }

    /// KiCad report `severity`.
    #[must_use]
    pub const fn kicad_severity(&self) -> &'static str {
        vocabulary::kicad_severity(self.severity, self.blocking, self.waiver.is_some())
    }
}

/// Which evidence map a finding or assertion rests on, and its grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvidenceLink {
    pub grade: EvidenceGrade,
    /// Index into `evidence.maps`.
    pub map_index: Option<usize>,
}

/// An accepted risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Waiver {
    pub reason: String,
    /// ISO date the waiver expires.
    pub until: Option<String>,
    pub approved_by: Option<String>,
    pub approved_on: Option<String>,
    pub ticket: Option<String>,
}

/// One spec assertion (`hauksbee-ci`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Assertion {
    /// Stable id: `<spec file>#<label>`.
    pub id: String,
    pub label: String,
    pub outcome: AssertionOutcome,
    pub spec_file: Option<String>,
    pub spec_line: Option<u32>,
    pub evidence: Option<EvidenceLink>,
    /// The `hauksbee-ci --json` v2 `CiJsonAssertion`, verbatim.
    pub detail: Value,
}

/// How one assertion came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssertionOutcome {
    Pass,
    Fail,
    /// Its evidence was undermined or the run could not judge it.
    Invalid,
    /// It failed and a waiver covers it.
    Waived,
    /// Measured with no limit to judge against.
    Measured,
}

/// The evidence spine, as its own types serialize it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvidenceSpine {
    #[serde(default)]
    #[schemars(with = "Vec<ArtifactProvenance>")]
    pub inventory: Vec<Value>,
    #[serde(default)]
    #[schemars(with = "Vec<Assumption>")]
    pub assumptions: Vec<Value>,
    #[serde(default)]
    #[schemars(with = "Vec<EvidenceMap>")]
    pub maps: Vec<Value>,
}

impl EvidenceSpine {
    /// Serialize the typed spine a run produced.
    ///
    /// # Errors
    ///
    /// A serialization error from `serde_json` (non-finite floats).
    pub fn from_typed(
        inventory: &[ArtifactProvenance],
        assumptions: &[Assumption],
        maps: &[EvidenceMap],
    ) -> serde_json::Result<Self> {
        fn values<T: Serialize>(items: &[T]) -> serde_json::Result<Vec<Value>> {
            items.iter().map(serde_json::to_value).collect()
        }
        Ok(Self {
            inventory: values(inventory)?,
            assumptions: values(assumptions)?,
            maps: values(maps)?,
        })
    }
}

/// A file the run wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Artifact {
    pub role: ArtifactOutput,
    /// Relative to the working directory the run was started in.
    pub path: String,
    pub sha256: Option<String>,
}

/// What kind of file an [`Artifact`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactOutput {
    Json,
    Junit,
    Sarif,
    Codequality,
    KicadReport,
    BomCsv,
    Markdown,
    Html,
    Waveforms,
    Csv,
    Manifest,
}

/// One approval of a sealed record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Signoff {
    pub by: String,
    /// RFC 3339 UTC time of the approval.
    pub on: String,
    pub statement: String,
    /// The `record_id` the signer approved.
    pub record_sha256: String,
}

impl RunRecord {
    /// A record with the header and verdict filled and every list empty.
    /// Call [`Self::seal`] once the producer has filled the rest.
    #[must_use]
    pub fn new(tool: Tool, started_utc: impl Into<String>, wall_s: f64, verdict: Verdict) -> Self {
        Self {
            schema: RUN_RECORD_SCHEMA.to_string(),
            record_id: String::new(),
            tool,
            started_utc: started_utc.into(),
            wall_s,
            manifest: None,
            backends: Vec::new(),
            methods: Methods::default(),
            scope: RunScope::default(),
            verdict,
            parts: Vec::new(),
            findings: Vec::new(),
            assertions: Vec::new(),
            evidence: EvidenceSpine::default(),
            artifacts: Vec::new(),
            signoffs: Vec::new(),
        }
    }

    /// The canonical JSON `record_id` is computed over: object keys sorted,
    /// no whitespace, `record_id`, `artifacts` and `signoffs` removed.
    ///
    /// # Errors
    ///
    /// A serialization error from `serde_json` (non-finite floats).
    pub fn canonical_json(&self) -> serde_json::Result<String> {
        let mut value = serde_json::to_value(self)?;
        if let Value::Object(map) = &mut value {
            for key in ["record_id", "artifacts", "signoffs"] {
                map.remove(key);
            }
        }
        serde_json::to_string(&canonicalize(value))
    }

    /// `sha256:<hex>` of [`Self::canonical_json`].
    ///
    /// # Errors
    ///
    /// As [`Self::canonical_json`].
    pub fn compute_id(&self) -> serde_json::Result<String> {
        let digest = Sha256::digest(self.canonical_json()?.as_bytes());
        let mut id = String::from("sha256:");
        for b in digest {
            // Writing to a String cannot fail.
            let _ = write!(id, "{b:02x}");
        }
        Ok(id)
    }

    /// Fill `record_id`. Call after the last change to anything it covers.
    ///
    /// # Errors
    ///
    /// As [`Self::canonical_json`].
    pub fn seal(&mut self) -> serde_json::Result<()> {
        self.record_id = self.compute_id()?;
        Ok(())
    }

    /// Whether `record_id` matches the content (false for an unsealed record).
    #[must_use]
    pub fn id_matches(&self) -> bool {
        self.compute_id().is_ok_and(|id| id == self.record_id)
    }

    /// Append an approval of the sealed record. It does not change
    /// `record_id`.
    pub fn sign(
        &mut self,
        by: impl Into<String>,
        on: impl Into<String>,
        statement: impl Into<String>,
    ) {
        self.signoffs.push(Signoff {
            by: by.into(),
            on: on.into(),
            statement: statement.into(),
            record_sha256: self.record_id.clone(),
        });
    }

    /// The findings that fail the run.
    pub fn blocking_findings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.blocks())
    }

    /// The headline counts, waived findings excluded: how many block, then
    /// how many of each grade, highest first ("2 blocking, 0 serious, 2 worth
    /// a look").
    #[must_use]
    pub fn headline_counts(&self) -> (usize, Vec<(Severity, usize)>) {
        let open = || self.findings.iter().filter(|f| f.waiver.is_none());
        let by_grade = Severity::ALL
            .iter()
            .map(|&s| (s, open().filter(|f| f.severity == s).count()))
            .collect();
        (open().filter(|f| f.blocking).count(), by_grade)
    }
}

/// Sort every object's keys, recursively. `serde_json` may keep insertion
/// order (its `preserve_order` feature is workspace-unified), so the order is
/// set explicitly rather than assumed.
fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, canonicalize(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

/// The manifest is the engine's type, out of this crate's reach; pin the
/// identity keys every v1 manifest has and leave the rest to its own contract.
fn manifest_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "description": "The engine's RunManifest v1 verbatim (crates/hauksbee-engine/src/run_manifest.rs): \
                        exact input hashes, argv, options, environment-selector hashes. Null when no \
                        manifest was captured.",
        "type": ["object", "null"],
        "required": ["schema_version", "manifest_id"],
        "properties": {
            "schema_version": { "type": "integer", "minimum": 1 },
            "manifest_id": { "type": "string", "pattern": "^sha256:[0-9a-f]{64}$" }
        }
    })
}
