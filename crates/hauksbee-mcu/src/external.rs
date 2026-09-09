//! The core every process-driven backend (Renode, Espressif QEMU) is built on.
//!
//! An external emulator is a child process reached over loopback sockets, and
//! the two backends share far more than their protocols differ: the spawn /
//! exit-detection / stderr-capture / tree-teardown of the child, the retrying
//! connect to a control port that is not open yet, the framed read loop every
//! line- or packet-oriented client needs, the free-port allocation, the UART
//! socket bridge with its lossless-or-loud accounting, and the poll-based GPIO
//! edge synthesis the [`crate::Mcu`] trait's push-style callbacks are fed from. All
//! of that lives here once; `renode` and `qemu` are the two protocol adapters.

use crate::children::{spawn_emulator, terminate_emulator, ProcessTreeGuard};
use crate::traits::{I2cEvent, McuState, PinId};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ── Child process ────────────────────────────────────────────────────────────

/// A spawned emulator child: owned as a process tree (see [`crate::children`]),
/// registered with the signal reaper, and torn down whole on drop.
pub(crate) struct EmulatorProcess {
    label: &'static str,
    child: Child,
    guard: ProcessTreeGuard,
    /// The child's stderr, when captured: a temp FILE rather than a pipe (a
    /// pipe needs a drain thread or the child blocks once the buffer fills)
    /// so a failed boot can be explained with the emulator's own words.
    stderr_log: Option<PathBuf>,
}

impl EmulatorProcess {
    /// Spawn `cmd` with stdin/stdout to null and stderr either captured to a
    /// temp file (`capture_stderr`) or discarded.
    pub(crate) fn spawn(
        label: &'static str,
        cmd: &mut Command,
        capture_stderr: bool,
    ) -> Result<Self> {
        let stderr_log = capture_stderr.then(|| {
            std::env::temp_dir().join(format!(
                "hauksbee-{}-stderr-{}-{:x}.log",
                label.to_ascii_lowercase().replace(' ', "-"),
                std::process::id(),
                Instant::now().elapsed().as_nanos() ^ (std::ptr::addr_of!(cmd) as usize as u128)
            ))
        });
        let sink = stderr_log
            .as_ref()
            .and_then(|p| std::fs::File::create(p).ok())
            .map_or_else(Stdio::null, Stdio::from);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(sink);
        let (child, guard) = spawn_emulator(cmd)
            .with_context(|| format!("spawning owned {label} from {:?}", cmd.get_program()))?;
        Ok(EmulatorProcess {
            label,
            child,
            guard,
            stderr_log,
        })
    }

    /// The child's OS process id (diagnostics and the reaping tests).
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `Some(reason)` once the child has exited, `None` while it still runs.
    /// A child that can no longer be waited on counts as gone: treating it as
    /// alive would hang the caller.
    pub(crate) fn exit_reason(&mut self) -> Option<String> {
        match self.child.try_wait() {
            // The status alone says a process died, not why. Where stderr was
            // captured, its tail is the emulator's own account and belongs in
            // the reason the caller reports.
            Ok(Some(status)) => Some(match self.stderr_output() {
                tail if tail.is_empty() => format!("exit status {status}"),
                tail => format!(
                    "exit status {status}; stderr tail: {}",
                    tail.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(" | ")
                ),
            }),
            Ok(None) => None,
            Err(e) => Some(format!("wait failed: {e}")),
        }
    }

