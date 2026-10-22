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

use hauksbee_ir::evidence::{
    ModelIntervalKind, ModelSourceTier, ModelUncertainty, ModelValidation,
};

use crate::check::positive_finite;
use crate::peripheral_limits::PeripheralLimits;
use hauksbee_ir::units::format_volts;

// ── Top-level file container ──────────────────────────────────────────────────

/// Contents of one `.toml` database file (the `[[models]]` array).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DbFile {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

// ── Model entry ───────────────────────────────────────────────────────────────

/// A single model entry in the database.
///
/// Every field but `id` and `kind` defaults, so a construction site spells the
/// fields it means and takes the rest from `..Default::default()`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModelEntry {
    /// Stable identifier used in diagnostics and as a cross-reference key.
    pub id: String,

    /// Component kind, drives which param fields are required by the solver.
    pub kind: ComponentKind,

    /// For a [`ComponentKind::Passive`] entry, *which* two-terminal element it
    /// is. [`ComponentKind`] deliberately lumps R, C, L, ferrites, crystals,
    /// fuses and net ties into one `Passive` arm because they share a solver
    /// path shape; this field is the DB's own answer to the finer question, so a
    /// consumer that needs "is this a plain resistor?" reads curated evidence
    /// instead of guessing from the board file's reference designator (which is
    /// whatever the CAD user typed). `None` on a non-passive entry, and on a
    /// pack / user entry that has not declared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passive_class: Option<PassiveClass>,

    /// Human-readable description for reports.
    #[serde(default)]
    pub description: String,

    /// Match rules; at least one must be present per entry.
    #[serde(default)]
    pub r#match: MatchRules,

    /// Kind-specific simulation parameters.
    #[serde(default)]
    pub params: Params,

    /// Digital logic levels that move with the part's supply, keyed `vih`,
    /// `vil`, `voh` or `vol`: ascending `[vcc, volts]` points copied from the
    /// datasheet's per-supply columns (or the edges of its supply bands). The
    /// level at a rail between two points is linear between them and holds at
    /// the first or last point beyond them. A level without a table stays the
    /// plain `params` value, taken at the cited corner.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub levels: BTreeMap<String, Vec<[f64; 2]>>,

    /// Dual-supply parts: supply role -> the pin roles whose levels are read at
    /// that rail instead of `vcc`/`vdd` (a translator's A port at `vcca`, its B
    /// port at `vccb`). While any rail named here is unpowered every output is
    /// high impedance, the xT45 family's VCC-isolation behaviour.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub level_rails: BTreeMap<String, Vec<String>>,

    /// Pad-number to role mapping.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,

    /// `[[models.supply_outputs]]`: rails this part SOURCES on its own pins
    /// while it is fitted and powered, such as a dev-board module's regulated
    /// `5V`/`3V3` header pins fed from its USB connector or on-board regulator.
    /// A board that takes its supply from such a pin has that rail at the
    /// declared level instead of an unknown one. A net something else already
    /// defines (a regulator on the carrier board, a named rail) keeps that.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supply_outputs: Vec<SupplyOutput>,

    /// `[[models.optional_supplies]]`: supply-role pins the datasheet lets a
    /// board leave unconnected (an AVR's `UVCC` when the USB pad regulator is
    /// bypassed and `UCAP` fed directly). The power-pin check does not hold
    /// the board to them; each names the figure or table that says so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_supplies: Vec<OptionalSupply>,

    /// Pin-scoped recommended operating conditions and rail relationships.
    /// Every entry carries its own source basis so a static finding can quote
    /// the exact table row that authorizes the constraint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub envelope: Vec<OperatingEnvelope>,

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

    /// `[[models.pin_bias]]`: non-strap pins the datasheet says need a pull
    /// resistor or a defined idle level (an open-drain flag that needs a
    /// pull-up, a bootloader UART RX that must idle high), each with the level,
    /// an optional resistance range and the table it is read from. A boot
    /// strap is [`Self::straps`], never a row here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pin_bias: Vec<PinBias>,

    /// `[[models.supply_cap]]`: the least capacitance the datasheet requires
    /// (or recommends) on a supply input or an output pin, to ground. Read by
    /// the supply-capacitance check against the capacitors on that net.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supply_cap: Vec<SupplyCap>,

    /// `[models.pin_capability]`: which pads can serve a given peripheral
    /// function, as structured data a board check can read (today the pads that
    /// can be an ADC input, with the channel each one is). MCU cards only.
    #[serde(default, skip_serializing_if = "PinCapability::is_empty")]
    pub pin_capability: PinCapability,

    /// `[models.peripherals]`: the datasheet limits of the MCU's UART and ADC
    /// (clock sources, baud generator, conversion timing, sampling front end),
    /// each block with its source URL and a basis. MCU cards only; see
    /// [`crate::peripheral_limits`].
    #[serde(default, skip_serializing_if = "PeripheralLimits::is_empty")]
    pub peripherals: PeripheralLimits,

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

    /// Pins whose resistor to ground selects one row of a published table
    /// (a strap that sets a float voltage, an input-current limit, an output
    /// voltage): the selected row's values become params at bind time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resistor_bins: Vec<ResistorBins>,

    /// Optional firmware-visible peripheral protocol implemented by this part.
    /// The board binder turns this data into an I2C/SPI slave automatically;
    /// users should not have to repeat a datasheet's address/geometry in every
    /// CI spec after the fitted component has resolved exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peripheral: Option<PeripheralSpec>,

    /// Electrical load coupled to the firmware-visible peripheral state.
    ///
    /// A protocol model that answers bytes but never draws power is not a full
    /// board model: EEPROM writes, flash reads and erase/program operations can
    /// be the load that exposes a marginal rail.  This separate block lets the
    /// engine stamp one rail-current leg and select its source-bound idle/read/
    /// write current from the transactions the peripheral actually executed.
    /// It is valid only alongside [`Self::peripheral`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peripheral_power: Option<PeripheralPower>,

    /// The orderable-variant family this entry is one member of. Siblings share
    /// it and the family match; `[models.variant]` tells them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<crate::variants::VariantSpec>,

    /// Machine-readable scope of the behavior this model actually implements.
    ///
    /// A model resolving is not the same claim as the whole component being
    /// represented. For example, a regulator may stamp only its nominal DC
    /// output while omitting startup, current limiting, switching ripple and
    /// thermal shutdown. Keeping those two lists in the card lets authoring,
    /// reports and analysis requirements distinguish a useful partial model
    /// from either an identity-only card or a model complete for a requested
    /// behavior profile. Legacy cards with neither list remain executable, but
    /// their scope is explicitly `unspecified` on the coverage surface.
    #[serde(default, skip_serializing_if = "ModelCoverage::is_empty")]
    pub coverage: ModelCoverage,

    /// `[models.source]`: provenance tier, citations and declared uncertainty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ModelSourceSpec>,

    /// The temperature curves read from `source`'s tagged rating rows, kept
    /// when the library takes `source` into the resolution's provenance.
    #[serde(skip)]
    pub derating: crate::derating::Derating,
    /// `source`'s pinout standard, kept likewise (see `validation::pin_standard`).
    #[serde(skip)]
    pub pin_standard: Option<String>,
    /// The package `[models.pins]` is read for, kept likewise (see `validation::pin_package`).
    #[serde(skip)]
    pub pin_package: Option<String>,
    /// `source`'s `[[models.source.doubt]]` rows, kept likewise: what a drafted card
    /// left unconfirmed ([`crate::card_trust`]).
    #[serde(skip)]
    pub doubts: Vec<Doubt>,
}

/// One rail a part sources on its own pin (see [`ModelEntry::supply_outputs`]).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct SupplyOutput {
    /// The `[models.pins]` role of the pin that sources the rail.
    pub role: String,
    /// The level a solve stamps on the rail, V.
    pub nominal: f64,
    /// The lowest level the rail can sit at across the part's source paths, V.
    pub min: f64,
    /// The highest, V.
    pub max: f64,
    /// Where the range comes from: the source path(s) and the datasheet rows.
    pub basis: String,
}

/// `[models.pin_capability]`: the peripheral functions the pads can serve.
///
/// A table is a COMPLETE statement about the package it sits on: a table that
/// is present says which pads have the function and that every other pad of the
/// card's `[models.pins]` does not. A card with no table says nothing, and a
/// check that reads it abstains. Each table carries the datasheet table and
/// page it was read from (`basis`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinCapability {
    /// `[models.pin_capability.adc]`: the pads wired to an ADC input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adc: Option<AdcPins>,
}

impl PinCapability {
    /// No capability table is declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.adc.is_none()
    }
}

