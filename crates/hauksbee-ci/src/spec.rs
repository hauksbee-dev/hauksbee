//! The `hauksbee-ci` spec: a TOML file a hardware repo checks in, describing one
//! headless co-simulation and the assertions that must hold for the build to
//! pass. Designed to be pleasant to hand-write.
//!
//! ```toml
//! name = "power-up sanity"
//! board = "hardware/board.kicad_pcb"        # .kicad_pcb / .kicad_sch / .net / .brd / .d356
//! firmware = "firmware/build/app.elf"        # optional ELF/hex
//! mcu = "atmega328p"                          # informational note only; the binder detects the MCU from the board
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
//! suppress_rail = ["VDD_A"]              # feed these nets through the
//!                                             # board only (no auto rail)
//!
//! [[override]]                                # 0+ component value overrides
//! ref = "R_SHUNT1"
//! value = "0.05"
//!
//! [fuzz]                                      # optional initial-state fuzzing
//! seeds = 16
//!
//! [[assert]]                                  # 1+ assertions
//! kind = "voltage"
//! net = "VDD_A"
//! min = 4.9
//! after_ms = 50
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::{near_matches, SpecError};
use hauksbee_ir::units::{format_amps, format_celsius_to, format_percent, format_volts};

// ── Declarative sensor ────────────────────────────────────────────────────────

/// A declarative I2C or SPI sensor attached to the co-sim for one run.
///
/// The sensor is described by a `[sensor]` TOML spec (parsed by
/// `RegisterMapSensor::from_toml`). Exactly one of `spec` (inline string) or
/// `spec_file` (path relative to the CI spec file) must be present.
///
/// ```toml
/// [[sensor]]
/// id          = "U2_temp"
/// spec_file   = "sensors/lm75.toml"      # or: spec = """ ... """
///
/// [sensor.inputs]
/// temperature_c = 40.0                   # override the default for this run
/// ```
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SensorAttach {
    /// Stable identifier (used in error messages and future state assertions).
    pub id: String,
    /// Inline sensor spec string (the `[sensor]` TOML block verbatim).
    #[serde(default)]
    pub spec: Option<String>,
    /// Path to a `.toml` file containing the `[sensor]` block, resolved
    /// relative to the CI spec file's directory.
    #[serde(default)]
    pub spec_file: Option<String>,
    /// Input overrides: map from input name to value. Applied after parsing,
    /// before the run starts. Unknown names are refused: a typo that quietly
    /// leaves the datasheet default in place would make the scenario lie.
    #[serde(default)]
    pub inputs: HashMap<String, f64>,
    /// Optional SPI controller name (e.g. `"spi2"`). When set, the sensor is
    /// attached to that specific controller via `attach_spi_bus_on` so it only
    /// receives traffic from that bus. When absent (the default), `attach_spi_bus`
    /// is used, which routes to the MCU's first/only SPI controller.
    /// Only meaningful for SPI sensors (`bus = "spi"` in the sensor spec).
    #[serde(default)]
    pub controller: Option<String>,
    /// Optional explicit board net that frames an SPI device's chip select.
    /// When absent, SPI retains the documented heuristic framing. Invalid net
    /// names are rejected with the rest of the spec's board references.
    #[serde(default)]
    pub cs_net: Option<String>,
    /// The I2C address the board straps this part to, for a part whose
    /// sensor spec lists `[[sensor.address_select]]` rows; it must be one of
    /// them. Without it the spec default is used and the run says so.
    #[serde(default)]
    #[schemars(range(min = 0, max = 127))]
    pub address: Option<u8>,
}

/// What a spec does with the Do-Not-Populate parts it does not name in `fit`
/// or `no_fit`. The spec spelling of [`hauksbee_extract::dnp::DnpPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DnpMode {
    /// Simulate DNP parts as fitted, except near-zero-ohm links.
    FitExceptLinks,
    /// Simulate every DNP part as fitted, links included.
    FitAll,
    /// Leave every DNP part out, as a fab house would build the board.
    #[default]
    Honour,
}

impl From<DnpMode> for hauksbee_extract::dnp::DnpPolicy {
    fn from(m: DnpMode) -> Self {
        match m {
            DnpMode::FitExceptLinks => Self::FitExceptLinks,
            DnpMode::FitAll => Self::FitAll,
            DnpMode::Honour => Self::Honour,
        }
    }
}

/// A checked-in assembly variant.
///
/// The board remains the electrical design authority; this artifact states only
/// which named placements are present in one build of that design. It is
/// separate from a BOM because a purchasing spreadsheet's populate column
/// remains advice, never hidden fit policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssemblyVariant {
    /// Human-readable variant name carried into the evidence inventory.
    pub name: String,
    /// Layout-DNP parts fitted in this assembly.
    #[serde(default)]
    pub fit: Vec<String>,
    /// Parts omitted from this assembly, even when the superset layout does
    /// not mark them DNP.
    #[serde(default)]
    pub no_fit: Vec<String>,
}

impl AssemblyVariant {
    pub(crate) fn load(path: &Path) -> Result<(Self, Vec<u8>), SpecError> {
        let shown = path.display();
        let raw = std::fs::read(path)
            .map_err(SpecError::io(format!("reading assembly variant '{shown}'")))?;
        let text = std::str::from_utf8(&raw).map_err(SpecError::invalid(format!(
            "assembly variant '{shown}' is not UTF-8 TOML"
        )))?;
        let variant: Self = toml::from_str(text).map_err(|error| SpecError::Toml {
            file: shown.to_string(),
            message: crate::error::cap_context_width(&error.to_string()),
        })?;
        need(
            !variant.name.trim().is_empty(),
            format!("assembly variant '{shown}' needs a non-empty `name`"),
        )?;
        need(
            !(variant.fit.is_empty() && variant.no_fit.is_empty()),
            format!("assembly variant '{shown}' names no `fit` or `no_fit` references; an empty variant changes no assembly"),
        )?;
        validate_fit_lists(
            &variant.fit,
            &variant.no_fit,
            &format!("assembly variant '{shown}'"),
        )?;
        Ok((variant, raw))
    }

    pub(crate) fn apply_to(&self, spec: &mut Spec, path: &Path) -> Result<(), SpecError> {
        for (into, from) in [(&mut spec.fit, &self.fit), (&mut spec.no_fit, &self.no_fit)] {
            into.extend(from.iter().cloned());
            let mut seen = std::collections::HashSet::new();
            into.retain(|reference| seen.insert(reference.clone()));
        }
        validate_fit_lists(
            &spec.fit,
            &spec.no_fit,
            &format!("spec plus assembly variant '{}'", path.display()),
        )
    }
}

/// `Ok(())` when `ok`, else the invalid-spec error carrying `msg`. The one
/// shape every structural check below reduces to.
fn need(ok: bool, msg: String) -> Result<(), SpecError> {
    if ok {
        Ok(())
    } else {
        Err(SpecError::Invalid(msg))
    }
}

/// Absolute zero (C). No temperature field of a spec may sit at or below it.
pub const ABSOLUTE_ZERO_C: f64 = -273.15;

/// The hottest ambient (C) a spec may state.
///
/// No electronic part sits in air above this (the hottest real cases are a few
/// hundred degrees), so a larger figure is a unit slip or a typo, and the
/// junction numbers built on it would be past silicon's melting point anyway.
pub const MAX_AMBIENT_C: f64 = 500.0;

/// The longest board time (ms) one run may simulate: ten minutes.
///
/// The co-simulation spends real time on every simulated millisecond, so a
/// larger figure does not finish in any useful time, and `1e15` ran until killed.
pub const MAX_DURATION_MS: f64 = 600_000.0;

/// The most co-sim frames one run may take (`duration_ms / frame_ms`).
pub const MAX_FRAMES: f64 = 1_000_000.0;

/// Why `value` is not a usable temperature in `field`, or `None` when it is a
/// finite temperature above absolute zero and at most `ceiling_c`. The one
/// place every temperature a spec states is checked, so `nan`, `inf`, `-0` and
/// sub-absolute-zero figures are refused with the same words.
fn temperature_problem(field: &str, value: f64, ceiling_c: f64, what: &str) -> Option<String> {
    (!(value.is_finite() && value > ABSOLUTE_ZERO_C && value <= ceiling_c)).then(|| {
        format!(
            "{field} must be a finite temperature above absolute zero ({ABSOLUTE_ZERO_C} C) and \
             at most {ceiling_c} C ({what}), got {value}"
        )
    })
}

fn validate_fit_lists(fit: &[String], no_fit: &[String], context: &str) -> Result<(), SpecError> {
    need(
        fit.iter().chain(no_fit).all(|r| !r.trim().is_empty()),
        format!("{context} contains an empty fit/no_fit reference"),
    )?;
    for (list, name) in [(fit, "fit"), (no_fit, "no_fit")] {
        let mut seen = std::collections::HashSet::new();
        if let Some(reference) = list.iter().find(|r| !seen.insert(*r)) {
            return Err(SpecError::Invalid(format!(
                "{context} names '{reference}' more than once in `{name}`"
            )));
        }
    }
    fit.iter()
        .find(|reference| no_fit.contains(reference))
        .map_or(Ok(()), |reference| {
            Err(SpecError::Invalid(format!(
                "{context} names '{reference}' as both fitted and left open; pick one"
            )))
        })
}

/// Timing coverage a strict check requires from the MCU/co-sim bridge.
///
/// Declaring this opts the run into adaptive poll chunking and fail-closed
/// capability negotiation; absent means the report still publishes measured
/// timing coverage but makes no extra timing claim.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimingSpec {
    /// Narrowest firmware pulse the check needs guaranteed observable.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub min_pulse_us: Option<f64>,
    /// Largest acceptable uncertainty on a firmware GPIO edge timestamp.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub max_edge_error_us: Option<f64>,
}

impl TimingSpec {
    fn validate(&self) -> Result<(), SpecError> {
        for (name, value) in [
            ("min_pulse_us", self.min_pulse_us),
            ("max_edge_error_us", self.max_edge_error_us),
        ] {
            need(
                !value.is_some_and(|v| !v.is_finite() || v <= 0.0),
                format!("timing.{name} must be a positive, finite number"),
            )?;
        }
        need(
            self.min_pulse_us.is_some() || self.max_edge_error_us.is_some(),
            "timing needs `min_pulse_us` or `max_edge_error_us`".into(),
        )
    }

    #[must_use]
    pub fn requirement(self) -> hauksbee_engine::scheduler::TimingRequirement {
        hauksbee_engine::scheduler::TimingRequirement {
            min_pulse_s: self.min_pulse_us.map(|v| v * 1e-6),
            max_edge_error_s: self.max_edge_error_us.map(|v| v * 1e-6),
        }
    }
}

/// A fully-parsed, validated spec.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
    /// Optional BOM, resolved relative to the spec. It is dialect-detected and
    /// reconciled through the same fail-closed identity path as `hauksbee run
    /// --bom` before any CI assertion binds.
    #[serde(default)]
    pub bom: Option<PathBuf>,
    /// Explicit BOM column confirmations, each `role=Header`, equivalent to a
    /// repeated `hauksbee run --bom-column` flag.
    #[serde(default)]
    pub bom_columns: Vec<String>,
    /// Optional pick-and-place file, resolved relative to the spec and checked
    /// against layout position, side, rotation, package and identity.
    #[serde(default)]
    pub placement: Option<PathBuf>,
    /// Optional highest-priority model-library directory, resolved relative to
    /// the spec file. `hauksbee-ci run` and `check` load it above the builtin
    /// db, packs and user model dirs; a missing directory is an error. A CLI
    /// `--models-dir` replaces it for that run (the operator's setting wins,
    /// as `$HAUKSBEE_MCU_DIR` does over `[mcu] descriptor_dir`). The browser
    /// stages selected model TOMLs here so Checks binds against the same model
    /// context as report analysis and Live Sim.
    #[serde(default)]
    pub models_dir: Option<PathBuf>,
    /// Optional assembly-variant TOML (`name`, `fit`, `no_fit`). Its exact
    /// bytes and decisions are inventoried separately from the BOM.
    #[serde(default)]
    pub variant: Option<PathBuf>,
    /// Optional companion Eagle schematic. CI identity-validates and inventories
    /// it as exact input provenance; assertions still use the board alone as the
    /// physical authority and do not consume its declarations.
    #[serde(default)]
    pub schematic: Option<PathBuf>,
    /// Optional firmware ELF/hex, relative to the spec file's directory.
    #[serde(default)]
    pub firmware: Option<PathBuf>,
    /// References of Do-Not-Populate parts to simulate as fitted, whatever
    /// `dnp` says. An unknown reference is a loud spec error.
    #[serde(default)]
    pub fit: Vec<String>,
    /// References to leave open, whatever the layout DNP state or `dnp` policy
    /// says. This is also how an assembly variant omits an ordinary part from
    /// a superset layout.
    #[serde(default)]
    pub no_fit: Vec<String>,
    /// What to do with the DNP parts neither `fit` nor `no_fit` names:
    /// `"honour"` (the default: leave every part the board marks not-fitted
    /// out, as a fab house would build it), `"fit-except-links"` (simulate DNP
    /// parts as fitted but leave near-zero-ohm links open because fitting one
    /// merges the nets it bridges), or `"fit-all"`.
    #[serde(default)]
    pub dnp: DnpMode,
    /// Optional as-built overlay (.asbuilt.toml), relative to the spec file's
    /// directory: the declarative physical delta between the design files and
    /// the real reworked board (cut traces, jumpers, lifted pins, fitted
    /// values), applied to the bound board before every run. Distinct from
    /// `[[override]]`, which swaps a component's VALUE string pre-bind: the
    /// overlay performs post-bind structural surgery.
    #[serde(default)]
    pub asbuilt: Option<PathBuf>,
    /// Either an informational MCU-kind note (`mcu = "atmega328p"`, the legacy
    /// string form; nothing reads it, the binder detects the MCU from the
    /// board), or an `[mcu]` table carrying co-sim MCU configuration such as
    /// `descriptor_dir` (a directory of `<part>.soc.toml` `SoC` descriptor
    /// overrides, resolved relative to the spec file's directory). Note the
    /// binder still detects WHICH MCU from the board's part value; neither
    /// form can force a backend.
    // `deserialize_with`: the enum's untagged derive reports every table-form
    // mistake as "data did not match any variant", naming no key; the shape
    // dispatch in `de_mcu_field` keeps McuConfig's real error instead.
    #[serde(default, deserialize_with = "de_mcu_field")]
    pub mcu: Option<McuField>,
    /// Simulated duration in milliseconds.
    #[serde(default = "default_duration_ms")]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub duration_ms: f64,
    /// Co-sim frame cadence in milliseconds (how often nets are sampled).
    #[serde(default = "default_frame_ms")]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub frame_ms: f64,
    /// Optional strict timing contract. Poll backends refine their real MCU
    /// slice to meet it; an unrepresentable contract makes the run INVALID.
    #[serde(default)]
    pub timing: Option<TimingSpec>,
    /// Ambient temperature (C) for the steady-state junction-temperature
    /// estimate (`max_temp` assertions) and temperature-dependent ratings.
    /// Default 25 C, disclosed where a part's hottest rating row would flip it.
    #[serde(default)]
    pub ambient_c: Option<f64>,
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
    /// Component-tolerance rules: sampled per ensemble seed so assertions must
    /// hold across the tolerance ensemble, not just at nominal values.
    #[serde(default, rename = "tolerance")]
    pub tolerances: Vec<ToleranceRule>,
    /// Tolerance-ensemble execution config (seed count, monte-carlo vs
    /// corners). Only meaningful when tolerances are declared.
    #[serde(default)]
    pub ensemble: Option<EnsembleSpec>,
    /// Initial-state fuzzing: run the sim under several random register/
    /// undefined-state seeds. An assertion must hold across *all* seeds.
    #[serde(default)]
    pub fuzz: Option<FuzzSpec>,
    /// Transient scenarios: dynamic load profiles attached to parts (drawn as
    /// current sinks on supply nets), exercising inrush / sag / brownout that DC
    /// cannot see.
    #[serde(default, rename = "scenario")]
    pub scenarios: Vec<crate::scenarios::Scenario>,
    /// Inline (spec-local) load-profile definitions, referenced by a scenario's
    /// `profile` field in addition to the built-in profile database.
    #[serde(default, rename = "profile")]
    pub profiles: Vec<crate::scenarios::InlineProfile>,
    /// Opt-in capacitor parasitics (ESR/ESL) for this run. Off by default, so a
    /// board's decoupling stays ideal unless realism is explicitly requested.
    #[serde(default)]
    pub decoupling: Option<crate::scenarios::Decoupling>,
    /// Optional small-signal AC analysis: a frequency sweep run once on the
    /// biased circuit, feeding the `phase_margin` / `ac_gain` assertions. The
    /// analysis is seed-independent (it linearises about the DC operating point),
    /// so it is computed once and shared across fuzz seeds.
    #[serde(default)]
    pub ac: Option<AcConfig>,
    /// The assertions, all of which must pass. A spec with no `[[assert]]`
    /// blocks is rejected (it would pass vacuously).
    // `length(min = 1)` mirrors Spec::validate's empty-asserts rejection in the
    // editor schema; the serde default stays so load() can produce the
    // friendlier validation error. The schema also lists `assert` as required,
    // but schemars never marks a defaulted field required, so the generator in
    // tests/schema_drift.rs adds that.
    #[serde(default, rename = "assert")]
    #[schemars(length(min = 1))]
    pub asserts: Vec<Assertion>,
    /// Operating conditions every outcome must hold across (outcome-driven
    /// fault finding, see `logic_level` / `fires`).
    #[serde(default)]
    pub search: Option<crate::search::Search>,
    /// Declarative sensors attached to the co-sim for this run. Each entry
    /// describes one I2C or SPI sensor (via an inline spec string or a file
    /// path) plus optional per-run input overrides.
    #[serde(default, rename = "sensor")]
    pub sensors: Vec<SensorAttach>,

    /// Directory the spec was loaded from (for resolving relative paths). Not
    /// part of the TOML; filled in by [`Spec::load`].
    #[serde(skip)]
    pub base_dir: PathBuf,
    /// Record every asserted net's per-frame series for `--waveforms`. Not
    /// part of the TOML; set by the caller.
    #[serde(skip)]
    pub record_waveforms: bool,
}

