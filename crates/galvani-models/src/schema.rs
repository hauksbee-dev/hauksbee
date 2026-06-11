//! TOML-serialisable schema for model database entries.
//!
//! Each entry describes how to match a KiCad component and what simulation
//! model to produce for it.

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

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

    /// Component kind — drives which param fields are required by the solver.
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
    /// Microcontroller unit — hands off to galvani-mcu backend.
    Mcu,
    /// Connector: models pin continuity only.
    Connector,
    /// Mounting hole, logo, test point, fiducial — silently ignored.
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

/// A parameter value — either a float, integer, or string.
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
}
