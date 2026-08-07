//! Altium `.PcbDoc` extraction tests.
//!
//! These are corpus-independent: each test synthesises a minimal Altium OLE2 /
//! Compound File Binary container in memory with the `cfb` crate (the same
//! crate the extractor reads with), writing the exact record encodings the
//! parser expects. That exercises the container handling, the properties-string
//! decoder (`Nets6`, `Components6`), the fixed-binary `Pads6` / `Tracks6`
//! layouts, net/component index resolution and the geometric DRC, without
//! needing a multi-megabyte board on disk.
//!
//! The sweep over real downloaded Altium boards (cobra, qfsae, HERON, ...) and
//! the closed-loop cross-validation against KiCad's Altium importer live in
//! `altium_corpus.rs`.

use hauksbee_extract::{ExtractedBoard, ViolationKind};
use std::io::{Cursor, Write};

const MM_PER_UNIT: f64 = 2.54e-6;

/// Altium coordinate (i32 internal units) from millimetres.
fn unit(mm: f64) -> i32 {
    (mm / MM_PER_UNIT).round() as i32
}

/// A properties block: u32 length (NUL terminated) then the `|KEY=VALUE|...`
/// ASCII string.
fn props(s: &str) -> Vec<u8> {
    let mut body = s.as_bytes().to_vec();
    body.push(0); // trailing NUL, counted in the length
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// A sub-record: u32 length then the payload.
fn subrecord(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// Build a PADS6 record (record-type marker 0x02 + six sub-records). Only the
/// geometry sub-record (#5) carries data the parser reads; the rest are minimal.
fn pad_record(
    name: &str,
    layer: u8,
    net: u16,
    component: u16,
    x_mm: f64,
    y_mm: f64,
    size_mm: f64,
) -> Vec<u8> {
    let mut rec = vec![0x02u8];
    // Sub-record 1: a Pascal string (1-byte len + bytes) holding the designator.
    let mut s1 = vec![name.len() as u8];
    s1.extend_from_slice(name.as_bytes());
    rec.extend(subrecord(&s1));
    // Sub-records 2,3,4: empty.
    rec.extend(subrecord(&[]));
    rec.extend(subrecord(&[]));
    rec.extend(subrecord(&[]));
    // Sub-record 5: geometry. Lay out the fields the parser reads at their exact
    // offsets; the block is 110 bytes (the shortest documented form).
    let mut g = vec![0u8; 110];
    g[0] = layer;
    g[3..5].copy_from_slice(&net.to_le_bytes());
    g[7..9].copy_from_slice(&component.to_le_bytes());
    g[13..17].copy_from_slice(&unit(x_mm).to_le_bytes());
    g[17..21].copy_from_slice(&unit(y_mm).to_le_bytes());
    g[21..25].copy_from_slice(&unit(size_mm).to_le_bytes()); // top size x
    g[25..29].copy_from_slice(&unit(size_mm).to_le_bytes()); // top size y
    g[49] = 1; // top shape = round
    rec.extend(subrecord(&g));
    // Sub-record 6: empty (no per-layer stack).
    rec.extend(subrecord(&[]));
    rec
}

/// Build a TRACKS6 record (marker 0x04 + one sub-record).
fn track_record(
    layer: u8,
    net: u16,
    component: u16,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    width: f64,
) -> Vec<u8> {
    let mut rec = vec![0x04u8];
    let mut p = vec![0u8; 36];
    p[0] = layer;
    p[3..5].copy_from_slice(&net.to_le_bytes());
    p[7..9].copy_from_slice(&component.to_le_bytes());
    p[13..17].copy_from_slice(&unit(x1).to_le_bytes());
    p[17..21].copy_from_slice(&unit(y1).to_le_bytes());
    p[21..25].copy_from_slice(&unit(x2).to_le_bytes());
    p[25..29].copy_from_slice(&unit(y2).to_le_bytes());
    p[29..33].copy_from_slice(&unit(width).to_le_bytes());
    rec.extend(subrecord(&p));
    rec
}

/// Build a TEXTS6 record (marker 0x05 + two sub-records): the fixed properties
/// block (component link at +7, `isComment` flag at +40, `isDesignator` at
/// +41) then the Pascal-string text.
fn text_record(component: u16, text: &str, is_comment: bool, is_designator: bool) -> Vec<u8> {
    let mut rec = vec![0x05u8];
    // Sub-record 1: 252 bytes, the length real exports use.
    let mut p = vec![0u8; 252];
    p[7..9].copy_from_slice(&component.to_le_bytes());
    p[40] = is_comment as u8;
    p[41] = is_designator as u8;
    rec.extend(subrecord(&p));
    // Sub-record 2: the text as a Pascal string.
    let mut t = vec![text.len() as u8];
    t.extend_from_slice(text.as_bytes());
    rec.extend(subrecord(&t));
    rec
}

/// One component named `refdes` (extra properties appended verbatim) with two
/// copper pads, as a (Components6, Pads6) stream pair.
fn one_passive(refdes: &str, extra_props: &str) -> (Vec<u8>, Vec<u8>) {
    let comps = props(&format!(
        "|LAYER=TOP|X=0mil|Y=0mil|ROTATION=0|PATTERN=RESC3216X70N|SOURCEDESIGNATOR={refdes}{extra_props}"
    ));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 0, 1.0, 0.0, 0.5));
    (comps, pads)
}

