//! `.gbrjob` manifest reading, proven two-sided.
//!
//! The Gerber Job File is the exporter's own manifest: it names each film's
//! role and each copper film's physical layer number. When present it settles
//! what filename inference can only guess: an Allegro-style plane named
//! without a stack digit (`pwr.art`, `gnd.art`) gets its true position, so a
//! blind via stitches the pair the drill actually joins. When absent, the
//! filename inference runs exactly as before.

use std::path::PathBuf;

use hauksbee_extract::gerber::from_gerber_dir;

/// A pad flash (1 mm disc) at (x, y) mm, optionally with an X2 net name.
fn film(body: &str) -> String {
    format!("%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.000000*%\n%ADD11C,0.300000*%\nD10*\n{body}M02*\n")
}

/// Four-layer job: top pad at (0,0) named VBUS; the PWR plane (physical L2)
/// carries a pad at (0,0) routed to a second pad at (10,0); the GND plane
/// (physical L3) carries a lone pad at (0,0); bottom is empty copper. A blind
/// L1-L2 drill sits at (0,0).
///
/// The stitch is the witness: only the film that truly sits at stack index 1
/// is reached by the blind barrel. If PWR lands there (correct), the VBUS net
/// carries PWR's routed track; if GND does (the unnumbered-name collapse),
/// VBUS reaches only the lone pad and carries no track.
fn write_job(dir: &PathBuf, with_gbrjob: bool) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("top.art"), film("%TO.N,VBUS*%\nX0Y0D03*\n%TD*%\n")).unwrap();
    std::fs::write(
        dir.join("pwr.art"),
        film("X0Y0D03*\nD11*\nX0Y0D02*\nX10000000Y0D01*\nD10*\nX10000000Y0D03*\n"),
    )
    .unwrap();
    std::fs::write(dir.join("gnd.art"), film("X0Y0D03*\n")).unwrap();
    std::fs::write(dir.join("bottom.art"), film("X20000000Y20000000D03*\n")).unwrap();
    std::fs::write(
        dir.join("blind-drill.txt"),
        "M48\n; #@! TF.FileFunction,Plated,1,2,PTH,Blind\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX0.0Y0.0\nT0\nM30\n",
    )
    .unwrap();
    if with_gbrjob {
        std::fs::write(
            dir.join("job.gbrjob"),
            r#"{
  "Header": {"GenerationSoftware": {"Vendor": "test"}},
  "FilesAttributes": [
    {"Path": "top.art", "FileFunction": "Copper,L1,Top", "FilePolarity": "Positive"},
    {"Path": "pwr.art", "FileFunction": "Copper,L2,Inr", "FilePolarity": "Positive"},
    {"Path": "gnd.art", "FileFunction": "Copper,L3,Inr", "FilePolarity": "Positive"},
    {"Path": "bottom.art", "FileFunction": "Copper,L4,Bot", "FilePolarity": "Positive"},
    {"Path": "blind-drill.txt", "FileFunction": "Plated,1,2,PTH,Blind"}
  ]
}"#,
        )
        .unwrap();
    }
}

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hauksbee_gbrjob_{tag}_{}", std::process::id()))
}

#[test]
fn gbrjob_orders_an_unnumbered_inner_stack_correctly() {
    let dir = tmp("with");
    write_job(&dir, true);
    let g = from_gerber_dir(&dir).expect("extract");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(g.stats.n_layers, 4);
    let vbus = g
        .board
        .nets
        .iter()
        .find(|n| n.name == "VBUS")
        .expect("the top pad names its net");
    let row = g
        .stats
        .net_copper
        .iter()
        .find(|nc| nc.net_id == vbus.id)
        .expect("net copper row for VBUS");
    assert_eq!(
        row.track_count, 1,
        "the blind L1-L2 barrel must reach the PWR plane (which carries the \
         routed track), not the GND plane the unnumbered filenames collapse to"
    );
}

#[test]
fn without_a_gbrjob_the_filename_inference_runs_as_before() {
    // The same job with NO manifest. `pwr.art` and `gnd.art` both collapse to
    // the single default inner slot (the documented Allegro-name limitation),
    // so which plane the blind via reaches is whatever the collapse picked;
    // the job still extracts, with 4 layers and the same pad count. This
    // pins the absence path: no manifest, no behavior change.
    let dir = tmp("without");
    write_job(&dir, false);
    let g = from_gerber_dir(&dir).expect("extract");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(g.stats.n_layers, 4);
    assert_eq!(g.stats.total_flashes, 5);
    assert!(g.board.nets.iter().any(|n| n.name == "VBUS"));
}

