//! hauksbee-bind: the binding layer of the engine.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/README.md.
//!
//! This crate turns an [`ExtractedBoard`](hauksbee_extract::ExtractedBoard)
//! plus a [`ModelLibrary`](hauksbee_models::ModelLibrary) into a
//! [`BoundBoard`]: the [`binder`] resolves every component to a model and
//! stamps it as IR devices, digital blocks or behavioral sources. Everything
//! the bound board is made of lives here too, so the co-simulation layer
//! (`hauksbee-cosim`) and the static checks (`hauksbee-checks`) can share one
//! definition of a device:
//!
//! * [`drivers`], [`digital`], [`logic`], [`behavioral`], [`power_supply`]:
//!   the device models the binder instantiates.
//! * [`stress`], [`thermal`], [`shorts`]: the per-device envelopes and the
//!   fault monitor that watches them during a run.
//! * [`board_input`], [`firmware_input`], [`boardcode`], [`schematic_ties`],
//!   [`asbuilt`], [`waiver`]: the inputs a run is bound from.
//! * [`bind_report`]: the per-component bind verdicts and their summary.
//!
//! It deliberately depends on neither the analog solver nor the MCU emulators:
//! a change to the scheduler or a report never rebuilds this layer.

pub mod asbuilt;
pub mod behavioral;
pub mod bind_report;
pub mod binder;
pub mod board_input;
pub mod boardcode;
pub mod component_evidence;
pub mod digital;
pub mod drivers;
pub mod firmware_input;
pub mod logic;
pub mod occurrence;
pub mod power_supply;
pub mod schematic_ties;
pub mod shorts;
pub mod stress;
pub mod thermal;
pub mod waiver;

/// Distinct process exit code for "the board is invalid for the analysis you
/// asked for", a meaningless result, not a clean one. Kept at this layer so the
/// binder's refusals, the CLI and hauksbee-ci share one source of truth.
pub const EXIT_INVALID_FOR_ANALYSIS: i32 = 3;

pub use behavioral::{BehavioralDevice, CustomBehavior, CustomRegistry};
pub use bind_report::{BindOutcome, BindReport, BindRow, BindSummary, UnresolvedActive};
pub use binder::{
    bind_board, bind_board_with, is_ground, names_a_supply_of_unknown_voltage, power_rail_voltage,
    BoundBoard,
};
pub use board_input::{BoardInputError, InputKind, NormalizedBoard};
pub use boardcode::{
    code_to_board_text, decompile_any_to_code, decompile_board_to_code, load_code,
    program_from_extracted,
};
pub use power_supply::{BatteryProtection, Chemistry, PowerSupply, SupplyLeg, UsbSpec};
pub use shorts::{AppliedShort, BRIDGE_OHMS};
pub use stress::{FaultEvent, FaultKind, StressMonitor};
pub use thermal::{junction_temp_c, theta_ja_from_footprint, DEFAULT_AMBIENT_C, DEFAULT_THETA_JA};
