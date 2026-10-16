use super::super::{LintCheck, Severity};
use crate::testutil::{board, part, pin};
use crate::{Component, ExtractedBoard, Pin};

const NETS: [(i64, &str); 7] = [
    (1, "GND"),
    (2, "+5V"),
    (3, "-12V"),
    (4, "VIN"),
    (5, "Net-(D1-K)"),
    (6, "/SIG"),
    (7, "VBUS"),
];

fn named(number: &str, net: i64, function: &str) -> Pin {
    Pin {
        function: function.into(),
        ..pin(number, Some(net))
    }
}

fn count(b: &ExtractedBoard, check: LintCheck) -> usize {
    b.net_lint().of_check(check).count()
}

fn connector(pins: Vec<Pin>) -> ExtractedBoard {
    board(
        &NETS,
        vec![part(
            "J1",
            "USB_C",
            "Connector:USB_C_Receptacle",
            "USB_C_Receptacle",
            pins,
        )],
    )
}

// ---- Supply and ground exchanged on a connector ----

/// Faulty/legal pair: VBUS on ground and GND on VBUS is the exchange; each pin
/// on the rail its name says is legal.
#[test]
fn a_connector_with_supply_and_ground_exchanged_is_serious() {
    let swapped = connector(vec![named("1", 1, "VBUS"), named("2", 7, "GND")]);
    let report = swapped.net_lint();
    let found: Vec<_> = report.of_check(LintCheck::SupplyGroundSwapped).collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::High);
    assert_eq!(found[0].refs, ["J1"]);
    assert!(found[0].message.contains("'VBUS' (pad 1) on 'GND'"));
    assert!(found[0].message.contains("'GND' (pad 2) on 'VBUS'"));
    assert!(found[0].own_remedy().is_some(), "{}", found[0].message);

    let right = connector(vec![named("1", 7, "VBUS"), named("2", 1, "GND")]);
    assert_eq!(count(&right, LintCheck::SupplyGroundSwapped), 0);
}

#[test]
fn a_supply_pin_on_ground_alone_is_flagged_with_its_pad_suffix_stripped() {
    let b = connector(vec![named("A4", 1, "VBUS_A4"), named("A1", 1, "GND_A1")]);
    assert_eq!(count(&b, LintCheck::SupplyGroundSwapped), 1);
    let b = connector(vec![named("1", 1, "+5V"), named("2", 1, "GND")]);
    assert_eq!(count(&b, LintCheck::SupplyGroundSwapped), 1);
    let b = connector(vec![named("1", 2, "GND"), named("2", 2, "VCC")]);
    assert_eq!(count(&b, LintCheck::SupplyGroundSwapped), 1);
}

/// Signals that merely carry a supply token, a ground pin on a negative rail
/// or on an unclassified net, and pins on unclassified nets are never judged.
#[test]
fn signals_negative_rails_and_unknown_nets_stay_silent() {
    let signal = connector(vec![
        named("1", 1, "Data_In_3V3"),
        named("2", 1, "VBUS_DET"),
    ]);
    assert_eq!(count(&signal, LintCheck::SupplyGroundSwapped), 0);
    let negative = connector(vec![named("1", 3, "GND"), named("2", 1, "VEE")]);
    assert_eq!(count(&negative, LintCheck::SupplyGroundSwapped), 0);
    let unknown = connector(vec![named("1", 6, "GND"), named("2", 5, "VCC")]);
    assert_eq!(count(&unknown, LintCheck::SupplyGroundSwapped), 0);
    let unnamed = connector(vec![pin("1", Some(1)), pin("2", Some(7))]);
    assert_eq!(count(&unnamed, LintCheck::SupplyGroundSwapped), 0);
}

#[test]
fn an_unfitted_connector_is_not_judged() {
    let mut b = connector(vec![named("1", 1, "VBUS"), named("2", 7, "GND")]);
    b.components[0].dnp = true;
    assert_eq!(count(&b, LintCheck::SupplyGroundSwapped), 0);
}

#[test]
fn an_ic_with_a_supply_pin_on_ground_is_not_this_check() {
    let b = board(
        &NETS,
        vec![part(
            "U1",
            "MCU",
            "MCU:X",
            "LQFP-48",
            vec![named("1", 1, "VCC"), named("2", 7, "GND")],
        )],
    );
    assert_eq!(count(&b, LintCheck::SupplyGroundSwapped), 0);
}

// ---- Diode across a rail ----

fn diode(value: &str, lib: &str, footprint: &str, pins: Vec<Pin>) -> Component {
    part("D1", value, lib, footprint, pins)
}

fn diode_board(d: Component) -> ExtractedBoard {
    board(&NETS, vec![d])
}

