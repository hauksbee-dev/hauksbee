//! Floating CMOS inputs: a logic input on no net, or on a net with no driver,
//! no pull resistor and no other connection.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A CMOS input is a gate capacitor with no DC path of its own. Left open it
//! charges to whatever leakage and coupling put on it: the part reads random
//! bits, and an input resting near mid-rail turns both input transistors half
//! on and draws shoot-through current. Unlike bipolar TTL (which floats high),
//! nothing about the input defines its level, so every CMOS input must be
//! driven, pulled, or tied.
//!
//! ## Evidence the check rests on
//!
//! - The part is CMOS logic: its value names a CMOS logic family (74HC/HCT/
//!   AC/AHC/LVC/AUP/..., CD4000/HEF4000/MC14000, NC7S/NC7W tiny logic, TI
//!   `TXB010x` translators).
//!   Bipolar and bus-hold families (74LS, 74ABT, 74LVT) are not judged.
//! - The pin is an input: a `logic.inputs` role of the part's model card mapped
//!   through the card's pin table, or a schematic pin whose electrical type is
//!   `input`.
//! - Its net carries nothing but CMOS inputs: no driver, no pull resistor, no
//!   connector, no other part. Test points and mounting holes do not count as
//!   a connection, and nor does a do-not-populate part. A net named as a
//!   supply rail or ground abstains (the tie is the designer's).
//!
//! An explicit no-connect flag is not an excuse: a CMOS input marked NC still
//! floats. The exception is an input the card lists in `logic.pulled_inputs`,
//! which the datasheet gives an internal pull, so leaving it open is allowed.
//!
//! The extract-layer `FloatingControlPin` lint already reports a named
//! enable/reset/chip-select input alone on its net; such a pin is left to it
//! here so one fault does not read as two.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_bind::binder::power_rail_voltage;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, is_connector_like, is_ground, is_logic_or_switch_ref, is_test_point_or_hole,
    is_unconnected_net, pin_is_no_connect, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity,
};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

/// CMOS logic families, matched after any vendor prefix is stripped from the
/// value. Ordered longest first so `AHCT` is not read as `AHC`.
const CMOS_74_FAMILIES: [&str; 20] = [
    "AHCT", "VHCT", "ALVC", "LVC", "AHC", "VHC", "HCT", "ACT", "AUP", "AXP", "AVC", "AUC", "LVX",
    "LCX", "VCX", "FCT", "HC", "AC", "LV", "C",
];

/// Vendor / grade prefixes that precede the bare `74xx` part number.
const VENDOR_PREFIXES: [&str; 8] = ["SN", "MC", "NLV", "NL", "TC", "HD", "IDT", "M"];

