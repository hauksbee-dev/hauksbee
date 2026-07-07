//! `--lint` (connectivity + strap-pin + resource-conflict + device-decode) and
//! `--resources` (the MCU internal resource-conflict subset). Both render through
//! the netlint renderers.

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{lint_findings_json, BindSummary, JsonReport};

use super::{lint_fails, OutputMode};

/// Print the full `--lint` bundle in `mode`, surface any pin-role guesses, then
/// (under `strict`) exit non-zero on a high/medium finding.
pub fn emit(board: &ExtractedBoard, lib: &ModelLibrary, mode: OutputMode, strict: bool) -> anyhow::Result<()> {
    let mut report = crate::checks::engine_lint(board, lib);
    report
        .findings
        .extend(crate::checks::device_decode::device_decode_lint(board, lib).findings);
    match mode {
        OutputMode::Json => {
            let bound = bind_board(board, lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.findings = Some(lint_findings_json(&report));
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => print!("{}", crate::plain_netlint(&report).render()),
        OutputMode::Text => print!("{}", hauksbee_extract::render_netlint(&report)),
    }
    // Surface pin-role GUESS warnings: roles the binder inferred from the
    // configurable pin-rule table rather than an explicit pin-function. Nothing
    // is silently guessed, so the lint reports each one.
    let bound = bind_board(board, lib);
    let guesses: Vec<(String, String)> = bound
        .report
        .guess_warnings()
        .map(|(r, g)| (r.to_string(), g.to_string()))
        .collect();
    if !guesses.is_empty() {
        println!("\npin-role guesses ({}):", guesses.len());
        for (r, g) in &guesses {
            println!("  ? {r}: {g}");
        }
    }
    if strict && lint_fails(&report) {
        std::process::exit(2);
    }
    Ok(())
}

/// `--resources`: only the MCU internal resource-conflict check (plus the
/// unchecked-MCU coverage note, so a clean result is not mistaken for "checked
/// and conflict-free").
pub fn emit_resources(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    mode: OutputMode,
    strict: bool,
) -> anyhow::Result<()> {
    let report = crate::checks::resources_lint(board, lib);
    match mode {
        OutputMode::Json => {
            let bound = bind_board(board, lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.findings = Some(lint_findings_json(&report));
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => print!("{}", crate::plain_netlint(&report).render()),
        OutputMode::Text => print!("{}", hauksbee_extract::render_netlint(&report)),
    }
    if strict && lint_fails(&report) {
        std::process::exit(2);
    }
    Ok(())
}
