//! Binder correctness regressions: DNP parts must stay off the board, split
//! grounds must stay split, analog-switch thresholds must follow the real
//! rail, and a "CR"-designated diode must never bind as a capacitor.
//!
//! Each test reproduces a confirmed end-to-end bug against a minimal
//! hand-built [`ExtractedBoard`] (constructed directly so DNP flags and
//! footprints are exact, independent of any netlist parser).

use hauksbee_engine::binder::{bind_board, BoundBoard};
use hauksbee_engine::reports::bind::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

#[test]
fn inferred_same_hierarchy_altium_merge_stays_unbound_end_to_end() {
    // A hierarchy identifies the channel, not the physical component. Two
    // records without UNIQUEID can be merged by a shared identically-netted
    // pad for connectivity, but that inference is not authoritative enough to
    // stamp a precise resistor model.
    let text = [
        "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00",
        "|RECORD=Net|ID=0|NAME=SIGNAL",
        "|RECORD=Net|ID=1|NAME=GND",
        "|RECORD=Component|ID=0|LAYER=TOP|PATTERN=R0603|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\ROOT",
        "|RECORD=Component|ID=1|LAYER=BOTTOM|PATTERN=R0603|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\ROOT",
        "|RECORD=Pad|COMPONENT=0|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil",
        "|RECORD=Pad|COMPONENT=1|NET=0|LAYER=MULTILAYER|NAME=1|X=0mil|Y=0mil",
        "|RECORD=Pad|COMPONENT=1|NET=1|LAYER=MULTILAYER|NAME=2|X=100mil|Y=0mil",
        "|RECORD=Text|COMPONENT=1|COMMENT=True|TEXT=1k",
    ]
    .join("\n");

    let board = ExtractedBoard::from_protel_ascii(&text).expect("extract ASCII Altium board");
    assert_eq!(
        board.components.len(),
        1,
        "split records remain one connectivity owner"
    );
    let resistor = board.component("R1").expect("merged R1");
    assert!(
        resistor
            .properties
            .iter()
            .any(|(key, _)| key == hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY),
        "the extractor must carry inferred identity into the shared refusal gate"
    );

    let bound = bind_board(&board, &ModelLibrary::builtin());
    let row = bound.report.row("R1").expect("R1 bind row");
    assert_eq!(row.model_id, None, "inferred identity must remain open");
    assert!(matches!(row.outcome, BindOutcome::Unresolved { .. }));
}

/// The devices `bound` stamped into its circuit.
fn devices(bound: &BoundBoard) -> std::slice::Iter<'_, Device> {
    bound.circuit.devices.iter()
}

fn pin(number: &str, net: i64, function: &str) -> Pin {
    crate::support::pin(number, Some(net), function, "passive")
}

fn comp(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
    crate::support::component(
        reference.to_string(),
        value.to_string(),
        String::new(),
        footprint.to_string(),
        pins,
    )
}

/// A two-pad 0402 resistor from net `a` to net `b`.
fn resistor_0402(reference: &str, value: &str, a: i64, b: i64) -> Component {
    let pins = vec![pin("1", a, ""), pin("2", b, "")];
    comp(reference, value, "Resistor_SMD:R_0402_1005Metric", pins)
}

fn board(nets: &[(i64, &str)], components: Vec<Component>) -> ExtractedBoard {
    ExtractedBoard {
        name: "binder_correctness_test".to_string(),
        nets: nets
            .iter()
            .map(|(id, name)| crate::support::net(*id, name.to_string()))
            .collect(),
        components,
    }
}

/// Bind a board holding only one part, whose pad `i` sits on net `i` named
/// `nets[i - 1]`.
fn bind_single_part(reference: &str, value: &str, footprint: &str, nets: &[&str]) -> BoundBoard {
    let numbered: Vec<(i64, &str)> = (1..).zip(nets.iter().copied()).collect();
    let pins = numbered
        .iter()
        .map(|(i, _)| pin(&i.to_string(), *i, ""))
        .collect();
    let part = comp(reference, value, footprint, pins);
    bind_board(&board(&numbered, vec![part]), &ModelLibrary::builtin())
}

/// Bind one SN74LVC1G3157 SPDT, wired per its SOT-23-6 pad map, with its VCC
/// pad on the net named `vcc_net`.
fn bind_spdt(vcc_net: &str) -> BoundBoard {
    bind_spdt_with(vcc_net, Vec::new())
}

