//! The host-facing serial endpoint: how software on the developer's own machine
//! talks to an emulated MCU's UART as if a USB serial cable were plugged in.
//!
//! The engine already exchanges UART bytes with the firmware
//! ([`crate::Mcu::uart_write`] in, [`crate::Mcu::on_uart`] out). What was missing
//! was a place for *ordinary* software to attach: a pyserial script, a vendor
//! configuration tool, `minicom`, the loader half of a bootloader protocol. This
//! module is that endpoint, and it is deliberately transport-level only: it moves
//! bytes and reports whether a peer is on the other end. Nothing here knows about
//! co-sim scheduling, so the CLI session loop, the websocket server and a future
//! GUI can all drive the same object.
//!
//! # Why a pty is the default
//!
//! A pseudo-terminal has a device path (`/dev/ttys006`), so a tool that already
//! knows how to open a serial port opens it with no changes at all:
//! `serial.Serial("/dev/ttys006")`, `minicom -D /dev/ttys006`. A TCP socket is
//! easier to implement and works on Windows, but it makes the *user* adapt: their
//! pyserial script becomes a socket script, and a closed-source vendor tool
//! cannot be adapted at all. The whole point of this feature is that unmodified
//! software works unmodified, so [`HostSerialTransport::Pty`] is the default and
//! [`HostSerialTransport::Tcp`] is the explicit opt-in (and the only option on
//! Windows, where there is no pty).
//!
//! # Peer detection, and the trick that makes it work
//!
//! A user who cannot tell whether their tool is attached assumes the simulator is
//! broken, so attach/detach has to be *observable*, not inferred from traffic.
//! That is harder than it looks on a pty. Measured behaviour of a nonblocking
//! master fd on macOS (Darwin 25), and the same shape on Linux:
//!
//! | state                              | `poll` revents | `read` |
//! |------------------------------------|----------------|--------|
//! | slave never opened                 | `0`            | `EAGAIN` |
//! | slave open, no data                | `0`            | `EAGAIN` |
//! | slave was opened and then closed   | `POLLHUP`      | `0` (Linux: `EIO`) |
//! | slave reopened after that          | `0`            | `EAGAIN` |
//!
//! So "hung up" is a reliable *no peer* signal, but a freshly created pty looks
//! identical to an attached-and-silent one. [`HostSerial::open`] therefore opens
//! the slave once itself and immediately closes it: that arms `POLLHUP`, which
//! then clears the moment a real peer opens the device. `POLLHUP` clear means a
//! peer is attached, and an observed transition either way is reportable. A
//! close+reopen completed between probes is necessarily one continuous
//! observation; callers must give both observations identical timing semantics.
//!
//! Line discipline is the other half. A pty comes up in *cooked* mode: `ECHO`
//! bounces every host byte back into the endpoint's own read path, `ONLCR`
//! rewrites a firmware's `0x0A` as `0x0D 0x0A`, `ICRNL` rewrites the host's
//! `0x0D` as `0x0A`, and `ISIG`/`ICANON` swallow control bytes outright. Binary
//! framing does not survive that, so the endpoint forces raw mode on every
//! *observed* attach as a convenience, through a slave fd rather than the master
//! (a `tcsetattr` on the master discards bytes already written). A close+reopen
//! completed between probes is not observable, so every host process must
//! configure its own serial fd before writing, as ordinary serial clients do.
//! See `raw_via_slave` for the failure that taught us.
//!
//! # Buffering, and what is honestly lost
//!
//! Real hardware transmits into the void when nothing is listening. That is
//! technically the most faithful behaviour and a terrible experience: a user who
//! starts the sim, reads the printed device path, and then attaches has already
//! missed the firmware's boot banner. So output produced while no peer is
//! attached is held in a bounded backlog ([`BACKLOG_CAP`]) and flushed on attach.
//! Past the cap the NEWEST bytes are dropped, keeping the earliest output (the
//! banner a late-attaching user came for) rather than a mid-stream window, and
//! every dropped byte is counted in [`HostSerialStats::dropped_to_peer`] so the
//! loss is reported, never silent.
//!
//! Nothing here spawns a process, so there is no child to reap (see
//! [`crate::children`] for the emulator-process rules); teardown is closing the
//! master fd, which hangs up any attached peer.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// How a host tool reaches the emulated UART.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSerialTransport {
    /// A pseudo-terminal with a real device path, so unmodified serial software
    /// works unmodified. Unix only.
    Pty,
    /// A loopback TCP socket. Needs the user's tool to speak TCP instead of
    /// serial, so it is the fallback, not the default.
    Tcp,
}