fn tvs(value: &str, anode: i64, cathode: i64) -> ExtractedBoard {
    diode_board(diode(
        value,
        "Device:D_TVS",
        "Diode_SMD:D_SMB",
        vec![named("1", cathode, "K"), named("2", anode, "A")],
    ))
}

/// Faulty/legal pair: a TVS with its anode on the rail is forward-biased; with
/// its cathode on the rail it blocks until breakdown.
#[test]
fn a_tvs_with_its_anode_on_the_rail_is_serious() {
    let reversed = tvs("SMBJ5.0A", 2, 1);
    let report = reversed.net_lint();
    let found: Vec<_> = report.of_check(LintCheck::DiodeReversed).collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::High);
    assert_eq!(found[0].refs, ["D1"]);
    assert_eq!(found[0].nets, ["+5V", "GND"]);
    assert!(found[0].own_remedy().is_some(), "{}", found[0].message);

    assert_eq!(count(&tvs("SMBJ5.0A", 1, 2), LintCheck::DiodeReversed), 0);
}

#[test]
fn a_negative_rail_clamp_is_reversed_when_its_anode_is_at_ground() {
    // Anode at ground, cathode on -12V: forward.
    assert_eq!(count(&tvs("SMBJ12A", 1, 3), LintCheck::DiodeReversed), 1);
    // Anode on -12V, cathode at ground: blocking, the right way for a negative rail.
    assert_eq!(count(&tvs("SMBJ12A", 3, 1), LintCheck::DiodeReversed), 0);
}

#[test]
fn bidirectional_diodes_and_diodes_between_rails_stay_silent() {
    assert_eq!(count(&tvs("SMBJ5.0CA", 2, 1), LintCheck::DiodeReversed), 0);
    assert_eq!(count(&tvs("P6KE15C", 2, 1), LintCheck::DiodeReversed), 0);
    assert_eq!(
        count(&tvs("TVS bidir 5V", 2, 1), LintCheck::DiodeReversed),
        0
    );
    // A diode joining two rails is an OR-ing or a drop: legal either way round.
    assert_eq!(count(&tvs("SS14", 2, 4), LintCheck::DiodeReversed), 0);
    assert_eq!(count(&tvs("SS14", 4, 2), LintCheck::DiodeReversed), 0);
    assert_eq!(count(&tvs("SS14", 2, 7), LintCheck::DiodeReversed), 0);
    // Neither end a known rail.
    assert_eq!(count(&tvs("SS14", 6, 5), LintCheck::DiodeReversed), 0);
}

#[test]
fn a_symbol_naming_a1_a2_or_a_led_or_an_unfitted_diode_is_not_judged() {
    let bidirectional = diode_board(diode(
        "ESD",
        "Device:D_TVS",
        "Diode_SMD:D_SOD-323",
        vec![named("1", 1, "A1"), named("2", 2, "A2")],
    ));
    assert_eq!(count(&bidirectional, LintCheck::DiodeReversed), 0);
    let led = diode_board(diode(
        "LED",
        "Device:LED",
        "LED_SMD:LED_0603",
        vec![named("1", 1, "K"), named("2", 2, "A")],
    ));
    assert_eq!(count(&led, LintCheck::DiodeReversed), 0);
    let mut dnp = tvs("SMBJ5.0A", 2, 1);
    dnp.components[0].dnp = true;
    assert_eq!(count(&dnp, LintCheck::DiodeReversed), 0);
    // A part not called a diode by its designator is left alone.
    let mut other = tvs("SMBJ5.0A", 2, 1);
    other.components[0].reference = "U1".into();
    assert_eq!(count(&other, LintCheck::DiodeReversed), 0);
}

/// With no pin names, only a library that fixes the pads speaks: the `KiCad`
/// `Diode_SMD` footprints (pad 1 cathode) and footprints that spell it.
#[test]
fn unnamed_pads_speak_only_through_the_library_convention() {
    let unnamed = |footprint: &str, pad1: i64, pad2: i64| {
        diode_board(diode(
            "SS14",
            "Device:D_Schottky",
            footprint,
            vec![pin("1", Some(pad1)), pin("2", Some(pad2))],
        ))
    };
    // Pad 1 (cathode) on the rail is the right way round.
    assert_eq!(
        count(&unnamed("Diode_SMD:D_SMA", 2, 1), LintCheck::DiodeReversed),
        0
    );
    assert_eq!(
        count(&unnamed("Diode_SMD:D_SMA", 1, 2), LintCheck::DiodeReversed),
        1
    );
    // A footprint outside the convention says nothing.
    assert_eq!(
        count(&unnamed("D_SMA_custom", 1, 2), LintCheck::DiodeReversed),
        0
    );
    // A footprint that spells out its cathode and anode pads.
    assert_eq!(
        count(
            &unnamed("VENDOR:DO214AA_1(K)-2(A)", 1, 2),
            LintCheck::DiodeReversed
        ),
        1
    );
    assert_eq!(
        count(
            &unnamed("VENDOR:DO214AA_1(K)-2(A)", 2, 1),
            LintCheck::DiodeReversed
        ),
        0
    );
}

