//! TOML-serialisable schema for model database entries -- the on-disk shape of
//! the declarative model library.
//!
//! A model database file is a `[[models]]` array ([`DbFile`]); each
//! [`ModelEntry`] describes how to recognise a real KiCad component and what
//! simulation model to hand the solver for it. An entry carries a stable `id`
//! (used in diagnostics and cross-references), a [`ComponentKind`] that decides
//! which parameters the solver requires, the [`MatchRules`] that bind it to a
//! part, and the kind-specific [`Params`]. This is the extension point the SDK
//! story rests on: adding device physics means adding rows here, not writing
//! Rust. `serde` derives keep the Rust structs and the TOML in lockstep.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-models/schema.md.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use hauksbee_ir::evidence::{ModelSourceTier, ModelUncertainty, ModelValidation};

// ── Top-level file container ──────────────────────────────────────────────────

/// Contents of one `.toml` database file (the `[[models]]` array).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DbFile {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

// ── Model entry ───────────────────────────────────────────────────────────────

/// A single model entry in the database.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelEntry {
    /// Stable identifier used in diagnostics and as a cross-reference key.
    pub id: String,

    /// Component kind, drives which param fields are required by the solver.
    pub kind: ComponentKind,

    /// Human-readable description for reports.
    #[serde(default)]
    pub description: String,

    /// Match rules; at least one must be present per entry.
    #[serde(default)]
    pub r#match: MatchRules,

    /// Kind-specific simulation parameters.
    #[serde(default)]
    pub params: Params,

    /// Pad-number to role mapping.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,

    /// Absolute maximum ratings for the fault/stress monitor. Absent fields
    /// mean "no limit known"; the engine may derive defaults (e.g. resistor
    /// power from footprint size).
    #[serde(default, skip_serializing_if = "Ratings::is_empty")]
    pub ratings: Ratings,

    /// Boot strapping pins: the pins this part samples at reset to choose a
    /// boot/configuration mode, and the level each requires for normal boot.
    /// Only populated for MCU-kind entries whose reference manual documents
    /// strapping. Drives the strap-pin lint (`hauksbee-engine` boot checks).
    /// Empty for parts with no documented strapping (e.g. AVR).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub straps: Vec<StrapPin>,

    /// Optional declarative behavioural model (pins/pulls, FSM, averaged
    /// converter, expression laws) for power ICs the SPICE-level kinds cannot
    /// express. See [`crate::behavioral`].
    #[serde(
        default,
        skip_serializing_if = "crate::behavioral::Behavioral::is_empty"
    )]
    pub behavioral: crate::behavioral::Behavioral,

    /// Optional declarative digital-logic model (combinational expressions,
    /// clocked registers, tri-state groups) interpreted by the engine's
    /// generic logic evaluator, a digital part's behaviour is data, not a
    /// Rust match arm. See [`crate::logic_spec`].
    #[serde(default, skip_serializing_if = "crate::logic_spec::Logic::is_empty")]
    pub logic: crate::logic_spec::Logic,

    /// How an external resistor sets this part's regulated current or
    /// protection threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_program: Option<CurrentProgram>,
}

/// Where a model entry's numbers come from, on the source accuracy ladder:
/// the tier, whether the entry has been validated, and any declared numeric
/// uncertainty. Feeds the evidence spine's `ModelSource` records.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ModelSourceSpec {
    pub tier: ModelSourceTier,
    #[serde(default = "unvalidated")]
    pub validation: ModelValidation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncertainty: Vec<ModelUncertainty>,
}

fn unvalidated() -> ModelValidation {
    ModelValidation::Unvalidated
}

