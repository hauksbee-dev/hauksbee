//! Galvani circuit intermediate representation.
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
        self.node_names.get(id.0 as usize).map(String::as_str).unwrap_or("?")
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Behavioral op-amp: `out = clamp(gain * (inp - inn), rails)`.
    OpAmp {
        name: String,
        out: NodeId,
        inp: NodeId,
        inn: NodeId,
        gain: f64,
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
            Device::VSwitch { a, b, ctrl_p, ctrl_n, .. } => vec![*a, *b, *ctrl_p, *ctrl_n],
            Device::OpAmp { out, inp, inn, .. } | Device::Comparator { out, inp, inn, .. } => {
                vec![*out, *inp, *inn]
            }
        }
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
