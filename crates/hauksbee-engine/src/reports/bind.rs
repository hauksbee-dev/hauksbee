//! `--report`: the attention-first bind table, augmented with an honest
//! role-aware summary. Bulk fallback passives collapse by default; `--verbose`
//! restores every component row. This is a description of the board, not a
//! pass/fail check, so `--strict` does not apply; `--plain` adds a one-line
//! bottom verdict.

use hauksbee_extract::{ExtractedBoard, LintCheck, NetLintReport};
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::report::{BindOutcome, BindReport};
use crate::result::{BindSummary, JsonInputEvidence, JsonReport};

use super::OutputMode;

/// Print the bind report in `mode`, then return.
pub fn emit(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    reader_notes: &[String],
    mode: OutputMode,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    emit_quiet(board, lib, reader_notes, mode, false, false, inputs)
}

pub(crate) fn emit_quiet(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    reader_notes: &[String],
    mode: OutputMode,
    quiet: bool,
    verbose: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    let bound = bind_board(board, lib);
    // A datasheet decoder can make a trustworthy static statement about a part
    // that has no executable simulation model. Keep the evidence-bearing bind
    // report unchanged and annotate a display copy, so the table states that
    // distinction without rewriting why simulation left the part open.
    let lint = crate::checks::engine_lint(board, lib);
    let mut display_report = bound.report.clone();
    let decode_only_count = mark_decode_only_rows(&mut display_report, &lint);
    let summary = BindSummary::from_report(&bound.report);
    let evidence = crate::evidence::BoardEvidence::from_bound(
        board,
        &bound.report,
        reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?;
    match mode {
        OutputMode::Json => {
            // Descriptive, non-gating: `emit` takes no `strict` flag and calls
            // no gate helper, so its verdict must not read `invalid` beside an
            // exit code that cannot be 2 or 3. Once this document prints, no
            // non-zero code is still ahead of it: the `?` on evidence
            // construction above has already passed, and `commands::run`'s
            // no-components exit 3 fires before `--report` reaches here, so no
            // bind document is printed on that path at all. The binding facts
            // stay in the evidence array and the
            // bind summary, both rendered in full.
            let report = JsonReport::new(&bound.name, summary)
                .with_descriptive_only()
                .with_inputs(inputs)
                .with_evidence(&evidence);
            println!("{}", report.to_json());
        }
        OutputMode::Text | OutputMode::Plain => {
            print!(
                "{}",
                if verbose {
                    display_report.render_table()
                } else {
                    display_report.render_table_compact()
                }
            );
            print!("{}", summary.render_banner());
            // Plain bottom line (Marco): a 74-row bind table that ends in scary
            // "NOT trustworthy" warnings reads like the tool broke. Give --plain a
            // one-line verdict that says what it means and what's still usable.
            if mode == OutputMode::Plain {
                if decode_only_count > 0 {
                    println!(
                        "Decode-only marker: these part(s) have no executable simulation model, \
                         but an existing datasheet-rule check still decoded their fitted \
                         configuration value. That static decode does not make analog/AC/thermal \
                         simulation of the part available."
                    );
                }
                let n = summary.critical_parts_bound_n;
                let m = summary.critical_parts_total;
                // Same union as the web/json personas: unresolved active ICs plus
                // resolved-but-open active ICs (a resolved MCU with all I/O pins
                // open makes its nets just as untrustworthy).
                let open = crate::result::coverage_open_active_refs(&summary).len();
                let open_path = summary.active_path_unresolved.len();
                println!();
                if open > 0 {
                    println!(
                        "Bottom line: all {m} critical active devices were discovered; {n} have executable \
                         behavioural models. {open} active IC(s) above are \
                         unresolved/open, so firmware/analog/AC/thermal results on their nets would be \
                         INCOMPLETE, but the copper checks are unaffected (run --drc). Add models with \
                         --models-dir to cover them."
                    );
                } else if open_path > 0 {
                    println!(
                        "Bottom line: all {m} discovered critical active devices have executable \
                         behavioural models, but {open_path} connected path element(s) above still \
                         default to OPEN, so the whole-board electrical path is INCOMPLETE. Run \
                         `hauksbee models coverage <BOARD>` to inspect the exact gaps, then \
                         `hauksbee models prepare <BOARD> --pack-dir <DIR>` to review an \
                         approval-gated local model pack; copper checks remain unaffected."
                    );
                } else if m > 0 {
                    println!(
                        "Bottom line: all {m} discovered critical active devices have executable \
                         behavioural models; the board binds cleanly."
                    );
                } else {
                    println!("Bottom line: no active ICs to model; this is a passive board.");
                }
            }
            print!(
                "{}",
                super::render_evidence_appendix(&evidence, quiet, verbose)
            );
        }
    }
    Ok(())
}

const DECODE_ONLY_PREFIX: &str = "decode-only; ";

/// Mark unresolved rows named by an existing datasheet `DeviceDecode` finding.
/// This classifies check output already produced by the engine; it does not
/// infer that an arbitrary unresolved part was decoded.
pub(crate) fn mark_decode_only_rows(report: &mut BindReport, lint: &NetLintReport) -> usize {
    let refs: std::collections::BTreeSet<&str> = lint
        .findings
        .iter()
        .filter(|finding| finding.check == LintCheck::DeviceDecode)
        .flat_map(|finding| finding.refs.iter().map(String::as_str))
        .collect();
    let mut marked = 0usize;
    for row in &mut report.rows {
        if !refs.contains(row.reference.as_str()) {
            continue;
        }
        let BindOutcome::Unresolved { reason } = &mut row.outcome else {
            continue;
        };
        if !reason.starts_with(DECODE_ONLY_PREFIX) {
            reason.insert_str(0, DECODE_ONLY_PREFIX);
            marked += 1;
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::BindRow;
    use hauksbee_extract::{LintFinding, Severity};
    use hauksbee_models::Confidence;

    #[test]
    fn unresolved_bind_row_marks_datasheet_decode_without_simulation_model() {
        let mut report = BindReport::default();
        report.push(BindRow {
            reference: "U19".into(),
            value: "TPS25982".into(),
            model_id: None,
            confidence: Confidence::Unresolved,
            source: None,
            outcome: BindOutcome::Unresolved {
                reason: "no model matched; left open".into(),
            },
            warning: None,
            guesses: Vec::new(),
        });
        let mut lint = NetLintReport::default();
        lint.findings.push(LintFinding {
            check: LintCheck::DeviceDecode,
            severity: Severity::Medium,
            message: "U19 eFuse connector budget decoded from R48".into(),
            refs: vec!["U19".into(), "R48".into()],
            nets: vec!["ILIM".into()],
        });

        assert_eq!(mark_decode_only_rows(&mut report, &lint), 1);
        let rendered = report.render_table_compact();
        assert!(rendered.contains("U19"), "{rendered}");
        assert!(
            rendered.contains("UNRESOLVED (decode-only"),
            "the marker must be on the bind row: {rendered}"
        );
    }
}
