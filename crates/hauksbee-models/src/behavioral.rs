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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
}

impl Behavioral {
    /// True when the block carries nothing the runtime would stamp.
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
            && self.fsm.is_none()
            && self.converter.is_none()
            && self.laws.is_empty()
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
/// 7.15k) lands.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct SenseProgram {
    /// The sense resistor on the input path (ohms). Either a literal, or read
    /// from a named board resistor via `rsense_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsense_ohms: Option<f64>,

    /// Board reference designator of the sense resistor, read at bind time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsense_ref: Option<String>,

    /// The programming resistor (ohms), or read from `prog_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prog_ohms: Option<f64>,

    /// Board reference designator of the programming resistor (e.g. "R8").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prog_ref: Option<String>,

    /// Internal reference voltage the programming divider works against (V).
    pub vprog_ref: f64,

    /// The programming-divider numerator resistance (ohms): the on-die or fixed
    /// resistor the external `prog_ohms` divides against. The threshold scales
    /// as `prog_ohms / prog_ref_ohms` — linearly with the programming resistor,
    /// consistent with the struct-level doc and the engine's `program_iin_limit`
    /// (an earlier version of this line had the ratio inverted).
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
                errs.push(format!("pin '{role}': pull_to_volts must be finite, got {v}"));
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
        if let Some(e) = c.efficiency {
            if !e.is_finite() || e <= 0.0 || e > 1.0 {
                errs.push(format!("converter: efficiency must be in (0,1], got {e}"));
            }
        }
        // A current limit must be a positive finite number. A NEGATIVE iout_limit_a
        // (a sign typo) is treated as a real CC threshold the output current always
        // exceeds (iout is `.abs()`), so the loop folds v_cmd negative and clamps it
        // to 0 V — the regulated rail silently reads 0 V for the whole run. A NaN
        // limit silently disables the CC loop. Reject both up front, like
        // vout_setpoint / efficiency above.
        for (name, lim) in [("iout_limit_a", c.iout_limit_a), ("iin_limit_a", c.iin_limit_a)] {
            if let Some(v) = lim {
                if !v.is_finite() || v <= 0.0 {
                    errs.push(format!("converter: {name} must be a positive finite number, got {v}"));
                }
            }
        }
        if let Some(sp) = &c.iin_program {
            if sp.rsense_ohms.is_none() && sp.rsense_ref.is_none() {
                errs.push("converter.iin_program: need rsense_ohms or rsense_ref".to_string());
            }
            if sp.prog_ohms.is_none() && sp.prog_ref.is_none() {
                errs.push("converter.iin_program: need prog_ohms or prog_ref".to_string());
            }
            if !sp.prog_ref_ohms.is_finite() || sp.prog_ref_ohms <= 0.0 {
                // A non-finite prog_ref_ohms (an `inf` overflow typo) passes a bare
                // `<= 0.0` test but the engine's `prog_ref.max(1.0)` yields inf, so
                // `v_sense = vprog_ref*prog/inf = 0` zeroes the input-current limit
                // and folds the regulated rail to 0 V for the whole run — the same
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
            // or zero value for either (a sign typo) drives v_sense — and hence
            // the limit — to 0, so update_converter folds v_cmd to 0 and the
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

    #[test]
    fn empty_behavioral_is_empty() {
        let b = Behavioral::default();
        assert!(b.is_empty());
        assert!(validate_behavioral(&b).is_empty());
    }

    #[test]
    fn pull_without_ohms_is_flagged() {
        let mut b = Behavioral::default();
        b.pins.insert(
            "shphld".into(),
            BehavioralPin {
                pull_to: Some("vsys".into()),
                ..Default::default()
            },
        );
        let errs = validate_behavioral(&b);
        assert!(errs.iter().any(|e| e.contains("pull_ohms")), "{errs:?}");
    }

    #[test]
    fn fsm_transition_nonfinite_min_dwell_is_rejected() {
        // R52: the engine applies min_dwell_s as `if t_in_state < min { continue }`;
        // a NaN makes that false, silently skipping the dwell gate so the
        // transition fires immediately instead of waiting the intended delay.
        let mut b = Behavioral::default();
        b.fsm = Some(Fsm {
            states: vec!["off".into(), "on".into()],
            initial: Some("off".into()),
            transitions: vec![Transition {
                from: "off".into(),
                to: "on".into(),
                guard: "v_en > 1.0".into(),
                min_dwell_s: Some(f64::NAN),
            }],
            state_pins: BTreeMap::new(),
        });
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("min_dwell_s")),
            "a NaN min_dwell_s must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // A finite non-negative dwell still validates clean.
        if let Some(fsm) = &mut b.fsm {
            fsm.transitions[0].min_dwell_s = Some(0.05);
        }
        assert!(
            validate_behavioral(&b).is_empty(),
            "a valid min_dwell_s must pass: {:?}",
            validate_behavioral(&b)
        );
    }

    #[test]
    fn nonfinite_pull_and_od_target_voltages_are_rejected() {
        // R49: pull_to_volts / od_to_volts are stamped verbatim as DC sources, so
        // a `nan`/`inf` literal poisons the whole MNA solve with no fault. Only
        // finiteness is checked (a negative rail is legal), like the pull_ohms
        // sibling. Base bug: these two voltage fields were never validated.
        let mut b = Behavioral::default();
        b.pins.insert(
            "shphld".into(),
            BehavioralPin {
                pull_to_volts: Some(f64::NAN),
                pull_ohms: Some(100_000.0),
                ..Default::default()
            },
        );
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("pull_to_volts")),
            "a NaN pull_to_volts must be rejected: {:?}",
            validate_behavioral(&b)
        );

