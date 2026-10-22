//! Datasheet extraction must not send anything without being asked.
//!
//! The extractor ships someone's datasheet to an LLM backend. That is fine
//! when they asked for it and wrong every other time, and it cannot be undone
//! afterwards, so the refusal is worth a test that drives the real binary.

use crate::support::{hauksbee_bin as hauksbee, run};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A file that is a valid path but not a real datasheet. Nothing here should
/// ever read far enough to care.
fn stub_pdf(dir: &std::path::Path) -> PathBuf {
    let p = dir.join("stub.pdf");
    std::fs::write(&p, b"%PDF-1.4\n").unwrap();
    p
}

/// Attempt the extraction with stdin piped (not a terminal), so there is
/// nobody to ask for consent.
fn extract_with_piped_stdin(pdf: &std::path::Path) -> std::process::Output {
    Command::new(hauksbee())
        .args([
            "models", "extract", "--part", "BC847B", "--kind", "bjt_npn", "--pdf",
        ])
        .arg(pdf)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee")
}

#[test]
fn a_pipe_is_refused_rather_than_answered_for() {
    // Assuming yes would send a datasheet because someone ran the wrong
    // command in CI.
    let dir = tempfile::tempdir().unwrap();
    let pdf = stub_pdf(dir.path());
    let out = extract_with_piped_stdin(&pdf);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a pipe must not proceed:\n{err}");
    assert!(
        err.contains("without consent"),
        "and it must say why, not just fail:\n{err}"
    );
}

#[test]
fn the_notice_is_shown_before_anything_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = stub_pdf(dir.path());
    let out = extract_with_piped_stdin(&pdf);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Naming the destination is the whole point: "uses an LLM" is not consent.
    assert!(
        all.contains("sends the datasheet"),
        "the notice must state what leaves:\n{all}"
    );
    assert!(
        all.contains("datasheet-derived"),
        "and that the result is a labelled draft:\n{all}"
    );
}

