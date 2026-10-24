//! Shadow simulation for three hand checks, and the parity ledger against them.
//!
//! `fet_orientation`, `back_power` and `power_states` each read a fault from
//! topology: a FET wired so its body diode defeats it, a signal pin pulled
//! above its own supply, a part back-fed through its clamps when its rail is
//! switched off. All three are junction physics. With every junction present
//! (the binder's body diodes, and the pin clamps and quiescent draw of
//! [`hauksbee_bind::junctions`]), the same faults show up as solved currents
//! and voltages in a scenario:
//!
//! - **Rest** (power-up state, every junction stamped): a pin clamp conducting
//!   into its own live supply is a pin held above its supply (`back_power`).
//! - **Each firmware-switchable switch off**, one at a time from rest: a FET
//!   whose gate an MCU or logic output drives, with its channel off, or a
//!   regulator whose enable such an output drives, opened. A FET still
//!   carrying current through its body diode there cannot switch its load off
//!   (`fet_orientation`). A part whose supply that switch moved and that now
//!   draws its supply through its own pin clamps is back-fed, and an I2C line
//!   that was high at rest and now sits low under a powered device's VIL is a
//!   locked bus (`power_states`).
//! - **Input supply reversed**: the board's inputs are read from topology, as
//!   the connector and battery pads whose nets a supplied rail reaches through
//!   wires (fuses, beads, inductors) and the FETs on at rest. Each is driven
//!   at minus its rest level, with the binder's own supply legs disconnected
//!   and regulators off, and the rest of the board solves around it. A FET the
//!   rest state holds on with a gate no firmware drives is a reverse-polarity
//!   FET; it does not block when its body diode carries a load's current, or
//!   when the diode is forward-biased and the protected side follows the
//!   reversal below ground (an unloaded rail passes it on with next to no
//!   current). A resistor across the channel that carries more than the diode
//!   passes the reversal whichever way the FET faces, so it is not blamed
//!   (`fet_orientation`).
//!
//! What picks the scenarios (which switch firmware can open, which pads are
//! inputs) is read from the board; what decides a finding is only the solve.
//! Nothing here enters a verdict: [`parity_ledger`] lays the sim findings
//! beside the hand checks' as (check, part) pairs, so a hand check is retired
//! only when the ledger shows the solve finds what it finds. The shadow
//! abstains, with the reason kept in [`Shadow::why_not`] (or
//! [`Shadow::unjudged`] for the whole board), wherever the solve cannot speak:
//! a board no net names a voltage for, a state that does not solve, a FET gate
//! no model sets, a FET whose channel nets the rest solve leaves dead, a FET
//! with a supply pad on each side. A hand finding there is an `abstained`
//! ledger row, not a hand-only miss.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use hauksbee_bind::binder::{bind_board, BoundBoard};
use hauksbee_bind::control_inputs::{hold_off, undetermined_gates};
use hauksbee_bind::junctions::{
    clamp_current, dc_with_junction_homotopy, junction_current, stamp_ic_junctions, IcJunctions,
    LIVE_SUPPLY_V,
};
use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::{Device, NodeId, SourceKind};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelLibrary};
use hauksbee_solve::{SolverOptions, Workspace};

use super::bus_loading::is_i2c_net;
use super::node_drive::Index;

/// Body-diode forward current that counts as the diode conducting (A): well
/// above a junction's leakage, well below any load a switch carries.
pub const BODY_DIODE_A: f64 = 1e-4;
/// Clamp forward current that counts as the clamp conducting (A): a thousand
/// times a clamp's leakage at its datasheet margin.
pub const CLAMP_A: f64 = 1e-6;
/// A supply moved by more than this (V) between rest and a switch-off state is
/// one the switch feeds.
const MOVED_V: f64 = 0.2;
/// Switches tried per board, so a large board's cost stays bounded.
const MAX_SWITCHES: usize = 48;
/// Logic sweeps per scenario, as `dc_stress` allows.
const SETTLE_SWEEPS: usize = 8;
/// A series element at or below this between an input and a FET is a wire (a
/// fuse, a bead, a sense resistor), in ohms.
const WIRE_OHMS: f64 = 10.0;
/// A disconnected supply leg's series resistance (ohms).
const OPEN_OHMS: f64 = 1e12;

/// The hand check a sim finding stands in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SimTarget {
    FetOrientation,
    BackPower,
    PowerStates,
}

impl SimTarget {
    /// The hand check's module name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FetOrientation => "fet_orientation",
            Self::BackPower => "back_power",
            Self::PowerStates => "power_states",
        }
    }
}

/// One finding the solve produced.
#[derive(Clone, Debug)]
pub struct SimFinding {
    pub target: SimTarget,
    /// The part the hand check would name first.
    pub part: String,
    pub nets: Vec<String>,
    /// The scenario it showed in.
    pub scenario: String,
    pub message: String,
}

/// What the shadow pass found and where it could not speak.
#[derive(Clone, Debug, Default)]
pub struct Shadow {
    pub findings: Vec<SimFinding>,
    /// Part -> why the solve said nothing about it (an abstention, not a pass).
    pub why_not: BTreeMap<String, String>,
    /// Board-wide notes (a scenario that did not solve).
    pub notes: Vec<String>,
    /// The whole board went unjudged (it solves unpowered, or its rest state
    /// does not solve): every hand finding on it is an abstention.
    pub unjudged: bool,
    /// DC solves run.
    pub solves: usize,
    /// Scenarios tried (rest, each switch, reversed supply).
    pub scenarios: usize,
    /// FET -> what the solve saw of it (the scenarios it was in and its body
    /// current there, or why it was in none).
    pub fet_notes: BTreeMap<String, String>,
}

/// A solved state: the workspace and its unknowns.
struct Solved {
    ws: Workspace,
    x: Vec<f64>,
}

impl Solved {
    fn v(&self, n: NodeId) -> f64 {
        self.ws
            .layout
            .node(n)
            .and_then(|i| self.x.get(i).copied())
            .unwrap_or(0.0)
    }
}

/// The circuit and logic drivers as a scenario found them, to put back after
/// it: each scenario starts from rest, not from the one before.
struct Snapshot {
    devices: Vec<Device>,
    drivers: Vec<HashMap<String, hauksbee_bind::drivers::PinDriver>>,
}

impl Snapshot {
    fn take(bound: &BoundBoard) -> Self {
        Self {
            devices: bound.circuit.devices.clone(),
            drivers: bound.digital.iter().map(|d| d.drivers.clone()).collect(),
        }
    }

    fn restore(&self, bound: &mut BoundBoard) {
        bound.circuit.devices.clone_from(&self.devices);
        for (d, drivers) in bound.digital.iter_mut().zip(&self.drivers) {
            d.drivers.clone_from(drivers);
        }
    }
}

/// Each logic driver's enable and level, to tell when the logic has settled.
fn drive_state(b: &BoundBoard) -> Vec<(bool, u64)> {
    (b.digital.iter())
        .flat_map(|d| d.drivers.values())
        .map(|drv| {
            let v = match b.circuit.devices.get(drv.vsource.0 as usize) {
                Some(Device::Vsource {
                    kind: SourceKind::Dc(v),
                    ..
                }) => v.to_bits(),
                _ => 0,
            };
            (drv.enabled, v)
        })
        .collect()
}

