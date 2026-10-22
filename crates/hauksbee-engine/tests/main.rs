//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[allow(dead_code)]
#[path = "support.rs"]
mod support;

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
#[path = "bench_observability.rs"]
mod bench_observability;
#[path = "binder_correctness.rs"]
mod binder_correctness;
#[path = "binder_gpio_promotion.rs"]
mod binder_gpio_promotion;
#[path = "binder_pin_map_merge.rs"]
mod binder_pin_map_merge;
#[path = "bitbang_spi_cosim.rs"]
mod bitbang_spi_cosim;
#[path = "bom_identity.rs"]
mod bom_identity;
#[path = "bom_placement_cli.rs"]
mod bom_placement_cli;
#[path = "boot_straps_cosim.rs"]
mod boot_straps_cosim;
#[path = "bug_hunt_physics.rs"]
mod bug_hunt_physics;
#[path = "bundled_sensor_catalog.rs"]
mod bundled_sensor_catalog;
#[path = "card_physical_ranges.rs"]
mod card_physical_ranges;
#[path = "ci_artifacts_cosim.rs"]
mod ci_artifacts_cosim;
#[path = "cli_doctor.rs"]
mod cli_doctor;
#[path = "cli_eagle_tie_contract.rs"]
mod cli_eagle_tie_contract;
#[path = "cli_firmware_guard.rs"]
mod cli_firmware_guard;
#[path = "cli_hostile_input_wording.rs"]
mod cli_hostile_input_wording;
#[path = "cli_kicad_inputs.rs"]
mod cli_kicad_inputs;
#[path = "cli_manifest_contract.rs"]
mod cli_manifest_contract;
#[path = "cli_missing_subsheet.rs"]
mod cli_missing_subsheet;
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
#[path = "cli_run_plain_seconds.rs"]
mod cli_run_plain_seconds;
#[path = "cli_sim_help_honesty.rs"]
mod cli_sim_help_honesty;
#[path = "cli_strict_plain.rs"]
mod cli_strict_plain;
#[path = "cli_terminal_safe.rs"]
mod cli_terminal_safe;
#[path = "cli_watch.rs"]
mod cli_watch;
mod common;
#[path = "contention_lint_corpus.rs"]
mod contention_lint_corpus;
#[path = "cosim_comparator_flip.rs"]
mod cosim_comparator_flip;
#[path = "cosim_failed_chunk.rs"]
mod cosim_failed_chunk;
#[path = "cosim_fallback_chunk.rs"]
mod cosim_fallback_chunk;
#[path = "cosim_not_judged.rs"]
mod cosim_not_judged;
#[path = "cosim_spi_cs_frames_transactions.rs"]
mod cosim_spi_cs_frames_transactions;
#[path = "datasheet_validation.rs"]
mod datasheet_validation;
#[path = "dc_overstress.rs"]
mod dc_overstress;
#[path = "deck_sim_http.rs"]
mod deck_sim_http;
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
#[path = "easyeda_inputs.rs"]
mod easyeda_inputs;
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
#[path = "findings_complete.rs"]
mod findings_complete;
#[path = "generic_led_corners.rs"]
mod generic_led_corners;
#[path = "generic_mosfet_corner_check.rs"]
mod generic_mosfet_corner_check;
#[path = "gerber_determinism.rs"]
mod gerber_determinism;
#[path = "ground_names.rs"]
mod ground_names;
#[path = "hardware_history.rs"]
mod hardware_history;
#[path = "host_serial_cosim.rs"]
mod host_serial_cosim;
#[path = "i2c_sensor_cosim.rs"]
mod i2c_sensor_cosim;
#[path = "i2c_sensor_cosim_qemu.rs"]
mod i2c_sensor_cosim_qemu;
#[path = "i2c_sensor_cosim_renode.rs"]
mod i2c_sensor_cosim_renode;
#[path = "indicator_not_judged.rs"]
mod indicator_not_judged;
#[path = "inhibitory_miswire.rs"]
mod inhibitory_miswire;
#[path = "input_order_stable.rs"]
mod input_order_stable;
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
#[path = "nep_private_acceptance.rs"]
mod nep_private_acceptance;
#[path = "netlist_drc_honesty.rs"]
mod netlist_drc_honesty;
#[path = "netlist_edit_miswire.rs"]
mod netlist_edit_miswire;
#[path = "no_firmware_no_avr.rs"]
mod no_firmware_no_avr;
#[path = "open_load_qualifiers.rs"]
mod open_load_qualifiers;
#[path = "package_fit_binding.rs"]
mod package_fit_binding;
#[path = "packaged_asset_sync.rs"]
mod packaged_asset_sync;
#[path = "parts_fallback_ratings.rs"]
mod parts_fallback_ratings;
#[path = "pin_role_rules.rs"]
mod pin_role_rules;
#[path = "power_invariants.rs"]
mod power_invariants;
#[path = "power_pin_corpus.rs"]
mod power_pin_corpus;
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
#[path = "run_emit_netlist.rs"]
mod run_emit_netlist;
#[path = "run_manifest_contract.rs"]
mod run_manifest_contract;
#[path = "run_report_schema_drift.rs"]
mod run_report_schema_drift;
#[path = "run_spice_models.rs"]
mod run_spice_models;
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
#[path = "stale_fill_inspect.rs"]
mod stale_fill_inspect;
#[path = "static_check_scaling.rs"]
mod static_check_scaling;
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
#[path = "strong_drive_overstress.rs"]
mod strong_drive_overstress;
#[path = "supply_chain_scope.rs"]
mod supply_chain_scope;
#[path = "synthetic_cosim.rs"]
mod synthetic_cosim;
#[path = "tarski_stretcher_transient.rs"]
mod tarski_stretcher_transient;
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
