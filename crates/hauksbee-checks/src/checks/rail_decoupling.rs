//! Rail decoupling ratio: supply rails whose ICs are short of local
//! decoupling capacitance.
//!
//! Every IC draws its supply current in edges: each output transition and each
//! clock pulls a burst of charge in nanoseconds, far faster than a regulator
//! (or a long trace back to one) can answer. The burst comes from a capacitor
//! near the supply pin, typically ~100 nF per IC, with a bulk capacitor
//! (microfarads) on the rail to refill them. Without them the rail bounces with
//! every edge: logic double-clocks, comparators chatter, and the fault looks
//! like firmware.
//!
//! A netlist carries no placement, so this check cannot say a capacitor is far
//! from its pin. It counts: the ICs drawing from each rail against the
//! capacitors from that rail to ground, and whether any of them is bulk. It is
//! a ratio, phrased as one, and reported as a non-gating note.
//!
//! ## What counts
//!
//! - An IC is a fitted part with a supply pin (a schematic pin named `VCC` /
//!   `VDD` / `V+` / `AVCC` / ..., or a supply role in its model card's pin map)
//!   on the rail. Connectors, passives and plug-in modules (an `A`-referenced
//!   assembly or a `*Module` library part, such as an MCU dev board) are not
//!   counted: a module carries its own decoupling.
//! - A decoupling capacitor is a fitted capacitor with one pad on the rail and
//!   the other on ground. Bulk is any such capacitor of 1 uF or more.
//!
//! ## When it fires
//!
//! - **Sparse decoupling:** a rail shared by [`MIN_SHARED_ICS`] or more ICs
//!   with no capacitor to ground, or fewer than one capacitor per
//!   [`ICS_PER_CAP`] ICs. A lone IC (with or without a capacitor), or a pair
//!   of logic parts sharing one, stays silent.
//! - **No bulk:** a rail feeding more than [`BULK_IC_THRESHOLD`] ICs with no
//!   capacitor of 1 uF or more on it.
//!
//! Both conditions on one rail make one finding.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_connector_like, is_ground, is_unconnected_net, Component, ExtractedBoard,
    LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::ModelLibrary;

/// More ICs than this per decoupling capacitor reads as sparse decoupling.
pub const ICS_PER_CAP: usize = 3;

/// The fewest ICs on a rail before the sparse-decoupling count is judged.
///
/// A lone chip on a rail is the minimal single-IC example (the shipped demo
/// boards are exactly that), where a per-IC capacitor count is the noise that
/// kept decoupling out of the lint for so long; the ratio earns its keep on a
/// rail shared by several ICs.
pub const MIN_SHARED_ICS: usize = 2;

/// A rail feeding more ICs than this wants a bulk capacitor.
pub const BULK_IC_THRESHOLD: usize = 4;

/// The smallest capacitance counted as bulk, in farads.
const BULK_FARADS: f64 = 1e-6;

/// How many IC references a finding lists before summarising the rest.
const LISTED_ICS: usize = 8;

/// Schematic supply-pin names, compared after upper-casing and dropping
/// `KiCad` markup and a trailing pin index (`VDD_1`, `VCC2`).
const SUPPLY_PIN_NAMES: [&str; 20] = [
    "VCC", "VDD", "V+", "+VS", "VCCA", "VCCB", "VCCIO", "VCC_IO", "VDDIO", "VDD_IO", "VIO", "VDDA",
    "VDDD", "AVCC", "AVDD", "DVCC", "DVDD", "VCCQ", "VDDQ", "VCC_IN",
];

/// Model-card supply roles (lower-case pin-map roles).
pub(crate) const SUPPLY_ROLES: [&str; 9] = [
    "vcc", "vdd", "avcc", "vdda", "vddio", "vio", "vcca", "vccb", "v+",
];

fn is_supply_pin_name(function: &str) -> bool {
    let f = function
        .trim()
        .to_ascii_uppercase()
        .replace(['{', '}', '~'], "");
    let f = f.trim_end_matches(|c: char| c.is_ascii_digit());
    let f = f.strip_suffix('_').unwrap_or(f);
    SUPPLY_PIN_NAMES.contains(&f)
}

