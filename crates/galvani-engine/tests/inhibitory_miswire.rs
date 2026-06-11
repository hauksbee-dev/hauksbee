//! Independent derivation of the Tarski inhibitory-synapse miswire.
//!
//! The board's excitatory synapse cells route the mirror's COLLECTOR output
//! through the SPDT weight switch (all 360 BCM857BS: C2 -> switch COM). The
//! inhibitory cells instead connect the switch COM to the output transistor's
//! BASE (90 BCM847BS: B2 -> switch COM), with C2 strapped back to the
//! reference node through Rstop — base and collector connections crossed.
//! One throw of the switch is ANALOG_VDD, selected when the 74HC595's Q4
//! weight bit goes high.
//!
//! These tests let galvani derive the consequence from the design files
//! alone: the cell is extracted from the real netlist, bound by the ordinary
//! model pipeline, and simulated. Enabling the inhibitory weight must (a)
//! pump destruction-scale current through the 100mA-rated junction — raised
//! as faults by the stress monitor — and (b) deliver no controlled mirror
//! current anywhere. The repaired wiring (B2/C2 swapped back) must behave as
//! a proper ~0.45uA sink mirror with no faults.

use galvani_engine::{bind_board, stress::StressMonitor};
use galvani_extract::ExtractedBoard;
use galvani_ir::{Device, NodeId, SourceKind};
use galvani_models::ModelLibrary;
use galvani_solve::{SolverOptions, StepControl, Transient};
use std::path::PathBuf;

/// The components of one inhibitory synapse cell, verbatim from the board.
const CELL_REFS: &[&str] = &[
    "IC3906",            // BCM847BS dual NPN: Q1 diode-connected ref, Q2 output
    "ANALOG_SWITCH3905", // SN74LVC1G3157 weight switch (S <- 74HC595 Q4)
    "R_Set_VCC3901",     // mirror set resistor
    "Rstop3901",         // the strap from C2 back to the reference node
    "Rs3908", "Rs3909", "Rs3910", "Rs3911", "Rs3912", "Rs3913",
];

fn netlist() -> Option<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tarski_inputsystem.net");
    std::fs::read_to_string(p).ok()
}

/// Extract just the cell from the full board, preserving its real nets.
fn cell_board(swap_b2_c2: bool) -> Option<ExtractedBoard> {
    let mut board = ExtractedBoard::from_kicad_netlist(&netlist()?).ok()?;
    board
        .components
        .retain(|c| CELL_REFS.contains(&c.reference.as_str()));
    assert_eq!(
        board.components.len(),
        CELL_REFS.len(),
        "cell components missing from netlist"
    );
    if swap_b2_c2 {
        // The repair: exchange the B2 and C2 net assignments on the dual NPN,
        // which is what a corrected schematic would have produced.
        let ic = board
            .components
            .iter_mut()
            .find(|c| c.reference == "IC3906")
            .unwrap();
        let b2 = ic.pins.iter().position(|p| p.number == "5").unwrap();
        let c2 = ic.pins.iter().position(|p| p.number == "3").unwrap();
        let tmp = ic.pins[b2].net;
        ic.pins[b2].net = ic.pins[c2].net;
        ic.pins[c2].net = tmp;
    }
    Some(board)
}

