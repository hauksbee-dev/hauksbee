//! End to end: a host tool opens a device path and talks to firmware running on
//! an emulated ATmega328P bound to a real board, through `--serial-attach`'s
//! session loop.
//!
//! This is the whole feature under test at once (transport + session pump +
//! `uart_write` metering + the co-sim loop), so it covers the cases that decide
//! whether a user's own software works against the simulator:
//!
//! - a single host record far larger than simavr's 64-byte RX fifo, which is the
//!   defect class that already wedged a host protocol here;
//! - binary payloads with embedded NUL and 0x0A, which a cooked pty would rewrite;
//! - a peer that attaches AFTER the firmware has already spoken;
//! - a peer that disconnects mid-run, which must not stop the co-sim.
//!
//! The peer runs on its own thread and only ever sees the device path the session
//! printed, exactly as a pyserial script does; the co-sim runs on the main thread
//! because the AVR core is not `Send`.
//!
//! The firmware is hand-assembled inline (no AVR toolchain needed, the same
//! pattern as hauksbee-mcu's avr_uart_flow.rs): it enables UART0, transmits one
//! banner byte, then echoes every byte it receives. Every reply asserted here is
//! produced by that firmware executing on the bound board; nothing is faked.

#![cfg(all(unix, feature = "avr"))]

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::commands::hostserial::{run_session, SerialSessionConfig};
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use std::io::{Read, Write};
use std::sync::mpsc;

/// Bare Nano board: the co-sim needs a bound MCU, and the UART is intercepted
/// inside the core rather than through copper, so no serial net is required.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "BUS")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4 thru_hole circle (at 0 4) (size 1 1) (net 1 "GND"))
    (pad 27 thru_hole circle (at 0 27) (size 1 1) (net 2 "+5V"))
    (pad 21 thru_hole circle (at 0 21) (size 1 1) (net 3 "BUS"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 3 "BUS"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

/// The byte the firmware transmits once at boot. A late-attaching peer that
/// receives it proves pre-attach output was held rather than thrown away.
const BANNER: u8 = 0x42;

/// UART0 at 115200, one banner byte, then a polling echo loop. Assembly listing
/// and register addresses: hauksbee-mcu/tests/avr_uart_flow.rs.
const UART_ECHO: &[u8] = &[
    0x02, 0xE0, // ldi r16, 0x02  (U2X0)
    0x00, 0x93, 0xC0, 0x00, // sts UCSR0A, r16
    0x00, 0xE1, // ldi r16, 16   (115200 @ 16 MHz, U2X0)
    0x00, 0x93, 0xC4, 0x00, // sts UBRR0L, r16
    0x08, 0xE1, // ldi r16, 0x18 (RXEN0 | TXEN0)
    0x00, 0x93, 0xC1, 0x00, // sts UCSR0B, r16
    0x02, 0xE4, // ldi r16, 0x42 (BANNER)
    0x00, 0x93, 0xC6, 0x00, // sts UDR0, r16
    0x10, 0x91, 0xC0, 0x00, // loop: lds r17, UCSR0A
    0x17, 0xFF, // sbrs r17, 7   (RXC0)
    0xFC, 0xCF, // rjmp loop
    0x20, 0x91, 0xC6, 0x00, // lds r18, UDR0
    0x10, 0x91, 0xC0, 0x00, // wait: lds r17, UCSR0A
    0x15, 0xFF, // sbrs r17, 5   (UDRE0)
    0xFC, 0xCF, // rjmp wait
    0x20, 0x93, 0xC6, 0x00, // sts UDR0, r18
    0xF3, 0xCF, // rjmp loop
];

fn ihex_record(record_type: u8, addr: u16, data: &[u8]) -> String {
    let mut bytes = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, record_type];
    bytes.extend_from_slice(data);
    let checksum = (!bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))).wrapping_add(1);
    bytes.push(checksum);
    let hexstr: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(":{hexstr}")
}

/// Write the echo firmware as an Intel HEX file under a per-test name.
fn echo_firmware(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("hauksbee_host_serial_cosim");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(name);
    let mut text = String::new();
    for (i, chunk) in UART_ECHO.chunks(16).enumerate() {
        text.push_str(&ihex_record(0x00, (i * 16) as u16, chunk));
        text.push('\n');
    }
    text.push_str(":00000001FF\n");
    std::fs::write(&path, text).expect("write hex");
    path
}

fn engine_with_echo_firmware(name: &str) -> HauksbeeEngine {
    let fw = echo_firmware(name);
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine")
}

#[test]
fn unknown_serial_mcu_refuses_before_opening_an_endpoint() {
    let mut engine = engine_with_echo_firmware("echo_invalid_mcu.hex");
    let mut narration = Vec::new();
    let error = run_session(
        &mut engine,
        1.0,
        &SerialSessionConfig {
            mcu: Some("TYPO".to_string()),
            ..Default::default()
        },
        &mut |line| narration.push(line.to_string()),
    )
    .expect_err("an unknown UART target must fail before offering an endpoint");
    assert!(error.to_string().contains("available references: A1"));
    assert!(
        narration.is_empty(),
        "validation must happen before a device path is opened or announced: {narration:?}"
    );
}