fn two_net_stream() -> Vec<u8> {
    let mut nets = Vec::new();
    nets.extend(props("|NAME=A"));
    nets.extend(props("|NAME=B"));
    nets
}

/// Assemble a minimal binary `.PcbDoc` in memory from the given stream bodies.
fn build_pcbdoc(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut cf = cfb::CompoundFile::create(cursor).expect("create CFB");
    // A small FileHeader so the file looks like a real one (not read by us).
    {
        let mut s = cf.create_stream("/FileHeader").unwrap();
        s.write_all(&[0u8; 24]).unwrap();
    }
    for (section, body) in streams {
        cf.create_storage(&format!("/{section}")).unwrap();
        let mut data = cf.create_stream(&format!("/{section}/Data")).unwrap();
        data.write_all(body).unwrap();
        drop(data);
        let mut hdr = cf.create_stream(&format!("/{section}/Header")).unwrap();
        hdr.write_all(&1u32.to_le_bytes()).unwrap();
    }
    cf.flush().unwrap();
    cf.into_inner().into_inner()
}

/// Two resistors, R1 and R2, sharing a net N1 across their inner pads and each
/// with an outer pad on its own net (N0_VCC, N2_GND). Plus a track on N1.
fn two_resistor_board() -> Vec<u8> {
    // Nets6: index 0 = VCC, 1 = MID, 2 = GND.
    let mut nets = Vec::new();
    nets.extend(props("|NAME=VCC"));
    nets.extend(props("|NAME=MID"));
    nets.extend(props("|NAME=GND"));

    // Components6: index 0 = R1, 1 = R2.
    let mut comps = Vec::new();
    comps.extend(props("|LAYER=TOP|X=1000mil|Y=1000mil|ROTATION=0|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEFOOTPRINTLIBRARY=Std.PcbLib"));
    comps.extend(props(
        "|LAYER=BOTTOM|X=2000mil|Y=1000mil|ROTATION=90|PATTERN=R0402|SOURCEDESIGNATOR=R2",
    ));

    // Pads6: R1.1 on VCC, R1.2 on MID; R2.1 on MID, R2.2 on GND.
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 10.0, 10.0, 0.5)); // R1.1 VCC
    pads.extend(pad_record("2", 1, 1, 0, 11.0, 10.0, 0.5)); // R1.2 MID
    pads.extend(pad_record("1", 1, 1, 1, 20.0, 10.0, 0.5)); // R2.1 MID
    pads.extend(pad_record("2", 1, 2, 1, 21.0, 10.0, 0.5)); // R2.2 GND

    // Tracks6: a MID track joining R1.2 to R2.1 (on net MID, index 1).
    let mut tracks = Vec::new();
    tracks.extend(track_record(1, 1, 0xFFFF, 11.0, 10.0, 20.0, 10.0, 0.2));

    build_pcbdoc(&[
        ("Nets6", nets),
        ("Components6", comps),
        ("Pads6", pads),
        ("Tracks6", tracks),
    ])
}

