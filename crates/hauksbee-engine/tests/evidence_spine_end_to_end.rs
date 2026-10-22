use hauksbee_engine::result::{BindSummary, DrcShort, DrcStructured, JsonFinding, JsonReport};
use hauksbee_engine::{bind_board, BoardEvidence};
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_ir::evidence::{AssumptionKind, EvidenceStatus, RunDate};
use hauksbee_models::ModelLibrary;
use hauksbee_solve::{Integration, SolverOptions};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::support::run_board;

use crate::support::netted_pin as pin;

fn part(reference: &str, value: &str, nets: &[i64]) -> Component {
    crate::support::component(
        reference.into(),
        value.into(),
        String::new(),
        String::new(),
        nets.iter()
            .enumerate()
            .map(|(i, net)| pin(&(i + 1).to_string(), *net))
            .collect(),
    )
}

fn board() -> ExtractedBoard {
    ExtractedBoard {
        name: "evidence-fixture".into(),
        nets: vec![
            crate::support::net(1, "3V3".into()),
            crate::support::net(2, "VBUS".into()),
            crate::support::net(3, "GND".into()),
        ],
        components: vec![part("R74", "", &[1, 3]), part("R1", "10k", &[2, 3])],
    }
}

fn bound_evidence() -> BoardEvidence {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666)).unwrap()
}

#[test]
fn output_capacitor_requirements_reach_evidence() {
    let net = |code, name: &str| crate::support::net(code, name.into());
    for (value, cap, expected) in [
        (
            "AMS1117-5.0",
            "10u",
            Some(("reduced_fidelity", "does not state")),
        ),
        // AMS1117's 22 µF is a recommendation ("will ensure stability",
        // datasheet p.4), not a stated minimum: a shortfall is disclosed as
        // unconfirmed stability, never as a violated limit.
        (
            "AMS1117-3.3",
            "100n",
            Some(("reduced_fidelity", "not a stated minimum")),
        ),
        ("AMS1117-3.3", "22u", None),
        // MCP1700's 1 µF is a stated minimum ("A minimum output capacitance
        // of 1.0 µF is required for small signal stability", DS20001826F p.13).
        (
            "MCP1700T-3302E/TT",
            "100n",
            Some(("operating_limit_violated", "C1")),
        ),
        ("MCP1700T-3302E/TT", "1u", None),
        (
            "NCP1117-3.3_SOT223",
            "10u",
            Some(("reduced_fidelity", "ESR")),
        ),
    ] {
        let board = ExtractedBoard {
            name: value.into(),
            nets: vec![net(1, "VIN"), net(2, "GND"), net(3, "VOUT")],
            components: vec![part("U1", value, &[2, 3, 1]), part("C1", cap, &[3, 2])],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        // The markers route the warning into evidence; the printed line never shows them.
        let marked =
            |text: &str| text.contains("[partial model]") || text.contains("[operating limit");
        assert!(bound.report.warnings().all(|(_, text)| !marked(text)));
        let evidence =
            BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::unknown()).unwrap();
        let about_u1: Vec<_> = evidence
            .assumptions()
            .iter()
            .filter(|a| a.statement().contains("U1"))
            .map(|a| (a.kind().as_str(), a.because()))
            .collect();
        match expected {
            None => assert!(about_u1.is_empty(), "{value}: {about_u1:?}"),
            Some((kind, needle)) => assert!(
                about_u1
                    .iter()
                    .any(|(k, why)| *k == kind && why.contains(needle)),
                "{value}: {about_u1:?}"
            ),
        }
    }
}

fn map<'a>(evidence: &'a BoardEvidence, net: &str) -> &'a hauksbee_ir::evidence::EvidenceMap {
    evidence
        .maps()
        .iter()
        .find(|map| map.assertion().ends_with(net))
        .unwrap_or_else(|| panic!("no evidence map for {net}"))
}

#[test]
fn actual_bind_incidence_rejects_vacuous_and_saturated_evidence() {
    let evidence = bound_evidence();

    assert_eq!(map(&evidence, "3V3").status(), EvidenceStatus::Undermined);
    assert_eq!(map(&evidence, "VBUS").status(), EvidenceStatus::Clean);
    assert_eq!(map(&evidence, "3V3").assumptions().len(), 1);
    assert!(map(&evidence, "VBUS").assumptions().is_empty());
}

