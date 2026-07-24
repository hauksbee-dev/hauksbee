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

use hauksbee_models::{ComponentQuery, ModelLibrary};

/// Resolve `value` through the builtin library and assert the entry id and the
/// exact, complete pin->role map (no extra pins, no missing pins, no swaps).
fn assert_pin_map(value: &str, id: &str, expected: &[(&str, &str)]) {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery { value: Some(value.into()), ..Default::default() };
    let m = lib
        .resolve(&q)
        .model
        .unwrap_or_else(|| panic!("{value} did not resolve to any model"));
    assert_eq!(m.id, id, "{value} resolved to the wrong model id");
    for (pin, role) in expected {
        assert_eq!(
            m.pins.get(*pin).map(String::as_str),
            Some(*role),
            "{id}: pin {pin} must be role {role:?} (datasheet pinout), got {:?}",
            m.pins.get(*pin),
        );
    }
    assert_eq!(
        m.pins.len(),
        expected.len(),
        "{id}: pin map has {} entries, datasheet package has {}",
        m.pins.len(),
        expected.len(),
    );
}

/// 74HC00/08/32/86 all share the standard TI quad-2-input-gate DIP/SOIC
/// arrangement: left side ascends input-input-output (1A 1B 1Y on 1-3,
/// 2A 2B 2Y on 4-6), GND 7, and the right side MIRRORS that order so the
/// gates there are OUTPUT-first: 3Y 3A 3B on 8-10, 4Y 4A 4B on 11-13,
/// VCC 14. Gate 4's output is pin 11, NOT pin 13.
fn quad_gate_00_family(role: fn(u8, char) -> String) -> Vec<(String, String)> {
    vec![
        ("1", role(1, 'a')), ("2", role(1, 'b')), ("3", role(1, 'y')),
        ("4", role(2, 'a')), ("5", role(2, 'b')), ("6", role(2, 'y')),
        ("7", "gnd".into()),
        ("8", role(3, 'y')), ("9", role(3, 'a')), ("10", role(3, 'b')),
        ("11", role(4, 'y')), ("12", role(4, 'a')), ("13", role(4, 'b')),
        ("14", "vcc".into()),
    ]
    .into_iter()
    .map(|(p, r)| (p.to_string(), r))
    .collect()
}

fn assert_quad_gate_00_family(value: &str, id: &str) {
    // db/digital.toml names these roles a<n>/b<n>/y<n>.
    let expected = quad_gate_00_family(|n, sig| format!("{sig}{n}"));
    let expected_refs: Vec<(&str, &str)> =
        expected.iter().map(|(p, r)| (p.as_str(), r.as_str())).collect();
    assert_pin_map(value, id, &expected_refs);
}

#[test]
fn hc00_quad_nand_pin_map() {
    assert_quad_gate_00_family("74HC00", "74hc00");
}

#[test]
fn hc08_quad_and_pin_map() {
    // The field-failure part: gate 4 output MUST be pin 11 (12/13 inputs).
    assert_quad_gate_00_family("74HC08", "74hc08");
}

#[test]
fn hc32_quad_or_pin_map() {
    assert_quad_gate_00_family("74HC32", "74hc32");
}

#[test]
fn hc86_quad_xor_pin_map() {
    assert_quad_gate_00_family("74HC86", "74hc86");
}

#[test]
fn hc02_quad_nor_pin_map() {
    // 74HC02 is the one quad gate with a genuinely different arrangement:
    // outputs come FIRST on the left (1Y 1A 1B on 1-3, 2Y 2A 2B on 4-6) and
    // LAST on the right (3A 3B 3Y on 8-10, 4A 4B 4Y on 11-13). Outputs land
    // on 1, 4, 10, 13. Copying the '00 template in either direction is wrong.
    assert_pin_map("74HC02", "74hc02", &[
        ("1", "g1y"), ("2", "g1a"), ("3", "g1b"),
        ("4", "g2y"), ("5", "g2a"), ("6", "g2b"),
        ("7", "gnd"),
        ("8", "g3a"), ("9", "g3b"), ("10", "g3y"),
        ("11", "g4a"), ("12", "g4b"), ("13", "g4y"),
        ("14", "vcc"),
    ]);
}

