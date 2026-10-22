//! What each rail's power rests on, when nothing the analysis models sources it.
//!
//! A static check with no supply declarations powers a rail by its name: a net
//! called `+3V3` that no modelled regulator drives takes an ideal 3.3 V source
//! (`binder` pass 3b). That is right for a rail a connector brings in, and it
//! is a fiction for a rail whose only source is a part the board does not fit:
//! the regulator marked do-not-populate, or the ferrite or 0 Ω link that would
//! join it to a powered rail. Judged at the fiction, every load on that rail
//! passed while on the real board the rail never comes up.
//!
//! This module classifies each such rail by the board's own evidence:
//! * an off-board entry: a connector, a test point or a battery on the rail
//!   (the rail is assumed supplied from there, a stated condition);
//! * unfitted sources: do-not-populate parts that would source the rail (a
//!   card's supply output or regulator output, a `power_out` pin, a part with
//!   IN/OUT pins whose OUT is on it) or link it to a powered rail (a series
//!   resistor of at most [`SERIES_OHMS`], an inductor, a ferrite);
//! * fitted parts that may source it although the analysis models none (a part
//!   with no model and a pin that can drive, a pass element or series part from
//!   another rail, a card's supply-output pin).
//!
//! A rail with unfitted sources, no entry, no possible fitted source and
//! fitted loads never comes up ([`RailBasis::never_up`]). The same test runs
//! over nets the analysis leaves unpowered (a rail whose name states no
//! voltage), so `AVDD` cut off by an unfitted ferrite is found too.

use std::collections::{BTreeSet, HashMap, HashSet};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_connector_like, is_led, is_plain_inductor, is_plain_resistor,
    is_test_point_or_hole, Component, ExtractedBoard,
};
use hauksbee_ir::NodeId;
use hauksbee_models::refdes::designator;
use hauksbee_models::schema::{ComponentKind, PassiveClass};
use hauksbee_models::ModelLibrary;

use crate::bind_report::pin_can_drive;
use crate::binder::{is_ground, library_resolution, power_rail_voltage, BoundBoard};
use crate::rail_owner::{RailDomains, SERIES_OHMS};

/// A do-not-populate part a rail would take its power from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfitted {
    /// Its reference designator.
    pub reference: String,
    /// Its value, as the board states it.
    pub value: String,
    /// For a series link: the powered net it would join the rail to. `None`
    /// for a part that would source the rail itself.
    pub link_to: Option<String>,
}

/// One rail (a group of nets series conductors join) and what powers it.
#[derive(Debug, Clone, PartialEq)]
pub struct RailBasis {
    /// The rail's nets, the net the analysis powers first.
    pub nets: Vec<String>,
    /// The voltage the analysis holds it at by its name, `None` when the
    /// analysis leaves it unpowered.
    pub volts: Option<f64>,
    /// Connectors, test points and batteries on it: where it can come in from
    /// off the board.
    pub entries: Vec<String>,
    /// Do-not-populate parts that would source it or link it to a powered rail.
    pub unfitted: Vec<Unfitted>,
    /// Fitted parts that may source it although the analysis models none.
    pub feeds: Vec<String>,
    /// Fitted parts that draw from it (anything but capacitors, resistors and
    /// inductors).
    pub loads: Vec<String>,
    /// The loads whose pin on the rail is a supply input (a `power_in` pin or
    /// one named `VDD`, `VCC`, ... in the schematic or the part's card), not a
    /// signal such as a reference or bias input, and not the pin of an internal
    /// regulator's output (`VDDCORE`, `VCAP`).
    pub supply_loads: Vec<String>,
    /// The supply loads that are active ICs by what their record says (see
    /// [`hauksbee_extract::is_active_ic`]).
    pub active_loads: Vec<String>,
    /// Power comes into the board somewhere: an off-board entry sits on a net
    /// that is powered or named for a voltage. A fragment with no way in at all
    /// (a sub-circuit linted on its own) says nothing about where its supply
    /// comes from.
    pub board_powered_in: bool,
    /// Fitted series parts (a fuse, diode, switch, ferrite, link) join the rail,
    /// within a few hops, to a net an off-board entry or a modelled source is
    /// on. Only filled for a rail that has no entry, feed or unfitted source of
    /// its own.
    pub fed_through: bool,
}

