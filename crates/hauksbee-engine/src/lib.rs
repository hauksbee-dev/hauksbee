//! hauksbee-engine: the integration heart.
//!
//! This crate turns an [`ExtractedBoard`](hauksbee_extract::ExtractedBoard)
//! plus a [`ModelLibrary`](hauksbee_models::ModelLibrary) into a *live*
//! co-simulation that couples three domains:
//!
//! 1. **Analog**; the MNA transient solver in `hauksbee-solve`, fed a
//!    [`Circuit`](hauksbee_ir::Circuit) the [`binder`] builds by resolving
//!    every component to a model and stamping it as IR devices.
//! 2. **MCU**, emulated microcontroller cores from `hauksbee-mcu`, coupled at
//!    the pin level: GPIO output edges drive analog nets, analog node voltages
//!    are injected into ADC channels, UART passes through.
//! 3. **Digital**, behavioral ICs (shift registers, gates) handled by the
//!    [`digital`] event layer, NOT solved in MNA. Their inputs sample net
//!    voltages against `vih`/`vil`; their outputs drive nets as Thevenin
//!    sources stamped into the circuit.
//!
//! The [`scheduler::Scheduler`] steps all three in lockstep chunks
//! (generalizing an earlier board-specific emulator's lockstep pattern), and [`engine::HauksbeeEngine`]
//! exposes the whole thing behind `hauksbee-server`'s `Engine` trait.
//!
//! The implementation is layered across four crates so an edit rebuilds one
//! layer and the layers compile in parallel; this crate is the top:
//!
//! * `hauksbee-bind`: the binder, the device models it instantiates
//!   (digital, logic, behavioral, drivers, power supply), stress/thermal
//!   envelopes and the board / firmware / schematic inputs.
//! * `hauksbee-cosim`: the scheduler, peripherals, responders and
//!   `HauksbeeEngine`.
//! * `hauksbee-checks`: the static check family, decoupling and DC-path.
//! * `hauksbee-engine` (this crate): results, reports, the web front door,
//!   the CLI commands and the binary.
//!
//! Every module of the lower layers is re-exported below under the path it
//! had before the split, so `hauksbee_engine::binder::bind_board` and friends
//! are unchanged for downstream crates.

// Printing: every file that prints imports `print!`/`println!`/`eprint!`/`eprintln!`
// from `hauksbee_ir` (defined once in `hauksbee_ir::terminal_safe`), so board-file
// text cannot reach a terminal raw.
pub mod commands;
pub mod deps;
pub mod evidence;
pub mod frontdoor;
pub mod import_failure;
pub mod plain;
pub mod reports;
pub mod result;
pub mod run_manifest;
pub mod scope;
pub mod verdict_conditions;
pub mod web_actions;
pub mod web_design;
pub mod web_dist;
pub mod web_transport;
pub mod webcheck;
pub mod webextract;
pub mod websettings;

/// The verified Hauksbee source commit `build.rs` stamped into this build.
///
/// Absent outside this repository's own clean checkout (a source tarball, a
/// vendored copy), in which case the bare crate version is all we honestly
/// have. `hauksbee-ci` and `hauksbee-mcp` report this same commit.
pub const GIT_HASH: Option<&str> = option_env!("GIT_HASH");

/// The release tag (`v` + the workspace version) `build.rs` verified for this
/// build, if any.
pub const GIT_TAG: Option<&str> = option_env!("GIT_TAG");

/// The `--version` string: crate version plus [`GIT_HASH`].
///
/// The hash is what lets an operator tie behaviour to an exact build. Returned
/// as `&'static str` because that is what clap's `version` wants; the one-time
/// `OnceLock` init is the cheapest way to a static composed string.
#[must_use]
pub fn version_string() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let v = env!("CARGO_PKG_VERSION");
    V.get_or_init(|| GIT_HASH.map_or_else(|| v.into(), |h| format!("{v} (git {h})")))
}

// Lower layers, under their pre-split paths.
pub use hauksbee_bind::{
    asbuilt, behavioral, bind_report, binder, board_input, component_evidence, digital, drivers,
    firmware_input, logic, occurrence, power_supply, rail_owner, schematic_ties, shorts, stress,
    thermal, variant_sweep, waiver,
};
pub use hauksbee_checks::{checks, dcpath, decoupling};
pub use hauksbee_cosim::{engine, error_budget, execution, peripherals, responders, scheduler};

pub use behavioral::{BehavioralDevice, CustomBehavior, CustomRegistry};
pub use binder::{
    bind_board, bind_board_driven, bind_board_with, held_off_rails, is_ground,
    names_a_supply_of_unknown_voltage, power_rail_voltage, unpowered_supply_nets, BoundBoard,
};
pub use board_input::{BoardInputError, InputKind, NormalizedBoard};
pub use checks::usb_c::{
    classify_attach, classify_board, extract_sink_termination, usb_c_report, Attach, Cable,
    CcResult, CcThresholds, PinState, Rp, SinkTermination, UsbcLevel, UsbcReport,
};
pub use decoupling::{apply_parasitics, CapClass, EsrEsl};
pub use engine::HauksbeeEngine;
pub use evidence::BoardEvidence;
pub use hauksbee_bind::dnp_rails::{dnp_sourced_rails, DnpSourcedRail, DNP_CAUSE_OPENING};
// Re-export the firmware-path guard so downstream crates (hauksbee-ci) can
// validate a spec's firmware path before it reaches the native emulator loader,
// without taking a direct dependency on hauksbee-mcu.
pub use frontdoor::{
    analyze, analyze_json, analyze_with_firmware, analyze_with_firmware_detailed,
    analyze_with_firmware_json, WebCosimSection, WebFirmwareAnalysis, WebFirmwareCoverage,
    WebGpioNet, WebReport, WebSection,
};
pub use hauksbee_mcu::{validate_firmware_format, validate_firmware_path};
pub use peripherals::{
    controls::{Encoder, Potentiometer, Pushbutton, Stimulus, StimulusKind, ToggleSwitch},
    i2c::{Eeprom24c, I2cBus, I2cSlave, Lm75},
    load::DynamicLoad,
    sink::VcdSink,
    spi::{
        CsProvenance, Mcp3008, ResolvedCs, Spi25Eeprom, SpiBus, SpiFramingMode, SpiNorFlash,
        SpiSlave,
    },
    Peripheral, PeripheralSet, RegisterMapSensor, TickCtx, TimelineEvent,
};
pub use plain::{
    plain_drc, plain_drc_structured, plain_faults, plain_netlint, plain_si, render_drc_condensed,
    PlainFinding, PlainLevel, PlainReport,
};
pub use power_supply::{BatteryProtection, Chemistry, PowerSupply, SupplyLeg, UsbSpec};
pub use reports::bind::{BindOutcome, BindReport, BindRow};
pub use responders::{
    BitBangSpiPins, BitBangSpiResponder, InputResponder, ResponderRegistry, SoftI2cResponder,
};
pub use result::{
    ac_is_all_sentinel, no_signal_path_reason, thermal_validity, BindSummary, DrcStructured,
    Validity, EXIT_INVALID_FOR_ANALYSIS,
};
pub use shorts::{AppliedShort, BRIDGE_OHMS};
pub use stress::{FaultEvent, FaultKind, StressMonitor};
pub use thermal::{junction_temp_c, theta_ja_from_footprint, DEFAULT_AMBIENT_C, DEFAULT_THETA_JA};
