//! Declarative behavioural model layer.
//!
//! Power ICs (chargers, PMICs, balancers) are not captured by the SPICE-level
//! R/C/L/diode/BJT/MOSFET classes: their behaviour is internal logic
//! (regulation loops, ship-mode pulls, balancing FETs, state machines) that the
//! datasheet describes functionally, not as a transistor netlist. This module
//! is the TOML schema for that functional description. It lives alongside the
//! ordinary [`ModelEntry`](crate::schema::ModelEntry) DB: a model entry may
//! carry an optional `[models.behavioral]` block whose contents are parsed here.
//!
//! A behavioural model is a bag of declarative facts, all optional, that the
//! engine's behavioural runtime stamps and iterates between solver chunks (the
//! same cadence the configurable power supplies already use):
//!
//! - **pins**: named pins with electrical semantics, an internal pull to a
//!   named rail through a resistance (the nPM1300 SHPHLD case), an open-drain
//!   output, an enable input with a threshold and polarity.
//! - **states / transitions**: a finite state machine whose transitions are
//!   guarded by pin-voltage / pin-current / time conditions, with per-state pin
//!   behaviour overrides.
//! - **converter**: an averaged buck / boost / buck-boost block, an output
//!   regulation setpoint, input/output current limits with foldback, an
//!   efficiency, and a sense-resistor pin that programs a limit the way real
//!   parts do (the LTC4020 ILIMIT/RSENSE case).
//! - **laws**: a current or voltage defined as an `evalexpr` expression over pin
//!   voltages, the active state, and the model params (the LTC6803 balancer leak
//!   case). Sandboxed: arithmetic and conditionals only, no I/O.
//!
//! Nothing here commits to a solve method; the engine decides how to realise
//! each fact as Thevenin legs / sense resistors / source updates.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::profile::Segment;

/// The optional behavioural block of a model entry.
///
/// Every field is optional so a model can describe just a pull (nPM1300), just
/// a converter (a plain buck regulator), or the full stack (the LTC4020).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Behavioral {
    /// Named pins with electrical semantics. Key is the pin role used in the
    /// component's `[models.pins]` map (e.g. "shphld", "ilimit", "csp").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: BTreeMap<String, BehavioralPin>,

    /// Finite-state machine. Absent for purely combinational parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fsm: Option<Fsm>,

    /// Averaged switching-converter block (buck/boost/buck-boost).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converter: Option<Converter>,

    /// Expression-defined laws: extra currents/voltages over pins+state+params.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub laws: Vec<Law>,

    /// Series conduction paths whose resistance may change with FSM state.
    ///
    /// This is the reusable seam for resettable fuses, relays, protection
    /// switches, and other parts whose externally visible behavior is a
    /// state-dependent resistance between two named pins.  The runtime also
    /// exposes each path current as `i_<name>` to FSM guards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series_paths: Vec<SeriesPath>,

    /// Time-varying current sinks owned by this fitted component.
    ///
    /// This is the model-card counterpart to a scenario-level dynamic load:
    /// datasheet/source-bound boot, active, standby and periodic burst current
    /// can travel with the part rather than being repeated in every CI spec.
    /// The runtime stamps an `Isource` from `supply_pin` to `return_pin` (or
    /// ground) and updates it once per solver chunk from the shared
    /// [`Segment`] waveform implementation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiled_loads: Vec<ProfiledLoad>,
}

impl Behavioral {
    /// True when the block carries nothing the runtime would stamp.
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
            && self.fsm.is_none()
            && self.converter.is_none()
            && self.laws.is_empty()
            && self.series_paths.is_empty()
            && self.profiled_loads.is_empty()
    }
}

// ── Pins ──────────────────────────────────────────────────────────────────────

/// One named pin's electrical semantics.
///
/// A pin can be several things at once in principle, but in practice the kind
/// is what the runtime keys on: a `pull` pin is a resistor to a rail; an
/// `open_drain` pin is a controllable low-side switch; an `enable` pin is read,
/// not driven.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct BehavioralPin {
    /// Internal pull to a named rail (another pin role, or a literal voltage via
    /// `pull_to_volts`). The nPM1300 SHPHLD has `pull_to = "vsys"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_to: Option<String>,

    /// Internal pull to a fixed voltage when the rail is a literal, not a pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_to_volts: Option<f64>,

    /// Resistance of the internal pull (ohms). Required when `pull_to`/
    /// `pull_to_volts` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_ohms: Option<f64>,

    /// Open-drain output: pulls the pin toward `od_to_volts` (default 0 = GND)
    /// through `od_ohms` when the controlling state asserts it. The set of
    /// states that assert it lives on the FSM's per-state pin overrides.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_drain: bool,

    /// Open-drain sink target voltage (default 0.0 = ground).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub od_to_volts: Option<f64>,

    /// Open-drain on-resistance (ohms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub od_ohms: Option<f64>,

    /// Enable-input threshold (V). When set, this pin is read as a logic enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_threshold_v: Option<f64>,

    /// Enable polarity: `true` = active-high (asserted above threshold),
    /// `false` = active-low (asserted below threshold).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enable_active_high: bool,
}

