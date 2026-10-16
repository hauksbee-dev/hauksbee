//! The `--usb-c` report: the USB-C CC-attach classifier and its compliance verdict.
//!
//! The classifier is the RPi-4 shared-CC-pulldown re-derivation, rendered in the
//! requested output mode. Only meaningful on a board carrying a USB-C receptacle
//! with CC nets; otherwise it reports "no receptacle" cleanly. Under `--strict` a
//! serious finding exits non-zero. CLI glue over the engine's `usb_c_report`.

use hauksbee_extract::ExtractedBoard;

use crate::result::{BindSummary, JsonInputEvidence, JsonReport, Refusal};

use super::OutputMode;

pub(crate) fn scoped_blockers(board: &ExtractedBoard, blockers: &[String]) -> Vec<String> {
    crate::checks::usb_c::receptacle_cc_net_names(board).map_or_else(Vec::new, |cc_nets| {
        blockers
            .iter()
            .filter(|reference| {
                board.components.iter().any(|component| {
                    component.reference == **reference
                        && component.pins.iter().any(|pin| {
                            pin.net
                                .and_then(|id| board.net(id))
                                .is_some_and(|net| cc_nets.contains(&net.name))
                        })
                })
            })
            .cloned()
            .collect()
    })
}

/// Print the USB-C CC compliance report in `mode`, then (under `strict`) exit
/// non-zero on a serious finding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_quiet(
    board: &ExtractedBoard,
    evidence: &crate::evidence::BoardEvidence,
    mode: OutputMode,
    strict: bool,
    quiet: bool,
    inputs: &[JsonInputEvidence],
    bind: &BindSummary,
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
    let cc_blockers = scoped_blockers(board, blockers);
    let blockers = cc_blockers.as_slice();
    let inconclusive =
        (!blockers.is_empty()).then(|| crate::result::inconclusive_verdict(blockers));
    // Failure beats inconclusiveness, same precedence as JsonReport::verdict:
    // a serious CC fault stays verdict "fail"-shaped (and exits 2 under
    // --strict) even when bind blockers also exist; the INCONCLUSIVE sentence
    // still prints beside it.
    let serious = matches!(&report, Some(r) if r.is_serious());
    let cc_nets = crate::checks::usb_c::receptacle_cc_net_names(board)
        .map(|nets| nets.into_iter().collect::<Vec<_>>())
        .unwrap_or_default();
    let human_maps = evidence.maps_for_surface_nets("USB-C CC input coverage", &cc_nets)?;
    let human_evidence = evidence.clone().with_maps(human_maps);
    // The machine verdict, from the same `JsonReport` derivation every other
    // surface uses (and which tells the CI artifacts the verdict, in every
    // output mode). The bind gate is not used: it would read ALL unbound
    // critical parts, and this surface is scoped to the CC nets, so the scoped
    // blockers ride a refusal instead. Failure still beats inconclusiveness
    // (`VerdictCause::pick`), exactly as the printed INCONCLUSIVE sentence
    // sits beside a serious fault.
    let mut machine = JsonReport::new(&board.name, bind.clone())
        .with_surface_gate(serious)
        .with_inputs(inputs);
    machine.findings = Some(
        report
            .as_ref()
            .and_then(crate::result::usbc_finding_json)
            .into_iter()
            .collect(),
    );
    if let Some(note) = &inconclusive {
        machine.refusal = Some(Refusal::new(
            "a USB-C CC compliance verdict",
            note.clone(),
            Vec::<String>::new(),
            "give the parts on the receptacle's CC nets a model, then rerun",
        ));
    }
    let run_verdict = machine.run_verdict();
    let machine_ok = run_verdict.verdict == "pass";
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
                    value["verdict"] = serde_json::Value::from(run_verdict.verdict);
                    value["ok"] = serde_json::Value::from(machine_ok);
                    if let Some(note) = &inconclusive {
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
                value["verdict"] = serde_json::Value::from(run_verdict.verdict);
                value["ok"] = serde_json::Value::from(machine_ok);
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
        print!(
            "{}",
            super::render_evidence_appendix(&human_evidence, quiet, false)
        );
    }
    super::note_ungated_findings(strict, serious);
    if strict && serious {
        let headline = &report.as_ref().expect("serious report exists").headline;
        super::strict_gate_exit(mode, &[format!("usb_c_cc {headline}")]);
    }
    // Same rule as the other model-dependent surfaces: strict exits 3 for
    // unbound verdict-critical parts, not for any open passive's per-net map.
    if strict && !blockers.is_empty() {
        super::exit_invalid_for_analysis(blockers);
    }
    Ok(())
}
