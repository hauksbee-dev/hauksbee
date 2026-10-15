//! Rating claims whose current path runs into a part the board leaves without
//! a working model.
//!
//! The DC stress pass ([`super::dc_stress`]) reports a part only when the
//! solved current breaks one of its ratings, so a part it does not report
//! reads as inside them. That reading is empty when the current path runs
//! into a part the binder left OPEN (an unresolved load, a resistor whose
//! value is not a number such as "PROG", a shunt with no usable value) or
//! modelled with a guessed setting (a trimmer whose wiper position the board
//! does not give): the solver carried no current, or an invented one, through
//! it, and the rating was never really tested.
//!
//! Each such claim is reported as an abstention ([`ABSTENTION_MARKER`]): a
//! note saying the rating is unverified, not met, that stays out of the
//! actionable tally. Its nets put the open parts' own evidence on its path.
//!
//! Which claims: every rated driver pin (a logic or MCU output), reached
//! directly or through one series resistor, and every current-rated junction
//! or channel (diode, LED, transistor) sharing a signal net with the open
//! part. Rails and ground are not paths: every part touches them, and neither
//! is a net with more than four fitted parts on it (a bus or
//! a shared control line, not a series load). A connector is never the open
//! part: it is the board's edge, whose far side no board file describes. A
//! resistor's own wattage is not qualified, so the note does not repeat on
//! every passive beside an unmodelled IC.
//!
//! Parts that run into the same open parts under the same rating share one
//! note, so sixteen LEDs behind one unmodelled LED driver read as one gap.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_bind::binder::{is_ground, power_rail_voltage, BoundBoard};
use hauksbee_bind::stress::strip_unit_suffix;
use hauksbee_bind::{BindOutcome, BindRow};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, Component, ExtractedBoard, LintCheck, LintFinding, Severity,
    ABSTENTION_MARKER,
};
use hauksbee_ir::{DeviceId, NodeId};
use hauksbee_models::ComponentKind;

/// The leading letters of a reference designator, upper-cased ("RV" of RV3).
fn prefix(reference: &str) -> String {
    reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// A trimmer or potentiometer, by designator or footprint.
fn is_pot(comp: &Component) -> bool {
    let fp = format!("{} {}", comp.footprint, comp.lib_id).to_ascii_lowercase();
    matches!(
        prefix(&comp.reference).as_str(),
        "RV" | "VR" | "POT" | "TRIM"
    ) || ["potentiometer", "trimmer", "trimpot"]
        .iter()
        .any(|w| fp.contains(w))
}

/// Why `comp` carries no trustworthy current, if it does not.
fn open_reason(comp: &Component, row: &BindRow) -> Option<&'static str> {
    if matches!(row.outcome, BindOutcome::Unresolved { .. }) {
        return Some("no model, left open");
    }
    let named = row
        .model_id
        .as_deref()
        .is_some_and(|m| !m.ends_with("_fallback"));
    (is_pot(comp) && !named).then_some("a trimmer whose wiper setting the board does not give")
}

/// A rail the binder holds at a stated voltage, or ground: not a current path
/// between two parts. A supply that states no voltage (`VBAT`) is not held, so
/// a part feeding it is still on the path.
fn is_rail(net: &str) -> bool {
    is_ground(net) || power_rail_voltage(net).is_some()
}

/// Most fitted parts a net may carry and still read as one part's load path
/// rather than a bus, a reset line or another shared signal.
const MAX_PATH_NET_PARTS: usize = 4;

/// Most routes a note spells out before it counts the rest.
const MAX_ROUTES_SHOWN: usize = 3;

/// Most parts a note names before it counts the rest.
const MAX_PARTS_SHOWN: usize = 6;

/// One part's qualified claims: what was claimed, the open parts, the routes
/// and the nets.
type PartClaims = (
    BTreeSet<String>,
    BTreeSet<String>,
    Vec<String>,
    BTreeSet<String>,
);

/// The parts that share a note, the first one's routes, and their nets.
type Group = (Vec<String>, Vec<String>, BTreeSet<String>);

/// One qualified claim: the part, what was claimed, the open part and the
/// route to it.
struct Claim {
    part: String,
    what: String,
    open: String,
    route: String,
    nets: Vec<String>,
}

