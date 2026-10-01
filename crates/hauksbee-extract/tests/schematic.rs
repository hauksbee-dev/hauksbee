//! Schematic extraction tests on the hand-written fixtures under `fixtures/`.

use hauksbee_extract::{Component, ExtractedBoard};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> Option<ExtractedBoard> {
    let p = fixtures_dir().join(name);
    ExtractedBoard::from_kicad_schematic_path(&p).ok()
}

/// Load a schematic fixture by name, or SKIP the test.
///
/// A missing fixture is a normal, reportable state in this suite (the same
/// contract `support::fixture` keeps), so every test announces the skip and
/// returns rather than failing. A macro because the skip has to `return` from
/// the caller.
macro_rules! fixture_or_skip {
    ($name:literal) => {
        match fixture($name) {
            Some(board) => board,
            None => {
                eprintln!("fixture missing; skipping");
                return;
            }
        }
    };
}

fn net_of(b: &ExtractedBoard, reference: &str, pin: &str) -> Option<i64> {
    let c = b.component(reference)?;
    c.pins.iter().find(|p| p.number == pin)?.net
}

fn same_net(b: &ExtractedBoard, a: (&str, &str), c: (&str, &str)) -> bool {
    net_of(b, a.0, a.1).is_some_and(|n| net_of(b, c.0, c.1) == Some(n))
}

#[test]
fn fixture_no_connect_control_pin_is_tagged_and_suppressed() {
    // A control pin (U1 "OE") left unwired with an explicit (no_connect)
    // must be TAGGED as a no-connect on the schematic path, exactly as the
    // KiCad-netlist loader preserves KiCad's "input+no_connect" pintype. Without
    // the tag, netlint's floating-control-pin check has no way to honor the
    // deliberate no-connect and cries wolf.
    let b = fixture_or_skip!("no_connect_control_pin.kicad_sch");
    let u1 = b.component("U1").expect("U1 present");
    let oe = u1.pins.iter().find(|p| p.number == "1").expect("OE pin");
    assert!(
        oe.kind.to_ascii_lowercase().contains("no_connect"),
        "the no-connected OE pin must be tagged no_connect, got kind={:?}",
        oe.kind
    );
    // End to end: the floating-control-pin lint must NOT fire on the deliberate
    // no-connect (the exact false-positive the missing tag caused).
    let r = b.net_lint();
    let floating = r
        .of_check(hauksbee_extract::LintCheck::FloatingControlPin)
        .count();
    assert_eq!(
        floating, 0,
        "explicit no-connect must suppress the floating-control finding"
    );
}

#[test]
fn fixture_two_resistors_in_series() {
    let b = fixture_or_skip!("two_resistors.kicad_sch");
    assert_eq!(b.components.len(), 2, "R1 and R2");
    // R1.2 and R2.1 are wired together at the midpoint.
    assert!(
        same_net(&b, ("R1", "2"), ("R2", "1")),
        "series node should be one net"
    );
    // R1.1 and R2.2 are distinct ends.
    assert!(!same_net(&b, ("R1", "1"), ("R2", "2")));
}

#[test]
fn fixture_power_and_labels_unify() {
    let b = fixture_or_skip!("power_labels.kicad_sch");
    // Both resistor top pins go to a +5V power symbol; they must share a net
    // even though they never touch geometrically.
    assert!(
        same_net(&b, ("R1", "1"), ("R2", "1")),
        "+5V power net must unify both resistors"
    );
    let vcc = b.net_by_name("+5V").expect("+5V net present");
    assert!(b.net_members(vcc.id).len() >= 2);
    // The two bottom pins share a local label "OUT".
    assert!(same_net(&b, ("R1", "2"), ("R2", "2")));
    assert!(b.net_by_name("OUT").is_some());
}

#[test]
fn fixture_unnamed_net_naming() {
    let b = fixture_or_skip!("two_resistors.kicad_sch");
    // The midpoint net is unnamed → KiCad-style Net-(Rx-PadN) after the
    // lowest member.
    let mid = net_of(&b, "R1", "2").unwrap();
    let name = &b.nets.iter().find(|n| n.id == mid).unwrap().name;
    assert!(
        name.starts_with("Net-(") && name.contains("R1"),
        "unnamed net named {name:?}"
    );
}

