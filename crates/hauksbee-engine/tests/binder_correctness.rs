//! Binder correctness regressions: DNP parts must stay off the board, split
//! grounds must stay split, analog-switch thresholds must follow the real
//! rail, and a "CR"-designated diode must never bind as a capacitor.
//!
//! Each test reproduces a confirmed end-to-end bug against a minimal
//! hand-built [`ExtractedBoard`] (constructed directly so DNP flags and
//! footprints are exact, independent of any netlist parser).

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::report::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

fn pin(number: &str, net: i64, function: &str) -> Pin {
    Pin {
        number: number.to_string(),
        net: Some(net),
        function: function.to_string(),
        kind: "passive".to_string(),
        position: None,
    }
}

fn comp(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: String::new(),
        footprint: footprint.to_string(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins,
    }
}

fn board(nets: &[(i64, &str)], components: Vec<Component>) -> ExtractedBoard {
    ExtractedBoard {
        name: "binder_correctness_test".to_string(),
        nets: nets
            .iter()
            .map(|(id, name)| Net {
                id: *id,
                name: name.to_string(),
            })
            .collect(),
        components,
    }
}

/// Bug #1: a DNP component is on the layout but NOT assembled. It must
/// contribute no device and no pin-to-net wiring — before the fix a DNP
/// resistor bridging two nets was stamped like a populated part, silently
/// joining nets that are open on the real board.
#[test]
fn dnp_component_is_not_stamped() {
    let mut r_dnp = comp(
        "R1",
        "0R",
        "Resistor_SMD:R_0402_1005Metric",
        vec![pin("1", 1, ""), pin("2", 2, "")],
    );
    r_dnp.dnp = true;
    // A populated resistor from each net to a third, so the nets are real.
    let r2 = comp(
        "R2",
        "10k",
        "Resistor_SMD:R_0402_1005Metric",
        vec![pin("1", 1, ""), pin("2", 3, "")],
    );
    let r3 = comp(
        "R3",
        "10k",
        "Resistor_SMD:R_0402_1005Metric",
        vec![pin("1", 2, ""), pin("2", 3, "")],
    );
    let b = board(&[(1, "NET_A"), (2, "NET_B"), (3, "NET_C")], vec![r_dnp, r2, r3]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    // No device for the DNP part at all.
    assert!(
        !bound.circuit.devices.iter().any(|d| d.name() == "R1"),
        "DNP R1 must not stamp any device, got: {:?}",
        bound
            .circuit
            .devices
            .iter()
            .map(|d| d.name().to_string())
            .collect::<Vec<_>>()
    );
    // NET_A and NET_B are not joined through it: no device touches both nodes.
    let a = bound.node("NET_A").expect("NET_A interned");
    let bnode = bound.node("NET_B").expect("NET_B interned");
    assert_ne!(a, bnode);
    assert!(
        !bound
            .circuit
            .devices
            .iter()
            .any(|d| d.nodes().contains(&a) && d.nodes().contains(&bnode)),
        "no single device may bridge NET_A and NET_B (R1 is DNP)"
    );
    // And the report says so, as a skip — not a bound or unresolved row.
    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "R1")
        .expect("DNP part still gets a bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Skipped { reason } if reason.contains("DNP")),
        "R1 outcome should be skipped (DNP), got {:?}",
        row.outcome
    );
}

