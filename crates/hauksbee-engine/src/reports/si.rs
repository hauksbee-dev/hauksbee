//! `--si`: the signal-integrity / physics static checks, augmented with the
//! engine-layer checks whose attribution needs the bound DB models (trace
//! ampacity and input-cap ripple).

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{si_findings_json, BindSummary, JsonReport};

use super::{si_fails, OutputMode};

/// Print the SI report in `mode`, then (under `strict`) exit non-zero on a real
/// finding.
pub fn emit(
    board: &ExtractedBoard,
    text: &str,
    altium_present: bool,
    lib: &ModelLibrary,
    mode: OutputMode,
    strict: bool,
) -> anyhow::Result<()> {
    // Altium geometry is not yet threaded into the SI text checks, so pass None
    // there; the connectivity-based SI checks still run on `board`.
    let geo_text = if altium_present { None } else { Some(text) };
    let mut report = board.si_checks(geo_text);
    // Engine-layer SI checks whose attribution needs the bound DB models: trace
    // ampacity (current attribution + IPC-2221) and input-cap ripple (converter
    // topology + cap ripple rating). These augment the extract-layer SI report
    // exactly the way --lint augments its report with the strap lint.
    crate::checks::ampacity::append_ampacity(board, lib, geo_text, &mut report);
    crate::checks::ripple::append_ripple(board, lib, &mut report);
    match mode {
        OutputMode::Json => {
            let bound = bind_board(board, lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.findings = Some(si_findings_json(&report));
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => print!("{}", crate::plain_si(&report).render()),
        OutputMode::Text => print!("{}", hauksbee_extract::render_si(&report)),
    }
    if strict && si_fails(&report) {
        std::process::exit(2);
    }
    Ok(())
}
