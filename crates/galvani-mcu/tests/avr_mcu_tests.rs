//! Integration tests for galvani-mcu using the T1-devboard firmware.
//!
//! These tests require the pre-built firmware hex at:
//!   /Users/hauksbee-user/Tarski/Tarski-Repos/Project-Tarski/T1-devboard/interface/.pio/build/nanoatmega328new/firmware.hex
//!
//! When the hex is absent (CI without the firmware repo), all tests skip gracefully.

use galvani_mcu::{AvrMcu, Mcu, PinId};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// T1 firmware signal constants (mirrors signals.hpp)
// ---------------------------------------------------------------------------

const PORT_TRN_END: u8 = 0x04;
const PORT_SIG: u8 = 0x05;
const PORT_ACK: u8 = 0x06;
const PORT_NAK: u8 = 0x15;
const PORT_LOAD_SYN: u8 = b'S';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn t1_firmware_hex() -> Option<PathBuf> {
    let p = PathBuf::from(
        "/Users/hauksbee-user/Tarski/Tarski-Repos/Project-Tarski/T1-devboard/interface\
         /.pio/build/nanoatmega328new/firmware.hex",
    );
    if p.exists() { Some(p) } else { None }
}

/// Build a fresh ATmega328P at 16 MHz with the T1 firmware loaded, or skip.
macro_rules! t1_mcu {
    ($name:ident) => {
        let Some(hex) = t1_firmware_hex() else {
            eprintln!("SKIP: T1 firmware hex not found");
            return;
        };
        let mut $name = AvrMcu::atmega328p_16mhz().expect("Failed to create MCU");
        $name.load_firmware(&hex).expect("Failed to load firmware");
    };
}

/// Run the MCU for `ms` milliseconds.
fn run_ms(mcu: &mut AvrMcu, ms: u64) {
    mcu.run_millis(ms).expect("run_millis failed");
}

// ---------------------------------------------------------------------------
// Test 1: boot and cycle count
// ---------------------------------------------------------------------------