/// Settle `bound` (logic and converters reacting to the solve, as
/// `dc_stress` does), each solve seeded from the last. `None` when a solve
/// fails.
fn settle(bound: &mut BoundBoard, seed: Option<&[f64]>, solves: &mut usize) -> Option<Solved> {
    let mut ws = Workspace::new(&bound.circuit);
    let opts = SolverOptions::default();
    let mut last: Option<Vec<f64>> = seed.map(<[f64]>::to_vec);
    for _ in 0..SETTLE_SWEEPS {
        *solves += 1;
        let seed = last.as_deref().filter(|s| s.len() == ws.x.len());
        dc_with_junction_homotopy(&mut ws, &mut bound.circuit, &opts, seed).ok()?;
        let x = ws.x.clone();
        let before = drive_state(bound);
        let layout = &ws.layout;
        let node_v = |n: NodeId| {
            layout
                .node(n)
                .and_then(|i| x.get(i).copied())
                .unwrap_or(0.0)
        };
        let BoundBoard {
            circuit,
            digital,
            behavioral,
            ..
        } = &mut *bound;
        for d in digital.iter_mut() {
            d.tick(circuit, &node_v);
        }
        let arming = (behavioral.iter_mut())
            .map(|d| d.update_converter(circuit, &node_v))
            .fold(0.0, f64::max);
        let moved = drive_state(bound) != before || arming >= 1e-6;
        last = Some(x);
        if !moved {
            return last.map(|x| Solved { ws, x });
        }
    }
    // Still moving: solve the last state once more and judge it.
    *solves += 1;
    let seed = last.as_deref().filter(|s| s.len() == ws.x.len());
    dc_with_junction_homotopy(&mut ws, &mut bound.circuit, &opts, seed).ok()?;
    let x = ws.x.clone();
    Some(Solved { ws, x })
}

/// A MOSFET in the bound circuit: (reference, device index, drain, gate,
/// source, bulk, P-channel, folded threshold, body-diode saturation current).
#[derive(Clone, Debug)]
struct Fet {
    reference: String,
    device: usize,
    d: NodeId,
    g: NodeId,
    s: NodeId,
    b: NodeId,
    pmos: bool,
    vto: f64,
    body_is: f64,
}

impl Fet {
    /// Body-diode (bulk-drain junction) forward current at `st` (A).
    fn body_current(&self, st: &Solved) -> f64 {
        junction_current(self.body_is, self.body_bias(st))
    }

    /// Body-diode anode-to-cathode voltage at `st` (V): positive is forward.
    fn body_bias(&self, st: &Solved) -> f64 {
        let (anode, cathode) = if self.pmos {
            (self.d, self.b)
        } else {
            (self.b, self.d)
        };
        st.v(anode) - st.v(cathode)
    }

    /// Whether the channel conducts at `st` (folded |VGS| past threshold).
    fn channel_on(&self, st: &Solved) -> bool {
        let vgs = st.v(self.g) - st.v(self.s);
        let folded = if self.pmos { -vgs } else { vgs };
        folded > self.vto
    }
}

fn fets(bound: &BoundBoard) -> Vec<Fet> {
    (bound.circuit.devices.iter().enumerate())
        .filter_map(|(i, dev)| match dev {
            Device::Mosfet {
                name,
                d,
                g,
                s,
                b,
                model,
            } if model.body_is > 0.0 => Some(Fet {
                reference: name.clone(),
                device: i,
                d: *d,
                g: *g,
                s: *s,
                b: b.unwrap_or(*s),
                pmos: model.polarity == hauksbee_ir::Polarity::P,
                vto: model.vto,
                body_is: model.body_is,
            }),
            _ => None,
        })
        .collect()
}

/// A pin that can move a gate: the same drivers the hand check reads (an
/// output of any type, an MCU GPIO, a logic output, a transistor's collector
/// or drain), so the shadow opens every switch the hand check judges.
fn firmware_pin(idx: &Index<'_>, o: &Component, p: &Pin) -> bool {
    super::fet_orientation::drives(idx, o, p)
}

/// `net` and every net one resistor away that is not a named rail.
fn with_resistor_hop(idx: &Index<'_>, net: i64, skip: &str) -> Vec<i64> {
    let mut nets = vec![net];
    for (o, _) in idx.members(net) {
        if o.reference != skip
            && o.pins.len() == 2
            && RefClass::of(&o.reference) == RefClass::Resistor
        {
            nets.extend(
                (o.pins.iter())
                    .filter_map(|p| p.net)
                    .filter(|n| *n != 0 && *n != net && idx.level(*n).is_none()),
            );
        }
    }
    nets
}

/// The firmware output that sets `net` (directly, through one resistor, or
/// through one transistor whose base or gate such an output drives), as a
/// "REF pin N" label; `None` when nothing firmware chooses moves it.
fn firmware_control(idx: &Index<'_>, net: i64, skip: &str) -> Option<String> {
    if idx.level(net).is_some() {
        return None;
    }
    let nets = with_resistor_hop(idx, net, skip);
    let direct = nets.iter().find_map(|&n| {
        (idx.members(n).iter())
            .find(|(o, p)| o.reference != skip && firmware_pin(idx, o, p))
            .map(|(o, p)| format!("{} pin {}", o.reference, p.number))
    });
    direct.or_else(|| {
        nets.iter().find_map(|&n| {
            idx.members(n).iter().find_map(|(o, p)| {
                let m = idx.model(o)?;
                let (out, ctrl) = match m.kind {
                    ComponentKind::BjtNpn | ComponentKind::BjtPnp => ("collector", "base"),
                    ComponentKind::Nmos | ComponentKind::Pmos => ("drain", "gate"),
                    _ => return None,
                };
                if o.reference == skip || idx.role(o, p) != Some(out) {
                    return None;
                }
                let c = (o.pins.iter()).find(|q| idx.role(o, q) == Some(ctrl))?;
                let cn = c.net.filter(|n| *n != 0)?;
                let inner = with_resistor_hop(idx, cn, &o.reference);
                inner.iter().find_map(|&k| {
                    (idx.members(k).iter())
                        .find(|(oo, pp)| oo.reference != o.reference && firmware_pin(idx, oo, pp))
                        .map(|(oo, pp)| {
                            format!(
                                "{} pin {} (through {})",
                                oo.reference, pp.number, o.reference
                            )
                        })
                })
            })
        })
    })
}

/// A switch firmware can open, and how to open it.
#[derive(Clone, Debug)]
enum Switch {
    /// A FET whose channel is turned off (threshold raised out of reach).
    Fet { fet: Fet, driver: String },
    /// A gated regulator whose enable switch is opened.
    Regulator {
        reference: String,
        switch: usize,
        driver: String,
    },
}

impl Switch {
    fn part(&self) -> &str {
        match self {
            Self::Fet { fet, .. } => &fet.reference,
            Self::Regulator { reference, .. } => reference,
        }
    }

    fn driver(&self) -> &str {
        match self {
            Self::Fet { driver, .. } | Self::Regulator { driver, .. } => driver,
        }
    }

    /// Open the switch in `bound`; `false` when its device is not what it
    /// was bound as.
    fn open(&self, bound: &mut BoundBoard) -> bool {
        let index = match self {
            Self::Fet { fet, .. } => fet.device,
            Self::Regulator { switch, .. } => *switch,
        };
        match bound.circuit.devices.get_mut(index) {
            Some(Device::Mosfet { model, .. }) => model.vto = 1e3,
            Some(Device::VSwitch { ron, roff, .. }) => *ron = *roff,
            _ => return false,
        }
        true
    }
}

/// Node <-> board net id lookups.
struct Nets {
    by_node: HashMap<NodeId, i64>,
    name: HashMap<NodeId, String>,
}

