//! Real BOM and placement files, read.
//!
//! A reader built against two invented examples is worthless; one built against
//! forty real ones is evidence. Every fixture here is a file a real project
//! shipped, and the assertions are about what that specific file contains, so a
//! regression in the column detection breaks a test rather than quietly changing
//! a number.
//!
//! ## Fixture provenance
//!
//! Each file is unmodified, byte for byte, from a public project under a
//! permissive licence, fetched at the commit named. The survey they were drawn
//! from is recorded in `docs/ingest/BOM.md`.
//!
//! * `bom/kicad_grouped.csv`: ataradov/usb-sniffer at 0d26e7e6
//!   (`bin/usb-sniffer-bom.csv`), BSD-3-Clause. A KiCad 7 grouped export with a
//!   six-line preamble and an all-empty `DNP` column.
//! * `bom/kicad_grouped_mpn.csv`: tinyvision-ai-inc/UPduino-v3.0 at e2b944b4
//!   (`Board/v2.0/UPduino_v3.csv`), MIT. Carries an `MPN` column filled on some
//!   rows and empty on others, which is the ordinary case.
//! * `bom/kicad_ungrouped.csv`: Gasman2014/KC2PK at 6c31c3b5 (`Demo2.csv`),
//!   MIT. Space-separated reference lists.
//! * `bom/kicad_ungrouped_mpn.csv`: osrf/ovc at 88c1077f
//!   (`ovc4/hardware/carrier/fab/bom.csv`), Apache-2.0. `MFN`/`MPN` columns
//!   beside a `D1N`/`D1PN` distributor pair.
//! * `bom/altium.csv`: marshallh/gbpp at 2aa972bf (`BOM.csv`), MIT.
//! * `bom/eagle_partlist.txt`: stephan192/hoermann_door at 2a46af4d
//!   (`board/RS485Interface_partlist.txt`), MIT. Fixed-width under an Eagle
//!   banner.
//! * `bom/lcsc.csv`: xyphro/UsbGpib at 4e7a46d6 (`HW/REV2/BOM-xyp-gpib.csv`),
//!   MIT.
//! * `bom/lcsc_parttype.csv`: Ottercast/OtterCastAmp at 0b5f7f9a
//!   (`gerber_v1.0/bom.csv`), MIT. Two LCSC columns, one of which is not a part
//!   number.
//! * `bom/jlcpcb.csv`: tinyvision-ai-inc/pico2-ice at 45e137e6
//!   (`Board/Rev0/jlcpcb/pico2-ice_BoM.csv`), MIT.
//! * `bom/digikey.csv`: dgoodlad/esp8266-mitsubishi-aircon at 95f9b963
//!   (`docs/bom-digikey.csv`), MIT. `Customer Reference` holds the designators,
//!   which is a guess, so this file is a refusal.
//! * `bom/digikey_no_refdes.csv`: attoparsec/TruthTable at 6ae2c7ed
//!   (`Bom.csv`), CC0-1.0. A cart export with both reference columns empty on
//!   every row.
//! * `bom/spreadsheet.csv`: hutscape/pine at 129ae8f8
//!   (`_data/bill_of_materials.csv`), MIT. Hand-maintained, `MPN` and `DNP`
//!   columns, reference lists with a trailing separator.
//! * `placement/kicad5.pos`: ReclaimerLabs/USB-PD-Breakout at fe55b617
//!   (`electrical/gerber/USB-PD-Breakout-all.pos`), MIT.
//! * `placement/kicad_pos.csv`: StuckAtPrototype/AirCube at f9ec414d
//!   (`kicad/AirCube-top-pos.csv`), Apache-2.0.
//! * `placement/altium_pnp.csv`: cifertech/nRFBox at 5616b306 (`PCB/CPL.csv`),
//!   MIT. Thirteen banner lines, every row padded with trailing commas.
//! * `placement/altium_pnp.txt`: amirmat98/Capacitive-Level-Meter-Sensor at
//!   bc8f491e (`.../Pick Place/Pick Place for PCB4.txt`), MIT. Fixed-width.
//! * `placement/generic_cpl.csv`: xyphro/UsbGpib at 4e7a46d6
//!   (`HW/REV2/CPL-xyp-gpib.csv`), MIT. The `Mid X`/`Mid Y` shape JLCPCB and
//!   PCBWay accept.
//! * `placement/watchy.pos` and `placement/watchy-pos.csv`: exported from this
//!   repo's own `crates/hauksbee-ci/examples/boards/watchy.kicad_pcb` with
//!   `kicad-cli pcb export pos --format {ascii,csv} --units mm` (KiCad 9.0.3).
//!   These are the pair with ground truth: the board they describe is in the
//!   repo, so the placement reading can be checked against it rather than
//!   against itself.

