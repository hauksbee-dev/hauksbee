//! The bench recorder: what a co-sim run looked like from outside the chip.
//!
//! A bench engineer clips a scope, a logic analyser and a terminal onto the
//! board. This module is the co-sim's version of the three:
//!
//! * [`Observer`] records, chunk by chunk, every MCU pin edge (with its time),
//!   every serial byte, every reset-line pulse and every watchdog bite;
//! * [`TxnRecorder`] lives inside each I2C/SPI bus bridge and records each
//!   transaction the firmware drove (address, bytes, ACK) as it is dispatched;
//! * [`bench`] turns those recordings into the [`BenchReport`] every surface
//!   shows (CLI block, JSON, app strip), [`timeline`] into the VCD and the
//!   bus log, and [`symbols`] names the function a parked core sits in.
//!
//! The wire shapes and every word they carry live in
//! `hauksbee_ir::observations`; this module only fills them in.
//!
//! Timing honesty: edge times are exact on simavr (cycle-stamped inside the
//! chunk) and chunk-resolution on the poll backends (Renode, QEMU), whose
//! per-pin stamps are coarse; a frequency averaged over many edges is still
//! right on both, a single edge time is only as good as the chunk.

pub mod bench;
pub mod symbols;
pub mod timeline;
pub mod vcd;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hauksbee_bind::digital::PinEdge;
use hauksbee_ir::observations::{BenchEvent, BusKind, EventKind};

/// Pin edges kept across the whole run before the recorder stops storing
/// waveform (counts stay exact).
pub const MAX_STORED_EDGES: usize = 2_000_000;
/// Bus transactions kept per run.
pub const MAX_BUS_TXNS: usize = 20_000;
/// Serial lines kept per MCU.
pub const MAX_UART_LINES: usize = 2_000;
/// Longest serial line kept, in bytes.
pub const MAX_LINE_BYTES: usize = 240;

/// One bus transaction as the bridge saw it, before the scheduler names it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawBusTxn {
    /// Simulated time the transaction began (the chunk it began in).
    pub t_s: f64,
    pub bus: BusKind,
    /// The bus bridge's id: the board reference or spec peripheral id.
    pub device: String,
    pub addr: Option<u8>,
    pub read: bool,
    /// I2C: bytes written or read. SPI: MOSI.
    pub bytes: Vec<u8>,
    /// SPI: MISO.
    pub reply: Vec<u8>,
    /// I2C: the address was acknowledged. SPI: the device was powered.
    pub acked: bool,
}

/// A bus bridge's transaction log: it opens a transaction when the firmware
/// addresses the device, fills it as bytes move, and closes it at STOP, a
/// repeated START, or chip-select release.
#[derive(Debug, Default)]
pub struct TxnRecorder {
    now_s: f64,
    open: Option<RawBusTxn>,
    done: Vec<RawBusTxn>,
    dropped: u64,
}

impl TxnRecorder {
    /// Stamp subsequent transactions with the current simulated time. The
    /// scheduler calls this at the start of every chunk.
    pub const fn set_time(&mut self, t_s: f64) {
        self.now_s = t_s;
    }

    /// Open a transaction, closing any still-open one first (a repeated START
    /// ends the previous leg).
    pub fn begin(&mut self, bus: BusKind, device: &str, addr: Option<u8>, read: bool, acked: bool) {
        self.finish();
        self.open = Some(RawBusTxn {
            t_s: self.now_s,
            bus,
            device: device.to_string(),
            addr,
            read,
            bytes: Vec::new(),
            reply: Vec::new(),
            acked,
        });
    }

    /// Whether a transaction is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The open transaction, to append bytes to.
    pub const fn open_mut(&mut self) -> Option<&mut RawBusTxn> {
        self.open.as_mut()
    }

    /// Close the open transaction. A chip-select frame that moved no bytes is
    /// dropped (nothing happened); an I2C address-only leg is kept (a probe).
    pub fn finish(&mut self) {
        let Some(txn) = self.open.take() else {
            return;
        };
        if txn.bus == BusKind::Spi && txn.bytes.is_empty() && txn.reply.is_empty() {
            return;
        }
        if self.done.len() >= MAX_BUS_TXNS {
            self.dropped += 1;
        } else {
            self.done.push(txn);
        }
    }

