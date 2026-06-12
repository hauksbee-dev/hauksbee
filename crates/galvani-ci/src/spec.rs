//! The `galvani-ci` spec: a TOML file a hardware repo checks in, describing one
//! headless co-simulation and the assertions that must hold for the build to
//! pass. Designed to be pleasant to hand-write.
//!
//! ```toml
//! name = "power-up sanity"
//! board = "hardware/board.kicad_pcb"        # .kicad_pcb / .kicad_sch / .net / .brd / .d356
//! firmware = "firmware/build/app.elf"        # optional ELF/hex
//! mcu = "atmega328p"                          # optional MCU kind hint
//! duration_ms = 200                           # simulated time
//!
//! [[supply]]                                  # 0+ power-supply legs
//! net = "+5V"
//! kind = "bench"                              # ideal|bench|wall|usb|battery
//! volts = 5.0
//! current_limit_a = 1.0
//!
//! [[net_drive]]                               # 0+ forced net voltages
//! net = "WSEL"
//! volts = 5.0
//!
//! suppress_rail = ["ANALOG_VDD"]              # feed these nets through the
//!                                             # board only (no auto rail)
//!
//! [[override]]                                # 0+ component value overrides
//! ref = "R_Shunt15301"
//! value = "0.05"
//!
//! [fuzz]                                      # optional initial-state fuzzing
//! seeds = 16
//!
//! [[assert]]                                  # 1+ assertions
//! kind = "voltage"
//! net = "ANALOG_VDD"
//! min = 4.9
//! after_ms = 50
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{near_matches, SpecError};

/// A fully-parsed, validated spec.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Human-readable name for the check (appears in reports).
    #[serde(default = "default_name")]
    pub name: String,
    /// Path to the board file, relative to the spec file's directory. A
    /// `.kicad_sch` (schematic-stage CI) is loaded by path so its sheet
    /// hierarchy resolves; `.kicad_pcb` / `.net` / `.brd` / `.d356` are sniffed
    /// from content.
    pub board: PathBuf,
    /// Optional firmware ELF/hex, relative to the spec file's directory.
    #[serde(default)]
    pub firmware: Option<PathBuf>,
    /// Optional MCU-kind hint (informational; the binder detects the MCU).
    #[serde(default)]
    pub mcu: Option<String>,
    /// Simulated duration in milliseconds.
    #[serde(default = "default_duration_ms")]
    pub duration_ms: f64,
    /// Co-sim frame cadence in milliseconds (how often nets are sampled).
    #[serde(default = "default_frame_ms")]
    pub frame_ms: f64,
    /// Power-supply legs to attach to named supply nets.
    #[serde(default, rename = "supply")]
    pub supplies: Vec<SupplySpec>,
    /// Nets forced to a fixed voltage for the whole run (external stimulus,
    /// register bits, strapping). Stamped as an ideal source.
    #[serde(default, rename = "net_drive")]
    pub net_drives: Vec<NetDrive>,
    /// Peripherals attached to the board for this run (buttons, pots, encoders,
    /// stimulus, I2C/SPI slaves, VCD sinks), each with type-specific config and
    /// an optional timeline of press/release/set events.
    #[serde(default, rename = "peripheral")]
    pub peripherals: Vec<PeripheralSpec>,
    /// Supply nets whose binder auto-rail is removed, so they are fed only
    /// through on-board components (e.g. a rail behind a sense shunt).
    #[serde(default)]
    pub suppress_rail: Vec<String>,
    /// Component value overrides applied before binding (e.g. swap a wrong
    /// resistor value for the documented repair).
    #[serde(default, rename = "override")]
    pub overrides: Vec<Override>,
    /// Initial-state fuzzing: run the sim under several random register/
    /// undefined-state seeds. An assertion must hold across *all* seeds.
    #[serde(default)]
    pub fuzz: Option<FuzzSpec>,
    /// The assertions, all of which must pass.
    #[serde(default, rename = "assert")]
    pub asserts: Vec<Assertion>,

    /// Directory the spec was loaded from (for resolving relative paths). Not
    /// part of the TOML; filled in by [`Spec::load`].
    #[serde(skip)]
    pub base_dir: PathBuf,
}