/// The spec's `mcu` key: a bare string (`mcu = "atmega328p"`) is the legacy
/// informational note; an `[mcu]` table carries real configuration.
// The `untagged` derive stays because the editor schema is generated from it
// (string-or-table), but the spec loader never runs it: `Spec.mcu` routes
// through `de_mcu_field`, because untagged deserialization swallows the
// variant's own error and reports only "data did not match any variant".
// (A `//` comment, not `///`: doc text here becomes the schema description
// an editor shows users, and they don't need serde internals in a hover.)
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum McuField {
    /// Informational MCU-kind note. Nothing reads it: the binder detects the
    /// MCU from the BOARD's part value via the model library's `[[models]]
    /// kind = "mcu"` routing entries.
    Note(String),
    /// The `[mcu]` table form, carrying co-sim MCU configuration.
    Config(McuConfig),
}

/// The `[mcu]` table.
///
/// ```toml
/// [mcu]
/// name           = "stm32f103"   # informational note (optional)
/// descriptor_dir = "mcu"         # SoC descriptor overrides, relative to the spec
/// ```
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McuConfig {
    /// Informational MCU-kind note, same meaning as the string form
    /// `mcu = "..."`; nothing reads it.
    #[serde(default)]
    pub name: Option<String>,
    /// Directory of `<part>.soc.toml` `SoC` descriptor overrides for this run,
    /// resolved relative to the spec file's directory. The same layer the
    /// `HAUKSBEE_MCU_DIR` environment variable provides, made declarative so a
    /// hardware repo can check its descriptor overrides in beside the spec.
    /// Precedence: an explicitly set `HAUKSBEE_MCU_DIR` environment variable
    /// WINS over this field (the env var is the operator's override of last
    /// resort; a spec must not be able to silently defeat it).
    #[serde(default)]
    pub descriptor_dir: Option<PathBuf>,
}

/// Deserialize the spec's `mcu` key by dispatching on the TOML value's shape
/// instead of trying the untagged variants in turn. Untagged reports every
/// table-form failure as "data did not match any variant of untagged enum
/// `McuField`", which names no key and (because the whole table is the value)
/// points nowhere useful. Dispatching on the shape keeps [`McuConfig`]'s own
/// error ("unknown field `x`, expected `name` or `descriptor_dir`"), and lets
/// the commonest real mistake get named as exactly what it is: TOML gives every
/// `key = value` line after an `[mcu]` header to that table, so a top-level key
/// written below it is swallowed silently and then rejected as an unknown MCU
/// field on a line the user believes is top-level.
fn de_mcu_field<'de, D>(deserializer: D) -> Result<Option<McuField>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = toml::Value::deserialize(deserializer)?;
    match value {
        toml::Value::String(s) => Ok(Some(McuField::Note(s))),
        toml::Value::Table(table) => {
            // A key legal in BOTH places (`name`) must not trip the hint: in an
            // [mcu] table it is simply the MCU note the user asked for.
            let (spec_keys, mcu_keys) = (struct_fields::<Spec>(), struct_fields::<McuConfig>());
            if let Some(key) = table
                .keys()
                .find(|k| spec_keys.contains(k) && !mcu_keys.contains(k))
            {
                return Err(D::Error::custom(format!(
                    "`{key}` is a top-level key; move it above the [mcu] table \
                     (everything below an [mcu] header belongs to that table)"
                )));
            }
            McuConfig::deserialize(toml::Value::Table(table))
                .map(|c| Some(McuField::Config(c)))
                .map_err(D::Error::custom)
        }
        other => Err(D::Error::custom(format!(
            "`mcu` must be a string like mcu = \"atmega328p\" (an informational \
             note) or an [mcu] table, not {}",
            other.type_str()
        ))),
    }
}

/// A struct's serialized field names (renames applied, `#[serde(skip)]` fields
/// absent), read off the JSON schema it already derives, so the swallowed-key
/// hint above can never drift from the struct definition.
pub(crate) fn struct_fields<T: JsonSchema>() -> Vec<String> {
    schemars::schema_for!(T)
        .as_object()
        .and_then(|s| s.get("properties")?.as_object())
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

fn default_name() -> String {
    "hauksbee-ci".to_string()
}
const fn default_duration_ms() -> f64 {
    100.0
}
const fn default_frame_ms() -> f64 {
    1.0
}

/// A power-supply leg attached to a supply net. Mirrors the engine's
/// behavioral supplies (bench / wall / USB / battery / ideal).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("anyOf" = [{"required": ["net"]}, {"required": ["ref", "pin"]}]))]
pub struct SupplySpec {
    /// The supply net this leg feeds, e.g. `"+5V"`. Must name a net that
    /// exists on the board (checked once the board is bound). Alternative to
    /// `ref` + `pin`, which is filled in here once the board is read.
    #[serde(default)]
    pub net: String,
    /// Component (usually the power-input connector) whose `pin` is on the fed
    /// net, for a net with no stable name: an unnamed net is spelled
    /// differently by each tool, the pin is not.
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// Pin/pad number on `ref`.
    #[serde(default)]
    pub pin: Option<String>,
    /// One of `ideal | bench | wall | usb | battery`. `ideal`/`bench`/`wall`
    /// need an explicit `volts`; `usb` needs a `usb` profile; `battery` needs
    /// a `chemistry`. Nothing is assumed, a wrong guess would fabricate
    /// faults on a healthy board.
    // The token enums on kind/usb/chemistry are string fields (not Rust enums)
    // because SupplySpec::validate owns the real acceptance and its error
    // messages; the extend() lists mirror exactly what validate accepts so the
    // editor flags a typo the same way the loader would.
    #[schemars(extend("enum" = ["ideal", "bench", "wall", "usb", "battery"]))]
    pub kind: String,
    /// Nominal output voltage (V). Required for `ideal` / `bench` / `wall`; a
    /// rail may be negative (e.g. -12 V), only a non-finite value is illegal.
    #[serde(default)]
    pub volts: Option<f64>,
    /// Cold power-on: ramp this rail linearly from 0 V to `volts` over this many
    /// milliseconds instead of stamping it already settled. Without it the
    /// transient starts from the DC operating point, which pre-charges every
    /// capacitor, so a power-on RC delay reads 0 ms.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub ramp_ms: Option<f64>,
    /// Current limit (A). Above it a bench/wall supply drops out of regulation
    /// into constant-current, which is how a brownout gets reproduced.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub current_limit_a: Option<f64>,
    /// Output impedance (ohms): the series resistance the leg presents, so a
    /// load step sags the rail instead of holding it stiff.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub r_out_ohms: Option<f64>,
    /// Peak-to-peak output ripple (V), superimposed on `volts` at `ripple_hz`.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub ripple_vpp: Option<f64>,
    /// Ripple frequency (Hz); pair it with `ripple_vpp`.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub ripple_hz: Option<f64>,
    /// USB profile: `5v0.5a | 5v1.5a | 5v3a` (underscore spellings accepted).
    #[serde(default)]
    #[schemars(extend("enum" = ["5v0.5a", "5v_0.5a", "5v1.5a", "5v_1.5a", "5v3a", "5v_3a"]))]
    pub usb: Option<String>,
    /// Battery chemistry: `liion | alkaline | nimh | lifepo4` (aliases `lipo`, `lfp`).
    #[serde(default)]
    #[schemars(extend("enum" = ["liion", "lipo", "alkaline", "nimh", "lifepo4", "lfp"]))]
    pub chemistry: Option<String>,
    /// Cells in series. The pack voltage is the chemistry's per-cell curve
    /// times this, so a wrong count is a wrong rail.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub cells: Option<u32>,
    /// Pack capacity (mAh), which sets how fast the state of charge (and so
    /// the terminal voltage) walks down under load.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub capacity_mah: Option<f64>,
    /// State of charge as a fraction in 0..1.
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub soc: Option<f64>,
    /// Pack internal resistance (ohms): the sag-per-amp of a real cell, and
    /// usually the reason a battery-powered board browns out at boot.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub r_internal_ohms: Option<f64>,
    /// BMS over-current protection trip threshold (A). Present = protected pack.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub protection_trip_a: Option<f64>,
    /// Sustained time above the trip threshold before the cutoff latches (ms).
    /// Default 0 (instant) when `protection_trip_a` is set.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub protection_delay_ms: Option<f64>,
    /// Current the load must fall below to re-arm the cutoff (A). Default: trip.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub protection_reset_a: Option<f64>,
}

/// Small-signal AC analysis configuration.
///
/// ```toml
/// [ac]
/// fstart = 10.0        # Hz
/// fstop  = 1e6         # Hz
/// points = 20          # per decade (dec) or total (lin)
/// sweep  = "dec"       # "dec" | "lin"  (default "dec")
/// ```
///
/// Every independent source in the circuit is driven with unit AC amplitude, so
/// node phasors are the transfer function from that stimulus. A `phase_margin`
/// assertion names the loop break/output net; an `ac_gain` assertion names a net
/// and bounds its magnitude (dB) at an optional `freq_hz`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcConfig {
    /// Sweep start (Hz).
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub fstart: f64,
    /// Sweep stop (Hz); must exceed `fstart`.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub fstop: f64,
    /// Points per decade (`dec`) or total (`lin`).
    #[schemars(range(min = 1))]
    pub points: usize,
    /// "dec" (per-decade, log) or "lin" (linear). Default "dec".
    #[schemars(extend("enum" = ["dec", "lin"]))]
    #[serde(default = "default_sweep")]
    pub sweep: String,
}

fn default_sweep() -> String {
    "dec".to_string()
}

impl AcConfig {
    fn validate(&self) -> Result<(), SpecError> {
        // TOML accepts `inf`/`nan` float literals, and every comparison against
        // them is false, so `fstop <= fstart` and `fstart <= 0` both pass for a
        // non-finite bound. That flows into AcSpec::frequencies() where
        // `(fstop/fstart).log(base)` becomes inf and the step count saturates to
        // usize::MAX, a `with_capacity` overflow panic (debug) or a bogus
        // inf-Hz sweep (release). Reject non-finite bounds up front, matching the
        // finiteness guards on duration_ms/frame_ms/after_ms/freq_hz.
        need(
            self.fstart.is_finite() && self.fstop.is_finite(),
            "[ac] fstart and fstop must be finite".into(),
        )?;
        need(
            self.fstart > 0.0 && self.fstop > self.fstart,
            "[ac] needs 0 < fstart < fstop".into(),
        )?;
        need(self.points > 0, "[ac] points must be >= 1".into())?;
        need(
            matches!(self.sweep.as_str(), "dec" | "lin"),
            format!(
                "[ac] sweep must be 'dec' or 'lin', got '{}'{}",
                self.sweep,
                crate::error::did_you_mean_hint(&self.sweep, &["dec", "lin"])
            ),
        )
    }
}

/// A net forced to a fixed DC voltage for the run.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetDrive {
    /// The net to hold at a fixed voltage (external stimulus, a strapping
    /// resistor, a register bit the firmware cannot set in a headless run).
    pub net: String,
    /// The voltage to force (V), stamped as an ideal source for the whole run.
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
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeripheralSpec {
    /// Stable id, used by events / live control / state assertions.
    pub id: String,
    /// Peripheral type.
    #[serde(rename = "type")]
    #[schemars(extend("enum" = [
        "pushbutton", "toggle", "potentiometer", "encoder", "stimulus",
        "i2c_eeprom", "i2c_lm75", "spi_eeprom", "spi_mcp3008", "vcd_sink"
    ]))]
    pub kind: String,

    // Attachment: by net name, or by connector ref + pin (resolved to a net).
    /// The net this peripheral attaches to. Alternative to `ref` + `pin`. Note
    /// a `vcd_sink` reads only `nets = [...]`, never this singular field.
    #[serde(default)]
    pub net: Option<String>,
    /// Reference designator of a board component, read two ways depending on the
    /// peripheral kind.
    ///
    /// On a net-attached control it is the CONNECTOR whose `pin` names the
    /// attachment pad (`ref` + `pin` resolve to a net).
    ///
    /// On a SPI slave (`spi_eeprom` / `spi_mcp3008`) there is no `pin`: `ref`
    /// names the board component the peripheral IS, which is what lets the chip
    /// select be found without `cs_net`. If the part is assembled,
    /// identity-trusted, and binds to a model declaring a `cs` pin role, the co-sim
    /// frames transactions on that net's real edges (exact framing). An explicit
    /// `cs_net` always wins over it. A `ref` naming no board component is a loud
    /// error, not a silent fall back to the framing heuristic.
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
    #[schemars(range(max = 127))]
    pub address: Option<u8>,
    /// EEPROM size in bytes (`i2c_eeprom` / `spi_eeprom`).
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub size: Option<usize>,
    /// Sensor temperature in Celsius (`i2c_lm75`).
    #[serde(default)]
    pub temp_c: Option<f64>,
    /// ADC reference voltage (`spi_mcp3008`).
    #[serde(default)]
    pub vref: Option<f64>,
    /// Chip-select net for a SPI slave (`spi_eeprom` / `spi_mcp3008` / SPI sensor).
    /// When set and the net resolves to an MCU GPIO pin, the co-sim frames the
    /// slave's transactions on the real CS edges (exact framing) instead
    /// of the chunk-boundary heuristic.
    ///
    /// Takes precedence over the `cs` pin role of the model bound to this
    /// peripheral's `ref`, so a board whose model pad map is wrong, or whose chip
    /// select is buffered through something the model cannot see, stays
    /// overridable by hand. Absent AND no model-declared `cs` role reachable
    /// through `ref` = heuristic framing.
    #[serde(default)]
    pub cs_net: Option<String>,
    /// Stimulus waveform: "dc"|"sine"|"pwl"|"noise".
    #[serde(default)]
    #[schemars(extend("enum" = ["dc", "sine", "pwl", "noise"]))]
    pub waveform: Option<String>,
    /// stimulus: DC offset (V) the waveform swings about.
    #[serde(default)]
    pub offset: Option<f64>,
    /// stimulus: waveform amplitude (V), peak about `offset`.
    #[serde(default)]
    pub amplitude: Option<f64>,
    /// stimulus: waveform frequency (Hz) for `sine` / `noise`.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// PWL points as `[[t_ms, value], ...]`.
    #[serde(default)]
    pub pwl: Option<Vec<[f64; 2]>>,
    /// Nets a `vcd_sink` should log.
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
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineEventSpec {
    /// When the event fires (ms into the run).
    pub t_ms: f64,
    /// The value to set: 1/0 for a button press/release or toggle, a position
    /// in 0..1 for a potentiometer, a level for a stimulus.
    pub value: f64,
}

/// The "unknown token" error for a closed vocabulary: names what was written,
/// the nearest legal spelling, and the whole list.
fn unknown_token(what: &str, got: &str, options: &[&str]) -> String {
    format!(
        "{what} '{got}'{} (expected one of {})",
        crate::error::did_you_mean_hint(got, options),
        options.join("|")
    )
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
        const WAVEFORMS: &[&str] = &["dc", "sine", "pwl", "noise"];
        let id = &self.id;
        need(
            KINDS.contains(&self.kind.as_str()),
            format!(
                "peripheral '{id}': {}",
                unknown_token("unknown type", &self.kind, KINDS)
            ),
        )?;
        // Net-attached controls need an attachment.
        let needs_net = matches!(
            self.kind.as_str(),
            "pushbutton" | "toggle" | "potentiometer" | "encoder" | "stimulus"
        );
        need(
            !needs_net
                || [&self.net, &self.reference, &self.net_a]
                    .iter()
                    .any(|f| f.is_some())
                || self.nets.is_some(),
            format!(
                "peripheral '{id}' ({}) needs a `net`, a `ref`+`pin`, or `nets`",
                self.kind
            ),
        )?;
        // A vcd_sink logs the signals named in `nets`, and the runtime reads
        // ONLY `p.nets` (never `net`/`ref`/`net_a`). A singular `net = "CLK"` (the
        // natural mistake, since every other control uses `net`) would validate
        // here and then log an EMPTY waveform with no diagnostic. Require `nets`.
        need(
            self.kind != "vcd_sink" || self.nets.as_ref().is_some_and(|n| !n.is_empty()),
            format!("peripheral '{id}' (vcd_sink) needs `nets = [...]` (the signals to log); a singular `net` is not read by the sink"),
        )?;
        // Schema-vs-validate parity: the published editor schema documents
        // these bounds, so the runtime validate path must enforce the same
        // ones (an editor-green spec must not fail differently at run time,
        // and a CLI-only author gets the same protection the editor gives).
        need(
            self.address.is_none_or(|a| a <= 127),
            format!(
                "peripheral '{id}': `address` must be a 7-bit I2C address (0..=127), got {}",
                self.address.unwrap_or(0)
            ),
        )?;
        need(
            self.size != Some(0),
            format!("peripheral '{id}': `size` must be at least 1 byte"),
        )?;
        match &self.waveform {
            Some(w) if !WAVEFORMS.contains(&w.as_str()) => Err(SpecError::Invalid(format!(
                "peripheral '{id}': {}",
                unknown_token("unknown waveform", w, WAVEFORMS)
            ))),
            _ => Ok(()),
        }
    }
}

/// `rel` as an absolute path: itself when already absolute, else joined onto
/// `base`. Every spec-relative path (board, firmware, sensor and trace files,
/// capture data) resolves through here.
pub(crate) fn resolve_in(base: &Path, rel: impl AsRef<Path>) -> PathBuf {
    let rel = rel.as_ref();
    if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    }
}

