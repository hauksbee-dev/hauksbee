//! EasyEDA board import: detection, known-answer connectivity, the accounting of
//! what was and was not read, and hostile input.
//!
//! The fixtures are real public boards (see `tests/fixtures/easyeda/README.md`
//! for licences and pinned sources). There is no KiCad export of them upstream,
//! so the known answers below were derived independently from the raw files by a
//! separate script (a plain Python read of the same records) and typed in.

use hauksbee_extract::easyeda::{
    extract, extract_archive, looks_like_easyeda, looks_like_easyeda_archive,
    looks_like_easyeda_pro, looks_like_easyeda_pro_v3, looks_like_easyeda_std, EasyedaExtraction,
    Edition,
};
use hauksbee_extract::reader::{BoardReader, EasyedaArchiveReader, EasyedaReader, Registry};
use hauksbee_extract::{ExtractError, ExtractedBoard};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const NOFILTER: &str = "rpitx-coax-nofilter.json";
const GP1212: &str = "rpitx-coax-gp1212-gp731.json";
const NANO: &str = "nano-hub-pcb1.epcb";

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/easyeda")
        .join(name)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn read(name: &str) -> EasyedaExtraction {
    extract(&fixture(name)).unwrap_or_else(|e| panic!("{name} must read: {e}"))
}

/// The message of a refusal; panics (failing the test) if the bytes read.
fn refusal(bytes: &[u8]) -> String {
    match extract(bytes) {
        Ok(got) => panic!(
            "expected a refusal, but read {} parts and {} nets",
            got.board.components.len(),
            got.board.nets.len()
        ),
        Err(e) => e.to_string(),
    }
}

fn says(msg: &str, phrases: &[&str]) {
    for phrase in phrases {
        assert!(
            msg.contains(phrase),
            "message must mention {phrase:?}; got: {msg}"
        );
    }
}

fn net_name(board: &ExtractedBoard, id: Option<i64>) -> String {
    id.and_then(|id| board.nets.iter().find(|n| n.id == id))
        .map_or_else(|| "-".to_string(), |n| n.name.clone())
}

/// Reference -> sorted `number=net` pairs (`-` for an unconnected pad).
fn pin_map(board: &ExtractedBoard) -> BTreeMap<String, Vec<String>> {
    board
        .components
        .iter()
        .map(|c| {
            let mut pins: Vec<String> = c
                .pins
                .iter()
                .map(|p| format!("{}={}", p.number, net_name(board, p.net)))
                .collect();
            pins.sort();
            (c.reference.clone(), pins)
        })
        .collect()
}

/// Parse `"R1 1=A 2=B\nR2 ..."` into the same shape as [`pin_map`].
fn expected(text: &str) -> BTreeMap<String, Vec<String>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace();
            let reference = it.next().expect("reference").to_string();
            let mut pins: Vec<String> = it.map(str::to_string).collect();
            pins.sort();
            (reference, pins)
        })
        .collect()
}

fn assert_pin_map(board: &ExtractedBoard, want: &str) {
    let got = pin_map(board);
    let want = expected(want);
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "the parts read differ from the known answer"
    );
    for (reference, pins) in &want {
        assert_eq!(&got[reference], pins, "pads and nets of {reference}");
    }
}

// ── detection ────────────────────────────────────────────────────────────────

#[test]
fn the_registry_routes_each_fixture_to_the_easyeda_reader() {
    let reg = Registry::builtin();
    for name in [NOFILTER, GP1212, NANO] {
        let bytes = fixture(name);
        let path = fixture_path(name);
        let with_path = reg.detect(&bytes, Some(&path)).map(BoardReader::name);
        assert_eq!(with_path, Some("easyeda"), "{name} by path and content");
        // Content alone decides: a renamed or pasted file still reads.
        let no_path = reg.detect(&bytes, None).map(BoardReader::name);
        assert_eq!(no_path, Some("easyeda"), "{name} by content alone");
    }
}

#[test]
fn only_the_easyeda_reader_claims_an_easyeda_board() {
    for name in [NOFILTER, GP1212, NANO] {
        let bytes = fixture(name);
        assert!(EasyedaReader.detects(&bytes, None), "{name}");
        assert!(!EasyedaArchiveReader.detects(&bytes, None), "{name}");
        assert!(looks_like_easyeda(&bytes), "{name}");
    }
    assert!(looks_like_easyeda_std(&fixture(NOFILTER)));
    assert!(!looks_like_easyeda_pro(&fixture(NOFILTER)));
    assert!(looks_like_easyeda_pro(&fixture(NANO)));
    assert!(!looks_like_easyeda_std(&fixture(NANO)));
}

