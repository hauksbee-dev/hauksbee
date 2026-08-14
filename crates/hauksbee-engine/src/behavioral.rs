//! Behavioural-device runtime.
//!
//! This is the engine-side realisation of the declarative
//! [`Behavioral`] layer. A behavioural
//! device participates in the solve loop exactly the way the configurable power
//! supplies do (`power_supply.rs`): it stamps controllable Thevenin legs and
//! sense resistors into the [`Circuit`] once, and the scheduler calls
//! [`BehavioralDevice::update`] between solver chunks to recompute each leg's
//! source value from the previous chunk's solved node voltages and branch
//! currents. It adds no new device kinds to the inner Newton loop: every
//! behaviour is expressed in terms of the existing `Vsource` / `Isource` /
//! `Resistor` / `Behavioral` (B-source) primitives the solver already stamps,
//! so the partitioned solver is untouched.
//!
//! What each declarative fact becomes:
//!
//! - an internal **pull** (pin -> rail through R) is a `Resistor` from the pin
//!   node to the rail node, stamped once. Static; nothing to update.
//! - an **open-drain** sink is a `Resistor` from the pin to its sink rail whose
//!   resistance the runtime swaps between on (`od_ohms`) and off (`1e12`) as the
//!   controlling state asserts it.
//! - a **converter** is a controllable `Vsource` behind a small series resistor
//!   on the output pin (the [`SupplyLeg`](crate::power_supply) trick, so the
//!   scheduler can read the delivered current), PLUS a controllable `Isource`
//!   on the input pin that draws the reflected input current
//!   `Vout*Iout/(eff*Vin)`. Regulation, output-current foldback and the
//!   programmable input-current limit are recomputed each chunk.
//! - a **law** whose expression is a pure function of pin voltages and params
//!   (a current law with no `only_in_state` gate) is stamped as a
//!   solver-implicit `Device::Behavioral`: Newton evaluates it at every
//!   iterate, so the law's conductance is in the Jacobian and a clamp on a
//!   floating net cannot blow the chunk-to-chunk relaxation up (see
//!   `LawStamp`). Every other law (voltage laws, state-gated laws,
//!   FSM-context expressions) is a controllable `Isource` (current law) or
//!   `Vsource`+R (voltage law) whose value is the `evalexpr` expression
//!   evaluated against the bound pin voltages / state / params each chunk.
//!
//! The FSM advances once per chunk: its guards are `evalexpr` booleans over the
//! same context. Per-state pin overrides retune the open-drain / drive legs.

use std::collections::BTreeMap;

use evalexpr::{ContextWithMutableVariables, HashMapContext, Node as EvalNode, Value};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_models::behavioral::{
    Behavioral, Converter, Law, LawKind, SenseProgram, StatePinBehaviour,
};
use hauksbee_models::Params;

use crate::stress::{FaultEvent, FaultKind};

// ── Escape-hatch trait for custom behaviours ────────────────────────────────

/// The escape hatch for behaviours the declarative TOML layer cannot express.
///
/// The declarative layer (pins/pulls, FSM, averaged converter, expression laws)
/// covers the great majority of power ICs, but some parts have behaviour no
/// finite declarative schema captures, a closed-loop controller with internal
/// state, a multi-phase sequencer with data-dependent timing, a part whose
/// output depends on an I2C register the firmware wrote. For those, a user
/// implements this trait in Rust and registers it under a part-match key
/// ([`CustomRegistry`]); the scheduler then drives it each chunk exactly like a
/// declarative [`BehavioralDevice`].
///
/// The contract mirrors the declarative device's lifecycle:
///   - [`Self::stamp`] runs once at bind time: stamp whatever `Vsource` /
///     `Isource` / `Resistor` legs you need onto the circuit and remember their
///     [`DeviceId`]s.
///   - [`Self::update`] runs once per solver chunk with the previous chunk's
///     solved node voltages: recompute your legs and write them back.
///
/// You never touch the inner Newton loop; you only mutate source values between
/// chunks, the same convergence-per-chunk pattern the supplies and the
/// declarative devices use, so the solver is untouched.
pub trait CustomBehavior: Send {
    /// Stamp this device's legs onto the circuit once. `role_nodes` maps the
    /// component's pin roles to nodes (from `[models.pins]`). `params` carries
    /// the model's params. Store any device ids you will update.
    fn stamp(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        params: &Params,
        role_nodes: &BTreeMap<String, NodeId>,
    );

    /// Advance one chunk: recompute and write your source values from the
    /// previous chunk's solved `node_v`. `t` is sim time (s), `dt` the chunk.
    /// Push any faults you raise into `faults`.
    fn update(
        &mut self,
        circuit: &mut Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        t: f64,
        dt: f64,
        faults: &mut Vec<FaultEvent>,
    );

    /// A short state label for diagnostics / frames (default empty).
    fn state(&self) -> &str {
        ""
    }
}

/// Registry of custom-behaviour factories, keyed by an exact match string
/// tested against a component's resolved model id, value, or MPN. A user (or a
/// downstream crate) registers a factory before binding; the binder consults the
/// registry for every component and, on a hit, instantiates the custom device
/// instead of (or alongside) the declarative one.
#[derive(Default)]
pub struct CustomRegistry {
    factories: Vec<(
        String,
        Box<dyn Fn() -> Box<dyn CustomBehavior> + Send + Sync>,
    )>,
}

impl CustomRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        CustomRegistry::default()
    }

    /// Register a factory under a match `key` (matched against model id / value /
    /// MPN, case-insensitively). The factory is called once per matching
    /// component to build a fresh device instance.
    pub fn register(
        &mut self,
        key: impl Into<String>,
        factory: impl Fn() -> Box<dyn CustomBehavior> + Send + Sync + 'static,
    ) {
        self.factories
            .push((key.into().to_ascii_lowercase(), Box::new(factory)));
    }

    /// Build a custom device for `keys` (any of the component's id/value/MPN), or
    /// `None` if nothing is registered for it.
    pub fn build_for(&self, keys: &[&str]) -> Option<Box<dyn CustomBehavior>> {
        for (k, factory) in &self.factories {
            if keys.iter().any(|key| key.to_ascii_lowercase() == *k) {
                return Some(factory());
            }
        }
        None
    }

    /// True when no factories are registered (the common case).
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

/// Tiny output impedance for behavioural sources so the source branch current
/// is well defined (mirrors `power_supply::STIFF_R_OHMS`).
const STIFF_R_OHMS: f64 = 1e-3;

/// High-impedance "off" resistance for a tri-stated open-drain / drive leg.
const OFF_OHMS: f64 = 1e12;

/// Numerical tolerance when nominally matched shunts arrive through separate
/// parsed component records.
const MATCHED_SHUNT_RELATIVE_TOLERANCE: f64 = 1e-12;
const MATCHED_SHUNT_MIN_SCALE_OHMS: f64 = 1.0;

/// One pull leg: a resistor from a pin to a rail, stamped once. The device id
/// is retained so a future extension can retune or tri-state the pull; the pull
/// itself is static, so the runtime never touches it after stamping.
#[derive(Debug, Clone)]
struct PullLeg {
    #[allow(dead_code)]
    resistor: DeviceId,
}