impl RailBasis {
    /// The analysis holds the rail at its name's voltage though nothing on the
    /// board sources it, on a board that takes power in elsewhere: no entry, no fitted part that may drive it, no
    /// unfitted source (that is [`Self::never_up`]'s case), no series part
    /// bringing it in from a net that has one, and fitted parts draw from it.
    #[must_use]
    pub const fn unsourced(&self) -> bool {
        self.volts.is_some()
            && self.board_powered_in
            && self.unfitted.is_empty()
            && self.entries.is_empty()
            && self.feeds.is_empty()
            && !self.fed_through
            && !self.supply_loads.is_empty()
    }

    /// The rail never comes up on the real board: only unfitted parts would
    /// power it, nothing fitted may, nothing brings it in from off the board,
    /// and fitted parts draw from it.
    #[must_use]
    pub const fn never_up(&self) -> bool {
        !self.unfitted.is_empty()
            && self.entries.is_empty()
            && self.feeds.is_empty()
            && !self.loads.is_empty()
    }

    /// The rail's first (powered) net.
    #[must_use]
    pub fn net(&self) -> &str {
        self.nets.first().map_or("", String::as_str)
    }
}

/// Every rail of a bound board whose power the analysis assumed or lacks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RailBases {
    /// One per rail the analysis powers by its name alone (an ideal source no
    /// modelled part backs), then one per unpowered rail an unfitted part
    /// would have powered.
    pub rails: Vec<RailBasis>,
    /// Whether any net a fitted part (other than a connector, test point or
    /// battery) is on is powered in the analysis.
    pub any_powered: bool,
    /// Fitted parts with three or more connected pins that are not connectors,
    /// test points or holes: the parts whose levels a powered rail lets the
    /// checks judge.
    pub active_parts: Vec<String>,
}

/// Output-pin roles a regulator or power-path card names.
fn output_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    let r = r.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    matches!(
        r,
        "out" | "vout" | "vo" | "output" | "sys" | "vsys" | "bat" | "vbat" | "vbatt" | "ldo"
    )
}

/// A pin function an IC draws power on.
fn supply_input_name(function: &str) -> bool {
    let f = function
        .trim()
        .to_ascii_uppercase()
        .replace(['{', '}', '~', '!', '/'], "");
    let f = f.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    let f = f.trim_start_matches(['A', 'D', 'P']);
    [
        "VDD", "VCC", "VIN", "VBUS", "VBAT", "VIO", "IOVDD", "VS", "V+", "VREF", "VSUP", "VM",
    ]
    .iter()
    .any(|p| f.starts_with(p))
}

/// A pin that carries an internal regulator's output out for decoupling
/// (`VDDCORE`, `VCORE`, `VCAP`, `VDDREG`), named like a supply but sourced by the
/// chip itself, or a reference input (`VREF`), which is a level to compare
/// against and not a supply.
fn regulator_output_name(function: &str) -> bool {
    let f = function.trim().to_ascii_uppercase();
    f.starts_with("VREF")
        || ["CORE", "VCAP", "VREG", "DECOUP"]
            .iter()
            .any(|k| f.contains(k))
}

/// The pin function base (`VOUT_2` -> `VOUT`).
fn function_base(function: &str) -> String {
    let f = function.trim().to_ascii_uppercase();
    match f.rsplit_once('_') {
        Some((head, tail)) if !head.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
            head.to_string()
        }
        _ => f,
    }
}

/// A battery: designator `BT`/`BAT`, or a battery symbol or footprint.
fn is_battery(c: &Component) -> bool {
    let r = designator(&c.reference);
    let blob = format!("{} {}", c.lib_id, c.footprint).to_ascii_lowercase();
    r.starts_with("BT") || r.starts_with("BAT") || blob.contains("battery")
}

