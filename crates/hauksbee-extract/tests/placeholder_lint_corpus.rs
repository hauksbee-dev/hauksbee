//! Corpus silence gate for the placeholder-value lint.
//!
//! The check's contract is "a passive whose value was never set". Real boards
//! carry parts whose value is empty BY DESIGN - solder jumpers, solder bridges,
//! net ties - and several of them look passive by reference prefix (the Arduino
//! Uno's RESET-EN solder jumper starts with 'R'). Before the link-class
//! exemption, the lint fired a [medium] "set the actual R value" on exactly
//! that part, a confident false positive on one of the most-manufactured boards
//! in existence.
//!
//! This gate pins the calibration: across the ENTIRE known-good corpus, on
//! every extraction path (layout, netlist, Eagle board, schematic), the
//! placeholder-value lint produces no medium-or-high finding that is not a
//! recorded, dated exception. Almost all of these boards shipped with fully
//! specified BOMs, so almost any fire is a false positive by construction.
//! Almost: see `EXCEPTIONS` below, which names the one shipped board where the
//! finding is real, says why, and expires. A gate with no room for a true
//! positive is a gate that gets quietened by weakening the check. KiCad's own
//! demonstration projects are skipped entirely, for the reason given at the walk.
//!
//! Corpus-gated: skipped when board-corpus is absent, with
//! `HAUKSBEE_REQUIRE_CORPUS=1` (CI) turning absence into a hard failure so the
//! gate cannot vacuously green-out.

use std::path::{Path, PathBuf};

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};

/// A corpus finding this gate accepts, on the same terms `hauksbee-waivers.toml`
/// accepts one: a stated reason and a stated expiry, never a bare suppression.
///
/// The gate's premise is "every corpus board shipped with a fully specified BOM,
/// so any fire is a false positive". That premise is not true of every file in
/// the corpus, and the honest way to say so is an exception that names the board,
/// the parts, why the finding is RIGHT, and the date the exception stops being
/// taken on trust. Weakening the check to make the corpus quiet would trade a
/// real finding for a green tick.
struct Exception {
    /// Substring of the board path this applies to. Anchored on enough of the
    /// path to name one file, not a whole vendor directory.
    board: &'static str,
    /// The references the finding must be about. An exception that matches a
    /// finding on any other part does not apply.
    refs: &'static [&'static str],
    /// Why the finding stands rather than why it is dismissed.
    reason: &'static str,
    /// `YYYY-MM-DD`. Past this date the exception stops suppressing and the gate
    /// goes red, which is the point: an exception nobody revisits is a hole.
    until: &'static str,
}

/// The corpus is not uniformly fully specified, and this is the one place it is
/// not. Keep this list at the size of the evidence, never at the size of the
/// noise.
const EXCEPTIONS: &[Exception] = &[Exception {
    // The original Lily58 (`pcb/`) and the Pro (`Pro/PCB/`), on every extraction
    // path each ships. Not Pro_V2, whose R1 carries a real value ("50k"); the
    // reference filter below is what keeps this from reaching anything else.
    board: "lily58/",
    refs: &["R1", "R2"],
    reason: "TRUE POSITIVE, kept on purpose. R1 and R2 are the I2C pull-ups: \
             R1 sits between SDA and VCC, R2 between SCL and VCC (Lily58.net \
             nets 6, 7 and 46), and upstream's value for both is the literal \
             string \"R\" - the KiCad library symbol name, never replaced with a \
             resistance. The Pro revision repeats it. A pull-up with no value is \
             not a cosmetic BOM gap: the bus rise time the SI checks compute \
             depends on it, and a builder ordering from this BOM has nothing to \
             order. The check is right and is not being weakened; this records \
             that the corpus contains a shipped keyboard that genuinely is \
             under-specified.",
    until: "2027-08-01",
}];

impl Exception {
    fn matches(&self, path: &Path, message: &str) -> bool {
        let p = path.to_string_lossy().replace('\\', "/");
        p.contains(self.board) && self.refs.iter().any(|r| message_names(message, r))
    }

    /// Days since the epoch that this exception stops applying, or `None` if the
    /// date does not parse (which the test treats as a failure, not a pass).
    fn until_days(&self) -> Option<i64> {
        let mut it = self.until.split('-');
        let y: i64 = it.next()?.parse().ok()?;
        let m: i64 = it.next()?.parse().ok()?;
        let d: i64 = it.next()?.parse().ok()?;
        if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        Some(days_from_civil(y, m, d))
    }
}

/// A reference appears in a message as a whole token, so `R1` does not match
/// `R12`. The lint quotes references verbatim, surrounded by punctuation or
/// whitespace.
fn message_names(message: &str, reference: &str) -> bool {
    message
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| tok == reference)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
/// Chosen over a date crate because this is the only date arithmetic in the
/// suite and a dev-dependency for it would be heavier than the eight lines.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Today, in days since the epoch. Captured once so a long run cannot have an
/// exception lapse halfway through it.
fn today_days() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    secs.div_euclid(86_400)
}