    /// What the child wrote to stderr so far, trimmed, capped to its last
    /// 2 KiB; empty when nothing was captured.
    pub(crate) fn stderr_output(&self) -> String {
        let bytes = self
            .stderr_log
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(2048)..])
            .trim()
            .to_string()
    }

    /// Refuse an operation once the child has exited, naming the exit status
    /// and quoting its stderr. Safe to call on every later chunk: `try_wait`
    /// keeps returning the same status, so a terminal emulator failure never
    /// degrades into a stream of bare socket errors.
    pub(crate) fn ensure_running(&mut self, operation: &str) -> Result<()> {
        match self.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => {
                let stderr = self.stderr_output();
                bail!(
                    "{} exited while {operation} ({status}). {} said: {}",
                    self.label,
                    self.label,
                    nonempty_or(&stderr, "(nothing on stderr)")
                )
            }
            Err(e) => Err(e).with_context(|| {
                format!(
                    "checking whether {} is still running while {operation}",
                    self.label
                )
            }),
        }
    }
}

impl Drop for EmulatorProcess {
    fn drop(&mut self) {
        // Neither emulator has a clean SIGTERM handler worth waiting on, so
        // kill the whole tree and reap rather than leave a zombie.
        terminate_emulator(&mut self.child, &self.guard);
        if let Some(log) = &self.stderr_log {
            let _ = std::fs::remove_file(log);
        }
    }
}

/// `s` unless it is empty, else the fallback: an error that embeds captured
/// stderr must say "nothing" rather than trail off into blank space.
pub(crate) fn nonempty_or<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() {
        fallback
    } else {
        s
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

/// An explicit `VAR=<path>` override: `Some(path)` when set and present, an
/// error when set and missing (a typo must not fall through to autodetection).
pub(crate) fn env_override(var: &str) -> Result<Option<PathBuf>> {
    let Some(p) = std::env::var_os(var).map(PathBuf::from) else {
        return Ok(None);
    };
    if !p.exists() {
        bail!("{var} is set to '{}' but it does not exist", p.display());
    }
    Ok(Some(p))
}

// ── Sockets ──────────────────────────────────────────────────────────────────

/// Allocate `N` distinct free loopback TCP ports, holding every listener until
/// all the numbers are read so the OS cannot reissue one to another. The
/// emulator binds each shortly after they are released.
pub(crate) fn free_ports<const N: usize>() -> Result<[u16; N]> {
    let listeners: Vec<std::net::TcpListener> = (0..N)
        .map(|_| {
            std::net::TcpListener::bind(("127.0.0.1", 0)).context("allocating a loopback port")
        })
        .collect::<Result<_>>()?;
    let mut ports = [0u16; N];
    for (slot, l) in ports.iter_mut().zip(&listeners) {
        *slot = l.local_addr()?.port();
    }
    let mut sorted = ports;
    sorted.sort_unstable();
    anyhow::ensure!(
        sorted.windows(2).all(|w| w[0] != w[1]),
        "port allocator returned a collision"
    );
    Ok(ports)
}

/// Connect to `127.0.0.1:port`, retrying until `timeout` elapses (the emulator
/// may still be starting). `dead` is polled between attempts and short-circuits
/// the wait when the peer process has already exited: without it a child that
/// died at startup costs the whole timeout and then reports "connection
/// refused", which names the symptom and hides the cause.
pub(crate) fn connect_loopback(
    label: &str,
    port: u16,
    timeout: Duration,
    mut dead: impl FnMut() -> Option<String>,
) -> Result<TcpStream> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + timeout;
    loop {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                return Ok(stream);
            }
            Err(e) => {
                if let Some(reason) = dead() {
                    bail!("{label} exited before its port came up ({reason}): {e}");
                }
                if Instant::now() >= deadline {
                    return Err(e).with_context(|| {
                        format!("connecting to {label} on port {port} within {timeout:?}")
                    });
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
}

/// One step of a framed read: append whatever the socket has to `buf`, or
/// sleep briefly through a read timeout. `Ok(false)` once `deadline` has
/// passed with nothing new, `Err` on EOF or a real socket error. The caller
/// keeps `buf` (its carry) on every path, so a half-read frame is never lost.
pub(crate) fn read_step(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    label: &str,
) -> Result<bool> {
    if Instant::now() >= deadline {
        return Ok(false);
    }
    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk) {
        Ok(0) => bail!("{label} closed the connection"),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            std::thread::sleep(Duration::from_millis(3));
            Ok(true)
        }
        Err(e) => Err(e).with_context(|| format!("reading from {label}")),
    }
}