#[test]
fn easyeda_does_not_claim_other_formats() {
    // Every committed KiCad board and netlist in this crate and the repo.
    let roots = [
        "tests/fixtures",
        "../../testdata/boards",
        "../../testdata",
        "../../crates/hauksbee-engine/tests/fixtures",
    ];
    let mut seen = 0;
    for root in roots {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(root);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !["kicad_pcb", "kicad_sch", "net", "d356", "brd", "xml"].contains(&ext) {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            assert!(
                !EasyedaReader.detects(&bytes, Some(&path)),
                "{} must not be claimed as EasyEDA",
                path.display()
            );
            assert!(!looks_like_easyeda(&bytes), "{}", path.display());
            seen += 1;
        }
    }
    assert!(seen >= 5, "expected a corpus of other formats, saw {seen}");
    for other in [
        &b"{\"a\":1}"[..],
        b"[1,2,3]",
        b"{\"head\":{}}",
        b"(kicad_pcb (version 20240108))",
        b"<eagle version=\"9\"></eagle>",
        b"",
    ] {
        assert!(!looks_like_easyeda(other));
    }
}

#[test]
fn a_non_pcb_easyeda_document_is_claimed_so_it_is_refused_by_name() {
    // Claimed (so the refusal is specific) but never read as a board.
    let schematic = json!({"head": {"docType": "1"}, "canvas": "CA~1", "shape": []}).to_string();
    assert!(looks_like_easyeda_std(schematic.as_bytes()));
    let msg = refusal(schematic.as_bytes());
    says(&msg, &["schematic", "not a PCB", "PCB editor"]);
}

#[test]
fn pro_v3_log_files_are_claimed_and_refused_by_name() {
    let v3 = b"{\"type\":\"DOCHEAD\",\"version\":\"3\"}||{\"docType\":\"PCB\"}\n";
    assert!(looks_like_easyeda_pro_v3(v3));
    assert!(looks_like_easyeda(v3));
    let msg = refusal(v3);
    says(&msg, &["v3", "Gerber", ".epro"]);
}

// ── known-answer connectivity ────────────────────────────────────────────────

const HEADER_H1: &str = "H1 12=- 11=- 10=- 9=GND 8=- 7=H1_7 6=GND 5=- 4=- 3=- 2=- 1=-";
const J1: &str = "J1 GND=GND GND=GND SIG=J1_SIG GND=GND GND=GND";

#[test]
fn rpitx_nofilter_matches_the_known_answer() {
    let got = read(NOFILTER);
    assert_eq!(got.stats.edition, Edition::Standard);
    assert_pin_map(
        &got.board,
        &format!("C1 1=H1_7 2=J1_SIG\n{J1}\n{HEADER_H1}"),
    );
    // Nets present on the board, whether or not a pad uses them: the three
    // used by pads plus the copper-only nets named by tracks, vias and pours.
    let names: Vec<&str> = got.board.nets.iter().map(|n| n.name.as_str()).collect();
    for want in ["GND", "H1_7", "J1_SIG"] {
        assert!(names.contains(&want), "net {want} missing from {names:?}");
    }
    assert!(!names.contains(&""), "an empty name is no net: {names:?}");
    // Net ids are unique and stable in first-seen order.
    let mut ids: Vec<i64> = got.board.nets.iter().map(|n| n.id).collect();
    let before = ids.clone();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids, before);
    assert_eq!(got.stats.connected_pins, 10);
    // The 100n on C1 is the only value the file carries.
    let c1 = &got.board.components[0];
    assert_eq!((c1.reference.as_str(), c1.value.as_str()), ("C1", "100n"));
    assert_eq!(c1.footprint, "0805 WITHOUT MASK");
}

#[test]
fn rpitx_gp1212_matches_the_known_answer() {
    let got = read(GP1212);
    assert_pin_map(
        &got.board,
        &format!(
            "C1 1=H1_7 2=C1_2\n{J1}\n{HEADER_H1}\n\
             U1 8=GND 4=GND 7=GND 6=J1_SIG 5=GND 1=GND 2=C1_2 3=GND"
        ),
    );
    assert_eq!(got.stats.connected_pins, 18);
}