fn default_true() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

// ── Finite-state machine ────────────────────────────────────────────────────

/// A finite-state machine over the part's operating modes.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Fsm {
    /// State names. The first is the power-up / reset state.
    pub states: Vec<String>,

    /// Initial state name; defaults to `states[0]` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<String>,

    /// Guarded transitions, evaluated in order each chunk; the first whose guard
    /// holds fires.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<Transition>,

    /// Per-state pin overrides: in state `S`, pin `P` behaves as `Behaviour`.
    /// Keyed `state -> pin role -> override`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state_pins: BTreeMap<String, BTreeMap<String, StatePinBehaviour>>,
}

/// A guarded FSM transition.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Transition {
    pub from: String,
    pub to: String,

    /// `evalexpr` boolean guard over `v_<pin>` (pin voltages), `i_<pin>` (pin
    /// currents, where measurable), `t` (sim time, s), `t_in_state` (time since
    /// entering `from`), the param values, and `state` (a string compare helper
    /// is not provided; use the `from` field for that). Non-zero / true fires.
    pub guard: String,

    /// Optional minimum dwell time in `from` before the transition may fire (s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_dwell_s: Option<f64>,

    /// Continuous time for which the guard itself must remain true before the
    /// transition fires. Unlike `min_dwell_s`, this timer resets whenever the
    /// guard becomes false. This distinction is essential for over-current and
    /// debounce behavior: five seconds spent safely in a state must not make a
    /// later one-chunk spike trip immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_dwell_s: Option<f64>,
}

/// A resistor-like path whose value is selected by the active FSM state.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct SeriesPath {
    /// Identifier used in expressions. The runtime publishes current from
    /// `a` to `b` as `i_<name>`.
    pub name: String,
    /// Named pin roles from `[models.pins]`.
    pub a: String,
    pub b: String,
    /// Resistance before any state-specific override (ohms).
    pub default_ohms: f64,
    /// Optional resistance by FSM state (ohms).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state_ohms: BTreeMap<String, f64>,
}

/// A source-bound current profile drawn by the fitted component.
///
/// The first segment is the pre-start/DC level. Subsequent segments use the
/// same piecewise/periodic semantics as [`crate::LoadProfile`]. `start_s`
/// shifts the activity timeline but deliberately does not invent a zero-current
/// pre-start state: authors must put the documented standby/off current in the
/// first segment.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ProfiledLoad {
    /// Expression-safe identifier, surfaced as `i_load_<name>` at runtime.
    pub name: String,
    /// Pin role from which positive load current is drawn.
    pub supply_pin: String,
    /// Optional return pin role. Absent means circuit ground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_pin: Option<String>,
    /// Activity timeline offset in seconds.
    #[serde(default)]
    pub start_s: f64,
    /// Deterministic seed for per-segment jitter.
    #[serde(default)]
    pub seed: u64,
    /// Ordered current waveform segments.
    #[serde(default, rename = "segment", skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<Segment>,
}

/// How a pin behaves while a given state is active. Overrides the pin's default
/// `[models.behavioral.pins]` semantics for the duration of the state.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct StatePinBehaviour {
    /// Drive the pin to this voltage through `drive_ohms` (a push-pull output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_volts: Option<f64>,

    /// Output resistance for `drive_volts` (ohms, default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_ohms: Option<f64>,

    /// Assert the pin's open-drain sink while in this state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub od_assert: bool,

    /// Tri-state the pin (present its high-impedance default) in this state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hi_z: bool,
}

// ── Averaged converter ──────────────────────────────────────────────────────

/// Switching topology of an averaged converter block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    Buck,
    Boost,
    BuckBoost,
}

impl Default for Topology {
    fn default() -> Self {
        Topology::Buck
    }
}

