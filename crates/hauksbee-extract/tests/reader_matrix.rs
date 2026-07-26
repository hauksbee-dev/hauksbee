//! Detection matrix for the board-reader registry (plan 06 §4 gate b).
//!
//! Proves two things across every board/netlist fixture committed to the repo,
//! plus a synthesised Eagle and Altium sample:
//!
//! 1. **No behaviour change.** The registry routes each file to the *same*
//!    format the legacy 512-char substring sniff did. [`legacy_format`]
//!    reproduces the old `from_auto` ladder verbatim as the oracle.
//! 2. **No false positives** (the trait's "must not false-positive" clause,
//!    tested pairwise): for every fixture *exactly one* reader claims it.

use hauksbee_extract::reader::Registry;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

/// The legacy `ExtractedBoard::from_auto` ladder, reproduced as the ground
/// truth of pre-registry behaviour. Returns the reader name each format routed
/// to; the trailing `ipc-d356` is the old universal fallback.
fn legacy_format(text: &str) -> &'static str {
    let head: String = text.chars().take(512).collect();
    if head.contains("<eagle") {
        "eagle"
    } else if head.trim_start().starts_with("(export") {
        "kicad-netlist"
    } else if head.contains("(kicad_sch") {
        "kicad-schematic"
    } else if head.contains("(kicad_pcb") {
        "kicad-pcb"
    } else {
        "ipc-d356"
    }
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Collect files with any of `exts` directly inside `dir` (non-recursive).
fn collect(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            if exts.contains(&ext) {
                out.push(p);
            }
        }
    }
}

/// A minimal but valid Eagle `.brd` (only the `<eagle` root matters here).
fn synth_eagle() -> Vec<u8> {
    b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<eagle version=\"6.6.0\"><drawing/></eagle>\n"
        .to_vec()
}

/// A minimal Altium `.PcbDoc`: an OLE2 container carrying a `Nets6` storage,
/// which is exactly what `looks_like_pcbdoc` keys on.
fn synth_altium() -> Vec<u8> {
    let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("create CFB");
    cf.create_storage("/Nets6").unwrap();
    let mut s = cf.create_stream("/Nets6/Data").unwrap();
    s.write_all(&[]).unwrap();
    drop(s);
    cf.into_inner().into_inner()
}

#[test]
fn detection_matrix_every_fixture() {
    let cd = crate_dir();
    // Fixture roots: testdata boards + netlists + d356, CI example boards,
    // frontend demo boards, and both crates' tests/fixtures dirs.
    let mut files: Vec<PathBuf> = Vec::new();
    collect(
        &cd.join("../../testdata/boards"),
        &["kicad_pcb"],
        &mut files,
    );
    collect(&cd.join("../../testdata"), &["net", "d356"], &mut files);
    collect(
        &cd.join("../../frontend/public/boards"),
        &["kicad_pcb"],
        &mut files,
    );
    collect(
        &cd.join("../../crates/hauksbee-ci/examples/boards"),
        &["kicad_pcb"],
        &mut files,
    );
    collect(
        &cd.join("tests/fixtures"),
        &["kicad_pcb", "kicad_sch"],
        &mut files,
    );
    collect(
        &cd.join("../../crates/hauksbee-engine/tests/fixtures"),
        &["kicad_pcb", "kicad_sch", "net"],
        &mut files,
    );
    files.sort();

    let reg = Registry::builtin();
    let names = reg.reader_names();

    // (label, bytes, path, expected reader). Text fixtures get their expectation
    // from the legacy ladder (the no-behaviour-change oracle); the synthesised
    // binary/eagle samples are labelled explicitly.
    struct Case {
        label: String,
        bytes: Vec<u8>,
        path: Option<PathBuf>,
        expected: &'static str,
    }
    let mut cases: Vec<Case> = Vec::new();

    for p in &files {
        let bytes = std::fs::read(p).expect("read fixture");
        let text = String::from_utf8_lossy(&bytes);
        let expected = legacy_format(&text);
        cases.push(Case {
            label: p
                .strip_prefix(&cd)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned(),
            bytes,
            path: Some(p.clone()),
            expected,
        });
    }
    cases.push(Case {
        label: "<synth> eagle.brd".into(),
        bytes: synth_eagle(),
        path: None,
        expected: "eagle",
    });
    cases.push(Case {
        label: "<synth> altium.PcbDoc".into(),
        bytes: synth_altium(),
        path: None,
        expected: "altium",
    });

    assert!(
        cases.len() >= 10,
        "expected a substantial fixture corpus, found {}",
        cases.len()
    );

    // The pairwise no-false-positive check needs the *full* set of readers that
    // claim each file, not just the registry's winner, so `probe_all` runs each
    // reader in isolation.
    let mut rows: Vec<(String, String, String)> = Vec::new(); // (label, expected, hits)
    let mut ok = true;
    for c in &cases {
        let hits = probe_all(&c.bytes, c.path.as_deref());
        let winner = reg
            .detect(&c.bytes, c.path.as_deref())
            .map(|r| r.name())
            .unwrap_or("<none>");
        let hits_str = hits.join("+");
        rows.push((c.label.clone(), c.expected.to_string(), hits_str.clone()));

        // Exactly one reader claims it (pairwise no-false-positive).
        if hits.len() != 1 {
            ok = false;
            eprintln!(
                "FALSE POSITIVE / no-claim: {} claimed by {:?} (expected only {})",
                c.label, hits, c.expected
            );
        }
        // And it is the expected one (no behaviour change vs the legacy sniff).
        if winner != c.expected {
            ok = false;
            eprintln!(
                "ROUTE MISMATCH: {} -> {} but legacy/expected was {}",
                c.label, winner, c.expected
            );
        }
    }

    // Print the matrix.
    eprintln!(
        "\nDetection matrix (readers in order: {}):",
        names.join(", ")
    );
    eprintln!(
        "{:<58} {:<16} {}",
        "fixture", "expected", "readers-that-claim"
    );
    eprintln!("{}", "-".repeat(96));
    let mut per_format: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (label, expected, hits) in &rows {
        eprintln!("{label:<58} {expected:<16} {hits}");
        *per_format.entry(expected.as_str()).or_default() += 1;
    }
    eprintln!("{}", "-".repeat(96));
    eprintln!(
        "Per-format counts: {per_format:?}  (total {} fixtures)",
        rows.len()
    );

    assert!(ok, "detection matrix failed; see stderr above");
}

