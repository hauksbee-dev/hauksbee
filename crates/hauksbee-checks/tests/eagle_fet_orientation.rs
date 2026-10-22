//! A backwards power FET on Eagle boards, whose package pads are named `G`,
//! `S` and `D` and carry no schematic pin function.

use hauksbee_checks::checks::engine_lint;
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding};
use hauksbee_models::ModelLibrary;

fn lint(brd: &str) -> Vec<LintFinding> {
    let board = ExtractedBoard::from_eagle_brd(brd).expect("fixture parses");
    let q = board
        .components
        .iter()
        .find(|c| c.reference == "Q1")
        .expect("fixture has Q1");
    assert!(
        q.pins.iter().all(|p| p.function.is_empty()) && q.pins.iter().any(|p| p.number == "G"),
        "the Eagle FET must carry bare G/S/D pad names: {q:?}"
    );
    engine_lint(&board, &ModelLibrary::builtin())
        .findings
        .into_iter()
        .filter(|f| f.check == LintCheck::PinFunction && f.refs == ["Q1"])
        .collect()
}

#[test]
fn an_eagle_reverse_polarity_pfet_with_drain_and_source_swapped_is_flagged() {
    let found = lint(include_str!("fixtures/eagle_fet/rpp_reversed.brd"));
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0].message.contains("reverse-polarity"),
        "{}",
        found[0].message
    );
}

#[test]
fn an_eagle_reverse_polarity_pfet_wired_correctly_is_silent() {
    let found = lint(include_str!("fixtures/eagle_fet/rpp_ok.brd"));
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn an_eagle_gpio_switched_pfet_wired_backwards_is_flagged() {
    let found = lint(include_str!("fixtures/eagle_fet/switch_reversed.brd"));
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0].message.contains("cannot turn it off"),
        "{}",
        found[0].message
    );
}

#[test]
fn an_eagle_gpio_switched_pfet_wired_correctly_is_silent() {
    let found = lint(include_str!("fixtures/eagle_fet/switch_ok.brd"));
    assert!(found.is_empty(), "{found:#?}");
}
