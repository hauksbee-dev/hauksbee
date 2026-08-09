//! The scenario-08 determinism contract on the gerber ingestion path: the same
//! fab archive analysed twice must produce byte-identical JSON.
//!
//! The corpus-exhaustive browser run caught this on
//! `board-corpus/inkplate6_gerber`, whose report carried
//! `board_name: "hauksbee_gerber_1786247903904093000"`: the zip reader named
//! the board after the throwaway extraction directory, whose name embedded a
//! nanosecond clock reading. Two analyses of one byte-identical upload
//! therefore disagreed in the exported JSON, which is the determinism check the
//! journey runs.
//!
//! The archive is built here the way `qc/unseen_boards.py::materialize_candidate`
//! builds it for the release gates: a flat zip of the sorted film names, so the
//! reader sees exactly the input the failing run saw. Skipped when the corpus is
//! absent; required under `HAUKSBEE_REQUIRE_CORPUS=1`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The Inkplate 6 gerber films, in whichever corpus layout this machine has.
fn inkplate_dir() -> Option<PathBuf> {
    hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), "famous/inkplate6_gerber")
}

/// A flat `.zip` of every file in `dir`, members in sorted order, deflated.
/// Mirrors the release gates' staging.
fn flat_zip(dir: &Path) -> Vec<u8> {
    let mut members: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read corpus gerber dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    members.sort();
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut out);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for member in &members {
            let name = member
                .file_name()
                .and_then(|s| s.to_str())
                .expect("film name is utf-8");
            writer.start_file(name, options).expect("zip entry");
            writer
                .write_all(&std::fs::read(member).expect("read film"))
                .expect("zip write");
        }
        writer.finish().expect("zip finish");
    }
    out.into_inner()
}

/// Two analyses of one gerber archive must export the same bytes.
///
/// Runs the archive through the web front door, which is the surface the
/// journey's "JSON export differs from an independent repeat analysis" check
/// exercises. Three passes, not two: a clock- or hash-seeded name can collide
/// across a single pair by luck.
#[test]
fn gerber_zip_analysis_is_byte_identical_across_runs() {
    let Some(dir) = inkplate_dir() else {
        if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
            panic!("corpus required but inkplate6_gerber missing");
        }
        eprintln!("skipping gerber determinism (corpus absent)");
        return;
    };

    let bytes = flat_zip(&dir);
    let name = "inkplate6_gerber-0915a100.zip";
    let first = hauksbee_engine::frontdoor::analyze_json(name, &bytes);
    for pass in 2..=3 {
        let again = hauksbee_engine::frontdoor::analyze_json(name, &bytes);
        assert_eq!(
            first, again,
            "gerber JSON export differs between pass 1 and pass {pass}"
        );
    }
    // The board name is what drifted, so pin that it is now derived from the
    // archive rather than from a temp path: a failure that only trips the
    // equality above reads as "something moved", this says what.
    assert!(
        first.contains("\"board_name\":\"inkplate6_gerber-0915a100\""),
        "the board should be named after the uploaded archive: {}",
        &first[..first.len().min(400)]
    );
}

/// The same contract one layer down, on the extraction itself: the reverse
/// extraction of a gerber zip must not carry a clock reading into the board
/// name. Held separately so a future front-door change cannot hide a
/// regression in the reader.
#[test]
fn gerber_zip_extraction_names_the_board_from_the_archive() {
    let Some(dir) = inkplate_dir() else {
        if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
            panic!("corpus required but inkplate6_gerber missing");
        }
        eprintln!("skipping gerber name determinism (corpus absent)");
        return;
    };

    let staging = tempfile::tempdir().expect("temp dir");
    let zip_path = staging.path().join("EPD_board_fab.zip");
    std::fs::write(&zip_path, flat_zip(&dir)).expect("stage zip");

    let first = hauksbee_extract::ExtractedBoard::from_gerber(&zip_path).expect("extract");
    let second = hauksbee_extract::ExtractedBoard::from_gerber(&zip_path).expect("extract");
    assert_eq!(
        first.name, "EPD_board_fab",
        "the board takes its name from the archive, not the extraction directory"
    );
    assert_eq!(first.name, second.name, "board name must be stable");
    assert_eq!(
        first
            .nets
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        second
            .nets
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        "reconstructed net order must be stable across runs"
    );
}