/// An off-board entry: a connector or pin header, a test point, a battery.
/// A mounting hole is not one.
fn is_entry(c: &Component, kinds: &HashMap<String, String>) -> bool {
    let r = designator(&c.reference);
    let hole = r.starts_with("MH") || r.starts_with("MK") || {
        let lib = c.lib_id.to_ascii_lowercase();
        lib.contains("mountinghole")
    };
    // A switch passes a rail through; it does not bring one in.
    if hole || r.starts_with("SW") {
        return false;
    }
    let port = ["J", "P", "X", "JP", "CN", "CON", "TP", "USB"]
        .iter()
        .any(|p| {
            r.strip_prefix(p)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        });
    // A capacitor, resistor, inductor or semiconductor is never an entry,
    // whatever its footprint geometry reads as (a resistor or capacitor
    // array, a solder-jumper footprint).
    let passive_or_semi = is_capacitor(c)
        || is_plain_resistor(c)
        || is_plain_inductor(c)
        || is_led(c)
        || ["R", "C", "L", "FB", "D", "Q", "Y"].iter().any(|p| {
            r.strip_prefix(p)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        });
    if passive_or_semi && !port {
        return false;
    }
    kinds.get(&c.reference).is_some_and(|k| k == "connector")
        || port
        || is_battery(c)
        || is_test_point_or_hole(c)
        || is_connector_like(c)
}

/// A two-terminal part that conducts DC with no meaningful drop: an inductor,
/// a ferrite bead, a resistor of at most [`SERIES_OHMS`].
fn is_series_link(lib: &ModelLibrary, c: &Component) -> bool {
    let r = designator(&c.reference);
    let text = format!("{} {} {}", c.value, c.lib_id, c.footprint).to_ascii_lowercase();
    let class = library_resolution(lib, c)
        .model
        .and_then(|m| m.passive_class);
    let ferrite = matches!(
        class,
        Some(PassiveClass::FerriteBead | PassiveClass::Inductor)
    ) || r.starts_with("FB")
        || r.starts_with("FL")
        || text.contains("ferrite")
        || text.contains("bead");
    let ohms = hauksbee_models::value::parse_value(&c.value)
        .filter(|p| matches!(p.unit.as_deref(), None | Some("Ω")))
        .map(|p| p.si);
    // An L designator is an inductor even when its value names no part (an
    // unfitted link whose value reads "DNP" or "NA").
    let inductor_designator = r
        .strip_prefix('L')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    ferrite
        || is_plain_inductor(c)
        || inductor_designator
        || (is_plain_resistor(c) && ohms.is_some_and(|o| o <= SERIES_OHMS))
}

/// How many series parts away a rail may be fed from.
const FEED_HOPS: usize = 3;

/// Which electrode of a diode a pin is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Polarity {
    Anode,
    Cathode,
    Unknown,
}

/// Which electrode of a diode `pin` is: the schematic's pin name first, then
/// the card's pad map (`binder::pad_role_sources`, which also reads the
/// electrode letters of a lettered pad). Unknown when neither says.
fn polarity(pin: &hauksbee_extract::Pin, model: Option<&hauksbee_models::ModelEntry>) -> Polarity {
    let name = pin
        .function
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches(|c: char| c.is_ascii_digit() || c == '_')
        .to_string();
    let by_name = |n: &str| match n {
        "a" | "anode" => Polarity::Anode,
        "k" | "c" | "cathode" | "kathode" => Polarity::Cathode,
        _ => Polarity::Unknown,
    };
    let named = by_name(&name);
    if named != Polarity::Unknown {
        return named;
    }
    let Some(m) = model.filter(|m| m.kind == ComponentKind::Diode) else {
        return Polarity::Unknown;
    };
    crate::binder::pad_role_sources(pin, &m.pins, Some(m.kind))
        .into_iter()
        .flatten()
        .find_map(|role| match by_name(role.trim_end_matches(|c: char| c.is_ascii_digit())) {
            Polarity::Unknown => None,
            known => Some(known),
        })
        .unwrap_or(Polarity::Unknown)
}

/// A diode or rectifier by what the part is: its card says diode, or (with no
/// card) its designator does.
fn is_diode(c: &Component, model: Option<&hauksbee_models::ModelEntry>) -> bool {
    model.map_or_else(
        || designator(&c.reference).starts_with('D') && !is_led(c),
        |m| m.kind == ComponentKind::Diode,
    )
}

/// A part that joins nets: current may pass from every net in `from` to every
/// net in `to`. A plain series part lists the same nets both ways.
struct Bridge {
    from: Vec<i64>,
    to: Vec<i64>,
}

