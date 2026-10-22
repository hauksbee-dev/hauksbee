//! Plain engineering English for the internal ids that reach a reader.
//!
//! The engine speaks in ids: capability slugs from the model cards
//! (`board_crystal_frequency_binding`), model coverage stages
//! (`executable_scope_unspecified`), model tiers, severity words and emulator
//! backends (`simavr:atmega328p`). They are the machine contract and stay as
//! they are in JSON, `JUnit` and SARIF. A human surface (the app, the Markdown
//! and CLI renderers) never prints one raw: it asks [`label_for`] for the words
//! and [`explain_for`] for a one-line tooltip.
//!
//! This table is the one place those words live. It reuses the wording that
//! already exists ([`crate::vocabulary`] for severities, verdicts, evidence
//! grades and checks; [`crate::glossary`] for the coverage-stage terms) instead
//! of restating it, and adds the groups that had no wording yet: capability
//! ids, model tiers and validation, match confidence, model layers and backends.
//!
//! The app reads the table from `frontend/src/lib/labels.ts`, GENERATED from
//! here ([`typescript`]) and drift-tested both ways: the Rust test fails when
//! the checked-in file differs from what this module generates, and the
//! frontend test fails when the file differs from `hauksbee-vocabulary.json`
//! (which carries [`label_table`]). Regenerate with the same flag as the other
//! generated contracts: `UPDATE_RUN_RECORD_SCHEMA=1 cargo test -p hauksbee-ir
//! --test it drift`.
//!
//! An id the table does not list is never shown raw: [`label_for`] humanises it
//! (underscores and hyphens to spaces, known acronyms restored, sentence case),
//! and the frontend's `labelFor` does the same with the same acronym table.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::OnceLock;

use serde::Serialize;

use crate::evidence::{
    ArtifactKind, MatchConfidence, ModelLayer, ModelSourceTier, ModelValidation,
};
use crate::glossary::Term;
use crate::vocabulary::{Check, EvidenceGrade, Severity, VerdictState, BLOCKING};

/// The `group` of a capability id from a model card's `[models.coverage]`.
pub const GROUP_CAPABILITY: &str = "capability";
/// The `group` of a model coverage stage.
pub const GROUP_COVERAGE_STAGE: &str = "coverage_stage";
/// The `group` of a model source tier.
pub const GROUP_MODEL_TIER: &str = "model_tier";
/// The `group` of a model validation level.
pub const GROUP_MODEL_VALIDATION: &str = "model_validation";
/// The `group` of a model resolution layer.
pub const GROUP_MODEL_LAYER: &str = "model_layer";
/// The `group` of a match confidence.
pub const GROUP_MATCH_CONFIDENCE: &str = "match_confidence";
/// The `group` of a severity word.
pub const GROUP_SEVERITY: &str = "severity";
/// The `group` of a verdict state.
pub const GROUP_VERDICT: &str = "verdict";
/// The `group` of an evidence grade.
pub const GROUP_EVIDENCE_GRADE: &str = "evidence_grade";
/// The `group` of a check name.
pub const GROUP_CHECK: &str = "check";
/// The `group` of a glossary term.
pub const GROUP_TERM: &str = "term";
/// The `group` of an emulator backend id (`kind:part`).
pub const GROUP_BACKEND: &str = "backend";
/// The `group` of a model kind (what a part was bound as).
pub const GROUP_MODEL_KIND: &str = "model_kind";
/// The `group` of a solver integration method (`trap`, `gear2`, ...).
pub const GROUP_SOLVER_METHOD: &str = "solver_method";
/// The `group` of a power supply kind (`bench`, `usb`, ...).
pub const GROUP_SUPPLY_KIND: &str = "supply_kind";
/// The `group` of a generic fallback model (a class stand-in, not a part).
pub const GROUP_GENERIC_MODEL: &str = "generic_model";
/// The `group` of an input file format (`ki_cad_pcb`).
pub const GROUP_ARTIFACT_KIND: &str = "artifact_kind";
/// The `group` of a USB supply profile (`5v1.5a`).
pub const GROUP_USB_PROFILE: &str = "usb_profile";
/// The `group` of a battery chemistry.
pub const GROUP_BATTERY_CHEMISTRY: &str = "battery_chemistry";
/// The `group` of a boot outcome (what the chip's reset sequence reached).
pub const GROUP_BOOT_OUTCOME: &str = "boot_outcome";

/// One row: an id, its short label and, optionally, a one-line explanation
/// for a tooltip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Label {
    /// The internal id, exactly as it appears in the engine's output.
    pub id: String,
    /// Which kind of id this is (one of the `GROUP_*` constants).
    pub group: &'static str,
    /// The words a reader sees.
    pub label: String,
    /// One line saying what it means, for a tooltip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<String>,
}

/// A backend kind (the part of `simavr:atmega328p` before the colon) and the
/// words for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendKind {
    pub kind: &'static str,
    /// Shown in brackets after the part: `ATmega328P (simavr)`.
    pub label: &'static str,
}

/// An input and what [`humanise`] makes of it. The frontend's `labelFor` must
/// agree on every example, which is how the two implementations stay equal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HumaniseExample {
    pub input: &'static str,
    pub output: String,
}

/// The whole table as data: what `hauksbee-vocabulary.json` carries and what
/// `frontend/src/lib/labels.ts` is generated from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LabelTable {
    pub entries: Vec<Label>,
    /// Words [`humanise`] restores to their usual casing, `(lower case, shown)`.
    pub acronyms: Vec<(&'static str, &'static str)>,
    pub backend_kinds: Vec<BackendKind>,
    pub humanise_examples: Vec<HumaniseExample>,
}

// ── Capability ids ──────────────────────────────────────────────────