#[test]
fn extracts_nets_components_and_pad_nets() {
    let bytes = two_resistor_board();
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(board.nets.len(), 3, "VCC, MID, GND");
    assert!(board.net_by_name("VCC").is_some());
    assert!(board.net_by_name("MID").is_some());
    assert!(board.net_by_name("GND").is_some());

    assert_eq!(board.components.len(), 2);
    let r1 = board.component("R1").expect("R1");
    assert_eq!(r1.footprint, "R0402");
    assert_eq!(r1.layer, "F.Cu", "TOP -> F.Cu");
    assert_eq!(r1.pins.len(), 2);
    let r2 = board.component("R2").expect("R2");
    assert_eq!(r2.layer, "B.Cu", "BOTTOM -> B.Cu");

    // Connectivity: R1.2 and R2.1 are both on MID.
    let mid = board.net_by_name("MID").unwrap();
    let members = board.net_members(mid.id);
    assert_eq!(members.len(), 2, "MID joins R1.2 and R2.1");
    let refs: Vec<&str> = members.iter().map(|(c, _)| c.reference.as_str()).collect();
    assert!(refs.contains(&"R1") && refs.contains(&"R2"));

    // VCC and GND each touch exactly one pad.
    assert_eq!(
        board
            .net_members(board.net_by_name("VCC").unwrap().id)
            .len(),
        1
    );
    assert_eq!(
        board
            .net_members(board.net_by_name("GND").unwrap().id)
            .len(),
        1
    );

    // Pads carry positions in mm.
    let p1 = r1.pins.iter().find(|p| p.number == "1").unwrap();
    let (x, _) = p1.position.unwrap();
    assert!((x - 10.0).abs() < 1e-3, "pad x ~ 10mm, got {x}");
}

#[test]
fn comment_text_is_the_value_designator_text_is_not() {
    // (i) A COMMENT-flagged text supplies the value, unchanged behavior.
    let (comps, pads) = one_passive("R1", "");
    let mut texts = Vec::new();
    texts.extend(text_record(0, "R1", false, true)); // the designator label
    texts.extend(text_record(0, "10k", true, false)); // the comment/value
    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
        ("Texts6", texts),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");
    assert_eq!(board.component("R1").unwrap().value, "10k");
}

#[test]
fn missing_value_is_unresolved_never_the_refdes() {
    // (iii) No comment, no SOURCEDESCRIPTION: the value stays empty with the
    // unresolved reason attached. The refdes must NOT leak into the value:
    // "R74" as a value binds downstream as a fabricated 0.74 ohm resistor,
    // and "RED" (an LED designator) as 1 ohm.
    for refdes in ["R74", "RED"] {
        let (comps, pads) = one_passive(refdes, "");
        let mut texts = Vec::new();
        // Only a designator text exists, carrying the refdes string; the old
        // flag mixup read exactly this record as the comment.
        texts.extend(text_record(0, refdes, false, true));
        let bytes = build_pcbdoc(&[
            ("Nets6", two_net_stream()),
            ("Components6", comps),
            ("Pads6", pads),
            ("Texts6", texts),
        ]);
        let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");
        let c = board.component(refdes).expect(refdes);
        assert_eq!(
            c.value, "",
            "{refdes}: a missing value must stay empty, not become the refdes"
        );
        assert!(
            c.properties.iter().any(|(k, v)| k == "value_unresolved"
                && v == "no value in the PcbDoc; Altium keeps values in the .SchDoc"),
            "{refdes}: the unresolved reason must be exposed, got {:?}",
            c.properties
        );
    }
}

