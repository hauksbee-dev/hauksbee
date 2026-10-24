//! A USB-C receptacle whose CC pull resistors are the wrong value or the wrong
//! kind for the port's role.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A sink announces itself with a 5.1 kΩ pull-down (Rd) on each CC pin; a
//! source advertises its current with a pull-up (Rp) to the CC pin. The values
//! come from the USB Type-C Cable and Connector Specification, Release 2.0
//! (sha256 `87d15160...dc9981`): Table 4-25 (p. 236) gives Rd as 5.1 kΩ ±20 %
//! to ground, and Table 4-24 (p. 235) gives Rp as 56 kΩ ±20 %, 22 kΩ ±5 % or
//! 10 kΩ ±5 % to a 4.75-5.5 V supply, and 36 kΩ ±20 %, 12 kΩ ±5 % or 4.7 kΩ
//! ±5 % to 3.3 V ±5 %. A sink with a 10 kΩ or 56 kΩ pull-down reads the wrong
//! voltage from a 3 A or 1.5 A source and is never recognised; a sink that has
//! Rp pull-ups, or a source with a pull-down, is never recognised either.
//!
//! The existing `--usb-c` report attaches a source to the receptacle and
//! judges the pin voltages; this check names the resistor.
//!
//! Judged only on definite evidence:
//!
//! - **The CC line is plain.** Only a CC net (with the nets joined to it
//!   through 0 Ω links and ferrites) that carries resistors to ground or to a
//!   supply, capacitors and ESD diodes. Any IC pin on it (a PD controller or a
//!   PMIC with its own Rd, an MCU with a CC pin) or a resistor to another
//!   signal leaves the line unjudged.
//! - **Rd value.** The pull-downs of one CC line in parallel outside 5.1 kΩ
//!   ±20 % (4.08-6.12 kΩ) are Medium; High where the receptacle would not be
//!   recognised by a 80 µA, 180 µA or 330 µA source (the same solver as the
//!   `--usb-c` report decides).
//! - **Rp value.** A single pull-up to a rail of 5 V or 3.3 V that is outside
//!   every band of that rail's row of Table 4-24 is Medium.
//! - **A CC pin on a ground net** is High.
//! - **Role.** A port with pull-ups and no pull-down is a source, and one with
//!   pull-downs and no pull-up is a sink. The side VBUS is on says the same: a
//!   voltage-regulator card output or a `power_out` pin on VBUS is a source,
//!   an input is a sink. When both are stated and they disagree the port is
//!   High; when VBUS is fed and drawn alike (a charger with an OTG boost) it
//!   says nothing.
//!
//! DNP resistors take no part.

use std::collections::BTreeSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_ground, is_plain_resistor, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Severity,
};
use hauksbee_models::netclass::{is_positive_supply, rail_voltage};
use hauksbee_models::value::parse_value;
use hauksbee_models::ModelLibrary;

use super::usb_c::{
    all_receptacle_cc_nets, classify_attach, ground_net_ids, nets_bridged_to, Cable, Rp,
    SinkTermination,
};

/// Rd in ohms and its tolerance (Table 4-25, p. 236).
const RD_NOMINAL: f64 = 5_100.0;
const RD_TOLERANCE: f64 = 0.20;

/// Rp in ohms and tolerance per supply (Table 4-24, p. 235): to 4.75-5.5 V and
/// to 3.3 V ±5 %.
const RP_TO_5V: [(f64, f64); 3] = [(56_000.0, 0.20), (22_000.0, 0.05), (10_000.0, 0.05)];
const RP_TO_3V3: [(f64, f64); 3] = [(36_000.0, 0.20), (12_000.0, 0.05), (4_700.0, 0.05)];

/// What the board states about the direction of a port's VBUS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vbus {
    Source,
    Sink,
}

/// What one CC line is made of.
#[derive(Default)]
struct Line {
    rd: Vec<f64>,
    /// Pull-ups with the rail voltage they go to.
    rp: Vec<(f64, f64)>,
    /// An IC pin, or a resistor to a signal: the line is not judged.
    opaque: bool,
}

fn parallel(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| 1.0 / values.iter().map(|v| 1.0 / v).sum::<f64>())
}

/// Whether `comp` is a part a CC line may carry without being a controller:
/// a capacitor, an ESD or TVS diode, a test point.
fn is_benign(comp: &Component) -> bool {
    let r = comp.reference.to_ascii_uppercase();
    let lib = comp.lib_id.to_ascii_lowercase();
    is_capacitor(comp)
        || r.starts_with("TP")
        || (r.starts_with('D') && !r.starts_with("DC"))
        || lib.contains("tvs")
        || lib.contains("esd")
}