    /// Take the completed transactions; with `include_open`, close and take
    /// the in-flight one too (end of run).
    pub fn take(&mut self, include_open: bool) -> Vec<RawBusTxn> {
        if include_open {
            self.finish();
        }
        std::mem::take(&mut self.done)
    }

    /// Transactions dropped for want of room.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Everything one pin did, kept compact: running aggregates stay exact after
/// the stored waveform hits its cap.
#[derive(Debug, Clone, Default)]
pub struct PinTrace {
    /// Stored edges `(t_s, level)`, in order, up to the recorder's cap.
    pub edges: Vec<(f64, bool)>,
    /// Edges seen, stored or not.
    pub edge_count: u64,
    /// Rising edges seen.
    pub rises: u64,
    pub first: Option<(f64, bool)>,
    pub last: Option<(f64, bool)>,
    pub first_rise_s: Option<f64>,
    pub last_rise_s: Option<f64>,
    /// Seconds spent high so far (from the first edge).
    high_acc: f64,
    high_at_first_rise: f64,
    high_at_last_rise: f64,
}

impl PinTrace {
    /// Record one level change. A repeat of the current level is ignored.
    fn push(&mut self, t: f64, level: bool, store: bool) {
        if self.last.is_some_and(|(_, l)| l == level) {
            return;
        }
        if let Some((pt, true)) = self.last {
            self.high_acc += (t - pt).max(0.0);
        }
        self.edge_count += 1;
        if level {
            self.rises += 1;
            if self.first_rise_s.is_none() {
                self.first_rise_s = Some(t);
                self.high_at_first_rise = self.high_acc;
            }
            self.last_rise_s = Some(t);
            self.high_at_last_rise = self.high_acc;
        }
        if self.first.is_none() {
            self.first = Some((t, level));
        }
        self.last = Some((t, level));
        if store {
            self.edges.push((t, level));
        }
    }

    /// Full cycles per second between the first and last rising edge.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // edge counts are far below 2^52
    pub fn freq_hz(&self) -> Option<f64> {
        let (a, b) = (self.first_rise_s?, self.last_rise_s?);
        (self.rises >= 2 && b > a).then(|| (self.rises - 1) as f64 / (b - a))
    }

    /// Fraction of the time between the first and last rising edge spent high.
    #[must_use]
    pub fn duty(&self) -> Option<f64> {
        let (a, b) = (self.first_rise_s?, self.last_rise_s?);
        (self.rises >= 2 && b > a)
            .then(|| ((self.high_at_last_rise - self.high_at_first_rise) / (b - a)).clamp(0.0, 1.0))
    }
}

/// One serial line the firmware printed.
#[derive(Debug, Clone, PartialEq)]
pub struct UartLine {
    pub t_s: f64,
    pub text: String,
}

/// A serial port's output over the run.
#[derive(Debug, Clone, Default)]
pub struct UartTrace {
    pub bytes: u64,
    pub printable: u64,
    pub first_byte_s: Option<f64>,
    pub lines: Vec<UartLine>,
    current: Vec<u8>,
    current_t: f64,
}

impl UartTrace {
    fn push(&mut self, t: f64, data: &[u8]) {
        for &b in data {
            if self.first_byte_s.is_none() {
                self.first_byte_s = Some(t);
            }
            self.bytes += 1;
            if b == b'\n' || b == b'\r' || b == b'\t' || (0x20..0x7f).contains(&b) {
                self.printable += 1;
            }
            if b == b'\n' || b == b'\r' {
                self.flush_line();
            } else {
                if self.current.is_empty() {
                    self.current_t = t;
                }
                if self.current.len() < MAX_LINE_BYTES {
                    self.current.push(b);
                }
            }
        }
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let text = printable_text(&self.current);
        self.current.clear();
        if self.lines.len() < MAX_UART_LINES {
            self.lines.push(UartLine {
                t_s: self.current_t,
                text,
            });
        }
    }

