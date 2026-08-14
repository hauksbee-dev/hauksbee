//! Altium fab-package metadata is authority; filename inference is fallback.

use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::from_gerber_dir;

fn film(x_mm: i32) -> String {
    format!(
        "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.000000*%\nD10*\nX{}Y0D03*\nM02*\n",
        x_mm * 1_000_000
    )
}

fn drill() -> &'static str {
    "M48\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX0.0Y0.0\nT0\nM30\n"
}

fn drill_with_span(from: u32, to: u32) -> String {
    format!(
        "M48\n; #@! TF.FileFunction,Plated,{from},{to},PTH\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX0.0Y0.0\nT0\nM30\n"
    )
}

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hauksbee_altium_metadata_{tag}_{}",
        std::process::id()
    ))
}

fn write_four_layer_job(dir: &Path, with_ldp: bool) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    for name in ["board.GTL", "board.G1", "board.G2", "board.GBL"] {
        std::fs::write(dir.join(name), film(0)).unwrap();
    }
    // Mixed-case member name versus lower-case manifest spelling is what real
    // Altium packages ship.
    std::fs::write(dir.join("Board-PTH.TXT"), drill()).unwrap();
    if with_ldp {
        std::fs::write(
            dir.join("board.LDP"),
            "Layer Pairs Export File for PCB: board.PcbDoc\n\
             LayersSetName=Top_In1_Plated_Blind_Holes|DrillFile=board-pth.txt|DrillLayers=gtl,g1\n",
        )
        .unwrap();
    }
}

#[test]
fn ldp_drill_span_overrides_the_filename_fallback() {
    let with = tmp("ldp_with");
    write_four_layer_job(&with, true);
    let from_metadata = from_gerber_dir(&with).expect("LDP-backed extraction");
    let _ = std::fs::remove_dir_all(&with);

    assert_eq!(from_metadata.stats.n_layers, 4);
    assert_eq!(from_metadata.stats.n_holes, 1);
    assert_eq!(
        from_metadata.stats.n_nets, 3,
        "the blind L1-L2 barrel joins only the first two coincident flashes"
    );
    assert!(from_metadata.stats.notes.iter().any(|note| {
        note.contains(".LDP")
            && note.contains("Board-PTH.TXT")
            && note.contains("physical copper span L1-L2")
    }));

    let without = tmp("ldp_without");
    write_four_layer_job(&without, false);
    let from_names = from_gerber_dir(&without).expect("filename fallback extraction");
    let _ = std::fs::remove_dir_all(&without);
    assert_eq!(from_names.stats.n_layers, 4);
    assert_eq!(
        from_names.stats.n_nets, 1,
        "without metadata, the one PTH file retains the prior through-hole fallback"
    );
    assert!(from_names
        .stats
        .notes
        .iter()
        .all(|note| !note.contains(".LDP")));
}

#[test]
fn ldp_names_an_opaque_drill_file_without_filename_help() {
    let dir = tmp("ldp_opaque_drill");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("board.GTL"), film(0)).unwrap();
    std::fs::write(dir.join("board.GBL"), film(0)).unwrap();
    std::fs::write(dir.join("opaque.dat"), drill()).unwrap();
    std::fs::write(
        dir.join("board.LDP"),
        "LayersSetName=Top_Bot_Plated_Thru_Holes|DrillFile=opaque.dat|DrillLayers=gtl,gbl\n",
    )
    .unwrap();

    let extracted = from_gerber_dir(&dir).expect("LDP classifies the opaque drill member");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(extracted.stats.n_layers, 2);
    assert_eq!(extracted.stats.n_holes, 1);
    assert_eq!(extracted.stats.n_nets, 1);
}

#[test]
fn an_ldp_and_drill_body_span_conflict_refuses_the_stitch_out_loud() {
    let dir = tmp("ldp_span_conflict");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["board.GTL", "board.G1", "board.G2", "board.GBL"] {
        std::fs::write(dir.join(name), film(0)).unwrap();
    }
    std::fs::write(dir.join("board-PTH.TXT"), drill_with_span(1, 3)).unwrap();
    std::fs::write(
        dir.join("board.LDP"),
        "LayersSetName=Top_In1_Plated_Blind_Holes|DrillFile=board-PTH.TXT|DrillLayers=gtl,g1\n",
    )
    .unwrap();

    let extracted = from_gerber_dir(&dir).expect("conflict is a visible non-stitching result");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(extracted.stats.n_holes, 1);
    assert_eq!(extracted.stats.refused_span_holes, 1);
    assert_eq!(
        extracted.stats.n_nets, 4,
        "a conflicting explicit span must not join any coincident layer"
    );
    assert!(extracted.stats.notes.iter().any(|note| {
        note.contains("drill file and .LDP declare different") && note.contains("stitch no layers")
    }));
}

#[test]
fn ldp_plated_authority_overrides_an_npth_filename() {
    let dir = tmp("ldp_over_name");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("board.GTL"), film(0)).unwrap();
    std::fs::write(dir.join("board.GBL"), film(0)).unwrap();
    std::fs::write(dir.join("board-NPTH.txt"), drill()).unwrap();
    std::fs::write(
        dir.join("board.LDP"),
        "LayersSetName=Top_Bot_Plated_Thru_Holes|DrillFile=board-NPTH.txt|DrillLayers=gtl,gbl\n",
    )
    .unwrap();

    let extracted = from_gerber_dir(&dir).expect("package plating outranks the name");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(extracted.stats.n_holes, 1);
    assert_eq!(extracted.stats.n_nets, 1);
    assert!(extracted.stats.notes.iter().any(|note| {
        note.contains(".LDP declares board-NPTH.txt as plated")
            && note.contains("physical copper span L1-L2")
    }));
}

