//! Card-driven electrical behaviour in the live scheduler: pin output
//! resistance per drive-strength setting, and the reset a part holds itself in
//! while its reset pin is low or its supply is browned out.

use super::*;
use hauksbee_bind::mcu_electrical::{DriveRow, McuElectrical};
use hauksbee_ir::SourceKind;
use hauksbee_models::{DriveSelect, OutputDrive, OutputLevel, OutputPoint, ResetSpec};

const GND: NodeId = NodeId::GROUND;

fn add_vsource(c: &mut Circuit, name: &str, p: NodeId, kind: SourceKind) -> DeviceId {
    c.add(Device::Vsource {
        name: name.into(),
        p,
        n: GND,
        kind,
    })
}

fn add_res(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, ohms: f64) {
    c.add(Device::Resistor {
        name: name.into(),
        a,
        b,
        ohms,
        tc1: None,
    });
}

/// The RP2040 3.3 V output stage as the card states it: `ma` mA at VOH 2.62 V /
/// VOL 0.5 V.
fn rp2040_row(selector: u32, ma: f64) -> DriveRow {
    DriveRow::from_card(&OutputDrive {
        setting: format!("{ma} mA"),
        selector: Some(selector),
        vdd: 3.3,
        points: vec![
            OutputPoint {
                level: OutputLevel::High,
                current_a: ma / 1000.0,
                volts: 2.62,
            },
            OutputPoint {
                level: OutputLevel::Low,
                current_a: ma / 1000.0,
                volts: 0.5,
            },
        ],
        ..OutputDrive::default()
    })
}

const PAD_BASE: u32 = 0x4001_c004;

fn pads_drive_select() -> DriveSelect {
    DriveSelect {
        port_bases: std::collections::BTreeMap::from([("A".to_string(), PAD_BASE)]),
        register_stride: 4,
        pins_per_register: 1,
        lsb: 4,
        width: 2,
        basis: "test".into(),
        ..DriveSelect::default()
    }
}

/// A scheduler on `circuit` (nets by name) with one mock MCU `U1` whose GPIO
/// drivers sit on `pins`, carrying `electrical` and `roles`.
fn sched_with(
    mut circuit: Circuit,
    nets: &[(&str, NodeId)],
    pins: &[((char, u8), &str)],
    roles: &[(&str, &str)],
    electrical: McuElectrical,
) -> (Scheduler, MockCore) {
    let net_nodes: HashMap<String, NodeId> =
        nets.iter().map(|&(n, id)| (n.to_string(), id)).collect();
    // A resistor to ground on every net keeps the matrix well posed.
    for &(name, node) in nets {
        add_res(&mut circuit, &format!("Rleak_{name}"), node, GND, 1e9);
    }
    let bound = bound_board(
        "electrics",
        circuit,
        net_nodes.clone(),
        Vec::new(),
        Vec::new(),
    );
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
    let nodes: Vec<_> = pins
        .iter()
        .map(|&(pin, net)| (pin, net_nodes[net]))
        .collect();
    let drivers = tristated_drivers(&mut sched, &nodes);
    let core = MockCore {
        count_cycles: true,
        ..Default::default()
    };
    let handles = core.handles();
    let mut b = binding("U1", "renode:test", drivers);
    b.role_nets = roles
        .iter()
        .map(|&(role, net)| (role.to_string(), net_nodes[net]))
        .collect();
    b.electrical = electrical;
    push_core(&mut sched, core, b);
    sched.relayout();
    (sched, handles)
}

fn fire_edge(sched: &Scheduler, pin: (char, u8), level: bool) {
    preload_edges(sched, pin, &[(10, level)]);
}

#[test]
fn drive_strength_follows_the_setting_the_firmware_programmed() {
    let mut c = Circuit::new();
    let led = c.node("LED");
    add_res(&mut c, "Rload", led, GND, 100.0);
    let electrical = McuElectrical {
        drive: vec![rp2040_row(0, 2.0), rp2040_row(1, 4.0), rp2040_row(3, 12.0)],
        drive_select: Some(pads_drive_select()),
        reset: None,
        ..McuElectrical::default()
    };
    let (mut sched, h) = sched_with(c, &[("LED", led)], &[(('A', 0), "LED")], &[], electrical);
    let high = sched.mcus[0].logic_high_v;
    let volts_for = |ohms: f64| high * 100.0 / (100.0 + ohms);

    // The firmware drives the pin before touching DRIVE: the reset default is
    // not in this table (no row is flagged), so the generic stage applies until
    // the register is read. Programme 2 mA, then drive high.
    h.registers.lock_or_recover().insert(PAD_BASE, 0);
    fire_edge(&sched, ('A', 0), true);
    sched.step(1e-3);
    let weak = sched.net_voltage("LED").expect("LED solved");
    let r_weak = 340.0;
    assert!(
        (weak - volts_for(r_weak)).abs() < 0.02,
        "2 mA setting: expected {:.3} V, got {weak:.3} V",
        volts_for(r_weak)
    );

    // Raise it to 12 mA with no pin edge: the periodic sweep finds it.
    h.registers.lock_or_recover().insert(PAD_BASE, 3 << 4);
    for _ in 0..10 {
        sched.step(1e-3);
    }
    let strong = sched.net_voltage("LED").expect("LED solved");
    let r_strong = 170.0 / 3.0;
    assert!(
        (strong - volts_for(r_strong)).abs() < 0.02,
        "12 mA setting: expected {:.3} V, got {strong:.3} V",
        volts_for(r_strong)
    );
    assert!(
        strong > weak + 0.5,
        "the stronger setting must pull the loaded pin visibly higher ({weak:.3} V -> {strong:.3} V)"
    );

    // Low level uses the low-side resistance: a pull-up load sees VOL rise
    // with the weaker setting.
    fire_edge(&sched, ('A', 0), false);
    sched.step(1e-3);
    let low = sched.net_voltage("LED").expect("LED solved");
    assert!(
        low < 0.05,
        "driving low into a load to ground is ~0 V, got {low}"
    );
}

