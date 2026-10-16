//! A fitted 0-ohm resistor must bind as a solver-safe link, loudly.
//!
//! Field case (anyshake/explorer): ten literal '0'-value resistors stamped at
//! the raw resistance floor left near-short conductances that wrecked the
//! analog solve's conditioning, non-convergence with nothing naming the
//! cause, an hour of manual bisection. The binder now stamps a 0-ohm value as
//! the same `STIFF_R_OHMS` link resistance the supply legs use, and says so on
//! the part's bind row.

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::power_supply::STIFF_R_OHMS;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

use crate::support::passive_pin as pin;
use crate::support::r0402 as comp;

/// +5V -- R12 (0R) -- MID -- R2 (1k) -- GND: a fitted zero-ohm link in series
/// with a real load, the exact shape the anyshake board carries tenfold.
fn zero_ohm_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "zero_ohm_test".to_string(),
        nets: vec![
            crate::support::net(1, "+5V".to_string()),
            crate::support::net(2, "MID".to_string()),
            crate::support::net(3, "GND".to_string()),
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
        ohms.to_bits(),
        STIFF_R_OHMS.to_bits(),
        "a 0-ohm value stamps the shared link resistance, not the raw floor"
    );

    let row = bound.report.row("R12").expect("R12 has a bind row");
    let warning = row.warning.as_deref().expect("the 0-ohm stamp is loud");
    assert_eq!(
        warning,
        "R12: value '0' is a 0 ohm jumper, bound as a 1 mohm link so the solve stays finite (an infinite conductance would poison the matrix)",
        "the warning names the part and states what was done"
    );

    // The real 1k next to it is untouched and unwarned.
    let r2 = bound.report.row("R2").expect("R2 has a bind row");
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

/// +5V -- R5 (`value`) -- MID -- R2 (1k) -- GND: a sense resistor of the given value.
fn shunt_board(value: &str) -> ExtractedBoard {
    ExtractedBoard {
        name: "shunt_test".to_string(),
        nets: vec![
            crate::support::net(1, "+5V".to_string()),
            crate::support::net(2, "MID".to_string()),
            crate::support::net(3, "GND".to_string()),
        ],
        components: vec![
            comp("R5", value, vec![pin("1", 1), pin("2", 2)]),
            comp("R2", "1k", vec![pin("1", 2), pin("2", 3)]),
        ],
    }
}

fn stamped_ohms(bound: &hauksbee_engine::BoundBoard, refdes: &str) -> f64 {
    bound
        .circuit
        .devices
        .iter()
        .find_map(|d| match d {
            Device::Resistor { name, ohms, .. } if name == refdes => Some(*ohms),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{refdes} is stamped"))
}

/// A 15 uohm shunt stamped 66 kS and stopped the whole board's DC solve (the LU lost the
/// gmin-sized pivots around it) while a literal 0 solved. Any sub-milliohm value is bound at the
/// milliohm conditioning floor and the bind row says so.
#[test]
fn a_sub_milliohm_resistor_binds_at_the_conditioning_floor_loudly() {
    for value in ["15uR", "10u", "1e-9"] {
        let bound = bind_board(&shunt_board(value), &ModelLibrary::builtin());
        assert_eq!(
            stamped_ohms(&bound, "R5").to_bits(),
            STIFF_R_OHMS.to_bits(),
            "{value} binds at the conditioning floor"
        );
        let warning = bound
            .report
            .row("R5")
            .and_then(|r| r.warning.clone())
            .unwrap_or_else(|| panic!("{value}: the substitution must be on the bind row"));
        assert!(
            warning.contains("R5")
                && warning.contains("conditioning floor")
                && warning.contains("1 mohm"),
            "{value}: {warning}"
        );
    }
}

/// Legal control: a milliohm-or-larger sense resistor is stamped as written and silent.
#[test]
fn a_milliohm_or_larger_resistor_is_stamped_as_written_and_silent() {
    for (value, want) in [("1m", 1e-3), ("5m", 5e-3), ("15mR", 15e-3), ("0.1", 0.1)] {
        let bound = bind_board(&shunt_board(value), &ModelLibrary::builtin());
        let got = stamped_ohms(&bound, "R5");
        assert!((got - want).abs() < 1e-12, "{value}: stamped {got}");
        assert!(
            bound.report.row("R5").is_some_and(|r| r.warning.is_none()),
            "{value}: no warning expected"
        );
    }
}

/// The micro-ohm board solves and sits at the rail, like the milliohm control.
#[test]
fn a_micro_ohm_shunt_board_solves_like_the_milliohm_one() {
    for value in ["15uR", "1e-9", "15mR"] {
        let bound = bind_board(&shunt_board(value), &ModelLibrary::builtin());
        let mut sched = Scheduler::new(bound, None, SolverOptions::default())
            .expect("scheduler builds for the shunt board");
        for _ in 0..10 {
            sched.step(1e-4);
        }
        assert!(
            sched.analog_valid(),
            "{value}: {:?}",
            sched.failed_windows()
        );
        let mid = sched.net_voltage("MID").expect("MID is a live net");
        assert!((mid - 5.0).abs() < 0.01, "{value}: MID at {mid} V");
    }
}
