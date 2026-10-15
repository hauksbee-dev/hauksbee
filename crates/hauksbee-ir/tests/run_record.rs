//! `hauksbee.run-record/1`: serde round trip, the canonical `record_id`, and
//! the verdict normalisation the constructors guarantee.

use hauksbee_ir::evidence::{
    Assumption, MatchConfidence, ModelLayer, ModelSource, ModelSourceTier, ModelUncertainty,
    ModelValidation,
};
use hauksbee_ir::run_record::{
    Artifact, ArtifactOutput, Condition, EvidenceLink, EvidenceSpine, Finding, Part, PartModel,
    Rating, RunRecord, SourceReference, Tool, Verdict, Waiver, RUN_RECORD_SCHEMA,
};
use hauksbee_ir::vocabulary::{Check, EvidenceGrade, Severity, VerdictState};
use serde_json::{json, Value};

fn finding(id: &str, severity: Severity, blocking: bool) -> Finding {
    Finding {
        id: id.into(),
        check: Check::Lint,
        kind: "unrouted_pad".into(),
        severity,
        blocking,
        message: "pad U1.3 is not routed".into(),
        plain: None,
        fix: Some("route it".into()),
        refs: vec!["U1".into()],
        nets: vec!["/D+".into()],
        location_mm: Some([12.5, 0.1 + 0.2]),
        layer: Some("F.Cu".into()),
        uuid_refs: vec![],
        evidence: Some(EvidenceLink {
            grade: EvidenceGrade::Qualified,
            map_index: Some(0),
        }),
        waiver: None,
    }
}

/// A record with every section populated, sealed.
pub fn sample() -> RunRecord {
    let tool = Tool {
        name: "hauksbee".into(),
        version: "0.1.0-beta.6".into(),
        git: Some("94e4330a".into()),
        os: "linux".into(),
        arch: "x86_64".into(),
        features: vec!["avr".into(), "qemu".into()],
    };
    let verdict = Verdict::new(
        VerdictState::Pass,
        None,
        vec![Condition {
            id: "default-clearance-rules".into(),
            text: "DRC ran on the default 0.2 mm clearance.".into(),
            how_to_lift: Some("Put the .kicad_pro next to the board.".into()),
        }],
    );
    let mut r = RunRecord::new(tool, "2026-10-15T09:30:00Z", 2.5, verdict);
    r.manifest = Some(json!({
        "schema_version": 1,
        "manifest_id": format!("sha256:{}", "a".repeat(64)),
        "invocation": {"argv": ["hauksbee", "run", "board.kicad_pcb", "--check"], "options": {}}
    }));
    r.methods.solver = Some("MNA/Newton".into());
    r.methods.tolerances.insert("reltol".into(), 1e-3);
    r.methods.tolerances.insert("vntol".into(), 1e-6);
    r.methods.seeds = vec![0, 1];
    r.scope.checks_run = vec![Check::Drc, Check::Lint];
    r.scope.checks_not_run = vec![Check::Thermal];
    let source = ModelSource::new(
        ModelSourceTier::CuratedLibrary,
        ModelLayer::Builtin,
        "builtin:atmega328p",
        ModelValidation::PhysicalBoundsOnly,
        vec![ModelUncertainty::Unknown {
            parameter: "vih".into(),
            reason: "not published".into(),
        }],
    )
    .unwrap();
    r.parts.push(Part {
        reference: "U1".into(),
        value: Some("ATmega328P".into()),
        mpn: None,
        footprint: Some("TQFP-32".into()),
        uuid: None,
        model: Some(PartModel {
            id: "atmega328p".into(),
            confidence: Some(MatchConfidence::Exact),
            source,
            references: vec![SourceReference {
                url: "https://example.com/atmega328p.pdf".into(),
                title: "ATmega328P datasheet".into(),
                locator: "p. 258, Table 28-1".into(),
                sha256: None,
            }],
        }),
        ratings: vec![Rating {
            param: "Vcc".into(),
            unit: Some("V".into()),
            limit: 6.0,
            worst: Some(5.02),
            margin: Some(0.98),
            finding_ids: vec![],
        }],
    });
    r.findings.push(finding(
        "lint/unrouted_pad/serious/711e36fbcb84",
        Severity::Serious,
        true,
    ));
    let mut waived = finding("drc/clearance/warning/0b1c", Severity::Warning, true);
    waived.check = Check::Drc;
    waived.waiver = Some(Waiver {
        reason: "fab confirmed".into(),
        until: Some("2027-01-01".into()),
        approved_by: Some("reviewer".into()),
        approved_on: None,
        ticket: None,
    });
    r.findings.push(waived);
    r.evidence = EvidenceSpine::from_typed(
        &[],
        &[Assumption::open_part("U2", "XC6206", "no model")],
        &[],
    )
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn serde_round_trip_is_lossless() {
    let r = sample();
    let text = serde_json::to_string_pretty(&r).unwrap();
    let back: RunRecord = serde_json::from_str(&text).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.schema, RUN_RECORD_SCHEMA);
    // The id still verifies after the trip through text (exact float parsing).
    assert!(back.id_matches(), "{}", back.record_id);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["parts"][0]["ref"], "U1");
    assert_eq!(v["verdict"]["state"], "pass_conditional");
    assert_eq!(v["findings"][0]["evidence"]["grade"], "qualified");
}

