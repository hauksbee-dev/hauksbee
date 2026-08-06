//! The `--usb-c` report: the USB-C CC-attach classifier (the RPi-4
//! shared-CC-pulldown re-derivation) and its compliance verdict, rendered in the
//! requested output mode. Only meaningful on a board carrying a USB-C receptacle
//! with CC nets; otherwise it reports "no receptacle" cleanly. Under `--strict` a
//! serious finding exits non-zero. CLI glue over the engine's `usb_c_report`.

use hauksbee_extract::ExtractedBoard;

use crate::result::JsonInputEvidence;

use super::OutputMode;

/// Print the USB-C CC compliance report in `mode`, then (under `strict`) exit
/// non-zero on a serious finding.
pub fn emit(
    board: &ExtractedBoard,
    mode: OutputMode,
    strict: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    match crate::usb_c_report(board) {
        None => {
            match mode {
                OutputMode::Json => {
                    let mut value = serde_json::json!({
                        "check": "usb_c_cc",
                        "level": "info",
                        "headline": "no USB-C receptacle detected"
                    });
                    value["inputs"] = serde_json::to_value(inputs)?;
                    println!("{}", serde_json::to_string(&value)?);
                }
                OutputMode::Plain | OutputMode::Text => {
                    println!("USB-C CC compliance: no USB-C receptacle with CC nets found on this board.");
                }
            }
        }
        Some(report) => {
            match mode {
                OutputMode::Json => {
                    let mut value: serde_json::Value = serde_json::from_str(&report.to_json())?;
                    value["inputs"] = serde_json::to_value(inputs)?;
                    println!("{}", serde_json::to_string(&value)?);
                }
                OutputMode::Plain => print!("{}", report.render_plain()),
                OutputMode::Text => print!("{}", report.render()),
            }
            super::note_ungated_findings(strict, report.is_serious());
            if strict && report.is_serious() {
                super::strict_gate_exit(mode, &[format!("usb_c_cc {}", report.headline)]);
            }
        }
    }
    Ok(())
}
