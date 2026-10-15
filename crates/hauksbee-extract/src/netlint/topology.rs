//! Net-topology lint: faults visible in how nets join, not in any one pin.
//!
//! - **Ground island**: a net named as a ground or return that nothing joins to
//!   the board's main ground. Copper, a resistor of any value, a ferrite, an
//!   inductor, a fuse, a net tie, a diode or any multi-pin part with pins on
//!   both counts as a join, so split grounds tied at one point, isolated domains
//!   bridged by an isolator or isolated converter, and kelvin sense nets stay
//!   quiet. A capacitor does not join (it passes no DC). Names that declare a
//!   separate domain (isolated, chassis, earth, shield) are left alone.
//! - **Unwired connector**: a connector whose every electrical pin sits on a net
//!   that touches only that pin, with at least one of those nets explicitly
//!   drawn (a label or wire, not a no-connect). Nothing on the board reaches it.
//! - **Reversed LED**: a two-pin LED whose anode is held at ground and whose
//!   cathode at a positive rail, each directly or through one resistor. It is
//!   reverse-biased in every state and never lights.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{
    is_ground, is_led, is_test_point_or_hole, is_unconnected_net, members, norm, parse_ohms,
    pin_is_no_connect, rail_voltage, LintCheck, LintFinding, NetLintReport, Severity,
};
use crate::assembly::AssemblyState;
use crate::dsu::Dsu;
use crate::part_class::{classify_two_terminal, ref_designator, PartClass};
use crate::{Component, ExtractedBoard};
use hauksbee_models::schema::PassiveClass;

fn tokens(name: &str) -> impl Iterator<Item = String> {
    norm(name)
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
}

/// A net named as a ground or return: a `GND`/`GROUND`/`RTN`/`RETURN` token, or
/// a token that starts or ends with `GND` (`AGND`, `GNDA`, `MICGND`). `VSS`,
/// `VEE` and `0` are not: analog boards use `VSS` for a negative supply.
fn ground_named(name: &str) -> bool {
    tokens(name).any(|t| {
        matches!(t.as_str(), "GROUND" | "RTN" | "RETURN")
            || t.starts_with("GND")
            || t.ends_with("GND")
    })
}

/// A ground-family word with an optional single-letter domain prefix and
/// number: `GND`, `AGND`, `GNDA`, `GND2`, `VSS`, `VSSA`, `GNDPWR`.
fn ground_word(word: &str) -> bool {
    let w = word.trim_end_matches(|c: char| c.is_ascii_digit());
    ["GND", "VSS"].iter().any(|g| {
        let rest = w
            .strip_prefix(g)
            .or_else(|| w.strip_suffix(g))
            .unwrap_or("x");
        rest.is_empty() || matches!(rest, "A" | "D" | "P" | "S" | "IO" | "PWR" | "DIG" | "ANA")
    })
}

/// Evidence that a net is a ground beyond a substring of its name: the whole
/// name is a ground word (optionally with one short qualifier token), or a pin
/// on it is named as a ground pin by its part. A signal pair whose name merely
/// contains `GND` (`SNS_GND+`, `ISENSE_GNDSENSE`) has neither.
fn ground_evidence(board: &ExtractedBoard, net: &crate::Net) -> bool {
    let words: Vec<String> = tokens(&net.name).collect();
    let by_name = match words.as_slice() {
        [w] => ground_word(w) || w == "GROUND",
        [a, b] => (ground_word(a) && b.len() <= 2) || (ground_word(b) && a.len() <= 2),
        _ => false,
    };
    by_name
        || members(board, net.id).into_iter().any(|(c, p)| {
            !is_test_point_or_hole(c) && ground_word(&p.function.trim().to_ascii_uppercase())
        })
}

/// A name that declares a deliberately separate return domain, or a signal that
/// merely mentions ground (a kelvin sense or detect line).
fn separate_domain(name: &str) -> bool {
    tokens(name).any(|t| {
        [
            "ISO", "CHASSIS", "EARTH", "SHIELD", "SHLD", "FRAME", "SCREEN", "CASE",
        ]
        .iter()
        .any(|k| t.contains(k))
            || matches!(
                t.as_str(),
                "PE" | "FG"
                    | "CGND"
                    | "FGND"
                    | "EGND"
                    | "PEGND"
                    | "SENSE"
                    | "SNS"
                    | "KELVIN"
                    | "DET"
                    | "DETECT"
                    | "FB"
                    | "EN"
                    | "SEL"
                    | "ID"
            )
    })
}