#[test]
fn nano_hub_pro_matches_the_known_answer() {
    let got = read(NANO);
    assert_eq!(got.stats.edition, Edition::Pro);
    assert_eq!(got.stats.editor_version, "2.2.40.3");
    assert_pin_map(
        &got.board,
        "C1 2=GND 1=$1N1772
C2 2=GND 1=$1N1778
C3 2=GND 1=+5V
C4 2=GND 1=+5V
C5 2=GND 1=+5V
C6 2=GND 1=+5V
C7 2=GND 1=+5V
D1 2=$1N2656 1=+5V
LED1 1=GND 2=$1N3391
R1 2=$1N1994 1=+5V
R2 2=+5V 1=$1N1986
R3 2=$1N2093 1=+5V
R4 2=+5V 1=$1N2096
R5 2=$1N2201 1=GND
R6 2=GND 1=$1N2204
R7 2=$1N3391 1=+5V
U1 1=D4- 2=D4+ 3=D3- 4=D3+ 5=D2- 6=D2+ 7=D1- 8=D1+ 16=$1N1337 15=$1N1394 14=$1N1772 13=$1N1778 12=GND 11=+5V 10=D+ 9=D-
USB1 1=+5V 2=D1+ 3=D1- 4=GND 5=GND 6=GND
USB2 1=+5V 2=D2+ 3=D2- 4=GND 5=GND 6=GND
USBC1 B8=- A5=$1N2204 B7=D- A6=D+ A7=D- B6=D+ A8=- B5=$1N2201 A1=GND B12=GND A4=$1N2656 B9=$1N2656 B4=$1N2656 A9=$1N2656 B1=GND A12=GND 2=GND 1=GND 0=GND 3=GND
USBC2 B8=- A5=$1N1986 B7=D3- A6=D3+ A7=D3- B6=D3+ A8=- B5=$1N1994 A1=GND B12=GND A4=+5V B9=+5V B4=+5V A9=+5V B1=GND A12=GND 2=GND 1=GND 0=GND 3=GND
USBC3 B8=- A5=$1N2096 B7=D4- A6=D4+ A7=D4- B6=D4+ A8=- B5=$1N2093 A1=GND B12=GND A4=+5V B9=+5V B4=+5V A9=+5V B1=GND A12=GND 2=GND 1=GND 0=GND 3=GND
X1 1=$1N1337 2=GND 3=$1N1394 4=GND",
    );
    let pads: usize = got.board.components.iter().map(|c| c.pins.len()).sum();
    assert_eq!(pads, 124, "every PAD_NET record is a pad");
    // 24 named nets: GND, +5V, D+, D-, D1+..D4-, and the $1N* nets.
    assert_eq!(
        got.board.nets.iter().filter(|n| !n.name.is_empty()).count(),
        24
    );
}

#[test]
fn a_part_with_no_value_in_the_file_is_unresolved_not_guessed() {
    use hauksbee_extract::altium::VALUE_UNRESOLVED_KEY;
    let std = read(GP1212);
    for c in &std.board.components {
        let unresolved = c.properties.iter().any(|(k, _)| k == VALUE_UNRESOLVED_KEY);
        assert_eq!(
            unresolved,
            c.value.is_empty(),
            "{}: value {:?} and unresolved flag must agree",
            c.reference,
            c.value
        );
    }
    assert_eq!(std.stats.value_unresolved, ["J1", "U1", "H1"]);
    // Pro keeps every value in the project library, so none is invented.
    let pro = read(NANO);
    assert!(pro.board.components.iter().all(|c| c.value.is_empty()));
    assert_eq!(pro.stats.value_unresolved.len(), 23);
    assert!(pro.board.components.iter().all(|c| c.footprint.is_empty()));
}

#[test]
fn placement_and_side_are_read() {
    let got = read(NOFILTER);
    let by = |r: &str| {
        got.board
            .components
            .iter()
            .find(|c| c.reference == r)
            .unwrap()
    };
    // C1 is placed at (4073.4408, 3327.8345) in 10 mil units from origin
    // (4000, 3000): (73.4408 * 0.254, 327.8345 * 0.254) mm.
    let (x, y, _) = by("C1").position.expect("C1 position");
    let (want_x, want_y) = (73.4408 * 0.254, 327.8345 * 0.254);
    assert!((x - want_x).abs() < 1e-6, "x = {x}");
    assert!((y - want_y).abs() < 1e-6, "y = {y}");
    assert_eq!(by("C1").layer, "F.Cu");
    assert_eq!(by("J1").layer, "B.Cu", "J1's pads are on the bottom layer");
}

// ── accounting: what was counted, read past, and refused ─────────────────────

#[test]
fn copper_is_counted_against_the_raw_records() {
    // Counts agree with an independent tally of the raw TRACK/VIA/COPPERAREA
    // (Standard) and LINE/VIA/POUR records (Pro) on copper layers.
    let a = read(NOFILTER).stats.copper;
    assert_eq!((a.tracks, a.arcs, a.vias, a.pours), (3, 1, 218, 3));
    let b = read(GP1212).stats.copper;
    assert_eq!((b.tracks, b.arcs, b.vias, b.pours), (4, 1, 199, 4));
    let c = read(NANO).stats.copper;
    assert_eq!((c.tracks, c.arcs, c.vias, c.pours), (263, 0, 23, 2));
    assert!(c.layers.iter().any(|l| l.contains("Top")));
    assert!(c.layers.iter().any(|l| l.contains("Bottom")));
}

