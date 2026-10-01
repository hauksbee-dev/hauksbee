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
    let run = Command::new(hauksbee())
        .args(["models", "extract", "--unmodelled"])
        .arg(&board)
        .arg("--out-dir")
        .arg(&out)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let (so, se) = (
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    assert!(so.contains("XOPAMP1-TR") && so.contains("stub.pdf"), "{so}");
    assert!(
        so.contains("no datasheet for XNOSHEET7") && so.contains("--datasheet 'XNOSHEET7="),
        "{so}"
    );
    assert!(
        !run.status.success() && se.contains("without consent"),
        "{so}\n{se}"
    );
    assert!(!out.exists(), "nothing is written before consent");
}

/// The whole user flow, offline: a fake agent drafts the card, it is stamped
/// with the board value it was asked for, and the next resolve binds it.
#[cfg(unix)]
#[test]
fn unmodelled_batch_drafts_a_card_the_next_run_binds() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let board = unmodelled_board(dir.path());
    let card = dir.path().join("card.toml");
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
    let agent = dir.path().join("agent");
    let script = format!("#!/bin/sh\ncat >/dev/null\nif grep -q 'EXACTLY ONE identifier' prompt.md; then echo opamp; else cp {card:?} model.toml; echo done; fi\n");
    std::fs::write(&agent, script).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join("extract.toml");
    std::fs::write(&config, "").unwrap();
    let out = dir.path().join("drafts");
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
        .env("HAUKSBEE_EXTRACT_CONFIG", &config)
        .env("HAUKSBEE_CLAUDE_COMMAND", &agent)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.status.success(), "{all}");
    assert!(
        all.contains("--models-dir"),
        "ends with the re-check command:\n{all}"
    );
    let drafted = std::fs::read_to_string(out.join("XOPAMP1-TR.toml")).unwrap();
    assert!(
        drafted.contains("OPAMP[-_ ]?XOPAMP1"),
        "the board value binds:\n{drafted}"
    );
    let resolve = run_resolve(&board, &out);
    assert!(resolve.contains("xopamp1"), "{resolve}");
}

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
