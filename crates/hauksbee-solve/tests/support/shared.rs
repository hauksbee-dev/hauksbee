//! Helpers shared across the integration-test modules of the `it` binary
//! (declared once in `tests/main.rs`): the standard RC low-pass fixture and
//! the waveform comparisons the parity gates use.

#![allow(dead_code)]

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_solve::Waveforms;

/// `V1(in, v) -> R1(r) -> out -> C1(farads, IC=0) -> gnd`: the RC step fixture.
pub fn rc_lowpass(v: f64, r: f64, farads: f64) -> Circuit {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: r,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads,
        ic: Some(0.0),
    });
    c
}

/// Driver island sensed by controlled/behavioral sources: `V1 -> Rd -> ctrl`,
/// with a capacitor to ground.
pub fn add_driver_leg(c: &mut Circuit) -> NodeId {
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    c.add(Device::Resistor {
        name: "Rd".into(),
        a: vin,
        b: ctrl,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "Cd".into(),
        a: ctrl,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
    ctrl
}

/// RC load island (`out`) driven by a controlled or behavioral source.
pub fn add_load_leg(c: &mut Circuit) -> NodeId {
    let out = c.node("out");
    c.add(Device::Resistor {
        name: "Rl".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "Cl".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
    out
}

/// Driver with an in-line zero-volt ammeter: `V1 -> Rd -> ctrl -> Cd`, plus
/// `ctrl -> Vs(0V) -> cm -> Rs -> gnd`. Returns the ammeter's id for F/H or
/// behavioral branch-current control; its current is `v_ctrl / 1k`.
pub fn add_sensed_driver_leg(c: &mut Circuit) -> DeviceId {
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    let cm = c.node("cm");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    c.add(Device::Resistor {
        name: "Rd".into(),
        a: vin,
        b: ctrl,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "Cd".into(),
        a: ctrl,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
    let vs = c.add(Device::Vsource {
        name: "Vs".into(),
        p: ctrl,
        n: cm,
        kind: SourceKind::Dc(0.0),
    });
    c.add(Device::Resistor {
        name: "Rs".into(),
        a: cm,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    vs
}

/// Four independent RC legs behind the input source cut make the automatic
/// partition heuristics fragment the circuit and engage the partitioned engine.
pub fn add_partition_rc_legs(c: &mut Circuit) {
    let vin = c.node("in");
    for k in 0..4 {
        let leg = c.node(&format!("leg{k}"));
        c.add(Device::Resistor {
            name: format!("Rx{k}"),
            a: vin,
            b: leg,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: format!("Cx{k}"),
            a: leg,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
    }
}

/// Compare two waveform sets sample-for-sample over every node. Returns
/// `(max_abs_err, worst_tol_ratio)` where the ratio is `|a-b| / (reltol *
/// max(|a|,|b|) + vntol)`; a ratio <= 1.0 means "within solver tolerance".
pub fn max_deviation(a: &Waveforms, b: &Waveforms, reltol: f64, vntol: f64) -> (f64, f64) {
    assert_eq!(
        a.time.len(),
        b.time.len(),
        "runs produced different sample grids"
    );
    assert_eq!(a.node_voltages.len(), b.node_voltages.len());
    let mut max_abs = 0.0f64;
    let mut worst_ratio = 0.0f64;
    for (wa, wb) in a.node_voltages.iter().zip(b.node_voltages.iter()) {
        for (&va, &vb) in wa.iter().zip(wb.iter()) {
            assert!(va.is_finite() && vb.is_finite(), "non-finite sample");
            let err = (va - vb).abs();
            let bound = reltol * va.abs().max(vb.abs()) + vntol;
            max_abs = max_abs.max(err);
            worst_ratio = worst_ratio.max(err / bound);
        }
    }
    (max_abs, worst_ratio)
}

/// First-order-hold sample of a series at time `t`, clamped at the ends.
pub fn lerp_at(times: &[f64], vals: &[f64], t: f64) -> f64 {
    match times.binary_search_by(|x| x.partial_cmp(&t).unwrap()) {
        Ok(i) => vals[i],
        Err(0) => vals[0],
        Err(i) if i >= times.len() => *vals.last().unwrap(),
        Err(i) => {
            let (t0, t1) = (times[i - 1], times[i]);
            vals[i - 1] + (t - t0) / (t1 - t0) * (vals[i] - vals[i - 1])
        }
    }
}
