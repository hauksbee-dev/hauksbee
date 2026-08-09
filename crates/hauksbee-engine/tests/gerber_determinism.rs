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
//! Archives are built here the way `qc/unseen_boards.py::materialize_candidate`
//! builds them for the release gates: a flat zip of the sorted film names, so
//! the reader sees exactly the input the failing run saw.
//!
//! The contract is held on the COMMITTED four-layer fixture as well as on the
//! corpus Inkplate, so ordinary PR CI (which has no corpus) still runs it. The
//! corpus test is the real-board version and skips when the corpus is absent,
//! like its sibling gerber suites.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The Inkplate 6 gerber films, in whichever corpus layout this machine has.
fn inkplate_dir() -> Option<PathBuf> {
    hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), "famous/inkplate6_gerber")
}

/// The committed four-layer gerber job (copper F/In1/In2/B plus two
/// layer-paired drills) as a flat archive. Needs no corpus. The fixture
/// directory also holds the native layout the films were exported from; only
/// the films go in, so this is a fab archive and nothing else.
fn fixture_zip() -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-extract/tests/fixtures/gerber_advanced");
    flat_zip_of(&dir, |p| p.extension().is_none_or(|e| e != "kicad_pcb"))
}

/// A flat `.zip` of every file in `dir`, members in sorted order, deflated.
/// Mirrors the release gates' staging. `keep` selects the members, so a fixture
/// directory that also holds the native layout contributes only its films.
fn flat_zip_of(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<u8> {
    let mut members: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read gerber dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && keep(p))
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

/// Analyse `bytes` as `name` three times and return the one JSON export they
/// must all agree on.
///
/// Three passes, not two: hash-seeded iteration order and a clock-derived name
/// can both agree across a single pair by luck. The front door is the surface
/// the journey's "JSON export differs from an independent repeat analysis" check
/// exercises, so that is where the equality is asserted.
fn one_stable_export(name: &str, bytes: &[u8]) -> String {
    let first = hauksbee_engine::frontdoor::analyze_json(name, bytes);
    assert!(
        first.contains("\"ok\":true"),
        "the archive must analyse cleanly before its determinism means anything: {first}"
    );
    for pass in 2..=3 {
        let again = hauksbee_engine::frontdoor::analyze_json(name, bytes);
        assert_eq!(
            first, again,
            "gerber JSON export differs between pass 1 and pass {pass}"
        );
    }
    first
}

/// The contract on the committed fixture, so it holds in ordinary CI with no
/// corpus present.
///
/// A four-layer job with a `.gbrjob`-free name-inferred stack and two
/// layer-paired drills: enough that a film reordered between runs would move a
/// blind via's span and change the net table, not only the field order.
#[test]
fn gerber_zip_analysis_is_byte_identical_across_runs() {
    let json = one_stable_export("advanced_fab.zip", &fixture_zip());
    // The board name is what drifted, so pin that it now comes from the upload
    // rather than from a temp path: a bare inequality above would only say
    // "something moved", this says what.
    assert!(
        json.contains("\"board_name\":\"advanced_fab\""),
        "the board should be named after the uploaded archive: {}",
        &json[..json.len().min(400)]
    );
}

/// The same contract on the real board that failed the release gate.
#[test]
fn inkplate_gerber_zip_analysis_is_byte_identical_across_runs() {
    let Some(dir) = inkplate_dir() else {
        if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
            panic!("corpus required but inkplate6_gerber missing");
        }
        eprintln!("skipping Inkplate gerber determinism (corpus absent)");
        return;
    };

    let bytes = flat_zip_of(&dir, |_| true);
    let json = one_stable_export("inkplate6_gerber-0915a100.zip", &bytes);
    assert!(
        json.contains("\"board_name\":\"inkplate6_gerber-0915a100\""),
        "the board should be named after the uploaded archive: {}",
        &json[..json.len().min(400)]
    );
}

/// The same contract one layer down, on the extraction itself: the reverse
/// extraction of a gerber zip must not carry a clock reading into the board
/// name, and must reconstruct the same net table twice. Held separately so a
/// future front-door change cannot hide a regression in the reader.
#[test]
fn gerber_zip_extraction_names_the_board_from_the_archive() {
    let staging = tempfile::tempdir().expect("temp dir");
    let zip_path = staging.path().join("EPD_board_fab.zip");
    std::fs::write(&zip_path, fixture_zip()).expect("stage zip");

    let first = hauksbee_extract::ExtractedBoard::from_gerber(&zip_path).expect("extract");
    let second = hauksbee_extract::ExtractedBoard::from_gerber(&zip_path).expect("extract");
    assert_eq!(
        first.name, "EPD_board_fab",
        "the board takes its name from the archive, not the extraction directory"
    );
    assert_eq!(first.name, second.name, "board name must be stable");
    let net_names = |b: &hauksbee_extract::ExtractedBoard| {
        b.nets.iter().map(|n| n.name.clone()).collect::<Vec<_>>()
    };
    assert!(
        !net_names(&first).is_empty(),
        "the fixture reconstructs nets"
    );
    assert_eq!(
        net_names(&first),
        net_names(&second),
        "reconstructed net order must be stable across runs"
    );
}

/// A FAILING report is under the same contract as a passing one.
///
/// The reader's messages used to quote the film's full path, which on the web
/// path is inside a throwaway directory named from a pid, a counter and a clock
/// reading. So one unreadable archive analysed twice produced two different
/// error JSONs, and the message shipped a local absolute path to whoever read
/// the report. The film's own name is the part the user can act on.
#[test]
fn an_unreadable_archive_fails_the_same_way_twice() {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut out);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer
            .start_file("board-F_Cu.gbr", options)
            .expect("zip entry");
        // Not valid UTF-8, so the copper film cannot even be read as text.
        writer.write_all(&[0xff, 0xfe, 0x00, 0x9c]).expect("write");
        writer.finish().expect("zip finish");
    }
    let bytes = out.into_inner();

    let first = hauksbee_engine::frontdoor::analyze_json("broken_fab.zip", &bytes);
    let second = hauksbee_engine::frontdoor::analyze_json("broken_fab.zip", &bytes);
    assert!(
        first.contains("\"ok\":false"),
        "an unreadable film must fail: {first}"
    );
    assert_eq!(first, second, "the failing report must be stable too");
    assert!(
        first.contains("board-F_Cu.gbr"),
        "the message should name the film: {first}"
    );
    assert!(
        !first.contains("hauksbee_gerber_") && !first.contains("hauksbee-web-gerber"),
        "no staging path may appear in the message: {first}"
    );
}

/// The web path must name the board from the UPLOAD, not from wherever the
/// bytes were parked. Nothing about the staging path may reach the report, and
/// an upload name the filesystem would have to mangle must survive intact.
#[test]
fn uploaded_archive_name_reaches_the_report_unmangled() {
    let json = one_stable_export("rev A+B (final).zip", &fixture_zip());
    assert!(
        json.contains("\"board_name\":\"rev A+B (final)\""),
        "the upload's own name should be the board name: {}",
        &json[..json.len().min(400)]
    );
    assert!(
        !json.contains("hauksbee-web-gerber") && !json.contains("hauksbee_gerber_"),
        "no staging path may appear in the report: {json}"
    );
}