fn default_name() -> String {
    "galvani-ci".to_string()
}
fn default_duration_ms() -> f64 {
    100.0
}
fn default_frame_ms() -> f64 {
    1.0
}

/// A power-supply leg attached to a supply net. Mirrors the engine's
/// behavioral supplies (bench / wall / USB / battery / ideal).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplySpec {
    pub net: String,
    /// One of `ideal | bench | wall | usb | battery`.
    pub kind: String,
    #[serde(default)]
    pub volts: Option<f64>,
    #[serde(default)]
    pub current_limit_a: Option<f64>,
    #[serde(default)]
    pub r_out_ohms: Option<f64>,
    #[serde(default)]
    pub ripple_vpp: Option<f64>,
    #[serde(default)]
    pub ripple_hz: Option<f64>,
    /// USB profile: `5v0.5a | 5v1.5a | 5v3a`.
    #[serde(default)]
    pub usb: Option<String>,
    /// Battery chemistry: `liion | alkaline | nimh | lifepo4`.
    #[serde(default)]
    pub chemistry: Option<String>,
    #[serde(default)]
    pub cells: Option<u32>,
    #[serde(default)]
    pub capacity_mah: Option<f64>,
    #[serde(default)]
    pub soc: Option<f64>,
    #[serde(default)]
    pub r_internal_ohms: Option<f64>,
}

/// A net forced to a fixed DC voltage for the run.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetDrive {
    pub net: String,
    pub volts: f64,
}

/// A peripheral attached to the board for one run.
///
/// ```toml
/// [[peripheral]]
/// id = "BTN1"
/// type = "pushbutton"        # pushbutton|toggle|potentiometer|encoder|
///                            # stimulus|i2c_eeprom|i2c_lm75|spi_eeprom|
///                            # spi_mcp3008|vcd_sink
/// net = "BUTTON"             # attach by net name (or use ref+pin)
/// to = "GND"                 # button/toggle: other terminal (default GND)
/// bounce_ms = 5.0            # optional contact-bounce model
/// [[peripheral.event]]       # timeline: press at 100ms, release at 150ms
/// t_ms = 100
/// value = 1
/// [[peripheral.event]]
/// t_ms = 150
/// value = 0
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeripheralSpec {
    /// Stable id, used by events / live control / state assertions.
    pub id: String,
    /// Peripheral type.
    #[serde(rename = "type")]
    pub kind: String,

    // Attachment: by net name, or by connector ref + pin (resolved to a net).
    #[serde(default)]
    pub net: Option<String>,
    /// Connector reference designator (e.g. "J1") for ref+pin attachment.
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// Pin/pad number on the connector for ref+pin attachment.
    #[serde(default)]
    pub pin: Option<String>,
    /// Second terminal net for a button / toggle / two-terminal control.
    #[serde(default)]
    pub to: Option<String>,
    /// Potentiometer terminals: a, wiper, b nets (net=wiper if omitted).
    #[serde(default)]
    pub a: Option<String>,
    #[serde(default)]
    pub wiper: Option<String>,
    #[serde(default)]
    pub b: Option<String>,
    /// Encoder quadrature output nets.
    #[serde(default)]
    pub net_a: Option<String>,
    #[serde(default)]
    pub net_b: Option<String>,

    // Type-specific config.
    /// Contact-bounce duration (ms) for a pushbutton; 0/absent disables it.
    #[serde(default)]
    pub bounce_ms: Option<f64>,
    /// Initial state: button pressed / toggle closed / control position.
    #[serde(default)]
    pub initial: Option<f64>,
    /// Total track resistance for a potentiometer (ohms).
    #[serde(default)]
    pub r_total: Option<f64>,
    /// High-level voltage for an encoder / digital control output.
    #[serde(default)]
    pub vhigh: Option<f64>,
    /// I2C 7-bit address (eeprom / sensor).
    #[serde(default)]
    pub address: Option<u8>,
    /// EEPROM size in bytes.
    #[serde(default)]
    pub size: Option<usize>,
    /// Sensor temperature in Celsius (i2c_lm75).
    #[serde(default)]
    pub temp_c: Option<f64>,
    /// ADC reference voltage (spi_mcp3008).
    #[serde(default)]
    pub vref: Option<f64>,
    /// Stimulus waveform: "dc"|"sine"|"pwl"|"noise".
    #[serde(default)]
    pub waveform: Option<String>,
    #[serde(default)]
    pub offset: Option<f64>,
    #[serde(default)]
    pub amplitude: Option<f64>,
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// PWL points as `[[t_ms, value], ...]`.
    #[serde(default)]
    pub pwl: Option<Vec<[f64; 2]>>,
    /// Nets a vcd_sink should log.
    #[serde(default)]
    pub nets: Option<Vec<String>>,
    /// Path to write the VCD file (relative to the spec dir).
    #[serde(default)]
    pub vcd_path: Option<String>,

    /// Timeline of events applied during the run.
    #[serde(default, rename = "event")]
    pub events: Vec<TimelineEventSpec>,
}