#[test]
fn a_missing_datasheet_fails_before_the_consent_question() {
    // Asking someone to approve sending a file that is not there would train
    // them to click through the question.
    let out = run(&[
        "models",
        "extract",
        "--pdf",
        "/no/such/file.pdf",
        "--part",
        "X",
        "--yes",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("no datasheet at"), "{err}");
}

/// A board with two unmodelled active parts: one names its datasheet (a file
/// beside the board) and a part number the value does not spell, one names none.
fn unmodelled_board(dir: &std::path::Path) -> PathBuf {
    stub_pdf(dir);
    let pad = |n: u32, net: u32| {
        format!(
            "(pad \"{n}\" smd rect (at {n} 0) (size 1 1) (layers \"F.Cu\") (net {net} \"N{net}\"))"
        )
    };
    let part = |r: &str, props: &str, at: u32| {
        let pads: String = (1..=5).map(|n| pad(n, n)).collect();
        format!("(footprint \"Package_TO_SOT_SMD:SOT-23-5\" (layer \"F.Cu\") (at {at} 100) (property \"Reference\" \"{r}\") {props} {pads})")
    };
    let nets = (1..=5).fold(String::new(), |mut acc, n| {
        let _ = write!(acc, "(net {n} \"N{n}\")");
        acc
    });
    let u1 = part(
        "U1",
        r#"(property "Value" "OPAMP_XOPAMP1") (property "MPN" "XOPAMP1-TR") (property "Datasheet" "stub.pdf")"#,
        100,
    );
    let u2 = part(
        "U2",
        r#"(property "Value" "XNOSHEET7") (property "Datasheet" "~")"#,
        120,
    );
    let board = dir.join("board.kicad_pcb");
    std::fs::write(
        &board,
        format!("(kicad_pcb (version 20221018) (generator pcbnew) (net 0 \"\") {nets} {u1} {u2})"),
    )
    .unwrap();
    board
}

#[test]
fn unmodelled_batch_lists_each_part_and_its_datasheet_then_asks_once() {
    let dir = tempfile::tempdir().unwrap();
    let board = unmodelled_board(dir.path());
    let out = dir.path().join("drafts");
    let config = dir.path().join("extract.toml");
    std::fs::write(&config, "").unwrap();
    // The API backend cannot search the web, so the part with no link is named
    // with the hint; a backend that can search would look for it after consent.
    let run = Command::new(hauksbee())
        .args(["models", "extract", "--backend", "api", "--unmodelled"])
        .arg(&board)
        .arg("--out-dir")
        .arg(&out)
        .env("HAUKSBEE_EXTRACT_CONFIG", &config)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let (so, se) = (
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    assert!(so.contains("XOPAMP1-TR") && so.contains("stub.pdf"), "{so}");
    assert!(
        so.contains("no datasheet for XNOSHEET7")
            && so.contains("--datasheet 'XNOSHEET7=")
            && so.contains("agent backend that can search the web"),
        "{so}"
    );
    assert!(
        !run.status.success() && se.contains("without consent"),
        "{so}\n{se}"
    );
    assert!(!out.exists(), "nothing is written before consent");
}

/// The whole user flow, offline: a fake agent drafts the card, it is stamped
/// with the board value it was asked for, and the next resolve binds it. The
/// output directory exists but is empty, as on a first batch.
#[cfg(unix)]
#[test]
fn unmodelled_batch_drafts_a_card_the_next_run_binds() {
    let dir = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch(dir.path(), &[], "echo opamp");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.status.success(), "{all}");
    // U2 names no datasheet, so the (fake) agent backend was asked to search for
    // it; its reply holds no link, so the part has no datasheet (its own
    // category, not an extraction failure): the run still succeeds and the end
    // of the run lists it with the way to supply one.
    assert!(
        all.contains("XNOSHEET7: the board gives no datasheet; searching the web")
            && all.contains("no datasheet for XNOSHEET7: the web search found no usable datasheet")
            && all.contains("Batch: 1 saved, 1 for hand supply")
            && !all.contains("extractions failed"),
        "{all}"
    );
    assert!(
        all.contains("Supply these by hand")
            && all.contains("--datasheet-dir")
            && all.contains("XNOSHEET7.pdf"),
        "the list says how to supply it:\n{all}"
    );
    let saved = std::fs::read_to_string(out.join("supply-by-hand.txt")).unwrap();
    assert!(
        saved.contains("XNOSHEET7") && saved.contains("--datasheet-dir"),
        "{saved}"
    );
    assert!(
        !all.contains("no model cards"),
        "an empty --out-dir is no models dir yet:\n{all}"
    );
    assert!(
        all.contains("--models-dir"),
        "ends with the re-check command:\n{all}"
    );
    let drafted = std::fs::read_to_string(out.join("XOPAMP1-TR.toml")).unwrap();
    assert!(
        drafted.contains("OPAMP[-_ ]?XOPAMP1"),
        "the board value binds:\n{drafted}"
    );
    let resolve = run_resolve(&dir.path().join("board.kicad_pcb"), &out);
    assert!(resolve.contains("xopamp1"), "{resolve}");
}

/// `--kind PART=KIND` sets one part's kind, so the backend is never asked to
/// identify it; a kind the schema does not know stops the batch before consent.
#[cfg(unix)]
#[test]
fn a_per_part_kind_skips_identification_and_a_bad_one_stops_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let kind = ["--kind", "XOPAMP1-TR=opamp"];
    let (run, out) = draft_batch(dir.path(), &kind, "exit 3");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.status.success(), "{all}");
    assert!(out.join("XOPAMP1-TR.toml").is_file(), "{all}");
    let bad = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch(bad.path(), &["--kind", "XOPAMP1-TR=widget"], "echo opamp");
    let err = String::from_utf8_lossy(&run.stderr);
    assert!(
        !run.status.success() && err.contains("unknown kind 'widget'"),
        "{err}"
    );
    assert!(!out.join("XOPAMP1-TR.toml").exists());
}

/// Run `models extract --unmodelled` on [`unmodelled_board`] with a fake agent
/// that answers the kind question with `on_kind` and drafts the card.
#[cfg(unix)]
fn draft_batch(
    dir: &std::path::Path,
    extra: &[&str],
    on_kind: &str,
) -> (std::process::Output, PathBuf) {
    draft_batch_with(dir, extra, on_kind, "cp CARD model.toml; echo done")
}