#[test]
fn record_id_ignores_key_order_and_whitespace() {
    /// JSON text with every object's keys in reverse order and extra
    /// whitespace, written by hand so no map type can re-sort it.
    fn reversed_text(v: &Value) -> String {
        match v {
            Value::Object(m) => {
                let fields: Vec<String> = m
                    .iter()
                    .rev()
                    .map(|(k, v)| {
                        format!("\n  {} :  {}", Value::from(k.as_str()), reversed_text(v))
                    })
                    .collect();
                format!("{{{}\n}}", fields.join(" ,"))
            }
            Value::Array(a) => {
                let items: Vec<String> = a.iter().map(reversed_text).collect();
                format!("[ {} ]", items.join(" ,\t"))
            }
            leaf => leaf.to_string(),
        }
    }
    let r = sample();
    assert!(r.record_id.starts_with("sha256:") && r.record_id.len() == 71);
    let shuffled = reversed_text(&serde_json::to_value(&r).unwrap());
    let back: RunRecord = serde_json::from_str(&shuffled).unwrap();
    assert_eq!(back.compute_id().unwrap(), r.record_id);
    assert_eq!(back.canonical_json().unwrap(), r.canonical_json().unwrap());
    assert!(!r.canonical_json().unwrap().contains('\n'));
}

#[test]
fn record_id_excludes_signoffs_and_artifacts_but_covers_content() {
    let mut r = sample();
    let id = r.record_id.clone();
    r.sign("A. Reviewer", "2026-10-16T10:00:00Z", "Approved for fab.");
    r.artifacts.push(Artifact {
        role: ArtifactOutput::Junit,
        path: "out/report.junit.xml".into(),
        sha256: None,
    });
    assert_eq!(r.compute_id().unwrap(), id);
    assert!(r.id_matches());
    assert_eq!(r.signoffs[0].record_sha256, id);

    r.findings[0].message.push('!');
    assert_ne!(r.compute_id().unwrap(), id, "content changes move the id");
    assert!(!r.id_matches());
}

#[test]
fn verdict_constructor_keeps_state_and_conditions_consistent() {
    let c = Condition {
        id: "x".into(),
        text: "t".into(),
        how_to_lift: None,
    };
    let v = Verdict::new(VerdictState::Pass, None, vec![c.clone()]);
    assert_eq!(v.state, VerdictState::PassConditional);
    assert_eq!(v.label, "pass, conditionally");
    assert_eq!(v.exit_code, 0);
    let v = Verdict::new(VerdictState::PassConditional, None, vec![]);
    assert_eq!(v.state, VerdictState::Pass);
    let v = Verdict::new(VerdictState::PartlyJudged, Some("U1 open".into()), vec![c]);
    assert_eq!((v.state, v.exit_code), (VerdictState::PartlyJudged, 3));
    assert_eq!(v.exit_contract, "hauksbee/1");
}

#[test]
fn blocking_and_headline_counts_skip_waived_findings() {
    let r = sample();
    let blocking: Vec<&str> = r.blocking_findings().map(|f| f.id.as_str()).collect();
    assert_eq!(blocking, ["lint/unrouted_pad/serious/711e36fbcb84"]);
    let (n_blocking, by_grade) = r.headline_counts();
    assert_eq!(n_blocking, 1);
    assert_eq!(by_grade[0], (Severity::Serious, 1));
    assert_eq!(by_grade[1], (Severity::Warning, 0));
    assert_eq!(r.findings[0].sarif_level(), "error");
    assert_eq!(r.findings[1].kicad_severity(), "exclusion");
}