/// Does this part value name a CMOS logic family?
#[must_use]
pub fn is_cmos_logic_value(value: &str) -> bool {
    let v = value.trim().to_ascii_uppercase();
    // 4000-series CMOS: CD4xxx, HEF4xxx, MC14xxx.
    for (prefix, digits) in [("CD4", 3), ("HEF4", 3), ("MC14", 3)] {
        if let Some(rest) = v.strip_prefix(prefix) {
            if rest.len() >= digits && rest.chars().take(digits).all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    // Fairchild/onsemi and Toshiba tiny logic: NC7S.., NC7SZ.., NC7WZ.., TC7S...
    // TI TXB010x push-pull translators: OE is a plain CMOS input with no
    // internal pull (TXB0101 SCES639F 5.6, II +-1 uA; 7.3 asks for an OE
    // pull-down), so it must not float either.
    if ["NC7S", "NC7W", "NC7N", "NC7P", "TC7S", "TC7W", "TXB0"]
        .iter()
        .any(|p| v.starts_with(p))
    {
        return true;
    }
    let mut core = v.as_str();
    for p in VENDOR_PREFIXES {
        if let Some(rest) = core.strip_prefix(p) {
            if rest.starts_with("74") {
                core = rest;
                break;
            }
        }
    }
    let Some(rest) = core.strip_prefix("74") else {
        return false;
    };
    // `74` then the family letters then a digit (`74HC165`, `74LVC1G04`,
    // `74C14`); `74LS..`, `74ABT..` and `74LVT..` never reach a digit.
    CMOS_74_FAMILIES.iter().any(|fam| {
        rest.strip_prefix(fam)
            .is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// One CMOS input pad of a fitted part, with the name it is reported by and
/// whether the card lets it be left open.
struct InputPin<'a> {
    pin: &'a Pin,
    label: String,
    pull_allowed: bool,
}

/// The input pads of a CMOS logic part: card `logic.inputs` roles mapped
/// through the card's pin table, plus schematic `input`-typed pins.
fn input_pins<'a>(comp: &'a Component, model: Option<&ModelEntry>) -> Vec<InputPin<'a>> {
    let card = model.filter(|m| {
        matches!(
            m.kind,
            ComponentKind::Digital | ComponentKind::ShiftRegister
        ) && !m.logic.inputs.is_empty()
    });
    let mut out = Vec::new();
    for pin in &comp.pins {
        let role = card.and_then(|m| {
            m.pins
                .get(&pin.number)
                .filter(|r| m.logic.inputs.iter().any(|i| i == *r))
        });
        let typed_input = pin.kind.to_ascii_lowercase().split('+').next() == Some("input");
        if role.is_none() && !typed_input {
            continue;
        }
        let pull_allowed = match (card, role) {
            (Some(m), Some(r)) => m.logic.pulled_inputs.iter().any(|p| p == r),
            _ => false,
        };
        let label = if !pin.function.trim().is_empty() {
            display_name(&pin.function)
        } else if let Some(r) = role {
            r.to_ascii_uppercase()
        } else {
            format!("pin {}", pin.number)
        };
        out.push(InputPin {
            pin,
            label,
            pull_allowed,
        });
    }
    out
}

/// A schematic pin name without `KiCad`'s overbar markup: `~{CE}` reads `/CE`.
fn display_name(function: &str) -> String {
    let f = function.trim();
    f.strip_prefix("~{")
        .and_then(|r| r.strip_suffix('}'))
        .map_or_else(|| f.replace(['{', '}'], ""), |inner| format!("/{inner}"))
}

/// Would the extract-layer `FloatingControlPin` lint already report this pin?
/// Mirrors its predicate so the two checks never report one pin twice.
pub(super) fn control_lint_covers(board: &ExtractedBoard, comp: &Component, pin: &Pin) -> bool {
    let active = is_logic_or_switch_ref(comp);
    let Some(net) = pin.net.and_then(|id| board.net(id)) else {
        return false;
    };
    active
        && control_role(&pin.function).is_some()
        && !pin_is_no_connect(pin)
        && !is_unconnected_net(&net.name)
        && board.net_members(net.id).len() == 1
}

/// Every floating CMOS input, one finding per part.
#[must_use]
pub fn floating_input_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let models: BTreeMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let cmos: Vec<(&Component, Vec<InputPin<'_>>)> = board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present() && is_cmos_logic_value(&c.value))
        .map(|c| (c, input_pins(c, models.get(c.reference.as_str()))))
        .filter(|(_, pins)| !pins.is_empty())
        .collect();
    // Every CMOS input pad on the board: another input sharing the net is not
    // a driver, so a net of nothing but inputs still floats.
    let inputs: BTreeSet<(&str, &str)> = cmos
        .iter()
        .flat_map(|(c, pins)| {
            pins.iter()
                .map(|p| (c.reference.as_str(), p.pin.number.as_str()))
        })
        .collect();
    let defines_level = |c: &Component, p: &Pin| {
        AssemblyState::of(c).is_present()
            && !is_test_point_or_hole(c)
            && !inputs.contains(&(c.reference.as_str(), p.number.as_str()))
    };
    let floats = |p: &Pin| match p.net.filter(|&id| id != 0).and_then(|id| board.net(id)) {
        None => true,
        Some(net) => {
            if is_ground(&net.name) || power_rail_voltage(&net.name).is_some() {
                return false;
            }
            !board
                .net_members(net.id)
                .into_iter()
                .any(|(c, q)| defines_level(c, q))
        }
    };

    // The do-not-populate active parts (an IC, a transistor, a module or a
    // connector) on an input's net. A keyboard marks its hand-soldered
    // controller module DNP so the assembler skips it; the module still drives
    // the input on the finished board. A DNP resistor is not one: it is the
    // pull the assembled board leaves out, which is the fault itself.
    let dnp_drivers = |p: &Pin| -> BTreeSet<&str> {
        p.net
            .filter(|&id| id != 0)
            .map(|id| board.net_members(id))
            .unwrap_or_default()
            .into_iter()
            .filter(|(c, _)| {
                matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
                    && !is_test_point_or_hole(c)
                    && (is_logic_or_switch_ref(c) || is_connector_like(c))
            })
            .map(|(c, _)| c.reference.as_str())
            .collect()
    };

    let mut report = NetLintReport::default();
    for (comp, pins) in &cmos {
        let (assumed, open): (Vec<&InputPin<'_>>, Vec<&InputPin<'_>>) = pins
            .iter()
            .filter(|p| !(p.pull_allowed && pin_is_no_connect(p.pin)))
            .filter(|p| !control_lint_covers(board, comp, p.pin))
            .filter(|p| floats(p.pin))
            .partition(|p| !dnp_drivers(p.pin).is_empty());
        if !assumed.is_empty() {
            report
                .findings
                .push(driven_only_by_dnp(board, comp, &assumed, &dnp_drivers));
        }
        if open.is_empty() {
            continue;
        }
        let flagged_nc = open.iter().filter(|p| pin_is_no_connect(p.pin)).count();
        let names = compress_labels(open.iter().map(|p| p.label.as_str()));
        let nc_note = if flagged_nc > 0 {
            " They carry a no-connect flag, but the part has no internal pull on them, so the flag leaves them floating."
        } else {
            ""
        };
        let nets: Vec<String> = open
            .iter()
            .filter_map(|p| p.pin.net.and_then(|id| board.net(id)))
            .map(|n| n.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        report.findings.push(LintFinding {
            check: LintCheck::FloatingInput,
            severity: Severity::High,
            message: format!(
                "{} {}: {} {} {} unconnected (no driver, pull resistor or other connection); CMOS inputs must not float. A floating input reads random levels and can draw shoot-through current; tie each one high or low, or drive it.{nc_note}",
                comp.reference,
                comp.value,
                if open.len() == 1 { "input" } else { "inputs" },
                names,
                if open.len() == 1 { "is" } else { "are" },
            ),
            refs: vec![comp.reference.clone()],
            nets,
        });
    }
    report
}

/// The stated condition for inputs whose only driver is a do-not-populate
/// active part: the board works when that part is fitted by hand, and floats
/// when it is left off.
fn driven_only_by_dnp<'b>(
    board: &'b ExtractedBoard,
    comp: &Component,
    pins: &[&InputPin<'_>],
    dnp_drivers: &impl Fn(&Pin) -> BTreeSet<&'b str>,
) -> LintFinding {
    let drivers: BTreeSet<&str> = pins.iter().flat_map(|p| dnp_drivers(p.pin)).collect();
    let drivers = drivers.into_iter().collect::<Vec<_>>().join(", ");
    let nets: Vec<String> = pins
        .iter()
        .filter_map(|p| p.pin.net.and_then(|id| board.net(id)))
        .map(|n| n.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let one = pins.len() == 1;
    LintFinding {
        check: LintCheck::FloatingInput,
        severity: Severity::Low,
        message: format!(
            "{} {}: {} {} on {} {} driven only by {drivers}, marked do-not-populate. Assumed: {drivers} (or the module it represents) is fitted when the board is used; if it is left off, the {} {}, so add a pull resistor if that state matters.",
            comp.reference,
            comp.value,
            if one { "input" } else { "inputs" },
            compress_labels(pins.iter().map(|p| p.label.as_str())),
            nets.join(", "),
            if one { "is" } else { "are" },
            if one { "input" } else { "inputs" },
            if one { "floats" } else { "float" },
        ),
        refs: vec![comp.reference.clone()],
        nets,
    }
}

/// Join pin labels, collapsing runs of one prefix with consecutive numbers:
/// `DS, D0, D1, D2` reads `DS, D0-D2`.
fn compress_labels<'a>(labels: impl Iterator<Item = &'a str>) -> String {
    let split = |s: &str| {
        let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let (head, tail) = s.split_at(s.len() - digits);
        (head.to_string(), tail.parse::<u32>().ok())
    };
    let mut items: Vec<(String, Option<u32>, String)> = labels
        .map(|l| {
            let (h, n) = split(l);
            (h, n, l.to_string())
        })
        .collect();
    items.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    items.dedup_by(|a, b| a.2 == b.2);
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let mut j = i;
        while let (Some(n), Some(next)) = (
            items[j].1,
            items
                .get(j + 1)
                .filter(|x| x.0 == items[i].0)
                .and_then(|x| x.1),
        ) {
            if next != n + 1 {
                break;
            }
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}-{}", items[i].2, items[j].2));
        } else {
            parts.extend(items[i..=j].iter().map(|x| x.2.clone()));
        }
        i = j + 1;
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::Net;

    fn pin(number: &str, net: Option<i64>, function: &str, kind: &str) -> Pin {
        Pin {
            number: number.into(),
            net,
            function: function.into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    /// A 74HC165 with its card's pinout; `wire(pad)` picks each input's net.
    fn hc165(reference: &str, wire: impl Fn(&str) -> (Option<i64>, &'static str)) -> Component {
        let names = [
            ("1", "~{PL}"),
            ("2", "CP"),
            ("3", "D4"),
            ("4", "D5"),
            ("5", "D6"),
            ("6", "D7"),
            ("10", "DS"),
            ("11", "D0"),
            ("12", "D1"),
            ("13", "D2"),
            ("14", "D3"),
            ("15", "~{CE}"),
        ];
        let mut pins: Vec<Pin> = names
            .iter()
            .map(|(n, f)| {
                let (net, kind) = wire(n);
                pin(n, net, f, kind)
            })
            .collect();
        pins.push(pin("7", None, "~{Q7}", "output+no_connect"));
        pins.push(pin("8", Some(1), "GND", "power_in"));
        pins.push(pin("9", Some(3), "Q7", "output"));
        pins.push(pin("16", Some(2), "VCC", "power_in"));
        Component {
            reference: reference.into(),
            value: "74HC165".into(),
            lib_id: "74xx:74HC165".into(),
            pins,
            ..Default::default()
        }
    }

    fn board(components: Vec<Component>, extra: &[(i64, &str)]) -> ExtractedBoard {
        let mut nets = vec![
            Net::new(1, "GND"),
            Net::new(2, "+5V"),
            Net::new(3, "SER_OUT"),
        ];
        nets.extend(extra.iter().map(|(id, n)| Net::new(*id, *n)));
        ExtractedBoard {
            name: "t".into(),
            nets,
            components,
        }
    }

    fn mcu_driving(nets: &[i64]) -> Component {
        Component {
            reference: "U9".into(),
            value: "ATmega328P-AU".into(),
            pins: nets
                .iter()
                .enumerate()
                .map(|(i, n)| pin(&(i + 1).to_string(), Some(*n), "", "bidirectional"))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn cmos_families_are_recognised_and_bipolar_ones_are_not() {
        for v in [
            "74HC165",
            "SN74HC595N",
            "74HCT08",
            "SN74LVC1G3157DBVR",
            "74AHCT1G32",
            "CD4051B",
            "HEF4094",
            "MC14066",
            "NC7SZ125",
            "74AUP1G04",
            "TXB0101DCKR",
        ] {
            assert!(is_cmos_logic_value(v), "{v}");
        }
        for v in [
            "74LS00",
            "SN74ABT245",
            "74LVT245",
            "ATmega328P",
            "LM358",
            "74F00",
        ] {
            assert!(!is_cmos_logic_value(v), "{v}");
        }
    }

    /// A TXB0101 on +5V/GND with A and B driven, OE on net `oe`.
    fn txb0101(oe: Option<i64>, kind: &str) -> Component {
        let pins = vec![
            pin("1", Some(2), "VCCA", "power_in"),
            pin("2", Some(1), "GND", "power_in"),
            pin("3", Some(3), "A", "bidirectional"),
            pin("4", Some(3), "B", "bidirectional"),
            pin("5", oe, "OE", kind),
            pin("6", Some(2), "VCCB", "power_in"),
        ];
        Component {
            reference: "U1".into(),
            value: "TXB0101DCK".into(),
            pins,
            ..Default::default()
        }
    }

    #[test]
    fn a_txb_oe_left_open_floats_and_a_pulldown_ties_it() {
        // SCES639F 7.3: OE "should be tied to GND through a pulldown resistor".
        let open = board(
            vec![txb0101(Some(9), "input+no_connect")],
            &[(9, "unconnected-(U1-OE-Pad5)")],
        );
        let found = floating_input_lint(&open, &ModelLibrary::builtin()).findings;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0]
                .message
                .starts_with("U1 TXB0101DCK: input OE is unconnected"),
            "{}",
            found[0].message
        );
        let pulldown = Component {
            reference: "R1".into(),
            value: "100k".into(),
            pins: vec![
                pin("1", Some(9), "", "passive"),
                pin("2", Some(1), "", "passive"),
            ],
            ..Default::default()
        };
        let tied = board(vec![txb0101(Some(9), "input"), pulldown], &[(9, "OE")]);
        assert!(floating_input_lint(&tied, &ModelLibrary::builtin())
            .findings
            .is_empty());
    }

    #[test]
    fn no_connect_flagged_inputs_on_a_74hc165_are_one_finding() {
        // Seven data inputs flagged no-connect on their own KiCad nets, the
        // shape of a partly used shift register; everything else is driven or tied.
        let mut extra = vec![(4, "LOAD"), (5, "CLK"), (6, "D6"), (7, "D7")];
        let open = ["10", "11", "12", "13", "14", "3", "4"];
        let names: Vec<String> = open
            .iter()
            .map(|n| format!("unconnected-(U1-X-Pad{n})"))
            .collect();
        for (i, n) in names.iter().enumerate() {
            extra.push((100 + i64::try_from(i).unwrap(), n.as_str()));
        }
        let u1 = hc165("U1", |n| match n {
            "1" => (Some(4), "input"),
            "2" => (Some(5), "input"),
            "5" => (Some(6), "input"),
            "6" => (Some(7), "input"),
            "15" => (Some(1), "input"),
            _ => {
                let i = open.iter().position(|o| *o == n).unwrap();
                (Some(100 + i64::try_from(i).unwrap()), "input+no_connect")
            }
        });
        let b = board(vec![u1, mcu_driving(&[4, 5, 6, 7, 3])], &extra);
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        let found: Vec<_> = report.of_check(LintCheck::FloatingInput).collect();
        assert_eq!(found.len(), 1, "{:?}", report.findings);
        let m = &found[0].message;
        assert!(
            m.starts_with("U1 74HC165: inputs D0-D5, DS are unconnected"),
            "{m}"
        );
        assert!(m.contains("no-connect flag"), "{m}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].refs, ["U1"]);
    }

    #[test]
    fn driven_tied_and_pulled_inputs_are_legal() {
        // Every input driven by the MCU, tied to a rail, or pulled by a resistor.
        let r = Component {
            reference: "R1".into(),
            value: "10k".into(),
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            pins: vec![
                pin("1", Some(8), "", "passive"),
                pin("2", Some(2), "", "passive"),
            ],
            ..Default::default()
        };
        let u1 = hc165("U1", |n| match n {
            "1" => (Some(4), "input"),
            "2" => (Some(5), "input"),
            "10" => (Some(8), "input"),
            "15" => (Some(1), "input"),
            _ => (Some(2), "input"),
        });
        let b = board(
            vec![u1, r, mcu_driving(&[4, 5, 3])],
            &[(4, "LOAD"), (5, "CLK"), (8, "SER_PULL")],
        );
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        assert!(report.is_clean(), "{:?}", report.findings);
    }

    #[test]
    fn inputs_sharing_a_net_with_only_other_inputs_still_float_and_card_pins_need_no_types() {
        // PCB-only shape: no pin types or names, the card's pin table names the
        // inputs. U1's serial input and U2's clock share a net nothing drives.
        let strip = |mut c: Component| {
            for p in &mut c.pins {
                p.kind.clear();
                p.function.clear();
            }
            c
        };
        let wire = |shared: &'static str| {
            move |n: &str| {
                if n == shared {
                    (Some(9), "input")
                } else {
                    (Some(2), "input")
                }
            }
        };
        let u1 = strip(hc165("U1", wire("10")));
        let u2 = strip(hc165("U2", wire("2")));
        let b = board(vec![u1, u2], &[(9, "SHARED")]);
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        let found: Vec<_> = report.of_check(LintCheck::FloatingInput).collect();
        assert_eq!(found.len(), 2, "{:?}", report.findings);
        assert!(
            found[0].message.contains("input SER is"),
            "{}",
            found[0].message
        );
        assert!(
            found[1].message.contains("input CLK is"),
            "{}",
            found[1].message
        );
    }

    #[test]
    fn a_named_control_pin_alone_on_its_net_is_left_to_the_control_pin_lint() {
        let u1 = hc165("U1", |n| match n {
            "15" => (Some(9), "input"),
            _ => (Some(2), "input"),
        });
        let b = board(vec![u1], &[(9, "CE_FLOAT")]);
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        assert!(report.is_clean(), "{:?}", report.findings);
        assert_eq!(
            b.net_lint().of_check(LintCheck::FloatingControlPin).count(),
            1
        );
    }

    #[test]
    fn a_bipolar_or_non_logic_part_is_not_judged() {
        let mut u1 = hc165("U1", |_| (None, "input"));
        u1.value = "74LS165".into();
        let report = floating_input_lint(&board(vec![u1], &[]), &ModelLibrary::builtin());
        assert!(report.is_clean(), "{:?}", report.findings);
    }

    /// One 74LVC1G125 whose A input (pad 2) sits on net 7 with `others`.
    fn buffer_input_with(others: Vec<Component>) -> NetLintReport {
        let u1 = Component {
            reference: "U1".into(),
            value: "74LVC1G125".into(),
            pins: vec![
                pin("1", Some(1), "~{OE}", "input"),
                pin("2", Some(7), "A", "input"),
                pin("3", Some(1), "GND", "power_in"),
                pin("4", Some(3), "Y", "output"),
                pin("5", Some(2), "VCC", "power_in"),
            ],
            ..Default::default()
        };
        let mut parts = vec![u1];
        parts.extend(others);
        let b = board(parts, &[(7, "SIG")]);
        floating_input_lint(&b, &ModelLibrary::builtin())
    }

    #[test]
    fn an_input_driven_only_by_a_dnp_module_is_a_stated_condition() {
        // Legal control: the controller module is marked DNP because it is
        // soldered by hand; it drives the input on the finished board.
        let mut module = mcu_driving(&[7]);
        module.reference = "MCU1".into();
        module.dnp = true;
        let r = buffer_input_with(vec![module]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert_eq!(r.findings[0].severity, Severity::Low);
        assert!(
            r.findings[0].message.contains("driven only by MCU1"),
            "{}",
            r.findings[0].message
        );
    }

    #[test]
    fn an_input_left_only_a_dnp_pull_or_nothing_still_floats() {
        // A DNP pull-up is the pull the assembled board leaves out.
        let pull = Component {
            reference: "R1".into(),
            value: "10k".into(),
            lib_id: "Device:R".into(),
            dnp: true,
            pins: vec![
                pin("1", Some(7), "", "passive"),
                pin("2", Some(2), "", "passive"),
            ],
            ..Default::default()
        };
        // Another buffer's input is not a driver either.
        let other = Component {
            reference: "U2".into(),
            value: "74LVC1G125".into(),
            pins: vec![pin("2", Some(7), "A", "input")],
            ..Default::default()
        };
        for others in [vec![pull], vec![], vec![other]] {
            let r = buffer_input_with(others);
            let high = r
                .findings
                .iter()
                .filter(|f| f.refs == ["U1"] && f.severity == Severity::High)
                .count();
            assert_eq!(high, 1, "{:?}", r.findings);
        }
    }

    #[test]
    fn labels_collapse_consecutive_runs() {
        assert_eq!(
            compress_labels(["DS", "D0", "D1", "D2", "D3", "D5"].into_iter()),
            "D0-D3, D5, DS"
        );
        assert_eq!(compress_labels(["A", "B"].into_iter()), "A, B");
        assert_eq!(display_name("~{CE}"), "/CE");
    }
}