#[test]
fn fixture_vector_bus_members() {
    let b = fixture_or_skip!("bus_vector.kicad_sch");
    // Three resistors. R1.1 and R3.1 both carry the member label D0 (far apart,
    // each mid-span on its wire, both feeding the D[0..1] bus): they unify.
    assert!(
        same_net(&b, ("R1", "1"), ("R3", "1")),
        "bus member D0 must unify R1 and R3"
    );
    // R2.1 is member D1: a different member, and the vector bus label D[0..1]
    // must NOT merge D0 and D1 into one net.
    assert!(
        !same_net(&b, ("R1", "1"), ("R2", "1")),
        "distinct bus members D0 and D1 must stay separate"
    );
    assert!(b.net_by_name("D0").is_some(), "D0 net present");
    assert!(b.net_by_name("D1").is_some(), "D1 net present");
    // The bus label itself must never become a net.
    assert!(
        b.net_by_name("D[0..1]").is_none(),
        "bus label D[0..1] must not appear as a net"
    );
}

#[test]
fn fixture_hierarchical_bus() {
    // A bus crossing a sheet pin: parent ADDR[0..1] sheet pin connects to a
    // child ADDR[0..1] hierarchical label, member-wise. The parent's R-A0/R-A1
    // and the child's RC-A0/RC-A1 must pair by member (A0 with A0, A1 with A1),
    // never cross.
    let b = fixture_or_skip!("bus_hier_top.kicad_sch");
    assert!(
        same_net(&b, ("RA", "1"), ("RC", "1")),
        "ADDR member A0 must connect parent RA to child RC across the sheet"
    );
    assert!(
        same_net(&b, ("RB", "1"), ("RD", "1")),
        "ADDR member A1 must connect parent RB to child RD across the sheet"
    );
    assert!(
        !same_net(&b, ("RA", "1"), ("RB", "1")),
        "members A0 and A1 must not merge"
    );
    assert!(
        !same_net(&b, ("RA", "1"), ("RD", "1")),
        "member A0 must not cross to member A1's child net"
    );
}

#[test]
fn fixture_bus_alias_crosses_sheet() {
    // End-to-end exercise of a bus-alias *reference* across a sheet boundary. Both sheets
    // define `(bus_alias "ADDR" (members "A[1..0]"))`. The parent sheet pin and
    // the child hierarchical label are both written `MEM{ADDR}`, which must
    // expand through the alias to the qualified members `MEM.A1`, `MEM.A0`. The
    // member labels on the resistors (`MEM.A1` on RA/RC, `MEM.A0` on RB/RD) then
    // unify member-wise across the boundary. If the alias were *not* expanded,
    // the pin would carry a single literal member `MEM.ADDR` and nothing would
    // cross - the symptom the fix removes.
    let b = fixture_or_skip!("bus_alias_top.kicad_sch");
    assert!(
        same_net(&b, ("RA", "1"), ("RC", "1")),
        "alias member MEM.A1 must connect parent RA to child RC across the sheet"
    );
    assert!(
        same_net(&b, ("RB", "1"), ("RD", "1")),
        "alias member MEM.A0 must connect parent RB to child RD across the sheet"
    );
    assert!(
        !same_net(&b, ("RA", "1"), ("RB", "1")),
        "distinct alias members MEM.A1 and MEM.A0 must not merge"
    );
    assert!(
        !same_net(&b, ("RA", "1"), ("RD", "1")),
        "member MEM.A1 must not cross to member MEM.A0's net"
    );
    // The qualified member nets exist; the bus/alias expression never becomes a net.
    assert!(b.net_by_name("MEM.A1").is_some(), "MEM.A1 net present");
    assert!(b.net_by_name("MEM.A0").is_some(), "MEM.A0 net present");
    assert!(
        b.net_by_name("MEM{ADDR}").is_none(),
        "the bus expression must not be a net"
    );
    assert!(
        b.net_by_name("MEM.ADDR").is_none(),
        "the alias must expand, not stay literal"
    );
}