/// What the parts on `group` make of the CC line.
fn read_line(board: &ExtractedBoard, receptacle: &str, group: &BTreeSet<i64>) -> Line {
    let grounds = ground_net_ids(board);
    let mut line = Line::default();
    for comp in &board.components {
        if comp.reference == receptacle {
            continue;
        }
        if !comp
            .pins
            .iter()
            .any(|p| p.net.is_some_and(|n| group.contains(&n)))
        {
            continue;
        }
        if !AssemblyState::of(comp).is_present() {
            // DNP takes no part; an identity-refused record cannot be trusted.
            if AssemblyState::of(comp).fitted().is_none() && !comp.dnp {
                line.opaque = true;
            }
            continue;
        }
        if is_benign(comp) {
            continue;
        }
        if !is_plain_resistor(comp) || comp.pins.len() != 2 {
            line.opaque = true;
            continue;
        }
        let Some(ohms) = parse_value(&comp.value).map(|v| v.si).filter(|v| *v > 0.0) else {
            line.opaque = true;
            continue;
        };
        let far = comp
            .pins
            .iter()
            .filter_map(|p| p.net)
            .find(|n| !group.contains(n));
        match far {
            Some(n) if grounds.contains(&n) => line.rd.push(ohms),
            Some(n) => {
                let name = board.net(n).map_or("", |x| x.name.as_str());
                match rail_voltage(name).filter(|_| is_positive_supply(name)) {
                    Some(v) => line.rp.push((ohms, v)),
                    None => line.opaque = true,
                }
            }
            None => {}
        }
    }
    line
}

/// Whether `value` is inside any `(nominal, tolerance)` band.
fn in_bands(value: f64, bands: &[(f64, f64)]) -> bool {
    bands
        .iter()
        .any(|(n, t)| (value - n).abs() <= n * t * 1.000_001)
}

/// The VBUS nets of a receptacle, joined through ferrites and 0 Ω links.
fn vbus_group(board: &ExtractedBoard, receptacle: &Component) -> BTreeSet<i64> {
    let mut group = BTreeSet::new();
    for pin in &receptacle.pins {
        let pad = pin.number.to_ascii_uppercase();
        let named = pin.function.to_ascii_uppercase().contains("VBUS");
        if let Some(net) = pin.net.filter(|n| *n != 0) {
            if named || matches!(pad.as_str(), "A4" | "A9" | "B4" | "B9") {
                group.extend(nets_bridged_to(board, net));
            }
        }
    }
    group
}

/// The direction the board's own parts give VBUS, when it states one.
fn vbus_direction(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    receptacle: &Component,
) -> Option<Vbus> {
    let group = vbus_group(board, receptacle);
    let (mut source, mut sink) = (false, false);
    for comp in &board.components {
        if comp.reference == receptacle.reference {
            continue;
        }
        let Some(fitted) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        for pin in comp.pins.iter().filter(|p| p.net.is_some_and(|n| group.contains(&n))) {
            let kind = pin.kind.to_ascii_lowercase();
            source |= kind.starts_with("power_out");
            sink |= kind.starts_with("power_in");
        }
        let model = hauksbee_bind::binder::resolve(lib, fitted)
            .model
            .filter(|m| super::card_fits(board, comp, m));
        if let Some(m) = model.filter(|m| m.kind == hauksbee_models::ComponentKind::Vreg) {
            let on = |role| {
                super::envelope::role_net_id(comp, &m, role).is_some_and(|n| group.contains(&n))
            };
            source |= on("out");
            sink |= on("in");
        }
    }
    match (source, sink) {
        (true, false) => Some(Vbus::Source),
        (false, true) => Some(Vbus::Sink),
        _ => None,
    }
}

/// Whether a source of every current detects a sink through each of its CC
/// pins taken alone: a plug goes in either way up.
fn recognised_by_every_source(rds: [Option<f64>; 2]) -> bool {
    rds.iter().flatten().all(|rd| {
        let term = SinkTermination {
            cc1_rd_ohms: Some(*rd),
            cc2_rd_ohms: None,
            shared_net: false,
        };
        [Rp::Default, Rp::Med1A5, Rp::High3A]
            .iter()
            .all(|rp| classify_attach(term, *rp, Cable::Passive).powers())
    })
}