/// Capability ids from the model cards' `[models.coverage]` `implements` /
/// `missing` lists: `(id, label, one-line explanation)`. Every id used by an
/// MCU card is here (a drift test reads `db/mcu.toml`); the other cards' ids
/// are plain slugs that [`humanise`] reads well, and the test lists the ones it
/// cannot.
const CAPABILITIES: &[(&str, &str, &str)] = &[
    ("adc_channels_6_and_7", "ADC channels 6 and 7", "The two ADC-only inputs that only the TQFP and QFN packages bring out."),
    ("adc_external_channels_0_to_3", "External ADC channels 0 to 3", "The firmware can read analog voltages on ADC inputs 0 to 3."),
    ("adc_external_channels_0_to_5", "External ADC channels 0 to 5", "The firmware can read analog voltages on ADC inputs 0 to 5."),
    ("adc_injection", "ADC input injection", "Feeding the simulated circuit's voltage into the chip's ADC inputs."),
    ("adc_input_mailbox_contract", "ADC input hand-off", "How the simulated circuit's voltages are handed to the emulated firmware's ADC."),
    ("adc_temperature_channel_4", "ADC temperature channel", "The on-chip temperature sensor read through ADC channel 4."),
    ("application_firmware_end_to_end_proof", "End-to-end application firmware run", "Real application firmware has not been run through the whole chip model and checked against hardware behaviour."),
    ("avr_firmware_execution", "AVR firmware execution", "The AVR core runs your compiled firmware instruction by instruction."),
    ("board_crystal_frequency_binding", "Crystal frequency from the board", "The chip runs at the clock its crystal sets. The model does not yet read that frequency from the board, so timing uses the default clock."),
    ("boot_rom_to_main", "Boot ROM through to main()", "The chip's boot ROM runs and hands over to the application's main()."),
    ("boot_strap_levels", "Boot strap pin levels", "The levels on the boot-mode strap pins at reset decide how the chip boots."),
    ("boot_to_application", "Boot through to the application", "The chip boots and starts the application firmware."),
    ("boot_to_main", "Boot through to main()", "The chip boots and reaches the application's main()."),
    ("clock_tree", "Clock tree", "The PLLs, dividers and clock gating that set each peripheral's clock."),
    ("cortex_m0plus_firmware_execution", "Cortex-M0+ firmware execution", "The Cortex-M0+ core runs your compiled firmware."),
    ("cortex_m3_firmware_execution", "Cortex-M3 firmware execution", "The Cortex-M3 core runs your compiled firmware."),
    ("cortex_m4_firmware_execution", "Cortex-M4 firmware execution", "The Cortex-M4 core runs your compiled firmware."),
    ("cortex_m7_firmware_execution", "Cortex-M7 firmware execution", "The Cortex-M7 core runs your compiled firmware."),
    ("cycle_exact_virtual_clock_and_timers", "Cycle-exact clock and timers", "Simulated time advances exactly one clock cycle per cycle executed, so timers and delays match the hardware."),
    ("dma_fidelity", "DMA accuracy", "DMA transfers match the hardware's timing and arbitration."),
    ("dma_transfers", "DMA transfers", "The DMA controller moving data without the CPU."),
    ("fdcan_frames", "FDCAN frames", "Sending and receiving CAN FD frames."),
    ("firmware_cosim_core", "Firmware co-simulation core", "The processor core that runs firmware alongside the circuit simulation."),
    ("gpio_32_to_48_observation", "Reading GPIO 32 to 48", "The state of the upper GPIO pins can be observed from the circuit."),
    ("gpio_behaviour", "GPIO behaviour", "How the general-purpose pins drive and read the circuit."),
    ("gpio_cluster_current_limits", "GPIO bank current limits", "The total current a group of GPIO pins may source or sink."),
    ("gpio_external_input", "GPIO external input", "The firmware reads the logic level the circuit puts on an input pin."),
    ("gpio_input_mailbox_contract", "GPIO input hand-off", "How the circuit's pin levels are handed to the emulated firmware."),
    ("gpio_input_mailbox_contract_0_to_31", "GPIO input hand-off (pins 0 to 31)", "How the circuit's levels on GPIO 0 to 31 are handed to the emulated firmware."),
    ("gpio_internal_pulls", "GPIO internal pull-ups and pull-downs", "The chip's built-in pull resistors on its pins."),
    ("gpio_output_and_direction", "GPIO output and direction", "The firmware sets pins as inputs or outputs and drives them high or low."),
    ("gpio_output_register_or_mailbox_observation", "GPIO output readback", "The circuit sees the levels the firmware drives onto its pins."),
    ("gpio_output_register_or_mailbox_observation_0_to_31", "GPIO output readback (pins 0 to 31)", "The circuit sees the levels the firmware drives onto GPIO 0 to 31."),
    ("i2c0_i2c1_controller_bridge", "I2C0 and I2C1 controller bridge", "Both I2C controllers talk to the devices on the board's I2C buses."),
    ("i2c1_controller_bridge", "I2C1 controller bridge", "The I2C1 controller talks to the devices on the board's I2C bus."),
    ("i2c_controller_bridge", "I2C controller bridge", "The firmware's I2C controller talks to the devices on the board's I2C bus."),
    ("i2c_spi_mailbox_contract", "I2C and SPI hand-off", "How bus traffic is handed between the emulated firmware and the simulated devices."),
    ("instruction_counted_timing", "Instruction-counted timing", "Time is counted per instruction, not per bus cycle, so tight timing is approximate."),
    ("instruction_exact_timing", "Instruction-exact timing", "Every instruction takes exactly its documented number of cycles."),
    ("missing_external_crystal_startup_validation", "External crystal start-up check", "Whether the external crystal oscillator starts reliably with the board's load capacitors."),
    ("native_adc_peripheral", "Native ADC peripheral", "The chip's own ADC converts the circuit's voltages instead of a stand-in."),
    ("pad_map", "Pad map", "Which package pad carries which pin function."),
    ("pin_and_package_current_ratings", "Pin and package current ratings", "The current limits for each pin and for the whole package."),
    ("pio", "PIO (programmable I/O)", "The RP2040's programmable I/O state machines."),
    ("proven_two_core_firmware", "Two-core firmware", "Firmware that uses both cores has been run on the model."),
    ("shared_lead_contention", "Shared-pin contention", "Two drivers fighting over the same pin."),
    ("shared_sio_register_state", "Shared SIO register state", "The RP2040's single-cycle I/O registers shared by both cores."),
    ("silicon_power_current_profile", "Supply current by state", "How much current the real chip draws when running, sleeping and starting up. Without it, power budgets cannot count the chip's own draw."),
    ("silicon_timing_and_errata", "Silicon timing and errata", "The datasheet's exact timing figures and the chip's known silicon bugs."),
    ("spi1_controller_bridge", "SPI1 controller bridge", "The SPI1 controller talks to the devices on the board's SPI bus."),
    ("spi_controller_bridge", "SPI controller bridge", "The firmware's SPI controller talks to the devices on the board's SPI bus."),
    ("spi_external_slave", "SPI as a slave", "The chip acts as an SPI slave to a controller elsewhere on the board."),
    ("supply_current_rating", "Supply current rating", "The most current the chip's supply pins may carry."),
    ("supply_envelope", "Supply voltage range", "The supply voltages the chip is specified to run at."),
    ("tmp105_temperature_injection", "TMP105 temperature input", "The simulated TMP105 sensor reports a temperature you set."),
    ("uart", "UART", "The serial port."),
    ("uart0_tx", "UART0 transmit", "Serial output from UART0."),
    ("uart_transport", "UART transport", "Serial bytes moving between the firmware and whatever is wired to the port."),
    ("uart_tx", "UART transmit", "Serial output from the firmware."),
    ("uart_tx_and_rx", "UART transmit and receive", "The firmware sends and receives serial data."),
    ("unmodified_firmware_gpio_input", "GPIO input with unmodified firmware", "Your firmware reads the pins as built, with no test hooks added."),
    ("unmodified_firmware_i2c_spi_observation", "I2C and SPI traffic with unmodified firmware", "Bus traffic from your firmware as built can be observed, with no test hooks added."),
    ("usb_device_controller", "USB device controller", "The chip's USB port, so USB enumeration and traffic are not simulated."),
    ("usb_serial_jtag", "USB serial and JTAG port", "The built-in USB port used for console output and debugging."),
    ("virtual_clock_and_systick", "Virtual clock and SysTick", "Simulated time and the SysTick timer firmware uses for delays."),
    ("virtual_clock_and_timers", "Virtual clock and timers", "Simulated time and the hardware timers firmware uses for delays."),
    ("wall_bounded_virtual_time", "Virtual time bounded by real time", "Simulated time cannot run faster than the emulator runs in real time."),
    ("watchdog_behavior", "Watchdog behaviour", "The watchdog timer and what it does when firmware stops feeding it."),
    ("watchdog_reboot", "Watchdog reboot", "The chip resets when the watchdog times out."),
    ("watchdog_reboot_continuation", "Resuming after a watchdog reboot", "The run carries on through the reboot instead of ending."),
    ("xtensa_lx6_firmware_execution", "Xtensa LX6 firmware execution", "The ESP32's Xtensa LX6 cores run your compiled firmware."),
    ("xtensa_lx7_machine_boot", "Xtensa LX7 boot", "The ESP32-S3's Xtensa LX7 cores boot."),
];

