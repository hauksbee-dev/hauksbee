//! Compact circuit builders shared by every crate's tests, so a fixture
//! states only the device and the number it expects instead of a raw
//! `Device::X { .. }` struct literal repeated at each call site.
//!
//! Moved here from `hauksbee-solve/src/test_fixtures.rs` (which stayed
//! `#[cfg(test)]`-private and so was invisible to that crate's own
//! integration tests, and to every other crate's tests). This module holds
//! only the device-literal builders, which need nothing but `hauksbee-ir`;
//! solver-internal helpers (transient running, stamp contexts) stay in
//! `hauksbee-solve`'s own `tests/support`, since they touch that crate's
//! private types.
#![allow(dead_code)]

use hauksbee_ir::{BjtModel, Circuit, Device, DeviceId, DiodeModel, NodeId, Polarity, SourceKind};

pub const GND: NodeId = NodeId::GROUND;

pub fn res(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, ohms: f64) -> DeviceId {
    c.add(Device::resistor(name, a, b, ohms))
}

pub fn cap(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, farads: f64) -> DeviceId {
    c.add(Device::capacitor(name, a, b, farads))
}

pub fn cap_ic(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, farads: f64, ic: f64) -> DeviceId {
    c.add(Device::Capacitor {
        name: name.into(),
        a,
        b,
        farads,
        ic: Some(ic),
    })
}

pub fn ind(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, henries: f64) -> DeviceId {
    c.add(Device::Inductor {
        name: name.into(),
        a,
        b,
        henries,
        ic: Some(0.0),
    })
}

pub fn vdc(c: &mut Circuit, name: &str, p: NodeId, volts: f64) -> DeviceId {
    vdc_between(c, name, p, GND, volts)
}

pub fn vdc_between(c: &mut Circuit, name: &str, p: NodeId, n: NodeId, volts: f64) -> DeviceId {
    c.add(Device::vsource(name, p, n, SourceKind::Dc(volts)))
}

pub fn idc(c: &mut Circuit, name: &str, p: NodeId, n: NodeId, amps: f64) -> DeviceId {
    c.add(Device::isource(name, p, n, SourceKind::Dc(amps)))
}

/// A one-shot pulse source `p -> GND`: `v1` until `delay`, then a `rise` ramp
/// to `v2`, held for `width`, then a `fall` ramp back.
pub fn vpulse(
    c: &mut Circuit,
    name: &str,
    p: NodeId,
    (v1, v2): (f64, f64),
    delay: f64,
    (rise, fall): (f64, f64),
    width: f64,
) -> DeviceId {
    c.add(Device::Vsource {
        name: name.into(),
        p,
        n: GND,
        kind: SourceKind::Pulse {
            v1,
            v2,
            delay,
            rise,
            fall,
            width,
            period: 0.0,
        },
    })
}

pub fn diode(c: &mut Circuit, name: &str, a: NodeId, k: NodeId, model: DiodeModel) -> DeviceId {
    c.add(Device::Diode {
        name: name.into(),
        a,
        k,
        model,
    })
}

pub fn bjt(
    c: &mut Circuit,
    name: &str,
    col: NodeId,
    base: NodeId,
    emit: NodeId,
    model: &BjtModel,
) -> DeviceId {
    c.add(Device::Bjt {
        name: name.into(),
        c: col,
        b: base,
        e: emit,
        model: *model,
    })
}

pub fn pnp() -> BjtModel {
    BjtModel {
        polarity: Polarity::P,
        ..BjtModel::default()
    }
}

/// A 0/5 V comparator.
pub fn comparator(
    c: &mut Circuit,
    name: &str,
    out: NodeId,
    inp: NodeId,
    inn: NodeId,
    hysteresis: f64,
) -> DeviceId {
    c.add(Device::Comparator {
        name: name.into(),
        out,
        inp,
        inn,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis,
    })
}

/// A switch whose control is `ctrl` against ground, `roff = 1e9`.
pub fn sw(
    c: &mut Circuit,
    name: &str,
    a: NodeId,
    b: NodeId,
    ctrl: NodeId,
    (von, voff): (f64, f64),
    ron: f64,
) -> DeviceId {
    vswitch(c, name, a, b, (ctrl, GND), (von, voff), (ron, 1e9))
}

pub fn vswitch(
    c: &mut Circuit,
    name: &str,
    a: NodeId,
    b: NodeId,
    (ctrl_p, ctrl_n): (NodeId, NodeId),
    (von, voff): (f64, f64),
    (ron, roff): (f64, f64),
) -> DeviceId {
    c.add(Device::VSwitch {
        name: name.into(),
        a,
        b,
        ctrl_p,
        ctrl_n,
        von,
        voff,
        ron,
        roff,
    })
}

/// 1 V through two 1 kΩ resistors to ground: the midpoint solves to 0.5 V.
/// Returns `(circuit, top, mid)`.
pub fn divider() -> (Circuit, NodeId, NodeId) {
    let mut c = Circuit::new();
    let top = c.node("top");
    let mid = c.node("mid");
    vdc(&mut c, "V", top, 1.0);
    res(&mut c, "R1", top, mid, 1e3);
    res(&mut c, "R2", mid, GND, 1e3);
    (c, top, mid)
}

/// `n` PNP blocks hanging off `rail`: emitter on the rail, base through `rb`
/// to `base_to`, collector through `rc` to ground. Returns `(base, col)` per
/// block, named `{prefix}b{k}` / `{prefix}c{k}`.
pub fn pnp_blocks(
    c: &mut Circuit,
    prefix: &str,
    rail: NodeId,
    n: usize,
    base_to: NodeId,
    rb: f64,
    rc: f64,
) -> Vec<(NodeId, NodeId)> {
    let model = pnp();
    (0..n)
        .map(|k| {
            let base = c.node(&format!("{prefix}b{k}"));
            let col = c.node(&format!("{prefix}c{k}"));
            bjt(c, &format!("{prefix}Q{k}"), col, base, rail, &model);
            res(c, &format!("{prefix}Rb{k}"), base, base_to, rb);
            res(c, &format!("{prefix}Rc{k}"), col, GND, rc);
            (base, col)
        })
        .collect()
}

/// The shunt-fed PNP array: +5V -> `r_shunt` -> ANALOG_VDD -> `n` blocks.
/// Returns `(circuit, rail, feed, shunt device, blocks)`.
pub fn shunt_array(
    n: usize,
    r_shunt: f64,
) -> (Circuit, NodeId, NodeId, DeviceId, Vec<(NodeId, NodeId)>) {
    let mut c = Circuit::new();
    let p5 = c.node("+5V");
    vdc(&mut c, "V5", p5, 5.0);
    let rail = c.node("ANALOG_VDD");
    let shunt = res(&mut c, "R_shunt", p5, rail, r_shunt);
    let blocks = pnp_blocks(&mut c, "", rail, n, GND, 100e3, 10e3);
    (c, rail, p5, shunt, blocks)
}
