//! Regression: a valueless Altium part must never bind a fabricated magnitude.
//!
//! The `.PcbDoc` extractor once substituted a part's DESIGNATOR text for its
//! missing value, and the binder's RKM value parser then happily decoded the
//! refdes: "R74" bound as a 0.74 ohm resistor and "RED" (an LED designator)
//! as 1 ohm, both at guessed confidence, fabricating overpower faults on real
//! boards (elk-audio ElkPi, miniFOC). The extractor now leaves the value
//! empty (with the `.SchDoc` reason exposed as a property); this test pins
//! the binder half of the contract: an EMPTY value on an R-class or D-class
//! reference stays UNRESOLVED, no magnitude appears from the refdes at any
//! confidence.

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::report::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_models::ModelLibrary;

fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.to_string(),
        net: Some(net),
        function: String::new(),
        kind: String::new(),
        position: None,
    }
}

/// A component exactly as the fixed Altium extractor emits it: empty value,
/// the unresolved reason attached as a property, PCB-only pins.
fn valueless(reference: &str, footprint: &str, nets: (i64, i64)) -> Component {
    Component {
        reference: reference.to_string(),
        value: String::new(),
        lib_id: footprint.to_string(),
        footprint: footprint.to_string(),
        position: None,
        layer: "F.Cu".to_string(),
        properties: vec![(
            "value_unresolved".to_string(),
            "no value in the PcbDoc; Altium keeps values in the .SchDoc".to_string(),
        )],
        dnp: false,
        pins: vec![pin("1", nets.0), pin("2", nets.1)],
    }
}

#[test]
fn refdes_shaped_names_never_become_magnitudes() {
    let board = ExtractedBoard {
        name: "no_fabricated_values".to_string(),
        nets: vec![
            Net {
                id: 1,
                name: "A".to_string(),
            },
            Net {
                id: 2,
                name: "B".to_string(),
            },
        ],
        components: vec![
            // "R74" as a VALUE would RKM-decode to 0.74 ohm; as a refdes with
            // no value it must stay unresolved.
            valueless("R74", "RESC3216X70N", (1, 2)),
            // "RED" as a VALUE would RKM-decode to 1 ohm ("R" = the decimal
            // point at zero); as an LED designator it must stay unresolved.
            valueless("RED", "LED0603", (1, 2)),
        ],
    };
    let bound = bind_board(&board, &ModelLibrary::builtin());

    // R74 has no value and no other evidence: it must bind UNRESOLVED.
    let r74 = bound
        .report
        .rows
        .iter()
        .find(|row| row.reference == "R74")
        .expect("R74 has a bind row");
    assert!(
        matches!(r74.outcome, BindOutcome::Unresolved { .. }),
        "R74: a missing value must bind UNRESOLVED, got {:?}",
        r74.outcome
    );
    // The bind row's reason carries the extractor's explanation, so the table
    // says WHY there is nothing to resolve, not a bare "no model".
    if let BindOutcome::Unresolved { reason } = &r74.outcome {
        assert!(
            reason.contains("no value in the PcbDoc")
                && reason.contains(".SchDoc"),
            "the unresolved reason must surface the value_unresolved property: {reason}"
        );
    }

    // RED sits on an LED footprint, so a generic-diode bind is legitimate
    // (footprint evidence, not a magnitude). What must never happen, for
    // either part, is an R/C/L stamped with a magnitude decoded from the
    // refdes: R74 as 0.74 ohm, RED as 1 ohm.
    for d in &bound.circuit.devices {
        let (kind, name) = match d {
            hauksbee_ir::Device::Resistor { name, .. } => ("resistor", name),
            hauksbee_ir::Device::Capacitor { name, .. } => ("capacitor", name),
            hauksbee_ir::Device::Inductor { name, .. } => ("inductor", name),
            _ => continue,
        };
        assert!(
            !name.contains("R74") && !name.contains("RED"),
            "a {kind} magnitude was fabricated from a refdes: {name}"
        );
    }
}