#[test]
fn ldp_and_file_body_plating_conflict_is_a_visible_refusal() {
    let dir = tmp("ldp_plating_conflict");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("board.GTL"), film(0)).unwrap();
    std::fs::write(dir.join("board.GBL"), film(0)).unwrap();
    std::fs::write(
        dir.join("opaque.dat"),
        "M48\n; #@! TF.FileFunction,NonPlated,1,2,NPTH\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX0.0Y0.0\nT0\nM30\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("board.LDP"),
        "LayersSetName=Top_Bot_Plated_Thru_Holes|DrillFile=opaque.dat|DrillLayers=gtl,gbl\n",
    )
    .unwrap();

    let extracted = from_gerber_dir(&dir).expect("plating conflict is reported, not guessed");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(extracted.stats.n_holes, 0);
    assert_eq!(extracted.stats.refused_plating_files, 1);
    assert!(extracted.stats.notes.iter().any(|note| {
        note.contains("disagree about whether its holes are plated") && note.contains("refused")
    }));
}

fn write_extrep_job(dir: &Path, with_extrep: bool) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    // `.GM1` normally means outline. The report deliberately declares it top
    // copper, proving the package metadata wins when it disagrees with the
    // filename convention.
    std::fs::write(dir.join("misleading.GM1"), film(0)).unwrap();
    std::fs::write(dir.join("board.GBL"), film(10)).unwrap();
    if with_extrep {
        std::fs::write(
            dir.join("board.EXTREP"),
            "Layer Extension     Layer Description\n\
             .GM1                Top Layer\n\
             .GBL                Bottom Layer\n",
        )
        .unwrap();
    }
}

#[test]
fn extrep_role_wins_and_names_its_disagreement_with_filename_inference() {
    let with = tmp("extrep_with");
    write_extrep_job(&with, true);
    let from_metadata = from_gerber_dir(&with).expect("EXTREP-backed extraction");
    let _ = std::fs::remove_dir_all(&with);
    assert_eq!(from_metadata.stats.n_layers, 2);
    assert!(from_metadata.stats.notes.iter().any(|note| {
        note.contains(".EXTREP")
            && note.contains("misleading.GM1")
            && note.contains("filename inference")
            && note.contains("exporter metadata was used")
    }));

    let without = tmp("extrep_without");
    write_extrep_job(&without, false);
    let from_names = from_gerber_dir(&without).expect("filename fallback extraction");
    let _ = std::fs::remove_dir_all(&without);
    assert_eq!(
        from_names.stats.n_layers, 1,
        "without metadata, .GM1 retains its existing outline classification"
    );
    assert!(from_names
        .stats
        .notes
        .iter()
        .all(|note| !note.contains(".EXTREP")));
}

#[test]
fn a_reused_extrep_extension_is_visible_but_never_arbitrarily_applied() {
    let dir = tmp("extrep_contested");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("board-Top.gbr"), film(0)).unwrap();
    std::fs::write(dir.join("board-Bottom.gbr"), film(10)).unwrap();
    std::fs::write(
        dir.join("board.EXTREP"),
        "Layer Extension     Layer Description\n\
         .gbr                Top Layer\n\
         .gbr                Top Overlay\n\
         .gbr                Bottom Layer\n",
    )
    .unwrap();
    let extracted = from_gerber_dir(&dir).expect("ambiguous EXTREP falls back by name");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(extracted.stats.n_layers, 2);
    assert!(extracted.stats.notes.iter().any(|note| {
        note.contains("more than one layer role to .gbr") && note.contains("falls back")
    }));
}

#[test]
fn exact_file_gbrjob_authority_names_an_extrep_disagreement() {
    let dir = tmp("gbrjob_extrep_conflict");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("misleading.GM1"), film(0)).unwrap();
    std::fs::write(dir.join("board.GBL"), film(10)).unwrap();
    std::fs::write(
        dir.join("board.EXTREP"),
        "Layer Extension     Layer Description\n.GM1 Top Layer\n.GBL Bottom Layer\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("board.gbrjob"),
        r#"{"FilesAttributes":[{"Path":"misleading.GM1","FileFunction":"Profile,NP"}]}"#,
    )
    .unwrap();

    let extracted = from_gerber_dir(&dir).expect("exact-file manifest wins visibly");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(extracted.stats.n_layers, 1);
    assert!(extracted.stats.notes.iter().any(|note| {
        note.contains(".gbrjob declares misleading.GM1 as the board outline")
            && note.contains(".EXTREP says top copper")
            && note.contains("exact-file .gbrjob entry was used")
    }));
}

#[test]
fn extrep_bottom_and_x2_bottom_are_not_reported_as_a_conflict() {
    let dir = tmp("extrep_x2_agree");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("board.GTL"), film(0)).unwrap();
    std::fs::write(
        dir.join("opaque.foo"),
        "%TF.FileFunction,Copper,L2,Bot*%\n%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.000000*%\nD10*\nX10000000Y0D03*\nM02*\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("board.EXTREP"),
        "Layer Extension     Layer Description\n.FOO Bottom Layer\n",
    )
    .unwrap();

    let extracted = from_gerber_dir(&dir).expect("X2 and EXTREP agree on bottom");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(extracted.stats.n_layers, 2);
    assert!(extracted
        .stats
        .notes
        .iter()
        .all(|note| !note.contains("opaque.foo")));
}