#[test]
fn the_notes_say_what_was_not_read_and_not_checked() {
    let notes = read(GP1212).stats.notes().join("\n");
    says(
        &notes,
        &[
            "EasyEDA Standard",
            "not reverse-engineered from copper",
            "clearance DRC",
            "were not run",
            "NOT read",
            "TEXT on a copper layer",
            "Read past",
            "NC",
        ],
    );
    let pro = read(NANO).stats.notes().join("\n");
    says(
        &pro,
        &[
            "EasyEDA Pro",
            "not reverse-engineered from copper",
            "libraries",
            "unresolved footprint",
            "PAD_NET",
            "computed copper-pour fill polygons",
        ],
    );
}

fn std_doc(shapes: &[&str]) -> Vec<u8> {
    json!({"head": {"docType": "3", "editorVersion": "6.4.17"}, "canvas": "CA~1", "shape": shapes})
        .to_string()
        .into_bytes()
}

const PART: &str = "LIB~4010~3010~package`P1`~0~~g1~1~id~1~0~#@$TEXT~P~4010~3000~0.6~0~0~3~~4.5~R1~~none~g2~~0~#@$PAD~RECT~4000~3000~4~4~1~GND~1~0~4000 3000~0~~0~g3~0";

#[test]
fn an_unknown_record_is_named_not_dropped() {
    let got = extract(&std_doc(&[PART, "FUTURE_WIDGET~1~2~3", "FUTURE_WIDGET~4"])).expect("reads");
    assert_eq!(got.board.components.len(), 1);
    assert_eq!(got.stats.unsupported.get("FUTURE_WIDGET"), Some(&2));
    let notes = got.stats.notes().join("\n");
    says(&notes, &["NOT read", "2 x FUTURE_WIDGET"]);
}

#[test]
fn a_non_text_shape_entry_is_counted_as_unsupported() {
    let doc = json!({
        "head": {"docType": "3"}, "canvas": "CA~1",
        "shape": [PART, 7, null, {"a": 1}, ["x"]],
    })
    .to_string();
    let got = extract(doc.as_bytes()).expect("reads");
    assert_eq!(
        got.stats.unsupported.get("shape entry that is not text"),
        Some(&4)
    );
}

#[test]
fn pro_unknown_records_are_named_and_known_ones_read_past() {
    let text = [
        r#"["DOCTYPE","PCB","1.8"]"#,
        r#"["HEAD",{"editorVersion":"2.2.0"}]"#,
        r#"["COMPONENT","c1",0,1,1000,2000,0,{},0]"#,
        r#"["ATTR","a1",0,"c1",3,null,null,"Designator","R1"]"#,
        r#"["PAD_NET","c1","1","GND","p1"]"#,
        r#"["HOLOGRAM","h1",1,2]"#,
        r#"["STRING","s1",0,1]"#,
        r#"["POURED","pp",1]"#,
    ]
    .join("\n");
    let got = extract(text.as_bytes()).expect("reads");
    assert_eq!(got.board.components.len(), 1);
    assert_pin_map(&got.board, "R1 1=GND");
    assert_eq!(got.stats.unsupported.get("HOLOGRAM"), Some(&1));
    assert_eq!(got.stats.read_past.get("text labels"), Some(&1));
    assert_eq!(
        got.stats
            .read_past
            .get("computed copper-pour fill polygons"),
        Some(&1)
    );
}

#[test]
fn a_pad_net_for_an_unknown_component_is_a_disagreement_note() {
    let text = [
        r#"["DOCTYPE","PCB","1.8"]"#,
        r#"["PAD_NET","ghost","1","GND","p1"]"#,
    ]
    .join("\n");
    let got = extract(text.as_bytes()).expect("reads");
    let notes = got.stats.notes().join("\n");
    says(&notes, &["PAD_NET", "ghost", "not read"]);
}

#[test]
fn a_part_without_a_designator_gets_a_flagged_synthetic_one() {
    use hauksbee_extract::altium::REFERENCE_UNRESOLVED_KEY;
    let part = "LIB~4010~3010~package`P1`~0~~g1~1~id~1~0~#@$PAD~RECT~4000~3000~4~4~1~GND~1~0~4000 3000~0~~0~g3~0";
    let got = extract(&std_doc(&[part])).expect("reads");
    let c = &got.board.components[0];
    assert!(c.reference.starts_with("UNK"), "{}", c.reference);
    assert!(c
        .properties
        .iter()
        .any(|(k, _)| k == REFERENCE_UNRESOLVED_KEY));
}

