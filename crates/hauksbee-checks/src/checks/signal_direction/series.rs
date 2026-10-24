//! A series resistor read as a wire, for the checks that judge which way a
//! signal runs.
//!
//! Two pins joined through one small resistor are, for direction, on one line:
//! the resistor carries the level and the pins at both ends see it. A UART
//! wired TX to TX through a pair of 1 kilohm resistors is a crossed pair as
//! surely as the same wire without them, and the resistors only hide it,
//! because net extraction never merges across a two-terminal passive.
//!
//! ## What is a series link
//!
//! A resistor (or one element of a resistor network) whose two ends are on two
//! different signal nets, where
//!
//! - **it is small** ([`SERIES_MAX_OHMS`]). A series termination is 22 to 100
//!   ohms, an ESD or short protection resistor 100 ohms to 1 kilohm, and a
//!   resistor that isolates a UART or a header from the outside world up to
//!   4.7 kilohms. A CMOS input draws well under a microamp, so across any of
//!   these the drop is millivolts and the level arrives whole. Above 4.7
//!   kilohms the part is a bias, a pull or a divider leg in practice (a pull is
//!   10 kilohms or more) and the two sides are not one wire.
//! - **neither end is a rail.** A resistor from a signal to a supply or to
//!   ground is a pull-up, a pull-down or a divider leg, not a hop between two
//!   signals. Supply and ground names are read as the net classification
//!   reads them.
//! - **it is the only hop on both nets.** A net that two resistors reach is a
//!   junction (a summing node, a divider midpoint, a fan-out through separate
//!   terminations), not a series hop, so a net takes part in at most one link
//!   and two resistors in series (`A - R - M - R - B`) are not traced. Any
//!   other resistor pin on the joined line is a pin the caller cannot read,
//!   as it always was, so a divider or a pull on the line still stops it.
//! - **it is not a filter.** A capacitor from either end to ground that puts the
//!   corner of the pair below [`FILTER_CORNER_HZ`] makes the pair an RC filter
//!   (the level still passes at DC, but the line is shaped on purpose and its
//!   two sides are not the one wire a series damping resistor leaves).
//!
//! ## Resistor networks
//!
//! An isolated network package (`RN1`, `R_Pack04`) holds several separate
//! resistors; each is a link of its own. Its elements are found from the pin
//! names where the symbol gives them (`R1.1` with `R1.2`) and otherwise from the
//! package's standard order, pin `i` with pin `n + 1 - i`. That order is only
//! trusted when no pin of the package is on a rail: a bussed network (one pin
//! common to every element, usually on a supply) or a network used partly as
//! pull-ups pairs differently, and is left alone unless its pin names state the
//! elements.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_ground, is_unconnected_net, Component, ExtractedBoard, Pin,
};
use hauksbee_models::netclass::may_be_supply;

/// The largest resistance read as a wire, in ohms. See the module docs.
pub(in crate::checks) const SERIES_MAX_OHMS: f64 = 4_700.0;

/// The lowest corner frequency, in hertz, of a pair a resistor and the capacitance
/// on it leave a wire. Below it the pair is a filter.
pub(in crate::checks) const FILTER_CORNER_HZ: f64 = 1.0e6;

/// One resistor read as a wire between two nets.
#[derive(Debug, Clone)]
pub(in crate::checks) struct Link {
    /// The resistor's reference designator.
    pub reference: String,
    /// The two pad numbers of the element, the nets they are on, and its value.
    pub pads: [String; 2],
    pub nets: [i64; 2],
    pub ohms: f64,
}

impl Link {
    /// The link as a message names it, `R3 (1 kΩ)`.
    pub(in crate::checks) fn describe(&self) -> String {
        format!("{} ({})", self.reference, ohms_text(self.ohms))
    }
}

/// An ohm value as a reader writes it: `22 Ω`, `1 kΩ`, `4.7 kΩ`.
fn ohms_text(ohms: f64) -> String {
    if ohms >= 1000.0 {
        format!("{} kΩ", trim_zeros(ohms / 1000.0))
    } else {
        format!("{} Ω", trim_zeros(ohms))
    }
}