use std::path::{Path, PathBuf};

use hauksbee_extract::bom::{
    Bom, BomDetection, BomDialect, BomError, ColumnOverrides, ColumnRole, MappingConfidence,
};
use hauksbee_extract::placement::{
    PlacementDetection, PlacementDialect, PlacementError, PlacementFile, Side,
};
use hauksbee_extract::ExtractedBoard;

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

fn read_bom(rel: &str) -> Bom {
    Bom::read(&fixture(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn read_placement(rel: &str) -> PlacementFile {
    PlacementFile::read(&fixture(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

// ── Dialect detection over every real fixture ───────────────────────────────

#[test]
fn every_bom_fixture_reads_as_the_dialect_it_is() {
    let cases: &[(&str, BomDialect)] = &[
        ("bom/kicad_grouped.csv", BomDialect::KicadGrouped),
        ("bom/kicad_grouped_mpn.csv", BomDialect::KicadGrouped),
        ("bom/kicad_ungrouped.csv", BomDialect::KicadUngrouped),
        ("bom/kicad_ungrouped_mpn.csv", BomDialect::KicadUngrouped),
        ("bom/altium.csv", BomDialect::Altium),
        ("bom/eagle_partlist.txt", BomDialect::EaglePartlist),
        ("bom/lcsc.csv", BomDialect::Lcsc),
        ("bom/lcsc_parttype.csv", BomDialect::Lcsc),
        ("bom/jlcpcb.csv", BomDialect::Jlcpcb),
        ("bom/spreadsheet.csv", BomDialect::Spreadsheet),
    ];
    for (rel, want) in cases {
        let bom = read_bom(rel);
        assert_eq!(bom.dialect, *want, "{rel}");
        assert!(!bom.rows.is_empty(), "{rel} produced no rows");
        // Every row that survived mapping has at least one designator, and the
        // mapping that produced it is recorded rather than implicit.
        assert!(bom.rows.iter().all(|r| !r.references.is_empty()), "{rel}");
        assert!(
            bom.provenance
                .column_map
                .used
                .iter()
                .any(|a| a.role == ColumnRole::Reference),
            "{rel} recorded no reference column"
        );
    }
}

#[test]
fn every_placement_fixture_reads_as_the_dialect_it_is() {
    let cases: &[(&str, PlacementDialect)] = &[
        ("placement/kicad5.pos", PlacementDialect::KicadPosAscii),
        ("placement/watchy.pos", PlacementDialect::KicadPosAscii),
        ("placement/kicad_pos.csv", PlacementDialect::KicadPosCsv),
        ("placement/watchy-pos.csv", PlacementDialect::KicadPosCsv),
        (
            "placement/altium_pnp.csv",
            PlacementDialect::AltiumPickPlace,
        ),
        (
            "placement/altium_pnp.txt",
            PlacementDialect::AltiumPickPlace,
        ),
        ("placement/generic_cpl.csv", PlacementDialect::GenericCpl),
    ];
    for (rel, want) in cases {
        let file = read_placement(rel);
        assert_eq!(file.dialect, *want, "{rel}");
        assert!(!file.placements.is_empty(), "{rel} produced no placements");
        assert!(
            file.placements
                .iter()
                .all(|p| p.x_mm.is_finite() && p.y_mm.is_finite()),
            "{rel} produced a non-finite coordinate"
        );
    }
}

// ── What each real file actually contains ───────────────────────────────────

#[test]
fn a_kicad_grouped_bom_reads_its_preamble_then_its_groups() {
    let bom = read_bom("bom/kicad_grouped.csv");
    // 24 capacitors in one row is the point of a grouped BOM.
    let row = bom.row_for("C29").expect("C29 is in the file");
    assert_eq!(row.value, "100nF");
    assert_eq!(row.references.len(), 24);
    assert_eq!(row.quantity, Some(24));
    // The DNP column exists and is empty on every row, which is "says nothing",
    // not "do not populate".
    assert!(bom.rows.iter().all(|r| r.populate.is_none()));
    assert_eq!(bom.provenance.kind, "kicad_grouped_bom");
    assert_eq!(bom.provenance.sha256.len(), 64);
}

#[test]
fn a_bom_mpn_column_is_read_where_it_is_filled_and_ignored_where_it_is_not() {
    let bom = read_bom("bom/kicad_grouped_mpn.csv");
    let filled = bom.row_for("C2").expect("C2 is in the file");
    assert_eq!(filled.mpn.as_deref(), Some("C1005X5R0J104K050BA"));
    let empty = bom.row_for("C1").expect("C1 is in the file");
    assert_eq!(
        empty.mpn, None,
        "an empty MPN cell must not reach the binder"
    );
    // `~` is KiCad's way of writing nothing and must never look like a value.
    assert!(bom.rows.iter().all(|r| r.mpn.as_deref() != Some("~")));
}

#[test]
fn space_separated_reference_lists_read_as_separate_designators() {
    let bom = read_bom("bom/kicad_ungrouped.csv");
    let row = bom.row_for("C16").expect("C16 is in the file");
    assert_eq!(
        row.references,
        vec!["C3", "C5", "C16", "C17", "C18", "C20", "C21", "C22"]
    );
    assert_eq!(row.value, "100nF");
}

#[test]
fn an_mpn_beside_a_distributor_code_maps_to_the_right_role() {
    let bom = read_bom("bom/kicad_ungrouped_mpn.csv");
    let row = bom.row_for("C23").expect("C23 is in the file");
    assert_eq!(row.mpn.as_deref(), Some("C3216X5R1A107M160AC"));
    assert_eq!(row.manufacturer.as_deref(), Some("TDK"));
    // `D1PN` is a Digi-Key order code. It is not identity, and the read says so.
    assert!(bom
        .provenance
        .ignored
        .iter()
        .any(|i| i.what.contains("D1PN") || i.what.contains("distributor")));
}

#[test]
fn an_altium_bom_reads_comment_as_the_value() {
    let bom = read_bom("bom/altium.csv");
    let row = bom.row_for("C1").expect("C1 is in the file");
    assert_eq!(row.value, "10uF");
    assert_eq!(row.references, vec!["C1", "C3", "C4", "C6"]);
    assert_eq!(row.quantity, Some(4));
    // `Comment` is only the value column because the dialect says so.
    let assignment = bom
        .provenance
        .column_map
        .used
        .iter()
        .find(|a| a.role == ColumnRole::Value)
        .expect("a value column");
    assert_eq!(assignment.header, "Comment");
    assert_eq!(assignment.confidence, MappingConfidence::Likely);
}

#[test]
fn an_eagle_partlist_is_sliced_by_its_header_offsets() {
    let bom = read_bom("bom/eagle_partlist.txt");
    assert_eq!(bom.row_for("C1").map(|r| r.value.as_str()), Some("10u"));
    assert_eq!(bom.row_for("C3").map(|r| r.value.as_str()), Some("33p"));
    let row = bom.row_for("C1").expect("C1 is in the file");
    assert_eq!(row.footprint.as_deref(), Some("C-EUC0805"));
    // `Part` and `Device` are certain in Eagle and a guess anywhere else.
    let refcol = bom
        .provenance
        .column_map
        .used
        .iter()
        .find(|a| a.role == ColumnRole::Reference)
        .expect("a reference column");
    assert_eq!(refcol.header, "Part");
    assert_eq!(refcol.confidence, MappingConfidence::Certain);
}

#[test]
fn an_lcsc_code_is_recorded_and_never_treated_as_identity() {
    let bom = read_bom("bom/lcsc.csv");
    let row = bom.row_for("U1").expect("U1 is in the file");
    assert_eq!(row.value, "ATmega32U4-MU");
    assert_eq!(row.distributor_part.as_deref(), Some("C112161"));
    assert_eq!(
        row.mpn, None,
        "an LCSC code is not a manufacturer part number"
    );
    assert!(bom
        .provenance
        .ignored
        .iter()
        .any(|i| i.what == "distributor order codes"));
}

#[test]
fn a_hand_maintained_spreadsheet_reads_its_mpn_and_its_dnp_column() {
    let bom = read_bom("bom/spreadsheet.csv");
    let row = bom.row_for("C1").expect("C1 is in the file");
    // The cell is `"C3, C2, C1, "`: a trailing separator is not a designator.
    assert_eq!(row.references, vec!["C3", "C2", "C1"]);
    assert_eq!(row.mpn.as_deref(), Some("GCM21BR72A104KA37L"));
    assert_eq!(row.manufacturer.as_deref(), Some("Murata Electronics"));
}

#[test]
fn the_mapping_used_is_recorded_in_the_shape_the_docs_quote() {
    // Half the non-interactive contract is that a run which proceeds on a
    // detected mapping SAYS which mapping it used. This asserts the exact block,
    // because `docs/ingest/BOM.md` quotes it and a paraphrased example in a doc
    // is a lie with a delay on it.
    let bom = read_bom("bom/kicad_grouped_mpn.csv");
    let lines = bom.provenance.lines();
    let rendered: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(
        &rendered[1..],
        &[
            "  reference <- \"Reference(s)\" (certain)",
            "  value <- \"Value\" (certain)",
            "  mpn <- \"MPN\" (certain)",
            "  quantity <- \"Qty\" (certain)",
            "  footprint <- \"Footprint\" (certain)",
            "  contributed: part identity: 60 reference designators over 22 rows, 1 of them \
             carrying a manufacturer part number",
            "  ignored:     column \"Item\": no analysis reads it",
            "  ignored:     column \"Datasheet\": no analysis reads it",
        ]
    );
    assert!(
        rendered[0].ends_with("(kicad_grouped_bom, sha256 5296073e)"),
        "{}",
        rendered[0]
    );
}

#[test]
fn rendering_public_provenance_never_panics_on_a_short_digest() {
    let mut provenance = read_bom("bom/kicad_grouped.csv").provenance;
    provenance.sha256 = "bad".into();
    let lines = provenance.lines();
    assert!(lines[0].contains("sha256 invalid:bad"), "{}", lines[0]);
}

// ── Refusals, on both their exit code and their message ─────────────────────

#[test]
fn a_distributor_cart_export_with_no_designators_refuses_and_says_why() {
    let err = Bom::read(&fixture("bom/digikey_no_refdes.csv")).expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    // The message has to name the column, the scale of the problem, and the fix.
    assert!(msg.contains("Reference Designator"), "{msg}");
    assert!(msg.contains("purchase list"), "{msg}");
    assert!(
        msg.contains("Re-export it with the reference-designator field filled in"),
        "{msg}"
    );
    assert!(matches!(err, BomError::EmptyReferenceColumn { .. }));
}

#[test]
fn a_reference_column_that_is_only_a_guess_refuses_with_the_flag_that_fixes_it() {
    let err = Bom::read(&fixture("bom/digikey.csv")).expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("Customer Reference"), "{msg}");
    assert!(
        msg.contains("--bom-column reference=Customer Reference"),
        "{msg}"
    );
    // And the flag it suggests must actually work.
    let mut ov = ColumnOverrides::new();
    ov.set(ColumnRole::Reference, "Customer Reference");
    let bom = Bom::read_with(&fixture("bom/digikey.csv"), &ov).expect("override resolves it");
    assert_eq!(bom.dialect, BomDialect::DigiKey);
    assert_eq!(
        bom.row_for("U1").map(|r| r.mpn.as_deref()),
        Some(Some("TXB0104DR"))
    );
}

#[test]
fn a_file_that_is_not_a_bom_refuses_rather_than_inventing_columns() {
    let text = "created_utc,score,domain,id,title\n1274985703.0,740,self.gnu,c8rrk,RMS\n";
    let err = Bom::from_text(text, "reddit.csv", &ColumnOverrides::new()).expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(
        msg.contains("does not read as a bill of materials"),
        "{msg}"
    );
    assert!(msg.contains("--bom-column reference="), "{msg}");
}

#[test]
fn two_columns_equally_entitled_to_one_role_refuse_rather_than_pick() {
    // `MPN` and `Manufacturer Part Number` are both unambiguous names for the
    // part number, and nothing in the file says which is authoritative. Picking
    // the first would bind whichever the exporter happened to write first.
    let text = "Designator,Value,MPN,Manufacturer Part Number\nR1,10k,A,B\n";
    let err = Bom::from_text(text, "twompn.csv", &ColumnOverrides::new()).expect_err("refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("two columns that could be the mpn"), "{msg}");
    assert!(msg.contains("--bom-column mpn="), "{msg}");
    // Naming one settles it, and the other becomes an ignored column.
    let mut ov = ColumnOverrides::new();
    ov.set(ColumnRole::Mpn, "MPN");
    let bom = Bom::from_text(text, "twompn.csv", &ov).expect("the override settles it");
    assert_eq!(
        bom.row_for("R1").and_then(|r| r.mpn.clone()),
        None,
        "\"A\" is not a part number"
    );
    assert!(bom
        .provenance
        .column_map
        .ignored_headers
        .contains(&"Manufacturer Part Number".to_string()));
}

#[test]
fn side_split_designator_columns_are_combined_without_losing_either_side() {
    let text = "topDesignator,bottomDesignator,Value,MPN\nR1,C1,10k,RC0402FR-0710KL\nR2,,47k,RC0402FR-0747KL\n,C2,100nF,CC0402KRX7R9BB104\n";
    let bom = Bom::from_text(text, "two-sides.csv", &ColumnOverrides::new())
        .expect("a side-split assembly BOM is one coherent reference source");

    assert_eq!(bom.references(), vec!["C1", "C2", "R1", "R2"]);
    assert_eq!(bom.row_for("R1").map(|r| r.value.as_str()), Some("10k"));
    assert_eq!(bom.row_for("C1").map(|r| r.value.as_str()), Some("10k"));
    assert_eq!(bom.row_for("C2").map(|r| r.value.as_str()), Some("100nF"));
}

#[test]
fn identically_named_identity_columns_refuse_when_their_cells_disagree() {
    for (header, row) in [
        ("MPN", "R1,10k,TPS62130,TPS62135"),
        ("Value", "R1,10k,10k,47k"),
        ("DNP", "R1,10k,yes,no"),
    ] {
        let text = format!("Designator,Description,{header},{header}\n{row}\n");
        let err = Bom::from_text(&text, "duplicate.csv", &ColumnOverrides::new())
            .expect_err("conflicting duplicate identity columns must not first-win");
        assert!(matches!(err, BomError::AmbiguousColumn { .. }), "{err:?}");
        assert!(err.to_string().contains("two columns"), "{err}");
    }
}

#[test]
fn a_multiline_quoted_csv_cell_refuses_instead_of_truncating_the_row() {
    let text = "Designator,Description,Value,MPN\nR1,\"precision resistor\nAEC-Q200 qualified\",10k,RC0402FR-0710KL\n";
    let err = Bom::from_text(text, "multiline.csv", &ColumnOverrides::new())
        .expect_err("a physical-line parser must not accept half of a logical CSV row");
    let message = err.to_string();
    assert!(message.contains("multiline quoted field"), "{message}");
    assert!(message.contains("line 2"), "{message}");
    assert!(message.contains("re-export"), "{message}");
}

#[test]
fn windows_1252_bytes_decode_as_windows_1252_not_latin1_controls() {
    let mut bytes = b"Designator,Value,MPN\nU1,regulator,ABC".to_vec();
    bytes.push(0x96); // Windows-1252 EN DASH; Latin-1 would produce U+0096.
    bytes.extend_from_slice(b"123\n");

    let bom = Bom::from_bytes(&bytes, "cp1252.csv", &ColumnOverrides::new())
        .expect("ordinary spreadsheet code-page output reads");
    assert_eq!(
        bom.row_for("U1").and_then(|row| row.mpn.as_deref()),
        Some("ABC–123")
    );
}

#[test]
fn an_override_naming_a_column_that_is_not_there_says_which_columns_are() {
    let mut ov = ColumnOverrides::new();
    ov.set(ColumnRole::Mpn, "PartNo");
    let err = Bom::read_with(&fixture("bom/lcsc.csv"), &ov).expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("has no column called \"PartNo\""), "{msg}");
    assert!(msg.contains("\"Designator\""), "{msg}");
}

#[test]
fn an_empty_file_refuses_with_the_two_reasons_it_is_usually_empty() {
    let err = Bom::from_text("   \n\n", "bom.csv", &ColumnOverrides::new()).expect_err("refuse");
    assert_eq!(err.exit_code(), 3);
    assert!(err.to_string().contains("git lfs pull"), "{err}");
}

#[test]
fn a_bad_column_flag_is_rejected_before_any_file_is_read() {
    assert!(ColumnOverrides::parse_pair("Designator").is_err());
    let err = ColumnOverrides::parse_pair("refdes=").unwrap_err();
    assert!(err.contains("no column"), "{err}");
    let err = ColumnOverrides::parse_pair("colour=Blue").unwrap_err();
    assert!(err.contains("mpn"), "{err}");
    assert_eq!(
        ColumnOverrides::parse_pair("reference=Designator").unwrap(),
        (ColumnRole::Reference, "Designator".to_string())
    );
}

#[test]
fn artifact_probing_distinguishes_not_mine_from_an_actionable_candidate() {
    let ambiguous = b"Designator,Value,MPN,Manufacturer Part Number\nR1,10k,ABC123,XYZ123\n";
    assert!(matches!(
        Bom::probe(ambiguous, "ambiguous.csv"),
        BomDetection::Candidate(BomError::AmbiguousColumn { .. })
    ));
    assert!(matches!(
        Bom::probe(b"name,email\nAda,ada@example.test\n", "people.csv"),
        BomDetection::NotRecognized
    ));

    let broken = b"Designator,Mid X,Mid Y,Layer\nR1,left,nowhere,Top\n";
    assert!(matches!(
        PlacementFile::probe(broken, "broken-cpl.csv"),
        PlacementDetection::Candidate(PlacementError::UnreadableCoordinates { .. })
    ));
}

// ── The BOM disagreeing with itself ─────────────────────────────────────────

#[test]
fn a_quantity_that_disagrees_with_its_own_reference_list_is_reported() {
    let text = "Designator,Value,Quantity\n\"R1, R2, R3\",10k,4\n\"C1, C2\",100nF,2\n";
    let bom = Bom::from_text(text, "bom.csv", &ColumnOverrides::new()).unwrap();
    let bad = bom.quantity_disagreements();
    assert_eq!(bad.len(), 1);
    let (row, stated, enumerated) = bad[0];
    assert_eq!((row.value.as_str(), stated, enumerated), ("10k", 4, 3));
}

#[test]
fn a_reference_appearing_in_two_bom_rows_refuses_instead_of_first_winning() {
    let text = "Designator,Value,MPN\nU1,regulator,TPS62130\nU1,regulator,TPS62135\n";
    let err = Bom::from_text(text, "duplicate-row.csv", &ColumnOverrides::new())
        .expect_err("one designator cannot acquire two identities");
    let message = err.to_string();
    assert!(message.contains("U1"), "{message}");
    assert!(message.contains("lines 2 and 3"), "{message}");
}

// ── Placement files ─────────────────────────────────────────────────────────

#[test]
fn a_kicad_position_file_agrees_with_the_board_it_was_exported_from() {
    let board_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/watchy.kicad_pcb");
    let board = ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&board_path).unwrap())
        .expect("the watchy example board reads");

    for rel in ["placement/watchy.pos", "placement/watchy-pos.csv"] {
        let file = read_placement(rel);
        let check = file.cross_check(&board);
        // The file WAS exported from this board, so nothing may be missing on
        // either side and every matched position must agree.
        assert!(check.only_in_placement.is_empty(), "{rel}: {check:?}");
        assert!(check.position_disagreements.is_empty(), "{rel}: {check:?}");
        assert!(check.side_disagreements.is_empty(), "{rel}: {check:?}");
        assert!(check.matched >= 70, "{rel}: only {} matched", check.matched);
        // Parts on the board that the position file omits are ordinary: KiCad
        // leaves out the artwork placeholders and the test points, which are
        // excluded from position files. That is a note, not a revision mismatch.
        assert_eq!(
            check.only_on_board,
            vec!["TP1", "TP2", "TP3", "TP4", "TP5", "TP6", "TP7"],
            "{rel}"
        );
        assert!(!check.is_different_board(), "{rel}");
        assert!(check.lines().is_empty(), "{rel}: {:?}", check.lines());
    }
}

#[test]
fn a_kicad_position_file_reads_its_values_and_its_terminator() {
    let file = read_placement("placement/kicad5.pos");
    let c1 = file.get("C1").expect("C1 is in the file");
    assert_eq!(c1.value, "10uF");
    assert_eq!(c1.package, "C_1206");
    assert!((c1.x_mm - 25.1460).abs() < 1e-9, "{}", c1.x_mm);
    // The `## End` terminator is a comment, not a placement.
    assert!(file.placements.iter().all(|p| p.reference != "##"));
    assert_eq!(file.placements.len(), 21);
}

#[test]
fn an_altium_pick_and_place_skips_its_banner_and_strips_its_unit_suffixes() {
    let csv = read_placement("placement/altium_pnp.csv");
    let c15 = csv.get("C15").expect("C15 is in the file");
    assert!((c15.x_mm - 100.457).abs() < 1e-6, "{}", c15.x_mm);
    assert_eq!(c15.side, Side::Bottom);
    // The logo row has no designator and is recorded as ignored, not dropped
    // silently.
    assert!(csv
        .provenance
        .ignored
        .iter()
        .any(|i| i.what.contains("row") || i.what.contains("designator")));

    // The fixed-width form of the same export, whose coordinates carry an `mm`
    // suffix inside the column.
    let txt = read_placement("placement/altium_pnp.txt");
    assert!(txt.placements.iter().all(|p| p.x_mm.abs() < 1e4));
    assert!(txt.placements.iter().any(|p| p.side == Side::Top));
}

#[test]
fn the_generic_cpl_shape_jlcpcb_accepts_reads_as_placement() {
    let file = read_placement("placement/generic_cpl.csv");
    assert_eq!(file.dialect, PlacementDialect::GenericCpl);
    let c1 = file.get("C1").expect("C1 is in the file");
    assert_eq!(c1.value, "4.7uF");
    assert!((c1.rotation_deg - 90.0).abs() < 1e-9);
}

#[test]
fn a_placement_file_supplies_identity_only_as_a_value_never_as_a_part_number() {
    let file = read_placement("placement/generic_cpl.csv");
    let hints = file.identity_hints();
    assert!(!hints.is_empty());
    assert!(
        hints.iter().all(|h| h.mpn.is_none()),
        "a placement file's Val column is a value string, not a part number"
    );
    assert!(hints.iter().any(|h| h.value.as_deref() == Some("4.7uF")));
}

#[test]
fn a_placement_file_that_is_not_one_refuses() {
    let err =
        PlacementFile::from_text("Designator,Value\nR1,10k\n", "bom.csv").expect_err("refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("no X and Y"), "{msg}");
}

#[test]
fn a_reference_appearing_twice_in_a_placement_file_refuses() {
    let text = "Designator,Mid X,Mid Y,Layer,Rotation\nU1,1,2,Top,0\nU1,3,4,Bottom,180\n";
    let err = PlacementFile::from_text(text, "duplicate-cpl.csv")
        .expect_err("a part cannot be placed twice without an explicit panel model");
    let message = err.to_string();
    assert!(message.contains("U1"), "{message}");
    assert!(message.contains("lines 2 and 3"), "{message}");
}