/// Distinct net ids a component's pins sit on, in pin order.
fn nets_of(c: &Component) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for id in c.pins.iter().filter_map(|p| p.net) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Whether a part carries DC between the nets it touches, for the ground-join
/// question. Capacitors, crystals, LEDs and connectors/switches do not (a
/// connector's mating cable is off the board). The strict connector test is
/// used, not `is_connector_like`, which also admits BGA footprints whose names
/// carry a ball grid (`Layout11x11`): an IC with pins on two grounds joins them.
fn joins_ground(c: &Component) -> Option<bool> {
    if is_connector(c) || ref_designator(&c.reference).starts_with("SW") || is_led(c) {
        return None;
    }
    Some(match classify_two_terminal(c) {
        PartClass::NotTwoTerminal => false,
        PartClass::Passive(PassiveClass::Capacitor | PassiveClass::Crystal) => return None,
        _ => true,
    })
}

pub(super) fn check_ground_islands(board: &ExtractedBoard, report: &mut NetLintReport) {
    let index: HashMap<i64, usize> = board
        .nets
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id, i))
        .collect();
    let grounds: Vec<bool> = board
        .nets
        .iter()
        .map(|n| ground_named(&n.name) && !is_unconnected_net(&n.name))
        .collect();
    let mut dsu = Dsu::new(board.nets.len());
    for c in &board.components {
        let Some(two_terminal) = joins_ground(c) else {
            continue;
        };
        let ids: Vec<usize> = nets_of(c)
            .iter()
            .filter_map(|id| index.get(id).copied())
            .collect();
        // A two-terminal conductor joins whatever it bridges; a multi-pin part
        // joins only the ground-named nets it touches (an isolator, a module
        // with two ground pins, an isolated converter).
        let joined: Vec<usize> = if two_terminal && ids.len() == 2 {
            ids
        } else {
            ids.into_iter().filter(|&i| grounds[i]).collect()
        };
        for pair in joined.windows(2) {
            dsu.union(pair[0], pair[1]);
        }
    }
    let loads = |net_id: i64| -> BTreeSet<String> {
        members(board, net_id)
            .into_iter()
            .filter(|(c, _)| AssemblyState::of(c).is_present() && !is_test_point_or_hole(c))
            .map(|(c, _)| c.reference.clone())
            .collect()
    };
    // The board ground: a conventionally named ground (`GND`, `AGND`, `GND*`)
    // ahead of any other return-named net, then the most-connected.
    let Some(main) = (0..board.nets.len())
        .filter(|&i| grounds[i])
        .max_by_key(|&i| {
            let net = &board.nets[i];
            (
                is_ground(&net.name),
                members(board, net.id).len(),
                std::cmp::Reverse(i),
            )
        })
    else {
        return;
    };
    let main_root = dsu.find(main);
    let circuits = Circuits::of(board, &index, &grounds);
    let main_circuits: BTreeSet<usize> = (0..board.nets.len())
        .filter(|&i| grounds[i] && dsu.find(i) == main_root)
        .flat_map(|i| circuits.on_net(board.nets[i].id))
        .collect();
    let mut islands: BTreeMap<usize, (Vec<String>, BTreeSet<String>)> = BTreeMap::new();
    for (i, net) in board.nets.iter().enumerate() {
        if !grounds[i] || separate_domain(&net.name) || dsu.find(i) == main_root {
            continue;
        }
        // A ground whose parts share no signal or supply net with the main
        // ground's parts is another circuit's own ground (the second half of a
        // split keyboard drawn in one file, a board joined only by a cable):
        // no return current on this board crosses between them.
        if circuits
            .on_net(net.id)
            .all(|circuit| !main_circuits.contains(&circuit))
        {
            continue;
        }
        let refs = loads(net.id);
        if refs.len() < 2 {
            continue;
        }
        let entry = islands.entry(dsu.find(i)).or_default();
        entry.0.push(net.name.clone());
        entry.1.extend(refs);
    }
    let main_name = &board.nets[main].name;
    for (nets, refs) in islands.into_values() {
        report
            .findings
            .push(island_finding(board, main_name, nets, refs));
    }
}

