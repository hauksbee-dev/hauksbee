//! The `--si` report: the signal-integrity / physics static checks from the
//! extractor, augmented with the engine-layer checks whose attribution needs the
//! bound database models, trace ampacity (IPC-2221) and input-cap ripple. It
//! renders in the requested mode and, under `--strict`, exits non-zero on a real
//! finding. CLI glue over the extract- and engine-layer SI checks.

use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{si_findings_json, BindSummary, JsonInputEvidence, JsonReport};

use super::{si_fails, OutputMode};

/// The human-surface refusal for an undermined SI input-coverage claim; the
/// same vocabulary as the INCONCLUSIVE bind refusal, single-sourced here so
/// the plain and text arms cannot drift apart.
const SI_COVERAGE_REFUSAL: &str = "INCONCLUSIVE: the signal-integrity input-coverage claim is \
undermined, so a clean result here is not a clean bill (see the evidence section).";

/// Print the SI report in `mode`, then (under `strict`) exit non-zero on a real
/// finding.
pub fn emit(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    input_kind: crate::board_input::InputKind,
    altium_present: bool,
    lib: &ModelLibrary,
    reader_notes: &[String],
    mode: OutputMode,
    strict: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    // Altium geometry is not yet threaded into the SI text checks, so pass None
    // there; the connectivity-based SI checks still run on `board`.
    let geo_text = if altium_present { None } else { Some(text) };
    // The single SI chokepoint: extract-layer SI checks plus the engine-layer
    // trace-ampacity + input-cap-ripple checks. Shared with `--check`, the JSON
    // aggregate, the TUI and the web front door so every SI surface runs the
    // identical set.
    let mut report = crate::checks::engine_si(board, lib, geo_text);
    // Waivers, same semantics as `--check`: an SI finding the board's owner
    // overruled must come out of THIS gate too, or `--si --strict` turns red on
    // a board `--check --strict` passes. Same key as check.rs, so one waiver
    // file covers both commands.
    let mut waivers = super::check::load_waivers(board_path);
    let (kept, waived) = waivers.partition("si", std::mem::take(&mut report.findings), |f| {
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
    let coverage = evidence.check_coverage_map("si", "Signal-integrity input coverage")?;
    let run_level = if coverage.status() != hauksbee_ir::evidence::EvidenceStatus::Clean {
        vec![coverage]
    } else {
        Vec::new()
    };
    let coverage_undermined = run_level
        .iter()
        .any(|m| m.status() == hauksbee_ir::evidence::EvidenceStatus::Undermined);
    let evidence = if run_level.is_empty() {
        evidence
    } else {
        evidence.with_maps(run_level.clone())
    };
    // The verdict blockers: a clean SI result over an unbound power FET / main
    // IC is a vacuous pass, so every surface says INCONCLUSIVE (count, named
    // parts, unlocking input) instead of a clean bill. Exit codes unchanged.
    let blockers =
        crate::result::unmodelled_critical_refs(&BindSummary::from_report(&bound.report));
    match mode {
        OutputMode::Json => {
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
                .with_bind_verdict_gate()
                .with_inputs(inputs)
                .with_evidence(&evidence);
            jr.findings = Some(si_findings_json(&report));
            jr.attach_finding_evidence(&evidence, run_level.clone())?;
            if !blockers.is_empty() {
                jr.notes.push(crate::result::JsonNote {
                    kind: crate::result::JsonNoteKind::Coverage,
                    message: crate::result::inconclusive_verdict(&blockers),
                });
            }
            // A green verdict that quietly dropped findings would be worse than
            // no waivers at all, so the machine surface carries them too.
            jr.waived = waived.iter().cloned().map(Into::into).collect();
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => {
            let mut plain = crate::plain_si(&report);
            plain.unmodelled_critical = blockers.clone();
            print!("{}", plain.render());
            // The coverage refusal on the human surface too: a healthy plain
            // verdict above an undermined input-coverage claim is the same
            // vacuous clean the JSON verdict refuses.
            if coverage_undermined {
                println!("{SI_COVERAGE_REFUSAL}");
            }
        }
        OutputMode::Text => {
            // Verdict first: "si-checks: no gating findings." over an unbound
            // FET reads as a clean bill unless the INCONCLUSIVE line leads.
            if !blockers.is_empty() {
                println!("{}", crate::result::inconclusive_verdict(&blockers));
            }
            if coverage_undermined {
                println!("{SI_COVERAGE_REFUSAL}");
            }
            print!("{}", hauksbee_extract::render_si(&report));
            if !report.is_clean() {
                eprintln!(
                    "note: run the same command with --plain for a plain-language \
                     explanation of each finding and how to fix it."
                );
            }
        }
    }
    if !matches!(mode, OutputMode::Json) {
        print!("{}", evidence.render_plain());
        print!(
            "{}",
            super::check::render_waivers_scoped(&waived, &waivers, &["si"], true)
        );
    }
    super::note_ungated_findings(strict, si_fails(&report));
    if strict && si_fails(&report) {
        super::strict_gate_exit(mode, &super::si_gate_items(&report));
    }
    if strict && evidence.is_undermined() {
        std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS);
    }
    Ok(())
}