#[test]
fn sourcedescription_supplies_the_value_before_unresolved() {
    // (ii) No comment, but a parseable SOURCEDESCRIPTION: value and ratings
    // come from it, and no unresolved reason is attached.
    let (comps, pads) = one_passive(
        "C7",
        "|SOURCEDESCRIPTION=Cap Ceramic 1uF 16V X7R 10% SMD 0603",
    );
    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");
    let c = board.component("C7").unwrap();
    assert_eq!(c.value, "1uF");
    assert!(c
        .properties
        .iter()
        .any(|(k, v)| k == "voltage_rating" && v == "16V"));
    assert!(
        !c.properties.iter().any(|(k, _)| k == "value_unresolved"),
        "a recovered value must not carry the unresolved reason"
    );

    // The resistor form with a spaced unit and a power rating.
    let (comps, pads) = one_passive(
        "R3",
        "|SOURCEDESCRIPTION=Resistor SMD chip 1 Ohm 250mW 1% 1206",
    );
    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");
    let c = board.component("R3").unwrap();
    assert_eq!(c.value, "1Ohm");
    assert!(c
        .properties
        .iter()
        .any(|(k, v)| k == "power_rating" && v == "250mW"));
}

#[test]
fn non_copper_pad_records_are_not_pins() {
    // A 2-pad passive whose footprint also carries a pad record on a
    // non-copper layer (paste/mask/mechanical). Counting that record as a pin
    // made the part a 3-pad "ambiguous bussed array" downstream.
    let comps =
        props("|LAYER=TOP|X=0mil|Y=0mil|ROTATION=0|PATTERN=RESC3216X70N|SOURCEDESIGNATOR=R9");
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 0, 1.0, 0.0, 0.5));
    // Layer 37 is a mask layer in Altium's numbering: not copper.
    pads.extend(pad_record("3", 37, 0xFFFF, 0, 0.5, 0.0, 0.6));
    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");
    let c = board.component("R9").unwrap();
    assert_eq!(
        c.pins.len(),
        2,
        "only copper pads are pins; got {:?}",
        c.pins.iter().map(|p| &p.number).collect::<Vec<_>>()
    );
    assert!(c.pins.iter().all(|p| p.number != "3"));
}

#[test]
fn duplicate_placements_without_channels_merge_as_one_electrical_part() {
    let mut comps = Vec::new();
    comps.extend(props(
        "|LAYER=TOP|X=0mil|Y=0mil|PATTERN=TESTPOINT|SOURCEDESIGNATOR=TP1",
    ));
    comps.extend(props(
        "|LAYER=BOTTOM|X=100mil|Y=0mil|PATTERN=TESTPOINT|SOURCEDESIGNATOR=TP1",
    ));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    // The second record repeats pad 1 on the same net, which is the positive
    // structural anchor required when no hierarchy identifies the placement.
    pads.extend(pad_record("1", 1, 0, 1, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 1, 1.0, 0.0, 0.5));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(
        board.components.len(),
        1,
        "two physical placements with one unchannelled designator are one part"
    );
    let tp1 = board.component("TP1").expect("unsuffixed TP1");
    assert_eq!(
        tp1.pins.len(),
        3,
        "both physical placements contribute their pad records"
    );
    assert_eq!(
        tp1.pins
            .iter()
            .map(|pin| pin.number.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["1", "2"]),
        "the three physical records represent two electrical pin numbers"
    );
    assert!(
        tp1.properties
            .iter()
            .any(|(key, _)| key == "reference_ambiguous"),
        "a hierarchy-free duplicate is merged only by structural evidence, so that inference must remain visible"
    );
    assert!(board.component("TP1_2").is_none());
}