#[test]
fn an_open_drain_setting_floats_the_high_level() {
    let mut c = Circuit::new();
    let bus = c.node("BUS");
    let rail = c.node("RAIL");
    add_vsource(&mut c, "Vrail", rail, SourceKind::Dc(3.3));
    add_res(&mut c, "Rpull", rail, bus, 10_000.0);
    let mut open_drain = OutputDrive {
        setting: "S0D1".into(),
        selector: Some(6),
        vdd: 3.0,
        points: vec![OutputPoint {
            level: OutputLevel::Low,
            current_a: 0.001,
            volts: 0.4,
        }],
        disconnected: vec![OutputLevel::High],
        ..OutputDrive::default()
    };
    open_drain.reset_default = true;
    let electrical = McuElectrical {
        drive: vec![DriveRow::from_card(&open_drain)],
        drive_select: None,
        reset: None,
        ..McuElectrical::default()
    };
    let (mut sched, _h) = sched_with(
        c,
        &[("BUS", bus), ("RAIL", rail)],
        &[(('A', 0), "BUS")],
        &[],
        electrical,
    );
    fire_edge(&sched, ('A', 0), true);
    sched.step(1e-3);
    let high = sched.net_voltage("BUS").expect("BUS solved");
    assert!(
        high > 3.0,
        "an open-drain high is released to the pull-up, got {high} V"
    );
    fire_edge(&sched, ('A', 0), false);
    sched.step(1e-3);
    let low = sched.net_voltage("BUS").expect("BUS solved");
    // 400 ohm (0.4 V at 1 mA) against the 10 k pull-up.
    assert!(
        (low - 3.3 * 400.0 / 10_400.0).abs() < 0.02,
        "an open-drain low pulls the bus down through the card's 400 ohm, got {low} V"
    );
}

fn f103_reset() -> ResetSpec {
    ResetSpec {
        supply_pin: Some("vdd".into()),
        assert_below_v: Some(1.88),
        release_above_v: Some(1.92),
        release_delay_s: Some(0.0025),
        pin_vil_v: Some(0.8),
        pin_vih_v: Some(2.0),
        basis: "test".into(),
    }
}

/// Step `n` 1 ms chunks, setting the DC source `src` to `script(t)` before each
/// (a board source that moves with the run: a supply dip, a reset button), and
/// return each chunk's `run_micros` argument.
fn run_scripted(
    sched: &mut Scheduler,
    h: &MockCore,
    n: usize,
    src: DeviceId,
    script: impl Fn(f64) -> f64,
) -> Vec<u64> {
    let start = h.micros.lock_or_recover().len();
    for _ in 0..n {
        let v = script(sched.sim_time);
        if let Some(Device::Vsource { kind, .. }) = sched.circuit.devices.get_mut(src.0 as usize) {
            *kind = SourceKind::Dc(v);
        }
        sched.step(1e-3);
    }
    h.micros.lock_or_recover()[start..].to_vec()
}

