//! Ingest robustness: one test per failure class found by running a corpus of
//! ~3400 real-world board files (1139 KiCad layouts spanning format versions
//! 20160815 through 20260624, 238 Altium/Protel, 335 Eagle, 295 IPC-D-356, 60
//! whole gerber fab folders, plus loose fab films) through every read surface.
//!
//! The bar these hold: a file either ingests correctly or is refused loudly with
//! a message that names what is wrong and what to do about it. A crash, a hang,
//! and a confident wrong answer are all failures; an honest refusal is a pass.
//!
//! Fixtures live in `testdata/ingest-robustness/` and are the SMALLEST file that
//! still reproduces its class.

use std::path::{Path, PathBuf};

use hauksbee_extract::ExtractedBoard;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/ingest-robustness")
        .join(name)
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixture(name))
        .unwrap_or_else(|e| panic!("fixture {name} must exist: {e}"))
}

/// Every refusal has to be actionable, which is testable: it must name the
/// problem and say what to do next. Checks the message contains each phrase.
fn message_says(msg: &str, phrases: &[&str]) {
    for p in phrases {
        assert!(
            msg.contains(p),
            "refusal message must mention {p:?}; got: {msg}"
        );
    }
}

#[test]
fn self_referencing_sheet_does_not_overflow_the_stack() {
    // Found in the wild as a copy-pasted sheet keeping the original's
    // `Sheetfile`. Following it recursed until the thread aborted the whole
    // process with "fatal runtime error: stack overflow": on `hauksbee serve`
    // that is a denial of service, not a bad parse. It must terminate.
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("sheet_cycle.kicad_sch"))
        .expect("a sheet cycle must return, not abort");
    assert!(board.components.is_empty(), "the fixture has no symbols");
}

#[test]
fn mutually_referencing_sheets_do_not_overflow_the_stack() {
    // The two-file form of the same cycle: a -> b -> a.
    for entry in ["sheet_cycle_a.kicad_sch", "sheet_cycle_b.kicad_sch"] {
        ExtractedBoard::from_kicad_schematic_path(&fixture(entry))
            .unwrap_or_else(|e| panic!("{entry} must return, not abort: {e}"));
    }
}

#[test]
fn non_finite_coordinate_is_refused_not_silently_passed() {
    // The one corruption shape that produced a CONFIDENT WRONG ANSWER: a pad at
    // NaN compares false against every distance, so the clearance check walked
    // the board and reported "no shorts or clearance violations". A green
    // verdict on geometry that does not exist is worse than any error.
    let err = ExtractedBoard::from_kicad_pcb(&read("non_finite_coordinate.kicad_pcb"))
        .expect_err("a NaN coordinate must be refused");
    message_says(
        &err.to_string(),
        &["geometry is corrupt", "nan", "Re-save the board"],
    );
}

#[test]
fn exponent_overflowing_to_infinity_is_refused() {
    // `1e400` parses as `inf`, which is how a mangled unit conversion arrives.
    let err = ExtractedBoard::from_kicad_pcb(&read("exponent_overflow_coordinate.kicad_pcb"))
        .expect_err("an infinite coordinate must be refused");
    message_says(&err.to_string(), &["geometry is corrupt"]);
}

#[test]
fn eagle_non_finite_coordinate_is_refused() {
    // Same hazard through the Eagle reader, whose `f64` parse also accepts
    // "NaN" and used to fall back to placing the element at the origin.
    let err = ExtractedBoard::from_eagle_brd(&read("eagle_non_finite_coordinate.brd"))
        .expect_err("a NaN element coordinate must be refused");
    message_says(
        &err.to_string(),
        &["geometry is corrupt", "not a finite number"],
    );
}

#[test]
fn merge_conflict_markers_are_refused_not_parsed() {
    // `<<<<<<<` and `>>>>>>>` are legal bare atoms, so a conflicted board
    // parsed happily with BOTH sides of the conflict in the netlist, and every
    // number in the report described a board that never existed.
    let err = ExtractedBoard::from_kicad_pcb(&read("merge_conflict.kicad_pcb"))
        .expect_err("a conflicted file must be refused");
    message_says(
        &err.to_string(),
        &["merge conflict", "Resolve the conflict"],
    );
}

#[test]
fn ipc_356_without_designators_says_what_is_missing() {
    // Real via-only / testpoint-only IPC-D-356 exports leave the
    // reference-designator columns blank on every record. Nine such files in the
    // corpus (24 to 397 records each) came out as zero components and were
    // refused with "this board parsed, but is empty", about a file with
    // hundreds of records in it.
    let err = ExtractedBoard::from_ipc_d356(&read("via_only_no_designators.d356"))
        .expect_err("a designator-less netlist cannot be bound");
    let msg = err.to_string();
    message_says(
        &msg,
        &[
            "test record",
            "reference-designator field",
            "Re-export the netlist",
        ],
    );
    // The message must PROVE the connectivity was read, so the user can tell
    // this apart from an unreadable file.
    assert!(
        msg.contains("GND") && msg.contains("named net"),
        "must name the nets it did read: {msg}"
    );
}

