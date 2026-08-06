//! The evidence spine as a CONSUMER sees it, compiled from outside the crate.
//!
//! The unit tests inside `evidence.rs` can reach private fields, so they cannot
//! prove the thing that actually matters: that a producer or a renderer in
//! another crate has read access and nothing more. This file is compiled as a
//! separate crate, so what it does is exactly what the engine, ci, the readers
//! and the web layer will be able to do.
//!
//! Everything the later phases need is exercised here. Anything they turn out to
//! need and cannot do belongs in this file first, as a failing test, so the
//! addition is a deliberate widening of the surface rather than a field quietly
//! going public.
//!
//! The lines that must NOT compile, kept as prose because proving a compile
//! failure would cost a whole test harness:
//!
//! ```text
//! a.kind = AssumptionKind::ReducedFidelity;   // private field: E0616
//! a.statement = "Looks fine.".to_string();    // private field: E0616
//! map.assertion = other.assertion;            // private field: E0616
//! EvidenceMap { status: Clean, .. }           // private fields: E0451
//! ```

use hauksbee_ir::evidence::{
    ArtifactKind, ArtifactProvenance, ArtifactRole, Assumption, AssumptionKind, AssumptionSource,
    CausalPathIndex, Contribution, ErrorBudget, EvidenceMap, EvidenceRegistry, EvidenceStatus,
    IntegrationTolerance, MatchConfidence, ModelLayer, ModelOnPath, NetScope, ParameterProvenance,
    RunDate, Scope, Subject, ValueOrigin,
};

/// 2026-08-01. A run builds this from its own clock reading; a fixed value keeps
/// the test off the wall clock.
fn today() -> RunDate {
    RunDate::from_epoch_days(20_666)
}

fn traversed_map(
    assertion: &str,
    net: &str,
    refs: &[&str],
    assumptions: Vec<Assumption>,
) -> (EvidenceRegistry, EvidenceMap) {
    let registry = EvidenceRegistry::new(assumptions).unwrap();
    let graph = CausalPathIndex::from_net_parts([(net, refs)]).unwrap();
    let traversal = graph
        .traverse(&NetScope::new([net], None).unwrap(), &registry)
        .unwrap();
    let map = EvidenceMap::from_traversal(assertion, traversal, &registry, today()).unwrap();
    (registry, map)
}

/// A renderer's whole job, done with public API only: pick the assumptions on an
/// assertion's path and lay their sentences out. If this needs a field it cannot
/// reach, the surface is too narrow; if it can re-word a sentence, the surface is
/// too wide.
fn rests_on_block(map: &EvidenceMap, registry: &[Assumption]) -> String {
    let mut out = format!("{} [{:?}]\n", map.assertion(), map.status());
    for id in map.assumptions() {
        // Degrades to the bare id rather than panicking. A renderer that dies
        // because a registry lookup missed takes the whole report with it, and
        // the id alone still names the gap, which is the point of the id being
        // deterministic.
        match registry.iter().find(|a| a.id() == id) {
            Some(a) => out.push_str(&format!(
                "  - [{}] {} {} Fix: {}\n",
                a.id(),
                a.statement(),
                a.consequence(),
                a.replacement()
            )),
            None => out.push_str(&format!("  - [{id}]\n")),
        }
    }
    out
}

#[test]
fn a_consumer_can_build_every_kind_and_read_every_sentence() {
    let registry = vec![
        Assumption::open_part("U2", "XC6206", "no model matched"),
        Assumption::substitute_model(
            AssumptionSource::Scheduler,
            "U1",
            "ATmega328PB",
            "atmega328p",
        ),
        Assumption::inferred_pin_role("U2", "3", "output"),
        Assumption::default_parameter("U2", "vout", "3.3 V"),
        Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::new("odbpp", "the ODB++ archive"),
            Scope::Board,
        ),
        Assumption::not_checked(
            AssumptionSource::Reader,
            "drc",
            None,
            "this input class carries no copper geometry",
            "supply a layout so the check has copper to read",
        ),
        Assumption::not_exercised(
            AssumptionSource::Scheduler,
            Subject::new("i2c0", "the i2c0 bus"),
            Scope::Nets(NetScope::new(["SDA"], None).unwrap()),
            "the MCU backend models no I2C controller on this platform",
            "run on a platform whose backend models the controller",
        ),
        Assumption::reduced_fidelity(
            AssumptionSource::Scheduler,
            Subject::new("spi0/framing", "SPI framing on spi0"),
            Scope::Nets(NetScope::new(["SCK"], None).unwrap()),
            "the chunk-boundary heuristic",
            "expose the chip-select GPIO",
        ),
        Assumption::held_by_ideal_source("3V3"),
        Assumption::parser_limitation(
            AssumptionSource::Reader,
            Subject::new("drc/short", "shorts on this board"),
            Scope::Check {
                check: "drc".into(),
                kind: Some("short".into()),
            },
            "the file was written by a newer KiCad than this reader models",
            "re-export from the supported KiCad version",
        ),
        Assumption::waived(
            "si",
            "controlled_impedance",
            "DDR_CLK",
            "the fab confirmed the stackup",
            "2027-06-01",
        ),
    ];
    for a in &registry {
        a.validate().expect("a constructor's output is well formed");
        assert!(!a.statement().is_empty());
        assert!(!a.because().is_empty());
        assert!(!a.consequence().is_empty());
        assert!(!a.replacement().is_empty());
        assert_eq!(a.expires().is_some(), a.kind() == AssumptionKind::Waived);
        // The traversal needs the scope, and it is readable without being
        // writable.
        match a.scope() {
            Scope::Board | Scope::Subjects(_) | Scope::Parameter(_) | Scope::Nets(_) => {}
            Scope::Check { check, .. } => assert!(!check.is_empty()),
        }
        assert!(matches!(
            a.source(),
            AssumptionSource::Reader
                | AssumptionSource::Binder
                | AssumptionSource::Scheduler
                | AssumptionSource::Solver
                | AssumptionSource::Check
                | AssumptionSource::User
        ));
    }
}

