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
//!   scheduler can read the delivered current), driven by an in-circuit
//!   regulation loop, PLUS a behavioural current on the input pin that draws
//!   the switch-node power `V(sw)*Iout` back from the input, divided by `eff`
//!   forward and multiplied by it in reverse, so power is conserved inside the
//!   solve. Arming and the programmable input-current limit update each chunk.
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

use hauksbee_ir::eprintln;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use evalexpr::{ContextWithMutableVariables, HashMapContext, Node as EvalNode, Value};
use hauksbee_ir::{BDep, BOutput, Circuit, CompiledExpr, Device, DeviceId, NodeId, SourceKind};
use hauksbee_models::behavioral::{
    Behavioral, Converter, EnableControl, Law, LawKind, SenseProgram, StatePinBehaviour, Topology,
};
use hauksbee_models::{LoadProfile, Params};

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
    // Implementors may return a label borrowed from `self`; only this default is static.
    #[allow(clippy::unnecessary_literal_bound)]
    fn state(&self) -> &str {
        ""
    }
}

/// Builds a fresh custom device instance for one matching component.
type CustomFactory = Box<dyn Fn() -> Box<dyn CustomBehavior> + Send + Sync>;

/// Registry of custom-behaviour factories.
///
/// Keyed by an exact match string tested against a component's resolved model
/// id, value, or MPN. A user (or a downstream crate) registers a factory before binding; the binder consults the
/// registry for every component and, on a hit, instantiates the custom device
/// instead of (or alongside) the declarative one.
#[derive(Default)]
pub struct CustomRegistry {
    factories: Vec<(String, CustomFactory)>,
}

impl CustomRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    #[must_use]
    pub fn build_for(&self, keys: &[&str]) -> Option<Box<dyn CustomBehavior>> {
        for (k, factory) in &self.factories {
            if keys.iter().any(|key| key.to_ascii_lowercase() == *k) {
                return Some(factory());
            }
        }
        None
    }

    /// True when no factories are registered (the common case).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

/// Tiny output impedance for behavioural sources so the source branch current
/// is well defined (mirrors `power_supply::STIFF_R_OHMS`).
const STIFF_R_OHMS: f64 = 1e-3;

/// Intern a hidden node holding a literal DC voltage, for a model-internal rail
/// (a pull target or an open-drain sink) the board itself does not name.
fn hidden_dc_node(
    circuit: &mut Circuit,
    reference: &str,
    tag: &str,
    volts: f64,
) -> (NodeId, DeviceId) {
    let node = circuit.node(&format!("__beh_{reference}_{tag}"));
    let name = format!("Vbeh_{reference}_{tag}");
    let source = Device::vsource(name, node, NodeId::GROUND, SourceKind::Dc(volts));
    (node, circuit.add(source))
}

/// A named device's terminals.
fn terminals(circuit: &Circuit, name: &str) -> Option<Vec<NodeId>> {
    circuit
        .iter()
        .find(|(_, d)| d.name() == name)
        .map(|(_, d)| d.nodes())
}

/// The node a converter regulates: the far end of the one output inductor
/// (through its bound DCR) leaving `out`, else `out` itself.
fn regulated_node(circuit: &Circuit, out: NodeId) -> NodeId {
    let mut far = circuit.iter().filter_map(|(_, d)| match d {
        Device::Inductor { name, a, b, .. } if !name.ends_with("_esl") => {
            let b = terminals(circuit, &format!("{name}_dcr")).map_or(*b, |t| t[1]);
            (*a == out)
                .then_some(b)
                .or_else(|| (b == out).then_some(*a))
        }
        _ => None,
    });
    match (far.next(), far.next()) {
        (Some(reg), None) => reg,
        _ => out,
    }
}

/// Capacitance (F) from `node` to ground, bound ESR/ESL chains followed.
fn out_farads(circuit: &Circuit, node: NodeId) -> f64 {
    let farads = |(_, d): (DeviceId, &Device)| match d {
        Device::Capacitor {
            name, a, b, farads, ..
        } => {
            let chain = |part| terminals(circuit, &format!("{name}_{part}")).map(|t| t[0]);
            let ends = [chain("esr").or_else(|| chain("esl")).unwrap_or(*a), *b];
            (ends.contains(&node) && ends.contains(&NodeId::GROUND)).then_some(*farads)
        }
        _ => None,
    };
    circuit.iter().filter_map(farads).sum()
}

/// The largest `C_out / k` whose full loop gain a DC solve takes (the largest
/// output measured to converge at every crossover from 10 kHz to 200 kHz).
const DC_FARADS: f64 = 22e-6;

/// The converter loop's delivered-current (`i - V(i)`, onto its hidden node),
/// PI-state, `g*e` and input-draw expressions over deps
/// `[out, pi, g*e, lim, vin, sense, ref, sw, i]`, then the lockout's `on`
/// level when `lockout` is set (see `stamp_converter`), for
/// `farads = C_out / k` and an input-current limit `full_lim` (1 if
/// none). "No limit" is 1 kA or 10 kV, not 1e9: a rounded clamp's `hypot`
/// difference near 1e9 would lose a probe's digits.
///
/// A DC solve (the only one at `time = 0`, SPICE's convention: every transient
/// step is at `t > 0`) scales the command's `g*e` and the leak `wl` by one
/// factor, down to the gain a `DC_FARADS` output would have. The operating
/// point, `e = wl*x/(wz*g)`, is unchanged, but a gain that scales with `C_out`
/// would, with the output capacitor open, kick a millifarad loop from one clamp
/// to the other each Newton iteration. A clamped loop's DC point moves by the
/// clamp's rounding, so an output at or below `DC_FARADS` keeps its real gain.
/// The factor also keeps the clamp's linear span, `cap/(g*dc)`, at least a
/// fifth of `V(ref)`: an input-limited `cap` of milliamps left a sub-millivolt
/// span that Newton stepped straight across, from clamp to clamp. The clamp's
/// slope at zero is 1 (`ask` scaled by `hypot(1, 0.02)`): its rounding's 0.9998
/// held the DC point's `g*e` at `2e-4*x/dc`, a step in `i` as `dc` went to 1
/// that LTE-rejected a 1 MΩ, 4.7 mF rail's first step down to femtoseconds.
///
/// The input-power cap `P/V` continues below 1 mV along its tangent, not flat:
/// a flat floor sent a Newton iterate at 0 V to `P/1 mV` and cycled from there.
///
/// The draw reads `V(sw)` held to `[0, hi]`, where the solved switch node
/// always sits: a Newton iterate with the input near 0 V and the switch node
/// not yet would otherwise divide into a kiloamp draw and throw the input rail.
/// Largest output/input voltage ratio an averaged boost or buck-boost stage is
/// allowed: a 95 % duty cycle, beyond any practical converter's maximum.
const MAX_STEP_UP_RATIO: f64 = 20.0;

