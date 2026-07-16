//! The `--si` report: the signal-integrity / physics static checks from the
//! extractor, augmented with the engine-layer checks whose attribution needs the
//! bound database models — trace ampacity (IPC-2221) and input-cap ripple. It
//! renders in the requested mode and, under `--strict`, exits non-zero on a real
//! finding. CLI glue over the extract- and engine-layer SI checks.

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
    // The single SI chokepoint: extract-layer SI checks plus the engine-layer
    // trace-ampacity + input-cap-ripple checks. Shared with `--check`, the JSON
    // aggregate, the TUI and the web front door so every SI surface runs the
    // identical set (the augmentation used to live only here).
    let report = crate::checks::engine_si(board, lib, geo_text);
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