/// One scheduled peripheral event: at `t_ms`, set the peripheral to `value`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineEventSpec {
    pub t_ms: f64,
    pub value: f64,
}

impl PeripheralSpec {
    fn validate(&self) -> Result<(), SpecError> {
        const KINDS: &[&str] = &[
            "pushbutton",
            "toggle",
            "potentiometer",
            "encoder",
            "stimulus",
            "i2c_eeprom",
            "i2c_lm75",
            "spi_eeprom",
            "spi_mcp3008",
            "vcd_sink",
        ];
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(SpecError::Invalid(format!(
                "peripheral '{}': unknown type '{}' (expected one of {})",
                self.id,
                self.kind,
                KINDS.join("|")
            )));
        }
        // Net-attached controls and sinks need an attachment.
        let needs_net = matches!(
            self.kind.as_str(),
            "pushbutton" | "toggle" | "potentiometer" | "encoder" | "stimulus" | "vcd_sink"
        );
        if needs_net
            && self.net.is_none()
            && self.reference.is_none()
            && self.nets.is_none()
            && self.net_a.is_none()
        {
            return Err(SpecError::Invalid(format!(
                "peripheral '{}' ({}) needs a `net`, a `ref`+`pin`, or `nets`",
                self.id, self.kind
            )));
        }
        Ok(())
    }
}

/// A component value override applied before binding.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(rename = "ref")]
    pub reference: String,
    pub value: String,
}

/// Initial-state fuzzing configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FuzzSpec {
    /// Number of random seeds to run (each perturbs undefined initial states).
    pub seeds: u32,
    /// Nets whose initial logic state is randomized per seed. When empty, the
    /// fuzzer randomizes every net listed in any `net_drive` (treating those
    /// drives as the undefined power-up bits).
    #[serde(default)]
    pub nets: Vec<String>,
    /// The two voltage levels a fuzzed net is strapped between (default 0/5 V).
    #[serde(default)]
    pub levels: Option<[f64; 2]>,
}

