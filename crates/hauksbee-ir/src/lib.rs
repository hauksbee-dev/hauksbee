//! Hauksbee circuit intermediate representation.
//!
//! A [`Circuit`] is a flat list of [`Device`]s wired to integer-indexed
//! [`NodeId`]s, plus a global temperature. Node 0 is always ground. Device
//! models (diode, BJT, MOSFET) carry their physical parameters inline so the
//! solver can stamp companion models without chasing pointers.
//!
//! The IR is deliberately solver-agnostic: every device reports whether it is
//! linear ([`Device::is_linear`]) and whether it is event-driven
//! ([`Device::is_event_driven`]), which is exactly the information the later
//! partitioning pass needs to split a circuit into linear / nonlinear /
//! digital islands. Nothing here commits to a solution method.

mod models;
mod source;
mod spice;

pub use models::{
    thermal_voltage as thermal_voltage_c, BjtModel, DiodeModel, MosLevel, MosfetModel, Polarity,
};
pub use source::{PwlPoint, SourceKind};
pub use spice::{SpiceError, SpiceLoader};

use serde::{Deserialize, Serialize};

/// Index of a circuit node. Node 0 is ground by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u32);

impl NodeId {
    /// The ground node.
    pub const GROUND: NodeId = NodeId(0);

    /// True for the ground reference node.
    pub fn is_ground(self) -> bool {
        self.0 == 0
    }
}

/// Stable handle to a device within a [`Circuit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub u32);

/// A circuit: interned node names, devices, and a global temperature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Circuit {
    /// Node names indexed by `NodeId.0`. Entry 0 is the ground name.
    node_names: Vec<String>,
    /// All devices, in insertion order. Index is the `DeviceId`.
    pub devices: Vec<Device>,
    /// Ambient temperature in Celsius. SPICE default is 27 C.
    pub temp_c: f64,
}

impl Default for Circuit {
    fn default() -> Self {
        Circuit::new()
    }
}

impl Circuit {
    /// Empty circuit with only the ground node defined and `temp_c = 27`.
    pub fn new() -> Self {
        Circuit {
            node_names: vec!["0".to_string()],
            devices: Vec::new(),
            temp_c: 27.0,
        }
    }

    /// Intern a node name, returning its id. `"0"` and `"gnd"`
    /// (case-insensitive) both map to ground.
    pub fn node(&mut self, name: &str) -> NodeId {
        if name == "0" || name.eq_ignore_ascii_case("gnd") {
            return NodeId::GROUND;
        }
        if let Some(i) = self.node_names.iter().position(|n| n == name) {
            return NodeId(i as u32);
        }
        let id = self.node_names.len() as u32;
        self.node_names.push(name.to_string());
        NodeId(id)
    }

