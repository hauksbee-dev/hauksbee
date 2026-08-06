use hauksbee_engine::result::{BindSummary, JsonFinding, JsonReport};
use hauksbee_engine::{bind_board, BoardEvidence};
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::evidence::{EvidenceStatus, RunDate};
use hauksbee_models::ModelLibrary;
use std::path::PathBuf;
use std::process::Command;

fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.into(),
        net: Some(net),
        function: String::new(),
        kind: String::new(),
        position: None,
    }
}

fn part(reference: &str, value: &str, nets: &[i64]) -> Component {
    Component {
        reference: reference.into(),
        value: value.into(),
        lib_id: String::new(),
        footprint: String::new(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins: nets
            .iter()
            .enumerate()
            .map(|(i, net)| pin(&(i + 1).to_string(), *net))
            .collect(),
    }
}

fn board() -> ExtractedBoard {
    ExtractedBoard {
        name: "evidence-fixture".into(),
        nets: vec![
            Net {
                id: 1,
                name: "3V3".into(),
            },
            Net {
                id: 2,
                name: "VBUS".into(),
            },
            Net {
                id: 3,
                name: "GND".into(),
            },
        ],
        components: vec![part("R74", "", &[1, 3]), part("R1", "10k", &[2, 3])],
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
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap();

    assert_eq!(map(&evidence, "3V3").status(), EvidenceStatus::Undermined);
    assert_eq!(map(&evidence, "VBUS").status(), EvidenceStatus::Clean);
    assert_eq!(map(&evidence, "3V3").assumptions().len(), 1);
    assert!(map(&evidence, "VBUS").assumptions().is_empty());
}

#[test]
fn actual_finding_scope_selects_only_its_causal_path() {
    let board = board();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap();
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
        assert!(rendered.contains("reader-note/1"), "{rendered}");
        assert!(rendered.contains("open-part:R74"), "{rendered}");
    }
    assert!(json.contains("\"evidence\""));
    assert!(json.contains("\"assumptions\""));
}

fn exchange_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml")
}

#[test]
fn ipc2581_reader_coverage_reaches_cli_json_and_plain_end_to_end() {
    let fixture = exchange_fixture();
    for mode in ["--json", "--plain"] {
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
            .args(["run", fixture.to_str().unwrap(), "--report", mode])
            .output()
            .expect("hauksbee CLI runs");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("IPC-2581 input"), "{mode}: {text}");
        assert!(text.contains("reader-note/1"), "{mode}: {text}");
        assert!(
            text.contains("Binding completeness for net"),
            "{mode}: {text}"
        );
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
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
            .arg("run")
            .arg(&fixture)
            .args(args)
            .output()
            .expect("specialist report runs");
        assert!(
            output.status.success(),
            "{args:?} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("IPC-2581 input"), "{args:?}: {text}");
        assert!(text.contains("reader-note/1"), "{args:?}: {text}");
    }
}

#[test]
fn web_report_serializes_the_same_first_class_assumptions_and_maps() {
    let bytes =
        include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml");
    let report = hauksbee_engine::analyze("boot_gate.ipc2581.xml", bytes);
    assert!(report.ok, "{:?}", report.error);
    assert!(!report.assumptions.is_empty());
    assert!(!report.evidence.is_empty());
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(serialized.contains("IPC-2581 input"), "{serialized}");
    assert!(serialized.contains("reader-note/1"), "{serialized}");
}