#[test]
fn repeated_designators_in_distinct_channels_remain_distinct_parts() {
    let mut comps = Vec::new();
    comps.extend(props(
        "|LAYER=TOP|X=0mil|Y=0mil|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\FLASH1",
    ));
    comps.extend(props(
        "|LAYER=TOP|X=100mil|Y=0mil|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\FLASH2",
    ));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("1", 1, 1, 1, 1.0, 0.0, 0.5));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(board.components.len(), 2);
    assert!(board.component("R1@FLASH1").is_some());
    assert!(board.component("R1@FLASH2").is_some());
    assert!(board.component("R1_2").is_none());
}

#[test]
fn authoritative_unique_ids_distinguish_same_designator_in_one_hierarchy() {
    // Real Altium files can repeat the compiled source designator and full
    // hierarchy for several physical component records.  UNIQUEID is the
    // authoritative record identity in that case (test-padshapes.PcbDoc does
    // exactly this); ignoring it collapses the whole population into one part.
    let mut comps = Vec::new();
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R14|SOURCEHIERARCHICALPATH=\\WMO-sensor|UNIQUEID=YAFHHBJN",
    ));
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R14|SOURCEHIERARCHICALPATH=\\WMO-sensor|UNIQUEID=HIIFAIMR",
    ));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 1, 2.0, 0.0, 0.5));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(board.components.len(), 2);
    let references: std::collections::BTreeSet<_> = board
        .components
        .iter()
        .map(|component| component.reference.as_str())
        .collect();
    assert_eq!(references.len(), 2, "UNIQUEID must prevent a false merge");
    assert!(board.components.iter().all(|component| component
        .properties
        .iter()
        .any(|(key, value)| key == "source_unique_id" && !value.is_empty())));
}

#[test]
fn same_authoritative_unique_id_merges_split_component_records() {
    // Conversely, two records carrying the same source identity are one
    // logical part even if each record contributes a disjoint physical pad.
    let mut comps = Vec::new();
    for _ in 0..2 {
        comps.extend(props(
            "|LAYER=TOP|PATTERN=TESTPOINT|SOURCEDESIGNATOR=TP1|SOURCEHIERARCHICALPATH=\\ROOT|UNIQUEID=ABC123",
        ));
    }
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 1, 1.0, 0.0, 0.5));

    let board = ExtractedBoard::from_altium_pcb(&build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]))
    .expect("extract");

    assert_eq!(board.components.len(), 1);
    assert_eq!(board.components[0].reference, "TP1");
    assert_eq!(board.components[0].pins.len(), 2);
}

#[test]
fn same_hierarchy_without_unique_id_or_shared_pad_stays_distinct() {
    // A hierarchy is a channel location, not a physical component identity.
    // Without UNIQUEID or a shared identically-netted pad, disjoint records in
    // the same hierarchy must remain separate and explicitly ambiguous.
    let mut comps = Vec::new();
    for _ in 0..2 {
        comps.extend(props(
            "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R14|SOURCEHIERARCHICALPATH=\\WMO-sensor",
        ));
    }
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 1, 1.0, 0.0, 0.5));

    let board = ExtractedBoard::from_altium_pcb(&build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]))
    .expect("extract");

    assert_eq!(board.components.len(), 2);
    assert!(board.components.iter().all(|component| component
        .properties
        .iter()
        .any(|(key, _)| key == "reference_ambiguous")));
}

#[test]
fn full_hierarchical_paths_and_real_reference_collisions_remain_distinct() {
    let mut comps = Vec::new();
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\A\\BANK",
    ));
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\B\\BANK",
    ));
    // This genuine source designator is exactly the first generated candidate.
    comps.extend(props("|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1@A/BANK"));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("1", 1, 1, 1, 2.0, 0.0, 0.5));
    pads.extend(pad_record("1", 1, 0, 2, 4.0, 0.0, 0.5));

    let board = ExtractedBoard::from_altium_pcb(&build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]))
    .expect("extract");

    assert_eq!(board.components.len(), 3);
    assert!(
        board.component("R1@A/BANK").is_some(),
        "the genuine designator must never be overwritten"
    );
    assert!(board.component("R1@B/BANK").is_some());
    let a_channel = board
        .components
        .iter()
        .find(|component| {
            component
                .properties
                .iter()
                .any(|(key, value)| key == "source_hierarchical_path" && value == "A/BANK")
        })
        .expect("A/BANK channel retained");
    assert_ne!(a_channel.reference, "R1@A/BANK");
    assert!(a_channel.reference.starts_with("R1@A/BANK@source-"));
}

