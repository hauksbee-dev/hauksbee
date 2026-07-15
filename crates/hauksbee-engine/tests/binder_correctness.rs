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

/// Bug (binder-r3 #1): a multi-element passive array (a 4-pad isolated
/// resistor network) used to bind as ONE 2-terminal resistor across its first
/// two pads — the other elements silently vanished from the circuit. An even
/// pad count is the isolated-array convention: sequential pad pairs (1-2,
/// 3-4, …), one element per pair at the pack's per-element value.
#[test]
fn isolated_passive_array_stamps_one_element_per_pad_pair() {
    // 4-pad isolated array: element 1 = pads 1-2 (NET_A..NET_B), element 2 =
    // pads 3-4 (NET_C..NET_D).
    let rn = comp(
        "RN1",
        "10k",
        "Resistor_SMD:R_Array_Concave_2x0603",
        vec![pin("1", 1, ""), pin("2", 2, ""), pin("3", 3, ""), pin("4", 4, "")],
    );
    let b = board(
        &[(1, "NET_A"), (2, "NET_B"), (3, "NET_C"), (4, "NET_D")],
        vec![rn],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let resistors: Vec<_> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Resistor { name, a, b, ohms, .. } if name.starts_with("RN1") => {
                Some((name.clone(), *a, *b, *ohms))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        resistors.len(),
        2,
        "a 4-pad isolated array is TWO resistors, never a silent single \
         element; got {resistors:?}"
    );
    let node = |n: &str| bound.node(n).unwrap();
    let bridges = |x: &str, y: &str| {
        resistors
            .iter()
            .any(|(_, a, b, _)| (*a == node(x) && *b == node(y)) || (*a == node(y) && *b == node(x)))
    };
    assert!(bridges("NET_A", "NET_B"), "element 1 spans pads 1-2");
    assert!(bridges("NET_C", "NET_D"), "element 2 spans pads 3-4");
    assert!(
        !bridges("NET_B", "NET_C"),
        "no element may span across the pad-pair boundary"
    );
    for (name, _, _, ohms) in &resistors {
        assert!(
            (*ohms - 10_000.0).abs() < 1e-6,
            "{name}: each element carries the per-element value, got {ohms}"
        );
    }
}

/// Odd pad counts are ambiguous (bussed common could sit at either end). The
/// binder assumes the bussed convention (lowest pad common) but must say so
/// LOUDLY in the bind report — never a silent single-element bind.
#[test]
fn bussed_passive_array_binds_all_elements_with_a_loud_warning() {
    // 5-pad bussed array: pad 1 common, elements to pads 2..5.
    let rn = comp(
        "RN2",
        "4k7",
        "Resistor_THT:R_Array_SIP5",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("3", 3, ""),
            pin("4", 4, ""),
            pin("5", 5, ""),
        ],
    );
    let b = board(
        &[(1, "COM"), (2, "N2"), (3, "N3"), (4, "N4"), (5, "N5")],
        vec![rn],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let resistors: Vec<_> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Resistor { name, a, b, .. } if name.starts_with("RN2") => {
                Some((name.clone(), *a, *b))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        resistors.len(),
        4,
        "a 5-pad bussed array is FOUR elements off the common, got {resistors:?}"
    );
    let com = bound.node("COM").unwrap();
    for (name, a, b) in &resistors {
        assert!(
            *a == com || *b == com,
            "{name}: every bussed element must touch the common pad's net"
        );
    }
    // And the assumption is loud, not silent.
    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "RN2")
        .expect("RN2 gets a bind row");
    let w = row.warning.as_deref().unwrap_or("");
    assert!(
        w.contains("BUSSED"),
        "ambiguous odd-pad array must carry a loud bussed-assumption warning, got {w:?}"
    );
}