/// A plug-in module (MCU dev board, radio module) brings its own decoupling.
fn is_module(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let assembly_ref = r
        .strip_prefix('A')
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()));
    assembly_ref
        || r.starts_with("MOD")
        || c.lib_id.to_ascii_lowercase().contains("module")
        || c.footprint.to_ascii_lowercase().contains("module")
}

/// A light emitter, including a smart (addressable) LED with a built-in
/// driver: designator `LED…`, or a library/footprint name whose first word is
/// `LED` (an `OLED` display is not one). Its supply pin feeds an LED driver
/// string, not an IC core: a chain of such LEDs commonly shares bulk
/// capacitance, so counting each one as an IC wanting its own bypass
/// capacitor inflates the per-IC ratio of every rail it sits on.
fn is_light_emitter(c: &Component) -> bool {
    let first_word_is_led = |s: &str| {
        let name = s.rsplit(':').next().unwrap_or(s);
        name.split(|ch: char| !ch.is_ascii_alphanumeric())
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("led"))
    };
    let r = c.reference.to_ascii_uppercase();
    let led_ref = r
        .strip_prefix("LED")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(|ch: char| ch.is_ascii_digit()));
    led_ref || first_word_is_led(&c.lib_id) || first_word_is_led(&c.footprint)
}

/// One rail's tally.
#[derive(Default)]
struct Rail {
    ics: BTreeSet<String>,
    caps: Vec<(String, Option<f64>)>,
}

/// Every rail short of decoupling, one finding per rail.
#[must_use]
pub fn decoupling_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let card_supply: BTreeMap<&str, BTreeSet<String>> = super::resolved_components(board, lib)
        .map(|(c, m)| {
            let pads = m
                .pins
                .iter()
                .filter(|(_, role)| SUPPLY_ROLES.contains(&role.to_ascii_lowercase().as_str()))
                .map(|(pad, _)| pad.clone())
                .collect();
            (c.reference.as_str(), pads)
        })
        .collect();
    let rail_net = |id: i64| {
        board
            .net(id)
            .filter(|n| n.id != 0 && !is_ground(&n.name) && !is_unconnected_net(&n.name))
    };

    let mut rails: BTreeMap<i64, Rail> = BTreeMap::new();
    for c in &board.components {
        if !AssemblyState::of(c).is_present() || is_connector_like(c) || is_module(c) {
            continue;
        }
        if is_capacitor(c) {
            let nets: Vec<i64> = c.pins.iter().filter_map(|p| p.net).collect();
            let grounded = nets
                .iter()
                .any(|&id| board.net(id).is_some_and(|n| is_ground(&n.name)));
            if nets.len() == 2 && grounded {
                let farads = parse_value(&c.value).map(|v| v.si);
                for &id in &nets {
                    if rail_net(id).is_some() {
                        rails
                            .entry(id)
                            .or_default()
                            .caps
                            .push((c.reference.clone(), farads));
                    }
                }
            }
            continue;
        }
        if c.pins.len() < 3 || is_light_emitter(c) {
            continue;
        }
        let card = card_supply.get(c.reference.as_str());
        for p in &c.pins {
            let supply =
                is_supply_pin_name(&p.function) || card.is_some_and(|s| s.contains(&p.number));
            let Some(net) = p.net.filter(|_| supply).and_then(rail_net) else {
                continue;
            };
            if board.net_members(net.id).len() < 2 {
                continue;
            }
            rails
                .entry(net.id)
                .or_default()
                .ics
                .insert(c.reference.clone());
        }
    }

    let mut report = NetLintReport::default();
    for (id, rail) in rails {
        let n = rail.ics.len();
        if n == 0 {
            continue;
        }
        let k = rail.caps.len();
        let sparse = n >= MIN_SHARED_ICS && k * ICS_PER_CAP < n;
        let has_bulk = rail
            .caps
            .iter()
            .any(|(_, f)| f.is_some_and(|f| f >= BULK_FARADS));
        let no_bulk = n > BULK_IC_THRESHOLD && !has_bulk;
        if !sparse && !no_bulk {
            continue;
        }
        let name = board.net(id).map(|n| n.name.clone()).unwrap_or_default();
        let message = rail_message(&name, &rail, sparse, no_bulk);
        let mut refs: Vec<String> = rail.ics.iter().take(LISTED_ICS).cloned().collect();
        refs.extend(rail.caps.iter().map(|(r, _)| r.clone()));
        report.findings.push(LintFinding {
            check: LintCheck::MissingDecoupling,
            severity: Severity::Low,
            message,
            refs,
            nets: vec![name],
        });
    }
    report
}