/// The ADC-capable pads of an MCU package (see [`PinCapability::adc`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcPins {
    /// The datasheet table and page the pad list is read from, and anything a
    /// firmware author needs beside it (a channel set that is unusable while a
    /// radio is on).
    pub basis: String,
    /// Pad number to the ADC channel on it (`ADC1_IN5`, `AIN2`, `ADC0`). A pad
    /// listed here is an analogue input; a pad of `[models.pins]` not listed is
    /// not one. A part with no ADC declares the table empty.
    #[serde(default)]
    pub pads: BTreeMap<String, String>,
}

impl ModelEntry {
    /// Whether the card says which of its pads are ADC inputs.
    #[must_use]
    pub const fn adc_declared(&self) -> bool {
        self.pin_capability.adc.is_some()
    }

    /// The ADC channel on `pad`, where the card lists the pad as one.
    #[must_use]
    pub fn adc_channel(&self, pad: &str) -> Option<&str> {
        self.pin_capability
            .adc
            .as_ref()?
            .pads
            .get(pad)
            .map(String::as_str)
    }
}

/// A supply pin a board may leave open (see [`ModelEntry::optional_supplies`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptionalSupply {
    /// The `[models.pins]` role of the pin.
    pub role: String,
    /// The datasheet figure, table or sentence that lets it be left open.
    pub basis: String,
    /// The role of the on-chip regulator's input when this pin is that
    /// regulator's output in a datasheet configuration (`CP210x` `VDD` made from
    /// `REGIN`): while the input is fed, the pin feeds the part's other supplies
    /// tied to it (the CP2104's `VIO` on `VDD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regulated_from: Option<String>,
}

/// A `[models.levels]` table (two or more ascending `[vcc, volts]` points) read
/// at `vcc`: linear between points, flat beyond the first and last.
#[must_use]
pub fn level_at(table: &[[f64; 2]], vcc: f64) -> f64 {
    let i = table
        .partition_point(|p| p[0] < vcc)
        .clamp(1, table.len().max(2) - 1);
    let (a, b) = (table[i - 1], table[i]);
    (b[1] - a[1]).mul_add(((vcc - a[0]) / (b[0] - a[0])).clamp(0.0, 1.0), a[1])
}

impl ModelEntry {
    /// The supply role a logic input `role` is referenced to: the
    /// `[models.level_rails]` entry naming it, else `vcc`, else `vdd`.
    #[must_use]
    pub fn level_rail_of(&self, role: &str) -> Option<&str> {
        let named = self
            .level_rails
            .iter()
            .find(|(_, roles)| roles.iter().any(|r| r == role));
        let has = |r: &str| self.pins.values().any(|p| p == r);
        named
            .map(|(rail, _)| rail.as_str())
            .or_else(|| ["vcc", "vdd"].into_iter().find(|r| has(r)))
    }

    /// The input band `(VIL, VIH)` at supply `vcc`: the `[models.levels]`
    /// tables where the card has both, else the flat `vil`/`vih` params.
    #[must_use]
    pub fn input_band_at(&self, vcc: f64) -> Option<(f64, f64)> {
        let tabled = |k: &str| {
            self.levels
                .get(k)
                .filter(|t| t.len() >= 2)
                .map(|t| level_at(t, vcc))
        };
        match (tabled("vil"), tabled("vih")) {
            (Some(vil), Some(vih)) => Some((vil, vih)),
            _ => Some((self.params.get_f64("vil")?, self.params.get_f64("vih")?)),
        }
        .filter(|(vil, vih)| vil < vih)
    }
}

/// Declared executable scope of a model card.
///
/// `implements` and `missing` are capability identifiers rather than prose so
/// callers can ask whether a requested analysis is covered. Descriptions and
/// citations still belong beside the source-bound parameters in the card.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelCoverage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

/// Data-driven firmware-visible peripheral behavior.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeripheralSpec {
    /// 24Cxx-compatible I2C EEPROM array.
    I2cEeprom {
        address: u8,
        size_bytes: usize,
        page_size: usize,
        word_address_bytes: u8,
    },
    /// JEDEC SPI NOR flash with three-byte addressing.
    SpiNorFlash {
        size_bytes: usize,
        page_size: usize,
        sector_size: usize,
        jedec_id: Vec<u8>,
        #[serde(default)]
        spi_mode: u8,
        #[serde(default = "default_cs_role")]
        cs_role: String,
        #[serde(default = "default_clk_role")]
        clk_role: String,
        #[serde(default = "default_mosi_role")]
        mosi_role: String,
        #[serde(default = "default_miso_role")]
        miso_role: String,
    },
    /// A datasheet-derived register map executed by the generic I2C/SPI
    /// interpreter. Keeping the validated TOML bytes in the model card makes
    /// the peripheral portable with a model pack: once the exact component
    /// resolves, the board binder can attach its firmware-visible behavior
    /// without a second scenario-only sensor declaration.
    RegisterMap {
        spec_toml: String,
        /// Optional named SPI controller (for multi-controller MCUs). I2C
        /// currently uses the backend's modeled I2C controller selection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        controller: Option<String>,
        #[serde(default = "default_scl_role")]
        scl_role: String,
        #[serde(default = "default_sda_role")]
        sda_role: String,
        #[serde(default = "default_cs_role")]
        cs_role: String,
        #[serde(default = "default_clk_role")]
        clk_role: String,
        #[serde(default = "default_mosi_role")]
        mosi_role: String,
        #[serde(default = "default_miso_role")]
        miso_role: String,
        /// Board straps that select this bus personality, e.g. CSB=high
        /// selects I2C. A strap the board holds at the other level (directly,
        /// through a closed link, or by a pull) leaves the personality
        /// unattached; one it does not hold (floating or driven by another
        /// part) attaches it with the level disclosed as assumed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        required_high_roles: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        required_low_roles: Vec<String>,
        /// Optional I2C address-select strap. All three fields are an
        /// all-or-none contract; a floating or otherwise unresolved strap
        /// takes the spec's default address, disclosed as assumed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address_select_role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address_when_low: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address_when_high: Option<u8>,
    },
}

/// Datasheet current envelope for a firmware-visible peripheral.
///
/// The current levels are conservative operating values selected by observed
/// protocol activity, not a claim to reproduce sub-byte current waveforms.
/// `power_on_threshold_v` also gates protocol responses: below that solved
/// supply voltage the peripheral is unpowered and returns no valid traffic.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PeripheralPower {
    #[serde(default = "default_supply_role")]
    pub supply_role: String,
    #[serde(default = "default_return_role")]
    pub return_role: String,
    pub power_on_threshold_v: f64,
    pub idle_a: f64,
    pub read_a: f64,
    pub write_a: f64,
    /// Current after a protocol-defined low-power command (for example SPI
    /// NOR deep-power-down). Omitted when the peripheral has no such state or
    /// the source does not bound it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_power_a: Option<f64>,
}

macro_rules! default_roles {
    ($($f:ident = $role:literal),* $(,)?) => {
        $(fn $f() -> String { $role.to_string() })*
    };
}
default_roles!(
    default_supply_role = "vcc",
    default_return_role = "gnd",
    default_cs_role = "cs",
    default_scl_role = "scl",
    default_sda_role = "sda",
    default_clk_role = "sck",
    default_mosi_role = "mosi",
    default_miso_role = "miso",
);

impl ModelCoverage {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.implements.is_empty() && self.missing.is_empty()
    }
}

/// Where a model entry's numbers come from, on the source accuracy ladder: the
/// tier, whether the entry has been validated, and any declared numeric
/// uncertainty.
///
/// Feeds the evidence spine's `ModelSource` records.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSourceSpec {
    pub tier: ModelSourceTier,
    #[serde(default = "unvalidated")]
    pub validation: ModelValidation,
    /// Exact human-auditable sources for the card's identity and numbers.
    /// URLs remain useful without a retained blob hash; when exact source bytes
    /// are available, `sha256` turns the citation into a reproducible input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ModelSourceReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncertainty: Vec<ModelUncertainty>,
    /// `[[models.source.characteristic]]`: published rows with no param or
    /// rating home, kept as data. Card-level only; never an uncertainty interval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub characteristic: Vec<Characteristic>,
    /// `[[models.source.pinout]]`: the pin table's column for each package the
    /// sheet prints, so `[models.pins]` is checked against its own package's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinout: Vec<Pinout>,
    /// `[[models.source.doubt]]`: what the drafting validator, or the drafter's own
    /// confidence, left unconfirmed on a drafted card ([`crate::card_trust`]). A finding
    /// whose only basis is such a field is reported as "check this card", never as a
    /// board fault. Delete a row once the card has been reviewed against the datasheet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub doubt: Vec<Doubt>,
}

