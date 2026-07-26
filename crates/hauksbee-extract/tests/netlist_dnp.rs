//! Regression for R23 (NETLIST-DNP-ALWAYS-FALSE): the KiCad netlist reader used
//! to hardcode `dnp = false`, so a do-not-populate part ingested through the
//! netlist path was reported as populated, DNP-aware analysis then reasoned
//! about a component physically absent from the assembled board. KiCad writes
//! DNP as a (usually value-less) `(property (name "dnp"))` child; presence must
//! set `dnp = true`, matching the PCB and schematic ingestion paths.

use hauksbee_extract::ExtractedBoard;

#[test]
fn netlist_dnp_property_marks_component_do_not_populate() {
    let text = r#"(export (version "E")
  (components
    (comp (ref "R1") (value "1k") (footprint "Resistor_SMD:R_0603"))
    (comp (ref "R2") (value "10k") (footprint "Resistor_SMD:R_0603")
      (property (name "dnp")))
  )
  (nets
    (net (code "1") (name "N1") (node (ref "R1") (pin "1")))
    (net (code "2") (name "N2") (node (ref "R1") (pin "2")))
  )
)
"#;
    let board = ExtractedBoard::from_kicad_netlist(text).expect("netlist parses");

    let r1 = board
        .components
        .iter()
        .find(|c| c.reference == "R1")
        .expect("R1");
    let r2 = board
        .components
        .iter()
        .find(|c| c.reference == "R2")
        .expect("R2");

    assert!(
        !r1.dnp,
        "a part with no dnp property must stay populated (dnp=false)"
    );
    assert!(
        r2.dnp,
        "a value-less (property (name \"dnp\")) must mark the part do-not-populate"
    );
}

#[test]
fn netlist_dnp_explicit_false_value_stays_populated() {
    // KiCad normally writes the property value-less, but if a tool writes an
    // explicit falsey value it must NOT flip the part to DNP.
    let text = r#"(export (version "E")
  (components
    (comp (ref "C1") (value "100n") (footprint "Capacitor_SMD:C_0402")
      (property (name "dnp") (value "no")))
  )
  (nets
    (net (code "1") (name "N1") (node (ref "C1") (pin "1")))
  )
)
"#;
    let board = ExtractedBoard::from_kicad_netlist(text).expect("netlist parses");
    let c1 = board
        .components
        .iter()
        .find(|c| c.reference == "C1")
        .expect("C1");
    assert!(
        !c1.dnp,
        "an explicit falsey dnp value must keep the part populated"
    );
}
