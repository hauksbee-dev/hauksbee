//! Spec-sheet-derived test vectors for the §1.3 declarative logic coverage:
//! the basic 74HC gates (00 NAND, 02 NOR, 04 NOT, 08 AND, 32 OR, 86 XOR),
//! the 74HC27 triple 3-input NOR and 74HC125 tri-state buffer that shipped
//! with real logic in the migration, and the 74HC74 dual D flip-flop.
//!
//! Each vector is the part's DATASHEET function table (Texas Instruments
//! SN74HCxx datasheets; the db entry for each part cites its source next to
//! the `[models.logic]` block), driven through the same builtin model entry a
//! board bind resolves, so these tests pin the shipped data, not a copy.

use std::collections::HashMap;

use hauksbee_engine::logic::LogicComponent;
use hauksbee_models::{ComponentQuery, ModelLibrary};

fn compile_builtin(value: &str) -> LogicComponent {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery::new(None, Some(value.to_string()), None);
    let model = lib
        .resolve(&q)
        .model
        .unwrap_or_else(|| panic!("builtin model for {value}"));
    assert!(
        !model.logic.is_empty(),
        "{value} must carry a [models.logic] block"
    );
    LogicComponent::compile(&model.id, &model.logic).expect("builtin spec compiles")
}

/// Tick from a plain name->level map; unlisted pins sample as unwired.
fn tick(lc: &mut LogicComponent, levels: &[(&str, bool)]) {
    let map: HashMap<&str, bool> = levels.iter().copied().collect();
    lc.tick(&mut |name, _prev| map.get(name).copied());
}

/// Drive every gate of a quad 2-input part through the full 4-row function
/// table simultaneously (each gate gets a different input pair per step, so
/// the vectors also prove per-gate independence).
fn check_quad_gate(value: &str, f: fn(bool, bool) -> bool) {
    let mut lc = compile_builtin(value);
    let rows: [(bool, bool); 4] = [(false, false), (false, true), (true, false), (true, true)];
    for rot in 0..4 {
        let mut inputs: Vec<(String, bool)> = Vec::new();
        let mut expect: Vec<(String, bool)> = Vec::new();
        for g in 0..4usize {
            let (a, b) = rows[(g + rot) % 4];
            inputs.push((format!("a{}", g + 1), a));
            inputs.push((format!("b{}", g + 1), b));
            expect.push((format!("y{}", g + 1), f(a, b)));
        }
        let named: Vec<(&str, bool)> = inputs.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        tick(&mut lc, &named);
        for (y, want) in &expect {
            assert_eq!(
                lc.output_level(y),
                Some(*want),
                "{value} {y} function-table row (rot {rot})"
            );
        }
    }
}

#[test]
fn hc00_nand_function_table() {
    // TI SN74HC00 (SCLS181): Y = !(A & B).
    check_quad_gate("74HC00", |a, b| !(a && b));
}

#[test]
fn hc08_and_function_table() {
    // TI SN74HC08: Y = A & B.
    check_quad_gate("74HC08", |a, b| a && b);
}

#[test]
fn hc32_or_function_table() {
    // TI SN74HC32: Y = A | B.
    check_quad_gate("74HC32", |a, b| a || b);
}

#[test]
fn hc86_xor_function_table() {
    // TI SN74HC86: Y = A XOR B.
    check_quad_gate("74HC86", |a, b| a != b);
}

#[test]
fn hc02_nor_function_table() {
    // TI SN74HC02 (SCLS050): Y = !(A | B). Role names are the db's g<N><pin>
    // convention (the same chip the binder promotes to SR latches when
    // cross-coupled; standalone gates evaluate here).
    let mut lc = compile_builtin("74HC02");
    let rows: [(bool, bool); 4] = [(false, false), (false, true), (true, false), (true, true)];
    for (a, b) in rows {
        tick(
            &mut lc,
            &[
                ("g1a", a),
                ("g1b", b),
                ("g2a", b),
                ("g2b", a),
                ("g3a", a),
                ("g3b", a),
                ("g4a", b),
                ("g4b", b),
            ],
        );
        assert_eq!(lc.output_level("g1y"), Some(!(a || b)), "NOR({a},{b})");
        assert_eq!(lc.output_level("g2y"), Some(!(b || a)));
        assert_eq!(lc.output_level("g3y"), Some(!a), "NOR(a,a) = !a");
        assert_eq!(lc.output_level("g4y"), Some(!b));
    }
}

#[test]
fn hc04_inverter_function_table() {
    // TI SN74HC04: Y = !A, six independent inverters.
    let mut lc = compile_builtin("74HC04");
    for pattern in 0..64u8 {
        let inputs: Vec<(String, bool)> = (0..6)
            .map(|i| (format!("a{}", i + 1), (pattern >> i) & 1 == 1))
            .collect();
        let named: Vec<(&str, bool)> = inputs.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        tick(&mut lc, &named);
        for i in 0..6 {
            assert_eq!(
                lc.output_level(&format!("y{}", i + 1)),
                Some((pattern >> i) & 1 == 0),
                "inverter {} for pattern {pattern:06b}",
                i + 1
            );
        }
    }
}