/// How an external resistor programs a regulated current or protection limit.
///
/// A charger's PROG pin makes the fitted resistor the source of truth for its
/// regulated-current phase. A load switch's ILIM/ISET resistor instead sets an
/// overload threshold. [`CurrentProgramSemantics`] keeps those statements from
/// being conflated; only the former can support steady-state rail attribution.
///
/// [`max_operating_current_a`](Self::max_operating_current_a) is deliberately
/// separate from [`Ratings::max_current_a`]. The former is the largest current
/// the manufacturer specifies the part can regulate in normal operation; the
/// latter remains a device-level analysis threshold (normally an absolute
/// maximum, or a deliberately documented lower operating ceiling). Conflating
/// the two turns a safety threshold into a promised operating point.
///
/// Cite the datasheet equation and operating limit next to each database entry:
/// neither is derivable from another field in the model.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CurrentProgram {
    /// The pin role (a value in [`ModelEntry::pins`]) the programming resistor
    /// sits on. The resistor runs from that pin to ground.
    pub pin: String,

    /// What the programmed value physically means. A regulated current is an
    /// operating state that may flow continuously and can therefore support a
    /// steady-state ampacity attribution. A protection limit is only an OCP or
    /// current-limit threshold; it must never be promoted into a fictional
    /// load without an independent load/current assertion.
    pub semantics: CurrentProgramSemantics,

    /// Model pin roles where the regulated current enters the component. These
    /// are required for `regulated_current`; explicit direction keeps cascaded
    /// stages from being counted twice and avoids guessing from role spelling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub current_in_roles: Vec<String>,

    /// Model pin roles where the regulated current leaves the component.
    /// Required for `regulated_current`; not needed for a protection threshold
    /// because that threshold is never attributed as steady load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub current_out_roles: Vec<String>,

    /// Highest current specified for ordinary programmed operation. This is a
    /// domain boundary, not by itself evidence that the silicon saturates at
    /// exactly this value when an undersized resistor asks for more.
    /// Required for `regulated_current`, because an inverse law alone is
    /// unbounded as the fitted resistance approaches zero. It may be omitted
    /// for `protection_limit` when the equation itself includes its physical
    /// full-scale bound. It is never inferred from a device-level rating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_operating_current_a: Option<f64>,

    /// Behavior when the published equation asks for more than the sourced
    /// normal-operating endpoint. The safe default is to abstain. Saturation
    /// must be explicit and cited by the model author.
    #[serde(default, skip_serializing_if = "AboveDomainBehavior::is_default")]
    pub above_domain: AboveDomainBehavior,

    /// Manufacturer programming equation, flattened into the TOML block so a
    /// model reads `equation = "inverse_resistance"` beside its constants.
    #[serde(flatten)]
    pub equation: CurrentProgramEquation,
}

/// Physical meaning of a current-programming equation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CurrentProgramSemantics {
    /// The device actively regulates this operating current (for example a
    /// linear charger's constant-current phase).
    RegulatedCurrent,
    /// The value is an overload, trip, or limiting threshold rather than proof
    /// of steady-state current through the board.
    ProtectionLimit,
}

/// What a current-program model may claim above its sourced operating domain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AboveDomainBehavior {
    /// The equation is outside its supported operating domain, so no precise
    /// current is returned.
    #[default]
    Abstain,
    /// The datasheet explicitly specifies saturation at the operating limit.
    Saturate,
}

impl AboveDomainBehavior {
    fn is_default(&self) -> bool {
        *self == Self::Abstain
    }
}

/// Supported current-programming equations.
///
/// The enum is intentionally data-driven: adding a genuinely different
/// datasheet shape extends this type and its validator once, rather than adding
/// part-number conditionals to the engine.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "equation", rename_all = "snake_case")]
pub enum CurrentProgramEquation {
    /// `I(A) = k_volts / R(ohms)`.
    InverseResistance {
        /// Programming constant in volts. For `I = 1000 / R`, this is 1000.
        k_volts: f64,
    },

    /// A continuous two-branch law: use `low_k_volts / R` while that result is
    /// at or below `transition_current_a`; above it, use
    /// `high_numerator_a / (R / resistance_scale_ohms + high_offset)`.
    ///
    /// Top Power's TP4054 Rev 2.1 is the motivating published equation, but the
    /// representation names the mathematics rather than the part.
    PiecewiseInverseResistance {
        low_k_volts: f64,
        transition_current_a: f64,
        high_numerator_a: f64,
        resistance_scale_ohms: f64,
        high_offset: f64,
    },