#[test]
fn fixture_global_bus_label_globalizes_members() {
    // A GLOBAL bus label `GB[0..1]` on both sheets must make each member a
    // global net: the top sheet's GB0 (RA) and the child sheet's GB0 (RC) are
    // one net even though no wire, sheet pin, or hierarchical label crosses
    // the boundary, exactly how KiCad treats a global bus label. Before the
    // fix the label's scope was dropped and the members stayed sheet-local.
    let b = fixture_or_skip!("global_bus_top.kicad_sch");
    assert!(
        same_net(&b, ("RA", "1"), ("RC", "1")),
        "global bus member GB0 must unify across sheets"
    );
    assert!(
        same_net(&b, ("RB", "1"), ("RD", "1")),
        "global bus member GB1 must unify across sheets"
    );
    assert!(
        !same_net(&b, ("RA", "1"), ("RB", "1")),
        "distinct members GB0 and GB1 must not merge"
    );
    // Control: a LOCAL bus label `LB[0..1]` on both sheets must NOT globalize
    // its members; the two LB0 nets are unrelated sheet-local nets.
    assert!(
        !same_net(&b, ("RE", "1"), ("RF", "1")),
        "local bus member LB0 must stay per-sheet"
    );
    // The bus expression itself never becomes a net.
    assert!(b.net_by_name("GB[0..1]").is_none());
}

#[test]
fn fixture_net_naming_priority() {
    // One net carrying two competing anchor kinds must take the
    // higher-priority name, per KiCad's driver precedence
    // global > power > local > hierarchical, NOT the alphabetically
    // smallest name of the old two-tier scheme.
    let b = fixture_or_skip!("naming_priority.kicad_sch");
    // R1's net carries both a +5V power pin and a global label "SIG".
    // Alphabetically "+5V" < "SIG", so the old tie inside the Global tier
    // picked the power name; the global label must win.
    let r1 = net_of(&b, "R1", "1").expect("R1.1 connected");
    let name = &b.nets.iter().find(|n| n.id == r1).unwrap().name;
    assert_eq!(name, "SIG", "global label must outrank the power name");
    assert!(b.net_by_name("+5V").is_none(), "power name must lose");
    // R2's net carries both a local label "Z_LOCAL" and a hierarchical label
    // "A_HIER". Alphabetically "A_HIER" wins the old Local-tier tie; the
    // local label must win.
    let r2 = net_of(&b, "R2", "1").expect("R2.1 connected");
    let name = &b.nets.iter().find(|n| n.id == r2).unwrap().name;
    assert_eq!(name, "Z_LOCAL", "local label must outrank the hier label");
    assert!(b.net_by_name("A_HIER").is_none(), "hier name must lose");
}

#[test]
fn fixture_multiunit_common_pin_bridges_nets() {
    // U1 is a two-unit part whose common (unit-0) pin "5" is drawn on both
    // gates: unit 1's copy is wired to NETA (with R1), unit 2's copy to NETB
    // (with R2). Pin 5 is ONE physical pin, so NETA and NETB are electrically
    // one net. Before the fix the dedup kept NETA on the pin and silently
    // dropped the short to NETB.
    let b = fixture_or_skip!("multiunit_common_pin.kicad_sch");
    assert!(
        same_net(&b, ("R1", "1"), ("R2", "1")),
        "nets bridged by the shared common pin must merge"
    );
    assert!(
        same_net(&b, ("U1", "5"), ("R1", "1")),
        "the common pin itself sits on the merged net"
    );
    // Deterministic winner: both nets are labelled, so the lower id (NETA,
    // ids being name-sorted) survives and NETB vanishes from the table.
    let merged = net_of(&b, "R1", "1").unwrap();
    let name = &b.nets.iter().find(|n| n.id == merged).unwrap().name;
    assert_eq!(name, "NETA");
    assert!(b.net_by_name("NETB").is_none(), "losing net must be pruned");
    // Self-consistency: no pin references a net id missing from the table.
    let ids: BTreeSet<i64> = b.nets.iter().map(|n| n.id).collect();
    for c in &b.components {
        for p in &c.pins {
            if let Some(id) = p.net {
                assert!(
                    ids.contains(&id),
                    "{}.{} on orphan net {id}",
                    c.reference,
                    p.number
                );
            }
        }
    }
}

#[test]
fn no_unconnected_components_in_fixtures() {
    for f in ["two_resistors.kicad_sch", "power_labels.kicad_sch"] {
        let Some(b) = fixture(f) else { continue };
        let comps: &[Component] = &b.components;
        for c in comps {
            assert!(
                c.pins.iter().any(|p| p.net.is_some()),
                "{f}: {} fully floating",
                c.reference
            );
        }
    }
}