/// [`draft_batch`] with the drafting step's shell too (`CARD` is the fake card).
#[cfg(unix)]
fn draft_batch_with(
    dir: &std::path::Path,
    extra: &[&str],
    on_kind: &str,
    on_draft: &str,
) -> (std::process::Output, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let board = unmodelled_board(dir);
    let card = dir.join("card.toml");
    std::fs::write(
        &card,
        r#"[[models]]
id = "xopamp1"
kind = "opamp"
description = "fake draft; models bounded DC gain and output rails, but does not model noise"
[models.source]
tier = "datasheet-derived"
validation = "physical-bounds-only"
[[models.source.uncertainty]]
status = "unknown"
parameter = "model"
reason = "datasheet publishes no validated model error interval"
[[models.source.pinout]]
package = "SOT-23-5"
pins = { "1" = "OUT", "2" = "V-", "3" = "IN+", "4" = "IN-", "5" = "V+" }
[models.match]
value_re = "(?i)^XOPAMP1-TR$"
[models.params]
gain = 100000.0 # Source: Electrical Characteristics, AOL typ
rail_lo = 0.05 # Source: Electrical Characteristics
rail_hi = 4.95 # Source: Electrical Characteristics
[models.ratings]
max_voltage_v = 6.0 # Source: Absolute Maximum Ratings
[models.pins]
"1" = "out"
"2" = "vee"
"3" = "in_plus"
"4" = "in_minus"
"5" = "vcc"
"#,
    )
    .unwrap();
    let agent = dir.join("agent");
    let script = format!("#!/bin/sh\ncat >/dev/null\nif grep -q 'EXACTLY ONE identifier' prompt.md; then {on_kind}; else {}; fi\n", on_draft.replace("CARD", &format!("'{}'", card.display())));
    std::fs::write(&agent, script).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.join("extract.toml");
    std::fs::write(&config, "").unwrap();
    let out = dir.join("drafts");
    std::fs::create_dir_all(&out).unwrap();
    let run = Command::new(hauksbee())
        .args([
            "models",
            "extract",
            "--backend",
            "claude-code",
            "--yes",
            "--unmodelled",
        ])
        .arg(&board)
        .arg("--out-dir")
        .arg(&out)
        .args(extra)
        .env("HAUKSBEE_EXTRACT_CONFIG", &config)
        .env("HAUKSBEE_CLAUDE_COMMAND", &agent)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    (run, out)
}

#[cfg(unix)]
fn run_resolve(board: &std::path::Path, models: &std::path::Path) -> String {
    let out = Command::new(hauksbee())
        .args(["models", "resolve"])
        .arg(board)
        .arg("--models-dir")
        .arg(models)
        .output()
        .expect("run hauksbee");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A board with two unmodelled op-amps: U1's `Datasheet` link names another
/// device (an OPA2336 sheet on an OPA379-class part), U2's names its own.
fn wrong_link_board(dir: &std::path::Path) -> PathBuf {
    let pad = |n: u32| {
        format!("(pad \"{n}\" smd rect (at {n} 0) (size 1 1) (layers \"F.Cu\") (net {n} \"N{n}\"))")
    };
    let part = |r: &str, value: &str, link: &str, at: u32| {
        let pads: String = (1..=5).map(pad).collect();
        format!(
            "(footprint \"Package_TO_SOT_SMD:SOT-23-5\" (layer \"F.Cu\") (at {at} 100) \
             (property \"Reference\" \"{r}\") (property \"Value\" \"{value}\") \
             (property \"Datasheet\" \"{link}\") {pads})"
        )
    };
    let nets = (1..=5).fold(String::new(), |mut acc, n| {
        let _ = write!(acc, "(net {n} \"N{n}\")");
        acc
    });
    let u1 = part(
        "U1",
        "XOPAMP379-TR",
        "https://example.com/ds/opa2336.pdf",
        100,
    );
    let u2 = part(
        "U2",
        "XCOMP555-TR",
        "https://example.com/ds/xcomp555.pdf",
        120,
    );
    let board = dir.join("wrong_link.kicad_pcb");
    std::fs::write(
        &board,
        format!("(kicad_pcb (version 20221018) (generator pcbnew) (net 0 \"\") {nets} {u1} {u2})"),
    )
    .unwrap();
    board
}

/// The datasheet a board names for a part but belongs to another device is
/// said before anything is sent, naming the flag that fixes it; a link that
/// names the part's own sheet is not mentioned.
#[test]
fn a_datasheet_link_naming_another_part_is_flagged_before_anything_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let board = wrong_link_board(dir.path());
    let run = Command::new(hauksbee())
        .args(["models", "extract", "--unmodelled"])
        .arg(&board)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let so = String::from_utf8_lossy(&run.stdout);
    assert!(
        so.contains(
            "the board's datasheet for XOPAMP379-TR appears to be for opa2336; \
             pass --datasheet 'XOPAMP379-TR=<correct pdf>' or confirm"
        ),
        "{so}"
    );
    assert!(
        !so.contains("datasheet for XCOMP555-TR appears"),
        "a right link stays silent:\n{so}"
    );
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("without consent"),
        "and nothing is sent unasked"
    );
}