fn loop_exprs(
    c: &Converter,
    farads: f64,
    r: f64,
    eff: f64,
    full_lim: f64,
    lockout: bool,
) -> [String; 4] {
    let wc = std::f64::consts::TAU * c.loop_crossover_hz().0;
    let (g, wz, wl) = (wc * farads, wc / 4.0, wc * 1e-5);
    let dmax = c.max_duty.unwrap_or(1.0);
    let armed = format!("min(max(__d3*{:?},0.0),1.0)", 1.0 / full_lim.min(0.01));
    let hi = match c.topology {
        Topology::Buck => format!("(1.0-{:?}*{armed})*max(__d4,0.0)", 1.0 - dmax),
        // A step-up stage moves energy from its input: its averaged switch
        // node is at most `vin / (1 - D)`. Bounding it by the input keeps a
        // dead (unfed) input from delivering power that the draw then has to
        // pull out of a floating net, which has no physical answer.
        _ => format!("{MAX_STEP_UP_RATIO:?}*max(__d4,0.0)"),
    };
    let iout_max = c.iout_limit_a.unwrap_or(1e3);
    let cap = if c.iin_limit_a.is_some() || c.iin_program.is_some() {
        let m = "max(__d0,1e-3)";
        format!("min({iout_max:?},max(__d3,0.0)*{eff:?}*max(__d4,0.0)*(2.0*{m}-__d0)/({m}*{m}))")
    } else {
        format!("{armed}*{iout_max:?}")
    };
    let dc = format!(
        "if(time>0.0,1.0,min({:?},({cap})/({g:?}*max(0.2*__d6,1e-6))))",
        (DC_FARADS / farads).min(1.0)
    );
    let ask = format!("{:?}*(__d1+{dc}*__d2)", 1.0_f64.hypot(0.02));
    let wid = format!("0.02*({cap})");
    let cmd = format!("0.5*(math::hypot({ask}+{cap},{wid})-math::hypot({ask}-({cap}),{wid}))");
    let top = format!("(({hi})-__d0)/{r:?}");
    let band = format!("max({:?}*__d4*{armed},1e-6)", 4e-3 / r);
    let dip = format!("max(1.0-math::abs({cmd}-{top})/{band},0.0)");
    let i = format!("max(min({cmd},{top})-0.25*{band}*{dip}*{dip},-__d0/{r:?})");
    // Below the input's lockout the stage is off: it neither sources nor
    // sinks, rather than holding the output at the dead input's 0 V. The
    // per-chunk update decides it, as it decides the enable, and holds the
    // answer on the hidden source `__d9` (1 V on, 0 V off): no Newton iterate
    // crosses the floor, so a solve sees the stage on or off throughout. The
    // scale saturates at 1% of the source, as `armed` does, so DC source
    // stepping keeps an on stage on along the homotopy.
    let on = if lockout {
        "min(max(__d9*100.0,0.0),1.0)*"
    } else {
        ""
    };
    let i = format!("{on}{i}-__d8");
    let pi = format!("1e-6*({wz:?}*(__d8-__d1)-{dc}*{wl:?}*__d1)");
    let (fwd, loss) = (f64::midpoint(1.0 / eff, eff), (1.0 / eff - eff) / 2.0);
    let pw = format!("max(min(__d7,{hi}),0.0)*__d8");
    let draw = format!("({fwd:?}*{pw}+{loss:?}*(math::hypot({pw},1e-4)-1e-4))/max(__d4,1e-3)");
    [i, pi, format!("{g:?}*(__d6-__d5)-__d2"), draw]
}

/// The most series links one divider leg is followed through.
const MAX_LEG_LINKS: usize = 8;

/// The fraction of the regulated voltage a resistive divider presents at a
/// feedback node.
///
/// Each resistor on the node starts a leg, followed on through any node
/// whose only other non-capacitor device is the chain's next resistor (a rail
/// always has a source on it, so no leg runs on through one), so a top leg built as 4.7 kΩ + 1 kΩ through a mid-node reads as
/// 5.7 kΩ. The ratio is the conductance of the legs to other nets over all
/// legs' (1 with no leg to another net), or `None` when no resistor touches
/// the node at all.
fn divider_ratio(circuit: &Circuit, fb: NodeId) -> Option<f64> {
    let devices: Vec<&Device> = circuit.iter().map(|(_, d)| d).collect();
    let (mut top, mut all) = (0.0, 0.0);
    for (first, d) in devices.iter().enumerate() {
        let Device::Resistor { a, b, ohms, .. } = d else {
            continue;
        };
        if (*a == fb) == (*b == fb) {
            continue;
        }
        let (mut far, mut total, mut last) = (if *a == fb { *b } else { *a }, *ohms, first);
        for _ in 1..MAX_LEG_LINKS {
            if far.is_ground() {
                break;
            }
            let others: Vec<usize> = (devices.iter().enumerate())
                .filter(|(j, d)| *j != last && !matches!(d, Device::Capacitor { .. }))
                .filter(|(_, d)| d.nodes().contains(&far))
                .map(|(j, _)| j)
                .collect();
            let [next] = others[..] else { break };
            let Device::Resistor {
                a: x, b: y, ohms, ..
            } = devices[next]
            else {
                break;
            };
            let (x, y, ohms) = (*x, *y, *ohms);
            let to = if x == far { y } else { x };
            if to == far || to == fb {
                break;
            }
            (far, total, last) = (to, total + ohms, next);
        }
        if total > 0.0 {
            let g = 1.0 / total;
            all += g;
            if !far.is_ground() {
                top += g;
            }
        }
    }
    if top > 0.0 {
        Some(top / all)
    } else {
        (all > 0.0).then_some(1.0)
    }
}

/// High-impedance "off" resistance for a tri-stated open-drain / drive leg.
const OFF_OHMS: f64 = 1e12;

/// Numerical tolerance when nominally matched shunts arrive through separate
/// parsed component records.
const MATCHED_SHUNT_RELATIVE_TOLERANCE: f64 = 1e-12;
const MATCHED_SHUNT_MIN_SCALE_OHMS: f64 = 1.0;

/// One open-drain leg: a resistor whose value we swap on/off by state.
#[derive(Debug, Clone)]
struct OpenDrainLeg {
    pin_role: String,
    resistor: DeviceId,
    on_ohms: f64,
}

/// A per-state push-pull drive leg (Vsource behind a resistor whose value is
/// swapped to `OFF_OHMS` when the state is inactive).
#[derive(Debug, Clone)]
struct DriveLeg {
    pin_role: String,
    vsource: DeviceId,
    resistor: DeviceId,
    on_ohms: f64,
}

/// A state-controlled resistor between two model pin roles. Its current from
/// `a` to `b` is exposed to FSM guards as `i_<name>`.
#[derive(Debug, Clone)]
struct SeriesPathLeg {
    name: String,
    a: NodeId,
    b: NodeId,
    resistor: DeviceId,
    default_ohms: f64,
    state_ohms: BTreeMap<String, f64>,
    current_ohms: f64,
}

/// A model-owned dynamic current sink. This uses the same [`LoadProfile`]
/// evaluator as scenario-level loads, but travels with the resolved part and
/// therefore needs no duplicate CI wiring.
#[derive(Debug, Clone)]
struct ProfiledLoadLeg {
    name: String,
    source: DeviceId,
    profile: LoadProfile,
    start_s: f64,
    seed: u64,
    last_i_a: f64,
    peak_i_a: f64,
    /// The card's `unknown_state_max_a`: its draw in a state the board cannot show.
    unknown_state_max_a: Option<f64>,
}

/// The stamped converter realisation.
#[derive(Debug, Clone)]
struct ConverterLeg {
    cfg: Converter,
    /// The 0 V ammeter the loop's averaged switch node drives through, the
    /// monitoring seam stress and dissipation read.
    out_vsource: DeviceId,
    out_node: NodeId,
    /// Optional enable input.  A low level disables both converter legs.
    enable_node: Option<NodeId>,
    /// False when a feedback pin the card needs is unconnected: never delivers.
    sensed: bool,
    /// The in-circuit input draw, a behavioural current from the input pin to
    /// ground that returns reverse power to the input.
    in_draw: DeviceId,
    /// Source holding the loop's input-current limit (A) on a hidden node the
    /// in-circuit current command reads; -1 V disarms the converter.
    limit_vsource: DeviceId,
    /// Effective input-current limit (A), computed from the sense program at
    /// bind time (or the literal). Recomputable if a board resistor is retuned.
    iin_limit_a: Option<f64>,
    /// Input current (A) the draw took at the last update.
    last_iin_a: f64,
    /// The input lockout, when the binder read a floor off the card.
    lockout: Option<InputLockout>,
}

/// A converter's input lockout: the stage is off while `input` is below
/// `floor_v`, which the per-chunk update decides and holds on `source`.
#[derive(Debug, Clone, Copy)]
struct InputLockout {
    input: NodeId,
    floor_v: f64,
    /// Hidden source the in-circuit current reads: 1 V on, 0 V off.
    source: DeviceId,
}

