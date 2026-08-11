//! Always-present release regressions for both sides of the Eagle tie boundary.

use hauksbee_extract::ExtractedBoard;

#[test]
fn tracked_declared_pair_qualifies_one_physical_multilayer_contact() {
    let board = include_str!("fixtures/eagle_ties/declared.brd");
    let schematic = include_str!("fixtures/eagle_ties/declared.sch");
    let report = ExtractedBoard::drc(board).expect("tracked board parses");
    let ties = hauksbee_extract::declared_net_ties(schematic).expect("tracked schematic parses");
    let qualified = report.qualify_with_declared_ties("declared.sch", &ties);
    assert_eq!(
        report.short_count(),
        2,
        "one contact appears on two copper layers"
    );
    assert_eq!(qualified.qualified_count(), 2);
    assert_eq!(qualified.undeclared_shorts(&report).count(), 0);
}

#[test]
fn tracked_undeclared_pair_leaves_the_short_gating() {
    let board = include_str!("fixtures/eagle_ties/undeclared.brd");
    let schematic = include_str!("fixtures/eagle_ties/undeclared.sch");
    let report = ExtractedBoard::drc(board).expect("tracked board parses");
    let ties = hauksbee_extract::declared_net_ties(schematic).expect("tracked schematic parses");
    let qualified = report.qualify_with_declared_ties("undeclared.sch", &ties);
    assert_eq!(ties.len(), 0);
    assert_eq!(report.short_count(), 1);
    assert_eq!(qualified.qualified_count(), 0);
    assert_eq!(qualified.undeclared_shorts(&report).count(), 1);
}
