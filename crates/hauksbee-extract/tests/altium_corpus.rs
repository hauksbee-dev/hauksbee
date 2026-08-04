//! Corpus sweep + closed-loop cross-validation over the real Altium `.PcbDoc`
//! boards in `board-corpus/famous/altium` (cobra, qfsae, PiDP-11 IO expander,
//! HERON CubeSat OBC, ebaz4205 FPGA, the altium2kicad test boards).
//!
//! Two confidence layers:
//!
//! 1. **Extraction + DRC sanity**, every real board extracts a plausible
//!    number of nets / components / netted pins, and its geometric DRC reports
//!    ZERO true shorts (these are shipped or near-shipped designs). Clearance
//!    violations are expected on dense boards and reported, not asserted away.
//!
//! 2. **Cross-validation against KiCad's Altium importer (ground truth)**,
//!    KiCad 9 converts the same `.PcbDoc` to `.kicad_pcb` (committed under
//!    `kicad_xval/`). We extract that with hauksbee's KiCad path and compare the
//!    NET PARTITION over shared (refdes, pad) pins: two pins sharing a net in
//!    one extraction must share a net in the other (net names differ, so the
//!    partition, not the labels, is compared). The routable boards must agree at
//!    100%.
//!
//! Reported NOT RUN (not failed, and never a silent pass) when the family is
//! absent. It is absent on any machine that ran the public
//! `scripts/fetch-corpus.sh`, because these boards are not in corpus.toml. That
//! is what `HAUKSBEE_REQUIRE_ALTIUM_CORPUS=1` is for: a maintainer with the
//! private set turns absence into a failure, while the public nightly can still
//! reach green. Keying this off `HAUKSBEE_REQUIRE_CORPUS` instead made the
//! nightly permanently red.

use hauksbee_extract::ExtractedBoard;
use std::collections::HashMap;
use std::path::PathBuf;

fn corpus_root() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")).unwrap_or_default();
    p.exists().then_some(p)
}

/// Whether an absent Altium family should FAIL rather than report NOT RUN.
///
/// Deliberately NOT `HAUKSBEE_REQUIRE_CORPUS`. That flag means "the boards
/// corpus.toml promises must be present", and the Altium family is not in
/// corpus.toml: its sources live in the maintainers' private SOURCES.md (see
/// docs/ingest/ALTIUM.md). Keyed off the public flag, these two tests failed on
/// every runner that followed CONTRIBUTING and ran `scripts/fetch-corpus.sh`, so
/// `corpus-gate.yml` had no green path at all and the whole nightly stopped being
/// read. A gate that is always red is worth as little as one that is always
/// green.
///
/// A maintainer with the private set exports `HAUKSBEE_REQUIRE_ALTIUM_CORPUS=1`
/// and gets the hard failure back. Either way the absence is printed, never
/// silently passed.
fn require_altium_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_ALTIUM_CORPUS").is_ok()
}

/// (board file, min nets, min components) for each real binary board. The two
/// `test-*` boards from altium2kicad are edge-case fixtures, exercised for
/// "does not panic" only.
const FAMOUS_ALTIUM: &[(&str, usize, usize)] = &[
    ("cobra.PcbDoc", 15, 20),
    ("qfsae-devkit.PcbDoc", 18, 20),
    ("pidp11-io-expander.PcbDoc", 25, 20),
    ("heron-obc.PcbDoc", 55, 60),
    ("ebaz4205-fpga.PcbDoc", 300, 400),
    ("test-vias.PcbDoc", 5, 4),
    ("test-padshapes.PcbDoc", 0, 8),
    // Old Protel stream naming (no `6` suffix): a near-empty template, so the
    // bar is just "extracts without panicking".
    ("stm32f103-core.PcbDoc", 0, 1),
];