// ── UART bridge ──────────────────────────────────────────────────────────────

/// Host side of an emulator's UART TCP bridge.
///
/// Renode (`emulation CreateServerSocketTerminal <port> "term" false`, then
/// `connector Connect sysbus.<usart> term`; the trailing `false` disables
/// Renode's terminal config handshake) and Espressif QEMU (`-serial
/// tcp:127.0.0.1:<port>,server,nowait`) both expose the firmware's UART as a
/// raw byte stream on a loopback port: bytes the firmware transmits arrive on
/// the socket, and bytes written here are injected into the UART receiver.
pub(crate) struct UartSocket {
    label: &'static str,
    stream: TcpStream,
}

impl UartSocket {
    pub(crate) fn connect(label: &'static str, port: u16, timeout: Duration) -> Result<Self> {
        let stream = connect_loopback(&format!("{label} UART socket"), port, timeout, || None)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .with_context(|| format!("setting {label} UART read timeout"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .with_context(|| format!("setting {label} UART write timeout"))?;
        Ok(UartSocket { label, stream })
    }

    /// Inject bytes into the firmware's UART receiver.
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) -> Result<usize> {
        write_uart_bytes_counted(&mut self.stream, bytes)
            .with_context(|| format!("writing to {} UART socket", self.label))
    }

    /// Drain any bytes the firmware has transmitted since the last call.
    pub(crate) fn drain(&mut self) -> Result<Vec<u8>> {
        drain_uart_bytes(&mut self.stream, self.label)
    }
}

/// A UART socket write that stopped partway: the trait's lossless-or-loud
/// accounting needs to know how many bytes the emulator did accept.
#[derive(Debug)]
pub(crate) struct UartWriteFailure {
    pub written: usize,
    pub source: std::io::Error,
}

impl std::fmt::Display for UartWriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UART socket write failed after {} byte(s): {}",
            self.written, self.source
        )
    }
}

impl std::error::Error for UartWriteFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn write_uart_bytes_counted(writer: &mut impl Write, bytes: &[u8]) -> Result<usize> {
    let mut written = 0;
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                let source = std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "UART socket accepted zero bytes",
                );
                return Err(UartWriteFailure { written, source }.into());
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) => return Err(UartWriteFailure { written, source }.into()),
        }
    }
    writer
        .flush()
        .map_err(|source| UartWriteFailure { written, source })?;
    Ok(written)
}

pub(crate) fn drain_uart_bytes(reader: &mut impl Read, backend: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => bail!("{backend} UART socket closed while draining firmware output"),
            Ok(count) => out.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(out)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading {backend} UART socket"))
            }
        }
    }
}

/// Convert a backend UART socket failure into the trait's lossless-or-loud
/// accounting signal. External emulators cannot return a `Result` through the
/// source-compatible [`Mcu::uart_write`] method, so they retain the failed
/// byte count and expose it through [`Mcu::uart_rx_overflow`].
pub(crate) fn account_uart_injection(
    backend: &str,
    bytes: usize,
    result: Result<usize>,
    failed: &mut u64,
) -> usize {
    match result {
        Ok(accepted) => accepted,
        Err(error) => {
            let accepted = error
                .downcast_ref::<UartWriteFailure>()
                .map_or(0, |failure| failure.written)
                .min(bytes);
            let lost = bytes - accepted;
            if *failed == 0 {
                eprintln!(
                    "ERROR: {backend} UART injection failed after {accepted} byte(s); counting \
                     {lost} host byte(s) as lost: {error:#}"
                );
            }
            *failed = failed.saturating_add(lost as u64);
            accepted
        }
    }
}

// ── Poll-based backend state ─────────────────────────────────────────────────

