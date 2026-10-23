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
//! * [`rail_owner`]: which supply nets may take an ideal source.
//! * [`program_resistor`]: the resistance a programming pin sees to ground.
//! * [`stress`], [`thermal`], [`shorts`]: the per-device envelopes and the
//!   fault monitor that watches them during a run.
//! * [`control_inputs`]: the FET gates whose level nothing in the DC netlist
//!   sets, so a rating judged at rest must not read them as 0 V.
//! * [`board_input`], [`firmware_input`], [`schematic_ties`],
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
pub mod component_evidence;
pub mod control_inputs;
pub mod digital;
pub mod dnp_rails;
pub mod drivers;
pub mod firmware_input;
pub mod logic;
pub mod mcu_clock;
pub mod mcu_electrical;
pub mod occurrence;
pub mod package_fit;
pub mod power_supply;
pub mod program_resistor;
pub mod rail_basis;
pub mod rail_owner;
pub mod schematic_ties;
pub mod shorts;
pub mod stress;
pub mod thermal;
pub mod variant_sweep;
pub mod vreg_enable;
pub mod waiver;

/// Distinct process exit code for "the board is invalid for the analysis you
/// asked for", a meaningless result, not a clean one.
///
/// Kept at this layer so the binder's refusals, the CLI and hauksbee-ci share
/// one source of truth.
pub const EXIT_INVALID_FOR_ANALYSIS: i32 = 3;

pub use behavioral::{BehavioralDevice, CustomBehavior, CustomRegistry};
pub use bind_report::{BindOutcome, BindReport, BindRow, BindSummary, UnresolvedActive};
pub use binder::{
    bind_board, bind_board_driven, bind_board_with, held_off_rails, is_ground,
    names_a_supply_of_unknown_voltage, power_rail_voltage, unpowered_supply_nets, BoundBoard,
};
pub use board_input::{BoardInputError, InputKind, NormalizedBoard};
pub use dnp_rails::{dnp_sourced_rails, DnpSourcedRail, DNP_CAUSE_OPENING};
pub use power_supply::{BatteryProtection, Chemistry, PowerSupply, SupplyLeg, UsbSpec};
pub use shorts::{AppliedShort, BRIDGE_OHMS};
pub use stress::{
    far_past_reason, implausible_bound, is_far_past_rating, is_implausible, overrides_stand_in,
    rating_noun, stress_clause, stress_quantity, FaultEvent, FaultKind, StressMonitor,
};
pub use thermal::{junction_temp_c, theta_ja_from_footprint, DEFAULT_AMBIENT_C, DEFAULT_THETA_JA};
