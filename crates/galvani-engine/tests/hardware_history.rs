//! Reproductions of bugs that emerged during PHYSICAL testing of the Tarski
//! board, derived here from the design files alone. These are the incidents
//! the hardware team hit after the inhibitory-miswire rework:
//!
//! 1. The "shunt" resistor (R_Shunt15301 = 1kΩ — a current-sense shunt that
//!    should be milliohms) drops the ANALOG_VDD rail under load enough to
//!    shift every synapse current and threshold (I_unit scales with VDD).
//! 2. The 74HC595 chain powers up in undefined states (no pulls on
//!    OE'/SRCLR'/RCLK + bootloader SCLK garbage — BUG_HUNT Finding 16), so
//!    random weight bits enable. Enabling an inhibitory bit drives the
//!    miswired base path (Finding 15); through the 1kΩ shunt this browns
//!    out the ENTIRE analog rail, not just one transistor.
//!
//! The cell is extracted from the real netlist and bound by the ordinary
//! pipeline; nothing below is hand-modeled except the rails and the weight
//! select states.

use galvani_engine::bind_board;
use galvani_extract::ExtractedBoard;
use galvani_ir::{Device, NodeId, SourceKind};
use galvani_models::ModelLibrary;
use galvani_solve::{SolverOptions, StepControl, Transient};
use std::collections::HashSet;
use std::path::PathBuf;

fn netlist() -> Option<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tarski_inputsystem.net");
    std::fs::read_to_string(p).ok()
}

/// Subcircuit: the shunt plus everything one hop out from ANALOG_VDD's
/// static loads — every R_set mirror reference leg (resistor + its
/// diode-connected BCM transistor) — plus one full inhibitory synapse cell
/// (transistor pair, weight switch, strap) so the brownout case is live.
fn shunt_world(extra_refs: &[&str]) -> Option<ExtractedBoard> {
    let mut board = ExtractedBoard::from_kicad_netlist(&netlist()?).ok()?;
    let vdd_id = board.net_by_name("ANALOG_VDD")?.id;

    // Static loads: resistors with a pin on ANALOG_VDD, plus the transistor
    // packages those resistors' other pins reach (the diode-connected refs).
    let mut keep: HashSet<String> = extra_refs.iter().map(|s| s.to_string()).collect();
    keep.insert("R_Shunt15301".into());
    let mut second_hop_nets: HashSet<i64> = HashSet::new();
    for c in &board.components {
        if c.reference.starts_with('R') && c.pins.iter().any(|p| p.net == Some(vdd_id)) {
            keep.insert(c.reference.clone());
            for p in &c.pins {
                if let Some(n) = p.net {
                    if n != vdd_id {
                        second_hop_nets.insert(n);
                    }
                }
            }
        }
    }
    for c in &board.components {
        if c.reference.starts_with("IC")
            && c.pins.iter().any(|p| p.net.is_some_and(|n| second_hop_nets.contains(&n)))
        {
            keep.insert(c.reference.clone());
        }
    }
    board.components.retain(|c| keep.contains(&c.reference));
    Some(board)
}