/// The corpus root. The sweep recurses, so it covers both the hand-built
/// (`board-corpus/famous/<id>`) and fetch (`board-corpus/<id>`) layouts - and
/// hybrids - without caring which level the boards sit at.
fn boards_root() -> Option<PathBuf> {
    match hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(p) => Some(p),
        None => {
            if hauksbee_testkit::require_assets() {
                panic!("HAUKSBEE_REQUIRE_CORPUS set but board-corpus is missing");
            }
            eprintln!("board-corpus absent; skipping placeholder-lint corpus gate");
            None
        }
    }
}

#[test]
fn placeholder_value_is_silent_at_medium_and_above_across_corpus() {
    let Some(root) = boards_root() else { return };
    let today = today_days();
    // Every exception must carry a parseable expiry before it is allowed to
    // suppress anything. A malformed date is a hole with no closing time.
    let expiries: Vec<i64> = EXCEPTIONS
        .iter()
        .map(|e| {
            e.until_days().unwrap_or_else(|| {
                panic!(
                    "exception for {} has an unparseable `until` ({:?}); use YYYY-MM-DD",
                    e.board, e.until
                )
            })
        })
        .collect();
    let mut offenders: Vec<String> = Vec::new();
    // Parallel to EXCEPTIONS: how many findings each one absorbed. An exception
    // that absorbed nothing has outlived its finding and is reported stale, the
    // same discipline hauksbee applies to a waiver file.
    let mut excused = vec![0usize; EXCEPTIONS.len()];
    let mut exercised = 0usize;
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                // KiCad's own demonstration projects are out of scope for THIS
                // gate, and only this one. Their whole tree uses the bare library
                // symbol name as a value - `R` on RCAN201/RCAN202 in
                // kit-dev-coldfire-xilinx_5213, on R2 in custom_pads_test, on R1
                // in simulation/analog-multiplier - because they illustrate
                // features and are never ordered from. The lint is right about
                // every one of them, and grading a "shipped boards have complete
                // BOMs" gate on files that were never a BOM says nothing either
                // way. They stay in the geometric gates, where being a demo does
                // not change what the copper does.
                if p.file_name().and_then(|s| s.to_str()) == Some("kicad_demos")
                    || p.file_name().and_then(|s| s.to_str()) == Some("kicad-demos-src")
                {
                    continue;
                }
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            // Every extraction path the lint runs on. `.kicad_sch` goes through
            // the schematic extractor; the rest through format auto-detection.
            // Parse defensively: a file the extractor cannot read is a coverage
            // gap for a different test, not a placeholder-lint false positive.
            let board = match ext {
                "kicad_sch" => match ExtractedBoard::from_kicad_schematic_path(&p) {
                    Ok(b) => b,
                    Err(_) => continue,
                },
                "kicad_pcb" | "net" | "brd" => {
                    let Ok(text) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    match ExtractedBoard::from_auto(&text) {
                        Ok(b) => b,
                        Err(_) => continue,
                    }
                }
                _ => continue,
            };
            exercised += 1;
            for f in board.net_lint().of_check(LintCheck::PlaceholderValue) {
                if !matches!(f.severity, Severity::Medium | Severity::High) {
                    continue;
                }
                if let Some(i) = EXCEPTIONS.iter().position(|e| e.matches(&p, &f.message)) {
                    if today <= expiries[i] {
                        excused[i] += 1;
                        continue;
                    }
                    offenders.push(format!(
                        "{} [{}]: {}\n    (the exception for {} EXPIRED on {}: {})",
                        p.display(),
                        f.severity.as_str(),
                        f.message,
                        EXCEPTIONS[i].board,
                        EXCEPTIONS[i].until,
                        EXCEPTIONS[i].reason
                    ));
                    continue;
                }
                offenders.push(format!(
                    "{} [{}]: {}",
                    p.display(),
                    f.severity.as_str(),
                    f.message
                ));
            }
        }
    }
    // A walk that parsed nothing proves nothing; refuse the vacuous pass. The
    // tally is printed so a run's coverage is auditable, not inferred.
    hauksbee_testkit::scanned("placeholder_value corpus gate", exercised);
    for (e, n) in EXCEPTIONS.iter().zip(&excused) {
        eprintln!(
            "EXCEPTION  {} {:?}: absorbed {n} finding(s), expires {}",
            e.board, e.refs, e.until
        );
        // Only a complete corpus can prove an exception stale. A local run may
        // hold a subset (`fetch-corpus.sh --only ...`), where absorbing nothing
        // means the board is absent rather than the finding gone. Under
        // HAUKSBEE_REQUIRE_CORPUS the whole corpus is present by contract, so
        // an exception that absorbed nothing there really is dead weight.
        assert!(
            *n > 0 || !hauksbee_testkit::require_assets(),
            "the exception for {} {:?} absorbed nothing under a required corpus. \
             Either the finding it was written for is gone (delete the exception) \
             or the sweep no longer reaches that board (fix the sweep). A \
             suppression nobody can point at a finding is dead weight.",
            e.board,
            e.refs
        );
    }
    assert!(
        offenders.is_empty(),
        "placeholder_value fired at medium+ on known-good corpus boards (false positive(s)):\n{}",
        offenders.join("\n")
    );
}
