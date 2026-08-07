use hauksbee_ir::evidence::{
    ArtifactKind, ArtifactProvenance, ArtifactRole, Assumption, AssumptionId, AssumptionKind,
    CausalPathIndex, EntityKind, EntityRef, ErrorBudget, EvidenceError, EvidenceMap,
    EvidenceRegistry, EvidenceStatus, IntegrationMethod, IntegrationTolerance, MatchConfidence,
    ModelLayer, ModelOnPath, ModelSource, ModelSourceTier, ModelUncertainty, ModelValidation,
    NetScope, ParameterProvenance, ParameterRef, Residual, RunDate, Scope, SubjectSet, TimeWindow,
    ValueOrigin, WindowMethod,
};

fn today() -> RunDate {
    RunDate::from_epoch_days(20_666)
}

fn tolerance() -> IntegrationTolerance {
    IntegrationTolerance::new(1e-3, 1e-12, 1e-14).expect("valid solver tolerances")
}

#[test]
fn invalid_numeric_evidence_returns_typed_errors_instead_of_disappearing() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
        assert!(matches!(
            IntegrationTolerance::new(value, 1e-12, 1e-14),
            Err(EvidenceError::NonFinite { .. } | EvidenceError::NonPositive { .. })
        ));
    }

    assert!(matches!(
        TimeWindow::new(2.0, 1.0),
        Err(EvidenceError::InvertedWindow { .. })
    ));
    assert!(matches!(
        ModelUncertainty::new("Q2.beta", 200.0, 100.0, "datasheet limits"),
        Err(EvidenceError::InvertedInterval { .. })
    ));

    let invalid_method = WindowMethod::new(
        TimeWindow::new(0.0, 1.0).unwrap(),
        IntegrationMethod::Trapezoidal,
        -0.1,
    );
    assert!(matches!(
        invalid_method,
        Err(EvidenceError::Negative { .. })
    ));

    let invalid_budget = ErrorBudget::new(tolerance()).with_event_time_error(f64::NAN);
    assert!(matches!(
        invalid_budget,
        Err(EvidenceError::NonFinite { .. })
    ));
}

#[test]
fn only_a_validated_traversal_can_construct_a_clean_or_undermined_map() {
    let registry = EvidenceRegistry::new(vec![Assumption::open_part(
        "X",
        "XC6206",
        "no model matched",
    )])
    .unwrap();
    let graph = CausalPathIndex::from_net_parts([
        ("3V3", ["X", "C1", "U5"].as_slice()),
        ("VBUS", ["J1", "C9", "D2"].as_slice()),
    ])
    .unwrap();

    let rail = graph
        .traverse(&NetScope::new(["3V3"], None).unwrap(), &registry)
        .unwrap();
    let unrelated = graph
        .traverse(&NetScope::new(["VBUS"], None).unwrap(), &registry)
        .unwrap();

    let undermined =
        EvidenceMap::from_traversal("3V3 stays above 3.1 V", rail, &registry, today()).unwrap();
    let clean =
        EvidenceMap::from_traversal("VBUS stays below 5.5 V", unrelated, &registry, today())
            .unwrap();

    assert_eq!(undermined.status(), EvidenceStatus::Undermined);
    assert_eq!(undermined.assumptions().len(), 1, "vacuous mapping fails");
    assert_eq!(clean.status(), EvidenceStatus::Clean);
    assert!(clean.assumptions().is_empty(), "saturated mapping fails");

    let unknown = graph.traverse(&NetScope::new(["MISSING"], None).unwrap(), &registry);
    assert!(matches!(unknown, Err(EvidenceError::UnknownNet { .. })));
}

#[test]
fn scope_preserves_subject_parameter_and_timed_net_causality() {
    let part = EntityRef::new(EntityKind::Part, "U2").unwrap();
    let subjects = Scope::Subjects(SubjectSet::new([part.clone()]).unwrap());
    let parameter = Scope::Parameter(ParameterRef::new(part.clone(), "vout").unwrap());
    let timed_net =
        Scope::Nets(NetScope::new(["3V3"], Some(TimeWindow::new(0.1, 0.2).unwrap())).unwrap());

    assert_ne!(subjects, parameter);
    assert_ne!(parameter, timed_net);
    assert_eq!(part.kind(), EntityKind::Part);
    assert_eq!(part.id(), "U2");
}