/// Which part of a drafted card a [`Doubt`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoubtArea {
    /// The whole card: the drafter reported low confidence, or the pad map is
    /// read from the wrong package. Every card-derived finding is doubted.
    Card,
    /// The pad-to-role map: findings that read what a pad is for.
    Pins,
    /// The operating-envelope rows: findings that compare a level with a window.
    Envelope,
}

/// One field group of a drafted card the validator could not confirm, and why.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Doubt {
    pub area: DoubtArea,
    pub reason: String,
}

/// One package's pin-table column: pad identifier to the name printed for it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pinout {
    pub package: String,
    pub pins: BTreeMap<String, String>,
    /// The standard and view the pads are numbered from when the sheet prints letters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
}

/// One published characteristic row carrying only the min/typ/max columns the
/// row prints, so a one-sided limit or a typical-only value keeps its number
/// without becoming a two-sided interval.
///
/// As a `[models.params]` value it is that param's row, and `parameter`
/// defaults to the key.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Characteristic {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parameter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typ: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Empty only for a dimensionless row.
    pub unit: String,
    /// The row's test conditions (bias, temperature, frequency, pulse width).
    pub conditions: String,
    pub basis: String,
    /// The `[models.ratings]` key this row is one printed condition of; the
    /// key's scalar is the least favourable such row (`crate::derating`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    /// The package, and the board or mounting, the row is printed for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mounting: Option<String>,
    /// The `[models.pins]` roles whose current a path's rating row bounds (IN1
    /// to OUT: `["in1"]`); none for the whole part, a printed total included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<String>,
}

const COLUMNS: [&str; 3] = ["min", "typ", "max"];

/// How a rail-relative or signed row is recorded (the lint and the prompts).
pub const PRINTED_REFERENCE: &str = "record each printed number with its sign in its column (±5 \
    is min = -5 and max = 5), never delete the record; a supply-relative limit (0.7 VDD, VDD + 0.5 V) \
    is no number: quote it in `conditions` and omit that column (a row left with none becomes an \
    `unknown` uncertainty record quoting it), never a rail as `unit`";

/// Which of min, typ, max a record's own text cites ("typ/max columns"); a
/// negated mention ("no typ published") cites nothing.
fn cited_columns(texts: &[&str]) -> [bool; 3] {
    let mut cited = [false; 3];
    for text in texts {
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .map(str::to_ascii_lowercase)
            .collect();
        for (i, word) in words.iter().enumerate() {
            let negated = i > 0 && matches!(words[i - 1].as_str(), "no" | "not" | "without");
            if let Some(slot) = COLUMNS.iter().position(|c| c == word) {
                cited[slot] |= !negated;
            }
        }
    }
    cited
}

fn column_list(which: [bool; 3]) -> String {
    let names = COLUMNS.iter().zip(which).filter(|(_, on)| *on);
    names.map(|(c, _)| *c).collect::<Vec<_>>().join("/")
}

impl Characteristic {
    /// Every problem with the row: at least one finite column, columns in the
    /// sign convention below, the columns its text cites, a conversion its text
    /// claims, and a named row and source.
    ///
    /// Sign convention: each value keeps its printed sign in its printed column,
    /// ordered min <= typ <= max by signed value; a row printed as negative
    /// values may instead keep the datasheet's magnitude order (PNP and
    /// negative-rail tables print |min| <= |max|), never a mix of the two.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let columns = [self.min, self.typ, self.max];
        let values: Vec<f64> = columns.iter().flatten().copied().collect();
        let mut out = Vec::new();
        if values.is_empty() {
            out.push(format!("requires at least one of min, typ, max: {PRINTED_REFERENCE}; only a row printing no number at all (\"see note\", a curve) is cited in a comment instead"));
        } else if values.iter().any(|v| !v.is_finite()) {
            out.push("min, typ and max must be finite".to_string());
        } else {
            let negative = values.iter().all(|v| *v < 0.0);
            let magnitude_order = negative && values.iter().map(|v| v.abs()).is_sorted();
            if !(values.is_sorted() || magnitude_order) {
                let offsets = if negative || values.iter().all(|v| *v > 0.0) {
                    "; a threshold printed as an offset from another spec is a signed offset (negative below it), ascending, with that spec named in conditions"
                } else {
                    ""
                };
                out.push(format!("requires min <= typ <= max by signed value (a row printed negative may keep its magnitude order); keep each printed sign in its printed column{offsets}"));
            }
        }
        if self.parameter.trim().is_empty() || self.basis.trim().is_empty() {
            out.push("requires non-empty parameter and basis".to_string());
        }
        let cited = cited_columns(&[&self.parameter, &self.basis]);
        let filled = columns.map(|c| c.is_some());
        if cited[1] && (self.typ.is_none() || (0..3).any(|i| filled[i] && !cited[i])) {
            out.push(format!(
                "its text cites the {} column(s) but the record fills {}: a typ value belongs in `typ`, never in min or max, and a column the row does not print stays out",
                column_list(cited),
                column_list(filled)
            ));
        }
        let texts = [self.parameter.as_str(), &self.conditions, &self.basis];
        let named = [("min", self.min), ("typ", self.typ), ("max", self.max)];
        out.extend(crate::units::conversion_problem(&texts, &self.unit, &named));
        out
    }

    /// A drafted row writing a supply-relative limit as a number: under a rail
    /// unit (`VDD`), or as the bare offset its own text quotes (`VDD + 0.5 V`
    /// stored as 0.5). Extraction only: a curated card may keep a checked ratio.
    #[must_use]
    pub fn supply_relative_problem(&self) -> Option<String> {
        static OFFSET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\bV([A-Z][A-Z0-9]*)\s*([-+−])\s*(\d+(?:\.\d+)?)")
                .expect("valid regex")
        });
        let volts = crate::units::si_unit(&self.unit).is_some_and(|(_, base)| base == "V");
        let texts = [self.conditions.as_str(), &self.basis];
        let offsets = texts.into_iter().flat_map(|t| OFFSET.captures_iter(t));
        let offsets = offsets.filter(|c| &c[1] != "SS").filter_map(|c| {
            let sign = if &c[2] == "+" { 1.0 } else { -1.0 };
            c[3].parse::<f64>().ok().map(|v| sign * v)
        });
        let columns = [self.min, self.typ, self.max];
        let bare = |o: f64| columns.iter().flatten().any(|v| (v - o).abs() < 1e-9);
        let bad = crate::units::rail_relative_unit(&self.unit)
            || (volts && offsets.into_iter().any(bare));
        bad.then(|| format!("characteristic '{}' writes a supply-relative limit as a number: {PRINTED_REFERENCE}", self.parameter))
    }

    /// The specification-limits interval a row printing both min and max
    /// states; the typ stays in the row.
    pub(crate) fn interval(&self) -> Option<ModelUncertainty> {
        let (a, b) = (self.min?, self.max?);
        ModelUncertainty::interval(&self.parameter, a.min(b), a.max(b), &self.unit, &self.basis)
            .ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelSourceReference {
    /// Empty only for a local file pinned by `sha256` (an extraction's own PDF).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    pub title: String,
    pub locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl ModelSourceSpec {
    /// Every citation and every uncertainty and characteristic record must be
    /// well formed; all problems at once, `; `-joined (see [`Self::problems`]).
    ///
    /// # Errors
    ///
    /// Fails with every problem [`Self::problems`] finds, `; `-joined.
    pub fn validate(&self) -> Result<(), String> {
        let problems = self.problems();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Every problem with every record, each naming its record, so one lint run
    /// reports them all. An interval whose basis cites columns must cite both a
    /// min and a max: a typ never becomes a bound, and an absent column is
    /// never an invented 0.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (index, reference) in self.references.iter().enumerate() {
            if let Err(e) = reference.validate() {
                out.push(format!("source reference {index} {e}"));
            }
        }
        for (index, record) in self.uncertainty.iter().enumerate() {
            let (ModelUncertainty::Interval { parameter, .. }
            | ModelUncertainty::Unknown { parameter, .. }) = record;
            let mut push = |e: String| out.push(format!("uncertainty {index} ({parameter}): {e}"));
            if let Err(e) = record.validate() {
                push(e.to_string());
            }
            if let ModelUncertainty::Interval {
                low,
                high,
                unit,
                kind,
                basis,
                ..
            } = record
            {
                // A zero bound the basis declares physical is stated, not invented.
                let cited = cited_columns(&[basis]);
                let declared = *low == 0.0 && basis.contains("physical lower bound");
                let limits = *kind == ModelIntervalKind::SpecificationLimits && !declared;
                if limits && cited.contains(&true) && !(cited[0] && cited[2]) {
                    push(format!(
                        "its basis cites only the {} column(s); specification limits need a \
                         printed min AND max (a typ is never a bound, a missing column never \
                         0): record the row as a characteristic",
                        column_list(cited)
                    ));
                }
                let named = [("low", Some(*low)), ("high", Some(*high))];
                if let Some(e) = crate::units::conversion_problem(&[basis], unit, &named) {
                    push(e);
                }
            }
        }
        for (index, row) in self.characteristic.iter().enumerate() {
            let name = &row.parameter;
            out.extend(
                row.problems()
                    .into_iter()
                    .map(|e| format!("characteristic {index} ({name}): {e}")),
            );
        }
        out
    }

    /// Whether any uncertainty record covers model-form error (`parameter = "model"`).
    #[must_use]
    pub fn declares_model_error(&self) -> bool {
        self.uncertainty.iter().any(|u| {
            matches!(u, ModelUncertainty::Interval { parameter, .. }
                | ModelUncertainty::Unknown { parameter, .. } if parameter == "model")
        })
    }
}

/// The lint warning for a datasheet-derived source with no model-error record.
#[must_use]
pub fn model_error_warning(source: Option<&ModelSourceSpec>) -> Option<&'static str> {
    let source = source.filter(|s| s.tier == ModelSourceTier::DatasheetDerived)?;
    let warning = "model-error record missing (extraction inserts the unknown record)";
    (!source.declares_model_error()).then_some(warning)
}