/// The state every poll-based external backend keeps between chunks: the UART
/// bridge and its accounting, the engine's callbacks, the wired-port hint, and
/// the virtual-cycle counter the coarse edge stamps come from.
pub(crate) struct PollState {
    pub label: &'static str,
    pub uart: Option<UartSocket>,
    /// Host bytes not accepted by the emulator UART socket. Sticky and exposed
    /// through `uart_rx_overflow` so a dead/missing transport cannot look clean.
    pub uart_rx_failed: u64,
    /// Host bytes handed to the socket since the last successful advance. They
    /// are not claimed as presented to the emulated UART until that next
    /// lockstep window completes.
    pub uart_rx_inflight: usize,
    pub on_pin_change: Option<Box<dyn FnMut(PinId, bool, u64) + Send>>,
    pub on_uart: Option<Box<dyn FnMut(u8) + Send>>,
    /// If set, only these port letters are polled each chunk (the ports the
    /// engine actually wired). `None` means poll every configured port.
    pub active_ports: Option<Vec<char>>,
    pub firmware_loaded: bool,
    /// Virtual time advanced so far, in cycles-equivalent (frequency * seconds).
    pub cycles: u64,
}

impl PollState {
    pub(crate) fn new(label: &'static str, uart: Option<UartSocket>) -> Self {
        PollState {
            label,
            uart,
            uart_rx_failed: 0,
            uart_rx_inflight: 0,
            on_pin_change: None,
            on_uart: None,
            active_ports: None,
            firmware_loaded: false,
            cycles: 0,
        }
    }

    /// [`Mcu::uart_write`]: hand the bytes to the socket, counting what it
    /// refused as lost rather than silently dropping it.
    pub(crate) fn uart_write(&mut self, bytes: &[u8]) {
        let result = self
            .uart
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("this {} descriptor has no UART socket", self.label))
            .and_then(|uart| uart.write_bytes(bytes));
        let accepted =
            account_uart_injection(self.label, bytes.len(), result, &mut self.uart_rx_failed);
        self.uart_rx_inflight = self.uart_rx_inflight.saturating_add(accepted);
    }

    /// Drain UART bytes the firmware emitted and dispatch them, echoing them
    /// to stderr when `trace` is set.
    pub(crate) fn pump_uart_out(&mut self, trace: bool) -> Result<()> {
        let Some(uart) = &mut self.uart else {
            return Ok(());
        };
        let bytes = uart.drain()?;
        if trace && !bytes.is_empty() {
            eprintln!(
                "{}-uart {}",
                self.label.to_ascii_lowercase(),
                String::from_utf8_lossy(&bytes)
            );
        }
        if let Some(cb) = &mut self.on_uart {
            bytes.into_iter().for_each(cb);
        }
        Ok(())
    }

    /// Whether `letter` is one of the ports the engine asked to be polled.
    pub(crate) fn polls(&self, letter: char) -> bool {
        self.active_ports
            .as_ref()
            .is_none_or(|active| active.contains(&letter))
    }

    /// Fire the pin-change callback for every set bit of `changed` on a
    /// `width`-bit port, at the current (poll-boundary, coarse) cycle stamp.
    pub(crate) fn publish_edges(&mut self, letter: char, width: u8, changed: u32, new: u32) {
        let cycle = self.cycles;
        if let Some(cb) = &mut self.on_pin_change {
            for bit in (0..width).filter(|bit| (changed >> bit) & 1 != 0) {
                cb(PinId { port: letter, bit }, (new >> bit) & 1 != 0, cycle);
            }
        }
    }

    /// Credit `seconds` of virtual time to the cycle counter.
    pub(crate) fn credit(&mut self, seconds: f64, frequency_hz: u64) {
        self.cycles += (seconds * frequency_hz as f64).round() as u64;
    }

    /// [`Mcu::state`]: the poll path carries no PC or terminal-CPU signal, so
    /// it reports the cached cycle count and "still running".
    pub(crate) fn state(&self) -> McuState {
        McuState {
            pc: 0,
            cycles: self.cycles,
            sleeping: false,
            done: false,
            crashed: false,
        }
    }
}