impl HostSerialTransport {
    /// The transport name as it appears in CLI flags and printed output.
    pub fn as_str(self) -> &'static str {
        match self {
            HostSerialTransport::Pty => "pty",
            HostSerialTransport::Tcp => "tcp",
        }
    }
}

/// A change in whether a host tool is attached to the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerEvent {
    /// A host tool opened the endpoint.
    Attached,
    /// The attached host tool closed it (or died).
    Detached,
}

/// Bytes of firmware output the endpoint holds while no peer is attached, or
/// while an attached peer is not reading. 64 KiB is the pty's own kernel buffer
/// size on Darwin, so this doubles the practical headroom without letting a
/// forgotten session grow without bound.
pub const BACKLOG_CAP: usize = 64 * 1024;

/// Ceiling on bytes drained from the peer in one [`HostSerial::read_from_peer`]
/// call. A peer that pipes a megabyte in one `write` must not stall the co-sim
/// loop for the whole transfer; the remainder is read on the next frames, which
/// is also what the emulated UART's baud rate would force anyway.
/// How many drain-then-poll rounds the liveness probe tolerates before
/// concluding that a peer which keeps the queue full is simply attached.
const PEER_PROBE_DRAIN_ROUNDS: usize = 16;

const READ_BUDGET: usize = 64 * 1024;

/// Byte counters for a session, for the "was my tool actually talking to it"
/// summary line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostSerialStats {
    /// Bytes read from the host tool into the endpoint's owned inbound queue.
    /// The session layer separately counts bytes actually injected into the
    /// firmware, because a simulation budget may end while bytes are queued.
    pub to_mcu: u64,
    /// Bytes actually handed to the host tool.
    pub to_peer: u64,
    /// Firmware output bytes dropped because the backlog was full or the
    /// session ended before a peer drained it. Non-zero means the host tool
    /// did not see everything the firmware sent.
    pub dropped_to_peer: u64,
    /// How many times a peer attached over the session (0 = the user never
    /// connected anything).
    pub attach_count: u64,
}

enum Inner {
    #[cfg(unix)]
    Pty {
        /// The pty master fd, owned: closed in `Drop`.
        master: libc::c_int,
        /// The slave device path, kept for the re-raw pass on each attach.
        slave: std::ffi::CString,
    },
    Tcp {
        listener: TcpListener,
        peer: Option<TcpStream>,
    },
}

/// A host-facing serial endpoint bridging a host tool and an emulated UART.
///
/// The caller owns the pumping: once per co-sim frame, call [`Self::poll_peer`]
/// (report the events), [`Self::read_from_peer`] (inject the bytes with
/// `Mcu::uart_write` or the scheduler's `serial`), and [`Self::write_to_peer`]
/// with whatever the firmware emitted. Every call is nonblocking, so the co-sim
/// keeps running whether or not anyone is attached.
pub struct HostSerial {
    inner: Inner,
    transport: HostSerialTransport,
    endpoint: String,
    attached: bool,
    /// Transitions observed but not yet reported by `poll_peer`.
    events: Vec<PeerEvent>,
    /// Host bytes drained before observing a hangup. PTYs may discard their
    /// unread tail once the last slave closes, so polling must preserve it
    /// before updating attach state.
    inbound: VecDeque<u8>,
    backlog: VecDeque<u8>,
    stats: HostSerialStats,
}

impl HostSerial {
    /// Create an endpoint and start listening. The returned object is already
    /// live: a host tool can attach before the first [`Self::poll_peer`].
    pub fn open(transport: HostSerialTransport) -> Result<Self> {
        match transport {
            HostSerialTransport::Pty => Self::open_pty(),
            HostSerialTransport::Tcp => Self::open_tcp(),
        }
    }