/// The board's fitted parts, their bind rows and net incidence.
struct Topology<'a> {
    rows: HashMap<&'a str, &'a BindRow>,
    comps: HashMap<&'a str, &'a Component>,
    /// Net name -> fitted parts with a pin on it.
    on_net: HashMap<String, BTreeSet<&'a str>>,
    /// Part -> the distinct nets of its connected pins.
    nets_of: HashMap<&'a str, Vec<String>>,
}

impl<'a> Topology<'a> {
    fn new(board: &'a ExtractedBoard, bound: &'a BoundBoard) -> Self {
        let names: HashMap<i64, &str> =
            board.nets.iter().map(|n| (n.id, n.name.as_str())).collect();
        let mut on_net: HashMap<String, BTreeSet<&str>> = HashMap::new();
        let mut nets_of: HashMap<&str, Vec<String>> = HashMap::new();
        let mut comps = HashMap::new();
        for comp in &board.components {
            if AssemblyState::of(comp).fitted().is_none() {
                continue;
            }
            comps.insert(comp.reference.as_str(), comp);
            let nets = nets_of.entry(comp.reference.as_str()).or_default();
            for net in comp
                .pins
                .iter()
                .filter_map(|p| p.net.and_then(|id| names.get(&id)))
            {
                if !nets.iter().any(|n| n == net) {
                    nets.push((*net).to_string());
                }
                on_net
                    .entry((*net).to_string())
                    .or_default()
                    .insert(comp.reference.as_str());
            }
        }
        let rows = bound
            .report
            .rows
            .iter()
            .map(|r| (r.reference.as_str(), r))
            .collect();
        Self {
            rows,
            comps,
            on_net,
            nets_of,
        }
    }

    /// The open parts on `net` other than `except`, as "R1 (PROG, why)".
    fn open_on(&self, net: &str, except: &str) -> Vec<String> {
        (self.on_net.get(net).into_iter().flatten())
            .filter(|&&r| r != except)
            .filter_map(|&r| {
                let comp = self.comps.get(r)?;
                if is_connector_like(comp) {
                    return None;
                }
                let why = open_reason(comp, self.rows.get(r)?)?;
                let value = &comp.value;
                Some(if value.trim().is_empty() {
                    format!("{r} ({why})")
                } else {
                    format!("{r} ({value}, {why})")
                })
            })
            .collect()
    }

    /// A bound plain resistor (`R` designator, two nets, an analog model).
    fn series_resistor(&self, reference: &str) -> Option<&[String]> {
        let row = self.rows.get(reference)?;
        let nets = self.nets_of.get(reference)?;
        (prefix(reference) == "R"
            && nets.len() == 2
            && matches!(row.outcome, BindOutcome::Analog { .. }))
        .then_some(nets.as_slice())
    }

    /// Open parts a driver on `net` reaches, directly or through one series
    /// resistor: `(open part, route, nets)` per open part.
    fn reached_from_pin(&self, net: &str, part: &str) -> Vec<(String, String, Vec<String>)> {
        let mut out: Vec<(String, String, Vec<String>)> = (self.open_on(net, part).into_iter())
            .map(|o| {
                let route = format!("drives {o} on net \"{net}\"");
                (o, route, vec![net.to_string()])
            })
            .collect();
        for &r in self.on_net.get(net).into_iter().flatten() {
            let Some(nets) = self.series_resistor(r) else {
                continue;
            };
            let Some(far) = nets.iter().find(|n| *n != net && !is_rail(n)) else {
                continue;
            };
            for o in self
                .open_on(far, r)
                .into_iter()
                .filter(|o| !o.starts_with(&format!("{part} ")))
            {
                let route = format!("drives {o} through {r} (net \"{net}\" to \"{far}\")");
                out.push((o, route, vec![net.to_string(), far.clone()]));
            }
        }
        out
    }
}

