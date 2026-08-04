//! Corpus sweep: run geometric DRC across every `.kicad_pcb` in board-corpus
//! and assert the known-good, shipped boards report zero TRUE shorts. Clearance
//! violations are expected on tightly-routed boards and are not asserted away.
//!
//! The sweep is skipped (not failed) when the corpus is absent, so the test is
//! safe in checkouts without the large board-corpus symlink.
//!
//! ## Documented corpus finding
//!
//! Earlier sweeps surfaced 2 "shorts" on several Olimex ESP32-EVB revisions
//! (REV-A..D, L) and were investigated: they were different-net pads placed
//! deliberately *abutting inside a single footprint* (a fuse-clip and a
//! capacitor footprint). KiCad does not treat intra-footprint pad copper as a
//! board short, so neither does the detector: pads sharing a footprint owner
//! are skipped. This is a real geometric fact handled by a principled rule, not
//! a per-board allowlist. With that rule the entire corpus is short-clean except
//! for one recorded contact, `SHORT_EXCEPTIONS` below, which carries its evidence
//! and an expiry.

use std::path::{Path, PathBuf};

use hauksbee_extract::ExtractedBoard;

/// Locate board-corpus relative to this crate, if present.
fn corpus_root() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")).unwrap_or_default();
    if p.exists() {
        Some(p)
    } else if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!(
            "HAUKSBEE_REQUIRE_CORPUS=1 but board-corpus is missing at {}",
            p.display()
        );
    } else {
        None
    }
}

/// Recursively collect every `.kicad_pcb` under `dir`.
fn find_boards(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            // The `hunt/` area holds un-reviewed, actively-probed boards (some
            // with genuine defects we are reporting upstream, e.g. the BMS
            // REG1_3V3<->GND short). They are deliberately not "known-good", so
            // they are excluded from this short-clean assertion.
            if path.file_name().and_then(|s| s.to_str()) == Some("hunt") {
                continue;
            }
            find_boards(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("kicad_pcb") {
            out.push(path);
        }
    }
}

/// A short this gate accepts, on the same terms `hauksbee-waivers.toml` accepts
/// one: a stated reason and a stated expiry, never a bare suppression.
struct ShortException {
    /// Board file name the finding must be on.
    board: &'static str,
    /// The two net names, in either order.
    nets: (&'static str, &'static str),
    /// Why the geometry is what it is, and why it does not indict the board.
    reason: &'static str,
    /// `YYYY-MM-DD`, after which this stops applying and the gate goes red.
    until: &'static str,
}

/// One entry, and it is a REAL zero-gap contact, not a measurement artefact.
///
/// Surfaced by adding the touching band to the shorts test (see
/// `hauksbee_extract::SHORT_TOUCH_EPS_MM`): the gap measures 9.769962616701378e-15
/// mm, so the old `gap <= 0.0` test filed it as a clearance note and the corpus
/// was called short-clean on the strength of a rounding error. The right response
/// is to record it, not to widen the test back until it disappears.
const SHORT_EXCEPTIONS: &[ShortException] = &[ShortException {
    board: "ESP32-EVB_Rev_",
    nets: ("Net-(400MA_E1-Pad1)", "GND"),
    reason: "A GND track meets pad 1 of 400MA_E1 at a measured 9.8e-15 mm on \
             B.Cu. 400MA_E1 is a CLOSED SOLDER JUMPER (footprint \
             OLIMEX_Jumpers-FP:SJ_Closed, value \"Closed\"): its pad 1 is on \
             Net-(400MA_E1-Pad1), its pad 2 is on GND, and a third bridging pad \
             joins them in copper. Those two nets are therefore connected BY \
             DESIGN, 0.76 mm away, and GND copper reaching the jumper's other \
             pad is the same connection arriving from the other side. The \
             geometry is real and the report is right about it; the board is not \
             faulty. Removing it properly needs the link-class recogniser \
             (hauksbee-engine's is_jumper_or_net_tie, already used to exempt \
             these parts from placeholder_value) to be reachable from the DRC, \
             which is a cross-crate change this exception holds the place for.",
    until: "2027-08-01",
}, ShortException {
    board: "vme-wren.kicad_pcb",
    nets: ("P3V3", "/FP_IO7"),
    reason: "TRUE POSITIVE, and it stays reported. A P3V3 via barrel meets a \
             /FP_IO7 track on In4.Cu at a measured 5.7e-15 mm: a via touching a \
             different net's inner-layer copper, which is a short by any reading. \
             It is excused here only because of what the file is. vme-wren is a \
             demonstration project inside KiCad's own source repository, not \
             manufactured hardware, so it is outside this gate's premise - the \
             premise is that SHIPPED, reviewed boards are short-clean, and a \
             finding on a demo neither confirms nor refutes that. The finding is \
             correct and is not being suppressed anywhere a user would see it.",
    until: "2027-08-01",
}];

impl ShortException {
    fn matches(&self, board_file: &str, a: &str, b: &str) -> bool {
        board_file.contains(self.board)
            && ((a == self.nets.0 && b == self.nets.1) || (a == self.nets.1 && b == self.nets.0))
    }
}

/// Days since 1970-01-01 for a `YYYY-MM-DD` string.
fn until_days(until: &str) -> Option<i64> {
    let mut it = until.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn today_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() as i64).div_euclid(86_400))
        .unwrap_or(0)
}

