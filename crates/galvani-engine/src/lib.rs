//! galvani-engine: the integration heart.
//!
//! This crate turns an [`ExtractedBoard`](galvani_extract::ExtractedBoard)
//! plus a [`ModelLibrary`](galvani_models::ModelLibrary) into a *live*
//! co-simulation that couples three domains:
//!
//! 1. **Analog** — the MNA transient solver in `galvani-solve`, fed a
//!    [`Circuit`](galvani_ir::Circuit) the [`binder`] builds by resolving
//!    every component to a model and stamping it as IR devices.
//! 2. **MCU** — emulated microcontroller cores from `galvani-mcu`, coupled at
//!    the pin level: GPIO output edges drive analog nets, analog node voltages
//!    are injected into ADC channels, UART passes through.
//! 3. **Digital** — behavioral ICs (shift registers, gates) handled by the
//!    [`digital`] event layer, NOT solved in MNA. Their inputs sample net
//!    voltages against `vih`/`vil`; their outputs drive nets as Thevenin
//!    sources stamped into the circuit.
//!
//! The [`scheduler::Scheduler`] steps all three in lockstep chunks
//! (generalizing the Tarski-Emulator pattern), and [`engine::GalvaniEngine`]
//! exposes the whole thing behind `galvani-server`'s `Engine` trait.

pub mod behavioral;
pub mod binder;
pub mod boardcode;
pub mod checks;
pub mod decoupling;
pub mod digital;
pub mod drivers;
pub mod engine;
pub mod peripherals;
pub mod power_supply;
pub mod report;
pub mod scheduler;
pub mod shorts;
pub mod stress;

pub use behavioral::{BehavioralDevice, CustomBehavior, CustomRegistry};
pub use binder::{bind_board, bind_board_with, BoundBoard};
pub use checks::usb_c::{
    classify_attach, classify_board, extract_sink_termination, Attach, Cable, CcResult,
    CcThresholds, PinState, Rp, SinkTermination,
};
pub use boardcode::{
    check_board_text, check_code, code_to_board_text, decompile_board_to_code, load_code,
    program_from_extracted, render_check_report, CheckOptions, CheckReport,
};
pub use decoupling::{apply_parasitics, CapClass, EsrEsl};
pub use engine::GalvaniEngine;
pub use peripherals::{
    controls::{Encoder, Potentiometer, Pushbutton, Stimulus, StimulusKind, ToggleSwitch},
    i2c::{Eeprom24c, I2cBus, I2cSlave, Lm75},
    load::DynamicLoad,
    sink::VcdSink,
    spi::{Mcp3008, Spi25Eeprom, SpiBus, SpiSlave},
    Peripheral, PeripheralSet, TickCtx, TimelineEvent,
};
pub use power_supply::{BatteryProtection, Chemistry, PowerSupply, SupplyLeg, UsbSpec};
pub use report::{BindOutcome, BindReport, BindRow};
pub use shorts::{AppliedShort, BRIDGE_OHMS};
pub use stress::{FaultEvent, FaultKind, StressMonitor};