#[test]
fn a_consumer_can_render_the_rests_on_block_but_cannot_reword_it() {
    let (registry, map) = traversed_map(
        "3V3 stays above 3.1 V",
        "3V3",
        &["U2"],
        vec![Assumption::open_part("U2", "XC6206", "no model matched")],
    );
    let map = map
        .with_models(vec![ModelOnPath::new(
            "U2",
            "xc6206",
            ModelLayer::Pack,
            MatchConfidence::High,
        )
        .unwrap()])
        .with_parameters(vec![ParameterProvenance {
            parameter: "U2.vout".into(),
            value: "3.3 V".into(),
            origin: ValueOrigin::Model {
                model_id: "xc6206".into(),
                layer: ModelLayer::Pack,
                confidence: MatchConfidence::Exact,
            },
        }])
        .with_error_budget(ErrorBudget::new(
            IntegrationTolerance::new(1e-3, 1e-12, 1e-14).unwrap(),
        ))
        .with_coverage("Monte Carlo, 32 members");

    let block = rests_on_block(&map, registry.assumptions());
    assert!(block.contains("Undermined"));
    assert!(block.contains("[open-part:U2]"));
    assert!(block.contains("is treated as an open circuit."));
    assert!(block.contains("Fix: Add a model for U2"));
    assert!(map.is_undermined());
    assert_eq!(map.assertion(), "3V3 stays above 3.1 V");
}

#[test]
fn a_consumer_cannot_obtain_a_clean_map_over_an_undermining_set() {
    // There is one constructor, and it decides. `with_*` cannot reach the
    // status, and there is no setter, no Default, and no public field to swap.
    let (_registry, map) = traversed_map(
        "A",
        "3V3",
        &["X"],
        vec![Assumption::open_part("X", "XC6206", "no model matched")],
    );
    let map = map
        .with_models(Vec::new())
        .with_coverage("whatever a caller likes");
    assert_eq!(map.status(), EvidenceStatus::Undermined);

    // Clone plus builder methods cannot launder it either.
    let cloned = map.clone();
    assert_eq!(cloned.status(), EvidenceStatus::Undermined);

    // And an empty set is the only route to Clean.
    let (_registry, clean) = traversed_map("B", "VBUS", &["J1"], Vec::new());
    assert_eq!(clean.status(), EvidenceStatus::Clean);
}

#[test]
fn a_producer_can_populate_the_inventory_and_the_json_round_trips() {
    let assumption = Assumption::fitted_by_default(
        AssumptionSource::Reader,
        Subject::new("odbpp", "the ODB++ archive"),
        Scope::Board,
    );
    let artifact = ArtifactProvenance::new(
        "boards/rev-c.tgz",
        ArtifactKind::OdbPlusPlus,
        ArtifactRole::FabArchive,
        "a".repeat(64),
        vec![assumption.id().clone()],
    )
    .unwrap()
    .with_contributions(vec![Contribution {
        what: "connectivity".into(),
        detail: "nets read from the archive's netlist section".into(),
    }]);
    let json = serde_json::to_string(&artifact).expect("serializes");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("is valid JSON");
    assert_eq!(doc["kind"], "odb_plus_plus");

    // The judgements serialize but do NOT deserialize: an assumption or an
    // evidence status is produced, and parsing one back would mint it outside
    // the constructors that compose its sentences and derive its status. A
    // consumer reads the fields it needs from the JSON, or holds the registry and
    // re-derives.
    let json = serde_json::to_string(&assumption).expect("serializes");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("is a JSON document");
    assert_eq!(doc["kind"], "fitted_by_default");
    assert_eq!(doc["scope"]["type"], "board");

    let (_registry, map) = traversed_map("A", "3V3", &["R7"], vec![assumption.clone()]);
    let json = serde_json::to_string(&map).expect("serializes");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("is a JSON document");
    assert_eq!(doc["status"], "undermined");
    assert_eq!(doc["assumptions"][0], "fitted-by-default:odbpp");
}
