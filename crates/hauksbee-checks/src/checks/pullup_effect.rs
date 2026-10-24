//! Pull-ups that are present and cannot do their job.
//!
//! A pull-up resistor exists to hold a line at a valid HIGH while nothing
//! drives it. Whether it does is a question about three numbers the board
//! fixes exactly: the voltage the resistor's far end sits at, the resistor, and
//! the receiver's guaranteed input-high level VIH(min), which the receiving
//! part's card states per supply. The other checks around it ask different
//! questions of the same resistor: `MissingI2cPullup` whether one exists,
//! `bus_loading` whether an I2C bus's pull-ups are sized for its sink current
//! and rise time, `back_power` whether its rail is ABOVE the receiver's supply.
//! This one asks whether the line it is meant to hold ever gets high enough:
//!
//! * **The rail is too low.** The pull-up's open-circuit level (the Thevenin
//!   level of all its pull-ups, an unpowered rail counting as 0 V) is below the
//!   VIH(min) of a receiver on the line: a 3.3 V pull-up on a 5 V CMOS input
//!   (VIH 3.5 V), a 1.8 V pull-up on a 3.3 V part. The line idles in or below
//!   the band where the datasheet guarantees no logic state. Below the
//!   receiver's VIL it reads firmly low and High is the severity; inside the
//!   undefined band it is Medium.
//! * **The rail never comes up.** A pull-up to a rail the board's rail model
//!   says never comes up (only unfitted parts would power it) holds the line
//!   at 0 V: the idle level the resistor was fitted for is not there.
//! * **The resistor is too large for the receivers' leakage.** The level the
//!   line settles at is `V - I_leak x R`, with `I_leak` the sum of the
//!   high-level input leakage (`iih_a`) the receivers' cards state. Only the
//!   stated leakage is counted, so the drop is a floor: a line below VIH at
//!   the floor is below it on the real board.
//!
//! The check fires only on a line it can judge exactly: every resistor on it
//! goes to a rail (a divider or bias network is a design intent this check
//! does not second-guess), the receiver's VIH(min) is on its card at its own
//! supply, and every other pin on the line is a known receiver, an open-drain
//! output, a passive or a capacitor. Anything that could push the line high
//! itself (a push-pull output, an unclassified pin, a connector) leaves the
//! pull-up a bias, not the only source of the high, and the check stays quiet.
//! A pull-up of 100 kohm or more onto a declared input whose card gives no
//! VIH at its supply is the one case it says it could not judge, as an
//! abstention.

use std::collections::HashSet;
use std::hash::BuildHasher;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity, ABSTENTION_MARKER,
    REMEDY_MARKER,
};
use hauksbee_ir::units::{format_amps, format_volts};
use hauksbee_models::ModelLibrary;

use super::logic_levels::{loads_on_net, unjudged_inputs_on_net, LogicLoad};

use hauksbee_bind::binder::power_rail_voltage;

/// A pull-up this large or larger is weak enough for input leakage to matter,
/// so a declared input whose card states no limit is worth an abstention.
const WEAK_PULLUP_OHMS: f64 = 100_000.0;

/// Pin electrical types that cannot push a line high by themselves.
fn is_passive_pin_kind(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "input" | "open_collector" | "open_emitter" | "passive" | "no_connect" | "unconnected"
    )
}

/// One pull-up resistor: ohms and the rail it goes to (`volts` 0 when the rail
/// never comes up).
struct Pull {
    reference: String,
    ohms: f64,
    rail: String,
    volts: f64,
    unpowered: bool,
}

