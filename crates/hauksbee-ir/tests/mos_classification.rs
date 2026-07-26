//! Dev-plan 04 §3.3: the MOSFET's conduction/sense split is MODEL-DEPENDENT.
//! Gate and bulk are sense terminals for a default (charge-free, body-free)
//! model; the pre-§3.3 classification, bit-identically, and become
//! conduction terminals exactly when the model carries gate capacitance /
//! bulk-junction physics, because their KCL rows then receive the companion
//! and junction currents. The solve-side zero-row cross-check holds the
//! default claim to the stamp; this test pins the flip itself.

use hauksbee_ir::{Circuit, Device, MosfetModel};

fn mos(model: MosfetModel) -> (Device, [hauksbee_ir::NodeId; 4]) {
    let mut c = Circuit::new();
    let n = [c.node("d"), c.node("g"), c.node("s"), c.node("b")];
    (
        Device::Mosfet {
            name: "M1".into(),
            d: n[0],
            g: n[1],
            s: n[2],
            b: Some(n[3]),
            model,
        },
        n,
    )
}

#[test]
fn default_model_keeps_gate_and_bulk_sense() {
    let (dev, n) = mos(MosfetModel::default());
    assert_eq!(dev.conduction_nodes(), vec![n[0], n[2]]);
    assert_eq!(dev.sense_nodes(), vec![n[1], n[3]]);
}

#[test]
fn gate_charge_makes_the_gate_conduct() {
    let model = MosfetModel {
        cgd_ov: 1e-12,
        ..MosfetModel::default()
    };
    let (dev, n) = mos(model);
    assert_eq!(dev.conduction_nodes(), vec![n[0], n[2], n[1]]);
    assert_eq!(dev.sense_nodes(), vec![n[3]], "bulk stays sense");
}

#[test]
fn body_diode_makes_the_bulk_conduct() {
    let model = MosfetModel {
        body_is: 1e-14,
        ..MosfetModel::default()
    };
    let (dev, n) = mos(model);
    assert_eq!(dev.conduction_nodes(), vec![n[0], n[2], n[3]]);
    assert_eq!(dev.sense_nodes(), vec![n[1]], "gate stays sense");
}

#[test]
fn every_terminal_is_exactly_one_of_the_two() {
    for model in [
        MosfetModel::default(),
        MosfetModel {
            cgs_ov: 1e-12,
            body_is: 1e-14,
            cbd: 1e-12,
            ..MosfetModel::default()
        },
    ] {
        let (dev, _) = mos(model);
        let mut all = dev.nodes();
        all.sort_unstable();
        all.dedup();
        let mut both: Vec<_> = dev
            .conduction_nodes()
            .into_iter()
            .chain(dev.sense_nodes())
            .collect();
        both.sort_unstable();
        both.dedup();
        assert_eq!(all, both);
    }
}
