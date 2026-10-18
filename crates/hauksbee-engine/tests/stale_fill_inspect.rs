//! A stored zone fill that an unrouted-after-fill track, via or pad sits in is
//! out of date, not shorted. It never gates, but it is not a clean board
//! either: gerbers are plotted from the stored fill, so ordering without
//! refilling sends the fab the old pour. The whole run must therefore read
//! "inspect before ordering" (the same `--json` pass the web headline reads),
//! never a clean OK TO ORDER and never a do-not-order.
//!
//! The kicad-cli 10.0.6 oracle reports a track, a pad and a via tied to its own
//! net inside a stored fill as `clearance` errors at "actual 0.0000 mm", never
//! as shorting items, which is why this is a refill reminder and not a short.

use crate::support::{run_board, stdout};

/// Two 0402 resistors in a chain, a GND pour with a stored fill, and one extra
/// piece of copper of another net lying inside it.
fn board_with_stale_fill(inside: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "SIG")
  (net 2 "MID")
  (net 3 "GND")
  (footprint "Resistor_SMD:R_0402_1005Metric" (layer "F.Cu") (at 30 10)
    (property "Reference" "R1" (at 0 0))
    (property "Value" "10k" (at 0 2))
    (pad "1" smd rect (at -0.5 0) (size 0.5 0.6) (layers "F.Cu") (net 1 "SIG"))
    (pad "2" smd rect (at 0.5 0) (size 0.5 0.6) (layers "F.Cu") (net 2 "MID"))
  )
  (footprint "Resistor_SMD:R_0402_1005Metric" (layer "F.Cu") (at 40 10)
    (property "Reference" "R2" (at 0 0))
    (property "Value" "10k" (at 0 2))
    (pad "1" smd rect (at -0.5 0) (size 0.5 0.6) (layers "F.Cu") (net 2 "MID"))
    (pad "2" smd rect (at 0.5 0) (size 0.5 0.6) (layers "F.Cu") (net 3 "GND"))
  )
  (segment (start 30.5 10) (end 39.5 10) (width 0.25) (layer "F.Cu") (net 2))
  (zone (net 3) (net_name "GND") (layer "B.Cu")
    (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))
    (filled_polygon (layer "B.Cu")
      (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))
    )
  )
{inside}
)
"#
    )
}

fn json_of(text: &str) -> serde_json::Value {
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("--json prints one document ({e}):\n{text}"))
}

#[test]
fn a_stale_fill_alone_reads_inspect_before_ordering_not_ok_to_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    for (what, inside) in [
        (
            "track",
            r#"  (segment (start 5 10) (end 15 10) (width 0.25) (layer "B.Cu") (net 1))"#,
        ),
        (
            "via",
            r#"  (via (at 10 10) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))"#,
        ),
    ] {
        let path = dir.path().join(format!("{what}.kicad_pcb"));
        std::fs::write(&path, board_with_stale_fill(inside)).expect("write board");

        let out = run_board(&path, &["--json"]);
        let doc = json_of(&stdout(&out));
        assert_eq!(
            doc["verdict"], "pass",
            "{what}: a stale fill never gates:\n{doc:#}"
        );
        assert_eq!(
            doc["drc"]["shorts"].as_array().map(Vec::len),
            Some(0),
            "{what}: not a short"
        );
        assert!(
            doc["drc"]["stale_fills"]
                .as_array()
                .is_some_and(|f| !f.is_empty()),
            "{what}: the stale fill is reported:\n{doc:#}"
        );
        assert_eq!(
            doc["triage"]["do_not_order"].as_array().map(Vec::len),
            Some(0),
            "{what}: not a do-not-order"
        );
        assert!(
            doc["triage"]["inspect"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["statement"]
                    .as_str()
                    .is_some_and(|s| s.contains("refill zones (B) in KiCad and re-run")))),
            "{what}: the refill is on the inspect list:\n{doc:#}"
        );

        let plain = stdout(&run_board(&path, &["--plain"]));
        assert!(
            plain.contains("INSPECT BEFORE ORDERING:"),
            "{what}: the headline lists the refill:\n{plain}"
        );
        assert!(
            !plain.contains("OK TO ORDER"),
            "{what}: never a clean OK TO ORDER:\n{plain}"
        );
        assert!(
            plain.contains("inspect before ordering ("),
            "{what}:\n{plain}"
        );
    }
}