    /// A programming resistor sets a 0-to-full-scale control voltage, which in
    /// turn scales the voltage allowed across a separate current-sense shunt:
    ///
    /// `Vprogram = min(program_bias_a * Rprogram, program_full_scale_v)`
    ///
    /// `I = (Vprogram / program_full_scale_v) * sense_full_scale_v / Rsense`
    ///
    /// `sense_roles` names every model pin whose adjacent shunt participates in
    /// the limit. `sense_far_roles` gives the required opposite side of each
    /// shunt (`"ground"` or another model pin role), in the same order. The
    /// engine accepts exactly one shunt on each path and requires their nominal
    /// resistances to agree; choosing the smallest nearby resistor would
    /// manufacture a precise result from a mismatched/filter network.
    SenseScaledResistance {
        sense_roles: Vec<String>,
        sense_far_roles: Vec<String>,
        program_bias_a: f64,
        program_full_scale_v: f64,
        sense_full_scale_v: f64,
    },
}

impl CurrentProgram {
    fn apply_operating_domain(&self, equation_current_a: f64) -> Option<f64> {
        let Some(limit) = self
            .max_operating_current_a
            .filter(|limit| limit.is_finite() && *limit > 0.0)
        else {
            return Some(equation_current_a);
        };
        if equation_current_a <= limit {
            return Some(equation_current_a);
        }
        match self.above_domain {
            AboveDomainBehavior::Abstain => None,
            AboveDomainBehavior::Saturate => Some(limit),
        }
    }

    /// Evaluate the published resistor equation without applying the part's
    /// normal-operating ceiling. Returns `None` for a non-positive/non-finite
    /// resistance or if malformed data would produce a non-physical result.
    pub fn equation_current_a(&self, resistance_ohms: f64) -> Option<f64> {
        if !resistance_ohms.is_finite() || resistance_ohms <= 0.0 {
            return None;
        }

        let current_a = match &self.equation {
            CurrentProgramEquation::InverseResistance { k_volts } => *k_volts / resistance_ohms,
            CurrentProgramEquation::PiecewiseInverseResistance {
                low_k_volts,
                transition_current_a,
                high_numerator_a,
                resistance_scale_ohms,
                high_offset,
            } => {
                let low_current_a = *low_k_volts / resistance_ohms;
                if low_current_a <= *transition_current_a {
                    low_current_a
                } else {
                    *high_numerator_a / (resistance_ohms / *resistance_scale_ohms + *high_offset)
                }
            }
            CurrentProgramEquation::SenseScaledResistance { .. } => {
                // This law requires the independent current-sense resistance;
                // callers that have it use `equation_current_with_sense_a`.
                return None;
            }
        };

        (current_a.is_finite() && current_a > 0.0).then_some(current_a)
    }

    /// Evaluate either a one-resistor law or a two-resistor sense-scaled law.
    /// For one-resistor equations `sense_resistance_ohms` is intentionally
    /// ignored; for the sense-scaled form both positive finite resistances are
    /// required.
    pub fn equation_current_with_sense_a(
        &self,
        program_resistance_ohms: f64,
        sense_resistance_ohms: f64,
    ) -> Option<f64> {
        let CurrentProgramEquation::SenseScaledResistance {
            program_bias_a,
            program_full_scale_v,
            sense_full_scale_v,
            ..
        } = &self.equation
        else {
            return self.equation_current_a(program_resistance_ohms);
        };
        if !program_resistance_ohms.is_finite()
            || program_resistance_ohms <= 0.0
            || !sense_resistance_ohms.is_finite()
            || sense_resistance_ohms <= 0.0
        {
            return None;
        }
        let program_v = (*program_bias_a * program_resistance_ohms)
            .min(*program_full_scale_v)
            .max(0.0);
        let current_a =
            (program_v / *program_full_scale_v) * *sense_full_scale_v / sense_resistance_ohms;
        (current_a.is_finite() && current_a > 0.0).then_some(current_a)
    }

    /// Evaluate the board's nominal programmed current, applying only the
    /// explicit normal-operating ceiling from this block—not a device rating.
    /// Datasheet/resistor tolerances are not implied by a point equation.
    pub fn operating_current_a(&self, resistance_ohms: f64) -> Option<f64> {
        let equation_current_a = self.equation_current_a(resistance_ohms)?;
        self.apply_operating_domain(equation_current_a)
    }

