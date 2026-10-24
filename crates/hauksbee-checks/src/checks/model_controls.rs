//! Model-declared static control-pin contracts.
//!
//! PCB layouts often retain pad numbers and nets but not schematic pin names,
//! so the extraction-only floating-control check cannot know that pad 7 is a
//! HOLD input. A source-bound model may opt individual roles into
//! `params.must_not_float_roles`: the datasheet gives the pin no internal pull
//! and says it must be driven or tied (an ESP32 chip enable, a flash HOLD). A
//! part whose datasheet gives the pin an internal pull (an AVR, STM32, SAM or
//! RP2040 reset) does not opt in, and its pin is not judged here. This check
//! supplies only the missing semantic map; the board still supplies the
//! topology. It fires when nothing on the net sets the pin's level: the net
//! holds only that pad, or only parts that cannot hold a level on their own: a
//! capacitor (the C of a reset RC), a button or switch, a connector or test
//! pad (nothing while unplugged), an NPN collector or N-channel drain (it only
//! pulls low, and only when on), or another part's enable or reset input. Any
//! other part on the net (a resistor, a driver, a pin no rule knows) may set
//! the level, and the pin is not reported. It does not guess bias polarity,
//! firmware mode, or behavior for models that did not opt in.
//!
//! A card lists in `params.float_tolerated_roles` a control input its
//! datasheet ignores at the part's default configuration (a flash write
//! protect that only acts once status-register protection is enabled). The
//! same topology there is a note citing `params.float_tolerated_basis`: a
//! floating CMOS input is still poor practice, but not a functional fault.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use std::collections::HashMap;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, drives_a_level, is_capacitor, is_connector_like, is_test_point_or_hole,
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity,
};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::floating_inputs::control_lint_covers;

fn unconnected_placeholder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("unconnected-(") || lower.starts_with("unconnected_")
}

/// How a pad that cannot hold a net's level on its own reads in a finding, or
/// `None` for a pad that may set the level (a resistor, a driver, a pin no rule
/// knows).
fn holds_no_level(c: &Component, p: &Pin, models: &HashMap<&str, &ModelEntry>) -> Option<String> {
    let class = RefClass::of(&c.reference);
    if is_capacitor(c) {
        return Some(c.reference.clone());
    }
    // A plug-in module's output pad drives the net though its footprint reads
    // as a board-to-board connector; a header or plug pad holds nothing.
    if class == RefClass::Switch || (is_connector_like(c) && !drives_a_level(c, p)) {
        return Some(format!("{} pin {}", c.reference, p.number));
    }
    if class == RefClass::Transistor {
        return low_side_sink(c, p, models.get(c.reference.as_str()).copied());
    }
    let input = p.kind.trim().eq_ignore_ascii_case("input");
    (input && matches!(control_role(&p.function), Some("enable" | "reset")))
        .then(|| format!("{} pin {} ({})", c.reference, p.number, p.function.trim()))
}

/// How a transistor pad reads when it can only pull its net low (an NPN
/// collector or N-channel drain: it sinks while on and lets go while off), or
/// `None` for any other pad, which may hold the net high (a PNP collector, a
/// P-channel drain, a base or gate).
pub(super) fn low_side_sink(c: &Component, p: &Pin, model: Option<&ModelEntry>) -> Option<String> {
    let sinks = model.map_or_else(
        || {
            let text = format!("{} {}", c.value, c.lib_id).to_ascii_uppercase();
            ["NPN", "NMOS", "N-CH", "NFET"]
                .iter()
                .any(|k| text.contains(k))
        },
        |m| matches!(m.kind, ComponentKind::BjtNpn | ComponentKind::Nmos),
    );
    // The card's role for the pad (by number, then by pin name), else the
    // symbol's pin name: `collector`, `drain_q2`, `C`, `D`.
    let role = model
        .and_then(|m| {
            m.pins
                .get(&p.number)
                .or_else(|| m.pins.get(p.function.trim()))
        })
        .map_or_else(|| p.function.trim(), String::as_str)
        .to_ascii_lowercase();
    match role.split('_').next().unwrap_or_default() {
        "c" | "collector" if sinks => Some(format!("{}'s collector", c.reference)),
        "d" | "drain" if sinks => Some(format!("{}'s drain", c.reference)),
        _ => None,
    }
}