fn trim_zeros(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The series links of a board and the lines they join.
pub(in crate::checks) struct SeriesLinks {
    links: Vec<Link>,
    /// Net id to the index of the link it is an end of.
    by_net: HashMap<i64, usize>,
}

impl SeriesLinks {
    /// Find the links of `board`.
    pub(in crate::checks) fn of(board: &ExtractedBoard) -> Self {
        let signal: BTreeSet<i64> = board
            .nets
            .iter()
            .filter(|n| {
                n.id != 0
                    && !is_unconnected_net(&n.name)
                    && !is_ground(&n.name)
                    && !may_be_supply(&n.name)
            })
            .map(|n| n.id)
            .collect();
        let rails: BTreeSet<i64> = board
            .nets
            .iter()
            .filter(|n| n.id != 0 && !signal.contains(&n.id))
            .map(|n| n.id)
            .collect();
        let ground_caps = ground_capacitance(board);
        let mut candidates: Vec<Link> = Vec::new();
        for c in &board.components {
            if AssemblyState::of(c).fitted().is_none() {
                continue;
            }
            if super::super::straps::is_assembled_resistor(c) {
                if let Some(link) = single(c, &signal) {
                    candidates.push(link);
                }
            } else if is_network(c) {
                candidates.extend(elements(c, &signal, &rails));
            }
        }
        candidates.sort_by(|a, b| (&a.reference, &a.pads).cmp(&(&b.reference, &b.pads)));
        let mut links: Vec<Link> = Vec::new();
        let mut by_net: HashMap<i64, usize> = HashMap::new();
        for link in candidates {
            let [a, b] = link.nets;
            if by_net.contains_key(&a) || by_net.contains_key(&b) {
                continue; // a junction, not a single hop
            }
            let farads = ground_caps.get(&a).copied().unwrap_or(0.0)
                + ground_caps.get(&b).copied().unwrap_or(0.0);
            if farads > 0.0 && corner_hz(link.ohms, farads) < FILTER_CORNER_HZ {
                continue; // a filter, not a wire
            }
            by_net.insert(a, links.len());
            by_net.insert(b, links.len());
            links.push(link);
        }
        Self { links, by_net }
    }

    /// The line `net` is on: the smallest net id of the nets the series link
    /// joins it to, or `net` itself.
    pub(in crate::checks) fn line(&self, net: i64) -> i64 {
        self.by_net
            .get(&net)
            .map_or(net, |&i| self.links[i].nets[0].min(self.links[i].nets[1]))
    }

    /// Every net of the line `line` stands for (see [`Self::line`]).
    pub(in crate::checks) fn nets_of(&self, line: i64) -> Vec<i64> {
        self.by_net.get(&line).map_or_else(
            || vec![line],
            |&i| {
                let mut nets = self.links[i].nets.to_vec();
                nets.sort_unstable();
                nets
            },
        )
    }

    /// The link that joins the line `line` (see [`Self::line`]), when it has one.
    pub(in crate::checks) fn link(&self, line: i64) -> Option<&Link> {
        self.by_net.get(&line).map(|&i| &self.links[i])
    }

    /// Whether `pad` of `component` is an end of a link (a wire here, not a pin).
    pub(in crate::checks) fn is_link_pin(&self, component: &Component, pin: &Pin) -> bool {
        pin.net.and_then(|n| self.by_net.get(&n)).is_some_and(|&i| {
            let l = &self.links[i];
            l.reference == component.reference && l.pads.contains(&pin.number)
        })
    }

    /// The pins on the line `line`, with the links' own resistor pins left out.
    pub(in crate::checks) fn members<'a>(
        &self,
        board: &'a ExtractedBoard,
        line: i64,
    ) -> Vec<(&'a Component, &'a Pin)> {
        self.nets_of(line)
            .into_iter()
            .flat_map(|n| board.net_members(n))
            .filter(|(c, p)| !self.is_link_pin(c, p))
            .collect()
    }

    /// The names of the nets of the line `line`, in net id order.
    pub(in crate::checks) fn names(&self, board: &ExtractedBoard, line: i64) -> Vec<String> {
        self.nets_of(line)
            .into_iter()
            .filter_map(|n| board.net(n).map(|n| n.name.clone()))
            .collect()
    }
}

/// The corner frequency of `ohms` against `farads`.
fn corner_hz(ohms: f64, farads: f64) -> f64 {
    if ohms <= 0.0 {
        return f64::INFINITY;
    }
    1.0 / (2.0 * std::f64::consts::PI * ohms * farads)
}

/// A resistance in ohms from a value field: no unit or an ohm unit, and
/// within [`SERIES_MAX_OHMS`]. A network's value may state its element count
/// first (`4x22`).
fn small_ohms(value: &str) -> Option<f64> {
    let v = value.trim();
    let v = v
        .split_once(['x', 'X', '×'])
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map_or(v, |(_, rest)| rest);
    let parsed = hauksbee_models::value::parse_value(v)?;
    if !matches!(parsed.unit.as_deref(), None | Some("Ω" | "R")) {
        return None;
    }
    (parsed.si.is_finite() && (0.0..=SERIES_MAX_OHMS).contains(&parsed.si)).then_some(parsed.si)
}

