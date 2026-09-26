use hauksbee_engine::binder::bind_board;
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

use crate::support::netted_pin as pin;

fn ina186_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "ina186_fixture".to_string(),
        nets: vec![
            crate::support::net(1, "GND".to_string()),
            crate::support::net(2, "+3V3".to_string()),
            crate::support::net(3, "VREF".to_string()),
            crate::support::net(4, "SENSE_P".to_string()),
            crate::support::net(5, "SENSE_N".to_string()),
            crate::support::net(6, "ISENSE_OUT".to_string()),
        ],
        components: vec![Component {
            reference: "U1".to_string(),
            value: "INA186".to_string(),
            lib_id: "Package_TO_SOT_SMD:SOT-363_SC-70-6".to_string(),
            footprint: "Package_TO_SOT_SMD:SOT-363_SC-70-6".to_string(),
            layer: "F.Cu".to_string(),
            pins: vec![
                pin("1", 3),
                pin("2", 1),
                pin("3", 2),
                pin("4", 4),
                pin("5", 5),
                pin("6", 6),
            ],
            ..Default::default()
        }],
    }
}

#[test]
fn ina186_sc70_binds_as_behavioral_current_sense_amp() {
    let board = ina186_board();
    let bound = bind_board(&board, &ModelLibrary::builtin());

    let opamps: Vec<_> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|device| match device {
            Device::OpAmp {
                out,
                inp,
                inn,
                reference,
                gain,
                pole_hz,
                ..
            } => Some((*out, *inp, *inn, *reference, *gain, *pole_hz)),
            _ => None,
        })
        .collect();

    assert_eq!(opamps.len(), 1, "expected INA186 to stamp one OpAmp");
    let (out, inp, inn, reference, gain, pole_hz) = opamps[0];
    assert_eq!(bound.circuit.node_name(out), "ISENSE_OUT");
    assert_eq!(bound.circuit.node_name(inp), "SENSE_P");
    assert_eq!(bound.circuit.node_name(inn), "SENSE_N");
    assert_eq!(reference.map(|n| bound.circuit.node_name(n)), Some("VREF"));
    assert_eq!(gain.to_bits(), 25.0_f64.to_bits());
    assert_eq!(pole_hz, Some(45_000.0));
    assert!(
        bound
            .report
            .rows
            .iter()
            .any(|row| row.reference == "U1"
                && row.model_id.as_deref() == Some("ina186_dck_a1_family")),
        "bind report should name the INA186 model"
    );
}