/// Find model-declared control pads on a net nothing holds at a level.
pub fn model_control_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let resolved: Vec<_> = super::resolved_components(board, lib).collect();
    let models: HashMap<&str, &ModelEntry> = resolved
        .iter()
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    for (comp, model) in &resolved {
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
                let name = &net.name;
                // A named control pin alone on its net is the extract lint's.
                if unconnected_placeholder(name) || control_lint_covers(board, comp, pin) {
                    continue;
                }
                // Every other fitted pad on the net, by what it does to the level.
                let mut held_by = Vec::new();
                let mut defined = false;
                for (o, q) in board.net_members(net_id) {
                    let this = std::ptr::eq(o, *comp) && std::ptr::eq(q, pin);
                    if this || !AssemblyState::of(o).is_present() || is_test_point_or_hole(o) {
                        continue;
                    }
                    match holds_no_level(o, q, &models) {
                        Some(label) => held_by.push(label),
                        None => defined = true,
                    }
                }
                if defined {
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
                let topology = if held_by.is_empty() {
                    "the net touches only this pad".to_string()
                } else {
                    format!(
                        "the net holds only {}, and none of them sets a level while no button is pressed and nothing is plugged in",
                        held_by.join(", ")
                    )
                };
                report.findings.push(LintFinding {
                    check: LintCheck::FloatingControlPin,
                    severity,
                    message: format!(
                        "{} pad {} ({role}, from model '{}') is a control input on floating net '{name}': {topology}; {consequence}",
                        comp.reference, pin.number, model.id
                    ),
                    refs: vec![comp.reference.clone()],
                    nets: vec![name.clone()],
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

    use crate::test_support::{board, pin, pin_fn};

    const GND: i64 = 1;
    const V3: i64 = 2;
    const EN: i64 = 10;

    fn fitted(reference: &str, value: &str, lib_id: &str, pins: Vec<Pin>) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: lib_id.into(),
            layer: "F.Cu".into(),
            pins,
            ..Default::default()
        }
    }

    /// The usual ESP32-S3 auto-reset corner without its pull-up: the module's
    /// EN (pad 3) with the RC's capacitor, a reset button, the USB-serial
    /// bridge's NPN, a header pin, a test pad, a do-not-populate 10k and a
    /// second board's `CHIP_PU` input.
    fn esp32s3_enable(extra: Vec<Component>) -> Vec<LintFinding> {
        let mut input = pin_fn("3", EN, "CHIP_PU");
        input.kind = "input".into();
        let mut dnp_pull = fitted("R1", "10k", "Device:R", vec![pin("1", V3), pin("2", EN)]);
        dnp_pull.dnp = true;
        let mut parts = vec![
            fitted(
                "U1",
                "ESP32-S3-WROOM-1",
                "RF_Module:ESP32-S3-WROOM-1",
                vec![pin("1", GND), pin("2", V3), pin_fn("3", EN, "EN")],
            ),
            fitted("C4", "1u", "Device:C", vec![pin("1", EN), pin("2", GND)]),
            fitted(
                "SW1",
                "SW_Push",
                "Switch:SW_Push",
                vec![pin("1", GND), pin("2", EN)],
            ),
            fitted(
                "Q1",
                "Q_NPN_BCE",
                "Device:Q_NPN_BCE",
                vec![
                    pin_fn("1", 20, "B"),
                    pin_fn("2", EN, "C"),
                    pin_fn("3", 21, "E"),
                ],
            ),
            fitted(
                "J1",
                "Conn_01x04",
                "Connector:Conn_01x04",
                vec![pin("2", EN)],
            ),
            fitted(
                "TP1",
                "TestPoint",
                "Connector:TestPoint",
                vec![pin("1", EN)],
            ),
            dnp_pull,
            fitted(
                "U2",
                "ESP32-S3-DevKitC",
                "Espressif:ESP32-S3-DevKitC",
                vec![input],
            ),
        ];
        parts.extend(extra);
        let nets = [
            (GND, "GND"),
            (V3, "+3V3"),
            (EN, "/EN"),
            (20, "/DTR"),
            (21, "/RTS"),
        ];
        model_control_lint(&board("t", &nets, parts), &ModelLibrary::builtin())
            .of_check(LintCheck::FloatingControlPin)
            .cloned()
            .collect()
    }

    #[test]
    fn an_enable_held_only_by_parts_that_set_no_level_floats() {
        let found = esp32s3_enable(vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["U1"]);
        for named in [
            "C4",
            "SW1 pin 2",
            "Q1's collector",
            "J1 pin 2",
            "U2 pin 3 (CHIP_PU)",
        ] {
            assert!(f.message.contains(named), "{named}: {}", f.message);
        }
        // A test pad and an unfitted resistor are not on the assembled net.
        assert!(
            !f.message.contains("TP1") && !f.message.contains("R1"),
            "{}",
            f.message
        );
    }

    #[test]
    fn a_fitted_pull_or_a_high_side_transistor_sets_the_enable() {
        let pull = fitted("R2", "10k", "Device:R", vec![pin("1", V3), pin("2", EN)]);
        assert!(esp32s3_enable(vec![pull]).is_empty());
        // A PNP collector may hold the net high; that is a driver, not a float.
        let pnp = fitted(
            "Q2",
            "Q_PNP_BCE",
            "Device:Q_PNP_BCE",
            vec![
                pin_fn("1", 22, "B"),
                pin_fn("2", EN, "C"),
                pin_fn("3", V3, "E"),
            ],
        );
        assert!(esp32s3_enable(vec![pnp]).is_empty());
    }

    /// An ESP32-S3 enable held only by a board-to-board part: a compute module
    /// (footprint reads as a connector) whose pad is typed `kind`, or a bare
    /// header.
    fn enable_held_by(value: &str, lib_id: &str, kind: &str) -> usize {
        let mut driver = pin_fn("75", EN, "SD_PWR_ON");
        driver.kind = kind.into();
        let mut module = fitted("Module1", value, lib_id, vec![driver]);
        module.footprint = "Connector_Hirose:Hirose_DF40C-100DS".into();
        let u1 = fitted(
            "U1",
            "ESP32-S3-WROOM-1",
            "RF_Module:ESP32-S3-WROOM-1",
            vec![pin("1", GND), pin("2", V3), pin_fn("3", EN, "EN")],
        );
        let b = board(
            "t",
            &[(GND, "GND"), (V3, "+3V3"), (EN, "/SD_PWR_ON")],
            vec![u1, module],
        );
        model_control_lint(&b, &ModelLibrary::builtin())
            .of_check(LintCheck::FloatingControlPin)
            .count()
    }

    #[test]
    fn a_module_output_pad_drives_the_enable_but_a_header_pin_holds_nothing() {
        // The compute module's output is a driver: its own sequencer sets the level.
        assert_eq!(enable_held_by("CM4", "Module:CM4", "output"), 0);
        // A module whose symbol types the pad passive is still a module by name.
        assert_eq!(
            enable_held_by("Compute Module 4", "Module:Raspberry_Pi_CM4", "passive"),
            0
        );
        // Control: a plain header pin (passive) holds no level, so the enable floats.
        assert_eq!(
            enable_held_by("Conn_01x02", "Connector:Conn_01x02", "passive"),
            1
        );
        // Control: a connector pad typed input is not a driver either.
        assert_eq!(enable_held_by("CM4", "Module:CM4", "input"), 1);
    }

    #[test]
    fn a_named_enable_alone_on_its_net_is_reported_once() {
        let u1 = fitted(
            "U1",
            "ESP32-S3-WROOM-1",
            "RF_Module:ESP32-S3-WROOM-1",
            vec![pin("1", GND), pin("2", V3), pin_fn("3", EN, "EN")],
        );
        let b = board("t", &[(GND, "GND"), (V3, "+3V3"), (EN, "/EN")], vec![u1]);
        let here = model_control_lint(&b, &ModelLibrary::builtin());
        assert_eq!(here.of_check(LintCheck::FloatingControlPin).count(), 0);
        let extract = b.net_lint();
        assert_eq!(extract.of_check(LintCheck::FloatingControlPin).count(), 1);
    }

    #[test]
    fn a_reset_with_an_internal_pull_is_not_judged() {
        // RP2040 RUN (pad 26) has an on-die pull-up (datasheet Table 619), so
        // its card does not opt RUN in: a button and a cap alone are fine.
        let mut pins: Vec<Pin> = (1..=57).map(|n| pin(&n.to_string(), 30 + n)).collect();
        pins[25] = pin_fn("26", EN, "RUN");
        let parts = vec![
            fitted("U1", "RP2040", "QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm", pins),
            fitted("C1", "100n", "Device:C", vec![pin("1", EN), pin("2", GND)]),
            fitted(
                "SW1",
                "SW_Push",
                "Switch:SW_Push",
                vec![pin("1", GND), pin("2", EN)],
            ),
        ];
        let b = board("t", &[(GND, "GND"), (EN, "/RUN")], parts);
        let lib = ModelLibrary::builtin();
        let rp2040 =
            crate::checks::resolved_components(&b, &lib).find(|(c, _)| c.reference == "U1");
        assert!(rp2040.is_some_and(|(_, m)| m.pins.get("26").is_some_and(|r| r == "run")));
        let report = model_control_lint(&b, &lib);
        assert_eq!(report.of_check(LintCheck::FloatingControlPin).count(), 0);
    }

    /// ADXL345 CS (pad 7) and SDO/ALT ADDRESS (pad 12) have no internal pull
    /// (ADXL345 Rev. G, Serial Communications): left open they float. A net an
    /// MCU pad drives is driven once firmware runs, whatever the pad is called
    /// and however the symbol types it, so the SPI chip select an MCU GPIO
    /// drives and the address pin an MCU output sets are not floating.
    #[test]
    fn a_control_pin_an_mcu_output_drives_is_not_floating_but_an_open_one_is() {
        const CS: i64 = 11;
        const SDO: i64 = 12;
        let mcu = |cs_kind: &str, sdo_kind: &str| {
            let mut cs = pin_fn("10", CS, "PA4");
            cs.kind = cs_kind.into();
            let mut sdo = pin_fn("11", SDO, "PA6");
            sdo.kind = sdo_kind.into();
            fitted(
                "U1",
                "STM32F103C8T6",
                "Package_QFP:LQFP-48_7x7mm_P0.5mm",
                vec![pin("1", V3), pin("8", GND), cs, sdo],
            )
        };
        let sensor = |cs: i64, sdo: i64| {
            fitted(
                "U2",
                "ADXL345",
                "Package_LGA:LGA-14_3x5mm_P0.8mm",
                vec![
                    pin("1", V3),
                    pin("2", GND),
                    pin("6", V3),
                    pin("7", cs),
                    pin("12", sdo),
                ],
            )
        };
        let run = |parts: Vec<Component>| {
            let nets = [
                (GND, "GND"),
                (V3, "+3V3"),
                (CS, "/SPI_CS"),
                (SDO, "SENSOR_SDO"),
            ];
            model_control_lint(&board("t", &nets, parts), &ModelLibrary::builtin())
                .of_check(LintCheck::FloatingControlPin)
                .cloned()
                .collect::<Vec<_>>()
        };
        // The chip select on a bidirectional MCU pad, the address pin on an
        // MCU pad typed as an input (symbols type output-capable pads so).
        for (cs_kind, sdo_kind) in [
            ("bidirectional", "bidirectional"),
            ("output", "input"),
            ("", ""),
        ] {
            let found = run(vec![mcu(cs_kind, sdo_kind), sensor(CS, SDO)]);
            assert!(found.is_empty(), "{cs_kind}/{sdo_kind}: {found:#?}");
        }
        // Wired to nothing else, both float.
        let open = run(vec![sensor(CS, SDO)]);
        assert_eq!(open.len(), 2, "{open:#?}");
        assert!(open.iter().all(|f| f.severity == Severity::High));
    }

    /// ADS122C04 RESET has no internal pull (TI SBAS751B sec 9.1.3: "Tie the
    /// RESET pin to DVDD if the RESET pin is not used"). A breakout that brings
    /// it to a header with no pull-up floats it whenever nothing is plugged in;
    /// with its pull-up fitted it is held high.
    #[test]
    fn a_reset_without_an_internal_pull_floats_without_its_pull_up() {
        const VCC: i64 = 3;
        let run = |with_pull: bool| {
            let mut parts = vec![
                fitted(
                    "IC1",
                    "ADC_ADS122C04RTE",
                    "WQFN16",
                    vec![pin("1", EN), pin("2", GND), pin("11", VCC)],
                ),
                fitted(
                    "JP2",
                    "",
                    "1X06_ROUND_70",
                    vec![pin("6", EN), pin("1", GND)],
                ),
            ];
            if with_pull {
                parts.push(fitted(
                    "R4",
                    "10K",
                    "0603",
                    vec![pin("1", VCC), pin("2", EN)],
                ));
            }
            let nets = [(GND, "GND"), (VCC, "VCC"), (EN, "!RESET")];
            model_control_lint(&board("t", &nets, parts), &ModelLibrary::builtin())
                .of_check(LintCheck::FloatingControlPin)
                .cloned()
                .collect::<Vec<_>>()
        };
        let found = run(false);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        for needle in [
            "IC1 pad 1",
            "reset_n",
            "ads122c04_rte",
            "!RESET",
            "JP2 pin 6",
        ] {
            assert!(
                found[0].message.contains(needle),
                "{needle}: {}",
                found[0].message
            );
        }
        assert!(run(true).is_empty(), "the fitted pull-up holds RESET high");
    }
}