#[test]
fn stable_ids_do_not_collapse_distinct_subjects() {
    let colon = AssumptionId::new(AssumptionKind::OpenPart, "sheet:A");
    let underscore = AssumptionId::new(AssumptionKind::OpenPart, "sheet_A");
    let whitespace = AssumptionId::new(AssumptionKind::OpenPart, "sheet A");

    assert_ne!(colon, underscore);
    assert_ne!(colon, whitespace);
    assert_ne!(underscore, whitespace);
}

#[test]
fn shared_model_and_artifact_vocabularies_are_typed() {
    let source = ModelSource::new(
        ModelSourceTier::CuratedPack,
        ModelLayer::Pack,
        "acme-regulators@1.2.0",
        ModelValidation::DatasheetCurves,
        vec![
            ModelUncertainty::interval("U2.vout", 3.201, 3.399, "V", "datasheet min/max").unwrap(),
        ],
    )
    .unwrap();
    let model = ModelOnPath::new("U2", "xc6206", source, MatchConfidence::Exact).unwrap();
    assert_eq!(model.layer(), ModelLayer::Pack);
    assert_eq!(model.source().tier(), ModelSourceTier::CuratedPack);
    assert_eq!(model.confidence(), MatchConfidence::Exact);

    let artifact_kind = ArtifactKind::OdbPlusPlus;
    assert_eq!(
        serde_json::to_value(artifact_kind).unwrap(),
        "odb_plus_plus"
    );
}

#[test]
fn unknown_model_uncertainty_is_explicit_machine_readable_data() {
    let source = ModelSource::new(
        ModelSourceTier::DatasheetDerived,
        ModelLayer::UserDir,
        "xc6206.toml",
        ModelValidation::PhysicalBoundsOnly,
        vec![ModelUncertainty::unknown(
            "U2.model",
            "the source publishes no validated error interval",
        )
        .unwrap()],
    )
    .unwrap();

    let json = serde_json::to_value(&source).unwrap();
    assert_eq!(json["tier"], "datasheet-derived");
    assert_eq!(json["validation"], "physical-bounds-only");
    assert_eq!(json["uncertainty"][0]["status"], "unknown");
    assert!(json["uncertainty"][0].get("low").is_none());
    assert!(json["uncertainty"][0].get("high").is_none());
}

#[test]
fn deserialized_intervals_are_revalidated_before_publication() {
    let inverted: ModelUncertainty = serde_json::from_value(serde_json::json!({
        "status": "interval",
        "parameter": "U2.vout",
        "low": 3.4,
        "high": 3.2,
        "unit": "V",
        "kind": "specification-limits",
        "basis": "datasheet limits"
    }))
    .unwrap();
    assert!(ModelSource::new(
        ModelSourceTier::DatasheetDerived,
        ModelLayer::UserDir,
        "u2.toml",
        ModelValidation::PhysicalBoundsOnly,
        vec![inverted],
    )
    .is_err());
}

#[test]
fn invalid_budget_members_cannot_be_constructed_or_silently_omitted() {
    assert!(matches!(
        Residual::new(f64::NAN, "3V3"),
        Err(EvidenceError::NonFinite { .. })
    ));
    assert!(matches!(
        Residual::new(-1.0, "3V3"),
        Err(EvidenceError::Negative { .. })
    ));

    let budget = ErrorBudget::new(tolerance())
        .with_method(
            WindowMethod::new(
                TimeWindow::new(0.0, 0.1).unwrap(),
                IntegrationMethod::Trapezoidal,
                0.0,
            )
            .unwrap(),
        )
        .with_residual(Residual::new(4.2e-9, "3V3").unwrap())
        .with_failed_window(TimeWindow::new(0.2, 0.3).unwrap())
        .with_uncertainty(
            ModelUncertainty::new("Q2.beta", 100.0, 200.0, "datasheet limits").unwrap(),
        )
        .with_event_time_error(1e-6)
        .unwrap();

    assert_eq!(budget.methods().len(), 1);
    assert_eq!(budget.failed_windows().len(), 1);
    assert_eq!(budget.residual().unwrap().max_abs(), 4.2e-9);
}

