//! Real-world fab packages as a fab house receives them: every major tool's
//! naming, archives with packer litter, Protel's `.TXT` drill, Excellon
//! dialects, and drill files that never say whether they are plated.
//!
//! One two-layer board throughout: R1 pads at (10,10) and (11,10), C1 pads at
//! (14,10) and (15,10), a top track from R1.2 to a via at (12,10), and bottom
//! copper from that via to (14,10). With the via stitched the job has 4 nets
//! (R1.1; R1.2 + via + bottom copper; C1.1; C1.2); without it, 5.

use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::layers::{classify, classify_file, LayerRole};
use hauksbee_extract::gerber::{excellon, from_gerber_dir, from_gerber_zip, GerberExtraction};

const TOP: &str =
    "G04 film*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n%ADD10C,0.600000*%\n%ADD11C,0.250000*%\nD10*\n\
X10000000Y10000000D03*\nX11000000Y10000000D03*\nX12000000Y10000000D03*\nX14000000Y10000000D03*\n\
X15000000Y10000000D03*\nD11*\nX11000000Y10000000D02*\nX12000000Y10000000D01*\nM02*\n";
const BOTTOM: &str = "G04 film*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n%ADD10C,0.600000*%\n%ADD11C,0.250000*%\nD10*\n\
X12000000Y10000000D03*\nX14000000Y10000000D03*\nD11*\nX12000000Y10000000D02*\nX14000000Y10000000D01*\nM02*\n";
const INNER: &str = "G04 film*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n%ADD10C,0.600000*%\nD10*\nX12000000Y10000000D03*\nM02*\n";
const MASK: &str = "G04 film*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n%ADD10C,0.700000*%\nD10*\nX10000000Y10000000D03*\nM02*\n";
const OUTLINE: &str = "G04 film*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n%ADD10C,0.100000*%\nD10*\n\
X8000000Y8000000D02*\nX17000000Y8000000D01*\nX17000000Y12000000D01*\nX8000000Y12000000D01*\nX8000000Y8000000D01*\nM02*\n";
const DRILL: &str = "M48\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX12.0Y10.0\nM30\n";
const NPTH: &str = "M48\nFMAT,2\nMETRIC\nT1C3.000\n%\nG90\nG05\nT1\nX9.0Y11.5\nM30\n";
const POS: &str = "### Module positions - created on Fri Dec 22 20:19:47 2017 ###\n\
## Unit = mm, Angle = deg.\n## Side : All\n\
# Ref     Val              Package                PosX       PosY       Rot  Side\n\
R1        10k 1%           R_0402_1005Metric   10.5000    10.0000    0.0000  top\n\
C1        100n             C_0402_1005Metric   14.5000    10.0000    0.0000  top\n\
## End\n";
const CSV: &str = "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\nR1,10k,R0402,10.5mm,10mm,0,T\nC1,100n,C0402,14.5mm,10mm,0,T\n";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_fab_pkg_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_package(dir: &Path, files: &[(&str, &str)]) {
    for (name, body) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in files {
        w.start_file(*name, opts).unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn extract_dir(tag: &str, files: &[(&str, &str)]) -> GerberExtraction {
    let dir = scratch(tag);
    write_package(&dir, files);
    let out = from_gerber_dir(&dir).unwrap_or_else(|e| panic!("{tag} must extract: {e}"));
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn extract_zip(tag: &str, files: &[(&str, &[u8])]) -> GerberExtraction {
    let dir = scratch(tag);
    let path = dir.join(format!("{tag}.zip"));
    std::fs::write(&path, zip_of(files)).unwrap();
    let out = from_gerber_zip(&path).unwrap_or_else(|e| panic!("{tag} must extract: {e}"));
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// The whole board came through: both copper layers, the via, both parts, and
/// the via-stitched net partition.
fn assert_complete(g: &GerberExtraction, what: &str) {
    let s = &g.stats;
    assert_eq!(s.n_layers, 2, "{what}: copper layers, notes {:?}", s.notes);
    assert_eq!(s.n_holes, 1, "{what}: plated holes, notes {:?}", s.notes);
    assert_eq!(s.n_nets, 4, "{what}: nets, notes {:?}", s.notes);
    assert_eq!(g.board.components.len(), 2, "{what}: components");
}

#[test]
fn archive_litter_is_not_a_copper_layer() {
    // A zip made on macOS carries `__MACOSX/._board-F_Cu.gbr` resource forks
    // and `.DS_Store`; Windows adds `Thumbs.db`. The fork's name classifies as
    // top copper and its body parses as an empty film, so the stack grew a
    // third copper layer.
    let junk: &[u8] = b"\x00\x05\x16\x07junk";
    let g = extract_zip(
        "litter",
        &[
            ("__MACOSX/._board-F_Cu.gbr", junk),
            ("__MACOSX/fab/._board-PTH.drl", junk),
            (".DS_Store", b"\x00\x00\x00\x01Bud1"),
            ("Thumbs.db", b"\xd0\xcf\x11\xe0"),
            ("board-F_Cu.gbr", TOP.as_bytes()),
            ("board-B_Cu.gbr", BOTTOM.as_bytes()),
            ("board-PTH.drl", DRILL.as_bytes()),
            ("board-all.pos", POS.as_bytes()),
        ],
    );
    assert_complete(&g, "zip with macOS and Windows litter");

    // The same litter copied into a plain folder.
    let g = extract_dir(
        "litter_dir",
        &[
            ("._board-F_Cu.gbr", "junk"),
            (".DS_Store", "junk"),
            ("board-F_Cu.gbr", TOP),
            ("board-B_Cu.gbr", BOTTOM),
            ("board-PTH.drl", DRILL),
            ("board-all.pos", POS),
        ],
    );
    assert_complete(&g, "folder with macOS litter");
}

#[test]
fn nested_folders_and_mixed_case_extensions_read_the_same() {
    let g = extract_zip(
        "nested",
        &[
            ("fab/gerbers/Board-F_Cu.GBR", TOP.as_bytes()),
            ("fab/gerbers/Board-B_Cu.Gbr", BOTTOM.as_bytes()),
            ("fab/drill/Board-PTH.DRL", DRILL.as_bytes()),
            ("fab/Board-all.POS", POS.as_bytes()),
        ],
    );
    assert_complete(&g, "nested zip with mixed-case extensions");
}

#[test]
fn a_protel_txt_drill_is_found_by_its_body_and_a_readme_txt_is_not() {
    // Protel and Altium name the drill `<board>.TXT`, the same extension the
    // fab notes use. Neither name says which is which; the Excellon header does.
    let g = extract_dir(
        "protel",
        &[
            ("board.GTL", TOP),
            ("board.GBL", BOTTOM),
            ("board.GTO", MASK),
            ("board.GTS", MASK),
            ("board.GKO", OUTLINE),
            ("board.TXT", DRILL),
            ("README.TXT", "Fab notes: 2 layer, 1.6 mm FR4, HASL.\n"),
            ("board-all.pos", POS),
        ],
    );
    assert_complete(&g, "Protel package with a .TXT drill and a README.TXT");
    assert!(
        g.stats.notes.iter().all(|n| !n.contains("README")),
        "the readme must not be read as anything: {:?}",
        g.stats.notes
    );

    // Header-less drills are still drills: a tool table or a body of hits.
    assert_eq!(
        classify_file(Path::new("x.txt"), "T1C0.300\nT1\nX12.0Y10.0\nM30\n"),
        LayerRole::Drill
    );
    assert_eq!(
        classify_file(Path::new("x.txt"), "T01\nX012000Y010000\nX013000Y010000\n"),
        LayerRole::Drill
    );
    assert_eq!(
        classify_file(Path::new("nc_param.txt"), "Units: MM\nFormat: 3.3\n"),
        LayerRole::Unknown
    );
    assert_eq!(
        classify_file(Path::new("README.TXT"), "Drill the 3 mm holes unplated.\n"),
        LayerRole::Unknown
    );
}

#[test]
fn eagle_cam_extensions_are_recognised() {
    for (name, want) in [
        (
            "board.cmp",
            LayerRole::Copper {
                index: 0,
                name: "board".into(),
            },
        ),
        (
            "board.sol",
            LayerRole::Copper {
                index: usize::MAX,
                name: "board".into(),
            },
        ),
        (
            "board.ly2",
            LayerRole::Copper {
                index: 1,
                name: "board".into(),
            },
        ),
        (
            "board.ly15",
            LayerRole::Copper {
                index: 14,
                name: "board".into(),
            },
        ),
        ("board.plc", LayerRole::Ignored),
        ("board.pls", LayerRole::Ignored),
        ("board.stc", LayerRole::Ignored),
        ("board.sts", LayerRole::Ignored),
        ("board.crc", LayerRole::Ignored),
        ("board.mil", LayerRole::Ignored),
        ("board.dim", LayerRole::Outline),
        ("board.drd", LayerRole::Drill),
        ("board.xln", LayerRole::Drill),
        ("board.dri", LayerRole::Unknown),
        ("board.gpi", LayerRole::Unknown),
        ("board.mnt", LayerRole::Unknown),
    ] {
        assert_eq!(classify(Path::new(name)), want, "{name}");
    }
    let g = extract_dir(
        "eagle",
        &[
            ("board.cmp", TOP),
            ("board.sol", BOTTOM),
            ("board.plc", MASK),
            ("board.stc", MASK),
            ("board.dim", OUTLINE),
            ("board.drd", "M48\nINCH,TZ\nT01C0.0118\n%\nT01\nX4724Y3937\nM30\n"),
            ("board.dri", "Drill Station Info File: board.dri\n\n  Data Mode : Absolute\n  Units : 1/10000 Inch\n\n  Tools:\n  T01 0.0118\n"),
            ("board.gpi", "Gerber Photoplotter Info File: board.gpi\n\n  Data Mode : Absolute\n"),
            ("board.csv", CSV),
        ],
    );
    // Eagle's lone `.drd` says nothing about plating; the via lands on a pad
    // flash on both layers, which is the evidence that reads it as plated.
    assert_complete(&g, "Eagle CAM package");
    assert_eq!(g.stats.inferred_plating_holes, 1);
}

#[test]
fn altium_drill_drawings_and_guides_are_not_drilling() {
    for name in [
        "Board.GD1",
        "Board.GG1",
        "Board.GPT",
        "Drill Drawing.gbr",
        "Drill Guide.gbr",
        "board-drl_map.gbr",
        "board-drl_map.pdf",
        "Board.REP",
        "Board.APR",
        "Board.DRR",
    ] {
        let role = classify(Path::new(name));
        assert_ne!(
            role,
            LayerRole::Drill,
            "{name} is a drawing about drilling, got {role:?}"
        );
        assert!(!role.is_copper(), "{name} is not copper, got {role:?}");
    }
    let drill_guide = "G04 film*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n%ADD10C,0.300000*%\nD10*\nX12000000Y10000000D03*\nM02*\n";
    let g = extract_dir(
        "altium",
        &[
            ("Board.GTL", TOP),
            ("Board.G1", INNER),
            ("Board.G2", INNER),
            ("Board.GBL", BOTTOM),
            ("Board.GTO", MASK),
            ("Board.GTS", MASK),
            ("Board.GTP", MASK),
            ("Board.GM1", OUTLINE),
            ("Board.GD1", drill_guide),
            ("Board.GG1", drill_guide),
            ("Board.TXT", DRILL),
            ("Board.REP", "Layer report\n"),
            ("Board.APR", "Aperture list\n"),
            ("Pick Place for Board.csv", CSV),
        ],
    );
    let s = &g.stats;
    assert_eq!(s.n_layers, 4, "GTL/G1/G2/GBL: {:?}", s.notes);
    assert_eq!(
        s.n_holes, 1,
        "one hit from Board.TXT, none from the guide: {:?}",
        s.notes
    );
    assert_eq!(s.n_nets, 4, "{:?}", s.notes);
    assert_eq!(g.board.components.len(), 2);
}

#[test]
fn the_files_own_file_function_outranks_its_name() {
    let top_copper = "%TF.FileFunction,Copper,L1,Top*%\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n";
    let mask = "%TF.FileFunction,Soldermask,Top*%\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n";
    let kicad5_bottom = "G04 #@! TF.GenerationSoftware,KiCad,Pcbnew,5.1.10*\nG04 #@! TF.FileFunction,Copper,L2,Bot*\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n";
    let x2_drill = "M48\n; #@! TF.FileFunction,Plated,1,2,PTH\nFMAT,2\nMETRIC\nT1C0.300\n%\nM30\n";
    assert!(matches!(
        classify_file(Path::new("mystery-name.ger"), top_copper),
        LayerRole::Copper { index: 0, .. }
    ));
    assert_eq!(
        classify_file(Path::new("Top Layer.gbr"), mask),
        LayerRole::Ignored
    );
    assert!(matches!(
        classify_file(Path::new("plot-2.ger"), kicad5_bottom),
        LayerRole::Copper {
            index: usize::MAX,
            ..
        }
    ));
    assert_eq!(
        classify_file(Path::new("board.txt"), x2_drill),
        LayerRole::Drill
    );
    assert_eq!(
        classify_file(
            Path::new("board-drl_map.gbr"),
            "%TF.FileFunction,Drillmap*%\n"
        ),
        LayerRole::Ignored
    );
    // A function this reader does not know leaves the name in charge.
    assert!(matches!(
        classify_file(
            Path::new("board-F_Cu.gbr"),
            "%TF.FileFunction,Futuristic,Top*%\n"
        ),
        LayerRole::Copper { index: 0, .. }
    ));
}

#[test]
fn excellon_digit_patterns_and_zero_suppression_place_the_hit() {
    let at_12_10 = |text: &str, what: &str| {
        let d = excellon::parse(text);
        assert_eq!(d.holes.len(), 1, "{what}: {:?}", d.notes);
        // Four-decimal inch data rounds 12 mm to 11.999 mm.
        assert!(
            (d.holes[0].x - 12.0).abs() < 5e-3 && (d.holes[0].y - 10.0).abs() < 5e-3,
            "{what}: ({}, {})",
            d.holes[0].x,
            d.holes[0].y
        );
    };
    at_12_10(
        "M48\nMETRIC,LZ,0000.00\nT1C0.300\n%\nT1\nX001200Y001000\nM30\n",
        "METRIC,LZ,0000.00",
    );
    at_12_10(
        "M48\nMETRIC,TZ,0000.00\nT1C0.300\n%\nT1\nX1200Y1000\nM30\n",
        "METRIC,TZ,0000.00",
    );
    at_12_10(
        "M48\nMETRIC,LZ\nT1C0.300\n%\nT1\nX012Y01\nM30\n",
        "METRIC,LZ trailing zeros stripped",
    );
    at_12_10(
        "M48\nMETRIC,TZ\nT1C0.300\n%\nT1\nX12000Y10000\nM30\n",
        "METRIC,TZ",
    );
    at_12_10(
        "M48\nINCH,LZ\nT1C0.0118\n%\nT1\nX004724Y003937\nM30\n",
        "INCH,LZ",
    );
    at_12_10(
        "M48\nINCH,TZ\nT1C0.0118\n%\nT1\nX4724Y3937\nM30\n",
        "INCH,TZ",
    );
    at_12_10("M48\n;FILE_FORMAT=2:5\nINCH,LZ\n;TYPE=PLATED\nT1F00S00C0.01181\n%\nG90\nG05\nT01\nX0047244Y0039370\nM30\n", "Altium 2:5");
    at_12_10(
        "M48\r\nFMAT,2\r\nMETRIC\r\nT1C0.300\r\n%\r\nG90\r\nG05\r\nT1\r\nX12.0Y10.0\r\nM30\r\n",
        "CRLF",
    );
    at_12_10("METRIC\nT1C0.300\nT1\nX12.0Y10.0\nM30\n", "no M48 header");
}

#[test]
fn an_undefined_tool_is_read_at_a_nominal_diameter_and_said_so() {
    let d = excellon::parse("M48\nMETRIC\n%\nT1\nX12.0Y10.0\nX13.0Y10.0\nT0\nM30\n");
    assert_eq!(d.holes.len(), 2, "{:?}", d.notes);
    assert!(d
        .holes
        .iter()
        .all(|h| (h.diameter - excellon::UNDEFINED_TOOL_DIAMETER_MM).abs() < 1e-9));
    assert_eq!(d.notes.len(), 1, "{:?}", d.notes);
    assert!(
        d.notes[0].contains("T1") && d.notes[0].contains("2 hit(s)"),
        "{}",
        d.notes[0]
    );

    // The note reaches the job, prefixed with the file.
    let g = extract_dir(
        "undefined_tool",
        &[
            ("board-F_Cu.gbr", TOP),
            ("board-B_Cu.gbr", BOTTOM),
            ("board-PTH.drl", "T1\nX12.0Y10.0\nM30\n"),
            ("board-all.pos", POS),
        ],
    );
    assert_complete(&g, "drill with no tool table");
    assert!(
        g.stats
            .notes
            .iter()
            .any(|n| n.starts_with("board-PTH.drl: tool T1")),
        "{:?}",
        g.stats.notes
    );
}

#[test]
fn header_less_integer_coordinates_are_refused_not_misplaced() {
    // No units, no format, no decimal point: `X012000` is 12.000 mm in 3.3
    // and 1.2000 inch in 2.4, and nothing in the file says which.
    let d = excellon::parse("T01\nX012000Y010000\nM30\n");
    assert!(d.holes.is_empty(), "{:?}", d.holes);
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("neither units nor a number format")),
        "{:?}",
        d.notes
    );
    let g = extract_dir(
        "headerless",
        &[
            ("board-F_Cu.gbr", TOP),
            ("board-B_Cu.gbr", BOTTOM),
            ("board-PTH.drl", "T01\nX012000Y010000\nM30\n"),
            ("board-all.pos", POS),
        ],
    );
    assert_eq!(g.stats.n_holes, 0);
    assert_eq!(
        g.stats.n_nets, 5,
        "nothing stitched on an unreadable position"
    );
    assert!(
        g.stats
            .notes
            .iter()
            .any(|n| n.starts_with("board-PTH.drl:")),
        "{:?}",
        g.stats.notes
    );
}

#[test]
fn a_silent_drill_file_is_plated_only_where_pad_rings_say_so() {
    // DipTrace's `Through.drl`, Eagle's `.drd`, Allegro's `ncdrill.drl` and an
    // Altium `.TXT` without TYPE sections all say nothing about plating. Three
    // hits: one under a pad flash on both layers (the via at 12,10), one on
    // bare board (9,11.5), one under top copper only (10,10).
    let silent = "M48\nFMAT,2\nMETRIC\nT1C0.300\nT2C3.000\n%\nG90\nG05\nT1\nX12.0Y10.0\nX10.0Y10.0\nT2\nX9.0Y11.5\nM30\n";
    let g = extract_dir(
        "silent_drill",
        &[
            ("Top.gbr", TOP),
            ("Bottom.gbr", BOTTOM),
            ("BoardOutline.gbr", OUTLINE),
            ("Through.drl", silent),
            ("board.csv", CSV),
        ],
    );
    assert_complete(&g, "silent drill with pad-ring evidence");
    assert_eq!(g.stats.inferred_plating_holes, 1);
    assert_eq!(g.stats.refused_plating_files, 0);
    let note = g
        .stats
        .notes
        .iter()
        .find(|n| n.starts_with("Through.drl:"))
        .unwrap_or_else(|| panic!("the inference must be named: {:?}", g.stats.notes));
    assert!(
        note.contains("1 of its 3 hit(s)") && note.contains("other 2"),
        "{note}"
    );

    // No ring anywhere: nothing can be read as plated, and the file says so.
    let g = extract_dir(
        "silent_drill_no_rings",
        &[
            ("Top.gbr", TOP),
            ("Bottom.gbr", BOTTOM),
            ("Through.drl", NPTH),
            ("board.csv", CSV),
        ],
    );
    assert_eq!(g.stats.n_holes, 0);
    assert_eq!(g.stats.inferred_plating_holes, 0);
    assert_eq!(g.stats.refused_plating_files, 1);
    assert!(
        g.stats
            .notes
            .iter()
            .any(|n| n.starts_with("Through.drl:") && n.contains("none of its 1 hit(s)")),
        "{:?}",
        g.stats.notes
    );

    // A single copper layer offers no second ring: the old refusal stands.
    let g = extract_dir(
        "silent_drill_one_layer",
        &[
            ("Top.gbr", TOP),
            ("Through.drl", silent),
            ("board.csv", CSV),
        ],
    );
    assert_eq!(g.stats.n_holes, 0);
    assert_eq!(g.stats.refused_plating_files, 1);
}

#[test]
fn the_no_copper_refusal_names_every_file_and_its_reading() {
    let dir = scratch("no_copper");
    write_package(
        &dir,
        &[
            ("board.ger", TOP),
            ("board.GTO", MASK),
            ("README.TXT", "notes\n"),
            ("board-PTH.drl", DRILL),
        ],
    );
    let msg = match from_gerber_dir(&dir) {
        Ok(_) => panic!("no copper film is recognisable here"),
        Err(e) => e.to_string(),
    };
    let _ = std::fs::remove_dir_all(&dir);
    for phrase in [
        "4 file(s)",
        "board.ger (unknown)",
        "board.GTO (electrically irrelevant artwork)",
        "README.TXT (unknown)",
        "board-PTH.drl (drilling)",
        "layer_map.txt",
        "point hauksbee at",
    ] {
        assert!(msg.contains(phrase), "refusal must say {phrase:?}: {msg}");
    }
}

#[test]
fn a_full_kicad_export_keeps_only_the_copper_films_as_copper() {
    // Every non-copper film carries the top copper's geometry, so a
    // misclassification would show as an extra copper layer.
    let g = extract_dir(
        "kicad_full",
        &[
            ("board-F_Cu.gbr", TOP),
            ("board-B_Cu.gbr", BOTTOM),
            ("board-F_Paste.gbr", TOP),
            ("board-F_Courtyard.gbr", TOP),
            ("board-B_Fab.gbr", TOP),
            ("board-F_Mask.gbr", TOP),
            ("board-B_Silkscreen.gbr", TOP),
            ("board-User_Comments.gbr", TOP),
            ("board-User_1.gbr", TOP),
            ("board-Keepout.gbr", TOP),
            ("board-F_Adhesive.gbr", TOP),
            ("board-drl_map.gbr", TOP),
            ("board-drl_map.pdf", "%PDF-1.4 junk"),
            ("board-Edge_Cuts.gbr", OUTLINE),
            ("board-Margin.gbr", OUTLINE),
            ("board-PTH.drl", DRILL),
            ("board-NPTH.drl", NPTH),
            ("board-all.pos", POS),
        ],
    );
    assert_complete(&g, "full KiCad 9 export without a .gbrjob");
}

#[test]
fn easyeda_and_diptrace_and_allegro_packages_read() {
    let g = extract_dir(
        "easyeda",
        &[
            ("Gerber_TopLayer.GTL", TOP),
            ("Gerber_BottomLayer.GBL", BOTTOM),
            ("Gerber_TopSilkscreenLayer.GTO", MASK),
            ("Gerber_TopSolderMaskLayer.GTS", MASK),
            ("Gerber_BoardOutlineLayer.GKO", OUTLINE),
            (
                "Drill_PTH_Through.DRL",
                "M48\nMETRIC,LZ\nT1C0.300\n%\nT1\nX012000Y010000\nM30\n",
            ),
            (
                "Drill_NPTH_Through.DRL",
                "M48\nMETRIC,LZ\nT1C3.000\n%\nT1\nX009000Y011500\nM30\n",
            ),
            ("PickAndPlace_PCB1.csv", CSV),
        ],
    );
    assert_complete(&g, "EasyEDA/JLC export");

    let g = extract_dir(
        "diptrace",
        &[
            ("Top.gbr", TOP),
            ("Bottom.gbr", BOTTOM),
            ("TopMask.gbr", MASK),
            ("TopAssy.gbr", TOP),
            ("BoardOutline.gbr", OUTLINE),
            ("Through.drl", DRILL),
            ("board.csv", CSV),
        ],
    );
    assert_complete(&g, "DipTrace export");

    let g = extract_dir(
        "allegro",
        &[
            ("TOP.art", TOP),
            ("BOTTOM.art", BOTTOM),
            ("SOLDERMASK_TOP.art", MASK),
            ("OUTLINE.art", OUTLINE),
            ("ncdrill-1-2.drl", DRILL),
            ("nc_param.txt", "Units: MM\nFormat: 3.3\n"),
            ("art_param.txt", "Units: MM\n"),
            ("board.csv", CSV),
        ],
    );
    assert_complete(&g, "Allegro export");
    assert!(
        g.stats.notes.iter().all(|n| !n.contains("nc_param")),
        "a parameter file is not a drill: {:?}",
        g.stats.notes
    );
}