/// One assertion over the run.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    /// `voltage | uart | toggle | no_faults | max_current`.
    pub kind: String,
    /// Optional label (defaults to a generated description).
    #[serde(default)]
    pub name: Option<String>,

    // voltage / toggle / max_current target net.
    #[serde(default)]
    pub net: Option<String>,
    // voltage bounds (at least one of min/max).
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Only sample at/after this time (ms) — lets the rail settle first.
    #[serde(default)]
    pub after_ms: Option<f64>,

    // uart.
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub matches: Option<String>,
    /// Which MCU's UART (by reference). Defaults to all MCUs concatenated.
    #[serde(default)]
    pub mcu: Option<String>,

    // toggle (blink): expected toggle frequency in Hz, with tolerance.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    #[serde(default)]
    pub tolerance: Option<f64>,
    /// Minimum toggle count over the run (alternative to freq_hz).
    #[serde(default)]
    pub min_toggles: Option<u64>,

    // max_current: ceiling in amps for the component named by `ref`.
    #[serde(rename = "ref", default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub amps: Option<f64>,

    // peripheral: reference a peripheral by `id`.
    #[serde(default)]
    pub id: Option<String>,
    /// EEPROM byte sequence (hex string, e.g. "48 69" or "4869") that must
    /// appear in the peripheral's memory.
    #[serde(default)]
    pub bytes: Option<String>,
    /// A peripheral state field that must lie in [min, max] (e.g. "transitions"
    /// on a vcd_sink, "temp_c" on a sensor). Uses the assertion's min/max.
    #[serde(default)]
    pub field: Option<String>,

    // boot-coverage: a control net (gate / enable / reset / CS) that must reach
    // and hold a defined level (`min`, in volts) within `deadline_ms` of reset,
    // with no stress fault raised during the boot window before it does. On a
    // board with no static bias on the net (a genuinely Hi-Z control input, the
    // case this is for) the only thing that can bring it to level is the
    // firmware, so this measures "the firmware drives it in time". If the board
    // statically biases the net it reads at level from t=0 and trivially passes:
    // such a board is out of scope, the assertion exists to adjudicate the
    // undefined-default case the netlist cannot.
    #[serde(default)]
    pub deadline_ms: Option<f64>,
}

impl Spec {
    /// Load and validate a spec from a TOML file.
    pub fn load(path: &Path) -> Result<Self, SpecError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SpecError::Io(format!("reading {}: {e}", path.display())))?;
        let mut spec: Spec = toml::from_str(&text).map_err(|e| SpecError::Toml {
            file: path.display().to_string(),
            message: e.message().to_string(),
        })?;
        spec.base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        spec.validate()?;
        Ok(spec)
    }

    /// The board file path, resolved against the spec's directory.
    pub fn board_path(&self) -> PathBuf {
        self.resolve(&self.board)
    }

    /// The firmware path, resolved against the spec's directory.
    pub fn firmware_path(&self) -> Option<PathBuf> {
        self.firmware.as_ref().map(|f| self.resolve(f))
    }

    fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base_dir.join(p)
        }
    }

    /// Structural validation independent of the board (fast, no extraction).
    /// Net-name validation happens later in the runner once the board is bound.
    fn validate(&self) -> Result<(), SpecError> {
        if self.asserts.is_empty() {
            return Err(SpecError::Invalid(
                "spec has no [[assert]] blocks: a check with no assertions always passes vacuously"
                    .into(),
            ));
        }
        if self.duration_ms <= 0.0 {
            return Err(SpecError::Invalid("duration_ms must be positive".into()));
        }
        for s in &self.supplies {
            s.validate()?;
        }
        for p in &self.peripherals {
            p.validate()?;
        }
        for a in &self.asserts {
            a.validate()?;
        }
        if let Some(f) = &self.fuzz {
            if f.seeds == 0 {
                return Err(SpecError::Invalid("[fuzz] seeds must be >= 1".into()));
            }
        }
        Ok(())
    }

    /// Every net name the spec references, for board-aware validation.
    pub fn referenced_nets(&self) -> Vec<(String, &'static str)> {
        let mut out = Vec::new();
        for s in &self.supplies {
            out.push((s.net.clone(), "supply"));
        }
        for d in &self.net_drives {
            out.push((d.net.clone(), "net_drive"));
        }
        for p in &self.peripherals {
            // Validate explicit net references (ref+pin is checked at attach).
            for n in [&p.net, &p.to, &p.a, &p.wiper, &p.b, &p.net_a, &p.net_b]
                .into_iter()
                .flatten()
            {
                out.push((n.clone(), "peripheral"));
            }
            if let Some(nets) = &p.nets {
                for n in nets {
                    out.push((n.clone(), "peripheral"));
                }
            }
        }
        for n in &self.suppress_rail {
            out.push((n.clone(), "suppress_rail"));
        }
        if let Some(f) = &self.fuzz {
            for n in &f.nets {
                out.push((n.clone(), "fuzz"));
            }
        }
        for a in &self.asserts {
            if let Some(n) = &a.net {
                out.push((n.clone(), "assert"));
            }
        }
        out
    }

    /// Validate that every referenced net exists on the bound board; produce a
    /// helpful error (with near-matches) for any that do not.
    pub fn check_nets(&self, known: &[String]) -> Result<(), SpecError> {
        let set: HashMap<&str, ()> = known.iter().map(|n| (n.as_str(), ())).collect();
        let mut unknown = Vec::new();
        for (net, ctx) in self.referenced_nets() {
            if !set.contains_key(net.as_str()) {
                let suggestions = near_matches(&net, known, 5);
                unknown.push((net, ctx, suggestions));
            }
        }
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(SpecError::UnknownNets(unknown))
        }
    }
}

