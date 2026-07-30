//! The pty host-serial endpoint, against a real peer that opens the device path.
//!
//! These are the cases that decide whether unmodified serial software works
//! unmodified, and every one of them is a way the endpoint can look like a
//! broken simulator instead of a broken transport:
//!
//! 1. **Binary transparency.** A pty comes up in cooked mode, which echoes the
//!    host's bytes back at it and rewrites `0x0A`/`0x0D`.
//!    `cooked_discipline_corrupts_the_same_round_trip` proves the premise by
//!    deliberately putting the discipline BACK into cooked mode and watching the
//!    same round trip corrupt, so the passing raw-mode assertions cannot be an
//!    accident.
//! 2. **A peer that attaches late**, after the firmware has already spoken.
//! 3. **A peer that disconnects mid-run**, then reattaches.
//! 4. **A single host write larger than any UART RX fifo**, the defect class
//!    that already bit us at the `Mcu::uart_write` layer.
//!
//! The "peer" here is the test itself opening the printed device path, which is
//! exactly what pyserial does. The real external-client proof (a Python
//! pyserial script against an emulated ATmega328P) lives in the engine's
//! host_serial_cosim test and in docs/cosim/MCU.md.

#![cfg(unix)]

use hauksbee_mcu::hostserial::{HostSerial, HostSerialTransport, PeerEvent};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

/// Open the endpoint's device path the way a host tool does. Non-blocking, so a
/// test that mis-predicts how many bytes are readable fails on its deadline
/// instead of hanging the suite forever inside `read`.
fn attach(ep: &HostSerial) -> std::fs::File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(ep.endpoint())
        .unwrap_or_else(|e| panic!("open {} as a host tool would: {e}", ep.endpoint()));
    unsafe {
        let flags = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(f.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    f
}

/// Pump the endpoint until it reports the wanted event or the budget runs out.
fn wait_for_event(ep: &mut HostSerial, want: PeerEvent) -> bool {
    for _ in 0..400 {
        let _ = ep.read_from_peer();
        if ep.poll_peer().contains(&want) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// Read from the endpoint until `n` bytes have arrived or the budget runs out.
fn drain_from_peer(ep: &mut HostSerial, n: usize) -> Vec<u8> {
    let mut got = Vec::new();
    for _ in 0..2000 {
        got.extend(ep.read_from_peer());
        if got.len() >= n {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    got
}

/// Read from the peer side until `n` bytes have arrived or the budget runs out.
fn drain_at_peer(peer: &mut std::fs::File, n: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while got.len() < n && std::time::Instant::now() < deadline {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => got.extend_from_slice(&buf[..k]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1))
            }
            Err(_) => break,
        }
    }
    got
}

/// Push every byte from the peer side, waiting out the pty's finite kernel
/// buffer. A host tool that writes more than the buffer holds has to wait for
/// the endpoint to drain, exactly as a real 115200-baud port would make it wait.
fn write_all_from_peer(peer: &mut std::fs::File, bytes: &[u8]) {
    let mut sent = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while sent < bytes.len() {
        assert!(
            std::time::Instant::now() < deadline,
            "the endpoint never drained the host write (stuck at {sent} of {})",
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
    peer.flush().ok();
}

/// Put a tty fd back into COOKED mode: echo on, CR/LF rewriting on. This is the
/// state a pty is created in, and the state the endpoint has to defeat.
fn make_cooked(fd: std::os::unix::io::RawFd) {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(fd, &mut t), 0, "tcgetattr");
        t.c_lflag |= libc::ECHO | libc::ICANON | libc::ISIG;
        t.c_oflag |= libc::OPOST | libc::ONLCR;
        t.c_iflag |= libc::ICRNL;
        assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &t), 0, "tcsetattr");
    }
}

/// A byte string that a cooked pty cannot carry: an embedded NUL, a bare LF, a
/// bare CR, and a high byte. Exactly the shape of a binary framed protocol.
const BINARY: &[u8] = &[0x05, 0x00, 0x0A, 0x0D, 0xFF, 0x7F, 0x00, 0x0A, 0x42];