impl ModelSourceReference {
    /// A citation must be fetchable over TLS, name what it cites, and carry a
    /// well-formed digest when it carries one.
    ///
    /// # Errors
    ///
    /// Fails when the url is not `https://` (and the citation is not a local file
    /// with a digest), the title or locator is blank, or the digest is not 64 hex
    /// characters.
    pub fn validate(&self) -> Result<(), String> {
        let local = self.url.is_empty() && self.sha256.is_some();
        if !local && !self.url.starts_with("https://") {
            return Err(format!("url must use https://, got '{}'", self.url));
        }
        if self.title.trim().is_empty() || self.locator.trim().is_empty() {
            return Err("requires non-empty title and locator".to_string());
        }
        if let Some(hash) = &self.sha256 {
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("sha256 must be 64 hexadecimal characters".to_string());
            }
        }
        Ok(())
    }
}

const fn unvalidated() -> ModelValidation {
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
    #[serde(default, skip_serializing_if = "is_default")]
    pub above_domain: AboveDomainBehavior,

    /// The smallest programming resistance (ohms) the datasheet allows ("the
    /// minimum allowed RILIM is 15 kOhm"). A board resistor below it is outside
    /// the part's specified range whatever the equation would return, so the
    /// device-decode check reports it rather than decoding a setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_program_ohms: Option<f64>,

    /// The largest programming resistance (ohms) the datasheet allows; above
    /// it is reported the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_program_ohms: Option<f64>,

    /// Manufacturer programming equation, flattened into the TOML block so a
    /// model reads `equation = "inverse_resistance"` beside its constants.
    #[serde(flatten)]
    pub equation: CurrentProgramEquation,
}

/// A pin whose resistor to ground picks one row of a datasheet's resistor table.
///
/// Such tables are discrete: a part reads its strap at power-up and
/// maps it to a setting, with no formula between the printed values. So a row
/// is either a printed nominal (`ohms`), accepted within `tolerance` of it, or
/// an open-ended printed range (`above_ohms` / `below_ohms`); a resistance
/// that lands in no row selects nothing and the binder says so, rather than
/// interpolating a setting the part never offers.
///
/// A row's `params` are ordinary `[models.params]` names; the binder adds them
/// to the card's params for the bound board, so every consumer that reads the
/// param reads the board's selection. A row with no params (a "charge
/// disabled" or "battery-only" row) selects a state the card's `note` names.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ResistorBins {
    /// The pin role (a value in [`ModelEntry::pins`]) the resistor sits on.
    pub pin: String,
    /// Relative window around each printed nominal within which a fitted
    /// resistance selects that row (0.05 = +/-5 %). The datasheet usually
    /// prints only nominals, so cite where the window comes from.
    pub tolerance: f64,
    /// The table rows, in any order.
    #[serde(rename = "bin")]
    pub bins: Vec<ResistorBin>,
}

/// One row of a [`ResistorBins`] table.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ResistorBin {
    /// The printed nominal resistance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ohms: Option<f64>,
    /// An open-ended row: any resistance strictly above this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above_ohms: Option<f64>,
    /// An open-ended row: any resistance strictly below this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below_ohms: Option<f64>,
    /// The params this row sets, by `[models.params]` name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, f64>,
    /// What the row means, in the datasheet's words ("charge disabled").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ResistorBin {
    /// The row's resistance as printed, for messages.
    #[must_use]
    pub fn label(&self) -> String {
        match (self.ohms, self.above_ohms, self.below_ohms) {
            (Some(r), ..) => format!("{r:.0} ohm"),
            (_, Some(r), _) => format!("> {r:.0} ohm"),
            (.., Some(r)) => format!("< {r:.0} ohm"),
            _ => "unspecified".to_string(),
        }
    }
}