#[test]
fn corpus_boards_have_no_true_shorts() {
    let Some(root) = corpus_root() else {
        eprintln!("board-corpus not present; skipping corpus DRC sweep");
        return;
    };
    let mut boards = Vec::new();
    find_boards(&root, &mut boards);
    boards.sort();
    assert!(!boards.is_empty(), "found at least one corpus board");

    let today = today_days();
    let expiries: Vec<i64> = SHORT_EXCEPTIONS
        .iter()
        .map(|e| {
            until_days(e.until).unwrap_or_else(|| {
                panic!(
                    "short exception for {} has an unparseable `until` ({:?}); use YYYY-MM-DD",
                    e.board, e.until
                )
            })
        })
        .collect();
    let mut excused = vec![0usize; SHORT_EXCEPTIONS.len()];
    let mut scanned = 0usize;
    let mut skipped = 0usize;
    let mut total_clearance = 0usize;
    let mut total_prims = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for board in &boards {
        let Ok(text) = std::fs::read_to_string(board) else {
            continue;
        };
        let report = match ExtractedBoard::drc(&text) {
            Ok(r) => r,
            Err(_) => {
                // A handful of corpus boards are malformed at the s-expression
                // level (e.g. RoyalBlue54L-Feather has a `)`-jammed token and
                // an unbalanced paren) and forge-sexpr rejects them upstream of
                // the DRC. That is a parser/data issue, not a short, so skip it.
                skipped += 1;
                continue;
            }
        };
        scanned += 1;
        total_clearance += report.clearance_violations().count();
        total_prims += report.primitive_count;
        let file = board.file_name().unwrap().to_string_lossy().to_string();
        let mut unexcused: Vec<String> = Vec::new();
        for f in report.shorts() {
            match SHORT_EXCEPTIONS
                .iter()
                .position(|e| e.matches(&file, &f.net_a_name, &f.net_b_name))
            {
                Some(i) if today <= expiries[i] => {
                    excused[i] += 1;
                }
                Some(i) => unexcused.push(format!(
                    "{}<->{}@{} (exception EXPIRED {}: {})",
                    f.net_a_name,
                    f.net_b_name,
                    f.layer,
                    SHORT_EXCEPTIONS[i].until,
                    SHORT_EXCEPTIONS[i].reason
                )),
                None => unexcused.push(format!(
                    "{}<->{}@{} gap={:e}",
                    f.net_a_name, f.net_b_name, f.layer, f.gap_mm
                )),
            }
        }
        if !unexcused.is_empty() {
            offenders.push(format!(
                "{file}: {} short(s) [{}]",
                unexcused.len(),
                unexcused
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    eprintln!(
        "corpus DRC: scanned {scanned} board(s) ({skipped} skipped unparseable), \
         {total_prims} primitive(s), {total_clearance} clearance violation(s)"
    );
    hauksbee_testkit::scanned("corpus DRC short sweep", scanned);
    for (e, n) in SHORT_EXCEPTIONS.iter().zip(&excused) {
        eprintln!(
            "EXCEPTION  {} {:?}: absorbed {n} short(s), expires {}",
            e.board, e.nets, e.until
        );
    }
    assert!(
        scanned >= 40,
        "the bulk of the corpus parsed and was scanned"
    );

    assert!(
        offenders.is_empty(),
        "known-good corpus boards must report zero true shorts; offenders:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn hunt_sbc_a13_project_rules_resolve_netclasses_and_diff_pairs() {
    let Some(root) = corpus_root() else {
        eprintln!("board-corpus not present; skipping sbc-a13 project-rule regression");
        return;
    };
    let board_path = root.join("famous/hunt/sbc-a13/hardware/module.kicad_pcb");
    let pro_path = root.join("famous/hunt/sbc-a13/hardware/module.kicad_pro");
    if !board_path.exists() || !pro_path.exists() {
        // `hunt/` is the maintainers' actively-probed set, deliberately outside
        // corpus.toml (see the `find_boards` note below: some of these boards have
        // genuine defects being reported upstream). So `HAUKSBEE_REQUIRE_CORPUS`
        // cannot be what makes it mandatory: keyed off that, this test failed on
        // every runner that ran the documented public fetch, and corpus-gate.yml
        // had no green path. `HAUKSBEE_REQUIRE_HUNT_CORPUS=1` is the flag for a
        // maintainer who does have it.
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_HUNT_CORPUS").is_err(),
            "HAUKSBEE_REQUIRE_HUNT_CORPUS set but the sbc-a13 hunt board/project files are missing"
        );
        eprintln!(
            "NOT RUN  sbc-a13 project-rule regression: the hunt/ set is not in \
             corpus.toml's public fetch. Set HAUKSBEE_REQUIRE_HUNT_CORPUS=1 to \
             make this mandatory."
        );
        return;
    }
    let board_text = std::fs::read_to_string(&board_path).expect("read sbc-a13 board");
    let project_text = std::fs::read_to_string(&pro_path).expect("read sbc-a13 project");
    let board = ExtractedBoard::from_kicad_pcb(&board_text).expect("extract sbc-a13 board nets");
    let rules = hauksbee_extract::clearance_rules_from_kicad_pro(
        &project_text,
        board.nets.iter().map(|n| n.name.as_str()),
    )
    .expect("parse sbc-a13 project rules");

    assert!((rules.clearance_for_net("/DDR3 Memory/ddr-ck+") - 0.2).abs() < 1e-9);
    assert!((rules.clearance_for_net("+3V3") - 0.2).abs() < 1e-9);
    assert!((rules.clearance_for_net("USB0-D+") - 0.2).abs() < 1e-9);
    assert!(
        (rules.effective_clearance("/DDR3 Memory/ddr-ck+", "/DDR3 Memory/ddr-ck-") - 0.127).abs()
            < 1e-9
    );
}