/// The [`Mcu`] methods every poll-based backend answers identically from its
/// [`PollState`] field, spelled once. Invoked inside each backend's `impl Mcu`.
/// A backend whose run failures carry extra diagnosis (Renode folds its exit
/// status and stderr tail into the error) passes `own_run_micros` and writes
/// that one method itself; everything else is shared either way.
macro_rules! poll_state_mcu_methods {
    ($core:ident) => {
        fn run_micros(&mut self, us: u64) -> Result<()> {
            self.run_seconds(us as f64 / 1_000_000.0)
        }

        poll_state_mcu_methods!(@shared $core);
    };
    ($core:ident, own_run_micros) => {
        poll_state_mcu_methods!(@shared $core);
    };
    (@shared $core:ident) => {
        fn frequency(&self) -> u64 {
            self.config.frequency_hz
        }

        fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {
            self.$core.on_pin_change = Some(cb);
        }

        fn current_cycle(&self) -> u64 {
            self.$core.cycles
        }

        /// Poll-based: GPIO edges are observed by diffing output registers per
        /// time slice, so toggles within a slice collapse and ordering is coarse.
        fn cycle_exact(&self) -> bool {
            false
        }

        fn uart_write(&mut self, bytes: &[u8]) {
            self.$core.uart_write(bytes);
        }

        fn uart_rx_overflow(&self) -> u64 {
            self.$core.uart_rx_failed
        }

        fn uart_rx_pending(&self) -> usize {
            self.$core.uart_rx_inflight
        }

        fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>) {
            self.$core.on_uart = Some(cb);
        }

        fn state(&self) -> McuState {
            self.$core.state()
        }
    };
}
pub(crate) use poll_state_mcu_methods;