// ── Other groups ────────────────────────────────────────────────────

/// Model coverage stages, as `hauksbee models coverage` writes them.
/// `(id, glossary term the stage is an instance of, fallback label)`.
const COVERAGE_STAGES: &[(&str, Option<Term>, &str, &str)] = &[
    ("unresolved", Some(Term::OpenPart), "", ""),
    ("identity_only", Some(Term::IdentityOnly), "", ""),
    ("executable_scope_unspecified", Some(Term::ScopeUnspecified), "", ""),
    ("executable_partial", Some(Term::ExecutablePartial), "", ""),
    (
        "executable_declared",
        None,
        "Modelled for its declared scope",
        "The model runs, and it states what it covers and what it does not. Nothing it declares is missing.",
    ),
    (
        "ignored",
        None,
        "Ignored",
        "The part is deliberately left out of the circuit (mechanical, test point or marked do-not-fit).",
    ),
];

const MODEL_TIERS: &[(ModelSourceTier, &str, &str)] = &[
    (
        ModelSourceTier::Open,
        "No model",
        "The part is left out of the circuit.",
    ),
    (
        ModelSourceTier::EstimatedFallback,
        "Estimated stand-in",
        "A generic estimate stood in because no model matched the part.",
    ),
    (
        ModelSourceTier::IntervalModel,
        "Interval model",
        "A model with stated minimum and maximum bounds, not one exact behaviour.",
    ),
    (
        ModelSourceTier::DatasheetDerived,
        "Derived from the datasheet",
        "Built from the part's datasheet figures, not from the vendor's own model.",
    ),
    (
        ModelSourceTier::CuratedLibrary,
        "Curated library model",
        "A model from the built-in library, reviewed against the datasheet.",
    ),
    (
        ModelSourceTier::CuratedPack,
        "Curated pack model",
        "A model from an installed model pack, reviewed against the datasheet.",
    ),
    (
        ModelSourceTier::VendorSpice,
        "Vendor SPICE model",
        "The manufacturer's own SPICE model.",
    ),
    (
        ModelSourceTier::UserModel,
        "Your model",
        "A model you supplied for this part; it overrides the others.",
    ),
];

const MODEL_VALIDATION: &[(ModelValidation, &str, &str)] = &[
    (
        ModelValidation::Unvalidated,
        "Not validated",
        "Nothing has checked the model against the real part.",
    ),
    (
        ModelValidation::PhysicalBoundsOnly,
        "Range-checked only",
        "Its values are checked to be physically possible, not compared with the part's curves.",
    ),
    (
        ModelValidation::DatasheetCurves,
        "Checked against datasheet curves",
        "Compared with the curves the datasheet publishes.",
    ),
    (
        ModelValidation::VendorQualified,
        "Vendor-qualified",
        "Qualified by the manufacturer.",
    ),
];

const MODEL_LAYERS: &[(ModelLayer, &str)] = &[
    (ModelLayer::Unspecified, "Source not recorded"),
    (ModelLayer::Builtin, "Built-in library"),
    (ModelLayer::Pack, "Model pack"),
    (ModelLayer::UserDir, "Your models folder"),
    (ModelLayer::UserConfigDir, "Your configuration folder"),
    (ModelLayer::ModelsDir, "Project models folder"),
    (ModelLayer::Spice, "SPICE file"),
    (ModelLayer::EngineFallback, "Engine fallback"),
];