#[test]
fn actual_finding_scope_selects_only_its_causal_path() {
    let evidence = bound_evidence();
    let finding = JsonFinding {
        check: "lint".into(),
        kind: "fixture".into(),
        severity: "warning".into(),
        nets: vec!["3V3".into()],
        location_mm: None,
        layer: None,
        refs: Vec::new(),
        actionable: true,
        message: "3V3 assertion".into(),
        plain: "3V3 assertion".into(),
        fix: None,
    };

    let maps = evidence.maps_for_findings(&[finding]).unwrap();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].assertion(), "3V3 assertion");
    assert_eq!(maps[0].status(), EvidenceStatus::Undermined);
    assert_eq!(maps[0].assumptions().len(), 1);
}

#[test]
fn drc_short_evidence_is_geometry_causal_and_cites_the_layout_artifact() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap()
            .with_input_artifact(
                "fixture.kicad_pcb",
                b"(kicad_pcb (net 1 \"3V3\"))",
                hauksbee_engine::board_input::InputKind::Text,
            )
            .unwrap();
    let drc = DrcStructured {
        clearance_rule_mm: 0.2,
        primitive_count: 2,
        stale_fills: Vec::new(),
        shorts: vec![DrcShort {
            net_a: "3V3".into(),
            net_b: "GND".into(),
            layer: "F.Cu".into(),
            loc_mm: [1.0, 2.0],
            gap_mm: 0.0,
            severity: "serious".into(),
            plain: "3V3 shorts GND".into(),
            fix: "separate copper".into(),
            refs: Vec::new(),
        }],
        violations: vec![],
        at_limit: vec![],
        version_warning: None,
        suppression_note: None,
    };

    let maps = evidence.maps_for_drc(&drc).unwrap();
    assert_eq!(maps.len(), 1);
    assert!(maps[0].assertion().contains("3V3 shorts GND"));
    assert_eq!(maps[0].status(), EvidenceStatus::Clean);
    assert!(
        maps[0].assumptions().is_empty(),
        "R74 being open is not causal to copper geometry"
    );
    assert_eq!(maps[0].artifacts().len(), 1);
    assert_eq!(evidence.inventory().len(), 1);
    assert_eq!(evidence.inventory()[0].sha256().len(), 64);
}

#[test]
fn schematic_provenance_is_causal_only_to_the_declared_contact() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap()
            .with_input_artifact(
                "fixture.brd",
                b"<eagle><board/></eagle>",
                hauksbee_engine::board_input::InputKind::Text,
            )
            .unwrap()
            .with_schematic_artifact(
                "fixture.sch",
                b"<eagle><schematic/></eagle>",
                "one declared tie supplied context for one measured contact",
            )
            .unwrap();
    let tie_board = include_str!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd");
    let tie_schematic =
        include_str!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch");
    let tie_report = ExtractedBoard::drc(tie_board).unwrap();
    let tie_declarations = hauksbee_extract::declared_net_ties(tie_schematic).unwrap();
    let qualification = tie_report.qualify_with_declared_ties("fixture.sch", &tie_declarations);
    let short = |plain: &str, net_a: &str, net_b: &str, loc_mm: [f64; 2]| DrcShort {
        net_a: net_a.into(),
        net_b: net_b.into(),
        layer: "F.Cu".into(),
        gap_mm: 0.0,
        loc_mm,
        severity: if net_a == "GND" { "note" } else { "serious" }.into(),
        plain: plain.into(),
        fix: String::new(),
        refs: Vec::new(),
    };
    let drc = DrcStructured {
        clearance_rule_mm: 0.2,
        primitive_count: 4,
        stale_fills: Vec::new(),
        shorts: vec![
            short("GND and AGND are joined", "GND", "AGND", [5.0, 0.0]),
            short("VBUS shorts GND", "VBUS", "GND", [1.0, 2.0]),
        ],
        violations: Vec::new(),
        at_limit: Vec::new(),
        version_warning: None,
        suppression_note: None,
    };

    let maps = evidence
        .maps_for_drc_with_ties(&drc, Some(&qualification))
        .unwrap();
    assert_eq!(
        maps[0].artifacts().len(),
        2,
        "declared contact cites board + schematic"
    );
    assert_eq!(
        maps[1].artifacts().len(),
        1,
        "ordinary short cites only the board"
    );
    let simulation = evidence
        .simulation_map("VBUS numeric result", &["VBUS".into()], &[], None)
        .unwrap();
    assert_eq!(
        simulation.artifacts().len(),
        1,
        "simulation did not consume the schematic"
    );
    let ci = evidence
        .ci_assertion_map("VBUS asserted", &["VBUS".into()], &[], None, None)
        .unwrap();
    assert_eq!(
        ci.artifacts().len(),
        1,
        "unrelated CI assertion did not consume the schematic"
    );
}