    #[cfg(unix)]
    fn open_pty() -> Result<Self> {
        // SAFETY: plain libc pty setup; every fd is checked before use and the
        // master is owned by the returned struct (closed in Drop).
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if master < 0 {
                bail!(
                    "cannot allocate a pseudo-terminal for the host serial endpoint: {}. \
                     Use --serial-transport tcp instead.",
                    std::io::Error::last_os_error()
                );
            }
            let guard = FdGuard(master);
            if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
                bail!(
                    "cannot unlock the pseudo-terminal: {}",
                    std::io::Error::last_os_error()
                );
            }
            // `ptsname` writes into one static, per-process buffer: a second
            // thread's call overwrites it in place, so two endpoints opened
            // concurrently can swap slave paths and each attach to the
            // other's pty. `ptsname_r` is not portably available; a lock held
            // across the call AND the copy out of the buffer is the portable
            // fix.
            static PTSNAME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let slave = {
                let _guard = PTSNAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let name_ptr = libc::ptsname(master);
                if name_ptr.is_null() {
                    bail!(
                        "cannot read the pseudo-terminal's device path: {}",
                        std::io::Error::last_os_error()
                    );
                }
                std::ffi::CStr::from_ptr(name_ptr).to_owned()
            };
            let endpoint = slave.to_string_lossy().into_owned();