/// Model kinds: the `kind` a card declares and the inspector's "bound model".
/// The first eighteen are the schema's own (`validation::KIND_NAMES`, held
/// equal by a drift test); the last five are the families the extractor also
/// writes cards for. `(id, label, one-line explanation)`.
const MODEL_KINDS: &[(&str, &str, &str)] = &[
    (
        "passive",
        "Passive",
        "A resistor, capacitor, inductor, ferrite or crystal.",
    ),
    (
        "diode",
        "Diode",
        "A rectifier, Schottky, Zener, TVS or LED.",
    ),
    (
        "bjt_npn",
        "NPN transistor",
        "A discrete bipolar transistor, NPN.",
    ),
    (
        "bjt_pnp",
        "PNP transistor",
        "A discrete bipolar transistor, PNP.",
    ),
    ("nmos", "N-channel MOSFET", "A discrete MOSFET, N-channel."),
    ("pmos", "P-channel MOSFET", "A discrete MOSFET, P-channel."),
    (
        "vreg",
        "Voltage regulator",
        "A linear or switching regulator, reference or other part whose job is a rail.",
    ),
    ("opamp", "Op-amp", "An amplifier with an analog output."),
    (
        "comparator",
        "Comparator",
        "A comparator or supervisor with a two-level output.",
    ),
    ("timer", "Timer", "A 555-class precision timer."),
    (
        "analog_switch",
        "Analog switch",
        "An analog switch, multiplexer or load switch.",
    ),
    (
        "digital",
        "Logic or driver IC",
        "Logic, a buffer, a level shifter, or a driver steered by logic inputs.",
    ),
    ("dac", "DAC", "A digital-to-analog converter."),
    ("adc", "ADC", "An analog-to-digital converter."),
    (
        "shift_register",
        "Shift register",
        "A shift or storage register.",
    ),
    (
        "mcu",
        "Microcontroller",
        "A programmable microcontroller; its firmware runs in an emulator.",
    ),
    (
        "connector",
        "Connector",
        "A connector, header or test point; only pin continuity is modelled.",
    ),
    (
        "ignore",
        "Left out",
        "A part with no electrical effect: a mounting hole, logo or fiducial.",
    ),
    (
        "i2c_sensor",
        "I2C sensor",
        "A register-map sensor or peripheral on an I2C bus.",
    ),
    (
        "spi_sensor",
        "SPI sensor",
        "A register-map sensor or peripheral on an SPI bus.",
    ),
    ("charger", "Battery charger", "A battery charger."),
    (
        "pmic",
        "Power-management IC",
        "A multi-rail power-management IC.",
    ),
    ("balancer", "Cell balancer", "A battery cell balancer."),
];

const MATCH_CONFIDENCE: &[(MatchConfidence, &str, &str)] = &[
    (
        MatchConfidence::Exact,
        "Exact match",
        "The part number matches the model exactly.",
    ),
    (
        MatchConfidence::High,
        "Close match",
        "The part number matches a model for the same device.",
    ),
    (
        MatchConfidence::Heuristic,
        "Probable match",
        "Chosen from the part's name and pins; check it.",
    ),
    (
        MatchConfidence::Guessed,
        "Guess",
        "No reliable match; the model is a guess.",
    ),
];

/// Solver integration methods. The wire names the app sends (`trap`, `gear2`)
/// and the names the solver reports for each window of a run
/// (`IntegrationMethod`, kebab-case) are both listed, so neither reaches a
/// reader raw. `(id, label, one-line explanation)`.
const SOLVER_METHODS: &[(&str, &str, &str)] = &[
    ("trap", "Trapezoidal", "The default: accurate for almost every board."),
    ("trapezoidal", "Trapezoidal", "The default: accurate for almost every board."),
    ("gear2", "Gear 2", "Damps numerical ringing, so a switching circuit that rings only in the simulation settles."),
    ("backward-euler", "Backward Euler", "A heavily damped fallback step, used where the default step would not converge."),
    ("reduced-step", "Smaller steps", "The default method re-run with smaller time steps where the run was struggling."),
    ("cold-start-backward-euler", "Restarted from rest (Backward Euler)", "The window was restarted from a settled state, so state carried in from before it was discarded."),
    ("subdivided-backward-euler", "Subdivided steps (Backward Euler)", "The window was split into many small steps of the damped fallback method."),
];

/// Power supply kinds, as the checks spec and the live supply panel spell
/// them. `(id, label, one-line explanation)`.
const SUPPLY_KINDS: &[(&str, &str, &str)] = &[
    (
        "ideal",
        "Ideal source",
        "A perfect voltage source: the rail holds its level whatever the board pulls.",
    ),
    (
        "bench",
        "Bench supply",
        "A constant voltage that folds back when the board pulls more than its current limit.",
    ),
    (
        "wall",
        "Wall adapter",
        "A nominal voltage behind some output resistance, with ripple.",
    ),
    (
        "usb",
        "USB port",
        "A USB source that sags under load and cuts out at the port's current limit.",
    ),
    (
        "battery",
        "Battery",
        "A battery pack that drains as the board draws from it.",
    ),
];

/// USB supply profiles, as the spec spells them (`5v1.5a`) and as the wire
/// mirror does (`v5_1_5a`). `(id, label)`.
const USB_PROFILES: &[(&str, &str)] = &[
    ("5v0.5a", "5 V, 0.5 A"),
    ("5v1.5a", "5 V, 1.5 A"),
    ("5v3a", "5 V, 3 A"),
    ("v5_0_5a", "5 V, 0.5 A"),
    ("v5_1_5a", "5 V, 1.5 A"),
    ("v5_3a", "5 V, 3 A"),
];

/// Battery chemistries, as the spec and the wire spell them. `(id, label)`.
const BATTERY_CHEMISTRIES: &[(&str, &str)] = &[
    ("li_ion", "Li-ion"),
    ("li-ion", "Li-ion"),
    ("alkaline", "Alkaline"),
    ("ni_mh", "NiMH"),
    ("nimh", "NiMH"),
    ("li_fe_po4", "LiFePO4"),
    ("lifepo4", "LiFePO4"),
];

/// The input formats a report's inventory lists, as the engine writes them
/// (`ArtifactKind`, `snake_case`). `(kind, label)`.
const ARTIFACT_KINDS: &[(ArtifactKind, &str)] = &[
    (ArtifactKind::KiCadPcb, "KiCad board"),
    (ArtifactKind::KiCadSchematic, "KiCad schematic"),
    (ArtifactKind::KiCadNetlist, "KiCad netlist"),
    (ArtifactKind::EagleBoard, "Eagle board"),
    (ArtifactKind::AltiumPcbDoc, "Altium board"),
    (ArtifactKind::GerberArchive, "Gerber files"),
    (ArtifactKind::OdbPlusPlus, "ODB++ archive"),
    (ArtifactKind::Ipc2581, "IPC-2581 file"),
    (ArtifactKind::Easyeda, "EasyEDA project"),
    (ArtifactKind::Ipc356, "IPC-356 netlist"),
    (ArtifactKind::Bom, "Bill of materials"),
    (ArtifactKind::Placement, "Placement file"),
    (ArtifactKind::Elf, "Firmware (ELF)"),
    (ArtifactKind::IntelHex, "Firmware (Intel HEX)"),
    (ArtifactKind::Toml, "Settings file"),
];

/// The generic MOSFET stand-ins the binder uses when a board names only a
/// transistor class: the family per package class and its two corners.
/// `(id stem, words)`: the package classes of `db/mosfet.toml`.
const GENERIC_MOSFET_CLASSES: &[(&str, &str)] = &[
    ("sot", "small-signal"),
    ("power", "power"),
    ("class", "other-package"),
];