/// Every rating claim the DC stress pass would read as met whose current path
/// runs into an open part, as one abstention per rated part.
#[must_use]
pub fn open_path_qualifiers(board: &ExtractedBoard, bound: &BoundBoard) -> Vec<LintFinding> {
    let topo = Topology::new(board, bound);
    let net_of: HashMap<NodeId, &str> = (bound.net_nodes.iter())
        .map(|(name, node)| (*node, name.as_str()))
        .collect();
    // Driver Vsource -> the net its pin drives.
    let mut pin_net: HashMap<DeviceId, NodeId> = HashMap::new();
    for d in &bound.digital {
        pin_net.extend(d.drivers.values().map(|drv| (drv.vsource, drv.net)));
    }
    for m in &bound.mcus {
        pin_net.extend(m.gpio_drivers.values().map(|drv| (drv.vsource, drv.net)));
    }
    let mut claims: Vec<Claim> = Vec::new();
    for meta in &bound.device_meta {
        if let Some((part, pin)) = meta.reference.split_once(':') {
            if meta.ratings.max_pin_current_a.is_none() {
                continue;
            }
            let Some(net) = (pin_net.get(&meta.device)).and_then(|n| net_of.get(n)) else {
                continue;
            };
            if is_rail(net) {
                continue;
            }
            for (open, route, nets) in topo.reached_from_pin(net, part) {
                claims.push(Claim {
                    part: part.to_string(),
                    what: "pin drive-current rating".to_string(),
                    open,
                    route: format!("pin {pin} {route}"),
                    nets,
                });
            }
            continue;
        }
        let current_rated = matches!(
            meta.kind,
            ComponentKind::Diode
                | ComponentKind::BjtNpn
                | ComponentKind::BjtPnp
                | ComponentKind::Nmos
                | ComponentKind::Pmos
        ) && (meta.ratings.max_current_a.is_some()
            || meta.ratings.max_power_w.is_some());
        if !current_rated {
            continue;
        }
        let part = strip_unit_suffix(&meta.reference);
        let nets = (topo.nets_of.get(part).into_iter().flatten())
            .filter(|n| !is_rail(n))
            .filter(|n| {
                topo.on_net
                    .get(*n)
                    .is_some_and(|p| p.len() <= MAX_PATH_NET_PARTS)
            });
        for net in nets {
            for open in topo.open_on(net, part) {
                claims.push(Claim {
                    part: part.to_string(),
                    what: "current and power ratings".to_string(),
                    route: format!("{open} shares net \"{net}\" with it"),
                    open,
                    nets: vec![net.clone()],
                });
            }
        }
    }
    notes(claims)
}

/// One part's claims folded together, then parts with the same ratings
/// claimed and the same open parts folded into one note.
fn notes(claims: Vec<Claim>) -> Vec<LintFinding> {
    let mut by_part: BTreeMap<String, PartClaims> = BTreeMap::new();
    for c in claims {
        let entry = by_part.entry(c.part).or_default();
        entry.0.insert(c.what);
        entry.1.insert(c.open);
        if !entry.2.contains(&c.route) {
            entry.2.push(c.route);
        }
        entry.3.extend(c.nets);
    }
    // (ratings claimed, open parts) -> the parts, the first one's routes, nets.
    let mut groups: BTreeMap<(BTreeSet<String>, BTreeSet<String>), Group> = BTreeMap::new();
    for (part, (what, open, routes, nets)) in by_part {
        let group = groups.entry((what, open)).or_default();
        if group.0.is_empty() {
            group.1 = routes;
        }
        group.0.push(part);
        group.2.extend(nets);
    }
    groups
        .into_iter()
        .map(|((what, _), (parts, routes, nets))| {
            let more = |shown: usize, total: usize| {
                if total > shown {
                    format!(" and {} more", total - shown)
                } else {
                    String::new()
                }
            };
            let who = format!(
                "{}{}",
                parts[..parts.len().min(MAX_PARTS_SHOWN)].join(", "),
                more(MAX_PARTS_SHOWN, parts.len())
            );
            let (subject, path) = if parts.len() == 1 {
                (format!("{who}'s"), "its")
            } else {
                (format!("{who}: the"), "their")
            };
            LintFinding {
                check: LintCheck::DcOverstress,
                severity: Severity::Low,
                message: format!(
                    "{subject} {} check{ABSTENTION_MARKER}: {path} current path runs into a part \
                     with no working model ({}{}), so the current there was not simulated and the \
                     rating is unverified, not met",
                    what.into_iter().collect::<Vec<_>>().join(" and "),
                    routes[..routes.len().min(MAX_ROUTES_SHOWN)].join("; "),
                    more(MAX_ROUTES_SHOWN, routes.len()),
                ),
                refs: parts,
                nets: nets.into_iter().collect(),
            }
        })
        .collect()
}