            let flags = libc::fcntl(master, libc::F_GETFL);
            if flags < 0 || libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                bail!(
                    "cannot set the pseudo-terminal master non-blocking: {}",
                    std::io::Error::last_os_error()
                );
            }

            // No pty fd may leak into a spawned child: an inherited slave fd
            // keeps the slave side open after the real peer closes, which
            // suppresses the hangup report and makes the liveness probe count
            // a peer that is gone. FD_CLOEXEC here and O_CLOEXEC on the
            // transient slave opens (the arming probe below and
            // `raw_via_slave`) close that inheritance path atomically.
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);

            // Arm the hung-up state so "no peer" is distinguishable from
            // "attached and silent" (see the module doc's table), and take the
            // opportunity to raw the discipline for the first peer.
            // Arm the hung-up state (see the module doc's table) by opening the
            // slave and closing it again. Nothing may `tcsetattr` the MASTER
            // afterwards: on Darwin that both clears the armed hangup and
            // discards pending input, which is why raw mode is applied per
            // attach through a slave fd in `refresh` instead of once here.
            let probe = libc::open(
                slave.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            );
            if probe >= 0 {
                libc::close(probe);
            }

            std::mem::forget(guard);
            Ok(Self {
                inner: Inner::Pty { master, slave },
                transport: HostSerialTransport::Pty,
                endpoint,
                attached: false,
                events: Vec::new(),
                inbound: VecDeque::new(),
                backlog: VecDeque::new(),
                stats: HostSerialStats::default(),
            })
        }
    }

    #[cfg(not(unix))]
    fn open_pty() -> Result<Self> {
        bail!(
            "there is no pseudo-terminal on this platform, so the host serial endpoint \
             needs --serial-transport tcp"
        )
    }

    fn open_tcp() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("binding a loopback TCP port for the host serial endpoint")?;
        listener
            .set_nonblocking(true)
            .context("setting the host serial listener non-blocking")?;
        let endpoint = listener
            .local_addr()
            .context("reading the host serial listener address")?
            .to_string();
        Ok(Self {
            inner: Inner::Tcp {
                listener,
                peer: None,
            },
            transport: HostSerialTransport::Tcp,
            endpoint,
            attached: false,
            events: Vec::new(),
            inbound: VecDeque::new(),
            backlog: VecDeque::new(),
            stats: HostSerialStats::default(),
        })
    }

    /// Which transport this endpoint uses.
    pub fn transport(&self) -> HostSerialTransport {
        self.transport
    }

    /// The device path (pty) or `host:port` (tcp) a host tool opens. This is the
    /// string the CLI prints for the user to paste into another terminal.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Whether a host tool is attached right now.
    pub fn peer_attached(&self) -> bool {
        self.attached
    }

    /// Session byte counters.
    pub fn stats(&self) -> HostSerialStats {
        self.stats
    }

    /// Update the attach state and return every transition since the last call.
    ///
    /// Call this once per co-sim frame. This is a point-in-time liveness probe:
    /// a close+reopen that completes between calls can remain one continuous
    /// observed attachment. Buffered bytes still flow, but callers must not
    /// assign different timing semantics to an observed versus hidden handoff.
    pub fn poll_peer(&mut self) -> Vec<PeerEvent> {
        if self.attached {
            // Drain before asking whether the last slave closed. This retains
            // bytes already available while the peer is open; an external
            // process that writes and closes before the next drain must use a
            // protocol acknowledgement or keep the tty open until consumed.
            self.buffer_peer_input();
            self.refresh();
        } else {
            // A new peer must be detected and switched to raw mode before its
            // first bytes are read, or the line discipline can rewrite them.
            self.refresh();
            self.buffer_peer_input();
        }
        std::mem::take(&mut self.events)
    }

    /// Bytes the host tool sent, to be injected into the firmware's UART RX.
    ///
    /// Returns everything readable up to `READ_BUDGET`, in order. A single host
    /// `write` far larger than the emulated UART's RX fifo is fine here and must
    /// stay fine downstream: the fifo-truncation defect class is the reason
    /// `Mcu::uart_write` queues and meters rather than raising bytes at one
    /// instant.
    /// The pty side attempts a final read while the peer is still observed.
    /// A host that writes and immediately closes without waiting for a protocol
    /// response or transport drain is not a checked delivery: Darwin may discard
    /// bytes that the master did not consume before the last slave closed.
    pub fn read_from_peer(&mut self) -> Vec<u8> {
        self.buffer_peer_input();
        let take = self.inbound.len().min(READ_BUDGET);
        self.inbound.drain(..take).collect()
    }

    /// Host bytes already owned by the endpoint but not yet taken by the
    /// session for simulated-time scheduling.
    pub fn pending_from_peer(&self) -> usize {
        self.inbound.len()
    }

    /// Drain the transport into an owned queue before any attach-state probe
    /// can turn a PTY's final readable byte into EOF.
    fn buffer_peer_input(&mut self) {
        let mut out = Vec::new();
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Pty { master, .. } => {
                let mut buf = [0u8; 4096];
                loop {
                    // SAFETY: read into a stack buffer we own, length bounded.
                    let n = unsafe {
                        libc::read(
                            *master,
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len().min(READ_BUDGET - out.len()),
                        )
                    };
                    if n > 0 {
                        out.extend_from_slice(&buf[..n as usize]);
                        if out.len() >= READ_BUDGET {
                            break;
                        }
                        continue;
                    }
                    if n == 0 {
                        // Darwin's hangup report. Linux uses EIO for the same
                        // state; both mean the peer closed the device.
                        self.mark_detached();
                        break;
                    }
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(libc::EINTR) => continue,
                        Some(libc::EAGAIN) => break,
                        Some(libc::EIO) => {
                            self.mark_detached();
                            break;
                        }
                        _ => break,
                    }
                }
            }
            Inner::Tcp { peer, .. } => {
                let mut drop_peer = false;
                if let Some(stream) = peer {
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf[..4096.min(READ_BUDGET - out.len())]) {
                            Ok(0) => {
                                drop_peer = true;
                                break;
                            }
                            Ok(n) => {
                                out.extend_from_slice(&buf[..n]);
                                if out.len() >= READ_BUDGET {
                                    break;
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                drop_peer = true;
                                break;
                            }
                        }
                    }
                }
                if drop_peer {
                    *peer = None;
                    self.mark_detached();
                }
            }
        }
        self.stats.to_mcu += out.len() as u64;
        self.inbound.extend(out);
    }

    /// Queue firmware output for the host tool, flushing as much as the peer
    /// will take. Bytes produced while no peer is attached are held (see the
    /// module doc) rather than thrown away.
    pub fn write_to_peer(&mut self, bytes: &[u8]) {
        self.backlog.extend(bytes);
        if self.backlog.len() > BACKLOG_CAP {
            // Drop the NEWEST: a late-attaching user came for the boot banner,
            // and truncating the tail is at least a prefix of the real stream
            // rather than a window with a hole in the middle. Counted loudly.
            let excess = self.backlog.len() - BACKLOG_CAP;
            self.backlog.truncate(BACKLOG_CAP);
            self.stats.dropped_to_peer += excess as u64;
        }
        self.flush();
    }

    /// Push as much of the backlog as the peer will accept. Called by
    /// [`Self::write_to_peer`] and on attach; safe to call at any time.
    pub fn flush(&mut self) {
        self.flush_with_after_probe(|| {});
    }

    fn flush_with_after_probe<F>(&mut self, after_probe: F)
    where
        F: FnOnce(),
    {
        if !self.attached || self.backlog.is_empty() {
            return;
        }

        #[cfg(unix)]
        if matches!(self.inner, Inner::Pty { .. }) {
            // The probe drains inbound itself, and the PTY stays free of
            // internal slave fds from here through every master write below:
            // a successful write is evidence of a real peer, never a
            // pseudo-peer owned by this endpoint.
            if !self.pty_externally_attached() {
                self.mark_detached();
                return;
            }
        }

        after_probe();
        loop {
            self.backlog.make_contiguous();
            let (front, _) = self.backlog.as_slices();
            if front.is_empty() {
                break;
            }
            let written = match &mut self.inner {
                #[cfg(unix)]
                Inner::Pty { master, .. } => {
                    // SAFETY: writing a slice we own, length from the slice.
                    let n = unsafe {
                        libc::write(*master, front.as_ptr() as *const libc::c_void, front.len())
                    };
                    if n > 0 {
                        n as usize
                    } else {
                        if n == 0 {
                            break;
                        }
                        let err = std::io::Error::last_os_error();
                        match err.raw_os_error() {
                            Some(libc::EINTR) => continue,
                            Some(libc::EAGAIN) => break,
                            Some(libc::EIO) => {
                                self.mark_detached();
                                break;
                            }
                            _ => break,
                        }
                    }
                }
                Inner::Tcp { peer, .. } => {
                    let mut drop_peer = false;
                    let mut written = 0usize;
                    if let Some(stream) = peer {
                        match stream.write(front) {
                            Ok(0) => drop_peer = true,
                            Ok(n) => written = n,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => drop_peer = true,
                        }
                    }
                    if drop_peer {
                        *peer = None;
                        self.mark_detached();
                        break;
                    }
                    written
                }
            };
            // A slave can close between the liveness probe above and this
            // write. Linux may still accept the bytes while propagating the
            // hangup, so a positive write alone is not evidence that an
            // external peer owned them. Re-probe (draining first, so queued
            // input cannot mask the hangup) before crediting output;
            // retaining the backlog is the conservative outcome.
            #[cfg(unix)]
            if written > 0
                && matches!(self.inner, Inner::Pty { .. })
                && !self.pty_externally_attached()
            {
                self.mark_detached();
                break;
            }
            if written == 0 {
                break;
            }
            self.backlog.drain(..written);
            self.stats.to_peer += written as u64;
            if self.backlog.is_empty() {
                break;
            }
        }
    }

    /// How many bytes are still waiting for the peer.
    pub fn pending_to_peer(&self) -> usize {
        self.backlog.len()
    }

    /// Account for terminal firmware output that no peer drained. Session
    /// teardown is the only valid caller: while the endpoint remains live the
    /// backlog must stay available for a later peer instead of being discarded.
    pub fn discard_terminal_output(&mut self) -> usize {
        let pending = self.backlog.len();
        self.backlog.clear();
        self.stats.dropped_to_peer += pending as u64;
        pending
    }

    /// A paste-ready hint for attaching a tool, tailored to the transport.
    pub fn attach_hint(&self) -> Vec<String> {
        match self.transport {
            HostSerialTransport::Pty => vec![
                format!("python3 -c \"import serial; s=serial.Serial('{ep}', 115200, timeout=1); s.write(b'\\x05'); print(s.read(16))\"", ep = self.endpoint),
                format!("minicom -D {ep}", ep = self.endpoint),
                format!("screen {ep} 115200", ep = self.endpoint),
            ],
            HostSerialTransport::Tcp => {
                let (host, port) = self
                    .endpoint
                    .rsplit_once(':')
                    .unwrap_or(("127.0.0.1", &self.endpoint));
                vec![
                    format!("nc {host} {port}"),
                    format!(
                        "python3 -c \"import socket; s=socket.create_connection(('{host}',{port})); s.sendall(b'\\x05'); print(s.recv(16))\""
                    ),
                ]
            }
        }
    }

    /// Whether an external peer holds the pty slave open. `POLLHUP` is the
    /// no-peer state (see the module doc: `open` arms it deliberately so a
    /// never-attached pty is not mistaken for an attached silent one), but
    /// undrained input MASKS it: Darwin reports only `POLLIN` while bytes a
    /// peer wrote before closing sit unread, so a bare poll would count a
    /// dead peer as live and let firmware output earn delivery credit nobody
    /// can read. Drain, then poll, and repeat while the drain finds bytes: a
    /// peer that keeps producing new input is attached by definition.
    #[cfg(unix)]
    fn pty_externally_attached(&mut self) -> bool {
        for _ in 0..PEER_PROBE_DRAIN_ROUNDS {
            self.buffer_peer_input();
            let master = match &self.inner {
                Inner::Pty { master, .. } => *master,
                Inner::Tcp { .. } => unreachable!("pty probe on a tcp endpoint"),
            };
            let mut p = libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: single valid pollfd, zero timeout, no allocation.
            unsafe {
                libc::poll(&mut p, 1, 0);
            }
            if (p.revents & (libc::POLLHUP | libc::POLLNVAL)) != 0 {
                return false;
            }
            if (p.revents & libc::POLLIN) == 0 {
                return true;
            }
        }
        // POLLIN persisted through every drain round: something is still
        // writing, and a writer is an attached peer.
        true
    }

    /// Transport-specific attach-state update, queueing any transition.
    fn refresh(&mut self) {
        #[cfg(unix)]
        if matches!(self.inner, Inner::Pty { .. }) {
            let now = self.pty_externally_attached();
            self.apply_attach_state(now);
            return;
        }
        let now = match &mut self.inner {
            #[cfg(unix)]
            Inner::Pty { .. } => unreachable!("handled above"),
            Inner::Tcp { listener, peer } => {
                if peer.is_none() {
                    if let Ok((stream, _)) = listener.accept() {
                        let _ = stream.set_nonblocking(true);
                        let _ = stream.set_nodelay(true);
                        *peer = Some(stream);
                    }
                }
                peer.is_some()
            }
        };
        self.apply_attach_state(now);
    }

    /// Queue the attach or detach transition `now` implies, applying raw mode
    /// and flushing the backlog on a fresh attach.
    fn apply_attach_state(&mut self, now: bool) {
        if now && !self.attached {
            self.attached = true;
            self.stats.attach_count += 1;
            self.events.push(PeerEvent::Attached);
            // A fresh slave open resets the pty discipline to cooked, so raw mode
            // has to be re-applied per attach, not just at startup, and it has to
            // go through a slave fd (see `raw_via_slave`: the master route eats
            // bytes the peer has already sent).
            #[cfg(unix)]
            if let Inner::Pty { slave, .. } = &self.inner {
                raw_via_slave(slave);
            }
            self.flush();
        } else if !now && self.attached {
            self.mark_detached();
        }
    }

    /// Record a detach discovered anywhere (poll, read, or write).
    fn mark_detached(&mut self) {
        if self.attached {
            self.attached = false;
            self.events.push(PeerEvent::Detached);
        }
    }
}

