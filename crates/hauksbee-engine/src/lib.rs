//! hauksbee-engine: the integration heart.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/README.md.
//!
//! This crate turns an [`ExtractedBoard`](hauksbee_extract::ExtractedBoard)
//! plus a [`ModelLibrary`](hauksbee_models::ModelLibrary) into a *live*
//! co-simulation that couples three domains:
//!
//! 1. **Analog** — the MNA transient solver in `hauksbee-solve`, fed a
//!    [`Circuit`](hauksbee_ir::Circuit) the [`binder`] builds by resolving
//!    every component to a model and stamping it as IR devices.
//! 2. **MCU** — emulated microcontroller cores from `hauksbee-mcu`, coupled at
//!    the pin level: GPIO output edges drive analog nets, analog node voltages
//!    are injected into ADC channels, UART passes through.
//! 3. **Digital** — behavioral ICs (shift registers, gates) handled by the
//!    [`digital`] event layer, NOT solved in MNA. Their inputs sample net
//!    voltages against `vih`/`vil`; their outputs drive nets as Thevenin
//!    sources stamped into the circuit.
//!
//! The [`scheduler::Scheduler`] steps all three in lockstep chunks
//! (generalizing the Tarski-Emulator pattern), and [`engine::HauksbeeEngine`]
//! exposes the whole thing behind `hauksbee-server`'s `Engine` trait.

pub mod asbuilt;
pub mod behavioral;
pub mod binder;
pub mod board_input;
pub mod boardcode;
pub mod checks;
pub mod commands;
pub mod decoupling;
pub mod digital;
pub mod logic;
pub mod drivers;
pub mod engine;
pub mod firmware_input;
pub mod frontdoor;
pub mod webcheck;
pub mod peripherals;
pub mod plain;
pub mod power_supply;
pub mod report;
pub mod reports;
pub mod responders;
pub mod result;
pub mod scheduler;
pub mod shorts;
pub mod stress;
pub mod tarski_decomp;
pub mod tarski_prep;
pub mod thermal;
pub mod tui;

pub use behavioral::{BehavioralDevice, CustomBehavior, CustomRegistry};
pub use binder::{bind_board, bind_board_with, is_ground, power_rail_voltage, BoundBoard};
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
// Re-export the firmware-path guard so downstream crates (hauksbee-ci) can
// validate a spec's firmware path before it reaches the native emulator loader,
// without taking a direct dependency on hauksbee-mcu.
pub use hauksbee_mcu::validate_firmware_path;
pub use frontdoor::{
    analyze, analyze_json, analyze_with_firmware, analyze_with_firmware_json, WebCosimSection,
    WebGpioNet, WebReport, WebSection,
};
pub use peripherals::{
    controls::{Encoder, Potentiometer, Pushbutton, Stimulus, StimulusKind, ToggleSwitch},
    i2c::{Eeprom24c, I2cBus, I2cSlave, Lm75},
    load::DynamicLoad,
    sink::VcdSink,
    spi::{Mcp3008, Spi25Eeprom, SpiBus, SpiFramingMode, SpiSlave},
    Peripheral, PeripheralSet, RegisterMapSensor, TickCtx, TimelineEvent,
};
pub use plain::{
    plain_drc, plain_drc_structured, plain_faults, plain_netlint, plain_si, PlainFinding,
    PlainLevel, PlainReport,
};
pub use power_supply::{BatteryProtection, Chemistry, PowerSupply, SupplyLeg, UsbSpec};
pub use report::{BindOutcome, BindReport, BindRow};
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