/// How the net's resistors read: all of them pull-ups to rails (`Some`), or the
/// net carries some other resistor (a divider, a bias, a series part) and the
/// check does not judge it (`None`).
fn pull_ups<S: BuildHasher>(
    board: &ExtractedBoard,
    net_id: i64,
    never_up: &HashSet<String, S>,
) -> Option<Vec<Pull>> {
    let mut pulls = Vec::new();
    for (c, _) in board.net_members(net_id) {
        if !AssemblyState::of(c).is_present() || !hauksbee_extract::is_resistor(c) {
            continue;
        }
        if pulls.iter().any(|p: &Pull| p.reference == c.reference) {
            continue;
        }
        let ohms = super::bus_loading::resistor_ohms(c)?;
        let far = c
            .pins
            .iter()
            .filter_map(|p| p.net)
            .find(|&id| id != 0 && id != net_id)?;
        let name = &board.net(far)?.name;
        if never_up.contains(name) {
            pulls.push(Pull {
                reference: c.reference.clone(),
                ohms,
                rail: name.clone(),
                volts: 0.0,
                unpowered: true,
            });
            continue;
        }
        let volts = power_rail_voltage(name).filter(|v| *v > 0.0)?;
        pulls.push(Pull {
            reference: c.reference.clone(),
            ohms,
            rail: name.clone(),
            volts,
            unpowered: false,
        });
    }
    (!pulls.is_empty()).then_some(pulls)
}

/// Whether every pin on the net besides the pull-ups is something that cannot
/// push the line high: a receiver the card declares, an open-drain output, a
/// passive or a capacitor.
fn only_passive_company(board: &ExtractedBoard, net_id: i64, loads: &[LogicLoad]) -> bool {
    board.net_members(net_id).into_iter().all(|(c, p)| {
        if !AssemblyState::of(c).is_present()
            || hauksbee_extract::is_resistor(c)
            || hauksbee_extract::is_capacitor(c)
        {
            return true;
        }
        if hauksbee_extract::is_connector_like(c) {
            return false;
        }
        if loads.iter().any(|l| l.reference == c.reference) {
            // A receiver; a part that is also an output on this pad would show
            // as an output pin type, which the schematic names.
            return p.kind.is_empty() || is_passive_pin_kind(&p.kind);
        }
        is_passive_pin_kind(&p.kind)
    })
}

/// The Thevenin level (V) and resistance (ohm) of the pull-ups together.
fn thevenin(pulls: &[Pull]) -> (f64, f64) {
    let g: f64 = pulls.iter().map(|p| 1.0 / p.ohms).sum();
    let i: f64 = pulls.iter().map(|p| p.volts / p.ohms).sum();
    (i / g, 1.0 / g)
}

/// The pull-ups, as a message names them.
fn pulls_clause(pulls: &[Pull]) -> String {
    let each = |p: &Pull| format!("{} ({:.0} ohm) to '{}'", p.reference, p.ohms, p.rail);
    pulls.iter().map(each).collect::<Vec<_>>().join(" and ")
}

fn finding(
    severity: Severity,
    message: String,
    pulls: &[Pull],
    loads: &[&LogicLoad],
    net: &str,
) -> LintFinding {
    let mut refs: Vec<String> = pulls.iter().map(|p| p.reference.clone()).collect();
    for l in loads {
        if !refs.contains(&l.reference) {
            refs.push(l.reference.clone());
        }
    }
    let mut nets = vec![net.to_string()];
    for p in pulls {
        if !nets.contains(&p.rail) {
            nets.push(p.rail.clone());
        }
    }
    LintFinding {
        check: LintCheck::PullupIneffective,
        severity,
        message,
        refs,
        nets,
    }
}

