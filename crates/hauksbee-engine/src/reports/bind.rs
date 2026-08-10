//! `--report`: the bind table (every component -> device model), augmented with
//! an honest role-aware summary. This is a description of the board, not a
//! pass/fail check, so `--strict` does not apply; `--plain` adds a one-line
//! bottom verdict.

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
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
    let bound = bind_board(board, lib);
    let summary = BindSummary::from_report(&bound.report);
    let evidence = crate::evidence::BoardEvidence::from_bound(
        board,
        &bound.report,
        reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?;
    match mode {
        OutputMode::Json => {
            // Descriptive, never gating: `--strict` does not reach this
            // surface, so its verdict must not read `invalid` where the exit
            // code will always be 0. The binding facts stay in the evidence
            // array and the bind summary, both rendered in full.
            let report = JsonReport::new(&bound.name, summary)
                .describing_only()
                .with_inputs(inputs)
                .with_evidence(&evidence);
            println!("{}", report.to_json());
        }
        OutputMode::Text | OutputMode::Plain => {
            print!("{}", bound.report.render_table());
            print!("{}", summary.render_banner());
            // Plain bottom line (Marco): a 74-row bind table that ends in scary
            // "NOT trustworthy" warnings reads like the tool broke. Give --plain a
            // one-line verdict that says what it means and what's still usable.
            if mode == OutputMode::Plain {
                let n = summary.critical_parts_bound_n;
                let m = summary.critical_parts_total;
                // Same union as the web/json personas: unresolved active ICs plus
                // resolved-but-open active ICs (a resolved MCU with all I/O pins
                // open makes its nets just as untrustworthy).
                let open = crate::result::coverage_open_active_refs(&summary).len();
                println!();
                if open > 0 {
                    println!(
                        "Bottom line: {n} of {m} critical parts modelled. {open} active IC(s) above are \
                         unresolved/open, so firmware/analog/AC/thermal results on their nets would be \
                         INCOMPLETE, but the copper checks are unaffected (run --drc). Add models with \
                         --models-dir to cover them."
                    );
                } else if m > 0 {
                    println!(
                        "Bottom line: all {m} critical parts modelled; the board binds cleanly."
                    );
                } else {
                    println!("Bottom line: no active ICs to model; this is a passive board.");
                }
            }
            print!("{}", evidence.render_plain());
        }
    }
    Ok(())
}