/// Bug #2: AGND and DGND are distinct real nets; the old `is_ground` catch-all
/// (`ends_with("GND")`) fused them both onto node 0 before binding, so a 0 Ω
/// star-point resistor between them became an inert self-loop and the board's
/// split-ground topology vanished. They must intern as two distinct non-ground
/// nodes, with the bridge a real 2-node device.
#[test]
fn split_grounds_stay_distinct_nodes() {
    let bridge = comp(
        "R1",
        "0R",
        "Resistor_SMD:R_0603_1608Metric",
        vec![pin("1", 1, ""), pin("2", 2, "")],
    );
    let b = board(&[(1, "AGND"), (2, "DGND"), (3, "GND")], vec![bridge]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let agnd = bound.node("AGND").expect("AGND interned");
    let dgnd = bound.node("DGND").expect("DGND interned");
    let gnd = bound.node("GND").expect("GND interned");

    // The canonical ground still folds onto node 0; the split grounds do not.
    assert!(gnd.is_ground(), "GND is the canonical ground node");
    assert!(!agnd.is_ground(), "AGND must NOT fuse onto node 0");
    assert!(!dgnd.is_ground(), "DGND must NOT fuse onto node 0");
    assert_ne!(agnd, dgnd, "AGND and DGND are distinct nets");

    // The star-point bridge is a real 2-node resistor, not a self-loop.
    let (a, b2) = bound
        .circuit
        .devices
        .iter()
        .find_map(|d| match d {
            Device::Resistor { name, a, b, .. } if name == "R1" => Some((*a, *b)),
            _ => None,
        })
        .expect("bridge resistor stamped");
    assert_ne!(a, b2, "bridge must span two distinct nodes (was a self-loop)");
    assert_eq!(
        [a, b2].iter().copied().collect::<std::collections::BTreeSet<_>>(),
        [agnd, dgnd].iter().copied().collect::<std::collections::BTreeSet<_>>(),
        "bridge connects AGND to DGND"
    );
}

/// Bug #4: the true-SPDT s0 leg senses (vcc - select) but hardcoded a 5 V rail
/// into its thresholds. On a 3.3 V board von was 5.0-1.5+0.25 = 3.75 V, which
/// (vcc - select) <= 3.3 V can never reach: the com<->s0 path was permanently
/// open. The thresholds must follow the actual rail on the vcc net.
#[test]
fn spdt_s0_thresholds_follow_the_actual_rail() {
    // SN74LVC1G3157 on a 3.3 V rail, wired per its SOT-23-6 pad map.
    let sw = comp(
        "SW1",
        "SN74LVC1G3157",
        "Package_TO_SOT_SMD:SOT-23-6",
        vec![
            pin("1", 4, ""), // B2 = s1
            pin("2", 6, ""), // GND
            pin("3", 3, ""), // B1 = s0
            pin("4", 2, ""), // A  = com
            pin("5", 1, ""), // VCC
            pin("6", 5, ""), // S  = select
        ],
    );
    let b = board(
        &[
            (1, "+3V3"),
            (2, "COM_NET"),
            (3, "S0_NET"),
            (4, "S1_NET"),
            (5, "SEL"),
            (6, "GND"),
        ],
        vec![sw],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let (von, voff) = bound
        .circuit
        .devices
        .iter()
        .find_map(|d| match d {
            Device::VSwitch { name, von, voff, .. } if name == "SW1_s0" => Some((*von, *voff)),
            _ => None,
        })
        .expect("true-SPDT s0 leg stamped as a VSwitch");

    // vth = 1.5 (model), rail = 3.3: von = 3.3-1.5+0.25 = 2.05, voff = 1.55.
    let rail = 3.3;
    let vth = 1.5;
    assert!(
        (von - (rail - vth + 0.25)).abs() < 1e-9,
        "s0 von must be rail-referenced (expected {}, got {von})",
        rail - vth + 0.25
    );
    assert!(
        (voff - (rail - vth - 0.25)).abs() < 1e-9,
        "s0 voff must be rail-referenced (expected {}, got {voff})",
        rail - vth - 0.25
    );
    // Reachability: with select low, (vcc - select) = 3.3 V must EXCEED von,
    // i.e. the leg actually conducts when selected. Before the fix von was
    // 3.75 V > 3.3 V — never.
    assert!(
        rail > von,
        "s0 leg must be able to close on a {rail} V rail (von = {von})"
    );
}

/// Bug #6: a "CR1" zener (MIL-STD/ANSI diode designator) with value "5.1V" and
/// a diode footprint used to skip the diode fallback (prefix isn't 'D') and
/// land in the R/C/L first-letter heuristic, where 'C' bound it as a 5.1 FARAD
/// capacitor. It must bind as a diode.
#[test]
fn cr_designated_diode_is_not_a_capacitor() {
    let zener = comp(
        "CR1",
        "5.1V",
        "Diode_SMD:D_SOD-123",
        vec![pin("1", 2, "K"), pin("2", 1, "A")],
    );
    let b = board(&[(1, "ANODE_NET"), (2, "CATHODE_NET")], vec![zener]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    // Specifically NOT a 5.1 F capacitor.
    assert!(
        !bound
            .circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Capacitor { name, .. } if name == "CR1")),
        "CR1 must not bind as a capacitor"
    );
    // It binds through the diode fallback as a conducting junction.
    let (a, k) = bound
        .circuit
        .devices
        .iter()
        .find_map(|d| match d {
            Device::Diode { name, a, k, .. } if name == "CR1" => Some((*a, *k)),
            _ => None,
        })
        .expect("CR1 must bind as a Device::Diode via the diode fallback");
    assert_eq!(bound.node("ANODE_NET"), Some(a), "anode on the A-pin net");
    assert_eq!(bound.node("CATHODE_NET"), Some(k), "cathode on the K-pin net");
}