/// Bug (binder-r3 #2): negative supply rails ("-5V", "-12V") were never
/// recognised by `power_rail_voltage` (the substring fallback required a
/// leading '+'), so the net got no SupplyLeg and silently floated at 0 V.
/// A negative rail must get a supply at the NEGATIVE voltage — and must not
/// be conflated with ground.
#[test]
fn negative_rails_get_supply_legs_at_negative_voltage() {
    // An op-amp-style split supply: R1 from -5V to GND, R2 from -12V to GND
    // (loads so the nets are real).
    let r1 = comp(
        "R1",
        "10k",
        "Resistor_SMD:R_0402_1005Metric",
        vec![pin("1", 1, ""), pin("2", 3, "")],
    );
    let r2 = comp(
        "R2",
        "10k",
        "Resistor_SMD:R_0402_1005Metric",
        vec![pin("1", 2, ""), pin("2", 3, "")],
    );
    let b = board(&[(1, "-5V"), (2, "-12V"), (3, "GND")], vec![r1, r2]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let leg_volts = |net: &str| -> f64 {
        bound
            .supplies
            .iter()
            .find(|l| l.net_name == net)
            .unwrap_or_else(|| panic!("net {net} must get a SupplyLeg, not float at 0 V"))
            .supply
            .nominal_volts()
    };
    assert_eq!(leg_volts("-5V"), -5.0, "-5V rail supplies -5.0 V");
    assert_eq!(leg_volts("-12V"), -12.0, "-12V rail supplies -12.0 V");

    // Not ground: the rail keeps its own (non-ground) node.
    let n5 = bound.node("-5V").expect("-5V interned");
    assert!(!n5.is_ground(), "-5V must never fold onto ground node 0");

    // And the report carries the rail rows at the negative voltage.
    for (name, v) in [("-5V", -5.0), ("-12V", -12.0)] {
        let row = bound
            .report
            .rows
            .iter()
            .find(|r| r.reference == format!("RAIL:{name}"))
            .unwrap_or_else(|| panic!("RAIL:{name} row expected"));
        assert!(
            matches!(row.outcome, BindOutcome::PowerRail { volts } if volts == v),
            "{name}: expected PowerRail at {v} V, got {:?}",
            row.outcome
        );
    }
}

/// Bug (binder-r3 #3): electrode-letter pad numbers ("A"/"K" on a diode
/// footprint, no pinfunction, numerically-keyed fallback pad map) were never
/// interpreted as roles, so the diode bound OPEN — a real junction deleted.
/// The pad letters must map to anode/cathode with correct polarity.
#[test]
fn diode_with_electrode_letter_pads_binds_with_correct_polarity() {
    // Footprint-only extraction: pads are NAMED "A"/"K", pinfunctions empty.
    // Value "D" resolves through the engine's 1N4148 fallback, whose pad map
    // is keyed "1"/"2" — so before the fix nothing matched and D1 bound open.
    let d = comp(
        "D1",
        "D",
        "Diode_SMD:D_SOD-323",
        vec![pin("A", 1, ""), pin("K", 2, "")],
    );
    let b = board(&[(1, "ANODE_NET"), (2, "CATHODE_NET")], vec![d]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let (a, k) = bound
        .circuit
        .devices
        .iter()
        .find_map(|dv| match dv {
            Device::Diode { name, a, k, .. } if name == "D1" => Some((*a, *k)),
            _ => None,
        })
        .expect("A/K-padded diode must bind as a Device::Diode, not OPEN");
    assert_eq!(bound.node("ANODE_NET"), Some(a), "pad A is the anode");
    assert_eq!(bound.node("CATHODE_NET"), Some(k), "pad K is the cathode");
}

/// Eagle "P$1"/"P$2" ordinal pads reach the model's numeric pad map after
/// normalisation ("P$1" -> "1" = cathode in the KiCad Device:D convention).
#[test]
fn diode_with_eagle_ordinal_pads_binds_via_numeric_pad_map() {
    let d = comp(
        "D2",
        "D",
        "Diode_SMD:D_SOD-323",
        vec![pin("P$1", 1, ""), pin("P$2", 2, "")],
    );
    let b = board(&[(1, "CATHODE_NET"), (2, "ANODE_NET")], vec![d]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let (a, k) = bound
        .circuit
        .devices
        .iter()
        .find_map(|dv| match dv {
            Device::Diode { name, a, k, .. } if name == "D2" => Some((*a, *k)),
            _ => None,
        })
        .expect("P$-padded diode must bind as a Device::Diode, not OPEN");
    assert_eq!(bound.node("ANODE_NET"), Some(a), "P$2 is the anode (1=K 2=A)");
    assert_eq!(bound.node("CATHODE_NET"), Some(k), "P$1 is the cathode");
}

/// Bug #12: a dual op-amp (LM358) defines out_a/in_plus_a/in_minus_a AND the
/// _b channel, but the binder only looked up channel A — one device stamped,
/// channel B's output net left floating with no warning. Both channels must
/// stamp, keyed with the multi-unit `_q<N>` suffix the CI aggregation matches.
#[test]
fn dual_opamp_stamps_one_device_per_channel() {
    // LM358 SOIC-8 pad map (opamp_comparator.toml): 1=out_a, 2=in_minus_a,
    // 3=in_plus_a, 4=vss, 5=in_plus_b, 6=in_minus_b, 7=out_b, 8=vcc.
    let u = comp(
        "U1",
        "LM358",
        "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("3", 3, ""),
            pin("4", 4, ""),
            pin("5", 5, ""),
            pin("6", 6, ""),
            pin("7", 7, ""),
            pin("8", 8, ""),
        ],
    );
    let b = board(
        &[
            (1, "OUT_A"),
            (2, "INM_A"),
            (3, "INP_A"),
            (4, "GND"),
            (5, "INP_B"),
            (6, "INM_B"),
            (7, "OUT_B"),
            (8, "+3V3"),
        ],
        vec![u],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let out_of = |unit: &str| {
        bound
            .circuit
            .devices
            .iter()
            .find_map(|d| match d {
                Device::OpAmp { name, out, .. } if name == unit => Some(*out),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{unit} must stamp as a Device::OpAmp"))
    };
    // One device per channel, per-unit keyed, wired to its OWN output net.
    assert_eq!(bound.node("OUT_A"), Some(out_of("U1_q1")));
    assert_eq!(bound.node("OUT_B"), Some(out_of("U1_q2")));
    assert_eq!(
        bound
            .circuit
            .devices
            .iter()
            .filter(|d| matches!(d, Device::OpAmp { .. }))
            .count(),
        2,
        "a dual op-amp is exactly two OpAmp devices"
    );
}

/// Bug #12 (comparator half): the LM393 dual comparator gets one Comparator
/// per channel, same `_q<N>` keying.
#[test]
fn dual_comparator_stamps_one_device_per_channel() {
    // LM393 SOIC-8 pad map: 1=out_a, 2=in_minus_a, 3=in_plus_a, 4=vss,
    // 5=in_plus_b, 6=in_minus_b, 7=out_b, 8=vcc.
    let u = comp(
        "U2",
        "LM393",
        "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("3", 3, ""),
            pin("4", 4, ""),
            pin("5", 5, ""),
            pin("6", 6, ""),
            pin("7", 7, ""),
            pin("8", 8, ""),
        ],
    );
    let b = board(
        &[
            (1, "OUT_A"),
            (2, "INM_A"),
            (3, "INP_A"),
            (4, "GND"),
            (5, "INP_B"),
            (6, "INM_B"),
            (7, "OUT_B"),
            (8, "+5V"),
        ],
        vec![u],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let outs: Vec<(String, hauksbee_ir::NodeId)> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Comparator { name, out, .. } => Some((name.clone(), *out)),
            _ => None,
        })
        .collect();
    assert_eq!(outs.len(), 2, "a dual comparator is exactly two devices: {outs:?}");
    assert!(outs.contains(&("U2_q1".to_string(), bound.node("OUT_A").unwrap())));
    assert!(outs.contains(&("U2_q2".to_string(), bound.node("OUT_B").unwrap())));
}

/// A single-channel op-amp-class part (LMV7219, unsuffixed out/in_plus/
/// in_minus roles) keeps its pre-fix shape exactly: ONE device under the bare
/// reference, no `_q` suffix.
#[test]
fn single_channel_comparator_keeps_the_bare_reference() {
    // LMV7219 SOT-23-5 pad map: 1=out, 2=vss, 3=in_plus, 4=in_minus, 5=vcc.
    let u = comp(
        "U3",
        "LMV7219",
        "Package_TO_SOT_SMD:SOT-23-5",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("3", 3, ""),
            pin("4", 4, ""),
            pin("5", 5, ""),
        ],
    );
    let b = board(
        &[(1, "OUT"), (2, "GND"), (3, "INP"), (4, "INM"), (5, "+5V")],
        vec![u],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());
    assert!(
        bound
            .circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Comparator { name, .. } if name == "U3")),
        "a single comparator stays under the bare ref"
    );
}

