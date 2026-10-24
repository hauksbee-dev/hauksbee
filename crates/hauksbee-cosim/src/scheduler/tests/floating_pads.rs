//! A pad on no net, or on a net nothing else touches, is a floating node. It
//! must read as a physical level, never as `I/gmin` volts, and no solved node
//! may leave what the board's sources can produce.

use super::*;
use hauksbee_ir::SourceKind;

const GND: NodeId = NodeId::GROUND;

fn res(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, ohms: f64) {
    c.add(Device::Resistor {
        name: name.into(),
        a,
        b,
        ohms,
        tc1: None,
    });
}

/// A module placed with its GPIO0 pad driving an LED load, but with the supply
/// pad (`3V3`) on a net nothing else touches (powered through another pad the
/// board never draws), and GPIO1 on a net with nothing else.
fn float_sched() -> (Scheduler, MockCore) {
    let mut c = Circuit::new();
    let led = c.node("LED");
    let float_vdd = c.node("UNCONNECTED_3V3");
    let float_pad = c.node("UNCONNECTED_GPIO1");
    let vin = c.node("VIN");
    c.add(Device::Vsource {
        name: "Vin".into(),
        p: vin,
        n: GND,
        kind: SourceKind::Dc(5.0),
    });
    res(&mut c, "Rin", vin, GND, 10_000.0);
    res(&mut c, "Rload", led, GND, 1_000.0);
    let nets = [
        ("LED", led),
        ("UNCONNECTED_3V3", float_vdd),
        ("UNCONNECTED_GPIO1", float_pad),
        ("VIN", vin),
    ];
    let net_nodes: HashMap<String, NodeId> =
        nets.iter().map(|&(n, id)| (n.to_string(), id)).collect();
    let bound = bound_board("floatpad", c, net_nodes, Vec::new(), Vec::new());
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
    let mut drivers = HashMap::new();
    for (pin, node, tag) in [(('A', 0), led, "G0"), (('A', 1), float_pad, "G1")] {
        let mut drv = hauksbee_bind::drivers::PinDriver::stamp(
            &mut sched.circuit,
            node,
            tag,
            tag,
            hauksbee_bind::drivers::DEFAULT_RO,
        );
        drv.route_supply(&mut sched.circuit, tag, float_vdd, GND);
        sched
            .driver_draws
            .push((drv.vsource, drv.supply.expect("routed")));
        drivers.insert(pin, drv);
    }
    sched
        .driver_draws
        .sort_unstable_by_key(|(source, _)| *source);
    let core = MockCore {
        count_cycles: true,
        ..Default::default()
    };
    let handles = core.handles();
    let mut b = binding("U1", "renode:test", drivers);
    b.role_nets = HashMap::from([("3v3".to_string(), float_vdd)]);
    push_core(&mut sched, core, b);
    sched.relayout();
    (sched, handles)
}

/// Run the board with GPIO0 driven high into the LED load.
fn run_driven() -> Scheduler {
    let (mut sched, _h) = float_sched();
    preload_edges(&sched, ('A', 0), &[(10, true)]);
    for _ in 0..20 {
        sched.step(1e-3);
    }
    sched
}

#[test]
fn a_supply_pad_nothing_holds_is_clamped_not_solved_at_i_over_gmin() {
    // Measured before the fix: UNCONNECTED_3V3 solved at -3.143e9 V (the
    // 3.14 mA the GPIO0 driver draws, over gmin) and every chunk was refused.
    let sched = run_driven();
    assert!(
        sched.analog_valid(),
        "the board solves: {:?}",
        sched.failed_window_reasons()
    );
    let led = sched.stats["LED"].max_v;
    assert!(
        (led - 3.1429).abs() < 0.01,
        "the driven pad is unaffected: {led}"
    );
    for net in ["UNCONNECTED_3V3", "UNCONNECTED_GPIO1"] {
        let st = &sched.stats[net];
        assert!(
            st.min_v >= -1.0 && st.max_v <= 6.0,
            "{net} read {:.3} V .. {:.3} V, outside the 0 V .. 5 V the sources span by a volt",
            st.min_v,
            st.max_v
        );
    }
    // The supply pin is pulled one diode drop below ground by the pad's ESD
    // path (the driver's draw has to come from somewhere), not floated away.
    let supply = sched.stats["UNCONNECTED_3V3"].min_v;
    assert!(
        (-0.9..=-0.4).contains(&supply),
        "clamped one diode drop below ground, got {supply} V"
    );
}

#[test]
fn a_node_outside_the_source_envelope_is_a_solver_fault_not_a_reading() {
    let mut sched = run_driven();
    let led = sched.net_nodes["LED"];
    let (min_before, max_before) = (sched.stats["LED"].min_v, sched.stats["LED"].max_v);
    // The sources span 0 V .. 5 V (the 5 V input and the driver's 0..3.3 V),
    // so the envelope is -1 V .. 6 V. Just inside is a reading; just outside is
    // a fault, at both ends.
    for (volts, fault) in [(5.99, false), (-0.99, false), (6.01, true), (-1.01, true)] {
        sched.node_volts[led.0 as usize] = volts;
        let reason = sched.update_stats();
        assert_eq!(reason.is_some(), fault, "{volts} V: {reason:?}");
        if let Some(reason) = reason {
            assert!(
                reason.contains("solver fault") && reason.contains("LED"),
                "{reason}"
            );
        }
    }
    sched.node_volts[led.0 as usize] = 11_000.0;
    let reason = sched.update_stats().expect("11 kV is a solver fault");
    assert!(reason.contains("11000"), "{reason}");
    // None of the rejected levels reached the statistics the activity summary
    // is built from.
    let st = &sched.stats["LED"];
    assert!(
        st.max_v <= 5.99 + 1e-9 && st.max_v >= max_before - 1e-9,
        "{}",
        st.max_v
    );
    assert!(
        st.min_v >= -0.99 - 1e-9 && st.min_v <= min_before + 1e-9,
        "{}",
        st.min_v
    );
}

#[test]
fn a_converged_state_outside_the_envelope_is_refused_by_name() {
    let sched = run_driven();
    let led = sched.net_nodes["LED"];
    let mut x = vec![0.0; sched.layout.size];
    let unknown = led.0 as usize - 1;
    x[unknown] = 11_000.0;
    let (hit, volts) = sched.insane_node_state(&x).expect("refused");
    assert_eq!((hit, volts.to_bits()), (unknown, 11_000.0f64.to_bits()));
    let reason = sched.insane_state_reason(hit, volts);
    assert!(
        reason.starts_with("solver fault: ") && reason.contains("LED") && reason.contains("11000"),
        "{reason}"
    );
    x[unknown] = 3.3;
    assert_eq!(sched.insane_node_state(&x), None);
}

#[test]
fn a_forced_level_is_the_callers_own_not_a_solver_fault() {
    let mut sched = run_driven();
    assert!(sched.force_net_voltage("LED", 12.0));
    assert!(sched.update_stats().is_none());
}
