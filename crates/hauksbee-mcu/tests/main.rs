//! One integration-test binary for the crate: every module below is compiled
//! and linked once instead of once per file.

#[allow(dead_code)]
#[path = "support.rs"]
mod support;

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
#[path = "esp_qemu_install.rs"]
mod esp_qemu_install;
#[path = "firmware_arch_gate.rs"]
mod firmware_arch_gate;
#[path = "host_serial_pty.rs"]
mod host_serial_pty;
#[path = "qemu_bus_mailbox.rs"]
mod qemu_bus_mailbox;
#[path = "qemu_clock_truth.rs"]
mod qemu_clock_truth;
#[path = "qemu_gpio_register_state.rs"]
mod qemu_gpio_register_state;
#[path = "qemu_run_window.rs"]
mod qemu_run_window;
#[path = "renode_adc_injection.rs"]
mod renode_adc_injection;
#[path = "renode_nrf52840_bus.rs"]
mod renode_nrf52840_bus;
#[path = "renode_rp2040.rs"]
mod renode_rp2040;
#[path = "renode_rp2040_adc.rs"]
mod renode_rp2040_adc;
#[path = "renode_rp2040_bus.rs"]
mod renode_rp2040_bus;
#[path = "renode_stm32.rs"]
mod renode_stm32;
#[path = "renode_stm32f072.rs"]
mod renode_stm32f072;
#[path = "soc_descriptors.rs"]
mod soc_descriptors;