impl ResistorBins {
    /// The row a fitted resistance selects: an open-ended range containing
    /// it, else the printed nominal within [`Self::tolerance`] of it (the
    /// nearest, should two windows overlap). `None` for a resistance in no
    /// row, or a non-positive / non-finite one.
    #[must_use]
    pub fn select(&self, ohms: f64) -> Option<&ResistorBin> {
        if !positive_finite(ohms) {
            return None;
        }
        let ranged = self.bins.iter().find(|b| {
            b.above_ohms.is_some_and(|r| ohms > r) || b.below_ohms.is_some_and(|r| ohms < r)
        });
        ranged.or_else(|| {
            self.bins
                .iter()
                .filter_map(|b| Some((b, (ohms / b.ohms?).ln().abs())))
                .filter(|(b, _)| {
                    b.ohms
                        .is_some_and(|r| (ohms - r).abs() <= self.tolerance * r)
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(b, _)| b)
        })
    }
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

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
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

    /// Datasheet best-fit power law:
    ///
    /// `I(A) = coefficient_a / (R / resistance_scale_ohms) ^ exponent`.
    ///
    /// Adjustable current-limit switches commonly publish this form because
    /// their ILIM transfer is close to, but measurably not, a simple inverse.
    /// Encoding the exponent avoids replacing a sourced curve with a hand-fit
    /// `k/R` constant that is only accurate at one resistor value.
    PowerLawResistance {
        coefficient_a: f64,
        resistance_scale_ohms: f64,
        exponent: f64,
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
    /// Whether `ohms` lies outside the datasheet's allowed programming range,
    /// as `(bound, below)`: the violated bound and whether it is the minimum.
    #[must_use]
    pub fn out_of_program_range(&self, ohms: f64) -> Option<(f64, bool)> {
        if let Some(min) = self.min_program_ohms.filter(|min| ohms < *min) {
            return Some((min, true));
        }
        self.max_program_ohms
            .filter(|max| ohms > *max)
            .map(|max| (max, false))
    }

    fn apply_operating_domain(&self, equation_current_a: f64) -> Option<f64> {
        match self
            .max_operating_current_a
            .filter(|limit| limit.is_finite() && *limit > 0.0 && equation_current_a > *limit)
        {
            None => Some(equation_current_a),
            Some(_) if self.above_domain == AboveDomainBehavior::Abstain => None,
            Some(limit) => Some(limit),
        }
    }

    /// Evaluate the published resistor equation without applying the part's
    /// normal-operating ceiling. Returns `None` for a non-positive/non-finite
    /// resistance or if malformed data would produce a non-physical result.
    #[must_use]
    pub fn equation_current_a(&self, resistance_ohms: f64) -> Option<f64> {
        if !positive_finite(resistance_ohms) {
            return None;
        }

        let current_a = match &self.equation {
            CurrentProgramEquation::InverseResistance { k_volts } => *k_volts / resistance_ohms,
            CurrentProgramEquation::PowerLawResistance {
                coefficient_a,
                resistance_scale_ohms,
                exponent,
            } => *coefficient_a / (resistance_ohms / *resistance_scale_ohms).powf(*exponent),
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

        positive_finite(current_a).then_some(current_a)
    }

    /// Evaluate either a one-resistor law or a two-resistor sense-scaled law.
    /// For one-resistor equations `sense_resistance_ohms` is intentionally
    /// ignored; for the sense-scaled form both positive finite resistances are
    /// required.
    #[must_use]
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
        if !positive_finite(program_resistance_ohms) || !positive_finite(sense_resistance_ohms) {
            return None;
        }
        let program_v = (*program_bias_a * program_resistance_ohms)
            .min(*program_full_scale_v)
            .max(0.0);
        let current_a =
            (program_v / *program_full_scale_v) * *sense_full_scale_v / sense_resistance_ohms;
        positive_finite(current_a).then_some(current_a)
    }

    /// Evaluate the board's nominal programmed current, applying only the
    /// explicit normal-operating ceiling from this block—not a device rating.
    /// Datasheet/resistor tolerances are not implied by a point equation.
    #[must_use]
    pub fn operating_current_a(&self, resistance_ohms: f64) -> Option<f64> {
        self.apply_operating_domain(self.equation_current_a(resistance_ohms)?)
    }

    /// Sense-aware counterpart to [`Self::operating_current_a`].
    #[must_use]
    pub fn operating_current_with_sense_a(
        &self,
        program_resistance_ohms: f64,
        sense_resistance_ohms: f64,
    ) -> Option<f64> {
        self.apply_operating_domain(
            self.equation_current_with_sense_a(program_resistance_ohms, sense_resistance_ohms)?,
        )
    }
}

/// One boot strapping pin, straight from the part's reference manual.
///
/// The `role` matches a value in [`ModelEntry::pins`] (the pad->role map), so
/// the lint can find which pad/net carries the strap. `level` is what the pin
/// must read at the reset latch window for *normal* boot. Cite the RM in the
/// TOML comment next to each entry.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct StrapPin {
    /// Pin role, matching an entry in [`ModelEntry::pins`] (e.g. "gpio0").
    pub role: String,
    /// Required level at the reset latch for normal boot.
    pub level: StrapLevel,
    /// True only for a *boot-select* strap whose wrong static level is
    /// unrecoverable: it latches which boot source / mode the part enters, and
    /// firmware cannot undo it (BOOT0, GPIO0/GPIO9 boot pin, `QSPI_SS/BOOTSEL`).
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
    /// For a strap whose level matters only in the alternate mode another
    /// strap selects (ESP32-C3 GPIO8: any level in SPI boot, high required for
    /// Joint Download Boot), the role of that boot-select strap. A floating
    /// net is then a fault only when the board can drive that strap (its net
    /// reaches a button, header or other part); otherwise the alternate mode
    /// is unreachable and the float is harmless.
    #[serde(default)]
    pub only_with: Option<String>,
    /// Sourced input thresholds in the datasheet's affine form
    /// `[per_vdd, offset_v]`: VIL max and VIH min are `per_vdd * VDD + offset_v`
    /// on the part's `vdd` pin. Without both, a pull divider is not judged.
    #[serde(default)]
    pub vil: Option<[f64; 2]>,
    #[serde(default)]
    pub vih: Option<[f64; 2]>,
}

/// One non-strap pin that needs a bias, straight from the part's datasheet
/// (see [`ModelEntry::pin_bias`]).
///
/// `role` matches a value in [`ModelEntry::pins`]. A pin that is also a row of
/// [`ModelEntry::straps`] is judged by the strap lint and refused here, so no
/// pin has two owners.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinBias {
    /// Pin role, matching an entry in [`ModelEntry::pins`].
    pub role: String,
    /// The level the pin must sit at when nothing is actively driving it.
    pub level: BiasLevel,
    /// What satisfies the requirement: a pull resistor, or any defined level.
    #[serde(default)]
    pub needs: BiasNeed,
    /// Smallest pull resistance the datasheet allows, ohms (a stronger pull
    /// than this overloads the pin's driver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ohm: Option<f64>,
    /// Largest pull resistance the datasheet allows, ohms (a weaker pull than
    /// this does not hold the level against leakage or the line's capacitance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ohm: Option<f64>,
    /// The pin may be left unconnected: the requirement applies only once
    /// something is wired to it (an open-drain flag the design does not use).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_ok: bool,
    /// The datasheet table or sentence, with page, the requirement is read from.
    pub basis: String,
    /// What the pin does and why the bias matters, for the finding's message.
    #[serde(default)]
    pub note: String,
}

/// The idle level a [`PinBias`] row requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BiasLevel {
    /// Pulled or held to the pin's I/O supply.
    High,
    /// Pulled or held to ground.
    Low,
}

impl BiasLevel {
    /// The word a message uses for the pull that makes this level.
    #[must_use]
    pub const fn pull_word(self) -> &'static str {
        match self {
            Self::High => "pull-up",
            Self::Low => "pull-down",
        }
    }

    /// The level opposite this one.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::High => Self::Low,
            Self::Low => Self::High,
        }
    }
}

/// What satisfies a [`PinBias`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BiasNeed {
    /// A resistor to the rail (high) or ground (low) on the pin's net: the
    /// pin is an open-drain output or a receiver nothing else may leave
    /// undefined, and a driver on the net does not stand in for the pull.
    #[default]
    Pull,
    /// The pin must idle at the level, by a resistor of the right direction
    /// or by a part that drives the net; an unconnected or release-able net
    /// does not do.
    Idle,
}

/// Which side of the part a [`SupplyCap`] row sits on, for the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapSide {
    /// A supply input (a driver's motor supply, a regulator or load-switch input).
    #[default]
    Input,
    /// An output (a regulator's or load switch's output).
    Output,
}

impl CapSide {
    /// The word a message uses for the pin.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// Whether the datasheet requires the capacitance or only recommends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapGrade {
    /// A stated minimum ("a minimum of 10 uF is required", a Recommended
    /// Operating Conditions row): falling short is a fault.
    #[default]
    Required,
    /// A recommendation ("10 uF is recommended"): falling short is a note.
    Recommended,
}

/// The least capacitance a supply input or output pin wants, from the
/// datasheet (see [`ModelEntry::supply_cap`]).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyCap {
    /// Pin role of the supply or output pin; the capacitance is counted on its
    /// net, to the part's ground.
    pub role: String,
    /// Input or output, for the message.
    #[serde(default)]
    pub side: CapSide,
    /// Required or only recommended.
    #[serde(default)]
    pub grade: CapGrade,
    /// Total capacitance on the pin's net to ground, farads.
    pub min_f: f64,
    /// When set, one capacitor on the net must itself reach this, farads: the
    /// datasheet's bulk capacitor, which a pile of smaller decoupling parts is
    /// not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_single_f: Option<f64>,
    /// The datasheet table or sentence, with page, the value is read from.
    pub basis: String,
    /// What the capacitor is for and its type or voltage-rating conditions.
    #[serde(default)]
    pub note: String,
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
            Self::Defined => "a defined level",
        }
    }
}

/// Datasheet safety thresholds used by static and live checks.
///
/// Most entries are absolute maxima. Some model files deliberately use a
/// lower recommended-operating ceiling so a check fires before the part leaves
/// its guaranteed region; those entries must say so beside the value. These
/// are analysis thresholds, never proof that a board normally draws or drives
/// the stated amount. Unknown keys are rejected: a misspelled rating is a
/// limit the monitor would silently never check.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
    /// (`hauksbee_checks::checks::ripple`). Only this part-specific value is
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
    /// a default from the footprint package class (see `hauksbee_bind::thermal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theta_ja_c_per_w: Option<f64>,

    /// Junction-to-case thermal resistance (C/W). Informational / for a future
    /// heatsinked path; the free-air estimate uses `theta_ja_c_per_w`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theta_jc_c_per_w: Option<f64>,

    /// FLOOR, not a ceiling: the least current (A) the datasheet guarantees regulation
    /// at (TL431 `IKmin`, an LDO minimum load); below it the run discloses, not answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_current_a: Option<f64>,

    /// MOSFET: lowest |VGS| (V) the datasheet specifies RDS(on) at; under it the
    /// card's fitted on-resistance is an extrapolation the vendor never guaranteed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rds_on_vgs: Option<f64>,

    /// MOSFET: MAXIMUM |VGS(th)| (V) on the datasheet; under it a worst-case part
    /// is not guaranteed to be enhanced at all, so conducting there breaks a limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vgs_th_max: Option<f64>,

    /// MOSFET: absolute-maximum gate-source voltage magnitude (V), the
    /// datasheet's VGS / VGSS row. The gate oxide breaks down past it in either
    /// direction, so the stress monitor judges |V(gate) - V(source)| against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vgs_max: Option<f64>,

    /// Set by the binder, never by a card: `max_current_a` is a fuse's or
    /// polyfuse's hold current, so a load past it trips the part and the rail
    /// it feeds collapses, which the DC stress finding says in those words.
    #[serde(skip)]
    pub trips_over_current: bool,

    /// Set by the binder, never by a card: whether these limits are the
    /// part's own, or a placeholder or guessed card's stand-in, whose
    /// exceedance is a stated condition and never a failing finding
    /// ([`crate::rating_basis`]).
    #[serde(skip)]
    pub basis: crate::rating_basis::RatingBasis,
}