        let mut b = Behavioral::default();
        b.pins.insert(
            "stat".into(),
            BehavioralPin {
                open_drain: true,
                od_ohms: Some(10.0),
                od_to_volts: Some(f64::INFINITY),
                ..Default::default()
            },
        );
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("od_to_volts")),
            "an inf od_to_volts must be rejected: {:?}",
            validate_behavioral(&b)
        );

        // A finite (even negative) rail voltage still validates clean.
        let mut b = Behavioral::default();
        b.pins.insert(
            "vneg".into(),
            BehavioralPin {
                pull_to_volts: Some(-5.0),
                pull_ohms: Some(1_000.0),
                ..Default::default()
            },
        );
        assert!(
            validate_behavioral(&b).is_empty(),
            "a finite negative rail is legal: {:?}",
            validate_behavioral(&b)
        );
    }

    #[test]
    fn state_pin_drive_fields_are_validated() {
        // R49: a per-state override's drive_volts/drive_ohms are stamped verbatim
        // (NaN DC source / negative source resistance) with no flooring. Validate
        // them like the pull/od siblings. Base bug: state_pins only checked names.
        let mk = |drive_volts: Option<f64>, drive_ohms: Option<f64>| {
            let mut sp: BTreeMap<String, BTreeMap<String, StatePinBehaviour>> = BTreeMap::new();
            let mut pins = BTreeMap::new();
            pins.insert(
                "out".to_string(),
                StatePinBehaviour { drive_volts, drive_ohms, ..Default::default() },
            );
            sp.insert("on".to_string(), pins);
            Behavioral {
                fsm: Some(Fsm {
                    states: vec!["on".into()],
                    initial: Some("on".into()),
                    transitions: Vec::new(),
                    state_pins: sp,
                }),
                ..Default::default()
            }
        };
        // Negative source resistance.
        let b = mk(Some(3.3), Some(-50.0));
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("drive_ohms")),
            "negative drive_ohms must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // NaN drive voltage.
        let b = mk(Some(f64::NAN), Some(50.0));
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("drive_volts")),
            "NaN drive_volts must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // A well-formed push-pull override validates clean.
        let b = mk(Some(3.3), Some(50.0));
        assert!(
            validate_behavioral(&b).is_empty(),
            "a valid drive override must pass: {:?}",
            validate_behavioral(&b)
        );
    }

    #[test]
    fn fsm_unknown_state_flagged() {
        let mut b = Behavioral::default();
        b.fsm = Some(Fsm {
            states: vec!["off".into(), "on".into()],
            initial: Some("idle".into()),
            transitions: vec![Transition {
                from: "off".into(),
                to: "nowhere".into(),
                guard: "v_en > 1.0".into(),
                min_dwell_s: None,
            }],
            state_pins: BTreeMap::new(),
        });
        let errs = validate_behavioral(&b);
        assert!(errs.iter().any(|e| e.contains("initial")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("nowhere")), "{errs:?}");
    }

    #[test]
    fn converter_parses_with_sense_program() {
        let toml = r#"
[converter]
topology = "buck_boost"
out_pin = "bat"
in_pin = "pvin"
vout_setpoint = 14.4
efficiency = 0.92

[converter.iin_program]
rsense_ref = "R49"
prog_ref = "R8"
vprog_ref = 1.19
prog_ref_ohms = 100000.0
v_sense_full = 0.05
"#;
        let b: Behavioral = toml::from_str(toml).expect("parse");
        let c = b.converter.as_ref().unwrap();
        assert_eq!(c.topology, Topology::BuckBoost);
        let sp = c.iin_program.as_ref().unwrap();
        assert_eq!(sp.prog_ref.as_deref(), Some("R8"));
        assert!(
            validate_behavioral(&b).is_empty(),
            "{:?}",
            validate_behavioral(&b)
        );
    }

    #[test]
    fn iin_program_rejects_nonpositive_vsense_full_and_vprog_ref() {
        // A sign-typo v_sense_full (or vprog_ref) drives the programmed
        // input-current limit to 0, so update_converter folds v_cmd to 0 and the
        // regulated rail silently reads 0 V for the whole run with no fault. It
        // must be rejected at validation, like the literal iout_limit_a/
        // iin_limit_a limits. Base bug: only prog_ref_ohms was validated.
        let base = |v_sense_full: f64, vprog_ref: f64| {
            format!(
                r#"
[converter]
topology = "buck_boost"
out_pin = "bat"
in_pin = "pvin"
vout_setpoint = 14.4
efficiency = 0.92

[converter.iin_program]
rsense_ref = "R49"
prog_ref = "R8"
vprog_ref = {vprog_ref}
prog_ref_ohms = 100000.0
v_sense_full = {v_sense_full}
"#
            )
        };
        // Sign-typo v_sense_full.
        let b: Behavioral = toml::from_str(&base(-0.05, 1.19)).expect("parse");
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("v_sense_full")),
            "negative v_sense_full must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // Zero v_sense_full.
        let b: Behavioral = toml::from_str(&base(0.0, 1.19)).expect("parse");
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("v_sense_full")),
            "zero v_sense_full must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // Sign-typo vprog_ref.
        let b: Behavioral = toml::from_str(&base(0.05, -1.19)).expect("parse");
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("vprog_ref")),
            "negative vprog_ref must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // The legitimate positive pair still validates clean.
        let b: Behavioral = toml::from_str(&base(0.05, 1.19)).expect("parse");
        assert!(
            validate_behavioral(&b).is_empty(),
            "valid programmed limits must pass: {:?}",
            validate_behavioral(&b)
        );
    }

    #[test]
    fn iin_program_rejects_nonfinite_prog_ref_ohms() {
        // R50: prog_ref_ohms was validated with only `<= 0.0`, so an `inf`
        // overflow typo passed. The engine's prog_ref.max(1.0)=inf then yields
        // v_sense = vprog_ref*prog/inf = 0, zeroing the input-current limit and
        // folding the regulated rail to 0 V for the whole run with no fault.
        let spec = |prog_ref_ohms: &str| {
            format!(
                r#"
[converter]
topology = "buck_boost"
out_pin = "bat"
in_pin = "pvin"
vout_setpoint = 14.4
efficiency = 0.92

[converter.iin_program]
rsense_ref = "R49"
prog_ref = "R8"
vprog_ref = 1.19
prog_ref_ohms = {prog_ref_ohms}
v_sense_full = 0.05
"#
            )
        };
        let b: Behavioral = toml::from_str(&spec("inf")).expect("parse");
        assert!(
            validate_behavioral(&b).iter().any(|e| e.contains("prog_ref_ohms")),
            "an inf prog_ref_ohms must be rejected: {:?}",
            validate_behavioral(&b)
        );
        // A finite positive value still validates clean.
        let b: Behavioral = toml::from_str(&spec("100000.0")).expect("parse");
        assert!(
            validate_behavioral(&b).is_empty(),
            "a finite positive prog_ref_ohms must pass: {:?}",
            validate_behavioral(&b)
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
        let errs = validate_behavioral(&b);
        assert!(errs.iter().any(|e| e.contains("sink")), "{errs:?}");
    }

    #[test]
    fn non_finite_converter_and_pin_values_are_rejected() {
        // R44: `nan`/`inf` are legal TOML floats and every `<= 0.0`/range compare is
        // false for them, so they slipped the gate — then a NaN vout_setpoint reaches
        // the engine's `v_cmd.clamp(0.0, vout_setpoint)` and PANICS (clamp with a NaN
        // max), and a NaN efficiency propagates a NaN input current. Reject up front.
        let nan_vout: Behavioral = toml::from_str(
            "[converter]\ntopology=\"buck\"\nout_pin=\"o\"\nin_pin=\"i\"\nvout_setpoint = nan\n",
        )
        .expect("parse");
        assert!(
            validate_behavioral(&nan_vout).iter().any(|e| e.contains("vout_setpoint")),
            "a NaN vout_setpoint must be rejected: {:?}",
            validate_behavioral(&nan_vout)
        );

        let nan_eff: Behavioral = toml::from_str(
            "[converter]\ntopology=\"buck\"\nout_pin=\"o\"\nin_pin=\"i\"\nvout_setpoint = 5.0\nefficiency = nan\n",
        )
        .expect("parse");
        assert!(
            validate_behavioral(&nan_eff).iter().any(|e| e.contains("efficiency")),
            "a NaN efficiency must be rejected: {:?}",
            validate_behavioral(&nan_eff)
        );

        let inf_pull: Behavioral = toml::from_str(
            "[pins.shphld]\npull_to = \"vsys\"\npull_ohms = inf\n",
        )
        .expect("parse");
        assert!(
            validate_behavioral(&inf_pull).iter().any(|e| e.contains("pull_ohms")),
            "an inf pull_ohms must be rejected: {:?}",
            validate_behavioral(&inf_pull)
        );
    }

    #[test]
    fn converter_current_limits_must_be_positive_finite() {
        // R46: a negative iout_limit_a (a sign typo) is treated as a real CC
        // threshold the output current always exceeds, folding v_cmd to 0 V — the
        // regulated rail reads 0 V for the whole run with no fault. A NaN limit
        // silently disables the CC loop. Both must be rejected, like vout_setpoint.
        let neg: Behavioral = toml::from_str(
            "[converter]\ntopology=\"buck\"\nout_pin=\"o\"\nin_pin=\"i\"\nvout_setpoint=5.0\niout_limit_a = -1.0\n",
        )
        .expect("parse");
        assert!(
            validate_behavioral(&neg).iter().any(|e| e.contains("iout_limit_a")),
            "a negative iout_limit_a must be rejected: {:?}",
            validate_behavioral(&neg)
        );
        let nan_iin: Behavioral = toml::from_str(
            "[converter]\ntopology=\"buck\"\nout_pin=\"o\"\nin_pin=\"i\"\nvout_setpoint=5.0\niin_limit_a = nan\n",
        )
        .expect("parse");
        assert!(
            validate_behavioral(&nan_iin).iter().any(|e| e.contains("iin_limit_a")),
            "a NaN iin_limit_a must be rejected: {:?}",
            validate_behavioral(&nan_iin)
        );
        // A valid positive limit still passes.
        let ok: Behavioral = toml::from_str(
            "[converter]\ntopology=\"buck\"\nout_pin=\"o\"\nin_pin=\"i\"\nvout_setpoint=5.0\niout_limit_a = 1.0\n",
        )
        .expect("parse");
        assert!(validate_behavioral(&ok).is_empty(), "a positive limit must pass: {:?}", validate_behavioral(&ok));
    }

    #[test]
    fn roundtrip_serialize() {
        let mut b = Behavioral::default();
        b.pins.insert(
            "shphld".into(),
            BehavioralPin {
                pull_to: Some("vsys".into()),
                pull_ohms: Some(100_000.0),
                ..Default::default()
            },
        );
        let s = toml::to_string(&b).unwrap();
        let back: Behavioral = toml::from_str(&s).unwrap();
        assert_eq!(b, back);
    }
}
