//! A part subject's causal path reaches up its supply chain and never through
//! ground.
//!
//! A claim on a part (a regulator's junction temperature, the output an MCU
//! printed) depends on the nets the part touches and on what powers it: the
//! rails upstream through its regulators, switches and series conductors, the
//! parts loading those rails (they set the current through the chain, which
//! matters wherever the source limits current), and the parts controlling the
//! chain (a switch's gate driver, a regulator's enable). An open part there
//! undermines the claim. An open part that only shares ground with the
//! subject does not.

use hauksbee_engine::{bind_board, BoardEvidence};
use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::evidence::{EvidenceMap, EvidenceStatus, RunDate};
use hauksbee_models::ModelLibrary;

use crate::support::pin;

const ENTRY: i64 = 1;
const SWITCHED: i64 = 2;
const OUT: i64 = 3;
const GND: i64 = 4;
const GATE: i64 = 5;
const CTRL_VIN: i64 = 6;
const ANT: i64 = 7;
const CTRL_IO: i64 = 8;

fn part(reference: &str, value: &str, pins: Vec<Pin>) -> Component {
    crate::support::component(
        reference.into(),
        value.into(),
        String::new(),
        String::new(),
        pins,
    )
}

fn passive(reference: &str, value: &str, a: i64, b: i64) -> Component {
    let pins = vec![
        pin("1", Some(a), "", "passive"),
        pin("2", Some(b), "", "passive"),
    ];
    part(reference, value, pins)
}

/// How the open controller U3 meets the chain.
#[derive(Clone, Copy)]
struct Controller {
    /// U3 drives the load switch's gate.
    drives_gate: bool,
    /// U3 takes its supply from `+5V` through an RC filter (R2, C3).
    fed_from_chain: bool,
}

const NEITHER: Controller = Controller {
    drives_gate: false,
    fed_from_chain: false,
};

/// A 3.3 V LDO (U1) fed from `+5V` through a high-side P-FET load switch (Q1)
/// onto `+5V_SW`, with an open controller (U3, no model) placed per
/// `controller`. `extra` adds parts to the board.
fn board(controller: Controller, extra: Vec<Component>) -> ExtractedBoard {
    let net = |id, name: &str| crate::support::net(id, name.into());
    let gate = if controller.drives_gate {
        GATE
    } else {
        CTRL_IO
    };
    let feed = if controller.fed_from_chain {
        ENTRY
    } else {
        CTRL_IO
    };
    let mut components = vec![
        part(
            "U1",
            "AMS1117-3.3",
            vec![
                pin("1", Some(GND), "GND", "power_in"),
                pin("2", Some(OUT), "VO", "power_out"),
                pin("3", Some(SWITCHED), "VI", "power_in"),
            ],
        ),
        passive("C1", "22u", OUT, GND),
        passive("C2", "10u", SWITCHED, GND),
        part(
            "Q1",
            "AO3401A",
            vec![
                pin("1", Some(GATE), "G", "input"),
                pin("2", Some(ENTRY), "S", "passive"),
                pin("3", Some(SWITCHED), "D", "passive"),
            ],
        ),
        passive("R1", "100k", ENTRY, GATE),
        part(
            "U3",
            "LOADSW-CTRL-X1",
            vec![
                pin("1", Some(CTRL_VIN), "VIN", "power_in"),
                pin("2", Some(gate), "GATE", "output"),
                pin("3", Some(GND), "GND", "power_in"),
            ],
        ),
        passive("R2", "10", feed, CTRL_VIN),
        passive("C3", "1u", CTRL_VIN, GND),
        passive("R3", "1k", OUT, GND),
    ];
    components.extend(extra);
    ExtractedBoard {
        name: "supply-chain-scope".into(),
        nets: vec![
            net(ENTRY, "+5V"),
            net(SWITCHED, "+5V_SW"),
            net(OUT, "+3V3"),
            net(GND, "GND"),
            net(GATE, "SW_GATE"),
            net(CTRL_VIN, "CTRL_VIN"),
            net(ANT, "ANT"),
            net(CTRL_IO, "CTRL_IO"),
        ],
        components,
    }
}