impl SupplySpec {
    fn validate(&self) -> Result<(), SpecError> {
        match self.kind.as_str() {
            "ideal" | "bench" | "wall" | "usb" | "battery" => Ok(()),
            other => Err(SpecError::Invalid(format!(
                "supply on net '{}': unknown kind '{}' (expected ideal|bench|wall|usb|battery)",
                self.net, other
            ))),
        }
    }
}

impl Assertion {
    fn validate(&self) -> Result<(), SpecError> {
        match self.kind.as_str() {
            "voltage" => {
                if self.net.is_none() {
                    return Err(SpecError::Invalid(
                        "voltage assertion needs a `net`".into(),
                    ));
                }
                if self.min.is_none() && self.max.is_none() {
                    return Err(SpecError::Invalid(format!(
                        "voltage assertion on '{}' needs a `min` and/or `max`",
                        self.net.as_deref().unwrap_or("?")
                    )));
                }
            }
            "uart" => {
                if self.contains.is_none() && self.matches.is_none() {
                    return Err(SpecError::Invalid(
                        "uart assertion needs `contains` or `matches`".into(),
                    ));
                }
                if let Some(re) = &self.matches {
                    regex::Regex::new(re).map_err(|e| {
                        SpecError::Invalid(format!("uart `matches` is not a valid regex: {e}"))
                    })?;
                }
            }
            "toggle" => {
                if self.net.is_none() {
                    return Err(SpecError::Invalid("toggle assertion needs a `net`".into()));
                }
                if self.freq_hz.is_none() && self.min_toggles.is_none() {
                    return Err(SpecError::Invalid(format!(
                        "toggle assertion on '{}' needs `freq_hz` or `min_toggles`",
                        self.net.as_deref().unwrap_or("?")
                    )));
                }
                if self.after_ms.is_some() {
                    // Toggle counts accumulate from t=0 (scheduler stats), so an
                    // `after_ms` window would be silently ignored. Reject it
                    // rather than mislead.
                    return Err(SpecError::Invalid(format!(
                        "toggle assertion on '{}' does not support `after_ms` (toggles are counted over the whole run)",
                        self.net.as_deref().unwrap_or("?")
                    )));
                }
            }
            "no_faults" => {}
            "max_current" => {
                if self.reference.is_none() || self.amps.is_none() {
                    return Err(SpecError::Invalid(
                        "max_current assertion needs `ref` and `amps`".into(),
                    ));
                }
            }
            "peripheral" => {
                if self.id.is_none() {
                    return Err(SpecError::Invalid(
                        "peripheral assertion needs an `id`".into(),
                    ));
                }
                let has_check =
                    self.bytes.is_some() || (self.field.is_some() && (self.min.is_some() || self.max.is_some()));
                if !has_check {
                    return Err(SpecError::Invalid(format!(
                        "peripheral assertion on '{}' needs `bytes` or a `field` with `min`/`max`",
                        self.id.as_deref().unwrap_or("?")
                    )));
                }
            }
            "boot-coverage" => {
                if self.net.is_none() {
                    return Err(SpecError::Invalid(
                        "boot-coverage assertion needs a `net` (the control net to watch)".into(),
                    ));
                }
                if self.min.is_none() {
                    return Err(SpecError::Invalid(format!(
                        "boot-coverage assertion on '{}' needs a `min` (the driven level in volts the firmware must reach)",
                        self.net.as_deref().unwrap_or("?")
                    )));
                }
                if self.deadline_ms.is_none() {
                    return Err(SpecError::Invalid(format!(
                        "boot-coverage assertion on '{}' needs a `deadline_ms` (the boot deadline)",
                        self.net.as_deref().unwrap_or("?")
                    )));
                }
            }
            other => {
                return Err(SpecError::Invalid(format!(
                    "unknown assertion kind '{other}' (expected voltage|uart|toggle|no_faults|max_current|peripheral|boot-coverage)"
                )));
            }
        }
        Ok(())
    }