/// One open-drain leg: a resistor whose value we swap on/off by state.
#[derive(Debug, Clone)]
struct OpenDrainLeg {
    pin_role: String,
    resistor: DeviceId,
    on_ohms: f64,
}

/// A per-state push-pull drive leg (Vsource behind a resistor whose value is
/// swapped to OFF_OHMS when the state is inactive).
#[derive(Debug, Clone)]
struct DriveLeg {
    pin_role: String,
    vsource: DeviceId,
    resistor: DeviceId,
    on_ohms: f64,
}

/// The stamped converter realisation.
#[derive(Debug, Clone)]
struct ConverterLeg {
    cfg: Converter,
    /// Output source (controllable Vsource behind `out_r`).
    out_vsource: DeviceId,
    /// Hidden driver node the output Vsource pushes onto, before `out_r`.
    out_drv_node: NodeId,
    /// Output-source series resistance (ohms), so the delivered current can be
    /// read as the voltage drop across it. This works under BOTH solver paths;
    /// reading the Vsource branch unknown does not, because the partitioned
    /// solver's global x carries node voltages only, not branch currents.
    out_r_ohms: f64,
    out_node: NodeId,
    in_node: NodeId,
    /// Input draw (controllable Isource from in_node to ground).
    in_isource: DeviceId,
    /// Effective input-current limit (A), computed from the sense program at
    /// bind time (or the literal). Recomputable if a board resistor is retuned.
    iin_limit_a: Option<f64>,
    /// Last commanded output voltage (for the CC foldback anchor).
    last_cmd_vout: f64,
    /// Last delivered output current (A).
    last_iout_a: f64,
    /// Last reflected input current commanded (A).
    last_iin_a: f64,
}

/// A stamped expression law: a controllable source whose value is recomputed
/// from `expr` each chunk.
#[derive(Debug, Clone)]
struct LawLeg {
    law: Law,
    /// Compiled expression (parsed once at stamp time).
    program: EvalNode,
    /// How the law reached the circuit (implicit B-source vs chunk-updated
    /// source); see [`LawStamp`].
    stamp: LawStamp,
}

/// How a law is realised in the circuit.
///
/// The IMPLICIT form exists because the chunk-updated form is an explicit
/// relaxation with no feedback inside the solve, and that loop is unstable on
/// a high-impedance net. Measured on the lily58 Pro V2 corpus board: the
/// USBLC6-2 steering-diode clamp laws sit on the USB data nets, which float
/// (connector skipped, no MCU on them). Chunk n's constant `Isource` drives
/// the gmin-anchored net to `I/gmin` volts, chunk n+1 evaluates the clamp law
/// AT that voltage and writes a current ~1e12x larger, alternating sign each
/// chunk (1.97 A -> 1.97e12 -> 1.98e24 -> ... -> inf), and every later Newton
/// solve fails on the poisoned state. Stamping the SAME expression as a
/// solver-side behavioral device closes the loop inside Newton (the law's
/// conductance is in the Jacobian), which is unconditionally stable here and
/// is simply the correct physics: the clamp conducts exactly when the solved
/// voltages say it conducts.
#[derive(Debug, Clone)]
enum LawStamp {
    /// Stamped as a solver-implicit [`Device::Behavioral`]: Newton evaluates
    /// the expression at every iterate, nothing to update per chunk. Taken by
    /// every current law whose expression is a pure function of pin voltages
    /// and params (no FSM state, no time, no `only_in_state` gating).
    Implicit,
    /// A chunk-updated controllable source (Isource for current, Vsource for
    /// voltage): the value is re-evaluated between chunks from the previous
    /// chunk's solved voltages. Kept for laws the implicit form cannot
    /// express (state-gated laws, FSM-context expressions).
    Runtime {
        /// The controllable source device.
        source: DeviceId,
        /// For a Voltage law, its series output resistor and that resistor's
        /// on-resistance (ohms). Deactivating an `only_in_state` voltage law
        /// must tri-state this resistor (OFF_OHMS) to RELEASE the pin,
        /// zeroing the Vsource alone would clamp the pin to 0 V through the
        /// stiff series R (a near-short). A Current law releases correctly by
        /// zeroing its Isource, so it carries `None` here.
        series_r: Option<(DeviceId, f64)>,
    },
}

/// Try to compile a law expression into the solver's canonical behavioral
/// form: every numeric param folds to a literal and every `v_<role>` becomes
/// a `__d{k}` voltage dependency, in THAT order, because the runtime context
/// sets params after pin voltages so a param sharing a `v_<role>` name shadows
/// the pin (see `build_context`); the two forms must resolve one identifier
/// the same way. Returns `None` when the expression reaches outside that
/// vocabulary (FSM state booleans, `t_in_state`, an unconnected pin's
/// voltage), in which case the caller keeps the chunk-updated runtime form,
/// which is the only form that can read FSM context. `t` also refuses: the
/// runtime evaluates it against the GLOBAL sim clock, while a B-source `time`
/// is the transient's chunk-local clock (restarting at 0 every chunk), so a
/// time-varying law cannot be expressed implicitly without changing what it
/// computes.
fn compile_law_implicit(
    expr: &str,
    role_nodes: &BTreeMap<String, NodeId>,
    params: &Params,
) -> Option<(hauksbee_ir::CompiledExpr, Vec<hauksbee_ir::BDep>)> {
    let mut out = String::with_capacity(expr.len());
    let mut deps: Vec<hauksbee_ir::BDep> = Vec::new();
    // role -> already-assigned dependency slot, so one pin read twice shares
    // a slot (the FD Jacobian probes each slot; duplicates would double work
    // and, worse, decouple the two reads of the same voltage).
    let mut slots: BTreeMap<String, usize> = BTreeMap::new();
    let bytes = expr.as_bytes();
    let is_ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if !is_ident_start(c) {
            out.push(c as char);
            i += 1;
            continue;
        }
        // An `e`/`E` directly after a digit or '.' is a numeric exponent
        // (`1e-6`), not an identifier: copy the exponent through verbatim.
        if (c == b'e' || c == b'E')
            && i > 0
            && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.')
        {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_digit() {
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                out.push_str(&expr[i..j]);
                i = j;
                continue;
            }
        }
        // Read a (possibly namespaced) identifier: `ident(::ident)*`.
        let start = i;
        while i < bytes.len() && is_ident(bytes[i]) {
            i += 1;
        }
        while i + 1 < bytes.len() && bytes[i] == b':' && bytes[i + 1] == b':' {
            i += 2;
            while i < bytes.len() && is_ident(bytes[i]) {
                i += 1;
            }
        }
        let token = &expr[start..i];
        // A function name (namespaced, or followed by an open paren) passes
        // through: which functions exist is evalexpr's business, and the
        // probe evaluation below is the gate that actually decides. A
        // hand-maintained allowlist was tried and got the vocabulary wrong in
        // both directions (rejecting valid `math::` builtins, admitting
        // misspelled namespaces).
        let next_nonspace = bytes[i..].iter().find(|b| !b.is_ascii_whitespace());
        if token.contains("::") || next_nonspace == Some(&b'(') {
            out.push_str(token);
            continue;
        }
        // Params FIRST: the runtime context sets them after pin voltages, so
        // a param named like a `v_<role>` shadows the pin there and must fold
        // to its literal here too. The runtime-owned names are the exception:
        // `t`, `t_in_state` and `state_<name>` are set AFTER params in
        // `build_context`, so a param spelled like one of them never wins at
        // runtime and must not fold here; those tokens fall through to the
        // refusal below, keeping such a law on the runtime path it actually
        // depends on.
        let runtime_owned =
            token == "t" || token == "time" || token == "t_in_state" || token.starts_with("state_");
        if !runtime_owned {
            if let Some(v) = params.0.get(token).and_then(|v| v.as_f64()) {
                // Round-trip-exact literal, parenthesised so `1/x` stays `1/(v)`.
                out.push_str(&format!("({v:?})"));
                continue;
            }
        }
        if let Some(role) = token.strip_prefix("v_") {
            if let Some(&node) = role_nodes.get(role) {
                let k = *slots.entry(role.to_string()).or_insert_with(|| {
                    deps.push(hauksbee_ir::BDep::Volt(node));
                    deps.len() - 1
                });
                out.push_str(&format!("__d{k}"));
                continue;
            }
            // A voltage the binder did not connect: the runtime form treats it
            // as an eval error (law inert); refuse the implicit form the same
            // honest way rather than substituting a made-up 0.
            return None;
        }
        // Anything else refuses. FSM context (state_<name>, t_in_state) exists
        // only in the runtime form; `t`/`time` refuse because the runtime's
        // clock is the GLOBAL sim time while a B-source `time` is the chunk's
        // LOCAL transient clock, and silently swapping one for the other would
        // change what a time-varying law computes.
        return None;
    }
    let compiled = hauksbee_ir::CompiledExpr::compile(&out).ok()?;
    // PROVE the expression evaluates FINITE before choosing the implicit
    // path. `CompiledExpr::compile` validates variable identifiers but not
    // function names, so an unknown/misnamespaced function (`bogus(x)`, bare
    // `exp` where evalexpr wants `math::exp`) compiles fine and would then
    // FAULT inside Newton at every stamp, failing the analog session; the
    // runtime form evaluates the same malformed law to a guarded 0 A, a
    // contract the CI gate pins (`hauksbee-ci exit3_reachability`: a
    // `v_in / sense_ohms` law with a 0-ohm sense resistor folds to a
    // division by zero and must contribute 0 A, never an aborted run). A
    // non-finite probe VALUE is the same hazard as a structural fault, so
    // both refuse: `0.0 / (0.0)` is NaN at the probe exactly because it is
    // NaN at every iterate. The probe point (all dependencies 0, t=0) cannot
    // certify every iterate Newton will visit, but it catches the whole
    // folded-constant class, which is what the contract covers.
    compiled
        .eval(&vec![0.0; deps.len()], 0.0)
        .ok()
        .filter(|v| v.is_finite())?;
    Some((compiled, deps))
}

