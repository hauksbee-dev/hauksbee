//! The IR must serialize and deserialize losslessly so circuits can be cached
//! and shipped to the solver / UI.

use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId, SourceKind, SpiceLoader};

#[test]
fn circuit_json_roundtrip() {
    let mut c = Circuit::new();
    let a = c.node("a");
    let k = c.node("k");
    c.temp_c = 50.0;
    c.add(Device::Vsource {
        name: "V1".into(),
        p: a,
        n: NodeId::GROUND,
        kind: SourceKind::Sin {
            offset: 0.0,
            amplitude: 5.0,
            freq: 1e3,
            delay: 0.0,
            theta: 0.0,
            phase: 0.0,
        },
    });
    c.add(Device::Diode {
        name: "D1".into(),
        a,
        k,
        model: DiodeModel::default(),
    });

    let json = serde_json::to_string(&c).expect("serialize");
    let back: Circuit = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.devices.len(), c.devices.len());
    assert!((back.temp_c - 50.0).abs() < 1e-12);
    assert_eq!(back.node_name(a), "a");
    assert_eq!(back.node_name(k), "k");
}

/// Every variant in the [`Device::examples`] inventory must round-trip
/// losslessly. `examples()` panics if a variant ships without an example
/// (the strum::EnumCount length assert), so a new device cannot dodge this
/// test by omission.
#[test]
fn every_device_variant_roundtrips() {
    let mut c = Circuit::new();
    let n = [c.node("n1"), c.node("n2"), c.node("n3"), c.node("n4")];
    for dev in Device::examples(n) {
        let json = serde_json::to_string(&dev).expect("serialize");
        let back: Device = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            format!("{dev:?}"),
            format!("{back:?}"),
            "{} must round-trip losslessly",
            dev.name()
        );
    }
}

#[test]
fn loaded_circuit_roundtrips() {
    let net = "rc\nV1 in 0 SIN(0 5 1k)\nR1 in out 1k\nC1 out 0 1u\n.end\n";
    let c = SpiceLoader::load(net).unwrap();
    let json = serde_json::to_string(&c).unwrap();
    let back: Circuit = serde_json::from_str(&json).unwrap();
    assert_eq!(back.devices.len(), 3);
    assert_eq!(back.node_name(NodeId::GROUND), "0");
}