/// Open the printed device path non-blocking, the way a host tool does.
fn open_peer(path: &str) -> std::fs::File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    unsafe {
        use std::os::unix::io::AsRawFd;
        let flags = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(f.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    f
}

/// Push every byte, waiting out the pty's finite kernel buffer.
fn write_all_to_board(peer: &mut std::fs::File, bytes: &[u8]) {
    let mut sent = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while sent < bytes.len() {
        assert!(
            std::time::Instant::now() < deadline,
            "the board never drained the host write (stuck at {sent} of {})",
            bytes.len()
        );
        match peer.write(&bytes[sent..]) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1))
            }
            Err(e) => panic!("host write failed: {e}"),
        }
    }
}

/// Read replies until `n` bytes have arrived or `budget` expires.
fn read_from_board(peer: &mut std::fs::File, n: usize, budget: std::time::Duration) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = std::time::Instant::now() + budget;
    while got.len() < n && std::time::Instant::now() < deadline {
        match peer.read(&mut buf) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Ok(k) => got.extend_from_slice(&buf[..k]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1))
            }
            Err(e) => panic!("host read failed: {e}"),
        }
    }
    got
}

/// Read until `wanted` appears or the budget expires. A reconnecting peer may
/// first receive a conservatively retained byte from the peer that just closed,
/// so a fixed-length read cannot prove that the new command was served.
fn read_through_byte(peer: &mut std::fs::File, wanted: u8, budget: std::time::Duration) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = std::time::Instant::now() + budget;
    while !got.contains(&wanted) && std::time::Instant::now() < deadline {
        match peer.read(&mut buf) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Ok(k) => got.extend_from_slice(&buf[..k]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1))
            }
            Err(e) => panic!("host read failed: {e}"),
        }
    }
    got
}

/// Pull the device path out of the session's own narration, which is the only
/// thing a host tool ever gets to see.
fn endpoint_from(lines: &mpsc::Receiver<String>) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(line) = lines.recv_timeout(std::time::Duration::from_millis(200)) {
            if let Some(rest) = line.strip_prefix("host serial: pty on ") {
                return rest.trim().to_string();
            }
        }
    }
    panic!("the session never printed a device path");
}