    /// Sense-aware counterpart to [`Self::operating_current_a`].
    pub fn operating_current_with_sense_a(
        &self,
        program_resistance_ohms: f64,
        sense_resistance_ohms: f64,
    ) -> Option<f64> {
        let equation_current_a =
            self.equation_current_with_sense_a(program_resistance_ohms, sense_resistance_ohms)?;
        self.apply_operating_domain(equation_current_a)
    }
}

/// One boot strapping pin, straight from the part's reference manual.
///
/// The `role` matches a value in [`ModelEntry::pins`] (the pad->role map), so
/// the lint can find which pad/net carries the strap. `level` is what the pin
/// must read at the reset latch window for *normal* boot. Cite the RM in the
/// TOML comment next to each entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StrapPin {
    /// Pin role, matching an entry in [`ModelEntry::pins`] (e.g. "gpio0").
    pub role: String,
    /// Required level at the reset latch for normal boot.
    pub level: StrapLevel,
    /// True only for a *boot-select* strap whose wrong static level is
    /// unrecoverable: it latches which boot source / mode the part enters, and
    /// firmware cannot undo it (BOOT0, GPIO0/GPIO9 boot pin, QSPI_SS/BOOTSEL).
    /// The wrong-bias lint arm fires only on these. Cosmetic or flash-voltage
    /// straps that a board may legitimately repurpose as ordinary GPIO with a
    /// pull (ESP32 GPIO15 boot-log, GPIO2, GPIO12/GPIO45 flash-voltage) leave
    /// this false, so the lint never asserts a wrong-level fault on a board that
    /// merely reuses the pin.
    #[serde(default)]
    pub boot_select: bool,
    /// Free-text sampling semantics / what the strap selects (for the finding
    /// message). E.g. "SPI boot when high; download mode when low".
    #[serde(default)]
    pub note: String,
    /// Whether the *silicon* provides an internal pull on this strap pin. This is
    /// what decides whether a *floating* strap net is a fault: ESP32 strapping
    /// pins each carry a documented internal pull, so an undriven net settles to a
    /// defined level and is fine, but an STM32 BOOT0 has **no** internal pull, so
    /// a floating BOOT0 leaves the boot source genuinely undefined and the part
    /// can enter the bootloader instead of the application. The lint's
    /// floating-strap arm fires only when this is [`StrapInternalPull::None`].
    /// Default [`StrapInternalPull::Unknown`] preserves the prior, conservative
    /// behaviour (never fire on a floating net).
    #[serde(default)]
    pub internal_pull: StrapInternalPull,
}

/// The internal (on-die) pull a strap pin has, which determines whether leaving
/// its net undriven is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrapInternalPull {
    /// No internal pull: a floating net is undefined at reset (STM32 BOOT0).
    None,
    /// An internal pull-down holds an undriven net low.
    PullDown,
    /// An internal pull-up holds an undriven net high.
    PullUp,
    /// Unknown / unspecified: be conservative and never treat floating as a fault.
    #[default]
    Unknown,
}

/// The level a strap pin must hold at reset for normal boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrapLevel {
    /// Must be high (pulled/driven to the I/O rail) at the latch window.
    High,
    /// Must be low (pulled/driven to ground) at the latch window.
    Low,
    /// A defined static level is required but either polarity is acceptable
    /// for *this* lint's purpose (the fault we catch is a free-running driver
    /// or no bias at all, not the wrong static polarity). Used where the
    /// correct polarity depends on a co-strap (e.g. flash voltage select).
    Defined,
}

impl StrapLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            StrapLevel::High => "high",
            StrapLevel::Low => "low",
            StrapLevel::Defined => "a defined level",
        }
    }
}

