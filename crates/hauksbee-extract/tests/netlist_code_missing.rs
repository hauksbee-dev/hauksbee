//! Regression for round-4 #6: two `(net ...)` blocks that both lack a parseable
//! `(code ...)` must stay electrically distinct, not fuse onto one shared
//! sentinel id (which silently shorted them).

use hauksbee_extract::ExtractedBoard;

#[test]
fn code_less_nets_do_not_fuse() {
    // A hand-edited / tool-variant netlist: two nets, neither carrying `code`.
    let text = r#"(export (version "E")
  (components
    (comp (ref R1) (value 1k) (footprint Resistor_SMD:R_0603))
  )
  (nets
    (net (name "N1") (node (ref R1) (pin 1)))
    (net (name "N2") (node (ref R1) (pin 2)))
  )
)
"#;
    let board = ExtractedBoard::from_kicad_netlist(text).expect("netlist parses");

    let n1 = board.nets.iter().find(|n| n.name == "N1").expect("net N1");
    let n2 = board.nets.iter().find(|n| n.name == "N2").expect("net N2");
    assert_ne!(
        n1.id, n2.id,
        "two code-less nets must not collapse onto one sentinel id"
    );

    let r1 = board
        .components
        .iter()
        .find(|c| c.reference == "R1")
        .expect("R1");
    let p1 = r1.pins.iter().find(|p| p.number == "1").expect("pin 1");
    let p2 = r1.pins.iter().find(|p| p.number == "2").expect("pin 2");
    assert_ne!(
        p1.net, p2.net,
        "pins on two distinct code-less nets must not share a net id"
    );
}