#[test]
fn alternate_body_style_pins_are_not_placed() {
    // A De Morgan part: body style 2 of unit 2 reuses unit 1's pin numbers
    // (as Olimex's resistor-array symbols do). Only the placed style's pins
    // exist; admitting style 2 gave unit 2 a phantom "1", and the unit merge
    // then shorted NA to NC.
    let pin =
        |n: &str, x: f64| format!("(pin passive line (at {x} 0 0) (length 1) (number \"{n}\"))");
    let unit = |u: u8, s: u8, a: &str, b: &str| {
        format!("(symbol \"RN_{u}_{s}\" {} {})", pin(a, -5.08), pin(b, 5.08))
    };
    let place = |u: u8, y: u8| {
        format!("(symbol (lib_id \"T:RN\") (at 100 {y} 0) (unit {u}) (property \"Reference\" \"RN1\" (at 0 0 0)))")
    };
    let label = |n: &str, x: f64, y: u8| format!("(label \"{n}\" (at {x} {y} 0))");
    let text = format!(
        "(kicad_sch (version 20230121) (uuid \"u\") (lib_symbols (symbol \"T:RN\" {} {} {} {})) {} {} {} {} {} {})",
        unit(1, 1, "1", "2"),
        unit(2, 1, "3", "4"),
        unit(1, 2, "1", "2"),
        unit(2, 2, "1", "2"),
        place(1, 100),
        place(2, 120),
        label("NA", 94.92, 100),
        label("NB", 105.08, 100),
        label("NC", 94.92, 120),
        label("ND", 105.08, 120),
    );
    let b = ExtractedBoard::from_kicad_schematic(&text).expect("extract");
    let names: Vec<_> = ["1", "2", "3", "4"]
        .iter()
        .map(|p| {
            let id = net_of(&b, "RN1", p).expect("pin on a net");
            b.nets.iter().find(|n| n.id == id).unwrap().name.clone()
        })
        .collect();
    assert_eq!(names, ["NA", "NB", "NC", "ND"]);
}

#[test]
fn olimex_evb_rev_l_resistor_arrays_do_not_short_gnd_to_3v3lan() {
    let Some(p) = hauksbee_testkit::corpus_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch",
        "Olimex EVB REV-L body styles",
    ) else {
        return;
    };
    let b = ExtractedBoard::from_kicad_schematic_path(&p).expect("extract");
    let gnd = b.net_by_name("GND").expect("GND net").id;
    assert_eq!(net_of(&b, "U1", "2"), Some(gnd), "TP4054 GND pin");
    assert_ne!(b.net_by_name("+3.3VLAN").map(|n| n.id), Some(gnd));
}

#[test]
fn a_wire_ending_on_another_wire_needs_a_junction() {
    // R1.2 sits at (105.08, 100); a wire drops from it onto the middle of a
    // labelled wire at y=110. eeschema joins the tee only with a junction,
    // and a pin placed alone on a wire's interior does not join it either
    // (kicad-cli 10.0.6; only a label joins mid-span).
    let sch = |extra: &str| {
        format!(
            "(kicad_sch (version 20230121) (uuid \"u\") (lib_symbols (symbol \"T:R\" (symbol \"R_1_1\" \
             (pin passive line (at -5.08 0 0) (length 1) (number \"1\")) \
             (pin passive line (at 5.08 0 0) (length 1) (number \"2\"))))) \
             (symbol (lib_id \"T:R\") (at 100 100 0) (unit 1) (property \"Reference\" \"R1\" (at 0 0 0))) \
             (wire (pts (xy 80 110) (xy 130 110))) (label \"BUS\" (at 80 110 0)) {extra})"
        )
    };
    let on_bus = |extra: &str, reference: &str, pin: &str| {
        let b = ExtractedBoard::from_kicad_schematic(&sch(extra)).expect("extract");
        let id = net_of(&b, reference, pin);
        id.and_then(|id| b.nets.iter().find(|n| n.id == id))
            .is_some_and(|n| n.name == "BUS")
    };
    let tee = "(wire (pts (xy 105.08 100) (xy 105.08 110)))";
    assert!(!on_bus(tee, "R1", "2"));
    assert!(on_bus(
        &format!("{tee} (junction (at 105.08 110))"),
        "R1",
        "2"
    ));
    let lone_pin = "(symbol (lib_id \"T:R\") (at 120 110 0) (unit 1) (property \"Reference\" \"R2\" (at 0 0 0)))";
    assert!(!on_bus(lone_pin, "R2", "1"));
}