/// An averaged (cycle-averaged) switching-converter block. The runtime realises
/// it as a controllable source on the output pin behind a measurable series
/// resistor (the [`SupplyLeg`](../../hauksbee-engine) pattern), regulating to
/// `vout_setpoint` until an input- or output-current limit folds the output
/// back. Power is conserved through `efficiency`, so the input current the
/// runtime draws on the input pin is `Vout*Iout / (efficiency*Vin)`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Converter {
    pub topology: Topology,

    /// Output pin role (the regulated rail).
    pub out_pin: String,

    /// Input pin role (where the converter draws its current).
    pub in_pin: String,

    /// Regulated output voltage (V). May be overridden by a feedback divider
    /// programming pin in a future extension; for now it is the setpoint.
    pub vout_setpoint: f64,

    /// Optional board-programmed feedback loop.  When present, the converter
    /// drives its averaged output until the voltage on `pin` reaches `vref_v`.
    /// This lets an adjustable buck use the board's actual divider instead of
    /// baking one product's output voltage into a reusable IC model.  The
    /// runtime relaxation is a DC convergence aid, not a claim about loop
    /// bandwidth or compensation stability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<FeedbackControl>,

    /// Optional enable input.  Below `high_threshold_v` the averaged output and
    /// reflected input draw are both disabled.  Hysteresis and UVLO timing stay
    /// outside this first-order DC primitive unless an FSM models them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable: Option<EnableControl>,

    /// Output-current limit (A). Past this the output folds back (CC mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iout_limit_a: Option<f64>,

    /// Input-current limit (A). Past this the converter throttles so the input
    /// draw is held at the limit. This is the LTC4020 ILIMIT behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iin_limit_a: Option<f64>,

    /// Conversion efficiency (0..1). Default 0.9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efficiency: Option<f64>,

    /// Output impedance of the averaged source (ohms). Small but non-zero so the
    /// source branch current is well defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_r_ohms: Option<f64>,

    /// Programmable input-current limit: a sense resistor on `in_pin` (the
    /// shunt the converter measures across) plus a programming resistor on
    /// `prog_pin`. When present, the runtime computes `iin_limit_a` from the
    /// resistors per [`SenseProgram`] instead of using the literal field above,
    /// exactly the way the real part is programmed on the board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iin_program: Option<SenseProgram>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FeedbackControl {
    /// Pin role connected to the external feedback divider midpoint.
    pub pin: String,
    /// Datasheet reference voltage at the feedback pin.
    pub vref_v: f64,
    /// Per-chunk numerical relaxation gain in output-volts per feedback-volt.
    /// This is intentionally explicit because it controls convergence only;
    /// it must not be confused with physical loop gain or bandwidth.
    #[serde(default = "default_feedback_relaxation_gain")]
    pub relaxation_gain: f64,
}

fn default_feedback_relaxation_gain() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EnableControl {
    pub pin: String,
    pub high_threshold_v: f64,
}

/// A programmable current limit set by an external sense resistor and a
/// programming resistor, the way the LTC4020 ILIMIT pin works.
///
/// The part regulates the sense-resistor voltage to a threshold
/// `v_sense_max = vprog_ref * (prog_ohms / prog_ref_ohms)` (a resistor-ratio
/// programmed threshold, clamped to `v_sense_full`), giving an input current
/// limit `i = v_sense_max / rsense_ohms`. The threshold scales *linearly* with
/// the programming resistor, matching `program_iin_limit` in the engine (which
/// computes `vprog_ref * prog / prog_ref_ohms`); the prior doc had this ratio
/// inverted, describing the reciprocal law. The resistor values are read off the
/// board at bind time, so changing the board resistor changes the limit, with
/// no model edit, which is precisely how the Reform mb2.5->3.0 fix (R8 100k ->
/// 7.15k) lands. Named resistors are strict evidence: every named shunt must
/// resolve and matching shunts must agree, otherwise the runtime abstains.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct SenseProgram {
    /// The sense resistor on the input path (ohms). Use this only for a model
    /// whose shunt is a literal rather than a named board component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsense_ohms: Option<f64>,

    /// Board reference designators of the sense shunts, read at bind time.
    /// Multi-shunt Kelvin topologies list every shunt; all must resolve to the
    /// same positive value. This is mutually exclusive with `rsense_ohms`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rsense_refs: Vec<String>,

    /// The programming resistor (ohms). Mutually exclusive with `prog_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prog_ohms: Option<f64>,

    /// Board reference designator of the programming resistor (e.g. "R8").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prog_ref: Option<String>,

    /// Internal reference voltage the programming divider works against (V).
    pub vprog_ref: f64,

    /// The programming-divider numerator resistance (ohms): the on-die or fixed
    /// resistor the external `prog_ohms` divides against. The threshold scales
    /// as `prog_ohms / prog_ref_ohms`, linearly with the programming resistor,
    /// consistent with the struct-level doc and the engine's
    /// `program_iin_limit`.
    pub prog_ref_ohms: f64,

    /// Full-scale sense voltage (V): the maximum current-sense threshold,
    /// reached when the programming resistor pulls the threshold to its ceiling.
    pub v_sense_full: f64,
}

// ── Expression laws ─────────────────────────────────────────────────────────

/// What physical quantity a [`Law`] defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LawKind {
    /// A current injected from pin `a` to pin `b` (a controlled current source).
    #[default]
    Current,
    /// A voltage forced on a pin through a series resistance (a controlled
    /// source behind `r_ohms`).
    Voltage,
}

