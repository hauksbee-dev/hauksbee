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
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_models::ModelLibrary;

use crate::binder::resolve;

fn unconnected_placeholder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("unconnected-(") || lower.starts_with("unconnected_")
}

/// Find model-declared control pads that sit alone on a named net.
pub fn model_control_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let Some(model) = resolve(lib, part).model else {
            continue;
        };
        let Some(raw_roles) = model.params.get_str("must_not_float_roles") else {
            continue;
        };
        for role in raw_roles
            .split(',')
            .map(str::trim)
            .filter(|role| !role.is_empty())
        {
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
                report.findings.push(LintFinding {
                    check: LintCheck::FloatingControlPin,
                    severity: Severity::High,
                    message: format!(
                        "{} pad {} ({role}, from model '{}') is a control input on floating net '{}': the net touches only this pad; the model requires this role not to float",
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
                function: String::new(),
                kind: String::new(),
                position: None,
            });
        }
        ExtractedBoard {
            name: "w25q-control-contract".into(),
            nets: vec![
                Net {
                    id: 1,
                    name: "GND".into(),
                },
                Net {
                    id: 2,
                    name: "+3V3".into(),
                },
                Net {
                    id: 10,
                    name: "SPI_CS".into(),
                },
                Net {
                    id: 11,
                    name: "SPI_MISO".into(),
                },
                Net {
                    id: 12,
                    name: "Net-(U6-Pad3)".into(),
                },
                Net {
                    id: 13,
                    name: "SPI_MOSI".into(),
                },
                Net {
                    id: 14,
                    name: "SPI_SCK".into(),
                },
                Net {
                    id: 15,
                    name: "Net-(U6-Pad7)".into(),
                },
            ],
            components: vec![Component {
                reference: "U6".into(),
                value: "W25Q80DVS".into(),
                lib_id: String::new(),
                footprint: "LibreSolar:SOIC-8_3.9x4.9mm_Pitch1.27mm".into(),
                position: None,
                layer: "Top".into(),
                properties: Vec::new(),
                dnp: false,
                pins,
            }],
        }
    }

    #[test]
    fn exact_flash_model_supplies_roles_to_pcb_only_topology() {
        let report = model_control_lint(&flash_board(false), &ModelLibrary::builtin());
        let findings: Vec<_> = report.of_check(LintCheck::FloatingControlPin).collect();
        assert_eq!(findings.len(), 2, "WP and HOLD must both be localized");
        assert!(findings
            .iter()
            .any(|finding| finding.message.contains("wp_n")));
        assert!(findings
            .iter()
            .any(|finding| finding.message.contains("hold_n")));
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
