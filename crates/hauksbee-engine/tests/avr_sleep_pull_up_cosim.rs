//! A firmware that reads DIP switches with the internal pull-ups on and goes to
//! sleep without turning them off leaks `Vdd / R_pu` through every closed
//! switch for the whole sleep: 66 to 165 uA per pin on an `ATmega328P` at
//! 3.3 V, against a power-down current of microamps. The pull-up lives inside
//! the chip, not in the analog circuit, so the run must account it from the
//! firmware's register state and the solved nets, and say so.
//!
//! Board: a bare `ATmega328P` (DIP-28) on a 3.3 V rail, PD2 (pad 4) and PD3
//! (pad 5) on nets `DIP1` and `DIP2`, each to ground through one contact of a
//! DIP switch (live toggles `SW1_1` / `SW1_2`). The firmware is hand-assembled
//! by `hauksbee_testkit::avr_firmware::pull_up_sleep_image`.
//!
//! - Positive: pull-ups left on, both contacts closed: a finding naming both
//!   pins, nets and switch contacts, 2 x 3.3 V / 20 kOhm = 330 uA at worst.
//! - Silent controls: the same firmware with `PORTD = 0` before sleep (the
//!   fix), and the same fault firmware with both contacts open.

#![cfg(feature = "avr")]

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::reports::cosim::build_cosim_json;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_frontdoor_api::protocol::LivePeripheralSpec;
use hauksbee_models::ModelLibrary;
use hauksbee_testkit::avr_firmware::{avr_elf, pull_up_sleep_image};

/// PD2 = DIP-28 pad 4, PD3 = pad 5. Power on pads 7/20, ground on 8/22.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "RESET")
  (net 4 "DIP1")
  (net 5 "DIP2")

  (module Package_DIP:DIP-28_W7.62mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 1  smd rect (at -3 0) (net 3 "RESET"))
    (pad 4  smd rect (at 3 0) (net 4 "DIP1"))
    (pad 5  smd rect (at 3 1) (net 5 "DIP2"))
    (pad 7  smd rect (at -3 1) (net 2 "+3V3"))
    (pad 8  smd rect (at -3 2) (net 1 "GND"))
    (pad 20 smd rect (at -3 3) (net 2 "+3V3"))
    (pad 22 smd rect (at -3 4) (net 1 "GND"))
  )

  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "RESET"))
    (pad 2 smd rect (at 1 0) (net 2 "+3V3"))
  )
)
"#;

/// Run the board with `clear_before_sleep` firmware and the DIP switch's two
/// contacts `closed` (or open) for 30 ms.
fn run(clear_before_sleep: bool, closed: bool) -> (HauksbeeEngine, tempfile::TempDir) {
    run_with_budget(clear_before_sleep, closed, None)
}

/// [`run`] with a stated sleep budget (A).
fn run_with_budget(
    clear_before_sleep: bool,
    closed: bool,
    budget_a: Option<f64>,
) -> (HauksbeeEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let fw = dir.path().join("sleep.elf");
    std::fs::write(
        &fw,
        avr_elf(&pull_up_sleep_image(0b0000_1100, clear_before_sleep)),
    )
    .unwrap();

    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine");
    for (id, net) in [("SW1_1", "DIP1"), ("SW1_2", "DIP2")] {
        engine
            .attach_peripheral(LivePeripheralSpec {
                id: id.into(),
                kind: "toggle".into(),
                net: net.into(),
                to: Some("GND".into()),
                offset: None,
                bounce_ms: None,
                initial: Some(if closed { 1.0 } else { 0.0 }),
            })
            .expect("attach a DIP switch contact");
    }
    engine.scheduler_mut().set_sleep_budget_a(budget_a);
    for _ in 0..30 {
        engine.step(1e-3);
    }
    (engine, dir)
}