#[test]
fn numeric_simulation_assertions_carry_budget_and_semantic_substitution_only() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap()
            .with_input_artifact(
                "fixture.kicad_pcb",
                b"(kicad_pcb)",
                hauksbee_engine::board_input::InputKind::Text,
            )
            .unwrap()
            .with_firmware_artifact("firmware.elf", b"\x7fELFfixture")
            .unwrap()
            .with_substitutions(&[hauksbee_engine::scheduler::McuSubstitution {
                reference: "R1".into(),
                backend: "renode:fixture".into(),
                requested_part: "real-part".into(),
                modelled_core: "stand-in-core".into(),
            }])
            .unwrap();
    let options = SolverOptions {
        integration: Integration::Gear2,
        reltol: 2e-4,
        vntol: 7e-7,
        abstol: 3e-13,
        chgtol: 9e-15,
        ..SolverOptions::default()
    };
    let budget = BoardEvidence::transient_error_budget(
        &options,
        0.0,
        0.01,
        0.001,
        &[(0.004, 0.005)],
        &[(0.006, 0.007, "backward-euler")],
    )
    .unwrap();
    let map = evidence
        .simulation_map(
            "VBUS peak stays below 5.5 V",
            &["VBUS".to_string()],
            &[],
            Some(budget),
        )
        .unwrap();

    assert_eq!(map.status(), EvidenceStatus::Undermined);
    assert!(map.error_budget().is_some());
    let budget = map.error_budget().unwrap();
    assert_eq!(budget.failed_windows().len(), 1);
    let tol = budget.tolerance();
    assert_eq!(tol.reltol().to_bits(), options.reltol.to_bits());
    assert_eq!(tol.vntol().to_bits(), options.vntol.to_bits());
    assert_eq!(
        budget.methods()[0].method(),
        hauksbee_ir::evidence::IntegrationMethod::Gear2
    );
    let json = serde_json::to_value(budget).unwrap();
    assert!(!json.to_string().contains("accuracy_cost"));
    assert_eq!(
        map.artifacts().len(),
        2,
        "board and firmware are causal inputs"
    );
    assert!(evidence
        .assumptions()
        .iter()
        .any(|a| a.kind() == hauksbee_ir::evidence::AssumptionKind::SubstituteModel));
}

#[test]
fn downstream_mcu_evidence_struct_literals_keep_the_legacy_field_contract() {
    let _binding = hauksbee_engine::binder::McuBinding {
        reference: "U1".into(),
        backend: "renode:stm32f4".into(),
        requested_part: "STM32F411RET6".into(),
        clock: hauksbee_bind::mcu_clock::McuClock::default(),
        internal_clock: None,
        pad_roles: HashMap::new(),
        role_nets: HashMap::new(),
        gpio_drivers: HashMap::new(),
        adc_nets: HashMap::new(),
        adc_pin: HashMap::new(),
        module: false,
        max_supply_v: None,
        supply_stand_in: None,
        min_supply_v: None,
        supply_floors: HashMap::new(),
        electrical: hauksbee_bind::mcu_electrical::McuElectrical::default(),
    };
    let _substitution = hauksbee_engine::scheduler::McuSubstitution {
        reference: "U1".into(),
        backend: "renode:stm32f4".into(),
        requested_part: "STM32F411RET6".into(),
        modelled_core: "STM32F407".into(),
    };
}

#[test]
fn transient_budget_partitions_solved_and_invalid_windows_without_overlap() {
    let options = SolverOptions::default();
    let budget = BoardEvidence::transient_error_budget(
        &options,
        0.0,
        1.0,
        0.01,
        &[(0.2, 0.3)],
        &[(0.5, 0.6, "backward-euler")],
    )
    .unwrap();
    let windows: Vec<_> = budget
        .methods()
        .iter()
        .map(|m| [m.window().start_s(), m.window().end_s()].map(f64::to_bits))
        .collect();
    let expected = [[0.0, 0.2], [0.3, 0.5], [0.6, 1.0], [0.5, 0.6]].map(|w| w.map(f64::to_bits));
    assert_eq!(windows, expected, "three primary spans plus one fallback");
    assert!(BoardEvidence::transient_error_budget(
        &options,
        0.0,
        1.0,
        0.01,
        &[(0.2, 0.4)],
        &[(0.3, 0.5, "backward-euler")],
    )
    .is_err());
    assert!(BoardEvidence::transient_error_budget(
        &options,
        0.0,
        1.0,
        0.01,
        &[],
        &[(0.3, 0.5, "mystery-method")],
    )
    .is_err());
}

