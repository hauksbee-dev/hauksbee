//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "ac_active_models.rs"]
mod ac_active_models;
#[path = "altium_no_fabricated_values.rs"]
mod altium_no_fabricated_values;
#[path = "apin_gpio_bind.rs"]
mod apin_gpio_bind;
#[path = "avr_at28_atomic_cosim.rs"]
mod avr_at28_atomic_cosim;
#[path = "avr_gpio_release_cosim.rs"]
mod avr_gpio_release_cosim;
#[path = "behavioral_faults.rs"]
mod behavioral_faults;
#[path = "behavioral_framework.rs"]
mod behavioral_framework;
#[path = "behavioral_vreg_rail.rs"]
mod behavioral_vreg_rail;
#[path = "binder_correctness.rs"]
mod binder_correctness;
#[path = "binder_gpio_promotion.rs"]
mod binder_gpio_promotion;
#[path = "binder_pin_map_merge.rs"]
mod binder_pin_map_merge;
#[path = "bitbang_spi_cosim.rs"]
mod bitbang_spi_cosim;
#[path = "boardcode_run.rs"]
mod boardcode_run;
#[path = "bom_identity.rs"]
mod bom_identity;
#[path = "bom_placement_cli.rs"]
mod bom_placement_cli;
#[path = "bug_hunt_physics.rs"]
mod bug_hunt_physics;
#[path = "bundled_sensor_catalog.rs"]
mod bundled_sensor_catalog;
#[path = "ci_artifacts_cosim.rs"]
mod ci_artifacts_cosim;
#[path = "cli_boardcode.rs"]
mod cli_boardcode;
#[path = "cli_doctor.rs"]
mod cli_doctor;
#[path = "cli_eagle_tie_contract.rs"]
mod cli_eagle_tie_contract;
#[path = "cli_firmware_guard.rs"]
mod cli_firmware_guard;
#[path = "cli_manifest_contract.rs"]
mod cli_manifest_contract;
#[path = "cli_models_cmds.rs"]
mod cli_models_cmds;
#[path = "cli_models_lint.rs"]
mod cli_models_lint;
#[path = "cli_probe_csv.rs"]
mod cli_probe_csv;
#[path = "cli_refusal_surfaces.rs"]
mod cli_refusal_surfaces;
#[path = "cli_round2_fixes.rs"]
mod cli_round2_fixes;
#[path = "cli_round3_fixes.rs"]
mod cli_round3_fixes;
#[path = "cli_sim_help_honesty.rs"]
mod cli_sim_help_honesty;
#[path = "cli_strict_plain.rs"]
mod cli_strict_plain;
#[path = "cli_watch.rs"]
mod cli_watch;
#[path = "contention_lint_corpus.rs"]
mod contention_lint_corpus;
#[path = "cosim_failed_chunk.rs"]
mod cosim_failed_chunk;
#[path = "cosim_fallback_chunk.rs"]
mod cosim_fallback_chunk;
#[path = "cosim_spi_cs_frames_transactions.rs"]
mod cosim_spi_cs_frames_transactions;
#[path = "datasheet_validation.rs"]
mod datasheet_validation;
#[path = "declarative_sensor_cosim.rs"]
mod declarative_sensor_cosim;
#[path = "diode_fallback.rs"]
mod diode_fallback;
#[path = "dnp_processor.rs"]
mod dnp_processor;
#[path = "drive_override_is_loud.rs"]
mod drive_override_is_loud;
#[path = "eagle_web_report.rs"]
mod eagle_web_report;
#[path = "esp32_qemu_cosim.rs"]
mod esp32_qemu_cosim;
#[path = "evidence_spine_end_to_end.rs"]
mod evidence_spine_end_to_end;
#[path = "extract_consent.rs"]
mod extract_consent;
#[path = "failed_chunk_reason.rs"]
mod failed_chunk_reason;
#[path = "faults.rs"]
mod faults;
#[path = "gerber_determinism.rs"]
mod gerber_determinism;
#[path = "host_serial_cosim.rs"]
mod host_serial_cosim;
#[path = "i2c_sensor_cosim.rs"]
mod i2c_sensor_cosim;
#[path = "i2c_sensor_cosim_qemu.rs"]
mod i2c_sensor_cosim_qemu;
#[path = "i2c_sensor_cosim_renode.rs"]
mod i2c_sensor_cosim_renode;
#[path = "interactive_coverage_parity.rs"]
mod interactive_coverage_parity;
#[path = "json_finding_public_api.rs"]
mod json_finding_public_api;
#[path = "live_divergence_usb_clamp.rs"]
mod live_divergence_usb_clamp;
#[path = "live_peripheral_attach.rs"]
mod live_peripheral_attach;
#[path = "logic_gates_74hc.rs"]
mod logic_gates_74hc;
#[path = "logic_migration.rs"]
mod logic_migration;
#[path = "mcu_family_router.rs"]
mod mcu_family_router;
#[path = "model_check.rs"]
mod model_check;
#[path = "models_resolve_layers.rs"]
mod models_resolve_layers;
#[path = "multi_spi_dispatch.rs"]
mod multi_spi_dispatch;
#[path = "netlist_drc_honesty.rs"]
mod netlist_drc_honesty;
#[path = "packaged_asset_sync.rs"]
mod packaged_asset_sync;
#[path = "pin_role_rules.rs"]
mod pin_role_rules;
#[path = "power_supply.rs"]
mod power_supply;
#[path = "rail_suppression_and_cc_floor.rs"]
mod rail_suppression_and_cc_floor;
#[path = "refusal_contract.rs"]
mod refusal_contract;
#[path = "renode_cosim_coverage_honesty.rs"]
mod renode_cosim_coverage_honesty;
#[path = "renode_riscv_arm_cosim.rs"]
mod renode_riscv_arm_cosim;
#[path = "run_manifest_contract.rs"]
mod run_manifest_contract;
#[path = "run_report_schema_drift.rs"]
mod run_report_schema_drift;
#[path = "sample_boards.rs"]
mod sample_boards;
#[path = "schematic_bind.rs"]
mod schematic_bind;
#[path = "shorts.rs"]
mod shorts;
#[path = "si_ampacity_ripple.rs"]
mod si_ampacity_ripple;
#[path = "soft_i2c_cosim.rs"]
mod soft_i2c_cosim;
#[path = "spi_sensor_cosim_qemu.rs"]
mod spi_sensor_cosim_qemu;
#[path = "spi_sensor_cosim_renode.rs"]
mod spi_sensor_cosim_renode;
#[path = "static_pass_says_its_limits.rs"]
mod static_pass_says_its_limits;
#[path = "stm32_bind_check.rs"]
mod stm32_bind_check;
#[path = "stm32_clock_readiness_cosim.rs"]
mod stm32_clock_readiness_cosim;
#[path = "stm32_renode_cosim.rs"]
mod stm32_renode_cosim;
#[path = "stormduino_bind.rs"]
mod stormduino_bind;
#[path = "strap_lint_corpus.rs"]
mod strap_lint_corpus;
#[path = "synthetic_cosim.rs"]
mod synthetic_cosim;
#[path = "thermal.rs"]
mod thermal;
#[path = "usb_c_double_termination.rs"]
mod usb_c_double_termination;
#[path = "usb_c_rpi4.rs"]
mod usb_c_rpi4;
#[path = "verdict_contract.rs"]
mod verdict_contract;
#[path = "waiver_gate.rs"]
mod waiver_gate;
#[path = "watchdog_coverage_surfaces.rs"]
mod watchdog_coverage_surfaces;
#[path = "zero_ohm_jumper.rs"]
mod zero_ohm_jumper;
#[path = "zero_ohm_link.rs"]
mod zero_ohm_link;