/// `(id letter, words)` for the two channel polarities.
const GENERIC_MOSFET_CHANNELS: &[(&str, &str)] = &[("n", "N-channel"), ("p", "P-channel")];

/// The label rows for the generic MOSFET family and corner models: the ids a
/// sentence like "Q1 passes across assumed corners (...)" names, in words.
fn generic_mosfet_rows() -> Vec<Label> {
    let mut out = Vec::new();
    for &(letter, channel) in GENERIC_MOSFET_CHANNELS {
        for &(class, class_words) in GENERIC_MOSFET_CLASSES {
            let family = format!("generic_{letter}mos_{class}");
            let name = format!("Generic {class_words} {channel} MOSFET");
            let packages = if class == "class" {
                "a package this library does not recognise"
            } else {
                "that package class"
            };
            out.push(row(
                &family,
                GROUP_GENERIC_MODEL,
                &name,
                Some(&format!(
                    "A stand-in for any {class_words} {channel} MOSFET in {packages}, used when the board names no part number."
                )),
            ));
            for (corner, corner_words, why) in [
                (
                    "high_vth",
                    "high-threshold",
                    "the weak end: highest threshold, weakest channel",
                ),
                (
                    "low_vth",
                    "low-threshold",
                    "the strong end: lowest threshold, strongest channel",
                ),
            ] {
                out.push(row(
                    &format!("{family}_corner_{corner}"),
                    GROUP_GENERIC_MODEL,
                    &format!("{name} at its {corner_words} corner"),
                    Some(&format!(
                        "An assumed corner of the {name}, {why}. Results are checked at both corners because no part number says which is fitted."
                    )),
                ));
            }
        }
    }
    out
}

/// Emulator backend kinds: the part of `simavr:atmega328p` before the colon.
const BACKEND_KINDS: &[(&str, &str)] = &[
    ("simavr", "simavr"),
    ("renode", "Renode"),
    ("qemu", "QEMU"),
    ("none", "no emulator"),
];

/// Known emulator backend ids: `(kind, part, chip as written on its datasheet)`.
const BACKENDS: &[(&str, &str, &str)] = &[
    ("simavr", "atmega168", "ATmega168"),
    ("simavr", "atmega1284", "ATmega1284"),
    ("simavr", "atmega1284p", "ATmega1284P"),
    ("simavr", "atmega328p", "ATmega328P"),
    ("simavr", "atmega32u4", "ATmega32U4"),
    ("simavr", "attiny85", "ATtiny85"),
    ("renode", "nrf52840", "nRF52840"),
    ("renode", "rp2040", "RP2040"),
    ("renode", "sifive_fe310", "SiFive FE310"),
    ("renode", "stm32f072", "STM32F072"),
    ("renode", "stm32f103", "STM32F103"),
    ("renode", "stm32f4", "STM32F4"),
    ("renode", "stm32f4_discovery", "STM32F4 Discovery"),
    ("renode", "stm32f7", "STM32F7"),
    ("renode", "stm32g4", "STM32G4"),
    ("renode", "stm32l0", "STM32L0"),
    ("qemu", "esp32", "ESP32"),
    ("qemu", "esp32c3", "ESP32-C3"),
    ("qemu", "esp32s3", "ESP32-S3"),
    ("none", "atmega16u2", "ATmega16U2"),
    ("none", "ch32v003", "CH32V003"),
    ("none", "efm32pg22", "EFM32PG22"),
    ("none", "esp32c6", "ESP32-C6"),
    ("none", "samd21", "SAMD21"),
    ("none", "stm32h7", "STM32H7"),
];

/// Words [`humanise`] restores to their usual casing, `(lower case, shown)`.
const ACRONYMS: &[(&str, &str)] = &[
    ("ac", "AC"),
    ("adc", "ADC"),
    ("avr", "AVR"),
    ("bjt", "BJT"),
    ("bom", "BOM"),
    ("can", "CAN"),
    ("cc", "CC"),
    ("cmos", "CMOS"),
    ("cpu", "CPU"),
    ("dac", "DAC"),
    ("dc", "DC"),
    ("dma", "DMA"),
    ("dnp", "DNP"),
    ("drc", "DRC"),
    ("eeprom", "EEPROM"),
    ("esd", "ESD"),
    ("fdcan", "FDCAN"),
    ("fifo", "FIFO"),
    ("gnd", "GND"),
    ("gpio", "GPIO"),
    ("i2c", "I2C"),
    ("i2s", "I2S"),
    ("ic", "IC"),
    ("id", "ID"),
    ("iec", "IEC"),
    ("io", "I/O"),
    ("jtag", "JTAG"),
    ("lcd", "LCD"),
    ("ldo", "LDO"),
    ("led", "LED"),
    ("mcu", "MCU"),
    ("mosfet", "MOSFET"),
    ("ocp", "OCP"),
    ("ovp", "OVP"),
    ("pcb", "PCB"),
    ("pio", "PIO"),
    ("pll", "PLL"),
    ("pmos", "PMOS"),
    ("nmos", "NMOS"),
    ("psrr", "PSRR"),
    ("pwm", "PWM"),
    ("ram", "RAM"),
    ("rom", "ROM"),
    ("rtc", "RTC"),
    ("sfdp", "SFDP"),
    ("sio", "SIO"),
    ("spi", "SPI"),
    ("spice", "SPICE"),
    ("sram", "SRAM"),
    ("tqfp", "TQFP"),
    ("uart", "UART"),
    ("usb", "USB"),
    ("uvlo", "UVLO"),
    ("vcc", "VCC"),
    ("vdd", "VDD"),
    ("vf", "Vf"),
    ("vss", "VSS"),
    // Datasheet vocabulary of the cards' capability ids.
    ("als", "ALS"),
    ("avdd", "AVDD"),
    ("cbus", "CBUS"),
    ("cmti", "CMTI"),
    ("cpvdd", "CPVDD"),
    ("crc", "CRC"),
    ("cv", "CV"),
    ("dcr", "DCR"),
    ("din", "DIN"),
    ("dout", "DOUT"),
    ("dpdt", "DPDT"),
    ("drdy", "DRDY"),
    ("dtr", "DTR"),
    ("dvdd", "DVDD"),
    ("ecc", "ECC"),
    ("erm", "ERM"),
    ("esr", "ESR"),
    ("fet", "FET"),
    ("fpwm", "FPWM"),
    ("gpif", "GPIF"),
    ("gpo", "GPO"),
    ("hdq", "HDQ"),
    ("i3c", "I3C"),
    ("ioff", "Ioff"),
    ("ir", "IR"),
    ("jedec", "JEDEC"),
    ("lra", "LRA"),
    ("mdio", "MDIO"),
    ("mux", "MUX"),
    ("ntc", "NTC"),
    ("nvm", "NVM"),
    ("odr", "ODR"),
    ("ois", "OIS"),
    ("pfm", "PFM"),
    ("phy", "PHY"),
    ("pmbus", "PMBus"),
    ("psm", "PSM"),
    ("rc", "RC"),
    ("rmii", "RMII"),
    ("rts", "RTS"),
    ("rx", "RX"),
    ("rxd", "RXD"),
    ("scl", "SCL"),
    ("sda", "SDA"),
    ("shdn", "SHDN"),
    ("smbus", "SMBus"),
    ("soa", "SOA"),
    ("spdt", "SPDT"),
    ("sys", "SYS"),
    ("tx", "TX"),
    ("txd", "TXD"),
    ("uvp", "UVP"),
    ("vbus", "VBUS"),
    ("vccio", "VCCIO"),
    ("vddio", "VDDIO"),
];

