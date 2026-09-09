//! hauksbee-engine: the integration heart.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/README.md.
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
//! (generalizing the Tarski-Emulator pattern), and [`engine::HauksbeeEngine`]
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

pub mod boardcode;
pub mod commands;
pub mod deps;
pub mod evidence;
pub mod frontdoor;
pub mod plain;
pub mod reports;
pub mod result;
pub mod run_manifest;
pub mod web_design;
pub mod web_dist;
pub mod webcheck;
pub mod webextract;

// Lower layers, under their pre-split paths.
pub use hauksbee_bind::{
    asbuilt, behavioral, bind_report, binder, board_input, component_evidence, digital, drivers,
    firmware_input, logic, occurrence, power_supply, schematic_ties, shorts, stress, thermal,
    waiver,
};
pub use hauksbee_checks::{checks, dcpath, decoupling};
pub use hauksbee_cosim::{engine, error_budget, peripherals, responders, scheduler};

pub use behavioral::{BehavioralDevice, CustomBehavior, CustomRegistry};
pub use binder::{
    bind_board, bind_board_with, is_ground, names_a_supply_of_unknown_voltage, power_rail_voltage,
    BoundBoard,
};
pub use board_input::{BoardInputError, InputKind, NormalizedBoard};
pub use boardcode::{
    check_board_text, check_code, code_to_board_text, decompile_any_to_code,
    decompile_board_to_code, load_code, program_from_extracted, render_check_report, CheckOptions,
    CheckReport,
};
pub use checks::usb_c::{
    classify_attach, classify_board, extract_sink_termination, usb_c_report, Attach, Cable,
    CcResult, CcThresholds, PinState, Rp, SinkTermination, UsbcLevel, UsbcReport,
};
pub use decoupling::{apply_parasitics, CapClass, EsrEsl};
pub use engine::HauksbeeEngine;
pub use evidence::BoardEvidence;
// Re-export the firmware-path guard so downstream crates (hauksbee-ci) can
// validate a spec's firmware path before it reaches the native emulator loader,
// without taking a direct dependency on hauksbee-mcu.
pub use frontdoor::{
    analyze, analyze_json, analyze_with_firmware, analyze_with_firmware_detailed,
    analyze_with_firmware_json, WebCosimSection, WebFirmwareAnalysis, WebFirmwareCoverage,
    WebGpioNet, WebReport, WebSection,
};
pub use hauksbee_mcu::validate_firmware_path;
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