/// Datasheet safety thresholds used by static and live checks.
///
/// Most entries are absolute maxima. Some model files deliberately use a
/// lower recommended-operating ceiling so a check fires before the part leaves
/// its guaranteed region; those entries must say so beside the value. These
/// are analysis thresholds, never proof that a board normally draws or drives
/// the stated amount.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Ratings {
    /// Continuous current through the device (A): diode IF, transistor IC/ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_current_a: Option<f64>,

    /// Non-repetitive surge current (A), checked against short spikes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_surge_current_a: Option<f64>,

    /// Total power dissipation (W).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_power_w: Option<f64>,

    /// Maximum blocking/working voltage (V): diode VRRM, cap rated voltage,
    /// transistor VCEO/VDS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_voltage_v: Option<f64>,

    /// Polarized part (electrolytic/tantalum cap): reverse bias beyond
    /// about -0.5V is a fault.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub polarized: bool,

    /// Per-pin source/sink limit for ICs and MCUs (A).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pin_current_a: Option<f64>,

    /// Rated RMS ripple current (A) for a capacitor, at its datasheet reference
    /// frequency / temperature. Drives the input-cap ripple-current check
    /// (`hauksbee_engine::checks::ripple`). Only this part-specific value is
    /// decision-grade; when absent, any capacitance-band estimate is context in
    /// an info note and cannot support pass/fail (e.g. the UCC EKYB 3.0 A at
    /// 100 kHz / 105 C belongs here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ripple_current_a: Option<f64>,

    /// Maximum junction temperature (C). When absent the thermal monitor
    /// applies a per-class default (125 C for discretes / passives, 150 C for
    /// power packs); set it here to override from the datasheet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_junction_temp_c: Option<f64>,

    /// Junction-to-ambient thermal resistance (C/W), still-air, no heatsink.
    /// Drives the steady-state junction-temperature estimate
    /// `Tj = Tambient + P * theta_JA`. When absent the thermal monitor derives
    /// a default from the footprint package class (see `hauksbee_engine::thermal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theta_ja_c_per_w: Option<f64>,

    /// Junction-to-case thermal resistance (C/W). Informational / for a future
    /// heatsinked path; the free-air estimate uses `theta_ja_c_per_w`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theta_jc_c_per_w: Option<f64>,
}

impl Ratings {
    pub fn is_empty(&self) -> bool {
        *self == Ratings::default()
    }
}

// ── Component kind ────────────────────────────────────────────────────────────

/// Classification of a component for the solver and extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    /// Passive element: resistor, capacitor, inductor. Value is parsed from
    /// the component `Value` field by the engineering-notation parser.
    Passive,
    /// PN junction diode.
    Diode,
    /// NPN bipolar transistor.
    BjtNpn,
    /// PNP bipolar transistor.
    BjtPnp,
    /// N-channel MOSFET.
    Nmos,
    /// P-channel MOSFET.
    Pmos,
    /// Linear or switching voltage regulator.
    Vreg,
    /// Operational amplifier (behavioral).
    Opamp,
    /// Voltage comparator (behavioral).
    Comparator,
    /// Single-pole analog switch / transmission gate.
    AnalogSwitch,
    /// Generic digital behavioral block.
    Digital,
    /// Digital-to-analogue converter.
    Dac,
    /// Analogue-to-digital converter.
    Adc,
    /// Shift register (serial in/out or parallel in/out).
    ShiftRegister,
    /// Microcontroller unit, hands off to hauksbee-mcu backend.
    Mcu,
    /// Connector: models pin continuity only.
    Connector,
    /// Mounting hole, logo, test point, fiducial, silently ignored.
    Ignore,
}

impl ComponentKind {
    /// Whether this kind carries SPICE-level analog params.
    pub fn is_analog(self) -> bool {
        matches!(
            self,
            ComponentKind::Passive
                | ComponentKind::Diode
                | ComponentKind::BjtNpn
                | ComponentKind::BjtPnp
                | ComponentKind::Nmos
                | ComponentKind::Pmos
        )
    }

    /// Whether this kind is event-driven / behavioral.
    pub fn is_behavioral(self) -> bool {
        matches!(
            self,
            ComponentKind::Opamp
                | ComponentKind::Comparator
                | ComponentKind::AnalogSwitch
                | ComponentKind::Digital
                | ComponentKind::Dac
                | ComponentKind::Adc
                | ComponentKind::ShiftRegister
        )
    }
}

// ── Match rules ───────────────────────────────────────────────────────────────