#[test]
fn ipc_356_with_no_connectivity_at_all_is_refused() {
    // A netlist whose every designator-bearing record is N/C names parts but
    // wires nothing. It used to read as an analysable board with 50 components
    // and zero nets, so every connectivity check passed over a netlist with no
    // connectivity in it.
    let err = ExtractedBoard::from_ipc_d356(&read("all_no_connect.d356"))
        .expect_err("a netlist with no nets cannot be checked");
    message_says(
        &err.to_string(),
        &["N/C (no-connect)", "no connectivity at all", "Re-export"],
    );
}

#[test]
fn kicad_pos_placement_export_is_read() {
    // KiCad's `.pos` export is whitespace-ALIGNED and puts its column header
    // inside a `#` comment. The CSV reader skipped the header as a comment and
    // then found no delimiter, so the file read as "not a P&P file" and every
    // KiCad fab folder silently lost the ONE input a gerber job needs to bind.
    let placed = hauksbee_extract::gerber::placement::parse_pnp(&read(
        "gerber_kicad_pos/board-all.pos",
    ));
    let refs: Vec<&str> = placed.iter().map(|p| p.reference.as_str()).collect();
    assert_eq!(refs, ["R1", "C1", "U1"], "all three rows must parse");
    // A value containing a space ("10k 1%") must not shift the columns after it:
    // the fields are read from the END of the row for exactly this reason.
    assert_eq!(placed[0].value, "10k 1%");
    assert_eq!(placed[0].package, "R_0402_1005Metric");
    assert!((placed[0].x - 10.0).abs() < 1e-9 && (placed[0].y - 10.0).abs() < 1e-9);
    assert!((placed[1].rotation - 180.0).abs() < 1e-9);
    assert!(placed[0].top && !placed[2].top, "side column must be read");
}

#[test]
fn gerber_job_with_a_kicad_pos_binds_components() {
    // End to end: copper plus a KiCad `.pos` is the only shape of gerber job
    // that CAN bind, and it now does.
    let extraction = hauksbee_extract::gerber::from_gerber_dir(&fixture("gerber_kicad_pos"))
        .expect("a job with copper and a placement file must extract");
    assert!(
        !extraction.board.components.is_empty(),
        "the placement file names parts, so components must land: {:?}",
        extraction.stats
    );
}

#[test]
fn a_lone_fab_film_says_to_pass_the_folder() {
    // A single `.gbr` or `.drl` is one file out of a job. Reciting the whole
    // accepted-formats list reads as "unsupported" when the real answer is
    // "pass the folder, not this one file".
    for (name, expect) in [
        ("lone_gerber_layer.gbr", "single gerber layer"),
        ("lone_excellon.drl", "single Excellon drill program"),
    ] {
        let bytes = std::fs::read(fixture(name)).expect("fixture");
        let err = ExtractedBoard::from_auto(&String::from_utf8_lossy(&bytes))
            .expect_err("one film is not a board");
        message_says(&err.to_string(), &[expect, "point it at the folder"]);
    }
}

#[test]
fn bare_role_name_fab_folder_is_recognised_as_copper() {
    // `Top.gbr` / `Bottom.gbr` was the single most common naming shape among 60
    // real fab folders (DipTrace, Sprint Layout, PCB Elegance and house CAM
    // scripts all plot this way), and matched none of the KiCad or Protel rules:
    // every such job was refused with "no copper gerber layers found here"
    // while the copper sat right there in the folder.
    use hauksbee_extract::gerber::layers::{classify, LayerRole};
    let dir = fixture("gerber_bare_role_names");
    let copper: Vec<String> = ["Top.gbr", "Bottom.gbr"]
        .iter()
        .map(|f| match classify(&dir.join(f)) {
            LayerRole::Copper { name, .. } => name,
            other => panic!("{f} must classify as copper, got {other:?}"),
        })
        .collect();
    assert_eq!(copper.len(), 2, "{copper:?}");

    // The films that carry the SAME role word must NOT become copper: reading an
    // assembly drawing as a copper layer invents nets out of silkscreen.
    for f in [
        "TopMask.gbr",
        "TopSilk.gbr",
        "TopAssy.gbr",
        "TopDimension.gbr",
    ] {
        let role = classify(&dir.join(f));
        assert!(
            !role.is_copper(),
            "{f} must not be read as copper, got {role:?}"
        );
    }
    assert_eq!(classify(&dir.join("BoardOutline.gbr")), LayerRole::Outline);
    assert_eq!(classify(&dir.join("Through.drl")), LayerRole::Drill);
}

#[test]
fn bare_role_words_do_not_match_inside_other_words() {
    // `top` sits inside `stopmask` and `bot` inside `robot`. Substring matching
    // would read either as a copper layer.
    use hauksbee_extract::gerber::layers::classify;
    for name in ["robotics-project.gbr", "photoplot.gbr"] {
        let role = classify(Path::new(name));
        assert!(
            !role.is_copper(),
            "{name} must not be read as copper, got {role:?}"
        );
    }
}

#[test]
fn bare_role_name_folder_reconstructs_copper() {
    // End to end: the folder above must now reach the reverse extractor rather
    // than being turned away at classification.
    let extraction = hauksbee_extract::gerber::from_gerber_dir(&fixture("gerber_bare_role_names"))
        .expect("a Top.gbr/Bottom.gbr job must reach the extractor");
    // A fab archive has no part list, so zero components here is CORRECT; what
    // matters is that copper was read at all.
    assert!(
        extraction.stats.n_layers >= 2,
        "both copper films must be parsed, got {:?}",
        extraction.stats
    );
}