/// Probe every builtin reader in isolation. Each is a zero-size unit struct, so
/// we construct them directly to get the full claim set per file (the registry
/// only surfaces the winner).
fn probe_all(bytes: &[u8], path: Option<&Path>) -> Vec<&'static str> {
    use hauksbee_extract::reader::{
        AltiumReader, BoardReader, EagleReader, Ipc356Reader, KicadNetlistReader, KicadPcbReader,
        KicadSchematicReader,
    };
    let readers: Vec<(&'static str, Box<dyn BoardReader>)> = vec![
        ("altium", Box::new(AltiumReader)),
        ("eagle", Box::new(EagleReader)),
        ("kicad-netlist", Box::new(KicadNetlistReader)),
        ("kicad-schematic", Box::new(KicadSchematicReader)),
        ("kicad-pcb", Box::new(KicadPcbReader)),
        ("ipc-d356", Box::new(Ipc356Reader)),
    ];
    readers
        .into_iter()
        .filter(|(_, r)| r.detects(bytes, path))
        .map(|(n, _)| n)
        .collect()
}

#[test]
fn unrecognized_error_enumerates_readers() {
    let reg = Registry::builtin();
    let err = reg
        .read(b"this is not a board file at all", None)
        .unwrap_err();
    let msg = err.to_string();
    // Every reader name appears in the failure.
    for name in reg.reader_names() {
        assert!(msg.contains(name), "error should name {name}: {msg}");
    }
    assert!(msg.contains("unrecognized"), "got: {msg}");
}

#[test]
fn third_party_reader_registers_and_wins() {
    use hauksbee_extract::reader::{BoardReader, ReadError};
    use hauksbee_extract::ExtractedBoard;

    struct ToyReader;
    impl BoardReader for ToyReader {
        fn name(&self) -> &str {
            "toy"
        }
        fn detects(&self, bytes: &[u8], _p: Option<&Path>) -> bool {
            bytes.starts_with(b"TOYBOARD")
        }
        fn read(&self, _b: &[u8], _p: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
            Ok(ExtractedBoard {
                name: "toy".into(),
                nets: vec![],
                components: vec![],
            })
        }
    }

    let mut reg = Registry::builtin();
    reg.register(Box::new(ToyReader));
    let board = reg
        .read(b"TOYBOARD v1", None)
        .expect("toy reader claims it");
    assert_eq!(board.name, "toy");
    // And a builtin format still routes correctly with the fork's reader present.
    assert_eq!(
        reg.detect(b"(kicad_pcb (version 20171130))", None)
            .map(|r| r.name()),
        Some("kicad-pcb")
    );
}