#[test]
fn famous_altium_boards_extract_and_are_short_clean() {
    let Some(root) = corpus_root() else {
        assert!(
            !require_altium_corpus(),
            "HAUKSBEE_REQUIRE_ALTIUM_CORPUS set but board-corpus is absent"
        );
        eprintln!("NOT RUN  board-corpus not present; famous Altium sweep");
        return;
    };
    // The family sits at `altium/` in either layout: the hand-built corpus puts
    // it under `famous/`, and `corpus_boards_root` resolves that level.
    let dir = hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or(root)
        .join("altium");

    let mut scanned = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    eprintln!(
        "{:<26} {:>5} {:>6} {:>6} {:>7} {:>7} {:>7}",
        "board", "nets", "comps", "pins", "netted", "shorts", "clrnce"
    );
    for (file, min_nets, min_comps) in FAMOUS_ALTIUM {
        let path = dir.join(file);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("{file:<26} (missing on disk, skipped)");
            continue;
        };
        let board = ExtractedBoard::from_altium_pcb(&bytes)
            .unwrap_or_else(|e| panic!("{file} extracts: {e}"));
        let report =
            ExtractedBoard::altium_drc(&bytes).unwrap_or_else(|e| panic!("{file} drc runs: {e}"));
        scanned += 1;

        let pins: usize = board.components.iter().map(|c| c.pins.len()).sum();
        let netted: usize = board
            .components
            .iter()
            .flat_map(|c| &c.pins)
            .filter(|p| p.net.is_some())
            .count();
        let shorts = report.short_count();
        eprintln!(
            "{:<26} {:>5} {:>6} {:>6} {:>7} {:>7} {:>7}",
            file,
            board.nets.len(),
            board.components.len(),
            pins,
            netted,
            shorts,
            report.clearance_violations().count(),
        );

        if board.nets.len() < *min_nets {
            offenders.push(format!("{file}: {} nets < {min_nets}", board.nets.len()));
        }
        if board.components.len() < *min_comps {
            offenders.push(format!(
                "{file}: {} comps < {min_comps}",
                board.components.len()
            ));
        }
        if shorts > 0 {
            let detail: Vec<String> = report
                .shorts()
                .take(4)
                .map(|f| {
                    format!(
                        "{}<->{}@{} gap{:.3}[{}/{}]",
                        f.net_a_name,
                        f.net_b_name,
                        f.layer,
                        f.gap_mm,
                        f.item_a.kind.as_str(),
                        f.item_b.kind.as_str()
                    )
                })
                .collect();
            offenders.push(format!("{file}: {shorts} short(s) [{}]", detail.join(", ")));
        }
    }

    if scanned == 0 {
        // board-corpus exists, but the Altium famous-board set is not in
        // corpus.toml yet (see docs/ingest/ALTIUM.md: sources are recorded in
        // the maintainers' private board-corpus/famous/SOURCES.md, not the
        // public fetch manifest). That is a different state from "no corpus
        // at all" and must not report as a clean pass, nor read as
        // "hauksbee is broken" to a contributor running the public fetch.
        let msg = "none of the famous Altium boards is present under \
                    <corpus>/altium (this family is not in corpus.toml's public \
                    fetch; see docs/ingest/ALTIUM.md). Set \
                    HAUKSBEE_REQUIRE_ALTIUM_CORPUS=1 to make this a failure.";
        assert!(
            !require_altium_corpus(),
            "HAUKSBEE_REQUIRE_ALTIUM_CORPUS set and {msg}"
        );
        eprintln!("NOT RUN  famous Altium sweep: {msg}");
        return;
    }
    hauksbee_testkit::scanned("famous Altium sweep", scanned);
    assert!(
        offenders.is_empty(),
        "real Altium boards must extract sanely and be short-clean; chase any \
         short to the binary before believing it (docs/ingest/ALTIUM.md). Offenders:\n  {}",
        offenders.join("\n  ")
    );
}

type PinKey = (String, String);

fn pin_to_net(b: &ExtractedBoard) -> HashMap<PinKey, i64> {
    let mut m = HashMap::new();
    for c in &b.components {
        for p in &c.pins {
            if let Some(n) = p.net {
                m.insert((c.reference.clone(), p.number.clone()), n);
            }
        }
    }
    m
}

