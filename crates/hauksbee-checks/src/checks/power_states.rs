//! Power-state checks: what a board does when one of its rails is switched off
//! while another stays on.
//!
//! The other power checks judge the board in its fully-on state. A board that
//! gates a rail from a microcontroller (a regulator whose enable pin is a GPIO,
//! a P-channel load switch) has a second state: the rail is off, the rest is
//! on. Two faults live only there.
//!
//! * `LintCheck::PowerDomainBackfeed`: a part with every supply off while a
//!   signal pin is pulled up to, or driven from, a rail that stays on. The
//!   pin's protection clamp conducts into the unpowered chip and half-wakes it.
//! * `LintCheck::PowerDomainBusLockup`: an I2C bus whose pull-ups sit on the
//!   gated rail while powered devices still use the bus, or an unpowered device
//!   clamping a bus that powered devices still use.
//!
//! ## The power graph
//!
//! Nets joined by a fitted inductor, ferrite bead, fuse, net tie or link of at
//! most 0.1 ohm are one rail group. Groups are joined by *feeds*: a regulator
//! card (input pin to output pin), an uncarded IC whose pins are named like a
//! regulator, a diode, a P-channel or other transistor channel between two
//! rails, a resistor of at most 100 ohm between two rails. A feed is
//! *controllable* when its enable pin (or the transistor's gate or base) is
//! driven by a GPIO, directly, through one series resistor, or through one
//! inverting transistor.
//!
//! A rail is **on** when it is reachable from a *source*: a battery holder, a
//! connector pad on a rail nothing on the board feeds, a module's own supply
//! pin. The check abstains (a rail whose state it cannot decide never supports
//! a finding) on a rail with no traced source, a rail an untraced output pin
//! drives (a charger's output, an unmodelled converter, a diode whose
//! orientation is not known), and everything downstream of one.
//!
//! For each controllable feed the check removes it, recomputes reachability and
//! calls the rails that fall out *off in that state*. A part is off when every
//! supply pad its card names sits in an off rail. Only a carded part is judged:
//! the card's pin roles are what say which pads are supplies and which are
//! signals.
//!
//! A pin the card rates for a voltage whatever the supply (a `supply_range` or
//! `input_tolerance` row, `declared_pin_ceiling_v`) is silent: the card has
//! said the pin is built for it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::bus_loading::is_i2c_net;
use hauksbee_bind::binder::power_rail_voltage;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, is_plain_resistor, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity,
};
use hauksbee_models::pin_roles::{power_pin, PowerKind};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{declared_pin_ceiling_v, netclass, ComponentKind, ModelEntry, ModelLibrary};

/// The most a series resistor between two rails may have before it stops being a
/// feed (a current-sense or filter resistor) and is a signal element.
const FEED_RESISTOR_MAX_OHMS: f64 = 100.0;

/// Resistance (ohm) at or above which a pull-up is too weak to be more than a
/// note: a few microamps through the clamp.
const WEAK_PULLUP_OHMS: f64 = 100_000.0;

/// One part, with the model that resolved it when it did.
struct Part<'a> {
    comp: &'a Component,
    model: Option<ModelEntry>,
    fitted: bool,
}

/// A set of nets that are one rail (joined by wire-like parts).
struct Group {
    nets: Vec<i64>,
    ground: bool,
    rail: bool,
    name: String,
}

/// A way to turn a rail off: the controllable feed and what drives it.
struct Switch {
    /// The regulator or transistor reference.
    part: String,
    /// "U3's enable pin EN" or "Q2's gate".
    what: String,
    /// "U1 pad 12 (GPIO5)".
    driver: String,
    /// Index of the part that drives it.
    driver_part: usize,
}

/// Power flows `from` to `to` unless `switch` is off.
struct Edge {
    from: usize,
    to: usize,
    switch: Option<usize>,
}

/// What a part's pad does in a feed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FeedRole {
    In,
    Out,
    Aux,
    Enable,
    Other,
}

fn feed_role(role: &str) -> FeedRole {
    match role {
        "in" | "vin" | "pvin" | "vina" | "in2" | "avin" => FeedRole::In,
        "out" | "out2" | "outs" | "vout" | "sw" | "l1" | "l2" | "lx" | "ph" | "vo" => FeedRole::Out,
        "bat" | "batt" | "vbat" | "sys" | "vsys" => FeedRole::Aux,
        "en" | "en1" | "en2" | "shdn" | "on_off" | "ce" | "ce_n" | "ctrl" | "on" | "enable" => {
            FeedRole::Enable
        }
        _ => FeedRole::Other,
    }
}

/// A pad name with `KiCad`'s `_<pad>` suffix and trailing channel digits removed,
/// lower-cased: `SW1` and `FB_3` read as `sw` and `fb`.
fn pin_base(function: &str) -> String {
    let f = function.trim().to_ascii_lowercase();
    let f = match f.rsplit_once('_') {
        Some((head, tail)) if !head.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
            head.to_string()
        }
        _ => f,
    };
    f.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
}

/// A card role (or pad name) that firmware drives: a port pin.
fn role_is_port(role: &str) -> bool {
    let b = role.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    role.starts_with("gpio")
        || (role.starts_with("gp") && digit(2))
        || (role.starts_with("io") && (digit(2) || b.get(2) == Some(&b'_')))
        || (b.len() >= 2 && b[0] == b'p' && (digit(1) || (b[1].is_ascii_lowercase() && digit(2))))
}

/// Roles that are the part's own supply or return, never a signal.
fn is_power_role(role: &str, kind: ComponentKind) -> bool {
    power_pin(role, kind).is_some() || is_supply_input_role(role)
}

/// A higher-voltage input role a part runs from (not a `[models.pins]` primary
/// supply by the vocabulary, still a way the part is powered).
fn is_supply_input_role(role: &str) -> bool {
    matches!(
        role,
        "vin" | "vbus" | "vbat" | "vsys" | "raw" | "5v" | "3v3" | "vusb" | "vmot"
    )
}

/// Whether the role is a supply pad (a pad whose rail, when off, leaves the part off).
fn is_supply_role(role: &str, kind: ComponentKind) -> bool {
    power_pin(role, kind).is_some_and(|p| p.kind == PowerKind::Supply) || is_supply_input_role(role)
}

/// Kinds of part a signal pin can back-feed: a chip with input protection.
const fn is_receiver_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Digital
            | ComponentKind::Adc
            | ComponentKind::Dac
            | ComponentKind::ShiftRegister
            | ComponentKind::Mcu
            | ComponentKind::Timer
            | ComponentKind::Comparator
            | ComponentKind::Opamp
            | ComponentKind::AnalogSwitch
    )
}