    /// Name of a node, or `"?"` if the id is out of range.
    pub fn node_name(&self, id: NodeId) -> &str {
        self.node_names
            .get(id.0 as usize)
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// Number of nodes including ground.
    pub fn node_count(&self) -> usize {
        self.node_names.len()
    }

    /// Append a device and return its id.
    pub fn add(&mut self, device: Device) -> DeviceId {
        let id = DeviceId(self.devices.len() as u32);
        self.devices.push(device);
        id
    }

    /// Iterate `(DeviceId, &Device)` over all devices.
    pub fn iter(&self) -> impl Iterator<Item = (DeviceId, &Device)> {
        self.devices
            .iter()
            .enumerate()
            .map(|(i, d)| (DeviceId(i as u32), d))
    }

    /// Highest non-ground node id referenced by any device, used to size the
    /// MNA system. Returns 0 if the circuit touches only ground.
    pub fn max_node(&self) -> u32 {
        self.devices
            .iter()
            .flat_map(|d| d.nodes())
            .map(|n| n.0)
            .max()
            .unwrap_or(0)
            .max(self.node_names.len().saturating_sub(1) as u32)
    }
}

/// A circuit element. The hot solver path matches this enum directly, so no
/// device costs a virtual dispatch per timestep.
///
/// Reference designators (`name`) are kept for diagnostics and for matching
/// `.model` cards during loading; the solver ignores them.
#[derive(Debug, Clone, Serialize, Deserialize, strum::EnumCount)]
pub enum Device {
    /// Linear resistor. `tc1` is the linear temperature coefficient (1/C);
    /// `R(T) = ohms * (1 + tc1 * (T - 27))`.
    Resistor {
        name: String,
        a: NodeId,
        b: NodeId,
        ohms: f64,
        tc1: Option<f64>,
    },
    /// Linear capacitor. `ic` is an optional initial voltage for `.tran uic`.
    Capacitor {
        name: String,
        a: NodeId,
        b: NodeId,
        farads: f64,
        ic: Option<f64>,
    },
    /// Linear inductor. `ic` is an optional initial current.
    Inductor {
        name: String,
        a: NodeId,
        b: NodeId,
        henries: f64,
        ic: Option<f64>,
    },
    /// Independent voltage source, current flows `p -> n` internally.
    Vsource {
        name: String,
        p: NodeId,
        n: NodeId,
        kind: SourceKind,
    },
    /// Independent current source, current flows from `p` to `n` in the branch.
    Isource {
        name: String,
        p: NodeId,
        n: NodeId,
        kind: SourceKind,
    },
    /// PN junction diode.
    Diode {
        name: String,
        a: NodeId,
        k: NodeId,
        model: DiodeModel,
    },
    /// Bipolar transistor (Gummel-Poon basics).
    Bjt {
        name: String,
        c: NodeId,
        b: NodeId,
        e: NodeId,
        model: BjtModel,
    },
    /// MOSFET. Bulk defaults to source when absent.
    Mosfet {
        name: String,
        d: NodeId,
        g: NodeId,
        s: NodeId,
        b: Option<NodeId>,
        model: MosfetModel,
    },
    /// Voltage-controlled switch with a smooth (tanh) ron/roff transition.
    VSwitch {
        name: String,
        a: NodeId,
        b: NodeId,
        ctrl_p: NodeId,
        ctrl_n: NodeId,
        von: f64,
        voff: f64,
        ron: f64,
        roff: f64,
    },
    /// Behavioral op-amp: `out = clamp(reference + gain * (inp - inn), rails)`.
    /// `pole_hz`, when present, applies a single AC pole to the gain path.
    OpAmp {
        name: String,
        out: NodeId,
        inp: NodeId,
        inn: NodeId,
        reference: Option<NodeId>,
        gain: f64,
        pole_hz: Option<f64>,
        rail_lo: f64,
        rail_hi: f64,
    },
    /// Behavioral comparator: output snaps to a rail at the threshold.
    Comparator {
        name: String,
        out: NodeId,
        inp: NodeId,
        inn: NodeId,
        out_lo: f64,
        out_hi: f64,
        hysteresis: f64,
    },
}

impl Device {
    /// Reference designator (`R1`, `Q3`, ...).
    pub fn name(&self) -> &str {
        match self {
            Device::Resistor { name, .. }
            | Device::Capacitor { name, .. }
            | Device::Inductor { name, .. }
            | Device::Vsource { name, .. }
            | Device::Isource { name, .. }
            | Device::Diode { name, .. }
            | Device::Bjt { name, .. }
            | Device::Mosfet { name, .. }
            | Device::VSwitch { name, .. }
            | Device::OpAmp { name, .. }
            | Device::Comparator { name, .. } => name,
        }
    }

    /// All nodes this device connects to (may include ground).
    pub fn nodes(&self) -> Vec<NodeId> {
        match self {
            Device::Resistor { a, b, .. }
            | Device::Capacitor { a, b, .. }
            | Device::Inductor { a, b, .. }
            | Device::Diode { a, k: b, .. } => vec![*a, *b],
            Device::Vsource { p, n, .. } | Device::Isource { p, n, .. } => vec![*p, *n],
            Device::Bjt { c, b, e, .. } => vec![*c, *b, *e],
            Device::Mosfet { d, g, s, b, .. } => {
                let mut v = vec![*d, *g, *s];
                if let Some(b) = b {
                    v.push(*b);
                }
                v
            }
            Device::VSwitch {
                a,
                b,
                ctrl_p,
                ctrl_n,
                ..
            } => vec![*a, *b, *ctrl_p, *ctrl_n],
            Device::OpAmp {
                out,
                inp,
                inn,
                reference,
                ..
            } => {
                let mut nodes = vec![*out, *inp, *inn];
                if let Some(reference) = reference {
                    nodes.push(*reference);
                }
                nodes
            }
            Device::Comparator { out, inp, inn, .. } => {
                vec![*out, *inp, *inn]
            }
        }
    }