#[test]
fn one_evidence_object_renders_reader_notes_in_json_and_plain() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let note = "ODB++ input: connectivity was read from EDA data; clearance DRC was not run.";
    let evidence = BoardEvidence::from_bound(
        &board,
        &bound.report,
        &[note.to_string()],
        RunDate::from_epoch_days(20_666),
    )
    .unwrap();
    let json = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
        .with_evidence(&evidence)
        .to_json();
    let plain = evidence.render_plain();

    for rendered in [&json, &plain] {
        assert!(rendered.contains("ODB++ input"), "{rendered}");
        assert!(rendered.contains("reader-note/"), "{rendered}");
        assert!(rendered.contains("open-part:R74"), "{rendered}");
    }
    assert!(json.contains("\"evidence\""));
    assert!(json.contains("\"assumptions\""));
}

#[test]
fn reader_accounting_is_decomposed_into_typed_facts_not_one_generic_caveat() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let notes = vec![
        "IPC-2581 input: the netlist was read from the document's LogicalNet section, not reverse-engineered from copper. Clearance DRC and trace-geometry SI need the original layout file and were not run.".to_string(),
        "This document carries no BOM, so no component has a value and no populate/do-not-populate flag could be read: every placed part has been treated as fitted.".to_string(),
        "CAD netlist and package connectivity disagree on U3 pin 2.".to_string(),
    ];
    let evidence = BoardEvidence::from_bound(
        &board,
        &bound.report,
        &notes,
        RunDate::from_epoch_days(20_666),
    )
    .unwrap()
    .with_input_artifact(
        "fixture.ipc2581.xml",
        b"<IPC-2581 revision=\"C\"/>",
        hauksbee_engine::board_input::InputKind::Ipc2581,
    )
    .unwrap();

    let kinds: Vec<_> = evidence
        .assumptions()
        .iter()
        .map(hauksbee_ir::evidence::Assumption::kind)
        .collect();
    assert!(kinds.contains(&AssumptionKind::NotChecked), "{kinds:?}");
    assert!(
        kinds.contains(&AssumptionKind::FittedByDefault),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&AssumptionKind::ParserLimitation),
        "{kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|&&kind| kind == AssumptionKind::ReducedFidelity)
            .count(),
        0,
        "positive connectivity provenance is not itself a caveat"
    );

    let artifact = serde_json::to_value(&evidence.inventory()[0]).unwrap();
    assert!(
        artifact["contributed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["what"] == "connectivity"
                && c["detail"].as_str().unwrap().contains("LogicalNet")),
        "the reader's own accounting belongs on the artifact: {artifact:?}"
    );
    assert!(
        artifact["cross_checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["agreed"] == false),
        "reader disagreements are inventory cross-checks as well as assumptions"
    );
}

#[test]
fn a_bind_default_warning_becomes_parameter_provenance_and_a_typed_assumption() {
    use hauksbee_engine::reports::bind::{BindOutcome, BindReport, BindRow};
    use hauksbee_models::Confidence;

    let mut report = BindReport::default();
    report.push(BindRow {
        reference: "U9".into(),
        value: "GENERIC-LDO".into(),
        model_id: Some("generic_ldo".into()),
        confidence: Confidence::Exact,
        source: None,
        outcome: BindOutcome::Behavioral {
            device: "vreg 5.0V source".into(),
        },
        warning: Some(
            "U9 (GENERIC-LDO): vreg model has no `vout` param; regulating its output net to an assumed 5.0 V; verify the regulator's actual output voltage".into(),
        ),
        guesses: Vec::new(),
    });
    let mut input = board();
    input.components.push(part("U9", "GENERIC-LDO", &[1, 3]));
    let evidence =
        BoardEvidence::from_bound(&input, &report, &[], RunDate::from_epoch_days(20_666)).unwrap();

    let assumption = evidence
        .assumptions()
        .iter()
        .find(|a| a.kind() == AssumptionKind::DefaultParameter)
        .expect("the binder's documented default reaches the registry");
    assert_eq!(assumption.id().as_str(), "default-parameter:U9.vout");
    let map = evidence
        .maps()
        .iter()
        .find(|map| map.assertion().ends_with("3V3"))
        .expect("map for U9's fixture net");
    assert!(
        map.parameters().iter().any(|p| {
            p.parameter() == "U9.vout"
                && matches!(
                    p.origin(),
                    hauksbee_ir::evidence::ValueOrigin::Default { assumption: id }
                        if id == assumption.id()
                )
        }),
        "the number and the assumption are cross-referenced: {map:?}"
    );
}