/// One FSM transition with its guard compiled once at stamp time. Re-parsing the
/// guard string every chunk was wasteful and swallowed parse errors silently; a
/// guard that fails to compile is reported once here and stored as `None` (never
/// fires) rather than being re-parsed (and re-failing) on every chunk.
#[derive(Debug, Clone)]
struct CompiledTransition {
    tr: hauksbee_models::behavioral::Transition,
    guard: Option<EvalNode>,
}

/// A behavioural device bound onto the circuit: its stamped legs plus live FSM
/// state. The scheduler iterates it each chunk via [`Self::update`].
pub struct BehavioralDevice {
    /// Component reference designator.
    pub reference: String,
    /// Role -> node for every connected, named pin of this device.
    role_nodes: BTreeMap<String, NodeId>,
    /// Static numeric params (from the model entry), bound into every context.
    params: Params,

    pulls: Vec<PullLeg>,
    open_drains: Vec<OpenDrainLeg>,
    drives: Vec<DriveLeg>,
    converter: Option<ConverterLeg>,
    laws: Vec<LawLeg>,

    /// FSM state names (empty when the model has no FSM).
    fsm_states: Vec<String>,
    fsm_transitions: Vec<CompiledTransition>,
    /// state -> pin role -> behaviour override.
    state_pins: BTreeMap<String, BTreeMap<String, StatePinBehaviour>>,
    /// Current FSM state index, and time spent in it.
    state_idx: usize,
    t_in_state: f64,

    /// Optional input-power budget (W) and the input voltage it is referenced
    /// to (V): when set, an overpower fault is raised if the converter's
    /// reflected input draw exceeds the budget. This covers reported charger
    /// overdraw faults without baking a particular board's measured wattage
    /// into the device model.
    input_budget: Option<(f64, f64)>,
    /// Faults this device raised since the last drain (e.g. input overdraw).
    pending_faults: Vec<FaultEvent>,
    /// Latch so a sustained-condition fault is reported once, not every chunk.
    overdraw_latched: bool,

    /// Escape-hatch custom behaviour (set when this device wraps a
    /// [`CustomBehavior`] instead of the declarative legs). When present, the
    /// declarative legs are empty and all lifecycle calls route to it.
    custom: Option<Box<dyn CustomBehavior>>,
}

impl BehavioralDevice {
    /// Remap every cached [`NodeId`] this device holds through `map`. Used when
    /// an as-built `[[jumper]]` merges two nets after binding: the circuit's
    /// device terminals are remapped in place, but this device also caches
    /// node ids (its `role_nodes` and any converter leg), which would otherwise
    /// point at the orphaned, now-unconnected node. (R8 #6)
    pub fn remap_node(&mut self, map: impl Fn(NodeId) -> NodeId) {
        for n in self.role_nodes.values_mut() {
            *n = map(*n);
        }
        if let Some(c) = self.converter.as_mut() {
            c.out_drv_node = map(c.out_drv_node);
            c.out_node = map(c.out_node);
            c.in_node = map(c.in_node);
        }
    }

    /// Wrap a user-supplied [`CustomBehavior`] as a behavioural device, stamping
    /// it onto the circuit. The scheduler then drives it each chunk exactly like
    /// a declarative device.
    pub fn from_custom(
        circuit: &mut Circuit,
        reference: &str,
        params: &Params,
        role_nodes: &BTreeMap<String, NodeId>,
        mut custom: Box<dyn CustomBehavior>,
    ) -> BehavioralDevice {
        custom.stamp(circuit, reference, params, role_nodes);
        BehavioralDevice {
            reference: reference.to_string(),
            role_nodes: role_nodes.clone(),
            params: params.clone(),
            pulls: Vec::new(),
            open_drains: Vec::new(),
            drives: Vec::new(),
            converter: None,
            laws: Vec::new(),
            fsm_states: Vec::new(),
            fsm_transitions: Vec::new(),
            state_pins: BTreeMap::new(),
            state_idx: 0,
            t_in_state: 0.0,
            input_budget: None,
            pending_faults: Vec::new(),
            overdraw_latched: false,
            custom: Some(custom),
        }
    }