fn kohm(ohms: f64) -> String {
    if ohms >= 1000.0 {
        let text = format!("{:.2}", ohms / 1000.0);
        format!("{} kΩ", text.trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("{ohms:.0} Ω")
    }
}

/// The CC-termination findings of `board`.
#[must_use]
pub fn cc_termination_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for rec in all_receptacle_cc_nets(board) {
        let Some(receptacle) = board
            .components
            .iter()
            .find(|c| c.reference == rec.reference)
            .filter(|c| AssemblyState::of(c).fitted().is_some())
        else {
            continue;
        };
        let name_of = |id: i64| board.net(id).map_or_else(String::new, |n| n.name.clone());
        let lines = [("CC1", rec.cc1_net), ("CC2", rec.cc2_net)];
        let mut seen = BTreeSet::new();
        let mut rd_eff: [Option<f64>; 2] = [None, None];
        let mut sink_lines = 0;
        let mut source_lines = 0;
        let mut pulldown_issues: Vec<String> = Vec::new();
        let mut pullup_issues: Vec<String> = Vec::new();
        let mut nets = Vec::new();
        for (i, (label, net)) in lines.iter().enumerate() {
            if !seen.insert(*net) {
                continue;
            }
            if is_ground(&name_of(*net)) {
                report
                    .findings
                    .push(grounded_finding(receptacle, label, &name_of(*net)));
                continue;
            }
            let group: BTreeSet<i64> = nets_bridged_to(board, *net).into_iter().collect();
            let line = read_line(board, &receptacle.reference, &group);
            if line.opaque {
                continue;
            }
            match (parallel(&line.rd), line.rp.is_empty()) {
                (Some(rd), true) => {
                    sink_lines += 1;
                    rd_eff[i] = Some(rd);
                    if (rd - RD_NOMINAL).abs() > RD_NOMINAL * RD_TOLERANCE * 1.000_001 {
                        pulldown_issues.push(format!("{label} {}", kohm(rd)));
                        nets.push(name_of(*net));
                    }
                }
                (None, false) => {
                    source_lines += 1;
                    if let [(ohms, volts)] = line.rp[..] {
                        let bands = if (4.75..=5.5).contains(&volts) {
                            Some(&RP_TO_5V)
                        } else if (3.135..=3.465).contains(&volts) {
                            Some(&RP_TO_3V3)
                        } else {
                            None
                        };
                        if bands.is_some_and(|b| !in_bands(ohms, b)) {
                            pullup_issues.push(format!("{label} {} to {volts} V", kohm(ohms)));
                            nets.push(name_of(*net));
                        }
                    }
                }
                _ => {}
            }
        }
        if !pulldown_issues.is_empty() {
            let high = !recognised_by_every_source(rd_eff);
            report
                .findings
                .push(pulldown_finding(receptacle, &pulldown_issues, &nets, high));
        }
        if !pullup_issues.is_empty() {
            report
                .findings
                .push(pullup_finding(receptacle, &pullup_issues, &nets));
        }
        let port_sink = sink_lines > 0 && source_lines == 0;
        let port_source = source_lines > 0 && sink_lines == 0;
        if !(port_sink || port_source) {
            continue;
        }
        match (vbus_direction(board, lib, receptacle), port_sink) {
            (Some(Vbus::Sink), false) => report.findings.push(role_finding(
                receptacle,
                "pull-ups (Rp) on its CC pins, a source's termination,",
                "VBUS only goes into the board (a sink)",
            )),
            (Some(Vbus::Source), true) => report.findings.push(role_finding(
                receptacle,
                "pull-downs (Rd) on its CC pins, a sink's termination,",
                "the board drives VBUS (a source)",
            )),
            _ => {}
        }
    }
    report
}

fn grounded_finding(receptacle: &Component, label: &str, net: &str) -> LintFinding {
    LintFinding {
        check: LintCheck::UsbCcTermination,
        severity: Severity::High,
        message: format!(
            "{} pin {label} is on ground net \"{net}\". A CC pin is a sensing line: grounded \
             directly it reads as a short and the port is never recognised (it needs a 5.1 kΩ Rd \
             to ground on a sink, USB Type-C R2.0 Table 4-25, p. 236)",
            receptacle.reference
        ),
        refs: vec![receptacle.reference.clone()],
        nets: vec![net.to_string()],
    }
}