/// An expression-defined law: a current or voltage computed each chunk from an
/// `evalexpr` expression over the device's pin voltages, active state, and
/// params. Sandboxed: only arithmetic, comparison, and `if`/`min`/`max`/`abs`
/// builtins are available, no variables the runtime did not bind, no I/O.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Law {
    /// Diagnostic name (e.g. "balancer_leak").
    pub name: String,

    pub kind: LawKind,

    /// For a current law: source pin (current flows `a -> b`). For a voltage
    /// law: the pin whose voltage is forced.
    pub a: String,

    /// For a current law: sink pin. For a voltage law: the reference the series
    /// resistor returns to (default ground).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<String>,

    /// The `evalexpr` expression. Variables: `v_<pin>` for each named pin's
    /// node voltage, `t` (sim time s), the param keys verbatim, and
    /// `state_<name>` booleans (1.0 in that state, else 0.0). Must evaluate to a
    /// number (amps for a current law, volts for a voltage law).
    pub expr: String,

    /// Series resistance for a voltage law (ohms). Ignored for current laws.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r_ohms: Option<f64>,

    /// Only apply this law while the named state is active. Absent = always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_in_state: Option<String>,
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Validate a behavioural block for internal consistency. Returns a list of
/// human-readable problems (empty = valid). Cheap structural checks only; the
/// expression syntax is checked separately by the engine when it compiles the
/// laws against a real pin set.
pub fn validate_behavioral(b: &Behavioral) -> Vec<String> {
    let mut errs = Vec::new();

    for (role, pin) in &b.pins {
        let has_pull = pin.pull_to.is_some() || pin.pull_to_volts.is_some();
        if has_pull && pin.pull_ohms.is_none() {
            errs.push(format!(
                "pin '{role}': pull target set but pull_ohms missing"
            ));
        }
        if let Some(r) = pin.pull_ohms {
            // A non-finite value (NaN/±inf) is false for every `<= 0.0` compare, so
            // it must be rejected explicitly or a `nan` TOML literal slips the gate
            // and reaches the solver (the R37 validation.rs hole, here too).
            if !r.is_finite() || r <= 0.0 {
                errs.push(format!("pin '{role}': pull_ohms must be positive, got {r}"));
            }
        }
        // The pull/open-drain TARGET voltages are stamped verbatim as DC sources
        // (engine behavioral.rs Dc(pull_to_volts) / Dc(od_to_volts)); a `nan`/`inf`
        // literal poisons the whole MNA solve with no fault. A negative rail is
        // legal, so only finiteness is checked (the sibling of the pull_ohms gate).
        if let Some(v) = pin.pull_to_volts {
            if !v.is_finite() {
                errs.push(format!(
                    "pin '{role}': pull_to_volts must be finite, got {v}"
                ));
            }
        }
        // The pull/open-drain TARGET voltages are stamped verbatim as DC sources
        // (engine behavioral.rs Dc(pull_to_volts) / Dc(od_to_volts)); a `nan`/`inf`
        // literal poisons the whole MNA solve with no fault. A negative rail is
        // legal, so only finiteness is checked (the sibling of the pull_ohms gate).
        if pin.open_drain {
            if let Some(r) = pin.od_ohms {
                if !r.is_finite() || r <= 0.0 {
                    errs.push(format!("pin '{role}': od_ohms must be positive, got {r}"));
                }
            }
            if let Some(v) = pin.od_to_volts {
                if !v.is_finite() {
                    errs.push(format!("pin '{role}': od_to_volts must be finite, got {v}"));
                }
            }
        }
    }

    if let Some(fsm) = &b.fsm {
        if fsm.states.is_empty() {
            errs.push("fsm: no states declared".to_string());
        }
        let known: std::collections::HashSet<&str> =
            fsm.states.iter().map(String::as_str).collect();
        if let Some(init) = &fsm.initial {
            if !known.contains(init.as_str()) {
                errs.push(format!("fsm: initial state '{init}' is not in states"));
            }
        }
        for (i, tr) in fsm.transitions.iter().enumerate() {
            if !known.contains(tr.from.as_str()) {
                errs.push(format!(
                    "fsm transition {i}: unknown from-state '{}'",
                    tr.from
                ));
            }
            if !known.contains(tr.to.as_str()) {
                errs.push(format!("fsm transition {i}: unknown to-state '{}'", tr.to));
            }
            if tr.guard.trim().is_empty() {
                errs.push(format!("fsm transition {i}: empty guard"));
            }
            // The engine applies min_dwell_s as `if t_in_state < min { continue }`;
            // a NaN makes that comparison false, silently skipping the dwell gate
            // so the transition fires immediately instead of waiting the intended
            // debounce/soft-start delay. Reject non-finite (and negative) like
            // every other solver-facing float in this function.
            if let Some(d) = tr.min_dwell_s {
                if !d.is_finite() || d < 0.0 {
                    errs.push(format!(
                        "fsm transition {i}: min_dwell_s must be a non-negative finite number, got {d}"
                    ));
                }
            }
            if let Some(d) = tr.guard_dwell_s {
                if !d.is_finite() || d < 0.0 {
                    errs.push(format!(
                        "fsm transition {i}: guard_dwell_s must be a non-negative finite number, got {d}"
                    ));
                }
            }
        }
        for (st, pins) in &fsm.state_pins {
            if !known.contains(st.as_str()) {
                errs.push(format!("fsm state_pins: unknown state '{st}'"));
            }
            // The per-state override's drive fields are stamped verbatim (engine
            // behavioral.rs set_source_dc(drive_volts) / set_resistor_ohms(
            // drive_ohms)) with no flooring: a non-finite drive_volts injects a
            // NaN DC source, and a zero/negative drive_ohms stamps a non-physical
            // (negative) source resistance that destabilises the solve. Guard both
            // like the pull/od siblings above.
            for (role, ov) in pins {
                if let Some(v) = ov.drive_volts {
                    if !v.is_finite() {
                        errs.push(format!(
                            "fsm state_pins '{st}.{role}': drive_volts must be finite, got {v}"
                        ));
                    }
                }
                if let Some(r) = ov.drive_ohms {
                    if !r.is_finite() || r <= 0.0 {
                        errs.push(format!(
                            "fsm state_pins '{st}.{role}': drive_ohms must be positive, got {r}"
                        ));
                    }
                }
            }
            // The per-state override's drive fields are stamped verbatim (engine
            // behavioral.rs set_source_dc(drive_volts) / set_resistor_ohms(
            // drive_ohms)) with no flooring: a non-finite drive_volts injects a
            // NaN DC source, and a zero/negative drive_ohms stamps a non-physical
            // (negative) source resistance that destabilises the solve. Guard both
            // like the pull/od siblings above.
        }
    }

    let known_states: BTreeSet<&str> = b
        .fsm
        .as_ref()
        .map(|fsm| fsm.states.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let mut path_names = BTreeSet::new();
    for (i, path) in b.series_paths.iter().enumerate() {
        let valid_name = !path.name.is_empty()
            && path.name.chars().enumerate().all(|(index, c)| {
                c == '_' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
            });
        if !valid_name {
            errs.push(format!(
                "series_path {i}: name '{}' must be a non-empty expression identifier",
                path.name
            ));
        } else if !path_names.insert(path.name.as_str()) {
            errs.push(format!("series_path {i}: duplicate name '{}'", path.name));
        }
        if path.a.trim().is_empty() || path.b.trim().is_empty() || path.a == path.b {
            errs.push(format!(
                "series_path {i}: a and b must be distinct non-empty pin roles"
            ));
        }
        if !path.default_ohms.is_finite() || path.default_ohms <= 0.0 {
            errs.push(format!(
                "series_path {i}: default_ohms must be a positive finite number, got {}",
                path.default_ohms
            ));
        }
        for (state, ohms) in &path.state_ohms {
            if b.fsm.is_none() {
                errs.push(format!(
                    "series_path {i}: state_ohms names '{state}' but no fsm is declared"
                ));
            } else if !known_states.contains(state.as_str()) {
                errs.push(format!(
                    "series_path {i}: state_ohms names unknown state '{state}'"
                ));
            }
            if !ohms.is_finite() || *ohms <= 0.0 {
                errs.push(format!(
                    "series_path {i}: resistance for state '{state}' must be a positive finite number, got {ohms}"
                ));
            }
        }
    }

    let mut load_names = BTreeSet::new();
    for (i, load) in b.profiled_loads.iter().enumerate() {
        let valid_name = !load.name.is_empty()
            && load.name.chars().enumerate().all(|(index, c)| {
                c == '_' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
            });
        if !valid_name {
            errs.push(format!(
                "profiled_load {i}: name '{}' must be a non-empty expression identifier",
                load.name
            ));
        } else if !load_names.insert(load.name.as_str()) {
            errs.push(format!("profiled_load {i}: duplicate name '{}'", load.name));
        }
        if load.supply_pin.trim().is_empty() {
            errs.push(format!("profiled_load {i}: supply_pin is empty"));
        }
        if load
            .return_pin
            .as_ref()
            .is_some_and(|role| role.trim().is_empty() || role == &load.supply_pin)
        {
            errs.push(format!(
                "profiled_load {i}: return_pin must be non-empty and different from supply_pin"
            ));
        }
        if !load.start_s.is_finite() || load.start_s < 0.0 {
            errs.push(format!(
                "profiled_load {i}: start_s must be a non-negative finite number, got {}",
                load.start_s
            ));
        }
        if load.segments.is_empty() {
            errs.push(format!("profiled_load {i}: no segments declared"));
        }
        for (j, segment) in load.segments.iter().enumerate() {
            if !segment.level_a.is_finite() || segment.level_a < 0.0 {
                errs.push(format!(
                    "profiled_load {i} segment {j}: level_a must be a non-negative finite number, got {}",
                    segment.level_a
                ));
            }
            if segment
                .idle_a
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            {
                errs.push(format!(
                    "profiled_load {i} segment {j}: idle_a must be a non-negative finite number"
                ));
            }
            for (name, value) in [
                ("rise_s", segment.rise_s),
                ("duration_s", segment.duration_s),
                ("period_s", segment.period_s),
                ("jitter_s", segment.jitter_s),
            ] {
                if !value.is_finite() || value < 0.0 {
                    errs.push(format!(
                        "profiled_load {i} segment {j}: {name} must be a non-negative finite number, got {value}"
                    ));
                }
            }
            if segment.period_s > 0.0 && segment.rise_s + segment.duration_s > segment.period_s {
                errs.push(format!(
                    "profiled_load {i} segment {j}: rise_s + duration_s exceeds period_s"
                ));
            }
            if segment.period_s > 0.0 && segment.jitter_s >= segment.period_s {
                errs.push(format!(
                    "profiled_load {i} segment {j}: jitter_s must be smaller than period_s"
                ));
            }
        }
    }

    if let Some(c) = &b.converter {
        if c.out_pin.trim().is_empty() {
            errs.push("converter: out_pin is empty".to_string());
        }
        if c.in_pin.trim().is_empty() {
            errs.push("converter: in_pin is empty".to_string());
        }
        // Reject non-finite up front: `nan`/`inf` pass every comparison below
        // (NaN <= 0.0 is false), then a NaN vout_setpoint reaches the engine's
        // `v_cmd.clamp(0.0, vout_setpoint)` where a NaN max PANICS the solver on a
        // model that "validated OK", and a NaN efficiency propagates a NaN input
        // current into the network (R37 finiteness hardening, extended here).
        if !c.vout_setpoint.is_finite() || c.vout_setpoint <= 0.0 {
            errs.push(format!(
                "converter: vout_setpoint must be a positive finite number, got {}",
                c.vout_setpoint
            ));
        }
        if let Some(feedback) = &c.feedback {
            if feedback.pin.trim().is_empty() {
                errs.push("converter.feedback: pin is empty".to_string());
            }
            if !feedback.vref_v.is_finite() || feedback.vref_v <= 0.0 {
                errs.push(format!(
                    "converter.feedback: vref_v must be a positive finite number, got {}",
                    feedback.vref_v
                ));
            }
            if !feedback.relaxation_gain.is_finite() || feedback.relaxation_gain <= 0.0 {
                errs.push(format!(
                    "converter.feedback: relaxation_gain must be a positive finite number, got {}",
                    feedback.relaxation_gain
                ));
            }
        }
        if let Some(enable) = &c.enable {
            if enable.pin.trim().is_empty() {
                errs.push("converter.enable: pin is empty".to_string());
            }
            if !enable.high_threshold_v.is_finite() || enable.high_threshold_v < 0.0 {
                errs.push(format!(
                    "converter.enable: high_threshold_v must be a non-negative finite number, got {}",
                    enable.high_threshold_v
                ));
            }
        }
        if let Some(e) = c.efficiency {
            if !e.is_finite() || e <= 0.0 || e > 1.0 {
                errs.push(format!("converter: efficiency must be in (0,1], got {e}"));
            }
        }
        // A current limit must be a positive finite number. A NEGATIVE iout_limit_a
        // (a sign typo) is treated as a real CC threshold the output current always
        // exceeds (iout is `.abs()`), so the loop folds v_cmd negative and clamps it
        // to 0 V; the regulated rail silently reads 0 V for the whole run. A NaN
        // limit silently disables the CC loop. Reject both up front, like
        // vout_setpoint / efficiency above.
        for (name, lim) in [
            ("iout_limit_a", c.iout_limit_a),
            ("iin_limit_a", c.iin_limit_a),
        ] {
            if let Some(v) = lim {
                if !v.is_finite() || v <= 0.0 {
                    errs.push(format!(
                        "converter: {name} must be a positive finite number, got {v}"
                    ));
                }
            }
        }
        if let Some(sp) = &c.iin_program {
            if sp.rsense_ohms.is_some() != sp.rsense_refs.is_empty() {
                errs.push(
                    "converter.iin_program: specify exactly one of rsense_ohms or rsense_refs"
                        .to_string(),
                );
            }
            if sp.prog_ohms.is_some() == sp.prog_ref.is_some() {
                errs.push(
                    "converter.iin_program: specify exactly one of prog_ohms or prog_ref"
                        .to_string(),
                );
            }
            let mut seen_shunts = BTreeSet::new();
            for reference in &sp.rsense_refs {
                if reference.trim().is_empty() {
                    errs.push(
                        "converter.iin_program: rsense_refs entries must be non-empty".to_string(),
                    );
                } else if !seen_shunts.insert(reference) {
                    errs.push(format!(
                        "converter.iin_program: rsense_refs repeats '{reference}'"
                    ));
                }
            }
            // The literal shunt / program resistances are the missing siblings of
            // the gates below: the engine floors them (`rsense.max(1e-6)`), so a
            // sign-typo `rsense_ohms = -0.005` becomes 1e-6 and the input-current
            // limit balloons to ~50 kA; the over-current fold-back can never
            // engage and the converter is silently unprotected. Reject non-positive.
            for (name, v) in [("rsense_ohms", sp.rsense_ohms), ("prog_ohms", sp.prog_ohms)] {
                if let Some(v) = v {
                    if !v.is_finite() || v <= 0.0 {
                        errs.push(format!(
                            "converter.iin_program: {name} must be a positive finite number, got {v}"
                        ));
                    }
                }
            }
            // The literal shunt / program resistances are the missing siblings of
            // the gates below: the engine floors them (`rsense.max(1e-6)`), so a
            // sign-typo `rsense_ohms = -0.005` becomes 1e-6 and the input-current
            // limit balloons to ~50 kA; the over-current fold-back can never
            // engage and the converter is silently unprotected. Reject non-positive.
            if !sp.prog_ref_ohms.is_finite() || sp.prog_ref_ohms <= 0.0 {
                // A non-finite prog_ref_ohms (an `inf` overflow typo) passes a bare
                // `<= 0.0` test but the engine's `prog_ref.max(1.0)` yields inf, so
                // `v_sense = vprog_ref*prog/inf = 0` zeroes the input-current limit
                // and folds the regulated rail to 0 V for the whole run; the same
                // silent-zero the sibling gates below prevent. Reject non-finite too.
                errs.push(format!(
                    "converter.iin_program: prog_ref_ohms must be a positive finite number, got {}",
                    sp.prog_ref_ohms
                ));
            }
            // `vprog_ref` and `v_sense_full` gate the programmed input-current
            // limit the same way iout_limit_a/iin_limit_a gate the literal one:
            // the engine computes `v_sense = (vprog_ref*prog/prog_ref).min(
            // v_sense_full).max(0.0)` and `i_limit = v_sense/rsense`. A negative
            // or zero value for either (a sign typo) drives v_sense, and hence
            // the limit, to 0, so update_converter folds v_cmd to 0 and the
            // regulated rail silently reads 0 V for the whole run with no fault.
            // Reject both up front, like the literal limits above.
            for (name, v) in [
                ("vprog_ref", sp.vprog_ref),
                ("v_sense_full", sp.v_sense_full),
            ] {
                if !v.is_finite() || v <= 0.0 {
                    errs.push(format!(
                        "converter.iin_program: {name} must be a positive finite number, got {v}"
                    ));
                }
            }
        }
    }

    for law in &b.laws {
        if law.name.trim().is_empty() {
            errs.push("law: empty name".to_string());
        }
        if law.expr.trim().is_empty() {
            errs.push(format!("law '{}': empty expr", law.name));
        }
        if law.a.trim().is_empty() {
            errs.push(format!("law '{}': empty 'a' pin", law.name));
        }
        if matches!(law.kind, LawKind::Current) && law.b.is_none() {
            errs.push(format!(
                "law '{}': current law needs a 'b' sink pin",
                law.name
            ));
        }
    }

    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Behavioral {
        toml::from_str(src).expect("parse")
    }

    fn flags(b: &Behavioral, needle: &str) {
        let errs = validate_behavioral(b);
        assert!(
            errs.iter().any(|e| e.contains(needle)),
            "expected {needle:?}: {errs:?}"
        );
    }

    fn clean(b: &Behavioral) {
        assert!(
            validate_behavioral(b).is_empty(),
            "{:?}",
            validate_behavioral(b)
        );
    }

    fn fsm(transitions: Vec<Transition>, initial: &str) -> Behavioral {
        Behavioral {
            fsm: Some(Fsm {
                states: vec!["off".into(), "on".into()],
                initial: Some(initial.into()),
                transitions,
                state_pins: BTreeMap::new(),
            }),
            ..Default::default()
        }
    }

    fn transition(to: &str, min_dwell_s: Option<f64>, guard_dwell_s: Option<f64>) -> Transition {
        Transition {
            from: "off".into(),
            to: to.into(),
            guard: "v_en > 1.0".into(),
            min_dwell_s,
            guard_dwell_s,
        }
    }

    #[test]
    fn pin_pull_and_open_drain_fields_must_be_finite() {
        let b = Behavioral::default();
        assert!(b.is_empty());
        clean(&b);

        flags(&parse("[pins.shphld]\npull_to = \"vsys\"\n"), "pull_ohms");
        flags(
            &parse("[pins.shphld]\npull_to = \"vsys\"\npull_ohms = inf\n"),
            "pull_ohms",
        );
        flags(
            &parse("[pins.shphld]\npull_to_volts = nan\npull_ohms = 100000.0\n"),
            "pull_to_volts",
        );
        flags(
            &parse("[pins.stat]\nopen_drain = true\nod_ohms = 10.0\nod_to_volts = inf\n"),
            "od_to_volts",
        );
        clean(&parse(
            "[pins.vneg]\npull_to_volts = -5.0\npull_ohms = 1000.0\n",
        ));

        let b = parse("[pins.shphld]\npull_to = \"vsys\"\npull_ohms = 100000.0\n");
        let back: Behavioral = toml::from_str(&toml::to_string(&b).unwrap()).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn fsm_states_dwells_and_state_pin_drives_are_validated() {
        let b = fsm(vec![transition("on", Some(f64::NAN), None)], "off");
        flags(&b, "min_dwell_s");
        clean(&fsm(vec![transition("on", Some(0.05), None)], "off"));

        let b = fsm(vec![transition("nowhere", None, None)], "idle");
        flags(&b, "initial");
        flags(&b, "nowhere");

        let drive = |drive_volts: f64, drive_ohms: f64| {
            let mut b = fsm(Vec::new(), "on");
            let pins = BTreeMap::from([(
                "out".to_string(),
                StatePinBehaviour {
                    drive_volts: Some(drive_volts),
                    drive_ohms: Some(drive_ohms),
                    ..Default::default()
                },
            )]);
            b.fsm.as_mut().unwrap().state_pins.insert("on".into(), pins);
            b
        };
        flags(&drive(3.3, -50.0), "drive_ohms");
        flags(&drive(f64::NAN, 50.0), "drive_volts");
        clean(&drive(3.3, 50.0));
    }

    #[test]
    fn series_path_and_continuous_guard_dwell_validate_fail_closed() {
        let mut b = Behavioral::default();
        b.fsm = Some(Fsm {
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
        });
        b.series_paths.push(SeriesPath {
            name: "fuse".into(),
            a: "a".into(),
            b: "b".into(),
            default_ohms: 0.04,
            state_ohms: [("closed".into(), 0.04), ("tripped".into(), 1e9)]
                .into_iter()
                .collect(),
        });
        clean(&b);

        b.series_paths[0].state_ohms.insert("unknown".into(), 0.1);
        b.fsm.as_mut().unwrap().transitions[0].guard_dwell_s = Some(f64::NAN);
        flags(&b, "unknown state");
        flags(&b, "guard_dwell_s");
    }

    #[test]
    fn profiled_load_validates_timing_and_current_fail_closed() {
        let mut b = Behavioral::default();
        b.profiled_loads.push(ProfiledLoad {
            name: "boot".into(),
            supply_pin: "vdd".into(),
            return_pin: Some("gnd".into()),
            start_s: 0.001,
            seed: 7,
            segments: vec![Segment {
                level_a: 0.040,
                rise_s: 0.001,
                duration_s: 0.010,
                period_s: 0.100,
                idle_a: Some(0.005),
                jitter_s: 0.001,
            }],
        });
        clean(&b);
        b.profiled_loads[0].segments[0].level_a = -1.0;
        b.profiled_loads[0].segments[0].jitter_s = 0.100;
        flags(&b, "level_a");
        flags(&b, "jitter_s");
    }

    fn converter(program: &str) -> Behavioral {
        parse(&format!(
            "[converter]\ntopology = \"buck_boost\"\nout_pin = \"bat\"\nin_pin = \"pvin\"\n\
             vout_setpoint = 14.4\nefficiency = 0.92\n[converter.iin_program]\nprog_ref = \"R8\"\n{program}\n"
        ))
    }

    #[test]
    fn iin_program_constants_must_be_positive_finite() {
        let b = converter("rsense_refs = [\"R49\", \"R50\"]\nvprog_ref = 1.19\nprog_ref_ohms = 100000.0\nv_sense_full = 0.05");
        let c = b.converter.as_ref().unwrap();
        assert_eq!(c.topology, Topology::BuckBoost);
        assert_eq!(
            c.iin_program.as_ref().unwrap().prog_ref.as_deref(),
            Some("R8")
        );
        clean(&b);

        let base = "vprog_ref = 1.19\nprog_ref_ohms = 100000.0\n";
        flags(
            &converter(&format!("{base}v_sense_full = -0.05")),
            "v_sense_full",
        );
        flags(
            &converter(&format!("{base}v_sense_full = 0.0")),
            "v_sense_full",
        );
        flags(
            &converter("vprog_ref = -1.19\nprog_ref_ohms = 100000.0\nv_sense_full = 0.05"),
            "vprog_ref",
        );
        flags(
            &converter("vprog_ref = 1.19\nprog_ref_ohms = inf\nv_sense_full = 0.05"),
            "prog_ref_ohms",
        );
        flags(
            &converter(&format!("{base}v_sense_full = 0.05\nrsense_ohms = -0.005")),
            "rsense_ohms",
        );
        flags(
            &converter(&format!(
                "{base}v_sense_full = 0.05\nrsense_ohms = 0.005\nprog_ohms = 0.0"
            )),
            "prog_ohms",
        );
        clean(&converter(&format!(
            "{base}v_sense_full = 0.05\nrsense_ohms = 0.005"
        )));
    }

    #[test]
    fn converter_setpoint_efficiency_and_limits_must_be_positive_finite() {
        let conv = |extra: &str| {
            parse(&format!(
                "[converter]\ntopology=\"buck\"\nout_pin=\"o\"\nin_pin=\"i\"\n{extra}\n"
            ))
        };
        flags(&conv("vout_setpoint = nan"), "vout_setpoint");
        flags(&conv("vout_setpoint = 5.0\nefficiency = nan"), "efficiency");
        flags(
            &conv("vout_setpoint = 5.0\niout_limit_a = -1.0"),
            "iout_limit_a",
        );
        flags(
            &conv("vout_setpoint = 5.0\niin_limit_a = nan"),
            "iin_limit_a",
        );
        clean(&conv("vout_setpoint = 5.0\niout_limit_a = 1.0"));
    }

    #[test]
    fn law_current_needs_sink() {
        let mut b = Behavioral::default();
        b.laws.push(Law {
            name: "leak".into(),
            kind: LawKind::Current,
            a: "c9".into(),
            b: None,
            expr: "0.01".into(),
            r_ohms: None,
            only_in_state: None,
        });
        flags(&b, "sink");
    }
}