    /// Stamp a behavioural model onto the circuit and return the live device.
    ///
    /// `role_nodes` maps every connected pin role of the component to its node.
    /// `rail_node` resolves a named rail (a pin role, a literal-volts pseudo
    /// node, or ground) to a node for internal pulls. `board_resistor` resolves
    /// a board reference designator (e.g. "R8") to its ohms, so a programmable
    /// limit is read off the actual board. Returns `None` if nothing was
    /// stampable (no connected pins the model needs).
    pub fn stamp(
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        params: &Params,
        role_nodes: &BTreeMap<String, NodeId>,
        board_resistor: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<BehavioralDevice> {
        let mut dev = BehavioralDevice {
            reference: reference.to_string(),
            role_nodes: role_nodes.clone(),
            params: params.clone(),
            pulls: Vec::new(),
            open_drains: Vec::new(),
            drives: Vec::new(),
            converter: None,
            laws: Vec::new(),
            fsm_states: Vec::new(),
            fsm_transitions: Vec::new(),
            state_pins: BTreeMap::new(),
            state_idx: 0,
            t_in_state: 0.0,
            input_budget: None,
            pending_faults: Vec::new(),
            overdraw_latched: false,
            custom: None,
        };

        // ── Pins: internal pulls and open-drain sinks ──────────────────────
        for (role, pin) in &model.pins {
            let Some(&pin_node) = role_nodes.get(role) else {
                continue; // pin not connected on this board; nothing to stamp
            };
            // Internal pull to a rail.
            if let Some(ohms) = pin.pull_ohms {
                let rail = if let Some(v) = pin.pull_to_volts {
                    // Literal-voltage rail: stamp a hidden stiff source node.
                    let n = circuit.node(&format!("__beh_{reference}_{role}_railv"));
                    circuit.add(Device::Vsource {
                        name: format!("Vbeh_{reference}_{role}_rail"),
                        p: n,
                        n: NodeId::GROUND,
                        kind: SourceKind::Dc(v),
                    });
                    Some(n)
                } else if let Some(rail_role) = &pin.pull_to {
                    role_nodes.get(rail_role).copied()
                } else {
                    None
                };
                if let Some(rail_node) = rail {
                    let r = circuit.add(Device::Resistor {
                        name: format!("Rbeh_{reference}_{role}_pull"),
                        a: pin_node,
                        b: rail_node,
                        ohms,
                        tc1: None,
                    });
                    dev.pulls.push(PullLeg { resistor: r });
                }
            }
            // Open-drain sink leg (starts off / high-impedance).
            if pin.open_drain {
                let sink = pin.od_to_volts.unwrap_or(0.0);
                let sink_node = if sink.abs() < 1e-12 {
                    NodeId::GROUND
                } else {
                    let n = circuit.node(&format!("__beh_{reference}_{role}_odsink"));
                    circuit.add(Device::Vsource {
                        name: format!("Vbeh_{reference}_{role}_odsink"),
                        p: n,
                        n: NodeId::GROUND,
                        kind: SourceKind::Dc(sink),
                    });
                    n
                };
                let on_ohms = pin.od_ohms.unwrap_or(20.0);
                let r = circuit.add(Device::Resistor {
                    name: format!("Rbeh_{reference}_{role}_od"),
                    a: pin_node,
                    b: sink_node,
                    ohms: OFF_OHMS,
                    tc1: None,
                });
                dev.open_drains.push(OpenDrainLeg {
                    pin_role: role.clone(),
                    resistor: r,
                    on_ohms,
                });
            }
        }

        // ── FSM ────────────────────────────────────────────────────────────
        if let Some(fsm) = &model.fsm {
            dev.fsm_states = fsm.states.clone();
            // Compile each transition guard once. A guard that fails to parse is
            // reported here (not silently, not re-parsed every chunk) and stored
            // as `None`, so it simply never fires.
            dev.fsm_transitions = fsm
                .transitions
                .iter()
                .map(|tr| {
                    let guard = match evalexpr::build_operator_tree(&tr.guard) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            eprintln!(
                                "[behavioural] {reference}: FSM guard '{}' ({} -> {}) \
                                 failed to parse: {e}; transition disabled",
                                tr.guard, tr.from, tr.to
                            );
                            None
                        }
                    };
                    CompiledTransition {
                        tr: tr.clone(),
                        guard,
                    }
                })
                .collect();
            dev.state_pins = fsm.state_pins.clone();
            dev.state_idx = fsm
                .initial
                .as_ref()
                .and_then(|init| fsm.states.iter().position(|s| s == init))
                .unwrap_or(0);

            // Stamp a drive leg for every (state, pin) override that drives a
            // voltage, so the runtime can swap it on while that state is active.
            // De-duplicate by pin role: one leg per pin, retuned per state.
            let mut driven_pins: BTreeMap<String, f64> = BTreeMap::new();
            for pins in fsm.state_pins.values() {
                for (role, beh) in pins {
                    if let Some(v) = beh.drive_volts {
                        driven_pins.entry(role.clone()).or_insert(v);
                    }
                }
            }
            for (role, _v) in driven_pins {
                let Some(&pin_node) = role_nodes.get(&role) else {
                    continue;
                };
                let drv = circuit.node(&format!("__beh_{reference}_{role}_drv"));
                let vsource = circuit.add(Device::Vsource {
                    name: format!("Vbeh_{reference}_{role}_drv"),
                    p: drv,
                    n: NodeId::GROUND,
                    kind: SourceKind::Dc(0.0),
                });
                let resistor = circuit.add(Device::Resistor {
                    name: format!("Rbeh_{reference}_{role}_drv"),
                    a: drv,
                    b: pin_node,
                    ohms: OFF_OHMS,
                    tc1: None,
                });
                dev.drives.push(DriveLeg {
                    pin_role: role,
                    vsource,
                    resistor,
                    on_ohms: 50.0,
                });
            }
        }

        // ── Converter ──────────────────────────────────────────────────────
        if let Some(c) = &model.converter {
            let iin_limit_a = resolve_iin_limit(c, board_resistor);
            let has_required_program_evidence = c.iin_program.is_none() || iin_limit_a.is_some();
            if has_required_program_evidence {
                if let (Some(&out_node), Some(&in_node)) =
                    (role_nodes.get(&c.out_pin), role_nodes.get(&c.in_pin))
                {
                    let out_r = c.out_r_ohms.unwrap_or(STIFF_R_OHMS).max(STIFF_R_OHMS);
                    let drv = circuit.node(&format!("__beh_{reference}_conv_out"));
                    let out_vsource = circuit.add(Device::Vsource {
                        name: format!("Vbeh_{reference}_conv"),
                        p: drv,
                        n: NodeId::GROUND,
                        kind: SourceKind::Dc(c.vout_setpoint),
                    });
                    circuit.add(Device::Resistor {
                        name: format!("Rbeh_{reference}_conv_out"),
                        a: drv,
                        b: out_node,
                        ohms: out_r,
                        tc1: None,
                    });
                    // Reflected input draw: an Isource pulling from in_node to GND.
                    let in_isource = circuit.add(Device::Isource {
                        name: format!("Ibeh_{reference}_conv_in"),
                        p: in_node,
                        n: NodeId::GROUND,
                        kind: SourceKind::Dc(0.0),
                    });
                    dev.converter = Some(ConverterLeg {
                        cfg: c.clone(),
                        out_vsource,
                        out_drv_node: drv,
                        out_r_ohms: out_r,
                        out_node,
                        in_node,
                        in_isource,
                        iin_limit_a,
                        last_cmd_vout: c.vout_setpoint,
                        last_iout_a: 0.0,
                        last_iin_a: 0.0,
                    });
                }
            }
        }