impl Nets {
    fn new(board: &ExtractedBoard, bound: &BoundBoard) -> Self {
        let ids: HashMap<&str, i64> = (board.nets.iter())
            .map(|n| (n.name.as_str(), n.id))
            .collect();
        let mut by_node = HashMap::new();
        let mut name = HashMap::new();
        for (net, &node) in &bound.net_nodes {
            name.insert(node, net.clone());
            if let Some(&id) = ids.get(net.as_str()) {
                by_node.insert(node, id);
            }
        }
        Self { by_node, name }
    }

    fn name(&self, n: NodeId) -> String {
        self.name
            .get(&n)
            .cloned()
            .unwrap_or_else(|| "?".to_string())
    }
}

fn switches(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    bound: &BoundBoard,
    nets: &Nets,
    fets: &[Fet],
) -> Vec<Switch> {
    let idx = Index::new(board, lib);
    let mut out = Vec::new();
    for fet in fets {
        let Some(&gate) = nets.by_node.get(&fet.g) else {
            continue;
        };
        if let Some(driver) = firmware_control(&idx, gate, &fet.reference) {
            out.push(Switch::Fet {
                fet: fet.clone(),
                driver,
            });
        }
    }
    for r in &bound.report.regulators {
        let Some(gate) = r.gate.as_ref().filter(|g| g.held_off.is_none()) else {
            continue;
        };
        let driver = (r.enable.iter())
            .filter_map(|n| nets.by_node.get(n))
            .find_map(|&n| firmware_control(&idx, n, &r.reference));
        if let Some(driver) = driver {
            out.push(Switch::Regulator {
                reference: r.reference.clone(),
                switch: gate.switch.0 as usize,
                driver,
            });
        }
    }
    out.truncate(MAX_SWITCHES);
    out
}

/// Clamps carrying current into a live supply at rest: a pin held above it.
fn rest_findings(ij: &IcJunctions, rest: &Solved, nets: &Nets, out: &mut Vec<SimFinding>) {
    let mut seen = BTreeSet::new();
    for c in &ij.clamps {
        if c.upper.is_none() {
            continue;
        }
        let (vp, vs) = (rest.v(c.node), rest.v(c.supply));
        let i = clamp_current(vp - vs);
        if i < CLAMP_A || !seen.insert((c.reference.clone(), c.node)) {
            continue;
        }
        let (net, rail) = (nets.name(c.node), nets.name(c.supply));
        out.push(SimFinding {
            target: SimTarget::BackPower,
            part: c.reference.clone(),
            nets: vec![net.clone()],
            scenario: "at rest, every junction stamped".to_string(),
            message: format!(
                "pin {} ({}) of {} sits at {:.2} V on '{net}', {:.2} V above its supply '{rail}' \
                 ({:.2} V): its clamp conducts {:.3e} A into that supply",
                c.pin,
                c.role,
                c.reference,
                vp,
                vp - vs,
                vs,
                i
            ),
        });
    }
}

/// The parts back-fed or buses locked with `sw` open.
fn power_state_findings(
    ij: &IcJunctions,
    (rest, off): (&Solved, &Solved),
    sw: &Switch,
    nets: &Nets,
    out: &mut Vec<SimFinding>,
) {
    let scenario = format!("with {} off ({} at its off level)", sw.part(), sw.driver());
    // Parts whose supply this switch moved, by reference.
    let mut moved: BTreeMap<&str, (NodeId, f64, f64)> = BTreeMap::new();
    for (r, supply, _, _) in &ij.supply_clamps {
        let (a, b) = (rest.v(*supply), off.v(*supply));
        if (a - b).abs() > MOVED_V {
            moved.insert(r.as_str(), (*supply, a, b));
        }
    }
    let mut by_part: BTreeMap<&str, Vec<(String, f64)>> = BTreeMap::new();
    for c in ij.clamps.iter().filter(|c| c.upper.is_some()) {
        if !moved.contains_key(c.reference.as_str()) {
            continue;
        }
        let i = clamp_current(off.v(c.node) - off.v(c.supply));
        if i >= CLAMP_A {
            by_part
                .entry(c.reference.as_str())
                .or_default()
                .push((nets.name(c.node), i));
        }
    }
    for (part, pins) in by_part {
        let (supply, a, b) = moved[part];
        let total: f64 = pins.iter().map(|p| p.1).sum();
        let mut pin_nets: Vec<String> = pins.iter().map(|p| p.0.clone()).collect();
        pin_nets.dedup();
        out.push(SimFinding {
            target: SimTarget::PowerStates,
            part: part.to_string(),
            nets: pin_nets.clone(),
            scenario: scenario.clone(),
            message: format!(
                "{part}'s supply '{}' falls from {a:.2} V to {b:.2} V, but its pin clamps on {} \
                 still feed it {total:.3e} A: the part is back-fed from its signal lines",
                nets.name(supply),
                pin_nets.join(", ")
            ),
        });
    }
    // An I2C line high at rest that this switch drags under 30 % of a still
    // powered device's supply: a locked bus.
    let mut bus: BTreeMap<NodeId, Vec<&str>> = BTreeMap::new();
    for c in &ij.clamps {
        if is_i2c_net(&nets.name(c.node)) {
            bus.entry(c.node).or_default().push(c.reference.as_str());
        }
    }
    for (node, parts) in bus {
        let live = (ij.supply_clamps.iter())
            .filter(|(r, s, ..)| parts.contains(&r.as_str()) && off.v(*s) > 0.8 * rest.v(*s))
            .map(|(_, s, ..)| off.v(*s))
            .fold(0.0, f64::max);
        if live <= 0.0 || rest.v(node) < 0.7 * live || off.v(node) > 0.3 * live {
            continue;
        }
        // Blame the unpowered device whose clamp sinks the line, else the switch.
        let clamp = (ij.clamps.iter())
            .filter(|c| c.node == node && moved.contains_key(c.reference.as_str()))
            .find(|c| clamp_current(off.v(c.node) - off.v(c.supply)) >= CLAMP_A);
        let part = clamp.map_or_else(|| sw.part().to_string(), |c| c.reference.clone());
        let net = nets.name(node);
        out.push(SimFinding {
            target: SimTarget::PowerStates,
            part,
            nets: vec![net.clone()],
            scenario: scenario.clone(),
            message: format!(
                "I2C line '{net}' is {:.2} V at rest and {:.2} V {scenario}, under the 30 % of \
                 {live:.2} V a powered device on it reads as low: the bus locks up",
                rest.v(node),
                off.v(node)
            ),
        });
    }
}

/// The board nets joined by a wire at DC: a bound resistor of a few ohms or
/// less (a fuse, a bead, a sense resistor) or an inductor, between two board
/// nets, neither of them ground. The supply legs' own resistors run to an
/// internal node and so never appear.
fn wire_edges(bound: &BoundBoard, nets: &Nets) -> Vec<(NodeId, NodeId)> {
    let board_net = |n: &NodeId| *n != NodeId::GROUND && nets.name.contains_key(n);
    (bound.circuit.devices.iter())
        .filter_map(|dev| match dev {
            Device::Resistor { a, b, ohms, .. } if *ohms <= WIRE_OHMS => Some((*a, *b)),
            Device::Inductor { a, b, .. } => Some((*a, *b)),
            _ => None,
        })
        .filter(|(a, b)| a != b && board_net(a) && board_net(b))
        .collect()
}

