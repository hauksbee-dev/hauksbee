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

use crate::check::{is_identifier, positive_finite, Problems};
use crate::profile::Segment;
use crate::units;

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
    #[must_use]
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

    /// The pin role the output transistor's emitter/source returns to (e.g.
    /// `"gnd"`, `"vss"`, `"pgnd"`). The asserted sink references THAT pin's
    /// net, so a part whose ground pin sits on a sense shunt or a negative
    /// rail sinks to it rather than to board ground. When neither this nor
    /// `od_to_volts` is set, the part's own ground-pin role (`gnd`, `vss`,
    /// `pgnd`, `agnd`, in that order) is used when connected; board ground
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub od_to: Option<String>,

    /// Open-drain on-resistance (ohms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub od_ohms: Option<f64>,

    /// Enable-input threshold (V). When set, this pin is read as a logic enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_threshold_v: Option<f64>,

    /// Enable polarity: `true` = active-high (asserted above threshold),
    /// `false` = active-low (asserted below threshold).
    #[serde(default = "default_true", skip_serializing_if = "Clone::clone")]
    pub enable_active_high: bool,
}

const fn default_true() -> bool {
    true
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
    /// The most this load draws in a state that data or firmware sets and the
    /// board cannot show (an addressable LED latched full white). Runs keep the
    /// segments; the DC stress pass also judges every such load at this level,
    /// and a fault found only there is reported as conditional on that state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_state_max_a: Option<f64>,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    #[default]
    Buck,
    Boost,
    BuckBoost,
}

/// How a converter's output net relates to its `out_pin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutVia {
    /// The output is the rail `out_pin` senses through a filter resistor.
    ///
    /// The rail is the far end of the one resistor on the pin's net that
    /// leads neither to ground nor back to the part (a divider resistor from
    /// the pin to the part's own feedback pin is not the filter). A pin on a
    /// net with other active parts, or with no such resistor, sits on the rail
    /// itself. A controller whose power stage has no output pin of its own
    /// (the LTC4020's output is the far side of its CSP filter) states its
    /// rail this way.
    SenseFilter,
}

/// An averaged (cycle-averaged) switching-converter block.
///
/// The runtime realises it as an averaged current-mode regulation loop: a PI
/// controller crossing over at [`Converter::loop_crossover_hz`] commands the
/// current delivered into the output pin, bounded by the output- and
/// input-current limits and, for a buck, by a switch node of at most
/// `max_duty * vin`. Power is conserved through `efficiency`, so the input
/// current the runtime draws on the input pin is `Vsw*Iout / (efficiency*Vin)`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Converter {
    pub topology: Topology,

    /// Output pin role (the regulated rail).
    pub out_pin: String,

    /// Where the output lands when it is not `out_pin`'s own net. Absent, the
    /// converter drives `out_pin`. See [`OutVia`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_via: Option<OutVia>,

    /// Input pin role (where the converter draws its current).
    pub in_pin: String,

    /// Regulated output voltage (V). May be overridden by a feedback divider
    /// programming pin in a future extension; for now it is the setpoint.
    pub vout_setpoint: f64,

    /// Optional board-programmed feedback loop.  When present, the converter
    /// drives its averaged output until the voltage on `pin` reaches `vref_v`.
    /// This lets an adjustable buck use the board's actual divider instead of
    /// baking one product's output voltage into a reusable IC model.  The
    /// loop's speed is [`Converter::crossover_hz`], not anything here.
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

    /// Datasheet maximum duty cycle (0..1) of a buck: its averaged switch node
    /// never exceeds `max_duty * vin`, so an input near the output drops out
    /// below it instead of passing through. Absent means the card states none
    /// and the ceiling is `vin` itself. A minimum off-time converts as
    /// `1 - t_off_min * f_sw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duty: Option<f64>,

    /// Programmable input-current limit: a sense resistor on `in_pin` (the
    /// shunt the converter measures across) plus a programming resistor on
    /// `prog_pin`. When present, the runtime computes `iin_limit_a` from the
    /// resistors per [`SenseProgram`] instead of using the literal field above,
    /// exactly the way the real part is programmed on the board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iin_program: Option<SenseProgram>,

    /// Datasheet switching frequency (Hz). Only sets the default loop
    /// crossover (`switching_hz / 10`) when `crossover_hz` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switching_hz: Option<f64>,

    /// Regulation-loop crossover frequency (Hz): the runtime closes an
    /// averaged current-mode loop at this bandwidth, so a load step `dI`
    /// dips the output about `dI / (2 pi crossover_hz C_out)`. Absent, the
    /// binder assumes a conventional `f_sw / 10` and discloses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crossover_hz: Option<f64>,
}

