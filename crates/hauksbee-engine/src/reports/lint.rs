//! The `--lint` report (connectivity, strap-pin, resource-conflict and
//! device-decode checks) and the `--resources` subset (the MCU-internal
//! resource-conflict checks). It renders through the netlint renderers, surfaces
//! any pin-role guesses the binder inferred rather than silently guessing, and
//! under `--strict` exits non-zero on a high/medium finding. CLI glue over the
//! engine's lint checks.

use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{
    lint_findings_json, BindSummary, JsonInputEvidence, JsonNote, JsonNoteKind, JsonReport,
};

use super::{lint_fails, OutputMode};

/// Print the full `--lint` bundle in `mode`, surface any pin-role guesses, then
/// (under `strict`) exit non-zero on a high/medium finding.
pub fn emit(
    board_path: &Path,
    board: &ExtractedBoard,
    raw: &[u8],
    input_kind: crate::board_input::InputKind,
    lib: &ModelLibrary,
    reader_notes: &[String],
    mode: OutputMode,
    strict: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    // device_decode lives inside engine_lint (so --check/--json/TUI/frontdoor
    // get it too), so it must not be spliced in here as well: that
    // double-counts every decode finding.
    let mut report = crate::checks::engine_lint(board, lib);
    // Waivers, same semantics as `--check`: without this, a lint finding the
    // board's owner overruled turns `--lint --strict` red on a board that
    // `--check --strict` passes, and the narrower command looks like it lost
    // the waiver. Same key as check.rs, so the same waiver file covers both.
    let mut waivers = super::check::load_waivers(board_path);
    let (kept, waived) = waivers.partition("lint", std::mem::take(&mut report.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    report.findings = kept;
    // Bind once: both the JSON header and the pin-role guess surfacing read it.
    let bound = bind_board(board, lib);
    let evidence = crate::evidence::BoardEvidence::from_bound(
        board,
        &bound.report,
        reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?
    .with_input_artifact(board_path, raw, input_kind)?;
    // Pin-role GUESS warnings: roles the binder inferred from the configurable
    // pin-rule table rather than an explicit pin-function. Nothing is silently
    // guessed, so the lint reports each one, but on the correct channel per
    // mode. In JSON mode they ride the structured `notes` array (kind
    // `bind_role`); printing them to stdout after the document would append
    // non-JSON text and corrupt the single JSON document a consumer parses.
    let guesses: Vec<(String, String)> = bound
        .report
        .guess_warnings()
        .map(|(r, g)| (r.to_string(), g.to_string()))
        .collect();
    // The verdict blockers: current-carrying / active parts with no model. A
    // lint that came back clean over an unbound power FET or main IC must say
    // INCONCLUSIVE (naming them, and what unlocks a conclusive verdict) rather
    // than a clean bill, on every surface. Without --strict the exit code is
    // unchanged; under it these blockers exit 3, matching the verdict field.
    let blockers =
        crate::result::unmodelled_critical_refs(&BindSummary::from_report(&bound.report));
    match mode {
        OutputMode::Json => {
            println!(
                "{}",
                lint_json(&bound, &report, &guesses, &waived, &blockers, inputs, &evidence)?
            );
        }
        OutputMode::Plain | OutputMode::Text => {
            match mode {
                OutputMode::Plain => {
                    let mut plain = crate::plain_netlint(&report);
                    plain.unmodelled_critical = blockers.clone();
                    print!("{}", plain.render());
                }
                _ => {
                    // The INCONCLUSIVE verdict LEADS: "net-lint: no findings."
                    // over an unmodelled FET/IC reads as a clean bill, so the
                    // refusal has to be the first line, with the factual body
                    // underneath it, not an afterthought below one.
                    if !blockers.is_empty() {
                        println!("{}", crate::result::inconclusive_verdict(&blockers));
                    }
                    print!("{}", hauksbee_extract::render_netlint(&report));
                }
            }
            if !guesses.is_empty() {
                print!("{}", render_pin_role_guesses(&guesses));
            }
            print!(
                "{}",
                super::check::render_waivers_scoped(&waived, &waivers, &["lint"], true)
            );
            print!("{}", evidence.render_plain());
        }
    }
    super::note_ungated_findings(strict, lint_fails(&report));
    if strict && lint_fails(&report) {
        super::strict_gate_exit(mode, &super::lint_gate_items(&report));
    }
    // Mirror of the JSON verdict's rule: binding completeness gates through
    // the verdict-critical set only, never through any open passive's per-net
    // map, so --strict cannot exit 3 where the verdict field says pass.
    if strict && !blockers.is_empty() {
        super::exit_invalid_for_analysis(&blockers);
    }
    Ok(())
}

/// Guesses beyond this many collapse to a per-pattern summary. A correct guess
/// on a standard 2-pin or 3-pin footprint is not news; printing one line each
/// buries the actual findings under 15 lines on a 137-part board and 300 on a
/// 3,000-part one. Below the threshold the full list is still the friendlier
/// output, so small boards are unchanged.
const GUESS_LIST_LIMIT: usize = 6;

/// Render the pin-role guesses: in full when there are few, otherwise one line
/// per distinct inferred pattern with a count. The complete per-part mapping
/// always remains available in `--json`, where it rides the `notes` array as
/// structured `bind_role` entries, so collapsing here hides nothing that a
/// consumer needs.
fn render_pin_role_guesses(guesses: &[(String, String)]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "\npin-role guesses ({}):", guesses.len());
    if guesses.len() <= GUESS_LIST_LIMIT {
        for (r, g) in guesses {
            let _ = writeln!(s, "  ? {r}: {g}");
        }
        return s;
    }
    // Group by the inferred pattern, keeping first-seen order so the output is
    // deterministic for a given board.
    let mut order: Vec<&str> = Vec::new();
    let mut counts: std::collections::HashMap<&str, (usize, &str)> =
        std::collections::HashMap::new();
    for (r, g) in guesses {
        let entry = counts.entry(g.as_str()).or_insert_with(|| {
            order.push(g.as_str());
            (0, r.as_str())
        });
        entry.0 += 1;
    }
    for pattern in order {
        let (n, first) = counts[pattern];
        let _ = writeln!(s, "  ? {pattern} (x{n}, e.g. {first})");
    }
    let _ = writeln!(s, "  the per-part mapping is in --json (bind_role notes)");
    s
}

/// Build the `--lint --json` document: the bind header, the lint findings, and
/// the pin-role guesses as structured `bind_role` notes. Kept as a pure helper
/// (no stdout) so a test can assert the whole thing is ONE valid JSON document;
/// the guesses must ride the `notes` array, never trail the document as loose
/// text that would break a JSON consumer.
fn lint_json(
    bound: &crate::binder::BoundBoard,
    report: &hauksbee_extract::NetLintReport,
    guesses: &[(String, String)],
    waived: &[crate::waiver::WaivedFinding],
    blockers: &[String],
    inputs: &[JsonInputEvidence],
    evidence: &crate::evidence::BoardEvidence,
) -> Result<String, hauksbee_ir::evidence::EvidenceError> {
    let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
        .with_bind_verdict_gate()
        // `lint_fails` is this surface's exit gate and it includes
        // medium-severity findings, which serialize as `warning`; without
        // telling the verdict so, the document read `pass` beside exit 2.
        .with_surface_gate(lint_fails(report))
        .with_inputs(inputs)
        .with_evidence(evidence);
    jr.findings = Some(lint_findings_json(report));
    jr.attach_finding_evidence(evidence, Vec::new())?;
    // The INCONCLUSIVE verdict on the machine surface: a coverage note with the
    // same sentence the text/plain verdicts print. The note itself gates
    // nothing, but the parts it names are the bind gate's, so this document's
    // `verdict` reads `invalid` beside it and --strict exits 3. The structured
    // part list is already in `bind.active_path_unresolved`.
    if !blockers.is_empty() {
        jr.notes.push(JsonNote {
            kind: JsonNoteKind::Coverage,
            message: crate::result::inconclusive_verdict(blockers),
        });
    }
    jr.notes.extend(guesses.iter().map(|(r, g)| JsonNote {
        kind: JsonNoteKind::BindRole,
        message: format!("pin-role guess {r}: {g}"),
    }));
    // A green verdict that quietly dropped findings would be worse than no
    // waivers at all, so the machine surface carries them too (same shape as
    // `--check --json`'s `waived` array).
    jr.waived = waived.iter().cloned().map(Into::into).collect();
    Ok(jr.to_json())
}

/// `--resources`: only the MCU internal resource-conflict check (plus the
/// unchecked-MCU coverage note, so a clean result is not mistaken for "checked
/// and conflict-free").
pub fn emit_resources(
    board_path: &Path,
    board: &ExtractedBoard,
    raw: &[u8],
    input_kind: crate::board_input::InputKind,
    lib: &ModelLibrary,
    reader_notes: &[String],
    mode: OutputMode,
    strict: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    let mut report = crate::checks::resources_lint(board, lib);
    // Resource conflicts are lint-class findings and ride the "lint" check in a
    // waiver file, exactly as they do inside `--check` (where they arrive via
    // engine_lint). Waiving them under a different check name here would mean
    // one waiver file cannot cover both commands.
    let mut waivers = super::check::load_waivers(board_path);
    let (kept, waived) = waivers.partition("lint", std::mem::take(&mut report.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    report.findings = kept;
    let bound = bind_board(board, lib);
    let evidence = crate::evidence::BoardEvidence::from_bound(
        board,
        &bound.report,
        reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?
    .with_input_artifact(board_path, raw, input_kind)?;
    // Same INCONCLUSIVE contract as the full `--lint`: this subset's clean
    // verdict is just as vacuous over an unmodelled critical part.
    let blockers =
        crate::result::unmodelled_critical_refs(&BindSummary::from_report(&bound.report));
    match mode {
        OutputMode::Json => {
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
                .with_bind_verdict_gate()
                // Same gate as `--lint` above, so the same widening applies.
                .with_surface_gate(lint_fails(&report))
                .with_inputs(inputs)
                .with_evidence(&evidence);
            jr.findings = Some(lint_findings_json(&report));
            jr.attach_finding_evidence(&evidence, Vec::new())?;
            if !blockers.is_empty() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: crate::result::inconclusive_verdict(&blockers),
                });
            }
            // Same honesty rule as every other machine surface: the verdict may
            // not quietly drop findings, so the waived list travels with it.
            jr.waived = waived.iter().cloned().map(Into::into).collect();
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => {
            let mut plain = crate::plain_netlint(&report);
            // `plain_netlint` is shared with `--lint`, whose subject is the whole
            // connectivity family. `--resources` ran one member of it, so it must
            // say so: "no connectivity problems found" claimed a clean bill for
            // checks this command never ran.
            plain.verdict_noun = Some("MCU resource conflicts".to_string());
            plain.unmodelled_critical = blockers.clone();
            print!("{}", plain.render());
        }
        OutputMode::Text => {
            // Same verdict-first rule as `--lint`: the refusal leads, the
            // "no findings" body sits under it.
            if !blockers.is_empty() {
                println!("{}", crate::result::inconclusive_verdict(&blockers));
            }
            print!("{}", render_resources_text(&report));
            // Route a novice from the expert text (bare severity + jargon) to the
            // already-built plain-language what/why/fix. Only when there is
            // something to explain, and only to stderr so pipes stay clean.
            if !report.findings.is_empty() {
                eprintln!(
                    "note: run the same command with --plain for a plain-language \
                     explanation of each finding and how to fix it."
                );
            }
        }
    }
    if !matches!(mode, OutputMode::Json) {
        print!("{}", evidence.render_plain());
        // No stale reporting here: --resources runs only part of the lint
        // family, so a lint waiver with no hit may simply belong to a check
        // this command never ran.
        print!(
            "{}",
            super::check::render_waivers_scoped(&waived, &waivers, &["lint"], false)
        );
    }
    super::note_ungated_findings(strict, lint_fails(&report));
    if strict && lint_fails(&report) {
        super::strict_gate_exit(mode, &super::lint_gate_items(&report));
    }
    // Mirror of the JSON verdict's rule: binding completeness gates through
    // the verdict-critical set only, never through any open passive's per-net
    // map, so --strict cannot exit 3 where the verdict field says pass.
    if strict && !blockers.is_empty() {
        super::exit_invalid_for_analysis(&blockers);
    }
    Ok(())
}

/// The `--resources` text surface. It shares the netlint finding type with
/// `--lint` but is a different report, so it must not be byte-identical to it:
/// its header names the check, and a clean run says what WAS checked instead of
/// a bare "no findings" indistinguishable from the lint report's.
fn render_resources_text(report: &hauksbee_extract::NetLintReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if report.findings.is_empty() {
        out.push_str(
            "resource-conflicts: no findings.\n\
             checked: MCU internal resource conflicts (shared timers, ADC channels, \
             UART/SPI/I2C pin muxing) for every bound MCU.\n",
        );
        return out;
    }
    let _ = writeln!(
        out,
        "resource-conflicts: {} finding(s)",
        report.findings.len()
    );
    for f in &report.findings {
        let _ = writeln!(
            out,
            "  [{}] {} - {}",
            f.severity.as_str(),
            f.check.as_str(),
            f.message
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The --resources surface must never be byte-identical to --lint: its
    /// header names the check and its clean line says what WAS checked.
    #[test]
    fn resources_text_is_distinguishable_from_lint_text() {
        let empty = hauksbee_extract::NetLintReport::default();
        let res = render_resources_text(&empty);
        let lint = hauksbee_extract::render_netlint(&empty);
        assert_ne!(res, lint);
        assert!(res.starts_with("resource-conflicts:"), "{res}");
        assert!(
            res.contains("checked:"),
            "a clean run names the scope: {res}"
        );
        assert!(!res.contains("net-lint:"), "{res}");
    }
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
            dnp_mcus: Vec::new(),
            component_kinds: HashMap::new(),
            input_sources: HashMap::new(),
            supplies: Vec::new(),
            behavioral: Vec::new(),
            device_meta: Vec::new(),
            dacs: Vec::new(),
            peripherals: Vec::new(),
            report: BindReport::default(),
        }
    }

    /// R16: `println!`ing the pin-role guess block AFTER the JSON document
    /// leaves stdout a valid JSON object followed by loose "pin-role guesses
    /// (...)" text, so the stream as a whole does not parse as one JSON
    /// document. The guesses must ride the structured `notes` array instead.
    #[test]
    fn lint_json_with_guesses_is_one_valid_json_document() {
        let bound = empty_bound();
        let board = ExtractedBoard {
            name: "t".to_string(),
            nets: Vec::new(),
            components: Vec::new(),
        };
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &bound.report,
            &[],
            hauksbee_ir::evidence::RunDate::unknown(),
        )
        .expect("empty evidence is valid");
        let report = NetLintReport::default();
        let guesses = vec![
            ("U1.PA0".to_string(), "adc0".to_string()),
            ("U1.PB3".to_string(), "spi_sck".to_string()),
        ];
        let out = lint_json(&bound, &report, &guesses, &[], &[], &[], &evidence)
            .expect("evidence attachment succeeds");
        // The ENTIRE output must parse as a single JSON value, no trailing text.
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("lint --json must emit ONE parseable JSON document");
        // The guesses are present, on the structured `notes` channel.
        let notes = v
            .get("notes")
            .and_then(|n| n.as_array())
            .expect("notes array");
        assert_eq!(notes.len(), 2, "both pin-role guesses surfaced as notes");
        assert!(notes
            .iter()
            .all(|n| n.get("kind").and_then(|k| k.as_str()) == Some("bind_role")));
        assert!(notes
            .iter()
            .any(|n| n.get("message").and_then(|m| m.as_str())
                == Some("pin-role guess U1.PA0: adc0")));
        // With no guesses, notes is omitted (skip_serializing_if) and it still parses.
        let clean = lint_json(&bound, &report, &[], &[], &[], &[], &evidence)
            .expect("evidence attachment succeeds");
        let cv: serde_json::Value = serde_json::from_str(&clean).expect("clean lint --json parses");
        assert!(cv.get("notes").is_none(), "no guesses => no notes key");
    }
}