impl SensorAttach {
    /// Structural validation: one of `spec` / `spec_file` must be present.
    /// Does NOT parse the sensor TOML, that happens in the runner so parse
    /// errors are attributed to the sensor id and include context.
    pub(crate) fn validate(&self) -> Result<(), SpecError> {
        match (&self.spec, &self.spec_file) {
            (None, None) => Err(SpecError::Invalid(format!(
                "sensor '{}': needs either `spec = \"...\"` (inline) or \
                 `spec_file = \"path/to/sensor.toml\"`",
                self.id
            ))),
            (Some(_), Some(_)) => Err(SpecError::Invalid(format!(
                "sensor '{}': `spec` and `spec_file` are mutually exclusive; \
                 provide only one",
                self.id
            ))),
            _ => Ok(()),
        }
    }

    /// Resolve the sensor TOML source against the spec's base directory and
    /// return the raw string to hand to `RegisterMapSensor::from_toml`.
    ///
    /// # Errors
    ///
    /// [`SpecError::Io`] when the `spec_file` cannot be read.
    ///
    /// # Panics
    ///
    /// If neither `spec` nor `spec_file` is set, which spec validation rules
    /// out before any sensor is resolved.
    pub fn toml_source(&self, base_dir: &Path) -> Result<String, SpecError> {
        if let Some(inline) = &self.spec {
            return Ok(inline.clone());
        }
        let rel = self
            .spec_file
            .as_deref()
            .expect("validated: one must be set");
        let path = resolve_in(base_dir, rel);
        std::fs::read_to_string(&path).map_err(SpecError::io(format!(
            "sensor '{}': reading spec_file '{}'",
            self.id,
            path.display()
        )))
    }
}

/// A component value override applied before binding.
///
/// With a `tolerance`, the `value` becomes the *nominal* and the component is
/// sampled around it as part of the tolerance ensemble (see [`ToleranceRule`]).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// Reference designator, e.g. "`R_SHUNT1`".
    #[serde(rename = "ref")]
    pub reference: String,
    /// The replacement value string, spelled the way the board file would spell
    /// it (e.g. `"0.05"`, `"4k7"`, `"100n"`). Applied before binding.
    #[serde(default)]
    pub value: Option<String>,
    /// The orderable variant fitted here: a member id of the part's `variant_of`
    /// set, written to the reserved `hauksbee_select` property before binding.
    #[serde(default)]
    pub select: Option<String>,
    /// How the part is mounted, naming one of the mountings its card's rating
    /// rows are printed for (e.g. `"1 in2 copper"`), written to the reserved
    /// `hauksbee_mounting` property. Unset, each rating is its least favourable
    /// mounting's row, and the result says so.
    #[serde(default)]
    pub mounting: Option<String>,
    /// Why that variant, for the spec's reader.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional tolerance, as a percentage of `value` (10.0 = ±10%). Present =
    /// this component joins the tolerance ensemble with `value` as nominal.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0, "exclusiveMaximum" = 100))]
    pub tolerance: Option<f64>,
    /// Sampling distribution: `"uniform"` (default) or `"gaussian"` (sigma =
    /// tolerance/3, truncated at the tolerance bound). Only meaningful with
    /// `tolerance`.
    #[serde(default)]
    #[schemars(extend("enum" = ["uniform", "gaussian"]))]
    pub distribution: Option<String>,
}

/// A component-tolerance rule over components whose reference matches `ref`.
///
/// `ref` is a literal reference, or a pattern where `*` matches any run of
/// characters; every match is sampled within ±`percent` of its value on every
/// ensemble seed.
///
/// ```toml
/// [[tolerance]]
/// ref = "R*"                 # every resistor
/// percent = 10.0             # ±10%
/// distribution = "gaussian"  # optional; default "uniform"
/// ```
///
/// Rules apply in order and the last matching rule wins per component, so a
/// broad pattern can be followed by a tighter per-part rule. The nominal is
/// the component's board value (after any `[[override]]`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToleranceRule {
    /// A literal reference, or a pattern where `*` matches any run of characters.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Tolerance as a percentage of nominal (10.0 = ±10%).
    #[schemars(extend("exclusiveMinimum" = 0, "exclusiveMaximum" = 100))]
    pub percent: f64,
    /// `"uniform"` (default) or `"gaussian"` (sigma = percent/3, truncated at
    /// the bound; the standard EDA 3-sigma convention).
    #[serde(default)]
    #[schemars(extend("enum" = ["uniform", "gaussian"]))]
    pub distribution: Option<String>,
}

/// Tolerance-ensemble execution configuration.
///
/// ```toml
/// [ensemble]
/// seeds = 24                 # Monte-Carlo sample count (default 16)
/// mode  = "monte-carlo"      # "monte-carlo" (default) | "corners"
/// ```
///
/// `monte-carlo` runs `seeds` members, each sampling every toleranced
/// component from its distribution (seed 0 is the nominal baseline). A pass is
/// **sampled coverage, not worst-case proof**.
///
/// `corners` deterministically enumerates every all-min/all-max combination
/// (2^n runs, n ≤ 10), which bounds the worst case only for monotonic
/// responses, and then adds a small stratified Latin-hypercube sample of the
/// INTERIOR to test that premise (4 probes for one toleranced component, 6 for
/// two, 8 from three up). An interior probe that fails an assertion every
/// corner passed **fails the assertion** and says the corners did not bound it.
/// A clean probe set is evidence for the monotonicity, not proof. In corner
/// mode `seeds` is ignored and `[fuzz]` must be absent (the two ensembles do
/// not compose).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnsembleSpec {
    /// Monte-Carlo member count. Ignored in corner mode.
    #[serde(default = "default_ensemble_seeds")]
    #[schemars(range(min = 1))]
    pub seeds: u32,
    /// `"monte-carlo"` (default) or `"corners"`.
    #[serde(default = "default_ensemble_mode")]
    #[schemars(extend("enum" = ["monte-carlo", "corners"]))]
    pub mode: String,
}

const fn default_ensemble_seeds() -> u32 {
    16
}
fn default_ensemble_mode() -> String {
    "monte-carlo".to_string()
}

impl Default for EnsembleSpec {
    fn default() -> Self {
        Self {
            seeds: default_ensemble_seeds(),
            mode: default_ensemble_mode(),
        }
    }
}

/// Initial-state fuzzing configuration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FuzzSpec {
    /// Number of random seeds to run (each perturbs undefined initial states).
    #[schemars(range(min = 1))]
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
// `Default` exists for tests, which would otherwise spell out thirty fields to
// exercise one. It yields an empty `kind`, which `validate` rejects, so a
// defaulted assertion cannot reach a run. Kept off the doc comment on purpose:
// that text becomes the schema `description`, which is what an editor shows
// someone hovering a TOML key, and this is a note for Rust readers.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    /// Assertion kind. `voltage`: net stays in [min, max]. `uart`: output
    /// contains/matches. `toggle`: net toggles at `freq_hz` or >= `min_toggles`.
    /// `no_faults`: no stress faults raised. `max_current`: I(ref) <= amps.
    /// `max_temp`: Tj(ref) <= celsius (or device max). `peripheral`: peripheral
    /// state check. `rail_window`: rail dip/recovery and spike/settling bounds
    /// over a scenario window. `protection_trip`: battery protection trips (or
    /// must not).
    /// `boot_coverage`: a control net is driven to `min` volts within
    /// `deadline_ms` of reset and held per `hold_ms` (the kebab-case
    /// `boot-coverage` is the accepted legacy spelling of the same kind). `phase_margin` / `ac_gain`:
    /// small-signal loop checks (need an `[ac]` block). `hwtrace`: the run
    /// reproduces a captured hardware trace. `model_coverage`: enough of the
    /// board bound to a real device model. `pulse_width`: the width (ms) of the
    /// first complete high (or `level = "low"`) pulse on `net` after `after_ms`.
    /// `logic_level`: `net` reaches a valid logic high (or `level = "low"`) at
    /// every logic input it drives, the VIH/VIL read from those parts' cards at
    /// their own supply. `fires`: the comparator `ref` fires (`min_toggles`
    /// times, default once), at the `target` trip level when one is given.
    /// Both are outcome assertions a `[search]` block searches.
    /// `bus`: an I2C address ACKs (or an SPI frame holds `bytes`) on the bus
    /// log. `resets`: the MCU resets at most `max_resets` times. `reaches`:
    /// the firmware reaches `symbol` (or is not parked in `not_parked_in`).
    /// `uart_baud`: the decoded UART baud is within `tolerance` of `baud`.
    /// These four read the run's bench record.
    /// On the numeric kinds, omitting every bound makes the assertion a
    /// MEASUREMENT: the value is reported, nothing is judged.
    // Both `boot_coverage` spellings are listed: the canonical snake_case one
    // and the `boot-coverage` alias Spec::normalize folds onto it. The editor
    // must accept a spec that was correct when it was written, so dropping the
    // alias from this list would flag valid files.
    #[schemars(extend("enum" = [
        "voltage", "uart", "toggle", "no_faults", "max_current", "max_temp",
        "peripheral", "rail_window", "protection_trip", "boot_coverage",
        "boot-coverage", "phase_margin", "ac_gain", "hwtrace", "model_coverage",
        "pulse_width", "logic_level", "fires", "increases", "decreases",
        "bus", "resets", "reaches", "uart_baud"
    ]))]
    pub kind: String,
    /// Optional label (defaults to a generated description).
    #[serde(default)]
    pub name: Option<String>,

    /// Target net (voltage / toggle / `rail_window` / boot-coverage /
    /// `phase_margin` / `ac_gain`).
    #[serde(default)]
    pub net: Option<String>,
    /// Lower bound (volts / dB / degrees / peripheral field). Voltage-class
    /// assertions need at least one of `min`/`max`.
    #[serde(default)]
    pub min: Option<f64>,
    /// Upper bound.
    #[serde(default)]
    pub max: Option<f64>,
    /// Only sample at/after this time (ms), lets the rail settle first.
    #[serde(default)]
    pub after_ms: Option<f64>,
    /// `pulse_width`: which pulse to time, `"high"` (the default) or `"low"`;
    /// `logic_level`: which level the net must reach.
    #[serde(default)]
    #[schemars(extend("enum" = ["high", "low"]))]
    pub level: Option<String>,

    /// uart: substring the UART output must contain.
    #[serde(default)]
    pub contains: Option<String>,
    /// uart: regex the UART output must match.
    #[serde(default)]
    pub matches: Option<String>,
    /// Which MCU's UART (by reference). Defaults to all MCUs concatenated.
    #[serde(default)]
    pub mcu: Option<String>,

    /// toggle: expected toggle frequency (Hz); `ac_gain`: measurement frequency.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// toggle: relative tolerance on `freq_hz`, as a FRACTION in (0, 1]
    /// (0.25 = ±25%). A value like `10`, thinking in percent, would accept
    /// ±1000% and green a net that never toggles, so it is rejected.
    #[serde(default)]
    #[schemars(extend("exclusiveMinimum" = 0, "maximum" = 1))]
    pub tolerance: Option<f64>,
    /// Minimum toggle count over the run (alternative to `freq_hz`); `fires`:
    /// how many rising output edges are needed.
    #[serde(default)]
    pub min_toggles: Option<u64>,
    /// `fires`: the intended trip level (V) of the comparator's first decision,
    /// held to `tolerance` (a fraction, default 0.05).
    #[serde(default)]
    pub target: Option<f64>,
    /// `fires`: the level (V) the comparator's +IN must fall below after
    /// every firing (reset completeness); a partial reset fails it.
    #[serde(default)]
    pub reset_below: Option<f64>,
    /// `logic_level`: how long (ms) the level must be HELD, continuously, for
    /// the next stage to take it ("high enough, for long enough"); at least
    /// `min_toggles` (default 1) separate pulses must hold it. An explicit
    /// `min` (high) or `max` (low) replaces the card-read VIH/VIL.
    #[serde(default)]
    pub min_width_ms: Option<f64>,
    /// `increases` / `decreases`: the `[[search.sweep]]` net the quantity
    /// must rise (fall) along, judged between neighbouring values of that
    /// sweep with every other condition held. The quantity is the comparator
    /// `ref`'s firing rate (Hz) or the `net`'s time-averaged level (V).
    #[serde(default)]
    pub along: Option<String>,

    /// `max_current` / `max_temp`: the component to check.
    #[serde(rename = "ref", default)]
    pub reference: Option<String>,
    /// `max_current`: ceiling in amps for the component named by `ref`.
    #[serde(default)]
    pub amps: Option<f64>,

    /// `max_temp`: ceiling in C for the steady-state junction temperature of the
    /// component named by `ref`. When omitted, the device's own max junction
    /// temperature (from the model DB, or the per-package-class default) is used.
    #[serde(default)]
    pub celsius: Option<f64>,

    /// peripheral: the `id` of the `[[peripheral]]` / `[[sensor]]` to read.
    #[serde(default)]
    pub id: Option<String>,
    /// EEPROM byte sequence (hex string, e.g. "48 69" or "4869") that must
    /// appear in the peripheral's memory.
    #[serde(default)]
    pub bytes: Option<String>,
    /// A peripheral state field that must lie in [min, max] (e.g. "transitions"
    /// on a `vcd_sink`, "`temp_c`" on a sensor). Uses the assertion's min/max.
    #[serde(default)]
    pub field: Option<String>,

    // ── transient-window assertions (rail_window / protection_trip) ──────────
    /// Scope the assertion to one scenario's window by its `id`. When unset, the
    /// assertion spans the whole run.
    #[serde(default)]
    pub scenario: Option<String>,
    /// `rail_window`: the rail is considered "dipped" while it is below this
    /// voltage. Combined with `for_max_ms` to bound dip duration, and with
    /// `recover_to` / `recover_within_ms` to bound recovery.
    #[serde(default)]
    pub dip_below: Option<f64>,
    /// `rail_window`: the rail must not stay below `dip_below` for longer than
    /// this many milliseconds (total, summed over the window).
    #[serde(default)]
    pub for_max_ms: Option<f64>,
    /// `rail_window`: the voltage the rail must climb back to for "recovered".
    #[serde(default)]
    pub recover_to: Option<f64>,
    /// `rail_window`: the rail must recover (reach `recover_to`) within this many
    /// milliseconds of first dipping below `dip_below`.
    #[serde(default)]
    pub recover_within_ms: Option<f64>,
    /// `rail_window`: the rail is considered "spiking" while it is strictly
    /// above this voltage. Combine with `spike_for_max_ms` to bound total
    /// overvoltage duration, and with `settle_to` / `settle_within_ms` to bound
    /// settling.
    #[serde(default)]
    pub spike_above: Option<f64>,
    /// `rail_window`: the rail must not stay above `spike_above` for longer than
    /// this many milliseconds (total, summed over the window).
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub spike_for_max_ms: Option<f64>,
    /// `rail_window`: the voltage the rail must return to (at or below) for
    /// "settled".
    #[serde(default)]
    pub settle_to: Option<f64>,
    /// `rail_window`: the rail must settle (reach `settle_to`) within this many
    /// milliseconds of first spiking above `spike_above`.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub settle_within_ms: Option<f64>,
    /// `protection_trip`: the supply net whose battery protection is checked.
    #[serde(default)]
    pub supply_net: Option<String>,
    /// `protection_trip`: whether a protection trip is expected (true) or must NOT
    /// occur (false).
    #[serde(default)]
    pub expect_trip: Option<bool>,

    /// hwtrace: path to a `trace.toml` (relative to the spec file) describing a
    /// captured hardware trace whose per-channel features the simulated run must
    /// reproduce within the trace's stated tolerances (T6; see `hwtrace.rs`).
    #[serde(default)]
    pub trace: Option<String>,

    // boot-coverage: a control net (gate / enable / reset / CS) that must reach
    // and hold a defined level (`min`, in volts) within `deadline_ms` of reset,
    // with no stress fault raised during the boot window before it does. On a
    // board with no static bias on the net (a genuinely Hi-Z control input, the
    // case this is for) the only thing that can bring it to level is the
    // firmware, so this measures "the firmware drives it in time". If the board
    // statically biases the net it reads at level from t=0 and trivially passes:
    // such a board is out of scope, the assertion exists to adjudicate the
    // undefined-default case the netlist cannot.
    /// boot-coverage: the boot deadline (ms after reset) by which the control
    /// net must reach and hold `min` volts.
    #[serde(default)]
    pub deadline_ms: Option<f64>,
    /// boot-coverage: how long (ms) the net must HOLD its level continuously
    /// after first reaching it. Absent = hold through the whole boot deadline
    /// (the strictest reading, right for a set-and-hold control net like a
    /// display reset). `0` = the level only needs to be REACHED by the
    /// deadline (right for a heartbeat / toggling net whose high phase is
    /// shorter than the boot window, where hold-to-deadline can never pass).
    /// A positive value = the level must hold continuously for that many ms
    /// after the first reach; the run must be long enough to observe the
    /// whole hold window or the check fails as unconfirmed.
    #[serde(default)]
    pub hold_ms: Option<f64>,

    // ── model_coverage ──────────────────────────────────────────────────────
    // How much of the board bound to a real device model. Vendors encrypt
    // SPICE and IBIS models, so part of the ceiling is outside anyone's
    // control here. What is inside our control is making the number visible
    // and holding the line on it: pin what a board reaches today, and the day
    // a new part drops coverage the build says so instead of quietly
    // simulating a hole. Every threshold below is opt-in, and at least one is
    // required, since an assertion that checks nothing must not read green.
    /// `model_coverage`: the minimum fraction (0.0 to 1.0) of active ICs that
    /// must bind to a real model. This is the metric that matters: an
    /// unbound regulator changes the answer, an unbound 0402 resistor
    /// usually does not.
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub min_critical: Option<f64>,
    /// `model_coverage`: the minimum fraction (0.0 to 1.0) of all non-ignored
    /// parts that must bind. Coarser than `min_critical`, and useful as a
    /// board-wide trend line.
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub min_resolved: Option<f64>,
    /// `model_coverage`: how many unresolved parts may sit on a connected net.
    /// These are the ones whose open default actually changes the solve, so 0
    /// is the meaningful setting on a board you trust.
    #[serde(default)]
    pub max_active_unresolved: Option<usize>,

    // ── observed-behaviour kinds (bus / resets / reaches / uart_baud) ───────
    /// `bus`: the bus protocol, `"i2c"` or `"spi"`.
    #[serde(default)]
    #[schemars(extend("enum" = ["i2c", "spi"]))]
    pub protocol: Option<String>,
    /// `bus`: which bus controller, e.g. `I2C1`; absent means any bus of the protocol.
    #[serde(default)]
    pub bus: Option<String>,
    /// `bus` over I2C: the 7-bit device address (0 to 127) that is addressed.
    #[serde(default)]
    #[schemars(range(min = 0, max = 127))]
    pub addr: Option<u32>,
    /// `bus` over I2C: whether the address ACKs (true, the default) or must NACK.
    #[serde(default)]
    pub acked: Option<bool>,
    /// `bus`: at least this many transactions match (default 1).
    #[serde(default)]
    pub min_transactions: Option<u64>,
    /// `resets`: the most resets the MCU may take over the run.
    #[serde(default)]
    pub max_resets: Option<u64>,
    /// `resets`: count only resets of this cause.
    #[serde(default)]
    #[schemars(extend("enum" = ["power_on", "brown_out", "watchdog", "software", "pin"]))]
    pub cause: Option<String>,
    /// `reaches`: a function the firmware must reach (`main`, `app_start`).
    #[serde(default)]
    pub symbol: Option<String>,
    /// `reaches`: a function the firmware must NOT end the run parked in.
    #[serde(default)]
    pub not_parked_in: Option<String>,
    /// `uart_baud`: the nominal baud the firmware configured (9600, 115200).
    #[serde(default)]
    pub baud: Option<u32>,
}

