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
