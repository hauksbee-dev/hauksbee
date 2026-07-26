//! `hauksbee run <file>.board` (issue #63): Board-as-Code is a first-class
//! analysis input.
//!
//! A `.board` file routes through `forge_codegen::Program::parse` ->
//! `code_to_board_text` -> `ExtractedBoard::from_auto`, producing the SAME
//! `ExtractedBoard` the layout formats feed the analysis. These tests prove the
//! round-trip preserves the bind: decompiling a board to `.board` and binding
//! the recompiled result reproduces the original board's stamped devices.

mod common;

use std::collections::BTreeSet;

use hauksbee_engine::{
    bind_board, code_to_board_text, decompile_any_to_code,
};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

/// The set of stamped IR device names, for a structural bind comparison.
fn device_names(text: &str) -> BTreeSet<String> {
    let board = ExtractedBoard::from_auto(text).expect("extract");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    bound
        .circuit
        .devices
        .iter()
        .map(|d| d.name().to_string())
        .collect()
}

#[test]
fn board_as_code_run_matches_kicad_pcb() {
    // Decompile the synthetic KiCad PCB to Board-as-Code, then recompile it and
    // bind; the device set must match binding the original PCB directly.
    let pcb = common::SYNTH_BOARD;
    let code = decompile_any_to_code(pcb).expect("to-code");
    assert!(
        code.contains("Board-as-Code"),
        "decompiled output carries the .board header"
    );
    let recompiled = code_to_board_text(&code).expect("from-code");

    let from_pcb = device_names(pcb);
    let from_board = device_names(&recompiled);
    assert!(!from_pcb.is_empty(), "the PCB binds at least one device");
    assert_eq!(
        from_pcb, from_board,
        "binding the .board round-trip must stamp the same devices as the .kicad_pcb"
    );
}

#[test]
fn netlist_to_board_preserves_components_and_nets() {
    // A netlist (no layout) becomes editable Board-as-Code with every component
    // and net carried through.
    let net = r#"(export (version "E")
  (components
    (comp (ref "D1") (value "1N4148") (footprint "Diode_SMD:D_SOD-323")
      (libsource (lib "Device") (part "D") (description "Diode")))
    (comp (ref "R1") (value "10k") (footprint "Resistor_SMD:R_0402_1005Metric")))
  (nets
    (net (code "1") (name "ANODE_NET")
      (node (ref "D1") (pin "2") (pinfunction "A"))
      (node (ref "R1") (pin "1")))
    (net (code "2") (name "CATHODE_NET")
      (node (ref "D1") (pin "1") (pinfunction "K"))
      (node (ref "R1") (pin "2")))))
"#;
    let code = decompile_any_to_code(net).expect("to-code netlist");
    assert!(code.contains("comp D1"), "D1 present in .board: {code}");
    assert!(code.contains("comp R1"), "R1 present in .board");
    assert!(code.contains("ANODE_NET") && code.contains("CATHODE_NET"));

    // The recompiled board binds the diode (pads 1/2 -> roles via the rule).
    let recompiled = code_to_board_text(&code).expect("from-code");
    let board = ExtractedBoard::from_auto(&recompiled).expect("re-extract");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    assert!(
        bound.circuit.devices.iter().any(|d| matches!(
            d,
            hauksbee_ir::Device::Diode { name, .. } if name == "D1"
        )),
        "the netlist-derived .board diode must bind to a Device::Diode"
    );
}
