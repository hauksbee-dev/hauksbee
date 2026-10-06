//! Model-declared static control-pin contracts.
//!
//! PCB layouts often retain pad numbers and nets but not schematic pin names,
//! so the extraction-only floating-control check cannot know that pad 7 is a
//! HOLD input. A source-bound model may opt individual roles into
//! `params.must_not_float_roles`. This check then supplies only the missing
//! semantic map; the board still supplies the topology. It fires solely on the
//! same high-confidence signature as the extraction check: a named net whose
//! only member is that control pad. It does not guess bias polarity, firmware
//! mode, or behavior for models that did not opt in.
//!
//! A card lists in `params.float_tolerated_roles` a control input its
//! datasheet ignores at the part's default configuration (a flash write
//! protect that only acts once status-register protection is enabled). The
//! same topology there is a note citing `params.float_tolerated_basis`: a
//! floating CMOS input is still poor practice, but not a functional fault.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_models::ModelLibrary;

fn unconnected_placeholder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("unconnected-(") || lower.starts_with("unconnected_")
}

/// Find model-declared control pads that sit alone on a named net.
pub fn model_control_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for (comp, model) in super::resolved_components(board, lib) {
        let roles = |key| {
            model
                .params
                .get_str(key)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|role| !role.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let required = roles("must_not_float_roles");
        let tolerated = roles("float_tolerated_roles");
        let contracts = required
            .iter()
            .map(|role| (role, true))
            .chain(tolerated.iter().map(|role| (role, false)));
        for (role, must) in contracts {
            for (pad, model_role) in &model.pins {
                if !model_role.eq_ignore_ascii_case(role) {
                    continue;
                }
                let Some(pin) = comp.pins.iter().find(|pin| pin.number == *pad) else {
                    continue;
                };
                let Some(net_id) = pin.net.filter(|id| *id != 0) else {
                    continue;
                };
                let Some(net) = board.net(net_id) else {
                    continue;
                };
                if unconnected_placeholder(&net.name) || board.net_members(net_id).len() != 1 {
                    continue;
                }
                let (severity, consequence) = if must {
                    (
                        Severity::High,
                        "the model requires this role not to float".to_string(),
                    )
                } else {
                    let basis = model
                        .params
                        .get_str("float_tolerated_basis")
                        .unwrap_or("the datasheet ignores it at the part's default configuration");
                    (
                        Severity::Low,
                        format!("harmless as shipped ({basis}), but tie it to its inactive rail so it cannot pick up noise or act if the configuration changes"),
                    )
                };
                report.findings.push(LintFinding {
                    check: LintCheck::FloatingControlPin,
                    severity,
                    message: format!(
                        "{} pad {} ({role}, from model '{}') is a control input on floating net '{}': the net touches only this pad; {consequence}",
                        comp.reference, pin.number, model.id, net.name
                    ),
                    refs: vec![comp.reference.clone()],
                    nets: vec![net.name.clone()],
                });
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Component, Net, Pin};

    fn flash_board(fixed: bool) -> ExtractedBoard {
        let mut pins = Vec::new();
        for (number, net) in [
            ("1", 10),
            ("2", 11),
            ("3", if fixed { 2 } else { 12 }),
            ("4", 1),
            ("5", 13),
            ("6", 14),
            ("7", if fixed { 2 } else { 15 }),
            ("8", 2),
        ] {
            pins.push(Pin {
                number: number.into(),
                net: Some(net),
                ..Default::default()
            });
        }
        ExtractedBoard {
            name: "w25q-control-contract".into(),
            nets: vec![
                Net::new(1, "GND"),
                Net::new(2, "+3V3"),
                Net::new(10, "SPI_CS"),
                Net::new(11, "SPI_MISO"),
                Net::new(12, "Net-(U6-Pad3)"),
                Net::new(13, "SPI_MOSI"),
                Net::new(14, "SPI_SCK"),
                Net::new(15, "Net-(U6-Pad7)"),
            ],
            components: vec![Component {
                reference: "U6".into(),
                value: "W25Q80DVS".into(),
                footprint: "LibreSolar:SOIC-8_3.9x4.9mm_Pitch1.27mm".into(),
                layer: "Top".into(),
                pins,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn exact_flash_model_supplies_roles_to_pcb_only_topology() {
        let report = model_control_lint(&flash_board(false), &ModelLibrary::builtin());
        let findings: Vec<_> = report.of_check(LintCheck::FloatingControlPin).collect();
        assert_eq!(findings.len(), 2, "WP and HOLD must both be localized");
        // HOLD freezes the bus when it drifts low: a fault. WP only acts once
        // status-register protection is enabled: a note.
        for (role, severity) in [("hold_n", Severity::High), ("wp_n", Severity::Low)] {
            let found = findings.iter().find(|f| f.message.contains(role));
            assert_eq!(
                found.map(|f| f.severity),
                Some(severity),
                "{role}: {findings:?}"
            );
        }
    }

    #[test]
    fn rail_tied_flash_controls_are_clean() {
        let report = model_control_lint(&flash_board(true), &ModelLibrary::builtin());
        assert_eq!(report.of_check(LintCheck::FloatingControlPin).count(), 0);
    }

    #[test]
    fn no_model_contract_means_abstain() {
        let mut board = flash_board(false);
        board.components[0].value = "UNKNOWN_FLASH".into();
        let report = model_control_lint(&board, &ModelLibrary::builtin());
        assert_eq!(report.of_check(LintCheck::FloatingControlPin).count(), 0);
    }
}