/// Severity requested by a sourced operating-envelope entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeSeverity {
    Serious,
    #[default]
    Medium,
    Note,
}

/// A pin-scoped recommended operating condition carried by a model card.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperatingEnvelope {
    SupplyRange {
        pin: String,
        min_v: f64,
        max_v: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abs_max_v: Option<f64>,
        #[serde(default)]
        severity: EnvelopeSeverity,
        /// The Recommended Operating Conditions row `min_v..max_v` is read from.
        #[serde(default)]
        basis: String,
        /// The Absolute Maximum Ratings row `abs_max_v` is read from. It is a
        /// different table from `basis`, so a finding that quotes the
        /// absolute-maximum figure quotes this too; empty when `basis` already
        /// names that table (see [`basis_names_abs_max`]).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        abs_max_basis: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        /// The printed row's temperature (`TA = 25 °C`, `TA = −40 °C to 85 °C`);
        /// empty for a row that holds at every temperature.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        conditions: String,
    },
    RailOrder {
        lower: String,
        upper: String,
        #[serde(default)]
        severity: EnvelopeSeverity,
        #[serde(default)]
        basis: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
    /// A SUPPLY-RELATIVE input window: the `inputs` roles of one channel may
    /// not ALL sit above `V(supply) - headroom_v`. The absolute `SupplyRange`
    /// above cannot express a limit that tracks the rail (a comparator's
    /// `VICR` upper end of `V+ - 1.5 V`), and the "all, not one" shape is the
    /// LM339/LM393 rule: one input inside the range still compares correctly.
    InputCommonMode {
        supply: String,
        inputs: Vec<String>,
        headroom_v: f64,
        #[serde(default)]
        severity: EnvelopeSeverity,
        #[serde(default)]
        basis: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        conditions: String,
    },
    /// A SUPPLY-INDEPENDENT ceiling on input pins: the `pins` roles take up
    /// to `max_v` (the recommended `VI` maximum) and `abs_max_v` (the
    /// absolute-maximum `VI`) whatever the part's supply is, the rating an
    /// over-voltage-tolerant input prints (74LVC `VI` −0.5 V to +6.5 V at any
    /// VCC, an MCU's five-volt-tolerant pin). Such a pin has no clamp to its
    /// own supply, so a rail above that supply but under this ceiling is no
    /// back-powering path. A pin rated `VCC + 0.5 V` has no such row: the
    /// clamp to its supply is its ceiling. A [`Self::SupplyRange`] cannot carry
    /// this: its `min_v` is read as a supply floor, and it takes one pin.
    InputTolerance {
        pins: Vec<String>,
        max_v: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abs_max_v: Option<f64>,
        #[serde(default)]
        severity: EnvelopeSeverity,
        /// The Recommended Operating Conditions `VI` row `max_v` is read from.
        #[serde(default)]
        basis: String,
        /// The Absolute Maximum Ratings `VI` row `abs_max_v` is read from.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        abs_max_basis: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
}

/// The highest voltage a card rates the signal pin `role` for, whatever its supply.
///
/// The absolute maximum (else the recommended maximum) of every `supply_range`
/// row on that pin and every `input_tolerance` row listing it. `None` when the
/// card rates the pin only through its supply. A `supply_range` row on a sense
/// or measurement input ([`crate::pin_roles::is_sense_input_role`]) is a
/// measurement window, not a ground-referenced pin rating, so it is not read;
/// an `input_tolerance` row on such a pin is.
#[must_use]
pub fn declared_pin_ceiling_v(envelope: &[OperatingEnvelope], role: &str) -> Option<f64> {
    let ceiling = |e: &OperatingEnvelope| match e {
        OperatingEnvelope::SupplyRange {
            pin,
            max_v,
            abs_max_v,
            ..
        } if pin.eq_ignore_ascii_case(role) && !crate::pin_roles::is_sense_input_role(role) => {
            Some(abs_max_v.unwrap_or(*max_v))
        }
        OperatingEnvelope::InputTolerance {
            pins,
            max_v,
            abs_max_v,
            ..
        } if pins.iter().any(|p| p.eq_ignore_ascii_case(role)) => Some(abs_max_v.unwrap_or(*max_v)),
        _ => None,
    };
    envelope.iter().filter_map(ceiling).reduce(f64::max)
}

/// Whether a supply row's `basis` already names a second source for its
/// `abs_max_v`.
///
/// That is the wording the vendors print for the absolute-maximum table
/// ("Absolute Maximum Ratings", "Limiting Values", ST's "Voltage
/// characteristics", "Maximum Ratings"), or a `;`-separated second citation that
/// carries a table or section number ("..., 5.3 VCCA 1.2 V to 3.6 V; 5.1 VCCA
/// -0.5 V to 4.6 V"). A basis that cites only the recommended-operating-conditions
/// row does not.
#[must_use]
pub fn basis_names_abs_max(basis: &str) -> bool {
    let lower = basis
        .to_ascii_lowercase()
        .replace(['-', '_'], " ")
        .replace('.', "");
    let worded = [
        "absolute max",
        "abs max",
        "limiting values",
        "maximum rating",
        "voltage characteristics",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    worded
        || basis
            .rsplit_once(';')
            .is_some_and(|(_, second)| second.chars().any(|c| c.is_ascii_digit()))
}

/// The source clause a finding quotes when it states the `limit` V absolute
/// maximum beside the recommended window.
///
/// It carries both rows' bases, since they are different tables of the sheet. A row that records no absolute-maximum source
/// says so, so the figure is never presented as read from the window's table.
#[must_use]
pub fn abs_max_citation(basis: &str, abs_max_basis: &str, limit: f64) -> String {
    if !abs_max_basis.trim().is_empty() {
        format!("basis: \"{basis}\"; absolute maximum basis: \"{abs_max_basis}\"")
    } else if basis_names_abs_max(basis) {
        format!("basis: \"{basis}\"")
    } else {
        format!(
            "basis: \"{basis}\"; the card records no source for its {} absolute maximum",
            format_volts(limit)
        )
    }
}

impl OperatingEnvelope {
    #[must_use]
    pub fn basis(&self) -> &str {
        match self {
            Self::SupplyRange { basis, .. }
            | Self::RailOrder { basis, .. }
            | Self::InputCommonMode { basis, .. }
            | Self::InputTolerance { basis, .. } => basis,
        }
    }

    #[must_use]
    pub const fn severity(&self) -> EnvelopeSeverity {
        match self {
            Self::SupplyRange { severity, .. }
            | Self::RailOrder { severity, .. }
            | Self::InputCommonMode { severity, .. }
            | Self::InputTolerance { severity, .. } => *severity,
        }
    }

    /// The printed temperature row's conditions; empty when it has none.
    #[must_use]
    pub fn conditions(&self) -> &str {
        match self {
            Self::SupplyRange { conditions: c, .. }
            | Self::InputCommonMode { conditions: c, .. } => c,
            Self::RailOrder { .. } | Self::InputTolerance { .. } => "",
        }
    }

    /// The limit every temperature row of one channel shares; `None` for a
    /// rail order or an input tolerance, which have no rows.
    #[must_use]
    pub fn channel(&self) -> Option<String> {
        match self {
            Self::SupplyRange { pin, .. } => Some(format!("supply range of {pin}")),
            Self::InputCommonMode { inputs, .. } => Some(format!("{inputs:?} common-mode range")),
            Self::RailOrder { .. } | Self::InputTolerance { .. } => None,
        }
    }
}

impl Ratings {
    /// Every rating key, one `  name  # meaning` line each, offered verbatim by
    /// the extraction prompt; a test pins the names to the struct's serde fields.
    pub const KEY_GUIDE: &str = concat!(
        "\
  max_current_a        # continuous rated current (A): diode IF, BJT IC, MOSFET ID, regulator or switch rated IOUT (a features list or operating-conditions row counts); never pulsed or peak, nor an abs-max surge unless it is the only current printed (then say so); for an IC the whole-device or supply/ground current limit, not per-pin
  max_surge_current_a  # non-repetitive surge / pulsed peak current (A): diode IFSM, BJT ICM, MOSFET IDM
  max_power_w          # total power dissipation (W): Ptot / PD for the selected package
  max_voltage_v        # maximum blocking/working voltage (V): diode VRRM, capacitor rated voltage, BJT VCEO, MOSFET VDS, regulator VIN abs max, IC supply abs max
  polarized            # true for a polarized (electrolytic/tantalum) capacitor, where reverse bias is a fault
  max_pin_current_a    # per-pin source/sink absolute limit (A) for an IC or MCU
  max_ripple_current_a # capacitor rated RMS ripple current (A) at its stated frequency/temperature
  max_junction_temp_c  # TJ maximum junction temperature (C)
  theta_ja_c_per_w     # Rth(j-a) junction-to-ambient thermal resistance (C/W = K/W): ",
        standard_condition!(),
        " (highest across the packages and board conditions it leaves open)
  theta_jc_c_per_w     # Rth(j-c) junction-to-case thermal resistance (C/W = K/W), selected package and stated case reference only
  min_current_a        # FLOOR, not a ceiling (A): least current the datasheet guarantees regulation at (shunt-reference IKmin, LDO minimum load)
  rds_on_vgs           # MOSFET: lowest |VGS| (V) at which the datasheet specifies RDS(on)
  vgs_th_max           # MOSFET: MAXIMUM |VGS(th)| (V) from Electrical Characteristics
  vgs_max              # MOSFET: absolute-maximum gate-source voltage magnitude (V), the VGS / VGSS row (+-12 V gives 12)"
    );

    /// The rating key names, in [`Self::KEY_GUIDE`] order.
    pub fn keys() -> impl Iterator<Item = &'static str> {
        Self::KEY_GUIDE
            .lines()
            .filter_map(|l| l.split_whitespace().next())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        Self {
            basis: crate::rating_basis::RatingBasis::default(),
            ..self.clone()
        } == Self::default()
    }
}

// ── Component kind ────────────────────────────────────────────────────────────

/// Classification of a component for the solver and extractor.
///
/// `Default` is [`ComponentKind::Ignore`], the inert arm: it exists so
/// [`ModelEntry`] can derive `Default` for struct-update construction, and it
/// is never reached through deserialization (`kind` has no `serde(default)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, Default)]
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
    /// 555-class precision timer, stamped at bind time as a composite of
    /// resistors, behavioral sources and a hysteretic switch (the SR latch).
    Timer,
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
    #[default]
    Ignore,
}

