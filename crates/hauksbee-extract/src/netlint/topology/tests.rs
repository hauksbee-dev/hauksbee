use super::super::{LintCheck, Severity};
use crate::testutil::{board, part, pin};
use crate::{Component, ExtractedBoard, Pin};

fn named(number: &str, net: i64, function: &str) -> Pin {
    Pin {
        function: function.into(),
        ..pin(number, Some(net))
    }
}

fn two(reference: &str, value: &str, lib: &str, a: i64, b: i64) -> Component {
    part(
        reference,
        value,
        lib,
        lib,
        vec![pin("1", Some(a)), pin("2", Some(b))],
    )
}

fn header(reference: &str, nets: &[i64]) -> Component {
    let pins = nets
        .iter()
        .enumerate()
        .map(|(i, n)| pin(&(i + 1).to_string(), Some(*n)))
        .collect();
    part(reference, "Conn", "Connector:Conn", "PinHeader_1x02", pins)
}

/// GND (1) feeds an IC; `SENSOR_GND` (2) carries a cap and two connectors.
fn island_board(extra: Vec<Component>) -> ExtractedBoard {
    let mut parts = vec![
        part(
            "U1",
            "MCU",
            "MCU:X",
            "LQFP-48",
            vec![pin("1", Some(1)), pin("2", Some(4)), pin("3", Some(3))],
        ),
        two("C1", "100n", "Device:C", 4, 1),
        two("C2", "100n", "Device:C", 3, 2),
        header("J1", &[3, 2]),
        header("J2", &[2]),
    ];
    parts.extend(extra);
    board(
        &[
            (1, "GND"),
            (2, "/SENSOR_GND"),
            (3, "SIG"),
            (4, "+3V3"),
            (5, "/GND_ISO"),
        ],
        parts,
    )
}

fn count(b: &ExtractedBoard, check: LintCheck) -> usize {
    b.net_lint().of_check(check).count()
}

#[test]
fn a_ground_net_never_joined_to_ground_is_serious() {
    let report = island_board(vec![]).net_lint();
    let found: Vec<_> = report.of_check(LintCheck::GroundIsland).collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::High);
    assert_eq!(found[0].nets, ["/SENSOR_GND"]);
}

#[test]
fn a_link_ferrite_or_bridging_part_joins_the_grounds() {
    for joint in [
        two("R9", "0", "Device:R", 2, 1),
        two("FB1", "600R@100MHz", "Device:FerriteBead", 2, 1),
        part(
            "U2",
            "ISO",
            "Isolator:X",
            "SOIC-8",
            vec![pin("1", Some(1)), pin("8", Some(2))],
        ),
        // A ball-grid footprint name must not make an IC read as a connector.
        part(
            "U3",
            "FPGA",
            "FPGA:X",
            "BGA-121_9.0x9.0mm_Layout11x11_P0.8mm",
            vec![pin("A1", Some(1)), pin("B2", Some(2)), pin("C3", Some(3))],
        ),
    ] {
        let reference = joint.reference.clone();
        let b = island_board(vec![joint]);
        assert_eq!(count(&b, LintCheck::GroundIsland), 0, "{reference}");
    }
}

#[test]
fn a_capacitor_or_connector_is_not_a_join_and_a_named_domain_is_left_alone() {
    let b = island_board(vec![
        two("C9", "1n", "Device:C", 2, 1),
        header("J9", &[2, 1]),
    ]);
    assert_eq!(count(&b, LintCheck::GroundIsland), 1);
    let iso = island_board(vec![
        two("C8", "100n", "Device:C", 3, 5),
        header("J8", &[5]),
    ]);
    let found: Vec<_> = iso
        .net_lint()
        .of_check(LintCheck::GroundIsland)
        .cloned()
        .collect();
    assert!(found.iter().all(|f| f.nets != ["/GND_ISO"]), "{found:?}");
}

#[test]
fn a_connector_on_lone_drawn_nets_is_a_warning() {
    let nets = [
        (1, "GND"),
        (2, "/LINE_A"),
        (3, "/LINE_B"),
        (4, "unconnected-(J3-Pin_1-Pad1)"),
        (5, "unconnected-(J3-Pin_2-Pad2)"),
    ];
    let parts = vec![
        two("C1", "100n", "Device:C", 1, 1),
        header("J7", &[2, 3]),
        header("J3", &[4, 5]),
        part(
            "R1",
            "10k",
            "Device:R",
            "R_0402",
            vec![pin("1", Some(2)), pin("2", Some(1))],
        ),
    ];
    // R1 shares /LINE_A, so J7 has a live pin and stays quiet.
    assert_eq!(
        count(&board(&nets, parts.clone()), LintCheck::UnwiredConnector),
        0
    );
    let lone = board(&nets, parts[..3].to_vec());
    let found: Vec<_> = lone
        .net_lint()
        .of_check(LintCheck::UnwiredConnector)
        .cloned()
        .collect();
    assert_eq!(
        found.len(),
        1,
        "explicit no-connects on J3 are not a fault: {found:?}"
    );
    assert_eq!(found[0].refs, ["J7"]);
    assert_eq!(found[0].severity, Severity::Medium);
}

#[test]
fn an_led_with_anode_at_ground_and_cathode_at_a_rail_is_reversed() {
    let led = |a: i64, k: i64| {
        part(
            "D1",
            "LED",
            "Device:LED",
            "LED_0603",
            vec![named("1", k, "K"), named("2", a, "A")],
        )
    };
    let nets = [(1, "GND"), (2, "+5V"), (3, "Net-(D1-K)"), (4, "Net-(D1-A)")];
    let reversed = board(&nets, vec![led(1, 3), two("R1", "1k", "Device:R", 3, 2)]);
    assert_eq!(count(&reversed, LintCheck::LedReversed), 1);
    let forward = board(&nets, vec![led(4, 1), two("R1", "1k", "Device:R", 4, 2)]);
    assert_eq!(count(&forward, LintCheck::LedReversed), 0);
    let unnamed = board(
        &nets,
        vec![
            two("D1", "LED", "Device:LED", 3, 1),
            two("R1", "1k", "Device:R", 3, 2),
        ],
    );
    assert_eq!(
        count(&unnamed, LintCheck::LedReversed),
        0,
        "no pin names, no claim"
    );
}