#[test]
fn typical_only_data_is_not_a_guaranteed_two_sided_accuracy_bound() {
    use hauksbee_ir::evidence::ModelIntervalKind;

    let typical = ModelUncertainty::interval_with_kind(
        "U1.ilim",
        0.75,
        1.0,
        "A",
        ModelIntervalKind::TypicalRange,
        "datasheet min/typ row; no maximum is published",
    )
    .unwrap();
    assert!(!typical.is_strict_bound());

    let limits = ModelUncertainty::interval_with_kind(
        "U1.ilim",
        3.1,
        4.2,
        "A",
        ModelIntervalKind::SpecificationLimits,
        "datasheet min/max limits under the published test conditions",
    )
    .unwrap();
    assert!(limits.is_strict_bound());
}

#[test]
fn artifact_and_map_references_are_checked_against_one_registry() {
    let assumption = Assumption::open_part("X", "XC6206", "no model matched");
    let known_id = assumption.id().clone();
    let mut registry = EvidenceRegistry::new(vec![assumption]).unwrap();

    let missing = ArtifactProvenance::new(
        "boards/a.kicad_pcb",
        ArtifactKind::KiCadPcb,
        ArtifactRole::Layout,
        "a".repeat(64),
        vec![AssumptionId::new(
            AssumptionKind::OpenPart,
            "not-in-registry",
        )],
    )
    .unwrap();
    assert!(matches!(
        registry.add_artifact(missing),
        Err(EvidenceError::MissingAssumption { .. })
    ));

    let artifact = ArtifactProvenance::new(
        "boards/a.kicad_pcb",
        ArtifactKind::KiCadPcb,
        ArtifactRole::Layout,
        "a".repeat(64),
        vec![known_id],
    )
    .unwrap();
    let artifact_id = registry.add_artifact(artifact).unwrap();
    let graph = CausalPathIndex::from_net_parts([("3V3", ["X"].as_slice())]).unwrap();
    let traversal = graph
        .traverse(&NetScope::new(["3V3"], None).unwrap(), &registry)
        .unwrap();
    let map = EvidenceMap::from_traversal("rail", traversal, &registry, today())
        .unwrap()
        .with_artifacts(&registry, [artifact_id])
        .unwrap();
    assert_eq!(map.artifacts(), &[artifact_id]);

    let missing_default = ParameterProvenance::new(
        "U2.vout",
        "3.3 V",
        ValueOrigin::Default {
            assumption: AssumptionId::new(AssumptionKind::DefaultParameter, "U2/vout"),
        },
    )
    .unwrap();
    assert!(matches!(
        map.with_parameters(&registry, vec![missing_default]),
        Err(EvidenceError::MissingAssumption { .. })
    ));
}

#[test]
fn waiver_status_and_prose_share_the_same_run_date() {
    let active = Assumption::waived(
        "si",
        "controlled_impedance",
        "DDR_CLK",
        "fab accepted the stackup",
        "2030-01-01",
        RunDate::from_epoch_days(20_666),
    )
    .unwrap();
    let expired = Assumption::waived(
        "si",
        "controlled_impedance",
        "DDR_CLK",
        "fab accepted the stackup",
        "2026-01-01",
        RunDate::from_epoch_days(20_666),
    )
    .unwrap();

    assert!(active.consequence().contains("does not gate this run"));
    assert!(expired.consequence().contains("has lapsed"));
    assert_eq!(
        EvidenceMap::derive_status(&[active], today()),
        EvidenceStatus::Qualified
    );
    assert_eq!(
        EvidenceMap::derive_status(&[expired], today()),
        EvidenceStatus::Undermined
    );

    assert!(matches!(
        Assumption::waived("si", "rule", "N1", "reason", "not-a-date", today()),
        Err(EvidenceError::InvalidDate { .. })
    ));
}