impl Drop for HostSerial {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Inner::Pty { master, .. } = &self.inner {
            // SAFETY: the fd is owned by this struct and not used again.
            unsafe {
                libc::close(*master);
            }
        }
    }
}

/// Closes an fd unless the caller forgets it: keeps the half-built pty from
/// leaking when a later setup step fails.
#[cfg(unix)]
struct FdGuard(libc::c_int);

#[cfg(unix)]
impl Drop for FdGuard {
    fn drop(&mut self) {
        // SAFETY: the guard owns the fd until it is forgotten.
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Force raw line discipline on a tty fd: no echo, no CR/LF rewriting, no signal
/// or canonical-mode interpretation. Without this a `0x0A` from the firmware
/// reaches the host as `0x0D 0x0A`, a `0x0D` from the host reaches the firmware
/// as `0x0A`, and every host byte is echoed straight back into the RX path,
/// which corrupts any binary protocol. Best effort: a non-tty fd simply has no
/// discipline to set.
#[cfg(unix)]
fn set_raw(fd: libc::c_int) {
    // SAFETY: termios is a plain POD struct; the fd is checked by the kernel.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return;
        }
        libc::cfmakeraw(&mut t);
        // No modem-control or hardware-flow-control lines exist on this link, so
        // CLOCAL keeps a peer's open from blocking on carrier detect.
        t.c_cflag |= libc::CLOCAL | libc::CREAD;
        let _ = libc::tcsetattr(fd, libc::TCSANOW, &t);
    }
}

