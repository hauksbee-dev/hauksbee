//! Node interning must be case-insensitive (SPICE net identity), so one net
//! written with mixed casing does not silently split into two nodes and
//! disagree with the case-insensitive `find_node`.

use hauksbee_ir::Circuit;

#[test]
fn node_interning_is_case_insensitive() {
    let mut c = Circuit::new();
    let a = c.node("OUT");
    let b = c.node("Out");
    let d = c.node("out");
    assert_eq!(
        a, b,
        "case variants of one net must intern to the same node"
    );
    assert_eq!(
        a, d,
        "case variants of one net must intern to the same node"
    );

    // node() and find_node() must agree.
    assert_eq!(c.find_node("out"), Some(a));
    assert_eq!(c.find_node("OUT"), Some(a));

    // First-seen casing is preserved for display.
    assert_eq!(c.node_name(a), "OUT");

    // Genuinely different names stay distinct.
    let e = c.node("IN");
    assert_ne!(a, e);
}