#[test]
fn the_at_suffix_on_a_pad_number_is_removed() {
    let part = "LIB~4010~3010~package`P1`~0~~g1~1~id~1~0~#@$TEXT~P~4010~3000~0.6~0~0~3~~4.5~J1~~none~g2~~0~#@$PAD~RECT~4000~3000~4~4~1~GND~GND@0~0~4000 3000~0~~0~g3~0#@$PAD~RECT~4010~3000~4~4~1~GND~GND@1~0~4010 3000~0~~0~g4~0";
    let got = extract(&std_doc(&[part])).expect("reads");
    assert_pin_map(&got.board, "J1 GND=GND GND=GND");
}

// ── hostile input ────────────────────────────────────────────────────────────

#[test]
fn truncated_json_is_refused_as_truncated() {
    for name in [NOFILTER, GP1212] {
        let bytes = fixture(name);
        let msg = refusal(&bytes[..bytes.len() / 2]);
        says(&msg, &["truncated", "nothing was read", "Re-export"]);
    }
    // Pro: a file cut mid-record.
    let pro = fixture(NANO);
    let cut = &pro[..pro.len() / 2];
    let msg = refusal(cut);
    says(&msg, &["Pro", "line", "nothing was read"]);
}

#[test]
fn wrong_types_are_refused_with_the_structure_they_should_have() {
    let cases: Vec<(String, &[&str])> = vec![
        (
            json!({"head": {"docType": "3"}, "shape": 7}).to_string(),
            &["'shape'", "a number", "list of records"],
        ),
        (
            json!({"head": {"docType": "3"}, "shape": {"a": 1}}).to_string(),
            &["'shape'", "an object"],
        ),
        (
            json!({"head": {"docType": "3"}, "shape": "LIB~1"}).to_string(),
            &["'shape'", "a string"],
        ),
        (
            json!({"head": {"docType": "3"}, "canvas": "CA~1"}).to_string(),
            &["no 'shape'"],
        ),
        (
            json!({"dataStr": 5, "title": "x"}).to_string(),
            &["no 'docType'"],
        ),
        (
            json!({"dataStr": "not json at all {", "docType": "3"}).to_string(),
            &["not valid JSON"],
        ),
        (
            "[1,2,3]".to_string(),
            &["a list", "rather than an EasyEDA document"],
        ),
        (
            json!({"head": {}, "canvas": "x", "docType": "3", "shape": []}).to_string(),
            &[],
        ),
        (
            json!({"head": {"docType": 4}, "canvas": "x", "shape": []}).to_string(),
            &["footprint", "not a PCB"],
        ),
        (
            json!({"head": {"docType": "14"}, "canvas": "x", "shape": []}).to_string(),
            &["not a PCB"],
        ),
        (
            json!({"head": {"foo": 1}, "canvas": "x", "shape": []}).to_string(),
            &["docType"],
        ),
    ];
    for (doc, phrases) in cases {
        // The documents above that carry a docType and canvas are sniffed as
        // EasyEDA; the others are fed straight to the reader.
        let result = extract(doc.as_bytes());
        match (phrases.is_empty(), result) {
            (true, Ok(_)) => {}
            (true, Err(e)) => panic!("{doc} should read, got {e}"),
            (false, Ok(_)) => panic!("{doc} should be refused"),
            (false, Err(e)) => says(&e.to_string(), phrases),
        }
    }
}

#[test]
fn a_data_str_wrapped_document_reads_whether_object_or_string() {
    let inner = json!({"head": {"docType": "3"}, "canvas": "CA~1", "shape": [PART]});
    let as_object = json!({"title": "Wrapped", "dataStr": inner}).to_string();
    let as_string = json!({"title": "Wrapped", "dataStr": inner.to_string()}).to_string();
    for doc in [as_object, as_string] {
        let got = extract(doc.as_bytes()).expect("reads");
        assert_eq!(got.board.name, "Wrapped");
        assert_pin_map(&got.board, "R1 1=GND");
    }
}