/// [`bind_spdt`] with `others` on the board too (nets 1..=6 as there, 7 spare).
fn bind_spdt_with(vcc_net: &str, mut others: Vec<Component>) -> BoundBoard {
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
            (1, vcc_net),
            (2, "COM_NET"),
            (3, "S0_NET"),
            (4, "S1_NET"),
            (5, "SEL"),
            (6, "GND"),
            (7, "MOD_3V3"),
        ],
        {
            others.insert(0, sw);
            others
        },
    );
    bind_board(&b, &ModelLibrary::builtin())
}

/// A rail fed only by a fitted dev-board module's 5V header pin (no
/// `power_out` pin type, no voltage in its name) is sourced at the card's
/// nominal 5 V: the Nano's +5V comes from USB through its Schottky or from
/// VIN through its UA78M05. A switch on that rail then switches at its 5 V
/// threshold (VIL/VIH midpoint 2.5 V), not the 3.3 V-rail fallback.
#[test]
fn a_fitted_modules_5v_pin_sources_the_rail_it_feeds() {
    let nano = comp(
        "A101",
        "Arduino Nano",
        "Module:Arduino_Nano",
        vec![pin("4", 6, ""), pin("27", 1, ""), pin("17", 7, "")],
    );
    // A load on the 3V3 pin, so that rail feeds something.
    let load = resistor_0402("R1", "10k", 7, 6);
    let bound = bind_spdt_with("+5P", vec![nano, load]);
    for (rail, v) in [("+5P", 5.0), ("MOD_3V3", 3.3)] {
        let row = bound.report.row(&format!("RAIL:{rail}"));
        let volts = row.and_then(|r| match r.outcome {
            BindOutcome::PowerRail { volts } => Some(volts),
            _ => None,
        });
        assert_eq!(volts, Some(v), "{rail} sourced by the module");
    }
    let von = devices(&bound).find_map(|d| match d {
        Device::VSwitch { name, von, .. } if name == "SW1_s0" => Some(*von),
        _ => None,
    });
    let von = von.expect("true-SPDT s0 leg stamped as a VSwitch");
    assert!(
        (von - (5.0 - 2.5 + 0.25)).abs() < 1e-9,
        "s0 leg at the 5 V threshold: {von}"
    );
    // Unfitted, the module sources nothing and the rail stays unresolved.
    let mut dnp = comp(
        "A101",
        "Arduino Nano",
        "Module:Arduino_Nano",
        vec![pin("27", 1, "")],
    );
    dnp.dnp = true;
    assert!(bind_spdt_with("+5P", vec![dnp])
        .report
        .row("RAIL:+5P")
        .is_none());
    // A module pin on a net nothing else touches feeds nothing.
    let nano = comp(
        "A101",
        "Arduino Nano",
        "Module:Arduino_Nano",
        vec![pin("17", 7, "")],
    );
    let bound = bind_spdt_with("+5P", vec![nano]);
    assert!(bound.report.row("RAIL:MOD_3V3").is_none());
}

/// The LTC4020 with its input, its battery monitor and its CSP sense pin, on
/// which the converter's output lands (no filter resistor here, so the rail is
/// CSP's own net).
fn ltc4020_program_board() -> ExtractedBoard {
    board(
        &[(1, "PVIN"), (2, "BAT"), (3, "VOUT")],
        vec![
            comp(
                "U2",
                "LTC4020EUHFPBF",
                "QFN-38",
                vec![pin("36", 1, ""), pin("20", 2, ""), pin("23", 3, "")],
            ),
            comp("R8", "7.15k", "R_0603", Vec::new()),
            comp("R49", "0.01R", "R_2512", Vec::new()),
            comp("R50", "0.01R", "R_2512", Vec::new()),
        ],
    )
}

fn sole_converter_limit(board: &ExtractedBoard) -> Option<f64> {
    let bound = bind_board(board, &ModelLibrary::builtin());
    match bound.behavioral.as_slice() {
        [] => None,
        [converter] => converter.converter_iin_limit(),
        devices => panic!(
            "the fixture may bind at most one LTC4020 behavioural device, got {}",
            devices.len()
        ),
    }
}

#[test]
fn missing_program_evidence_does_not_stamp_an_unbounded_converter() {
    let mut incomplete = ltc4020_program_board();
    incomplete
        .components
        .retain(|component| component.reference != "R50");

    let bound = bind_board(&incomplete, &ModelLibrary::builtin());
    assert_eq!(
        bound.behavioral.len(),
        1,
        "independently evidenced FSM behavior remains observable"
    );
    assert_eq!(
        bound.behavioral[0].converter_out_node(),
        None,
        "an unresolved named shunt must suppress the converter output leg"
    );
}