/// The headline case: 256 distinct bytes in ONE host write, four times simavr's
/// 64-byte RX fifo, with an embedded NUL and 0x0A. Every byte must come back
/// echoed by the firmware, in order. Truncation past byte 64 is the historical
/// defect; a cooked pty or a chunking bridge would corrupt the 0x0A and 0x0D.
#[test]
fn host_record_longer_than_the_rx_fifo_round_trips() {
    let mut engine = engine_with_echo_firmware("echo_record.hex");
    let (tx, rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let record: Vec<u8> = (0u16..256).map(|b| b as u8).collect();
    let want = record.clone();

    let peer = std::thread::spawn(move || {
        let path = endpoint_from(&rx);
        let mut peer = open_peer(&path);
        // The banner lands first; it is not part of the echo stream.
        let banner = read_from_board(&mut peer, 1, std::time::Duration::from_secs(10));
        write_all_to_board(&mut peer, &want);
        let echoed = read_from_board(&mut peer, want.len(), std::time::Duration::from_secs(20));
        result_tx
            .send((banner, echoed))
            .expect("report the host's complete read");
        // Keep the slave open until the session has flushed and accounted for
        // its final byte. Closing here used to race Linux's post-write PTY
        // liveness probe: the host had already read the byte, but the endpoint
        // conservatively retained and then counted it as dropped.
        release_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("session finalization releases the peer");
    });

    let mut say = |line: &str| {
        eprintln!("{line}");
        let _ = tx.send(line.to_string());
    };
    let cfg = SerialSessionConfig {
        // Wait for the peer so the record is sent inside the run, and give the
        // 256-byte round trip (~44 ms of emulated serial time) room.
        wait_secs: Some(20.0),
        ..Default::default()
    };
    let summary = run_session(&mut engine, 2.0, &cfg, &mut say).expect("serial session");
    let (banner, echoed) = result_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("peer completed the round trip");
    release_tx
        .send(())
        .expect("release peer after finalization");
    peer.join().expect("peer thread");

    assert_eq!(
        banner,
        vec![BANNER],
        "the firmware's boot banner must arrive"
    );
    assert_eq!(
        echoed.len(),
        record.len(),
        "every byte of a {}-byte host record must reach the firmware and come back \
         (got {}); truncation at 64 is the RX-fifo defect",
        record.len(),
        echoed.len()
    );
    assert_eq!(
        echoed, record,
        "the echoed bytes must match the host's record exactly, NUL and 0x0A included"
    );
    assert_eq!(
        summary.bytes_to_mcu, 256,
        "the session must account for every host byte it forwarded"
    );
    assert!(
        summary.rx_overflow.is_empty(),
        "no host byte may be lost on its way into the firmware: {:?}",
        summary.rx_overflow
    );
    assert_eq!(
        summary.dropped_to_peer, 0,
        "no firmware output may be dropped"
    );
    assert_eq!(summary.attach_count, 1);
}

/// A peer that shows up after the firmware has already booted still gets the
/// banner: this is the ordinary case (read the printed path, then start your
/// tool), and losing it would look like dead firmware.
#[test]
fn late_peer_still_receives_the_boot_banner() {
    let mut engine = engine_with_echo_firmware("echo_late.hex");
    let (tx, rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let peer = std::thread::spawn(move || {
        let path = endpoint_from(&rx);
        // Long enough that the firmware has booted and sent its banner into a
        // port nobody is holding.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let mut peer = open_peer(&path);
        let banner = read_from_board(&mut peer, 1, std::time::Duration::from_secs(5));
        write_all_to_board(&mut peer, b"Z");
        let echoed = read_from_board(&mut peer, 1, std::time::Duration::from_secs(5));
        result_tx
            .send((banner, echoed))
            .expect("report the late peer's reads");
        release_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("session finalization releases the late peer");
    });

    let mut say = |line: &str| {
        eprintln!("{line}");
        let _ = tx.send(line.to_string());
    };
    // No --serial-wait: the run starts immediately, which is what makes the peer
    // late.
    let summary = run_session(&mut engine, 2.0, &SerialSessionConfig::default(), &mut say)
        .expect("serial session");
    let (banner, echoed) = result_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("late peer completed its reads");
    release_tx
        .send(())
        .expect("release late peer after finalization");
    peer.join().expect("peer thread");

    assert_eq!(
        banner,
        vec![BANNER],
        "output produced before the tool attached must be held and flushed on attach"
    );
    assert_eq!(echoed, b"Z", "and the link works normally afterwards");
    assert_eq!(summary.dropped_to_peer, 0);
    assert_eq!(summary.attach_count, 1);
}

/// A peer that disconnects mid-run must not take the co-sim with it: the run
/// completes its simulated seconds, the detach is reported, and a second tool
/// can attach to the same port and be served by the same firmware.
#[test]
fn peer_disconnect_mid_run_leaves_the_cosim_running() {
    let mut engine = engine_with_echo_firmware("echo_detach.hex");
    let (tx, rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let peer = std::thread::spawn(move || {
        let path = endpoint_from(&rx);
        let mut first = open_peer(&path);
        let _banner = read_from_board(&mut first, 1, std::time::Duration::from_secs(10));
        write_all_to_board(&mut first, b"A");
        let first_echo = read_from_board(&mut first, 1, std::time::Duration::from_secs(5));
        drop(first);

        // Give the session a moment to notice the hangup, then come back.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut second = open_peer(&path);
        write_all_to_board(&mut second, b"B");
        let second_stream = read_through_byte(&mut second, b'B', std::time::Duration::from_secs(5));
        result_tx
            .send((first_echo, second_stream))
            .expect("report both peers' reads");
        // The second peer is the live peer at session teardown. Keep it open
        // until the endpoint has flushed and credited the final echo.
        release_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("session finalization releases the second peer");
    });

    let mut say = |line: &str| {
        eprintln!("{line}");
        let _ = tx.send(line.to_string());
    };
    let cfg = SerialSessionConfig {
        wait_secs: Some(20.0),
        ..Default::default()
    };
    let summary = run_session(&mut engine, 3.0, &cfg, &mut say).expect("serial session");
    let (first_echo, second_stream) = result_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("both peers completed their reads");
    release_tx
        .send(())
        .expect("release second peer after finalization");
    peer.join().expect("peer thread");

    assert_eq!(first_echo, b"A", "the first tool's byte must be echoed");
    assert!(
        second_stream == b"B" || second_stream == b"AB",
        "after a disconnect the SAME running firmware must serve the next tool; a byte written \
         during the close race may be replayed at least once, got {second_stream:?}"
    );
    assert_eq!(summary.dropped_to_peer, 0);
    assert_eq!(
        summary.attach_count, 2,
        "both attaches must be counted, so a detach is never silently ignored"
    );
    assert!(
        summary.sim_seconds >= 2.9,
        "the co-sim must complete its simulated seconds despite the disconnect, got {:.3}s",
        summary.sim_seconds
    );
}

/// Refusals a user can act on: a run that waits for a peer that never comes must
/// fail loudly rather than report a quiet, empty session.
#[test]
fn wait_for_a_peer_that_never_attaches_fails_loudly() {
    let mut engine = engine_with_echo_firmware("echo_nowait.hex");
    let cfg = SerialSessionConfig {
        wait_secs: Some(0.3),
        ..Default::default()
    };
    let mut say = |line: &str| eprintln!("{line}");
    let err = run_session(&mut engine, 1.0, &cfg, &mut say)
        .expect_err("a session nobody attached to must not report success");
    let msg = err.to_string();
    assert!(
        msg.contains("no host tool attached") && msg.contains("--serial-wait"),
        "the refusal must name the cause and the flag: {msg}"
    );
}