/// Raw the discipline through a SECOND slave fd, opened and closed around the
/// `tcsetattr` while the peer holds its own fd open.
///
/// The obvious implementation, `tcsetattr` on the master fd, reaches the same
/// discipline but DISCARDS whatever the peer has already written and the endpoint
/// has not yet read (measured on Darwin: a byte written by the peer before the
/// re-raw pass simply disappears). Since the re-raw happens when an attach is
/// NOTICED, which is always slightly after the peer's `open`, that races directly
/// with a script whose first act is to write a command byte: the command
/// vanishes and the firmware looks wedged. Going through a slave fd preserves the
/// pending bytes.
///
/// Closing this fd cannot hang the peer up, because the peer's own fd keeps the
/// slave count above zero. If the peer has ALREADY gone, this open/close re-arms
/// the hung-up state, which is exactly the right answer.
#[cfg(unix)]
fn raw_via_slave(slave: &std::ffi::CStr) {
    // SAFETY: opening a path we constructed from `ptsname`, closing the same fd.
    unsafe {
        let fd = libc::open(
            slave.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return;
        }
        set_raw(fd);
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backlog is bounded and the loss is COUNTED, not silent: a session
    /// whose peer never attached (or never read) must be able to say how many
    /// firmware bytes the host tool did not see.
    #[test]
    fn overflowing_backlog_counts_every_dropped_byte() {
        let mut ep = HostSerial::open(HostSerialTransport::Tcp).expect("tcp endpoint");
        assert!(!ep.peer_attached(), "nothing is attached yet");
        let slab = vec![0xA5u8; BACKLOG_CAP / 2];
        for _ in 0..6 {
            ep.write_to_peer(&slab);
        }
        let st = ep.stats();
        assert_eq!(ep.pending_to_peer(), BACKLOG_CAP, "backlog is capped");
        assert_eq!(
            st.dropped_to_peer,
            (6 * (BACKLOG_CAP / 2) - BACKLOG_CAP) as u64,
            "every byte past the cap is counted"
        );
        assert_eq!(st.to_peer, 0, "no peer took anything");
    }

    /// A TCP endpoint reports attach and detach, and holds pre-attach output
    /// until someone shows up for it.
    #[test]
    fn tcp_peer_attach_detach_and_backlog_flush() {
        let mut ep = HostSerial::open(HostSerialTransport::Tcp).expect("tcp endpoint");
        assert!(
            ep.poll_peer().is_empty(),
            "no events before anyone attaches"
        );
        ep.write_to_peer(b"boot banner\n");

        let mut peer = TcpStream::connect(ep.endpoint()).expect("connect");
        // `connect` returns before the listener has accepted, so the attach is
        // observed on one of the next polls, not necessarily the first.
        let mut saw_attach = false;
        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Attached) {
                saw_attach = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(saw_attach, "a peer that connects must be reported");

        let mut got = [0u8; 32];
        peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let n = peer.read(&mut got).expect("read banner");
        assert_eq!(
            &got[..n],
            b"boot banner\n",
            "output produced before the peer attached is delivered on attach"
        );

        peer.write_all(b"hello").expect("write");
        // The peer's bytes surface through read_from_peer, not through poll.
        let mut inbound = Vec::new();
        for _ in 0..200 {
            inbound.extend(ep.read_from_peer());
            if !inbound.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(inbound, b"hello");

        drop(peer);
        let mut saw_detach = false;
        for _ in 0..200 {
            let _ = ep.read_from_peer();
            if ep.poll_peer().contains(&PeerEvent::Detached) {
                saw_detach = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(saw_detach, "a peer that disconnects must be reported");
        assert!(!ep.peer_attached());
    }

    /// A checked command remains lossless when the peer stays open until the
    /// endpoint consumes it. Immediate write-and-close without an ACK/drain is
    /// deliberately outside the PTY contract: keeping an internal slave open
    /// to save that external bug would make firmware output falsely deliverable.
    #[cfg(unix)]
    #[test]
    fn pty_checked_command_is_consumed_before_peer_close() {
        // Hold spawns still while this test arranges and observes pty
        // hangups; see children::SPAWN_PTY_EXCLUSION.
        let _quiesce = crate::children::SPAWN_PTY_EXCLUSION
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
        let mut peer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(ep.endpoint())
            .expect("open pty peer");

        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Attached) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ep.peer_attached(), "peer attach must be observed and rawed");

        // Match the original NEP host: it re-applies its own noncanonical
        // termios options after the endpoint has observed and rawed the peer.
        unsafe {
            let mut options: libc::termios = std::mem::zeroed();
            options.c_cflag = libc::B9600 | libc::CS8 | libc::CLOCAL | libc::CREAD;
            options.c_iflag = libc::IGNPAR;
            options.c_oflag = 0;
            options.c_lflag = 0;
            assert_eq!(libc::tcflush(peer.as_raw_fd(), libc::TCIFLUSH), 0);
            assert_eq!(
                libc::tcsetattr(peer.as_raw_fd(), libc::TCSANOW, &options),
                0
            );
        }

        peer.write_all(&[0x05]).expect("write signature request");
        let mut first = Vec::new();
        for _ in 0..200 {
            first.extend(ep.read_from_peer());
            if first == [0x05] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(first, [0x05]);

        ep.write_to_peer(&[0x06, 0x00, 0x01, 0x01, b'\n']);
        let mut signature = [0u8; 5];
        peer.read_exact(&mut signature).expect("read signature");
        assert_eq!(signature, [0x06, 0x00, 0x01, 0x01, b'\n']);

        peer.write_all(&[0x07]).expect("write final command");
        let mut command = Vec::new();
        for _ in 0..200 {
            command.extend(ep.read_from_peer());
            if command == [0x07] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(command, [0x07]);
        drop(peer);

        let mut saw_detach = false;
        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Detached) {
                saw_detach = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            saw_detach,
            "the peer close must be observed within the bounded poll window"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pty_reopen_before_poll_preserves_bytes_as_one_continuous_observation() {
        // Hold spawns still while this test arranges and observes pty
        // hangups; see children::SPAWN_PTY_EXCLUSION.
        let _quiesce = crate::children::SPAWN_PTY_EXCLUSION
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use std::os::fd::AsRawFd;

        let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
        let mut first = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ep.endpoint())
            .expect("first peer");
        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Attached) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ep.peer_attached());
        first.write_all(b"a").expect("first peer byte");
        let mut first_byte = Vec::new();
        for _ in 0..200 {
            first_byte.extend(ep.read_from_peer());
            if first_byte == b"a" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(first_byte, b"a");
        drop(first);

        let mut second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ep.endpoint())
            .expect("second peer before next poll");
        // A hidden replacement cannot be rawed by a point-in-time observer.
        // Real serial clients configure their fd before writing; use binary
        // bytes here so an accidental cooked-mode test cannot false-green.
        set_raw(second.as_raw_fd());
        let replacement = [0x00, 0x0a, 0x0d, b'b'];
        second
            .write_all(&replacement)
            .expect("replacement peer bytes");

        assert!(
            ep.poll_peer().is_empty(),
            "a point-in-time PTY probe cannot invent a close+reopen history"
        );
        assert!(ep.peer_attached());
        assert_eq!(ep.read_from_peer(), replacement);
    }

    #[cfg(unix)]
    #[test]
    fn pty_output_after_peer_close_stays_backlogged_for_the_next_peer() {
        // Hold spawns still while this test arranges and observes pty
        // hangups; see children::SPAWN_PTY_EXCLUSION.
        let _quiesce = crate::children::SPAWN_PTY_EXCLUSION
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
        let first = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ep.endpoint())
            .expect("first peer");
        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Attached) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ep.peer_attached());
        drop(first);

        ep.write_to_peer(b"retained");
        assert_eq!(
            ep.pending_to_peer(),
            b"retained".len(),
            "an internal PTY fd is not a host and cannot earn delivery credit"
        );
        assert!(ep.poll_peer().contains(&PeerEvent::Detached));

        let mut second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ep.endpoint())
            .expect("replacement peer");
        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Attached) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut received = [0u8; 8];
        second.read_exact(&mut received).expect("retained output");
        assert_eq!(&received, b"retained");
        assert_eq!(ep.stats().to_peer, b"retained".len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn pty_peer_close_after_probe_cannot_deliver_output_to_the_guard() {
        // Hold spawns still while this test arranges and observes pty
        // hangups; see children::SPAWN_PTY_EXCLUSION.
        let _quiesce = crate::children::SPAWN_PTY_EXCLUSION
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut ep = HostSerial::open(HostSerialTransport::Pty).expect("pty endpoint");
        let mut peer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ep.endpoint())
            .expect("peer");
        for _ in 0..200 {
            if ep.poll_peer().contains(&PeerEvent::Attached) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ep.peer_attached());
        ep.backlog.extend(b"racy");

        ep.flush_with_after_probe(move || {
            peer.write_all(b"unchecked-final")
                .expect("external write before immediate close");
            drop(peer);
        });

        assert_eq!(ep.pending_to_peer(), 4);
        assert_eq!(ep.stats().to_peer, 0);
        assert!(!ep.peer_attached());
    }
}
