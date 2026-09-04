//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "avr_hex_flash_bounds.rs"]
mod avr_hex_flash_bounds;
#[path = "avr_mcu_tests.rs"]
mod avr_mcu_tests;
#[path = "avr_run_clock.rs"]
mod avr_run_clock;
#[path = "avr_twi_ack_gate.rs"]
mod avr_twi_ack_gate;
#[path = "avr_uart_flow.rs"]
mod avr_uart_flow;
#[path = "avr_watchdog.rs"]
mod avr_watchdog;
#[path = "child_reaping.rs"]
mod child_reaping;
#[path = "clock_truth.rs"]
mod clock_truth;
#[path = "demo_firmware.rs"]
mod demo_firmware;
#[path = "firmware_arch_gate.rs"]
mod firmware_arch_gate;
#[path = "host_serial_pty.rs"]
mod host_serial_pty;
#[path = "qemu_clock_truth.rs"]
mod qemu_clock_truth;
#[path = "soc_descriptors.rs"]
mod soc_descriptors;
