use std::process::Command;

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
}
