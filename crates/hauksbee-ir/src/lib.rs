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
pub mod boot;
pub mod connector_pinouts;
pub mod debug;
pub mod docs_url;
pub mod evidence;
pub mod glossary;
pub mod header_pinouts;
pub mod labels;
mod models;
pub mod observations;
pub mod order;
pub mod peripheral_limits;
pub mod pin_functions;
pub mod progress;
pub mod run_record;
mod source;
mod spice;
pub mod terminal_safe;
pub mod units;
pub mod vocabulary;

pub use bexpr::CompiledExpr;
pub use docs_url::docs_url;
pub use models::{
    thermal_voltage as thermal_voltage_c, BjtModel, DiodeModel, MosLevel, MosfetModel, Polarity,
    GENERIC_BODY_DIODE_IS,
};
pub use source::{AcStim, PwlPoint, SourceKind};
pub use spice::{
    levenshtein, parse_spice_number, AcDirective, AcSweep, DcDirective, DcSweep, Directives,
    PrintRequest, SpiceError, SpiceLoader, TranDirective,
};

use serde::{Deserialize, Serialize};

/// `s` unchanged when it fits in `max` characters, else its first `max - 1`
/// characters followed by `…`, so the result is never longer than `max`.
#[must_use]
pub fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('\u{2026}');
        t
    }
}

/// What a behavioral B-source's expression drives.
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
    pub const GROUND: Self = Self(0);

    /// True for the ground reference node.
    #[must_use]
    pub const fn is_ground(self) -> bool {
        self.0 == 0
    }
}

/// A node or device index as the `u32` its id wraps.
///
/// # Panics
///
/// Past `u32::MAX` nodes or devices, where a wrapped id would alias another.
fn id_u32(i: usize) -> u32 {
    u32::try_from(i).expect("a circuit holds at most u32::MAX nodes and devices")
}

/// Stable handle to a device within a [`Circuit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub u32);

/// An op-amp output saturation level, `V(pin) + offset` volts.
///
/// `pin` is the part's supply pad when the card gives a datasheet output-swing
/// HEADROOM from it (so a sagging, ramping or split supply is honoured), and
/// ground for a plain fixed level ([`Rail::fixed`]). A supply pad is a SENSE
/// terminal, read at the current iterate: explicit, no Jacobian column, exact
/// on convergence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rail {
    pub pin: NodeId,
    pub offset: f64,
}

impl Rail {
    /// A fixed level in volts, tracking no supply pin.
    #[must_use]
    pub const fn fixed(offset: f64) -> Self {
        Self {
            pin: NodeId::GROUND,
            offset,
        }
    }

    /// The ORDERED `(lo, hi)` band a pair of rails resolves to, each read as
    /// `V(pin) + offset` from a node-voltage reader. A supply that has not come
    /// up (the DC homotopy starts every source at zero, and a ramping rail
    /// passes through it) puts a headroom ceiling below the floor; the band
    /// collapses onto the floor there instead of inverting.
    pub fn band(lo: Self, hi: Self, v: impl Fn(NodeId) -> f64) -> (f64, f64) {
        let (lo, hi) = (v(lo.pin) + lo.offset, v(hi.pin) + hi.offset);
        (lo, hi.max(lo))
    }
}

/// The supply pins a behavioral output stage draws its output current from.
///
/// The stage drives its output node from an ideal EMF behind a 1 ohm
/// conductance whose return is ground, so without this a load current would
/// close through ground and the part's supply pins would never see it. With
/// `hi`/`lo` set, the current the stage delivers is instead drawn from `hi`
/// (sourcing) or returned into `lo` (sinking), blended smoothly through zero
/// ([`Supply::share_hi`]). A `None` side is not a pin the part has on a net:
/// that share returns through ground as before. Both are CONDUCTION terminals.
///
/// The part's own quiescent current (`quiescent_a`) is drawn `hi -> lo` as well,
/// once the supply is up ([`Supply::quiescent_draw`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Supply {
    /// The positive supply pin (`V+`, `VCC`, `VS`).
    pub hi: Option<NodeId>,
    /// The negative supply pin (`V-`, `VEE`), or `None` when it is ground.
    pub lo: Option<NodeId>,
    /// The package's quiescent supply current in amps (the datasheet's ICC with
    /// the outputs unloaded), drawn `hi -> lo`; `0` draws nothing. Carried on one
    /// output stage of a package, not on each.
    #[serde(default)]
    pub quiescent_a: f64,
}

impl Supply {
    /// The output current (A) over which the draw changes sides: below it the
    /// part draws from both pins, above it from one.
    pub const BLEND_A: f64 = 1e-6;

