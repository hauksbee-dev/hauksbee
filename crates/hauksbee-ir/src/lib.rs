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
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/README.md (the crate
//! tour) and docs/how-and-why/hauksbee-ir/lib.md (this module's deep-dive).

mod bexpr;
pub mod debug;
mod models;
mod source;
mod spice;

pub use bexpr::CompiledExpr;
pub use models::{
    thermal_voltage as thermal_voltage_c, BjtModel, DiodeModel, MosLevel, MosfetModel, Polarity,
};
pub use source::{AcStim, PwlPoint, SourceKind};
pub use spice::{
    AcDirective, AcSweep, DcDirective, DcSweep, Directives, PrintRequest, SpiceError, SpiceLoader,
    TranDirective,
};

use serde::{Deserialize, Serialize};

/// What a behavioral B-source's expression drives (dev-plan 04 §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BOutput {
    /// `Bxxx n+ n- V={expr}`: the output port voltage is constrained to the
    /// expression value. Owns a branch-current unknown like an ideal Vsource.
    Voltage,
    /// `Bxxx n+ n- I={expr}`: the expression value flows as a current
    /// `p -> n`. No unknown of its own (an Isource-like injection).
    Current,
}

/// One dependency slot of a behavioral expression, positionally aligned with
/// the `__d{k}` variables in its [`CompiledExpr`] (slot `k` = `deps[k]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BDep {
    /// `V(node)`: reads a node voltage. A NODE reference; it must remap in
    /// [`Device::map_nodes`] and it is a SENSE terminal for the tear layers.
    Volt(NodeId),
    /// `I(vname)`: reads the branch current of an independent Vsource,
    /// resolved by the loader's resolve-by-name pass exactly like an F/H
    /// control (and retargeted by the same
    /// [`Device::retarget_controlling_source_slot`] machinery).
    Branch(DeviceId),
}

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
    /// `.ic V(node)=val` transient initial conditions: node voltages the
    /// `uic`-flagged transient starts from at `t = 0` (consumed by
    /// `transient.rs`). Empty for a deck without `.ic` cards.
    #[serde(default)]
    pub initial_conditions: Vec<(NodeId, f64)>,
    /// `.nodeset V(node)=val` DC convergence aids: seed values for the DC Newton
    /// start vector. Unlike `.ic` these are a GUESS, not a constraint; the
    /// solver may converge elsewhere (consumed by `newton::dc_solve`). Empty for
    /// a deck without `.nodeset` cards.
    #[serde(default)]
    pub nodesets: Vec<(NodeId, f64)>,
    /// Small-signal AC stimulus per source, captured from `AC <mag> [phase]`
    /// tokens on source cards (consumed by `hauksbee_solve::AcAnalysis`). Empty
    /// for a deck whose sources carry no `AC` spec; the honest signal that a
    /// `.ac` analysis would have a zero stimulus and must be refused rather than
    /// faked. The `DeviceId` is the flattened source's id, resolved post-splice.
    #[serde(default)]
    pub ac_stimulus: Vec<(DeviceId, AcStim)>,
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
            initial_conditions: Vec::new(),
            nodesets: Vec::new(),
            ac_stimulus: Vec::new(),
        }
    }

    /// Intern a node name, returning its id. `"0"` and `"gnd"`
    /// (case-insensitive) both map to ground.
    ///
    /// Matching is case-INSENSITIVE, like SPICE net identity and every other
    /// resolution path in this crate (`find_node`, subckt ports, device/model
    /// lookup). A case-sensitive match here split one net into two, `node("OUT")`
    /// and `node("Out")` returned different ids, and disagreed with the
    /// case-insensitive `find_node`, so a `.ic V(out)=…` card seeded only one of
    /// the halves. The first-seen casing is kept for display.
    pub fn node(&mut self, name: &str) -> NodeId {
        if name == "0" || name.eq_ignore_ascii_case("gnd") {
            return NodeId::GROUND;
        }
        if let Some(i) = self.node_names.iter().position(|n| n.eq_ignore_ascii_case(name)) {
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

    /// Look up an already-interned node by name (case-insensitive), honoring the
    /// `0`/`gnd` ground aliases. Unlike [`Circuit::node`] this does NOT create a
    /// new node for an unknown name; it returns `None`, so callers that must
    /// reference an EXISTING node (`.ic`/`.nodeset`) can refuse a typo rather
    /// than silently interning a floating node.
    pub fn find_node(&self, name: &str) -> Option<NodeId> {
        if name == "0" || name.eq_ignore_ascii_case("gnd") {
            return Some(NodeId::GROUND);
        }
        self.node_names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .map(|i| NodeId(i as u32))
    }

    /// All node names (including ground at index 0), for diagnostics such as
    /// did-you-mean suggestions on an unresolved `.ic`/`.nodeset` reference.
    pub fn node_names(&self) -> impl Iterator<Item = &str> {
        self.node_names.iter().map(String::as_str)
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
    /// `pole_hz`, when present, applies a single pole to the gain path, in
    /// `.ac` as the complex `gain / (1 + j·w/wp)`, in transient as a
    /// first-order lag of the driven output toward the clipped ideal target
    /// (time constant `1/(2π·pole_hz)`). `slew`, when present, additionally
    /// rate-limits the transient output in V/µs (the datasheet unit); it has
    /// no small-signal (`.ac`) effect, matching physics, slew is a
    /// large-signal limit. `None`/`0`/non-finite in either field degrades to
    /// the ideal instantaneous behavior for that mechanism.
    OpAmp {
        name: String,
        out: NodeId,
        inp: NodeId,
        inn: NodeId,
        reference: Option<NodeId>,
        gain: f64,
        pole_hz: Option<f64>,
        /// Output slew-rate limit in V/µs; `None`/`0` = unlimited.
        #[serde(default)]
        slew: Option<f64>,
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
    /// Voltage-controlled voltage source (SPICE `E` card):
    /// `V(p,n) = gain * V(cp,cn)`. Owns a branch-current unknown exactly like
    /// an ideal [`Device::Vsource`]; the control pair is read-only.
    Vcvs {
        name: String,
        p: NodeId,
        n: NodeId,
        cp: NodeId,
        cn: NodeId,
        gain: f64,
    },
    /// Voltage-controlled current source (SPICE `G` card):
    /// `I(p->n) = gm * V(cp,cn)`. Pure transconductance stamp, no branch
    /// unknown, no RHS; the control pair is read-only.
    Vccs {
        name: String,
        p: NodeId,
        n: NodeId,
        cp: NodeId,
        cn: NodeId,
        gm: f64,
    },
    /// Current-controlled current source (SPICE `F` card):
    /// `I(p->n) = gain * I(ctrl_src)`, where `ctrl_src` must be an independent
    /// [`Device::Vsource`] whose branch current is the control (the idiomatic
    /// probe is a zero-volt "ammeter" `V... a b 0` in series with the sensed
    /// wire). The first device variant that references ANOTHER DEVICE by id:
    /// the loader resolves the card's `vname` in a deferred second pass, and
    /// every pass that clones devices into a sub-circuit must retarget
    /// `ctrl_src` through [`Device::retarget_controlling_source`] or the id
    /// goes stale. No branch unknown of its own; two matrix entries in the
    /// control source's branch COLUMN.
    Cccs {
        name: String,
        p: NodeId,
        n: NodeId,
        ctrl_src: DeviceId,
        gain: f64,
    },
    /// Current-controlled voltage source (SPICE `H` card):
    /// `V(p,n) = transres * I(ctrl_src)`. Same control-by-branch-current
    /// coupling as [`Device::Cccs`]; additionally owns a branch-current
    /// unknown of its own, exactly like an ideal [`Device::Vsource`], because
    /// it fixes its output-port voltage.
    Ccvs {
        name: String,
        p: NodeId,
        n: NodeId,
        ctrl_src: DeviceId,
        transres: f64,
    },
    /// Behavioral B-source (SPICE `B` card, dev-plan 04 §2.5): the output is
    /// an arbitrary expression `f(V(...), I(...), time)`. `V={expr}` owns a
    /// branch unknown (constraint row `v_p - v_n - f = 0`); `I={expr}` injects
    /// `f` as a current `p -> n`. The expression is canonical (see
    /// [`CompiledExpr`]): its `__d{k}` variables align positionally with
    /// `deps[k]`, so cloning/remapping the device means keeping `expr` and
    /// `deps` in lock-step (the expression itself carries no node/device ids).
    /// `V(node)` deps are node references that MUST remap in `map_nodes`;
    /// `I(vname)` deps are device references resolved by the loader's
    /// name pass and retargeted per slot by sub-circuit extraction.
    Behavioral {
        name: String,
        p: NodeId,
        n: NodeId,
        output: BOutput,
        expr: CompiledExpr,
        deps: Vec<BDep>,
    },
    /// Mutual-inductance coupling (SPICE `K` card, dev-plan 04 §2.3):
    /// `Kxxx L1 L2 k` couples two existing [`Device::Inductor`]s with mutual
    /// inductance `M = k·sqrt(L1·L2)`. This is a RELATIONSHIP between two
    /// devices, not a stamped-per-Newton element: it has NO terminals of its
    /// own (empty `nodes()`), it stamps NOTHING itself, and the solver's
    /// inductor stamps consult the coupling map derived from these variants
    /// to add the symmetric `−M·(rule/dt)` cross terms between the windings'
    /// branch rows/columns plus each winding's mutual history voltage.
    /// `l1`/`l2` are device references like an F/H `ctrl_src`, resolved by the
    /// loader's name pass and retargeted per slot by sub-circuit extraction;
    /// they surface through [`Device::controlling_sources`] because the
    /// coupled stamp reads BOTH windings' branch-current unknowns, which is
    /// exactly the fusion/retargeting contract that accessor already carries.
    /// `0 < k <= 1` (validated at load); `k == 1` makes the group inductance
    /// matrix singular, which is legal on the card; the stamp uses L itself,
    /// never its inverse. Multiple K cards may chain 3+ windings; each card
    /// contributes one pairwise M.
    ///
    /// Fidelity (plan §2.3): this is the LOSSLESS LINEAR mutual model, which
    /// matches ngspice's `K` closely (the differential decks pin it).
    /// Saturating/hysteretic cores are UNSUPPORTED, no core model card
    /// parses, so a deck needing one refuses at load rather than running
    /// linear physics under a nonlinear name.
    Coupling {
        name: String,
        l1: DeviceId,
        l2: DeviceId,
        k: f64,
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
            | Device::Comparator { name, .. }
            | Device::Vcvs { name, .. }
            | Device::Vccs { name, .. }
            | Device::Cccs { name, .. }
            | Device::Ccvs { name, .. }
            | Device::Behavioral { name, .. }
            | Device::Coupling { name, .. } => name,
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
            Device::Vcvs { p, n, cp, cn, .. } | Device::Vccs { p, n, cp, cn, .. } => {
                vec![*p, *n, *cp, *cn]
            }
            // The control of an F/H is a DEVICE reference, not a node: the
            // only wires these touch are their output port.
            Device::Cccs { p, n, .. } | Device::Ccvs { p, n, .. } => vec![*p, *n],
            // Output port plus every V(node) dependency: like the E/G control
            // pair, a sensed node IS a node reference this device carries, so
            // it participates in node walks (partitioner union, max_node).
            // I(vname) deps are device references, not nodes (see F/H).
            Device::Behavioral { p, n, deps, .. } => {
                let mut v = vec![*p, *n];
                for d in deps {
                    if let BDep::Volt(dn) = d {
                        v.push(*dn);
                    }
                }
                v
            }
            // A coupling is a relationship between two inductors, not a wired
            // element: it touches NO nodes of its own (the windings' terminals
            // belong to the windings). Node walks (max_node, partitioner
            // unions, sub-circuit remapping) legitimately see nothing here;
            // the l1/l2 device references participate through
            // `controlling_sources` instead.
            Device::Coupling { .. } => Vec::new(),
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
            Device::Vcvs { p, n, cp, cn, .. } | Device::Vccs { p, n, cp, cn, .. } => {
                *p = f(*p);
                *n = f(*n);
                *cp = f(*cp);
                *cn = f(*cn);
            }
            // NOTE: `ctrl_src` is a DeviceId, not a node, node remapping does
            // not touch it. A pass that extracts devices into a sub-circuit
            // must ALSO call [`Device::retarget_controlling_source`] or the
            // reference silently points at whatever occupies that index in the
            // new circuit (see the partitioned executor's island build).
            Device::Cccs { p, n, .. } | Device::Ccvs { p, n, .. } => {
                *p = f(*p);
                *n = f(*n);
            }
            // The dep list carries the device's V(node) references: remap
            // every one IN PLACE, keeping slot order (the expression's __d{k}
            // variables are positional). BDep::Branch is a DeviceId, untouched
            // here, extraction passes retarget it via
            // `retarget_controlling_source_slot`, exactly like F/H.
            Device::Behavioral { p, n, deps, .. } => {
                *p = f(*p);
                *n = f(*n);
                for d in deps.iter_mut() {
                    if let BDep::Volt(dn) = d {
                        *dn = f(*dn);
                    }
                }
            }
            // Deliberately empty, NOT an oversight: a coupling carries no
            // NodeId at all, `l1`/`l2` are DeviceIds, the same reference
            // class as an F/H `ctrl_src`, and extraction passes retarget them
            // through `retarget_controlling_source_slot` (slots 0 and 1).
            Device::Coupling { .. } => {}
        }
    }

    /// The devices whose BRANCH CURRENTS this device reads, in slot order
    /// (the F/H control; a B-source's `I(vname)` deps; a K-coupling's two
    /// windings; the mutual stamp reads BOTH inductors' branch-current
    /// unknowns, so the coupling declares them here and inherits the fusion
    /// and retargeting contracts below wholesale). This is a sense
    /// declaration in a vocabulary [`Device::sense_nodes`] cannot express:
    /// the coupling is to another device's branch-current unknown, not to any
    /// node voltage. Plural since the B-source landed, an expression may
    /// read several ammeters; F/H return exactly one. Consumers:
    ///
    /// * the partitioner and the conduction graph, which must FUSE this
    ///   device's island with EVERY control source's island (a branch current
    ///   is not replayable across a Gauss-Seidel lag, and unlike a sensed node
    ///   voltage it cannot even be expressed as a boundary source, so this
    ///   coupling is never a tear candidate);
    /// * sub-circuit extraction, which must keep every control source in the
    ///   same sub-system (the stamp needs its branch column) and retarget each
    ///   id via [`Device::retarget_controlling_source_slot`];
    /// * tests, which must place a `Vsource` behind the id(s) before stamping.
    pub fn controlling_sources(&self) -> Vec<DeviceId> {
        // Exhaustive, no `_` arm: a future variant carrying a device reference
        // that fell into a catch-all empty-vec would be invisible to the
        // partitioner's fusion rule and to sub-circuit retargeting; the same
        // silent-drop hazard class as a `_` arm in `map_nodes`.
        match self {
            Device::Cccs { ctrl_src, .. } | Device::Ccvs { ctrl_src, .. } => vec![*ctrl_src],
            Device::Behavioral { deps, .. } => deps
                .iter()
                .filter_map(|d| match d {
                    BDep::Branch(id) => Some(*id),
                    BDep::Volt(_) => None,
                })
                .collect(),
            // Slot 0 = l1, slot 1 = l2. The mutual companion writes into (and
            // its history reads from) both windings' branch unknowns, so the
            // partitioner must union the two windings' islands (mutual flux is
            // never a tear candidate) and sub-circuit extraction must carry
            // both inductors along and retarget these ids, exactly the F/H
            // contracts this accessor already grants.
            Device::Coupling { l1, l2, .. } => vec![*l1, *l2],
            Device::Resistor { .. }
            | Device::Capacitor { .. }
            | Device::Inductor { .. }
            | Device::Vsource { .. }
            | Device::Isource { .. }
            | Device::Diode { .. }
            | Device::Bjt { .. }
            | Device::Mosfet { .. }
            | Device::VSwitch { .. }
            | Device::OpAmp { .. }
            | Device::Comparator { .. }
            | Device::Vcvs { .. }
            | Device::Vccs { .. } => Vec::new(),
        }
    }

    /// Rewrite the `slot`-th [`DeviceId`] of [`Device::controlling_sources`]
    /// (its index in that vec), in place. The device-reference analogue of
    /// [`Device::map_nodes`]: any pass that copies devices into a circuit with
    /// different device indices routes through here, once per slot, and the
    /// loader's resolve-by-name pass patches each deferred `vname` through its
    /// own slot. Panics on a slot the device does not have: a mis-addressed
    /// retarget is the silent-corruption bug class this API exists to prevent,
    /// so it fails loudly.
    pub fn retarget_controlling_source_slot(&mut self, slot: usize, new_id: DeviceId) {
        // Exhaustive for the same reason as `controlling_sources`.
        match self {
            Device::Cccs { ctrl_src, .. } | Device::Ccvs { ctrl_src, .. } => {
                assert_eq!(slot, 0, "F/H have exactly one control slot");
                *ctrl_src = new_id;
            }
            Device::Behavioral { deps, .. } => {
                let target = deps
                    .iter_mut()
                    .filter_map(|d| match d {
                        BDep::Branch(id) => Some(id),
                        BDep::Volt(_) => None,
                    })
                    .nth(slot);
                match target {
                    Some(id) => *id = new_id,
                    None => panic!("behavioral source has no branch-dep slot {slot}"),
                }
            }
            Device::Coupling { l1, l2, .. } => match slot {
                0 => *l1 = new_id,
                1 => *l2 = new_id,
                _ => panic!("coupling has exactly two winding slots, not slot {slot}"),
            },
            Device::Resistor { .. }
            | Device::Capacitor { .. }
            | Device::Inductor { .. }
            | Device::Vsource { .. }
            | Device::Isource { .. }
            | Device::Diode { .. }
            | Device::Bjt { .. }
            | Device::Mosfet { .. }
            | Device::VSwitch { .. }
            | Device::OpAmp { .. }
            | Device::Comparator { .. }
            | Device::Vcvs { .. }
            | Device::Vccs { .. } => {
                panic!("device without control references cannot retarget slot {slot}")
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
            // Level-1 channel current flows d<->s. The gate row receives
            // entries exactly when the model carries gate capacitance
            // (displacement current through the §3.3 charge companions), and
            // the bulk row exactly when it carries bulk-junction physics
            // (body-diode DC branch and/or depletion caps). A default model
            // (no cap/body fields) keeps the pre-§3.3 [d, s] classification
            // bit-identically, gate stays sense, bulk stays unstamped.
            Device::Mosfet { d, g, s, b, model, .. } => {
                let mut v = vec![*d, *s];
                if model.has_gate_charge() {
                    v.push(*g);
                }
                if model.has_body_diode() {
                    if let Some(b) = b {
                        v.push(*b);
                    }
                }
                v
            }
            // The switch channel conducts; the control pair only steers it.
            Device::VSwitch { a, b, .. } => vec![*a, *b],
            // Behavioral output stages drive their out node through a 1 Ohm
            // Thevenin; the inputs are ideal (infinite input impedance) and the
            // opamp reference only offsets the target voltage (its own row gets
            // nothing; it appears as a column of the out row).
            Device::OpAmp { out, .. } => vec![*out],
            Device::Comparator { out, .. } => vec![*out],
            // Controlled sources drive current through their output port only:
            // the VCVS branch current enters the p/n KCL rows, the VCCS
            // transconductance current likewise. The control pair appears
            // exclusively as matrix COLUMNS of other rows; this is the
            // canonical sense-vs-conduction split the W1 classifier was built
            // for, and the solve-side cross-check test holds the stamp to it.
            Device::Vcvs { p, n, .. } | Device::Vccs { p, n, .. } => vec![*p, *n],
            // F/H drive current through their output port; the control is not
            // a terminal at all (see `controlling_sources`).
            Device::Cccs { p, n, .. } | Device::Ccvs { p, n, .. } => vec![*p, *n],
            // A B-source drives current through its output port only. Every
            // V(node) dep is read-only (Jacobian COLUMNS of the p/n or branch
            // rows); the maximal-coupling device is still, per terminal, the
            // same conduct-vs-sense split as a VCVS.
            Device::Behavioral { p, n, .. } => vec![*p, *n],
            // A coupling has NO terminals: current flows through the WINDINGS,
            // whose own a/b declarations above carry the conduction claim. The
            // stamp cross-check holds this to account trivially (the coupling
            // variant stamps nothing itself; the mutual terms land in the
            // windings' branch rows, which are not node rows). The inter-
            // winding solver coupling is declared via `controlling_sources`,
            // the branch-current vocabulary, exactly like F/H.
            Device::Coupling { .. } => Vec::new(),
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
            // Exactly the complement of the conduction claim above: gate and
            // bulk are sense terminals only while the model gives their rows
            // nothing (no gate caps / no bulk junctions, dev-plan 04 §3.3).
            Device::Mosfet { g, b, model, .. } => {
                let mut v = Vec::new();
                if !model.has_gate_charge() {
                    v.push(*g);
                }
                if let Some(b) = b {
                    if !model.has_body_diode() {
                        v.push(*b);
                    }
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
            // The control pair steers the gain term but its own KCL rows
            // receive nothing (ideal infinite input impedance).
            Device::Vcvs { cp, cn, .. } | Device::Vccs { cp, cn, .. } => vec![*cp, *cn],
            // DELIBERATELY EMPTY, and a vocabulary gap made explicit: what an
            // F/H senses is another device's BRANCH CURRENT, which this
            // node-voltage sense list cannot express. Declaring the control
            // source's nodes here would be a lie; the stamp never reads their
            // voltages (it reads the control branch-current COLUMN), and the
            // zero-row cross-check would pass vacuously while the tearing
            // passes drew the wrong conclusion (a node-voltage sense is a free
            // tear candidate; a branch-current sense never is). The coupling
            // is declared through [`Device::controlling_sources`] instead, and
            // the partitioner/conduction graph consume that directly.
            Device::Cccs { .. } | Device::Ccvs { .. } => Vec::new(),
            // Every V(node) dep is a declared SENSE edge; the tear layers
            // must see that this device reads across island boundaries (plan
            // §2.5). Deduped, and EXCLUDING the output terminals: a
            // self-referencing expression (`B1 out 0 I={tanh(V(out))}`, the
            // nonlinear-resistor idiom) senses a node it also conducts into,
            // and a terminal must be in exactly one of the two sets; the
            // conduction claim wins because the p/n rows really do receive
            // current. I(vname) deps are branch-current reads with no node
            // vocabulary, declared via `controlling_sources` like F/H.
            Device::Behavioral { p, n, deps, .. } => {
                let mut v: Vec<NodeId> = deps
                    .iter()
                    .filter_map(|d| match d {
                        BDep::Volt(dn) if dn != p && dn != n => Some(*dn),
                        _ => None,
                    })
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            }
            // No node-voltage reads either: what a coupling "senses" is both
            // windings' BRANCH CURRENTS, which this node vocabulary cannot
            // express (the F/H precedent, same reasoning verbatim), declared
            // through `controlling_sources`, never as a free-tear candidate.
            Device::Coupling { .. } => Vec::new(),
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
                slew: None,
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
            Device::Vcvs {
                name: "Eex".into(),
                p: n[0],
                n: n[1],
                cp: n[2],
                cn: n[3],
                gain: 10.0,
            },
            Device::Vccs {
                name: "Gex".into(),
                p: n[0],
                n: n[1],
                cp: n[2],
                cn: n[3],
                gm: 1e-3,
            },
            // CONVENTION (load-bearing for every consumer that stamps these):
            // the F/H/B examples point their control at `DeviceId(0)`. A
            // consumer that builds a circuit around an example must ensure
            // device index 0 is an independent Vsource BEFORE adding the
            // example (the conduction cross-check in hauksbee-solve inserts a
            // 0 V ammeter from n[3] to ground first for exactly this reason,
            // n[3]-to-ground so the ammeter's own incidence entries stay off
            // n[2], which the Behavioral example declares as a SENSE node
            // whose row must provably receive nothing). Consumers that only
            // inspect structure (serde round-trip, node walks) need nothing:
            // DeviceId round-trips like any field.
            Device::Cccs {
                name: "Fex".into(),
                p: n[0],
                n: n[1],
                ctrl_src: DeviceId(0),
                gain: 2.0,
            },
            Device::Ccvs {
                name: "Hex".into(),
                p: n[0],
                n: n[1],
                ctrl_src: DeviceId(0),
                transres: 50.0,
            },
            // Exercises every structural feature at once: a V(node) dep
            // (slot 0, remappable node reference), an I(vname) dep (slot 1,
            // the DeviceId(0) control convention above), a `time` read, and a
            // Voltage output (owns a branch unknown). The canonical expression
            // compiles here so every property-test consumer gets a genuinely
            // evaluable device; `expect` is safe; the text is a constant.
            Device::Behavioral {
                name: "Bex".into(),
                p: n[0],
                n: n[1],
                output: BOutput::Voltage,
                expr: CompiledExpr::compile("0.5*__d0 + 100.0*math::tanh(__d1) + 0.1*time")
                    .expect("examples(): canonical behavioral expression compiles"),
                deps: vec![BDep::Volt(n[2]), BDep::Branch(DeviceId(0))],
            },
            // CONVENTION (the coupling analogue of the F/H/B ammeter rule
            // above): the Coupling example points its windings at DeviceId(0)
            // and DeviceId(1). A consumer that builds a STAMPABLE circuit
            // around it must place two Inductors at indices 0 and 1 before
            // adding it (the solve-side cross-check does exactly that);
            // structure-only consumers (serde, node walks) need nothing.
            // Named `KMex`, not `Kex`: the Comparator example above already
            // wears `Kex`, and a duplicate refdes would poison any consumer
            // that indexes examples by name.
            Device::Coupling {
                name: "KMex".into(),
                l1: DeviceId(0),
                l2: DeviceId(1),
                k: 0.9,
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
    /// that contains them. `Device::Behavioral` is nonlinear BY CONSTRUCTION
    /// (an arbitrary expression's Jacobian moves with the iterate, even a
    /// linear-looking `{2*V(a)}` ships the finite-difference tangent path), so
    /// it deliberately stays off this whitelist and taints its island.
    pub fn is_linear(&self) -> bool {
        matches!(
            self,
            Device::Resistor { .. }
                | Device::Capacitor { .. }
                | Device::Inductor { .. }
                | Device::Vsource { .. }
                | Device::Isource { .. }
                // Controlled sources E/G: the stamp is a CONSTANT gain/gm times
                // node voltages, so the Jacobian never moves, linear. Linear
                // does NOT mean state-space-reducible: `LinearIsland::compile`
                // must still explicitly refuse islands containing them (it
                // models only R/C/L/I) or they would vanish from the A matrix.
                | Device::Vcvs { .. }
                | Device::Vccs { .. }
                // Controlled sources F/H: constant gain/transresistance times
                // a branch-current UNKNOWN, still constant matrix entries, so
                // the Jacobian never moves. Same caveat as E/G: linear does
                // not mean state-space-reducible, and `LinearIsland::compile`
                // refuses islands containing them.
                | Device::Cccs { .. }
                | Device::Ccvs { .. }
                // Coupled inductors: the mutual inductance M = k*sqrt(L1*L2)
                // is a CONSTANT; the companion cross terms scale with 1/dt
                // like any inductor's, never with the iterate, so coupling
                // never taints an island nonlinear. Linear does NOT mean
                // state-space-reducible: `LinearIsland::compile` refuses
                // islands containing a Coupling (the reduction would need the
                // group inductance matrix INVERTED, and k == 1 makes it
                // legally singular), routing them to MNA which stamps L
                // directly.
                | Device::Coupling { .. }
        )
    }

    /// Whether the device is best handled by the event queue rather than the
    /// analog solver. The behavioral comparator is the one digital-ish element
    /// here; the partitioning pass will use this to peel it off. Controlled
    /// sources (`Vcvs`/`Vccs`/`Cccs`/`Ccvs`) are continuous analog constraints
    /// with no discrete state, so they stay with the analog solver (`false`).
    pub fn is_event_driven(&self) -> bool {
        matches!(self, Device::Comparator { .. })
    }
}