/// Whether the "spec" handed to us is actually a BOARD design file, detected
/// by extension (`ext` lowercase) or content prefix, BEFORE the TOML parse.
/// A `KiCad` board is one enormous line; letting it hit the TOML parser used to
/// dump the entire board file into the terminal as error context.
fn board_file_ext(ext: &str) -> bool {
    matches!(
        ext,
        "kicad_pcb" | "kicad_sch" | "brd" | "pcbdoc" | "d356" | "net" | "board" | "zip"
    )
}

/// Content sniff for the extensionless/renamed case: the `KiCad` s-expression
/// headers and the netlist `(export` header.
fn board_file_content(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("(kicad_pcb") || head.starts_with("(kicad_sch") || head.starts_with("(export")
}

/// The board-not-a-spec repair message (names
/// what happened and the exact command that fixes it).
fn board_not_spec_message(path: &Path) -> String {
    format!(
        "'{}' is a board, not a spec: run  hauksbee-ci init {}  to scaffold a \
         spec for it, then  hauksbee-ci run <spec.toml>",
        path.display(),
        path.display()
    )
}

impl Spec {
    /// Load and validate a spec from a TOML file.
    ///
    /// # Errors
    ///
    /// [`SpecError::Io`] when the path is a board file rather than a spec or
    /// cannot be read, [`SpecError::Toml`] when it does not parse, and
    /// [`SpecError::Invalid`] (or [`SpecError::Many`]) when it fails validation.
    pub fn load(path: &Path) -> Result<Self, SpecError> {
        // Board-by-extension is decided before the read: a binary board
        // (.PcbDoc) fails read_to_string with a UTF-8 error, which would hide
        // the actual mistake.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if board_file_ext(&ext) {
            return Err(SpecError::Io(board_not_spec_message(path)));
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                // Never suggest an unrunnable command: the checkout-relative
                // example path only exists inside a hauksbee source tree; from
                // a bare binary, point at the embedded example instead.
                let checkout_example = Path::new("crates/hauksbee-ci/examples/blinky.toml");
                let suggestion = if checkout_example.exists() {
                    "hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml"
                } else {
                    "hauksbee-ci run --example blinky"
                };
                // A path with glob metacharacters in it never came from the
                // shell: the shell would have expanded it, or reported no
                // matches itself. So the user quoted it, and the multi-spec
                // documentation ("hauksbee-ci run ci/*.toml") is exactly the
                // thing that invites the quotes. Say so, because "no spec file
                // at 'ci/*.toml'" sends people to check a path that is fine.
                if looks_like_a_glob(path) {
                    return SpecError::Io(format!(
                        "no spec file at '{}', and that looks like a glob. hauksbee-ci \
                         does not expand one itself; your shell does, and quoting it \
                         stopped that. Drop the quotes:\n  hauksbee-ci run {}",
                        path.display(),
                        path.display()
                    ));
                }
                SpecError::Io(format!(
                    "no spec file at '{}'. Check the path, or try a bundled example:\n  \
                     {suggestion}",
                    path.display()
                ))
            } else {
                SpecError::Io(format!("reading {}: {e}", path.display()))
            }
        })?;
        // Content sniff for a board file that was renamed .toml (or has no
        // extension): same repair as the extension case above.
        if board_file_content(&text) {
            return Err(SpecError::Io(board_not_spec_message(path)));
        }
        let mut spec: Self = toml::from_str(&text).map_err(|e| SpecError::Toml {
            file: path.display().to_string(),
            // `e.to_string()` keeps the "at line N, column M" locator and the
            // caret-annotated snippet; `e.message()` dropped both, so a hand-author
            // with a typo got the reason but no line to jump to. The snippet's
            // context lines are width-capped: a machine-written file can be one
            // enormous line, and the terminal is not the place to dump it.
            message: crate::error::cap_context_width(&e.to_string()),
        })?;
        spec.base_dir = base_dir_of(path);
        spec.normalize();
        spec.validate()?;
        Ok(spec)
    }

    /// Fold accepted aliases onto their canonical spelling, so everything
    /// downstream (evaluation, reports, waiver matching) sees ONE name.
    /// `boot-coverage` was the lone kebab-case kind among fourteen `snake_case`
    /// ones; the canonical spelling is now `boot_coverage` and the old one is
    /// accepted here as a silent alias, forever, because a rename must never
    /// break a spec that was correct when it was written.
    pub fn normalize(&mut self) {
        for a in &mut self.asserts {
            if a.kind == "boot-coverage" {
                a.kind = "boot_coverage".to_string();
            }
        }
    }

    /// The board file path, resolved against the spec's directory.
    #[must_use]
    pub fn board_path(&self) -> PathBuf {
        resolve_in(&self.base_dir, &self.board)
    }

    #[must_use]
    pub fn bom_path(&self) -> Option<PathBuf> {
        self.resolve(self.bom.as_ref())
    }

    #[must_use]
    pub fn placement_path(&self) -> Option<PathBuf> {
        self.resolve(self.placement.as_ref())
    }

    #[must_use]
    pub fn models_dir_path(&self) -> Option<PathBuf> {
        self.resolve(self.models_dir.as_ref())
    }

    #[must_use]
    pub fn variant_path(&self) -> Option<PathBuf> {
        self.resolve(self.variant.as_ref())
    }

    /// The optional Eagle companion path, resolved against the spec directory.
    #[must_use]
    pub fn schematic_path(&self) -> Option<PathBuf> {
        self.resolve(self.schematic.as_ref())
    }

    /// The firmware path, resolved against the spec's directory.
    #[must_use]
    pub fn firmware_path(&self) -> Option<PathBuf> {
        self.resolve(self.firmware.as_ref())
    }

    /// The as-built overlay path, resolved against the spec's directory.
    #[must_use]
    pub fn asbuilt_path(&self) -> Option<PathBuf> {
        self.resolve(self.asbuilt.as_ref())
    }

    /// The informational MCU-kind note, whichever spelling carried it
    /// (`mcu = "..."` or `[mcu] name = "..."`).
    #[must_use]
    pub fn mcu_note(&self) -> Option<&str> {
        match &self.mcu {
            Some(McuField::Note(s)) => Some(s),
            Some(McuField::Config(c)) => c.name.as_deref(),
            None => None,
        }
    }

    /// The `[mcu] descriptor_dir` SoC-descriptor override directory, resolved
    /// against the spec's directory. `None` when unset (or when `mcu` is the
    /// legacy string form). Precedence against the `HAUKSBEE_MCU_DIR`
    /// environment variable is applied by the runner, not here: an explicitly
    /// set env var wins over this field.
    #[must_use]
    pub fn mcu_descriptor_dir(&self) -> Option<PathBuf> {
        match &self.mcu {
            Some(McuField::Config(c)) => self.resolve(c.descriptor_dir.as_ref()),
            _ => None,
        }
    }

    /// An optional spec-relative path, resolved against the spec's directory.
    fn resolve(&self, p: Option<&PathBuf>) -> Option<PathBuf> {
        Some(resolve_in(&self.base_dir, p?))
    }
}

/// Does this path carry shell glob metacharacters? Only meaningful once the
/// path is known not to exist: a file genuinely named `*.toml` is possible, and
/// if it exists nothing here runs.
fn looks_like_a_glob(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains('*') || s.contains('?') || (s.contains('[') && s.contains(']'))
}

/// The directory a spec's relative paths resolve against: the spec file's
/// parent, or `.` when there is none.
///
/// `Path::parent()` on a bare filename returns `Some("")`, not `None`, so the
/// obvious `unwrap_or(".")` never fires and the empty string reaches every
/// message that names where a path was resolved from ("resolved relative to the
/// spec file at "). Joining is unaffected either way; the printing is not.
pub(crate) fn base_dir_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

impl Spec {
    /// Structural validation independent of the board (fast, no extraction).
    /// Net-name validation happens later in the runner once the board is bound.
    ///
    /// Wraps [`Spec::validate_all`]: one error comes back as itself, several
    /// come back as one [`SpecError::Many`], so callers holding a `Result` see
    /// every independent finding in one invocation.
    pub(crate) fn validate(&self) -> Result<(), SpecError> {
        SpecError::from_many(self.validate_all())
    }

    /// Every board-independent validation error in the spec, collected in one
    /// pass. Sections are validated independently (each supply, peripheral,
    /// assertion, scenario, ...) so a spec with several mistakes reports them
    /// all at once instead of one per invocation. An empty vec = a valid spec.
    // One independent check per spec section, collected into a single list.
    #[allow(clippy::too_many_lines)]
    pub fn validate_all(&self) -> Vec<SpecError> {
        let mut errs: Vec<SpecError> = Vec::new();
        let mut check = |ok: bool, msg: String| errs.extend(need(ok, msg).err());
        check(
            !self.asserts.is_empty(),
            "spec has no [[assert]] blocks: a check with no assertions always passes vacuously"
                .into(),
        );
        for problem in self.search.iter().flat_map(crate::search::Search::problems) {
            check(false, problem);
        }
        for a in self
            .asserts
            .iter()
            .filter(|a| crate::search::is_relation(a))
        {
            let along = a.along.as_deref().unwrap_or_default();
            let swept = self.search.iter().flat_map(|s| &s.sweep);
            check(
                swept
                    .filter(|s| s.net == along)
                    .any(|s| s.volts.len().max(s.amps.len()) >= 2),
                format!(
                    "{} along '{along}' needs a [[search.sweep]] with at least two values \
                     on that net",
                    a.kind
                ),
            );
        }
        check(
            self.bom.is_some() || self.bom_columns.is_empty(),
            "`bom_columns` needs `bom = ...`; there is no BOM to map".into(),
        );
        // TOML accepts `inf`/`nan` floats, so a non-finite time field must be
        // rejected explicitly: `duration_ms = inf` passes `<= 0.0` yet makes the
        // frame loop `t < total_s` always true, an infinite CI hang, and `nan`
        // runs zero frames so every assertion fails "never sampled" (confusing
        // all-RED). Check finiteness before the sign. A non-positive frame_ms
        // (a typo, or "as fine as possible") was silently clamped to 1 µs
        // downstream, running ~1000x more frames than any real cadence.
        let duration_ok = self.duration_ms.is_finite() && self.duration_ms > 0.0;
        check(
            duration_ok,
            "duration_ms must be a positive, finite number".into(),
        );
        // A finite duration can still be absurd: the run would take real time
        // for every simulated millisecond. Refuse it here, with the limit.
        check(
            !duration_ok || self.duration_ms <= MAX_DURATION_MS,
            format!(
                "duration_ms {} is longer than the {MAX_DURATION_MS} ms (ten minutes of board \
                 time) a run may simulate; shorten it to the window the assertions need",
                self.duration_ms
            ),
        );
        let frame_ok = self.frame_ms.is_finite() && self.frame_ms > 0.0;
        check(
            frame_ok,
            "frame_ms must be a positive, finite number".into(),
        );
        check(
            !(duration_ok && frame_ok) || self.duration_ms / self.frame_ms <= MAX_FRAMES,
            format!(
                "duration_ms {} at frame_ms {} is {:.3e} co-sim frames, over the {MAX_FRAMES} \
                 a run may take; raise frame_ms or shorten duration_ms",
                self.duration_ms,
                self.frame_ms,
                self.duration_ms / self.frame_ms
            ),
        );
        // Ambient feeds every junction-temperature figure and temperature-
        // dependent rating row: a NaN poisons them all, and a value below
        // absolute zero produces a "peak" colder than physics allows.
        if let Some(problem) = self.ambient_c.and_then(|t| {
            temperature_problem(
                "ambient_c",
                t,
                MAX_AMBIENT_C,
                "no part's surroundings are hotter",
            )
        }) {
            check(false, problem);
        }
        // Transient scenarios: the schema documents start_ms >= 0, so the
        // runtime validate path must hold the same line (schema-vs-validate
        // parity); a negative or non-finite start would otherwise slide the
        // window silently.
        for s in &self.scenarios {
            check(
                s.start_ms.is_finite() && s.start_ms >= 0.0,
                format!(
                    "[[scenario]] on part '{}': `start_ms` must be zero or positive, got {}",
                    s.part, s.start_ms
                ),
            );
        }
        // Time windows that cannot overlap the run. Each of these fields is
        // individually in bounds, so per-field validation lets them through, and
        // the RUNTIME then catches them as a degenerate failure ("never sampled
        // (no window at 500ms)", "boot deadline past the end of the simulation").
        // `check` exists so an editor can say that without a co-simulation:
        // an impossible window is a spec mistake, and finding it after a
        // minutes-long solve is finding it in the wrong place.
        //
        // Only meaningful against a usable duration; a bad `duration_ms` already
        // has its own error above and would turn every window into noise.
        if duration_ok {
            let duration = self.duration_ms;
            for a in &self.asserts {
                if let Some(after) = a.after_ms.filter(|v| v.is_finite() && *v >= duration) {
                    check(
                        false,
                        format!(
                            "assertion '{}': `after_ms` ({after}) must be less than `duration_ms` \
                         ({duration}); the sample window would start at or after the end of \
                         the run, so nothing would ever be measured",
                            a.label()
                        ),
                    );
                }
                if let Some(deadline) = a.deadline_ms.filter(|v| v.is_finite() && *v > duration) {
                    check(
                        false,
                        format!(
                            "assertion '{}': `deadline_ms` ({deadline}) must be at or before \
                         `duration_ms` ({duration}); the window would extend past the end of \
                         the run, so it could never be confirmed",
                            a.label()
                        ),
                    );
                }
            }
            for s in &self.scenarios {
                check(
                    !(s.start_ms.is_finite() && s.start_ms >= duration),
                    format!(
                        "[[scenario]] on part '{}': `start_ms` ({}) must be less than \
                         `duration_ms` ({duration}); the scenario would never fire inside \
                         the run",
                        s.part, s.start_ms
                    ),
                );
            }
        }
        // Decoupling ESR/ESL overrides: same parity rule, the schema documents
        // both as >= 0 (a negative parasitic is not physical and would be
        // stamped into the solve as-is).
        for ov in self.decoupling.iter().flat_map(|d| &d.overrides) {
            for (field, v) in [("esr_ohms", ov.esr_ohms), ("esl_henries", ov.esl_henries)] {
                if let Some(x) = v {
                    check(
                        x.is_finite() && x >= 0.0,
                        format!(
                            "decoupling override on '{}': `{field}` must be zero or positive, got {x}",
                            ov.reference
                        ),
                    );
                }
            }
        }
        // boot_coverage without firmware is a hollow gate. The assertion exists
        // to adjudicate "does the FIRMWARE drive this control net in time"; with
        // no firmware loaded, nothing in the run can drive the net, so the only
        // way it reaches its level is passively through the board (a pull, a
        // supply divider settling) - exactly the vacuous pass the check exists
        // to prevent. Refuse at load, like the empty-asserts rejection above.
        if self.firmware.is_none() {
            if let Some(a) = self.asserts.iter().find(|a| a.is_boot_coverage()) {
                check(
                    false,
                    format!(
                        "boot_coverage assertion '{}' needs `firmware = ...`: with no firmware \
                     loaded, nothing in the run can drive the net, so it could only reach \
                     its level passively (a board pull / bias), and the check would pass \
                     without measuring anything; if the level is meant to be reached \
                     passively, assert it with a `voltage` check instead",
                        a.label()
                    ),
                );
            }
        }
        // An assertion's `scenario` scope must name a declared [[scenario]] id.
        // Without this, an unknown scope would silently fall back to a window
        // starting at t=0 and the assertion would be measured over the WHOLE
        // run instead of the scenario window it claims to judge, a check that
        // never fails the way the spec author intended. Same fail-loud pattern
        // as the unknown-net / unknown-profile validation. An explicit ""
        // means the run-wide window, same as leaving it unset.
        let scenario_ids: Vec<&str> = self
            .scenarios
            .iter()
            .filter_map(|s| s.id.as_deref())
            .collect();
        for a in &self.asserts {
            if let Some(scope) = a.scenario.as_deref().filter(|s| !s.is_empty()) {
                let hint = if self.scenarios.is_empty() {
                    "the spec declares no [[scenario]] blocks".to_string()
                } else if scenario_ids.is_empty() {
                    "the declared [[scenario]] blocks have no `id`; give the scenario an \
                     `id` and reference it here"
                        .to_string()
                } else {
                    format!("declared scenario ids: {}", scenario_ids.join(", "))
                };
                check(
                    scenario_ids.contains(&scope),
                    format!(
                        "{} assertion '{}' is scoped to scenario '{scope}', but no [[scenario]] \
                         declares that id ({hint}); an unknown scope would silently be measured \
                         over the whole run instead of the scenario window",
                        a.kind,
                        a.label(),
                    ),
                );
            }
        }
        // A `peripheral` assertion's `id` must name a declared [[peripheral]] or
        // [[sensor]], otherwise a typo fails only after a full co-sim runs (or
        // silently reads nothing), the same class the scenario-scope check closes.
        // A missing id was already rejected by Assertion::validate.
        let known_ids: Vec<&str> = (self.peripherals.iter().map(|p| p.id.as_str()))
            .chain(self.sensors.iter().map(|s| s.id.as_str()))
            .collect();
        for a in self.asserts.iter().filter(|a| a.kind == "peripheral") {
            if let Some(id) = a.id.as_deref() {
                let hint = if known_ids.is_empty() {
                    "the spec declares no [[peripheral]] or [[sensor]] blocks".to_string()
                } else {
                    format!("declared ids: {}", known_ids.join(", "))
                };
                check(
                    known_ids.contains(&id),
                    format!(
                        "{} assertion '{}' reads id '{id}', but no [[peripheral]] or [[sensor]] \
                         declares it ({hint})",
                        a.kind,
                        a.label()
                    ),
                );
            }
        }
        // AC assertions need the [ac] sweep block to drive them.
        check(
            self.ac.is_some()
                || !self
                    .asserts
                    .iter()
                    .any(|a| matches!(a.kind.as_str(), "phase_margin" | "ac_gain")),
            "a phase_margin / ac_gain assertion needs an [ac] sweep block (fstart, fstop, points)"
                .into(),
        );
        check(
            self.fuzz.as_ref().is_none_or(|f| f.seeds > 0),
            "[fuzz] seeds must be >= 1".into(),
        );
        // Tolerance-ensemble structural checks (board-independent; pattern
        // matching against real components happens in the runner). Upper bound
        // < 100: the min corner is `nominal * (1 - percent/100)`, so percent ==
        // 100 stamps a 0-value part (dead short / open) and percent > 100 a
        // NEGATIVE component value, both solved as an ordinary pass/fail over a
        // physically-impossible circuit rather than rejected.
        let in_band = |p: f64| p > 0.0 && p < 100.0 && p.is_finite();
        for t in &self.tolerances {
            check(
                in_band(t.percent),
                format!(
                    "[[tolerance]] on '{}': percent must be in (0, 100), got {}",
                    t.reference, t.percent
                ),
            );
        }
        for ov in &self.overrides {
            let chosen = ov.select.is_some() || ov.mounting.is_some();
            let valued = ov.value.is_some() || (chosen && ov.tolerance.is_none());
            let why = "give a `value` (a `tolerance` spreads around it), `select` a variant, or a `mounting`";
            check(valued, format!("override on '{}': {why}", ov.reference));
            if let Some(p) = ov.tolerance {
                check(
                    in_band(p),
                    format!(
                        "override on '{}': tolerance must be a percentage in (0, 100), got {p}",
                        ov.reference
                    ),
                );
            }
            check(
                ov.distribution.is_none() || ov.tolerance.is_some(),
                format!(
                    "override on '{}': `distribution` is only meaningful with `tolerance`",
                    ov.reference
                ),
            );
        }
        if let Some(e) = &self.ensemble {
            check(e.seeds > 0, "[ensemble] seeds must be >= 1".into());
            check(
                self.has_tolerances(),
                "[ensemble] without any [[tolerance]] rules (or an override with a \
                 `tolerance`) has nothing to sample"
                    .into(),
            );
        }
        // Per-section validators, each its own independent finding. The
        // corners/fuzz composition check rides on the mode parse: an
        // unparseable mode IS the error, and the composition question does not
        // arise until it is fixed (a genuine cascade, not independence).
        let distributions = (self.tolerances.iter().map(|t| &t.distribution))
            .chain(self.overrides.iter().map(|o| &o.distribution))
            .flatten()
            .map(|d| crate::tolerance::Distribution::parse(d).map(drop));
        let ensemble = self.ensemble.as_ref().map(|e| {
            crate::tolerance::Mode::parse(&e.mode).and_then(|mode| {
                need(
                    mode != crate::tolerance::Mode::Corners || self.fuzz.is_none(),
                    "[ensemble] mode = \"corners\" does not compose with [fuzz] (the corner \
                     index enumerates min/max combinations, not fuzz seeds); use \
                     mode = \"monte-carlo\" to run tolerances and net fuzz together"
                        .into(),
                )
            })
        });
        let sections = std::iter::once(validate_fit_lists(&self.fit, &self.no_fit, "spec"))
            .chain(self.bom_columns.iter().map(|mapping| {
                hauksbee_extract::bom::ColumnOverrides::parse_pair(mapping)
                    .map(drop)
                    .map_err(SpecError::invalid(format!(
                        "invalid `bom_columns` entry {mapping:?}"
                    )))
            }))
            .chain(self.timing.iter().map(TimingSpec::validate))
            .chain(self.supplies.iter().map(SupplySpec::validate))
            .chain(self.peripherals.iter().map(PeripheralSpec::validate))
            .chain(self.asserts.iter().map(Assertion::validate))
            .chain(self.sensors.iter().map(SensorAttach::validate))
            .chain(self.ac.iter().map(AcConfig::validate))
            .chain(distributions)
            .chain(ensemble);
        errs.extend(sections.filter_map(Result::err));
        errs
    }