/// Every pin's net-mates, as a set of `REF.PIN` groups.
fn partition(b: &ExtractedBoard) -> BTreeSet<BTreeSet<String>> {
    let mut by_net = std::collections::BTreeMap::<String, BTreeSet<String>>::new();
    for c in &b.components {
        for p in &c.pins {
            let key = format!("{}.{}", c.reference, p.number);
            let id = p.net.map_or_else(|| key.clone(), |id| id.to_string());
            by_net.entry(id).or_default().insert(key);
        }
    }
    by_net.into_values().collect()
}

/// The schematic reader's net partition equals `kicad-cli sch export netlist
/// --format kicadsexpr` (KiCad 10.0.6) on each fixture, kept as `<name>.net`:
/// a local label meets same-named labels and power symbols on its own sheet
/// only, a `(power local)` symbol is sheet-scoped, a `#PWR` symbol without the
/// power flag names nothing, and a pin on a wire's interior does not join it.
#[test]
fn net_partition_matches_kicad_cli() {
    let mut diffs = Vec::new();
    for (sch, net) in [
        ("label_scope_top.kicad_sch", "label_scope.net"),
        ("power_rails_top.kicad_sch", "power_rails.net"),
        ("wire_interior_pins.kicad_sch", "wire_interior_pins.net"),
    ] {
        let ours = fixture(sch).expect("extract");
        let text = std::fs::read_to_string(fixtures_dir().join(net)).expect("oracle");
        let oracle = ExtractedBoard::from_kicad_netlist(&text).expect("parse oracle");
        let (a, b) = (partition(&ours), partition(&oracle));
        let only = |x: &BTreeSet<_>, y: &BTreeSet<_>| x.difference(y).cloned().collect::<Vec<_>>();
        if a != b {
            diffs.push(format!(
                "{sch}: ours only {:?}, kicad only {:?}",
                only(&a, &b),
                only(&b, &a)
            ));
        }
        // KiCad's netlist standing in for an up-to-date layout: no mismatch.
        let lint = oracle.layout_lint("", Some(&ours)).findings;
        let check = hauksbee_extract::LintCheck::PcbSchematicMismatch;
        diffs.extend(
            lint.into_iter()
                .filter(|f| f.check == check)
                .map(|f| f.message),
        );
    }
    assert!(diffs.is_empty(), "{diffs:#?}");
}

/// The PCB/schematic parity check, fed this reader's view of a hierarchy and
/// KiCad's own netlist standing in for the layout, finds nothing: a local
/// `SIG` on the root and a global `SIG` on the sub-sheet share a name but not
/// a net. Moving one pin on the layout side is still a mismatch.
#[test]
fn parity_against_kicad_netlist_is_clean_and_still_flags_a_moved_pin() {
    use hauksbee_extract::LintCheck;
    let sch = fixture("label_scope_top.kicad_sch").expect("extract");
    let text = std::fs::read_to_string(fixtures_dir().join("label_scope.net")).expect("oracle");
    let mut layout = ExtractedBoard::from_kicad_netlist(&text).expect("parse oracle");
    let mismatches = |layout: &ExtractedBoard| -> Vec<String> {
        let report = layout.layout_lint("", Some(&sch));
        let found = report.findings.into_iter();
        found
            .filter(|f| f.check == LintCheck::PcbSchematicMismatch)
            .map(|f| f.message)
            .collect()
    };
    assert_eq!(mismatches(&layout), Vec::<String>::new());
    let gsig = layout.net_by_name("GSIG").expect("GSIG").id;
    let r11 = layout.components.iter_mut().find(|c| c.reference == "R11");
    let pin = r11.and_then(|c| c.pins.iter_mut().find(|p| p.number == "2"));
    pin.expect("R11.2").net = Some(gsig);
    let found = mismatches(&layout);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].contains("R11.2"), "{}", found[0]);
}