/// The findings for one pulled line, judged against its receivers.
fn judge(net: &str, pulls: &[Pull], loads: &[LogicLoad]) -> Option<LintFinding> {
    let (v_open, r_eff) = thevenin(pulls);
    let all_unpowered = pulls.iter().all(|p| p.unpowered);
    // The receiver that needs the most: the highest VIH(min).
    let hardest = loads.iter().max_by(|a, b| {
        a.vih
            .partial_cmp(&b.vih)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let names: Vec<&LogicLoad> = loads.iter().collect();

    if all_unpowered {
        let rails: Vec<&str> = pulls.iter().map(|p| p.rail.as_str()).collect();
        return Some(finding(
            Severity::Medium,
            format!(
                "'{net}' is pulled up by {}, a rail the board never powers (see the \
                 rail_unpowered finding), so the resistor holds the line at 0 V and \
                 {} {} never sees the idle HIGH the pull-up was fitted for. \
                 {REMEDY_MARKER}Fit the source of {}, or pull '{net}' up to a rail that is \
                 powered.",
                pulls_clause(pulls),
                hardest.reference,
                hardest.role,
                rails.join(", "),
            ),
            pulls,
            &names,
            net,
        ));
    }

    if v_open < hardest.vih {
        let firmly_low = v_open < hardest.vil;
        let band = if firmly_low {
            format!(
                "below its VIL of {}, so it reads low",
                format_volts(hardest.vil)
            )
        } else {
            format!(
                "inside the {} to {} band where its datasheet guarantees no logic state",
                format_volts(hardest.vil),
                format_volts(hardest.vih)
            )
        };
        return Some(finding(
            if firmly_low {
                Severity::High
            } else {
                Severity::Medium
            },
            format!(
                "'{net}' is pulled up by {} to {} at best, but {} {} (supply {}) needs at \
                 least VIH = {} to read a high: the line idles {band}. The pull-up cannot hold \
                 the input high with nothing driving it. {REMEDY_MARKER}Pull '{net}' up to \
                 the receiver's own supply, or add a level shifter.",
                pulls_clause(pulls),
                format_volts(v_open),
                hardest.reference,
                hardest.role,
                format_volts(hardest.vcc),
                format_volts(hardest.vih),
            ),
            pulls,
            &names,
            net,
        ));
    }

    // The leakage the cards state, summed: a floor on the real drop.
    let leak: f64 = loads.iter().filter_map(|l| l.leak_a).sum();
    let level = leak.mul_add(-r_eff, v_open);
    if leak > 0.0 && level < hardest.vih {
        return Some(finding(
            Severity::Medium,
            format!(
                "'{net}' is pulled up by {} to {}, but the receivers' stated input leakage \
                 ({}) across {:.0} ohm drops {} and leaves the line at {} at best, below \
                 {} {}'s VIH of {} at its {} supply. The pull-up is too large for the load. \
                 {REMEDY_MARKER}Use a pull-up under {:.0} ohm, which keeps the line at \
                 or above VIH with the stated leakage.",
                pulls_clause(pulls),
                format_volts(v_open),
                format_amps(leak),
                r_eff,
                format_volts(v_open - level.max(0.0)),
                format_volts(level.max(0.0)),
                hardest.reference,
                hardest.role,
                format_volts(hardest.vih),
                format_volts(hardest.vcc),
                (v_open - hardest.vih) / leak,
            ),
            pulls,
            &names,
            net,
        ));
    }
    None
}

/// The abstention for a weak pull-up onto a declared input whose VIH the card
/// does not give at its supply.
fn abstention(net: &str, pulls: &[Pull], unjudged: &[(String, String)]) -> LintFinding {
    let (reference, role) = &unjudged[0];
    let mut f = finding(
        Severity::Low,
        format!(
            "Pull-up check{ABSTENTION_MARKER} on '{net}': {} is weak, and {reference} {role} is a \
             declared input whose card gives no VIH at its supply (or its supply level is \
             unknown), so whether the line reaches a valid high was not judged.",
            pulls_clause(pulls),
        ),
        pulls,
        &[],
        net,
    );
    f.refs.push(reference.clone());
    f
}

/// The do-not-populate resistors on a net: left out of the assembled board, so
/// never counted as pull-ups, but named in a finding so a reader who sees a
/// resistor drawn on the line knows why it was not credited.
fn dnp_resistor_refs(board: &ExtractedBoard, net_id: i64) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for (c, _) in board.net_members(net_id) {
        if matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
            && hauksbee_extract::is_resistor(c)
            && !refs.contains(&c.reference)
        {
            refs.push(c.reference.clone());
        }
    }
    refs
}

/// Say in a finding that `dnp` resistors on its line were not counted. The note
/// goes ahead of the remedy so the remedy stays the message's last sentence.
fn note_uncounted_dnp(finding: &mut LintFinding, dnp: &[String]) {
    if dnp.is_empty() {
        return;
    }
    let one = dnp.len() == 1;
    let note = format!(
        "{} on this line {} marked do-not-populate and not counted as {}. ",
        dnp.join(", "),
        if one { "is" } else { "are" },
        if one { "a pull-up" } else { "pull-ups" },
    );
    let at = finding
        .message
        .find(REMEDY_MARKER)
        .unwrap_or(finding.message.len());
    finding.message.insert_str(at, &note);
}

/// The pull-up effect lint. `never_up` names the nets of rails the rail model
/// says never come up.
#[must_use]
pub fn pullup_effect_lint<S: BuildHasher>(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    never_up: &HashSet<String, S>,
) -> NetLintReport {
    let mut report = NetLintReport::default();
    let rails = |net: &str| power_rail_voltage(net);
    for net in &board.nets {
        if net.id == 0
            || power_rail_voltage(&net.name).is_some()
            || never_up.contains(&net.name)
            || super::is_ground_name(&net.name)
            || super::is_unconnected_net(&net.name)
        {
            continue;
        }
        let members = board.net_members(net.id);
        if members.len() < 2 || !members.iter().any(|(c, _)| is_resistor_ref(c)) {
            continue;
        }
        let Some(pulls) = pull_ups(board, net.id, never_up) else {
            continue;
        };
        let loads = loads_on_net(board, lib, &net.name, &rails);
        if !only_passive_company(board, net.id, &loads) {
            continue;
        }
        let first_new = report.findings.len();
        let dnp = dnp_resistor_refs(board, net.id);
        if loads.is_empty() {
            let (_, r_eff) = thevenin(&pulls);
            // The conductance round trip can land a hair under the value the
            // designer wrote, so the comparison allows for it.
            if r_eff >= WEAK_PULLUP_OHMS * (1.0 - 1e-9) {
                let unjudged = unjudged_inputs_on_net(board, lib, &net.name, &rails);
                if !unjudged.is_empty() {
                    report
                        .findings
                        .push(abstention(&net.name, &pulls, &unjudged));
                }
            }
        } else {
            report.findings.extend(judge(&net.name, &pulls, &loads));
        }
        for f in &mut report.findings[first_new..] {
            note_uncounted_dnp(f, &dnp);
        }
    }
    report
}

/// A reference that reads as a resistor, a cheap pre-filter.
fn is_resistor_ref(c: &Component) -> bool {
    hauksbee_extract::is_resistor(c)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use hauksbee_extract::Pin;

    /// A two-pad resistor `reference` of `value` on nets `a` and `b`.
    fn resistor(reference: &str, value: &str, a: i64, b: i64) -> Component {
        Component {
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            layer: "F.Cu".into(),
            ..part(reference, value, &[("1", a), ("2", b)])
        }
    }

    /// A 74LVC1G66 (pads 1 A, 2 B, 3 GND, 4 C, 5 VCC) on `vcc`, its control
    /// `C` pulled up by a 10k to `rail`. Nets: 1 GND, 2 `+3V3`, 3 `+5V`, 4
    /// `+1V2`, 5 CTRL, 6 SIGA, 7 SIGB.
    fn switch_board(vcc: i64, rail: i64, value: &str) -> ExtractedBoard {
        let nets = &[
            (1, "GND"),
            (2, "+3V3"),
            (3, "+5V"),
            (4, "+1V2"),
            (5, "CTRL"),
            (6, "SIGA"),
            (7, "SIGB"),
        ];
        let sw = part(
            "U1",
            "SN74LVC1G66DBVR",
            &[("1", 6), ("2", 7), ("3", 1), ("4", 5), ("5", vcc)],
        );
        board(nets, vec![sw, resistor("R1", value, 5, rail)])
    }

    fn run(b: &ExtractedBoard) -> Vec<LintFinding> {
        pullup_effect_lint(b, &ModelLibrary::builtin(), &HashSet::new()).findings
    }

    /// A 3.3 V pull-up on the control input of a part supplied from 5 V (VIH
    /// 3.5 V) never reaches a valid high: one Medium finding (3.3 V is inside
    /// the undefined band above the 1.5 V VIL). A 1.2 V pull-up is below VIL:
    /// High.
    #[test]
    fn a_pullup_rail_below_the_receivers_vih_fires() {
        let f = run(&switch_board(3, 2, "10k"));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, LintCheck::PullupIneffective);
        assert_eq!(f[0].severity, Severity::Medium);
        for says in ["'CTRL'", "R1", "U1 ctrl", "VIH = 3.5 V", "3.3 V"] {
            assert!(f[0].message.contains(says), "{says:?} in {}", f[0].message);
        }
        assert!(f[0].refs.contains(&"U1".to_string()) && f[0].refs.contains(&"R1".to_string()));
        let low = run(&switch_board(3, 4, "10k"));
        assert_eq!(low.len(), 1, "{low:?}");
        assert_eq!(low[0].severity, Severity::High);
        assert!(low[0].message.contains("reads low"), "{}", low[0].message);
    }

    /// Controls that stay silent: the pull-up on the receiver's own rail (the
    /// right rail at 3.3 V and at 5 V), a 5 V pull-up onto a 3.3 V part whose
    /// input is above VIH (that is `back_power`'s question, not this one), and a
    /// pull-up whose rail the board never powers is the only other fault.
    #[test]
    fn a_pullup_on_the_receivers_own_rail_is_silent() {
        assert!(run(&switch_board(2, 2, "10k")).is_empty(), "3.3 V on 3.3 V");
        assert!(run(&switch_board(3, 3, "10k")).is_empty(), "5 V on 5 V");
        assert!(
            run(&switch_board(2, 3, "10k")).is_empty(),
            "a 5 V pull-up onto 3.3 V reaches VIH; back_power judges it"
        );
    }

    /// A bias or divider network is design intent: a second resistor to ground
    /// on the line takes it out of this check's hands.
    #[test]
    fn a_divider_is_not_judged() {
        let mut b = switch_board(3, 2, "10k");
        b.components.push(resistor("R2", "10k", 5, 1));
        assert!(run(&b).is_empty());
    }

    /// Anything that could push the line high itself makes the pull-up a bias:
    /// a push-pull (output-typed) pin, an unclassified part, or a connector on
    /// the line keeps the check quiet.
    #[test]
    fn a_line_with_another_driver_is_not_judged() {
        let pin = |kind: &str| Pin {
            kind: kind.into(),
            ..crate::test_support::pin("1", 5)
        };
        let with = |reference: &str, value: &str, kind: &str| {
            let mut b = switch_board(3, 2, "10k");
            let mut other = part(reference, value, &[]);
            other.pins = vec![pin(kind)];
            b.components.push(other);
            run(&b).len()
        };
        assert_eq!(with("U9", "SOMEMCU", "output"), 0, "push-pull output");
        assert_eq!(with("U9", "SOMEMCU", ""), 0, "pin type unknown");
        assert_eq!(with("J1", "Conn_01x02", "passive"), 0, "connector");
        assert_eq!(
            with("U9", "SOMEMCU", "open_collector"),
            1,
            "an open-drain output cannot push high: still judged"
        );
    }

    /// A do-not-populate resistor is not a pull-up: it neither rescues a bad
    /// fitted one (here a second, DNP 10k to the receiver's own 5 V rail) nor is
    /// judged itself, and the finding says it was left out. A line whose only
    /// resistor is DNP has no pull-up to judge (the missing-pull-up checks cover
    /// that), so it stays silent here.
    #[test]
    fn a_dnp_resistor_is_not_counted_and_the_finding_says_so() {
        let mut b = switch_board(3, 2, "10k");
        let mut spare = resistor("R9", "10k", 5, 3);
        spare.dnp = true;
        b.components.push(spare);
        let f = run(&b);
        assert_eq!(f.len(), 1, "{f:?}");
        let msg = &f[0].message;
        assert!(
            msg.contains("R9 on this line is marked do-not-populate"),
            "{msg}"
        );
        assert!(msg.contains("R1") && msg.contains("3.3 V"), "{msg}");
        assert!(
            msg.find("R9").unwrap() < msg.find(REMEDY_MARKER).unwrap(),
            "the note sits ahead of the remedy: {msg}"
        );

        let mut only_dnp = switch_board(3, 3, "10k");
        only_dnp.components.retain(|c| c.reference != "R1");
        let mut lone = resistor("R9", "10k", 5, 2);
        lone.dnp = true;
        only_dnp.components.push(lone);
        assert!(run(&only_dnp).is_empty());
    }

    /// A pull-up to a rail the board never powers holds the line at 0 V.
    #[test]
    fn a_pullup_to_a_rail_that_never_comes_up_fires() {
        let b = switch_board(2, 4, "10k");
        let never_up = HashSet::from(["+1V2".to_string()]);
        let f = pullup_effect_lint(&b, &ModelLibrary::builtin(), &never_up).findings;
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].message.contains("never powers"), "{}", f[0].message);
        // The same pull-up with the rail up is the right rail for nothing here
        // (1.2 V into a 3.3 V part) and is judged as a low rail instead.
        assert_eq!(run(&b).len(), 1);
        // A powered rail the model does not flag stays silent.
        assert!(run(&switch_board(2, 2, "10k")).is_empty());
    }

    /// A one-input logic part `LEAKY` (pad 1 input, 2 ground, 3 supply) whose
    /// card states `extra` (the levels and leakage under test), on `+3V3`
    /// with a pull-up of `value` on its input.
    fn leaky_findings(extra: &str, value: &str) -> Vec<LintFinding> {
        let source = format!(
            r#"
[[models]]
id = "leaky"
kind = "analog_switch"
description = "pull-up fixture"
[models.match]
value_re = "^LEAKY$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
{extra}
[models.pins]
"1" = "a"
"2" = "b"
"3" = "gnd"
"4" = "ctrl"
"5" = "vcc"
"#
        );
        let dir = std::env::temp_dir().join(format!(
            "hauksbee_pullup_effect_{}_{}",
            source.len(),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), source).unwrap();
        let mut lib = ModelLibrary::empty();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "fixture card loads: {errors:?}");
        let nets = &[(1, "GND"), (2, "+3V3"), (3, "LINE"), (4, "SA"), (5, "SB")];
        let mut u = part(
            "U1",
            "LEAKY",
            &[("1", 4), ("2", 5), ("3", 1), ("4", 3), ("5", 2)],
        );
        // The schematic types the control pad an input.
        for pin in u.pins.iter_mut().filter(|p| p.number == "4") {
            pin.kind = "input".into();
        }
        let b = board(nets, vec![u, resistor("R1", value, 3, 2)]);
        pullup_effect_lint(&b, &lib, &HashSet::new()).findings
    }

    /// The receiver's stated input leakage across a large pull-up drops the
    /// line under VIH: 100 uA across 20k is 2 V, 3.3 V -> 1.3 V under a 2.0 V
    /// VIH. The same card with 10k keeps 2.3 V (silent), and a card stating
    /// no leakage is not guessed at.
    #[test]
    fn leakage_across_a_large_pullup_fires_only_on_the_stated_numbers() {
        let card = "vil = 0.8\nvih = 2.0\niih_a = 1e-4";
        let f = leaky_findings(card, "20k");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].message.contains("leakage"), "{}", f[0].message);
        assert!(leaky_findings(card, "10k").is_empty(), "2.3 V is above VIH");
        assert!(
            leaky_findings("vil = 0.8\nvih = 2.0", "20k").is_empty(),
            "no stated leakage: nothing to subtract"
        );
    }

    /// A 100k pull-up onto an input whose card gives no VIH is the one case
    /// the check says it could not judge: a Low abstention. A 10k pull-up
    /// there stays silent, since a normal pull-up needs no verdict.
    #[test]
    fn an_unknown_level_abstains_for_a_weak_pullup_and_is_silent_otherwise() {
        let f = leaky_findings("", "100k");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(f[0].message.contains(ABSTENTION_MARKER), "{}", f[0].message);
        assert!(leaky_findings("", "10k").is_empty());
    }
}
