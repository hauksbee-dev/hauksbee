//! Behavioural-device runtime.
//!
//! This is the engine-side realisation of the declarative
//! [`Behavioral`] layer. A behavioural
//! device participates in the solve loop exactly the way the configurable power
//! supplies do (`power_supply.rs`): it stamps controllable Thevenin legs and
//! sense resistors into the [`Circuit`] once, and the scheduler calls
//! [`BehavioralDevice::update`] between solver chunks to recompute each leg's
//! source value from the previous chunk's solved node voltages and branch
//! currents. It does NOT add new device kinds to the inner Newton loop, every
//! behaviour is expressed in terms of the existing `Vsource` / `Isource` /
//! `Resistor` primitives, so the partitioned solver is untouched.
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
//! - a **law** is a controllable `Isource` (current law) or `Vsource`+R
//!   (voltage law) whose value is the `evalexpr` expression evaluated against
//!   the bound pin voltages / state / params each chunk.
//!
//! The FSM advances once per chunk: its guards are `evalexpr` booleans over the
//! same context. Per-state pin overrides retune the open-drain / drive legs.

use std::collections::BTreeMap;

use evalexpr::{
    ContextWithMutableVariables, DefaultNumericTypes, HashMapContext, Node as EvalNode, Value,
};
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
    iin_limit_a: f64,
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
    program: EvalNode<DefaultNumericTypes>,
    /// The controllable source device (Isource for current, Vsource for voltage).
    source: DeviceId,
    /// For a Voltage law, its series output resistor and that resistor's
    /// on-resistance (ohms). Deactivating an `only_in_state` voltage law must
    /// tri-state this resistor (OFF_OHMS) to RELEASE the pin, zeroing the
    /// Vsource alone would clamp the pin to 0 V through the stiff series R (a
    /// near-short). A Current law releases correctly by zeroing its Isource, so
    /// it carries `None` here.
    series_r: Option<(DeviceId, f64)>,
}