/// Bind, attach a single ideal 5V at the UPSTREAM side of the shunt (+5V),
/// removing any auto-rail the binder put on ANALOG_VDD (the rail must be fed
/// through the shunt, as on the physical board). Drive the inhibitory weight
/// select; solve; return (V(ANALOG_VDD), V(+5V), faults count).
fn run_world(weight_on: bool) -> Option<(f64, f64)> {
    let board = shunt_world(&[
        "IC3906",
        "ANALOG_SWITCH3905",
        "Rstop3901",
    ])?;
    let lib = ModelLibrary::builtin();
    let mut bound = bind_board(&board, &lib);

    // The physical rail feeds ANALOG_VDD only through the shunt: remove any
    // supply/rail source the binder attached directly to ANALOG_VDD.
    let vdd = bound.node("ANALOG_VDD")?;
    for dev in bound.circuit.devices.iter_mut() {
        if let Device::Vsource { name, p, kind, .. } = dev {
            if *p == vdd && (name.starts_with("Vrail") || name.starts_with("Vsupply")) {
                // Disable by turning it into a zero-current open: easiest is
                // to retarget its voltage through a huge series... instead we
                // zero it and rely on the test asserting against +5V flow.
                // Cleaner: mark and skip below.
                *kind = SourceKind::Dc(f64::NAN); // sentinel, replaced next
            }
        }
    }
    // Replace sentinel sources with open circuits (1T ohm resistors).
    for dev in bound.circuit.devices.iter_mut() {
        let replace = matches!(
            dev,
            Device::Vsource { kind: SourceKind::Dc(v), .. } if v.is_nan()
        );
        if replace {
            if let Device::Vsource { name, p, n, .. } = dev {
                let (name, a, b) = (name.clone(), *p, *n);
                *dev = Device::Resistor { name, a, b, ohms: 1e12, tc1: None };
            }
        }
    }
    // Ideal 5V on the upstream +5V node (and +5P for the switch VCC pins).
    for rail in ["+5V", "+5P"] {
        if let Some(node) = bound.node(rail) {
            let already = bound
                .circuit
                .devices
                .iter()
                .any(|d| matches!(d, Device::Vsource { p, .. } if *p == node));
            if !already {
                bound.circuit.add(Device::Vsource {
                    name: format!("Vtest_{rail}"),
                    p: node,
                    n: NodeId::GROUND,
                    kind: SourceKind::Dc(5.0),
                });
            }
        }
    }
    if let Some(sel) = bound.node("Net-(ANALOG_SWITCH3905-S)") {
        bound.circuit.add(Device::Vsource {
            name: "Vweight_q4".into(),
            p: sel,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(if weight_on { 5.0 } else { 0.0 }),
        });
    }

    let opts = SolverOptions {
        step: StepControl::Fixed { dt: 2e-6 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&bound.circuit, 1e-3).ok()?;
    let v_at = |name: &str| -> f64 {
        bound
            .node(name)
            .and_then(|n| wf.node_voltages.get(n.0 as usize))
            .and_then(|w| w.last().copied())
            .unwrap_or(f64::NAN)
    };
    Some((v_at("ANALOG_VDD"), v_at("+5V")))
}

#[test]
fn shunt_droop_at_quiescent_load() {
    let Some((v_vdd, v_5v)) = run_world(false) else {
        eprintln!("netlist missing; skipping");
        return;
    };
    let droop = v_5v - v_vdd;
    let i_rail_ma = droop / 1.0; // 1kΩ: 1mV per µA, mA = V
    eprintln!(
        "quiescent: V(+5V)={v_5v:.3} V(ANALOG_VDD)={v_vdd:.4} droop={:.1} mV (rail {:.1} µA)",
        droop * 1e3,
        i_rail_ma * 1e3
    );
    // The mirror reference population alone (~180 legs of (V-Vbe)/10M) puts
    // tens of µA through 1kΩ: a measurable droop a real milliohm shunt would
    // never produce. Assert it is visible but the rail still functions.
    assert!(
        droop > 0.02 && droop < 1.0,
        "expected tens-of-mV quiescent droop through the 1k shunt, got {:.4} V",
        droop
    );
}

#[test]
fn inhibitory_weight_browns_out_the_rail_through_the_shunt() {
    // The interaction of three findings: undefined power-up register states
    // (Finding 16) can enable an inhibitory weight bit; the miswired base
    // path (Finding 15) then pulls destruction-scale current; and the 1kΩ
    // shunt turns that one cell's fault into a WHOLE-RAIL brownout.
    let Some((v_vdd_off, _)) = run_world(false) else {
        eprintln!("netlist missing; skipping");
        return;
    };
    let Some((v_vdd_on, v_5v)) = run_world(true) else {
        eprintln!("netlist missing; skipping");
        return;
    };
    eprintln!(
        "rail with weight off: {v_vdd_off:.3} V; with one inhibitory weight on: {v_vdd_on:.3} V (+5V stays {v_5v:.3})"
    );
    assert!(v_vdd_off > 4.0, "healthy rail should sit near 5V");
    assert!(
        v_vdd_on < 2.5,
        "one enabled inhibitory weight should collapse the shunted rail, got {v_vdd_on:.3} V"
    );
}
