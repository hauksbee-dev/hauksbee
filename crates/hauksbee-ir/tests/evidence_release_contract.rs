use hauksbee_ir::evidence::{
    Assumption, CausalPathIndex, ErrorBudget, EvidenceError, EvidenceMap, EvidenceRegistry,
    EvidenceStatus, IntegrationMethod, IntegrationTolerance, ModelUncertainty, NetScope, RunDate,
    TimeWindow, WindowMethod,
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
