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
    evidence: &crate::evidence::BoardEvidence,
    mode: OutputMode,
    strict: bool,
    inputs: &[JsonInputEvidence],
    blockers: &[String],
) -> anyhow::Result<()> {
    let report = crate::usb_c_report(board);
    // The INCONCLUSIVE refusal on every arm below: a CC verdict rests on part
    // identity (which resistor is a real Rd), so unbound verdict-critical
    // parts must be said out loud on this surface too, not only exit-coded
    // under --strict.
    // Scoped to the CC analysis: the CC verdict rests on the identity of the
    // parts on the receptacle's CC nets, so only unbound critical parts that
    // actually touch those nets can make THIS surface inconclusive. An
    // unrelated unmodelled MCU elsewhere on the board does not, and with no
    // receptacle at all there is no CC claim to qualify.
    let cc_blockers: Vec<String> = match crate::checks::usb_c::receptacle_cc_net_names(board) {
        Some(cc_nets) => blockers
            .iter()
            .filter(|reference| {
                board.components.iter().any(|c| {
                    c.reference == **reference
                        && c.pins.iter().any(|p| {
                            p.net
                                .and_then(|id| board.net(id))
                                .is_some_and(|n| cc_nets.contains(&n.name))
                        })
                })
            })
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let blockers = cc_blockers.as_slice();
    let inconclusive =
        (!blockers.is_empty()).then(|| crate::result::inconclusive_verdict(blockers));
    // Failure beats inconclusiveness, same precedence as JsonReport::verdict:
    // a serious CC fault stays verdict "fail"-shaped (and exits 2 under
    // --strict) even when bind blockers also exist; the INCONCLUSIVE sentence
    // still prints beside it.
    let serious = report.as_ref().is_some_and(|report| report.is_serious());
    match &report {
        None => {
            match mode {
                OutputMode::Json => {
                    let value = serde_json::json!({
                        "check": "usb_c_cc",
                        "level": "info",
                        "headline": "no USB-C receptacle detected"
                    });
                    let mut value = evidence.enrich_json(value);
                    value["inputs"] = serde_json::to_value(inputs)?;
                    // No receptacle: nothing to fail, and the CC blockers are
                    // scoped to audited nets, so none exist here either.
                    value["verdict"] = serde_json::Value::from("pass");
                    value["ok"] = serde_json::Value::from(true);
                    if let Some(note) = &inconclusive {
                        value["verdict"] = serde_json::Value::from("invalid");
                        value["ok"] = serde_json::Value::from(false);
                        value["coverage_note"] = serde_json::Value::from(note.clone());
                    }
                    println!("{}", serde_json::to_string(&value)?);
                }
                OutputMode::Plain | OutputMode::Text => {
                    println!("USB-C CC compliance: no USB-C receptacle with CC nets found on this board.");
                    if let Some(note) = &inconclusive {
                        println!("{note}");
                    }
                }
            }
        }
        Some(report) => match mode {
            OutputMode::Json => {
                let value: serde_json::Value = serde_json::from_str(&report.to_json())?;
                let mut value = evidence.enrich_json(value);
                value["inputs"] = serde_json::to_value(inputs)?;
                // The machine contract, unconditionally: fail beats invalid
                // beats pass, and a consumer can always gate on these fields
                // instead of re-deriving severity from the level.
                value["verdict"] = serde_json::Value::from(if serious {
                    "fail"
                } else if inconclusive.is_some() {
                    "invalid"
                } else {
                    "pass"
                });
                value["ok"] = serde_json::Value::from(!serious && inconclusive.is_none());
                if let Some(note) = &inconclusive {
                    value["coverage_note"] = serde_json::Value::from(note.clone());
                }
                println!("{}", serde_json::to_string(&value)?);
            }
            OutputMode::Plain => {
                print!("{}", report.render_plain());
                if let Some(note) = &inconclusive {
                    println!("{note}");
                }
            }
            OutputMode::Text => {
                print!("{}", report.render());
                if let Some(note) = &inconclusive {
                    println!("{note}");
                }
            }
        },
    }
    if !matches!(mode, OutputMode::Json) {
        print!("{}", evidence.render_plain());
    }
    super::note_ungated_findings(strict, serious);
    if strict && serious {
        let headline = &report.as_ref().expect("serious report exists").headline;
        super::strict_gate_exit(mode, &[format!("usb_c_cc {headline}")]);
    }
    // Same rule as the other model-dependent surfaces: strict exits 3 for
    // unbound verdict-critical parts, not for any open passive's per-net map.
    if strict && !blockers.is_empty() {
        std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS);
    }
    Ok(())
}