#[test]
fn hc27_triple_3input_nor_function_table() {
    // TI SN74HC27 (SCLS089): Y = !(A | B | C), all 8 rows per gate. (The
    // pre-migration buffer fallback mirrored only the A input; this is the
    // fixed behaviour.)
    let mut lc = compile_builtin("74HC27");
    for pattern in 0..8u8 {
        let (a, b, c) = (pattern & 1 != 0, pattern & 2 != 0, pattern & 4 != 0);
        tick(
            &mut lc,
            &[
                ("a1", a),
                ("b1", b),
                ("c1", c),
                ("a2", c),
                ("b2", a),
                ("c2", b),
                ("a3", b),
                ("b3", c),
                ("c3", a),
            ],
        );
        let want = !(a || b || c);
        for y in ["y1", "y2", "y3"] {
            assert_eq!(lc.output_level(y), Some(want), "{y} for {pattern:03b}");
        }
    }
}

#[test]
fn hc125_tristate_buffer_function_table() {
    // TI SN74HC125 (SCLS049): Y follows A while the gate's own OE_n is LOW;
    // OE_n HIGH puts Y in high impedance, per-gate independent enables.
    let mut lc = compile_builtin("74HC125");
    tick(
        &mut lc,
        &[
            ("a1", true),
            ("oe_n_1", false),
            ("a2", false),
            ("oe_n_2", false),
            ("a3", true),
            ("oe_n_3", true),
            ("a4", false),
            ("oe_n_4", true),
        ],
    );
    assert_eq!(
        lc.output_level("y1"),
        Some(true),
        "enabled buffer passes HIGH"
    );
    assert_eq!(lc.output_enabled("y1"), Some(true));
    assert_eq!(
        lc.output_level("y2"),
        Some(false),
        "enabled buffer passes LOW"
    );
    assert_eq!(lc.output_enabled("y2"), Some(true));
    assert_eq!(lc.output_enabled("y3"), Some(false), "OE_n HIGH -> Hi-Z");
    assert_eq!(
        lc.output_enabled("y4"),
        Some(false),
        "independent per-gate enables"
    );
}

/// TI SN74HC74 function table, one row at a time:
///   PRE_n L, CLR_n H          -> Q=H, Q_n=L   (async preset)
///   PRE_n H, CLR_n L          -> Q=L, Q_n=H   (async clear)
///   PRE_n H, CLR_n H, CLK ^  D=H -> Q=H
///   PRE_n H, CLR_n H, CLK ^  D=L -> Q=L
///   PRE_n H, CLR_n H, CLK=L  D=X -> Q holds (level-insensitive)
#[test]
fn hc74_dff_function_table() {
    let mut lc = compile_builtin("74HC74");
    let released = [("pre_n1", true), ("clr_n1", true)];

    // Async preset dominates the clock.
    tick(
        &mut lc,
        &[
            ("pre_n1", false),
            ("clr_n1", true),
            ("d1", false),
            ("clk1", true),
        ],
    );
    assert_eq!(lc.output_level("q1"), Some(true), "PRE_n low sets Q");
    assert_eq!(lc.output_level("q_n1"), Some(false));

    // Async clear.
    tick(
        &mut lc,
        &[
            ("pre_n1", true),
            ("clr_n1", false),
            ("d1", true),
            ("clk1", true),
        ],
    );
    assert_eq!(lc.output_level("q1"), Some(false), "CLR_n low clears Q");
    assert_eq!(lc.output_level("q_n1"), Some(true));

    // Clocked capture of D = H (release controls, clock low, then rising edge).
    let mut step = |lc: &mut LogicComponent, d: bool, clk: bool| {
        let mut v: Vec<(&str, bool)> = released.to_vec();
        v.push(("d1", d));
        v.push(("clk1", clk));
        tick(lc, &v);
    };
    step(&mut lc, true, false);
    step(&mut lc, true, true);
    assert_eq!(lc.output_level("q1"), Some(true), "CLK rising captures D=H");

    // D changes while CLK is high or low: no effect until the next edge.
    step(&mut lc, false, true);
    assert_eq!(
        lc.output_level("q1"),
        Some(true),
        "D change at CLK high ignored"
    );
    step(&mut lc, false, false);
    assert_eq!(lc.output_level("q1"), Some(true), "CLK falling holds");

    // Next rising edge captures D = L.
    step(&mut lc, false, true);
    assert_eq!(
        lc.output_level("q1"),
        Some(false),
        "CLK rising captures D=L"
    );
    assert_eq!(lc.output_level("q_n1"), Some(true));

    // The second flop is independent: clocking FF2 must not disturb FF1.
    tick(
        &mut lc,
        &[
            ("pre_n1", true),
            ("clr_n1", true),
            ("d1", false),
            ("clk1", false),
            ("pre_n2", true),
            ("clr_n2", true),
            ("d2", true),
            ("clk2", false),
        ],
    );
    tick(
        &mut lc,
        &[
            ("pre_n1", true),
            ("clr_n1", true),
            ("d1", false),
            ("clk1", false),
            ("pre_n2", true),
            ("clr_n2", true),
            ("d2", true),
            ("clk2", true),
        ],
    );
    assert_eq!(lc.output_level("q2"), Some(true), "FF2 captured its own D");
    assert_eq!(lc.output_level("q1"), Some(false), "FF1 undisturbed");
}