    /// The fraction of a delivered output current `amps` that is drawn from
    /// `hi`; the rest returns into `lo`. A smooth step through 1/2 at no load.
    #[must_use]
    pub fn share_hi(amps: f64) -> f64 {
        0.5 * (1.0 + (amps / Self::BLEND_A).tanh())
    }

    /// The knee of the quiescent draw ([`Supply::quiescent_draw`]), volts.
    pub const QUIESCENT_KNEE_V: f64 = 0.5;

    /// The quiescent draw `hi -> lo` at supply voltage `v = V(hi) - V(lo)`, and
    /// its slope `d/dv`: `quiescent_a * tanh(v / QUIESCENT_KNEE_V)`.
    ///
    /// A plain current source here would pull a `hi` pin nothing feeds to
    /// `-quiescent_a / gmin`, billions of volts below ground, and the solve
    /// would be refused; on a rail fed only through resistors it drags the rail
    /// volts negative (an unfed analog supply on a large board sat many volts
    /// below ground under a bank of comparators). Around 0 V this is a conductance of
    /// `quiescent_a / QUIESCENT_KNEE_V`, the part's bias circuitry out of
    /// headroom, so an unfed supply rests at 0 V; a reversed one returns at
    /// most `quiescent_a`, as the substrate junction would conduct. From 1.5 V
    /// up the draw is the card's figure within 0.5% (1e-5 at 3 V), with a
    /// slope that vanishes as fast, so a powered part solves as the plain
    /// source did. A tanh rather than a clamped fade: no flat region, so Newton
    /// always sees the conductance that holds a weakly fed rail.
    #[must_use]
    pub fn quiescent_draw(self, v: f64) -> (f64, f64) {
        let t = (v / Self::QUIESCENT_KNEE_V).tanh();
        let amps = self.quiescent_a;
        (amps * t, amps * t.mul_add(-t, 1.0) / Self::QUIESCENT_KNEE_V)
    }

    /// The supply nodes that are not ground, `hi` then `lo`.
    #[must_use]
    pub fn nodes(self) -> Vec<NodeId> {
        [self.hi, self.lo]
            .into_iter()
            .flatten()
            .filter(|n| !n.is_ground())
            .collect()
    }
}

/// A circuit: interned node names, devices, and a global temperature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Circuit {
    /// Node names indexed by `NodeId.0`. Entry 0 is the ground name.
    node_names: Vec<String>,
    /// `node_names[1..]` by lower-cased name, so interning a name is a lookup, not
    /// a scan of every node (a board of 100k nets cost 100k squared compares).
    /// Not serialized; a circuit read back rebuilds it on its first `node` call.
    #[serde(skip)]
    node_index: std::collections::HashMap<String, u32>,
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
        Self::new()
    }
}