fn looks_like_battery_holder(comp: &Component) -> bool {
    let text = format!("{} {} {}", comp.value, comp.lib_id, comp.footprint).to_ascii_lowercase();
    text.contains("batt_holder")
        || text.contains("cell_holder")
        || (text.contains("battery") && (text.contains("holder") || text.contains("clip")))
}

pub(super) fn is_fuse(comp: &Component) -> bool {
    let text = format!("{} {}", comp.lib_id, comp.value).to_ascii_lowercase();
    text.contains("fuse")
        || comp
            .reference
            .strip_prefix('F')
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

pub(super) fn is_net_tie(comp: &Component) -> bool {
    let text = format!("{} {}", comp.lib_id, comp.footprint).to_ascii_lowercase();
    text.contains("nettie")
        || text.contains("net_tie")
        || comp
            .reference
            .strip_prefix("NT")
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

pub(super) fn ohms(comp: &Component) -> Option<f64> {
    hauksbee_models::value::parse_value(&comp.value)
        .filter(|p| matches!(p.unit.as_deref(), None | Some("Ω")))
        .map(|p| p.si)
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// The distinct non-zero nets a part's pads are on.
fn nets_of(comp: &Component) -> BTreeSet<i64> {
    comp.pins
        .iter()
        .filter_map(|p| p.net)
        .filter(|&n| n != 0)
        .collect()
}

/// The board's power graph.
struct Graph<'a> {
    board: &'a ExtractedBoard,
    parts: Vec<Part<'a>>,
    /// Net id to every fitted (part, pin) on it.
    net_pins: HashMap<i64, Vec<(usize, usize)>>,
    group_of: HashMap<i64, usize>,
    groups: Vec<Group>,
    edges: Vec<Edge>,
    switches: Vec<Switch>,
    fed: Vec<bool>,
    untraced: Vec<bool>,
    battery: Vec<bool>,
    module: Vec<bool>,
    connector: Vec<bool>,
    unknown: Vec<bool>,
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

impl<'a> Graph<'a> {
    fn build(board: &'a ExtractedBoard, lib: &ModelLibrary) -> Self {
        let mut models: HashMap<usize, ModelEntry> = super::BoardStatics::resolve_all(board, lib)
            .map(|(i, _, m)| (i, m))
            .collect();
        let parts: Vec<Part<'a>> = board
            .components
            .iter()
            .enumerate()
            .map(|(i, comp)| Part {
                comp,
                model: models.remove(&i),
                fitted: AssemblyState::of(comp).is_present(),
            })
            .collect();
        let mut net_pins: HashMap<i64, Vec<(usize, usize)>> = HashMap::new();
        for (i, part) in parts.iter().enumerate().filter(|(_, p)| p.fitted) {
            for (j, pin) in part.comp.pins.iter().enumerate() {
                if let Some(n) = pin.net.filter(|&n| n != 0) {
                    net_pins.entry(n).or_default().push((i, j));
                }
            }
        }
        let mut graph = Self {
            board,
            parts,
            net_pins,
            group_of: HashMap::new(),
            groups: Vec::new(),
            edges: Vec::new(),
            switches: Vec::new(),
            fed: Vec::new(),
            untraced: Vec::new(),
            battery: Vec::new(),
            module: Vec::new(),
            connector: Vec::new(),
            unknown: Vec::new(),
        };
        graph.make_groups();
        graph.make_feeds();
        graph.settle();
        graph
    }

    fn model_of(&self, part: usize) -> Option<&ModelEntry> {
        self.parts[part].model.as_ref()
    }

    /// The card role of a pad, lower-cased, by pad number then pad name.
    fn role(&self, part: usize, pin: &Pin) -> Option<String> {
        let m = self.model_of(part)?;
        m.pins
            .get(&pin.number)
            .or_else(|| m.pins.get(pin.function.trim()))
            .map(|r| r.to_ascii_lowercase())
    }

    fn net_name(&self, net: i64) -> String {
        self.board
            .net(net)
            .map_or_else(|| net.to_string(), |n| n.name.clone())
    }

    fn group(&self, net: Option<i64>) -> Option<usize> {
        self.group_of.get(&net.filter(|&n| n != 0)?).copied()
    }

    /// Join the nets wire-like parts join, and classify each group.
    fn make_groups(&mut self) {
        let ids: Vec<i64> = self
            .board
            .nets
            .iter()
            .map(|n| n.id)
            .filter(|&n| n != 0)
            .collect();
        let dense: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        let mut parent: Vec<usize> = (0..ids.len()).collect();
        for part in self.parts.iter().filter(|p| p.fitted) {
            let nets = nets_of(part.comp);
            if nets.len() != 2 || part.comp.pins.len() > 4 {
                continue;
            }
            let class = RefClass::of(&part.comp.reference);
            let wire_like = matches!(class, RefClass::Inductor | RefClass::FerriteBead)
                || is_fuse(part.comp)
                || is_net_tie(part.comp)
                || (is_plain_resistor(part.comp) && ohms(part.comp).is_some_and(|o| o <= 0.1));
            if !wire_like {
                continue;
            }
            let mut it = nets.iter().filter_map(|n| dense.get(n).copied());
            if let (Some(a), Some(b)) = (it.next(), it.next()) {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                parent[ra] = rb;
            }
        }
        let mut index: HashMap<usize, usize> = HashMap::new();
        for (i, &net) in ids.iter().enumerate() {
            let root = find(&mut parent, i);
            let g = *index.entry(root).or_insert_with(|| {
                self.groups.push(Group {
                    nets: Vec::new(),
                    ground: false,
                    rail: false,
                    name: String::new(),
                });
                self.groups.len() - 1
            });
            self.groups[g].nets.push(net);
            self.group_of.insert(net, g);
        }
        for g in 0..self.groups.len() {
            let names: Vec<String> = self.groups[g]
                .nets
                .iter()
                .map(|&n| self.net_name(n))
                .collect();
            let group = &mut self.groups[g];
            group.ground = names.iter().any(|n| netclass::is_ground(n));
            group.rail = !group.ground
                && names
                    .iter()
                    .any(|n| power_rail_voltage(n).is_some() || netclass::is_positive_supply(n));
            group.name = names
                .iter()
                .find(|n| power_rail_voltage(n).is_some())
                .or_else(|| names.first())
                .cloned()
                .unwrap_or_default();
        }
        // A net a card's primary supply pad sits on is a rail whatever it is called.
        let mut powered: Vec<usize> = Vec::new();
        for (i, part) in self.parts.iter().enumerate().filter(|(_, p)| p.fitted) {
            let Some(model) = part.model.as_ref() else {
                continue;
            };
            for pin in &part.comp.pins {
                let Some(role) = self.role(i, pin) else {
                    continue;
                };
                if is_supply_role(&role, model.kind) && model.kind != ComponentKind::Vreg {
                    powered.extend(self.group(pin.net));
                }
            }
        }
        for g in powered {
            if !self.groups[g].ground {
                self.groups[g].rail = true;
            }
        }
        let n = self.groups.len();
        self.fed = vec![false; n];
        self.untraced = vec![false; n];
        self.battery = vec![false; n];
        self.module = vec![false; n];
        self.connector = vec![false; n];
        self.unknown = vec![false; n];
    }

    /// A GPIO-like pad of some other part on `net`, described for the finding.
    fn port_on(&self, net: i64, not: usize) -> Option<(usize, String)> {
        let members = self.net_pins.get(&net)?;
        for &(i, j) in members {
            if i == not {
                continue;
            }
            let part = &self.parts[i];
            let vreg = part
                .model
                .as_ref()
                .is_some_and(|m| m.kind == ComponentKind::Vreg);
            let chip = RefClass::of(&part.comp.reference) == RefClass::ActiveIc
                || part
                    .model
                    .as_ref()
                    .is_some_and(|m| m.kind == ComponentKind::Mcu);
            if vreg || !chip {
                continue;
            }
            let pin = &part.comp.pins[j];
            if pin.kind.to_ascii_lowercase().contains("input")
                || pin.kind.to_ascii_lowercase().contains("power")
            {
                continue;
            }
            let role = self
                .role(i, pin)
                .unwrap_or_else(|| pin.function.trim().to_ascii_lowercase());
            if role_is_port(&role) {
                let name = if pin.function.trim().is_empty() {
                    role
                } else {
                    pin.function.trim().to_string()
                };
                return Some((
                    i,
                    format!("{} pad {} ({name})", part.comp.reference, pin.number),
                ));
            }
        }
        None
    }

    /// The far net of a series resistor on `net` that is not a rail or ground.
    fn resistor_far_nets(&self, net: i64) -> Vec<i64> {
        let mut out = Vec::new();
        for &(i, _) in self.net_pins.get(&net).into_iter().flatten() {
            let comp = self.parts[i].comp;
            let nets = nets_of(comp);
            if nets.len() != 2 || !is_plain_resistor(comp) {
                continue;
            }
            for far in nets.into_iter().filter(|&n| n != net) {
                let g = self.group(Some(far));
                if g.is_some_and(|g| !self.groups[g].ground && !self.groups[g].rail) {
                    out.push(far);
                }
            }
        }
        out
    }

    /// What drives a control net: a GPIO on it, one series resistor away, or
    /// through one inverting transistor. `not` is the part the net belongs to.
    fn controller(&self, net: i64, not: usize) -> Option<(usize, String)> {
        if self
            .group(Some(net))
            .is_none_or(|g| self.groups[g].rail || self.groups[g].ground)
        {
            return None;
        }
        let direct = |n: i64| {
            self.port_on(n, not).or_else(|| {
                self.resistor_far_nets(n)
                    .into_iter()
                    .find_map(|f| self.port_on(f, not))
            })
        };
        if let Some(found) = direct(net) {
            return Some(found);
        }
        // An NPN or N-channel stage whose collector or drain is this net.
        for &(i, j) in self.net_pins.get(&net).into_iter().flatten() {
            let part = &self.parts[i];
            let Some(model) = part.model.as_ref() else {
                continue;
            };
            if !matches!(model.kind, ComponentKind::BjtNpn | ComponentKind::Nmos) {
                continue;
            }
            let role = self.role(i, &part.comp.pins[j]).unwrap_or_default();
            if !matches!(role.as_str(), "collector" | "drain" | "c" | "d") {
                continue;
            }
            for pin in &part.comp.pins {
                let r = self.role(i, pin).unwrap_or_default();
                if matches!(r.as_str(), "base" | "gate" | "b" | "g") {
                    if let Some(g) = pin.net.filter(|&n| n != 0) {
                        if let Some(found) = direct(g) {
                            return Some(found);
                        }
                    }
                }
            }
        }
        None
    }

    fn add_switch(&mut self, part: usize, what: String, driver: (usize, String)) -> usize {
        self.switches.push(Switch {
            part: self.parts[part].comp.reference.clone(),
            what,
            driver: driver.1,
            driver_part: driver.0,
        });
        self.switches.len() - 1
    }

    fn add_edge(&mut self, from: usize, to: usize, switch: Option<usize>) {
        if from != to && !self.groups[from].ground && !self.groups[to].ground {
            self.edges.push(Edge { from, to, switch });
            self.fed[to] = true;
        }
    }

    /// Every feed, source and untraced output on the board.
    fn make_feeds(&mut self) {
        for i in 0..self.parts.len() {
            if !self.parts[i].fitted {
                continue;
            }
            self.regulator_feeds(i);
            self.transistor_feed(i);
            self.two_pin_feed(i);
            self.source_evidence(i);
        }
    }

    /// The pads of `part` read as feed roles: from the card, or for an uncarded
    /// regulator-shaped IC from its pad names.
    fn feed_pads(&self, i: usize) -> Vec<(usize, FeedRole)> {
        let part = &self.parts[i];
        if let Some(model) = part.model.as_ref() {
            if model.kind != ComponentKind::Vreg {
                return Vec::new();
            }
            return part
                .comp
                .pins
                .iter()
                .enumerate()
                .filter_map(|(j, p)| Some((j, feed_role(&self.role(i, p)?))))
                .collect();
        }
        if RefClass::of(&part.comp.reference) != RefClass::ActiveIc {
            return Vec::new();
        }
        let bases: Vec<String> = part
            .comp
            .pins
            .iter()
            .map(|p| pin_base(&p.function))
            .collect();
        if bases
            .iter()
            .any(|b| matches!(b.as_str(), "vcc" | "vdd" | "v+"))
        {
            return Vec::new();
        }
        let pads: Vec<(usize, FeedRole)> = bases
            .iter()
            .enumerate()
            .map(|(j, b)| {
                let role = match b.as_str() {
                    "in" | "vin" | "vi" | "pvin" | "avin" | "vina" | "input" => FeedRole::In,
                    "out" | "vout" | "vo" | "output" | "sw" | "lx" | "ph" => FeedRole::Out,
                    "en" | "enable" | "shdn" | "on" | "ce" => FeedRole::Enable,
                    _ => FeedRole::Other,
                };
                (j, role)
            })
            .collect();
        let has = |r| pads.iter().any(|&(_, x)| x == r);
        if has(FeedRole::In) && has(FeedRole::Out) {
            pads
        } else {
            Vec::new()
        }
    }

    fn regulator_feeds(&mut self, i: usize) {
        let pads = self.feed_pads(i);
        if pads.is_empty() {
            return;
        }
        let comp = self.parts[i].comp;
        let groups = |role: FeedRole| -> Vec<usize> {
            let mut gs: Vec<usize> = pads
                .iter()
                .filter(|&&(_, r)| r == role)
                .filter_map(|&(j, _)| self.group(comp.pins[j].net))
                .collect();
            gs.sort_unstable();
            gs.dedup();
            gs
        };
        let (ins, outs, aux) = (
            groups(FeedRole::In),
            groups(FeedRole::Out),
            groups(FeedRole::Aux),
        );
        for &g in ins.iter().chain(&outs) {
            if !self.groups[g].ground {
                self.groups[g].rail = true;
            }
        }
        if ins.is_empty() || outs.is_empty() {
            for g in outs.into_iter().chain(aux) {
                self.untraced[g] = true;
            }
            return;
        }
        // Where a battery pad shares a group with no battery holder, the part
        // sources it.
        for g in aux {
            if !ins.contains(&g) {
                self.untraced[g] = true;
            }
        }
        let enables: Vec<usize> = pads
            .iter()
            .filter(|&&(_, r)| r == FeedRole::Enable)
            .map(|&(j, _)| j)
            .collect();
        let mut switch = None;
        if !enables.is_empty() {
            let driven = enables.iter().find_map(|&j| {
                let net = comp.pins[j].net.filter(|&n| n != 0)?;
                Some((j, self.controller(net, i)?))
            });
            if let Some((j, found)) = driven {
                let what = format!(
                    "{}'s enable pin {}",
                    comp.reference,
                    pin_label(&comp.pins[j])
                );
                // A part with several outputs or enables cannot say which one
                // the driven enable gates: abstain on its outputs.
                if outs.len() > 1 || enables.len() > 1 {
                    for &g in &outs {
                        self.untraced[g] = true;
                    }
                    return;
                }
                switch = Some(self.add_switch(i, what, found));
            }
        }
        for &from in &ins {
            for &to in &outs {
                self.add_edge(from, to, switch);
            }
        }
    }

    /// A transistor channel between two rails.
    fn transistor_feed(&mut self, i: usize) {
        let comp = self.parts[i].comp;
        let Some(kind) = self.model_of(i).map(|m| m.kind) else {
            return;
        };
        if !matches!(
            kind,
            ComponentKind::Pmos
                | ComponentKind::Nmos
                | ComponentKind::BjtNpn
                | ComponentKind::BjtPnp
        ) {
            return;
        }
        let find_pad = |names: &[&str]| {
            comp.pins
                .iter()
                .position(|p| self.role(i, p).is_some_and(|r| names.contains(&r.as_str())))
        };
        let (a, b, ctl) = match kind {
            ComponentKind::Pmos | ComponentKind::Nmos => (
                find_pad(&["source", "s"]),
                find_pad(&["drain", "d"]),
                find_pad(&["gate", "g"]),
            ),
            ComponentKind::BjtPnp => (
                find_pad(&["emitter", "e"]),
                find_pad(&["collector", "c"]),
                find_pad(&["base", "b"]),
            ),
            _ => (
                find_pad(&["collector", "c"]),
                find_pad(&["emitter", "e"]),
                find_pad(&["base", "b"]),
            ),
        };
        let (Some(a), Some(b)) = (a, b) else { return };
        let (Some(ga), Some(gb)) = (self.group(comp.pins[a].net), self.group(comp.pins[b].net))
        else {
            return;
        };
        if self.groups[ga].ground || self.groups[gb].ground {
            return;
        }
        if !self.groups[ga].rail || !self.groups[gb].rail {
            return;
        }
        let switch = ctl
            .and_then(|c| comp.pins[c].net.filter(|&n| n != 0).map(|n| (c, n)))
            .and_then(|(c, n)| {
                self.controller(n, i)
                    .map(|found| (pin_label(&comp.pins[c]), found))
            });
        let sw = switch.map(|(label, found)| {
            self.add_switch(i, format!("{}'s {label}", comp.reference), found)
        });
        // The P-channel (or PNP) conducts source to drain; its body diode (or
        // junction) always conducts drain to source. Others conduct both ways.
        match kind {
            ComponentKind::Pmos | ComponentKind::BjtPnp => {
                self.add_edge(ga, gb, sw);
                self.add_edge(gb, ga, None);
            }
            _ => {
                self.add_edge(ga, gb, sw);
                self.add_edge(gb, ga, sw);
            }
        }
    }

    /// Diodes and low-value resistors between two rails.
    fn two_pin_feed(&mut self, i: usize) {
        let comp = self.parts[i].comp;
        let nets = nets_of(comp);
        if nets.len() != 2 {
            return;
        }
        let groups: Vec<usize> = nets.iter().filter_map(|&n| self.group(Some(n))).collect();
        let [g0, g1] = groups[..] else { return };
        if g0 == g1 || self.groups[g0].ground || self.groups[g1].ground {
            return;
        }
        match RefClass::of(&comp.reference) {
            RefClass::Diode => {
                // Orientation from the pad names the symbol carries.
                let side = |name: &str| {
                    comp.pins
                        .iter()
                        .find(|p| p.function.trim().eq_ignore_ascii_case(name))
                        .and_then(|p| self.group(p.net))
                };
                match (side("A"), side("K")) {
                    (Some(a), Some(k)) if a != k => {
                        if self.groups[a].rail {
                            self.add_edge(a, k, None);
                        } else {
                            self.untraced[k] = true;
                        }
                    }
                    _ => {
                        if self.groups[g0].rail && self.groups[g1].rail {
                            self.add_edge(g0, g1, None);
                            self.add_edge(g1, g0, None);
                        }
                    }
                }
            }
            RefClass::Resistor
                if is_plain_resistor(comp)
                    && ohms(comp).is_some_and(|o| o <= FEED_RESISTOR_MAX_OHMS) =>
            {
                if self.groups[g0].rail && self.groups[g1].rail {
                    self.add_edge(g0, g1, None);
                    self.add_edge(g1, g0, None);
                }
            }
            _ => {}
        }
    }

    /// Batteries, connector pads, modules' own supply pins and outputs the
    /// graph cannot trace.
    fn source_evidence(&mut self, i: usize) {
        let comp = self.parts[i].comp;
        let battery = looks_like_battery_holder(comp);
        let connector = !battery && is_connector_like(comp);
        let supplies: Vec<String> = self
            .model_of(i)
            .map(|m| m.supply_outputs.iter().map(|o| o.role.clone()).collect())
            .unwrap_or_default();
        let vreg = self
            .model_of(i)
            .is_some_and(|m| m.kind == ComponentKind::Vreg);
        let feeds = !self.feed_pads(i).is_empty();
        for pin in &comp.pins {
            let Some(g) = self.group(pin.net) else {
                continue;
            };
            if self.groups[g].ground {
                continue;
            }
            if battery {
                self.battery[g] = true;
            } else if connector {
                self.connector[g] = true;
            }
            let role = self.role(i, pin);
            if role
                .as_deref()
                .is_some_and(|r| supplies.iter().any(|s| s == r))
            {
                self.module[g] = true;
            }
            // A pad the symbol calls a power output and no feed above explains.
            let kind = pin.kind.to_ascii_lowercase();
            if (kind == "power_out" || kind == "power_output") && !vreg && !feeds {
                self.untraced[g] = true;
            }
        }
    }

    /// Decide which rails have a traced source and which do not.
    fn settle(&mut self) {
        let n = self.groups.len();
        for g in 0..n {
            let evidence = self.battery[g] || self.module[g] || (self.connector[g] && !self.fed[g]);
            self.unknown[g] = !self.groups[g].ground
                && !self.battery[g]
                && (self.untraced[g] || (!self.fed[g] && !evidence));
        }
        // Everything an unknown rail feeds is unknown, battery holders aside.
        loop {
            let mut changed = false;
            for e in &self.edges {
                if self.unknown[e.from] && !self.unknown[e.to] && !self.battery[e.to] {
                    self.unknown[e.to] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// The rails reachable from a source with the `skip` switch open.
    fn reach(&self, skip: Option<usize>) -> Vec<bool> {
        let n = self.groups.len();
        let mut on = vec![false; n];
        let mut stack: Vec<usize> = (0..n)
            .filter(|&g| {
                !self.unknown[g]
                    && !self.groups[g].ground
                    && (self.battery[g] || self.module[g] || (self.connector[g] && !self.fed[g]))
            })
            .collect();
        for &g in &stack {
            on[g] = true;
        }
        while let Some(g) = stack.pop() {
            for e in self.edges.iter().filter(|e| e.from == g) {
                if e.switch.is_some() && e.switch == skip {
                    continue;
                }
                if !on[e.to] && !self.unknown[e.to] {
                    on[e.to] = true;
                    stack.push(e.to);
                }
            }
        }
        on
    }

    /// A part's supply groups: `None` when it names none, or any is on a rail
    /// the graph cannot decide, or one sits on ground.
    fn supply_groups(&self, i: usize) -> Option<Vec<usize>> {
        let part = &self.parts[i];
        let model = part.model.as_ref()?;
        let mut out = Vec::new();
        for pin in &part.comp.pins {
            let Some(role) = self.role(i, pin) else {
                continue;
            };
            if !is_supply_role(&role, model.kind) {
                continue;
            }
            let Some(g) = self.group(pin.net) else {
                continue;
            };
            if self.groups[g].ground || self.unknown[g] {
                return None;
            }
            out.push(g);
        }
        out.sort_unstable();
        out.dedup();
        (!out.is_empty()).then_some(out)
    }
}

fn pin_label(pin: &Pin) -> String {
    if pin.function.trim().is_empty() {
        format!("pad {}", pin.number)
    } else {
        format!("{} (pad {})", pin.function.trim(), pin.number)
    }
}

/// One signal pin of an off part and what keeps it high.
struct Backfeed {
    net: i64,
    role: String,
    pulls: Vec<(String, String, f64)>,
    drivers: Vec<String>,
}

/// One state of the board: `switch` open.
struct State<'g> {
    graph: &'g Graph<'g>,
    all: Vec<bool>,
    on: Vec<bool>,
}

impl State<'_> {
    fn off(&self, g: usize) -> bool {
        self.all[g] && !self.on[g]
    }

    /// The part is powered in this state: every supply it names is on.
    fn part_on(&self, i: usize) -> bool {
        self.graph
            .supply_groups(i)
            .is_some_and(|gs| gs.iter().all(|&g| self.on[g]))
    }

    /// The part has every supply off in this state.
    fn part_off(&self, i: usize) -> bool {
        self.graph
            .supply_groups(i)
            .is_some_and(|gs| gs.iter().all(|&g| self.off(g)))
    }

    fn rails_of(&self, i: usize) -> Vec<String> {
        let mut names: Vec<String> = self
            .graph
            .supply_groups(i)
            .unwrap_or_default()
            .into_iter()
            .map(|g| self.graph.groups[g].name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Pull-ups to a rail that stays on, and parts that drive the net while on.
    fn held_high(&self, net: i64, owner: usize) -> (Vec<(String, String, f64)>, Vec<String>) {
        let graph = self.graph;
        let (mut pulls, mut drivers) = (Vec::new(), Vec::new());
        for &(i, j) in graph.net_pins.get(&net).into_iter().flatten() {
            if i == owner {
                continue;
            }
            let part = &graph.parts[i];
            let comp = part.comp;
            let nets = nets_of(comp);
            if nets.len() == 2 && is_plain_resistor(comp) {
                let Some(far) = nets.into_iter().find(|&n| n != net) else {
                    continue;
                };
                let (Some(g), Some(o)) = (graph.group(Some(far)), ohms(comp)) else {
                    continue;
                };
                if graph.groups[g].rail && !graph.unknown[g] && self.on[g] && o > 0.0 {
                    pulls.push((comp.reference.clone(), graph.groups[g].name.clone(), o));
                }
                continue;
            }
            let pin = &comp.pins[j];
            let kind = pin.kind.to_ascii_lowercase();
            let pushes = matches!(kind.as_str(), "output" | "out");
            if pushes && part.model.is_some() && self.part_on(i) {
                drivers.push(format!("{} pad {}", comp.reference, pin.number));
            }
        }
        pulls.sort_by(|a, b| a.0.cmp(&b.0));
        pulls.dedup_by(|a, b| a.0 == b.0);
        drivers.sort();
        drivers.dedup();
        (pulls, drivers)
    }

    /// Every signal pin of off part `i` held high from a rail that is on.
    fn backfeeds(&self, i: usize) -> Vec<Backfeed> {
        let graph = self.graph;
        let part = &graph.parts[i];
        let Some(model) = part.model.as_ref() else {
            return Vec::new();
        };
        let mut out: BTreeMap<i64, Backfeed> = BTreeMap::new();
        for pin in &part.comp.pins {
            let Some(role) = graph.role(i, pin) else {
                continue;
            };
            if is_power_role(&role, model.kind)
                || role.starts_with("nc")
                || role.starts_with("gnd")
                || declared_pin_ceiling_v(&model.envelope, &role).is_some()
            {
                continue;
            }
            let Some(net) = pin.net.filter(|&n| n != 0) else {
                continue;
            };
            let Some(g) = graph.group(Some(net)) else {
                continue;
            };
            if graph.groups[g].ground || graph.groups[g].rail {
                continue;
            }
            let (pulls, drivers) = self.held_high(net, i);
            if pulls.is_empty() && drivers.is_empty() {
                continue;
            }
            out.entry(net).or_insert(Backfeed {
                net,
                role,
                pulls,
                drivers,
            });
        }
        out.into_values().collect()
    }

    /// Distinct powered bus devices on `net`, apart from `except`.
    fn bus_devices(&self, net: i64, except: Option<usize>) -> Vec<String> {
        let graph = self.graph;
        let mut refs = BTreeSet::new();
        for &(i, _) in graph.net_pins.get(&net).into_iter().flatten() {
            let part = &graph.parts[i];
            if Some(i) == except
                || !part
                    .model
                    .as_ref()
                    .is_some_and(|m| is_receiver_kind(m.kind))
                || !self.part_on(i)
            {
                continue;
            }
            refs.insert(part.comp.reference.clone());
        }
        refs.into_iter().collect()
    }
}

fn list(items: &[String]) -> String {
    items.join(", ")
}

fn pulls_text(pulls: &[(String, String, f64)]) -> String {
    let parts: Vec<String> = pulls
        .iter()
        .map(|(r, rail, o)| format!("{r} ({}) to {rail}", super::regulator_enable::ohms(*o)))
        .collect();
    parts.join(", ")
}

/// All findings of the power-state checks over `board`.
#[must_use]
pub fn power_state_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let graph = Graph::build(board, lib);
    let mut report = NetLintReport::default();
    if graph.switches.is_empty() {
        return report;
    }
    let all = graph.reach(None);
    for (s, switch) in graph.switches.iter().enumerate() {
        let state = State {
            graph: &graph,
            all: all.clone(),
            on: graph.reach(Some(s)),
        };
        if !state.all.iter().zip(&state.on).any(|(a, o)| *a && !*o) {
            continue;
        }
        // The switch only counts when what drives it is itself powered.
        if !state.part_on(switch.driver_part) {
            continue;
        }
        bus_pullups_off(&state, switch, &mut report);
        for i in 0..graph.parts.len() {
            if !graph.parts[i].fitted
                || !graph.parts[i]
                    .model
                    .as_ref()
                    .is_some_and(|m| is_receiver_kind(m.kind))
                || i == switch.driver_part
                || !state.part_off(i)
            {
                continue;
            }
            device_findings(&state, switch, i, &mut report);
        }
    }
    report
}

/// What one set of off rails takes down: (nets, pull-up resistors, devices).
type BusGroup = (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>);

/// An I2C bus whose every pull-up sits on a rail the switch turns off, with
/// two or more powered devices still on it.
fn bus_pullups_off(state: &State<'_>, switch: &Switch, report: &mut NetLintReport) {
    let graph = state.graph;
    let mut found: BTreeMap<Vec<String>, BusGroup> = BTreeMap::new();
    for net in &graph.board.nets {
        if net.id == 0 || !is_i2c_net(&net.name) {
            continue;
        }
        let mut rails = BTreeSet::new();
        let mut resistors = BTreeSet::new();
        let mut decided = true;
        for &(i, _) in graph.net_pins.get(&net.id).into_iter().flatten() {
            let comp = graph.parts[i].comp;
            let nets = nets_of(comp);
            if nets.len() != 2 || !is_plain_resistor(comp) {
                continue;
            }
            let Some(far) = nets.into_iter().find(|&n| n != net.id) else {
                continue;
            };
            let Some(g) = graph.group(Some(far)) else {
                continue;
            };
            if graph.groups[g].ground || !graph.groups[g].rail {
                continue;
            }
            if graph.unknown[g] || !(state.on[g] || state.off(g)) {
                decided = false;
                break;
            }
            if !state.off(g) {
                // A pull-up that stays on keeps the bus high.
                decided = false;
                break;
            }
            rails.insert(graph.groups[g].name.clone());
            resistors.insert(comp.reference.clone());
        }
        if !decided || resistors.is_empty() {
            continue;
        }
        let devices = state.bus_devices(net.id, None);
        if devices.len() < 2 {
            continue;
        }
        let entry = found.entry(rails.into_iter().collect()).or_default();
        entry.0.insert(net.name.clone());
        entry.1.extend(resistors);
        entry.2.extend(devices);
    }
    for (rails, (nets, resistors, devices)) in found {
        let nets: Vec<String> = nets.into_iter().collect();
        let (resistors, devices): (Vec<String>, Vec<String>) = (
            resistors.into_iter().collect(),
            devices.into_iter().collect(),
        );
        let mut refs = vec![switch.part.clone()];
        refs.extend(resistors.iter().cloned());
        refs.extend(devices.iter().cloned());
        let mut nets_out = nets.clone();
        nets_out.extend(rails.iter().cloned());
        report.findings.push(LintFinding {
            check: LintCheck::PowerDomainBusLockup,
            severity: Severity::High,
            message: format!(
                "I2C bus {} has all its pull-ups ({}) on {}, which {} turns off (driven by {}), \
                 while {} stay powered on the bus: with the rail off nothing holds the lines \
                 high, so the powered devices cannot talk, and each pull-up's own off rail \
                 leaves the lines floating.",
                nets.join("/"),
                list(&resistors),
                rails.join("/"),
                switch.what,
                switch.driver,
                list(&devices),
            ),
            refs,
            nets: nets_out,
        });
    }
}

/// The finding for an unpowered device clamping I2C buses that `busy` names.
fn bus_clamp_finding(
    state: &State<'_>,
    switch: &Switch,
    i: usize,
    busy: &[&Backfeed],
    who: &str,
    report: &mut NetLintReport,
) {
    let graph = state.graph;
    let part = &graph.parts[i];
    let rails = state.rails_of(i);
    let nets: Vec<String> = busy.iter().map(|h| graph.net_name(h.net)).collect();
    let mut users: BTreeSet<String> = BTreeSet::new();
    let mut resistors: BTreeSet<String> = BTreeSet::new();
    let mut rails_up: BTreeSet<String> = BTreeSet::new();
    for h in busy {
        users.extend(state.bus_devices(h.net, Some(i)));
        for (r, rail, _) in &h.pulls {
            resistors.insert(r.clone());
            rails_up.insert(rail.clone());
        }
    }
    let users: Vec<String> = users.into_iter().collect();
    let mut refs = vec![part.comp.reference.clone(), switch.part.clone()];
    refs.extend(resistors.iter().cloned());
    let mut nets_out = nets.clone();
    nets_out.extend(rails_up.iter().cloned());
    report.findings.push(LintFinding {
        check: LintCheck::PowerDomainBusLockup,
        severity: Severity::High,
        message: format!(
            "{who} has every supply ({}) off whenever {} turns it off (driven by {}), but \
             sits on I2C bus {} that {} still use, pulled up to {}: the unpowered chip's \
             pin clamp to its dead supply holds the lines near a diode drop, which every \
             device on the bus reads as a permanent low (the bus locks up).",
            list(&rails),
            switch.what,
            switch.driver,
            nets.join("/"),
            list(&users),
            rails_up.into_iter().collect::<Vec<_>>().join("/"),
        ),
        refs,
        nets: nets_out,
    });
}

/// Back-feed and bus-clamp findings for off part `i`.
fn device_findings(state: &State<'_>, switch: &Switch, i: usize, report: &mut NetLintReport) {
    let graph = state.graph;
    let part = &graph.parts[i];
    let hits = state.backfeeds(i);
    if hits.is_empty() {
        return;
    }
    let rails = state.rails_of(i);
    let value = part.comp.value.trim();
    let who = if value.is_empty() {
        part.comp.reference.clone()
    } else {
        format!("{} ({value})", part.comp.reference)
    };
    let (bus, other): (Vec<&Backfeed>, Vec<&Backfeed>) = hits.iter().partition(|h| {
        is_i2c_net(&graph.net_name(h.net)) && !state.bus_devices(h.net, Some(i)).is_empty()
    });
    // An unpowered device clamping a bus the others still use.
    let busy: Vec<&Backfeed> = bus
        .iter()
        .copied()
        .filter(|h| state.bus_devices(h.net, Some(i)).len() >= 2)
        .collect();
    let rest_of_bus: Vec<&Backfeed> = bus
        .iter()
        .copied()
        .filter(|h| !busy.iter().any(|b| b.net == h.net))
        .collect();
    if !busy.is_empty() {
        bus_clamp_finding(state, switch, i, &busy, &who, report);
    }
    let backfed: Vec<&Backfeed> = other.into_iter().chain(rest_of_bus).collect();
    if backfed.is_empty() {
        return;
    }
    let weak_only = backfed
        .iter()
        .all(|h| h.drivers.is_empty() && h.pulls.iter().all(|(_, _, o)| *o >= WEAK_PULLUP_OHMS));
    let mut refs = vec![part.comp.reference.clone(), switch.part.clone()];
    let mut nets: Vec<String> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    for h in &backfed {
        let net = graph.net_name(h.net);
        let mut how = Vec::new();
        if !h.pulls.is_empty() {
            how.push(format!("pulled up through {}", pulls_text(&h.pulls)));
        }
        if !h.drivers.is_empty() {
            how.push(format!("driven by {}", list(&h.drivers)));
        }
        clauses.push(format!("{} (net {net}) {}", h.role, how.join(" and ")));
        refs.extend(h.pulls.iter().map(|p| p.0.clone()));
        refs.extend(
            h.drivers
                .iter()
                .filter_map(|d| d.split_whitespace().next().map(str::to_string)),
        );
        nets.push(net);
        nets.extend(h.pulls.iter().map(|p| p.1.clone()));
    }
    nets.sort();
    nets.dedup();
    report.findings.push(LintFinding {
        check: LintCheck::PowerDomainBackfeed,
        severity: if weak_only {
            Severity::Low
        } else {
            Severity::Medium
        },
        message: format!(
            "{who} has every supply ({}) off whenever {} turns it off (driven by {}), but pin {} \
             stays held high from a rail that is still on: the pin's input protection clamp \
             conducts into the unpowered chip and half-wakes it (it sits near a diode drop \
             instead of off, so it can brown out, never reset, and draw from the line). \
             Unless the datasheet states the pin tolerates a voltage with the supply off.",
            list(&rails),
            switch.what,
            switch.driver,
            clauses.join("; "),
        ),
        refs: dedup_keep_order(refs),
        nets,
    });
}

fn dedup_keep_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    type Pads<'a> = &'a [(&'a str, &'a str)];

    /// A `KiCad` layout from `(reference, value, footprint, [(pad, net)])` rows.
    fn layout(parts: &[(&str, &str, &str, Pads<'_>)]) -> String {
        let mut nets: Vec<&str> = Vec::new();
        for (_, _, _, pads) in parts {
            for (_, n) in *pads {
                if !nets.contains(n) {
                    nets.push(n);
                }
            }
        }
        let id = |n: &str| nets.iter().position(|x| *x == n).unwrap() + 1;
        let mut out =
            String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n  (net 0 \"\")\n");
        for n in &nets {
            let _ = writeln!(out, "  (net {} \"{n}\")", id(n));
        }
        for (reference, value, footprint, pads) in parts {
            let _ = write!(
                out,
                "  (module {footprint} (layer F.Cu) (at 100 100)\n    (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n    (fp_text value {value} (at 0 2) (layer F.Fab))\n"
            );
            for (pad, net) in *pads {
                let _ = writeln!(
                    out,
                    "    (pad {pad} smd rect (at 0 0) (net {} \"{net}\"))",
                    id(net)
                );
            }
            out += "  )\n";
        }
        out + ")"
    }

    const SOT5: &str = "Package_TO_SOT_SMD:SOT-23-5";
    const R: &str = "Resistor_SMD:R_0603_1608Metric";

    /// Test-only: a pad list that lives for the whole run.
    fn pads(v: Vec<(&'static str, &'static str)>) -> Pads<'static> {
        Box::leak(v.into_boxed_slice())
    }

    type Row<'a> = (&'a str, &'a str, &'a str, Pads<'a>);

    /// A 5 V header feeds an always-on 3.3 V regulator (U3), and a second one
    /// (U2) whose enable is `en_net` and whose output `+3V3_S` powers the
    /// AT24CS01 U4. The ESP32 U1 runs from `+3V3` and drives GPIO0 onto
    /// `/SENS_EN`. R1 pulls U4's WP pad up to `wp_pull`.
    fn base(en_net: &'static str, wp_pull: &'static str) -> Vec<Row<'static>> {
        vec![
            (
                "J1",
                "Conn_01x02",
                "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
                &[("1", "+5V"), ("2", "GND")],
            ),
            (
                "U3",
                "AP2112K-3.3",
                SOT5,
                &[("1", "+5V"), ("2", "GND"), ("3", "+5V"), ("5", "+3V3")],
            ),
            (
                "U2",
                "AP2112K-3.3",
                SOT5,
                pads(vec![
                    ("1", "+5V"),
                    ("2", "GND"),
                    ("3", en_net),
                    ("5", "+3V3_S"),
                ]),
            ),
            (
                "U1",
                "ESP-WROOM-32",
                "RF_Module:ESP32-WROOM-32",
                &[("1", "GND"), ("2", "+3V3"), ("25", "/SENS_EN")],
            ),
            (
                "U4",
                "AT24CS01-STUM",
                SOT5,
                &[("2", "GND"), ("4", "+3V3_S"), ("5", "/WP")],
            ),
            ("R1", "10k", R, pads(vec![("1", "/WP"), ("2", wp_pull)])),
        ]
    }

    fn run(rows: &[Row<'_>]) -> NetLintReport {
        let board = ExtractedBoard::from_kicad_pcb(&layout(rows)).expect("parse synthetic board");
        power_state_lint(&board, &ModelLibrary::builtin())
    }

    fn of(r: &NetLintReport, check: LintCheck) -> Vec<&LintFinding> {
        r.findings.iter().filter(|f| f.check == check).collect()
    }

    /// Faulty side: U4 loses its supply when GPIO0 drops U2's enable, while its
    /// WP pad stays pulled up to the always-on rail.
    #[test]
    fn pullup_to_a_rail_that_stays_on_back_feeds_the_off_part() {
        let r = run(&base("/SENS_EN", "+3V3"));
        let f = of(&r, LintCheck::PowerDomainBackfeed);
        assert_eq!(f.len(), 1, "one back-feed finding: {:?}", r.findings);
        assert_eq!(f[0].severity, Severity::Medium);
        for want in ["U4", "U2", "R1"] {
            assert!(
                f[0].refs.contains(&want.to_string()),
                "{want} in {:?}",
                f[0].refs
            );
        }
        assert!(f[0].message.contains("+3V3_S") && f[0].message.contains("U1"));
    }

    /// Legal side: the same pull-up on the switched rail goes off with the part.
    #[test]
    fn pullup_on_the_switched_rail_is_silent() {
        let r = run(&base("/SENS_EN", "+3V3_S"));
        assert!(
            of(&r, LintCheck::PowerDomainBackfeed).is_empty(),
            "{:?}",
            r.findings
        );
    }

    /// Legal side: an enable tied to the input rail is no switch at all.
    #[test]
    fn an_enable_no_gpio_drives_is_not_a_switch() {
        let r = run(&base("+5V", "+3V3"));
        assert!(
            of(&r, LintCheck::PowerDomainBackfeed).is_empty(),
            "{:?}",
            r.findings
        );
    }

    /// A weak pull-up is only a note.
    #[test]
    fn a_megohm_pullup_is_low() {
        let mut rows = base("/SENS_EN", "+3V3");
        rows[5].1 = "1M";
        let r = run(&rows);
        let f = of(&r, LintCheck::PowerDomainBackfeed);
        assert_eq!(f.len(), 1, "{:?}", r.findings);
        assert_eq!(f[0].severity, Severity::Low);
    }

    /// Abstention: a rail with no traced source is never judged on or off.
    #[test]
    fn a_rail_nothing_traces_a_source_to_abstains() {
        let mut rows = base("/SENS_EN", "+3V3");
        rows.remove(1); // U3: nothing feeds +3V3 any more
        let r = run(&rows);
        assert!(
            r.findings.iter().all(|f| !matches!(
                f.check,
                LintCheck::PowerDomainBackfeed | LintCheck::PowerDomainBusLockup
            )),
            "{:?}",
            r.findings
        );
    }

    /// An unpowered device on an I2C bus two others use clamps it.
    #[test]
    fn an_unpowered_device_clamping_a_used_bus_locks_it_up() {
        let mut rows = base("/SENS_EN", "+3V3_S");
        rows[3].3 = &[
            ("1", "GND"),
            ("2", "+3V3"),
            ("25", "/SENS_EN"),
            ("14", "/SDA"),
        ];
        rows[4].3 = &[("2", "GND"), ("4", "+3V3_S"), ("3", "/SDA"), ("5", "/WP")];
        rows.push((
            "U5",
            "BME280",
            "Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering",
            &[("1", "GND"), ("8", "+3V3"), ("6", "+3V3"), ("3", "/SDA")],
        ));
        rows.push(("R2", "4.7k", R, &[("1", "/SDA"), ("2", "+3V3")]));
        let r = run(&rows);
        let f = of(&r, LintCheck::PowerDomainBusLockup);
        assert_eq!(f.len(), 1, "lock-up expected: {:?}", r.findings);
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].refs.contains(&"U4".to_string()));
    }

    /// Pull-ups on the gated rail with powered devices still on the bus.
    #[test]
    fn pullups_on_the_gated_rail_leave_the_bus_floating() {
        let mut rows = base("/SENS_EN", "+3V3_S");
        rows[3].3 = &[
            ("1", "GND"),
            ("2", "+3V3"),
            ("25", "/SENS_EN"),
            ("14", "/SDA"),
        ];
        rows.push((
            "U5",
            "BME280",
            "Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering",
            &[("1", "GND"), ("8", "+3V3"), ("6", "+3V3"), ("3", "/SDA")],
        ));
        rows.push(("R2", "4.7k", R, &[("1", "/SDA"), ("2", "+3V3_S")]));
        let r = run(&rows);
        let f = of(&r, LintCheck::PowerDomainBusLockup);
        assert_eq!(f.len(), 1, "lock-up expected: {:?}", r.findings);
        assert!(f[0].refs.contains(&"R2".to_string()));
    }
}
