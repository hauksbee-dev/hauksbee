//! Back-powering / cross-domain pull lint: a modelled part's signal pin tied
//! or pulled up to a rail ABOVE the part's own supply domain.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The physics: virtually every IC input carries a protection clamp diode from
//! the pin to its supply rail. Pull a pin to a rail higher than the part's
//! VDD + one diode drop and the clamp conducts: current flows from the higher
//! rail *through the part* into the lower rail. Two field failures follow:
//!
//! 1. **Back-powering.** With the part's own rail unpowered (main supply off,
//!    a USB-UART adapter still plugged in), the higher rail feeds the "off"
//!    rail through the clamp, holding the whole domain half-alive: parts brown
//!    out instead of resetting, flash writes corrupt, and the current path was
//!    never designed to carry the load.
//! 2. **Abs-max violation.** Powered up, the pin sits past the datasheet's
//!    VCC + 0.3..0.5 V input abs-max. Some pins are explicitly tolerant (a
//!    5 V-tolerant STM32 FT pin); everything else is slow damage or latch-up.
//!
//! Cross-layer, per the C6.1 audit: the fault spans the POWER layer (which
//! rail feeds which part) and the SIGNAL layer (which net pulls where); each
//! layer alone looks clean. A 5 V-pulled I2C bus wired to a 3.3 V-supplied
//! sensor routes fine, ERC passes, and every part is individually healthy.
//!
//! ## Evidence the check rests on
//!
//! Only decision-grade inputs fire a finding:
//! - The part's supply domain comes from its OWN model pin map: a supply-role
//!   pad (`vcc`/`vdd`/`avcc`/`vdda`/`vddio`/`vio`/`5v`) whose net resolves
//!   through [`power_rail_voltage`], the binder's one rail-name table, so this
//!   check and the co-sim's supply stamping cannot disagree about a rail. A
//!   part with several resolvable supply pads uses the HIGHEST (a dual-domain
//!   part is judged against its most permissive rail, failing toward silence).
//! - The offending pull is a plain assembled resistor from the pin's net to a
//!   resolvable rail, or the pin's net IS a resolvable rail (a direct tie).
//!
//! A part whose supply nets the rail table cannot resolve, a pull to an
//! unresolvable net, or a missing model all stay silent: no guessed voltage
//! may support a finding.
//!
//! ## Severity calibration
//!
//! - Direct tie of a signal pin to a higher rail: **High**. There is no series
//!   element; the clamp carries whatever the rail sources.
//! - Pull-up resistor to a higher rail: **Medium**. The resistor bounds the
//!   clamp current, and higher-voltage-tolerant pins make this idiom
//!   legitimate on SOME parts, but the model db does not carry per-pin
//!   tolerance, so the finding says what the clamp path is and names the
//!   exception the reader must verify.
//!
//! Pins the check deliberately skips: supply and ground roles (they belong on
//! rails), and roles that are higher-voltage inputs by design (`vin`, `vbus`,
//! `vbat`, `vsys`, `raw`, `vmot` feed regulators/monitors rated for it).

use std::collections::BTreeSet;

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::ModelLibrary;

use crate::binder::{power_rail_voltage, resolve};
use hauksbee_extract::assembly::AssemblyState;

/// Margin (V) a rail must exceed the part's supply by before the finding
/// fires: one clamp-diode drop, the threshold at which the protection clamp
/// actually CONDUCTS and the back-powering current path exists. This is
/// deliberately keyed off conduction, not the tighter VCC + 0.3..0.5 V
/// abs-max window: rails 0.3..0.6 V above the supply may violate a
/// datasheet's abs-max without conducting meaningfully, and firing there on
/// name-derived nominal rail voltages would trade the check's silence
/// calibration for a band no field failure in the corpus occupies. The
/// finding therefore claims conduction, and the under-reported band is
/// documented here rather than hidden.
const CLAMP_MARGIN_V: f64 = 0.6;

/// Roles that define the part's supply domain (mirrors the scheduler's
/// direct-supply role list for its rail watches).
fn is_supply_role(role: &str) -> bool {
    matches!(
        role,
        "vcc" | "avcc" | "vdd" | "vdda" | "vddio" | "vio" | "5v"
    )
}

/// Roles never judged as signal pins: supplies, grounds, and inputs that are
/// higher-voltage by design (regulator/monitor feeds).
fn is_skipped_role(role: &str) -> bool {
    is_supply_role(role)
        || role.starts_with("gnd")
        || matches!(role, "vss" | "agnd" | "dgnd" | "pgnd" | "gndio" | "nc")
        || matches!(role, "vin" | "vbus" | "vbat" | "vsys" | "raw" | "vmot")
}