/// The nets each fitted series part joins: a ferrite, inductor or low-ohm link,
/// or any other part of at most three connected pads that is not a capacitor,
/// resistor, LED or off-board entry (a fuse, diode, switch, jumper, pass FET).
/// Ground is left out; a part that touches fewer than two other nets joins
/// nothing. A diode whose electrodes are known passes power from its anode to
/// its cathode only: a diode pointing at a source blocks it.
fn series_bridges(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    kinds: &HashMap<String, String>,
    grounded: &dyn Fn(i64) -> bool,
) -> Vec<Bridge> {
    let mut out = Vec::new();
    for c in &board.components {
        if !AssemblyState::of(c).is_present() || is_entry(c, kinds) {
            continue;
        }
        let pads: BTreeSet<&str> = (c.pins.iter())
            .filter(|p| p.net.is_some_and(|n| n != 0))
            .map(|p| p.number.as_str())
            .collect();
        let series = is_series_link(lib, c)
            || (pads.len() <= 3
                && !(is_capacitor(c) || is_plain_resistor(c) || is_led(c) || is_plain_inductor(c)));
        if !series {
            continue;
        }
        let nets: BTreeSet<i64> = (c.pins.iter())
            .filter_map(|p| p.net)
            .filter(|n| !grounded(*n))
            .collect();
        if nets.len() < 2 {
            continue;
        }
        let model = library_resolution(lib, c).model;
        if is_diode(c, model.as_ref()) {
            let on = |want: Polarity| -> Vec<i64> {
                let ids: BTreeSet<i64> = (c.pins.iter())
                    .filter(|p| polarity(p, model.as_ref()) == want)
                    .filter_map(|p| p.net)
                    .filter(|n| !grounded(*n))
                    .collect();
                ids.into_iter().collect()
            };
            let (from, to) = (on(Polarity::Anode), on(Polarity::Cathode));
            if !from.is_empty() && !to.is_empty() {
                out.push(Bridge { from, to });
                continue;
            }
        }
        let all: Vec<i64> = nets.into_iter().collect();
        out.push(Bridge {
            from: all.clone(),
            to: all,
        });
    }
    out
}

