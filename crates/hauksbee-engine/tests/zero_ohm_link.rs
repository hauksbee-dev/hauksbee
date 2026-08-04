//! A fitted 0-ohm resistor must bind as a solver-safe link, loudly.
//!
//! Field case (anyshake/explorer): ten literal '0'-value resistors stamped at
//! the raw resistance floor left near-short conductances that wrecked the
//! analog solve's conditioning, non-convergence with nothing naming the
//! cause, an hour of manual bisection. The binder now stamps a 0-ohm value as
//! the same STIFF_R_OHMS link resistance the supply legs use, and says so on
//! the part's bind row.

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::power_supply::STIFF_R_OHMS;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.to_string(),
        net: Some(net),
        function: String::new(),
        kind: "passive".to_string(),
        position: None,
    }
}

fn comp(reference: &str, value: &str, pins: Vec<Pin>) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: String::new(),
        footprint: "Resistor_SMD:R_0402_1005Metric".to_string(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins,
    }
}

/// +5V -- R12 (0R) -- MID -- R2 (1k) -- GND: a fitted zero-ohm link in series
/// with a real load, the exact shape the anyshake board carries tenfold.
fn zero_ohm_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "zero_ohm_test".to_string(),
        nets: vec![
            Net {
                id: 1,
                name: "+5V".to_string(),
            },
            Net {
                id: 2,
                name: "MID".to_string(),
            },
            Net {
                id: 3,
                name: "GND".to_string(),
            },
        ],
        components: vec![
            comp("R12", "0", vec![pin("1", 1), pin("2", 2)]),
            comp("R2", "1k", vec![pin("1", 2), pin("2", 3)]),
        ],
    }
}

#[test]
fn zero_ohm_resistor_stamps_the_link_resistance_with_a_warning() {
    let bound = bind_board(&zero_ohm_board(), &ModelLibrary::builtin());

    let ohms = bound
        .circuit
        .devices
        .iter()
        .find_map(|d| match d {
            Device::Resistor { name, ohms, .. } if name == "R12" => Some(*ohms),
            _ => None,
        })
        .expect("R12 is stamped");
    assert_eq!(
        ohms, STIFF_R_OHMS,
        "a 0-ohm value stamps the shared link resistance, not the raw floor"
    );

    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "R12")
        .expect("R12 has a bind row");
    let warning = row.warning.as_deref().expect("the 0-ohm stamp is loud");
    assert_eq!(
        warning,
        "R12: value '0' is a 0 ohm jumper, bound as a 1 mohm link so the solve stays finite (an infinite conductance would poison the matrix)",
        "the warning names the part and states what was done"
    );

    // The real 1k next to it is untouched and unwarned.
    let r2 = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "R2")
        .expect("R2 has a bind row");
    assert!(r2.warning.is_none(), "R2 must not warn: {:?}", r2.warning);
}

/// The board with the link actually solves: MID sits at the rail (the link is
/// electrically negligible) and no chunk fails.
#[test]
fn zero_ohm_resistor_circuit_solves() {
    let bound = bind_board(&zero_ohm_board(), &ModelLibrary::builtin());
    let mut sched = Scheduler::new(bound, None, SolverOptions::default())
        .expect("scheduler builds for the zero-ohm board");
    let chunk = 1e-4_f64;
    for _ in 0..10 {
        sched.step(chunk);
    }
    assert!(
        sched.analog_valid(),
        "the zero-ohm link must not break convergence: {:?}",
        sched.failed_windows()
    );
    let mid = sched.net_voltage("MID").expect("MID is a live net");
    assert!(
        (mid - 5.0).abs() < 0.01,
        "MID sits at the rail through the milliohm link, got {mid}"
    );
}