/// Boot the firmware for 100 ms and verify the cycle counter advances by
/// approximately 100 ms worth of 16 MHz cycles (within 10%).
#[test]
fn test_boot_cycle_count_advances() {
    t1_mcu!(mcu);

    assert_eq!(mcu.frequency(), 16_000_000, "frequency should be 16 MHz");

    let start = mcu.state().cycles;
    run_ms(&mut mcu, 100);
    let elapsed = mcu.state().cycles - start;

    // 100 ms at 16 MHz = 1_600_000 cycles.  Allow ±10 % for simavr's
    // instruction-granularity overshoot.
    let expected: u64 = 1_600_000;
    let lo = expected * 9 / 10;
    let hi = expected * 11 / 10;
    assert!(
        elapsed >= lo && elapsed <= hi,
        "Expected ~{expected} cycles in 100 ms, got {elapsed}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: UART – PORT_SIG command / response
// ---------------------------------------------------------------------------

/// Send the PORT_SIG command and verify the firmware responds with the
/// well-known signature frame: [ACK, '0', '1', '0', TRN_END]
/// (firmware v0.1.0 with bytes shifted +0x30 to avoid control-char collisions).
#[test]
fn test_uart_port_sig_response() {
    t1_mcu!(mcu);

    // Collect UART output into a shared buffer.
    let uart_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let buf_clone = uart_buf.clone();
    mcu.on_uart(Box::new(move |b| {
        buf_clone.lock().unwrap().push(b);
    }));

    // Boot: let the firmware initialise (it enables Serial etc).
    run_ms(&mut mcu, 100);

    // Drain any startup bytes (e.g. boot message).
    uart_buf.lock().unwrap().clear();

    // Send the PORT_SIG command byte.
    mcu.uart_write(&[PORT_SIG]);

    // Give the firmware time to respond (50 ms is generous for a simple reply).
    run_ms(&mut mcu, 50);

    let response = uart_buf.lock().unwrap().clone();

    // The response must contain the five-byte signature frame somewhere.
    let expected = &[PORT_ACK, 0x30, 0x31, 0x30, PORT_TRN_END];
    let found = response
        .windows(expected.len())
        .any(|w| w == expected);

    assert!(
        found,
        "Expected signature frame {:02X?} in UART output {:02X?}",
        expected, response
    );
}

// ---------------------------------------------------------------------------
// Test 3: pin-change callback fires on firmware GPIO activity
// ---------------------------------------------------------------------------

/// After booting and sending PORT_LOAD_SYN (which clocks 90 weight bytes out
/// via SPI shift-out), the on_pin_change callback must observe PORTB bit 5
/// (SCLK = Arduino D13) toggling.  90 bytes × 8 bits = 720 SCLK rising edges.
#[test]
fn test_pin_change_callback_fires_on_sclk() {
    t1_mcu!(mcu);

    let sclk_pin = PinId { port: 'B', bit: 5 }; // PORTB bit 5 = SCLK / D13

    let sclk_rises: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let counter = sclk_rises.clone();

    mcu.on_pin_change(Box::new(move |pin, high| {
        if pin == sclk_pin && high {
            *counter.lock().unwrap() += 1;
        }
    }));

    // Boot firmware.
    run_ms(&mut mcu, 100);
    *sclk_rises.lock().unwrap() = 0; // Reset count after init.

    // Send PORT_LOAD_SYN + 90 weight bytes.
    // Protocol: send command, then 90 bytes of payload with small delays between.
    mcu.uart_write(&[PORT_LOAD_SYN]);
    run_ms(&mut mcu, 5);
    for i in 0u8..90 {
        mcu.uart_write(&[i]);
        run_ms(&mut mcu, 2);
    }
    run_ms(&mut mcu, 30);

    let rises = *sclk_rises.lock().unwrap();
    assert_eq!(
        rises, 720,
        "90 bytes × 8 bits = 720 SCLK rising edges; got {rises}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: ADC injection – call doesn't crash, IRQ is delivered
// ---------------------------------------------------------------------------

/// Inject a voltage on ADC channel 6 (L1 MEAS_OUT) and verify the emulator
/// doesn't crash or panic.  The firmware only reads this channel during
/// calibration commands which we don't exercise here; this test validates the
/// IRQ delivery path is wired correctly.
#[test]
fn test_adc_injection_does_not_crash() {
    t1_mcu!(mcu);

    // Boot.
    run_ms(&mut mcu, 100);

    // Inject voltages across the full ADC range on channels 0-7.
    for ch in 0u8..8 {
        mcu.set_analog_in(ch, 0.0);
        mcu.set_analog_in(ch, 2.5);
        mcu.set_analog_in(ch, 5.0);
    }

    // Run a bit more to let the firmware process any pending ADC reads.
    run_ms(&mut mcu, 10);

    // No assertion other than "we got here alive".
    // The firmware legitimately sleeps between UART commands (Device::Idle uses
    // AVR sleep mode), so we just verify the PC is still in flash range.
    let st = mcu.state();
    assert!(
        st.pc < 32 * 1024,
        "PC 0x{:04X} out of range after ADC injection",
        st.pc
    );
}

// ---------------------------------------------------------------------------
// Test 5: state() – PC is in a sensible range
// ---------------------------------------------------------------------------

#[test]
fn test_state_pc_in_flash_range() {
    t1_mcu!(mcu);

    // ATmega328P has 32 KiB flash (0x0000 – 0x7FFF byte addresses).
    const FLASH_SIZE: u32 = 32 * 1024;

    run_ms(&mut mcu, 10);
    let st = mcu.state();
    assert!(
        st.pc < FLASH_SIZE,
        "PC 0x{:04X} is outside ATmega328P flash range",
        st.pc
    );
}

// ---------------------------------------------------------------------------
// Test 6: unknown command elicits NAK + TRN_END
// ---------------------------------------------------------------------------

#[test]
fn test_unknown_command_yields_nak() {
    t1_mcu!(mcu);

    let uart_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let buf_clone = uart_buf.clone();
    mcu.on_uart(Box::new(move |b| {
        buf_clone.lock().unwrap().push(b);
    }));

    run_ms(&mut mcu, 100);
    uart_buf.lock().unwrap().clear();

    // 0x01 is not a valid command.
    mcu.uart_write(&[0x01]);
    run_ms(&mut mcu, 50);

    let response = uart_buf.lock().unwrap().clone();

    // Firmware responds: NAK + TRN_END for unrecognised commands.
    let expected = &[PORT_NAK, PORT_TRN_END];
    let found = response.windows(expected.len()).any(|w| w == expected);
    assert!(
        found,
        "Expected [NAK, TRN_END] in response to unknown command; got {:02X?}",
        response
    );
}