    /// Rewrite every [`NodeId`] this device carries, in place, through `f`.
    ///
    /// This is the *one* node-walk over a `Device`. Node remapping (partitioner
    /// sub-circuit extraction, tear-engine island building) routes through here
    /// instead of re-listing the variants at each call site, so there is a
    /// single place to update when a variant is added.
    ///
    /// The match is exhaustive with no `_` arm on purpose: a new variant must
    /// fail to compile until its nodes are wired in here. The hazard that guards
    /// against is silent, not loud. A catch-all would let a new device's nodes
    /// slip through unremapped and drop the device from every partitioned
    /// sub-circuit with no error (see `docs/dev-plans/04-spice-compat.md` §1,
    /// the `clone_remapped` row of the touchpoint table). Optional terminals
    /// (`Mosfet` bulk, `OpAmp` reference) are remapped only when present.
    pub fn map_nodes(&mut self, f: &mut impl FnMut(NodeId) -> NodeId) {
        match self {
            Device::Resistor { a, b, .. }
            | Device::Capacitor { a, b, .. }
            | Device::Inductor { a, b, .. }
            | Device::Diode { a, k: b, .. } => {
                *a = f(*a);
                *b = f(*b);
            }
            Device::Vsource { p, n, .. } | Device::Isource { p, n, .. } => {
                *p = f(*p);
                *n = f(*n);
            }
            Device::Bjt { c, b, e, .. } => {
                *c = f(*c);
                *b = f(*b);
                *e = f(*e);
            }
            Device::Mosfet { d, g, s, b, .. } => {
                *d = f(*d);
                *g = f(*g);
                *s = f(*s);
                if let Some(b) = b {
                    *b = f(*b);
                }
            }
            Device::VSwitch {
                a,
                b,
                ctrl_p,
                ctrl_n,
                ..
            } => {
                *a = f(*a);
                *b = f(*b);
                *ctrl_p = f(*ctrl_p);
                *ctrl_n = f(*ctrl_n);
            }
            Device::OpAmp {
                out,
                inp,
                inn,
                reference,
                ..
            } => {
                *out = f(*out);
                *inp = f(*inp);
                *inn = f(*inn);
                if let Some(reference) = reference {
                    *reference = f(*reference);
                }
            }
            Device::Comparator { out, inp, inn, .. } => {
                *out = f(*out);
                *inp = f(*inp);
                *inn = f(*inn);
            }
        }
    }

    /// Terminals through which this device injects current: the nodes whose
    /// KCL rows receive contributions (matrix entries or RHS) from its stamp.
    ///
    /// Together with [`Device::sense_nodes`] this is the classification the
    /// decomposition engine's tearing proofs rest on: electrical reachability
    /// must traverse conduction terminals only, or two independently solvable
    /// blocks fuse through a wire that carries no current (see
    /// `docs/dev-plans/02-tearing-architecture.md` §1, §2.1). The claim made
    /// here is about the *stamp as implemented today*, not the physical part:
    /// a MOSFET gate conducts no current in the Level-1 stamp, so it is sense,
    /// and the day gate charge lands the classification here must change with
    /// it. That drift is caught mechanically: the cross-check test in
    /// `hauksbee-solve` stamps every example device and fails if a declared
    /// sense node's row receives anything, so this declaration cannot silently
    /// disagree with the stamp.
    pub fn conduction_nodes(&self) -> Vec<NodeId> {
        match self {
            Device::Resistor { a, b, .. }
            | Device::Capacitor { a, b, .. }
            | Device::Inductor { a, b, .. }
            | Device::Diode { a, k: b, .. } => vec![*a, *b],
            Device::Vsource { p, n, .. } | Device::Isource { p, n, .. } => vec![*p, *n],
            Device::Bjt { c, b, e, .. } => vec![*c, *b, *e],
            // Level-1 stamp: channel current flows d<->s; the gate row gets no
            // entries (gm lands in the d/s rows referencing the gate COLUMN)
            // and the optional bulk is not stamped at all. Both become
            // conduction terminals when W3 adds gate charge / the body diode.
            Device::Mosfet { d, s, .. } => vec![*d, *s],
            // The switch channel conducts; the control pair only steers it.
            Device::VSwitch { a, b, .. } => vec![*a, *b],
            // Behavioral output stages drive their out node through a 1 Ohm
            // Thevenin; the inputs are ideal (infinite input impedance) and the
            // opamp reference only offsets the target voltage (its own row gets
            // nothing; it appears as a column of the out row).
            Device::OpAmp { out, .. } => vec![*out],
            Device::Comparator { out, .. } => vec![*out],
        }
    }

    /// Terminals this device only reads: they steer the stamp (Jacobian
    /// columns, target voltages) but their own KCL rows receive no current.
    ///
    /// A sense terminal is what makes a *free tear* exact: cutting the wire and
    /// replaying its voltage as a source changes nothing electrically, because
    /// no current ever crossed it (`docs/dev-plans/02-tearing-architecture.md`
    /// §1). Every node the device touches is in exactly one of
    /// [`Device::conduction_nodes`] or this set; the solve-side cross-check
    /// test enforces both the partition and the zero-row property.
    pub fn sense_nodes(&self) -> Vec<NodeId> {
        match self {
            Device::Resistor { .. }
            | Device::Capacitor { .. }
            | Device::Inductor { .. }
            | Device::Diode { .. }
            | Device::Vsource { .. }
            | Device::Isource { .. }
            | Device::Bjt { .. } => Vec::new(),
            Device::Mosfet { g, b, .. } => {
                let mut v = vec![*g];
                if let Some(b) = b {
                    v.push(*b);
                }
                v
            }
            Device::VSwitch { ctrl_p, ctrl_n, .. } => vec![*ctrl_p, *ctrl_n],
            Device::OpAmp {
                inp,
                inn,
                reference,
                ..
            } => {
                let mut v = vec![*inp, *inn];
                if let Some(reference) = reference {
                    v.push(*reference);
                }
                v
            }
            Device::Comparator { inp, inn, .. } => vec![*inp, *inn],
        }
    }

