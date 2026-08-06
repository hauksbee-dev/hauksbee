use std::{path::Path, process::Command};

#[test]
fn board_bom_and_placement_feed_the_release_cli_and_json_inventory() {
    let dir = tempfile::tempdir().expect("temp directory");
    let board = dir.path().join("board.kicad_pcb");
    let bom = dir.path().join("bom.csv");
    let placement = dir.path().join("positions.csv");

    std::fs::write(
        &board,
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (module Package_DFN_QFN:QFN-10 (layer F.Cu)
    (at 10 20)
    (fp_text reference U9 (at 0 0) (layer F.SilkS))
    (fp_text value "" (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 1 "GND"))
    (pad 4 smd rect (at 3 0) (net 1 "GND"))
    (pad 5 smd rect (at 4 0) (net 1 "GND"))
    (pad 6 smd rect (at 5 0) (net 1 "GND"))
    (pad 7 smd rect (at 6 0) (net 1 "GND"))
    (pad 8 smd rect (at 7 0) (net 1 "GND"))
    (pad 9 smd rect (at 8 0) (net 1 "GND"))
    (pad 10 smd rect (at 9 0) (net 1 "GND"))))"#,
    )
    .expect("board fixture");
    std::fs::write(
        &bom,
        "Assembly Ref,Value,MPN,Manufacturer,Footprint\nU9,,MCP4728,Microchip,QFN-10\n",
    )
    .expect("BOM fixture");
    std::fs::write(
        &placement,
        "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\n\
         U9,MCP4728,QFN-10,10,20,0,top\n",
    )
    .expect("placement fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .args([
            "run",
            board.to_str().unwrap(),
            "--bom",
            bom.to_str().unwrap(),
            "--bom-column",
            "reference=Assembly Ref",
            "--placement",
            placement.to_str().unwrap(),
            "--report",
            "--json",
        ])
        .output()
        .expect("hauksbee runs");

    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    let inputs = json["inputs"]
        .as_array()
        .expect("structured input inventory");
    assert_eq!(inputs.len(), 3, "board + BOM + placement: {json}");
    assert!(inputs.iter().any(|input| input["kind"] == "bom"));
    assert!(inputs.iter().any(|input| input["kind"] == "placement"));
    assert!(inputs.iter().any(|input| {
        input["identity"].as_array().is_some_and(|lines| {
            lines
                .iter()
                .any(|line| line.as_str().is_some_and(|s| s.contains("U9 identified")))
        })
    }));

    std::fs::write(
        &bom,
        "Assembly Ref,Value,MPN\nU9,,MCP4728\nU9,,STM32F103C8\n",
    )
    .expect("conflicting BOM fixture");
    let refused = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .args([
            "run",
            board.to_str().unwrap(),
            "--bom",
            bom.to_str().unwrap(),
            "--bom-column",
            "reference=Assembly Ref",
            "--report",
            "--json",
        ])
        .output()
        .expect("hauksbee refusal runs");
    assert_eq!(
        refused.status.code(),
        Some(3),
        "unsafe identity input is invalid for analysis: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let error: serde_json::Value =
        serde_json::from_slice(&refused.stdout).expect("refusal is one JSON document");
    assert_eq!(error["ok"], false);
    assert!(error["error"].as_str().is_some_and(|message| {
        message.contains("lines") && message.contains("One part cannot take two BOM rows")
    }));
}