    /// All lines, including a final unterminated one.
    #[must_use]
    pub fn all_lines(&self) -> Vec<UartLine> {
        let mut lines = self.lines.clone();
        if !self.current.is_empty() && lines.len() < MAX_UART_LINES {
            lines.push(UartLine {
                t_s: self.current_t,
                text: printable_text(&self.current),
            });
        }
        lines
    }
}

/// Bytes as text a person can read: printable ASCII kept, the rest escaped.
fn printable_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x20..=0x7e => out.push(char::from(b)),
            b'\t' => out.push(' '),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.trim().to_string()
}

/// What the recorder knows about one MCU.
#[derive(Debug, Clone, Default)]
pub struct McuObs {
    pub mcu_ref: String,
    pub pins: BTreeMap<(char, u8), PinTrace>,
    pub uart: UartTrace,
    /// Reset-line state at the last observation (`true` = released/high).
    reset_released: Option<bool>,
    /// Whether the line has been seen released since power-on, so a later
    /// assertion is a real reset rather than the power-on hold.
    reset_seen_released: bool,
    /// Index in [`Observer::events`] of the reset event still waiting for its
    /// release time.
    open_reset_event: Option<usize>,
    reset_low_since: f64,
    /// Completed reset pulses.
    pub reset_pulses: u32,
    /// Watchdog reboots already booked.
    pub watchdog_booked: u64,
    pub first_activity_s: Option<f64>,
    pub last_activity_s: Option<f64>,
}

impl McuObs {
    fn touch(&mut self, t: f64) {
        if self.first_activity_s.is_none() {
            self.first_activity_s = Some(t);
        }
        if self.last_activity_s.is_none_or(|l| t > l) {
            self.last_activity_s = Some(t);
        }
    }
}

/// The run recorder.
#[derive(Debug, Default)]
pub struct Observer {
    pub mcus: Vec<McuObs>,
    pub bus: Vec<RawBusTxn>,
    pub events: Vec<BenchEvent>,
    /// Simulated seconds observed so far.
    pub end_s: f64,
    /// The solver chunk, for the width of VCD marker pulses.
    pub chunk_s: f64,
    stored_edges: usize,
    /// Set when a cap was hit.
    pub truncated: bool,
}

/// Release-threshold fractions of the logic-high level for the reset line.
const RESET_RELEASED_FRAC: f64 = 0.7;
const RESET_ASSERTED_FRAC: f64 = 0.3;

impl Observer {
    /// Make sure MCU `mi` exists.
    pub fn ensure_mcu(&mut self, mi: usize, mcu_ref: &str) {
        while self.mcus.len() <= mi {
            self.mcus.push(McuObs::default());
        }
        if self.mcus[mi].mcu_ref.is_empty() {
            self.mcus[mi].mcu_ref = mcu_ref.to_string();
        }
    }