/// What the binder reads off the board for a converter before stamping it.
///
/// The default is a converter stamped from its card alone.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConverterBoard {
    /// The rail the output lands on, when not `out_pin`'s own node.
    ///
    /// Set when the card's `out_via` moves the output off `out_pin`'s net (the
    /// far side of a sense filter, see
    /// [`crate::sense_filter::sense_filter_net`]). `None`: `out_pin`'s node.
    pub out_node: Option<NodeId>,
    /// The input's lockout (V): the card's `supply_range` minimum on `in_pin`.
    ///
    /// Below it the converter neither sources nor sinks: its stage is off and
    /// the output is released to whatever else holds it (a battery through a
    /// power-path FET). `None`: the card states no input range.
    pub input_floor_v: Option<f64>,
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
    /// every law, current or voltage, whose expression is a pure function of
    /// pin voltages and params (no FSM state, no time, no `only_in_state`
    /// gating); a voltage law's device forces an internal node behind the
    /// law's series resistor.
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
        /// must tri-state this resistor (`OFF_OHMS`) to RELEASE the pin,
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
) -> Option<(CompiledExpr, Vec<BDep>)> {
    let mut out = String::with_capacity(expr.len());
    let mut deps: Vec<BDep> = Vec::new();
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
        let runtime_owned = token == "t"
            || token == "time"
            || token == "temperature_c"
            || token == "t_in_state"
            || token.starts_with("state_");
        if !runtime_owned {
            if let Some(v) = params.get_f64(token) {
                // Round-trip-exact literal, parenthesised so `1/x` stays `1/(v)`.
                let _ = write!(out, "({v:?})");
                continue;
            }
        }
        if let Some(role) = token.strip_prefix("v_") {
            if let Some(&node) = role_nodes.get(role) {
                let k = *slots.entry(role.to_string()).or_insert_with(|| {
                    deps.push(BDep::Volt(node));
                    deps.len() - 1
                });
                let _ = write!(out, "__d{k}");
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
    let compiled = CompiledExpr::compile(&out).ok()?;
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
#[derive(Default)]
pub struct BehavioralDevice {
    /// Component reference designator.
    pub reference: String,
    /// Role -> node for every connected, named pin of this device.
    role_nodes: BTreeMap<String, NodeId>,
    /// Static numeric params (from the model entry), bound into every context.
    params: Params,

    /// Count of static pull legs (a resistor from a pin to a rail) stamped for
    /// this device. The legs never change after stamping, so only their
    /// presence matters.
    pulls: usize,
    open_drains: Vec<OpenDrainLeg>,
    drives: Vec<DriveLeg>,
    series_paths: Vec<SeriesPathLeg>,
    profiled_loads: Vec<ProfiledLoadLeg>,
    converter: Option<ConverterLeg>,
    laws: Vec<LawLeg>,

    /// FSM state names (empty when the model has no FSM).
    fsm_states: Vec<String>,
    fsm_transitions: Vec<CompiledTransition>,
    /// Continuous true-time accumulated independently for each transition.
    transition_guard_true_s: Vec<f64>,
    /// state -> pin role -> behaviour override.
    state_pins: BTreeMap<String, BTreeMap<String, StatePinBehaviour>>,
    /// Current FSM state index, and time spent in it.
    state_idx: usize,
    t_in_state: f64,

    /// Optional input-power budget (W) and the input voltage it is referenced
    /// to (V): when set, an overpower fault is raised if the converter's
    /// input draw exceeds the budget. This covers reported charger
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
            c.out_node = map(c.out_node);
            c.enable_node = c.enable_node.map(&map);
        }
        for path in &mut self.series_paths {
            path.a = map(path.a);
            path.b = map(path.b);
        }
        // Profiled loads retain only their source DeviceId. Circuit device
        // terminals are remapped by the as-built circuit rewrite itself.
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
    ) -> Self {
        custom.stamp(circuit, reference, params, role_nodes);
        Self {
            reference: reference.to_string(),
            role_nodes: role_nodes.clone(),
            params: params.clone(),
            custom: Some(custom),
            ..Self::default()
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
    ) -> Option<Self> {
        let board = ConverterBoard::default();
        Self::stamp_on_board(
            circuit,
            reference,
            model,
            params,
            role_nodes,
            board_resistor,
            board,
        )
    }

    /// [`Self::stamp`], with what the binder read off the board.
    ///
    /// That is the rail a sense-filter output lands on and the converter
    /// input's lockout (see [`ConverterBoard`]).
    pub fn stamp_on_board(
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        params: &Params,
        role_nodes: &BTreeMap<String, NodeId>,
        board_resistor: &dyn Fn(&str) -> Option<f64>,
        board: ConverterBoard,
    ) -> Option<Self> {
        let mut dev = Self {
            reference: reference.to_string(),
            role_nodes: role_nodes.clone(),
            params: params.clone(),
            ..Self::default()
        };

        dev.stamp_pins(circuit, reference, model, role_nodes);
        dev.stamp_fsm(circuit, reference, model, role_nodes);
        dev.stamp_series_paths(circuit, reference, model, role_nodes);
        dev.stamp_profiled_loads(circuit, reference, model, role_nodes);
        dev.stamp_converter(circuit, reference, model, role_nodes, board_resistor, board);
        dev.stamp_laws(circuit, reference, model, params, role_nodes);

        if dev.is_inert() {
            None
        } else {
            Some(dev)
        }
    }

    /// Internal pull resistors and open-drain sink legs for every connected pin
    /// the model declares. A sink leg starts at [`OFF_OHMS`], i.e. released.
    fn stamp_pins(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        role_nodes: &BTreeMap<String, NodeId>,
    ) {
        for (role, pin) in &model.pins {
            let Some(&pin_node) = role_nodes.get(role) else {
                continue; // pin not connected on this board; nothing to stamp
            };
            // Internal pull to a rail.
            if let Some(ohms) = pin.pull_ohms {
                // A literal-voltage rail stamps a hidden stiff source node.
                let rail = pin
                    .pull_to_volts
                    .map(|v| hidden_dc_node(circuit, reference, &format!("{role}_rail"), v).0)
                    .or_else(|| {
                        let rail_role = pin.pull_to.as_ref()?;
                        role_nodes.get(rail_role).copied()
                    });
                if let Some(rail_node) = rail {
                    circuit.add(Device::resistor(
                        format!("Rbeh_{reference}_{role}_pull"),
                        pin_node,
                        rail_node,
                        ohms,
                    ));
                    self.pulls += 1;
                }
            }
            // Open-drain sink leg (starts off / high-impedance).
            if pin.open_drain {
                let sink = pin.od_to_volts.unwrap_or(0.0);
                // The emitter/source the sink returns to: an explicit `od_to`
                // role, else the part's own ground pin. Its net, not board
                // ground, is what an asserted output pulls toward.
                let emitter_node = match (&pin.od_to, pin.od_to_volts) {
                    (Some(r), _) => role_nodes.get(r).copied(),
                    (None, None) => ["gnd", "vss", "pgnd", "agnd"]
                        .iter()
                        .find_map(|r| role_nodes.get(*r).copied()),
                    (None, Some(_)) => None,
                };
                let sink_node = emitter_node.unwrap_or_else(|| {
                    if sink.abs() < 1e-12 {
                        NodeId::GROUND
                    } else {
                        hidden_dc_node(circuit, reference, &format!("{role}_odsink"), sink).0
                    }
                });
                let on_ohms = pin.od_ohms.unwrap_or(20.0);
                let r = circuit.add(Device::resistor(
                    format!("Rbeh_{reference}_{role}_od"),
                    pin_node,
                    sink_node,
                    OFF_OHMS,
                ));
                self.open_drains.push(OpenDrainLeg {
                    pin_role: role.clone(),
                    resistor: r,
                    on_ohms,
                });
            }
        }
    }

    /// The FSM: its compiled transition guards, its initial state, and one
    /// swappable drive leg per pin any state drives to a voltage.
    fn stamp_fsm(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        role_nodes: &BTreeMap<String, NodeId>,
    ) {
        if let Some(fsm) = &model.fsm {
            self.fsm_states.clone_from(&fsm.states);
            // Compile each transition guard once. A guard that fails to parse is
            // reported here (not silently, not re-parsed every chunk) and stored
            // as `None`, so it simply never fires.
            self.fsm_transitions = fsm
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
            self.transition_guard_true_s = vec![0.0; self.fsm_transitions.len()];
            self.state_pins = fsm.state_pins.clone();
            self.state_idx = fsm
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
                let vsource = circuit.add(Device::vsource(
                    format!("Vbeh_{reference}_{role}_drv"),
                    drv,
                    NodeId::GROUND,
                    SourceKind::Dc(0.0),
                ));
                let resistor = circuit.add(Device::resistor(
                    format!("Rbeh_{reference}_{role}_drv"),
                    drv,
                    pin_node,
                    OFF_OHMS,
                ));
                self.drives.push(DriveLeg {
                    pin_role: role,
                    vsource,
                    resistor,
                    on_ohms: 50.0,
                });
            }
        }
    }

    /// State-controlled series conduction paths (a load switch's pass element,
    /// a mux arm), stamped at whichever resistance the initial state selects.
    fn stamp_series_paths(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        role_nodes: &BTreeMap<String, NodeId>,
    ) {
        for path in &model.series_paths {
            let (Some(&a), Some(&b)) = (role_nodes.get(&path.a), role_nodes.get(&path.b)) else {
                continue;
            };
            let active = self.state();
            let initial_ohms = path
                .state_ohms
                .get(active)
                .copied()
                .unwrap_or(path.default_ohms);
            let resistor = circuit.add(Device::resistor(
                format!("Rbeh_{reference}_{}", path.name),
                a,
                b,
                initial_ohms,
            ));
            self.series_paths.push(SeriesPathLeg {
                name: path.name.clone(),
                a,
                b,
                resistor,
                default_ohms: path.default_ohms,
                state_ohms: path.state_ohms.clone(),
                current_ohms: initial_ohms,
            });
        }
    }

    /// Model-owned dynamic current profiles: one Isource per declared load,
    /// seeded at the profile's own pre-start current.
    fn stamp_profiled_loads(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        role_nodes: &BTreeMap<String, NodeId>,
    ) {
        for load in &model.profiled_loads {
            let Some(&supply) = role_nodes.get(&load.supply_pin) else {
                continue;
            };
            let return_node = match load.return_pin.as_ref() {
                Some(role) => {
                    let Some(&node) = role_nodes.get(role) else {
                        continue;
                    };
                    node
                }
                None => NodeId::GROUND,
            };
            let profile = LoadProfile {
                id: format!("model:{reference}:{}", load.name),
                description: format!("model-owned current profile for {reference}"),
                match_rule: hauksbee_models::profile::ProfileMatch::default(),
                segments: load.segments.clone(),
            };
            let initial_i = profile.current_at(-1.0, load.seed);
            let source = circuit.add(Device::isource(
                format!("Ibeh_{reference}_load_{}", load.name),
                supply,
                return_node,
                SourceKind::Dc(initial_i),
            ));
            self.profiled_loads.push(ProfiledLoadLeg {
                name: load.name.clone(),
                source,
                profile,
                start_s: load.start_s,
                seed: load.seed,
                last_i_a: initial_i,
                peak_i_a: initial_i,
                unknown_state_max_a: load.unknown_state_max_a,
            });
        }
    }

    /// The averaged converter as an in-circuit current-mode regulation loop,
    /// so Newton, not a per-chunk relaxation, closes it:
    ///
    /// * `i_cmd = clamp(x + g*(ref - V(sense)), -cap, cap)`, its corners
    ///   rounded over `cap/50` (exact well inside): a kink within one
    ///   finite-difference probe hands Newton a straddled partial it cannot
    ///   converge on, as a load step through the limit does. A synchronous
    ///   switch sinks as it sources, so a light load's operating point sits
    ///   inside the linear range, not on a no-sink kink. `cap =
    ///   min(iout_limit, lim*eff*V(in)/V(out))` under an
    ///   input-current limit, else `iout_limit` gated by
    ///   `clamp(lim/min(full_lim, 0.01), 0, 1)` (so is a buck's `max_duty`): a
    ///   1e9 "no limit" or a step gate would kick Newton on arming, or on a
    ///   damped step landing on the switch, while a ramp full from `lim = 0.01`
    ///   survives DC source stepping, and a milliamp limit still arms fully at
    ///   its full value;
    /// * the delivered current `i`, `i_cmd` held where the averaged switch
    ///   node `V(out) + out_r*i` stays within `[0, max_duty*V(in)]` for a buck
    ///   (`[0, V(in)]` while disarmed: the high-side body diode), so the
    ///   output inductor slews at `(V(in) - V(out))/L` instead of stepping.
    ///   Armed, the ceiling is a smooth minimum over 0.4% of `V(in)`, exact
    ///   outside it (a sagging input's probes straddle a hard one), which only
    ///   ever lowers `i`: the integrator absorbs that while regulating, and no
    ///   limit is exceeded. Disarmed it stays hard: in early source steps the
    ///   smoothing would swamp milliamp currents and stall Newton.
    ///   `i` is injected as a current through `out_r` into the output pin: as
    ///   a millivolt drop between volt-scale nodes, Newton's voltage tolerance
    ///   could not resolve it;
    /// * the PI state `x' = wz*(i - x) - wl*x`: integral action `wz*g*e`
    ///   while regulating (`i = x + g*e`), tracking `i` against any clamp
    ///   instead of winding up, which also keeps the DC solve off a
    ///   limit cycle between the regulating and the clamped branch.
    ///
    /// `g = 2 pi f_c C_out / k` crosses the loop over at `f_c` against the
    /// output capacitance `C_out` on the regulated node, `k` being the
    /// feedback divider's ratio; `wz = wc/4` damps it critically, so a step
    /// `dI` dips the output about `0.74 dI / (2 pi f_c C_out)`. `g*e` is a
    /// linear source's node, so the loop's finite-difference probe moves it
    /// by a part in 1e3 of itself, not by `g` times a part in 1e3 of
    /// `V(sense)`: amps, against a large `C_out`, that straddle the cap. A buck's
    /// regulated node is the far side of its one output inductor. `lim` is a
    /// hidden source holding the input-current limit (1 if none), -1 while
    /// disarmed, so no finite-difference probe of it straddles the switch.
    ///
    /// The input pin draws `V(sw)*i/(eff*V(in))` forward and returns
    /// `eff*V(sw)*|i|/V(in)` in reverse (one smooth `hypot` form, losses always
    /// positive), inside the solve: a reverse current, such as an output
    /// capacitor discharging back through a synchronous switch, lands on the
    /// input instead of vanishing, and the draw has no one-chunk lag. `i` is
    /// solved onto a hidden node like `g*e`, which the output, the PI state and
    /// the draw read, so each probe evaluates the clamp once, not three times.
    ///
    /// Below the input's lockout ([`ConverterBoard::input_floor_v`]) `i` is
    /// scaled to zero, sourcing and sinking alike: the stage is off, so a
    /// dead input neither feeds the output nor clamps it to the input's 0 V
    /// through `out_r` (the disarmed ceiling alone would). The per-chunk
    /// update reads the solved input against the floor, as it reads the
    /// enable, and sets a hidden source the loop scales `i` by: no Newton
    /// iterate crosses the floor, so a solve sees the stage on or off
    /// throughout, and the DC settle takes one more solve for each stage
    /// whose input another converter makes. The output lands on
    /// [`ConverterBoard::out_node`] when the binder resolved one.
    ///
    /// Skipped when a programmable limit has no board evidence to read, or
    /// when either power pin is unconnected.
    fn stamp_converter(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        role_nodes: &BTreeMap<String, NodeId>,
        board_resistor: &dyn Fn(&str) -> Option<f64>,
        board: ConverterBoard,
    ) {
        let Some(c) = &model.converter else { return };
        let iin_limit_a = resolve_iin_limit(c, board_resistor);
        let (Some(&pin_out), Some(&vin)) = (role_nodes.get(&c.out_pin), role_nodes.get(&c.in_pin))
        else {
            return;
        };
        let out = board.out_node.unwrap_or(pin_out);
        if c.iin_program.is_some() && iin_limit_a.is_none() {
            return;
        }
        let r = c.out_r_ohms.unwrap_or(STIFF_R_OHMS).max(STIFF_R_OHMS);
        let eff = c.efficiency.unwrap_or(0.9).clamp(1e-9, 1.0);
        let reg = regulated_node(circuit, out);
        let fb = c
            .feedback
            .as_ref()
            .map(|f| (f.vref_v, role_nodes.get(&f.pin)));
        let (sense, vref, k) = match fb {
            Some((vref, Some(&n))) => {
                // Away from the output, a feedback pin with no resistor on it
                // has no divider to set the rail: the binder leaves the part
                // open, and no rail is invented here.
                let ratio = divider_ratio(circuit, n);
                let unity = n == out || n == reg || n == pin_out;
                let Some(k) = ratio.or_else(|| unity.then_some(1.0)) else {
                    return;
                };
                (n, vref, k)
            }
            _ => (reg, c.vout_setpoint, 1.0),
        };
        let farads = out_farads(circuit, reg).max(1e-6) / k;
        let full_lim = iin_limit_a.unwrap_or(1.0);
        let floor = board.input_floor_v.filter(|v| *v > 0.0);
        let [drive, pi, ge, draw] = loop_exprs(c, farads, r, eff, full_lim, floor.is_some());
        let [drv, src, pi_node, err, i_node] = ["out", "sw", "pi", "err", "i"]
            .map(|t| circuit.node(&format!("__beh_{reference}_conv_{t}")));
        let (lim, limit_vsource) = hidden_dc_node(circuit, reference, "conv_lim", -1.0);
        // The reference as a source, so DC source stepping ramps it with the
        // supplies: a soft start, which is how Newton finds a clamped loop.
        let ref_node = hidden_dc_node(circuit, reference, "conv_ref", vref).0;
        let mut deps = vec![out, pi_node, err, lim, vin, sense, ref_node, drv, i_node];
        // The input lockout's on/off level, stamped on: until an update reads
        // the input, the loop is the one a card without a floor gets.
        let lockout = floor.map(|floor_v| {
            let (node, source) = hidden_dc_node(circuit, reference, "conv_on", 1.0);
            deps.push(node);
            InputLockout {
                input: vin,
                floor_v,
                source,
            }
        });
        let mut add = |t: &str, p, n, expr: &str| {
            let name = format!("Bbeh_{reference}_conv_{t}");
            let dev = Device::behavioral(name, (p, n), BOutput::Current, expr, &deps);
            circuit.add(dev.expect("converter loop expression compiles"))
        };
        let g = NodeId::GROUND;
        add("i", g, i_node, &drive);
        add("pi", g, pi_node, &pi);
        add("err", g, err, &ge);
        add("sw", g, src, "__d8");
        let in_draw = add("in", vin, g, &draw);
        let cpi = format!("Cbeh_{reference}_conv_pi");
        circuit.add(Device::capacitor(cpi, pi_node, NodeId::GROUND, 1e-6));
        let out_vsource = circuit.add(Device::vsource(
            format!("Vbeh_{reference}_conv"),
            drv,
            src,
            SourceKind::Dc(0.0),
        ));
        circuit.add(Device::resistor(
            format!("Rbeh_{reference}_conv_out"),
            drv,
            out,
            r,
        ));
        self.converter = Some(ConverterLeg {
            cfg: c.clone(),
            out_vsource,
            out_node: out,
            enable_node: c
                .enable
                .as_ref()
                .and_then(|en| role_nodes.get(&en.pin).copied()),
            sensed: !matches!(fb, Some((_, None))),
            in_draw,
            limit_vsource,
            iin_limit_a,
            last_iin_a: 0.0,
            lockout,
        });
    }

    /// Expression laws, preferring the solver-implicit form where the law is a
    /// pure function of pin voltages and params.
    fn stamp_laws(
        &mut self,
        circuit: &mut Circuit,
        reference: &str,
        model: &Behavioral,
        params: &Params,
        role_nodes: &BTreeMap<String, NodeId>,
    ) {
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
            // Prefer the solver-implicit form for a law that is a pure
            // function of pin voltages / params: Newton then owns the
            // law (its conductance is in the Jacobian), where the chunk-updated
            // constant-source form is an explicit relaxation that diverges on
            // high-impedance nets (see [`LawStamp`]) and holds 0 until the
            // first chunk, so an operating point solved before it (the rest
            // state, a regulator's input support) never sees the law. A voltage
            // law forces an internal node through its series resistance, as
            // the runtime form does. State-gated laws and FSM-context
            // expressions cannot be expressed implicitly and keep the runtime
            // form.
            if law.only_in_state.is_none() {
                if let Some((expr, deps)) = compile_law_implicit(&law.expr, role_nodes, params) {
                    let (p, output) = match law.kind {
                        LawKind::Current => (a_node, BOutput::Current),
                        LawKind::Voltage => {
                            let drv = circuit.node(&format!("__beh_{reference}_{}_law", law.name));
                            circuit.add(Device::resistor(
                                format!("Rbeh_{reference}_{}_law", law.name),
                                drv,
                                a_node,
                                law.r_ohms.unwrap_or(STIFF_R_OHMS).max(STIFF_R_OHMS),
                            ));
                            (drv, BOutput::Voltage)
                        }
                    };
                    circuit.add(Device::Behavioral {
                        name: format!("Bbeh_{reference}_{}", law.name),
                        p,
                        n: b_node,
                        output,
                        expr,
                        deps,
                    });
                    self.laws.push(LawLeg {
                        law: law.clone(),
                        program,
                        stamp: LawStamp::Implicit,
                    });
                    continue;
                }
            }
            let (source, series_r): (DeviceId, Option<(DeviceId, f64)>) = match law.kind {
                LawKind::Current => (
                    circuit.add(Device::isource(
                        format!("Ibeh_{reference}_{}", law.name),
                        a_node,
                        b_node,
                        SourceKind::Dc(0.0),
                    )),
                    None,
                ),
                LawKind::Voltage => {
                    let drv = circuit.node(&format!("__beh_{reference}_{}_law", law.name));
                    let vs = circuit.add(Device::vsource(
                        format!("Vbeh_{reference}_{}", law.name),
                        drv,
                        b_node,
                        SourceKind::Dc(0.0),
                    ));
                    let on_ohms = law.r_ohms.unwrap_or(STIFF_R_OHMS).max(STIFF_R_OHMS);
                    let r = circuit.add(Device::resistor(
                        format!("Rbeh_{reference}_{}_law", law.name),
                        drv,
                        a_node,
                        on_ohms,
                    ));
                    (vs, Some((r, on_ohms)))
                }
            };
            self.laws.push(LawLeg {
                law: law.clone(),
                program,
                stamp: LawStamp::Runtime { source, series_r },
            });
        }
    }

    /// True when nothing was stamped (no legs / FSM / custom): inert.
    fn is_inert(&self) -> bool {
        self.custom.is_none()
            && self.pulls == 0
            && self.open_drains.is_empty()
            && self.drives.is_empty()
            && self.series_paths.is_empty()
            && self.profiled_loads.is_empty()
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
            .map_or("", String::as_str)
    }

    /// The node the converter's output source drives, or None (no converter).
    #[must_use]
    pub fn converter_out_node(&self) -> Option<NodeId> {
        self.converter.as_ref().map(|c| c.out_node)
    }

    /// Every board node one of this device's pins is wired to.
    pub fn role_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.role_nodes.values().copied()
    }

    /// The converter's input node: where it takes the power it delivers.
    #[must_use]
    pub fn converter_in_node(&self) -> Option<NodeId> {
        let c = self.converter.as_ref()?;
        self.role_nodes.get(&c.cfg.in_pin).copied()
    }

    /// Every board node this device holds stiffly: its converter's output and
    /// each voltage law's pin behind a series-conductor resistance. Its other
    /// legs (pulls, open drains, 50 Ω state drives) are weak beside a rail.
    /// The rail-ownership pass reads this, so no ideal rail lands beside them.
    #[must_use]
    pub fn driven_nodes(&self) -> Vec<NodeId> {
        let stiff = |l: &&LawLeg| {
            let r = l.law.r_ohms.unwrap_or(STIFF_R_OHMS);
            matches!(l.law.kind, LawKind::Voltage) && r <= crate::rail_owner::SERIES_OHMS
        };
        let laws = self.laws.iter().filter(stiff);
        let pins = laws.filter_map(|l| self.role_nodes.get(&l.law.a).copied());
        self.converter_out_node().into_iter().chain(pins).collect()
    }

    /// The converter's output source and how its dissipation is estimated.
    #[must_use]
    pub fn converter_heat(&self) -> Option<(DeviceId, crate::stress::RegulatorHeat)> {
        let c = self.converter.as_ref()?;
        let heat = crate::stress::RegulatorHeat::Converter { draw: c.in_draw };
        Some((c.out_vsource, heat))
    }

    /// The effective input-current limit of the converter (A), or None.
    #[must_use]
    pub fn converter_iin_limit(&self) -> Option<f64> {
        self.converter.as_ref().and_then(|c| c.iin_limit_a)
    }

    /// Input current the converter drew at the last update (A), or None.
    #[must_use]
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
                let ctx = self.build_context(node_v, t, circuit.temp_c);
                eval_number(&leg.program, &ctx)
            }
            LawStamp::Runtime { source, .. } => match circuit.devices.get(source.0 as usize) {
                Some(
                    Device::Isource {
                        kind: SourceKind::Dc(v),
                        ..
                    }
                    | Device::Vsource {
                        kind: SourceKind::Dc(v),
                        ..
                    },
                ) => Some(*v),
                _ => None,
            },
        }
    }

    /// Drain faults raised since the last call.
    pub fn drain_faults(&mut self) -> Vec<FaultEvent> {
        std::mem::take(&mut self.pending_faults)
    }

    /// Advance one chunk. `node_v(n)` returns the node's solved voltage from the
    /// previous chunk. Recompute every controllable leg's value and write it
    /// onto the circuit.
    pub fn update(
        &mut self,
        circuit: &mut Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        t: f64,
        dt: f64,
    ) {
        // Escape hatch: a custom-behaviour device owns its whole update.
        if let Some(custom) = &mut self.custom {
            custom.update(circuit, node_v, t, dt, &mut self.pending_faults);
            return;
        }

        // 0. Advance model-owned current profiles to the end of this chunk,
        //    matching the zero-order-hold convention used by scenario loads.
        self.update_profiled_loads(circuit, t + dt);

        // 1. Build the evaluation context from current pin voltages + params +
        //    state booleans.
        let ctx = self.build_context(node_v, t, circuit.temp_c);

        // 2. Advance the FSM (guards over the same context).
        self.advance_fsm(&ctx, dt);

        // 3. Apply per-state pin overrides (open-drain asserts, drives).
        self.apply_state_pins(circuit);
        self.apply_series_paths(circuit);

        // 4. Converter regulation + limits.
        self.update_converter(circuit, node_v);

        // 5. Expression laws. Evaluate against a context rebuilt AFTER the FSM
        //    advanced: `advance_fsm` may have changed the active state and reset
        //    `t_in_state`, and a law's expr can read `state_<name>` / `t_in_state`
        //    / its own gating. Using the pre-advance `ctx` here lagged the law one
        //    chunk behind the state that gates it.
        let ctx = self.build_context(node_v, t, circuit.temp_c);
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
    /// each chunk raises an overpower fault if the converter's input draw
    /// exceeds the budget.
    pub const fn set_input_budget(&mut self, vin: f64, budget_w: f64) {
        self.input_budget = Some((vin, budget_w));
    }

    /// Build an evalexpr context: `v_<role>` for each connected pin's voltage,
    /// every param key verbatim, `t`, solver `temperature_c`, `t_in_state`, and
    /// `state_<name>` booleans.
    fn build_context(
        &self,
        node_v: &dyn Fn(NodeId) -> f64,
        t: f64,
        temperature_c: f64,
    ) -> HashMapContext {
        let mut ctx = HashMapContext::new();
        for (role, &node) in &self.role_nodes {
            let _ = ctx.set_value(format!("v_{role}"), Value::Float(node_v(node)));
        }
        for path in &self.series_paths {
            let current = (node_v(path.a) - node_v(path.b)) / path.current_ohms;
            let _ = ctx.set_value(format!("i_{}", path.name), Value::Float(current));
        }
        for load in &self.profiled_loads {
            let _ = ctx.set_value(format!("i_load_{}", load.name), Value::Float(load.last_i_a));
        }
        for k in self.params.0.keys() {
            if let Some(f) = self.params.get_f64(k) {
                let _ = ctx.set_value(k.clone(), Value::Float(f));
            }
        }
        let _ = ctx.set_value("t".into(), Value::Float(t));
        let _ = ctx.set_value("temperature_c".into(), Value::Float(temperature_c));
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
        for index in 0..self.fsm_transitions.len() {
            let tr = self.fsm_transitions[index].tr.clone();
            if tr.from != cur {
                self.transition_guard_true_s[index] = 0.0;
                continue;
            }
            // Guard compiled once at stamp time; a guard that failed to parse is
            // `None` and never fires.
            let fired = self.fsm_transitions[index]
                .guard
                .as_ref()
                .is_some_and(|p| guard_true(p, ctx));
            if fired {
                self.transition_guard_true_s[index] += dt;
            } else {
                self.transition_guard_true_s[index] = 0.0;
                continue;
            }
            if tr.min_dwell_s.is_some_and(|min| self.t_in_state < min) {
                continue;
            }
            if tr
                .guard_dwell_s
                .is_some_and(|min| self.transition_guard_true_s[index] < min)
            {
                continue;
            }
            if fired {
                if let Some(idx) = self.fsm_states.iter().position(|s| s == &tr.to) {
                    self.state_idx = idx;
                    self.t_in_state = 0.0;
                    self.transition_guard_true_s.fill(0.0);
                }
                break;
            }
        }
    }

    /// Retune open-drain and drive legs from the active state's pin overrides.
    fn apply_state_pins(&self, circuit: &mut Circuit) {
        let active = self.state().to_string();
        let overrides = self.state_pins.get(&active).cloned().unwrap_or_default();

        // Open drains: assert (on_ohms) when the active state lists od_assert.
        for od in &self.open_drains {
            let assert = overrides.get(&od.pin_role).is_some_and(|b| b.od_assert);
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

    /// Retune each series path to the active state's declared resistance.
    fn apply_series_paths(&mut self, circuit: &mut Circuit) {
        let active = self.state().to_string();
        for path in &mut self.series_paths {
            let ohms = path
                .state_ohms
                .get(&active)
                .copied()
                .unwrap_or(path.default_ohms);
            path.current_ohms = ohms;
            set_resistor_ohms(circuit, path.resistor, ohms);
        }
    }

    /// Hold every load that declares `unknown_state_max_a` at that level
    /// (`worst`) or back at its last profile current, for a DC pass that judges
    /// the state the board cannot show. False when no load declares one.
    pub fn set_unknown_state_loads(&mut self, circuit: &mut Circuit, worst: bool) -> bool {
        let mut any = false;
        for load in &self.profiled_loads {
            if let Some(max) = load.unknown_state_max_a {
                set_source_dc(
                    circuit,
                    load.source,
                    if worst { max } else { load.last_i_a },
                );
                any = true;
            }
        }
        any
    }

    /// Update each model-owned current sink for the next solve.
    fn update_profiled_loads(&mut self, circuit: &mut Circuit, t_end: f64) {
        for load in &mut self.profiled_loads {
            let current = load.profile.current_at(t_end - load.start_s, load.seed);
            load.last_i_a = current;
            load.peak_i_a = load.peak_i_a.max(current);
            set_source_dc(circuit, load.source, current);
        }
    }

    /// The converter's per-chunk bookkeeping (the loop itself is in-circuit,
    /// see `Self::stamp_converter`): arm it with its input-current limit, or
    /// disarm it while the enable input is low, a needed feedback pin or
    /// programming evidence is missing; turn the stage off while its input is
    /// below the lockout floor; and record the input current the in-circuit
    /// draw took (none once disarmed: the gated draw is zero from the next
    /// solve on). Also the DC settle's iteration; returns how far the arming
    /// or the lockout moved.
    pub fn update_converter(
        &mut self,
        circuit: &mut Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
    ) -> f64 {
        let Some(c) = self.converter.as_mut() else {
            return 0.0;
        };
        let high = |en: &EnableControl| {
            c.enable_node
                .is_some_and(|n| node_v(n) >= en.high_threshold_v)
        };
        // A source's level at the last solve, the value it is compared to.
        let solved = |id: DeviceId, otherwise: f64| match circuit.devices.get(id.0 as usize) {
            Some(Device::Vsource { p, .. }) => node_v(*p),
            _ => otherwise,
        };
        let powered = (c.lockout.as_ref()).is_none_or(|l| node_v(l.input) >= l.floor_v);
        let evidenced = c.cfg.iin_program.is_none() || c.iin_limit_a.is_some();
        let on = c.sensed && evidenced && powered && c.cfg.enable.as_ref().is_none_or(high);
        let limit = if on { c.iin_limit_a } else { Some(-1.0) }.unwrap_or(1.0);
        let level = if powered { 1.0 } else { 0.0 };
        let moved = (c.lockout).map_or(0.0, |l| (level - solved(l.source, level)).abs());
        let moved = moved.max((limit - solved(c.limit_vsource, limit)).abs());
        let iin = behavioral_draw(circuit, c.in_draw, node_v).1;
        c.last_iin_a = if on { iin } else { 0.0 };
        set_source_dc(circuit, c.limit_vsource, limit);
        if let Some(l) = c.lockout {
            set_source_dc(circuit, l.source, level);
        }
        moved
    }

    /// Re-evaluate the converter's input-current limit from the board (called if
    /// a programming resistor changes). Missing required evidence disables the
    /// converter sources on their next update. Returns the new limit. The
    /// in-circuit arming gate keeps its bind-time scale (see `loop_exprs`).
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
    let prog = sp
        .prog_ref
        .as_deref()
        .map_or(sp.prog_ohms, board_resistor)?;
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
    if let Some(Device::Vsource { kind, .. } | Device::Isource { kind, .. }) =
        circuit.devices.get_mut(id.0 as usize)
    {
        *kind = SourceKind::Dc(v);
    }
}

/// The `p` node of a node-voltage-driven behavioural current source and the
/// current it draws from it at voltages `v` (ground and 0 when `id` is not one).
pub(crate) fn behavioral_draw(
    c: &Circuit,
    id: DeviceId,
    v: &dyn Fn(NodeId) -> f64,
) -> (NodeId, f64) {
    let Some(Device::Behavioral { p, expr, deps, .. }) = c.devices.get(id.0 as usize) else {
        return (NodeId::GROUND, 0.0);
    };
    let volt = |d: &BDep| match *d {
        BDep::Volt(n) => v(n),
        BDep::Branch(_) => 0.0,
    };
    let volts: Vec<f64> = deps.iter().map(volt).collect();
    (*p, expr.eval(&volts, 0.0).unwrap_or(0.0))
}

fn set_resistor_ohms(circuit: &mut Circuit, id: DeviceId, ohms: f64) {
    if let Some(Device::Resistor { ohms: r, .. }) = circuit.devices.get_mut(id.0 as usize) {
        *r = ohms;
    }
}

/// Evaluate a parsed expression to a number against a context.
// Script integers are read as numbers; rounding beyond 2^53 is the intended view.
#[allow(clippy::cast_precision_loss)]
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
    use hauksbee_models::behavioral::{Fsm, ProfiledLoad, SeriesPath, Transition};
    use hauksbee_models::Segment;

    fn roles() -> BTreeMap<String, NodeId> {
        BTreeMap::from([
            ("io".to_string(), NodeId(1)),
            ("vbus".to_string(), NodeId(2)),
            ("gnd".to_string(), NodeId::GROUND),
        ])
    }

    fn params() -> Params {
        let mut p = Params::default();
        p.set_f64("vt_clamp", 1.1);
        p.set_f64("rd_clamp", 0.5);
        p
    }

    #[test]
    fn series_path_guard_dwell_measures_continuous_overcurrent_and_opens() {
        let mut circuit = Circuit::new();
        let a = circuit.node("a");
        let b = NodeId::GROUND;
        let role_nodes: BTreeMap<String, NodeId> =
            [("a".into(), a), ("b".into(), b)].into_iter().collect();
        let model = Behavioral {
            fsm: Some(Fsm {
                states: vec!["closed".into(), "tripped".into()],
                initial: Some("closed".into()),
                transitions: vec![Transition {
                    from: "closed".into(),
                    to: "tripped".into(),
                    guard: "i_fuse * i_fuse >= 64.0".into(),
                    min_dwell_s: None,
                    guard_dwell_s: Some(1.5),
                }],
                state_pins: BTreeMap::new(),
            }),
            series_paths: vec![SeriesPath {
                name: "fuse".into(),
                a: "a".into(),
                b: "b".into(),
                default_ohms: 0.04,
                state_ohms: [("closed".into(), 0.04), ("tripped".into(), 1e9)]
                    .into_iter()
                    .collect(),
            }],
            ..Default::default()
        };
        let mut dev = BehavioralDevice::stamp(
            &mut circuit,
            "F1",
            &model,
            &Params::default(),
            &role_nodes,
            &|_| None,
        )
        .expect("series path stamps");
        let high = |node: NodeId| if node == a { 0.36 } else { 0.0 }; // 9 A at 40 mΩ
        let low = |node: NodeId| if node == a { 0.04 } else { 0.0 }; // 1 A

        dev.update(&mut circuit, &high, 1.0, 1.0);
        assert_eq!(dev.state(), "closed");
        // A safe interval resets continuous guard time. Total device age is now
        // long, but the next short pulse must not trip immediately.
        dev.update(&mut circuit, &low, 11.0, 10.0);
        dev.update(&mut circuit, &high, 11.5, 0.5);
        dev.update(&mut circuit, &high, 12.4, 0.9);
        assert_eq!(dev.state(), "closed");
        dev.update(&mut circuit, &high, 12.51, 0.11);
        assert_eq!(dev.state(), "tripped");

        let resistance = circuit
            .devices
            .iter()
            .find_map(|device| match device {
                Device::Resistor { name, ohms, .. } if name == "Rbeh_F1_fuse" => Some(*ohms),
                _ => None,
            })
            .expect("stamped series resistor");
        assert!((resistance - 1e9).abs() < 1e-3, "{resistance}");
    }

    #[test]
    fn behavioral_guards_receive_the_configured_solver_temperature() {
        let mut circuit = Circuit::new();
        circuit.temp_c = 25.0;
        let model = Behavioral {
            fsm: Some(Fsm {
                states: vec!["normal".into(), "hot".into()],
                initial: Some("normal".into()),
                transitions: vec![Transition {
                    from: "normal".into(),
                    to: "hot".into(),
                    guard: "temperature_c >= 80.0".into(),
                    min_dwell_s: None,
                    guard_dwell_s: None,
                }],
                state_pins: BTreeMap::new(),
            }),
            ..Default::default()
        };
        let mut dev = BehavioralDevice::stamp(
            &mut circuit,
            "U1",
            &model,
            &Params::default(),
            &BTreeMap::new(),
            &|_| None,
        )
        .expect("temperature-only FSM stamps");
        let zero = |_node: NodeId| 0.0;

        dev.update(&mut circuit, &zero, 0.1, 0.1);
        assert_eq!(dev.state(), "normal");
        circuit.temp_c = 85.0;
        dev.update(&mut circuit, &zero, 0.2, 0.1);
        assert_eq!(dev.state(), "hot");
    }

    #[test]
    fn model_owned_profile_draws_current_and_can_drive_an_fsm_guard() {
        let mut circuit = Circuit::new();
        let vdd = circuit.node("vdd");
        let roles: BTreeMap<String, NodeId> = [("vdd".into(), vdd), ("gnd".into(), NodeId::GROUND)]
            .into_iter()
            .collect();
        let model = Behavioral {
            fsm: Some(Fsm {
                states: vec!["idle".into(), "busy".into()],
                initial: Some("idle".into()),
                transitions: vec![Transition {
                    from: "idle".into(),
                    to: "busy".into(),
                    guard: "i_load_core >= 0.2".into(),
                    min_dwell_s: None,
                    guard_dwell_s: None,
                }],
                state_pins: BTreeMap::new(),
            }),
            profiled_loads: vec![ProfiledLoad {
                name: "core".into(),
                supply_pin: "vdd".into(),
                return_pin: Some("gnd".into()),
                start_s: 0.0,
                seed: 0,
                unknown_state_max_a: Some(0.3),
                segments: vec![
                    Segment {
                        level_a: 0.010,
                        rise_s: 0.0,
                        duration_s: 0.001,
                        period_s: 0.0,
                        idle_a: None,
                        jitter_s: 0.0,
                    },
                    Segment {
                        level_a: 0.250,
                        rise_s: 0.0,
                        duration_s: 0.0,
                        period_s: 0.0,
                        idle_a: None,
                        jitter_s: 0.0,
                    },
                ],
            }],
            ..Default::default()
        };
        let mut dev = BehavioralDevice::stamp(
            &mut circuit,
            "U1",
            &model,
            &Params::default(),
            &roles,
            &|_| None,
        )
        .expect("profile stamps");
        let zero = |_node: NodeId| 0.0;

        dev.update(&mut circuit, &zero, 0.0, 0.0005);
        assert_eq!(dev.state(), "idle");
        dev.update(&mut circuit, &zero, 0.0005, 0.0006);
        assert_eq!(dev.state(), "busy");
        let current = |circuit: &Circuit| {
            circuit
                .devices
                .iter()
                .find_map(|device| match device {
                    Device::Isource { name, kind, p, n } if name == "Ibeh_U1_load_core" => {
                        assert_eq!(*p, vdd);
                        assert_eq!(*n, NodeId::GROUND);
                        Some(kind.eval(0.0))
                    }
                    _ => None,
                })
                .expect("profile current source exists")
        };
        assert!((current(&circuit) - 0.250).abs() < 1e-12);
        // The unknown-state corner swaps in the card's maximum and back.
        assert!(dev.set_unknown_state_loads(&mut circuit, true));
        assert!((current(&circuit) - 0.3).abs() < 1e-12);
        dev.set_unknown_state_loads(&mut circuit, false);
        assert!((current(&circuit) - 0.250).abs() < 1e-12);
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

    /// A voltage law that is a pure function of pin voltages is solved
    /// implicitly, like a current law: a behavioral voltage device forces an
    /// internal node behind the law's series resistor, so the operating point
    /// sees the law's level, not the runtime source's 0 V before its first
    /// chunk.
    #[test]
    fn pure_voltage_law_stamps_implicitly_behind_its_series_resistor() {
        let mut circuit = Circuit::new();
        let gate = circuit.node("gate");
        let sense = circuit.node("sense");
        let roles: BTreeMap<String, NodeId> = [("bgate".into(), gate), ("csn".into(), sense)]
            .into_iter()
            .collect();
        let model = Behavioral {
            laws: vec![hauksbee_models::behavioral::Law {
                name: "clamp".into(),
                kind: LawKind::Voltage,
                a: "bgate".into(),
                b: None,
                expr: "max(0.0, v_csn - 9.5)".into(),
                r_ohms: Some(1.0e4),
                only_in_state: None,
            }],
            ..Default::default()
        };
        let dev = BehavioralDevice::stamp(
            &mut circuit,
            "U1",
            &model,
            &Params::default(),
            &roles,
            &|_| None,
        )
        .expect("law stamps");
        assert!(matches!(dev.laws[0].stamp, LawStamp::Implicit));
        let forced = (circuit.devices.iter())
            .find_map(|d| match d {
                Device::Behavioral {
                    name,
                    p,
                    n,
                    output: BOutput::Voltage,
                    deps,
                    ..
                } if name == "Bbeh_U1_clamp" => Some((*p, *n, deps.clone())),
                _ => None,
            })
            .expect("a behavioral voltage device");
        assert_eq!(forced.1, NodeId::GROUND);
        assert!(matches!(forced.2[..], [BDep::Volt(n)] if n == sense));
        let series = (circuit.devices.iter()).any(|d| {
            matches!(d, Device::Resistor { name, a, b, ohms, .. }
                if name == "Rbeh_U1_clamp_law" && *a == forced.0 && *b == gate
                    && (*ohms - 1.0e4).abs() < 1e-9)
        });
        assert!(
            series,
            "the series resistor joins the forced node to the pin"
        );
        assert!(
            !(circuit.devices.iter()).any(|d| matches!(d, Device::Vsource { .. })),
            "no chunk-updated source"
        );
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

    /// A divider's legs are followed through series links: 4.7k + 1k through
    /// a mid-node (with a capacitor on it) over 1k is 1/6.7 of the rail, as
    /// one 5.7k is; a mid-node a third resistor loads ends the leg there.
    #[test]
    fn divider_ratio_follows_series_chains() {
        let build = |split: bool, loaded: bool| {
            let mut c = Circuit::new();
            let (out, mid, fb) = (c.node("out"), c.node("mid"), c.node("fb"));
            if split {
                c.add(Device::resistor("R1", out, mid, 4700.0));
                c.add(Device::capacitor("C1", out, mid, 22e-12));
                c.add(Device::resistor("R2", mid, fb, 1000.0));
            } else {
                c.add(Device::resistor("R1", out, fb, 5700.0));
            }
            if loaded {
                c.add(Device::resistor("R4", mid, NodeId::GROUND, 10_000.0));
            }
            c.add(Device::resistor("R3", fb, NodeId::GROUND, 1000.0));
            (c, fb)
        };
        let ratio = |split, loaded| {
            let (c, fb) = build(split, loaded);
            divider_ratio(&c, fb).expect("a resistor touches fb")
        };
        assert!((ratio(true, false) - 1.0 / 6.7).abs() < 1e-12);
        assert!((ratio(false, false) - 1.0 / 6.7).abs() < 1e-12);
        // R2 alone ends on the loaded mid-node: half the conductance is "top".
        assert!((ratio(true, true) - 0.5).abs() < 1e-12);
    }
}