#[test]
fn reader_assumption_identity_survives_note_reordering_and_deduplicates_repeats() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let first = "IPC-2581 input: connectivity came from the logical netlist.".to_string();
    let second = "IPC-2581 input: the exporter renamed one reference designator.".to_string();

    let original = BoardEvidence::from_bound(
        &board,
        &bound.report,
        &[first.clone(), second.clone()],
        RunDate::from_epoch_days(20_666),
    )
    .unwrap();
    let reordered = BoardEvidence::from_bound(
        &board,
        &bound.report,
        &[second.clone(), first.clone(), first.clone()],
        RunDate::from_epoch_days(20_666),
    )
    .unwrap();

    let id_for = |evidence: &BoardEvidence, note: &str| {
        evidence
            .assumptions()
            .iter()
            .find(|assumption| assumption.because().contains(note.trim_end_matches('.')))
            .expect("reader note is represented")
            .id()
            .to_string()
    };
    assert_eq!(id_for(&original, &first), id_for(&reordered, &first));
    assert_eq!(id_for(&original, &second), id_for(&reordered, &second));
    assert!(
        id_for(&original, &first).len() < 128,
        "a stable id is repeated by every affected map and must not embed the full note"
    );
    assert_eq!(
        reordered
            .assumptions()
            .iter()
            .filter(
                |assumption| assumption.source() == hauksbee_ir::evidence::AssumptionSource::Reader
            )
            .count(),
        2,
        "a repeated reader limitation is one assumption, not a construction failure or duplicate"
    );
}

fn exchange_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml")
}

#[test]
fn ipc2581_reader_coverage_stays_full_in_json_and_scoped_in_plain() {
    let fixture = exchange_fixture();
    for mode in ["--json", "--plain"] {
        let output = run_board(&fixture, &["--report", mode]);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        if mode == "--json" {
            assert!(text.contains("not-checked:drc"), "{mode}: {text}");
            assert!(text.contains("The drc check did not run"), "{mode}: {text}");
            assert!(
                text.contains("Binding completeness for net"),
                "{mode}: {text}"
            );
        } else {
            assert!(
                !text.contains("The drc check did not run"),
                "bind-only human output must omit unrelated DRC provenance: {text}"
            );
            assert!(text.contains("Evidence coverage:"), "{mode}: {text}");
            assert!(text.contains("surface's --json output"), "{mode}: {text}");
        }
    }
}

#[test]
fn specialist_report_surfaces_reuse_the_reader_assumption() {
    let fixture = exchange_fixture();
    for args in [
        &["--drc", "--json"][..],
        &["--drc", "--plain"],
        &["--lint", "--json"],
        &["--lint", "--plain"],
        &["--resources", "--json"],
        &["--resources", "--plain"],
        &["--si", "--json"],
        &["--si", "--plain"],
        &["--usb-c", "--json"],
        &["--usb-c", "--plain"],
        &["--ampacity"],
    ] {
        let output = run_board(&fixture, args);
        assert!(
            output.status.success(),
            "{args:?} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        if args.contains(&"--json") {
            assert!(text.contains("not-checked:drc"), "{args:?}: {text}");
            assert!(
                text.contains("The drc check did not run"),
                "{args:?}: {text}"
            );
        } else if args.contains(&"--drc") {
            assert!(
                text.contains("[not-checked] The drc check did not run"),
                "{args:?}: {text}"
            );
        } else if args.contains(&"--si") {
            assert!(
                text.contains("[not-checked] The si check did not run"),
                "{args:?}: {text}"
            );
            assert!(
                !text.contains("The drc check did not run"),
                "{args:?}: {text}"
            );
        } else {
            assert!(
                !text.contains("The drc check did not run"),
                "unrelated DRC provenance leaked onto {args:?}: {text}"
            );
            assert!(text.contains("Evidence coverage:"), "{args:?}: {text}");
        }
    }
}