/// The same wrong link is a low-severity note in the lint, not a gate.
#[test]
fn the_lint_notes_a_datasheet_link_naming_another_part() {
    let dir = tempfile::tempdir().unwrap();
    let board = wrong_link_board(dir.path());
    let out = run(&["run", board.to_str().unwrap(), "--lint", "--json"]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(so.contains("datasheet_mismatch"), "{so}");
    assert!(
        so.contains("the board's datasheet for XOPAMP379-TR appears to be for opa2336"),
        "{so}"
    );
    assert!(!so.contains("datasheet for XCOMP555-TR"), "{so}");
}

/// A discrete transistor the verdict counts as having no usable model is
/// offered for extraction like an IC, and the list's count reconciles with the
/// verdict's. (`verdict_fet_unbound` is the board whose power FET Q1 has none.)
#[test]
fn a_discrete_mosfet_the_verdict_counts_is_offered_for_extraction() {
    let board = crate::support::board("tests/fixtures/verdict_fet_unbound.kicad_pcb");
    let check = run(&["run", board.to_str().unwrap(), "--lint", "--plain"]);
    let said = String::from_utf8_lossy(&check.stdout);
    assert!(said.contains("Q1"), "the verdict names Q1:\n{said}");
    let run = Command::new(hauksbee())
        .args(["models", "extract", "--unmodelled"])
        .arg(&board)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let so = String::from_utf8_lossy(&run.stdout);
    assert!(so.contains("XFET9999") && so.contains("Q1"), "{so}");
    assert!(
        so.contains("The check verdict counts 1 key part(s) with no usable model: 1 offered above"),
        "{so}"
    );
}

/// A part whose card is already in the target library is reported with the
/// card's own reason when it is an identity-only abstention, nothing is sent for
/// it, and it is not a failure (exit 0).
#[cfg(unix)]
#[test]
fn an_identity_only_card_already_in_the_library_is_reported_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let board = unmodelled_board(dir.path());
    let out = dir.path().join("drafts");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        out.join("XOPAMP1-TR.toml"),
        "[[models]]\nid = \"xopamp1\"\nkind = \"digital\"\n[models.params]\n\
         identity_only = true\nwarning = \"Pin 3 function is not stated.\"\n\
         unlocked_by = \"state pin 3\"\n",
    )
    .unwrap();
    let config = dir.path().join("extract.toml");
    std::fs::write(&config, "").unwrap();
    let run = Command::new(hauksbee())
        .args([
            "models",
            "extract",
            "--backend",
            "api",
            "--yes",
            "--unmodelled",
        ])
        .arg(&board)
        .arg("--out-dir")
        .arg(&out)
        .env("HAUKSBEE_EXTRACT_CONFIG", &config)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        all.contains(
            "XOPAMP1-TR: has a card that cannot simulate it yet (identity-only): Pin 3 function is not stated; unlocked by: state pin 3; open it in the model editor or re-extract with --force"
        ),
        "{all}"
    );
    assert!(
        all.contains("Batch: 0 saved, 1 already had a card") && !all.contains("extractions failed"),
        "{all}"
    );
    assert!(run.status.success(), "a kept card is not a failure:\n{all}");
}

#[cfg(unix)]
fn text_of(run: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    )
}

/// The exit-status contract is in `--help` where a person looks for it, and 4
/// is named as the partial success it is.
#[test]
fn extract_help_lists_every_exit_status_and_names_4_as_partial_success() {
    let out = Command::new(hauksbee())
        .args(["models", "extract", "--help"])
        .output()
        .expect("run hauksbee");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Exit status of --unmodelled")
            && help.contains("4  PARTIAL SUCCESS, not a total failure")
            && help.contains("--retry-failed"),
        "{help}"
    );
    let top = Command::new(hauksbee())
        .arg("--help")
        .output()
        .expect("run hauksbee");
    assert!(
        String::from_utf8_lossy(&top.stdout).contains("PARTIAL SUCCESS"),
        "the top-level exit codes point at 4"
    );
}