#[test]
fn kicad9_internal_layer_ids_order_the_stack_without_inventing_layers() {
    // KiCad 9 writes its INTERNAL layer IDs into the manifest: a four-layer
    // board's copper entries read L1(Top), L5(Inr), L7(Inr), L4(Bot). Trusting
    // those as physical positions sorted B.Cu into stack index 1 (shredding
    // every via stitch) and implied a 7-layer board. The rank of (side,
    // number) still orders the stack correctly, and the non-contiguous
    // numbers must NOT feed the physical-layer table or the layer count.
    let dir = tmp("kicad9");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Names give no order hints: classification must come from the manifest.
    std::fs::write(dir.join("a.gbr"), film("%TO.N,VBUS*%\nX0Y0D03*\n%TD*%\n")).unwrap();
    std::fs::write(
        dir.join("b.gbr"),
        film("X0Y0D03*\nD11*\nX0Y0D02*\nX10000000Y0D01*\nD10*\nX10000000Y0D03*\n"),
    )
    .unwrap();
    std::fs::write(dir.join("c.gbr"), film("X0Y0D03*\n")).unwrap();
    std::fs::write(dir.join("d.gbr"), film("%TO.N,BOTNET*%\nX0Y0D03*\n%TD*%\n")).unwrap();
    std::fs::write(
        dir.join("blind-drill.txt"),
        "M48\n; #@! TF.FileFunction,Plated,1,2,PTH,Blind\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX0.0Y0.0\nT0\nM30\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("job.gbrjob"),
        r#"{"FilesAttributes": [
    {"Path": "a.gbr", "FileFunction": "Copper,L1,Top"},
    {"Path": "b.gbr", "FileFunction": "Copper,L5,Inr"},
    {"Path": "c.gbr", "FileFunction": "Copper,L7,Inr"},
    {"Path": "d.gbr", "FileFunction": "Copper,L4,Bot"},
    {"Path": "blind-drill.txt", "FileFunction": "Plated,1,2,PTH,Blind"}
  ]}"#,
    )
    .unwrap();
    let g = from_gerber_dir(&dir).expect("extract");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(g.stats.n_layers, 4);
    assert!(
        !g.stats.notes.iter().any(|n| n.contains("7-layer")),
        "non-contiguous manifest numbers must not imply extra layers: {:?}",
        g.stats.notes
    );
    // The blind L1-L2 barrel reaches the film ranked second (b.gbr, which
    // carries the routed track), NOT the bottom film the raw L-numbers would
    // have sorted there.
    let vbus = g.board.nets.iter().find(|n| n.name == "VBUS").unwrap();
    let row = g
        .stats
        .net_copper
        .iter()
        .find(|nc| nc.net_id == vbus.id)
        .unwrap();
    assert_eq!(row.track_count, 1, "the L5 inner film sits at stack 1");
    assert!(
        g.board.nets.iter().any(|n| n.name == "BOTNET"),
        "the bottom pad stays on its own net; raw-number ordering would have \
         stitched it to VBUS instead"
    );
}

#[test]
fn a_gbrjob_that_agrees_with_numbered_filenames_changes_nothing() {
    // Two copies of a job whose inner planes DO carry stack digits
    // (`gnd02.art` = L2, `pwr03.art` = L3), one with a manifest saying the
    // same thing, one without. The reconstructions must be identical: the
    // manifest adds information only where the filenames had none.
    let build = |dir: &PathBuf, with_job: bool| {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("top.art"), film("X0Y0D03*\n")).unwrap();
        std::fs::write(dir.join("gnd02.art"), film("X0Y0D03*\nX5000000Y0D03*\n")).unwrap();
        std::fs::write(dir.join("pwr03.art"), film("X5000000Y0D03*\n")).unwrap();
        std::fs::write(dir.join("bottom.art"), film("X9000000Y0D03*\n")).unwrap();
        if with_job {
            std::fs::write(
                dir.join("job.gbrjob"),
                r#"{"FilesAttributes": [
    {"Path": "top.art", "FileFunction": "Copper,L1,Top"},
    {"Path": "gnd02.art", "FileFunction": "Copper,L2,Inr"},
    {"Path": "pwr03.art", "FileFunction": "Copper,L3,Inr"},
    {"Path": "bottom.art", "FileFunction": "Copper,L4,Bot"}
  ]}"#,
            )
            .unwrap();
        }
    };
    let summarize = |dir: &PathBuf| {
        let g = from_gerber_dir(dir).expect("extract");
        (
            g.stats.n_layers,
            g.stats.n_nets,
            g.stats.total_flashes,
            g.board
                .nets
                .iter()
                .map(|n| n.name.clone())
                .collect::<Vec<_>>(),
        )
    };
    let d1 = tmp("agree_with");
    let d2 = tmp("agree_without");
    build(&d1, true);
    build(&d2, false);
    let (a, b) = (summarize(&d1), summarize(&d2));
    let _ = std::fs::remove_dir_all(&d1);
    let _ = std::fs::remove_dir_all(&d2);
    assert_eq!(a, b, "an agreeing manifest must change nothing");
}
