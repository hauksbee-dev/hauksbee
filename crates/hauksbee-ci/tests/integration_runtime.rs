//! Runtime and co-simulation integration tests bundled to avoid repeated linking.

#[path = "ac_stability.rs"]
mod ac_stability;
#[path = "analog_invalid.rs"]
mod analog_invalid;
#[path = "cosim_coverage_honesty.rs"]
mod cosim_coverage_honesty;
#[path = "flagship_brownout.rs"]
mod flagship_brownout;
#[path = "inkplate_class_demo.rs"]
mod inkplate_class_demo;
#[path = "olimex_burst_calibration.rs"]
mod olimex_burst_calibration;
#[path = "peripherals.rs"]
mod peripherals;
#[path = "powerup_state_fuzz.rs"]
mod powerup_state_fuzz;
#[path = "round2_ci_surface.rs"]
mod round2_ci_surface;
#[path = "schematic_ci.rs"]
mod schematic_ci;
#[path = "watchdog_coverage_hole.rs"]
mod watchdog_coverage_hole;
