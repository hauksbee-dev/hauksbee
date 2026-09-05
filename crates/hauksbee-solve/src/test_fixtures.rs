//! Compact circuit builders, stamp scaffolding and waveform comparisons
//! shared by the crate's unit tests, so each test states only the circuit
//! and the number it expects. Test-only: `lib.rs` declares this module under
//! `#[cfg(test)]`, and nothing in the shipped library depends on it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::LazyLock;

use hauksbee_ir::{BjtModel, Circuit, Device, DeviceId, DiodeModel, NodeId, Polarity, SourceKind};

use crate::options::{Integration, Partitioning, SolverOptions, StepControl};
use crate::stamp::{IntegCoeffs, StampCtx};
use crate::system::{Layout, ReactiveState};
use crate::transient::{Transient, Waveforms};

pub const GND: NodeId = NodeId::GROUND;

pub fn res(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, ohms: f64) -> DeviceId {
    c.add(Device::Resistor {
        name: name.into(),
        a,
        b,
        ohms,
        tc1: None,
    })
}

pub fn cap(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, farads: f64) -> DeviceId {
    c.add(Device::Capacitor {
        name: name.into(),
        a,
        b,
        farads,
        ic: None,
    })
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
    c.add(Device::Vsource {
        name: name.into(),
        p,
        n,
        kind: SourceKind::Dc(volts),
    })
}

pub fn idc(c: &mut Circuit, name: &str, p: NodeId, n: NodeId, amps: f64) -> DeviceId {
    c.add(Device::Isource {
        name: name.into(),
        p,
        n,
        kind: SourceKind::Dc(amps),
    })
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
        model: model.clone(),
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

pub fn fixed_opts(dt: f64) -> SolverOptions {
    SolverOptions {
        step: StepControl::Fixed { dt },
        integration: Integration::Trapezoidal,
        ..SolverOptions::default()
    }
}

/// The fused monolithic transient of `c` under `opts` (partitioning off).
pub fn monolith(c: &Circuit, opts: &SolverOptions, tstop: f64) -> Waveforms {
    let mut mono_opts = *opts;
    mono_opts.partitioning = Partitioning::Off;
    Transient::new(mono_opts).run(c, tstop).expect("monolith")
}

pub fn lerp_at(times: &[f64], vals: &[f64], t: f64) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    match times.binary_search_by(|x| x.partial_cmp(&t).expect("non-finite sample time")) {
        Ok(i) => vals[i],
        Err(0) => vals[0],
        Err(i) if i >= times.len() => *vals.last().unwrap(),
        Err(i) => {
            let (t0, t1) = (times[i - 1], times[i]);
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            vals[i - 1] + f * (vals[i] - vals[i - 1])
        }
    }
}

/// Max |sample - reference| over every node (ground excluded) at every sample
/// time of `time`, interpolating the reference.
pub fn max_error(
    c: &Circuit,
    time: &[f64],
    nodes: &[Vec<f64>],
    reference: &Waveforms,
) -> (f64, NodeId) {
    let mut worst = (0.0f64, NodeId(0));
    for node in 1..c.node_count() {
        for (k, &t) in time.iter().enumerate() {
            let err = (nodes[node][k]
                - lerp_at(&reference.time, &reference.node_voltages[node], t))
            .abs();
            if err > worst.0 {
                worst = (err, NodeId(node as u32));
            }
        }
    }
    worst
}

/// Two-sided capture-grid compare: every sample must agree with the reference
/// within `tol(reference value)`, or the reference must attain the sample's
/// value somewhere within `±dt` (an edge placed up to one grid interval away).
pub fn assert_matches_within_grid(
    c: &Circuit,
    time: &[f64],
    nodes: &[Vec<f64>],
    reference: &Waveforms,
    dt: f64,
    tol: &dyn Fn(f64) -> f64,
    what: &str,
) {
    for node in 1..c.node_count() {
        let m = &reference.node_voltages[node];
        for (k, &t) in time.iter().enumerate() {
            let sv = nodes[node][k];
            let mv = lerp_at(&reference.time, m, t);
            let tol = tol(mv);
            if (sv - mv).abs() <= tol {
                continue;
            }
            let edge = (0..=8).any(|j| {
                let tt = t - dt + (j as f64) * (dt / 4.0);
                (lerp_at(&reference.time, m, tt) - sv).abs() <= tol
            });
            assert!(
                edge,
                "{what}: diverged at {} t={t:.3e}: {sv:.9} vs {mv:.9} (tol {tol:.3e})",
                c.node_name(NodeId(node as u32))
            );
        }
    }
}

pub fn swing(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
}

/// Up-crossings of `level`: one per spike.
pub fn up_crossings(v: &[f64], level: f64) -> usize {
    v.windows(2)
        .filter(|w| w[0] < level && w[1] >= level)
        .count()
}

static NO_SPDT: LazyLock<HashMap<DeviceId, DeviceId>> = LazyLock::new(HashMap::new);

/// A transient-shaped stamp context with every optional knob off: gmin 0,
/// no regularizer, no frozen decisions. Callers adjust the public fields.
pub fn stamp_ctx<'a>(
    circuit: &'a Circuit,
    layout: &'a Layout,
    opts: &'a SolverOptions,
    x: &'a [f64],
    state: &'a ReactiveState,
    coeffs: IntegCoeffs,
) -> StampCtx<'a> {
    StampCtx {
        circuit,
        layout,
        opts,
        x,
        x_prev: x,
        time: 0.0,
        coeffs,
        state,
        dc: false,
        use_ic: false,
        gmin: 0.0,
        src_scale: 1.0,
        branch_reg: 0.0,
        cmp_freeze: None,
        switch_freeze: None,
        switch_latch: None,
        spdt_sibling: &NO_SPDT,
        junction_eval: None,
    }
}

pub fn trapz(dt: f64) -> IntegCoeffs {
    IntegCoeffs::for_step(Integration::Trapezoidal, dt, dt, false)
}
