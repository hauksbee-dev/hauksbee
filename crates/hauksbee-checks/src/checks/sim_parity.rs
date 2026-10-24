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
//! - **Input supply reversed** (every supply the binder powers the board from,
//!   negated, regulators off): a FET the rest state holds on with a gate no
//!   firmware drives, sitting on an input, that conducts through its body
//!   diode is a reverse-polarity FET that does not block (`fet_orientation`).
//!
//! What picks the scenarios (which switch firmware can open) is read from the
//! board; what decides a finding is only the solve. Nothing here enters a
//! verdict: [`parity_ledger`] lays the sim findings beside the hand checks'
//! as (check, part) pairs, so a hand check is retired only when the ledger
//! shows the solve finds what it finds. The shadow abstains, with the reason
//! kept in [`Shadow::why_not`], wherever the solve cannot speak: a state that
//! does not solve, a FET gate no model sets, a part whose rail the rest solve
//! leaves dead.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use hauksbee_bind::binder::{bind_board, BoundBoard};
use hauksbee_bind::control_inputs::{hold_off, undetermined_gates};
use hauksbee_bind::junctions::{
    clamp_current, dc_with_junction_homotopy, junction_current, stamp_ic_junctions, IcJunctions,
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
        let (anode, cathode) = if self.pmos {
            (self.d, self.b)
        } else {
            (self.b, self.d)
        };
        junction_current(self.body_is, st.v(anode) - st.v(cathode))
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

/// Nodes an input supply sits on, plus the far side of any wire-like series
/// element on them.
fn input_nodes(bound: &BoundBoard, inputs: &[bool]) -> BTreeSet<NodeId> {
    let mut nodes: BTreeSet<NodeId> = (bound.supplies.iter().zip(inputs))
        .filter(|(_, input)| **input)
        .map(|(l, _)| l.net)
        .collect();
    let near: Vec<NodeId> = nodes.iter().copied().collect();
    for dev in &bound.circuit.devices {
        if let Device::Resistor { a, b, ohms, .. } = dev {
            if *ohms <= WIRE_OHMS {
                if near.contains(a) && *b != NodeId::GROUND {
                    nodes.insert(*b);
                }
                if near.contains(b) && *a != NodeId::GROUND {
                    nodes.insert(*a);
                }
            }
        }
    }
    nodes
}

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

/// Negate every input supply, disconnect every other supply leg (a rail the
/// binder powers only because it names a voltage) and zero every simple
/// regulator's source.
fn reverse_inputs(bound: &mut BoundBoard, inputs: &[bool]) {
    disconnect_legs(bound, &|i, _| !inputs[i]);
    let legs: Vec<usize> = (bound.supplies.iter().zip(inputs))
        .filter(|(_, input)| **input)
        .map(|(l, _)| l.vsource.0 as usize)
        .collect();
    let regs: Vec<usize> = (bound.report.regulators.iter())
        .map(|r| r.source.0 as usize)
        .collect();
    for (i, dev) in bound.circuit.devices.iter_mut().enumerate() {
        if let Device::Vsource {
            kind: SourceKind::Dc(v),
            ..
        } = dev
        {
            if legs.contains(&i) {
                *v = -*v;
            } else if regs.contains(&i) {
                *v = 0.0;
            }
        }
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
    }
    // Rest without the IC junctions: which supplies the circuit powers.
    shadow.scenarios += 1;
    let Some(bare) = settle(&mut bound, None, &mut shadow.solves) else {
        shadow
            .notes
            .push("the rest state did not solve: nothing judged".to_string());
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
        return shadow;
    };
    rest_findings(&ij, &rest, &nets, &mut shadow.findings);
    let at_rest = Snapshot::take(&bound);
    let inputs_fed = input_legs(board, lib, &bound);

    let fets = fets(&bound);
    let switches = switches(board, lib, &bound, &nets, &fets);
    let firmware_switched: BTreeSet<&str> = switches.iter().map(Switch::part).collect();
    for sw in &switches {
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
    let inputs = input_nodes(&bound, &inputs_fed);
    let candidates: Vec<&Fet> = (fets.iter())
        .filter(|f| !firmware_switched.contains(f.reference.as_str()))
        .filter(|f| !shadow.why_not.contains_key(&f.reference))
        .filter(|f| f.channel_on(&rest))
        .filter(|f| inputs.contains(&f.s) || inputs.contains(&f.d))
        .collect();
    if !candidates.is_empty() {
        shadow.scenarios += 1;
        reverse_inputs(&mut bound, &inputs_fed);
        let rev = settle(&mut bound, None, &mut shadow.solves);
        at_rest.restore(&mut bound);
        match rev {
            None => shadow
                .notes
                .push("the reversed-supply state did not solve".to_string()),
            Some(rev) => {
                for f in candidates {
                    let i = f.body_current(&rev);
                    shadow.fet_notes.insert(
                        f.reference.clone(),
                        format!("input reversed: body diode carries {i:.1e} A"),
                    );
                    if i < BODY_DIODE_A {
                        continue;
                    }
                    shadow.findings.push(SimFinding {
                        target: SimTarget::FetOrientation,
                        part: f.reference.clone(),
                        nets: vec![nets.name(f.s), nets.name(f.d)],
                        scenario: "input supply reversed".to_string(),
                        message: format!(
                            "{} is on at rest with no firmware on its gate, as a reverse-polarity \
                             FET is, but with the input reversed it conducts {:.3e} A through its \
                             body diode ('{}' at {:.2} V, '{}' at {:.2} V): the reversed supply is \
                             not blocked",
                            f.reference,
                            i,
                            nets.name(f.s),
                            rev.v(f.s),
                            nets.name(f.d),
                            rev.v(f.d)
                        ),
                    });
                }
            }
        }
    }
    for f in &fets {
        if shadow.fet_notes.contains_key(&f.reference) {
            continue;
        }
        let why = if !f.channel_on(&rest) {
            "no firmware output reaches its gate and it is off at rest: no scenario opens it"
        } else if !inputs.contains(&f.s) && !inputs.contains(&f.d) {
            "on at rest with no firmware on its gate, but on no input supply: not reversed"
        } else {
            "not tried"
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
    HandOnly,
    SimOnly,
}

impl Side {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::HandOnly => "hand-only",
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

/// Why the shadow is silent on a hand finding's part, as far as it can say.
fn silent_reason(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    shadow: &Shadow,
    t: SimTarget,
    part: &str,
) -> String {
    if let Some(why) = shadow.why_not.get(part) {
        return why.clone();
    }
    if !shadow.notes.is_empty() {
        return shadow.notes.join("; ");
    }
    let comp = board.components.iter().find(|c| c.reference == part);
    let kind = comp.and_then(|c| {
        super::resolved_components(board, lib)
            .find(|(r, _)| r.reference == c.reference)
            .map(|(_, m)| m.kind)
    });
    match (t, kind) {
        (_, None) => "no card resolved: nothing bound to solve".to_string(),
        (SimTarget::FetOrientation, Some(ComponentKind::Nmos | ComponentKind::Pmos)) => {
            shadow.fet_notes.get(part).map_or_else(
                || "not bound as a MOSFET with a body diode".to_string(),
                |n| format!("solved and silent: {n}"),
            )
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
            format!("{k:?} part: outside the logic/MCU clamp scope")
        }
        _ => "solved and silent: no clamp conducts in any scenario tried".to_string(),
    }
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
            let found = sim.get(key);
            let side = match (hand.contains_key(key), found.is_some()) {
                (true, true) => Side::Both,
                (true, false) => Side::HandOnly,
                _ => Side::SimOnly,
            };
            let detail = found.map_or_else(
                || silent_reason(board, lib, &shadow, *t, part),
                |f| format!("{}: {}", f.scenario, f.message),
            );
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

    const NETS: &[(i64, &str)] = &[
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
            (bad.shadow.findings.iter()).any(|f| f.scenario == "input supply reversed"),
            "{:#?}",
            bad.shadow.findings
        );
        let good = ledger(rpp(VPROT, VFUSED));
        assert!(good.rows.is_empty(), "{:#?}", good.rows);
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