impl ComponentKind {
    /// Every kind, in declaration order; the `ALL` test fails to compile when a
    /// variant is added and not listed.
    pub const ALL: [Self; 18] = [
        Self::Passive,
        Self::Diode,
        Self::BjtNpn,
        Self::BjtPnp,
        Self::Nmos,
        Self::Pmos,
        Self::Vreg,
        Self::Opamp,
        Self::Comparator,
        Self::Timer,
        Self::AnalogSwitch,
        Self::Digital,
        Self::Dac,
        Self::Adc,
        Self::ShiftRegister,
        Self::Mcu,
        Self::Connector,
        Self::Ignore,
    ];
}

// ── Passive class ─────────────────────────────────────────────────────────────

/// Which two-terminal element a [`ComponentKind::Passive`] entry actually is.
///
/// The solver treats every one of these as "a two-node element whose magnitude
/// comes from the value field", which is why they share one [`ComponentKind`].
/// Checks that reason about *topology* need the distinction: a pull-up must be a
/// resistor, a crystal load cap must be a capacitor, and a ferrite bead sitting
/// between a rail and a net is neither. Before this field the only available
/// answer was the reference designator's first letter, which is a comment the
/// CAD user wrote, not evidence: a resistor in an `RN`-numbered slot reads as a
/// resistor network and a capacitor labelled `R5` counts as a pull-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PassiveClass {
    /// Plain resistor: an ohmic two-terminal element.
    Resistor,
    /// Capacitor of any dielectric / polarity.
    Capacitor,
    /// Inductor (a wound or multilayer choke).
    Inductor,
    /// Ferrite bead / chip inductor sold as an impedance at a frequency. Ohmic
    /// at DC, so its value often *looks* like a resistance; it is not a
    /// resistor and must never be read as a pull-up.
    FerriteBead,
    /// Quartz crystal or ceramic resonator.
    Crystal,
    /// Fuse, polyfuse, or PTC resettable protector.
    Fuse,
    /// Net tie: a zero-length copper bridge, not a fitted component at all.
    NetTie,
}

impl PassiveClass {
    /// True only for a plain resistor. The one-line spelling every caller that
    /// asks "can this be a pull-up / pull-down / series termination?" uses, so
    /// no call site re-derives the answer from the variant list and drifts when
    /// a variant is added.
    #[must_use]
    pub const fn is_resistor(self) -> bool {
        matches!(self, Self::Resistor)
    }
}

// ── Match rules ───────────────────────────────────────────────────────────────

/// Rules that determine whether a model entry matches a component.
///
/// All populated rules are AND-ed: the entry matches only when every
/// non-`None` rule fires. At least one rule must be populated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct MatchRules {
    /// Exact KiCad `lib_id` string (e.g. `"Device:R"`) or prefix ending with
    /// `":"` (e.g. `"Device:"` matches any `lib_id` in the Device library).
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

    /// Regexes matched against named source properties carried by the board.
    ///
    /// Property names are compared case-insensitively after normalising spaces
    /// and hyphens to underscores. Every populated property rule is required.
    /// This lets an exact supplier/order-code property disambiguate a generic
    /// Value such as `Polyfuse 1.8A` without copying a board reference into the
    /// model or pretending the Value itself is an MPN.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
}

impl MatchRules {
    /// True when at least one rule field is populated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lib_id.is_none()
            && self.value_re.is_none()
            && self.footprint_re.is_none()
            && self.mpn_re.is_none()
            && self.properties.is_empty()
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
    /// Retrieve a floating-point param by name: a number, or the column of a
    /// printed row that [`crate::param_names::row_value`] picks for `key`.
    #[must_use]
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        match self.0.get(key)? {
            ParamValue::Row(row) => crate::param_names::row_value(key, row).ok(),
            value => value.as_f64(),
        }
    }

    /// The params written as printed rows, by key.
    pub fn rows(&self) -> impl Iterator<Item = (&str, &Characteristic)> {
        self.0.iter().filter_map(|(key, value)| match value {
            ParamValue::Row(row) => Some((key.as_str(), &**row)),
            _ => None,
        })
    }

    /// Retrieve a string param by name.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.as_str()
    }

    /// Retrieve a boolean param by name. `None` when the key is absent or its
    /// value is not a bool, so `flag = false` is distinguishable from omitted.
    #[must_use]
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.0.get(key)?.as_bool()
    }

    /// Insert a param of any value type.
    pub fn set(&mut self, key: impl Into<String>, v: impl Into<ParamValue>) {
        self.0.insert(key.into(), v.into());
    }

    /// Insert a float param.
    pub fn set_f64(&mut self, key: impl Into<String>, v: f64) {
        self.set(key, v);
    }

    /// Insert a string param.
    pub fn set_str(&mut self, key: impl Into<String>, v: impl Into<String>) {
        self.set(key, v.into());
    }

    /// Insert an integer param.
    pub fn set_int(&mut self, key: impl Into<String>, v: i64) {
        self.set(key, v);
    }

    /// Insert a boolean param.
    pub fn set_bool(&mut self, key: impl Into<String>, v: bool) {
        self.set(key, v);
    }

    /// True when no params are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A parameter value: a float, integer, bool or string, or a printed datasheet
/// row whose column and unit code chooses (read it through [`Params::get_f64`]).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ParamValue {
    Float(f64),
    Int(i64),
    Bool(bool),
    String(String),
    Row(Box<Characteristic>),
}

macro_rules! param_from {
    ($($t:ty => $variant:ident),* $(,)?) => {
        $(impl From<$t> for ParamValue {
            fn from(v: $t) -> Self {
                ParamValue::$variant(v)
            }
        })*
    };
}
param_from!(f64 => Float, i64 => Int, bool => Bool, String => String);