#[test]
fn a_supply_dip_below_the_brownout_threshold_resets_the_core() {
    let mut c = Circuit::new();
    let vdd = c.node("VDD");
    let src = add_vsource(&mut c, "Vdd", vdd, SourceKind::Dc(3.3));
    let electrical = McuElectrical {
        reset: Some(f103_reset()),
        ..McuElectrical::default()
    };
    let (mut sched, h) = sched_with(c, &[("VDD", vdd)], &[], &[("vdd", "VDD")], electrical);
    assert!(
        sched.chunk_s <= 1.0e-3 + 1e-12,
        "the test steps 1 ms chunks"
    );
    // 3.3 V, dipping to 1.0 V from 3.5 ms to 7.5 ms.
    let runs = run_scripted(&mut sched, &h, 20, src, |t| {
        if (0.0035..0.0075).contains(&t) {
            1.0
        } else {
            3.3
        }
    });

    let episodes = sched.reset_episodes();
    assert_eq!(episodes.len(), 1, "{episodes:?}");
    let (mcu, ep) = &episodes[0];
    assert_eq!(mcu, "U1");
    assert_eq!(ep.cause, ResetCause::BrownOut);
    assert!(ep.low_v < 1.88, "{ep:?}");
    let end = ep.end_s.expect("released");
    assert!(
        ep.start_s >= 0.0035 && ep.start_s <= 0.006,
        "starts at the dip: {ep:?}"
    );
    // The supply is back at 7.6 ms and the part stays in reset for the
    // 2.5 ms power-on temporization on top of that.
    assert!(
        end >= 0.0075 + 0.0025 - 1e-9,
        "temporization honoured: {ep:?}"
    );
    assert!(ep.rebooted, "the core is rebooted on release: {ep:?}");
    assert!(*h.was_reset.lock().unwrap(), "Mcu::reset was called");

    let held_chunks = runs.iter().filter(|&&us| us == 0).count();
    assert!(
        held_chunks >= 5,
        "the core does not advance while held ({held_chunks} held chunks of {runs:?})"
    );
    assert!(runs.iter().take(3).all(|&us| us > 0), "runs before the dip");
    assert!(
        runs.last().is_some_and(|&us| us > 0),
        "runs again after release"
    );

    let messages = coverage_limitation_messages(&sched);
    assert!(
        messages
            .iter()
            .any(|m| m.contains("U1") && m.contains("held in reset") && m.contains("brown-out")),
        "{messages:?}"
    );
}

#[test]
fn a_slow_rc_on_the_reset_pin_holds_the_core_until_it_rises_past_vih() {
    let mut c = Circuit::new();
    let src = c.node("SRC");
    let nrst = c.node("NRST");
    // The button pulls the line low at 5 ms and lets go at 8 ms; the 10 k and
    // 100 nF on the pin (tau = 1 ms) stretch both edges.
    let btn = add_vsource(&mut c, "Vbtn", src, SourceKind::Dc(3.3));
    add_res(&mut c, "Rrc", src, nrst, 10_000.0);
    c.add(Device::Capacitor {
        name: "Crc".into(),
        a: nrst,
        b: GND,
        farads: 100e-9,
        ic: None,
    });
    let electrical = McuElectrical {
        reset: Some(f103_reset()),
        ..McuElectrical::default()
    };
    let (mut sched, h) = sched_with(
        c,
        &[("SRC", src), ("NRST", nrst)],
        &[],
        &[("reset", "NRST")],
        electrical,
    );
    let runs = run_scripted(&mut sched, &h, 16, btn, |t| {
        if (0.005..0.008).contains(&t) {
            0.0
        } else {
            3.3
        }
    });

    let episodes = sched.reset_episodes();
    assert_eq!(episodes.len(), 1, "{episodes:?}");
    let ep = &episodes[0].1;
    assert_eq!(ep.cause, ResetCause::ResetPin);
    assert!(ep.low_v < 0.8, "{ep:?}");
    assert!(
        ep.start_s > 0.0051 && ep.start_s < 0.009,
        "asserts once the RC has fallen below VIL, not at the button: {ep:?}"
    );
    let end = ep.end_s.expect("released");
    assert!(
        end > 0.0085,
        "releases only after the RC has climbed past VIH, later than the button: {ep:?}"
    );
    assert!(ep.rebooted && *h.was_reset.lock().unwrap());
    assert!(runs.contains(&0), "{runs:?}");
}

#[test]
fn a_board_inside_every_threshold_is_never_held() {
    let mut c = Circuit::new();
    let vdd = c.node("VDD");
    let nrst = c.node("NRST");
    let src = add_vsource(&mut c, "Vdd", vdd, SourceKind::Dc(3.3));
    add_res(&mut c, "Rup", vdd, nrst, 10_000.0);
    let electrical = McuElectrical {
        reset: Some(f103_reset()),
        ..McuElectrical::default()
    };
    let (mut sched, h) = sched_with(
        c,
        &[("VDD", vdd), ("NRST", nrst)],
        &[],
        &[("vdd", "VDD"), ("reset", "NRST")],
        electrical,
    );
    let runs = run_scripted(&mut sched, &h, 10, src, |_| 3.3);
    assert!(sched.reset_episodes().is_empty());
    assert!(runs.iter().all(|&us| us > 0), "{runs:?}");
    assert!(!*h.was_reset.lock().unwrap());
}

#[test]
fn a_part_whose_card_states_no_reset_is_never_held() {
    let mut c = Circuit::new();
    let vdd = c.node("VDD");
    let src = add_vsource(&mut c, "Vdd", vdd, SourceKind::Dc(0.0));
    let (mut sched, h) = sched_with(
        c,
        &[("VDD", vdd)],
        &[],
        &[("vdd", "VDD")],
        McuElectrical::default(),
    );
    let runs = run_scripted(&mut sched, &h, 5, src, |_| 0.0);
    assert!(sched.reset_episodes().is_empty());
    assert!(runs.iter().all(|&us| us > 0), "{runs:?}");
}