fn pulldown_finding(
    receptacle: &Component,
    issues: &[String],
    nets: &[String],
    high: bool,
) -> LintFinding {
    LintFinding {
        check: LintCheck::UsbCcTermination,
        severity: if high { Severity::High } else { Severity::Medium },
        message: format!(
            "{} CC pull-down is not 5.1 kΩ ({}). A USB-C sink presents Rd = 5.1 kΩ ±20 % \
             (4.08 to 6.12 kΩ) on each CC pin (USB Type-C R2.0 Table 4-25, p. 236); a source \
             reads the current it may offer from the voltage across it, so another value is {}",
            receptacle.reference,
            issues.join(", "),
            if high {
                "not recognised as a sink by a 1.5 A or 3 A source, and VBUS is never applied"
            } else {
                "read as the wrong advertisement"
            }
        ),
        refs: vec![receptacle.reference.clone()],
        nets: nets.to_vec(),
    }
}

fn pullup_finding(receptacle: &Component, issues: &[String], nets: &[String]) -> LintFinding {
    LintFinding {
        check: LintCheck::UsbCcTermination,
        severity: Severity::Medium,
        message: format!(
            "{} CC pull-up is not a standard Rp value ({}). A source advertises its current \
             with Rp = 56 kΩ ±20 %, 22 kΩ ±5 % or 10 kΩ ±5 % to 5 V, or 36 kΩ ±20 %, 12 kΩ ±5 % \
             or 4.7 kΩ ±5 % to 3.3 V (USB Type-C R2.0 Table 4-24, p. 235)",
            receptacle.reference,
            issues.join(", ")
        ),
        refs: vec![receptacle.reference.clone()],
        nets: nets.to_vec(),
    }
}