/// Fitted parts grouped into circuits: two parts are one circuit when a
/// non-ground net joins them, directly or through other parts.
struct Circuits {
    /// Part index -> its circuit.
    circuit: Vec<usize>,
    /// Net id -> indices of the fitted parts with a pin on it.
    parts_on: HashMap<i64, Vec<usize>>,
}

impl Circuits {
    fn of(board: &ExtractedBoard, index: &HashMap<i64, usize>, grounds: &[bool]) -> Self {
        let parts = board.components.len();
        let mut dsu = Dsu::new(parts + board.nets.len());
        let mut parts_on: HashMap<i64, Vec<usize>> = HashMap::new();
        for (ci, c) in board.components.iter().enumerate() {
            if !AssemblyState::of(c).is_present() {
                continue;
            }
            for id in nets_of(c) {
                parts_on.entry(id).or_default().push(ci);
                let Some(&ni) = index.get(&id) else { continue };
                if id != 0 && !grounds[ni] && !is_unconnected_net(&board.nets[ni].name) {
                    dsu.union(ci, parts + ni);
                }
            }
        }
        let circuit = (0..parts).map(|ci| dsu.find(ci)).collect();
        Self { circuit, parts_on }
    }

    /// The circuits of the parts on `net_id`.
    fn on_net(&self, net_id: i64) -> impl Iterator<Item = usize> + '_ {
        self.parts_on
            .get(&net_id)
            .into_iter()
            .flatten()
            .map(|&ci| self.circuit[ci])
    }
}

/// The finding for one island of ground-named nets never joined to the main ground.
fn island_finding(
    board: &ExtractedBoard,
    main_name: &str,
    nets: Vec<String>,
    refs: BTreeSet<String>,
) -> LintFinding {
    let refs: Vec<String> = refs.into_iter().collect();
    let listed = nets
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // A name alone proves nothing: a signal pair named for its return
    // (`SNS_GND+`) is no ground. Only a ground word as the whole name or a
    // ground-named pin makes this a fault; otherwise it is a question.
    let proven = nets.iter().any(|n| {
        board
            .nets
            .iter()
            .any(|b| b.name == *n && ground_evidence(board, b))
    });
    let (severity, message) = if proven {
        (
                Severity::High,
                format!(
                    "ground net {listed} ({}) is never joined to the board ground '{main_name}': no copper, \
                     resistor, ferrite, net tie or part ties them, so its return current has no path \
                     on the board",
                    refs.join(", "),
                ),
            )
    } else {
        (
                Severity::Low,
                format!(
                    "{listed} ({}) has ground in its name but no ground pin, and nothing joins it to '{main_name}'. \
                     If it is a ground, join it; if it is a signal or sense pair, ignore this.",
                    refs.join(", "),
                ),
            )
    };
    LintFinding {
        check: LintCheck::GroundIsland,
        severity,
        message,
        refs,
        nets,
    }
}