/// Inputs the frontend's `labelFor` must humanise to the same text.
const HUMANISE_INPUTS: &[&str] = &[
    "board_crystal_frequency_binding",
    "spi_dual_quad_io",
    "adc_injection",
    "i2c0_uart1_bridge",
    "led_generic_corner_high_vf",
    "logic_levels_at_3v3_vddio",
    "some-kebab-id",
    "already Fine",
    "x",
    "",
    "__double__underscore__",
    "UPPER_CASE_ID",
];

// ── Humanising ──────────────────────────────────────────────────────

fn acronym(word: &str) -> Option<&'static str> {
    ACRONYMS.iter().find(|(k, _)| *k == word).map(|(_, v)| *v)
}

/// One lower-case word as shown: a known acronym in its usual casing (also
/// with trailing digits, `uart0` to `UART0`), a voltage such as `3v3` as
/// `3V3`, anything else unchanged.
fn word(token: &str) -> String {
    if let Some(shown) = acronym(token) {
        return shown.to_string();
    }
    let stem = token.trim_end_matches(|c: char| c.is_ascii_digit());
    if !stem.is_empty() && stem.len() < token.len() {
        if let Some(shown) = acronym(stem) {
            return format!("{shown}{}", &token[stem.len()..]);
        }
    }
    let bytes = token.as_bytes();
    if let Some(v) = token.find('v') {
        if v > 0
            && v + 1 < token.len()
            && bytes[..v].iter().all(u8::is_ascii_digit)
            && bytes[v + 1..].iter().all(u8::is_ascii_digit)
        {
            return token.to_ascii_uppercase();
        }
    }
    token.to_string()
}