    /// A human label for this assertion.
    pub fn label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match self.kind.as_str() {
            "voltage" => {
                let net = self.net.clone().unwrap_or_default();
                let bound = match (self.min, self.max) {
                    (Some(lo), Some(hi)) => format!("in [{lo}, {hi}] V"),
                    (Some(lo), None) => format!(">= {lo} V"),
                    (None, Some(hi)) => format!("<= {hi} V"),
                    (None, None) => "voltage".into(),
                };
                let when = self
                    .after_ms
                    .map(|t| format!(" after {t}ms"))
                    .unwrap_or_default();
                format!("{net} {bound}{when}")
            }
            "uart" => {
                if let Some(c) = &self.contains {
                    format!("UART contains {c:?}")
                } else if let Some(m) = &self.matches {
                    format!("UART matches /{m}/")
                } else {
                    "UART".into()
                }
            }
            "toggle" => {
                let net = self.net.clone().unwrap_or_default();
                if let Some(f) = self.freq_hz {
                    format!("{net} toggles at ~{f} Hz")
                } else {
                    format!("{net} toggles >= {} times", self.min_toggles.unwrap_or(0))
                }
            }
            "no_faults" => "no stress faults raised".into(),
            "max_current" => format!(
                "I({}) <= {} A",
                self.reference.clone().unwrap_or_default(),
                self.amps.unwrap_or(0.0)
            ),
            "peripheral" => {
                let id = self.id.clone().unwrap_or_default();
                if let Some(b) = &self.bytes {
                    format!("peripheral {id} contains bytes {b}")
                } else if let Some(f) = &self.field {
                    let bound = match (self.min, self.max) {
                        (Some(lo), Some(hi)) => format!("in [{lo}, {hi}]"),
                        (Some(lo), None) => format!(">= {lo}"),
                        (None, Some(hi)) => format!("<= {hi}"),
                        (None, None) => "set".into(),
                    };
                    format!("peripheral {id}.{f} {bound}")
                } else {
                    format!("peripheral {id}")
                }
            }
            "boot-coverage" => {
                let net = self.net.clone().unwrap_or_default();
                format!(
                    "{net} driven to >= {} V within {} ms of reset",
                    self.min.unwrap_or(0.0),
                    self.deadline_ms.unwrap_or(0.0)
                )
            }
            other => other.to_string(),
        }
    }
}
