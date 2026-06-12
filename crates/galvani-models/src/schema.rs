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

    /// Absolute maximum ratings for the fault/stress monitor. Absent fields
    /// mean "no limit known"; the engine may derive defaults (e.g. resistor
    /// power from footprint size).
    #[serde(default, skip_serializing_if = "Ratings::is_empty")]
    pub ratings: Ratings,

    /// Boot strapping pins: the pins this part samples at reset to choose a
    /// boot/configuration mode, and the level each requires for normal boot.
    /// Only populated for MCU-kind entries whose reference manual documents
    /// strapping. Drives the strap-pin lint (`galvani-engine` boot checks).
    /// Empty for parts with no documented strapping (e.g. AVR).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub straps: Vec<StrapPin>,
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

/// Absolute maximum ratings, straight from the datasheet's table. The
/// stress monitor compares the live operating point against these and
/// raises faults (optionally destructive) when exceeded.
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

    /// Maximum junction temperature (C), for when self-heating lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_junction_temp_c: Option<f64>,
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