/// Both directions carry arbitrary bytes intact, NUL and 0x0A included.
#[test]
fn binary_bytes_survive_both_directions() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let mut peer = attach(&ep);
    assert!(
        wait_for_event(&mut ep, PeerEvent::Attached),
        "opening the device path must be reported as an attach"
    );

    // Host -> firmware.
    write_all_from_peer(&mut peer, BINARY);
    let inbound = drain_from_peer(&mut ep, BINARY.len());
    assert_eq!(
        inbound, BINARY,
        "host bytes must reach the firmware side unmodified"
    );

    // Firmware -> host.
    ep.write_to_peer(BINARY);
    let outbound = drain_at_peer(&mut peer, BINARY.len());
    assert_eq!(
        outbound, BINARY,
        "firmware bytes must reach the host tool unmodified"
    );
}

/// Premise proof for the raw-mode pass: with the discipline forced BACK to
/// cooked, the very same round trip corrupts in two independent ways. If this
/// test ever fails, the transparency assertions above have stopped proving
/// anything and the raw pass has become untested decoration.
#[test]
fn cooked_discipline_corrupts_the_same_round_trip() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let mut peer = attach(&ep);
    assert!(wait_for_event(&mut ep, PeerEvent::Attached));
    // Undo the endpoint's raw pass from the peer side, which is also the state a
    // naive implementation leaves, and the state `cat > /dev/ttysNNN` runs in.
    make_cooked(peer.as_raw_fd());

    // Corruption 1 (host -> firmware): ONLCR rewrites the host's 0x0A on its way
    // out of the tty, so the firmware would receive a byte nobody sent.
    write_all_from_peer(&mut peer, &[0x41, 0x0A, 0x42]);
    let inbound = drain_from_peer(&mut ep, 4);
    assert_eq!(
        inbound,
        vec![0x41, 0x0D, 0x0A, 0x42],
        "a cooked pty rewrites 0x0A as 0x0D 0x0A; that is the corruption raw mode \
         exists to prevent"
    );

    // Corruption 2 (firmware -> host): ECHO bounces the firmware's own output
    // back at the endpoint, which would then inject it into the MCU's UART RX as
    // if the host had sent it.
    ep.write_to_peer(&[0x5A]);
    let echoed = drain_from_peer(&mut ep, 1);
    assert_eq!(
        echoed,
        vec![0x5A],
        "a cooked pty echoes firmware output straight back into the RX path"
    );
}

/// Firmware output produced before anyone attached is held and delivered on
/// attach, and the attach itself is reported. A user reads the printed device
/// path, THEN starts their tool, so this is the normal case, not the edge one.
#[test]
fn late_peer_gets_the_output_it_missed() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    assert!(!ep.peer_attached(), "a fresh endpoint has no peer");
    assert!(
        ep.poll_peer().is_empty(),
        "and reports no event until one arrives"
    );
    ep.write_to_peer(b"BOOT v0.1.0\n");
    assert_eq!(
        ep.stats().to_peer,
        0,
        "nothing can have been delivered with no peer attached"
    );

    let mut peer = attach(&ep);
    assert!(wait_for_event(&mut ep, PeerEvent::Attached));
    let got = drain_at_peer(&mut peer, 12);
    assert_eq!(
        got, b"BOOT v0.1.0\n",
        "the banner emitted before the tool attached must still arrive"
    );
    assert_eq!(ep.stats().attach_count, 1);
}

/// A peer that disconnects mid-run is reported, the endpoint keeps working, and
/// a second tool can attach to the same device path afterwards. This is the
/// difference between "my tool crashed" and "the simulator died with it".
#[test]
fn peer_disconnect_is_reported_and_reattach_works() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let peer = attach(&ep);
    assert!(wait_for_event(&mut ep, PeerEvent::Attached));
    drop(peer);
    assert!(
        wait_for_event(&mut ep, PeerEvent::Detached),
        "a peer that goes away must be reported, not silently assumed present"
    );
    assert!(!ep.peer_attached());

    // Output while detached is held, not an error, and the run continues.
    ep.write_to_peer(b"still running\n");

    let mut second = attach(&ep);
    assert!(
        wait_for_event(&mut ep, PeerEvent::Attached),
        "a second tool must be able to attach to the same device path"
    );
    let got = drain_at_peer(&mut second, 14);
    assert_eq!(got, b"still running\n");
    assert_eq!(ep.stats().attach_count, 2, "both attaches are counted");
}