#[test]
fn web_report_serializes_evidence_and_does_not_call_qualified_evidence_healthy() {
    let bytes =
        include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml");
    let report = hauksbee_engine::analyze("boot_gate.ipc2581.xml", bytes);
    assert!(report.ok, "{:?}", report.error);
    assert!(!report.assumptions.is_empty());
    assert!(!report.evidence.is_empty());
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(serialized.contains("IPC-2581 input"), "{serialized}");
    assert!(serialized.contains("not-checked:drc"), "{serialized}");
    assert!(
        serialized.contains("The drc check did not run"),
        "{serialized}"
    );
    assert!(
        !report.headline.contains("Looks healthy"),
        "qualified evidence must not sit below an unqualified web verdict: {}",
        report.headline
    );
}

// ---------------------------------------------------------------------------
// SI findings are geometry-and-stated-value claims. The SI analysis runs in
// hauksbee-extract, which has no access to bound models, and each of its
// checks abstains when an input it needs is absent, so a part that is merely
// OPEN (no simulation model) is never a causal input of an SI claim: an
// unmodelled crystal cannot undermine the statement of what load capacitance
// the caps around it present. This is the same discipline DRC shorts already
// have. Two-sided: the identical finding under a model-consuming check on the
// same net IS undermined by the same open part.
// ---------------------------------------------------------------------------

fn finding_on_3v3(check: &str) -> JsonFinding {
    finding_on_3v3_kind(check, "fixture")
}

fn finding_on_3v3_kind(check: &str, kind: &str) -> JsonFinding {
    JsonFinding {
        check: check.into(),
        kind: kind.into(),
        severity: "info".into(),
        nets: vec!["3V3".into()],
        location_mm: None,
        layer: None,
        refs: Vec::new(),
        actionable: false,
        message: format!("{check} assertion on 3V3"),
        plain: format!("{check} assertion on 3V3"),
        fix: None,
    }
}

#[test]
fn si_findings_are_geometry_causal_and_open_parts_do_not_undermine_them() {
    let evidence = bound_evidence();
    // The premise the contrast rests on: R74 on 3V3 is open (no model), and a
    // model-consuming claim on that net is undermined by it.
    let lint_maps = evidence
        .maps_for_findings(&[finding_on_3v3("lint")])
        .unwrap();
    assert_eq!(lint_maps[0].status(), EvidenceStatus::Undermined);

    // The same net, the same open part, an extract-computed SI claim: not
    // undermined, because nothing that check computes consumes a bound model.
    let si_maps = evidence
        .maps_for_findings(&[finding_on_3v3_kind("si", "controlled_impedance")])
        .unwrap();
    assert_eq!(si_maps.len(), 1);
    assert_eq!(
        si_maps[0].status(),
        EvidenceStatus::Clean,
        "an open part is not on a geometry-class SI claim's causal path: {:?}",
        si_maps[0].assumptions()
    );

    // The allowlist edge, both directions: the engine-appended SI kinds
    // (trace ampacity, input-cap ripple) consume the model library and skip
    // open-part sources in their current attribution, so the same open part
    // DOES undermine them, and an unknown future kind fails closed the same
    // way.
    for kind in ["trace_ampacity", "input_cap_ripple", "some_future_kind"] {
        let maps = evidence
            .maps_for_findings(&[finding_on_3v3_kind("si", kind)])
            .unwrap();
        assert_eq!(
            maps[0].status(),
            EvidenceStatus::Undermined,
            "model-consuming SI kind {kind} keeps the full traversal"
        );
    }
}

#[test]
fn si_check_scoped_assumptions_still_attach_to_si_findings() {
    // The geometry-class traversal must not detach the SI check's OWN scoped
    // assumptions: a "this rule could not run" NotChecked on check `si` still
    // lands on the SI claim it names, and still undermines it.
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap()
            .with_assumptions([hauksbee_ir::evidence::Assumption::not_checked(
                hauksbee_ir::evidence::AssumptionSource::Reader,
                "si",
                None,
                "the stackup declaration could not be read",
                "a board file whose (setup (stackup ...)) parses",
            )])
            .unwrap();
    let si_maps = evidence
        .maps_for_findings(&[finding_on_3v3_kind("si", "controlled_impedance")])
        .unwrap();
    assert_eq!(
        si_maps[0].status(),
        EvidenceStatus::Undermined,
        "check-scoped assumptions still attach on the geometry branch: {:?}",
        si_maps[0].assumptions()
    );
}