fn rail_message(name: &str, rail: &Rail, sparse: bool, no_bulk: bool) -> String {
    let n = rail.ics.len();
    let k = rail.caps.len();
    let total: f64 = rail.caps.iter().filter_map(|(_, f)| *f).sum();
    let ics = if n == 1 { "IC" } else { "ICs" };
    let caps = match k {
        0 => "no capacitor".to_string(),
        1 => "1 capacitor".to_string(),
        _ => format!("{k} capacitors"),
    };
    let total = if k > 0 && total > 0.0 {
        format!(" ({})", farads_text(total))
    } else {
        String::new()
    };
    let listed: Vec<&str> = rail
        .ics
        .iter()
        .take(LISTED_ICS)
        .map(String::as_str)
        .collect();
    let more = if n > LISTED_ICS {
        format!(" and {} more", n - LISTED_ICS)
    } else {
        String::new()
    };
    let mut msg = format!(
        "{name} powers {n} {ics} with {caps}{total} to ground ({}{more})",
        listed.join(", ")
    );
    if sparse {
        msg.push_str(
            "; typical practice is ~100 nF per IC near its supply pin plus bulk on the rail",
        );
    }
    if no_bulk {
        msg.push_str("; no bulk capacitor (1 uF or more) on a rail feeding this many ICs");
    }
    msg.push_str(". A netlist does not record placement, so this is a count, not a layout check.");
    msg
}

