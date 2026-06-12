//! Framework unit tests for the declarative behavioural-model runtime.
//!
//! These exercise each of the four declarative facts in isolation against a
//! hand-built circuit, driven by a small iterate-to-consistency loop that
//! mirrors the scheduler's per-chunk cadence (solve a transient over a chunk,
//! read the final node voltages and source branch currents, feed them back into
//! `BehavioralDevice::update`, repeat). No board files, no corpus: pure runtime.
//!
//! Covered:
//!   - internal pull to a rail through a resistance (the nPM1300 SHPHLD shape);
//!   - an open-drain output asserted by an FSM state;
//!   - an expression-law current (the LTC6803 balancer-leak shape);
//!   - averaged-converter output regulation, output-current foldback, and the
//!     programmable input-current limit (the LTC4020 ILIMIT shape).

use std::collections::BTreeMap;

use galvani_engine::behavioral::BehavioralDevice;
use galvani_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use galvani_models::behavioral::Behavioral;
use galvani_models::Params;
use galvani_solve::{Layout, SolverOptions, StepControl, Transient};

/// A minimal iterate-to-consistency driver over one behavioural device.
struct Harness {
    circuit: Circuit,
    dev: BehavioralDevice,
    node_volts: Vec<f64>,
    branch_x: Vec<f64>,
    layout: Layout,
    t: f64,
}

impl Harness {
    fn new(circuit: Circuit, dev: BehavioralDevice) -> Self {
        let layout = Layout::new(&circuit);
        let n_nodes = circuit.node_count();
        let n_branch = layout.size.saturating_sub(layout.n_nodes);
        Harness {
            circuit,
            dev,
            node_volts: vec![0.0; n_nodes],
            branch_x: vec![0.0; n_branch],
            layout,
            t: 0.0,
        }
    }

    /// Run `n` chunks of `dt`, updating the device each chunk from the previous
    /// solve, then re-solving.
    fn run(&mut self, n: usize, dt: f64) {
        for _ in 0..n {
            let volts = self.node_volts.clone();
            let branch = self.branch_x.clone();
            let layout = self.layout.clone();
            let node_v = |nd: NodeId| volts.get(nd.0 as usize).copied().unwrap_or(0.0);
            let branch_current = |id: DeviceId| -> Option<f64> {
                layout
                    .branch(id)
                    .and_then(|b| branch.get(b.saturating_sub(layout.n_nodes)).copied())
            };
            self.dev
                .update(&mut self.circuit, &node_v, &branch_current, self.t, dt);
            self.solve(dt);
            self.t += dt;
        }
    }

    fn solve(&mut self, dt: f64) {
        let opts = SolverOptions {
            step: StepControl::Fixed { dt: dt.min(1e-5) },
            ..SolverOptions::default()
        };
        let mut final_x: Vec<f64> = Vec::new();
        let res = Transient::new(opts).run_streaming(&self.circuit, dt, |s| {
            final_x.clear();
            final_x.extend_from_slice(s.x);
        });
        if res.is_ok() {
            let n_nodes = self.circuit.node_count();
            self.node_volts.resize(n_nodes, 0.0);
            self.node_volts[0] = 0.0;
            for node in 1..n_nodes {
                self.node_volts[node] = final_x.get(node - 1).copied().unwrap_or(0.0);
            }
            let n_branch = self.layout.size.saturating_sub(self.layout.n_nodes);
            self.branch_x.resize(n_branch, 0.0);
            for b in 0..n_branch {
                self.branch_x[b] = final_x.get(self.layout.n_nodes + b).copied().unwrap_or(0.0);
            }
        }
    }

    fn v(&self, name: &str) -> f64 {
        // Re-intern is safe: node() returns the existing id for a known name.
        let id = self
            .circuit
            .iter()
            .flat_map(|(_, d)| d.nodes())
            .find(|n| self.circuit.node_name(*n) == name);
        id.map(|n| self.node_volts.get(n.0 as usize).copied().unwrap_or(0.0))
            .unwrap_or(f64::NAN)
    }
}

