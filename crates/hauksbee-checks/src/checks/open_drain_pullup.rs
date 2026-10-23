//! An open-drain output with nothing to pull its line high.
//!
//! An open-drain or open-collector output (an alert, interrupt, fault or
//! power-good flag, an SPI data-out that is open-drain) can only pull its net
//! low. The high level is a resistor to a rail. The I2C and SD pull-up checks
//! cover those buses by net name; this one covers every other open-drain pad a
//! card lists under `[models.pin_capability.open_drain]`, whatever the net is
//! called.
//!
//! A net has a pull-up when
//!
//! - a resistor reaches a positive rail from it, directly or through at most
//!   two more resistors, LEDs or diodes (an LED fed from the rail through its
//!   resistor is the usual way a status output is "pulled up"), or
//! - a part on it is a microcontroller whose card lists the receiving pad under
//!   `[models.pin_capability.pull_up]`: firmware can enable its internal
//!   pull-up there. The card says the pad has one; whether firmware enables it
//!   is not on the board, so it is taken as the intended arrangement.
//!
//! It abstains where it cannot know: a connector on the net (the pull-up may
//! be off-board), and a net no other chip reads (an unused flag output).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hauksbee_extract::{
    is_capacitor, is_connector_like, is_led, is_resistor, is_unconnected_net, Component,
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_models::{ComponentKind, ModelLibrary};

use super::node_drive::Index;

/// Series parts a pull-up path may pass through.
const MAX_HOPS: usize = 3;

/// A resistor array (`RN1`, `RP2`): its pads are separate elements.
fn is_array(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    ["RN", "RP", "RM", "RA", "RZ"]
        .iter()
        .any(|p| r.starts_with(p))
        && c.pins
            .iter()
            .filter(|p| p.net.is_some_and(|n| n != 0))
            .count()
            >= 3
}

/// A part a pull-up path may run through: a resistor, an LED or a diode.
fn is_series_part(idx: &Index<'_>, c: &Component) -> bool {
    is_resistor(c)
        || is_array(c)
        || is_led(c)
        || idx.model(c).is_some_and(|m| m.kind == ComponentKind::Diode)
}

/// Whether `net` is held at a positive supply level.
fn is_positive_rail(idx: &Index<'_>, net: i64) -> bool {
    if idx.level(net).is_some_and(|v| v > 0.0) {
        return true;
    }
    idx.members(net).iter().any(|(c, p)| {
        p.kind.trim().eq_ignore_ascii_case("power_out")
            || idx.model(c).is_some_and(|m| {
                m.kind == ComponentKind::Vreg
                    && idx
                        .role(c, p)
                        .is_some_and(|r| matches!(r, "out" | "vout" | "sw" | "lx"))
            })
    })
}

/// A pin the board's own symbol marks as an input.
fn is_input_pin(p: &Pin) -> bool {
    p.kind.trim().eq_ignore_ascii_case("input")
}

/// What the nets reachable from an open-drain net through series parts hold.
struct Reach {
    pulled: bool,
    external: bool,
    receivers: bool,
}

fn reach(idx: &Index<'_>, start: i64, od: &HashSet<(String, String)>) -> Reach {
    let mut out = Reach {
        pulled: false,
        external: false,
        receivers: false,
    };
    let mut seen = BTreeSet::from([start]);
    let mut frontier = vec![start];
    for _ in 0..MAX_HOPS {
        let mut next = Vec::new();
        for net in frontier {
            for (c, p) in idx.members(net) {
                if od.contains(&(c.reference.clone(), p.number.clone())) {
                    continue;
                }
                if is_connector_like(c) {
                    out.external = true;
                } else if idx.model(c).is_some_and(|m| {
                    m.kind == ComponentKind::Mcu && m.has_internal_pull_up(&p.number)
                }) {
                    out.pulled = true;
                    out.receivers = true;
                } else if idx.model(c).is_some_and(|m| {
                    m.kind == ComponentKind::Mcu && m.pin_capability.pull_up.is_none()
                }) {
                    // An MCU whose card states no pull-up pads: firmware may
                    // well enable one, and the card cannot say, so abstain.
                    out.external = true;
                } else if is_series_part(idx, c) {
                    for q in &c.pins {
                        let Some(far) = q.net.filter(|&n| n != 0 && n != net) else {
                            continue;
                        };
                        if std::ptr::eq(q, *p) {
                            continue;
                        }
                        if is_positive_rail(idx, far) {
                            out.pulled = true;
                        } else if idx.level(far) != Some(0.0) && seen.insert(far) {
                            next.push(far);
                        }
                    }
                } else if !is_capacitor(c) && (idx.model(c).is_some() || is_input_pin(p)) {
                    // A part with a card, or a pin the board calls an input.
                    // A part nothing describes (a module, an unmodelled chip
                    // with bidirectional pins) may carry its own pull-up.
                    out.receivers = true;
                }
            }
        }
        if out.pulled {
            break;
        }
        frontier = next;
    }
    out
}

