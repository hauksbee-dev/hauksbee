//! One integration-test binary for the crate: every module below is compiled
//! and linked once instead of once per file.

#[allow(dead_code)]
mod support;

mod ac_stability;
mod adj_regulator_validity;
mod analog_invalid;
mod assembly_inputs_ci;
// Its cases are all AVR or Espressif QEMU co-sims.
#[cfg(any(feature = "avr", feature = "qemu"))]
mod boot_coverage;
mod charger_cell;
mod check_cli;
mod ci_report_schema_drift;
mod cli_diagnostics;
mod comparator_hysteresis;
#[cfg(feature = "renode")]
mod cosim_coverage_honesty;
mod d2pak_ldo;
mod derated_rating_ci;
mod doc_coverage;
mod evidence_spine_ci;
mod exit3_reachability;
mod firmware_guard;
mod firmware_input_ci;
mod flagship_brownout;
mod floating_net_verdict;
mod gate_drive;
mod generic_mosfet_corners;
mod hook_gate_e2e;
mod hwtrace;
mod indicator_intent;
mod init_scaffold;
mod inkplate_class_demo;
mod local_undermining;
mod manifest_cli_contract;
#[cfg(feature = "renode")]
mod mcu_descriptor_dir;
mod multiunit_keying;
#[cfg(any(feature = "avr", feature = "renode"))]
mod no_firmware_spec;
mod olimex_burst_calibration;
mod open_driver_validity;
mod open_part_criticality;
mod outcome_search;
mod packaged_asset_sync;
mod peripherals;
mod powerup_state_fuzz;
mod progress_stays_out_of_the_way;
// Every case is an AVR, Renode or Espressif QEMU co-sim of a real board.
#[cfg(any(feature = "avr", feature = "renode", feature = "qemu"))]
mod real_firmware_boards;
mod regulator_enable;
mod regulator_headroom_boundary;
mod regulator_validity;
mod round2_ci_surface;
mod round2_cli;
mod round3_cli;
mod schema_drift;
mod schematic_ci;
mod schmitt_oscillator;
mod sensor_attach;
mod shipped_examples_run;
mod shunt_reference;
mod spec_and_assertions;
mod spec_models_dir_and_waveforms;
mod spice_models_cli;
mod static_verdict_agreement;
#[cfg(any(feature = "avr", feature = "renode"))]
mod suggest_asserts;
mod tarski_outcome_search;
mod tarski_staged_replay;
mod timer555;
mod tolerance;
mod unpowered_rail_is_declared;
mod variant_sweep;
mod watchdog_coverage_hole;
// Boots the AVR blinky example through the web runner and the export route.
#[cfg(all(feature = "avr", feature = "serve"))]
mod web_export;