/// Stamp a stiff ideal rail (Vsource to ground) on a node.
fn stamp_rail(c: &mut Circuit, node: NodeId, name: &str, volts: f64) {
    c.add(Device::Vsource {
        name: name.to_string(),
        p: node,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(volts),
    });
}

// ── 1. Internal pull to a rail (nPM1300 SHPHLD shape) ───────────────────────

#[test]
fn internal_pull_drags_floating_pin_to_rail() {
    // A pin with an internal 100k pull to a VSYS rail at 3.7 V, the SHPHLD pin
    // sitting on a net that the MCU GPIO has left high-impedance (modelled here
    // as a weak 10M leak to ground). The pull should drag the pin most of the
    // way to VSYS, demonstrating "VSYS feeds the GPIO in sleep".
    let mut c = Circuit::new();
    let vsys = c.node("VSYS");
    let pin = c.node("SHPHLD_NET");
    stamp_rail(&mut c, vsys, "Vvsys", 3.7);
    // Weak leak to ground = a sleeping GPIO's high-Z input.
    c.add(Device::Resistor {
        name: "Rgpio_leak".into(),
        a: pin,
        b: NodeId::GROUND,
        ohms: 10e6,
        tc1: None,
    });

    let toml = r#"
[pins.shphld]
pull_to = "vsys"
pull_ohms = 100000.0
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("shphld".to_string(), pin);
    roles.insert("vsys".to_string(), vsys);
    let dev = BehavioralDevice::stamp(&mut c, "IC401", &model, &Params::default(), &roles, &|_| None)
        .expect("pull stamps a device");

    let mut h = Harness::new(c, dev);
    h.run(20, 1e-4);
    let v_pin = h.v("SHPHLD_NET");
    // 100k pull vs 10M leak: divider puts the pin at 3.7 * 10M/(10M+100k) ≈ 3.66 V.
    assert!(
        v_pin > 3.5,
        "internal pull should drag the floating SHPHLD net up to ~VSYS, got {v_pin:.3} V"
    );
}

// ── 2. Open-drain asserted by an FSM state ──────────────────────────────────

#[test]
fn fsm_open_drain_pulls_low_in_asserted_state() {
    // A STAT open-drain pin idles high (external 10k to 3.3 V) in state "idle",
    // and is asserted (pulled low) once the FSM transitions to "charging" when
    // an enable pin crosses its threshold.
    let mut c = Circuit::new();
    let v33 = c.node("V33");
    let stat = c.node("STAT");
    let en = c.node("EN");
    stamp_rail(&mut c, v33, "Vv33", 3.3);
    stamp_rail(&mut c, en, "Ven", 3.3); // enable asserted
    c.add(Device::Resistor {
        name: "Rstat_pullup".into(),
        a: stat,
        b: v33,
        ohms: 10_000.0,
        tc1: None,
    });

    let toml = r#"
[pins.stat]
open_drain = true
od_ohms = 50.0

[fsm]
states = ["idle", "charging"]

[[fsm.transitions]]
from = "idle"
to = "charging"
guard = "v_en > 1.5"

[fsm.state_pins.charging.stat]
od_assert = true
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("stat".to_string(), stat);
    roles.insert("en".to_string(), en);
    let dev = BehavioralDevice::stamp(&mut c, "U2", &model, &Params::default(), &roles, &|_| None)
        .expect("fsm + open-drain stamps");

    let mut h = Harness::new(c, dev);
    // The FSM advances on the PREVIOUS chunk's solved voltages, so the EN net
    // is first seen high on the second chunk (the first solve establishes it).
    h.run(2, 1e-4);
    assert_eq!(h.dev.state(), "charging", "EN high should move FSM to charging");
    // After the assert, STAT is pulled low through the 50 ohm OD vs 10k pullup.
    h.run(10, 1e-4);
    let v_stat = h.v("STAT");
    assert!(
        v_stat < 0.2,
        "asserted open-drain STAT should be near ground, got {v_stat:.3} V"
    );
}