#[test]
fn ambiguous_programming_resistor_identity_makes_the_limit_unknown() {
    let clean = ltc4020_program_board();
    let clean_limit = sole_converter_limit(&clean).expect("complete resistor evidence");
    assert!((clean_limit - 1.7875).abs() < 1e-12, "got {clean_limit}");

    for reference in ["R8", "R49", "R50"] {
        let mut ambiguous = ltc4020_program_board();
        ambiguous
            .components
            .iter_mut()
            .find(|component| component.reference == reference)
            .expect("fixture resistor")
            .properties
            .push((
                hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY.to_string(),
                "same inferred designator has no authoritative source UID".to_string(),
            ));
        assert_eq!(
            sole_converter_limit(&ambiguous),
            None,
            "identity-refused {reference} must not supply a precise current limit"
        );
    }
}

#[test]
fn duplicate_programming_reference_never_uses_last_record_wins() {
    let mut duplicated = ltc4020_program_board();
    duplicated
        .components
        .push(comp("R8", "100k", "R_0603", Vec::new()));
    assert_eq!(
        sole_converter_limit(&duplicated),
        None,
        "two physical records with one designator are contradictory evidence, even when both parse"
    );
}

/// The LTC4020 has no output pin: CSP reads the DC/DC output through a 100 ohm
/// filter resistor (Rev E, PDF p.12), so the converter lands on that resistor's
/// far side, the rail itself, not on CSP's filtered stub. A second resistor
/// from CSP to another rail leaves no telling which one it regulates, so the
/// converter is left open rather than guessed.
#[test]
fn ltc4020_output_lands_on_the_rail_past_its_csp_filter() {
    let filtered = |extra: Vec<Component>| {
        let mut b = ltc4020_program_board();
        b.nets.extend(
            [(4, "SYS"), (5, "GND"), (6, "OTHER")]
                .map(|(id, name)| crate::support::net(id, name.to_string())),
        );
        b.components.push(resistor_0402("R175", "100", 3, 4));
        let cap = vec![pin("1", 3, ""), pin("2", 5, "")];
        b.components
            .push(comp("C1", "100n", "Capacitor_SMD:C_0402_1005Metric", cap));
        b.components.extend(extra);
        b
    };
    let converter_out = |bound: &BoundBoard| {
        (bound.behavioral.iter()).find_map(hauksbee_engine::BehavioralDevice::converter_out_node)
    };

    let bound = bind_board(&filtered(Vec::new()), &ModelLibrary::builtin());
    let sys = bound.net_nodes.get("SYS").copied();
    assert!(sys.is_some(), "the rail is on the board");
    assert_eq!(
        converter_out(&bound),
        sys,
        "the output is the far side of R175"
    );

    let second = resistor_0402("R176", "100", 3, 6);
    let bound = bind_board(&filtered(vec![second]), &ModelLibrary::builtin());
    assert_eq!(
        converter_out(&bound),
        None,
        "two candidate rails: no output"
    );
    let row = bound.report.row("U2").expect("U2 bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Unresolved { reason } if reason.contains("two resistors")),
        "{:?}",
        row.outcome
    );
}