/// A part that is a connector proper: not a test point, mounting hole, switch
/// or jumper, which `is_connector_like` also admits.
fn is_connector(c: &Component) -> bool {
    let r = ref_designator(&c.reference);
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    if is_test_point_or_hole(c) || r.starts_with("JP") || r.starts_with("SW") {
        return false;
    }
    let numbered = |prefix: &str| {
        r.strip_prefix(prefix)
            .is_some_and(|rest| rest.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
    };
    numbered("J")
        || numbered("CN")
        || numbered("CON")
        || numbered("P") && r.len() <= 4
        || [
            "connector",
            "terminalblock",
            "terminal_block",
            "screw_terminal",
        ]
        .iter()
        .any(|k| lib.contains(k) || fp.contains(k))
}

/// A pad that carries no signal by construction: an unnumbered or mechanical
/// (`MP`, `SH`) pad, or one whose pin name marks a shield, mount or NC.
fn is_mechanical_pin(number: &str, function: &str) -> bool {
    let n = number.to_ascii_uppercase();
    let f = function.to_ascii_uppercase();
    n.is_empty()
        || !n.chars().any(|ch| ch.is_ascii_digit())
        || n.starts_with("MP")
        || n.starts_with("SH")
        || ["SHIELD", "SHLD", "MOUNT", "NC"].iter().any(|k| {
            f.split(|ch: char| !ch.is_ascii_alphanumeric())
                .any(|t| t == *k)
        })
}

pub(super) fn check_unwired_connectors(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present() && is_connector(c))
    {
        let mut pads: BTreeMap<&str, Option<i64>> = BTreeMap::new();
        for p in c
            .pins
            .iter()
            .filter(|p| !pin_is_no_connect(p) && !is_mechanical_pin(&p.number, &p.function))
        {
            let slot = pads.entry(p.number.as_str()).or_insert(None);
            *slot = slot.or(p.net);
        }
        if pads.len() < 2 {
            continue;
        }
        let mut drawn = Vec::new();
        let all_dead = pads.values().all(|net| {
            let Some(net) = net.and_then(|id| board.net(id)) else {
                return true;
            };
            if is_unconnected_net(&net.name) {
                return true;
            }
            let lone = members(board, net.id).len() == 1;
            if lone {
                drawn.push(net.name.clone());
            }
            lone
        });
        if !all_dead || drawn.is_empty() {
            continue;
        }
        report.findings.push(LintFinding {
            check: LintCheck::UnwiredConnector,
            severity: Severity::Medium,
            message: format!(
                "connector {} is wired to nothing: each of its {} signal pins sits on a net that \
                 touches only that pin ({}), so whatever plugs into it is not connected to the board",
                c.reference,
                pads.len(),
                drawn.join(", "),
            ),
            refs: vec![c.reference.clone()],
            nets: drawn,
        });
    }
}

/// The far end of a net seen from an LED terminal: the net itself when it is a
/// ground or rail, else the far net of one fitted resistor on it.
fn level_through_resistor(board: &ExtractedBoard, net: i64) -> Option<(bool, String)> {
    let classify = |id: i64| {
        let n = board.net(id)?;
        if is_ground(&n.name) {
            Some((false, n.name.clone()))
        } else {
            rail_voltage(&n.name)
                .filter(|v| *v > 0.0)
                .map(|_| (true, n.name.clone()))
        }
    };
    if let Some(level) = classify(net) {
        return Some(level);
    }
    let mut found = None;
    for (c, _) in members(board, net) {
        if !AssemblyState::of(c).is_present()
            || !classify_two_terminal(c).is_resistor()
            || parse_ohms(&c.value).is_none()
        {
            continue;
        }
        let far = nets_of(c).into_iter().find(|id| *id != net)?;
        match (found.as_ref(), classify(far)) {
            (_, None) => return None,
            (None, Some(level)) => found = Some(level),
            (Some(prev), Some(level)) if prev.0 != level.0 => return None,
            _ => {}
        }
    }
    found
}

pub(super) fn check_reversed_leds(board: &ExtractedBoard, report: &mut NetLintReport) {
    for led in board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present() && is_led(c))
    {
        if nets_of(led).len() != 2 {
            continue;
        }
        let role = |names: &[&str]| {
            led.pins
                .iter()
                .find(|p| {
                    names
                        .iter()
                        .any(|n| p.function.trim().eq_ignore_ascii_case(n))
                })
                .and_then(|p| p.net)
        };
        let (Some(anode), Some(cathode)) =
            (role(&["A", "ANODE", "+"]), role(&["K", "CATHODE", "-"]))
        else {
            continue;
        };
        if anode == cathode {
            continue;
        }
        let (Some((false, gnd)), Some((true, rail))) = (
            level_through_resistor(board, anode),
            level_through_resistor(board, cathode),
        ) else {
            continue;
        };
        report.findings.push(LintFinding {
            check: LintCheck::LedReversed,
            severity: Severity::Medium,
            message: format!(
                "{} is reverse-biased: its anode is held at ground ('{gnd}') and its cathode at the \
                 positive rail '{rail}', so it never lights",
                led.reference
            ),
            refs: vec![led.reference.clone()],
            nets: vec![gnd, rail],
        });
    }
}

#[cfg(test)]
mod tests;
