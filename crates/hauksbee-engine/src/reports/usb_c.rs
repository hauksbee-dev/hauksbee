//! The `--usb-c` report: the USB-C CC-attach classifier (the RPi-4
//! shared-CC-pulldown re-derivation) and its compliance verdict, rendered in the
//! requested output mode. Only meaningful on a board carrying a USB-C receptacle
//! with CC nets; otherwise it reports "no receptacle" cleanly. Under `--strict` a
//! serious finding exits non-zero. CLI glue over the engine's `usb_c_report`.

use hauksbee_extract::ExtractedBoard;

use super::OutputMode;

/// Print the USB-C CC compliance report in `mode`, then (under `strict`) exit
/// non-zero on a serious finding.
pub fn emit(
    board: &ExtractedBoard,
    evidence: &crate::evidence::BoardEvidence,
    mode: OutputMode,
    strict: bool,
) -> anyhow::Result<()> {
    let report = crate::usb_c_report(board);
    match &report {
        None => {
            match mode {
                OutputMode::Json => {
                    let value = serde_json::json!({
                        "check": "usb_c_cc",
                        "level": "info",
                        "headline": "no USB-C receptacle detected"
                    });
                    println!("{}", evidence.enrich_json(value));
                }
                OutputMode::Plain | OutputMode::Text => {
                    println!("USB-C CC compliance: no USB-C receptacle with CC nets found on this board.");
                }
            }
        }
        Some(report) => match mode {
            OutputMode::Json => {
                let value: serde_json::Value = serde_json::from_str(&report.to_json())?;
                println!("{}", evidence.enrich_json(value));
            }
            OutputMode::Plain => print!("{}", report.render_plain()),
            OutputMode::Text => print!("{}", report.render()),
        },
    }
    if !matches!(mode, OutputMode::Json) {
        print!("{}", evidence.render_plain());
    }
    let serious = report.as_ref().is_some_and(|report| report.is_serious());
    super::note_ungated_findings(strict, serious);
    if strict && serious {
        let headline = &report.as_ref().expect("serious report exists").headline;
        super::strict_gate_exit(mode, &[format!("usb_c_cc {headline}")]);
    }
    if strict && evidence.is_undermined() {
        std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS);
    }
    Ok(())
}