/// The two signal nets of a two-pin resistor, when it is a link candidate.
fn single(c: &Component, signal: &BTreeSet<i64>) -> Option<Link> {
    let [a, b] = c.pins.as_slice() else {
        return None;
    };
    link_of(c, a, b, signal)
}

fn link_of(c: &Component, a: &Pin, b: &Pin, signal: &BTreeSet<i64>) -> Option<Link> {
    let na = a.net.filter(|&n| n != 0)?;
    let nb = b.net.filter(|&n| n != 0)?;
    if na == nb || !signal.contains(&na) || !signal.contains(&nb) {
        return None;
    }
    let ohms = small_ohms(&c.value)?;
    Some(Link {
        reference: c.reference.clone(),
        pads: [a.number.clone(), b.number.clone()],
        nets: [na, nb],
        ohms,
    })
}

/// A resistor network package: several resistors in one part.
fn is_network(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let fp = c.footprint.to_ascii_lowercase();
    let by_name = ["RN", "RP", "RM", "RA", "RZ"]
        .iter()
        .any(|p| r.starts_with(p));
    let by_package = r.starts_with('R')
        && ["r_array", "cay16", "cat16", "exb"]
            .iter()
            .any(|k| fp.contains(k));
    let bussed = [&c.lib_id, &c.value, &c.footprint].iter().any(|s| {
        let s = s.to_ascii_lowercase();
        s.contains("network") || s.contains("bussed") || s.contains("common")
    });
    (by_name || by_package) && !bussed && c.pins.len() >= 4 && c.pins.len().is_multiple_of(2)
}

/// The element named by a pin name `R2.1` (`R2`), when it has that form.
fn element_name(function: &str) -> Option<&str> {
    let (element, pad) = function.trim().split_once('.')?;
    (!element.is_empty() && pad.chars().all(|c| c.is_ascii_digit()) && !pad.is_empty())
        .then_some(element)
}

/// The links of the elements of a network package.
fn elements(c: &Component, signal: &BTreeSet<i64>, rails: &BTreeSet<i64>) -> Vec<Link> {
    let named: Vec<Option<&str>> = c.pins.iter().map(|p| element_name(&p.function)).collect();
    if named.iter().all(Option::is_some) {
        let mut by_element: BTreeMap<&str, Vec<&Pin>> = BTreeMap::new();
        for (p, e) in c.pins.iter().zip(&named) {
            if let Some(e) = e {
                by_element.entry(e).or_default().push(p);
            }
        }
        return by_element
            .values()
            .filter_map(|pins| match pins.as_slice() {
                [a, b] => link_of(c, a, b, signal),
                _ => None,
            })
            .collect();
    }
    // The standard order: only where no pin is on a rail (see the module docs).
    if c.pins
        .iter()
        .any(|p| p.net.is_some_and(|n| rails.contains(&n)))
    {
        return Vec::new();
    }
    let mut pins: Vec<&Pin> = c.pins.iter().collect();
    if pins
        .iter()
        .all(|p| p.number.chars().all(|ch| ch.is_ascii_digit()))
    {
        pins.sort_by_key(|p| p.number.parse::<u32>().unwrap_or(0));
    }
    let n = pins.len();
    (0..n / 2)
        .filter_map(|i| link_of(c, pins[i], pins[n - 1 - i], signal))
        .collect()
}

/// The capacitance each net has to ground: fitted capacitors with one pad
/// on the net and the other on a ground net.
fn ground_capacitance(board: &ExtractedBoard) -> HashMap<i64, f64> {
    let ground: BTreeSet<i64> = board
        .nets
        .iter()
        .filter(|n| is_ground(&n.name))
        .map(|n| n.id)
        .collect();
    let mut out: HashMap<i64, f64> = HashMap::new();
    for c in &board.components {
        if !is_capacitor(c) || AssemblyState::of(c).fitted().is_none() {
            continue;
        }
        let [a, b] = c.pins.as_slice() else { continue };
        let (Some(na), Some(nb)) = (a.net, b.net) else {
            continue;
        };
        let Some(farads) = hauksbee_models::value::parse_value(&c.value)
            .filter(|p| matches!(p.unit.as_deref(), None | Some("F")))
            .map(|p| p.si)
            .filter(|f| f.is_finite() && *f > 0.0)
        else {
            continue;
        };
        if ground.contains(&nb) && !ground.contains(&na) {
            *out.entry(na).or_insert(0.0) += farads;
        } else if ground.contains(&na) && !ground.contains(&nb) {
            *out.entry(nb).or_insert(0.0) += farads;
        }
    }
    out
}
