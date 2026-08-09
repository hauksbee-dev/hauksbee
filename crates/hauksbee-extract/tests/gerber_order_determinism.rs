//! Two places where the gerber reader used to let hash-map iteration order
//! decide what it reported, both proven on synthetic jobs that need no corpus.
//!
//! Neither is a cosmetic ordering. The `.gbrjob` copper rank IS the provisional
//! stack index, so a tie resolved by hash order moves a film up or down the
//! stackup and changes which layers a blind via stitches. The X2 disagreement
//! notes become evidence assumptions in the exported report, so their order is
//! part of the "same board twice, byte-identical JSON" contract that the
//! scenario-08 journey checks.
//!
//! Each test runs the extraction repeatedly IN ONE PROCESS, which is what makes
//! it a real guard: `HashMap`'s hasher is seeded per map, not per process, so
//! two extractions in one test already walk their maps differently. A single
//! pass can agree by luck on a two-way tie, so both tests pin the expected
//! resolution as well as repeating it.

use std::path::PathBuf;

use hauksbee_extract::gerber::from_gerber_dir;

/// How many extractions each test compares. A two-way tie decided by chance
/// survives one pass half the time and eight passes once in 128.
const PASSES: usize = 8;

/// A one-layer film with `body`'s drawing commands: a 1 mm pad aperture (D10)
/// and a 0.25 mm conductor (D11).
fn film(body: &str) -> String {
    format!("%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.000000*%\n%ADD11C,0.250000*%\nD10*\n{body}M02*\n")
}

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hauksbee_gerber_order_{tag}_{}",
        std::process::id()
    ))
}

/// A four-layer job whose manifest declares BOTH inner films as `Copper,L2,Inr`.
///
/// A real exporter does this: KiCad 9 writes internal layer ids, so a manifest's
/// numbers are a rank rather than a position, and two films can arrive carrying
/// the same one. The rank still has to be decided, and the two candidates are
/// distinguishable: `a_inner.art` carries a routed track, `z_inner.art` carries
/// a lone pad. A blind L1-L2 drill reaches whichever film took stack index 1, so
/// the top pad's net picks up a track from one and nothing from the other.
fn write_tied_job(dir: &PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("create job dir");
    std::fs::write(dir.join("top.art"), film("%TO.N,VBUS*%\nX0Y0D03*\n%TD*%\n"))
        .expect("write top");
    std::fs::write(
        dir.join("a_inner.art"),
        film("X0Y0D03*\nD11*\nX0Y0D02*\nX10000000Y0D01*\nD10*\nX10000000Y0D03*\n"),
    )
    .expect("write a_inner");
    std::fs::write(dir.join("z_inner.art"), film("X0Y0D03*\n")).expect("write z_inner");
    std::fs::write(dir.join("bottom.art"), film("X20000000Y20000000D03*\n")).expect("write bottom");
    std::fs::write(
        dir.join("blind-drill.txt"),
        "M48\n; #@! TF.FileFunction,Plated,1,2,PTH,Blind\nFMAT,2\nMETRIC\nT1C0.300\n%\nG90\nG05\nT1\nX0.0Y0.0\nT0\nM30\n",
    )
    .expect("write drill");
    std::fs::write(
        dir.join("job.gbrjob"),
        r#"{
  "Header": {"GenerationSoftware": {"Vendor": "test"}},
  "FilesAttributes": [
    {"Path": "top.art", "FileFunction": "Copper,L1,Top", "FilePolarity": "Positive"},
    {"Path": "a_inner.art", "FileFunction": "Copper,L2,Inr", "FilePolarity": "Positive"},
    {"Path": "z_inner.art", "FileFunction": "Copper,L2,Inr", "FilePolarity": "Positive"},
    {"Path": "bottom.art", "FileFunction": "Copper,L4,Bot", "FilePolarity": "Positive"},
    {"Path": "blind-drill.txt", "FileFunction": "Plated,1,2,PTH,Blind"}
  ]
}"#,
    )
    .expect("write manifest");
}

/// Two films declaring the same layer must rank by file name, every run.
#[test]
fn a_gbrjob_layer_tie_is_broken_by_file_name_not_by_hash_order() {
    let dir = tmp("gbrjob_tie");
    write_tied_job(&dir);

    let mut seen = Vec::new();
    for _ in 0..PASSES {
        let g = from_gerber_dir(&dir).expect("extract");
        let vbus = g
            .board
            .nets
            .iter()
            .find(|n| n.name == "VBUS")
            .expect("the top pad names its net")
            .id;
        let tracks = g
            .stats
            .net_copper
            .iter()
            .find(|nc| nc.net_id == vbus)
            .map(|nc| nc.track_count)
            .unwrap_or(0);
        seen.push(tracks);
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        seen,
        vec![1; PASSES],
        "the blind L1-L2 barrel must reach a_inner.art (the film that sorts \
         first among the tied pair, and the one carrying the routed track) on \
         every run; got track counts {seen:?}"
    );
}

/// Two copper islands, each joining two pads the film assigns to DIFFERENT
/// nets, so each island is an X2 disagreement and each produces one note.
const CONFLICTING_X2: &str = "\
%TO.N,A1*%
X0Y0D03*
%TO.N,A2*%
X2000000Y0D03*
%TD*%
D11*
%TO.N,A1*%
X0Y0D02*
X2000000Y0D01*
%TD*%
D10*
%TO.N,B1*%
X10000000Y0D03*
%TO.N,B2*%
X12000000Y0D03*
%TD*%
D11*
%TO.N,B1*%
X10000000Y0D02*
X12000000Y0D01*
%TD*%
";

/// The X2 disagreement notes must come out in net order, every run.
#[test]
fn x2_disagreement_notes_come_out_in_net_order() {
    let dir = tmp("x2_notes");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create job dir");
    std::fs::write(dir.join("board-F_Cu.gbr"), film(CONFLICTING_X2)).expect("write film");

    let mut runs = Vec::new();
    for _ in 0..PASSES {
        let g = from_gerber_dir(&dir).expect("extract");
        let conflicts: Vec<String> = g
            .stats
            .notes
            .iter()
            .filter(|n| n.contains("different X2"))
            .cloned()
            .collect();
        runs.push(conflicts);
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        runs[0].len(),
        2,
        "both islands should be reported as X2 disagreements; got {:?}",
        runs[0]
    );
    for (pass, notes) in runs.iter().enumerate() {
        assert_eq!(
            notes,
            &runs[0],
            "the X2 disagreement notes differ between pass 1 and pass {}",
            pass + 1
        );
    }
    // And the order is the net order, not merely a stable arbitrary one: the
    // note names its net, so the ids it mentions must ascend.
    let ids: Vec<usize> = runs[0]
        .iter()
        .map(|n| {
            let tail = n.split("net NET_").nth(1).expect("the note names its net");
            tail.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .expect("a net id follows NET_")
        })
        .collect();
    let mut ascending = ids.clone();
    ascending.sort_unstable();
    assert_eq!(ids, ascending, "notes must be emitted in net-id order");
}