/// The current resistors wired straight across `a` and `b` carry at `st` (A):
/// a pre-charge or soft-start path across a FET's channel.
fn bridge_current(bound: &BoundBoard, (a, b): (NodeId, NodeId), st: &Solved) -> f64 {
    let dv = (st.v(a) - st.v(b)).abs();
    (bound.circuit.devices.iter())
        .filter_map(|dev| match dev {
            Device::Resistor {
                a: p, b: n, ohms, ..
            } if *ohms > 0.0 && ((*p, *n) == (a, b) || (*p, *n) == (b, a)) => Some(dv / ohms),
            _ => None,
        })
        .sum()
}

/// Every node `edges` reach from `start`.
fn reach(start: &[NodeId], edges: &[(NodeId, NodeId)]) -> BTreeSet<NodeId> {
    let mut seen: BTreeSet<NodeId> = start.iter().copied().collect();
    let mut frontier: Vec<NodeId> = start.to_vec();
    while let Some(n) = frontier.pop() {
        for &(a, b) in edges {
            let far = if a == n {
                b
            } else if b == n {
                a
            } else {
                continue;
            };
            if seen.insert(far) {
                frontier.push(far);
            }
        }
    }
    seen
}

/// Where a reversed supply comes in, read from the board's topology.
struct SupplyInputs {
    /// Each input net and the level (V) the rest state holds it at: the pad of
    /// a connector or battery (not a ground) that wires and the always-on FETs
    /// join to a rail the binder supplies, and that the rest solve holds up.
    nodes: Vec<(NodeId, f64)>,
    /// The nets joined to a supplied rail through those wires and FETs.
    joined: BTreeSet<NodeId>,
    /// The wire-only edges, to tell which side of a FET an input is on.
    wires: Vec<(NodeId, NodeId)>,
}

/// The board's supply inputs: the pads a reversed supply would be plugged
/// into. A connector or battery pad counts when its net is joined to a rail
/// the binder supplies through wires and the FETs in `through` (on at rest,
/// no firmware on their gate): that is the pad the board's power arrives on,
/// whatever rail the binder put its supply on (it supplies a rail because the
/// name gives a voltage, often downstream of the protection the scenario is
/// testing). A signal pad behind a pull-up is not joined, a regulator's output
/// is not a supplied rail, and a pad the rest solve leaves dead has no level
/// to reverse.
fn supply_inputs(
    board: &ExtractedBoard,
    bound: &BoundBoard,
    nets: &Nets,
    rest: &Solved,
    through: &[&Fet],
) -> SupplyInputs {
    let wires = wire_edges(bound, nets);
    let mut edges = wires.clone();
    edges.extend(through.iter().map(|f| (f.d, f.s)));
    let legs: Vec<NodeId> = bound.supplies.iter().map(|l| l.net).collect();
    let joined = reach(&legs, &edges);
    let mut nodes: Vec<(NodeId, f64)> = Vec::new();
    let ids: HashMap<i64, &str> = (board.nets.iter())
        .map(|n| (n.id, n.name.as_str()))
        .collect();
    for c in (board.components.iter()).filter(|c| super::fet_orientation::is_supply_source(c)) {
        for p in &c.pins {
            let Some(name) = p.net.filter(|n| *n != 0).and_then(|n| ids.get(&n)) else {
                continue;
            };
            let Some(&node) = bound.net_nodes.get(*name) else {
                continue;
            };
            if hauksbee_extract::is_ground(name) || !joined.contains(&node) {
                continue;
            }
            let v = rest.v(node);
            if v >= LIVE_SUPPLY_V && !nodes.iter().any(|(n, _)| *n == node) {
                nodes.push((node, v));
            }
        }
    }
    SupplyInputs {
        nodes,
        joined,
        wires,
    }
}

/// Series resistance of a reversed supply (ohms): the stiff source the
/// binder's own supplies are.
const REVERSED_SOURCE_OHMS: f64 = hauksbee_bind::power_supply::STIFF_R_OHMS;

/// Open the series resistor of every supply leg `which` (leg index, net)
/// picks, so the rail it held is left to the circuit.
fn disconnect_legs(bound: &mut BoundBoard, which: &dyn Fn(usize, NodeId) -> bool) {
    let open: Vec<usize> = (bound.supplies.iter().enumerate())
        .filter(|(i, l)| which(*i, l.net))
        .map(|(_, l)| l.resistor.0 as usize)
        .collect();
    for i in open {
        if let Some(Device::Resistor { ohms, .. }) = bound.circuit.devices.get_mut(i) {
            *ohms = OPEN_OHMS;
        }
    }
}

/// Which supply legs a connector or battery feeds (on the net or one
/// wire-like series element away): the board's real inputs. When none is, every
/// leg counts, since the board then names no other way in.
fn input_legs(board: &ExtractedBoard, lib: &ModelLibrary, bound: &BoundBoard) -> Vec<bool> {
    let idx = Index::new(board, lib);
    let ids: HashMap<&str, i64> = (board.nets.iter())
        .map(|n| (n.name.as_str(), n.id))
        .collect();
    let fed = |net: i64| {
        let mut nets = vec![net];
        for (o, _) in idx.members(net) {
            if super::fet_orientation::is_series_element(o) {
                nets.extend(o.pins.iter().filter_map(|p| p.net).filter(|n| *n != 0));
            }
        }
        nets.iter().any(|&n| {
            (idx.members(n).iter()).any(|(o, _)| super::fet_orientation::is_supply_source(o))
        })
    };
    let out: Vec<bool> = (bound.supplies.iter())
        .map(|l| ids.get(l.net_name.as_str()).is_some_and(|&n| fed(n)))
        .collect();
    if out.iter().any(|f| *f) {
        out
    } else {
        vec![true; out.len()]
    }
}

/// Plug the supply in backwards: every supply leg disconnected (the binder's
/// rails stand in for the real input, which is now reversed), every simple
/// regulator's source zeroed, and each input net driven to minus its rest
/// level through [`REVERSED_SOURCE_OHMS`] (a Norton source, so no node is
/// added and the snapshot restores the circuit). The rest of the board solves
/// from there.
fn reverse_inputs(bound: &mut BoundBoard, inputs: &[(NodeId, f64)]) {
    disconnect_legs(bound, &|_, _| true);
    let regs: Vec<usize> = (bound.report.regulators.iter())
        .map(|r| r.source.0 as usize)
        .collect();
    for i in regs {
        if let Some(Device::Vsource {
            kind: SourceKind::Dc(v),
            ..
        }) = bound.circuit.devices.get_mut(i)
        {
            *v = 0.0;
        }
    }
    for (k, &(node, v)) in inputs.iter().enumerate() {
        bound.circuit.add(Device::Isource {
            name: format!("Irev_in{k}"),
            p: NodeId::GROUND,
            n: node,
            kind: SourceKind::Dc(-v / REVERSED_SOURCE_OHMS),
        });
        bound.circuit.add(Device::resistor(
            format!("Rrev_in{k}"),
            node,
            NodeId::GROUND,
            REVERSED_SOURCE_OHMS,
        ));
    }
}