    /// One representative instance of every variant, wired to the given nodes
    /// (cycled as needed). This is the inventory the per-variant enforcement
    /// tests iterate. Completeness is enforced by the derived
    /// `strum::EnumCount` length assert inside: adding a `Device` variant bumps
    /// the count and this function panics (in every test that calls it) until
    /// the new example ships, and every example is then automatically subjected
    /// to the stamp/sense cross-check, serde round-trip, and node-walk coverage
    /// tests. Parameter values are arbitrary but physically sane (the tests
    /// probe structure, not accuracy).
    pub fn examples(n: [NodeId; 4]) -> Vec<Device> {
        let out = vec![
            Device::Resistor {
                name: "Rex".into(),
                a: n[0],
                b: n[1],
                ohms: 1e3,
                tc1: None,
            },
            Device::Capacitor {
                name: "Cex".into(),
                a: n[0],
                b: n[1],
                farads: 1e-9,
                ic: None,
            },
            Device::Inductor {
                name: "Lex".into(),
                a: n[0],
                b: n[1],
                henries: 1e-6,
                ic: None,
            },
            Device::Vsource {
                name: "Vex".into(),
                p: n[0],
                n: n[1],
                kind: SourceKind::Dc(1.0),
            },
            Device::Isource {
                name: "Iex".into(),
                p: n[0],
                n: n[1],
                kind: SourceKind::Dc(1e-3),
            },
            Device::Diode {
                name: "Dex".into(),
                a: n[0],
                k: n[1],
                model: DiodeModel::default(),
            },
            Device::Bjt {
                name: "Qex".into(),
                c: n[0],
                b: n[1],
                e: n[2],
                model: BjtModel::default(),
            },
            Device::Mosfet {
                name: "Mex".into(),
                d: n[0],
                g: n[1],
                s: n[2],
                b: Some(n[3]),
                model: MosfetModel::default(),
            },
            Device::VSwitch {
                name: "Sex".into(),
                a: n[0],
                b: n[1],
                ctrl_p: n[2],
                ctrl_n: n[3],
                von: 2.0,
                voff: 1.0,
                ron: 1.0,
                roff: 1e9,
            },
            Device::OpAmp {
                name: "Aex".into(),
                out: n[0],
                inp: n[1],
                inn: n[2],
                reference: Some(n[3]),
                gain: 1e5,
                pole_hz: Some(1e6),
                rail_lo: -5.0,
                rail_hi: 5.0,
            },
            Device::Comparator {
                name: "Kex".into(),
                out: n[0],
                inp: n[1],
                inn: n[2],
                out_lo: 0.0,
                out_hi: 5.0,
                hysteresis: 1e-3,
            },
        ]
        ;
        // The derived variant count is the enforcement the match-arm trick
        // could not provide: an OR-arm satisfies a match without shipping an
        // instance, but it cannot satisfy this length check. A new variant
        // bumps `Device::COUNT` automatically (strum::EnumCount), so this
        // inventory fails loudly until the example exists, and every example
        // then flows into the stamp/sense cross-check, serde round-trip, and
        // node-walk coverage tests.
        assert_eq!(
            out.len(),
            <Device as strum::EnumCount>::COUNT,
            "Device::examples() must ship exactly one instance per variant"
        );
        debug_assert_eq!(
            out.iter().map(std::mem::discriminant).collect::<std::collections::HashSet<_>>().len(),
            out.len(),
            "Device::examples() has a duplicate variant"
        );
        out
    }

    /// Whether the element's stamp is constant w.r.t. node voltages.
    ///
    /// Independent sources are linear in the MNA sense (their value depends on
    /// time, not on the solution). Diodes, transistors, switches, and the
    /// behavioral blocks are nonlinear and force Newton iteration in any island
    /// that contains them.
    pub fn is_linear(&self) -> bool {
        matches!(
            self,
            Device::Resistor { .. }
                | Device::Capacitor { .. }
                | Device::Inductor { .. }
                | Device::Vsource { .. }
                | Device::Isource { .. }
        )
    }

    /// Whether the device is best handled by the event queue rather than the
    /// analog solver. The behavioral comparator is the one digital-ish element
    /// here; the partitioning pass will use this to peel it off.
    pub fn is_event_driven(&self) -> bool {
        matches!(self, Device::Comparator { .. })
    }
}