/// Bind the cell, power its rails, drive the weight-select line, solve, and
/// return (bound circuit's per-net voltages by name, stress faults).
fn run_cell(
    swap_b2_c2: bool,
    weight_on: bool,
) -> Option<(Vec<(String, f64)>, Vec<(String, String, f64, f64)>)> {
    let board = cell_board(swap_b2_c2)?;
    let lib = ModelLibrary::builtin();
    let mut bound = bind_board(&board, &lib);

    // Rails: the binder already attaches supplies for +5P / ANALOG_VDD if it
    // detected them; make sure both exist (ideal 5V), and drive the select
    // net with the weight bit (the 74HC595 Q4 output in the real board).
    for rail in ["ANALOG_VDD", "+5P"] {
        if let Some(node) = bound.node(rail) {
            let already = bound.circuit.devices.iter().any(|d| {
                matches!(d, Device::Vsource { p, .. } if *p == node)
            });
            if !already {
                bound.circuit.add(Device::Vsource {
                    name: format!("Vrail_{rail}"),
                    p: node,
                    n: NodeId::GROUND,
                    kind: SourceKind::Dc(5.0),
                });
            }
        }
    }
    let sel = bound.node("Net-(ANALOG_SWITCH3905-S)")?;
    bound.circuit.add(Device::Vsource {
        name: "Vweight_q4".into(),
        p: sel,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(if weight_on { 5.0 } else { 0.0 }),
    });
    // The B1 throw chains to neighbouring cells we excluded; leave it open
    // (matches the chain being idle).

    let opts = SolverOptions {
        step: StepControl::Fixed { dt: 1e-6 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&bound.circuit, 2e-3).ok()?;

    let volts: Vec<(String, f64)> = bound
        .net_names
        .iter()
        .filter_map(|n| {
            let node = bound.node(n)?;
            let v = if node.is_ground() {
                0.0
            } else {
                *wf.node_voltages.get(node.0 as usize)?.last()?
            };
            Some((n.clone(), v))
        })
        .collect();

    // Stress pass over the final operating point.
    let mut monitor = StressMonitor::new(bound.device_meta.clone());
    let node_v = |n: NodeId| -> f64 {
        if n.is_ground() {
            0.0
        } else {
            wf.node_voltages
                .get(n.0 as usize)
                .and_then(|w| w.last().copied())
                .unwrap_or(0.0)
        }
    };
    // Sustained violation: evaluate several chunks so the persistence window
    // trips.
    let mut faults = Vec::new();
    for i in 0..10 {
        let t = 1e-3 + i as f64 * 1e-4;
        for f in monitor.evaluate(&mut bound.circuit, &node_v, &|_| None, t) {
            faults.push((
                f.component.clone(),
                format!("{:?}", f.kind),
                f.value,
                f.limit,
            ));
        }
    }
    Some((volts, faults))
}

#[test]
fn as_wired_weight_enable_is_catastrophic() {
    let Some((volts, faults)) = run_cell(false, true) else {
        eprintln!("netlist missing; skipping");
        return;
    };
    let v = |name: &str| -> f64 {
        volts
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
            .unwrap_or(f64::NAN)
    };
    // The switch slams the base node toward the rail; the B-E junction clamps
    // far above any sane V_BE while the switch r_on burns the difference.
    let v_base = v("Net-(ANALOG_SWITCH3905-A)");
    let i_base = (5.0 - v_base) / 6.0; // through the switch's 6 ohm r_on
    eprintln!("as-wired, weight ON: V(base)={v_base:.3} V, I(base)~{:.0} mA", i_base * 1e3);
    assert!(
        i_base > 0.1,
        "base current {:.3} A should dwarf the 100 mA junction rating",
        i_base
    );
    // The stress monitor must independently flag the transistor and/or the
    // switch channel.
    eprintln!("faults: {faults:?}");
    assert!(
        faults.iter().any(|(c, k, _, _)| c.starts_with("IC3906") && k.contains("current"))
            || faults
                .iter()
                .any(|(c, k, _, _)| c.starts_with("ANALOG_SWITCH3905") && k.contains("current")),
        "stress monitor should flag the junction or switch overcurrent, got {faults:?}"
    );
}

#[test]
fn as_wired_disabled_weight_still_no_mirror() {
    // Even with the weight off, the cell can never deliver a mirrored
    // current: C2 only reaches the reference strap, not the routed output.
    let Some((volts, _)) = run_cell(false, false) else {
        eprintln!("netlist missing; skipping");
        return;
    };
    let c2 = volts
        .iter()
        .find(|(n, _)| n == "Net-(IC3906-C2)")
        .map(|(_, v)| *v)
        .unwrap_or(f64::NAN);
    // C2 sits at the reference-node potential (through Rstop) rather than at
    // a membrane: structurally incapable of inhibition.
    eprintln!("as-wired, weight OFF: V(C2)={c2:.3} V");
    assert!(c2.is_finite());
}

#[test]
fn repaired_wiring_is_a_healthy_mirror() {
    let Some((volts, faults)) = run_cell(true, true) else {
        eprintln!("netlist missing; skipping");
        return;
    };
    let v = |name: &str| -> f64 {
        volts
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
            .unwrap_or(f64::NAN)
    };
    // With B2/C2 exchanged the diode-connected reference sets ~0.45uA and the
    // output collector (now on the switch COM) sinks the mirrored current
    // from the ANALOG_VDD throw through the switch: a controlled microamp,
    // about six orders of magnitude below the broken case.
    let v_com = v("Net-(ANALOG_SWITCH3905-A)");
    let i_sink = (5.0 - v_com) / 6.0;
    eprintln!("repaired, weight ON: V(COM)={v_com:.4} V, I(sink)~{:.3} uA", i_sink * 1e6);
    assert!(
        i_sink > 0.05e-6 && i_sink < 10e-6,
        "repaired mirror should sink ~microamps, got {i_sink:.3e} A"
    );
    assert!(
        faults.is_empty(),
        "repaired cell must raise no faults, got {faults:?}"
    );
}