#[test]
fn mixed_missing_channel_metadata_never_collapses_all_replicas() {
    let mut comps = Vec::new();
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\FLASH1",
    ));
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\FLASH2",
    ));
    comps.extend(props("|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1"));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("1", 1, 1, 1, 2.0, 0.0, 0.5));
    pads.extend(pad_record("1", 1, 0, 2, 4.0, 0.0, 0.5));

    let board = ExtractedBoard::from_altium_pcb(&build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]))
    .expect("extract");

    assert_eq!(board.components.len(), 3);
    assert!(board.component("R1@FLASH1").is_some());
    assert!(board.component("R1@FLASH2").is_some());
    let missing = board
        .components
        .iter()
        .find(|component| component.reference.starts_with("R1@unknown-"))
        .expect("missing path kept separate");
    assert!(missing
        .properties
        .iter()
        .any(|(key, _)| key == "reference_ambiguous"));
}

#[test]
fn hierarchy_free_duplicate_pin_numbers_on_different_nets_are_not_merged() {
    let mut comps = Vec::new();
    comps.extend(props("|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1"));
    comps.extend(props("|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1"));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("1", 1, 1, 1, 0.0, 0.0, 0.5));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(board.components.len(), 2);
    assert!(board
        .components
        .iter()
        .all(|component| component.reference.starts_with("R1@record-")));
    assert!(board.components.iter().all(|component| component
        .properties
        .iter()
        .any(|(key, _)| key == "reference_ambiguous")));
    assert!(board.components.iter().all(|component| component
        .properties
        .iter()
        .any(|(key, _)| key == "duplicate_reference_conflict")));
    let drc = ExtractedBoard::altium_drc(&bytes).expect("DRC");
    assert_eq!(
        drc.short_count(),
        1,
        "ambiguous records must also be distinct DRC owners"
    );
}

#[test]
fn hierarchy_free_disjoint_pads_are_distinct_drc_owners() {
    let mut comps = Vec::new();
    comps.extend(props("|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1"));
    comps.extend(props("|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1"));
    let mut pads = Vec::new();
    // Disjoint pad numbers are not proof that these records describe one
    // physical footprint.  If they overlap on different nets, merging their
    // owner would invoke the same-footprint exemption and hide a real short.
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 0.5));
    pads.extend(pad_record("2", 1, 1, 1, 0.0, 0.0, 0.5));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(board.components.len(), 2);
    assert!(board
        .components
        .iter()
        .all(|component| component.reference.starts_with("R1@record-")));
    let drc = ExtractedBoard::altium_drc(&bytes).expect("DRC");
    assert_eq!(
        drc.short_count(),
        1,
        "disjoint ambiguous records must not share a same-owner exemption"
    );
}

#[test]
fn channel_qualified_components_are_distinct_owners_for_drc() {
    let mut comps = Vec::new();
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\FLASH1",
    ));
    comps.extend(props(
        "|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\FLASH2",
    ));
    let mut pads = Vec::new();
    // Different nets, exactly overlapping. These are different channel
    // instances and therefore a real short, not two custom pads owned by one
    // physical component.
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 1.0));
    pads.extend(pad_record("1", 1, 1, 1, 0.0, 0.0, 1.0));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let report = ExtractedBoard::altium_drc(&bytes).expect("DRC runs");
    assert_eq!(
        report.short_count(),
        1,
        "R1@FLASH1 and R1@FLASH2 must not share a same-owner exemption"
    );
}