fn farads_text(f: f64) -> String {
    let (v, unit) = if f >= 1e-6 {
        (f * 1e6, "uF")
    } else if f >= 1e-9 {
        (f * 1e9, "nF")
    } else {
        (f * 1e12, "pF")
    };
    let v = (v * 10.0).round() / 10.0;
    format!("{v} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Net, Pin};

    fn pin(number: &str, net: i64, function: &str) -> Pin {
        Pin {
            number: number.into(),
            net: Some(net),
            function: function.into(),
            ..Default::default()
        }
    }

    fn ic(reference: &str, rail: i64) -> Component {
        Component {
            reference: reference.into(),
            value: "74HC02".into(),
            lib_id: "74xx:74HC02".into(),
            pins: vec![pin("7", 1, "GND"), pin("14", rail, "VCC"), pin("1", 9, "Y")],
            ..Default::default()
        }
    }

    fn cap(reference: &str, value: &str, rail: i64) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: "Device:C".into(),
            footprint: "Capacitor_SMD:C_0603_1608Metric".into(),
            pins: vec![pin("1", rail, ""), pin("2", 1, "")],
            ..Default::default()
        }
    }

    fn board(components: Vec<Component>) -> ExtractedBoard {
        ExtractedBoard {
            name: "t".into(),
            nets: vec![Net::new(1, "GND"), Net::new(2, "+5P"), Net::new(9, "SIG")],
            components,
        }
    }

    fn findings(b: &ExtractedBoard) -> Vec<LintFinding> {
        decoupling_lint(b, &ModelLibrary::builtin()).findings
    }

    #[test]
    fn many_ics_on_one_small_cap_is_one_rail_finding() {
        let mut parts: Vec<Component> = (1..=12).map(|i| ic(&format!("U{i}"), 2)).collect();
        parts.push(cap("C1", "100nF", 2));
        let found = findings(&board(parts));
        assert_eq!(found.len(), 1, "{found:?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::MissingDecoupling);
        assert_eq!(f.severity, Severity::Low, "a note, never a gate");
        assert!(
            f.message
                .starts_with("+5P powers 12 ICs with 1 capacitor (100 nF) to ground"),
            "{}",
            f.message
        );
        assert!(f.message.contains("~100 nF per IC"), "{}", f.message);
        assert!(f.message.contains("no bulk capacitor"), "{}", f.message);
        assert!(f.message.contains("and 4 more"), "{}", f.message);
        assert_eq!(f.nets, ["+5P"]);
    }

    #[test]
    fn ics_sharing_a_rail_with_no_capacitor_fire_and_a_lone_ic_does_not() {
        // The rail comes in on a header; nothing else sits on it.
        let header = Component {
            reference: "J1".into(),
            value: "Conn_01x02".into(),
            lib_id: "Connector:Conn_01x02".into(),
            pins: vec![pin("1", 2, ""), pin("2", 1, "")],
            ..Default::default()
        };
        let found = findings(&board(vec![ic("U1", 2), ic("U2", 2), header.clone()]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0]
            .message
            .contains("2 ICs with no capacitor to ground"));
        assert!(
            !found[0].message.contains("no bulk"),
            "two ICs need no bulk"
        );
        // A lone IC is the minimal-example case: silent with or without a cap.
        assert!(findings(&board(vec![ic("U1", 2), header])).is_empty());
        assert!(findings(&board(vec![ic("U1", 2), cap("C1", "100n", 2)])).is_empty());
    }

    #[test]
    fn a_decoupled_rail_with_bulk_is_silent() {
        let mut parts: Vec<Component> = Vec::new();
        for i in 1..=6 {
            parts.push(ic(&format!("U{i}"), 2));
            parts.push(cap(&format!("C{i}"), "100nF", 2));
        }
        // Six ICs and no bulk: the bulk half alone fires.
        let found = findings(&board(parts.clone()));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            !found[0].message.contains("~100 nF per IC"),
            "{}",
            found[0].message
        );
        assert!(found[0].message.contains("no bulk"), "{}", found[0].message);
        parts.push(cap("C10", "10uF", 2));
        assert!(findings(&board(parts)).is_empty());
    }

    #[test]
    fn modules_connectors_and_dnp_caps_follow_the_rules() {
        // A dev-board module on the rail brings its own decoupling.
        let nano = Component {
            reference: "A1".into(),
            value: "Arduino_Nano_v3.x".into(),
            lib_id: "MCU_Module:Arduino_Nano_v3.x".into(),
            pins: vec![pin("4", 1, "GND"), pin("27", 2, "+5V"), pin("30", 2, "VCC")],
            ..Default::default()
        };
        assert!(findings(&board(vec![nano])).is_empty());
        // A do-not-populate capacitor decouples nothing.
        let mut c1 = cap("C1", "100nF", 2);
        c1.dnp = true;
        assert_eq!(
            findings(&board(vec![ic("U1", 2), ic("U2", 2), c1])).len(),
            1
        );
    }

    #[test]
    fn addressable_leds_are_not_counted_as_ics() {
        // A chain of smart LEDs (VDD, DOUT, VSS, DIN) on a rail: by designator
        // or by an `LED_…` footprint, none counts as an IC wanting a bypass cap.
        let led = |r: &str, fp: &str| Component {
            reference: r.into(),
            value: "SmartLED".into(),
            footprint: fp.into(),
            pins: vec![
                pin("1", 2, "VDD"),
                pin("2", 9, "DOUT"),
                pin("3", 1, "VSS"),
                pin("4", 9, "DIN"),
            ],
            ..Default::default()
        };
        let chain = vec![
            led("LED1", "Custom:Smart_2020"),
            led("LED2", "Custom:Smart_2020"),
            led("D3", "LED_SMD:LED_RGB_2020_PLCC4"),
            led("D4", "LED_SMD:LED_RGB_2020_PLCC4"),
        ];
        assert!(
            findings(&board(chain.clone())).is_empty(),
            "LEDs are not ICs"
        );
        // Real ICs on the same rail are still counted, and only they.
        let mut parts = chain;
        parts.extend([ic("U1", 2), ic("U2", 2)]);
        let found = findings(&board(parts));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("2 ICs"), "{}", found[0].message);
        // An OLED display is not mistaken for an LED.
        assert!(!is_light_emitter(&led("U9", "Display:OLED_128x64")));
    }

    #[test]
    fn supply_pin_names_are_matched_whole() {
        for f in ["VCC", "VDD_1", "V+", "AVDD", "VCCIO", "~{VCC}"] {
            assert!(is_supply_pin_name(f), "{f}");
        }
        for f in ["VCCSENSE", "VIN", "VREF", "GND", "EN", "VOUT"] {
            assert!(!is_supply_pin_name(f), "{f}");
        }
    }
}