/// A real extraction failure (the agent writes no card) fails the run, and is
/// kept apart from the part with no datasheet in the same batch.
#[cfg(unix)]
#[test]
fn a_failed_extraction_exits_non_zero_and_is_not_counted_as_no_datasheet() {
    let dir = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch_with(dir.path(), &[], "echo opamp", "echo nothing written");
    let all = text_of(&run);
    assert_eq!(
        run.status.code(),
        Some(1),
        "something failed and nothing was saved: exit 1\n{all}"
    );
    assert!(
        all.contains("1 of the extractions failed: XOPAMP1-TR")
            && all.contains("Batch: 0 saved, 1 for hand supply, 1 failed (XOPAMP1-TR)")
            && all.contains("Exit status 1: 1 part(s) failed and none was saved")
            && all.contains("error: 1 of the extractions failed"),
        "{all}"
    );
    let saved = std::fs::read_to_string(out.join("supply-by-hand.txt")).unwrap();
    assert!(
        saved.contains("XNOSHEET7") && !saved.contains("XOPAMP1-TR"),
        "{saved}"
    );
}

/// Some parts saved and some failed is its own exit status (4), not the 1 of a
/// batch where nothing could be done; the tally uses the same three words.
#[cfg(unix)]
#[test]
fn a_partly_saved_batch_exits_4_and_says_saved_for_hand_supply_and_failed() {
    let dir = tempfile::tempdir().unwrap();
    // The fake agent's one card fits only XOPAMP1-TR. XNOSHEET7 is given a
    // supplied PDF, so it is extracted, and refused by validation.
    let supply = dir.path().join("pdfs");
    std::fs::create_dir_all(&supply).unwrap();
    std::fs::copy(stub_pdf(dir.path()), supply.join("xnosheet7.pdf")).unwrap();
    let dir_arg = supply.display().to_string();
    let (run, out) = draft_batch(dir.path(), &["--datasheet-dir", &dir_arg], "echo opamp");
    let all = text_of(&run);
    assert_eq!(run.status.code(), Some(4), "{all}");
    assert!(
        all.contains("Batch: 1 saved, 1 failed (XNOSHEET7)")
            && all
                .contains("1 of the extractions failed: XNOSHEET7 (1 others were saved and kept)"),
        "{all}"
    );
    // Exit 4 is worded as the partial success it is, where the person reads it.
    assert!(
        all.contains("Exit status 4 = partial success, not a total failure")
            && all.contains("partial success: 1 of the extractions failed")
            && !all.contains("error: 1 of the extractions failed"),
        "{all}"
    );
    assert!(out.join("XOPAMP1-TR.toml").is_file(), "{all}");
}

/// Hand supply is not a failure: a batch whose only misses are parts without a
/// datasheet exits 0 (and says so in the same words).
#[cfg(unix)]
#[test]
fn a_batch_with_only_hand_supply_misses_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = draft_batch(dir.path(), &[], "echo opamp");
    let all = text_of(&run);
    assert_eq!(run.status.code(), Some(0), "{all}");
    assert!(all.contains("Batch: 1 saved, 1 for hand supply"), "{all}");
}