impl ParamValue {
    /// Return as `f64`, converting integers; `None` for a row, whose value
    /// depends on its key.
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            // A TOML integer is a count or a small constant; past 2^53 it rounds, as f64 must.
            #[allow(clippy::cast_precision_loss)]
            Self::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// Return as `&str` for string values.
    #[must_use]
    pub const fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Return as `bool` for boolean values. `None` for non-bool params, so a
    /// caller can distinguish `flag = false` from an omitted/non-bool `flag`.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        declared_pin_ceiling_v, AboveDomainBehavior, CurrentProgram, EnvelopeSeverity,
        ModelSourceSpec, OperatingEnvelope, Params, Ratings,
    };

    fn supply_range(pin: &str, max_v: f64, abs_max_v: f64) -> OperatingEnvelope {
        OperatingEnvelope::SupplyRange {
            pin: pin.into(),
            min_v: -0.3,
            max_v,
            abs_max_v: Some(abs_max_v),
            severity: EnvelopeSeverity::default(),
            basis: "Absolute Maximum Ratings".into(),
            abs_max_basis: String::new(),
            note: String::new(),
            conditions: String::new(),
        }
    }

    /// A sense input's `supply_range` is a measurement window, not a pin
    /// ceiling: only an `input_tolerance` row rates that pin. A signal pin that
    /// is no sense input still takes its `supply_range` as its ceiling.
    #[test]
    fn a_sense_inputs_supply_range_is_not_its_pin_ceiling() {
        let envelope = [
            supply_range("isense", 0.3, 0.5),
            supply_range("in_plus", 40.0, 42.0),
        ];
        assert_eq!(declared_pin_ceiling_v(&envelope, "isense"), None);
        assert_eq!(declared_pin_ceiling_v(&envelope, "in_plus"), Some(42.0));
        let tolerant = [
            supply_range("isense", 0.3, 0.5),
            OperatingEnvelope::InputTolerance {
                pins: vec!["isense".into()],
                max_v: 26.0,
                abs_max_v: Some(28.0),
                severity: EnvelopeSeverity::default(),
                basis: "Recommended Operating Conditions".into(),
                abs_max_basis: String::new(),
                note: String::new(),
            },
        ];
        assert_eq!(declared_pin_ceiling_v(&tolerant, "isense"), Some(28.0));
    }

    #[test]
    fn ratings_reject_unknown_keys_and_the_key_guide_names_every_field() {
        let err = toml::from_str::<Ratings>("totally_made_up_rating = 7.0").unwrap_err();
        let err = err.to_string();
        assert!(
            err.contains("unknown field `totally_made_up_rating`"),
            "{err}"
        );
        // serde's own "expected one of" list is the struct's field set.
        let listed = err.split("expected one of ").nth(1).unwrap();
        let fields: Vec<&str> = listed.split('`').skip(1).step_by(2).collect();
        assert_eq!(Ratings::keys().collect::<Vec<_>>(), fields);
    }

    #[test]
    fn component_kind_all_lists_every_variant() {
        use super::ComponentKind::{
            Adc, AnalogSwitch, BjtNpn, BjtPnp, Comparator, Connector, Dac, Digital, Diode, Ignore,
            Mcu, Nmos, Opamp, Passive, Pmos, ShiftRegister, Timer, Vreg,
        };
        for k in super::ComponentKind::ALL {
            // Exhaustive: a new variant fails to compile here until it is in `ALL`.
            match k {
                Passive | Diode | BjtNpn | BjtPnp | Nmos | Pmos | Vreg | Opamp | Comparator
                | Timer | AnalogSwitch | Digital | Dac | Adc | ShiftRegister | Mcu | Connector
                | Ignore => {}
            }
        }
    }

    #[test]
    fn characteristic_rows_keep_printed_columns_and_validate() {
        let parse = |records: &str| {
            let text = format!("tier = \"datasheet-derived\"\n{records}");
            toml::from_str::<ModelSourceSpec>(&text).map_err(|e| e.to_string())
        };
        let spec = |columns: &str, basis: &str| {
            parse(&format!("[[characteristic]]\nparameter = \"VF\"\n{columns}\nunit = \"V\"\nconditions = \"IF = 1 mA, pulsed\"\nbasis = \"Table 5{basis}\"\n"))
        };
        for (columns, basis, error) in [
            ("max = 0.37", "", ""),
            ("typ = 2e-12", "", ""),
            ("min = 1.0\ntyp = 1.0\nmax = 2.0", "", ""),
            ("min = 1.0\nmax = 1.0", "", ""),
            ("min = -0.85\nmax = -0.65", "", ""),
            ("min = -0.65\ntyp = -0.7\nmax = -0.85", "", ""),
            (
                "min = 0.3\nmax = 0.4",
                ", min/max columns, no typ published",
                "",
            ),
            ("typ = 0.3\nmax = 0.4", ", typ/max columns", ""),
            ("typ = 2e-6", ", typ column, converted from µV to V", ""),
            ("", "", "at least one of min, typ, max"),
            ("max = nan", "", "finite"),
            ("min = -inf", "", "finite"),
            ("min = 2.0\ntyp = 1.0", "", "min <= typ <= max"),
            ("typ = 3.0\nmax = 2.0", "", "signed offset"),
            ("min = -0.6\ntyp = 0.1\nmax = -0.8", "", "by signed value"),
            ("min = -0.65\nmax = -0.85", "", ""),
            ("min = 0.65\nmax = -0.85", "", "by signed value"),
            ("max = 1.0\nlow = 0.0", "", "unknown field `low`"),
            (
                "min = 0.01\nmax = 0.08",
                ", min/typ columns",
                "fills min/max",
            ),
            ("max = 0.4", ", typ column", "cites the typ column"),
            (
                "typ = 2.0",
                ", typ, converted from µV to V",
                "converted from µV",
            ),
        ] {
            match (
                error,
                spec(columns, basis).and_then(|s| s.validate().map(|()| s)),
            ) {
                ("", Ok(s)) => {
                    let text = toml::to_string(&s).unwrap();
                    assert_eq!(toml::from_str::<ModelSourceSpec>(&text).unwrap(), s);
                    let printed = ["min =", "typ =", "max ="];
                    assert_eq!(
                        printed.map(|c| columns.contains(c)),
                        printed.map(|c| text.contains(c))
                    );
                }
                (needle, Err(e)) if !needle.is_empty() => {
                    assert!(e.contains(needle), "{columns}: {e}");
                }
                (want, got) => panic!("{columns}: wanted {want:?}, got {got:?}"),
            }
        }
        for (unit, max) in [("VDD", "max = 1"), ("V", "max = 0.5"), ("V", "")] {
            let row = format!("[[characteristic]]\nparameter = \"VI\"\nmin = -0.5\n{max}\nunit = \"{unit}\"\nconditions = \"VSS - 0.5 V to VDD + 0.5 V\"\nbasis = \"b\"");
            let row = &parse(&row).unwrap().characteristic[0];
            assert_eq!(
                row.supply_relative_problem().is_none(),
                max.is_empty(),
                "{unit}"
            );
            assert!(
                row.problems().is_empty(),
                "a curated card may keep a rail ratio"
            );
        }
        let mut blank = spec("typ = 2.0", "").unwrap();
        blank.characteristic[0].basis = " ".into();
        assert_eq!(
            blank.validate().unwrap_err(),
            "characteristic 0 (VF): requires non-empty parameter and basis"
        );
        // Every bad record is reported, each by name, in one pass.
        let unordered = spec("min = 2.0\ntyp = 1.0", "").unwrap();
        blank.characteristic.extend(unordered.characteristic);
        let interval = |basis: &str| {
            parse(&format!("[[uncertainty]]\nstatus = \"interval\"\nparameter = \"idd\"\nlow = 0.0\nhigh = 2e-3\nunit = \"A\"\nkind = \"specification-limits\"\nbasis = \"{basis}\"\n")).unwrap()
        };
        for (basis, bad) in [
            ("typ/max columns", true),
            ("min/max columns", false),
            ("maximum; zero is the physical lower bound", false),
        ] {
            let problems = interval(basis).problems();
            let flagged = problems
                .iter()
                .any(|p| p.contains("need a printed min AND max"));
            assert_eq!(flagged, bad, "{basis}");
        }
        blank.uncertainty = interval("typ/max").uncertainty;
        assert_eq!(blank.problems().len(), 3, "{:?}", blank.problems());
    }

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

    #[test]
    fn current_program_supports_datasheet_power_law_curves() {
        let program: CurrentProgram = toml::from_str(
            r#"
pin = "ilim"
semantics = "protection_limit"
equation = "power_law_resistance"
coefficient_a = 19.94
resistance_scale_ohms = 1000.0
exponent = 0.925
"#,
        )
        .unwrap();

        let at_20k = program.equation_current_a(20_000.0).unwrap();
        assert!((at_20k - 1.251).abs() < 0.005, "{at_20k}");
        let at_49k9 = program.equation_current_a(49_900.0).unwrap();
        assert!((at_49k9 - 0.530).abs() < 0.010, "{at_49k9}");
    }
}