/// Rules that determine whether a model entry matches a component.
///
/// All populated rules are ANDed: the entry matches only when every
/// non-`None` rule fires. At least one rule must be populated.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MatchRules {
    /// Exact KiCad lib_id string (e.g. `"Device:R"`) or prefix ending with
    /// `":"` (e.g. `"Device:"` matches any lib_id in the Device library).
    #[serde(default)]
    pub lib_id: Option<String>,

    /// Regex matched against the component `Value` field (case-insensitive).
    #[serde(default)]
    pub value_re: Option<String>,

    /// Regex matched against the `Footprint` field.
    #[serde(default)]
    pub footprint_re: Option<String>,

    /// Regex matched against a manufacturer part-number property (if present).
    #[serde(default)]
    pub mpn_re: Option<String>,
}

impl MatchRules {
    /// True when at least one rule field is populated.
    pub fn is_empty(&self) -> bool {
        self.lib_id.is_none()
            && self.value_re.is_none()
            && self.footprint_re.is_none()
            && self.mpn_re.is_none()
    }
}

// ── Params ────────────────────────────────────────────────────────────────────

/// Free-form key/value parameter bag.
///
/// We use a `BTreeMap<String, ParamValue>` so the TOML can carry any numeric
/// or string param without a fixed schema. The solver reads params by name.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Params(pub BTreeMap<String, ParamValue>);

impl Params {
    /// Retrieve a floating-point param by name.
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.0.get(key)?.as_f64()
    }

    /// Retrieve a string param by name.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.as_str()
    }

    /// Retrieve a boolean param by name. `None` when the key is absent or its
    /// value is not a bool, so `flag = false` is distinguishable from omitted.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.0.get(key)?.as_bool()
    }

    /// Insert a float param.
    pub fn set_f64(&mut self, key: impl Into<String>, v: f64) {
        self.0.insert(key.into(), ParamValue::Float(v));
    }

    /// Insert a string param.
    pub fn set_str(&mut self, key: impl Into<String>, v: impl Into<String>) {
        self.0.insert(key.into(), ParamValue::String(v.into()));
    }

    /// Insert an integer param.
    pub fn set_int(&mut self, key: impl Into<String>, v: i64) {
        self.0.insert(key.into(), ParamValue::Int(v));
    }

    /// True when no params are present.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A parameter value, either a float, integer, or string.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ParamValue {
    Float(f64),
    Int(i64),
    Bool(bool),
    String(String),
}

impl ParamValue {
    /// Return as `f64`, converting integers.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ParamValue::Float(v) => Some(*v),
            ParamValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// Return as `&str` for string values.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ParamValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Return as `bool` for boolean values. `None` for non-bool params, so a
    /// caller can distinguish `flag = false` from an omitted/non-bool `flag`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ParamValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AboveDomainBehavior, CurrentProgram, Params};

    #[test]
    fn get_bool_distinguishes_false_from_absent() {
        // R24: the MCU `module` flag was presence-checked, so `module = false`
        // wrongly activated module (Arduino-header) mapping. get_bool must return
        // Some(false) for an explicit false and None for an absent/non-bool key.
        let p: Params = toml::from_str("module = false\nother = 3\nname = \"x\"").unwrap();
        assert_eq!(p.get_bool("module"), Some(false));
        assert_eq!(p.get_bool("missing"), None);
        // A non-bool value is not coerced to a bool.
        assert_eq!(p.get_bool("other"), None);
        assert_eq!(p.get_bool("name"), None);

        let t: Params = toml::from_str("module = true").unwrap();
        assert_eq!(t.get_bool("module"), Some(true));
    }

    #[test]
    fn current_program_saturates_only_when_the_model_explicitly_says_so() {
        let base = r#"
pin = "prog"
semantics = "regulated_current"
current_in_roles = ["in"]
current_out_roles = ["out"]
max_operating_current_a = 0.4
equation = "inverse_resistance"
k_volts = 1000.0
"#;
        let abstaining: CurrentProgram = toml::from_str(base).unwrap();
        assert_eq!(abstaining.above_domain, AboveDomainBehavior::Abstain);
        assert_eq!(abstaining.operating_current_a(100.0), None);

        let saturating: CurrentProgram =
            toml::from_str(&format!("{base}\nabove_domain = \"saturate\"\n")).unwrap();
        assert_eq!(saturating.above_domain, AboveDomainBehavior::Saturate);
        assert_eq!(saturating.operating_current_a(100.0), Some(0.4));
    }
}