/// A part that failed is not sent to the model again on the next board with the
/// same inputs; `--retry-failed` sends it; a part whose search found nothing is
/// not searched again but stays on the hand-supply list.
#[cfg(unix)]
#[test]
fn a_failed_part_is_not_sent_again_unless_asked_or_its_inputs_change() {
    let dir = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch_with(dir.path(), &[], "echo opamp", "echo nothing written");
    let all = text_of(&run);
    assert_eq!(run.status.code(), Some(1), "{all}");
    let ledger = std::fs::read_to_string(out.join("extract-attempts.json")).unwrap();
    assert!(
        ledger.contains("XOPAMP1-TR") && ledger.contains("XNOSHEET7"),
        "{ledger}"
    );

    // The next board in the same library: the same two parts, nothing sent.
    let (run, out) = draft_batch_with(dir.path(), &[], "echo opamp", "echo nothing written");
    let all = text_of(&run);
    assert_eq!(run.status.code(), Some(0), "skips are not failures:\n{all}");
    assert!(
        all.contains("XOPAMP1-TR: skipped, it failed earlier today")
            && all.contains("--retry-failed")
            && all.contains("XNOSHEET7: not searching again")
            && all.contains(
                "Batch: 0 saved, 1 for hand supply, 1 skipped (failed before; --retry-failed tries them again)"
            ),
        "{all}"
    );
    assert!(
        !all.contains("searching the web") && !all.contains("[model-extract]"),
        "nothing was sent:\n{all}"
    );
    let listed = std::fs::read_to_string(out.join("supply-by-hand.txt")).unwrap();
    assert!(
        listed.contains("XNOSHEET7"),
        "still listed for hand supply:\n{listed}"
    );

    // A different model is a different input: tried again by itself.
    let (run, _) = draft_batch_with(
        dir.path(),
        &["--model", "another-model"],
        "echo opamp",
        "cp CARD model.toml; echo done",
    );
    let all = text_of(&run);
    assert!(
        !all.contains("skipped, it failed") && all.contains("XOPAMP1-TR: drafted"),
        "{all}"
    );
    assert_eq!(run.status.code(), Some(0), "{all}");
}

/// `--retry-failed` lifts the skip without any change of input.
#[cfg(unix)]
#[test]
fn retry_failed_sends_the_remembered_part_again() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = draft_batch_with(dir.path(), &[], "echo opamp", "echo nothing written");
    assert_eq!(run.status.code(), Some(1), "{}", text_of(&run));
    let (run, out) = draft_batch(dir.path(), &["--retry-failed"], "echo opamp");
    let all = text_of(&run);
    assert!(
        !all.contains("skipped, it failed") && out.join("XOPAMP1-TR.toml").is_file(),
        "{all}"
    );
    let ledger = std::fs::read_to_string(out.join("extract-attempts.json")).unwrap();
    assert!(
        !ledger.contains("XOPAMP1-TR"),
        "a part that is saved is forgotten:\n{ledger}"
    );
}

/// A board link that cannot be fetched is labelled exactly like a part with no
/// link: "no datasheet for", on the end-of-run list with the link tried, and
/// the run still exits 0.
#[cfg(unix)]
#[test]
fn a_dead_board_link_is_labelled_like_no_link_and_lists_the_link_tried() {
    let dir = tempfile::tempdir().unwrap();
    let board = unmodelled_board(dir.path());
    // Point XNOSHEET7 at a URL nothing listens on.
    // U1 gets no link either, so no extraction is attempted (the API backend
    // here has no pdftotext to rely on); it is the no-link case beside the dead link.
    let text = std::fs::read_to_string(&board)
        .unwrap()
        .replace(
            r#"(property "Value" "XNOSHEET7") (property "Datasheet" "~")"#,
            r#"(property "Value" "XNOSHEET7") (property "Datasheet" "http://127.0.0.1:9/x.pdf")"#,
        )
        .replace(
            r#"(property "Datasheet" "stub.pdf")"#,
            r#"(property "Datasheet" "~")"#,
        );
    std::fs::write(&board, text).unwrap();
    let config = dir.path().join("extract.toml");
    std::fs::write(&config, "").unwrap();
    let out = dir.path().join("drafts");
    let run = Command::new(hauksbee())
        .args([
            "models",
            "extract",
            "--backend",
            "api",
            "--yes",
            "--unmodelled",
        ])
        .arg(&board)
        .arg("--out-dir")
        .arg(&out)
        .env("HAUKSBEE_EXTRACT_CONFIG", &config)
        .env("HAUKSBEE_API_KEY", "unused")
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let all = text_of(&run);
    assert!(
        all.contains("no datasheet for XNOSHEET7") && all.contains("http://127.0.0.1:9/x.pdf"),
        "{all}"
    );
    assert!(
        all.contains("no datasheet for XOPAMP1-TR"),
        "the no-link part reads the same:\n{all}"
    );
    assert!(
        run.status.success(),
        "no datasheet is not a failure:\n{all}"
    );
    assert!(!all.contains("extractions failed"), "{all}");
    let saved = std::fs::read_to_string(out.join("supply-by-hand.txt")).unwrap();
    assert!(saved.contains("tried http://127.0.0.1:9/x.pdf"), "{saved}");
    assert!(saved.contains("XOPAMP1-TR"), "{saved}");
}