// ---- Polarised capacitor ----

fn cap(value: &str, lib: &str, footprint: &str, pins: Vec<Pin>) -> ExtractedBoard {
    board(&NETS, vec![part("C1", value, lib, footprint, pins)])
}

fn radial(plus_pad_net: i64, minus_pad_net: i64) -> ExtractedBoard {
    cap(
        "100u",
        "Device:CP",
        "Capacitor_THT:CP_Radial_D8.0mm_P3.50mm",
        vec![pin("1", Some(plus_pad_net)), pin("2", Some(minus_pad_net))],
    )
}

/// Faulty/legal pair: pad 1 (the + of a CP footprint) on ground with pad 2 on
/// +5V is reversed; the other way round is right.
#[test]
fn a_polarised_capacitor_with_plus_at_the_lower_potential_is_serious() {
    let reversed = radial(1, 2);
    let report = reversed.net_lint();
    let found: Vec<_> = report.of_check(LintCheck::PolarisedCapReversed).collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::High);
    assert_eq!(found[0].refs, ["C1"]);
    assert_eq!(found[0].nets, ["+5V", "GND"]);
    assert!(found[0].own_remedy().is_some(), "{}", found[0].message);

    assert_eq!(count(&radial(2, 1), LintCheck::PolarisedCapReversed), 0);
}

#[test]
fn a_negative_rail_electrolytic_wants_plus_at_ground() {
    assert_eq!(count(&radial(1, 3), LintCheck::PolarisedCapReversed), 0);
    assert_eq!(count(&radial(3, 1), LintCheck::PolarisedCapReversed), 1);
}

#[test]
fn pin_names_and_tantalum_footprints_fix_the_polarity() {
    let named_pins = cap(
        "10u",
        "Device:C_Polarized",
        "Capacitor_SMD:C_0805",
        vec![named("1", 1, "+"), named("2", 2, "-")],
    );
    assert_eq!(count(&named_pins, LintCheck::PolarisedCapReversed), 1);
    let right = cap(
        "10u",
        "Device:C_Polarized",
        "Capacitor_SMD:C_0805",
        vec![named("1", 2, "+"), named("2", 1, "-")],
    );
    assert_eq!(count(&right, LintCheck::PolarisedCapReversed), 0);
    let tantalum = cap(
        "10u",
        "Device:C",
        "Capacitor_Tantalum_SMD:CP_EIA-3216-18_Kemet-A",
        vec![pin("1", Some(1)), pin("2", Some(2))],
    );
    assert_eq!(count(&tantalum, LintCheck::PolarisedCapReversed), 1);
}

/// A ceramic part, a footprint outside the convention, a part that says it is
/// not polarised, two nets of unknown order and two positive rails never fire.
#[test]
fn unpolarised_unknown_and_unordered_capacitors_stay_silent() {
    let ceramic = cap(
        "100n",
        "Device:C",
        "Capacitor_SMD:C_0603_1608Metric",
        vec![pin("1", Some(1)), pin("2", Some(2))],
    );
    assert_eq!(count(&ceramic, LintCheck::PolarisedCapReversed), 0);
    let custom = cap(
        "100u",
        "Device:C",
        "complib:C_ELEC_8x11",
        vec![pin("1", Some(1)), pin("2", Some(2))],
    );
    assert_eq!(count(&custom, LintCheck::PolarisedCapReversed), 0);
    let non_polar = cap(
        "47u NP",
        "Device:CP",
        "Capacitor_THT:CP_Radial_D8.0mm_P3.50mm",
        vec![pin("1", Some(1)), pin("2", Some(2))],
    );
    assert_eq!(count(&non_polar, LintCheck::PolarisedCapReversed), 0);
    let coupling = cap(
        "10u",
        "Device:CP",
        "Capacitor_THT:CP_Radial_D8.0mm_P3.50mm",
        vec![pin("1", Some(5)), pin("2", Some(6))],
    );
    assert_eq!(count(&coupling, LintCheck::PolarisedCapReversed), 0);
    // Two positive rails: the names do not fix the order.
    assert_eq!(count(&radial(2, 4), LintCheck::PolarisedCapReversed), 0);
    assert_eq!(count(&radial(4, 2), LintCheck::PolarisedCapReversed), 0);
}