/// Run the shadow scenarios over `board`.
#[must_use]
#[allow(clippy::too_many_lines)] // one scenario after another over one bound board
pub fn shadow_findings(board: &ExtractedBoard, lib: &ModelLibrary) -> Shadow {
    let mut shadow = Shadow::default();
    let mut bound = bind_board(board, lib);
    let held = undetermined_gates(&bound);
    hold_off(&mut bound, &held);
    for h in &held {
        shadow.why_not.insert(
            h.reference.clone(),
            "gate driven only by a pin with no DC model: held off, not judged".to_string(),
        );
    }
    let nets = Nets::new(board, &bound);
    if bound.supplies.is_empty() && bound.report.regulators.is_empty() {
        shadow.notes.push(
            "no input the binder can power (no supply net names a voltage): the board solves \
             unpowered"
                .to_string(),
        );
        shadow.unjudged = true;
    }
    // Rest without the IC junctions: which supplies the circuit powers.
    shadow.scenarios += 1;
    let Some(bare) = settle(&mut bound, None, &mut shadow.solves) else {
        shadow
            .notes
            .push("the rest state did not solve: nothing judged".to_string());
        shadow.unjudged = true;
        return shadow;
    };
    let ij = stamp_ic_junctions(&mut bound, &|n| bare.v(n));
    for r in &ij.abstained {
        shadow.why_not.insert(
            r.clone(),
            "every supply dead in the rest solve: no clamps stamped".to_string(),
        );
    }
    // The junctions add devices but no nodes: the bare rest point seeds it.
    let Some(rest) = settle(&mut bound, Some(&bare.x), &mut shadow.solves) else {
        shadow
            .notes
            .push("the rest state with every junction did not solve: nothing judged".to_string());
        shadow.unjudged = true;
        return shadow;
    };
    rest_findings(&ij, &rest, &nets, &mut shadow.findings);
    let at_rest = Snapshot::take(&bound);
    let inputs_fed = input_legs(board, lib, &bound);

    let fets = fets(&bound);
    let switches = switches(board, lib, &bound, &nets, &fets);
    let firmware_switched: BTreeSet<&str> = switches.iter().map(Switch::part).collect();
    for sw in &switches {
        // A FET whose channel nets both solve dead at rest has nothing to
        // switch: opening it shows nothing, which is not a pass.
        if let Switch::Fet { fet, .. } = sw {
            if rest.v(fet.d).abs().max(rest.v(fet.s).abs()) < LIVE_SUPPLY_V {
                shadow.why_not.insert(
                    fet.reference.clone(),
                    format!(
                        "both channel nets ('{}', '{}') solve dead at rest: nothing the binder \
                         models powers them, so its gate-off state is not judged",
                        nets.name(fet.d),
                        nets.name(fet.s)
                    ),
                );
                continue;
            }
        }
        shadow.scenarios += 1;
        if !sw.open(&mut bound) {
            at_rest.restore(&mut bound);
            continue;
        }
        // A rail the binder holds up only because its name gives a voltage
        // is fed through this switch on the board: let it go with the switch.
        if let Switch::Fet { fet, .. } = sw {
            let ends = [fet.d, fet.s];
            disconnect_legs(&mut bound, &|i, net| !inputs_fed[i] && ends.contains(&net));
        }
        let off = settle(&mut bound, Some(&rest.x), &mut shadow.solves);
        at_rest.restore(&mut bound);
        let Some(off) = off else {
            shadow
                .notes
                .push(format!("the state with {} off did not solve", sw.part()));
            shadow.why_not.insert(
                sw.part().to_string(),
                "its switch-off state did not solve".to_string(),
            );
            continue;
        };
        if let Switch::Fet { fet, driver } = sw {
            let i = fet.body_current(&off);
            shadow.fet_notes.insert(
                fet.reference.clone(),
                format!("gate off ({driver}): body diode carries {i:.1e} A"),
            );
            if i >= BODY_DIODE_A {
                let (anode, cathode) = if fet.pmos {
                    (fet.d, fet.b)
                } else {
                    (fet.b, fet.d)
                };
                shadow.findings.push(SimFinding {
                    target: SimTarget::FetOrientation,
                    part: fet.reference.clone(),
                    nets: vec![nets.name(fet.s), nets.name(fet.d)],
                    scenario: format!("with {}'s gate off ({driver})", fet.reference),
                    message: format!(
                        "with its gate off, {} still carries {:.3e} A through its body diode from \
                         '{}' ({:.2} V) to '{}' ({:.2} V): the switch cannot turn its load off",
                        fet.reference,
                        i,
                        nets.name(anode),
                        off.v(anode),
                        nets.name(cathode),
                        off.v(cathode)
                    ),
                });
            }
        }
        power_state_findings(&ij, (&rest, &off), sw, &nets, &mut shadow.findings);
    }

    // Input supply reversed.
    let always_on: Vec<&Fet> = (fets.iter())
        .filter(|f| !firmware_switched.contains(f.reference.as_str()))
        .filter(|f| !shadow.why_not.contains_key(&f.reference))
        .filter(|f| f.channel_on(&rest))
        .collect();
    let inputs = supply_inputs(board, &bound, &nets, &rest, &always_on);
    // Each candidate with its protected end, when the input's side is read.
    let mut candidates: Vec<(&Fet, Option<NodeId>)> = Vec::new();
    for f in always_on {
        if !inputs.joined.contains(&f.s) {
            continue;
        }
        if inputs.nodes.is_empty() {
            shadow.why_not.insert(
                f.reference.clone(),
                "on at rest with no firmware on its gate, but no connector or battery pad feeds \
                 its rail: no input to reverse, not judged"
                    .to_string(),
            );
            continue;
        }
        // A supply pad on each side (an input on one, a header passing the
        // protected rail out on the other) leaves which side is the input to
        // the reader: reversing both would hide the diode.
        let side_has_input = |end: NodeId| {
            let near = reach(&[end], &inputs.wires);
            inputs.nodes.iter().any(|(n, _)| near.contains(n))
        };
        let (at_s, at_d) = (side_has_input(f.s), side_has_input(f.d));
        if at_s && at_d {
            shadow.why_not.insert(
                f.reference.clone(),
                format!(
                    "a connector or battery pad on each side ('{}', '{}'): which is the input is \
                     not read, not judged",
                    nets.name(f.s),
                    nets.name(f.d)
                ),
            );
            continue;
        }
        let far = match (at_s, at_d) {
            (true, false) => Some(f.d),
            (false, true) => Some(f.s),
            _ => None,
        };
        candidates.push((f, far));
    }
    if !candidates.is_empty() {
        shadow.scenarios += 1;
        reverse_inputs(&mut bound, &inputs.nodes);
        let rev = settle(&mut bound, None, &mut shadow.solves);
        at_rest.restore(&mut bound);
        let input_names: Vec<String> = (inputs.nodes.iter())
            .map(|(n, v)| format!("'{}' at -{v:.1} V", nets.name(*n)))
            .collect();
        let scenario = format!("input supply reversed ({})", input_names.join(", "));
        match rev {
            None => {
                shadow
                    .notes
                    .push("the reversed-supply state did not solve".to_string());
                for (f, _) in candidates {
                    shadow.why_not.insert(
                        f.reference.clone(),
                        "the reversed-supply state did not solve".to_string(),
                    );
                }
            }
            Some(rev) => {
                for (f, far) in candidates {
                    // The protected side: the end away from the input, else
                    // the end the reversal pulls down least.
                    let far =
                        far.unwrap_or_else(|| if rev.v(f.s) > rev.v(f.d) { f.s } else { f.d });
                    let near = if far == f.s { f.d } else { f.s };
                    let i = f.body_current(&rev);
                    let bias = f.body_bias(&rev);
                    let (v_far, v_near) = (rev.v(far), rev.v(near));
                    shadow.fet_notes.insert(
                        f.reference.clone(),
                        format!(
                            "{scenario}: body diode at {bias:.2} V carries {i:.1e} A; '{}' (the \
                             protected side) at {v_far:.2} V",
                            nets.name(far)
                        ),
                    );
                    // Not blocked: the diode carries a load's current, or it
                    // is forward-biased and the protected side follows the
                    // reversed input below ground (an unloaded rail passes the
                    // reversal on with next to no current).
                    let passes =
                        bias > 0.0 && v_far < -LIVE_SUPPLY_V && rest.v(far) > LIVE_SUPPLY_V;
                    if i < BODY_DIODE_A && !passes {
                        continue;
                    }
                    // A resistor across the channel (a soft-start or
                    // pre-charge path) that carries more than the diode passes
                    // the reversal whichever way the FET faces: its
                    // orientation is not what lets it through.
                    let bridge = bridge_current(&bound, (f.s, f.d), &rev);
                    if bridge > i {
                        shadow.fet_notes.insert(
                            f.reference.clone(),
                            format!(
                                "{scenario}: a resistor across its channel carries the reversal \
                                 ({bridge:.1e} A against {i:.1e} A in the body diode): it passes \
                                 whichever way the FET faces, so its orientation is not judged \
                                 by it"
                            ),
                        );
                        continue;
                    }
                    shadow.findings.push(SimFinding {
                        target: SimTarget::FetOrientation,
                        part: f.reference.clone(),
                        nets: vec![nets.name(f.s), nets.name(f.d)],
                        scenario: scenario.clone(),
                        message: format!(
                            "{} is on at rest with no firmware on its gate, as a reverse-polarity \
                             FET is, but with the input reversed its body diode is forward-biased \
                             ({bias:.2} V, {i:.3e} A) and passes the reversal on: '{}' sits at \
                             {v_far:.2} V behind '{}' at {v_near:.2} V. The reversed supply is not \
                             blocked",
                            f.reference,
                            nets.name(far),
                            nets.name(near),
                        ),
                    });
                }
            }
        }
    }
    for f in &fets {
        if shadow.fet_notes.contains_key(&f.reference) || shadow.why_not.contains_key(&f.reference)
        {
            continue;
        }
        let why = if f.channel_on(&rest) {
            "on at rest with no firmware on its gate, but on no rail a supply pad feeds: not \
             reversed"
        } else {
            "no firmware output reaches its gate and it is off at rest: no scenario opens it"
        };
        shadow
            .fet_notes
            .insert(f.reference.clone(), why.to_string());
    }
    shadow
}

