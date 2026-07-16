//! The `--lint` report (connectivity, strap-pin, resource-conflict and
//! device-decode checks) and the `--resources` subset (the MCU-internal
//! resource-conflict checks). It renders through the netlint renderers, surfaces
//! any pin-role guesses the binder inferred rather than silently guessing, and
//! under `--strict` exits non-zero on a high/medium finding. CLI glue over the
//! engine's lint checks.

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{lint_findings_json, BindSummary, JsonNote, JsonNoteKind, JsonReport};

use super::{lint_fails, OutputMode};

/// Print the full `--lint` bundle in `mode`, surface any pin-role guesses, then
/// (under `strict`) exit non-zero on a high/medium finding.
pub fn emit(board: &ExtractedBoard, lib: &ModelLibrary, mode: OutputMode, strict: bool) -> anyhow::Result<()> {
    // device_decode is now inside engine_lint (so --check/--json/TUI/frontdoor
    // get it too); no longer spliced here, which would double-count it.
    let report = crate::checks::engine_lint(board, lib);
    // Bind once: both the JSON header and the pin-role guess surfacing read it.
    let bound = bind_board(board, lib);
    // Pin-role GUESS warnings: roles the binder inferred from the configurable
    // pin-rule table rather than an explicit pin-function. Nothing is silently
    // guessed, so the lint reports each one — but on the correct channel per
    // mode. In JSON mode they ride the structured `notes` array (kind
    // `bind_role`); printing them to stdout after the document would append
    // non-JSON text and corrupt the single JSON document a consumer parses.
    let guesses: Vec<(String, String)> = bound
        .report
        .guess_warnings()
        .map(|(r, g)| (r.to_string(), g.to_string()))
        .collect();
    match mode {
        OutputMode::Json => {
            println!("{}", lint_json(&bound, &report, &guesses));
        }
        OutputMode::Plain | OutputMode::Text => {
            match mode {
                OutputMode::Plain => print!("{}", crate::plain_netlint(&report).render()),
                _ => print!("{}", hauksbee_extract::render_netlint(&report)),
            }
            if !guesses.is_empty() {
                println!("\npin-role guesses ({}):", guesses.len());
                for (r, g) in &guesses {
                    println!("  ? {r}: {g}");
                }
            }
        }
    }
    if strict && lint_fails(&report) {
        std::process::exit(2);
    }
    Ok(())
}

/// Build the `--lint --json` document: the bind header, the lint findings, and
/// the pin-role guesses as structured `bind_role` notes. Kept as a pure helper
/// (no stdout) so a test can assert the whole thing is ONE valid JSON document
/// — the guesses must ride the `notes` array, never trail the document as loose
/// text that would break a JSON consumer.
fn lint_json(
    bound: &crate::binder::BoundBoard,
    report: &hauksbee_extract::NetLintReport,
    guesses: &[(String, String)],
) -> String {
    let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
    jr.findings = Some(lint_findings_json(report));
    jr.notes.extend(guesses.iter().map(|(r, g)| JsonNote {
        kind: JsonNoteKind::BindRole,
        message: format!("pin-role guess {r}: {g}"),
    }));
    jr.to_json()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::BoundBoard;
    use crate::report::BindReport;
    use hauksbee_extract::NetLintReport;
    use hauksbee_ir::Circuit;
    use std::collections::HashMap;

    fn empty_bound() -> BoundBoard {
        BoundBoard {
            name: "t".to_string(),
            circuit: Circuit::new(),
            net_nodes: HashMap::new(),
            net_names: Vec::new(),
            digital: Vec::new(),
            mcus: Vec::new(),
            component_kinds: HashMap::new(),
            input_sources: HashMap::new(),
            supplies: Vec::new(),
            behavioral: Vec::new(),
            device_meta: Vec::new(),
            dacs: Vec::new(),
            report: BindReport::default(),
        }
    }

    /// R16: `--lint --json` used to `println!` the pin-role guess block AFTER the
    /// JSON document, so stdout was a valid JSON object followed by loose
    /// "pin-role guesses (...)" text — the whole stream no longer parsed as one
    /// JSON document. The guesses must now ride the structured `notes` array.
    #[test]
    fn lint_json_with_guesses_is_one_valid_json_document() {
        let bound = empty_bound();
        let report = NetLintReport::default();
        let guesses = vec![
            ("U1.PA0".to_string(), "adc0".to_string()),
            ("U1.PB3".to_string(), "spi_sck".to_string()),
        ];
        let out = lint_json(&bound, &report, &guesses);
        // The ENTIRE output must parse as a single JSON value — no trailing text.
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("lint --json must emit ONE parseable JSON document");
        // The guesses are present, on the structured `notes` channel.
        let notes = v.get("notes").and_then(|n| n.as_array()).expect("notes array");
        assert_eq!(notes.len(), 2, "both pin-role guesses surfaced as notes");
        assert!(notes.iter().all(|n| n.get("kind").and_then(|k| k.as_str()) == Some("bind_role")));
        assert!(notes
            .iter()
            .any(|n| n.get("message").and_then(|m| m.as_str()) == Some("pin-role guess U1.PA0: adc0")));
        // With no guesses, notes is omitted (skip_serializing_if) and it still parses.
        let clean = lint_json(&bound, &report, &[]);
        let cv: serde_json::Value = serde_json::from_str(&clean).expect("clean lint --json parses");
        assert!(cv.get("notes").is_none(), "no guesses => no notes key");
    }
}