#[test]
fn fsm_stays_when_guard_false() {
    // Same FSM but the enable is LOW: the device must remain in "idle" and the
    // open-drain must stay off (STAT high).
    let mut c = Circuit::new();
    let v33 = c.node("V33");
    let stat = c.node("STAT");
    let en = c.node("EN");
    stamp_rail(&mut c, v33, "Vv33", 3.3);
    stamp_rail(&mut c, en, "Ven", 0.0); // enable NOT asserted
    c.add(Device::Resistor {
        name: "Rstat_pullup".into(),
        a: stat,
        b: v33,
        ohms: 10_000.0,
        tc1: None,
    });
    let toml = r#"
[pins.stat]
open_drain = true
od_ohms = 50.0
[fsm]
states = ["idle", "charging"]
[[fsm.transitions]]
from = "idle"
to = "charging"
guard = "v_en > 1.5"
[fsm.state_pins.charging.stat]
od_assert = true
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("stat".to_string(), stat);
    roles.insert("en".to_string(), en);
    let dev =
        BehavioralDevice::stamp(&mut c, "U2", &model, &Params::default(), &roles, &|_| None).unwrap();
    let mut h = Harness::new(c, dev);
    h.run(10, 1e-4);
    assert_eq!(h.dev.state(), "idle");
    assert!(h.v("STAT") > 3.0, "idle STAT should stay high");
}

// ── 3. Expression-law current (LTC6803 balancer-leak shape) ─────────────────

#[test]
fn expression_law_injects_computed_current() {
    // A law that sinks a current from a cell node proportional to its voltage
    // (a leak conductance): i = v_cell / 1000  (a 1k leak), expressed as a law.
    // We verify the node settles where the external source current balances the
    // law-injected sink.
    let mut c = Circuit::new();
    let cell = c.node("CELL");
    // Drive the cell node from 3.3 V through a 100 ohm source resistor.
    let src = c.node("CELL_SRC");
    stamp_rail(&mut c, src, "Vsrc", 3.3);
    c.add(Device::Resistor {
        name: "Rsrc".into(),
        a: src,
        b: cell,
        ohms: 100.0,
        tc1: None,
    });

    // Law: current from cell -> ground equal to v_cell / 1000 (a 1k leak).
    let toml = r#"
[[laws]]
name = "leak"
kind = "current"
a = "cell"
b = "gnd"
expr = "v_cell / 1000.0"
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("cell".to_string(), cell);
    roles.insert("gnd".to_string(), NodeId::GROUND);
    let dev = BehavioralDevice::stamp(&mut c, "U4", &model, &Params::default(), &roles, &|_| None)
        .expect("law stamps a device");

    let mut h = Harness::new(c, dev);
    h.run(40, 1e-4);
    let v_cell = h.v("CELL");
    // Steady state: 3.3 V source, 100 ohm series, 1k leak to ground.
    // The law uses the *previous* chunk's v_cell, so it converges to the
    // fixed point of v = 3.3 * 1000/(1000+100) ≈ 3.0 V.
    assert!(
        (2.7..=3.15).contains(&v_cell),
        "law-driven leak should settle the cell near a 100/1k divider (~3.0 V), got {v_cell:.3} V"
    );
    assert!(v_cell < 3.29, "the leak law must pull the cell BELOW the open 3.3 V");
}

// ── 4. Averaged converter: regulation + limits (LTC4020 ILIMIT shape) ───────