    /// Does this spec declare any component tolerance (a `[[tolerance]]` rule
    /// or an `[[override]]` carrying a `tolerance`)?
    #[must_use]
    pub fn has_tolerances(&self) -> bool {
        !self.tolerances.is_empty() || self.overrides.iter().any(|o| o.tolerance.is_some())
    }

    /// Does this run start at a settled DC operating point and stay there, so
    /// that no clock reading can mean anything? True when no supply is ramped,
    /// every declared leg is `ideal` (a bench/wall/usb/battery leg droops and
    /// ripples, and an absent leg is an ideal auto-rail), and nothing in the
    /// spec injects an event: no peripheral timeline, no scenario, no firmware.
    /// Deliberately narrow, so the start-up-timing note lands on the specs it
    /// is actually about and on no others.
    #[must_use]
    pub fn dc_initialised(&self) -> bool {
        self.supplies
            .iter()
            .all(|s| s.ramp_ms.is_none() && s.kind == "ideal")
            && self.peripherals.iter().all(|p| p.events.is_empty())
            && self.scenarios.is_empty()
            && self.firmware.is_none()
    }

    /// The ensemble mode in effect (defaults to Monte-Carlo when tolerances
    /// exist without an explicit `[ensemble]` block).
    ///
    /// # Errors
    ///
    /// [`SpecError::Invalid`] when `[ensemble] mode` is not a known mode.
    pub fn ensemble_mode(&self) -> Result<crate::tolerance::Mode, SpecError> {
        self.ensemble
            .as_ref()
            .map_or(Ok(crate::tolerance::Mode::MonteCarlo), |e| {
                crate::tolerance::Mode::parse(&e.mode)
            })
    }

    /// Total ensemble seed count for a Monte-Carlo run: one shared seed stream
    /// drives both net fuzz and tolerance sampling, so the count is the larger
    /// of `[fuzz] seeds` and `[ensemble] seeds` (each seed does both).
    #[must_use]
    pub fn ensemble_seed_count(&self) -> u32 {
        let fuzz = self.fuzz.as_ref().map_or(1, |f| f.seeds);
        let tol = if self.has_tolerances() {
            self.ensemble
                .as_ref()
                .map_or_else(|| EnsembleSpec::default().seeds, |e| e.seeds)
        } else {
            1
        };
        fuzz.max(tol).max(1)
    }

    /// Every net name the spec references, for board-aware validation.
    #[must_use]
    pub fn referenced_nets(&self) -> Vec<(String, &'static str)> {
        // Explicit peripheral net references (ref+pin is checked at attach).
        // cs_net is included: a typo there silently degrades exact SPI
        // chip-select framing to the chunk-boundary heuristic at runtime
        // (resolve_cs_pin misses the net map and returns None) with no error,
        // so it must fail loud at load like every other net reference.
        let peripheral_nets = self.peripherals.iter().flat_map(|p| {
            [
                &p.net, &p.to, &p.a, &p.wiper, &p.b, &p.net_a, &p.net_b, &p.cs_net,
            ]
            .into_iter()
            .flatten()
            .chain(p.nets.iter().flatten())
        });
        let tag = |ctx: &'static str| move |n: &String| (n.clone(), ctx);
        (self.supplies.iter().map(|s| &s.net).map(tag("supply")))
            .chain(self.net_drives.iter().map(|d| &d.net).map(tag("net_drive")))
            .chain(peripheral_nets.map(tag("peripheral")))
            .chain(
                self.sensors
                    .iter()
                    .filter_map(|s| s.cs_net.as_ref())
                    .map(tag("sensor")),
            )
            .chain(self.suppress_rail.iter().map(tag("suppress_rail")))
            .chain(
                (self.search.iter().flat_map(|s| &s.sweep))
                    .map(|s| &s.net)
                    .map(tag("search")),
            )
            .chain(self.fuzz.iter().flat_map(|f| &f.nets).map(tag("fuzz")))
            .chain(
                self.asserts
                    .iter()
                    .flat_map(|a| [&a.net, &a.supply_net].into_iter().flatten())
                    .map(tag("assert")),
            )
            .chain(
                self.scenarios
                    .iter()
                    .filter_map(|s| s.supply_net.as_ref())
                    .map(tag("scenario")),
            )
            .collect()
    }

    /// Validate that every referenced net exists on the bound board; produce a
    /// helpful error (with near-matches) for any that do not.
    ///
    /// # Errors
    ///
    /// [`SpecError::UnknownNets`] listing every referenced net missing from
    /// `known`, each with its context and near-matches.
    pub fn check_nets(&self, known: &[String]) -> Result<(), SpecError> {
        let set: std::collections::HashSet<&str> = known.iter().map(String::as_str).collect();
        let unknown: Vec<_> = self
            .referenced_nets()
            .into_iter()
            .filter(|(net, _)| !set.contains(net.as_str()))
            .map(|(net, ctx)| {
                let suggestions = near_matches(&net, known, 5);
                (net, ctx, suggestions)
            })
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(SpecError::UnknownNets(unknown))
        }
    }
}

/// Whether a peripheral `type` is a SPI slave.
///
/// The co-sim has to frame a SPI slave's transactions, so it participates in
/// the chip-select ladder (spec `cs_net`, else the `cs` pin role of the model
/// bound to its `ref`, else the chunk-boundary heuristic).
///
/// One list, so the `ref` validation and the framing path cannot disagree about
/// which kinds care.
#[must_use]
pub fn is_spi_slave_kind(kind: &str) -> bool {
    matches!(kind, "spi_eeprom" | "spi_mcp3008")
}

/// The built-in model entry a SPI peripheral kind describes, when the model DB
/// ships one for it.
///
/// Used to refuse a `ref` that names a real board component of the WRONG part.
/// Pointing a `spi_eeprom` at the board's MCP3008 would otherwise resolve that
/// ADC's `cs` role and frame the EEPROM's transactions off a chip-select that
/// belongs to a different device, reported as `exact`. A wrong answer wearing
/// the exact tier is worse than the heuristic it replaced.
///
/// `None` means "no built-in entry for this kind", and an unrecognised bound
/// model id is ALLOWED: a user model pack may legitimately supply the part, and
/// this list cannot know its id. The check therefore only fires on the case it
/// can actually judge, one built-in SPI-slave model bound to the kind that
/// describes the other.
#[must_use]
pub fn builtin_model_id_for_spi_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "spi_eeprom" => Some("eeprom_25xx_spi"),
        "spi_mcp3008" => Some("mcp3008"),
        _ => None,
    }
}

/// The engine USB profile a spec `usb = "..."` token names (underscore
/// spellings accepted), or `None` for a token nothing matches.
pub(crate) fn usb_profile(token: &str) -> Option<hauksbee_engine::power_supply::UsbSpec> {
    use hauksbee_engine::power_supply::UsbSpec;
    match token {
        "5v0.5a" | "5v_0.5a" => Some(UsbSpec::V5_0_5A),
        "5v1.5a" | "5v_1.5a" => Some(UsbSpec::V5_1_5A),
        "5v3a" | "5v_3a" => Some(UsbSpec::V5_3A),
        _ => None,
    }
}

/// The engine battery chemistry a spec `chemistry = "..."` token names
/// (`lipo` / `lfp` aliases accepted), or `None` for a token nothing matches.
pub(crate) fn chemistry(token: &str) -> Option<hauksbee_engine::power_supply::Chemistry> {
    use hauksbee_engine::power_supply::Chemistry;
    match token {
        "liion" | "lipo" => Some(Chemistry::LiIon),
        "alkaline" => Some(Chemistry::Alkaline),
        "nimh" => Some(Chemistry::NiMh),
        "lifepo4" | "lfp" => Some(Chemistry::LiFePO4),
        _ => None,
    }
}

/// The message for a supply token nothing matches, with the did-you-mean hint
/// and the canonical spellings; shared by load-time validation and the
/// run-time builder so the two cannot disagree.
pub(crate) fn unknown_supply_token(net: &str, what: &str, got: &str, options: &[&str]) -> String {
    // The hint may name an alias (`lipo`, `lfp`); the list shows the canonical
    // spellings only, so an alias never reads as the documented form.
    let canonical: Vec<&str> = options
        .iter()
        .copied()
        .filter(|o| !ALIASES.contains(o))
        .collect();
    format!(
        "supply on '{net}': unknown {what} '{got}'{} (expected {})",
        crate::error::did_you_mean_hint(got, options),
        canonical.join("|")
    )
}

pub(crate) const USB_PROFILES: &[&str] = &["5v0.5a", "5v1.5a", "5v3a"];
pub(crate) const CHEMISTRIES: &[&str] = &["liion", "lipo", "alkaline", "nimh", "lifepo4", "lfp"];
const ALIASES: &[&str] = &["lipo", "lfp"];

impl SupplySpec {
    // One independent check per supply field, in the order a spec author reads them.
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), SpecError> {
        // (field, value, validity test, what a valid value is)
        type RangeCheck<'a> = (&'a str, Option<f64>, &'a dyn Fn(f64) -> bool, &'a str);
        const KINDS: [&str; 5] = ["ideal", "bench", "wall", "usb", "battery"];
        let net = match (&self.reference, &self.pin) {
            (Some(r), Some(p)) if self.net.is_empty() => format!("{r} pin {p}"),
            (None, None) if !self.net.is_empty() => self.net.clone(),
            _ => {
                return Err(SpecError::Invalid(format!(
                    "supply on '{}': name the fed net with either `net` or both `ref` and `pin`",
                    self.net
                )))
            }
        };
        let kind = self.kind.as_str();
        need(
            KINDS.contains(&kind),
            format!(
                "supply on net '{net}': unknown kind '{kind}'{} (expected {})",
                crate::error::did_you_mean_hint(kind, &KINDS),
                KINDS.join("|"),
            ),
        )?;

        // Numeric fields flow straight into the behavioral PowerSupply, where a
        // non-finite volts poisons every node it touches, a `soc` outside 0..1
        // reads a bogus point off the OCV curve, and `cells = 0` silently
        // collapses the pack to 0 V. TOML accepts `nan`/`inf`, so guard the
        // range here at load, fail-loud, rather than shipping garbage into a
        // run. Errors name the field and net so a spec typo is obvious. A rail
        // may be negative (e.g. -12 V): only a non-finite `volts` is illegal.
        let finite = |x: f64| x.is_finite();
        let positive = |x: f64| x.is_finite() && x > 0.0;
        let non_negative = |x: f64| x.is_finite() && x >= 0.0;
        let ranges: [RangeCheck; 11] = [
            ("volts", self.volts, &finite, "a finite number"),
            (
                "current_limit_a",
                self.current_limit_a,
                &positive,
                "a positive number",
            ),
            (
                "r_out_ohms",
                self.r_out_ohms,
                &non_negative,
                "zero or positive",
            ),
            (
                "ripple_vpp",
                self.ripple_vpp,
                &non_negative,
                "zero or positive",
            ),
            ("ripple_hz", self.ripple_hz, &positive, "a positive number"),
            (
                "capacity_mah",
                self.capacity_mah,
                &positive,
                "a positive number",
            ),
            (
                "r_internal_ohms",
                self.r_internal_ohms,
                &non_negative,
                "zero or positive",
            ),
            (
                "protection_trip_a",
                self.protection_trip_a,
                &positive,
                "a positive number",
            ),
            (
                "protection_delay_ms",
                self.protection_delay_ms,
                &non_negative,
                "zero or positive",
            ),
            (
                "protection_reset_a",
                self.protection_reset_a,
                &positive,
                "a positive number",
            ),
            ("ramp_ms", self.ramp_ms, &positive, "a positive number"),
        ];
        for (field, value, ok, wants) in ranges {
            need(
                value.is_none_or(ok),
                format!("supply on '{net}': `{field}` must be {wants}"),
            )?;
        }
        need(
            self.soc
                .is_none_or(|soc| soc.is_finite() && (0.0..=1.0).contains(&soc)),
            format!("supply on '{net}': `soc` must be a state-of-charge fraction in 0..1"),
        )?;
        need(
            self.cells != Some(0),
            format!("supply on '{net}': `cells` must be at least 1"),
        )?;
        // A ramp interpolates towards a stated end voltage, so it is only
        // expressible on a leg that names one.
        need(
            self.ramp_ms.is_none() || self.volts.is_some(),
            format!("supply on '{net}': `ramp_ms` needs an explicit `volts` to ramp up to"),
        )?;

        // No silent electrical assumptions: a supply's defining parameter must
        // be written down. Defaulting a missing `volts` to 5.0 on a 3.3 V board
        // would manufacture phantom overcurrent faults the author then debugs
        // on a healthy design. Same class for a usb leg's profile and a
        // battery's chemistry: both set the source's voltage/limit behaviour.
        // The usb / chemistry tokens are validated here too, so a typo fails
        // at load like every other spec error, not only once a run starts.
        match kind {
            "ideal" | "bench" | "wall" => need(
                self.volts.is_some(),
                format!(
                    "supply on '{net}': `{kind}` needs an explicit `volts`; add the rail's \
                     real voltage (e.g. `volts = 3.3`). Nothing is assumed: a wrong \
                     guess here would fabricate faults on a healthy board"
                ),
            )?,
            "usb" => need(
                self.usb.is_some(),
                format!(
                    "supply on '{net}': `usb` needs an explicit profile; add \
                     `usb = \"5v0.5a\"` (or 5v1.5a | 5v3a) to say what the port can \
                     actually deliver"
                ),
            )?,
            "battery" => need(
                self.chemistry.is_some(),
                format!(
                    "supply on '{net}': `battery` needs an explicit `chemistry`; add \
                     `chemistry = \"liion\"` (or alkaline | nimh | lifepo4), it sets \
                     the pack's voltage curve"
                ),
            )?,
            _ => {}
        }
        if let Some(usb) = self.usb.as_deref().filter(|t| usb_profile(t).is_none()) {
            return Err(SpecError::Invalid(unknown_supply_token(
                &net,
                "usb profile",
                usb,
                USB_PROFILES,
            )));
        }
        if let Some(chem) = self.chemistry.as_deref().filter(|t| chemistry(t).is_none()) {
            return Err(SpecError::Invalid(unknown_supply_token(
                &net,
                "chemistry",
                chem,
                CHEMISTRIES,
            )));
        }
        Ok(())
    }
}

