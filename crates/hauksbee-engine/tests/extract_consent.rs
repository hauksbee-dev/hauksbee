//! Datasheet extraction must not send anything without being asked.
//!
//! The extractor ships someone's datasheet to an LLM backend. That is fine
//! when they asked for it and wrong every other time, and it cannot be undone
//! afterwards, so the refusal is worth a test that drives the real binary.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn hauksbee() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hauksbee"))
}

/// A file that is a valid path but not a real datasheet. Nothing here should
/// ever read far enough to care.
fn stub_pdf(dir: &std::path::Path) -> PathBuf {
    let p = dir.join("stub.pdf");
    std::fs::write(&p, b"%PDF-1.4\n").unwrap();
    p
}

#[test]
fn a_pipe_is_refused_rather_than_answered_for() {
    // stdin is not a terminal here, so there is nobody to ask. Assuming yes
    // would send a datasheet because someone ran the wrong command in CI.
    let dir = tempfile::tempdir().unwrap();
    let pdf = stub_pdf(dir.path());
    let out = Command::new(hauksbee())
        .args([
            "models", "extract", "--part", "BC847B", "--kind", "bjt_npn", "--pdf",
        ])
        .arg(&pdf)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
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
    let out = Command::new(hauksbee())
        .args([
            "models", "extract", "--part", "BC847B", "--kind", "bjt_npn", "--pdf",
        ])
        .arg(&pdf)
        .stdin(Stdio::null())
        .output()
        .expect("run hauksbee");
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
        all.contains("datasheet-extracted"),
        "and that the result is a labelled draft:\n{all}"
    );
}

#[test]
fn a_missing_datasheet_fails_before_the_consent_question() {
    // Asking someone to approve sending a file that is not there would train
    // them to click through the question.
    let out = Command::new(hauksbee())
        .args([
            "models",
            "extract",
            "--pdf",
            "/no/such/file.pdf",
            "--part",
            "X",
            "--yes",
        ])
        .output()
        .expect("run hauksbee");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("no datasheet at"), "{err}");
}