/// The saved list names the folder the PDFs go in; putting `<part>.pdf` there
/// and rerunning with `--datasheet-dir` picks it up, and the stale list goes.
#[cfg(unix)]
#[test]
fn a_supplied_datasheet_is_picked_up_on_rerun_and_clears_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch(dir.path(), &[], "echo opamp");
    assert!(run.status.success(), "{}", text_of(&run));
    assert!(out.join("supply-by-hand.txt").is_file());
    let supply = dir.path().join("pdfs");
    std::fs::create_dir_all(&supply).unwrap();
    std::fs::copy(dir.path().join("stub.pdf"), supply.join("xnosheet7.pdf")).unwrap();
    let dir_arg = supply.display().to_string();
    let (run, out) = draft_batch(
        dir.path(),
        &["--datasheet-dir", &dir_arg, "--force"],
        "echo opamp",
    );
    let all = text_of(&run);
    // The fake agent's one card only fits XOPAMP1-TR, so XNOSHEET7's extraction
    // is refused by validation: that is a real extraction failure (non-zero),
    // and it proves the supplied PDF reached the extractor.
    assert!(
        all.contains("1 of the extractions failed: XNOSHEET7"),
        "{all}"
    );
    assert!(
        all.contains("using the datasheet you supplied") && all.contains("xnosheet7.pdf"),
        "{all}"
    );
    assert!(
        !all.contains("no datasheet for XNOSHEET7"),
        "it is no longer missing:\n{all}"
    );
    assert!(!out.join("supply-by-hand.txt").exists(), "{all}");
}

/// An agent question nobody can answer in a `--yes` run is not lost: it is on
/// the end-of-run list as "needs:" with how to supply it.
#[cfg(unix)]
#[test]
fn an_unanswerable_agent_question_is_listed_as_a_need() {
    let dir = tempfile::tempdir().unwrap();
    let envelope = r#"{"status":"drafted","confidence":"medium","reason":"r","questions":["which package variant is fitted?"]}"#;
    let (run, out) = draft_batch_with(
        dir.path(),
        &[],
        "echo opamp",
        &format!("cp CARD model.toml; echo '{envelope}'"),
    );
    let all = text_of(&run);
    assert!(run.status.success(), "{all}");
    assert!(
        all.contains("XOPAMP1-TR: needs: which package variant is fitted?")
            && all.contains("hauksbee models extract --part XOPAMP1-TR --pdf"),
        "{all}"
    );
    let saved = std::fs::read_to_string(out.join("supply-by-hand.txt")).unwrap();
    assert!(
        saved.contains("needs: which package variant is fitted?"),
        "{saved}"
    );
}

/// `--jobs` is bounded, and `--jobs 1` is the serial loop (same results).
#[cfg(unix)]
#[test]
fn jobs_one_is_the_serial_loop_and_the_bound_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch(dir.path(), &["--jobs", "1"], "echo opamp");
    let all = text_of(&run);
    assert!(run.status.success(), "{all}");
    assert!(out.join("XOPAMP1-TR.toml").is_file(), "{all}");
    assert!(
        !all.contains("[XOPAMP1-TR] "),
        "no prefixes when serial:\n{all}"
    );
    let bad = tempfile::tempdir().unwrap();
    let (run, _) = draft_batch(bad.path(), &["--jobs", "99"], "echo opamp");
    assert!(!run.status.success());
    assert!(text_of(&run).contains("99"), "{}", text_of(&run));
}

/// Several at once: the same drafted card, lines prefixed by part.
#[cfg(unix)]
#[test]
fn jobs_four_drafts_the_same_cards_with_prefixed_lines() {
    let dir = tempfile::tempdir().unwrap();
    let (run, out) = draft_batch(dir.path(), &["--jobs", "4"], "echo opamp");
    let all = text_of(&run);
    assert!(run.status.success(), "{all}");
    assert!(out.join("XOPAMP1-TR.toml").is_file(), "{all}");
    assert!(all.contains("Batch: 1 saved, 1 for hand supply"), "{all}");
    assert!(
        all.lines()
            .filter(|l| l.contains("searching the web"))
            .all(|l| l.contains("XNOSHEET7")),
        "{all}"
    );
}

