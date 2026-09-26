//! The `--si` report: the signal-integrity / physics static checks.
//!
//! The extractor's checks are augmented with the engine-layer checks whose
//! attribution needs the bound database models, trace ampacity (IPC-2221) and
//! input-cap ripple. It renders in the requested mode and, under `--strict`,
//! exits non-zero on a real finding. CLI glue over the extract- and engine-layer
//! SI checks.

use crate::binder::bind_board;
use crate::result::{si_findings_json, BindSummary, JsonReport};

use super::{si_fails, OutputMode, StaticInputs};

/// The human-surface refusal for an undermined SI input-coverage claim; the
/// same vocabulary as the INCONCLUSIVE bind refusal, single-sourced here so
/// the plain and text arms cannot drift apart.
const SI_COVERAGE_REFUSAL: &str = "INCONCLUSIVE: the signal-integrity input-coverage claim is \
undermined, so a clean result here is not a clean bill (see the evidence section).";

/// Print the SI report in `mode`, then (under `strict`) exit non-zero on a real
/// finding. `quiet` suppresses the text surface.
pub(crate) fn emit_quiet(
    input: &StaticInputs,
    mode: OutputMode,
    strict: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let StaticInputs { board, lib, .. } = *input;
    // The single SI chokepoint: extract-layer SI checks plus the engine-layer
    // trace-ampacity + input-cap-ripple checks. Shared with `--check`, the JSON
    // aggregate, the TUI and the web front door so every SI surface runs the
    // identical set.
    let mut report = crate::checks::engine_si(board, lib, input.geo_text());
    // Waivers, same semantics as `--check`: an SI finding the board's owner
    // overruled must come out of THIS gate too, or `--si --strict` turns red on
    // a board `--check --strict` passes. Same key as check.rs, so one waiver
    // file covers both commands.
    let mut waivers = super::check::load_waivers(input.board_path);
    let waived = super::check::waive_si(&mut waivers, &mut report);
    let bound = bind_board(board, lib);
    let evidence = input.evidence(&bound.report)?;
    let coverage = evidence.check_coverage_map("si", "Signal-integrity input coverage")?;
    let run_level = if coverage.status() == hauksbee_ir::evidence::EvidenceStatus::Clean {
        Vec::new()
    } else {
        vec![coverage]
    };
    let coverage_undermined = run_level
        .iter()
        .any(|m| m.status() == hauksbee_ir::evidence::EvidenceStatus::Undermined);
    let mut surface_maps = evidence.maps_for_findings(&si_findings_json(&report))?;
    surface_maps.extend(run_level.clone());
    let evidence = evidence.with_maps(surface_maps);
    // The verdict blockers: a clean SI result over an unbound power FET / main
    // IC is a vacuous pass, so every surface says INCONCLUSIVE (count, named
    // parts, unlocking input) instead of a clean bill. Without --strict the exit
    // code is unchanged; under it these blockers exit 3, matching the verdict.
    let blockers =
        crate::result::unmodelled_critical_refs(&BindSummary::from_report(&bound.report));
    match mode {
        OutputMode::Json => {
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
                .with_bind_verdict_gate()
                // `si_fails` is this surface's exit gate and it counts every
                // real finding, including the low ones that serialize as
                // `note` (its informational computed-value notes are excluded),
                // so the verdict has to know or the document reads `pass`
                // beside exit 2.
                .with_surface_gate(si_fails(&report))
                .with_inputs(input.inputs)
                .with_evidence(&evidence);
            jr.findings = Some(si_findings_json(&report));
            jr.attach_finding_evidence(&evidence, run_level)?;
            if !blockers.is_empty() {
                jr.push_coverage_note(crate::result::inconclusive_verdict(&blockers));
            }
            // A green verdict that quietly dropped findings would be worse than
            // no waivers at all, so the machine surface carries them too.
            jr.waived = waived.iter().cloned().map(Into::into).collect();
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => {
            let mut plain = crate::plain_si(&report);
            plain.unmodelled_critical.clone_from(&blockers);
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
        print!(
            "{}",
            super::render_evidence_appendix(&evidence, quiet, false)
        );
        print!(
            "{}",
            super::check::render_waivers_scoped(&waived, &waivers, &["si"], true)
        );
    }
    super::note_ungated_findings(strict, si_fails(&report));
    if strict && si_fails(&report) {
        super::strict_gate_exit(mode, &super::si_gate_items(&report));
    }
    // Mirror of the JSON verdict's rule: the undermined coverage claim and
    // the verdict-critical bind gate exit 3; an open passive's per-net map or
    // a finding's own badge never does.
    if strict && (coverage_undermined || !blockers.is_empty()) {
        super::exit_invalid_for_analysis(&blockers);
    }
    Ok(())
}