/// Open-drain outputs, from the cards, on a net with no way to go high.
#[must_use]
pub fn open_drain_pullup_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    // net -> the open-drain pads on it, as (reference, pad number, role)
    let mut by_net: BTreeMap<i64, Vec<(&Component, &Pin, String)>> = BTreeMap::new();
    for c in &board.components {
        let Some(model) = idx.model(c) else { continue };
        for p in &c.pins {
            let Some(net) = p.net.filter(|&n| n != 0) else {
                continue;
            };
            if model.is_open_drain_pad(&p.number) {
                let role = model.pins.get(&p.number).cloned().unwrap_or_default();
                by_net.entry(net).or_default().push((c, p, role));
            }
        }
    }
    let mut report = NetLintReport::default();
    for (net, pads) in by_net {
        let name = idx.name(net);
        if is_unconnected_net(name) || idx.level(net).is_some() {
            continue;
        }
        let od: HashSet<(String, String)> = pads
            .iter()
            .map(|(c, p, _)| (c.reference.clone(), p.number.clone()))
            .collect();
        let found = reach(&idx, net, &od);
        if found.pulled || found.external || !found.receivers {
            continue;
        }
        let who: Vec<String> = pads
            .iter()
            .map(|(c, p, role)| {
                if role.is_empty() {
                    format!("{} pin {}", c.reference, p.number)
                } else {
                    format!("{} pin {} ({role})", c.reference, p.number)
                }
            })
            .collect();
        let refs: BTreeSet<String> = pads.iter().map(|(c, _, _)| c.reference.clone()).collect();
        report.findings.push(LintFinding {
            check: LintCheck::MissingOpenDrainPullup,
            severity: Severity::Medium,
            message: format!(
                "net '{name}' is driven by the open-drain output {} and nothing pulls it high: no \
                 resistor to a power rail, and no receiving microcontroller pin whose card lists an \
                 internal pull-up. An open-drain output only pulls the line low, so the line floats \
                 whenever the part releases it and the receiver reads an undefined level. {REMEDY_MARKER}\
                 Add a pull-up resistor (10 kilohms is typical) from '{name}' to the rail the \
                 receiver reads, or enable an internal pull-up on the receiving pin if its \
                 datasheet gives one.",
                who.join(", "),
            ),
            refs: refs.into_iter().collect(),
            nets: vec![name.to_string()],
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};

    const GND: i64 = 1;
    const V33: i64 = 2;
    const ALERT: i64 = 10;
    const MID: i64 = 11;

    /// An INA228 with ALERT (pad 3 here per the card) on `alert`.
    fn sensor(alert: i64) -> Component {
        part(
            "U1",
            "INA228AIDGSR",
            &[("3", alert), ("6", V33), ("7", GND)],
        )
    }

    fn resistor(reference: &str, a: i64, b: i64) -> Component {
        let mut c = part(reference, "10k", &[("1", a), ("2", b)]);
        c.lib_id = "Device:R".into();
        c
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        let nets = [
            (GND, "GND"),
            (V33, "+3V3"),
            (ALERT, "ALERT_N"),
            (MID, "MID"),
        ];
        open_drain_pullup_lint(&board(&nets, parts), &ModelLibrary::builtin()).findings
    }

    /// An `ATmega` whose pin 2 reads `net`.
    fn mcu(net: i64) -> Component {
        part("U2", "ATmega328P", &[("2", net), ("7", V33), ("8", GND)])
    }

    /// A receiver the library knows nothing about, on a pin the board calls an
    /// input: no stated pull.
    fn receiver(net: i64) -> Component {
        let mut c = part("U3", "XYZ-LOGIC-9", &[("1", net)]);
        c.pins[0].kind = "input".into();
        c
    }

    /// A module nothing describes, on a bidirectional pin: it may carry its own pull-up.
    fn module(net: i64) -> Component {
        let mut c = part("U3", "XYZ-MODULE-9", &[("1", net)]);
        c.pins[0].kind = "bidirectional".into();
        c
    }

    #[test]
    fn a_receiving_mcu_pin_with_a_stated_internal_pull_up_is_silent() {
        let found = run(vec![sensor(ALERT), mcu(ALERT)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn an_open_drain_flag_with_no_pull_up_is_flagged() {
        let found = run(vec![sensor(ALERT), receiver(ALERT)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].check, LintCheck::MissingOpenDrainPullup);
        assert_eq!(found[0].refs, ["U1"]);
        assert_eq!(found[0].nets, ["ALERT_N"]);
    }

    #[test]
    fn an_undescribed_module_on_the_net_is_not_judged() {
        let found = run(vec![sensor(ALERT), module(ALERT)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_pull_up_resistor_or_a_status_led_to_the_rail_is_silent() {
        let direct = run(vec![
            sensor(ALERT),
            receiver(ALERT),
            resistor("R1", ALERT, V33),
        ]);
        assert!(direct.is_empty(), "{direct:#?}");
        // The line pulled through a series resistor still reaches the rail.
        let series = run(vec![
            sensor(ALERT),
            receiver(ALERT),
            resistor("R1", ALERT, MID),
            resistor("R2", MID, V33),
        ]);
        assert!(series.is_empty(), "{series:#?}");
        // A pull-down alone is not a pull-up.
        let down = run(vec![
            sensor(ALERT),
            receiver(ALERT),
            resistor("R1", ALERT, GND),
        ]);
        assert_eq!(down.len(), 1, "{down:#?}");
    }

    #[test]
    fn nets_nothing_reads_or_a_connector_can_pull_are_not_judged() {
        let unused = run(vec![sensor(ALERT)]);
        assert!(unused.is_empty(), "{unused:#?}");
        let mut header = part("J1", "Conn_01x01", &[("1", ALERT)]);
        header.lib_id = "Connector:Conn_01x01".into();
        let off_board = run(vec![sensor(ALERT), header]);
        assert!(off_board.is_empty(), "{off_board:#?}");
    }
}