/// The closed vocabulary of assertion kinds (canonical spellings only; the
/// `boot-coverage` alias is folded onto `boot_coverage` before matching), used
/// for the unknown-kind error's did-you-mean hint.
pub(crate) const ASSERTION_KINDS: &[&str] = &[
    "voltage",
    "uart",
    "toggle",
    "no_faults",
    "max_current",
    "max_temp",
    "peripheral",
    "rail_window",
    "protection_trip",
    "boot_coverage",
    "phase_margin",
    "ac_gain",
    "hwtrace",
    "model_coverage",
    "pulse_width",
    "logic_level",
    "fires",
    "increases",
    "decreases",
    "bus",
    "resets",
    "reaches",
    "uart_baud",
];

/// The reset causes a `resets` assertion can name.
pub(crate) const RESET_CAUSES: &[&str] = &["power_on", "brown_out", "watchdog", "software", "pin"];

/// The `uart_baud` tolerance (a fraction of the nominal rate) used when a spec
/// gives none: +-3 %, inside what an asynchronous receiver samples through.
pub const UART_BAUD_TOLERANCE: f64 = 0.03;

impl Assertion {
    /// Is this the boot-coverage kind, under either accepted spelling?
    /// `Spec::load` folds the alias, but a directly-constructed Assertion
    /// (tests, library callers) may still carry it.
    #[must_use]
    pub fn is_boot_coverage(&self) -> bool {
        matches!(self.kind.as_str(), "boot_coverage" | "boot-coverage")
    }

    /// Is this a MEASUREMENT: a kind that reduces the run to ONE number,
    /// carrying no bound at all? It reports its value with the same uncertainty
    /// and evidence notes a judged assertion carries, and claims nothing, so it
    /// can neither pass nor fail. Every other kind (`uart`, `no_faults`,
    /// `model_coverage`, ...) judges a condition and has nothing to report on
    /// its own, so an unbounded one stays the loud spec error it always was.
    pub fn measurement(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "voltage" | "toggle" | "max_current" | "phase_margin" | "ac_gain" | "pulse_width"
        ) && [self.min, self.max, self.amps, self.freq_hz]
            .iter()
            .all(Option::is_none)
            && self.min_toggles.is_none()
    }

    /// Does this assertion read a START-UP time off the transient, i.e. would a
    /// rail that came up at t=0 instead of being ramped change its answer? A
    /// pulse width is timed from edges, and any `after_ms` selects a window by
    /// the clock; both are meaningless on a DC-initialised board.
    #[must_use]
    pub fn times_startup(&self) -> bool {
        // An outcome kind's `after_ms` only skips the settle; it times nothing.
        self.kind == "pulse_width"
            || (self.after_ms.is_some_and(|t| t > 0.0)
                && !crate::search::is_outcome(self)
                && !crate::search::is_relation(self))
    }

    // One independent rule per assertion kind; each arm is a short guard.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn validate(&self) -> Result<(), SpecError> {
        let kind = self.kind.as_str();
        let net = self.net.as_deref().unwrap_or("?");
        let has_bound = self.min.is_some() || self.max.is_some() || self.measurement();
        // A non-finite time window must be rejected loud: TOML accepts `nan`, and
        // a NaN `after_ms` makes the threshold-bucket sort's `partial_cmp` return
        // None, so its `.unwrap()` PANICS (a crash, not the crate's fail-loud
        // SpecError). Check every window field up front.
        for (field, val) in [
            ("after_ms", self.after_ms),
            ("deadline_ms", self.deadline_ms),
            ("hold_ms", self.hold_ms),
        ] {
            need(
                val.is_none_or(f64::is_finite),
                format!("{kind} assertion `{field}` must be a finite number"),
            )?;
        }
        for (field, val) in [
            ("spike_above", self.spike_above),
            ("spike_for_max_ms", self.spike_for_max_ms),
            ("settle_to", self.settle_to),
            ("settle_within_ms", self.settle_within_ms),
        ] {
            need(
                val.is_none_or(f64::is_finite),
                format!("{kind} assertion `{field}` must be a finite number"),
            )?;
        }
        for (field, val) in [
            ("spike_for_max_ms", self.spike_for_max_ms),
            ("settle_within_ms", self.settle_within_ms),
        ] {
            need(
                val.is_none_or(|v| v >= 0.0),
                format!("{kind} assertion `{field}` must be non-negative"),
            )?;
        }
        match kind {
            "voltage" => {
                need(self.net.is_some(), "voltage assertion needs a `net`".into())?;
                need(
                    has_bound,
                    format!("voltage assertion on '{net}' needs a `min` and/or `max`"),
                )?;
            }
            "pulse_width" => {
                need(
                    self.net.is_some(),
                    "pulse_width assertion needs a `net`".into(),
                )?;
                need(
                    self.level
                        .as_deref()
                        .is_none_or(|l| l == "high" || l == "low"),
                    format!(
                        "pulse_width assertion on '{net}': {}",
                        unknown_token(
                            "unknown level",
                            self.level.as_deref().unwrap_or(""),
                            &["high", "low"]
                        )
                    ),
                )?;
            }
            "logic_level" => {
                need(
                    self.net.is_some(),
                    "logic_level assertion needs a `net`".into(),
                )?;
                need(
                    self.min_width_ms.is_none_or(|w| w.is_finite() && w > 0.0)
                        && self.min.into_iter().chain(self.max).all(f64::is_finite),
                    format!(
                        "logic_level assertion on '{net}': `min_width_ms` must be a positive \
                         time and `min`/`max` finite voltages"
                    ),
                )?;
                need(
                    self.level
                        .as_deref()
                        .is_none_or(|l| l == "high" || l == "low"),
                    format!(
                        "logic_level assertion on '{net}': {}",
                        unknown_token(
                            "unknown level",
                            self.level.as_deref().unwrap_or(""),
                            &["high", "low"]
                        )
                    ),
                )?;
            }
            "increases" | "decreases" => need(
                self.along.is_some() && self.reference.is_some() != self.net.is_some(),
                format!("{kind} assertion needs `along` and exactly one of `ref` or `net`"),
            )?,
            "fires" => {
                need(
                    self.reference.is_some(),
                    "fires assertion needs a `ref` naming the comparator".into(),
                )?;
                need(
                    self.target.is_none_or(f64::is_finite),
                    "fires assertion `target` must be a finite voltage".into(),
                )?;
                need(
                    self.reset_below.is_none_or(f64::is_finite),
                    "fires assertion `reset_below` must be a finite voltage".into(),
                )?;
                need(
                    self.tolerance
                        .is_none_or(|t| t.is_finite() && t > 0.0 && t <= 1.0),
                    "fires assertion `tolerance` is a fraction of `target` in (0, 1]".into(),
                )?;
            }
            "uart" => {
                need(
                    self.contains.is_some() || self.matches.is_some(),
                    "uart assertion needs `contains` or `matches`".into(),
                )?;
                if let Some(re) = &self.matches {
                    regex::Regex::new(re)
                        .map_err(SpecError::invalid("uart `matches` is not a valid regex"))?;
                }
            }
            "toggle" => {
                need(self.net.is_some(), "toggle assertion needs a `net`".into())?;
                need(
                    self.freq_hz.is_some() || self.min_toggles.is_some() || self.measurement(),
                    format!("toggle assertion on '{net}' needs `freq_hz` or `min_toggles`"),
                )?;
                // The two forms are mutually exclusive ("freq_hz OR
                // min_toggles"): check_toggle evaluates min_toggles first and
                // ignores freq_hz, yet the label reports the frequency form, so
                // a spec with both is silently evaluated as a count check while
                // claiming to be a ~N Hz check. Reject it rather than mislead.
                need(
                    self.freq_hz.is_none() || self.min_toggles.is_none(),
                    format!("toggle assertion on '{net}' sets both `freq_hz` and `min_toggles`; use one (frequency OR count)"),
                )?;
                // Counts accumulate from t=0 and a rate is timed between the
                // run's own edges, so an `after_ms` window would be ignored.
                need(
                    self.after_ms.is_none(),
                    format!("toggle assertion on '{net}' does not support `after_ms` (counts and rates both span the whole run)"),
                )?;
                // `tolerance` is a FRACTION of freq_hz (0.25 = +-25%), and the
                // check widens the accepted band by it. A value like 10 (someone
                // thinking in percent) accepts 5 Hz +-1000%, greening a net that
                // never toggles at all; a zero/negative value accepts nothing or
                // inverts the band. Only (0, 1] is meaningful.
                if let Some(tol) = self.tolerance {
                    need(
                        tol.is_finite() && tol > 0.0 && tol <= 1.0,
                        format!(
                            "toggle assertion on '{net}': tolerance is a fraction \
                             (0.25 = +-25%), got {tol}; did you mean {}?",
                            if tol > 1.0 {
                                format!("{}", tol / 100.0)
                            } else {
                                "a value in (0, 1]".to_string()
                            }
                        ),
                    )?;
                }
            }
            "no_faults" => {}
            "max_current" => need(
                self.reference.is_some() && (self.amps.is_some() || self.measurement()),
                "max_current assertion needs `ref` and `amps`".into(),
            )?,
            // `celsius` is optional: absent means "use the device's own max
            // junction temperature".
            "max_temp" => {
                need(
                    self.reference.is_some(),
                    "max_temp assertion needs a `ref` (the component to check)".into(),
                )?;
                // When given, `celsius` is a real ceiling: `inf` would pass
                // everything, `nan` compares false with everything, and one
                // past silicon's melting point can never be judged.
                let problem = self.celsius.and_then(|c| {
                    temperature_problem(
                        "max_temp `celsius`",
                        c,
                        crate::assertions::TJ_IMPOSSIBLE_C,
                        "past silicon's melting point no junction exists to judge",
                    )
                });
                need(problem.is_none(), problem.unwrap_or_default())?;
            }
            "peripheral" => {
                need(
                    self.id.is_some(),
                    "peripheral assertion needs an `id`".into(),
                )?;
                let id = self.id.as_deref().unwrap_or("?");
                need(
                    self.bytes.is_some() || (self.field.is_some() && has_bound),
                    format!("peripheral assertion on '{id}' needs `bytes` or a `field` with `min`/`max`"),
                )?;
                // The `bytes` and `field` forms are mutually exclusive:
                // check_peripheral evaluates `bytes` first and RETURNS, so a
                // spec that sets both silently drops the field/min/max constraint
                // (and label() reports only the bytes check), a false green if
                // the field bound is violated. Reject it, like toggle's
                // freq_hz+min_toggles rejection above.
                need(
                    self.bytes.is_none() || self.field.is_none(),
                    format!("peripheral assertion on '{id}' sets both `bytes` and `field`; use one (EEPROM-bytes OR a field range); a combined spec silently drops the field check"),
                )?;
            }
            "rail_window" => {
                need(
                    self.net.is_some(),
                    "rail_window assertion needs a `net`".into(),
                )?;
                // Reject partnerless overvoltage fields before the generic
                // "needs a check" error so a typo cannot silently no-op and
                // the diagnostic names the field that needs completion.
                need(
                    self.settle_within_ms.is_none()
                        || (self.spike_above.is_some() && self.settle_to.is_some()),
                    "rail_window `settle_within_ms` needs both `spike_above` and `settle_to`"
                        .into(),
                )?;
                need(
                    self.settle_to.is_none()
                        || (self.spike_above.is_some() && self.settle_within_ms.is_some()),
                    "rail_window `settle_to` needs both `spike_above` and `settle_within_ms`"
                        .into(),
                )?;
                need(
                    self.spike_for_max_ms.is_none() || self.spike_above.is_some(),
                    "rail_window `spike_for_max_ms` needs `spike_above` or it is never evaluated"
                        .into(),
                )?;
                let spike_partnered =
                    self.spike_for_max_ms.is_some() || self.settle_within_ms.is_some();
                need(
                    self.spike_above.is_none() || spike_partnered,
                    "rail_window `spike_above` needs `spike_for_max_ms` or `settle_within_ms` or it is never evaluated"
                        .into(),
                )?;
                let dip_partnered = self.for_max_ms.is_some() || self.recover_within_ms.is_some();
                need(
                    has_bound
                        || (self.dip_below.is_some() && dip_partnered)
                        || (self.spike_above.is_some() && spike_partnered),
                    format!("rail_window on '{net}' needs at least one of: `min`, `max`, `dip_below` with `for_max_ms`/`recover_within_ms`, or `spike_above` with `spike_for_max_ms`/`settle_within_ms`"),
                )?;
                need(
                    self.recover_within_ms.is_none()
                        || (self.dip_below.is_some() && self.recover_to.is_some()),
                    "rail_window `recover_within_ms` needs both `dip_below` and `recover_to`"
                        .into(),
                )?;
                // The converse: a `recover_to` (recovery intent) is only evaluated
                // by check_rail_window when `recover_within_ms` is also present
                // (its match binds all three). Without it the recovery check is a
                // silent no-op, accepted only because a `min`/`max` also happens
                // to be set, so the author's recovery constraint never runs. Fail
                // loud instead of passing a spec whose recovery clause does nothing.
                need(
                    self.recover_to.is_none() || self.recover_within_ms.is_some(),
                    "rail_window `recover_to` needs `recover_within_ms` (and `dip_below`) or it is never evaluated"
                        .into(),
                )?;
                // Same silent-no-op class for `dip_below`: check_rail_window only
                // reads it inside guards that require `for_max_ms` or
                // `recover_within_ms` as a partner. A bare `dip_below` alongside a
                // `min`/`max` (which satisfies the check above) is therefore never
                // evaluated; the author's dip threshold does nothing. Require it
                // to carry a partner that actually consumes it.
                need(
                    self.dip_below.is_none() || dip_partnered,
                    "rail_window `dip_below` needs `for_max_ms` or `recover_within_ms` or it is never evaluated"
                        .into(),
                )?;
                need(
                    !matches!((self.spike_above, self.settle_to), (Some(spike), Some(settle)) if settle > spike),
                    "rail_window `settle_to` must be <= `spike_above`".into(),
                )?;
            }
            "protection_trip" => {
                need(
                    self.supply_net.is_some(),
                    "protection_trip assertion needs a `supply_net`".into(),
                )?;
                need(
                    self.expect_trip.is_some(),
                    "protection_trip assertion needs `expect_trip = true|false`".into(),
                )?;
            }
            "phase_margin" => {
                need(
                    self.net.is_some(),
                    "phase_margin assertion needs a `net` (the loop break/output net)".into(),
                )?;
                need(
                    has_bound,
                    format!("phase_margin on '{net}' needs a `min` (and/or `max`) in degrees, e.g. min = 45"),
                )?;
            }
            "ac_gain" => {
                need(self.net.is_some(), "ac_gain assertion needs a `net`".into())?;
                need(
                    has_bound,
                    format!("ac_gain on '{net}' needs a `min` and/or `max` in dB"),
                )?;
            }
            "hwtrace" => need(
                self.trace.is_some(),
                "hwtrace assertion needs a `trace` (path to the trace.toml, relative \
                 to the spec file)"
                    .into(),
            )?,
            "boot_coverage" | "boot-coverage" => {
                need(
                    self.net.is_some(),
                    "boot_coverage assertion needs a `net` (the control net to watch)".into(),
                )?;
                need(
                    self.min.is_some(),
                    format!("boot_coverage assertion on '{net}' needs a `min` (the driven level in volts the firmware must reach)"),
                )?;
                need(
                    self.deadline_ms.is_some(),
                    format!("boot_coverage assertion on '{net}' needs a `deadline_ms` (the boot deadline)"),
                )?;
                need(
                    self.hold_ms.is_none_or(|h| h >= 0.0),
                    format!(
                        "boot_coverage assertion on '{net}': `hold_ms` must be >= 0 \
                         (0 = the level only needs to be reached; absent = hold \
                         through the whole deadline)"
                    ),
                )?;
            }
            "bus" => {
                let protocol = self.protocol.as_deref();
                need(
                    matches!(protocol, Some("i2c" | "spi")),
                    format!(
                        "bus assertion needs a `protocol`: {}",
                        unknown_token("unknown protocol", protocol.unwrap_or(""), &["i2c", "spi"])
                    ),
                )?;
                if protocol == Some("i2c") {
                    need(
                        self.addr.is_some_and(|a| a <= 0x7f),
                        "bus assertion over i2c needs an `addr`, the 7-bit device address (0 to 127)"
                            .into(),
                    )?;
                    need(
                        self.bytes.is_none(),
                        "bus assertion over i2c names a device by `addr`; `bytes` is for an spi frame"
                            .into(),
                    )?;
                } else {
                    need(
                        self.bytes.is_some(),
                        "bus assertion over spi needs `bytes`, the hex a frame must contain".into(),
                    )?;
                    need(
                        self.addr.is_none() && self.acked.is_none(),
                        "bus assertion over spi has no address to ACK; use `bytes`".into(),
                    )?;
                }
                if let Some(bytes) = &self.bytes {
                    let hex: String = bytes.chars().filter(|c| !c.is_whitespace()).collect();
                    need(
                        !hex.is_empty()
                            && hex.len().is_multiple_of(2)
                            && hex.chars().all(|c| c.is_ascii_hexdigit()),
                        format!(
                            "bus assertion `bytes` is whole hex bytes (\"48 69\"), got {bytes:?}"
                        ),
                    )?;
                }
                need(
                    self.min_transactions.is_none_or(|n| n >= 1),
                    "bus assertion `min_transactions` is at least 1".into(),
                )?;
            }
            "resets" => {
                need(
                    self.max_resets.is_some(),
                    "resets assertion needs `max_resets`, the most resets the MCU may take".into(),
                )?;
                need(
                    self.cause
                        .as_deref()
                        .is_none_or(|c| RESET_CAUSES.contains(&c)),
                    format!(
                        "resets assertion: {}",
                        unknown_token(
                            "unknown cause",
                            self.cause.as_deref().unwrap_or(""),
                            RESET_CAUSES
                        )
                    ),
                )?;
            }
            "reaches" => {
                need(
                    self.symbol.is_some() != self.not_parked_in.is_some(),
                    "reaches assertion needs exactly one of `symbol` (a function the firmware \
                     must reach) or `not_parked_in` (a function it must not end parked in)"
                        .into(),
                )?;
                need(
                    [&self.symbol, &self.not_parked_in]
                        .into_iter()
                        .flatten()
                        .all(|s| !s.trim().is_empty()),
                    "reaches assertion names an empty symbol".into(),
                )?;
                need(
                    self.deadline_ms.is_none_or(|d| d > 0.0),
                    "reaches assertion `deadline_ms` must be a positive time".into(),
                )?;
                need(
                    self.not_parked_in.is_none() || self.deadline_ms.is_none(),
                    "reaches assertion: `deadline_ms` bounds when `symbol` is reached; \
                     `not_parked_in` judges where the run ends"
                        .into(),
                )?;
            }
            "uart_baud" => {
                need(
                    self.baud.is_some_and(|b| b > 0),
                    "uart_baud assertion needs `baud`, the nominal rate (9600, 115200)".into(),
                )?;
                need(
                    self.tolerance
                        .is_none_or(|t| t.is_finite() && t > 0.0 && t <= 1.0),
                    "uart_baud assertion `tolerance` is a fraction of `baud` in (0, 1]; \
                     0.03 is +-3 %"
                        .into(),
                )?;
            }
            "model_coverage" => {
                // Caught here rather than at run time: an assertion with no
                // threshold would otherwise sit in a spec looking like a
                // coverage gate while checking nothing, which is the exact
                // failure this assertion exists to prevent.
                need(
                    self.min_critical.is_some()
                        || self.min_resolved.is_some()
                        || self.max_active_unresolved.is_some(),
                    "model_coverage assertion needs at least one of `min_critical` \
                     (fraction of active ICs bound), `min_resolved` (fraction of all \
                     parts bound) or `max_active_unresolved` (unresolved parts on \
                     connected nets)"
                        .into(),
                )?;
                for (name, v) in [
                    ("min_critical", self.min_critical),
                    ("min_resolved", self.min_resolved),
                ] {
                    need(
                        v.is_none_or(|v| (0.0..=1.0).contains(&v)),
                        format!(
                            "model_coverage `{name}` is a fraction between 0.0 and 1.0, got {}",
                            v.unwrap_or(0.0)
                        ),
                    )?;
                }
            }
            other => {
                return Err(SpecError::Invalid(format!(
                    "unknown assertion kind '{other}'{} (expected {}); see: hauksbee-ci explain",
                    crate::error::did_you_mean_hint(other, ASSERTION_KINDS),
                    ASSERTION_KINDS.join("|")
                )));
            }
        }
        // An inverted window (min > max) can never hold, so it always reads as
        // a hardware RED (exit 1) blaming the board for a bound no measurement
        // could satisfy. It is a spec error (exit 2): name both values here at
        // load, for every kind that takes a [min, max] window.
        let windowed = matches!(
            kind,
            "voltage" | "rail_window" | "phase_margin" | "ac_gain" | "peripheral"
        );
        match (self.min, self.max) {
            (Some(lo), Some(hi)) if windowed && lo > hi => Err(SpecError::Invalid(format!(
                "{kind} assertion '{}': min ({lo}) is greater than max ({hi}), a window \
                 nothing can satisfy; swap the bounds or fix the typo",
                self.label()
            ))),
            _ => Ok(()),
        }
    }

    /// A human label for this assertion.
    #[must_use]
    // One short format per assertion kind.
    #[allow(clippy::too_many_lines)]
    pub fn label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        let net = self.net.as_deref().unwrap_or_default();
        let reference = self.reference.as_deref().unwrap_or_default();
        let bound = |unit: &str, none: &str| {
            bound_label(self.min, self.max, |x| format!("{x}{unit}"), none)
        };
        let volt_bound = |none: &str| match (self.min, self.max) {
            (Some(lo), Some(hi)) => volt_range_label(lo, hi),
            _ => bound_label(self.min, self.max, format_volts, none),
        };
        match self.kind.as_str() {
            "voltage" => {
                let when = self
                    .after_ms
                    .map(|t| format!(" after {t}ms"))
                    .unwrap_or_default();
                format!("{net} {}{when}", volt_bound("voltage"))
            }
            "uart" => match (&self.contains, &self.matches) {
                (Some(c), _) => format!("UART contains {c:?}"),
                (None, Some(m)) => format!("UART matches /{m}/"),
                (None, None) => "UART".into(),
            },
            "toggle" => self.freq_hz.map_or_else(
                || format!("{net} toggles >= {} times", self.min_toggles.unwrap_or(0)),
                |f| format!("{net} toggles at ~{f} Hz"),
            ),
            "bus" => {
                let bus = self
                    .bus
                    .as_deref()
                    .map(|b| format!(" {b}"))
                    .unwrap_or_default();
                let n = self.min_transactions.unwrap_or(1);
                if self.protocol.as_deref() == Some("spi") {
                    format!(
                        "SPI{bus} frame contains {} (>= {n})",
                        self.bytes.as_deref().unwrap_or_default()
                    )
                } else {
                    format!(
                        "I2C{bus} 0x{:02x} {} (>= {n})",
                        self.addr.unwrap_or(0),
                        if self.acked.unwrap_or(true) {
                            "ACKs"
                        } else {
                            "NACKs"
                        }
                    )
                }
            }
            "resets" => format!(
                "MCU resets at most {} time(s){}",
                self.max_resets.unwrap_or(0),
                self.cause
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            ),
            "reaches" => match (&self.symbol, &self.not_parked_in) {
                (Some(s), _) => self.deadline_ms.map_or_else(
                    || format!("firmware reaches {s}"),
                    |d| format!("firmware reaches {s} within {d} ms"),
                ),
                (None, Some(p)) => format!("firmware is not parked in {p}"),
                (None, None) => "firmware reaches".into(),
            },
            "uart_baud" => format!(
                "UART runs at {} baud +-{}",
                self.baud.unwrap_or(0),
                format_percent(self.tolerance.unwrap_or(UART_BAUD_TOLERANCE) * 100.0, 1)
            ),
            "no_faults" => "no stress faults raised".into(),
            "logic_level" => format!(
                "{net} reaches a valid logic {}",
                self.level.as_deref().unwrap_or("high")
            ),
            "increases" | "decreases" => format!(
                "{} {} along {}",
                self.reference.as_deref().unwrap_or(net),
                self.kind,
                self.along.as_deref().unwrap_or_default()
            ),
            "fires" => self.target.map_or_else(
                || format!("{reference} fires"),
                |t| format!("{reference} fires at {}", format_volts(t)),
            ),
            // No bound is a measurement, not a limit of 0 A.
            "max_current" => self.amps.map_or_else(
                || format!("I({reference})"),
                |a| format!("I({reference}) <= {}", format_amps(a)),
            ),
            "max_temp" => self.celsius.map_or_else(
                || format!("Tj({reference}) <= device max"),
                |c| format!("Tj({reference}) <= {}", format_celsius_to(c, 3)),
            ),
            "peripheral" => {
                let id = self.id.as_deref().unwrap_or_default();
                match (&self.bytes, &self.field) {
                    (Some(b), _) => format!("peripheral {id} contains bytes {b}"),
                    (None, Some(f)) => format!("peripheral {id}.{f} {}", bound("", "set")),
                    (None, None) => format!("peripheral {id}"),
                }
            }
            "rail_window" => {
                let recover = self.recover_to.zip(self.recover_within_ms);
                let settle = self.settle_to.zip(self.settle_within_ms);
                let parts: Vec<String> = [
                    self.min.map(|lo| format!("min >= {}", format_volts(lo))),
                    self.max.map(|hi| format!("max <= {}", format_volts(hi))),
                    self.dip_below
                        .zip(self.for_max_ms)
                        .map(|(d, ms)| format!("dip <{} for <= {ms} ms", format_volts(d))),
                    self.dip_below.zip(recover).map(|(d, (r, ms))| {
                        format!(
                            "recover to {} within {ms} ms of dipping <{}",
                            format_volts(r),
                            format_volts(d)
                        )
                    }),
                    self.spike_above
                        .zip(self.spike_for_max_ms)
                        .map(|(s, ms)| format!("spike >{} for <= {ms} ms", format_volts(s))),
                    self.spike_above.zip(settle).map(|(s, (settle, ms))| {
                        format!(
                            "settle to <= {} within {ms} ms of spiking >{}",
                            format_volts(settle),
                            format_volts(s)
                        )
                    }),
                ]
                .into_iter()
                .flatten()
                .collect();
                let scope = self
                    .scenario
                    .as_ref()
                    .map(|s| format!(" [{s}]"))
                    .unwrap_or_default();
                format!("{net} window: {}{scope}", parts.join(", "))
            }
            "protection_trip" => format!(
                "{} protection {}",
                self.supply_net.as_deref().unwrap_or_default(),
                if self.expect_trip.unwrap_or(false) {
                    "trips"
                } else {
                    "does NOT trip"
                }
            ),
            "hwtrace" => {
                format!("hardware trace {}", self.trace.clone().unwrap_or_default())
            }
            "boot_coverage" | "boot-coverage" => {
                let hold = match self.hold_ms {
                    None => String::new(),
                    Some(h) if h > 0.0 => format!(", held {h} ms"),
                    Some(_) => ", reach only".to_string(),
                };
                format!(
                    "{net} driven to >= {} within {} ms of reset{hold}",
                    format_volts(self.min.unwrap_or(0.0)),
                    self.deadline_ms.unwrap_or(0.0)
                )
            }
            "phase_margin" => format!("loop {net} phase margin {}", bound(" deg", "phase margin")),
            "ac_gain" => {
                let at = self
                    .freq_hz
                    .map(|f| format!(" at {f} Hz"))
                    .unwrap_or_default();
                format!("{net} gain {}{at}", bound(" dB", "gain"))
            }
            other => other.to_string(),
        }
    }
}

