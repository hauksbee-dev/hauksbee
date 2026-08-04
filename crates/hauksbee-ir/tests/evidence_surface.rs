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
    ArtifactProvenance, ArtifactRole, Assumption, AssumptionKind, AssumptionSource, Contribution,
    ErrorBudget, EvidenceMap, EvidenceStatus, IntegrationTolerance, ModelOnPath,
    ParameterProvenance, Scope, Subject, ValueOrigin,
};

/// 2026-01-01, as days since the Unix epoch. A run passes its own floored clock
/// reading; a fixed value here keeps the test off the wall clock.
const TODAY: i64 = 20_454;

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
            Scope::Nets {
                nets: vec!["SDA".into()],
            },
            "the MCU backend models no I2C controller on this platform",
            "run on a platform whose backend models the controller",
        ),
        Assumption::reduced_fidelity(
            AssumptionSource::Scheduler,
            Subject::new("spi0/framing", "SPI framing on spi0"),
            Scope::Nets {
                nets: vec!["SCK".into()],
            },
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
            "2026-06-01",
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
            Scope::Board | Scope::Parts { .. } | Scope::Nets { .. } => {}
            Scope::Check { check, .. } => assert!(!check.is_empty()),
            Scope::TimeWindow { .. } => {}
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
    let registry = vec![Assumption::open_part("U2", "XC6206", "no model matched")];
    let map = EvidenceMap::new("3V3 stays above 3.1 V", &registry, TODAY)
        .with_artifacts(vec![0])
        .with_models(vec![ModelOnPath {
            reference: "U2".into(),
            model_id: "xc6206".into(),
            layer: "pack".into(),
            confidence: "high".into(),
        }])
        .with_parameters(vec![ParameterProvenance {
            parameter: "U2.vout".into(),
            value: "3.3 V".into(),
            origin: ValueOrigin::Model {
                model_id: "xc6206".into(),
                layer: "pack".into(),
                confidence: "exact".into(),
            },
        }])
        .with_error_budget(ErrorBudget::new(IntegrationTolerance {
            reltol: 1e-3,
            abstol: 1e-12,
            chgtol: 1e-14,
        }))
        .with_coverage("Monte Carlo, 32 members");

    let block = rests_on_block(&map, &registry);
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
    let undermining = [Assumption::open_part("X", "XC6206", "no model matched")];
    let map = EvidenceMap::new("A", &undermining, TODAY)
        .with_artifacts(vec![0, 1, 2])
        .with_models(Vec::new())
        .with_coverage("whatever a caller likes");
    assert_eq!(map.status(), EvidenceStatus::Undermined);

    // Clone plus builder methods cannot launder it either.
    let cloned = map.clone().with_artifacts(Vec::new());
    assert_eq!(cloned.status(), EvidenceStatus::Undermined);

    // And an empty set is the only route to Clean.
    assert_eq!(
        EvidenceMap::new("B", &[], TODAY).status(),
        EvidenceStatus::Clean
    );
}

#[test]
fn a_producer_can_populate_the_inventory_and_the_json_round_trips() {
    let assumption = Assumption::fitted_by_default(
        AssumptionSource::Reader,
        Subject::new("odbpp", "the ODB++ archive"),
        Scope::Board,
    );
    let artifact = ArtifactProvenance {
        path: "boards/rev-c.tgz".into(),
        kind: "odbpp".into(),
        role: ArtifactRole::FabArchive,
        sha256: "a".repeat(64),
        contributed: vec![Contribution {
            what: "connectivity".into(),
            detail: "nets read from the archive's netlist section".into(),
        }],
        ignored: Vec::new(),
        cross_checks: Vec::new(),
        assumptions: vec![assumption.id().clone()],
    };
    let json = serde_json::to_string(&artifact).expect("serializes");
    let back: ArtifactProvenance = serde_json::from_str(&json).expect("round-trips");
    assert_eq!(back, artifact);

    // The judgements serialize but do NOT deserialize: an assumption or an
    // evidence status is produced, and parsing one back would mint it outside
    // the constructors that compose its sentences and derive its status. A
    // consumer reads the fields it needs from the JSON, or holds the registry and
    // re-derives.
    let json = serde_json::to_string(&assumption).expect("serializes");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("is a JSON document");
    assert_eq!(doc["kind"], "fitted_by_default");
    assert_eq!(doc["scope"]["type"], "board");

    let map = EvidenceMap::new("A", std::slice::from_ref(&assumption), TODAY);
    let json = serde_json::to_string(&map).expect("serializes");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("is a JSON document");
    assert_eq!(doc["status"], "undermined");
    assert_eq!(doc["assumptions"][0], "fitted-by-default:odbpp");
}