/// Switching frequency assumed when a card states neither `crossover_hz` nor
/// `switching_hz`: a typical point-of-load part.
pub const DEFAULT_SWITCHING_HZ: f64 = 300e3;

impl Converter {
    /// The loop crossover (Hz) and whether the card stated it: `crossover_hz`,
    /// else a tenth of `switching_hz` (or of [`DEFAULT_SWITCHING_HZ`]).
    #[must_use]
    pub fn loop_crossover_hz(&self) -> (f64, bool) {
        let fc = self.crossover_hz;
        let fsw = self.switching_hz.unwrap_or(DEFAULT_SWITCHING_HZ);
        (fc.unwrap_or(fsw / 10.0), fc.is_some())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FeedbackControl {
    /// Pin role connected to the external feedback divider midpoint.
    pub pin: String,
    /// Datasheet reference voltage at the feedback pin.
    pub vref_v: f64,
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
/// params.
///
/// Sandboxed: only arithmetic, comparison, and `if`/`min`/`max`/`abs` builtins
/// are available, no variables the runtime did not bind, no I/O.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Law {
    /// Diagnostic name (e.g. "`balancer_leak`").
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

/// Record a series-path or profiled-load name that is not an identifier, or
/// that an earlier one of the same list already took.
fn unique_identifier<'a>(
    p: &mut Problems,
    seen: &mut BTreeSet<&'a str>,
    what: &str,
    name: &'a str,
) {
    if !is_identifier(name) {
        p.push(format!(
            "{what}: name '{name}' must be a non-empty expression identifier"
        ));
    } else if !seen.insert(name) {
        p.push(format!("{what}: duplicate name '{name}'"));
    }
}

/// Validate a behavioural block for internal consistency.
///
/// Returns a list of human-readable problems (empty = valid). Cheap structural
/// checks only; the expression syntax is checked separately by the engine when
/// it compiles the laws against a real pin set.
///
/// Every float here is stamped verbatim into the solve (pull / open-drain /
/// drive targets as DC sources, resistances unfloored, dwells as
/// `t_in_state < min` compares, converter limits as fold-back thresholds), so
/// each gate refuses NaN and the infinities as well as the wrong sign: a `nan`
/// that "validated OK" poisons the whole MNA solve or panics a `clamp`, and a
/// negative limit silently folds a regulated rail to 0 V for the whole run.
#[must_use]
// One flat walk, one rule per line, in the order the block's sections are declared.
#[allow(clippy::too_many_lines)]
pub fn validate_behavioral(b: &Behavioral) -> Vec<String> {
    let mut p = Problems::default();

    for (role, pin) in &b.pins {
        let has_pull = pin.pull_to.is_some() || pin.pull_to_volts.is_some();
        p.require(!has_pull || pin.pull_ohms.is_some(), || {
            format!("pin '{role}': pull target set but pull_ohms missing")
        });
        p.positive(format!("pin '{role}': pull_ohms"), pin.pull_ohms);
        p.finite(format!("pin '{role}': pull_to_volts"), pin.pull_to_volts);
        if pin.open_drain {
            p.positive(format!("pin '{role}': od_ohms"), pin.od_ohms);
            p.finite(format!("pin '{role}': od_to_volts"), pin.od_to_volts);
        }
    }

    let states: BTreeSet<&str> = b
        .fsm
        .as_ref()
        .map(|fsm| fsm.states.iter().map(String::as_str).collect())
        .unwrap_or_default();
    if let Some(fsm) = &b.fsm {
        p.require(!fsm.states.is_empty(), || "fsm: no states declared".into());
        if let Some(init) = &fsm.initial {
            p.require(states.contains(init.as_str()), || {
                format!("fsm: initial state '{init}' is not in states")
            });
        }
        for (i, tr) in fsm.transitions.iter().enumerate() {
            p.require(states.contains(tr.from.as_str()), || {
                format!("fsm transition {i}: unknown from-state '{}'", tr.from)
            });
            p.require(states.contains(tr.to.as_str()), || {
                format!("fsm transition {i}: unknown to-state '{}'", tr.to)
            });
            p.require(!tr.guard.trim().is_empty(), || {
                format!("fsm transition {i}: empty guard")
            });
            if let Err(e) = units::check_dimensions(&tr.guard, None) {
                p.push(format!("fsm transition {i}: guard {e}"));
            }
            p.nonneg(format!("fsm transition {i}: min_dwell_s"), tr.min_dwell_s);
            p.nonneg(
                format!("fsm transition {i}: guard_dwell_s"),
                tr.guard_dwell_s,
            );
        }
        for (st, pins) in &fsm.state_pins {
            p.require(states.contains(st.as_str()), || {
                format!("fsm state_pins: unknown state '{st}'")
            });
            for (role, ov) in pins {
                let at = format!("fsm state_pins '{st}.{role}'");
                p.finite(format!("{at}: drive_volts"), ov.drive_volts);
                p.positive(format!("{at}: drive_ohms"), ov.drive_ohms);
            }
        }
    }

    let mut path_names = BTreeSet::new();
    for (i, path) in b.series_paths.iter().enumerate() {
        let what = format!("series_path {i}");
        unique_identifier(&mut p, &mut path_names, &what, &path.name);
        p.require(
            !path.a.trim().is_empty() && !path.b.trim().is_empty() && path.a != path.b,
            || format!("{what}: a and b must be distinct non-empty pin roles"),
        );
        p.positive(format!("{what}: default_ohms"), path.default_ohms);
        for (state, ohms) in &path.state_ohms {
            if b.fsm.is_none() {
                p.push(format!(
                    "{what}: state_ohms names '{state}' but no fsm is declared"
                ));
            } else {
                p.require(states.contains(state.as_str()), || {
                    format!("{what}: state_ohms names unknown state '{state}'")
                });
            }
            p.positive(format!("{what}: resistance for state '{state}'"), *ohms);
        }
    }

    let mut load_names = BTreeSet::new();
    for (i, load) in b.profiled_loads.iter().enumerate() {
        let what = format!("profiled_load {i}");
        unique_identifier(&mut p, &mut load_names, &what, &load.name);
        p.non_empty(format!("{what}: supply_pin"), &load.supply_pin);
        let bad_return = load
            .return_pin
            .as_ref()
            .is_some_and(|role| role.trim().is_empty() || role == &load.supply_pin);
        p.require(!bad_return, || {
            format!("{what}: return_pin must be non-empty and different from supply_pin")
        });
        p.nonneg(format!("{what}: start_s"), load.start_s);
        p.require(!load.segments.is_empty(), || {
            format!("{what}: no segments declared")
        });
        if let Some(max) = load.unknown_state_max_a {
            let floor = load.segments.iter().map(|s| s.level_a).fold(0.0, f64::max);
            p.require(max.is_finite() && max >= floor, || {
                format!("{what}: unknown_state_max_a must be finite and at least every segment's level_a")
            });
        }
        for (j, s) in load.segments.iter().enumerate() {
            let seg = format!("{what} segment {j}");
            p.nonneg(format!("{seg}: level_a"), s.level_a);
            p.nonneg(format!("{seg}: idle_a"), s.idle_a);
            for (name, v) in [
                ("rise_s", s.rise_s),
                ("duration_s", s.duration_s),
                ("period_s", s.period_s),
                ("jitter_s", s.jitter_s),
            ] {
                p.nonneg(format!("{seg}: {name}"), v);
            }
            if s.period_s > 0.0 {
                p.require(s.rise_s + s.duration_s <= s.period_s, || {
                    format!("{seg}: rise_s + duration_s exceeds period_s")
                });
                p.require(s.jitter_s < s.period_s, || {
                    format!("{seg}: jitter_s must be smaller than period_s")
                });
            }
        }
    }

    if let Some(c) = &b.converter {
        p.non_empty("converter: out_pin", &c.out_pin);
        p.non_empty("converter: in_pin", &c.in_pin);
        if c.out_via == Some(OutVia::SenseFilter) {
            // A filter moves the rail one resistor off a pin that only senses
            // it. From the input, a supply, ground or output pin, or the switch
            // node, it would land the output on whatever that pin's resistor
            // reaches.
            let pin = c.out_pin.trim();
            let power = crate::pin_roles::power_pin(pin, crate::ComponentKind::Vreg).is_some()
                || ["out", "vout"].iter().any(|o| pin.eq_ignore_ascii_case(o));
            let switch = crate::pin_roles::is_switch_node_role(pin);
            p.require(
                !pin.eq_ignore_ascii_case(c.in_pin.trim()) && !power && !switch,
                || {
                    format!(
                        "converter: out_via = \"sense_filter\" needs out_pin to be a sense \
                         input, but '{pin}' is the input, a supply, ground or output pin, or \
                         the switch node"
                    )
                },
            );
        }
        p.positive("converter: vout_setpoint", c.vout_setpoint);
        if let Some(fb) = &c.feedback {
            p.non_empty("converter.feedback: pin", &fb.pin);
            p.positive("converter.feedback: vref_v", fb.vref_v);
        }
        if let Some(en) = &c.enable {
            p.non_empty("converter.enable: pin", &en.pin);
            p.nonneg("converter.enable: high_threshold_v", en.high_threshold_v);
        }
        for (name, v) in [("efficiency", c.efficiency), ("max_duty", c.max_duty)] {
            let v = v.unwrap_or(1.0);
            p.require(positive_finite(v) && v <= 1.0, || {
                format!("converter: {name} must be in (0,1], got {v}")
            });
        }
        p.require(c.max_duty.is_none() || c.topology == Topology::Buck, || {
            "converter: max_duty is only modelled for a buck".to_string()
        });
        p.positive("converter: iout_limit_a", c.iout_limit_a);
        p.positive("converter: iin_limit_a", c.iin_limit_a);
        p.positive("converter: switching_hz", c.switching_hz);
        p.positive("converter: crossover_hz", c.crossover_hz);
        let fc = c.loop_crossover_hz().0;
        let fsw = c.switching_hz.unwrap_or(f64::INFINITY);
        p.require(fc < fsw / 2.0, || {
            format!("converter: crossover_hz {fc} must be below switching_hz/2")
        });
        if let Some(sp) = &c.iin_program {
            let one_rsense = sp.rsense_ohms.is_some() == sp.rsense_refs.is_empty();
            p.require(one_rsense, || {
                "converter.iin_program: specify exactly one of rsense_ohms or rsense_refs".into()
            });
            p.require(sp.prog_ohms.is_some() != sp.prog_ref.is_some(), || {
                "converter.iin_program: specify exactly one of prog_ohms or prog_ref".into()
            });
            let mut shunts = BTreeSet::new();
            for r in &sp.rsense_refs {
                if r.trim().is_empty() {
                    p.push("converter.iin_program: rsense_refs entries must be non-empty");
                } else if !shunts.insert(r) {
                    p.push(format!("converter.iin_program: rsense_refs repeats '{r}'"));
                }
            }
            // The literal resistances are floored by the engine (`rsense.max(1e-6)`),
            // so a sign typo balloons the limit to ~50 kA; the reference constants
            // are divisors, so a zero/inf collapses it to 0 A. Both silently.
            for (name, v) in [
                ("rsense_ohms", sp.rsense_ohms),
                ("prog_ohms", sp.prog_ohms),
                ("prog_ref_ohms", Some(sp.prog_ref_ohms)),
                ("vprog_ref", Some(sp.vprog_ref)),
                ("v_sense_full", Some(sp.v_sense_full)),
            ] {
                p.positive(format!("converter.iin_program: {name}"), v);
            }
        }
    }

    for law in &b.laws {
        p.require(!law.name.trim().is_empty(), || "law: empty name".into());
        p.require(!law.expr.trim().is_empty(), || {
            format!("law '{}': empty expr", law.name)
        });
        p.require(!law.a.trim().is_empty(), || {
            format!("law '{}': empty 'a' pin", law.name)
        });
        p.require(law.kind != LawKind::Current || law.b.is_some(), || {
            format!("law '{}': current law needs a 'b' sink pin", law.name)
        });
        let unit = match law.kind {
            LawKind::Current => units::CURRENT,
            LawKind::Voltage => units::VOLTAGE,
        };
        if let Err(e) = units::check_dimensions(&law.expr, Some(unit)) {
            p.push(format!("law '{}': {e}", law.name));
        }
    }

    p.0
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
        let mut b = Behavioral {
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
            ..Behavioral::default()
        };
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
            unknown_state_max_a: None,
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
        b.profiled_loads[0].unknown_state_max_a = Some(0.039);
        flags(&b, "unknown_state_max_a");
        b.profiled_loads[0].unknown_state_max_a = Some(0.040);
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
        flags(&conv("vout_setpoint = 5.0\nmax_duty = 1.2"), "max_duty");
        clean(&conv("vout_setpoint = 5.0\nmax_duty = 0.9"));
        flags(&conv("vout_setpoint = 5\ncrossover_hz = -1"), "crossover");
        let fast_loop = "vout_setpoint = 5\ncrossover_hz = 2e5\nswitching_hz = 3e5";
        flags(&conv(fast_loop), "below");
        let boost = conv("vout_setpoint = 5.0\nmax_duty = 0.9")
            .converter
            .map(|c| Converter {
                topology: Topology::Boost,
                ..c
            });
        flags(
            &Behavioral {
                converter: boost,
                ..Default::default()
            },
            "only modelled for a buck",
        );
    }

    #[test]
    fn a_sense_filter_output_needs_a_sense_pin() {
        let conv = |out: &str| {
            parse(&format!(
                "[converter]\ntopology=\"buck_boost\"\nout_pin=\"{out}\"\nout_via=\"sense_filter\"\n\
                 in_pin=\"pvin\"\nvout_setpoint = 28.8\n"
            ))
        };
        let b = conv("csp");
        assert_eq!(
            b.converter.as_ref().unwrap().out_via,
            Some(OutVia::SenseFilter)
        );
        clean(&b);
        for out in ["pvin", "gnd", "vcc", "vout", "sw", "lx", "sw2"] {
            flags(&conv(out), "sense_filter");
        }
        let unknown = "[converter]\nout_pin=\"csp\"\nout_via=\"far_side\"\nin_pin=\"pvin\"\n\
                       vout_setpoint = 1.0\n";
        assert!(
            toml::from_str::<Behavioral>(unknown).is_err(),
            "an unknown out_via is refused at parse"
        );
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