/// An id as a sentence, never with an underscore.
///
/// Split on `_`, `-` and spaces, restore acronyms, capitalise the first word:
/// `board_crystal_frequency_binding` is `Board crystal frequency binding` and
/// `spi_dual_quad_io` is `SPI dual quad I/O`.
///
/// The frontend's `humanise` is the same function; [`label_table`] carries
/// examples both must agree on.
#[must_use]
pub fn humanise(id: &str) -> String {
    let lower = id.to_lowercase();
    let mut out = String::new();
    for (i, token) in lower
        .split(['_', '-', ' '])
        .filter(|t| !t.is_empty())
        .enumerate()
    {
        let shown = word(token);
        if i > 0 {
            out.push(' ');
            out.push_str(&shown);
        } else if acronym(token).is_some() || shown != token {
            out.push_str(&shown);
        } else {
            let mut chars = shown.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

// ── The table ───────────────────────────────────────────────────────

fn row(id: &str, group: &'static str, label: &str, explain: Option<&str>) -> Label {
    Label {
        id: id.to_string(),
        group,
        label: label.to_string(),
        explain: explain.map(str::to_string),
    }
}

fn term_id(term: Term) -> String {
    serde_json::to_value(term)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The rows for the words a run's setup and inputs are described in: solver
/// methods, supply kinds and profiles, battery chemistries, the generic
/// MOSFET stand-ins and the input file formats.
fn run_setup_rows() -> Vec<Label> {
    let mut out = Vec::new();
    for &(id, label, explain) in SOLVER_METHODS {
        out.push(row(id, GROUP_SOLVER_METHOD, label, Some(explain)));
    }
    for &(id, label, explain) in SUPPLY_KINDS {
        out.push(row(id, GROUP_SUPPLY_KIND, label, Some(explain)));
    }
    for &(id, label) in USB_PROFILES {
        out.push(row(id, GROUP_USB_PROFILE, label, None));
    }
    for &(id, label) in BATTERY_CHEMISTRIES {
        out.push(row(id, GROUP_BATTERY_CHEMISTRY, label, None));
    }
    out.extend(generic_mosfet_rows());
    for &(kind, label) in ARTIFACT_KINDS {
        let id = serde_json::to_value(kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        out.push(row(&id, GROUP_ARTIFACT_KIND, label, None));
    }
    out
}

/// Every row in precedence order, duplicates included.
fn raw_entries() -> Vec<Label> {
    let mut out = Vec::new();
    for &(id, label, explain) in CAPABILITIES {
        out.push(row(id, GROUP_CAPABILITY, label, Some(explain)));
    }
    for &(id, term, label, explain) in COVERAGE_STAGES {
        match term {
            Some(term) => {
                let e = term.entry();
                out.push(row(id, GROUP_COVERAGE_STAGE, e.label, Some(e.short)));
            }
            None => out.push(row(id, GROUP_COVERAGE_STAGE, label, Some(explain))),
        }
    }
    for &(tier, label, explain) in MODEL_TIERS {
        out.push(row(tier.as_str(), GROUP_MODEL_TIER, label, Some(explain)));
    }
    for &(v, label, explain) in MODEL_VALIDATION {
        out.push(row(
            v.as_str(),
            GROUP_MODEL_VALIDATION,
            label,
            Some(explain),
        ));
    }
    for &(layer, label) in MODEL_LAYERS {
        let id = serde_json::to_value(layer)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        out.push(row(&id, GROUP_MODEL_LAYER, label, None));
    }
    for &(id, label, explain) in MODEL_KINDS {
        out.push(row(id, GROUP_MODEL_KIND, label, Some(explain)));
        // The binder's `component_kinds` map spells a kind from its Rust name
        // (`BjtNpn` lower-cased), so `bjt_npn` also arrives as `bjtnpn`.
        let compact = id.replace('_', "");
        if compact != id {
            out.push(row(&compact, GROUP_MODEL_KIND, label, Some(explain)));
        }
    }
    out.extend(run_setup_rows());
    for (id, label, explain) in crate::boot::outcome_rows() {
        out.push(row(id, GROUP_BOOT_OUTCOME, label, Some(explain)));
    }
    for &(c, label, explain) in MATCH_CONFIDENCE {
        let id = serde_json::to_value(c)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        out.push(row(&id, GROUP_MATCH_CONFIDENCE, label, Some(explain)));
    }
    for s in Severity::ALL {
        out.push(row(
            s.as_str(),
            GROUP_SEVERITY,
            s.label(),
            Some(s.meaning()),
        ));
    }
    out.push(row(
        BLOCKING.id,
        GROUP_SEVERITY,
        BLOCKING.label,
        Some(BLOCKING.meaning),
    ));
    for s in VerdictState::ALL {
        out.push(row(s.as_str(), GROUP_VERDICT, s.label(), Some(s.meaning())));
    }
    for g in EvidenceGrade::ALL {
        out.push(row(
            g.as_str(),
            GROUP_EVIDENCE_GRADE,
            g.label(),
            Some(g.meaning()),
        ));
    }
    for c in Check::ALL {
        out.push(row(c.as_str(), GROUP_CHECK, c.label(), Some(c.meaning())));
    }
    for e in crate::glossary::all() {
        out.push(row(&term_id(e.term), GROUP_TERM, e.label, Some(e.short)));
    }
    for e in crate::observations::observation_kinds() {
        out.push(row(
            e.id,
            crate::observations::GROUP_OBSERVATION,
            e.label,
            Some(e.meaning),
        ));
    }
    for &(kind, part, chip) in BACKENDS {
        let kind_label = BACKEND_KINDS
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or(kind, |(_, l)| *l);
        out.push(row(
            &format!("{kind}:{part}"),
            GROUP_BACKEND,
            &format!("{chip} ({kind_label})"),
            None,
        ));
    }
    out
}

/// The ids [`entries`] dropped because an earlier row already owns them, in
/// order. The documented overlap is `pass` and `fail`, which are both a
/// verdict word and a glossary term; the verdict row wins.
#[must_use]
pub fn dropped_duplicates() -> Vec<String> {
    let mut seen = HashSet::new();
    raw_entries()
        .into_iter()
        .filter(|e| !seen.insert(e.id.clone()))
        .map(|e| e.id)
        .collect()
}

/// The table, one row per id (the first owner of an id wins; see
/// [`dropped_duplicates`]).
#[must_use]
pub fn entries() -> Vec<Label> {
    let mut seen = HashSet::new();
    raw_entries()
        .into_iter()
        .filter(|e| seen.insert(e.id.clone()))
        .collect()
}

fn index() -> &'static HashMap<String, Label> {
    static INDEX: OnceLock<HashMap<String, Label>> = OnceLock::new();
    INDEX.get_or_init(|| entries().into_iter().map(|e| (e.id.clone(), e)).collect())
}

/// The table as data, for `hauksbee-vocabulary.json` and the generated
/// frontend module.
#[must_use]
pub fn label_table() -> LabelTable {
    LabelTable {
        entries: entries(),
        acronyms: ACRONYMS.to_vec(),
        backend_kinds: BACKEND_KINDS
            .iter()
            .map(|&(kind, label)| BackendKind { kind, label })
            .collect(),
        humanise_examples: HUMANISE_INPUTS
            .iter()
            .map(|&input| HumaniseExample {
                input,
                output: humanise(input),
            })
            .collect(),
    }
}

/// A backend id (`kind:part`) as words, or `None` when it is not one.
fn backend_label(id: &str) -> Option<String> {
    let (kind, part) = id.split_once(':')?;
    let kind_label = BACKEND_KINDS.iter().find(|(k, _)| *k == kind)?.1;
    if part.is_empty() {
        return None;
    }
    Some(format!("{} ({kind_label})", part.to_uppercase()))
}

/// The words for an internal id: its table label, an emulator backend as
/// `ATmega328P (simavr)`, or the id humanised. Never the raw `snake_case`.
#[must_use]
pub fn label_for(id: &str) -> String {
    if let Some(e) = index().get(id) {
        return e.label.clone();
    }
    backend_label(id).unwrap_or_else(|| humanise(id))
}

/// The one-line explanation for an id, when the table has one.
#[must_use]
pub fn explain_for(id: &str) -> Option<String> {
    index().get(id).and_then(|e| e.explain.clone())
}

/// A comma-joined list of ids as words (a CLI table cell), or `-` when empty.
#[must_use]
pub fn labels_for<S: AsRef<str>>(ids: &[S]) -> String {
    if ids.is_empty() {
        "-".to_string()
    } else {
        ids.iter()
            .map(|id| label_for(id.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ── The generated frontend module ───────────────────────────────────

/// Opens the generated region of `frontend/src/lib/labels.ts`.
pub const TS_BEGIN: &str =
    "// BEGIN GENERATED (crates/hauksbee-ir/src/labels.rs; do not hand-edit)";
/// Closes the generated region of `frontend/src/lib/labels.ts`.
pub const TS_END: &str = "// END GENERATED";

fn ts_str(s: &str) -> String {
    // Single quotes like the rest of the frontend; double quotes when the text
    // has an apostrophe and no double quote.
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The generated region of `frontend/src/lib/labels.ts`, markers included, with
/// a trailing newline. The file's hand-written helpers sit after it.
#[must_use]
pub fn typescript() -> String {
    let table = label_table();
    let mut out = String::new();
    let _ = writeln!(out, "{TS_BEGIN}");
    out.push_str(
        "// Regenerate: UPDATE_RUN_RECORD_SCHEMA=1 cargo test -p hauksbee-ir --test it drift\n\n",
    );
    out.push_str(
        "export interface LabelEntry {\n  id: string\n  group: string\n  label: string\n  explain?: string\n}\n\n",
    );
    out.push_str("export const LABEL_ENTRIES: readonly LabelEntry[] = [\n");
    for e in &table.entries {
        let _ = write!(
            out,
            "  {{ id: {}, group: {}, label: {}",
            ts_str(&e.id),
            ts_str(e.group),
            ts_str(&e.label)
        );
        if let Some(x) = &e.explain {
            let _ = write!(out, ", explain: {}", ts_str(x));
        }
        out.push_str(" },\n");
    }
    out.push_str("]\n\n");
    out.push_str("/** Words humanise() restores to their usual casing. */\n");
    out.push_str("export const ACRONYMS: Readonly<Record<string, string>> = {\n");
    for (k, v) in &table.acronyms {
        let _ = writeln!(out, "  {}: {},", ts_str(k), ts_str(v));
    }
    out.push_str("}\n\n");
    out.push_str(
        "/** The part of an emulator backend id before the colon, as shown after the chip. */\n",
    );
    out.push_str("export const BACKEND_KINDS: Readonly<Record<string, string>> = {\n");
    for k in &table.backend_kinds {
        let _ = writeln!(out, "  {}: {},", ts_str(k.kind), ts_str(k.label));
    }
    out.push_str("}\n");
    let _ = writeln!(out, "{TS_END}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanise_reads_like_a_sentence() {
        assert_eq!(
            humanise("board_crystal_frequency_binding"),
            "Board crystal frequency binding"
        );
        assert_eq!(humanise("spi_dual_quad_io"), "SPI dual quad I/O");
        assert_eq!(humanise("adc_injection"), "ADC injection");
        assert_eq!(humanise("i2c0_uart1_bridge"), "I2C0 UART1 bridge");
        assert_eq!(
            humanise("logic_levels_at_3v3_vddio"),
            "Logic levels at 3V3 VDDIO"
        );
        assert_eq!(humanise("some-kebab-id"), "Some kebab ID");
        assert_eq!(humanise("UPPER_CASE_ID"), "Upper case ID");
        assert_eq!(humanise(""), "");
    }

    #[test]
    fn each_acronym_is_listed_once_in_lower_case() {
        let mut seen = HashSet::new();
        for (k, v) in ACRONYMS {
            assert!(seen.insert(*k), "{k} is listed twice");
            assert_eq!(*k, k.to_lowercase(), "{k}");
            assert_eq!(
                k.to_lowercase(),
                v.to_lowercase().replace('/', ""),
                "{k} -> {v}"
            );
        }
    }

    #[test]
    fn an_unknown_backend_still_reads_as_a_chip() {
        assert_eq!(label_for("simavr:atmega328p"), "ATmega328P (simavr)");
        assert_eq!(label_for("simavr:atmega2560"), "ATMEGA2560 (simavr)");
        assert_eq!(label_for("none:samd21"), "SAMD21 (no emulator)");
        assert_eq!(label_for("bogus:thing"), "Bogus:thing");
    }

    #[test]
    fn a_stage_uses_the_glossary_words() {
        assert_eq!(label_for("identity_only"), Term::IdentityOnly.entry().label);
        assert_eq!(
            label_for("executable_partial"),
            Term::ExecutablePartial.entry().label
        );
        assert_eq!(
            label_for("executable_scope_unspecified"),
            Term::ScopeUnspecified.entry().label
        );
        assert_eq!(label_for("unresolved"), Term::OpenPart.entry().label);
    }

    /// Solver methods, supply kinds and USB profiles never reach a reader as
    /// the id the engine or the spec writes.
    #[test]
    fn solver_methods_and_supply_kinds_have_plain_words() {
        for (id, want) in [
            ("trap", "Trapezoidal"),
            ("trapezoidal", "Trapezoidal"),
            ("gear2", "Gear 2"),
            ("backward-euler", "Backward Euler"),
            ("reduced-step", "Smaller steps"),
            ("ideal", "Ideal source"),
            ("bench", "Bench supply"),
            ("wall", "Wall adapter"),
            ("usb", "USB port"),
            ("battery", "Battery"),
            ("5v1.5a", "5 V, 1.5 A"),
            ("v5_0_5a", "5 V, 0.5 A"),
            ("li_ion", "Li-ion"),
            ("ni_mh", "NiMH"),
        ] {
            assert_eq!(label_for(id), want, "{id}");
        }
        // Every method the solver can report has a row of its own, so the
        // fallback humaniser never has to guess.
        for method in [
            "trapezoidal",
            "gear2",
            "backward-euler",
            "reduced-step",
            "cold-start-backward-euler",
            "subdivided-backward-euler",
        ] {
            let row = entries().into_iter().find(|e| e.id == method);
            assert_eq!(row.map(|e| e.group), Some(GROUP_SOLVER_METHOD), "{method}");
        }
        for id in ["trap", "gear2", "bench", "usb"] {
            assert!(explain_for(id).is_some(), "{id} has no tooltip");
        }
        // The rows do not shadow an id another group already owns.
        let dropped = dropped_duplicates();
        for id in ["trap", "ideal", "bench", "wall", "usb", "battery"] {
            assert!(!dropped.iter().any(|d| d == id), "{id} is already taken");
        }
    }

    #[test]
    fn generic_mosfet_models_read_as_what_they_stand_in_for() {
        assert_eq!(
            label_for("generic_pmos_sot_corner_high_vth"),
            "Generic small-signal P-channel MOSFET at its high-threshold corner"
        );
        assert_eq!(
            label_for("generic_nmos_power_corner_low_vth"),
            "Generic power N-channel MOSFET at its low-threshold corner"
        );
        assert_eq!(
            label_for("generic_nmos_class"),
            "Generic other-package N-channel MOSFET"
        );
        // No comma inside a label, so a list of them in a sentence stays a list.
        for e in entries().iter().filter(|e| e.group == GROUP_GENERIC_MODEL) {
            assert!(!e.label.contains(','), "{:?}", e.label);
            assert!(e.explain.is_some(), "{} has no tooltip", e.id);
        }
    }

    #[test]
    fn input_formats_have_plain_words_and_assumption_kinds_use_the_glossary() {
        assert_eq!(label_for("ki_cad_pcb"), "KiCad board");
        assert_eq!(label_for("gerber_archive"), "Gerber files");
        // An assumption kind is a glossary term of the same id, so the table
        // has one word for it, not two.
        assert_eq!(label_for("open_part"), Term::OpenPart.entry().label);
        assert_eq!(label_for("reduced_fidelity"), "Reduced fidelity");
        assert_eq!(label_for("waived"), "Waived");
        assert_eq!(label_for("not_checked"), "Not checked");
    }

    #[test]
    fn labels_for_joins_or_dashes() {
        assert_eq!(labels_for::<&str>(&[]), "-");
        assert_eq!(
            labels_for(&["adc_injection", "uart"]),
            "ADC input injection, UART"
        );
    }
}