/// Bug #1: a DNP component is on the layout but NOT assembled. It must
/// contribute no device and no pin-to-net wiring, before the fix a DNP
/// resistor bridging two nets was stamped like a populated part, silently
/// joining nets that are open on the real board.
#[test]
fn dnp_component_is_not_stamped() {
    let mut r_dnp = resistor_0402("R1", "0R", 1, 2);
    r_dnp.dnp = true;
    // A populated resistor from each net to a third, so the nets are real.
    let r2 = resistor_0402("R2", "10k", 1, 3);
    let r3 = resistor_0402("R3", "10k", 2, 3);
    let b = board(
        &[(1, "NET_A"), (2, "NET_B"), (3, "NET_C")],
        vec![r_dnp, r2, r3],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    // No device for the DNP part at all.
    assert!(
        !bound.circuit.devices.iter().any(|d| d.name() == "R1"),
        "DNP R1 must not stamp any device, got: {:?}",
        devices(&bound)
            .map(|d| d.name().to_string())
            .collect::<Vec<_>>()
    );
    // NET_A and NET_B are not joined through it: no device touches both nodes.
    let a = bound.node("NET_A").expect("NET_A interned");
    let bnode = bound.node("NET_B").expect("NET_B interned");
    assert_ne!(a, bnode);
    assert!(
        !devices(&bound).any(|d| d.nodes().contains(&a) && d.nodes().contains(&bnode)),
        "no single device may bridge NET_A and NET_B (R1 is DNP)"
    );
    // And the report says so, as a skip, not a bound or unresolved row.
    let row = bound
        .report
        .row("R1")
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
    // A part on GND makes it the board's canonical ground (a net no part
    // touches is no ground at all, and a board with only split grounds takes
    // one of them as its reference instead).
    let decoupling = comp(
        "C1",
        "100n",
        "Capacitor_SMD:C_0603_1608Metric",
        vec![pin("1", 2, ""), pin("2", 3, "")],
    );
    let b = board(
        &[(1, "AGND"), (2, "DGND"), (3, "GND")],
        vec![bridge, decoupling],
    );
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
    let ends = devices(&bound)
        .find_map(|d| match d {
            Device::Resistor { name, a, b, .. } if name == "R1" => Some([*a, *b]),
            _ => None,
        })
        .expect("bridge resistor stamped");
    assert_ne!(
        ends[0], ends[1],
        "bridge must span two distinct nodes (was a self-loop)"
    );
    assert_eq!(
        std::collections::BTreeSet::from(ends),
        std::collections::BTreeSet::from([agnd, dgnd]),
        "bridge connects AGND to DGND"
    );
}

/// Bug #4: the true-SPDT s0 leg senses (vcc - select) but hardcoded a 5 V rail
/// into its thresholds. On a 3.3 V board von was 5.0-1.5+0.25 = 3.75 V, which
/// (vcc - select) <= 3.3 V can never reach: the com<->s0 path was permanently
/// open. The thresholds must follow the actual rail on the vcc net.
#[test]
fn spdt_s0_thresholds_follow_the_actual_rail() {
    // SN74LVC1G3157 on a 3.3 V rail.
    let bound = bind_spdt("+3V3");

    let (von, voff) = devices(&bound)
        .find_map(|d| match d {
            Device::VSwitch {
                name, von, voff, ..
            } if name == "SW1_s0" => Some((*von, *voff)),
            _ => None,
        })
        .expect("true-SPDT s0 leg stamped as a VSwitch");

    // The select threshold is the VIL..VIH midpoint read at the rail
    // (SCES424O 5.4: VIL 0.3 x VCC, VIH 0.7 x VCC at 2.3-5.5 V), so on 3.3 V
    // vth = 1.65 and von = 3.3-1.65+0.25 = 1.9, voff = 1.4.
    let rail = 3.3;
    let vth = 0.5 * (0.3 + 0.7) * rail;
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
    // 3.75 V > 3.3 V, never.
    assert!(
        rail > von,
        "s0 leg must be able to close on a {rail} V rail (von = {von})"
    );
}

/// R5: an analog-switch VCC net that does not resolve to a known rail voltage
/// (a non-canonical name like `"VDD_MUX"`) must NOT silently assume 5 V, that
/// reintroduces the "s0 permanently open on a low-voltage board" bug the
/// rail-referenced thresholds fixed. It still falls back to 5 V, but says so in
/// the bind report.
#[test]
fn spdt_unresolved_vcc_rail_warns() {
    // VCC on a non-canonical net name with no recognizable rail voltage.
    let bound = bind_spdt("VDD_MUX");
    let row = bound.report.row("SW1").expect("SW1 gets a bind row");
    let w = row.warning.as_deref().unwrap_or("");
    assert!(
        w.contains("assumed") && w.contains("VDD_MUX"),
        "an unresolved analog-switch VCC rail must warn instead of silently \
         assuming 5 V, got {w:?}"
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
        !devices(&bound).any(|d| matches!(d, Device::Capacitor { name, .. } if name == "CR1")),
        "CR1 must not bind as a capacitor"
    );
    // It binds through the diode fallback as a conducting junction.
    assert_diode_polarity(&bound, "CR1");
}

/// `reference` bound as a `Device::Diode` (not OPEN, not a passive) with its
/// anode on `ANODE_NET` and its cathode on `CATHODE_NET`.
fn assert_diode_polarity(bound: &BoundBoard, reference: &str) {
    let (a, k) = devices(bound)
        .find_map(|d| match d {
            Device::Diode { name, a, k, .. } if name == reference => Some((*a, *k)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{reference} must bind as a Device::Diode"));
    assert_eq!(bound.node("ANODE_NET"), Some(a), "{reference} anode");
    assert_eq!(bound.node("CATHODE_NET"), Some(k), "{reference} cathode");
}

/// Some PCB formats preserve one physical pad record per copper primitive or
/// layer. A two-terminal resistor can therefore arrive as pads 1,2,1,2 even
/// though it is one logical component. Repeated records must not turn it into
/// a two-element resistor array. A missing net on one copy of a numbered pad
/// is enriched from the connected copy of that same pad.
#[test]
fn repeated_physical_pads_bind_one_logical_two_terminal_resistor() {
    let r = comp(
        "R7",
        "10k",
        "Resistor_SMD:R_1206_3216Metric",
        vec![
            Pin {
                number: "1".to_string(),
                kind: "passive".to_string(),
                ..Default::default()
            },
            pin("2", 2, ""),
            pin("1", 1, ""),
            pin("2", 2, ""),
        ],
    );
    let b = board(&[(1, "NET_A"), (2, "NET_B")], vec![r]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let resistors: Vec<_> = devices(&bound)
        .filter_map(|device| match device {
            Device::Resistor {
                name, a, b, ohms, ..
            } if name == "R7" || name.starts_with("R7_e") => Some((name.as_str(), *a, *b, *ohms)),
            _ => None,
        })
        .collect();
    assert_eq!(
        resistors.len(),
        1,
        "four physical pad records for two numbered terminals are one resistor: {resistors:?}"
    );
    let (name, a, b, ohms) = resistors[0];
    assert_eq!(name, "R7", "a scalar resistor keeps its original reference");
    assert!(
        (ohms - 10_000.0).abs() < 1e-9,
        "the original R is preserved"
    );
    let expected = [bound.node("NET_A").unwrap(), bound.node("NET_B").unwrap()]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let actual = [a, b]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        actual, expected,
        "the logical terminals span the original nets"
    );
}

/// Duplicate physical records only provide evidence when their known nets
/// agree. If two records claim different nets for the same numbered terminal,
/// selecting either one would fabricate connectivity; leave the passive open
/// and report the contradiction.
#[test]
fn contradictory_duplicate_passive_pad_nets_are_refused() {
    let r = comp(
        "R8",
        "4k7",
        "Resistor_SMD:R_0603_1608Metric",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("1", 3, ""),
            pin("2", 2, ""),
        ],
    );
    let b = board(
        &[(1, "NET_A"), (2, "NET_B"), (3, "CONTRADICTORY_NET")],
        vec![r],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    assert!(
        !devices(&bound).any(|device| device.name() == "R8" || device.name().starts_with("R8_e")),
        "a contradictory logical terminal must not stamp guessed connectivity"
    );
    let row = bound.report.row("R8").expect("R8 gets a bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Unresolved { reason } if reason.contains("pad 1") && reason.contains("conflicting nets")),
        "the unresolved reason identifies the contradictory pad: {:?}",
        row.outcome
    );
    assert!(
        row.warning
            .as_deref()
            .is_some_and(|warning| warning.contains("pad 1") && warning.contains("left open")),
        "the warning must make the refusal visible: {:?}",
        row.warning
    );
}

/// Logical-terminal normalization is shared by all scalar passives, not just
/// resistors. Repeated physical records must keep a capacitor or inductor a
/// single device with its original reference as well.
#[test]
fn repeated_physical_pads_keep_capacitors_and_inductors_scalar() {
    let c = comp(
        "C7",
        "100nF",
        "Capacitor_SMD:C_0603_1608Metric",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("1", 1, ""),
            pin("2", 2, ""),
        ],
    );
    let l = comp(
        "L7",
        "22uH",
        "Inductor_SMD:L_1210_3225Metric",
        vec![
            pin("1", 3, ""),
            pin("2", 4, ""),
            pin("1", 3, ""),
            pin("2", 4, ""),
        ],
    );
    let b = board(
        &[(1, "C_A"), (2, "C_B"), (3, "L_A"), (4, "L_B")],
        vec![c, l],
    );
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let capacitors: Vec<_> = devices(&bound)
        .filter(|device| matches!(device, Device::Capacitor { name, .. } if name.starts_with("C7")))
        .collect();
    let inductors: Vec<_> = devices(&bound)
        .filter(|device| matches!(device, Device::Inductor { name, .. } if name.starts_with("L7")))
        .collect();
    assert_eq!(capacitors.len(), 1, "C7 is one logical capacitor");
    assert_eq!(capacitors[0].name(), "C7", "C7 keeps its scalar reference");
    assert_eq!(inductors.len(), 1, "L7 is one logical inductor");
    assert_eq!(inductors[0].name(), "L7", "L7 keeps its scalar reference");
}

/// Altium can infer that split placement records are one component from a
/// shared pad/net. That is useful for DRC ownership, but it does not prove one
/// physical part: two duplicate-reference capacitors can both span VCC/GND.
/// Without an authoritative source UID, the binder must leave the inferred
/// identity open rather than stamping one precise capacitor for two possible
/// parts.
#[test]
fn reference_ambiguous_merge_without_source_uid_is_left_open() {
    let mut c = comp(
        "C9",
        "100nF",
        "Capacitor_SMD:C_0603_1608Metric",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("1", 1, ""),
            pin("2", 2, ""),
        ],
    );
    c.properties.push((
        "reference_ambiguous".to_string(),
        "records merged from shared pad/net evidence".to_string(),
    ));
    let b = board(&[(1, "VCC"), (2, "GND")], vec![c]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    assert!(
        !devices(&bound)
            .any(|device| { matches!(device, Device::Capacitor { name, .. } if name == "C9") }),
        "an inferred identity without a UID must not stamp precise physics"
    );
    let row = bound
        .report
        .row("C9")
        .expect("ambiguous C9 gets a visible bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Unresolved { reason }
            if reason.contains("ambiguous inferred reference") && reason.contains("UID")),
        "the unresolved reason must identify the missing identity evidence: {:?}",
        row.outcome
    );
    assert!(
        row.warning
            .as_deref()
            .is_some_and(|warning| warning.contains("left open") && warning.contains("UID")),
        "the refusal must be prominent to users: {:?}",
        row.warning
    );
}

/// A non-empty Altium UNIQUEID, retained by extraction as `source_unique_id`,
/// is authoritative component identity. Repeated physical pad records for that
/// identity still normalize to one scalar component rather than an array.
#[test]
fn reference_ambiguous_merge_with_source_uid_binds_one_scalar() {
    let mut c = comp(
        "C10",
        "100nF",
        "Capacitor_SMD:C_0603_1608Metric",
        vec![
            pin("1", 1, ""),
            pin("2", 2, ""),
            pin("1", 1, ""),
            pin("2", 2, ""),
        ],
    );
    c.properties.extend([
        (
            "reference_ambiguous".to_string(),
            "split placement records".to_string(),
        ),
        ("source_unique_id".to_string(), "ABC123".to_string()),
    ]);
    let b = board(&[(1, "VCC"), (2, "GND")], vec![c]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let capacitors: Vec<_> = devices(&bound)
        .filter(
            |device| matches!(device, Device::Capacitor { name, .. } if name.starts_with("C10")),
        )
        .collect();
    assert_eq!(capacitors.len(), 1, "one UID means one scalar capacitor");
    assert_eq!(capacitors[0].name(), "C10");
    let row = bound.report.row("C10").expect("C10 gets a bind row");
    assert!(
        matches!(row.outcome, BindOutcome::Analog { .. }),
        "authoritative UID identity should bind normally: {:?}",
        row.outcome
    );
}

/// Bug (binder-r3 #1): a multi-element passive array (a 4-pad isolated
/// resistor network) used to bind as ONE 2-terminal resistor across its first
/// two pads; the other elements silently vanished from the circuit. An even
/// pad count is an isolated array: a chip array pairs pads across the package
/// (1-4, 2-3), one element per pair at the pack's per-element value.
#[test]
fn isolated_passive_array_stamps_one_element_per_pad_pair() {
    // 4-pad chip array: element 1 = pads 1-4 (NET_A..NET_D), element 2 =
    // pads 2-3 (NET_B..NET_C).
    let bound = bind_single_part(
        "RN1",
        "10k",
        "Resistor_SMD:R_Array_Concave_2x0603",
        &["NET_A", "NET_B", "NET_C", "NET_D"],
    );

    let resistors: Vec<_> = devices(&bound)
        .filter_map(|d| match d {
            Device::Resistor {
                name, a, b, ohms, ..
            } if name.starts_with("RN1") => Some((name.clone(), *a, *b, *ohms)),
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
        resistors.iter().any(|(_, a, b, _)| {
            (*a == node(x) && *b == node(y)) || (*a == node(y) && *b == node(x))
        })
    };
    assert!(bridges("NET_A", "NET_D"), "element 1 spans pads 1-4");
    assert!(bridges("NET_B", "NET_C"), "element 2 spans pads 2-3");
    assert!(
        !bridges("NET_A", "NET_B") && !bridges("NET_C", "NET_D"),
        "a chip array never pairs neighbouring pads"
    );
    for (name, _, _, ohms) in &resistors {
        assert!(
            (*ohms - 10_000.0).abs() < 1e-6,
            "{name}: each element carries the per-element value, got {ohms}"
        );
    }
    // R5: the pairing is a footprint-family convention, so the even-count path
    // must say so in the bind report, matching the odd-count branch.
    let row = bound.report.row("RN1").expect("RN1 gets a bind row");
    let w = row.warning.as_deref().unwrap_or("");
    assert!(
        w.contains("1-N, 2-(N-1)"),
        "even-pad isolated array must carry the pairing note, got {w:?}"
    );
}

/// Odd pad counts are ambiguous (bussed common could sit at either end). The
/// binder assumes the bussed convention (lowest pad common) but must say so
/// LOUDLY in the bind report, never a silent single-element bind.
#[test]
fn bussed_passive_array_binds_all_elements_with_a_loud_warning() {
    // 5-pad bussed array: pad 1 common, elements to pads 2..5.
    let bound = bind_single_part(
        "RN2",
        "4k7",
        "Resistor_THT:R_Array_SIP5",
        &["COM", "N2", "N3", "N4", "N5"],
    );

    let resistors: Vec<_> = devices(&bound)
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
    let row = bound.report.row("RN2").expect("RN2 gets a bind row");
    let w = row.warning.as_deref().unwrap_or("");
    assert!(
        w.contains("BUSSED"),
        "ambiguous odd-pad array must carry a loud bussed-assumption warning, got {w:?}"
    );
}

/// Bug (binder-r3 #2): negative supply rails ("-5V", "-12V") were never
/// recognised by `power_rail_voltage` (the substring fallback required a
/// leading '+'), so the net got no `SupplyLeg` and silently floated at 0 V.
/// A negative rail must get a supply at the NEGATIVE voltage, and must not
/// be conflated with ground.
#[test]
fn negative_rails_get_supply_legs_at_negative_voltage() {
    // An op-amp-style split supply: R1 from -5V to GND, R2 from -12V to GND
    // (loads so the nets are real).
    let r1 = resistor_0402("R1", "10k", 1, 3);
    let r2 = resistor_0402("R2", "10k", 2, 3);
    let b = board(&[(1, "-5V"), (2, "-12V"), (3, "GND")], vec![r1, r2]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    let leg_volt_bits = |net: &str| -> u64 {
        bound
            .supplies
            .iter()
            .find(|l| l.net_name == net)
            .unwrap_or_else(|| panic!("net {net} must get a SupplyLeg, not float at 0 V"))
            .supply
            .nominal_volts()
            .to_bits()
    };
    let (minus_5, minus_12) = ((-5.0_f64).to_bits(), (-12.0_f64).to_bits());
    assert_eq!(leg_volt_bits("-5V"), minus_5, "-5V rail supplies -5.0 V");
    assert_eq!(
        leg_volt_bits("-12V"),
        minus_12,
        "-12V rail supplies -12.0 V"
    );

    // Not ground: the rail keeps its own (non-ground) node.
    let n5 = bound.node("-5V").expect("-5V interned");
    assert!(!n5.is_ground(), "-5V must never fold onto ground node 0");

    // And the report carries the rail rows at the negative voltage.
    for (name, v) in [("-5V", -5.0), ("-12V", -12.0)] {
        let row = bound
            .report
            .row(&format!("RAIL:{name}"))
            .unwrap_or_else(|| panic!("RAIL:{name} row expected"));
        assert!(
            matches!(row.outcome, BindOutcome::PowerRail { volts } if (volts - v).abs() < 1e-12),
            "{name}: expected PowerRail at {v} V, got {:?}",
            row.outcome
        );
    }
}

/// Bug (binder-r3 #3): electrode-letter pad numbers ("A"/"K" on a diode
/// footprint, no pinfunction, numerically-keyed fallback pad map) were never
/// interpreted as roles, so the diode bound OPEN, a real junction deleted.
/// The pad letters must map to anode/cathode with correct polarity.
#[test]
fn diode_with_electrode_letter_pads_binds_with_correct_polarity() {
    // Footprint-only extraction: pads are NAMED "A"/"K", pinfunctions empty.
    // Value "D" resolves through the engine's 1N4148 fallback, whose pad map
    // is keyed "1"/"2", so before the fix nothing matched and D1 bound open.
    let d = comp(
        "D1",
        "D",
        "Diode_SMD:D_SOD-323",
        vec![pin("A", 1, ""), pin("K", 2, "")],
    );
    let b = board(&[(1, "ANODE_NET"), (2, "CATHODE_NET")], vec![d]);
    let bound = bind_board(&b, &ModelLibrary::builtin());

    // Pad A is the anode, pad K the cathode.
    assert_diode_polarity(&bound, "D1");
}

/// Eagle "P$1"/"P$2" ordinal pads reach the model's numeric pad map after
/// normalisation ("P$1" -> "1" = cathode in the `KiCad` Device:D convention).
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

    // P$2 is the anode, P$1 the cathode (1=K 2=A).
    assert_diode_polarity(&bound, "D2");
}

/// Bug #12: a dual op-amp (LM358) defines `out_a/in_plus_a/in_minus_a` AND the
/// _b channel, but the binder only looked up channel A, one device stamped,
/// channel B's output net left floating with no warning. Both channels must
/// stamp, keyed with the multi-unit `_q<N>` suffix the CI aggregation matches.
#[test]
fn dual_opamp_stamps_one_device_per_channel() {
    // LM358 SOIC-8 pad map (opamp_comparator.toml): 1=out_a, 2=in_minus_a,
    // 3=in_plus_a, 4=vss, 5=in_plus_b, 6=in_minus_b, 7=out_b, 8=vcc.
    let bound = bind_single_part(
        "U1",
        "LM358",
        "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
        &[
            "OUT_A", "INM_A", "INP_A", "GND", "INP_B", "INM_B", "OUT_B", "+3V3",
        ],
    );

    let out_of = |unit: &str| {
        devices(&bound)
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
        devices(&bound)
            .filter(|d| matches!(d, Device::OpAmp { .. }))
            .count(),
        2,
        "a dual op-amp is exactly two OpAmp devices"
    );
}

/// Bug #12 (comparator half): the LM393 dual comparator gets one output stage
/// per channel, same `_q<N>` keying. The LM393 is OPEN COLLECTOR, so that
/// stage is the `Boc_<name>` Behavioral sink (`bind_open_collector`), not a
/// push-pull `Device::Comparator`, and it is the sink on the channel's net.
#[test]
fn dual_comparator_stamps_one_device_per_channel() {
    // LM393 SOIC-8 pad map: 1=out_a, 2=in_minus_a, 3=in_plus_a, 4=vss,
    // 5=in_plus_b, 6=in_minus_b, 7=out_b, 8=vcc.
    let bound = bind_single_part(
        "U2",
        "LM393",
        "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
        &[
            "OUT_A", "INM_A", "INP_A", "GND", "INP_B", "INM_B", "OUT_B", "+5V",
        ],
    );

    let outs: Vec<(String, hauksbee_ir::NodeId)> = devices(&bound)
        .filter_map(|d| match d {
            Device::Behavioral { name, p, .. } => Some((name.clone(), *p)),
            _ => None,
        })
        .collect();
    assert_eq!(
        outs.len(),
        2,
        "a dual comparator is exactly two devices: {outs:?}"
    );
    assert!(outs.contains(&("Boc_U2_q1".to_string(), bound.node("OUT_A").unwrap())));
    assert!(outs.contains(&("Boc_U2_q2".to_string(), bound.node("OUT_B").unwrap())));
}

/// A single-channel op-amp-class part (LMV7219, unsuffixed `out/in_plus`/
/// `in_minus` roles) keeps its pre-fix shape exactly: ONE device under the bare
/// reference, no `_q` suffix.
#[test]
fn single_channel_comparator_keeps_the_bare_reference() {
    // LMV7219 SOT-23-5 pad map: 1=out, 2=vss, 3=in_plus, 4=in_minus, 5=vcc.
    let bound = bind_single_part(
        "U3",
        "LMV7219",
        "Package_TO_SOT_SMD:SOT-23-5",
        &["OUT", "GND", "INP", "INM", "+5V"],
    );
    assert!(
        devices(&bound).any(|d| matches!(d, Device::Comparator { name, .. } if name == "U3")),
        "a single comparator stays under the bare ref"
    );
}

/// Bug #13: the CD74HC4066 quad bilateral switch has FOUR independent gates
/// (`in_out`_<n>a/<n>b + ctrl_<n>); the old single-SPST fall-through bound gate
/// 1 only and silently dropped gates 2..4. All four must stamp, `_s<n>`-keyed,
/// each switching its own pair under its own control net.
#[test]
fn quad_bilateral_switch_stamps_one_vswitch_per_gate() {
    // CD74HC4066 pad map (analog_switch.toml): 1=in_out_1a, 2=in_out_1b,
    // 3=in_out_2a, 4=in_out_2b, 5=ctrl_2, 6=ctrl_3, 7=vss,
    // 8=in_out_3a, 9=in_out_3b, 10=in_out_4a, 11=in_out_4b, 12=ctrl_4, 13=ctrl_1, 14=vcc.
    let bound = bind_single_part(
        "SW2",
        "CD74HC4066",
        "Package_SO:TSSOP-14_4.4x5mm_P0.65mm",
        &[
            "G1A", "G1B", "G2A", "G2B", "CTRL2", "CTRL3", "GND", "G3A", "G3B", "G4A", "G4B",
            "CTRL4", "CTRL1", "+3V3",
        ],
    );

    for n in 1..=4 {
        let (a, bnode, ctrl) = devices(&bound)
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
        devices(&bound)
            .filter(|d| matches!(d, Device::VSwitch { .. }))
            .count(),
        4,
        "a quad bilateral switch is exactly four VSwitch devices"
    );
}

/// Bug #14: bare "VDD" carries no magnitude, on a 3.3 V/1.8 V board it is the
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
