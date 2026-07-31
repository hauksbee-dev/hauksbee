//! The extraction sandbox must contain a copy, never the user's own directory.
//!
//! Codex runs full-auto with write access to its working directory. If that
//! directory were the folder the datasheet sits in, `--pdf
//! ~/Downloads/part.pdf` would hand an autonomous agent write access to the
//! whole of Downloads. These tests pin the copy-not-the-original property.
//!
//! What they do NOT establish, and what the module doc is careful about: the
//! `workspace-write` profile confines writes, not reads. These tests cover
//! where the agent can WRITE and what we hand it, which is what the sandbox
//! actually controls.

use std::path::Path;

/// A minimal PDF. Nothing in these tests reads it as a document.
const STUB_PDF: &[u8] = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n";

#[test]
fn the_agent_never_sees_the_directory_the_datasheet_came_from() {
    // The user's directory, with a neighbour file that must stay unreachable.
    let users_dir = tempfile::tempdir().unwrap();
    let pdf = users_dir.path().join("part.pdf");
    std::fs::write(&pdf, STUB_PDF).unwrap();
    let secret = users_dir.path().join("tax-return.txt");
    std::fs::write(&secret, b"not for an LLM").unwrap();

    let ws = hauksbee_models::datasheet::sandbox_for_test(&pdf).expect("build sandbox");

    assert_ne!(
        ws.path(),
        users_dir.path(),
        "the sandbox must not BE the user's directory"
    );
    assert!(
        !ws.path().starts_with(users_dir.path()),
        "nor sit inside it: {} is under {}",
        ws.path().display(),
        users_dir.path().display()
    );

    // Everything reachable from the sandbox root, and what is not there.
    let names: Vec<String> = std::fs::read_dir(ws.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "datasheet.pdf"),
        "the datasheet is copied in: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("tax-return")),
        "and nothing else of the user's came with it: {names:?}"
    );
}

#[test]
fn the_sandbox_is_removed_when_the_run_ends() {
    // A failed extraction must not leave the datasheet copy behind. This covers
    // Drop, which runs on both the error path and a panic. It does NOT cover
    // SIGKILL: no Drop runs there, and the copy plus its page renders stay in
    // $TMPDIR until the OS reaps it. The directory is 0700, so that is disk
    // residue rather than exposure.
    let users_dir = tempfile::tempdir().unwrap();
    let pdf = users_dir.path().join("part.pdf");
    std::fs::write(&pdf, STUB_PDF).unwrap();

    let path = {
        let ws = hauksbee_models::datasheet::sandbox_for_test(&pdf).expect("build sandbox");
        ws.path().to_path_buf()
    };
    assert!(!path.exists(), "{} outlived the run", path.display());
}

#[test]
fn the_answer_file_is_inside_the_sandbox() {
    // The agent writes its answer where it is allowed to write, and we read it
    // from there. A path outside would either fail or, worse, succeed.
    let users_dir = tempfile::tempdir().unwrap();
    let pdf = users_dir.path().join("part.pdf");
    std::fs::write(&pdf, STUB_PDF).unwrap();

    let ws = hauksbee_models::datasheet::sandbox_for_test(&pdf).expect("build sandbox");
    let answer = ws.answer_path();
    assert!(
        answer.starts_with(ws.path()),
        "answer path {} escapes the sandbox {}",
        answer.display(),
        ws.path().display()
    );
    assert_eq!(
        answer.file_name().and_then(|n| n.to_str()),
        Some("model.toml")
    );
}

#[test]
fn a_missing_datasheet_fails_before_a_sandbox_exists() {
    let err = hauksbee_models::datasheet::sandbox_for_test(Path::new("/no/such/part.pdf"))
        .expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("copying") || msg.contains("no such"),
        "and say what went wrong: {msg}"
    );
}

/// A real multi-page datasheet must actually render, since the whole reason for
/// page images is that a text dump loses the tables. A silently empty render
/// would degrade every extraction with nothing to show it happened.
///
/// Ignored rather than silently skipped. It needs a real PDF, which the repo
/// cannot ship because datasheets are not ours to redistribute, and as a silent
/// skip it had never run once: nothing in the repo sets the variable. `cargo
/// test` now REPORTS it as ignored instead of counting it among the passes.
#[test]
#[ignore = "needs a real datasheet PDF: set HAUKSBEE_TEST_DATASHEET and run with --ignored"]
fn a_real_datasheet_renders_its_pages() {
    let Some(pdf) = std::env::var_os("HAUKSBEE_TEST_DATASHEET").map(std::path::PathBuf::from)
    else {
        eprintln!("skipping: set HAUKSBEE_TEST_DATASHEET to a real PDF to check rendering");
        return;
    };
    if !pdf.is_file() {
        eprintln!("skipping: HAUKSBEE_TEST_DATASHEET is not a file");
        return;
    }
    if which_pdftoppm().is_none() {
        eprintln!("skipping: pdftoppm not installed");
        return;
    }
    let ws = hauksbee_models::datasheet::sandbox_for_test(&pdf).expect("build sandbox");
    assert!(
        !ws.pages.is_empty(),
        "poppler is installed and the PDF is real, so pages should have rendered"
    );
    for page in &ws.pages {
        let len = std::fs::metadata(page).map(|m| m.len()).unwrap_or(0);
        assert!(
            len > 1000,
            "{} is {len} bytes, which is not a page",
            page.display()
        );
        assert!(
            page.starts_with(ws.path()),
            "renders must stay in the sandbox"
        );
    }
    eprintln!("rendered {} page(s)", ws.pages.len());
}

fn which_pdftoppm() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("pdftoppm"))
            .find(|c| c.is_file())
    })
}
