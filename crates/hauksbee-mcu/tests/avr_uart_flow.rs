//! UART RX flow control and core reset, against a real emulated ATmega328P.
//!
//! Reproduces (and now guards) the NEP-board study's two SEV defects on the
//! flagship AVR + host-serial path:
//!
//! 1. `uart_write` used to raise every byte onto simavr's `UART_IRQ_INPUT` at
//!    a single sim instant. simavr's RX fifo is 64 bytes, so byte 65 onward of
//!    any host record vanished silently and protocol firmwares wedged forever
//!    mid-record. The fix queues bytes and drains them under simavr's own
//!    XON/XOFF flow control, so a 256-byte page write (the study's real
//!    reproduction) arrives complete.
//! 2. There was no way to reboot a wedged core: `Reset` rezeroed sim time but
//!    the MCU kept its wedged PC and SRAM. `Mcu::reset` now reboots the core.
//!
//! The firmware is hand-assembled inline (no toolchain dependency, same
//! pattern as avr_run_clock.rs): it configures UART0 at 115200, transmits one
//! boot banner byte, then echoes every received byte forever. The banner makes
//! a reboot observable; the echo makes delivery (and order) checkable.

#![cfg(feature = "avr")]

use hauksbee_mcu::{AvrMcu, Mcu};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Inline Intel-HEX fixture (mirrors avr_run_clock.rs)
// ---------------------------------------------------------------------------

fn ihex_record(record_type: u8, addr: u16, data: &[u8]) -> String {
    let mut bytes = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, record_type];
    bytes.extend_from_slice(data);
    let checksum = (!bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))).wrapping_add(1);
    bytes.push(checksum);
    let hexstr: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(":{hexstr}")
}

fn write_program_hex(name: &str, program: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join("hauksbee_avr_uart_flow");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(name);
    let mut text = String::new();
    for (i, chunk) in program.chunks(16).enumerate() {
        text.push_str(&ihex_record(0x00, (i * 16) as u16, chunk));
        text.push('\n');
    }
    text.push_str(":00000001FF\n");
    std::fs::write(&path, text).expect("write hex");
    path
}

/// The boot banner byte the firmware transmits once, immediately after
/// enabling the UART. Seeing it a second time is proof of a real reboot.
const BANNER: u8 = 0x42;

/// Hand-assembled ATmega328P program: UART0 at 115200 (U2X off is fine for the
/// emulator; the divisor only sets simavr's per-byte pacing), boot banner,
/// then a polling echo loop.
///
/// ```text
///  0x00  ldi  r16, 0x02        ; U2X0
///  0x01  sts  UCSR0A, r16      ; 0xC0
///  0x03  ldi  r16, 16          ; 115200 @ 16 MHz with U2X0
///  0x04  sts  UBRR0L, r16      ; 0xC4
///  0x06  ldi  r16, 0x18        ; RXEN0 | TXEN0
///  0x07  sts  UCSR0B, r16      ; 0xC1
///  0x09  ldi  r16, BANNER
///  0x0A  sts  UDR0, r16        ; 0xC6, boot banner (UDRE0 is set at reset)
///  0x0C  loop: lds r17, UCSR0A
///  0x0E  sbrs r17, 7           ; RXC0
///  0x0F  rjmp loop
///  0x10  lds  r18, UDR0
///  0x12  wait: lds r17, UCSR0A
///  0x14  sbrs r17, 5           ; UDRE0
///  0x15  rjmp wait
///  0x16  sts  UDR0, r18        ; echo
///  0x18  rjmp loop
/// ```
const UART_ECHO: &[u8] = &[
    0x02, 0xE0, // ldi r16, 0x02
    0x00, 0x93, 0xC0, 0x00, // sts UCSR0A, r16
    0x00, 0xE1, // ldi r16, 0x10
    0x00, 0x93, 0xC4, 0x00, // sts UBRR0L, r16
    0x08, 0xE1, // ldi r16, 0x18
    0x00, 0x93, 0xC1, 0x00, // sts UCSR0B, r16
    0x02, 0xE4, // ldi r16, 0x42 (BANNER)
    0x00, 0x93, 0xC6, 0x00, // sts UDR0, r16
    0x10, 0x91, 0xC0, 0x00, // loop: lds r17, UCSR0A
    0x17, 0xFF, // sbrs r17, 7
    0xFC, 0xCF, // rjmp loop
    0x20, 0x91, 0xC6, 0x00, // lds r18, UDR0
    0x10, 0x91, 0xC0, 0x00, // wait: lds r17, UCSR0A
    0x15, 0xFF, // sbrs r17, 5
    0xFC, 0xCF, // rjmp wait
    0x20, 0x93, 0xC6, 0x00, // sts UDR0, r18
    0xF3, 0xCF, // rjmp loop
];