    /// Fold one MCU's chunk: its ordered cycle-stamped pin edges and serial
    /// bytes. `t0` is the chunk's start time, `span` its `[start, end)` cycle
    /// window.
    #[allow(clippy::cast_precision_loss)] // cycle offsets within one chunk are far below 2^52
    pub fn record_chunk(
        &mut self,
        mi: usize,
        t0: f64,
        chunk_s: f64,
        span: (u64, u64),
        edge_log: &[PinEdge],
        uart: &[u8],
    ) {
        self.end_s = self.end_s.max(t0 + chunk_s);
        if self.chunk_s <= 0.0 {
            self.chunk_s = chunk_s;
        }
        let (c0, c1) = span;
        let width = c1.saturating_sub(c0) as f64;
        for e in edge_log {
            let frac = if width > 0.0 {
                (e.cycle.saturating_sub(c0) as f64 / width).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let t = frac.mul_add(chunk_s, t0);
            let store = self.stored_edges < MAX_STORED_EDGES;
            if !store {
                self.truncated = true;
            }
            let mcu = &mut self.mcus[mi];
            let trace = mcu.pins.entry((e.port, e.bit)).or_default();
            let before = trace.edge_count;
            trace.push(t, e.level, store);
            if trace.edge_count > before {
                if store {
                    self.stored_edges += 1;
                }
                mcu.touch(t);
            }
        }
        if !uart.is_empty() {
            let mcu = &mut self.mcus[mi];
            mcu.uart.push(t0, uart);
            mcu.touch(t0);
        }
    }

    /// Fold the reset line's level after a solve: `Some(true)` released,
    /// `Some(false)` asserted, `None` unobservable. Edges are detected with
    /// hysteresis (below 30% asserted, above 70% released).
    pub fn record_reset_level(&mut self, mi: usize, t: f64, volts: Option<f64>, logic_high_v: f64) {
        let Some(v) = volts else {
            return;
        };
        let released = if v >= RESET_RELEASED_FRAC * logic_high_v {
            Some(true)
        } else if v <= RESET_ASSERTED_FRAC * logic_high_v {
            Some(false)
        } else {
            None
        };
        let Some(now) = released else {
            return;
        };
        let mcu = &mut self.mcus[mi];
        let prev = mcu.reset_released.replace(now);
        match (prev, now) {
            (Some(true), false) => {
                mcu.reset_low_since = t;
                let mcu_ref = mcu.mcu_ref.clone();
                mcu.open_reset_event = Some(self.events.len());
                self.events.push(BenchEvent {
                    t_s: t,
                    kind: EventKind::Reset,
                    mcu_ref,
                    detail: "reset line pulled low (still low at the end of the run)".into(),
                });
            }
            (Some(false), true) => {
                if let Some(idx) = mcu.open_reset_event.take() {
                    let low_s = (t - mcu.reset_low_since).max(0.0);
                    mcu.reset_pulses += 1;
                    if let Some(ev) = self.events.get_mut(idx) {
                        ev.detail = format!("reset line pulsed low for {}", fmt_duration(low_s));
                    }
                }
                mcu.reset_seen_released = true;
            }
            (None | Some(true), true) => mcu.reset_seen_released = true,
            _ => {}
        }
    }

    /// Fold the backend's cumulative watchdog-reboot count.
    pub fn record_watchdog(&mut self, mi: usize, t: f64, total: u64, exact_time: bool) {
        let mcu = &mut self.mcus[mi];
        if total <= mcu.watchdog_booked {
            return;
        }
        let new = total - mcu.watchdog_booked;
        mcu.watchdog_booked = total;
        let mcu_ref = mcu.mcu_ref.clone();
        let detail = if exact_time {
            format!("the watchdog rebooted the core ({new} new)")
        } else {
            format!("the watchdog rebooted the core {new} time(s); when is not resolved on this backend")
        };
        self.events.push(BenchEvent {
            t_s: t,
            kind: EventKind::WatchdogReset,
            mcu_ref,
            detail,
        });
    }

    /// Fold bus transactions drained from the bridges.
    pub fn push_bus(&mut self, mut txns: Vec<RawBusTxn>) {
        let room = MAX_BUS_TXNS.saturating_sub(self.bus.len());
        if txns.len() > room {
            txns.truncate(room);
            self.truncated = true;
        }
        self.bus.extend(txns);
    }
}

/// `1.5 ms`, `20 us`, `2.00 s`.
#[must_use]
pub fn fmt_duration(s: f64) -> String {
    let abs = s.abs();
    if abs >= 1.0 {
        format!("{s:.2} s")
    } else if abs >= 1e-3 {
        format!("{:.1} ms", s * 1e3)
    } else if abs >= 1e-6 {
        format!("{:.0} us", s * 1e6)
    } else {
        format!("{:.0} ns", s * 1e9)
    }
}

/// `5.0 Hz`, `1.2 kHz`, `0.50 Hz`.
#[must_use]
pub fn fmt_hz(hz: f64) -> String {
    if hz >= 1e6 {
        format!("{:.2} MHz", hz / 1e6)
    } else if hz >= 1e3 {
        format!("{:.2} kHz", hz / 1e3)
    } else if hz >= 10.0 {
        format!("{hz:.1} Hz")
    } else {
        format!("{hz:.2} Hz")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(cycle: u64, level: bool) -> PinEdge {
        PinEdge {
            cycle,
            port: 'C',
            bit: 13,
            level,
        }
    }

    #[test]
    fn a_square_wave_reports_its_frequency_and_duty() {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        // 5 Hz at 25% duty (period 0.2 s = 400_000 cycles at 2 MHz) over two
        // 0.5 s chunks of 1_000_000 cycles each: rises at 0, 0.2, 0.4, 0.6, 0.8 s.
        let mut log0 = Vec::new();
        let mut log1 = Vec::new();
        for k in 0..5_u64 {
            let (rise, fall) = (k * 400_000, k * 400_000 + 100_000);
            let log = if rise < 1_000_000 {
                &mut log0
            } else {
                &mut log1
            };
            log.push(edge(rise, true));
            log.push(edge(fall, false));
        }
        obs.record_chunk(0, 0.0, 0.5, (0, 1_000_000), &log0, &[]);
        obs.record_chunk(0, 0.5, 0.5, (1_000_000, 2_000_000), &log1, &[]);
        let trace = &obs.mcus[0].pins[&('C', 13)];
        assert_eq!(trace.edge_count, 10);
        let f = trace.freq_hz().unwrap();
        assert!((f - 5.0).abs() < 0.01, "5 Hz (period 0.2 s), got {f}");
        let d = trace.duty().unwrap();
        assert!((d - 0.25).abs() < 0.01, "25% duty, got {d}");
    }

    #[test]
    fn a_pin_set_once_is_static() {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        obs.record_chunk(
            0,
            0.0,
            0.1,
            (0, 100),
            &[edge(10, true), edge(20, true)],
            &[],
        );
        let trace = &obs.mcus[0].pins[&('C', 13)];
        assert_eq!(trace.edge_count, 1, "a repeated level is not an edge");
        assert!(trace.freq_hz().is_none());
    }

    #[test]
    fn serial_lines_split_on_newline_and_keep_the_first() {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        obs.record_chunk(0, 0.25, 0.1, (0, 1), &[], b"hello\r\nworld\n\x01x");
        let uart = &obs.mcus[0].uart;
        assert_eq!(uart.first_byte_s, Some(0.25));
        let lines = uart.all_lines();
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text, "world");
        assert_eq!(lines[2].text, "\\x01x");
        assert_eq!(uart.bytes, 15);
    }

    #[test]
    fn reset_pulse_counts_only_after_the_line_was_released() {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        // Power-on hold low, release (not a reset), then a real pulse.
        for (t, v) in [(0.0, 0.0), (0.1, 3.3), (0.2, 3.3), (0.3, 0.0), (0.4, 3.3)] {
            obs.record_reset_level(0, t, Some(v), 3.3);
        }
        assert_eq!(obs.mcus[0].reset_pulses, 1);
        assert_eq!(obs.events.len(), 1);
        assert!(obs.events[0].detail.contains("pulsed low for"));
        assert!((obs.events[0].t_s - 0.3).abs() < 1e-9);
    }

    #[test]
    fn txn_recorder_closes_on_repeated_start_and_drops_empty_spi_frames() {
        let mut rec = TxnRecorder::default();
        rec.set_time(1.5);
        rec.begin(BusKind::I2c, "U2", Some(0x48), false, true);
        rec.open_mut().unwrap().bytes.push(0x00);
        rec.begin(BusKind::I2c, "U2", Some(0x48), true, true); // repeated start
        rec.open_mut().unwrap().bytes.extend([0x1a, 0x80]);
        rec.begin(BusKind::Spi, "U3", None, false, true);
        rec.finish(); // empty frame
        let all = rec.take(true);
        assert_eq!(all.len(), 2);
        assert!(!all[0].read && all[0].bytes == [0x00]);
        assert!(all[1].read && all[1].bytes == [0x1a, 0x80]);
        assert!((all[0].t_s - 1.5).abs() < 1e-12);
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_hz(5.0), "5.00 Hz");
        assert_eq!(fmt_hz(1500.0), "1.50 kHz");
        assert_eq!(fmt_duration(0.0015), "1.5 ms");
        assert_eq!(fmt_duration(2.0), "2.00 s");
    }
}