fn role_finding(receptacle: &Component, has: &str, vbus: &str) -> LintFinding {
    LintFinding {
        check: LintCheck::UsbCcTermination,
        severity: Severity::High,
        message: format!(
            "{} has {has} but {vbus}. Two sources or two sinks on a Type-C link never recognise \
             each other and VBUS is not applied (USB Type-C R2.0 Tables 4-24 and 4-25, pp. 235-236). \
             Fit Rd (5.1 kΩ to ground) on a sink and Rp on a source",
            receptacle.reference
        ),
        refs: vec![receptacle.reference.clone()],
        nets: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    const GND: i64 = 1;
    const VBUS: i64 = 2;
    const CC1: i64 = 3;
    const CC2: i64 = 4;
    const V33: i64 = 5;
    const SIG: i64 = 6;
    const VOUT: i64 = 7;

    const NETS: [(i64, &str); 7] = [
        (GND, "GND"),
        (VBUS, "VBUS"),
        (CC1, "CC1"),
        (CC2, "CC2"),
        (V33, "+3V3"),
        (SIG, "SIG"),
        (VOUT, "+5V"),
    ];

    fn receptacle(cc1: i64, cc2: i64) -> Component {
        let mut j = part(
            "J1",
            "USB4105-GF-A",
            &[("A4", VBUS), ("B9", VBUS), ("A5", cc1), ("B5", cc2)],
        );
        j.lib_id = "Connector:USB_C_Receptacle_USB2.0".into();
        j.footprint = "Connector_USB:USB_C_Receptacle_GCT_USB4105".into();
        j.pins[2].function = "CC1".into();
        j.pins[3].function = "CC2".into();
        j
    }

    fn res(reference: &str, value: &str, a: i64, b: i64) -> Component {
        let mut r = part(reference, value, &[("1", a), ("2", b)]);
        r.lib_id = "Device:R".into();
        r
    }

    /// A sink-side load on VBUS: a `power_in` pin of a charger.
    fn sink_load() -> Component {
        let mut u = part("U1", "BQ24075", &[("1", VBUS), ("2", GND)]);
        u.pins[0].kind = "power_in".into();
        u
    }

    /// A source-side driver of VBUS: a `power_out` pin of a power switch.
    fn source_driver() -> Component {
        let mut u = part("U2", "TPS2051", &[("1", VBUS), ("2", VOUT)]);
        u.pins[0].kind = "power_out".into();
        u
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        cc_termination_lint(&board(&NETS, parts), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_sink_with_ten_kilohm_pull_downs_is_flagged_high() {
        // 10 kΩ is read as 3.3 V by a 3 A source: open, never recognised.
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "10k", CC1, GND),
            res("R2", "10k", CC2, GND),
            sink_load(),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::UsbCcTermination);
        assert_eq!(f.severity, Severity::High);
        assert!(f.message.contains("CC1 10 kΩ"), "{}", f.message);
        assert!(f.message.contains("Table 4-25"), "{}", f.message);
        assert_eq!(f.refs, ["J1"]);
    }

    #[test]
    fn one_wrong_pull_down_is_high_because_the_plug_goes_in_either_way_up() {
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "5.1k", CC1, GND),
            res("R2", "10k", CC2, GND),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(found[0].message.contains("CC2 10 kΩ"), "{}", found[0].message);
    }

    #[test]
    fn a_pull_down_slightly_out_of_tolerance_is_medium() {
        // 3.9 kΩ is outside 5.1 kΩ ±20 % but every source still sees a sink.
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "3.9k", CC1, GND),
            res("R2", "3.9k", CC2, GND),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
    }

    #[test]
    fn a_correct_sink_is_silent() {
        for rd in ["5.1k", "5k1"] {
            let found = run(vec![
                receptacle(CC1, CC2),
                res("R1", rd, CC1, GND),
                res("R2", rd, CC2, GND),
                sink_load(),
            ]);
            assert!(found.is_empty(), "{rd}: {found:#?}");
        }
        // Two 10 kΩ in parallel are 5 kΩ.
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "10k", CC1, GND),
            res("R3", "10k", CC1, GND),
            res("R2", "5.1k", CC2, GND),
        ]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_dnp_wrong_value_resistor_and_an_ic_on_the_line_are_silent() {
        let mut dnp = res("R3", "56k", CC1, GND);
        dnp.dnp = true;
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "5.1k", CC1, GND),
            dnp,
            res("R2", "5.1k", CC2, GND),
        ]);
        assert!(found.is_empty(), "{found:#?}");
        // A PD controller on the line: its own Rd, not judged.
        let mut pd = part("U3", "STUSB4500", &[("1", CC1), ("2", CC2)]);
        pd.pins[0].function = "CC1".into();
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "10k", CC1, GND),
            res("R2", "10k", CC2, GND),
            pd,
        ]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_cc_pin_on_ground_is_flagged() {
        let found = run(vec![receptacle(GND, CC2), res("R2", "5.1k", CC2, GND)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(found[0].message.contains("ground net"), "{}", found[0].message);
    }

    #[test]
    fn a_pull_up_that_is_not_an_rp_value_is_flagged() {
        // 33 kΩ to 5 V is none of 56k±20 %, 22k±5 % or 10k±5 %.
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "33k", CC1, VOUT),
            res("R2", "33k", CC2, VOUT),
            source_driver(),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(found[0].message.contains("Table 4-24"), "{}", found[0].message);
        // Standard values to either rail are silent.
        for (v, rail) in [("56k", VOUT), ("22k", VOUT), ("10k", VOUT), ("36k", V33), ("12k", V33), ("4.7k", V33)] {
            let found = run(vec![
                receptacle(CC1, CC2),
                res("R1", v, CC1, rail),
                res("R2", v, CC2, rail),
                source_driver(),
            ]);
            assert!(found.is_empty(), "{v}: {found:#?}");
        }
    }

    #[test]
    fn pull_ups_on_a_port_whose_vbus_is_an_input_are_flagged() {
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "56k", CC1, VOUT),
            res("R2", "56k", CC2, VOUT),
            sink_load(),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(found[0].message.contains("(Rp)"), "{}", found[0].message);
    }

    #[test]
    fn pull_downs_on_a_port_that_drives_vbus_are_flagged() {
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "5.1k", CC1, GND),
            res("R2", "5.1k", CC2, GND),
            source_driver(),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(found[0].message.contains("(Rd)"), "{}", found[0].message);
    }

    #[test]
    fn a_dual_role_vbus_or_an_unstated_one_says_nothing_of_the_role() {
        // Fed and drawn alike: a charger with an OTG boost.
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "5.1k", CC1, GND),
            res("R2", "5.1k", CC2, GND),
            sink_load(),
            source_driver(),
        ]);
        assert!(found.is_empty(), "{found:#?}");
        // Nothing on VBUS but the receptacle.
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "5.1k", CC1, GND),
            res("R2", "5.1k", CC2, GND),
        ]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_resistor_to_a_signal_leaves_the_line_unjudged() {
        let found = run(vec![
            receptacle(CC1, CC2),
            res("R1", "10k", CC1, GND),
            res("R9", "100k", CC1, SIG),
            res("R2", "5.1k", CC2, GND),
        ]);
        assert!(found.is_empty(), "{found:#?}");
    }
}
