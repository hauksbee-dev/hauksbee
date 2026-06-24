use hauksbee_extract::ExtractedBoard;
use std::path::PathBuf;

fn corpus(rel: &str) -> Option<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../board-corpus")
        .join(rel);
    std::fs::read_to_string(p).ok()
}

#[test]
fn dnp_flag_parsed_from_pcb_attr_and_schematic_symbol() {
    // PCB footprint: `(attr ... dnp)` marks Do-Not-Populate; a plain `(attr smd)`
    // does not. A minimal two-footprint board.
    let pcb = r#"(kicad_pcb (version 20240108)
  (net 0 "")
  (net 1 "N1")
  (footprint "Resistor_SMD:R_0402_1005Metric" (layer "F.Cu")
    (attr smd exclude_from_bom dnp)
    (property "Reference" "R1")
    (property "Value" "5k1")
    (pad "1" smd roundrect (at 0 0) (size 0.5 0.5) (layers "F.Cu") (net 1 "N1")))
  (footprint "Resistor_SMD:R_0402_1005Metric" (layer "F.Cu")
    (attr smd)
    (property "Reference" "R2")
    (property "Value" "10k")
    (pad "1" smd roundrect (at 5 0) (size 0.5 0.5) (layers "F.Cu") (net 1 "N1"))))
"#;
    let board = ExtractedBoard::from_kicad_pcb(pcb).unwrap();
    let r1 = board
        .components
        .iter()
        .find(|c| c.reference == "R1")
        .unwrap();
    let r2 = board
        .components
        .iter()
        .find(|c| c.reference == "R2")
        .unwrap();
    assert!(r1.dnp, "R1 has `(attr ... dnp)`, must be DNP");
    assert!(!r2.dnp, "R2 has `(attr smd)` only, must not be DNP");
}

#[test]
fn pic_programmer_pcb() {
    let Some(src) = corpus("kicad-demos-src/demos/pic_programmer/pic_programmer.kicad_pcb") else {
        eprintln!("corpus missing; skipping");
        return;
    };
    let board = ExtractedBoard::from_kicad_pcb(&src).unwrap();
    assert!(
        board.components.len() > 50,
        "got {}",
        board.components.len()
    );
    assert!(board.nets.len() > 50);
    let gnd = board.net_by_name("GND").expect("GND net");
    assert!(board.net_members(gnd.id).len() > 10);
    // Every pad's net id must exist in the net table.
    assert!(board.lint().undeclared_nets.is_empty());
    // Every component must have a reference.
    for c in &board.components {
        assert!(!c.reference.is_empty(), "unnamed component {:?}", c.lib_id);
    }
}

#[test]
fn kicad5_module_format() {
    let Some(src) = corpus("stormduino/stormduino Rev2.kicad_pcb") else {
        eprintln!("corpus missing; skipping");
        return;
    };
    let board = ExtractedBoard::from_kicad_pcb(&src).unwrap();
    assert!(
        board.components.len() > 20,
        "got {}",
        board.components.len()
    );
    assert!(
        board.components.iter().any(|c| !c.reference.is_empty()),
        "KiCad 5 fp_text references not extracted"
    );
}

#[test]
fn tarski_netlist() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tarski_inputsystem.net");
    let Ok(src) = std::fs::read_to_string(p) else {
        eprintln!("tarski netlist missing; skipping");
        return;
    };
    let board = ExtractedBoard::from_kicad_netlist(&src).unwrap();
    assert_eq!(board.components.len(), 3442);
    assert!(board.nets.len() > 2000, "got {}", board.nets.len());
    // The known Tarski structure: 90 shift registers, 19 comparators.
    let count = |pred: &dyn Fn(&hauksbee_extract::Component) -> bool| {
        board.components.iter().filter(|c| pred(c)).count()
    };
    assert_eq!(count(&|c| c.lib_id.contains("74HC595")), 90);
    assert_eq!(count(&|c| c.value.contains("LMV7219")), 19);
    // Pin functions came through from the schematic.
    let with_funcs = board
        .components
        .iter()
        .flat_map(|c| &c.pins)
        .filter(|p| !p.function.is_empty())
        .count();
    assert!(with_funcs > 1000);
}

#[test]
fn tarski_pcb_full_board() {
    let p = PathBuf::from(
        "/Users/hauksbee-user/Tarski/Tarski-Repos/Tarski-Schematics/Neuron/InputSystem/InputSystem.kicad_pcb",
    );
    let Ok(src) = std::fs::read_to_string(p) else {
        eprintln!("tarski pcb missing; skipping");
        return;
    };
    let t0 = std::time::Instant::now();
    let board = ExtractedBoard::from_kicad_pcb(&src).unwrap();
    let dt = t0.elapsed();
    eprintln!(
        "tarski pcb: {} components, {} nets in {dt:?}",
        board.components.len(),
        board.nets.len()
    );
    assert!(board.components.len() > 3000);
    assert!(board.lint().undeclared_nets.is_empty());
    assert!(dt.as_secs() < 30, "44MB extraction took {dt:?}");
}