/// Run `models extract --unmodelled` with a fake codex that is signed in with a
/// plan, reports `session` and `week` percent used, and cannot extract (so the
/// run's plan lines are what is under test, not its drafts).
#[cfg(unix)]
fn plan_batch(
    dir: &std::path::Path,
    session: u32,
    week: u32,
    extra: &[&str],
) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    let board = unmodelled_board(dir);
    let agent = dir.join("codex");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  login) echo 'Logged in using ChatGPT'; exit 0;;\n  app-server) while read line; do case \"$line\" in *account/rateLimits/read*) echo '{{\"id\":2,\"result\":{{\"rateLimits\":{{\"primary\":{{\"usedPercent\":{session},\"windowDurationMins\":300}},\"secondary\":{{\"usedPercent\":{week},\"windowDurationMins\":10080}}}}}}}}';; esac; done; exit 0;;\n  *) cat >/dev/null; echo 'fake codex cannot extract' >&2; exit 1;;\nesac\n"
    );
    std::fs::write(&agent, script).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.join("extract.toml");
    std::fs::write(&config, "").unwrap();
    let out = dir.join("drafts");
    std::fs::create_dir_all(&out).unwrap();
    Command::new(hauksbee())
        .args(["models", "extract", "--backend", "codex", "--unmodelled"])
        .arg(&board)
        .arg("--out-dir")
        .arg(&out)
        .args(extra)
        .env("HAUKSBEE_EXTRACT_CONFIG", &config)
        .env("HAUKSBEE_CODEX_COMMAND", &agent)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee")
}

/// Plenty of headroom: one line says the cost and the pool, no warning, and the
/// run leaves its usage record beside the cards.
#[cfg(unix)]
#[test]
fn the_preflight_line_names_the_cost_and_the_pool_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let run = plan_batch(dir.path(), 10, 20, &["--model", "gpt-6-luna", "--yes"]);
    let all = text_of(&run);
    assert!(
        all.contains("2 parts, about 2% of your 5-hour window and 0.1% of your weekly; running up to 2 agents at once (fewer if the service rate-limits)"),
        "{all}"
    );
    assert!(!all.contains("warning:"), "{all}");
    let record = std::fs::read_to_string(dir.path().join("drafts/batch-summary.txt")).unwrap();
    assert!(record.contains("plan: 2 parts, about 2%"), "{record}");
    assert!(
        record.contains("agents: up to 2 planned, peak ")
            && record.contains("no rate-limit refusals"),
        "{record}"
    );
    assert!(record.contains("usage:"), "{record}");
}

/// `--jobs` overrides the recommendation but the plan is still said.
#[cfg(unix)]
#[test]
fn jobs_overrides_the_recommendation_and_the_line_still_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let run = plan_batch(
        dir.path(),
        10,
        20,
        &["--model", "gpt-6-luna", "--yes", "--jobs", "1"],
    );
    let all = text_of(&run);
    assert!(
        all.contains("running 1 agent at a time (as asked; the default is 2)"),
        "{all}"
    );
}

/// A run that would overrun the 5-hour window warns; under --yes it goes on,
/// holds the pool to what the window can carry, and says so.
#[cfg(unix)]
#[test]
fn a_run_past_the_window_warns_and_yes_holds_the_pool() {
    let dir = tempfile::tempdir().unwrap();
    let run = plan_batch(
        dir.path(),
        95,
        20,
        &["--model", "gpt-6.1-sol", "--yes", "--jobs", "2"],
    );
    let all = text_of(&run);
    assert!(
        all.contains("warning: it needs more than your 5-hour window has left")
            && all.contains("continuing because of --yes, with 1 agent"),
        "{all}"
    );
    // Without --yes the same warning is shown, then consent is refused (no terminal).
    let ask = tempfile::tempdir().unwrap();
    let run = plan_batch(ask.path(), 95, 20, &["--model", "gpt-6.1-sol"]);
    let all = text_of(&run);
    assert!(!run.status.success(), "{all}");
    assert!(
        all.contains("warning: it needs more than your 5-hour window has left")
            && all.contains("pass --yes"),
        "{all}"
    );
}

/// An agent whose plan cannot be read gets a small pool and says why.
#[cfg(unix)]
#[test]
fn an_unreadable_plan_runs_a_conservative_pool_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = draft_batch(dir.path(), &[], "echo opamp");
    let all = text_of(&run);
    assert!(
        all.contains("plan headroom unknown; running up to 2 agents at once (conservative; fewer if the service rate-limits)"),
        "{all}"
    );
}
