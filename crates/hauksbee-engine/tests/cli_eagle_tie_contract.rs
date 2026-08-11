use std::process::Command;

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
fn declared_and_undeclared_pairs_have_matching_json_exit_and_junit_outcomes() {
    let (declared, declared_junit) = run_pair(
        "declared",
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd"),
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch"),
    );
    assert!(
        declared.status.success(),
        "{}",
        String::from_utf8_lossy(&declared.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&declared.stdout).unwrap();
    assert_eq!(json["verdict"], "pass");
    assert!(json["drc"]["shorts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|short| short["severity"] == "note"));
    assert!(
        declared_junit.contains("failures=\"0\""),
        "{declared_junit}"
    );

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
