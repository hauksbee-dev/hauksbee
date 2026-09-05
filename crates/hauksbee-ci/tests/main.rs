//! One integration-test binary for the crate: every module below is compiled
//! and linked once instead of once per file.

#[path = "ac_stability.rs"]
mod ac_stability;
#[path = "analog_invalid.rs"]
mod analog_invalid;
#[path = "assembly_inputs_ci.rs"]
mod assembly_inputs_ci;
#[path = "boot_coverage.rs"]
mod boot_coverage;
#[path = "check_cli.rs"]
mod check_cli;
#[path = "ci_report_schema_drift.rs"]
mod ci_report_schema_drift;
#[path = "cli_diagnostics.rs"]
mod cli_diagnostics;
#[path = "cosim_coverage_honesty.rs"]
mod cosim_coverage_honesty;
#[path = "doc_coverage.rs"]
mod doc_coverage;
#[path = "evidence_spine_ci.rs"]
mod evidence_spine_ci;
#[path = "exit3_reachability.rs"]
mod exit3_reachability;
#[path = "firmware_guard.rs"]
mod firmware_guard;
#[path = "firmware_input_ci.rs"]
mod firmware_input_ci;
#[path = "flagship_brownout.rs"]
mod flagship_brownout;
#[path = "floating_net_verdict.rs"]
mod floating_net_verdict;
#[path = "hook_gate_e2e.rs"]
mod hook_gate_e2e;
#[path = "hwtrace.rs"]
mod hwtrace;
#[path = "init_scaffold.rs"]
mod init_scaffold;
#[path = "inkplate_class_demo.rs"]
mod inkplate_class_demo;
#[path = "mcu_descriptor_dir.rs"]
mod mcu_descriptor_dir;
#[path = "multiunit_keying.rs"]
mod multiunit_keying;
#[path = "olimex_burst_calibration.rs"]
mod olimex_burst_calibration;
#[path = "packaged_asset_sync.rs"]
mod packaged_asset_sync;
#[path = "peripherals.rs"]
mod peripherals;
#[path = "powerup_state_fuzz.rs"]
mod powerup_state_fuzz;
#[path = "progress_stays_out_of_the_way.rs"]
mod progress_stays_out_of_the_way;
#[path = "round2_ci_surface.rs"]
mod round2_ci_surface;
#[path = "round2_cli.rs"]
mod round2_cli;
#[path = "round3_cli.rs"]
mod round3_cli;
#[path = "schema_drift.rs"]
mod schema_drift;
#[path = "schematic_ci.rs"]
mod schematic_ci;
#[path = "sensor_attach.rs"]
mod sensor_attach;
#[path = "shipped_examples_run.rs"]
mod shipped_examples_run;
#[path = "spec_and_assertions.rs"]
mod spec_and_assertions;
#[path = "tarski_staged_replay.rs"]
mod tarski_staged_replay;
#[path = "tolerance.rs"]
mod tolerance;
#[path = "unpowered_rail_is_declared.rs"]
mod unpowered_rail_is_declared;
#[path = "watchdog_coverage_hole.rs"]
mod watchdog_coverage_hole;
