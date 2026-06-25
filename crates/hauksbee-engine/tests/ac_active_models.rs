use hauksbee_engine::binder::bind_board;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::Device;
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

fn ina186_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "ina186_fixture".to_string(),
        nets: vec![
            Net {
                id: 1,
                name: "GND".to_string(),
            },
            Net {
                id: 2,
                name: "+3V3".to_string(),
            },
            Net {
                id: 3,
                name: "VREF".to_string(),
            },
            Net {
                id: 4,
                name: "SENSE_P".to_string(),
            },
            Net {
                id: 5,
                name: "SENSE_N".to_string(),
            },
            Net {
                id: 6,
                name: "ISENSE_OUT".to_string(),
            },
        ],
        components: vec![Component {
            reference: "U1".to_string(),
            value: "INA186".to_string(),
            lib_id: "Package_TO_SOT_SMD:SOT-363_SC-70-6".to_string(),
            footprint: "Package_TO_SOT_SMD:SOT-363_SC-70-6".to_string(),
            position: None,
            layer: "F.Cu".to_string(),
            properties: Vec::new(),
            dnp: false,
            pins: vec![
                pin("1", 3),
                pin("2", 1),
                pin("3", 2),
                pin("4", 4),
                pin("5", 5),
                pin("6", 6),
            ],
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
    assert_eq!(
        reference.map(|n| bound.circuit.node_name(n)),
        Some("VREF")
    );
    assert_eq!(gain, 25.0);
    assert_eq!(pole_hz, Some(45_000.0));
    assert!(
        bound
            .report
            .rows
            .iter()
            .any(|row| row.reference == "U1" && row.model_id.as_deref() == Some("ina186_dck_a1_family")),
        "bind report should name the INA186 model"
    );
}