/// One host write far larger than any UART RX fifo (and larger than a single
/// endpoint read) must arrive complete and in order. This is the defect class
/// that already cost us a wedged host protocol at the `uart_write` layer, so the
/// transport in front of it is held to the same standard. 4096 distinct-ish
/// bytes make truncation AND reordering visible.
#[test]
fn one_huge_host_write_arrives_complete_and_in_order() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let mut peer = attach(&ep);
    assert!(wait_for_event(&mut ep, PeerEvent::Attached));

    let record: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    // A pty's kernel buffer is finite, so the writer has to keep pushing while
    // the reader drains: that interleaving is precisely the real case.
    let want = record.clone();
    let writer = std::thread::spawn(move || {
        write_all_from_peer(&mut peer, &want);
        peer
    });
    let got = drain_from_peer(&mut ep, record.len());
    let _peer = writer.join().expect("writer thread");
    assert_eq!(
        got.len(),
        record.len(),
        "every byte of a {}-byte host record must arrive (got {})",
        record.len(),
        got.len()
    );
    assert_eq!(got, record, "and in the order the host sent them");
}

/// Dropping the endpoint hangs up any attached peer, so a host tool learns the
/// session ended instead of hanging on a device that will never answer. There is
/// no child process to reap here (nothing is spawned), only the master fd.
#[test]
fn dropping_the_endpoint_hangs_up_the_peer() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let mut peer = attach(&ep);
    assert!(wait_for_event(&mut ep, PeerEvent::Attached));
    drop(ep);

    let mut buf = [0u8; 8];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the peer never saw the hangup"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("unexpected peer error: {e}"),
        }
    }
}

/// A host tool whose FIRST act after opening the port is to write a command
/// byte: that byte must survive the endpoint's attach handling.
///
/// It did not. The attach is noticed slightly after the peer's `open`, and the
/// re-raw pass that runs then used to go through the pty MASTER fd, which on
/// Darwin discards whatever the peer has already written. A pyserial script's
/// opening command vanished and the firmware looked wedged. `poll_peer` is
/// called here WITHOUT reading first, so the byte is genuinely still pending
/// when the attach pass runs; that ordering is what makes this test able to fail.
#[test]
fn a_byte_written_right_after_open_survives_the_attach_pass() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let mut peer = attach(&ep);
    write_all_from_peer(&mut peer, &[0x05]);
    std::thread::sleep(std::time::Duration::from_millis(20));

    let mut saw_attach = false;
    for _ in 0..400 {
        if ep.poll_peer().contains(&PeerEvent::Attached) {
            saw_attach = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(saw_attach, "the open must be reported");
    let got = drain_from_peer(&mut ep, 1);
    assert_eq!(
        got,
        vec![0x05],
        "the command byte the tool sent before the attach was noticed must not be \
         discarded by the endpoint's own line-discipline pass"
    );
}

/// The same guarantee across a reconnect: a second tool that opens and writes
/// immediately is served, not silently swallowed.
#[test]
fn a_reattaching_tool_is_served_immediately() {
    let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
    let first = attach(&ep);
    assert!(wait_for_event(&mut ep, PeerEvent::Attached));
    drop(first);
    assert!(wait_for_event(&mut ep, PeerEvent::Detached));

    let mut second = attach(&ep);
    write_all_from_peer(&mut second, b"B");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut saw_attach = false;
    for _ in 0..400 {
        if ep.poll_peer().contains(&PeerEvent::Attached) {
            saw_attach = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(saw_attach, "the reattach must be reported");
    assert_eq!(drain_from_peer(&mut ep, 1), b"B");
}