/// A plain two-terminal, assembled resistor (ref R*, not RV/RT/RN/RP/RM).
/// Value parse is not needed here: the pull's EXISTENCE and its far rail are
/// the evidence; its ohms only scale the clamp current.
fn is_plain_resistor(c: &Component) -> bool {
    // Three-state contract: a DNP pull-up is absent and an identity-refused
    // record is unprovable; neither may count as a rail tie.
    if !AssemblyState::of(c).is_present() {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM")
        && c.pins.iter().filter(|p| p.net.is_some()).count() == 2
        && !lib.contains("ferrite")
        && !lib.contains("inductor")
}

/// The part's supply-domain voltage: the highest resolvable rail among its
/// supply-role pads, with the rail's net name. `None` when no supply pad
/// resolves (the check then has no domain to judge against and stays silent).
fn supply_domain(
    board: &ExtractedBoard,
    comp: &Component,
    model_pins: &std::collections::BTreeMap<String, String>,
) -> Option<(f64, String)> {
    let mut best: Option<(f64, String)> = None;
    for (pad, role) in model_pins {
        if !is_supply_role(role) {
            continue;
        }
        let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
            continue;
        };
        let Some(id) = pin.net.filter(|&id| id != 0) else {
            continue;
        };
        let Some(net) = board.net(id) else { continue };
        let Some(v) = power_rail_voltage(&net.name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| v > *b) {
            best = Some((v, net.name.clone()));
        }
    }
    best
}

/// Pull-up resistors from `net_id` to a resolvable rail: `(resistor ref,
/// rail net name, rail volts)`, one entry per resistor.
fn pullups_to_rails(board: &ExtractedBoard, net_id: i64) -> Vec<(String, String, f64)> {
    let mut out = Vec::new();
    for (c, _p) in board.net_members(net_id) {
        if !is_plain_resistor(c) {
            continue;
        }
        for op in &c.pins {
            let Some(oid) = op.net.filter(|&id| id != 0 && id != net_id) else {
                continue;
            };
            let Some(on) = board.net(oid) else { continue };
            if let Some(v) = power_rail_voltage(&on.name) {
                out.push((c.reference.clone(), on.name.clone(), v));
            }
        }
    }
    out
}