/// Build a buck-boost converter charging a `r_load` from a `vin_rail` brick,
/// with the input-current limit programmed by a sense resistor + a programming
/// resistor read off the "board". Returns the harness after `n` chunks.
fn converter_harness(prog_ohms: f64, r_load: f64) -> Harness {
    let mut c = Circuit::new();
    let pvin = c.node("PVIN");
    let bat = c.node("BAT");
    // Brick: 20 V behind 0.1 ohm.
    let brick = c.node("BRICK");
    stamp_rail(&mut c, brick, "Vbrick", 20.0);
    c.add(Device::Resistor {
        name: "Rbrick".into(),
        a: brick,
        b: pvin,
        ohms: 0.1,
        tc1: None,
    });
    // Battery load: a resistor from BAT to ground (the charge current sink).
    c.add(Device::Resistor {
        name: "Rload".into(),
        a: bat,
        b: NodeId::GROUND,
        ohms: r_load,
        tc1: None,
    });

    // Converter: regulate BAT to 14.4 V, input limit programmed by R8/R49.
    // The threshold scales LINEARLY with the programming resistor up to a
    // full-scale ceiling: v_sense = min(vprog_ref * prog/prog_ref_ohms,
    // v_sense_full), iin = v_sense/rsense. Here vprog_ref = v_sense_full = 0.05,
    // prog_ref_ohms = 400k, rsense = 0.01:
    //   prog = 200k => v_sense = 0.05*0.5 = 0.025 => iin = 2.5 A
    //   prog = 400k => v_sense = 0.05*1.0 = 0.05  => iin = 5.0 A (full scale)
    // i.e. a LARGER programming resistor RAISES the limit — the LTC4020 ILIMIT
    // direction the concrete model uses (100k over budget, 7.15k at budget).
    let toml = format!(
        r#"
[converter]
topology = "buck_boost"
out_pin = "bat"
in_pin = "pvin"
vout_setpoint = 14.4
efficiency = 0.9

[converter.iin_program]
rsense_ohms = 0.01
prog_ref = "R8"
vprog_ref = 0.05
prog_ref_ohms = 400000.0
v_sense_full = 0.05
"#
    );
    let model: Behavioral = toml::from_str(&toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("pvin".to_string(), pvin);
    roles.insert("bat".to_string(), bat);
    let board_r = move |r: &str| if r == "R8" { Some(prog_ohms) } else { None };
    let dev =
        BehavioralDevice::stamp(&mut c, "U2", &model, &Params::default(), &roles, &board_r).unwrap();
    let mut h = Harness::new(c, dev);
    h.run(60, 5e-4);
    h
}

#[test]
fn converter_regulates_output_under_light_load() {
    // A light load (high resistance): the converter holds 14.4 V, input draw is
    // well under the limit.
    let h = converter_harness(400_000.0, 100.0); // prog=400k => full-scale limit 5 A
    let v_bat = h.v("BAT");
    assert!(
        (14.0..=14.6).contains(&v_bat),
        "converter should regulate BAT to ~14.4 V under light load, got {v_bat:.3} V"
    );
    let iin = h.dev.converter_iin().unwrap();
    assert!(iin < 5.0, "light-load input draw should be under the 5 A limit, got {iin:.3} A");
}

#[test]
fn converter_input_limit_caps_the_draw() {
    // A heavy load (low resistance) demands more than the input limit allows:
    // the converter throttles so the input draw is held at the programmed limit.
    let h = converter_harness(400_000.0, 2.0); // prog=400k => 5 A limit, heavy load
    let iin = h.dev.converter_iin().unwrap();
    let limit = h.dev.converter_iin_limit().unwrap();
    assert!((limit - 5.0).abs() < 0.5, "programmed limit should be ~5 A, got {limit:.3}");
    assert!(
        iin <= limit + 0.2,
        "input draw {iin:.3} A must be held at/under the programmed limit {limit:.3} A"
    );
    // And it should actually be PULLING near the limit (the load wants more).
    assert!(iin > limit * 0.7, "heavy load should drive the input draw up to the limit, got {iin:.3}");
}

#[test]
fn program_resistor_changes_the_limit_with_no_model_edit() {
    // The load-bearing physics: a different on-board programming resistor
    // changes the input-current limit, read off the board at bind time, with no
    // model edit. With prog_ref_ohms = 400k: prog = 400k => 5 A (full scale);
    // prog = 200k => 2.5 A (half). A LARGER programming resistor RAISES the
    // limit — exactly the LTC4020 ILIMIT direction (drop R8 to lower the limit).
    let lim_400k = converter_harness(400_000.0, 2.0).dev.converter_iin_limit().unwrap();
    let lim_200k = converter_harness(200_000.0, 2.0).dev.converter_iin_limit().unwrap();
    assert!((lim_400k - 5.0).abs() < 0.3, "prog=400k => ~5 A, got {lim_400k:.3}");
    assert!((lim_200k - 2.5).abs() < 0.3, "prog=200k => ~2.5 A, got {lim_200k:.3}");
    assert!(lim_200k < lim_400k, "a smaller programming resistor lowers the limit");
}