/// Whether series parts join `rail` within [`FEED_HOPS`] hops to a net `source`
/// accepts, walking from the rail back toward the source against each diode's
/// direction.
fn fed_through(rail: &BTreeSet<i64>, bridges: &[Bridge], source: impl Fn(i64) -> bool) -> bool {
    let mut seen: BTreeSet<i64> = rail.clone();
    let mut frontier: Vec<i64> = rail.iter().copied().collect();
    for _ in 0..FEED_HOPS {
        let mut next = Vec::new();
        for bridge in bridges {
            if !bridge.to.iter().any(|n| frontier.contains(n)) {
                continue;
            }
            for &n in &bridge.from {
                if seen.insert(n) {
                    if source(n) {
                        return true;
                    }
                    next.push(n);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    false
}

/// Nets a fitted part sources on the evidence of its own pins: a `power_out`
/// pin, a card's supply output or regulator output, or an `OUT` pin on a part
/// that also has an `IN` pin (a converter no card models). Off-board entries
/// are left out; they are counted on their own.
fn strong_source_nets(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    kinds: &HashMap<String, String>,
) -> HashSet<i64> {
    let mut out = HashSet::new();
    for c in &board.components {
        if !AssemblyState::of(c).is_present() || is_entry(c, kinds) {
            continue;
        }
        let model = library_resolution(lib, c).model;
        let has_in = c
            .pins
            .iter()
            .any(|p| matches!(function_base(&p.function).as_str(), "IN" | "VIN" | "VI"));
        for p in &c.pins {
            let Some(net) = p.net.filter(|n| *n != 0) else {
                continue;
            };
            let role = model.as_ref().and_then(|m| m.pins.get(&p.number));
            let card_out = model.as_ref().zip(role).is_some_and(|(m, role)| {
                m.supply_outputs.iter().any(|s| s.role == *role)
                    || (m.kind == ComponentKind::Vreg && output_role(role))
            });
            let named_out = has_in
                && matches!(function_base(&p.function).as_str(), "OUT" | "VOUT" | "VO");
            if p.kind.starts_with("power_out") || card_out || named_out {
                out.insert(net);
            }
        }
    }
    out
}

/// A rail to judge: its nets, and the leg that powers it by name (net, volts).
type Group = (BTreeSet<i64>, Option<(String, f64)>);

/// The board's nets by id, and each net's analysis node.
struct Nets<'a> {
    name: HashMap<i64, &'a str>,
    node: HashMap<i64, NodeId>,
}

/// What powers each rail the analysis powers by name alone, and each unpowered
/// rail an unfitted part would have powered. See the module doc.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one pass: domains, then each rail's parts"
)]
pub fn rail_bases(board: &ExtractedBoard, lib: &ModelLibrary, bound: &BoundBoard) -> RailBases {
    let nets = Nets {
        name: board.nets.iter().map(|n| (n.id, n.name.as_str())).collect(),
        node: (board.nets.iter())
            .filter_map(|n| Some((n.id, bound.node(&n.name)?)))
            .collect(),
    };
    let legs: Vec<_> = bound.supplies.iter().map(|l| l.vsource).collect();
    let mut domains = RailDomains::new(&bound.circuit, &bound.behavioral, &legs);
    let board_nodes: HashSet<NodeId> = bound.net_nodes.values().copied().collect();
    domains.join_switches(&bound.circuit, |n| board_nodes.contains(&n));
    let powered = |node: NodeId| {
        domains.fed_by_driver(node)
            || bound
                .supplies
                .iter()
                .any(|l| domains.fed_joined(l.net, node))
    };
    let grounded = |id: i64| {
        id == 0
            || nets.name.get(&id).is_none_or(|n| is_ground(n))
            || nets.node.get(&id).is_none_or(|n| n.is_ground())
    };
    // A net that can carry power into a rail: powered in the analysis, or a
    // name that states a voltage.
    let power_ish = |id: i64| {
        !grounded(id)
            && (nets
                .name
                .get(&id)
                .is_some_and(|n| power_rail_voltage(n).is_some())
                || nets.node.get(&id).is_some_and(|n| powered(*n)))
    };
    let kinds = &bound.component_kinds;
    let mut active_parts = Vec::new();
    // Nets a fitted part other than an off-board entry has a pin on: a rail
    // with none powers nothing on the board.
    let mut used: HashSet<i64> = HashSet::new();
    // Nets a fitted off-board entry is on: what an unfitted link would have
    // carried power from.
    let mut entry_nets: HashSet<i64> = HashSet::new();
    for c in &board.components {
        if !AssemblyState::of(c).is_present() {
            continue;
        }
        let nets_on = c.pins.iter().filter_map(|p| p.net);
        if is_entry(c, kinds) {
            entry_nets.extend(nets_on);
            continue;
        }
        used.extend(nets_on);
        let pins: BTreeSet<&str> = (c.pins.iter())
            .filter(|p| p.net.is_some_and(|n| n != 0))
            .map(|p| p.number.as_str())
            .collect();
        // A key switch's levels are not what a powered rail lets the checks
        // judge.
        if pins.len() >= 3 && !designator(&c.reference).starts_with("SW") {
            active_parts.push(c.reference.clone());
        }
    }
    let any_powered =
        (nets.node.iter()).any(|(id, n)| used.contains(id) && !grounded(*id) && powered(*n));

    // The rails to judge: each name-powered leg's domain, then each group of
    // unpowered nets.
    let mut groups: Vec<Group> = Vec::new();
    for leg in &bound.supplies {
        let ids: BTreeSet<i64> = (nets.node.iter())
            .filter(|(_, n)| !n.is_ground() && domains.joined(leg.net, **n))
            .map(|(id, _)| *id)
            .collect();
        groups.push((
            ids,
            Some((leg.net_name.clone(), leg.supply.nominal_volts())),
        ));
    }
    // Only a group an unfitted part touches can have lost its source to it.
    let unfitted_nets: HashSet<i64> = (board.components.iter())
        .filter(|c| matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_)))
        .flat_map(|c| c.pins.iter().filter_map(|p| p.net))
        .collect();
    let mut unpowered: Vec<(i64, NodeId)> = (nets.node.iter())
        .filter(|(id, n)| !grounded(**id) && !powered(**n))
        .map(|(id, n)| (*id, *n))
        .collect();
    unpowered.sort_unstable_by_key(|(id, _)| *id);
    let mut taken: HashSet<i64> = HashSet::new();
    for &(id, node) in &unpowered {
        if !unfitted_nets.contains(&id) || !taken.insert(id) {
            continue;
        }
        let mut ids = BTreeSet::from([id]);
        for &(other, n) in &unpowered {
            if other != id && domains.joined(node, n) {
                taken.insert(other);
                ids.insert(other);
            }
        }
        groups.push((ids, None));
    }

    let board_powered_in = entry_nets.iter().any(|n| power_ish(*n));
    let mut rails = Vec::new();
    let mut rail_ids: Vec<BTreeSet<i64>> = Vec::new();
    for (ids, leg) in groups {
        let mut basis = RailBasis {
            nets: Vec::new(),
            volts: leg.as_ref().map(|(_, v)| *v),
            entries: Vec::new(),
            unfitted: Vec::new(),
            feeds: Vec::new(),
            loads: Vec::new(),
            supply_loads: Vec::new(),
            active_loads: Vec::new(),
            board_powered_in,
            fed_through: false,
        };
        let mut names: Vec<String> = ids
            .iter()
            .filter_map(|id| nets.name.get(id).map(|n| (*n).to_string()))
            .collect();
        names.sort();
        if let Some((first, _)) = &leg {
            names.retain(|n| n != first);
            names.insert(0, first.clone());
        }
        basis.nets = names;
        for c in &board.components {
            let on: Vec<_> = (c.pins.iter())
                .filter(|p| p.net.is_some_and(|n| ids.contains(&n)))
                .collect();
            if on.is_empty() {
                continue;
            }
            let others: BTreeSet<i64> = (c.pins.iter())
                .filter_map(|p| p.net)
                .filter(|n| !ids.contains(n) && !grounded(*n))
                .collect();
            let model = library_resolution(lib, c).model;
            match AssemblyState::of(c) {
                AssemblyState::DnpAbsent(_) => {
                    let link = (others.len() == 1 && is_series_link(lib, c))
                        .then(|| others.iter().next().copied())
                        .flatten()
                        .filter(|n| power_ish(*n) || entry_nets.contains(n));
                    let card_out = model.as_ref().is_some_and(|m| {
                        let regulating = m.kind == ComponentKind::Vreg || !m.behavioral.is_empty();
                        on.iter().any(|p| {
                            m.pins.get(&p.number).is_some_and(|role| {
                                (regulating && output_role(role))
                                    || m.supply_outputs.iter().any(|s| s.role == *role)
                            })
                        })
                    });
                    let has_in = (c.pins.iter()).any(|p| {
                        matches!(function_base(&p.function).as_str(), "IN" | "VIN" | "VI")
                    });
                    let named_out = has_in
                        && on.iter().any(|p| {
                            matches!(function_base(&p.function).as_str(), "OUT" | "VOUT" | "VO")
                        });
                    let power_out = on.iter().any(|p| p.kind.starts_with("power_out"));
                    if let Some(to) = link {
                        basis.unfitted.push(Unfitted {
                            reference: c.reference.clone(),
                            value: c.value.clone(),
                            link_to: nets.name.get(&to).map(|n| (*n).to_string()),
                        });
                    } else if card_out || named_out || power_out {
                        basis.unfitted.push(Unfitted {
                            reference: c.reference.clone(),
                            value: c.value.clone(),
                            link_to: None,
                        });
                    }
                }
                AssemblyState::IdentityUnknown(_) => basis.feeds.push(c.reference.clone()),
                AssemblyState::Present(_) => {
                    if is_entry(c, kinds) {
                        basis.entries.push(c.reference.clone());
                        continue;
                    }
                    let into_rail = || others.iter().any(|n| power_ish(*n));
                    let supply_out = on.iter().any(|p| p.kind.starts_with("power_out"))
                        || model.as_ref().is_some_and(|m| {
                            on.iter().any(|p| {
                                m.pins.get(&p.number).is_some_and(|role| {
                                    m.supply_outputs.iter().any(|s| s.role == *role)
                                        || (m.kind == ComponentKind::Vreg && output_role(role))
                                })
                            })
                        });
                    let pads: BTreeSet<&str> = (c.pins.iter())
                        .filter(|p| p.net.is_some_and(|n| n != 0))
                        .map(|p| p.number.as_str())
                        .collect();
                    let two = pads.len() <= 2;
                    let feeds = if supply_out {
                        true
                    } else if is_capacitor(c) {
                        false
                    } else if let Some(m) = &model {
                        match m.kind {
                            ComponentKind::Passive => {
                                m.passive_class != Some(PassiveClass::Capacitor) && into_rail()
                            }
                            // A diode feeds only its cathode's net (an
                            // electrode nothing names may be either).
                            ComponentKind::Diode => {
                                on.iter()
                                    .any(|p| polarity(p, Some(m)) != Polarity::Anode)
                                    && into_rail()
                            }
                            ComponentKind::BjtNpn
                            | ComponentKind::BjtPnp
                            | ComponentKind::Nmos
                            | ComponentKind::Pmos
                            | ComponentKind::AnalogSwitch
                            | ComponentKind::Ignore => into_rail(),
                            _ => false,
                        }
                    } else if two && is_led(c) {
                        // An LED's anode is never a rail's source.
                        false
                    } else if is_diode(c, None)
                        && on.iter().all(|p| polarity(p, None) == Polarity::Anode)
                    {
                        // A diode with no card, its anode on the rail: it
                        // draws from the rail, it does not feed it.
                        false
                    } else if two {
                        // A resistor, inductor or diode with no model feeds a
                        // rail only from another rail.
                        into_rail()
                    } else {
                        // No model: a pin that can drive and is not a supply
                        // input may be the rail's source.
                        on.iter()
                            .any(|p| pin_can_drive(&p.kind) && !supply_input_name(&p.function))
                    };
                    if feeds {
                        basis.feeds.push(c.reference.clone());
                    } else if !(is_capacitor(c) || is_plain_resistor(c) || is_plain_inductor(c)) {
                        basis.loads.push(c.reference.clone());
                        let draws = on.iter().any(|p| {
                            let role = model.as_ref().and_then(|m| m.pins.get(&p.number));
                            p.kind.starts_with("power_in")
                                || [Some(p.function.as_str()), role.map(String::as_str)]
                                    .into_iter()
                                    .flatten()
                                    .any(supply_input_name)
                        }) && !on.iter().all(|p| regulator_output_name(&p.function));
                        if draws {
                            basis.supply_loads.push(c.reference.clone());
                            if hauksbee_extract::is_active_ic(c) {
                                basis.active_loads.push(c.reference.clone());
                            }
                        }
                    }
                }
            }
        }
        // An unpowered rail matters here only when an unfitted part would
        // have powered it.
        if leg.is_some() || !basis.unfitted.is_empty() {
            for list in [
                &mut basis.entries,
                &mut basis.feeds,
                &mut basis.loads,
                &mut basis.supply_loads,
                &mut basis.active_loads,
            ] {
                list.sort_by_key(|r| crate::bind_report::natural_ref_key(r));
                list.dedup();
            }
            rail_ids.push(ids);
            rails.push(basis);
        }
    }
    // A rail with nothing of its own behind it may still be fed through
    // series parts and diodes from a net something sources: an off-board entry,
    // a modelled driver, a part's supply output, or another rail that has an
    // entry or a fitted source (a diode-OR of two feeds, a converter output).
    let strong = strong_source_nets(board, lib, kinds);
    let rail_sourced: HashSet<i64> = (rails.iter().zip(&rail_ids))
        .filter(|(r, _)| !r.entries.is_empty() || !r.feeds.is_empty())
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();
    let bridges: std::cell::OnceCell<Vec<Bridge>> = std::cell::OnceCell::new();
    for (basis, ids) in rails.iter_mut().zip(&rail_ids) {
        if basis.volts.is_some()
            && basis.unfitted.is_empty()
            && basis.entries.is_empty()
            && basis.feeds.is_empty()
            && !basis.supply_loads.is_empty()
        {
            let bridges = bridges.get_or_init(|| series_bridges(board, lib, kinds, &grounded));
            basis.fed_through = fed_through(ids, bridges, |id| {
                entry_nets.contains(&id)
                    || strong.contains(&id)
                    || rail_sourced.contains(&id)
                    || nets
                        .node
                        .get(&id)
                        .is_some_and(|n| domains.fed_by_driver(*n))
            });
        }
    }
    RailBases {
        rails,
        any_powered,
        active_parts,
    }
}