/// Run the back-powering lint over every modelled part on the board.
pub fn back_power_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    // (part ref, net id, rail net) already reported, so a bus with several
    // pull-ups to one rail, or a part with two pins on the net, reads once.
    let mut seen: BTreeSet<(String, i64, String)> = BTreeSet::new();

    for comp in &board.components {
        // Three-state contract: only a Present record can be a resolvable
        // part with a supply domain; DNP and identity-refused records abstain.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let Some(model) = resolve(lib, part).model else {
            continue;
        };
        // Deterministic pad order for stable finding order.
        let pins: std::collections::BTreeMap<String, String> = model
            .pins
            .iter()
            .map(|(pad, role)| (pad.clone(), role.clone()))
            .collect();
        let Some((supply_v, supply_net)) = supply_domain(board, comp, &pins) else {
            continue;
        };

        for (pad, role) in &pins {
            if is_skipped_role(role) {
                continue;
            }
            let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
                continue;
            };
            let Some(id) = pin.net.filter(|&id| id != 0) else {
                continue;
            };
            let Some(net) = board.net(id) else { continue };

            // Direct tie: the signal pin's own net IS a higher rail.
            if let Some(rail_v) = power_rail_voltage(&net.name) {
                if rail_v > supply_v + CLAMP_MARGIN_V
                    && seen.insert((comp.reference.clone(), id, net.name.clone()))
                {
                    report.findings.push(LintFinding {
                        check: LintCheck::BackPower,
                        severity: Severity::High,
                        message: format!(
                            "{} pad {pad} ({role}) is wired directly to {} ({rail_v:.1} V) \
                             while {}'s supply {} is {supply_v:.1} V: the pin's protection \
                             clamp conducts from the higher rail into the {supply_v:.1} V \
                             domain with nothing to limit the current (back-powering; past \
                             the VCC+0.5 V input abs-max).",
                            comp.reference, net.name, comp.reference, supply_net
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: vec![net.name.clone()],
                    });
                }
                continue; // a rail net has no pull-ups to judge
            }

            // Pull-up to a higher rail.
            for (r_ref, rail_net, rail_v) in pullups_to_rails(board, id) {
                if rail_v > supply_v + CLAMP_MARGIN_V
                    && seen.insert((comp.reference.clone(), id, rail_net.clone()))
                {
                    report.findings.push(LintFinding {
                        check: LintCheck::BackPower,
                        severity: Severity::Medium,
                        message: format!(
                            "{} pad {pad} ({role}) on \"{}\" is pulled up through {r_ref} \
                             to {rail_net} ({rail_v:.1} V) while {}'s supply {} is \
                             {supply_v:.1} V: current flows through the pin clamp into the \
                             {supply_v:.1} V domain, back-powering it when it is off and \
                             sitting past the VCC+0.5 V input abs-max when it is on, \
                             unless this pin is explicitly {rail_v:.1} V-tolerant.",
                            comp.reference, net.name, comp.reference, supply_net
                        ),
                        refs: vec![comp.reference.clone(), r_ref],
                        nets: vec![net.name.clone(), rail_net],
                    });
                }
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::LintCheck;

    fn bare_resistor() -> Component {
        Component {
            reference: "R7".into(),
            value: "10k".into(),
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            position: None,
            layer: "F.Cu".into(),
            properties: Vec::new(),
            dnp: false,
            pins: vec![
                hauksbee_extract::Pin {
                    number: "1".into(),
                    net: Some(1),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                },
                hauksbee_extract::Pin {
                    number: "2".into(),
                    net: Some(2),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                },
            ],
        }
    }

    /// Two-sided three-state contract: a fitted plain resistor counts as a
    /// rail tie; a DNP or identity-refused record of the same part must not.
    #[test]
    fn dnp_or_refused_resistor_is_not_a_rail_tie() {
        assert!(is_plain_resistor(&bare_resistor()));

        let mut dnp = bare_resistor();
        dnp.dnp = true;
        assert!(!is_plain_resistor(&dnp));

        let mut refused = bare_resistor();
        refused.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.into(),
            "two contradictory R7 records".into(),
        ));
        assert!(!is_plain_resistor(&refused));
    }

    /// An ESP-WROOM-32 (3.3 V part: pad 2 = vdd on +3V3) whose GPIO0 net
    /// carries a 10k pull-up. `rail` picks the pull-up's far side: "+5V"
    /// back-powers, "+3V3" is the correct same-domain bias.
    fn esp32_pull_board(rail: &str) -> String {
        let rail_net = if rail == "+5V" { 2 } else { 5 };
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "/SENSOR_INT")
  (net 5 "+3V3")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 0 1) (net 5 "+3V3"))
    (pad 25 smd rect (at 0 5) (net 3 "/SENSOR_INT"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R7 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SENSOR_INT"))
    (pad 2 smd rect (at 2 0) (net {rail_net} "{rail}"))
  )
)"#
        )
    }

    fn run(text: &str) -> NetLintReport {
        let board = ExtractedBoard::from_kicad_pcb(text).expect("parse synthetic board");
        let lib = ModelLibrary::builtin();
        back_power_lint(&board, &lib)
    }

    /// Faulty side: a 3.3 V-supplied ESP32 GPIO pulled to +5V must fire one
    /// Medium finding naming the part, the resistor, and both rails.
    #[test]
    fn pullup_above_supply_domain_fires() {
        let r = run(&esp32_pull_board("+5V"));
        let f: Vec<_> = r.of_check(LintCheck::BackPower).collect();
        assert_eq!(f.len(), 1, "exactly one back-power finding, got {f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].refs.contains(&"U3".to_string()));
        assert!(f[0].refs.contains(&"R7".to_string()));
        assert!(
            f[0].message.contains("5.0 V") && f[0].message.contains("3.3 V"),
            "finding must carry both domain voltages: {}",
            f[0].message
        );
    }

    /// Clean side: the same pull-up to the part's OWN +3V3 rail is the
    /// correct bias and must stay silent.
    #[test]
    fn pullup_to_own_rail_is_silent() {
        let r = run(&esp32_pull_board("+3V3"));
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "same-domain pull-up is the normal idiom, got {:?}",
            r.findings
        );
    }

    /// The clamp margin is pinned from the silent side too: a rail 0.3 V
    /// above the 3.3 V supply (+3V6) sits below one diode drop, the clamp
    /// does not conduct, and the check must stay silent. Together with the
    /// +5V case (1.7 V over) this bounds the margin inside (0.3, 1.7) V, so
    /// a margin of zero (fires on every mixed-nominal board) or of several
    /// volts (never fires) both fail.
    #[test]
    fn sub_clamp_margin_rail_is_silent() {
        // Rename net 2 from +5V to +3V6 everywhere (declaration + pads), so
        // the pull-up lands on a rail only 0.3 V above the supply.
        let board = esp32_pull_board("+5V").replace("(net 2 \"+5V\")", "(net 2 \"+3V6\")");
        let r = run(&board);
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "+3V6 is 0.3 V over the 3.3 V supply, under the clamp drop, got {:?}",
            r.findings
        );
    }

    /// Direct tie of a signal pin to a higher rail is High: no series element
    /// bounds the clamp current.
    #[test]
    fn direct_tie_to_higher_rail_is_high() {
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 5 "+3V3")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 0 1) (net 5 "+3V3"))
    (pad 25 smd rect (at 0 5) (net 2 "+5V"))
  )
)"#;
        let r = run(text);
        let f: Vec<_> = r.of_check(LintCheck::BackPower).collect();
        assert_eq!(f.len(), 1, "direct tie fires, got {f:?}");
        assert_eq!(f[0].severity, Severity::High);
    }

    /// A part whose supply net the rail table cannot resolve has no domain
    /// evidence, so the check must stay silent rather than guess.
    #[test]
    fn unresolvable_supply_stays_silent() {
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "/SENSOR_INT")
  (net 5 "VDD")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 0 1) (net 5 "VDD"))
    (pad 25 smd rect (at 0 5) (net 3 "/SENSOR_INT"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R7 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SENSOR_INT"))
    (pad 2 smd rect (at 2 0) (net 2 "+5V"))
  )
)"#;
        let r = run(text);
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "bare VDD carries no magnitude; no guessed voltage may support a finding"
        );
    }
}