/// The claim `Tj(U1) <= 125 C` as the CI runner maps it, after checking each
/// of `open` really is an open part on this board, so a test that finds one
/// off the path is not passing because it was never open.
fn u1_claim(board: &ExtractedBoard, open: &[&str]) -> EvidenceMap {
    let bound = bind_board(board, &ModelLibrary::builtin());
    let evidence =
        BoardEvidence::from_bound(board, &bound.report, &[], RunDate::from_epoch_days(20_666))
            .unwrap();
    for part in open {
        let id = format!("open-part:{part}");
        let known = evidence.assumptions().iter().any(|a| a.id().as_str() == id);
        assert!(known, "{part} is not open: {:?}", evidence.assumptions());
    }
    evidence
        .ci_assertion_map("Tj(U1) <= 125 C", &[], &["U1".into()], None, None)
        .unwrap()
}

fn on_path(map: &EvidenceMap, part: &str) -> bool {
    let id = format!("open-part:{part}");
    map.assumptions().iter().any(|a| a.as_str() == id)
}

/// The binder sees the switched branch and the regulator feeding `+3V3` from it.
#[test]
fn the_binder_records_the_switched_branch_and_the_regulator_feed() {
    let bound = bind_board(&board(NEITHER, Vec::new()), &ModelLibrary::builtin());
    let groups = &bound.report.supply_groups;
    let switched = groups.iter().find(|g| g.nets.contains("+5V_SW"));
    let switched = switched.unwrap_or_else(|| panic!("no group holds +5V_SW: {groups:?}"));
    assert!(switched.nets.contains("+5V"), "{switched:?}");
    assert!(switched.passes.contains("Q1"), "{switched:?}");
    let out = groups.iter().find(|g| g.nets.contains("+3V3")).unwrap();
    let feed = ("U1".to_string(), "+5V_SW".to_string());
    assert!(out.feeds.contains(&feed), "{out:?}");
}

/// (a) A linear regulator fed through a load switch whose controller is open:
/// the regulator's junction-temperature claim stays undermined, and names the
/// controller. The same controller on none of the chain's pins is off it.
#[test]
fn an_open_controller_on_the_load_switch_feeding_a_regulator_undermines_it() {
    let gate = Controller {
        drives_gate: true,
        fed_from_chain: false,
    };
    let map = u1_claim(&board(gate, Vec::new()), &["U3"]);
    assert_eq!(map.status(), EvidenceStatus::Undermined, "{map:?}");
    assert!(on_path(&map, "U3"), "{:?}", map.assumptions());
    let map = u1_claim(&board(NEITHER, Vec::new()), &["U3"]);
    assert!(!on_path(&map, "U3"), "{:?}", map.assumptions());
}

/// (b) An open part that shares only ground with the regulator is off its
/// path.
#[test]
fn an_open_part_sharing_only_ground_stays_off_the_path() {
    let trimmer = passive("C9", "TRIM-CAP-X1", ANT, GND);
    let antenna = part(
        "U9",
        "ANTENNA-X1",
        vec![
            pin("1", Some(ANT), "FEED", "passive"),
            pin("2", Some(GND), "GND", "passive"),
        ],
    );
    let map = u1_claim(&board(NEITHER, vec![trimmer, antenna]), &["U9", "C9"]);
    assert!(!on_path(&map, "U9"), "{:?}", map.assumptions());
    assert!(!on_path(&map, "C9"), "{:?}", map.assumptions());
}

/// (c) An open load on the rail upstream of the switch draws from the same
/// source as the regulator: when that source limits current, the load decides
/// how much is left, so it is on the regulator's path.
#[test]
fn an_open_load_on_an_upstream_rail_is_on_the_path() {
    let motor = part(
        "U8",
        "MOTOR-DRV-X1",
        vec![
            pin("1", Some(ENTRY), "VM", "power_in"),
            pin("2", Some(CTRL_IO), "IN1", "input"),
            pin("3", Some(GND), "GND", "power_in"),
        ],
    );
    let map = u1_claim(&board(NEITHER, vec![motor]), &["U8"]);
    assert_eq!(map.status(), EvidenceStatus::Undermined, "{map:?}");
    assert!(on_path(&map, "U8"), "{:?}", map.assumptions());
}

/// A part powered from a chain rail through an RC filter loads that rail even
/// though the filter's resistor keeps it a separate net: the controller is on
/// the path through its own supply alone.
#[test]
fn a_part_fed_from_the_chain_through_a_filter_is_on_the_path() {
    let fed = Controller {
        drives_gate: false,
        fed_from_chain: true,
    };
    let map = u1_claim(&board(fed, Vec::new()), &["U3"]);
    assert!(on_path(&map, "U3"), "{:?}", map.assumptions());
}