/// Named constructors over the shipped `db/mcu/*.soc.toml` descriptors: the
/// embedded table in [`crate::soc`] is the single source of truth for register
/// offsets, platform paths and port maps, and each accessor is one lookup in
/// it. `.expect` is correct here: a shipped descriptor failing to load is a
/// build bug `tests/soc_descriptors.rs` catches, never a runtime condition. A
/// fresh part is added purely as data via [`crate::SocConfig::resolve`].
macro_rules! builtin_configs {
    ($($(#[$doc:meta])* $name:ident => $spec:literal;)*) => {$(
        $(#[$doc])*
        pub fn $name() -> Self {
            let src = crate::soc::SocConfig::builtin_source($spec)
                .expect(concat!($spec, " is an embedded descriptor"));
            Self::from_soc_toml(src).expect(concat!("built-in ", $spec, " descriptor is valid"))
        }
    )*};
}
pub(crate) use builtin_configs;

/// The open I2C transaction a bus master's transaction-level requests are
/// turned into Start/Stop events against: `(addr, read)`. Both external
/// backends see the bus at that granularity (the Renode bridge peripheral's
/// Write/Read/FinishTransmission, the QEMU mailbox's op cells), so they share
/// one state machine and the engine's slave models see identical event streams.
#[derive(Default)]
pub(crate) struct I2cTxn(Option<(u8, bool)>);

impl I2cTxn {
    /// Bring the transaction into `(addr, read)` mode, synthesising the Stop
    /// and Start that implies: switching TO write stops any open transaction;
    /// switching to read on the SAME address is a repeated START (no Stop, a
    /// register-read slave must not see its transaction boundary mid-read); a
    /// read on a DIFFERENT address stops the old transaction first.
    pub(crate) fn ensure(
        &mut self,
        addr: u8,
        read: bool,
        cb: &mut impl FnMut(I2cEvent) -> Option<u8>,
    ) {
        if self.0 == Some((addr, read)) {
            return;
        }
        if let Some((prev, _)) = self.0 {
            if !read || prev != addr {
                let _ = cb(I2cEvent::Stop { addr: prev });
                self.0 = None;
            }
        }
        let _ = cb(I2cEvent::Start { addr, read });
        self.0 = Some((addr, read));
    }

    /// Stop the open transaction, if any.
    pub(crate) fn stop(&mut self, cb: &mut impl FnMut(I2cEvent) -> Option<u8>) {
        if let Some((addr, _)) = self.0.take() {
            let _ = cb(I2cEvent::Stop { addr });
        }
    }
}

/// Quantize a voltage to an n-bit ADC code. An n-bit converter's transfer
/// function is round(frac * 2^n) saturated at 2^n - 1: multiply by
/// (`max_count` + 1) then clamp to `max_count`. Multiplying by `max_count`
/// itself (2^n - 1) systematically under-reads sub-full-scale voltages by up to
/// ~1 LSB and only reaches the top code at exactly full scale. One function for
/// every backend (and the engine's SPI ADC path uses the same law) so a given
/// voltage quantizes to the same code everywhere. A non-positive full scale
/// reads as stuck at zero, never NaN-poisoned.
pub(crate) fn adc_count(volts: f64, full_scale_volts: f64, max_count: u32) -> u32 {
    if !(full_scale_volts > 0.0) {
        return 0;
    }
    let frac = (volts / full_scale_volts).clamp(0.0, 1.0);
    ((frac * (f64::from(max_count) + 1.0)).round() as u32).min(max_count)
}

/// Firmware discovery helper shared by the process locators: the first
/// candidate that is a regular file and passes `accept`.
pub(crate) fn first_accepted(
    candidates: impl IntoIterator<Item = PathBuf>,
    accept: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates.into_iter().find(|c| c.is_file() && accept(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_external_uart_write_is_counted_loudly() {
        let mut failed = 3;
        let accepted = account_uart_injection(
            "test backend",
            7,
            Err(anyhow::anyhow!("closed socket")),
            &mut failed,
        );
        assert_eq!(accepted, 0);
        assert_eq!(failed, 10);
    }

    #[test]
    fn partial_uart_write_counts_only_the_unwritten_suffix() {
        struct PrefixThenFail(bool);
        impl Write for PrefixThenFail {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0 {
                    Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
                } else {
                    self.0 = true;
                    Ok(bytes.len().min(3))
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let result = write_uart_bytes_counted(&mut PrefixThenFail(false), b"1234567");
        let mut failed = 0;
        let accepted = account_uart_injection("test backend", 7, result, &mut failed);
        assert_eq!(accepted, 3);
        assert_eq!(failed, 4);
    }

    #[test]
    fn uart_drain_refuses_eof_and_non_timeout_errors() {
        assert!(drain_uart_bytes(&mut std::io::Cursor::new(Vec::<u8>::new()), "test").is_err());

        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _bytes: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ))
            }
        }
        assert!(drain_uart_bytes(&mut Broken, "test").is_err());
    }

    #[test]
    fn free_ports_are_distinct() {
        let [a, b, c] = free_ports::<3>().unwrap();
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn adc_count_uses_2n_scaling() {
        assert_eq!(adc_count(0.0, 3.3, 4095), 0);
        assert_eq!(adc_count(3.3, 3.3, 4095), 4095);
        assert_eq!(adc_count(5.0, 3.3, 4095), 4095);
        assert_eq!(adc_count(-1.0, 3.3, 4095), 0);
        // (2.0/3.3)*4096 = 2482.4 -> 2482; the top-code clamp only bites at
        // true full scale.
        assert_eq!(adc_count(2.0, 3.3, 4095), 2482);
        // Near full scale the 2^n scaling reads 4095 where (2^n-1) would
        // under-read to 4094.
        assert_eq!(adc_count(3.2992, 3.3, 4095), 4095);
        assert_eq!(adc_count(1.0, 0.0, 4095), 0);
    }
}