#[test]
fn pull_ups_left_on_through_sleep_leak_through_the_closed_switches() {
    let (engine, _dir) = run(false, true);
    assert!(
        engine.scheduler().sleeping_mcus() == ["U1"],
        "the firmware must have reached sleep"
    );
    let accounts = engine.scheduler().sleep_currents();
    assert_eq!(accounts.len(), 1, "one MCU slept: {accounts:?}");
    let a = &accounts[0];
    assert_eq!(a.mcu_ref, "U1");
    assert_eq!(a.mode.as_deref(), Some("Power-down"));
    assert!((a.vdd_v - 3.3).abs() < 0.1, "Vdd {} V", a.vdd_v);
    let pins: Vec<&str> = a.pins.iter().map(|p| p.pin.pin.as_str()).collect();
    assert_eq!(pins, ["PD2", "PD3"], "{a:?}");
    assert_eq!(a.pins[0].pin.net, "DIP1");
    assert_eq!(a.pins[1].pin.net, "DIP2");
    assert_eq!(a.pins[0].pin.pad.as_deref(), Some("4"));
    assert!(a.pins[0].pin.low_path[0].starts_with("SW1_1"), "{a:?}");
    assert!(a.pins[1].pin.low_path[0].starts_with("SW1_2"), "{a:?}");

    // 2 x 3.3 V / 20 kOhm = 330 uA at the card's smallest pull-up resistance,
    // 2 x 3.3 V / 50 kOhm = 132 uA at its largest.
    let (hi, lo) = (a.total_max_a.unwrap(), a.total_min_a.unwrap());
    assert!((hi - 330e-6).abs() < 20e-6, "worst case {hi} A");
    assert!((lo - 132e-6).abs() < 8e-6, "best case {lo} A");
    assert!(a.flagged() && a.over_card && !a.over_budget);
    assert_eq!(a.card_max_a, Some(5e-6));

    let message = a.message().expect("flagged account has a warning");
    for needle in [
        "U1",
        "PD2",
        "PD3",
        "pad 4",
        "pad 5",
        "DIP1",
        "DIP2",
        "SW1_1",
        "SW1_2",
        "PORTxn = 0",
        "pull-up",
    ] {
        assert!(message.contains(needle), "missing {needle:?} in {message}");
    }
}

#[test]
fn the_report_json_and_warnings_carry_the_finding() {
    let (engine, _dir) = run(false, true);
    let json = build_cosim_json(&engine, false)
        .unwrap()
        .expect("an MCU ran, so there is a co-sim report");
    assert_eq!(json.sleep_current.len(), 1);
    let s = &json.sleep_current[0];
    assert!(s.flagged && s.over_card);
    assert_eq!(s.pins.len(), 2);
    assert!(s.message.as_deref().unwrap().contains("PORTxn = 0"));
    let value = serde_json::to_value(&json).unwrap();
    assert!(
        value["sleep_current"][0]["total_max_a"].as_f64().unwrap() > 300e-6,
        "{value}"
    );
    // The same sentence reaches the shared coverage notes.
    assert!(engine
        .scheduler()
        .sleep_current_warnings()
        .iter()
        .any(|w| w.contains("PD2") && w.contains("PD3")));
}

#[test]
fn a_stated_budget_is_checked_against_the_worst_case() {
    // Above the worst case: the card-relative check still fires, the budget does not.
    let (engine, _dir) = run_with_budget(false, true, Some(1e-3));
    let a = engine.scheduler().sleep_currents().remove(0);
    assert!(!a.over_budget && a.over_card, "{a:?}");
    // Below it.
    let (engine, _dir) = run_with_budget(false, true, Some(100e-6));
    let a = engine.scheduler().sleep_currents().remove(0);
    assert!(a.over_budget, "{a:?}");
    assert!(a.message().unwrap().contains("budget of 100 uA"));
}

#[test]
fn clearing_the_pull_ups_before_sleep_is_silent() {
    let (engine, _dir) = run(true, true);
    assert!(
        engine.scheduler().sleeping_mcus() == ["U1"],
        "the control firmware must also reach sleep"
    );
    assert!(
        engine.scheduler().sleep_currents().is_empty(),
        "no pull-up is on: {:?}",
        engine.scheduler().sleep_currents()
    );
    let json = build_cosim_json(&engine, false).unwrap().unwrap();
    assert!(json.sleep_current.is_empty());
}

#[test]
fn open_switches_leave_nothing_to_leak_through() {
    let (engine, _dir) = run(false, false);
    assert!(
        engine.scheduler().sleeping_mcus() == ["U1"],
        "the firmware must have reached sleep"
    );
    assert!(
        engine.scheduler().sleep_currents().is_empty(),
        "an open contact is no path to ground: {:?}",
        engine.scheduler().sleep_currents()
    );
}