#[test]
fn hc04_hex_inverter_pin_map() {
    // Standard '04: left side alternates A,Y ascending (1=1A 2=1Y ... 6=3Y),
    // GND 7; the right side mirrors, so outputs sit on the EVEN pins 8/10/12
    // (4Y 5Y 6Y) with their inputs on 9/11/13, VCC 14.
    assert_pin_map("74HC04", "74hc04", &[
        ("1", "a1"), ("2", "y1"),
        ("3", "a2"), ("4", "y2"),
        ("5", "a3"), ("6", "y3"),
        ("7", "gnd"),
        ("8", "y4"), ("9", "a4"),
        ("10", "y5"), ("11", "a5"),
        ("12", "y6"), ("13", "a6"),
        ("14", "vcc"),
    ]);
}

#[test]
fn hc27_triple_nor_pin_map() {
    // TI '27 triple 3-input NOR: gate 1 is SPLIT across the package
    // (1A/1B on 1/2, 1C on 12, 1Y on 13); gate 2 sits on 3-6 (2A 2B 2C 2Y);
    // gate 3 is output-first on the right (3Y=8, 3A/3B/3C on 9-11).
    assert_pin_map("74HC27", "74hc27", &[
        ("1", "a1"), ("2", "b1"),
        ("3", "a2"), ("4", "b2"), ("5", "c2"), ("6", "y2"),
        ("7", "gnd"),
        ("8", "y3"), ("9", "a3"), ("10", "b3"), ("11", "c3"),
        ("12", "c1"), ("13", "y1"),
        ("14", "vcc"),
    ]);
}

#[test]
fn hc125_quad_tristate_buffer_pin_map() {
    // '125: each buffer is OE,A,Y ascending on the left (1-3, 4-6); the right
    // side mirrors to Y,A,OE (8-10, 11-13). Outputs on 3, 6, 8, 11.
    assert_pin_map("74HC125", "74hc125", &[
        ("1", "oe_n_1"), ("2", "a1"), ("3", "y1"),
        ("4", "oe_n_2"), ("5", "a2"), ("6", "y2"),
        ("7", "gnd"),
        ("8", "y3"), ("9", "a3"), ("10", "oe_n_3"),
        ("11", "y4"), ("12", "a4"), ("13", "oe_n_4"),
        ("14", "vcc"),
    ]);
}

#[test]
fn hc74_dual_dff_pin_map() {
    // TI '74 dual D flip-flop: FF1 on 1-6 (CLR, D, CLK, PRE, Q, Qbar),
    // GND 7; FF2 mirrors on 8-13 (Qbar, Q, PRE, CLK, D, CLR), VCC 14.
    assert_pin_map("74HC74", "74hc74", &[
        ("1", "clr_n1"), ("2", "d1"), ("3", "clk1"),
        ("4", "pre_n1"), ("5", "q1"), ("6", "q_n1"),
        ("7", "gnd"),
        ("8", "q_n2"), ("9", "q2"), ("10", "pre_n2"),
        ("11", "clk2"), ("12", "d2"), ("13", "clr_n2"),
        ("14", "vcc"),
    ]);
}

#[test]
fn hc595_shift_register_pin_map() {
    // TI '595 (SCLS041): QB..QH on 1-7, GND 8, QH' cascade on 9, SRCLR 10,
    // SRCLK 11, RCLK 12, OE 13, SER 14, QA 15, VCC 16.
    assert_pin_map("74HC595", "74hc595", &[
        ("1", "qb"), ("2", "qc"), ("3", "qd"), ("4", "qe"),
        ("5", "qf"), ("6", "qg"), ("7", "qh"),
        ("8", "gnd"),
        ("9", "qh_serial"), ("10", "srclr_n"), ("11", "srclk"),
        ("12", "rclk"), ("13", "oe_n"), ("14", "ser"), ("15", "qa"),
        ("16", "vcc"),
    ]);
}

#[test]
fn hc165_shift_register_pin_map() {
    // TI '165 (SCLS052): SH/LD 1, CLK 2, parallel E..H on 3-6, QHbar 7,
    // GND 8, QH 9, SER 10, parallel A..D on 11-14, CLK INH 15, VCC 16.
    assert_pin_map("74HC165", "74hc165", &[
        ("1", "pl_n"), ("2", "clk"),
        ("3", "e"), ("4", "f"), ("5", "g"), ("6", "h"),
        ("7", "qh_n"),
        ("8", "gnd"),
        ("9", "qh"), ("10", "ser"),
        ("11", "a"), ("12", "b"), ("13", "c"), ("14", "d"),
        ("15", "clk_inh"),
        ("16", "vcc"),
    ]);
}