/// Fraction (%) of shared-pin pairs whose same-net / different-net relation
/// agrees between the two extractions, plus the shared-pin count.
fn partition_agreement(a: &ExtractedBoard, k: &ExtractedBoard) -> (f64, usize) {
    let pa = pin_to_net(a);
    let pk = pin_to_net(k);
    let shared: Vec<&PinKey> = pa.keys().filter(|key| pk.contains_key(*key)).collect();
    let n = shared.len();
    let (mut agree, mut total) = (0u64, 0u64);
    for i in 0..n {
        for j in (i + 1)..n {
            let same_a = pa[shared[i]] == pa[shared[j]];
            let same_k = pk[shared[i]] == pk[shared[j]];
            if same_a == same_k {
                agree += 1;
            }
            total += 1;
        }
    }
    let pct = if total > 0 {
        100.0 * agree as f64 / total as f64
    } else {
        100.0
    };
    (pct, n)
}

/// Boards whose KiCad conversion is committed under `kicad_xval/`, with the
/// minimum shared-pin count we expect to actually compare (so a regression that
/// silently drops the join is caught).
const XVAL: &[(&str, usize)] = &[
    ("cobra", 90),
    ("qfsae-devkit", 55),
    ("pidp11-io-expander", 90),
    ("heron-obc", 240),
    ("test-vias", 15),
];

#[test]
fn cross_validate_against_kicad_altium_importer() {
    let Some(root) = corpus_root() else {
        assert!(
            !require_altium_corpus(),
            "HAUKSBEE_REQUIRE_ALTIUM_CORPUS set but board-corpus is absent"
        );
        eprintln!("NOT RUN  board-corpus not present; Altium cross-validation");
        return;
    };
    let dir = hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_else(|| root.clone())
        .join("altium");

    let mut compared = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    eprintln!("{:<22} {:>8} {:>14}", "board", "shared", "agreement");
    for (name, min_shared) in XVAL {
        let alt_path = dir.join(format!("{name}.PcbDoc"));
        // KiCad re-exports live outside `famous/` (under `board-corpus/
        // altium_xval/`) so the recursive corpus walkers that match
        // `*.kicad_pcb` do not mistake these machine conversions for curated
        // known-good reference boards.
        let kic_path = root.join(format!("altium_xval/{name}.kicad_pcb"));
        let (Ok(alt_bytes), Ok(kic_text)) =
            (std::fs::read(&alt_path), std::fs::read_to_string(&kic_path))
        else {
            eprintln!("{name:<22} (conversion or board missing, skipped)");
            continue;
        };
        let a = ExtractedBoard::from_altium_pcb(&alt_bytes).expect("altium extract");
        let k = ExtractedBoard::from_kicad_pcb(&kic_text).expect("kicad extract");
        let (pct, shared) = partition_agreement(&a, &k);
        compared += 1;
        eprintln!("{name:<22} {shared:>8} {pct:>13.3}%");

        if shared < *min_shared {
            offenders.push(format!(
                "{name}: only {shared} shared pins (< {min_shared})"
            ));
        }
        // The net partition is the electrical ground truth; it must agree
        // exactly. (Tiny floating-point-free integer comparison, so 100% is the
        // honest bar, not 99.x%.)
        if pct < 100.0 {
            offenders.push(format!("{name}: partition agreement {pct:.3}% < 100%"));
        }
    }

    if compared == 0 {
        // Same three-state reasoning as the sweep above: board-corpus exists
        // but this family is not in the public fetch manifest, which is not
        // the same as "no corpus" and must not read as a clean pass. It also
        // needs kicad-cli, so a machine with the boards and no KiCad lands
        // here too.
        let msg = "no famous Altium board could be cross-validated (the family is not in \
                   corpus.toml's public fetch, or kicad-cli is absent). Set \
                   HAUKSBEE_REQUIRE_ALTIUM_CORPUS=1 to make this a failure.";
        assert!(
            !require_altium_corpus(),
            "HAUKSBEE_REQUIRE_ALTIUM_CORPUS set and {msg}"
        );
        eprintln!("NOT RUN  the Altium cross-validation: {msg}");
        return;
    }
    hauksbee_testkit::scanned("Altium cross-validation", compared);
    assert!(
        offenders.is_empty(),
        "hauksbee's Altium extraction must agree with KiCad's independent Altium \
         importer on the net partition. Offenders:\n  {}",
        offenders.join("\n  ")
    );
}