/// Bug #13: the CD74HC4066 quad bilateral switch has FOUR independent gates
/// (in_out_<n>a/<n>b + ctrl_<n>); the old single-SPST fall-through bound gate
/// 1 only and silently dropped gates 2..4. All four must stamp, `_s<n>`-keyed,
/// each switching its own pair under its own control net.
#[test]
fn quad_bilateral_switch_stamps_one_vswitch_per_gate() {
    // CD74HC4066 pad map (analog_switch.toml): 1=in_out_1a, 2=in_out_1b,
    // 3=in_out_2a, 4=in_out_2b, 5=ctrl_2, 6=ctrl_1, 7=vss, 8=ctrl_4,
    // 9=ctrl_3, 10=in_out_3a, 11=in_out_3b, 12=in_out_4a, 13=in_out_4b, 14=vcc.
    let u = comp(
        "SW2",
        "CD74HC4066",
        "Package_SO:TSSOP-14_4.4x5mm_P0.65mm",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("3", 3, ""),
            pin("4", 4, ""),
            pin("5", 12, ""),
            pin("6", 11, ""),
            pin("7", 9, ""),
            pin("8", 14, ""),
            pin("9", 13, ""),
            pin("10", 5, ""),
            pin("11", 6, ""),
            pin("12", 7, ""),
            pin("13", 8, ""),
            pin("14", 10, ""),
        ],
    );
    let b = board(
        &[
            (1, "G1A"),
            (2, "G1B"),
            (3, "G2A"),
            (4, "G2B"),
            (5, "G3A"),
            (6, "G3B"),
            (7, "G4A"),
            (8, "G4B"),
            (9, "GND"),
            (10, "+3V3"),
            (11, "CTRL1"),
            (12, "CTRL2"),
            (13, "CTRL3"),
            (14, "CTRL4"),
        ],
        vec![u],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    for n in 1..=4 {
        let (a, bnode, ctrl) = bound
            .circuit
            .devices
            .iter()
            .find_map(|d| match d {
                Device::VSwitch {
                    name, a, b, ctrl_p, ..
                } if *name == format!("SW2_s{n}") => Some((*a, *b, *ctrl_p)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("gate {n} must stamp as SW2_s{n}"));
        assert_eq!(bound.node(&format!("G{n}A")), Some(a), "gate {n} A leg");
        assert_eq!(bound.node(&format!("G{n}B")), Some(bnode), "gate {n} B leg");
        assert_eq!(
            bound.node(&format!("CTRL{n}")),
            Some(ctrl),
            "gate {n} control"
        );
    }
    assert_eq!(
        bound
            .circuit
            .devices
            .iter()
            .filter(|d| matches!(d, Device::VSwitch { .. }))
            .count(),
        4,
        "a quad bilateral switch is exactly four VSwitch devices"
    );
}

/// Bug #14: bare "VDD" carries no magnitude — on a 3.3 V/1.8 V board it is the
/// local core rail, so assuming 5 V overdrives the whole net. It must resolve
/// to None (no ideal-supply stamp); voltage-suffixed forms keep their volts.
#[test]
fn bare_vdd_is_not_assumed_to_be_5v() {
    use hauksbee_engine::power_rail_voltage;
    assert_eq!(power_rail_voltage("VDD"), None, "bare VDD has no magnitude");
    assert_eq!(power_rail_voltage("/VDD"), None);
    // Voltage-suffixed VDD forms still resolve.
    assert_eq!(power_rail_voltage("VDD3V3"), Some(3.3));
    assert_eq!(power_rail_voltage("VDD_3V3"), Some(3.3));
    assert_eq!(power_rail_voltage("VDD_5V"), Some(5.0));
    // Bare VCC keeps the TTL 5 V convention.
    assert_eq!(power_rail_voltage("VCC"), Some(5.0));
}