impl Circuit {
    /// Empty circuit with only the ground node defined and `temp_c = 27`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            node_names: vec!["0".to_string()],
            node_index: std::collections::HashMap::new(),
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
        if self.node_index.len() + 1 != self.node_names.len() {
            self.node_index = (self.node_names.iter().enumerate().skip(1))
                .rev()
                .map(|(i, n)| (n.to_ascii_lowercase(), id_u32(i)))
                .collect();
        }
        let lowered = name.to_ascii_lowercase();
        if let Some(&i) = self.node_index.get(&lowered) {
            return NodeId(i);
        }
        let id = id_u32(self.node_names.len());
        self.node_names.push(name.to_string());
        self.node_index.insert(lowered, id);
        NodeId(id)
    }

    /// Name of a node, or `"?"` if the id is out of range.
    pub fn node_name(&self, id: NodeId) -> &str {
        self.node_names
            .get(id.0 as usize)
            .map_or("?", String::as_str)
    }

    /// Number of nodes including ground.
    #[must_use]
    pub const fn node_count(&self) -> usize {
        self.node_names.len()
    }

    /// Look up an already-interned node by name (case-insensitive), honoring the
    /// `0`/`gnd` ground aliases. Unlike [`Circuit::node`] this does NOT create a
    /// new node for an unknown name; it returns `None`, so callers that must
    /// reference an EXISTING node (`.ic`/`.nodeset`) can refuse a typo rather
    /// than silently interning a floating node.
    #[must_use]
    pub fn find_node(&self, name: &str) -> Option<NodeId> {
        if name == "0" || name.eq_ignore_ascii_case("gnd") {
            return Some(NodeId::GROUND);
        }
        if self.node_index.len() + 1 == self.node_names.len() {
            return self
                .node_index
                .get(&name.to_ascii_lowercase())
                .map(|&i| NodeId(i));
        }
        self.node_names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .map(|i| NodeId(id_u32(i)))
    }

    /// All node names (including ground at index 0), for diagnostics such as
    /// did-you-mean suggestions on an unresolved `.ic`/`.nodeset` reference.
    pub fn node_names(&self) -> impl Iterator<Item = &str> {
        self.node_names.iter().map(String::as_str)
    }

    /// Append a device and return its id.
    pub fn add(&mut self, device: Device) -> DeviceId {
        let id = DeviceId(id_u32(self.devices.len()));
        self.devices.push(device);
        id
    }

    /// Iterate `(DeviceId, &Device)` over all devices.
    pub fn iter(&self) -> impl Iterator<Item = (DeviceId, &Device)> {
        self.devices
            .iter()
            .enumerate()
            .map(|(i, d)| (DeviceId(id_u32(i)), d))
    }

    /// Highest non-ground node id referenced by any device, used to size the
    /// MNA system. Returns 0 if the circuit touches only ground.
    pub fn max_node(&self) -> u32 {
        self.devices
            .iter()
            .flat_map(Device::nodes)
            .map(|n| n.0)
            .max()
            .unwrap_or(0)
            .max(id_u32(self.node_names.len().saturating_sub(1)))
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
        rail_lo: Rail,
        rail_hi: Rail,
        /// The supply pins the output current is drawn from (see [`Supply`]).
        #[serde(default)]
        supply: Supply,
    },
    /// Behavioral comparator: output snaps to a rail at the threshold.
    Comparator {
        name: String,
        out: NodeId,
        inp: NodeId,
        inn: NodeId,
        out_lo: f64,
        out_hi: f64,
        /// FULL band width in volts, the datasheet VHYS (VT+ minus VT-), not
        /// the offset: the output rises at `V(inp) - V(inn) > +hysteresis/2`
        /// and falls below `-hysteresis/2`. `0` is a bare zero-crossing.
        hysteresis: f64,
        /// Propagation delay in seconds, input crossing to output mid-swing:
        /// the card's `tpd_s`. A transient march whose comparators latch
        /// their decisions between steps (`hauksbee_solve::stamp::ComparatorLatch`)
        /// holds the output for `tpd_s / 2` after a decision, then slews it
        /// linearly across its swing in `tpd_s`, so it passes mid-swing
        /// `tpd_s` after the decision; a decision reverting inside the hold
        /// never reaches the output. `0` (no stated delay) is an output that
        /// steps at the decision.
        #[serde(default)]
        tpd_s: f64,
        /// The supply pins a driven output's current is drawn from (see
        /// [`Supply`]).
        #[serde(default)]
        supply: Supply,
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
    /// `ctrl_src` through [`Device::retarget_controlling_source_slot`] or the id
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
    /// Behavioral B-source (SPICE `B` card): the output is
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
    /// Mutual-inductance coupling (SPICE `K` card):
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
    /// Fidelity: this is the LOSSLESS LINEAR mutual model, which
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

/// Smallest resistance the solver stamps, in ohms.
///
/// A resistor at or below this value (a 0 ohm jumper, a 1e-9 ohm typo) is
/// stamped as exactly this much: its conductance is the largest the matrix is
/// asked to hold, and a stiffer link would swamp the milli-siemens entries
/// around it. Every stamping path (interpreted, planned, AC, linear fast path)
/// and the SPICE loader share this one constant so they cannot disagree; a
/// front end that substitutes it says so (the SPICE loader adds a note to
/// [`Directives::notes`]).
pub const MIN_RESISTANCE_OHMS: f64 = 1e-6;

impl Device {
    /// A linear resistor with no temperature coefficient.
    #[must_use]
    pub fn resistor(name: impl Into<String>, a: NodeId, b: NodeId, ohms: f64) -> Self {
        Self::Resistor {
            name: name.into(),
            a,
            b,
            ohms,
            tc1: None,
        }
    }

    /// A linear capacitor with no initial condition.
    #[must_use]
    pub fn capacitor(name: impl Into<String>, a: NodeId, b: NodeId, farads: f64) -> Self {
        Self::Capacitor {
            name: name.into(),
            a,
            b,
            farads,
            ic: None,
        }
    }

    /// A linear inductor with no initial condition.
    #[must_use]
    pub fn inductor(name: impl Into<String>, a: NodeId, b: NodeId, henries: f64) -> Self {
        Self::Inductor {
            name: name.into(),
            a,
            b,
            henries,
            ic: None,
        }
    }

    /// An independent voltage source driving `p` relative to `n`.
    #[must_use]
    pub fn vsource(name: impl Into<String>, p: NodeId, n: NodeId, kind: SourceKind) -> Self {
        Self::Vsource {
            name: name.into(),
            p,
            n,
            kind,
        }
    }

    /// An independent current source pushing current from `p` to `n`.
    #[must_use]
    pub fn isource(name: impl Into<String>, p: NodeId, n: NodeId, kind: SourceKind) -> Self {
        Self::Isource {
            name: name.into(),
            p,
            n,
            kind,
        }
    }

    /// A behavioural source over node voltages: `__d{k}` in `expr` reads
    /// `V(volts[k])`.
    ///
    /// # Errors
    /// When `expr` does not compile (see [`CompiledExpr::compile`]).
    pub fn behavioral(
        name: impl Into<String>,
        (p, n): (NodeId, NodeId),
        output: BOutput,
        expr: &str,
        volts: &[NodeId],
    ) -> Result<Self, String> {
        Ok(Self::Behavioral {
            name: name.into(),
            p,
            n,
            output,
            expr: CompiledExpr::compile(expr)?,
            deps: volts.iter().copied().map(BDep::Volt).collect(),
        })
    }

    /// Reference designator (`R1`, `Q3`, ...).
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Resistor { name, .. }
            | Self::Capacitor { name, .. }
            | Self::Inductor { name, .. }
            | Self::Vsource { name, .. }
            | Self::Isource { name, .. }
            | Self::Diode { name, .. }
            | Self::Bjt { name, .. }
            | Self::Mosfet { name, .. }
            | Self::VSwitch { name, .. }
            | Self::OpAmp { name, .. }
            | Self::Comparator { name, .. }
            | Self::Vcvs { name, .. }
            | Self::Vccs { name, .. }
            | Self::Cccs { name, .. }
            | Self::Ccvs { name, .. }
            | Self::Behavioral { name, .. }
            | Self::Coupling { name, .. } => name,
        }
    }

    /// All nodes this device connects to (may include ground).
    #[must_use]
    pub fn nodes(&self) -> Vec<NodeId> {
        match self {
            Self::Resistor { a, b, .. }
            | Self::Capacitor { a, b, .. }
            | Self::Inductor { a, b, .. }
            | Self::Diode { a, k: b, .. } => vec![*a, *b],
            // The control of an F/H is a DEVICE reference, not a node: the
            // only wires these touch are their output port.
            Self::Vsource { p, n, .. }
            | Self::Isource { p, n, .. }
            | Self::Cccs { p, n, .. }
            | Self::Ccvs { p, n, .. } => vec![*p, *n],
            Self::Bjt { c, b, e, .. } => vec![*c, *b, *e],
            Self::Mosfet { d, g, s, b, .. } => [*d, *g, *s].into_iter().chain(*b).collect(),
            Self::VSwitch {
                a,
                b,
                ctrl_p,
                ctrl_n,
                ..
            } => vec![*a, *b, *ctrl_p, *ctrl_n],
            Self::OpAmp {
                out,
                inp,
                inn,
                reference,
                rail_lo,
                rail_hi,
                supply,
                ..
            } => {
                let mut nodes = vec![*out, *inp, *inn];
                nodes.extend(*reference);
                nodes.extend([rail_lo.pin, rail_hi.pin].iter().filter(|n| !n.is_ground()));
                let held = nodes.clone();
                nodes.extend(supply.nodes().into_iter().filter(|n| !held.contains(n)));
                nodes
            }
            Self::Comparator {
                out,
                inp,
                inn,
                supply,
                ..
            } => {
                let mut nodes = vec![*out, *inp, *inn];
                let held = nodes.clone();
                nodes.extend(supply.nodes().into_iter().filter(|n| !held.contains(n)));
                nodes
            }
            Self::Vcvs { p, n, cp, cn, .. } | Self::Vccs { p, n, cp, cn, .. } => {
                vec![*p, *n, *cp, *cn]
            }
            // Output port plus every V(node) dependency: like the E/G control
            // pair, a sensed node IS a node reference this device carries, so
            // it participates in node walks (partitioner union, max_node).
            // I(vname) deps are device references, not nodes (see F/H).
            Self::Behavioral { p, n, deps, .. } => {
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
            Self::Coupling { .. } => Vec::new(),
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
    /// sub-circuit with no error. Optional terminals (`Mosfet` bulk, `OpAmp`
    /// reference) are remapped only when present.
    pub fn map_nodes(&mut self, f: &mut impl FnMut(NodeId) -> NodeId) {
        match self {
            Self::Resistor { a, b, .. }
            | Self::Capacitor { a, b, .. }
            | Self::Inductor { a, b, .. }
            | Self::Diode { a, k: b, .. } => {
                *a = f(*a);
                *b = f(*b);
            }
            Self::Vsource { p, n, .. }
            | Self::Isource { p, n, .. }
            | Self::Cccs { p, n, .. }
            | Self::Ccvs { p, n, .. } => {
                *p = f(*p);
                *n = f(*n);
            }
            Self::Bjt { c, b, e, .. } => {
                *c = f(*c);
                *b = f(*b);
                *e = f(*e);
            }
            Self::Mosfet { d, g, s, b, .. } => {
                for x in [d, g, s].into_iter().chain(b.as_mut()) {
                    *x = f(*x);
                }
            }
            Self::VSwitch {
                a,
                b,
                ctrl_p,
                ctrl_n,
                ..
            } => {
                for x in [a, b, ctrl_p, ctrl_n] {
                    *x = f(*x);
                }
            }
            Self::OpAmp {
                out,
                inp,
                inn,
                reference,
                rail_lo,
                rail_hi,
                supply,
                ..
            } => {
                for x in [out, inp, inn].into_iter().chain(reference.as_mut()) {
                    *x = f(*x);
                }
                for x in [supply.hi.as_mut(), supply.lo.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    *x = f(*x);
                }
                // A fixed rail's ground sentinel is not a terminal, so it is
                // NOT remapped: sub-circuit extraction would otherwise move it
                // onto a real net and change the level.
                for r in [rail_lo, rail_hi] {
                    if !r.pin.is_ground() {
                        r.pin = f(r.pin);
                    }
                }
            }
            Self::Comparator {
                out,
                inp,
                inn,
                supply,
                ..
            } => {
                *out = f(*out);
                *inp = f(*inp);
                *inn = f(*inn);
                for x in [supply.hi.as_mut(), supply.lo.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    *x = f(*x);
                }
            }
            Self::Vcvs { p, n, cp, cn, .. } | Self::Vccs { p, n, cp, cn, .. } => {
                for x in [p, n, cp, cn] {
                    *x = f(*x);
                }
            }
            // NOTE: `ctrl_src` is a DeviceId, not a node, node remapping does
            // not touch it. A pass that extracts devices into a sub-circuit
            // must ALSO call [`Device::retarget_controlling_source`] or the
            // reference silently points at whatever occupies that index in the
            // new circuit (see the partitioned executor's island build).
            // The dep list carries the device's V(node) references: remap
            // every one IN PLACE, keeping slot order (the expression's __d{k}
            // variables are positional). BDep::Branch is a DeviceId, untouched
            // here, extraction passes retarget it via
            // `retarget_controlling_source_slot`, exactly like F/H.
            Self::Behavioral { p, n, deps, .. } => {
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
            Self::Coupling { .. } => {}
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
    #[must_use]
    pub fn controlling_sources(&self) -> Vec<DeviceId> {
        // Exhaustive, no `_` arm: a future variant carrying a device reference
        // that fell into a catch-all empty-vec would be invisible to the
        // partitioner's fusion rule and to sub-circuit retargeting; the same
        // silent-drop hazard class as a `_` arm in `map_nodes`.
        match self {
            Self::Cccs { ctrl_src, .. } | Self::Ccvs { ctrl_src, .. } => vec![*ctrl_src],
            Self::Behavioral { deps, .. } => deps
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
            Self::Coupling { l1, l2, .. } => vec![*l1, *l2],
            Self::Resistor { .. }
            | Self::Capacitor { .. }
            | Self::Inductor { .. }
            | Self::Vsource { .. }
            | Self::Isource { .. }
            | Self::Diode { .. }
            | Self::Bjt { .. }
            | Self::Mosfet { .. }
            | Self::VSwitch { .. }
            | Self::OpAmp { .. }
            | Self::Comparator { .. }
            | Self::Vcvs { .. }
            | Self::Vccs { .. } => Vec::new(),
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
    ///
    /// # Panics
    ///
    /// When `slot` is not one of the device's control slots, including any slot
    /// on a device that has no controlling source.
    pub fn retarget_controlling_source_slot(&mut self, slot: usize, new_id: DeviceId) {
        // Exhaustive for the same reason as `controlling_sources`.
        match self {
            Self::Cccs { ctrl_src, .. } | Self::Ccvs { ctrl_src, .. } => {
                assert_eq!(slot, 0, "F/H have exactly one control slot");
                *ctrl_src = new_id;
            }
            Self::Behavioral { deps, .. } => {
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
            Self::Coupling { l1, l2, .. } => match slot {
                0 => *l1 = new_id,
                1 => *l2 = new_id,
                _ => panic!("coupling has exactly two winding slots, not slot {slot}"),
            },
            Self::Resistor { .. }
            | Self::Capacitor { .. }
            | Self::Inductor { .. }
            | Self::Vsource { .. }
            | Self::Isource { .. }
            | Self::Diode { .. }
            | Self::Bjt { .. }
            | Self::Mosfet { .. }
            | Self::VSwitch { .. }
            | Self::OpAmp { .. }
            | Self::Comparator { .. }
            | Self::Vcvs { .. }
            | Self::Vccs { .. } => {
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
    /// blocks fuse through a wire that carries no current. The claim made
    /// here is about the *stamp as implemented today*, not the physical part:
    /// a MOSFET gate conducts no current in the Level-1 stamp, so it is sense,
    /// and the day gate charge lands the classification here must change with
    /// it. That drift is caught mechanically: the cross-check test in
    /// `hauksbee-solve` stamps every example device and fails if a declared
    /// sense node's row receives anything, so this declaration cannot silently
    /// disagree with the stamp.
    #[must_use]
    pub fn conduction_nodes(&self) -> Vec<NodeId> {
        match self {
            // Controlled sources drive current through their output port only:
            // the VCVS branch current enters the p/n KCL rows, the VCCS
            // transconductance current likewise. The control pair appears
            // exclusively as matrix COLUMNS of other rows; this is the
            // canonical sense-vs-conduction split the W1 classifier was built
            // for, and the solve-side cross-check test holds the stamp to it.
            // F/H drive current through their output port; the control is not
            // a terminal at all (see `controlling_sources`).
            // A B-source drives current through its output port only. Every
            // V(node) dep is read-only (Jacobian COLUMNS of the p/n or branch
            // rows); the maximal-coupling device is still, per terminal, the
            // same conduct-vs-sense split as a VCVS.
            Self::Vsource { p, n, .. }
            | Self::Isource { p, n, .. }
            | Self::Vcvs { p, n, .. }
            | Self::Vccs { p, n, .. }
            | Self::Cccs { p, n, .. }
            | Self::Ccvs { p, n, .. }
            | Self::Behavioral { p, n, .. } => vec![*p, *n],
            Self::Bjt { c, b, e, .. } => vec![*c, *b, *e],
            // Level-1 channel current flows d<->s. The gate row receives
            // entries exactly when the model carries gate capacitance
            // (displacement current through the charge companions), and
            // the bulk row exactly when it carries bulk-junction physics
            // (body-diode DC branch and/or depletion caps). A default model
            // (no cap/body fields) keeps the plain [d, s] classification
            // bit-identically, gate stays sense, bulk stays unstamped.
            Self::Mosfet {
                d, g, s, b, model, ..
            } => {
                let mut v = vec![*d, *s];
                if model.has_gate_charge() {
                    v.push(*g);
                }
                if model.has_body_diode() {
                    v.extend(*b);
                }
                v
            }
            // Two-terminal parts conduct end to end; a switch's channel
            // conducts while its control pair only steers it.
            Self::Resistor { a, b, .. }
            | Self::Capacitor { a, b, .. }
            | Self::Inductor { a, b, .. }
            | Self::Diode { a, k: b, .. }
            | Self::VSwitch { a, b, .. } => vec![*a, *b],
            // Behavioral output stages drive their out node through a 1 Ohm
            // Thevenin; the inputs are ideal (infinite input impedance) and the
            // opamp reference only offsets the target voltage (its own row gets
            // nothing; it appears as a column of the out row).
            Self::OpAmp { out, supply, .. } => {
                // The supply pins carry the output current (see [`Supply`]).
                let mut v = vec![*out];
                v.extend(supply.nodes().into_iter().filter(|n| n != out));
                v
            }
            Self::Comparator { out, supply, .. } => {
                // The supply pins carry the output current (see [`Supply`]).
                let mut v = vec![*out];
                v.extend(supply.nodes().into_iter().filter(|n| n != out));
                v
            }
            // A coupling has NO terminals: current flows through the WINDINGS,
            // whose own a/b declarations above carry the conduction claim. The
            // stamp cross-check holds this to account trivially (the coupling
            // variant stamps nothing itself; the mutual terms land in the
            // windings' branch rows, which are not node rows). The inter-
            // winding solver coupling is declared via `controlling_sources`,
            // the branch-current vocabulary, exactly like F/H.
            Self::Coupling { .. } => Vec::new(),
        }
    }

    /// Terminals this device only reads: they steer the stamp (Jacobian
    /// columns, target voltages) but their own KCL rows receive no current.
    ///
    /// A sense terminal is what makes a *free tear* exact: cutting the wire and
    /// replaying its voltage as a source changes nothing electrically, because
    /// no current ever crossed it. Every node the device touches is in exactly
    /// one of [`Device::conduction_nodes`] or this set; the solve-side
    /// cross-check test enforces both the partition and the zero-row property.
    #[must_use]
    pub fn sense_nodes(&self) -> Vec<NodeId> {
        match self {
            // Exactly the complement of the conduction claim above: gate and
            // bulk are sense terminals only while the model gives their rows
            // nothing (no gate caps / no bulk junctions).
            Self::Mosfet { g, b, model, .. } => {
                let mut v = Vec::new();
                if !model.has_gate_charge() {
                    v.push(*g);
                }
                if !model.has_body_diode() {
                    v.extend(*b);
                }
                v
            }
            Self::VSwitch { ctrl_p, ctrl_n, .. } => vec![*ctrl_p, *ctrl_n],
            Self::OpAmp {
                inp,
                inn,
                reference,
                rail_lo,
                rail_hi,
                supply,
                ..
            } => {
                let mut v = vec![*inp, *inn];
                v.extend(*reference);
                // A rail pin that is also a supply pin conducts, not senses.
                let conducts = supply.nodes();
                let rails = [rail_lo.pin, rail_hi.pin];
                v.extend(
                    rails
                        .iter()
                        .filter(|n| !n.is_ground() && !conducts.contains(n)),
                );
                v
            }
            Self::Comparator { inp, inn, .. } => vec![*inp, *inn],
            // The control pair steers the gain term but its own KCL rows
            // receive nothing (ideal infinite input impedance).
            Self::Vcvs { cp, cn, .. } | Self::Vccs { cp, cn, .. } => vec![*cp, *cn],
            // Every V(node) dep is a declared SENSE edge; the tear layers
            // must see that this device reads across island boundaries.
            // Deduped, and EXCLUDING the output terminals: a
            // self-referencing expression (`B1 out 0 I={tanh(V(out))}`, the
            // nonlinear-resistor idiom) senses a node it also conducts into,
            // and a terminal must be in exactly one of the two sets; the
            // conduction claim wins because the p/n rows really do receive
            // current. I(vname) deps are branch-current reads with no node
            // vocabulary, declared via `controlling_sources` like F/H.
            Self::Behavioral { p, n, deps, .. } => {
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
            // No node-voltage reads either: what a coupling "senses" is both
            // windings' BRANCH CURRENTS, which this node vocabulary cannot
            // express (the F/H precedent, same reasoning verbatim), declared
            // through `controlling_sources`, never as a free-tear candidate.
            Self::Resistor { .. }
            | Self::Capacitor { .. }
            | Self::Inductor { .. }
            | Self::Diode { .. }
            | Self::Vsource { .. }
            | Self::Isource { .. }
            | Self::Bjt { .. }
            | Self::Cccs { .. }
            | Self::Ccvs { .. }
            | Self::Coupling { .. } => Vec::new(),
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
    ///
    /// # Panics
    ///
    /// When a `Device` variant has no example here (the `EnumCount` assert), or
    /// if the canonical behavioral expression stops compiling.
    // One flat table, one entry per variant; splitting it would only scatter it.
    #[allow(clippy::too_many_lines)]
    pub fn examples(n: [NodeId; 4]) -> Vec<Self> {
        let out = vec![
            Self::resistor("Rex", n[0], n[1], 1e3),
            Self::capacitor("Cex", n[0], n[1], 1e-9),
            Self::inductor("Lex", n[0], n[1], 1e-6),
            Self::vsource("Vex", n[0], n[1], SourceKind::Dc(1.0)),
            Self::isource("Iex", n[0], n[1], SourceKind::Dc(1e-3)),
            Self::Diode {
                name: "Dex".into(),
                a: n[0],
                k: n[1],
                model: DiodeModel::default(),
            },
            Self::Bjt {
                name: "Qex".into(),
                c: n[0],
                b: n[1],
                e: n[2],
                model: BjtModel::default(),
            },
            Self::Mosfet {
                name: "Mex".into(),
                d: n[0],
                g: n[1],
                s: n[2],
                b: Some(n[3]),
                model: MosfetModel::default(),
            },
            Self::VSwitch {
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
            Self::OpAmp {
                name: "Aex".into(),
                out: n[0],
                inp: n[1],
                inn: n[2],
                reference: Some(n[3]),
                gain: 1e5,
                pole_hz: Some(1e6),
                slew: None,
                rail_lo: Rail::fixed(-5.0),
                rail_hi: Rail::fixed(5.0),
                supply: Supply::default(),
            },
            Self::Comparator {
                name: "Kex".into(),
                out: n[0],
                inp: n[1],
                inn: n[2],
                out_lo: 0.0,
                out_hi: 5.0,
                hysteresis: 1e-3,
                tpd_s: 0.0,
                supply: Supply::default(),
            },
            Self::Vcvs {
                name: "Eex".into(),
                p: n[0],
                n: n[1],
                cp: n[2],
                cn: n[3],
                gain: 10.0,
            },
            Self::Vccs {
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
            Self::Cccs {
                name: "Fex".into(),
                p: n[0],
                n: n[1],
                ctrl_src: DeviceId(0),
                gain: 2.0,
            },
            Self::Ccvs {
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
            Self::Behavioral {
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
            Self::Coupling {
                name: "KMex".into(),
                l1: DeviceId(0),
                l2: DeviceId(1),
                k: 0.9,
            },
        ];
        // The derived variant count is the enforcement the match-arm trick
        // could not provide: an OR-arm satisfies a match without shipping an
        // instance, but it cannot satisfy this length check. A new variant
        // bumps `Device::COUNT` automatically (strum::EnumCount), so this
        // inventory fails loudly until the example exists, and every example
        // then flows into the stamp/sense cross-check, serde round-trip, and
        // node-walk coverage tests.
        assert_eq!(
            out.len(),
            <Self as strum::EnumCount>::COUNT,
            "Device::examples() must ship exactly one instance per variant"
        );
        debug_assert_eq!(
            out.iter()
                .map(std::mem::discriminant)
                .collect::<std::collections::HashSet<_>>()
                .len(),
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
    #[must_use]
    pub const fn is_linear(&self) -> bool {
        matches!(
            self,
            Self::Resistor { .. }
                | Self::Capacitor { .. }
                | Self::Inductor { .. }
                | Self::Vsource { .. }
                | Self::Isource { .. }
                // Controlled sources E/G: the stamp is a CONSTANT gain/gm times
                // node voltages, so the Jacobian never moves, linear. Linear
                // does NOT mean state-space-reducible: `LinearIsland::compile`
                // must still explicitly refuse islands containing them (it
                // models only R/C/L/I) or they would vanish from the A matrix.
                | Self::Vcvs { .. }
                | Self::Vccs { .. }
                // Controlled sources F/H: constant gain/transresistance times
                // a branch-current UNKNOWN, still constant matrix entries, so
                // the Jacobian never moves. Same caveat as E/G: linear does
                // not mean state-space-reducible, and `LinearIsland::compile`
                // refuses islands containing them.
                | Self::Cccs { .. }
                | Self::Ccvs { .. }
                // Coupled inductors: the mutual inductance M = k*sqrt(L1*L2)
                // is a CONSTANT; the companion cross terms scale with 1/dt
                // like any inductor's, never with the iterate, so coupling
                // never taints an island nonlinear. Linear does NOT mean
                // state-space-reducible: `LinearIsland::compile` refuses
                // islands containing a Coupling (the reduction would need the
                // group inductance matrix INVERTED, and k == 1 makes it
                // legally singular), routing them to MNA which stamps L
                // directly.
                | Self::Coupling { .. }
        )
    }

    /// Whether the device is best handled by the event queue rather than the
    /// analog solver. The behavioral comparator is the one digital-ish element
    /// here; the partitioning pass will use this to peel it off. Controlled
    /// sources (`Vcvs`/`Vccs`/`Cccs`/`Ccvs`) are continuous analog constraints
    /// with no discrete state, so they stay with the analog solver (`false`).
    #[must_use]
    pub const fn is_event_driven(&self) -> bool {
        matches!(self, Self::Comparator { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_ellipsis;

    #[test]
    fn ellipsis_keeps_short_text_and_caps_long_text_at_max() {
        assert_eq!(truncate_ellipsis("abc", 3), "abc");
        assert_eq!(truncate_ellipsis("abcd", 3), "ab\u{2026}");
        assert_eq!(truncate_ellipsis("µΩµΩ", 3), "µΩ\u{2026}");
        assert_eq!(truncate_ellipsis("abc", 0), "\u{2026}");
    }
}