/// `in [lo, hi]<unit>` / `>= lo<unit>` / `<= hi<unit>`, or `none` when the
/// assertion carries no bound at all.
/// `in [4.75, 5.25] V`: the unit stated once when both ends print with the
/// same prefix, on each end (`in [0 V, 50 mV]`) when they differ.
fn volt_range_label(lo: f64, hi: f64) -> String {
    let (lo, hi) = (format_volts(lo), format_volts(hi));
    match (lo.rsplit_once(' '), hi.rsplit_once(' ')) {
        (Some((l, lu)), Some((h, hu))) if lu == hu => format!("in [{l}, {h}] {lu}"),
        _ => format!("in [{lo}, {hi}]"),
    }
}

fn bound_label(
    min: Option<f64>,
    max: Option<f64>,
    show: impl Fn(f64) -> String,
    none: &str,
) -> String {
    match (min, max) {
        (Some(lo), Some(hi)) => format!("in [{}, {}]", show(lo), show(hi)),
        (Some(lo), None) => format!(">= {}", show(lo)),
        (None, Some(hi)) => format!("<= {}", show(hi)),
        (None, None) => none.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "board = \"b.kicad_pcb\"\nduration_ms = 10\n";
    const VCC: &str = "[[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\n";
    const CLK: &str = "[[assert]]\nkind = \"toggle\"\nnet = \"CLK\"\nmin_toggles = 1\n";
    const AC: &str = "[ac]\nfstart = 10.0\nfstop = 1e6\npoints = 10\n";
    const EEPROM: &str = "[[peripheral]]\nid = \"EE1\"\ntype = \"i2c_eeprom\"\n";

    fn spec_from(src: &str) -> Spec {
        let mut spec: Spec = toml::from_str(src).expect("valid toml");
        spec.base_dir = PathBuf::from(".");
        spec
    }

    fn validation_error(src: &str) -> String {
        spec_from(src)
            .validate()
            .expect_err("spec must be rejected")
            .to_string()
    }

    fn assert_invalid(src: &str, needles: &[&str]) {
        let error = validation_error(src);
        for needle in needles {
            assert!(error.contains(needle), "expected {needle:?} in: {error}");
        }
    }

    #[test]
    fn well_formed_specs_validate() {
        let bodies = [
            format!("{BASE}frame_ms = 0.1\n{VCC}"),
            format!("{BASE}{VCC}[[tolerance]]\nref = \"R1\"\npercent = 50\n"),
            format!("{BASE}{VCC}[[supply]]\nnet = \"VCC\"\nkind = \"ideal\"\nvolts = -12.0\n"),
            format!("{BASE}{VCC}[[supply]]\nnet = \"VCC\"\nkind = \"usb\"\nusb = \"5v0.5a\"\n"),
            format!(
                "{BASE}{VCC}[[supply]]\nnet = \"VCC\"\nkind = \"battery\"\nchemistry = \"liion\"\n\
                 cells = 3\ncapacity_mah = 2200\nsoc = 0.8\nvolts = 11.1\n\
                 protection_trip_a = 5.0\nprotection_delay_ms = 100\n"
            ),
            format!("{BASE}[[assert]]\nkind = \"toggle\"\nnet = \"LED\"\nfreq_hz = 5.0\ntolerance = 1.0\n"),
            format!("{BASE}[[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\nmax = 3.6\n"),
            format!(
                "{BASE}[[assert]]\nkind = \"rail_window\"\nnet = \"VBUS\"\ndip_below = 3.1\n\
                 recover_to = 3.3\nrecover_within_ms = 5.0\n"
            ),
            format!("{BASE}[[assert]]\nkind = \"rail_window\"\nnet = \"VBUS\"\ndip_below = 3.2\nfor_max_ms = 2.0\n"),
            format!("{BASE}{VCC}[[peripheral]]\nid = \"scope\"\ntype = \"vcd_sink\"\nnets = [\"CLK\"]\nvcd_path = \"w.vcd\"\n"),
            format!("{BASE}{EEPROM}[[assert]]\nkind = \"peripheral\"\nid = \"EE1\"\nbytes = \"48 69\"\n"),
            format!("{BASE}{EEPROM}[[assert]]\nkind = \"peripheral\"\nid = \"EE1\"\nfield = \"writes\"\nmin = 5\n"),
            format!("{BASE}{VCC}[[peripheral]]\nid = \"EE\"\ntype = \"i2c_eeprom\"\naddress = 0x50\nsize = 256\n"),
            format!("{BASE}{VCC}[[peripheral]]\nid = \"S1\"\ntype = \"stimulus\"\nnet = \"IN\"\nwaveform = \"sine\"\nfreq_hz = 50.0\n"),
            format!("{BASE}{VCC}[[scenario]]\npart = \"U5\"\nprofile = \"esp32_boot_wifi\"\nstart_ms = 5.0\n"),
            format!(
                "{BASE}{VCC}[decoupling]\nparasitics = true\n\
                 [[decoupling.override]]\nref = \"C1\"\nesr_ohms = 0.02\nesl_henries = 1e-9\n"
            ),
            format!("{BASE}timing = {{ min_pulse_us = 2.0, max_edge_error_us = 0.25 }}\n{CLK}"),
            format!("{BASE}[mcu]\nname = \"stm32f103\"\ndescriptor_dir = \"mcu\"\n{VCC}"),
        ];
        for body in bodies {
            let spec = spec_from(&body);
            assert!(spec.validate().is_ok(), "{body}\n{:?}", spec.validate());
        }
    }

    /// Every rejection names the field the author has to fix. Each case is a
    /// value that would otherwise run as a silently wrong (or hanging, or
    /// panicking) simulation rather than failing at load.
    #[test]
    fn malformed_specs_are_rejected_naming_the_offending_field() {
        let supply = |body: &str| format!("{BASE}{VCC}[[supply]]\nnet = \"VCC\"\n{body}\n");
        let cases: Vec<(String, &[&str])> = vec![
            // A tolerance of 100% or more makes the min corner a zero or
            // negative component value.
            (format!("{BASE}{VCC}[[tolerance]]\nref = \"R1\"\npercent = 100\n"), &["percent"]),
            (supply("kind = \"ideal\"\nvolts = nan"), &["volts"]),
            (supply("kind = \"bench\"\nvolts = 5.0\ncurrent_limit_a = -1.0"), &["current_limit_a"]),
            (supply("kind = \"battery\"\nchemistry = \"liion\"\nsoc = 5.0"), &["soc"]),
            (supply("kind = \"usb\"\nusb = \"5v9a\""), &["usb profile"]),
            (supply("kind = \"battery\"\nchemistry = \"unobtainium\""), &["chemistry"]),
            // A supply's defining parameter is written down, never defaulted.
            (supply("kind = \"ideal\""), &["volts", "VCC"]),
            (supply("kind = \"ideal\"\nvolts = 5.0\nref = \"J1\"\npin = \"1\""), &["either `net` or both"]),
            (supply("kind = \"battery\""), &["chemistry", "VCC"]),
            // toggle tolerance is a fraction; the percent-style slip gets the fix.
            (
                format!("{BASE}[[assert]]\nkind = \"toggle\"\nnet = \"LED\"\nfreq_hz = 5.0\ntolerance = 10\n"),
                &["fraction", "did you mean 0.1"],
            ),
            (
                format!("{BASE}[[assert]]\nkind = \"toggle\"\nnet = \"D13\"\nfreq_hz = 5\nmin_toggles = 1\n"),
                &["both", "freq_hz"],
            ),
            // An inverted window is a spec error, not a hardware red.
            (format!("{BASE}[[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 5.0\nmax = 3.0\n"), &["greater than max"]),
            (format!("{BASE}{AC}[[assert]]\nkind = \"ac_gain\"\nnet = \"OUT\"\nmin = 20\nmax = 10\n"), &["greater than max"]),
            (
                format!("{BASE}{EEPROM}[[assert]]\nkind = \"peripheral\"\nid = \"EE1\"\nfield = \"writes\"\nmin = 9\nmax = 2\n"),
                &["greater than max"],
            ),
            // Time fields are positive and finite, or the frame loop spins or starves.
            (format!("{BASE}frame_ms = 0\n{VCC}"), &["frame_ms"]),
            (format!("{BASE}frame_ms = inf\n{VCC}"), &["frame_ms"]),
            (format!("board = \"b.kicad_pcb\"\nduration_ms = inf\n{VCC}"), &["duration_ms"]),
            (format!("{BASE}[[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\nafter_ms = nan\n"), &["after_ms"]),
            (format!("{BASE}timing = {{ min_pulse_us = 0.0 }}\n{CLK}"), &["positive, finite"]),
            (format!("{BASE}[ac]\nfstart = 10.0\nfstop = inf\npoints = 20\nsweep = \"dec\"\n{VCC}"), &["finite"]),
            // rail_window clauses that would otherwise silently never run.
            (
                format!("{BASE}[[assert]]\nkind = \"rail_window\"\nnet = \"VBUS\"\nmin = 3.0\ndip_below = 3.1\nrecover_to = 3.3\n"),
                &["recover_to", "recover_within_ms"],
            ),
            (format!("{BASE}[[assert]]\nkind = \"rail_window\"\nnet = \"VBUS\"\nmin = 3.0\ndip_below = 3.2\n"), &["dip_below"]),
            // Peripheral shapes.
            (
                format!("{BASE}{VCC}[[peripheral]]\nid = \"scope\"\ntype = \"vcd_sink\"\nnet = \"CLK\"\nvcd_path = \"w.vcd\"\n"),
                &["nets"],
            ),
            (
                format!("{BASE}{EEPROM}[[assert]]\nkind = \"peripheral\"\nid = \"EE1\"\nbytes = \"48 69\"\nfield = \"writes\"\nmin = 5\n"),
                &["bytes", "field"],
            ),
            (
                format!("{BASE}{EEPROM}[[assert]]\nkind = \"peripheral\"\nid = \"TYPO\"\nfield = \"writes\"\nmin = 1\n"),
                &["TYPO", "EE1"],
            ),
            // Bounds the published editor schema documents.
            (format!("{BASE}{VCC}[[peripheral]]\nid = \"EE\"\ntype = \"i2c_eeprom\"\naddress = 200\n"), &["address"]),
            (
                format!("{BASE}{VCC}[[peripheral]]\nid = \"S1\"\ntype = \"stimulus\"\nnet = \"IN\"\nwaveform = \"square\"\n"),
                &["waveform"],
            ),
            (format!("{BASE}{VCC}[[scenario]]\npart = \"U5\"\nprofile = \"esp32_boot_wifi\"\nstart_ms = -5.0\n"), &["start_ms"]),
            (
                format!("{BASE}{VCC}[decoupling]\nparasitics = true\n[[decoupling.override]]\nref = \"C1\"\nesr_ohms = -0.1\n"),
                &["esr_ohms"],
            ),
        ];
        for (body, needles) in cases {
            assert_invalid(&body, needles);
        }
    }

    #[test]
    fn timing_requirement_parses_into_its_fields() {
        let spec = spec_from(&format!(
            "{BASE}timing = {{ min_pulse_us = 2.0, max_edge_error_us = 0.25 }}\n{CLK}"
        ));
        let timing = spec.timing.expect("timing request");
        assert_eq!(timing.min_pulse_us, Some(2.0));
        assert_eq!(timing.max_edge_error_us, Some(0.25));
    }

    #[test]
    fn cs_net_is_board_validated_like_every_other_net() {
        let spec = spec_from(&format!(
            "{BASE}[[peripheral]]\nid = \"EE\"\ntype = \"spi_eeprom\"\ncs_net = \"CS1\"\n{VCC}"
        ));
        assert!(spec.referenced_nets().iter().any(|(n, _)| n == "CS1"));
        let board_without = vec!["SPI_CS1".to_string(), "VCC".to_string()];
        assert!(
            spec.check_nets(&board_without).is_err(),
            "a cs_net not on the board must fail loud, not silently degrade framing"
        );
        let board_with = vec!["CS1".to_string(), "VCC".to_string()];
        assert!(spec.check_nets(&board_with).is_ok());
    }

    #[test]
    fn mcu_accepts_both_the_note_string_and_the_config_table() {
        let note = spec_from(&format!("{BASE}mcu = \"atmega328p\"\n{VCC}"));
        assert_eq!(note.mcu_note(), Some("atmega328p"));
        assert_eq!(note.mcu_descriptor_dir(), None);

        let mut table = spec_from(&format!(
            "{BASE}[mcu]\nname = \"stm32f103\"\ndescriptor_dir = \"mcu-overrides\"\n{VCC}"
        ));
        table.base_dir = PathBuf::from("/repo/ci");
        assert_eq!(table.mcu_note(), Some("stm32f103"));
        assert_eq!(
            table.mcu_descriptor_dir(),
            Some(PathBuf::from("/repo/ci/mcu-overrides")),
            "descriptor_dir resolves relative to the spec's directory"
        );

        let mut abs = spec_from(&format!(
            "{BASE}[mcu]\ndescriptor_dir = \"/opt/socs\"\n{VCC}"
        ));
        abs.base_dir = PathBuf::from("/repo/ci");
        assert_eq!(abs.mcu_descriptor_dir(), Some(PathBuf::from("/opt/socs")));
    }

    #[test]
    fn mcu_table_errors_name_the_key_and_the_fix() {
        let parse_err = |src: &str| {
            toml::from_str::<Spec>(src)
                .expect_err("the spec must be rejected")
                .to_string()
        };
        let err = parse_err(&format!("{BASE}[mcu]\ndescriptor_dirr = \"mcu\"\n"));
        assert!(
            err.contains("unknown field `descriptor_dirr`") && err.contains("descriptor_dir"),
            "the mistyped key is named and the legal ones offered: {err}"
        );
        // `[mcu]` placed before a top-level scalar swallows it; the hint names the fix.
        let err = parse_err(
            "board = \"board.kicad_pcb\"\n[mcu]\nname = \"stm32f103\"\nduration_ms = 10\n",
        );
        assert!(
            err.contains("`duration_ms` is a top-level key; move it above the [mcu] table"),
            "{err}"
        );
    }

    // `frame_ms` must sit at the top level, BEFORE the [[assert]] table, or TOML
    // captures it as a field of the assert.
    fn spec_src(frame_ms: &str) -> String {
        format!(
            r#"
name = "t"
board = "board.kicad_pcb"
duration_ms = 10
frame_ms = {frame_ms}

[[assert]]
kind = "voltage"
net = "VCC"
min = 3.0
"#
        )
    }
    #[test]
    fn toggle_with_both_freq_and_count_is_rejected() {
        // Round-29: check_toggle evaluates min_toggles and ignores freq_hz when
        // both are set, yet the label reports the ~N Hz frequency form. Reject
        // both-at-once instead of silently checking a count as a frequency.
        assert_invalid(
            &format!("{BASE}frame_ms=0.1\n[[assert]]\nkind=\"toggle\"\nnet=\"D13\"\nfreq_hz=5\nmin_toggles=1\n"),
            &["both", "freq_hz"],
        );
        // Each form ALONE is still accepted.
        for one in ["freq_hz = 5", "min_toggles = 1"] {
            let src = format!(
                "name=\"t\"\nboard=\"b.kicad_pcb\"\nduration_ms=10\nframe_ms=0.1\n\n[[assert]]\nkind=\"toggle\"\nnet=\"D13\"\n{one}\n"
            );
            assert!(
                spec_from(&src).validate().is_ok(),
                "one field is valid: {one}"
            );
        }
    }
    #[test]
    fn non_positive_frame_ms_is_rejected() {
        // Round-26: a zero/negative frame_ms was silently clamped to 1 µs downstream,
        // running ~1000x more frames than any real cadence and hanging the check with
        // no explanation. Validation must name it up front rather than clamp silently.
        for bad in ["0", "-0.5"] {
            assert_invalid(&spec_src(bad), &["frame_ms"]);
        }
    }
    #[test]
    fn positive_frame_ms_passes_validation() {
        assert!(
            spec_from(&spec_src("0.1")).validate().is_ok(),
            "a positive frame_ms is a valid cadence"
        );
    }
    #[test]
    fn non_finite_time_fields_are_rejected() {
        // R33: TOML accepts `inf`/`nan`. `duration_ms = inf` passed the `<= 0`
        // check and made the frame loop `t < inf` spin forever (a silent CI hang);
        // `nan` ran zero frames so every assertion failed "never sampled". Both
        // duration_ms and frame_ms must reject non-finite values.
        let base = |dur: &str, frame: &str| {
            format!(
                r#"
name = "t"
board = "board.kicad_pcb"
duration_ms = {dur}
frame_ms = {frame}

[[assert]]
kind = "voltage"
net = "VCC"
min = 3.0
"#
            )
        };
        for (dur, frame, field) in [
            ("inf", "1", "duration_ms"),
            ("nan", "1", "duration_ms"),
            ("10", "inf", "frame_ms"),
            ("10", "nan", "frame_ms"),
        ] {
            assert_invalid(&base(dur, frame), &[field]);
        }
    }
    /// Every temperature a spec states is finite, above absolute zero and
    /// physically bounded; a legal spelling of each is still accepted.
    #[test]
    fn temperatures_must_be_finite_and_physical() {
        let with_ambient = |v: &str| format!("{BASE}ambient_c = {v}\n{VCC}");
        for bad in ["nan", "inf", "-inf", "-300", "-273.15", "1000", "1e5"] {
            assert_invalid(&with_ambient(bad), &["ambient_c", "absolute zero"]);
        }
        for good in ["25", "-40", "85.5", "-0.0", "500", "-273.0"] {
            let spec = spec_from(&with_ambient(good));
            assert!(
                spec.validate().is_ok(),
                "ambient_c = {good}: {:?}",
                spec.validate()
            );
        }
        let ceiling = |v: &str| {
            format!("{BASE}[[assert]]\nkind = \"max_temp\"\nref = \"R1\"\ncelsius = {v}\n")
        };
        for bad in ["nan", "inf", "-inf", "-300", "-273.15", "1e9"] {
            assert_invalid(&ceiling(bad), &["celsius", "absolute zero"]);
        }
        for good in ["125", "0", "-0.0", "-40", "150.5"] {
            let spec = spec_from(&ceiling(good));
            assert!(
                spec.validate().is_ok(),
                "celsius = {good}: {:?}",
                spec.validate()
            );
        }
        // One spelling in the label: a space before the unit, and never `-0`.
        let spec = spec_from(&ceiling("-0.0"));
        assert_eq!(spec.asserts[0].label(), "Tj(R1) <= 0 C");
        let spec = spec_from(&ceiling("125"));
        assert_eq!(spec.asserts[0].label(), "Tj(R1) <= 125 C");
    }

    /// A finite but absurd run length is refused up front with the limit; the
    /// longest legal run, and the same duration at a coarser cadence, are not.
    #[test]
    fn an_absurd_run_length_is_refused_and_a_long_legal_one_is_not() {
        let run = |dur: &str, frame: &str| {
            format!("name = \"t\"\nboard = \"b.kicad_pcb\"\nduration_ms = {dur}\nframe_ms = {frame}\n{VCC}")
        };
        for (dur, frame, needle) in [
            ("1e15", "1", "ten minutes"),
            ("600001", "1000", "ten minutes"),
            ("10", "1e-6", "co-sim frames"),
            ("600000", "0.1", "co-sim frames"),
        ] {
            assert_invalid(&run(dur, frame), &[needle]);
        }
        for (dur, frame) in [("600000", "1"), ("2000", "0.01"), ("100", "1")] {
            let spec = spec_from(&run(dur, frame));
            assert!(
                spec.validate().is_ok(),
                "{dur} @ {frame}: {:?}",
                spec.validate()
            );
        }
    }

    #[test]
    fn nan_after_ms_is_rejected_not_panicked() {
        // R33: a NaN made threshold sorting panic instead of returning a
        // fail-loud SpecError. Validation rejects the non-finite window.
        assert_invalid(
            &format!("{BASE}[[assert]]\nkind=\"voltage\"\nnet=\"VCC\"\nmin=3.0\nafter_ms=nan\n"),
            &["after_ms"],
        );
    }
    const RAIL_WINDOW: &str = "[[assert]]\nkind=\"rail_window\"\nnet=\"VBUS\"\ndip_below=3.1\n";

    #[test]
    fn rail_window_dip_and_recovery_fields_require_partners() {
        // R35/R36: each field must be consumed by a complete duration clause;
        // otherwise it silently does nothing and can false-GREEN a run. The
        // `min` matters: it satisfies the has-a-check rule on its own, which is
        // exactly how the unconsumed field used to slip through.
        assert_invalid(
            &format!("{BASE}{RAIL_WINDOW}min=3.0\nrecover_to=3.3\n"),
            &["recover_to", "recover_within_ms"],
        );
        assert_invalid(
            &format!("{BASE}{RAIL_WINDOW}min=3.0\n"),
            &["dip_below", "for_max_ms"],
        );

        // Each complete form still validates, with no `min` beside it.
        for fields in [
            "recover_to=3.3\nrecover_within_ms=5.0\n",
            "for_max_ms=2.0\n",
        ] {
            assert!(spec_from(&format!("{BASE}{RAIL_WINDOW}{fields}"))
                .validate()
                .is_ok());
        }
    }
    #[test]
    fn rail_window_spike_schema_requires_consuming_partners() {
        let base = |fields: &str| {
            spec_from(&format!(
                "name=\"t\"\nboard=\"b.kicad_pcb\"\nduration_ms=10\nframe_ms=1.0\n\n[[assert]]\nkind=\"rail_window\"\nnet=\"VBUS\"\n{fields}"
            ))
        };

        for (fields, needle) in [
            ("spike_above=3.6", "spike_above"),
            ("spike_for_max_ms=2.0", "spike_for_max_ms"),
            ("settle_to=3.4", "settle_to"),
            ("settle_within_ms=10.0", "settle_within_ms"),
        ] {
            let err = base(fields)
                .validate()
                .expect_err("a partnerless overvoltage field must be rejected");
            assert!(
                err.to_string().contains(needle),
                "error for {fields} should name {needle}: {err}"
            );
        }

        let valid =
            base("spike_above=3.6\nspike_for_max_ms=2.0\nsettle_to=3.4\nsettle_within_ms=10.0");
        assert!(
            valid.validate().is_ok(),
            "a complete spike/settling assertion must validate"
        );
    }
    #[test]
    fn rail_window_spike_duration_and_settling_validation_is_fail_loud() {
        let base = |fields: &str| {
            spec_from(&format!(
                "name=\"t\"\nboard=\"b.kicad_pcb\"\nduration_ms=10\nframe_ms=1.0\n\n[[assert]]\nkind=\"rail_window\"\nnet=\"VBUS\"\nspike_above=3.6\n{fields}"
            ))
        };
        let err = base("spike_for_max_ms=-1.0")
            .validate()
            .expect_err("a negative spike duration must be rejected");
        assert!(err.to_string().contains("non-negative"), "got {err}");

        let err = base("settle_to=3.8\nsettle_within_ms=10.0")
            .validate()
            .expect_err("settling above the spike threshold must be rejected");
        assert!(err.to_string().contains("settle_to"), "got {err}");

        let err = base("settle_to=3.4\nsettle_within_ms=nan")
            .validate()
            .expect_err("a non-finite settling duration must be rejected");
        assert!(err.to_string().contains("settle_within_ms"), "got {err}");
    }
    #[test]
    fn vcd_sink_with_singular_net_is_rejected() {
        // R42: attach_peripherals reads a vcd_sink's logged signals ONLY from
        // `p.nets`. A singular `net = "CLK"` (the natural mistake, every other
        // control uses `net`) validated clean and then logged an EMPTY waveform
        // with no diagnostic. vcd_sink must require `nets`.
        let assert_block = "\n[[assert]]\nkind=\"voltage\"\nnet=\"VCC\"\nmin=3.0\n";
        let bad = spec_from(&format!(
            "name=\"t\"\nboard=\"b.kicad_pcb\"\nduration_ms=10\nframe_ms=1.0\n\n[[peripheral]]\nid=\"scope\"\ntype=\"vcd_sink\"\nnet=\"CLK\"\nvcd_path=\"w.vcd\"\n{assert_block}"
        ));
        let err = bad
            .validate()
            .expect_err("a vcd_sink with a singular `net` must fail, not log an empty VCD");
        assert!(
            matches!(&err, SpecError::Invalid(m) if m.contains("nets")),
            "the error must point the user at `nets`, got {err:?}"
        );
        // The correct plural `nets` form validates.
        let ok = spec_from(&format!(
            "name=\"t\"\nboard=\"b.kicad_pcb\"\nduration_ms=10\nframe_ms=1.0\n\n[[peripheral]]\nid=\"scope\"\ntype=\"vcd_sink\"\nnets=[\"CLK\"]\nvcd_path=\"w.vcd\"\n{assert_block}"
        ));
        assert!(
            ok.validate().is_ok(),
            "a vcd_sink with `nets = [...]` must pass: {:?}",
            ok.validate()
        );
    }
}