#[test]
fn non_finite_coordinates_are_refused_never_passed_through() {
    for bad in ["NaN", "inf", "-inf", "1e999", "Infinity"] {
        let part = format!(
            "LIB~{bad}~3010~package`P1`~0~~g1~1~id~1~0~#@$TEXT~P~4010~3000~0.6~0~0~3~~4.5~R1~~none~g2~~0~#@$PAD~RECT~4000~3000~4~4~1~GND~1~0~4000 3000~0~~0~g3~0"
        );
        let err = extract(&std_doc(&[&part])).expect_err("position must be refused");
        assert!(matches!(err, ExtractError::Corrupt(_)), "{bad}: {err:?}");
        says(
            &err.to_string(),
            &["not a finite number", "nothing was read"],
        );

        let pad = format!(
            "LIB~4010~3010~package`P1`~0~~g1~1~id~1~0~#@$TEXT~P~4010~3000~0.6~0~0~3~~4.5~R1~~none~g2~~0~#@$PAD~RECT~{bad}~3000~4~4~1~GND~1~0~4000 3000~0~~0~g3~0"
        );
        assert!(matches!(
            extract(&std_doc(&[&pad])),
            Err(ExtractError::Corrupt(_))
        ));

        let track = format!("TRACK~1~1~GND~4000 3000 {bad} 3000~g9~0");
        assert!(matches!(
            extract(&std_doc(&[PART, &track])),
            Err(ExtractError::Corrupt(_))
        ));
    }
    // Pro: a non-finite number in a LINE or COMPONENT position.
    for row in [
        r#"["COMPONENT","c1",0,1,1e999,2,0,{},0]"#,
        r#"["LINE","l1",0,"GND",1,0,0,1e999,5]"#,
    ] {
        let text = ["[\"DOCTYPE\",\"PCB\",\"1.8\"]", row].join("\n");
        // serde_json rejects an out-of-range literal itself; either way it is a
        // named refusal and not a panic or a wrong board.
        let msg = refusal(text.as_bytes());
        assert!(!msg.is_empty());
    }
}

#[test]
fn huge_strings_are_bounded_in_the_notes_and_do_not_panic() {
    let huge = "A".repeat(1_000_000);
    // A million-character net name, designator, attribute, record name.
    let part = format!(
        "LIB~4010~3010~package`{huge}`k`{huge}`~0~~g1~1~id~1~0~#@$TEXT~P~4010~3000~0.6~0~0~3~~4.5~{huge}~~none~g2~~0~#@$PAD~RECT~4000~3000~4~4~1~{huge}~{huge}~0~4000 3000~0~~0~g3~0"
    );
    let weird = format!("{huge}~1~2");
    let got = extract(&std_doc(&[&part, &weird])).expect("reads without panicking");
    let notes = got.stats.notes();
    let total: usize = notes.iter().map(String::len).sum();
    assert!(
        total < 20_000,
        "notes must stay bounded, were {total} bytes"
    );
    let c = &got.board.components[0];
    for (k, v) in &c.properties {
        assert!(k.chars().count() <= 300, "property key unbounded");
        assert!(v.chars().count() <= 300, "property value unbounded");
    }
    assert_eq!(got.stats.unsupported.len(), 1);
    assert!(got
        .stats
        .unsupported
        .keys()
        .all(|k| k.chars().count() <= 300));
}