/// One FSM transition with its guard compiled once at stamp time. Re-parsing the
/// guard string every chunk was wasteful and swallowed parse errors silently; a
/// guard that fails to compile is reported once here and stored as `None` (never
/// fires) rather than being re-parsed (and re-failing) on every chunk.
#[derive(Debug, Clone)]
struct CompiledTransition {
    tr: hauksbee_models::behavioral::Transition,
    guard: Option<EvalNode<DefaultNumericTypes>>,
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
    /// reflected input draw exceeds the budget. This is the "88 W from a 60 W
    /// brick" check.
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
                    let guard =
                        match evalexpr::build_operator_tree::<DefaultNumericTypes>(&tr.guard) {
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
                let iin_limit_a = resolve_iin_limit(c, board_resistor).unwrap_or(f64::INFINITY);
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
            let program = match evalexpr::build_operator_tree::<DefaultNumericTypes>(&law.expr) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "[behavioural] {reference}: law '{}' expr failed to parse: {e}; skipped",
                        law.name
                    );
                    continue;
                }
            };
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
                source,
                series_r,
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

    /// The effective input-current limit of the converter (A), or None.
    pub fn converter_iin_limit(&self) -> Option<f64> {
        self.converter.as_ref().map(|c| c.iin_limit_a)
    }

    /// Last delivered output current of the converter (A), or None.
    pub fn converter_iout(&self) -> Option<f64> {
        self.converter.as_ref().map(|c| c.last_iout_a)
    }

    /// Last reflected input current the converter drew (A), or None.
    pub fn converter_iin(&self) -> Option<f64> {
        self.converter.as_ref().map(|c| c.last_iin_a)
    }

    /// The current value (A) of a named current-law source, read from the
    /// circuit (the value the runtime last wrote). `None` if no such law. Used by
    /// the balancer-leak validation to read the leak magnitude directly.
    pub fn law_value(&self, circuit: &Circuit, name: &str) -> Option<f64> {
        let leg = self.laws.iter().find(|l| l.law.name == name)?;
        match circuit.devices.get(leg.source.0 as usize) {
            Some(Device::Isource {
                kind: SourceKind::Dc(v),
                ..
            })
            | Some(Device::Vsource {
                kind: SourceKind::Dc(v),
                ..
            }) => Some(*v),
            _ => None,
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
            if let Some(req) = &leg.law.only_in_state {
                if req != &active {
                    // Deactivate: RELEASE the pin. A voltage law tri-states its
                    // series resistor (OFF_OHMS) so the pin floats; leaving the
                    // resistor on while zeroing the Vsource would clamp the pin to
                    // 0 V through the stiff series R (a near-short). A current law
                    // releases by zeroing its Isource.
                    if let Some((r, _)) = leg.series_r {
                        set_resistor_ohms(circuit, r, OFF_OHMS);
                    }
                    set_source_dc(circuit, leg.source, 0.0);
                    continue;
                }
            }
            // Active: restore the series resistor's on-resistance in case a prior
            // chunk tri-stated it.
            if let Some((r, on)) = leg.series_r {
                set_resistor_ohms(circuit, r, on);
            }
            // Guard against a non-finite law value (e.g. a divide-by-zero when a
            // programming resistor is a 0-ohm jumper): a NaN/Inf source would
            // blow up the solve. Clamp to 0 rather than poison the matrix.
            let val = eval_number(&leg.program, &ctx).unwrap_or(0.0);
            let val = if val.is_finite() { val } else { 0.0 };
            set_source_dc(circuit, leg.source, val);
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
    fn build_context(
        &self,
        node_v: &dyn Fn(NodeId) -> f64,
        t: f64,
    ) -> HashMapContext<DefaultNumericTypes> {
        let mut ctx = HashMapContext::<DefaultNumericTypes>::new();
        for (role, &node) in &self.role_nodes {
            let _ = ctx.set_value(format!("v_{role}"), Value::from_float(node_v(node)));
        }
        for (k, v) in &self.params.0 {
            if let Some(f) = v.as_f64() {
                let _ = ctx.set_value(k.clone(), Value::from_float(f));
            }
        }
        let _ = ctx.set_value("t".into(), Value::from_float(t));
        let _ = ctx.set_value("t_in_state".into(), Value::from_float(self.t_in_state));
        for (i, name) in self.fsm_states.iter().enumerate() {
            let on = if i == self.state_idx { 1.0 } else { 0.0 };
            let _ = ctx.set_value(format!("state_{name}"), Value::from_float(on));
        }
        ctx
    }

    /// Evaluate FSM guards against the context; fire the first that holds.
    fn advance_fsm(&mut self, ctx: &HashMapContext<DefaultNumericTypes>, dt: f64) {
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
        if c.iin_limit_a.is_finite() && iin > 1e-9 {
            let v_in_lim = c.last_cmd_vout.max(1e-6) * (c.iin_limit_a / iin).sqrt();
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
    /// a programming resistor changes). Returns the new limit.
    pub fn recompute_iin_limit(
        &mut self,
        board_resistor: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<f64> {
        let c = self.converter.as_mut()?;
        let lim = resolve_iin_limit(&c.cfg, board_resistor).unwrap_or(f64::INFINITY);
        c.iin_limit_a = lim;
        Some(lim)
    }

    /// Raise an input-overdraw fault if the converter is drawing more input
    /// power than `budget_w` from its supply (the "88 W from a 60 W brick"
    /// check). Called by the scheduler with the configured brick budget. Latches
    /// so it fires once per crossing.
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
        return Some(program_iin_limit(sp, board_resistor));
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
/// fix dropped R8 from 100k to 7.15k to LOWER the input-current limit, and with
/// `prog_ref_ohms` / `vprog_ref` / `v_sense_full` chosen from the part's ILIMIT
/// programming curve the two real board values straddle the 60 W brick budget
/// (100k saturates well above 60 W, 7.15k lands at ~60 W). `rsense` and `prog`
/// are read off the actual board (R49 and R8), so the limit moves when the board
/// resistor moves, with no model edit, which is exactly the fix.
pub fn program_iin_limit(sp: &SenseProgram, board_resistor: &dyn Fn(&str) -> Option<f64>) -> f64 {
    let rsense = sp
        .rsense_ref
        .as_ref()
        .and_then(|r| board_resistor(r))
        .or(sp.rsense_ohms)
        .unwrap_or(0.01)
        .max(1e-6);
    let prog = sp
        .prog_ref
        .as_ref()
        .and_then(|r| board_resistor(r))
        .or(sp.prog_ohms)
        .unwrap_or(sp.prog_ref_ohms)
        .max(1.0);
    let prog_ref = sp.prog_ref_ohms.max(1.0);
    let v_sense = (sp.vprog_ref * prog / prog_ref)
        .min(sp.v_sense_full)
        .max(0.0);
    v_sense / rsense
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
fn eval_number(
    program: &EvalNode<DefaultNumericTypes>,
    ctx: &HashMapContext<DefaultNumericTypes>,
) -> Option<f64> {
    program.eval_with_context(ctx).ok().and_then(|v| match v {
        Value::Float(f) => Some(f),
        Value::Int(i) => Some(i as f64),
        Value::Boolean(b) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    })
}

/// Evaluate a guard expression as a boolean: true for `true` or any non-zero
/// number.
fn guard_true(
    program: &EvalNode<DefaultNumericTypes>,
    ctx: &HashMapContext<DefaultNumericTypes>,
) -> bool {
    match program.eval_with_context(ctx) {
        Ok(Value::Boolean(b)) => b,
        Ok(Value::Float(f)) => f != 0.0,
        Ok(Value::Int(i)) => i != 0,
        _ => false,
    }
}