#[test]
fn same_board_watchy_position_exports_pass_the_release_cli() {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let board = engine.join("../hauksbee-ci/examples/boards/watchy.kicad_pcb");

    for relative in [
        "../hauksbee-extract/tests/fixtures/placement/watchy.pos",
        "../hauksbee-extract/tests/fixtures/placement/watchy-pos.csv",
    ] {
        let placement = engine.join(relative);
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
            .args([
                "run",
                board.to_str().expect("UTF-8 board fixture path"),
                "--placement",
                placement.to_str().expect("UTF-8 placement fixture path"),
                "--report",
                "--json",
            ])
            .output()
            .expect("hauksbee runs on its ground-truth placement fixture");

        assert!(
            output.status.success(),
            "the KiCad-exported placement belongs to this exact board ({relative})\n\
             status: {:?}\nstderr: {}\nstdout: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
        let inputs = json["inputs"]
            .as_array()
            .expect("the successful report retains its input inventory");
        assert_eq!(inputs.len(), 2, "board + placement: {json}");
        assert!(inputs.iter().any(|input| {
            input["kind"] == "placement"
                && input["sha256"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64)
        }));
        let reconciliation = inputs
            .iter()
            .find(|input| input["kind"] == "placement")
            .and_then(|input| input["identity"].as_array())
            .and_then(|lines| {
                lines.iter().find_map(|line| {
                    line.as_str()
                        .filter(|line| line.contains("placement reconciliation:"))
                })
            })
            .unwrap_or_else(|| panic!("successful reconciliation evidence is missing: {json}"));
        for fact in [
            "75 of 75 placements match",
            "75 positions",
            "75 rotations",
            "75 sides",
            "Y axis mirrored",
            "origin offset (0.0000, 0.0000) mm",
        ] {
            assert!(reconciliation.contains(fact), "{fact}: {reconciliation}");
        }
    }
}

#[test]
fn ambiguous_release_artifacts_refuse_with_exit_three_and_the_actual_columns() {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let board = engine.join("../hauksbee-ci/examples/boards/watchy.kicad_pcb");
    let dir = tempfile::tempdir().expect("temp directory");

    let cases = [
        (
            "placement",
            "ambiguous-placement.csv",
            "Designator,Mid X,X,Mid Y,Rotation,Layer\nU4,86.12,999,-85.91,-90,Top\n",
            "X position",
        ),
        (
            "bom",
            "duplicate-nonnumeric.csv",
            "Designator,Value,MPN\nRX,receiver,SN65HVD230\nRX,receiver,MCP2562\n",
            "RX on lines 2 and 3",
        ),
        (
            "bom",
            "ambiguous-manufacturer.csv",
            "Designator,Value,Manufacturer,Manufacturer Name\nU4,ESP32-S3,Espressif,Microchip\n",
            "two columns that could be the manufacturer",
        ),
    ];

    for (flag, name, body, expected) in cases {
        let artifact = dir.path().join(name);
        std::fs::write(&artifact, body).expect("artifact fixture");
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
            .args([
                "run",
                board.to_str().expect("UTF-8 board fixture path"),
                &format!("--{flag}"),
                artifact.to_str().expect("UTF-8 artifact fixture path"),
                "--report",
                "--json",
            ])
            .output()
            .expect("hauksbee refusal runs");

        assert_eq!(
            output.status.code(),
            Some(3),
            "{name} must be invalid for analysis\nstderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("refusal is one JSON document");
        let message = json["error"].as_str().expect("structured refusal message");
        assert!(message.contains(expected), "{name}: {message}");
    }
}

#[test]
fn every_applicable_board_analysis_json_document_carries_the_input_inventory() {
    let dir = tempfile::tempdir().expect("temp directory");
    let board = dir.path().join("inventory-board.kicad_pcb");
    let bom = dir.path().join("inventory-bom.csv");
    let placement = dir.path().join("inventory-placement.csv");
    std::fs::write(
        &board,
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "VCC")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 10 20)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "VCC"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))))"#,
    )
    .expect("board fixture");
    std::fs::write(
        &bom,
        "Designator,Value,MPN,Manufacturer,Footprint\n\
         R1,10k,RC0402FR-0710KL,Yageo,R_0402_1005Metric\n",
    )
    .expect("BOM fixture");
    std::fs::write(
        &placement,
        "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\n\
         R1,10k,R_0402_1005Metric,10,20,0,top\n",
    )
    .expect("placement fixture");

    // `--list-nets --json` deliberately remains a JSON array: it is a discovery
    // command whose answer is only net names, and BOM/placement identity does
    // not alter connectivity. Every actual board-analysis JSON document below
    // uses the JsonReport/result contract and must retain the same evidence.
    let modes: [(&str, &[&str]); 11] = [
        ("combined", &[]),
        ("report", &["--report"]),
        ("check", &["--check"]),
        ("drc", &["--drc"]),
        ("lint", &["--lint"]),
        ("resources", &["--resources"]),
        ("usb-c", &["--usb-c"]),
        ("si", &["--si"]),
        ("ac", &["--ac", "10:100:2", "--ac-node", "VCC"]),
        ("thermal", &["--thermal", "--seconds", "0.05"]),
        ("headless", &["--headless", "--seconds", "0.05"]),
    ];

    for (label, mode_args) in modes {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hauksbee"));
        command.args([
            "run",
            board.to_str().expect("UTF-8 board path"),
            "--bom",
            bom.to_str().expect("UTF-8 BOM path"),
            "--placement",
            placement.to_str().expect("UTF-8 placement path"),
        ]);
        command.args(mode_args).arg("--json");
        let output = command.output().expect("hauksbee analysis runs");

        assert!(
            output.status.success(),
            "{label} failed with {:?}\nstderr: {}\nstdout: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{label} emitted invalid JSON: {error}"));
        let inputs = json["inputs"]
            .as_array()
            .unwrap_or_else(|| panic!("{label} omitted its input inventory: {json}"));
        assert_eq!(
            inputs
                .iter()
                .map(|input| &input["kind"])
                .collect::<Vec<_>>(),
            vec!["board", "bom", "placement"],
            "{label}: {json}"
        );
    }
}