#[test]
fn a_hundred_thousand_records_read_in_bounded_time() {
    let shapes: Vec<String> = (0..100_000)
        .map(|i| format!("TRACK~1~1~N{}~{i} 3000 {i} 3010~g{i}~0", i % 500))
        .collect();
    let refs: Vec<&str> = shapes.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let got = extract(&std_doc(&refs)).expect("reads");
    assert_eq!(got.stats.copper.tracks, 100_000);
    assert!(
        started.elapsed().as_secs() < 20,
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn deeply_nested_json_is_refused_not_a_stack_overflow() {
    let depth = 100_000;
    let mut doc = String::from("{\"docType\":\"3\",\"shape\":");
    doc.push_str(&"[".repeat(depth));
    doc.push_str(&"]".repeat(depth));
    doc.push('}');
    let msg = refusal(doc.as_bytes());
    says(&msg, &["EasyEDA", "nothing was read"]);
}

#[test]
fn pro_files_that_are_not_pro_boards_are_refused_by_name() {
    let msg = refusal(b"[\"DOCTYPE\",\"SCH\",\"1.8\"]\n");
    says(&msg, &["SCH", "not a PCB"]);
    let msg = refusal(b"[\"DOCTYPE\",\"PCB\",\"1.8\"]\nnot json\n");
    says(&msg, &["line 2", "nothing was read"]);
    let msg = refusal(b"[\"DOCTYPE\",\"PCB\",\"1.8\"]\n{\"a\":1}\n");
    says(&msg, &["line 2", "not a record"]);
    let msg = refusal(b"[\"DOCTYPE\",\"PCB\",\"1.8\"]\n[\"\",1]\n");
    says(&msg, &["line 2", "without a name"]);
    let msg = refusal(b"[\"DOCTYPE\"");
    says(&msg, &["nothing was read"]);
}

#[test]
fn empty_and_binary_inputs_are_refused_without_panicking() {
    for bytes in [
        &b""[..],
        b"   \n\t ",
        b"\xff\xfe\x00\x01garbage",
        b"{",
        b"[",
        b"\"DOCTYPE\"",
        b"null",
    ] {
        let _ = extract(bytes).expect_err("must refuse");
        let _ = extract_archive(bytes).expect_err("must refuse");
    }
}

#[test]
fn a_byte_order_mark_is_tolerated() {
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend(fixture(NOFILTER));
    assert!(looks_like_easyeda_std(&bytes));
    assert!(extract(&bytes).is_ok());
}

#[test]
fn every_truncation_of_a_small_fixture_is_handled() {
    // No prefix of a board may panic: each reads or is refused with a message.
    let bytes = std_doc(&[PART, "TRACK~1~1~GND~4000 3000 4010 3000~g9~0"]);
    for end in 0..bytes.len() {
        match extract(&bytes[..end]) {
            Ok(_) => {}
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }
    let pro = fixture(NANO);
    // Sample prefixes across the Pro fixture (every byte would be slow).
    for end in (0..pro.len()).step_by(997) {
        match extract(&pro[..end]) {
            Ok(_) => {}
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }
}

// ── archives ─────────────────────────────────────────────────────────────────

fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in files {
        w.start_file(*name, opts).expect("start file");
        w.write_all(data).expect("write");
    }
    w.finish().expect("finish").into_inner()
}

fn pro_project(extra: &[(&str, &[u8])]) -> Vec<u8> {
    let nano = fixture(NANO);
    let mut files: Vec<(&str, &[u8])> = vec![
        ("project.json", br#"{"schematics":{},"pcbs":{}}"#),
        ("PCB/Nano.epcb", &nano),
    ];
    files.extend_from_slice(extra);
    zip_of(&files)
}

#[test]
fn a_pro_project_archive_reads_its_board() {
    let zip = pro_project(&[]);
    assert!(looks_like_easyeda_archive(&zip));
    assert!(EasyedaArchiveReader.detects(&zip, None));
    assert!(!EasyedaReader.detects(&zip, None));
    let reg = Registry::builtin();
    assert_eq!(
        reg.detect(&zip, None).map(BoardReader::name),
        Some("easyeda-archive")
    );
    let got = extract_archive(&zip).expect("reads");
    assert_eq!(got.stats.edition, Edition::Pro);
    assert_eq!(got.board.components.len(), 23);
    assert!(got.stats.other_boards.is_empty());
    let via_board = ExtractedBoard::from_easyeda_archive(&zip).expect("reads");
    assert_eq!(via_board.components.len(), 23);
}

#[test]
fn a_second_board_in_the_archive_is_named_not_merged() {
    let extra = fixture(NANO);
    let zip = pro_project(&[("PCB/Second.epcb", &extra)]);
    let got = extract_archive(&zip).expect("reads");
    assert_eq!(got.board.components.len(), 23, "boards are not merged");
    assert_eq!(got.stats.other_boards, ["PCB/Second.epcb"]);
    says(
        &got.stats.notes().join("\n"),
        &["1 other board", "Second.epcb"],
    );
}

#[test]
fn a_zip_of_standard_json_boards_reads_the_first() {
    let a = fixture(NOFILTER);
    let b = fixture(GP1212);
    let zip = zip_of(&[
        ("a/one.json", &a),
        ("a/two.json", &b),
        ("readme.txt", b"hi"),
    ]);
    assert!(looks_like_easyeda_archive(&zip));
    let got = extract_archive(&zip).expect("reads");
    assert_eq!(got.stats.edition, Edition::Standard);
    assert_eq!(got.board.components.len(), 3);
    assert_eq!(got.stats.other_boards, ["a/two.json"]);
}

#[test]
fn a_zip_that_holds_no_easyeda_board_is_refused_and_not_claimed() {
    let gerbers = zip_of(&[("board.GTL", b"G04 top*\nM02*\n"), ("board.TXT", b"M48\n")]);
    assert!(!looks_like_easyeda_archive(&gerbers));
    assert!(!EasyedaArchiveReader.detects(&gerbers, None));
    let msg = extract_archive(&gerbers).expect_err("no board").to_string();
    says(&msg, &["no EasyEDA PCB", "Export"]);

    let other_json = zip_of(&[("data.json", br#"{"a":1}"#)]);
    assert!(!looks_like_easyeda_archive(&other_json));

    let empty = zip_of(&[]);
    assert!(!looks_like_easyeda_archive(&empty));
    assert!(extract_archive(&empty).is_err());
}

#[test]
fn a_damaged_or_hostile_archive_is_refused_without_panicking() {
    let zip = pro_project(&[]);
    // Truncated in the middle and with the central directory gone.
    for end in [0, 10, zip.len() / 2, zip.len() - 30] {
        let msg = extract_archive(&zip[..end])
            .expect_err("damaged")
            .to_string();
        assert!(!msg.is_empty());
    }
    // An epcb member that is not a Pro board.
    let bad = zip_of(&[
        ("project.json", b"{}"),
        ("PCB/x.epcb", b"[\"DOCTYPE\",\"SCH\",\"1\"]\n"),
    ]);
    let msg = extract_archive(&bad).expect_err("not a board").to_string();
    says(&msg, &["SCH", "not a PCB"]);
    // A hostile project.json is ignored for the title, not trusted.
    let nested = "[".repeat(100_000);
    let hostile = zip_of(&[
        ("project.json", nested.as_bytes()),
        ("PCB/n.epcb", &fixture(NANO)),
    ]);
    let got = extract_archive(&hostile).expect("board still reads");
    assert_eq!(got.board.components.len(), 23);
}

#[test]
fn an_archive_member_that_inflates_without_bound_is_refused() {
    // A deflated member of zeros far larger than the inflation budget.
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    w.start_file("project.json", opts).unwrap();
    w.write_all(b"{}").unwrap();
    w.start_file("PCB/bomb.epcb", opts).unwrap();
    let chunk = vec![b' '; 1 << 20];
    for _ in 0..600 {
        w.write_all(&chunk).unwrap();
    }
    let zip = w.finish().unwrap().into_inner();
    assert!(zip.len() < 5_000_000, "the bomb itself must be small");
    let msg = extract_archive(&zip).expect_err("bomb").to_string();
    says(&msg, &["EasyEDA"]);
}

#[test]
fn the_ladder_of_public_apis_agree() {
    let bytes = fixture(NANO);
    let direct = ExtractedBoard::from_easyeda(&bytes).expect("reads");
    let reg = Registry::builtin().read(&bytes, None).expect("reads");
    assert_eq!(pin_map(&direct), pin_map(&reg));
}

// ── values from the project's device library ─────────────────────────────────

/// The `Device` attribute on a Pro part is a UUID into the project's device
/// records. The `devices` layout below is INFERRED from public project
/// exports (no vendor specification may be used), so it is covered by these
/// in-test archives only.
fn pro_project_with_devices(project_json: &str) -> Vec<u8> {
    let nano = fixture(NANO);
    zip_of(&[
        ("project.json", project_json.as_bytes()),
        ("PCB/Nano.epcb", &nano),
    ])
}

fn component<'a>(board: &'a ExtractedBoard, reference: &str) -> &'a hauksbee_extract::Component {
    board
        .components
        .iter()
        .find(|c| c.reference == reference)
        .unwrap_or_else(|| panic!("{reference} present"))
}

#[test]
fn a_pro_project_resolves_values_through_its_device_records() {
    use hauksbee_extract::altium::VALUE_UNRESOLVED_KEY;
    let zip = pro_project_with_devices(
        r#"{"devices":{
            "3627182c3ed84f81908efec03cdc4632":{"title":"CAP_0603","attributes":{
                "Value":"10uF","Manufacturer Part":"CL10A106MA8NRNC",
                "Manufacturer":"SAMSUNG","Supplier Part":"C96446"}},
            "74a5788e70784dcba75fcacc5223fa3f":{"title":"CAP_0603","attributes":{
                "Manufacturer Part":"CC0603KRX7R9BB104"}}
        }}"#,
    );
    let got = extract_archive(&zip).expect("reads");
    let c1 = component(&got.board, "C1");
    assert_eq!(c1.value, "10uF");
    assert!(
        !c1.properties.iter().any(|(k, _)| k == VALUE_UNRESOLVED_KEY),
        "a resolved value carries no unresolved marker"
    );
    assert!(c1
        .properties
        .iter()
        .any(|(k, v)| k == "Manufacturer Part" && v == "CL10A106MA8NRNC"));
    assert!(!c1.value_not_carried());
    // A device with a part number but no `Value` falls back to the part number.
    assert_eq!(component(&got.board, "C3").value, "CC0603KRX7R9BB104");
    // Devices the project does not describe stay honestly unresolved.
    let u1 = component(&got.board, "U1");
    assert!(u1.value.is_empty() && u1.value_not_carried());
    assert!(got.stats.value_unresolved.len() < 23);
    assert!(!got.stats.value_unresolved.iter().any(|r| r == "C1"));
}

#[test]
fn a_project_without_device_records_leaves_values_unresolved_not_invented() {
    let zip = pro_project_with_devices(r#"{"schematics":{},"pcbs":{}}"#);
    let got = extract_archive(&zip).expect("reads");
    assert!(got.board.components.iter().all(|c| c.value.is_empty()));
    assert!(got
        .board
        .components
        .iter()
        .all(hauksbee_extract::Component::value_not_carried));
}