        // ── Laws ───────────────────────────────────────────────────────────
        for law in &model.laws {
            let Some(&a_node) = role_nodes.get(&law.a) else {
                continue;
            };
            let b_node = law
                .b
                .as_ref()
                .and_then(|b| role_nodes.get(b).copied())
                .unwrap_or(NodeId::GROUND);
            let program = match evalexpr::build_operator_tree(&law.expr) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "[behavioural] {reference}: law '{}' expr failed to parse: {e}; skipped",
                        law.name
                    );
                    continue;
                }
            };
            // Prefer the solver-implicit form for a current law that is a pure
            // function of pin voltages / params: Newton then owns the
            // law (its conductance is in the Jacobian), where the chunk-updated
            // constant-source form is an explicit relaxation that diverges on
            // high-impedance nets (see [`LawStamp`]). State-gated laws and
            // FSM-context expressions cannot be expressed implicitly and keep
            // the runtime form.
            if matches!(law.kind, LawKind::Current) && law.only_in_state.is_none() {
                if let Some((expr, deps)) = compile_law_implicit(&law.expr, role_nodes, params) {
                    circuit.add(Device::Behavioral {
                        name: format!("Bbeh_{reference}_{}", law.name),
                        p: a_node,
                        n: b_node,
                        output: hauksbee_ir::BOutput::Current,
                        expr,
                        deps,
                    });
                    dev.laws.push(LawLeg {
                        law: law.clone(),
                        program,
                        stamp: LawStamp::Implicit,
                    });
                    continue;
                }
            }
            let (source, series_r): (DeviceId, Option<(DeviceId, f64)>) = match law.kind {
                LawKind::Current => (
                    circuit.add(Device::Isource {
                        name: format!("Ibeh_{reference}_{}", law.name),
                        p: a_node,
                        n: b_node,
                        kind: SourceKind::Dc(0.0),
                    }),
                    None,
                ),
                LawKind::Voltage => {
                    let drv = circuit.node(&format!("__beh_{reference}_{}_law", law.name));
                    let vs = circuit.add(Device::Vsource {
                        name: format!("Vbeh_{reference}_{}", law.name),
                        p: drv,
                        n: b_node,
                        kind: SourceKind::Dc(0.0),
                    });
                    let on_ohms = law.r_ohms.unwrap_or(STIFF_R_OHMS).max(STIFF_R_OHMS);
                    let r = circuit.add(Device::Resistor {
                        name: format!("Rbeh_{reference}_{}_law", law.name),
                        a: drv,
                        b: a_node,
                        ohms: on_ohms,
                        tc1: None,
                    });
                    (vs, Some((r, on_ohms)))
                }
            };
            dev.laws.push(LawLeg {
                law: law.clone(),
                program,
                stamp: LawStamp::Runtime { source, series_r },
            });
        }

        if dev.is_inert() {
            None
        } else {
            Some(dev)
        }
    }

    /// True when nothing was stamped (no legs / FSM / custom): inert.
    fn is_inert(&self) -> bool {
        self.custom.is_none()
            && self.pulls.is_empty()
            && self.open_drains.is_empty()
            && self.drives.is_empty()
            && self.fsm_states.is_empty()
            && self.converter.is_none()
            && self.laws.is_empty()
    }

    /// Current state label: the FSM state, or a custom device's own label.
    pub fn state(&self) -> &str {
        if let Some(c) = &self.custom {
            return c.state();
        }
        self.fsm_states
            .get(self.state_idx)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The node the converter's output source drives, or None (no converter).
    /// The binder consults this to suppress the ideal auto-rail on a
    /// converter-driven supply net: two stiff sources on one net, and the
    /// ideal one wins.
    pub fn converter_out_node(&self) -> Option<NodeId> {
        self.converter.as_ref().map(|c| c.out_node)
    }

    /// The effective input-current limit of the converter (A), or None.
    pub fn converter_iin_limit(&self) -> Option<f64> {
        self.converter.as_ref().and_then(|c| c.iin_limit_a)
    }

    /// Last delivered output current of the converter (A), or None.
    pub fn converter_iout(&self) -> Option<f64> {
        self.converter.as_ref().map(|c| c.last_iout_a)
    }

    /// Last reflected input current the converter drew (A), or None.
    pub fn converter_iin(&self) -> Option<f64> {
        self.converter.as_ref().map(|c| c.last_iin_a)
    }

    /// The current value (A) of a named current-law source. A runtime law is
    /// read from the circuit (the value the runtime last wrote); an implicit
    /// law is its expression evaluated at the solved voltages `node_v`, which
    /// is exactly the current the solver stamped. `None` if no such law. Used
    /// by the balancer-leak validation to read the leak magnitude directly.
    pub fn law_value(
        &self,
        circuit: &Circuit,
        name: &str,
        node_v: &dyn Fn(NodeId) -> f64,
        t: f64,
    ) -> Option<f64> {
        let leg = self.laws.iter().find(|l| l.law.name == name)?;
        match leg.stamp {
            LawStamp::Implicit => {
                let ctx = self.build_context(node_v, t);
                eval_number(&leg.program, &ctx)
            }
            LawStamp::Runtime { source, .. } => match circuit.devices.get(source.0 as usize) {
                Some(Device::Isource {
                    kind: SourceKind::Dc(v),
                    ..
                })
                | Some(Device::Vsource {
                    kind: SourceKind::Dc(v),
                    ..
                }) => Some(*v),
                _ => None,
            },
        }
    }

    /// Drain faults raised since the last call.
    pub fn drain_faults(&mut self) -> Vec<FaultEvent> {
        std::mem::take(&mut self.pending_faults)
    }

    /// Advance one chunk. `node_v(n)` returns the node's solved voltage from the
    /// previous chunk; `branch_current(dev)` returns a Vsource's branch current
    /// (for reading the converter's delivered output current). Recompute every
    /// controllable leg's value and write it onto the circuit.
    pub fn update(
        &mut self,
        circuit: &mut Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        _branch_current: &dyn Fn(DeviceId) -> Option<f64>,
        t: f64,
        dt: f64,
    ) {
        // Escape hatch: a custom-behaviour device owns its whole update.
        if let Some(custom) = &mut self.custom {
            custom.update(circuit, node_v, t, dt, &mut self.pending_faults);
            return;
        }

        // 1. Build the evaluation context from current pin voltages + params +
        //    state booleans.
        let ctx = self.build_context(node_v, t);

        // 2. Advance the FSM (guards over the same context).
        self.advance_fsm(&ctx, dt);

        // 3. Apply per-state pin overrides (open-drain asserts, drives).
        self.apply_state_pins(circuit);

        // 4. Converter regulation + limits.
        if self.converter.is_some() {
            self.update_converter(circuit, node_v);
        }

        // 5. Expression laws. Evaluate against a context rebuilt AFTER the FSM
        //    advanced: `advance_fsm` may have changed the active state and reset
        //    `t_in_state`, and a law's expr can read `state_<name>` / `t_in_state`
        //    / its own gating. Using the pre-advance `ctx` here lagged the law one
        //    chunk behind the state that gates it.
        let ctx = self.build_context(node_v, t);
        let active = self.state().to_string();
        for leg in &self.laws {
            // An implicit law lives inside the Newton loop as a behavioral
            // device; there is no per-chunk source to retune.
            let LawStamp::Runtime { source, series_r } = leg.stamp else {
                continue;
            };
            if let Some(req) = &leg.law.only_in_state {
                if req != &active {
                    // Deactivate: RELEASE the pin. A voltage law tri-states its
                    // series resistor (OFF_OHMS) so the pin floats; leaving the
                    // resistor on while zeroing the Vsource would clamp the pin to
                    // 0 V through the stiff series R (a near-short). A current law
                    // releases by zeroing its Isource.
                    if let Some((r, _)) = series_r {
                        set_resistor_ohms(circuit, r, OFF_OHMS);
                    }
                    set_source_dc(circuit, source, 0.0);
                    continue;
                }
            }
            // Active: restore the series resistor's on-resistance in case a prior
            // chunk tri-stated it.
            if let Some((r, on)) = series_r {
                set_resistor_ohms(circuit, r, on);
            }
            // Guard against a non-finite law value (e.g. a divide-by-zero when a
            // programming resistor is a 0-ohm jumper): a NaN/Inf source would
            // blow up the solve. Clamp to 0 rather than poison the matrix.
            let val = eval_number(&leg.program, &ctx).unwrap_or(0.0);
            let val = if val.is_finite() { val } else { 0.0 };
            set_source_dc(circuit, source, val);
        }

        // 6. Input-power budget check (raises an overpower fault on overdraw).
        if let Some((vin, budget)) = self.input_budget {
            self.check_input_budget(vin, budget, t);
        }
    }

    /// Configure an input-power budget (W) at input voltage `vin` (V). When set,
    /// each chunk raises an overpower fault if the converter's reflected input
    /// draw exceeds the budget.
    pub fn set_input_budget(&mut self, vin: f64, budget_w: f64) {
        self.input_budget = Some((vin, budget_w));
    }

    /// Build an evalexpr context: `v_<role>` for each connected pin's voltage,
    /// every param key verbatim, `t`, `t_in_state`, and `state_<name>` booleans.
    fn build_context(&self, node_v: &dyn Fn(NodeId) -> f64, t: f64) -> HashMapContext {
        let mut ctx = HashMapContext::new();
        for (role, &node) in &self.role_nodes {
            let _ = ctx.set_value(format!("v_{role}"), Value::Float(node_v(node)));
        }
        for (k, v) in &self.params.0 {
            if let Some(f) = v.as_f64() {
                let _ = ctx.set_value(k.clone(), Value::Float(f));
            }
        }
        let _ = ctx.set_value("t".into(), Value::Float(t));
        let _ = ctx.set_value("t_in_state".into(), Value::Float(self.t_in_state));
        for (i, name) in self.fsm_states.iter().enumerate() {
            let on = if i == self.state_idx { 1.0 } else { 0.0 };
            let _ = ctx.set_value(format!("state_{name}"), Value::Float(on));
        }
        ctx
    }

    /// Evaluate FSM guards against the context; fire the first that holds.
    fn advance_fsm(&mut self, ctx: &HashMapContext, dt: f64) {
        self.t_in_state += dt;
        if self.fsm_states.is_empty() {
            return;
        }
        let cur = self.state().to_string();
        for ct in &self.fsm_transitions {
            let tr = &ct.tr;
            if tr.from != cur {
                continue;
            }
            if let Some(min) = tr.min_dwell_s {
                if self.t_in_state < min {
                    continue;
                }
            }
            // Guard compiled once at stamp time; a guard that failed to parse is
            // `None` and never fires.
            let fired = ct.guard.as_ref().is_some_and(|p| guard_true(p, ctx));
            if fired {
                if let Some(idx) = self.fsm_states.iter().position(|s| s == &tr.to) {
                    self.state_idx = idx;
                    self.t_in_state = 0.0;
                }
                break;
            }
        }
    }

    /// Retune open-drain and drive legs from the active state's pin overrides.
    fn apply_state_pins(&mut self, circuit: &mut Circuit) {
        let active = self.state().to_string();
        let overrides = self.state_pins.get(&active).cloned().unwrap_or_default();

        // Open drains: assert (on_ohms) when the active state lists od_assert.
        for od in &self.open_drains {
            let assert = overrides
                .get(&od.pin_role)
                .map(|b| b.od_assert)
                .unwrap_or(false);
            let ohms = if assert { od.on_ohms } else { OFF_OHMS };
            set_resistor_ohms(circuit, od.resistor, ohms);
        }
        // Drives: when the active state drives this pin, set the source + on R;
        // otherwise tri-state via OFF_OHMS.
        for d in &self.drives {
            match overrides.get(&d.pin_role) {
                Some(b) if b.drive_volts.is_some() && !b.hi_z => {
                    set_source_dc(circuit, d.vsource, b.drive_volts.unwrap());
                    set_resistor_ohms(circuit, d.resistor, b.drive_ohms.unwrap_or(d.on_ohms));
                }
                _ => set_resistor_ohms(circuit, d.resistor, OFF_OHMS),
            }
        }
    }

    /// Converter regulation: hold `vout_setpoint`, fold the output back under an
    /// output-current limit, and throttle so the reflected input draw never
    /// exceeds the (programmable) input-current limit. Mirrors
    /// `power_supply::cc_regulate`, anchored to the previous command.
    fn update_converter(&mut self, circuit: &mut Circuit, node_v: &dyn Fn(NodeId) -> f64) {
        let c = self.converter.as_mut().unwrap();
        if c.cfg.iin_program.is_some() && c.iin_limit_a.is_none() {
            c.last_iout_a = 0.0;
            c.last_iin_a = 0.0;
            set_source_dc(circuit, c.out_vsource, 0.0);
            set_source_dc(circuit, c.in_isource, 0.0);
            return;
        }
        // Delivered output current = the drop across the output series resistor
        // divided by its resistance. We read this from node voltages (always in
        // the solver's global x) rather than the output Vsource's branch current
        // unknown, because the partitioned solver's global x carries node
        // voltages only, a branch read would silently return 0 whenever the
        // fast path is taken.
        let iout = ((node_v(c.out_drv_node) - node_v(c.out_node)) / c.out_r_ohms).abs();
        let vout = node_v(c.out_node).max(1e-6);
        let vin = node_v(c.in_node).max(1e-6);
        // Validation (`hauksbee-models::validate_behavioral`) accepts any
        // efficiency in (0,1]; honour it. The tiny floor is purely a divide
        // guard for the reflected-current computation below, not a physics
        // clamp, a 1% efficient stage really does reflect 100x the output
        // power onto its input.
        let eff = c.cfg.efficiency.unwrap_or(0.9).clamp(1e-9, 1.0);

        // Reflected input current for this output power.
        let mut iin = (vout * iout) / (eff * vin);

        // Output CV/CC: if iout exceeds the output limit, fold vout back so the
        // current is held at the limit (anchor to the last command).
        let mut v_cmd = c.cfg.vout_setpoint;
        if let Some(ilim) = c.cfg.iout_limit_a {
            if iout > ilim && iout > 1e-9 {
                let v_cc = ilim * (c.last_cmd_vout.max(1e-6) / iout);
                v_cmd = v_cmd.min(v_cc);
            }
        }

        // Input-current limit: throttle the output further so the input draw
        // is held at the limit. Anchored to the *previous command* like the
        // output-CC fold and `cc_regulate`, and ungated (computed every chunk,
        // applied through `min`) so an under-limit chunk holds the CC point
        // instead of resetting to the setpoint. The reflected draw scales with
        // the SQUARE of the command into a resistive load (iin ∝ vout·iout),
        // so the command that draws exactly the limit is
        // `last_cmd·sqrt(limit/iin)`; a setpoint-scaled (or linear-anchored)
        // law is a period-2 limit cycle that never settles.
        if let Some(iin_limit_a) = c.iin_limit_a.filter(|_| iin > 1e-9) {
            let v_in_lim = c.last_cmd_vout.max(1e-6) * (iin_limit_a / iin).sqrt();
            if v_in_lim < v_cmd {
                v_cmd = v_in_lim;
                // The output runs THIS chunk at the throttled command, so the
                // input Isource must draw that operating point's power, not
                // the stale measured one: re-reflect through the load
                // conductance seen this chunk (iout/vout). Otherwise the
                // output would deliver full power while the input drew the
                // previous under-limit current, energy from nowhere, and the
                // limit never actually enforced.
                iin = (v_cmd * v_cmd * (iout / vout)) / (eff * vin);
            }
        }
        v_cmd = v_cmd.clamp(0.0, c.cfg.vout_setpoint);

        c.last_cmd_vout = v_cmd;
        c.last_iout_a = iout;
        c.last_iin_a = iin;

        let out_v = c.out_vsource;
        let in_i = c.in_isource;
        set_source_dc(circuit, out_v, v_cmd);
        set_source_dc(circuit, in_i, iin);
        // The input-overdraw fault against a brick-power budget is raised
        // separately by [`Self::check_input_budget`], which the scheduler calls
        // with the configured supply budget after the leg has been updated.
    }

    /// Re-evaluate the converter's input-current limit from the board (called if
    /// a programming resistor changes). Missing required evidence disables the
    /// converter sources on their next update. Returns the new limit.
    pub fn recompute_iin_limit(
        &mut self,
        board_resistor: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<f64> {
        let c = self.converter.as_mut()?;
        let lim = resolve_iin_limit(&c.cfg, board_resistor);
        c.iin_limit_a = lim;
        lim
    }

    /// Raise an input-overdraw fault if the converter is drawing more input
    /// power than `budget_w` from its supply. Called by the scheduler with the
    /// configured brick budget. Latches so it fires once per crossing.
    pub fn check_input_budget(&mut self, vin: f64, budget_w: f64, t: f64) {
        let Some(c) = &self.converter else { return };
        let in_power = c.last_iin_a * vin.max(0.0);
        if in_power > budget_w {
            if !self.overdraw_latched {
                self.overdraw_latched = true;
                self.pending_faults.push(FaultEvent {
                    component: self.reference.clone(),
                    kind: FaultKind::Overpower,
                    value: in_power,
                    limit: budget_w,
                    t,
                    destroyed: false,
                });
            }
        } else if in_power < budget_w * 0.9 {
            self.overdraw_latched = false;
        }
    }
}

/// Compute the input-current limit from a converter's sense program, reading
/// board resistor values where references are given. Returns None if a literal
/// limit should be used instead (no program present).
fn resolve_iin_limit(c: &Converter, board_resistor: &dyn Fn(&str) -> Option<f64>) -> Option<f64> {
    if let Some(sp) = &c.iin_program {
        return program_iin_limit(sp, board_resistor);
    }
    c.iin_limit_a
}

/// The programmed input-current limit for a [`SenseProgram`].
///
/// The part regulates its input-shunt voltage to a current-sense threshold set
/// by the external programming resistor, then the limit is that threshold over
/// the sense resistor: `i = v_sense / rsense`. The threshold scales *linearly*
/// with the programming resistor up to a full-scale ceiling:
///
/// ```text
/// v_sense = min(vprog_ref * prog / prog_ref_ohms, v_sense_full)
/// i_limit = v_sense / rsense
/// ```
///
/// so a LARGER programming resistor gives a HIGHER current limit (saturating at
/// `v_sense_full`). This is the LTC4020 ILIMIT direction: the Reform handbook
/// fix dropped R8 from 100k to 7.15k to LOWER the input-current limit. Its model
/// uses the published 50 uA * RILIMIT and 50 mV sense-limit relationship;
/// `rsense` and `prog` are read off the actual board (R49 and R8), so the limit
/// moves when the board resistor moves, with no model edit.
pub fn program_iin_limit(
    sp: &SenseProgram,
    board_resistor: &dyn Fn(&str) -> Option<f64>,
) -> Option<f64> {
    // A named board value is evidence, not a convenience hint. If any named
    // resistor is absent (including because its component identity was
    // refused), the programmed limit is unknown. Falling back to a literal in
    // that case manufactures a precise limit for a circuit we could not bind.
    let rsense = if sp.rsense_refs.is_empty() {
        sp.rsense_ohms
    } else {
        let mut values = sp
            .rsense_refs
            .iter()
            .map(|reference| board_resistor(reference));
        let first = values.next().flatten()?;
        if !first.is_finite() || first <= 0.0 {
            return None;
        }
        for value in values {
            let value = value?;
            if !value.is_finite()
                || value <= 0.0
                || (value - first).abs()
                    > first
                        .abs()
                        .max(value.abs())
                        .max(MATCHED_SHUNT_MIN_SCALE_OHMS)
                        * MATCHED_SHUNT_RELATIVE_TOLERANCE
            {
                return None;
            }
        }
        Some(first)
    }?;
    let prog = match sp.prog_ref.as_deref() {
        Some(reference) => board_resistor(reference),
        None => sp.prog_ohms,
    }?;
    if !rsense.is_finite() || rsense <= 0.0 || !prog.is_finite() || prog <= 0.0 {
        return None;
    }
    let prog_ref = sp.prog_ref_ohms;
    let v_sense = (sp.vprog_ref * prog / prog_ref)
        .min(sp.v_sense_full)
        .max(0.0);
    let limit = v_sense / rsense;
    (limit.is_finite() && limit > 0.0).then_some(limit)
}

// ── small circuit-mutation helpers (mirror power_supply/drivers) ────────────

fn set_source_dc(circuit: &mut Circuit, id: DeviceId, v: f64) {
    match circuit.devices.get_mut(id.0 as usize) {
        Some(Device::Vsource { kind, .. }) | Some(Device::Isource { kind, .. }) => {
            *kind = SourceKind::Dc(v);
        }
        _ => {}
    }
}

fn set_resistor_ohms(circuit: &mut Circuit, id: DeviceId, ohms: f64) {
    if let Some(Device::Resistor { ohms: r, .. }) = circuit.devices.get_mut(id.0 as usize) {
        *r = ohms;
    }
}

/// Evaluate a parsed expression to a number against a context.
fn eval_number(program: &EvalNode, ctx: &HashMapContext) -> Option<f64> {
    program.eval_with_context(ctx).ok().and_then(|v| match v {
        Value::Float(f) => Some(f),
        Value::Int(i) => Some(i as f64),
        Value::Boolean(b) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    })
}

/// Evaluate a guard expression as a boolean: true for `true` or any non-zero
/// number.
fn guard_true(program: &EvalNode, ctx: &HashMapContext) -> bool {
    match program.eval_with_context(ctx) {
        Ok(Value::Boolean(b)) => b,
        Ok(Value::Float(f)) => f != 0.0,
        Ok(Value::Int(i)) => i != 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> BTreeMap<String, NodeId> {
        let mut r = BTreeMap::new();
        r.insert("io".to_string(), NodeId(1));
        r.insert("vbus".to_string(), NodeId(2));
        r.insert("gnd".to_string(), NodeId::GROUND);
        r
    }

    fn params() -> Params {
        let mut p = Params::default();
        p.set_f64("vt_clamp", 1.1);
        p.set_f64("rd_clamp", 0.5);
        p
    }

    /// The USBLC6 clamp shape compiles: pin voltages become dependency slots
    /// (one per distinct pin), params fold to literals, and the canonical text
    /// carries no law-local identifiers.
    #[test]
    fn clamp_law_compiles_to_canonical_form() {
        let (expr, deps) = compile_law_implicit(
            "max(0.0, (v_io - v_vbus - vt_clamp) / rd_clamp)",
            &roles(),
            &params(),
        )
        .expect("pure pin-voltage law must compile");
        assert_eq!(deps.len(), 2, "one slot per distinct pin voltage");
        assert!(matches!(deps[0], hauksbee_ir::BDep::Volt(NodeId(1))));
        assert!(matches!(deps[1], hauksbee_ir::BDep::Volt(NodeId(2))));
        assert!(expr.src().contains("__d0") && expr.src().contains("__d1"));
        assert!(
            expr.src().contains("(1.1)") && expr.src().contains("(0.5)"),
            "params must fold to literals, got: {}",
            expr.src()
        );
    }

    /// A pin read twice shares one dependency slot.
    #[test]
    fn repeated_pin_reads_share_a_slot() {
        let (_, deps) = compile_law_implicit("v_io + max(0.0, v_io - v_vbus)", &roles(), &params())
            .expect("compiles");
        assert_eq!(deps.len(), 2);
    }

    /// Scientific-notation literals are numbers, not identifiers: the law
    /// stays implicit rather than silently downgrading to the runtime form.
    #[test]
    fn scientific_notation_is_not_an_identifier() {
        let (expr, deps) =
            compile_law_implicit("1e-6 * v_io + 2.5E+2", &roles(), &params()).expect("compiles");
        assert_eq!(deps.len(), 1);
        assert!(expr.src().contains("1e-6") && expr.src().contains("2.5E+2"));
    }

    /// Time-varying laws refuse the implicit form: the runtime's `t` is the
    /// GLOBAL sim clock, a B-source `time` is the transient's chunk-local
    /// clock (restarting at 0 every chunk), and swapping one for the other
    /// would silently change what the law computes.
    #[test]
    fn time_varying_laws_refuse_implicit() {
        assert!(compile_law_implicit("v_io * t", &roles(), &params()).is_none());
        assert!(compile_law_implicit("v_io * time", &roles(), &params()).is_none());
    }

    /// A param sharing a `v_<role>` name shadows the pin voltage, exactly as
    /// the runtime context resolves it (params are set after pin voltages).
    #[test]
    fn param_shadows_same_named_pin_voltage() {
        let mut p = params();
        p.set_f64("v_io", 42.0);
        let (expr, deps) = compile_law_implicit("v_io + v_vbus", &roles(), &p).expect("compiles");
        assert_eq!(deps.len(), 1, "the shadowed pin must not become a dep");
        assert!(matches!(deps[0], hauksbee_ir::BDep::Volt(NodeId(2))));
        assert!(
            expr.src().contains("(42.0)"),
            "the param value must fold, got: {}",
            expr.src()
        );
    }

    /// FSM context (state booleans, `t_in_state`) and unconnected pins cannot
    /// be expressed implicitly: the compiler must refuse so the runtime form
    /// (the only one with that context) is kept.
    #[test]
    fn fsm_context_and_unconnected_pins_refuse() {
        assert!(compile_law_implicit("v_io * state_on", &roles(), &params()).is_none());
        assert!(compile_law_implicit("v_io + t_in_state", &roles(), &params()).is_none());
        assert!(
            compile_law_implicit("v_nc / 2.0", &roles(), &params()).is_none(),
            "an unconnected pin's voltage must refuse, not read as 0"
        );
    }

    /// Runtime-owned names refuse even when a numeric param shares the name:
    /// `build_context` sets `t`/`t_in_state`/`state_<name>` AFTER params, so
    /// the FSM value (not the param) wins at runtime, and folding the param
    /// would silently change what the law computes.
    #[test]
    fn runtime_owned_names_never_fold_from_params() {
        let mut p = params();
        p.set_f64("state_on", 2.0);
        p.set_f64("t_in_state", 7.0);
        p.set_f64("t", 3.0);
        assert!(compile_law_implicit("v_io * state_on", &roles(), &p).is_none());
        assert!(compile_law_implicit("v_io + t_in_state", &roles(), &p).is_none());
        assert!(compile_law_implicit("v_io * t", &roles(), &p).is_none());
    }

    /// The probe evaluation gates the function vocabulary: an unknown or
    /// misnamespaced function refuses the implicit path (compiled, it would
    /// fault inside Newton at every stamp, killing the analog session, where
    /// the runtime form evaluates the same law to a guarded 0 A), while
    /// everything evalexpr genuinely evaluates passes, with no allowlist to
    /// drift out of date.
    #[test]
    fn unknown_functions_refuse_known_ones_pass() {
        assert!(compile_law_implicit("bogus(v_io)", &roles(), &params()).is_none());
        assert!(compile_law_implicit("math::bogus(v_io)", &roles(), &params()).is_none());
        // evalexpr's vocabulary is namespaced for math functions: bare `exp`
        // does not exist and must refuse, `math::exp` must pass.
        assert!(compile_law_implicit("exp(v_io)", &roles(), &params()).is_none());
        assert!(compile_law_implicit("math::exp(v_io)", &roles(), &params()).is_some());
        assert!(compile_law_implicit("max(0.0, v_io)", &roles(), &params()).is_some());
        assert!(compile_law_implicit("min(v_io, v_vbus)", &roles(), &params()).is_some());
    }

    /// A law that probes non-finite refuses the implicit path and keeps the
    /// runtime form's clamp-to-0-A contract (pinned by hauksbee-ci's
    /// `an_unevaluable_behavioural_law_is_clamped_rather_than_poisoning_the_solve`):
    /// the canonical case is a division by a parameter that folded to zero.
    #[test]
    fn non_finite_probe_refuses_and_keeps_the_clamp_contract() {
        let mut p = params();
        p.set_f64("sense_ohms", 0.0);
        assert!(compile_law_implicit("v_io / sense_ohms", &roles(), &p).is_none());
        assert!(compile_law_implicit("math::ln(v_io + 0.0)", &roles(), &p).is_none());
    }
}
