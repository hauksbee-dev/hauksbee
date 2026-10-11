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

#[must_use]
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
        tpd_s: 0.0,
        supply: hauksbee_ir::Supply::default(),
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

/// A 0/5 V comparator with propagation delay `tpd_s`.
pub fn comparator_tpd(
    c: &mut Circuit,
    name: &str,
    (out, inp, inn): (NodeId, NodeId, NodeId),
    hysteresis: f64,
    tpd_s: f64,
) -> DeviceId {
    c.add(Device::Comparator {
        name: name.into(),
        out,
        inp,
        inn,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis,
        tpd_s,
        supply: hauksbee_ir::Supply::default(),
    })
}

/// Timing of [`comparator_relaxation`]: the charge resistor, the discharge
/// resistor and the timing capacitor.
pub const RELAX_RC: f64 = 2e3;
pub const RELAX_RD: f64 = 10e3;
pub const RELAX_C: f64 = 1e-9;

/// A diode-steered comparator relaxation oscillator; returns `(out, ct)`.
///
/// `K{tag}` compares a 2.5 V reference against the timing cap `ct{tag}` with
/// a 1 V band (trips at 2.0 V and 3.0 V). High, its output `out{tag}`
/// charges the cap through a diode and [`RELAX_RC`] toward ~3.6 V; low, the
/// diode blocks and the cap discharges through [`RELAX_RD`] alone, so the low
/// phase is exactly `RELAX_RD * RELAX_C * ln 1.5`. The diode also breaks the loop for the
/// relaxed (diodes open) DC seed, the route by which a board's astable
/// comparator loops reach their pinned t = 0 operating point.
pub fn comparator_relaxation(c: &mut Circuit, tag: &str, tpd_s: f64) -> (NodeId, NodeId) {
    let out = c.node(&format!("out{tag}"));
    let ct = c.node(&format!("ct{tag}"));
    let rf = c.node(&format!("ref{tag}"));
    let a = c.node(&format!("steer{tag}"));
    vdc(c, &format!("VREF{tag}"), rf, 2.5);
    comparator_tpd(c, &format!("K{tag}"), (out, rf, ct), 1.0, tpd_s);
    diode(c, &format!("DSTEER{tag}"), out, a, DiodeModel::default());
    res(c, &format!("RCHG{tag}"), a, ct, RELAX_RC);
    res(c, &format!("RDIS{tag}"), ct, GND, RELAX_RD);
    cap(c, &format!("CT{tag}"), ct, GND, RELAX_C);
    (out, ct)
}

/// Three comparators in cascade, each flip driving the next one's input.
///
/// `K1` is [`comparator_relaxation`] (tag `1`); its output drives `K2`'s
/// inverting input directly against 2.5 V, so `o2` is high through `K1`'s
/// ~4 us low phase; `o2` charges an integrate-and-fire node `m` (2 kOhm,
/// 1 nF) into `K3` (trip 2.1 V rising, 1.9 V falling), whose fire is
/// stretched through a diode onto `s` (10 kOhm, 10 pF) and closes a 10 ohm
/// reset switch across its own integrator, as a self-resetting oscillator does.
#[must_use]
pub fn comparator_cascade(tpd_s: f64) -> Circuit {
    let mut c = Circuit::new();
    let (o1, _) = comparator_relaxation(&mut c, "1", tpd_s);
    let (o2, o3, m, s) = (c.node("o2"), c.node("o3"), c.node("m"), c.node("s"));
    let (mid, th) = (c.node("mid"), c.node("th"));
    vdc(&mut c, "VMID", mid, 2.5);
    vdc(&mut c, "VTH", th, 2.0);
    comparator_tpd(&mut c, "K2", (o2, mid, o1), 0.1, tpd_s);
    res(&mut c, "RIN", o2, m, 2e3);
    cap(&mut c, "CM", m, GND, 1e-9);
    comparator_tpd(&mut c, "K3", (o3, m, th), 0.2, tpd_s);
    diode(&mut c, "DSTRETCH", o3, s, DiodeModel::default());
    res(&mut c, "RS", s, GND, 10e3);
    cap(&mut c, "CS", s, GND, 10e-12);
    sw(&mut c, "SRESET", m, GND, s, (3.0, 2.0), 10.0);
    c
}

/// 1 V through two 1 kΩ resistors to ground: the midpoint solves to 0.5 V.
/// Returns `(circuit, top, mid)`.
#[must_use]
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

/// The shunt-fed PNP array: +5V -> `r_shunt` -> `VDD_A` -> `n` blocks.
/// Returns `(circuit, rail, feed, shunt device, blocks)`.
#[must_use]
pub fn shunt_array(
    n: usize,
    r_shunt: f64,
) -> (Circuit, NodeId, NodeId, DeviceId, Vec<(NodeId, NodeId)>) {
    let mut c = Circuit::new();
    let p5 = c.node("+5V");
    vdc(&mut c, "V5", p5, 5.0);
    let rail = c.node("VDD_A");
    let shunt = res(&mut c, "R_shunt", p5, rail, r_shunt);
    let blocks = pnp_blocks(&mut c, "", rail, n, GND, 100e3, 10e3);
    (c, rail, p5, shunt, blocks)
}