/// Where a (check, part) pair landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Both,
    /// The hand check found it; the solve judged the part and was silent.
    HandOnly,
    /// The hand check found it; the solve could not judge the part (the
    /// reason is the row's detail). Not a match: it blocks a retirement as a
    /// hand-only row does, but it names a missing model or scenario, not a
    /// miss.
    Abstained,
    SimOnly,
}

impl Side {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::HandOnly => "hand-only",
            Self::Abstained => "abstained",
            Self::SimOnly => "sim-only",
        }
    }
}

/// One row of the parity ledger.
#[derive(Clone, Debug)]
pub struct LedgerRow {
    pub check: SimTarget,
    pub part: String,
    pub side: Side,
    /// The sim's scenario and message, or for a hand-only row why the solve
    /// said nothing.
    pub detail: String,
}

/// The ledger for one board, with the shadow it came from and its cost.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    pub rows: Vec<LedgerRow>,
    pub shadow: Shadow,
    /// Wall time of the shadow pass (s).
    pub shadow_s: f64,
}

/// The hand checks' (check, part) pairs, with the first message for each.
#[must_use]
pub fn hand_pairs(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) -> BTreeMap<(SimTarget, String), String> {
    let mut out = BTreeMap::new();
    let mut add = |t: SimTarget, findings: Vec<hauksbee_extract::LintFinding>| {
        for f in findings {
            if let Some(part) = f.refs.first() {
                out.entry((t, part.clone())).or_insert(f.message);
            }
        }
    };
    add(
        SimTarget::FetOrientation,
        super::fet_orientation::fet_orientation_lint(board, lib).findings,
    );
    add(
        SimTarget::BackPower,
        super::back_power::back_power_lint(board, lib).findings,
    );
    add(
        SimTarget::PowerStates,
        super::power_states::power_state_lint(board, lib).findings,
    );
    out
}

/// Why the shadow is silent on a hand finding's part, as far as it can say,
/// and whether that is an abstention (the solve could not judge the part)
/// rather than a solved silence.
fn silent_reason(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    shadow: &Shadow,
    t: SimTarget,
    part: &str,
) -> (String, bool) {
    if let Some(why) = shadow.why_not.get(part) {
        return (why.clone(), true);
    }
    if shadow.unjudged {
        return (shadow.notes.join("; "), true);
    }
    let comp = board.components.iter().find(|c| c.reference == part);
    let kind = comp.and_then(|c| {
        super::resolved_components(board, lib)
            .find(|(r, _)| r.reference == c.reference)
            .map(|(_, m)| m.kind)
    });
    let silent = match (t, kind) {
        (_, None) => return ("no card resolved: nothing bound to solve".to_string(), true),
        (SimTarget::FetOrientation, Some(ComponentKind::Nmos | ComponentKind::Pmos)) => {
            match shadow.fet_notes.get(part) {
                None => return ("not bound as a MOSFET with a body diode".to_string(), true),
                Some(n) => format!("solved and silent: {n}"),
            }
        }
        (SimTarget::BackPower | SimTarget::PowerStates, Some(k))
            if !matches!(
                k,
                ComponentKind::Mcu
                    | ComponentKind::Digital
                    | ComponentKind::ShiftRegister
                    | ComponentKind::Adc
                    | ComponentKind::Dac
            ) =>
        {
            return (
                format!("{k:?} part: outside the logic/MCU clamp scope"),
                true,
            );
        }
        _ => "solved and silent: no clamp conducts in any scenario tried".to_string(),
    };
    let notes = if shadow.notes.is_empty() {
        String::new()
    } else {
        format!(" (board notes: {})", shadow.notes.join("; "))
    };
    (format!("{silent}{notes}"), false)
}