/// Build a 16 MHz ATmega328P running the echo firmware, with UART output
/// captured into a shared buffer.
fn echo_mcu(name: &str) -> (AvrMcu, Arc<Mutex<Vec<u8>>>) {
    let hex = write_program_hex(name, UART_ECHO);
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("load echo firmware");
    let out = Arc::new(Mutex::new(Vec::new()));
    let sink = out.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
    (mcu, out)
}

/// Run in scheduler-sized chunks (100 µs), the same cadence the engine uses,
/// so the flow-control drain sees realistic chunk boundaries.
fn run_ms_chunked(mcu: &mut AvrMcu, ms: u64) {
    for _ in 0..(ms * 10) {
        mcu.run_micros(100).expect("run_micros");
    }
}

/// Defect 1 reproduction: a single host record much longer than simavr's
/// 64-byte RX fifo must arrive complete and in order. The study's realistic
/// case is a 256-byte EEPROM page write; use 256 distinct bytes so ordering
/// errors are visible too.
#[test]
fn record_longer_than_rx_fifo_is_delivered_completely() {
    let (mut mcu, out) = echo_mcu("echo_256.hex");

    // Boot: banner appears once RX/TX are enabled.
    run_ms_chunked(&mut mcu, 20);
    assert_eq!(
        out.lock().unwrap().as_slice(),
        &[BANNER],
        "firmware must boot and transmit its banner"
    );

    // One 256-byte record at a single instant, exactly what a host page write
    // delivers through the websocket.
    let record: Vec<u8> = (0u16..256).map(|b| b as u8).collect();
    mcu.uart_write(&record);

    // 256 bytes at 115200 baud is ~22 ms in and ~22 ms back out; give it
    // ample time. simavr paces RX delivery at cycles_per_byte, so this also
    // exercises the refill path (fifo drains, XON, queue refills) many times.
    run_ms_chunked(&mut mcu, 200);

    let got = out.lock().unwrap();
    assert_eq!(
        &got[1..],
        record.as_slice(),
        "every byte of a >64-byte record must be delivered, in order \
         (got {} of {} echoed)",
        got.len() - 1,
        record.len()
    );
    assert_eq!(mcu.uart_rx_overflow(), 0, "nothing may be dropped");
}

/// Bytes sent before the firmware has enabled its receiver must be held, not
/// silently dropped: simavr discards `UART_IRQ_INPUT` raises while RXEN is
/// clear, so eager injection at t=0 (host connects faster than the firmware
/// boots) lost the opening byte of the host protocol. The queue holds bytes
/// until the UART itself signals readiness (XON on RXEN-enable).
#[test]
fn bytes_sent_before_rx_enabled_are_held_not_dropped() {
    let (mut mcu, out) = echo_mcu("echo_early.hex");

    // Inject before a single cycle has run: the UART is still disabled.
    mcu.uart_write(b"Z");
    run_ms_chunked(&mut mcu, 20);

    assert_eq!(
        out.lock().unwrap().as_slice(),
        &[BANNER, b'Z'],
        "a byte sent before boot must be delivered once the UART is ready"
    );
}

/// Defect 2 reproduction: `Mcu::reset` must actually reboot the core. The
/// banner byte is transmitted from the reset vector path, so seeing it again
/// after `reset()` proves the firmware restarted rather than resuming its
/// wedged PC.
#[test]
fn reset_reboots_the_core_and_firmware_answers_again() {
    let (mut mcu, out) = echo_mcu("echo_reset.hex");

    run_ms_chunked(&mut mcu, 20);
    mcu.uart_write(b"A");
    run_ms_chunked(&mut mcu, 20);
    assert_eq!(out.lock().unwrap().as_slice(), &[BANNER, b'A']);

    mcu.reset().expect("AVR backend must support core reset");

    // After a reboot the firmware starts from the reset vector: banner again,
    // and the echo loop works again. Also proves the emulated cycle clock
    // restarted cleanly (run_micros would stall if run_target were stale).
    run_ms_chunked(&mut mcu, 20);
    mcu.uart_write(b"C");
    run_ms_chunked(&mut mcu, 20);
    assert_eq!(
        out.lock().unwrap().as_slice(),
        &[BANNER, b'A', BANNER, b'C'],
        "reset must reboot the firmware (banner re-sent) and leave it functional"
    );
}

/// A genuine overflow (host floods a firmware that never drains) must be
/// reported loudly, not swallowed: the pending queue is capped and the
/// overflow counter exposes exactly how many bytes were lost.
#[test]
fn pending_queue_overflow_is_counted_not_silent() {
    // Firmware that never enables the UART: two-byte infinite loop, so every
    // queued byte just sits there.
    let hex = write_program_hex("rjmp_self.hex", &[0xFF, 0xCF]); // rjmp .-2
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("load firmware");

    // Push well past any sane cap in modest slabs.
    let slab = vec![0u8; 64 * 1024];
    for _ in 0..40 {
        mcu.uart_write(&slab);
    }
    assert!(
        mcu.uart_rx_overflow() > 0,
        "flooding a never-draining UART must trip the loud overflow counter"
    );
}
