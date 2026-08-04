//! A literal `0` resistor value is a jumper, not a mathematical short (E29b).
//!
//! Ten resistors with the value `0` made a real 259-part board (anyshake
//! /explorer) unassertable: bound at the solver's 1 µΩ floor they stamped 1e6 S
//! into a matrix of milli-siemens, and the only diagnosis path was bisecting by
//! ignoring model classes. Bind them at a milliohm, which is the physical truth
//! of an 0402 link, and SAY so, because the value on the board is then not the
//! value in the matrix.
//!
//! Two-sided: the jumper board binds, notes and solves; a genuinely singular
//! board still refuses and names the net.

use hauksbee_engine::binder::bind_board;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};

fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.to_string(),
        net: Some(net),
        function: String::new(),
        kind: "passive".to_string(),
        position: None,
    }
}

fn comp(reference: &str, value: &str, pins: Vec<Pin>) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: String::new(),
        footprint: "Resistor_SMD:R_0402_1005Metric".to_string(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins,
    }
}

/// A chain of `0`-valued links between real nets, exactly the anyshake shape.
fn jumper_board() -> ExtractedBoard {
    let mut components = Vec::new();
    // Ten 0R links in a chain: NET_1 - NET_2 - ... - NET_11. Net ids start at 1
    // because KiCad reserves 0 for "no net".
    for i in 1..=10 {
        components.push(comp(
            &format!("R{i}"),
            "0",
            vec![pin("1", i), pin("2", i + 1)],
        ));
    }
    // Real loads so the board is not just a floating chain of shorts.
    components.push(comp("R100", "10k", vec![pin("1", 1), pin("2", 99)]));
    components.push(comp("R101", "4k7", vec![pin("1", 11), pin("2", 99)]));
    let mut nets: Vec<Net> = (1..=11)
        .map(|i| Net {
            id: i,
            name: format!("NET_{i}"),
        })
        .collect();
    nets.push(Net {
        id: 99,
        name: "GND".to_string(),
    });
    ExtractedBoard {
        name: "zero_ohm_jumper_test".to_string(),
        nets,
        components,
    }
}

#[test]
fn a_zero_ohm_link_binds_as_a_milliohm_jumper_with_a_note() {
    let bound = bind_board(&jumper_board(), &ModelLibrary::builtin());

    // Every sub-ohm resistor on this board is one of the ten 0R links (the two
    // loads are 10k and 4k7), and every one must be a milliohm, not a microohm.
    let links: Vec<(&str, f64)> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Resistor { name, ohms, .. } if *ohms < 1.0 => {
                Some((name.as_str(), *ohms))
            }
            _ => None,
        })
        .collect();
    assert_eq!(links.len(), 10, "all ten 0R jumpers must be bound: {links:?}");
    for (name, ohms) in &links {
        assert!(
            (*ohms - 1e-3).abs() < 1e-12,
            "{name} must bind at 1 mohm, got {ohms} ohm"
        );
    }

    // And every one of them must be NAMED in a bind note. A silent value
    // substitution is the exact class of thing this project refuses.
    let noted: Vec<&str> = bound
        .report
        .rows
        .iter()
        .filter_map(|row| row.warning.as_deref())
        .filter(|w| w.contains("jumper"))
        .collect();
    assert_eq!(
        noted.len(),
        10,
        "each 0R jumper needs its own printed note, got: {noted:?}"
    );
    assert!(
        noted[0].contains("1 mohm") && noted[0].contains("0 ohm jumper"),
        "the note must say what the value became and why: {}",
        noted[0]
    );
}

#[test]
fn a_board_of_zero_ohm_links_actually_solves() {
    let bound = bind_board(&jumper_board(), &ModelLibrary::builtin());
    let opts = SolverOptions::default();
    let mut ws = Workspace::new(&bound.circuit);
    dc_operating_point(&mut ws, &bound.circuit, &opts)
        .expect("a board of 0R jumpers must solve, not poison the matrix");

    // And no jumper reads as a pathological suspect afterwards, so a later
    // failure on this board never gets blamed on the jumpers by default.
    assert!(
        hauksbee_solve::blame::stiff_links(&bound.circuit).is_empty(),
        "milliohm jumpers must not read as matrix poison: {:?}",
        hauksbee_solve::blame::stiff_links(&bound.circuit)
    );
}