/// The parity ledger: every (check, part) pair the hand checks or the shadow
/// report, on which side it landed.
#[must_use]
pub fn parity_ledger(board: &ExtractedBoard, lib: &ModelLibrary) -> Ledger {
    let hand = hand_pairs(board, lib);
    let start = Instant::now();
    let shadow = shadow_findings(board, lib);
    let shadow_s = start.elapsed().as_secs_f64();
    let mut sim: BTreeMap<(SimTarget, String), &SimFinding> = BTreeMap::new();
    for f in &shadow.findings {
        sim.entry((f.target, f.part.clone())).or_insert(f);
    }
    let keys: BTreeSet<&(SimTarget, String)> = hand.keys().chain(sim.keys()).collect();
    let rows = (keys.into_iter())
        .map(|key| {
            let (t, part) = key;
            let (side, detail) = match (hand.contains_key(key), sim.get(key)) {
                (true, Some(f)) => (Side::Both, format!("{}: {}", f.scenario, f.message)),
                (false, Some(f)) => (Side::SimOnly, format!("{}: {}", f.scenario, f.message)),
                (_, None) => {
                    let (why, abstained) = silent_reason(board, lib, &shadow, *t, part);
                    let side = if abstained {
                        Side::Abstained
                    } else {
                        Side::HandOnly
                    };
                    (side, why)
                }
            };
            LedgerRow {
                check: *t,
                part: part.clone(),
                side,
                detail,
            }
        })
        .collect();
    Ledger {
        rows,
        shadow,
        shadow_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};

    const GND: i64 = 1;
    const V5: i64 = 2;
    const V3: i64 = 3;
    const LOAD: i64 = 10;
    const GATE: i64 = 11;
    const SIG: i64 = 12;
    const VIN: i64 = 20;
    const VFUSED: i64 = 21;
    const VPROT: i64 = 22;
    const RAW: i64 = 23;

    const NETS: &[(i64, &str)] = &[
        (RAW, "Net-(J1-Pad1)"),
        (GND, "GND"),
        (V5, "+5V"),
        (V3, "+3V3"),
        (LOAD, "LOAD"),
        (GATE, "GATE"),
        (SIG, "SIG"),
        (VIN, "+12V"),
        (VFUSED, "VIN_FUSED"),
        (VPROT, "VPROT"),
    ];

    fn part(reference: &str, value: &str, lib_id: &str, pins: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: "X".into(),
            pins: pins.iter().map(|(n, net)| pin_fn(n, *net, "")).collect(),
            ..Default::default()
        }
    }

    /// A FET named only by its class (no part number: the binder's generic
    /// corner set, with the generic body diode), pads G-S-D.
    fn generic_fet(class: &str, (s, d, g): (i64, i64, i64)) -> Component {
        Component {
            reference: "Q1".into(),
            value: class.into(),
            lib_id: format!("Device:Q_{class}_GSD"),
            footprint: "Package_TO_SOT_SMD:SOT-23".into(),
            pins: vec![
                pin_fn("1", g, "G"),
                pin_fn("2", s, "S"),
                pin_fn("3", d, "D"),
            ],
            ..Default::default()
        }
    }

    fn header(reference: &str, net: i64) -> Component {
        part(
            reference,
            "Conn",
            "Connector:Conn_01x02",
            &[("1", net), ("2", GND)],
        )
    }

    /// An MCU on +5V whose GPIO pad 2 lands on `gate`.
    fn mcu(gate: i64) -> Component {
        part(
            "U2",
            "ATmega328P",
            "MCU:ATmega328P",
            &[("2", gate), ("7", V5), ("8", GND)],
        )
    }

    fn res(reference: &str, value: &str, a: i64, b: i64) -> Component {
        part(reference, value, "Device:R", &[("1", a), ("2", b)])
    }

    /// A quad buffer (74x125 pinout: pad 2 A1, 7 GND, 14 VCC) on `vcc` with
    /// input A1 on `sig`.
    fn buffer(value: &str, vcc: i64, sig: i64) -> Component {
        part(
            "U5",
            value,
            "74xx:74HC125",
            &[("2", sig), ("7", GND), ("14", vcc)],
        )
    }

    fn ledger(parts: Vec<Component>) -> Ledger {
        parity_ledger(&board("t", NETS, parts), &ModelLibrary::builtin())
    }

    fn rows(l: &Ledger, check: SimTarget, side: Side) -> Vec<String> {
        (l.rows.iter())
            .filter(|r| r.check == check && r.side == side)
            .map(|r| r.part.clone())
            .collect()
    }

    /// A generic low-side switch: an N-FET with its source on ground, its
    /// drain on the load, its gate pulled to its source (and, in one variant,
    /// driven by an MCU pin). At rest the gate sits at the source, so the
    /// channel is off; with the gate driven off the body diode faces the
    /// supply and stays reverse-biased. The solve must find it off and say
    /// nothing, and the hand checks must agree.
    #[test]
    fn a_generic_low_side_nfet_with_its_gate_pulled_to_its_grounded_source_solves_off_and_silent() {
        for driven in [true, false] {
            let mut parts = vec![
                header("J1", V5),
                res("R2", "1k", V5, LOAD),
                generic_fet("NMOS", (GND, LOAD, GATE)),
                res("R1", "100k", GATE, GND),
            ];
            if driven {
                parts.push(mcu(GATE));
            }
            let b = board("t", NETS, parts);
            let lib = ModelLibrary::builtin();
            let mut bound = bind_board(&b, &lib);
            assert!(
                bound.report.assumed_body_diodes.contains(&"Q1".to_string()),
                "a class-only FET carries the generic, assumed body diode"
            );
            let mut solves = 0;
            let rest = settle(&mut bound, None, &mut solves).expect("the rest state solves");
            let q = (fets(&bound).into_iter())
                .find(|f| f.reference == "Q1")
                .expect("Q1 bound");
            assert!(!q.channel_on(&rest), "the gate sits at the source: off");
            assert!(q.body_current(&rest) < BODY_DIODE_A);
            assert!(
                rest.v(q.d) > 4.5,
                "the load is not pulled down: {}",
                rest.v(q.d)
            );
            let l = parity_ledger(&b, &lib);
            assert!(
                l.shadow.findings.is_empty(),
                "driven={driven}: {:#?}",
                l.shadow.findings
            );
            assert!(l.rows.is_empty(), "driven={driven}: {:#?}", l.rows);
        }
    }

    #[test]
    fn a_driven_high_side_pfet_with_its_drain_on_the_supply_still_feeds_its_load_when_off() {
        let bad = ledger(vec![
            header("J1", V5),
            generic_fet("PMOS", (LOAD, V5, GATE)),
            mcu(GATE),
            res("R2", "1k", LOAD, GND),
        ]);
        assert_eq!(
            rows(&bad, SimTarget::FetOrientation, Side::Both),
            ["Q1"],
            "{:#?}",
            bad.rows
        );
        let f = &bad.shadow.findings[0];
        assert!(f.message.contains("body diode"), "{}", f.message);
        let good = ledger(vec![
            header("J1", V5),
            generic_fet("PMOS", (V5, LOAD, GATE)),
            mcu(GATE),
            res("R2", "1k", LOAD, GND),
        ]);
        assert!(good.rows.is_empty(), "{:#?}", good.rows);
    }

    /// The reverse-polarity P-FET behind a fuse: source facing the input is
    /// reversed (the body diode passes a reversed supply), drain facing it is
    /// right.
    fn rpp(s: i64, d: i64) -> Vec<Component> {
        vec![
            header("J1", VIN),
            part("F1", "1A", "Device:Fuse", &[("1", VIN), ("2", VFUSED)]),
            generic_fet("PMOS", (s, d, GATE)),
            res("R1", "100k", GATE, GND),
            res("R2", "1k", VPROT, GND),
        ]
    }

    #[test]
    fn a_reversed_protection_pfet_conducts_a_reversed_supply_and_the_right_one_blocks_it() {
        let bad = ledger(rpp(VFUSED, VPROT));
        assert_eq!(
            rows(&bad, SimTarget::FetOrientation, Side::Both),
            ["Q1"],
            "{:#?}",
            bad.rows
        );
        assert!(
            (bad.shadow.findings.iter())
                .any(|f| f.scenario.starts_with("input supply reversed ('+12V'")),
            "{:#?}",
            bad.shadow.findings
        );
        let good = ledger(rpp(VPROT, VFUSED));
        assert!(good.rows.is_empty(), "{:#?}", good.rows);
    }

    /// The protection FET where the only named rail is downstream of it: the
    /// connector's pad is an unnamed net, the fuse and FET feed `+12V`, and
    /// nothing loads `+12V`. The reversal must be applied at the connector,
    /// not at the named rail, and an unloaded rail that follows the reversal
    /// below ground through the body diode is still not blocked.
    fn rpp_downstream(s: i64, d: i64) -> Vec<Component> {
        vec![
            header("J1", RAW),
            part("F1", "1A", "Device:Fuse", &[("1", RAW), ("2", VFUSED)]),
            generic_fet("PMOS", (s, d, GATE)),
            res("R1", "100k", GATE, GND),
        ]
    }

    #[test]
    fn a_reversed_protection_pfet_is_found_when_only_its_downstream_rail_is_named() {
        let bad = ledger(rpp_downstream(VFUSED, VIN));
        assert_eq!(
            rows(&bad, SimTarget::FetOrientation, Side::Both),
            ["Q1"],
            "{:#?}",
            bad.rows
        );
        let f = &bad.shadow.findings[0];
        assert!(
            f.scenario
                .starts_with("input supply reversed ('Net-(J1-Pad1)'"),
            "the reversal sits on the connector's pad: {}",
            f.scenario
        );
        assert!(f.message.contains("forward-biased"), "{}", f.message);
        let good = ledger(rpp_downstream(VIN, VFUSED));
        assert!(good.rows.is_empty(), "{:#?}", good.rows);
        assert!(
            (good.shadow.fet_notes.get("Q1"))
                .is_some_and(|n| n.starts_with("input supply reversed")),
            "the right orientation is judged, not skipped: {:#?}",
            good.shadow
        );
    }

    /// The same FET with a resistor straight across its channel (a soft-start
    /// FET with a pre-charge path, source on the input as such a switch has
    /// it): the reversal reaches the rail through the resistor whichever way
    /// the FET faces, so its orientation is not blamed. Without the resistor
    /// the same board is flagged (the test above).
    #[test]
    fn a_resistor_across_the_channel_carries_the_reversal_and_the_fet_is_not_blamed() {
        let mut parts = rpp_downstream(VFUSED, VIN);
        parts.push(res("R3", "62", VFUSED, VIN));
        let l = ledger(parts);
        assert!(
            !(l.shadow.findings.iter()).any(|f| f.target == SimTarget::FetOrientation),
            "{:#?}",
            l.shadow.findings
        );
        assert!(
            (l.shadow.fet_notes.get("Q1")).is_some_and(|n| n.contains("across its channel")),
            "{:#?}",
            l.shadow
        );
        assert!(l.rows.is_empty(), "{:#?}", l.rows);
    }

    /// A high-side switch with no connector or battery: nothing on the board
    /// is an input, so no reversal is tried and nothing is reported.
    #[test]
    fn a_switch_with_no_connector_or_battery_gets_no_reversed_supply() {
        let l = ledger(vec![
            generic_fet("PMOS", (V5, LOAD, GATE)),
            res("R1", "100k", GATE, GND),
            res("R2", "1k", LOAD, GND),
        ]);
        assert!(
            !(l.shadow.findings.iter()).any(|f| f.scenario.starts_with("input supply reversed")),
            "{:#?}",
            l.shadow.findings
        );
        assert!(
            (l.shadow.why_not.get("Q1")).is_some_and(|w| w.contains("no connector or battery")),
            "{:#?}",
            l.shadow
        );
        assert!(
            rows(&l, SimTarget::FetOrientation, Side::SimOnly).is_empty(),
            "{:#?}",
            l.rows
        );
    }

    /// A protection FET with a connector on each side (an input on one, the
    /// protected rail brought out on the other): which side is the input is
    /// not read, so the solve abstains rather than guess.
    #[test]
    fn a_fet_with_a_supply_pad_on_each_side_is_not_judged_reversed() {
        let mut parts = rpp(VFUSED, VPROT);
        parts.push(header("J2", VPROT));
        let l = ledger(parts);
        assert!(
            (l.shadow.why_not.get("Q1")).is_some_and(|w| w.contains("on each side")),
            "{:#?}",
            l.shadow
        );
        for r in l.rows.iter().filter(|r| r.part == "Q1") {
            assert_eq!(r.side, Side::Abstained, "{r:#?}");
        }
    }

    /// A firmware-switched FET between two nets nothing powers: its gate-off
    /// state has nothing to judge.
    #[test]
    fn a_switch_whose_channel_nets_solve_dead_is_not_judged() {
        let l = ledger(vec![
            header("J1", V5),
            mcu(GATE),
            generic_fet("PMOS", (SIG, LOAD, GATE)),
            res("R2", "1k", LOAD, GND),
        ]);
        assert!(
            (l.shadow.why_not.get("Q1")).is_some_and(|w| w.contains("solve dead at rest")),
            "{:#?}",
            l.shadow
        );
        assert!(l.shadow.findings.is_empty(), "{:#?}", l.shadow.findings);
    }

    /// With no net naming a voltage the board solves unpowered: a hand
    /// finding on it is an abstention, not a hand-only miss.
    #[test]
    fn a_hand_finding_on_an_unpowered_board_is_an_abstention() {
        let nets: &[(i64, &str)] = &[
            (GND, "GND"),
            (GATE, "GATE"),
            (VIN, "VIN"),
            (VFUSED, "VIN_FUSED"),
            (VPROT, "VPROT"),
        ];
        let b = board("t", nets, rpp(VFUSED, VPROT));
        let l = parity_ledger(&b, &ModelLibrary::builtin());
        assert!(l.shadow.unjudged, "{:#?}", l.shadow);
        assert!(l.shadow.findings.is_empty(), "{:#?}", l.shadow.findings);
        assert!(!l.rows.is_empty(), "the hand check flags Q1: {:#?}", l.rows);
        for r in &l.rows {
            assert_eq!(r.side, Side::Abstained, "{r:#?}");
        }
    }

    #[test]
    fn a_five_volt_pull_up_on_a_three_volt_input_conducts_through_its_clamp() {
        let pulled = |value: &str, rail: i64| {
            ledger(vec![
                header("J1", V5),
                header("J2", V3),
                buffer(value, V3, SIG),
                res("R7", "10k", SIG, rail),
            ])
        };
        let hc = pulled("74HC125D", V5);
        assert_eq!(
            rows(&hc, SimTarget::BackPower, Side::Both),
            ["U5"],
            "{:#?}",
            hc.rows
        );
        // Same domain: the clamp stays at leakage.
        let same = pulled("74HC125D", V3);
        assert!(same.rows.is_empty(), "{:#?}", same.rows);
        // A 5 V-tolerant input has no clamp to its supply.
        let lvc = pulled("74LVC125AD", V5);
        assert!(lvc.rows.is_empty(), "{:#?}", lvc.rows);
    }

    #[test]
    fn a_part_on_a_switched_rail_is_back_fed_through_a_pull_up_to_the_live_rail() {
        // Q1 switches +3V3 onto LOAD, where U5 and a 1k load sit; the gate is
        // pulled low (on) and an MCU pin can lift it. U5's input is pulled to
        // the always-on +3V3, so with Q1 off its clamp feeds LOAD.
        let l = ledger(vec![
            header("J1", V5),
            header("J2", V3),
            generic_fet("PMOS", (V3, LOAD, GATE)),
            res("R1", "100k", GATE, GND),
            mcu(GATE),
            res("R2", "1k", LOAD, GND),
            buffer("74HC125D", LOAD, SIG),
            res("R7", "10k", SIG, V3),
        ]);
        let found: Vec<&SimFinding> = (l.shadow.findings.iter())
            .filter(|f| f.target == SimTarget::PowerStates)
            .collect();
        assert_eq!(found.len(), 1, "{:#?} / {:#?}", l.shadow, l.rows);
        assert_eq!(found[0].part, "U5");
        assert!(
            found[0].message.contains("back-fed"),
            "{}",
            found[0].message
        );
        assert!(
            rows(&l, SimTarget::FetOrientation, Side::SimOnly).is_empty(),
            "{:#?}",
            l.rows
        );
    }
}