#[test]
fn missing_designators_stay_distinct_with_stable_placeholders() {
    let mut comps = Vec::new();
    comps.extend(props("|LAYER=TOP|PATTERN=MOUNTINGHOLE"));
    comps.extend(props("|LAYER=TOP|PATTERN=MOUNTINGHOLE"));
    let mut pads = Vec::new();
    pads.extend(pad_record("1", 1, 0, 0, 0.0, 0.0, 1.0));
    pads.extend(pad_record("1", 1, 1, 1, 5.0, 0.0, 1.0));

    let bytes = build_pcbdoc(&[
        ("Nets6", two_net_stream()),
        ("Components6", comps),
        ("Pads6", pads),
    ]);
    let board = ExtractedBoard::from_altium_pcb(&bytes).expect("extract");

    assert_eq!(board.components.len(), 2);
    assert!(board.component("UNK0").is_some());
    assert!(board.component("UNK1").is_some());
    assert!(board
        .components
        .iter()
        .all(|component| !component.reference.is_empty()));
}

#[test]
fn auto_detects_altium_binary() {
    let bytes = two_resistor_board();
    // OLE2 magic present.
    assert_eq!(&bytes[0..4], &[0xD0, 0xCF, 0x11, 0xE0]);
    // from_auto_bytes routes a real .PcbDoc to the Altium path.
    let board = ExtractedBoard::from_auto_bytes(&bytes)
        .expect("recognised as binary board")
        .expect("extracts");
    assert_eq!(board.components.len(), 2);

    // A non-Altium OLE2 file (magic but no Nets stream) is not claimed.
    let empty_ole = build_pcbdoc(&[("Junk", vec![1, 2, 3])]);
    assert!(
        ExtractedBoard::from_auto_bytes(&empty_ole).is_none(),
        "OLE2 without Altium streams must not be claimed as a .PcbDoc"
    );

    // Plain text is not binary-claimed (the caller falls back to from_auto).
    assert!(ExtractedBoard::from_auto_bytes(b"(kicad_pcb)").is_none());
}

#[test]
fn drc_detects_a_deliberate_short() {
    // Build a board where two different-net tracks overlap: N0 and N1 both run
    // through the same point on F.Cu.
    let mut nets = Vec::new();
    nets.extend(props("|NAME=A"));
    nets.extend(props("|NAME=B"));
    let mut tracks = Vec::new();
    // Net A: a horizontal track.
    tracks.extend(track_record(1, 0, 0xFFFF, 0.0, 0.0, 10.0, 0.0, 0.25));
    // Net B: a vertical track crossing it at (5,0) -> overlap == short.
    tracks.extend(track_record(1, 1, 0xFFFF, 5.0, -5.0, 5.0, 5.0, 0.25));
    let bytes = build_pcbdoc(&[("Nets6", nets), ("Tracks6", tracks)]);

    let report = ExtractedBoard::altium_drc(&bytes).expect("drc runs");
    assert!(
        report.short_count() >= 1,
        "crossing tracks of different nets must short"
    );
    let short = report.shorts().next().unwrap();
    assert_eq!(short.kind, ViolationKind::Short);
    assert!(
        (short.net_a_name == "A" && short.net_b_name == "B")
            || (short.net_a_name == "B" && short.net_b_name == "A"),
        "short is A<->B, got {}<->{}",
        short.net_a_name,
        short.net_b_name
    );
}

#[test]
fn drc_clean_when_nets_match() {
    // Same crossing geometry but both tracks on the SAME net: no short.
    let mut nets = Vec::new();
    nets.extend(props("|NAME=A"));
    let mut tracks = Vec::new();
    tracks.extend(track_record(1, 0, 0xFFFF, 0.0, 0.0, 10.0, 0.0, 0.25));
    tracks.extend(track_record(1, 0, 0xFFFF, 5.0, -5.0, 5.0, 5.0, 0.25));
    let bytes = build_pcbdoc(&[("Nets6", nets), ("Tracks6", tracks)]);

    let report = ExtractedBoard::altium_drc(&bytes).expect("drc runs");
    assert_eq!(report.short_count(), 0, "same-net crossing is not a short");
}
