//! Pin-map pinning tests for the built-in logic-IC entries in db/digital.toml.
//!
//! These maps are exactly the kind of data a copy-paste template quietly
//! corrupts: the 74HC00/08/32/86 quad-gate family and the 74HC02 quad NOR
//! share a package but NOT a pin arrangement, and a wrong map binds a gate
//! backwards (the model drives an input net and leaves the real output net
//! undriven). That happened on a real board: U8 (74HC08) gate 4 was mapped
//! 11=A 12=B 13=Y, so the model drove pin 13's MCU-driven net and a quarter
//! of the datapath went dark with no error. Each test below asserts the FULL
//! pin->role map against the industry-standard pinout, so any future template
//! regression fails loudly and names the pin.

use super::{assert_pin_map, expect_model, resolve_value};

/// [`assert_pin_map`] for a package numbered 1..=N in order: `roles[i]` is the
/// role the datasheet gives pin `i + 1`, so the list reads down the package.
fn assert_sequential_pin_map(value: &str, id: &str, roles: &[&str]) {
    let pins: Vec<String> = (1..=roles.len()).map(|n| n.to_string()).collect();
    let expected: Vec<(&str, &str)> = pins
        .iter()
        .map(String::as_str)
        .zip(roles.iter().copied())
        .collect();
    assert_pin_map(&expect_model(value), id, &expected);
}

/// 74HC00/08/32/86 all share the standard TI quad-2-input-gate DIP/SOIC
/// arrangement: left side ascends input-input-output (1A 1B 1Y on 1-3,
/// 2A 2B 2Y on 4-6), GND 7, and the right side MIRRORS that order so the
/// gates there are OUTPUT-first: 3Y 3A 3B on 8-10, 4Y 4A 4B on 11-13,
/// VCC 14. Gate 4's output is pin 11, NOT pin 13. db/digital.toml names these
/// roles a<n>/b<n>/y<n>.
#[test]
fn quad_gate_00_family_pin_maps() {
    // 74HC08 is the field-failure part: gate 4 output MUST be pin 11 (12/13
    // inputs).
    for (value, id) in [
        ("74HC00", "74hc00"),
        ("74HC08", "74hc08"),
        ("74HC32", "74hc32"),
        ("74HC86", "74hc86"),
    ] {
        assert_sequential_pin_map(
            value,
            id,
            &[
                "a1", "b1", "y1", "a2", "b2", "y2", "gnd", "y3", "a3", "b3", "y4", "a4", "b4",
                "vcc",
            ],
        );
    }
}

#[test]
fn hc02_quad_nor_pin_map() {
    // 74HC02 is the one quad gate with a genuinely different arrangement:
    // outputs come FIRST on the left (1Y 1A 1B on 1-3, 2Y 2A 2B on 4-6) and
    // LAST on the right (3A 3B 3Y on 8-10, 4A 4B 4Y on 11-13). Outputs land
    // on 1, 4, 10, 13. Copying the '00 template in either direction is wrong.
    assert_sequential_pin_map(
        "74HC02",
        "74hc02",
        &[
            "g1y", "g1a", "g1b", "g2y", "g2a", "g2b", "gnd", "g3a", "g3b", "g3y", "g4a", "g4b",
            "g4y", "vcc",
        ],
    );
}

#[test]
fn hc04_hex_inverter_pin_map() {
    // Standard '04: left side alternates A,Y ascending (1=1A 2=1Y ... 6=3Y),
    // GND 7; the right side mirrors, so outputs sit on the EVEN pins 8/10/12
    // (4Y 5Y 6Y) with their inputs on 9/11/13, VCC 14.
    assert_sequential_pin_map(
        "74HC04",
        "74hc04",
        &[
            "a1", "y1", "a2", "y2", "a3", "y3", "gnd", "y4", "a4", "y5", "a5", "y6", "a6", "vcc",
        ],
    );
}

#[test]
fn hc27_triple_nor_pin_map() {
    // TI '27 triple 3-input NOR: gate 1 is SPLIT across the package
    // (1A/1B on 1/2, 1C on 12, 1Y on 13); gate 2 sits on 3-6 (2A 2B 2C 2Y);
    // gate 3 is output-first on the right (3Y=8, 3A/3B/3C on 9-11).
    assert_sequential_pin_map(
        "74HC27",
        "74hc27",
        &[
            "a1", "b1", "a2", "b2", "c2", "y2", "gnd", "y3", "a3", "b3", "c3", "c1", "y1", "vcc",
        ],
    );
}

#[test]
fn hc125_quad_tristate_buffer_pin_map() {
    // '125: each buffer is OE,A,Y ascending on the left (1-3, 4-6); the right
    // side mirrors to Y,A,OE (8-10, 11-13). Outputs on 3, 6, 8, 11.
    assert_sequential_pin_map(
        "74HC125",
        "74hc125",
        &[
            "oe_n_1", "a1", "y1", "oe_n_2", "a2", "y2", "gnd", "y3", "a3", "oe_n_3", "y4", "a4",
            "oe_n_4", "vcc",
        ],
    );
}

