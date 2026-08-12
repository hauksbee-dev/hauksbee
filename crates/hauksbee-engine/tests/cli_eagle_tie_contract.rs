use std::process::Command;

use sha2::{Digest, Sha256};

fn run_pair(
    name: &str,
    board_bytes: &[u8],
    schematic_bytes: &[u8],
) -> (std::process::Output, String) {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join(format!("{name}.brd"));
    let schematic = dir.path().join(format!("{name}.sch"));
    let junit = dir.path().join("drc.xml");
    std::fs::write(&board, board_bytes).unwrap();
    std::fs::write(&schematic, schematic_bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .arg("run")
        .arg(&board)
        .args(["--drc", "--json", "--strict", "--schematic"])
        .arg(&schematic)
        .arg("--junit")
        .arg(&junit)
        .output()
        .unwrap();
    let junit = std::fs::read_to_string(junit).expect("JUnit finalized on every terminal outcome");
    (output, junit)
}

#[test]
fn schematic_context_never_downgrades_the_json_exit_or_junit_short() {
    let (declared, declared_junit) = run_pair(
        "declared",
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd"),
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch"),
    );
    assert_eq!(declared.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&declared.stdout).unwrap();
    assert_eq!(json["verdict"], "fail");
    assert!(json["drc"]["shorts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|short| {
            short["severity"] == "serious"
                && short["plain"]
                    .as_str()
                    .is_some_and(|plain| plain.contains("does not identify or authorize"))
        }));
    let schematic = include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch");
    let expected_sha = Sha256::digest(schematic)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let input = json["inputs"]
        .as_array()
        .expect("top-level input inventory")
        .iter()
        .find(|input| input["kind"] == "schematic")
        .expect("the exact contributing schematic is a top-level input");
    assert_eq!(input["format"], "eagle_schematic");
    assert_eq!(input["sha256"], expected_sha);
    assert!(input["path"].as_str().unwrap().ends_with("declared.sch"));
    assert!(declared_junit.contains("<failure"), "{declared_junit}");

    let (undeclared, undeclared_junit) = run_pair(
        "undeclared",
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/undeclared.brd"),
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/undeclared.sch"),
    );
    assert_eq!(undeclared.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&undeclared.stdout).unwrap();
    assert_eq!(json["verdict"], "fail");
    assert_eq!(json["drc"]["shorts"][0]["severity"], "serious");
    assert!(
        !undeclared_junit.contains("failures=\"0\""),
        "{undeclared_junit}"
    );
    assert!(undeclared_junit.contains("<failure"), "{undeclared_junit}");
}

#[test]
fn companion_identity_is_checked_before_placement_enriches_board_values() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("design.brd");
    let schematic = dir.path().join("design.sch");
    let placement = dir.path().join("placement.csv");
    let original_board =
        include_str!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd");
    let original_schematic =
        include_str!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch");
    std::fs::write(
        &board,
        original_board.replace("value=\"10k\"", "value=\"\""),
    )
    .unwrap();
    std::fs::write(
        &schematic,
        original_schematic.replace("value=\"10k\"", "value=\"\""),
    )
    .unwrap();
    std::fs::write(
        &placement,
        "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\nR1,10k,R0603,20,20,0,Top\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .arg("run")
        .arg(&board)
        .args(["--drc", "--json", "--schematic"])
        .arg(&schematic)
        .arg("--placement")
        .arg(&placement)
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "report-only DRC keeps exit 0 while companion resolution succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], false, "{json}");
    assert!(json["inputs"]
        .as_array()
        .expect("input inventory")
        .iter()
        .any(|input| input["format"] == "eagle_schematic"));
}