#[test]
fn hc74_dual_dff_pin_map() {
    // TI '74 dual D flip-flop: FF1 on 1-6 (CLR, D, CLK, PRE, Q, Qbar),
    // GND 7; FF2 mirrors on 8-13 (Qbar, Q, PRE, CLK, D, CLR), VCC 14.
    assert_sequential_pin_map(
        "74HC74",
        "74hc74",
        &[
            "clr_n1", "d1", "clk1", "pre_n1", "q1", "q_n1", "gnd", "q_n2", "q2", "pre_n2", "clk2",
            "d2", "clr_n2", "vcc",
        ],
    );
}

#[test]
fn hc595_shift_register_pin_map() {
    // TI '595 (SCLS041): QB..QH on 1-7, GND 8, QH' cascade on 9, SRCLR 10,
    // SRCLK 11, RCLK 12, OE 13, SER 14, QA 15, VCC 16.
    assert_sequential_pin_map(
        "74HC595",
        "74hc595",
        &[
            "qb",
            "qc",
            "qd",
            "qe",
            "qf",
            "qg",
            "qh",
            "gnd",
            "qh_serial",
            "srclr_n",
            "srclk",
            "rclk",
            "oe_n",
            "ser",
            "qa",
            "vcc",
        ],
    );
}

#[test]
fn hc165_shift_register_pin_map() {
    // TI '165 (SCLS052): SH/LD 1, CLK 2, parallel E..H on 3-6, QHbar 7,
    // GND 8, QH 9, SER 10, parallel A..D on 11-14, CLK INH 15, VCC 16.
    assert_sequential_pin_map(
        "74HC165",
        "74hc165",
        &[
            "pl_n", "clk", "e", "f", "g", "h", "qh_n", "gnd", "qh", "ser", "a", "b", "c", "d",
            "clk_inh", "vcc",
        ],
    );
}

/// BMA423: Bosch Sensortec BST-BMA423-DS000-01 Rev 1.1, Section 7.1 Pin
/// description (LGA-12). The two supply domains are the thing a template
/// corrupts here: VDDIO on 3 with GNDIO on 8, VDD on 7 with GND on 9, and the
/// two grounds are NOT interchangeable pads. SDX/SCX carry the bus vocabulary
/// (`sda`/`scl`) rather than the datasheet spelling, per tests/pin_roles.rs.
#[test]
fn bma423_accelerometer_pin_map() {
    assert_sequential_pin_map(
        "BMA423",
        "bma423",
        &[
            "sdo", "sda", "vddio", "asda", "int1", "int2", "vdd", "gndio", "gnd", "csb", "ascl",
            "scl",
        ],
    );
}

/// The BMA423's numbers, and the fact that its logic levels are the VDDIO
/// FRACTIONS the datasheet publishes rather than round guesses. Section 1
/// gives VIH >= 0.7 x VDDIO, VIL <= 0.3 x VDDIO, VOH >= 0.8 x VDDIO,
/// VOL <= 0.2 x VDDIO; db/digital.toml evaluates them at VDDIO = 3.3 V.
/// Section 2 caps every pin at 4 V, which is the whole point of carrying a
/// rating on a 3.3 V part: a 5 V rail reaching this pin is a fault.
#[test]
fn bma423_carries_the_ratiometric_levels_and_the_four_volt_ceiling() {
    let m = expect_model("BMA423");
    let vddio = 3.3_f64;
    for (key, factor) in [("vih", 0.7), ("vil", 0.3), ("voh", 0.8), ("vol", 0.2)] {
        let got = m.params.get_f64(key).unwrap_or_else(|| panic!("{key} set"));
        assert!(
            (got - factor * vddio).abs() < 5e-3,
            "{key} must be {factor} x VDDIO at 3.3 V ({:.3} V), got {got}",
            factor * vddio,
        );
    }
    // IDD in performance mode, the highest of the sheet's three modes, so a
    // rail budget reads the worst case and not the 3.5 uA suspend figure.
    assert_eq!(m.params.get_f64("supply_static_ua"), Some(150.0));
    assert_eq!(m.params.get_str("supply_pin"), Some("7"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("9"));
    assert_eq!(
        m.ratings.max_voltage_v,
        Some(4.0),
        "Section 2 abs max is 4 V on the supply pins and on any logic pin"
    );
    // A sibling with a different register map and pinout must not borrow this
    // entry's map.
    for sibling in ["BMA400", "BMA456", "BMA425"] {
        assert!(
            resolve_value(sibling)
                .model
                .is_none_or(|m| m.id != "bma423"),
            "{sibling} must not resolve to the BMA423 entry"
        );
    }
}
