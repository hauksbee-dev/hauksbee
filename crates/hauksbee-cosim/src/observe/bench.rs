//! The bench view: the recorder's data turned into the words a person reads.
//!
//! [`build_report`] fills [`hauksbee_ir::observations::BenchReport`] from an
//! [`Observer`] plus what the scheduler knows about each MCU (its pin nets, its
//! execution evidence, its ELF symbols). [`render_text`] prints it for the
//! CLI. The vocabulary and the data shapes are `hauksbee_ir::observations`;
//! nothing here invents a word.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use hauksbee_ir::observations::{
    is_trap_symbol, BenchEvent, BenchReport, BusKind, BusTransaction, CoreState, EdgeSample,
    EventKind, McuBench, ParkSite, PinRow, PinState, UartRow, MAX_EDGE_SAMPLES,
};

use super::symbols::SymbolTable;
use super::{fmt_duration, fmt_hz, McuObs, Observer, PinTrace, RawBusTxn};
use crate::execution::ExecutionEvidence;

/// Samples a parked-loop verdict needs before it is trusted.
const PARK_MIN_SAMPLES: u64 = 16;
/// A loop this tight (distinct program counters) is a park, not a program.
const PARK_MAX_DISTINCT_PCS: u64 = 3;
/// A core is quiet only if its last output is older than this fraction of the
/// run (and at least two chunks).
const QUIET_FRACTION: f64 = 0.25;

/// What the scheduler knows about one MCU that the recorder does not.
#[derive(Debug, Clone, Default)]
pub struct McuContext {
    pub mcu_ref: String,
    pub backend: String,
    /// Net each wired pin drives.
    pub pin_nets: HashMap<(char, u8), String>,
    /// Net the UART TX pin drives, when the binder knows it.
    pub uart_net: Option<String>,
    /// Pins the core reports as configured outputs at the end of the run.
    pub configured_outputs: BTreeSet<(char, u8)>,
    pub evidence: Option<ExecutionEvidence>,
    pub symbols: Option<SymbolTable>,
}

/// `PC13`.
#[must_use]
pub fn pin_name(port: char, bit: u8) -> String {
    format!("P{port}{bit}")
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut s = String::from("[");
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "0x{b:02x}");
    }
    s.push(']');
    s
}

/// One bus transaction in words: `U2 0x48 write [0x00] ACK`.
#[must_use]
pub fn bus_summary(t: &RawBusTxn) -> String {
    match t.bus {
        BusKind::I2c => {
            let addr = t.addr.map_or_else(String::new, |a| format!(" 0x{a:02x}"));
            let dir = if t.read { "read" } else { "write" };
            let data = if t.bytes.is_empty() {
                String::new()
            } else {
                format!(" {}", hex_bytes(&t.bytes))
            };
            let ack = if t.acked {
                "ACK"
            } else {
                "NACK (no device answered)"
            };
            format!("{}{addr} {dir}{data} {ack}", t.device)
        }
        BusKind::Spi => {
            let powered = if t.acked { "" } else { " (device unpowered)" };
            format!(
                "{} SPI mosi {} miso {}{powered}",
                t.device,
                hex_bytes(&t.bytes),
                hex_bytes(&t.reply)
            )
        }
        BusKind::Uart => format!("{} UART {:?}", t.device, String::from_utf8_lossy(&t.bytes)),
    }
}

fn pin_row(
    port: char,
    bit: u8,
    trace: Option<&PinTrace>,
    net: Option<&String>,
    configured: bool,
) -> PinRow {
    let pin = pin_name(port, bit);
    let net = net.cloned();
    let Some(trace) = trace.filter(|t| t.edge_count > 0) else {
        let state = if configured {
            PinState::StaticLow
        } else {
            PinState::Input
        };
        let summary = if configured {
            "configured as an output, never changed".to_string()
        } else {
            "never driven (input or Hi-Z)".to_string()
        };
        return PinRow {
            pin,
            net,
            state,
            edges: 0,
            freq_hz: None,
            duty: None,
            first_edge_s: None,
            final_high: None,
            summary,
            edge_samples: Vec::new(),
            samples_truncated: false,
        };
    };
    let final_high = trace.last.map(|(_, l)| l);
    let toggling = trace.edge_count >= 2;
    let state = if toggling {
        PinState::Toggling
    } else if final_high == Some(true) {
        PinState::StaticHigh
    } else {
        PinState::StaticLow
    };
    let (freq_hz, duty) = if toggling {
        (trace.freq_hz(), trace.duty())
    } else {
        (None, None)
    };
    let first_edge_s = trace.first.map(|(t, _)| t);
    let summary = match (toggling, freq_hz, duty) {
        (true, Some(f), Some(d)) => format!("toggling {}, {:.0}% duty", fmt_hz(f), d * 100.0),
        (true, _, _) => format!("changed level {} times", trace.edge_count),
        (false, _, _) => format!(
            "static {}, set at {}",
            if final_high == Some(true) {
                "high"
            } else {
                "low"
            },
            fmt_duration(first_edge_s.unwrap_or(0.0))
        ),
    };
    let stored = trace.edges.len().min(MAX_EDGE_SAMPLES);
    PinRow {
        pin,
        net,
        state,
        edges: trace.edge_count,
        freq_hz,
        duty,
        first_edge_s,
        final_high,
        summary,
        edge_samples: trace.edges[..stored]
            .iter()
            .map(|&(t_s, high)| EdgeSample { t_s, high })
            .collect(),
        samples_truncated: trace.edge_count > stored as u64,
    }
}

/// Where the core ended the run, and the one-line words for it.
fn core_state(
    obs: &McuObs,
    ctx: &McuContext,
    duration_s: f64,
    chunk_s: f64,
) -> (CoreState, Option<ParkSite>, String) {
    let Some(ev) = ctx.evidence.as_ref() else {
        return (
            CoreState::Unknown,
            None,
            "this backend cannot say where the core is".into(),
        );
    };
    if !ev.faults.is_empty() {
        return (
            CoreState::Faulted,
            None,
            format!("faulted: {}", ev.faults.join(", ")),
        );
    }
    let Some(pc) = ev.last_pc else {
        let words = if ev.executed {
            "running"
        } else {
            "no sign the firmware ran"
        };
        return (
            if ev.executed {
                CoreState::Running
            } else {
                CoreState::Unknown
            },
            None,
            words.into(),
        );
    };
    let symbol = ctx
        .symbols
        .as_ref()
        .and_then(|s| s.lookup(pc))
        .map(str::to_string);
    let quiet_for_s = duration_s - obs.last_activity_s.unwrap_or(0.0);
    let quiet = quiet_for_s >= (QUIET_FRACTION * duration_s).max(2.0 * chunk_s);
    let tight_loop = ev.pc_samples >= PARK_MIN_SAMPLES && ev.distinct_pcs <= PARK_MAX_DISTINCT_PCS;
    let trapped = symbol.as_deref().is_some_and(is_trap_symbol);
    let site = || ParkSite {
        pc,
        symbol: symbol.clone(),
        pc_samples: ev.pc_samples,
        distinct_pcs: ev.distinct_pcs,
        quiet_since_s: obs.last_activity_s,
    };
    let at = symbol
        .as_deref()
        .map_or_else(|| format!("0x{pc:08x}"), str::to_string);
    if trapped && (tight_loop || quiet) {
        return (
            CoreState::Trapped,
            Some(site()),
            format!("trapped in {at}: the firmware gave up here"),
        );
    }
    if tight_loop && quiet {
        let since = obs.last_activity_s.map_or_else(
            || "never produced output".to_string(),
            |t| format!("quiet since {}", fmt_duration(t)),
        );
        return (
            CoreState::Parked,
            Some(site()),
            format!("parked at {at} ({since})"),
        );
    }
    if ev.executed {
        (CoreState::Running, None, "running".into())
    } else {
        (CoreState::Unknown, None, "no sign the firmware ran".into())
    }
}

#[allow(clippy::cast_precision_loss)] // byte counts are far below 2^52
fn uart_row(obs: &McuObs, ctx: &McuContext) -> Option<UartRow> {
    if obs.uart.bytes == 0 && ctx.uart_net.is_none() {
        return None;
    }
    let first_line = obs.uart.all_lines().into_iter().next().map(|l| l.text);
    let printable_ratio = if obs.uart.bytes == 0 {
        0.0
    } else {
        obs.uart.printable as f64 / obs.uart.bytes as f64
    };
    Some(UartRow {
        port: "UART".into(),
        net: ctx.uart_net.clone(),
        baud: None,
        baud_note: Some("the baud rate is not read from the peripheral on this backend".into()),
        bytes: obs.uart.bytes,
        printable_ratio,
        first_line,
        first_byte_s: obs.uart.first_byte_s,
    })
}

fn mcu_events(
    obs: &McuObs,
    ctx: &McuContext,
    bench: &McuBench,
    duration_s: f64,
) -> Vec<BenchEvent> {
    let mut events = Vec::new();
    let mcu_ref = &ctx.mcu_ref;
    let first_edge = obs
        .pins
        .iter()
        .filter_map(|(&(p, b), tr)| tr.first.map(|(t, l)| (t, pin_name(p, b), l)))
        .min_by(|a, b| a.0.total_cmp(&b.0));
    if let Some((t, pin, level)) = first_edge {
        events.push(BenchEvent {
            t_s: t,
            kind: EventKind::FirstEdge,
            mcu_ref: mcu_ref.clone(),
            detail: format!("{pin} went {}", if level { "high" } else { "low" }),
        });
    }
    if let (Some(t), Some(line)) = (
        obs.uart.first_byte_s,
        bench.uarts.first().and_then(|u| u.first_line.as_ref()),
    ) {
        events.push(BenchEvent {
            t_s: t,
            kind: EventKind::FirstUartByte,
            mcu_ref: mcu_ref.clone(),
            detail: format!("first serial output: {line:?}"),
        });
    }
    if let Some(site) = &bench.park {
        let at = site
            .symbol
            .clone()
            .unwrap_or_else(|| format!("0x{:08x}", site.pc));
        events.push(BenchEvent {
            t_s: site.quiet_since_s.unwrap_or(0.0),
            kind: EventKind::Stall,
            mcu_ref: mcu_ref.clone(),
            detail: format!("no output after this; the core is parked at {at}"),
        });
    }
    if bench.core == CoreState::Faulted {
        events.push(BenchEvent {
            t_s: duration_s,
            kind: EventKind::Fault,
            mcu_ref: mcu_ref.clone(),
            detail: format!("{} (by the end of the run)", bench.headline),
        });
    }
    events
}

/// Fill the report. `contexts[i]` describes `obs.mcus[i]`.
#[must_use]
pub fn build_report(obs: &Observer, contexts: &[McuContext]) -> BenchReport {
    let duration_s = obs.end_s;
    let mut mcus = Vec::new();
    let mut events: Vec<BenchEvent> = obs.events.clone();
    let blank = McuObs::default();
    for (i, ctx) in contexts.iter().enumerate() {
        let mobs = obs.mcus.get(i).unwrap_or(&blank);
        // Rows: every pin that moved, plus every configured output.
        let mut keys: BTreeSet<(char, u8)> = mobs.pins.keys().copied().collect();
        keys.extend(ctx.configured_outputs.iter().copied());
        let mut pins: Vec<PinRow> = keys
            .into_iter()
            .map(|(p, b)| {
                pin_row(
                    p,
                    b,
                    mobs.pins.get(&(p, b)),
                    ctx.pin_nets.get(&(p, b)),
                    ctx.configured_outputs.contains(&(p, b)),
                )
            })
            .collect();
        let rank = |s: PinState| match s {
            PinState::Toggling => 0,
            PinState::StaticHigh | PinState::StaticLow => 1,
            PinState::Input => 2,
        };
        pins.sort_by(|a, b| {
            rank(a.state)
                .cmp(&rank(b.state))
                .then(
                    b.freq_hz
                        .unwrap_or(0.0)
                        .total_cmp(&a.freq_hz.unwrap_or(0.0)),
                )
                .then(a.pin.cmp(&b.pin))
        });
        let (core, park, core_words) = core_state(mobs, ctx, duration_s, obs.chunk_s);
        let resets = mobs.reset_pulses + u32::try_from(mobs.watchdog_booked).unwrap_or(u32::MAX);
        let headline = match resets {
            0 => core_words,
            1 => format!("{core_words}; restarted once"),
            n => format!("{core_words}; restarted {n} times"),
        };
        let mut bench = McuBench {
            mcu_ref: ctx.mcu_ref.clone(),
            backend: ctx.backend.clone(),
            core,
            headline,
            park,
            resets,
            first_activity_s: mobs.first_activity_s,
            pins,
            uarts: uart_row(mobs, ctx).into_iter().collect(),
        };
        events.extend(mcu_events(mobs, ctx, &bench, duration_s));
        // Keep the JSON bounded: a board with hundreds of wired pins shows the
        // ones that did something first, and the long tail of inputs last.
        bench.pins.truncate(256);
        mcus.push(bench);
    }
    events.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));

    BenchReport {
        duration_s,
        mcus,
        bus_log: build_bus_log(obs, contexts),
        events,
        truncated: obs.truncated,
    }
}

/// Every bus transaction, plus each serial line as a UART entry, in time order.
fn build_bus_log(obs: &Observer, contexts: &[McuContext]) -> Vec<BusTransaction> {
    let mut bus_log: Vec<BusTransaction> = obs
        .bus
        .iter()
        .map(|t| BusTransaction {
            t_s: t.t_s,
            bus: t.bus,
            device: t.device.clone(),
            addr: t.addr,
            read: t.read,
            bytes: t.bytes.clone(),
            reply: t.reply.clone(),
            acked: t.acked,
            summary: bus_summary(t),
        })
        .collect();
    for (i, ctx) in contexts.iter().enumerate() {
        let Some(mobs) = obs.mcus.get(i) else {
            continue;
        };
        for line in mobs.uart.all_lines() {
            let raw = RawBusTxn {
                t_s: line.t_s,
                bus: BusKind::Uart,
                device: ctx.mcu_ref.clone(),
                addr: None,
                read: false,
                bytes: line.text.clone().into_bytes(),
                reply: Vec::new(),
                acked: true,
            };
            bus_log.push(BusTransaction {
                t_s: raw.t_s,
                bus: raw.bus,
                device: raw.device.clone(),
                addr: None,
                read: false,
                bytes: raw.bytes.clone(),
                reply: Vec::new(),
                acked: true,
                summary: bus_summary(&raw),
            });
        }
    }
    bus_log.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
    bus_log
}

/// The bench block the CLI prints in place of the old top-nets table.
#[must_use]
pub fn render_text(report: &BenchReport) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "bench view: what the board did over {}",
        fmt_duration(report.duration_s)
    );
    for mcu in &report.mcus {
        let _ = writeln!(s, "\n{}  ({})  {}", mcu.mcu_ref, mcu.backend, mcu.headline);
        let shown = mcu
            .pins
            .iter()
            .filter(|p| p.state != PinState::Input)
            .collect::<Vec<_>>();
        if shown.is_empty() {
            let _ = writeln!(s, "  no pin was driven");
        }
        for p in shown {
            let _ = writeln!(
                s,
                "  {:<6} {:<24} {}",
                p.pin,
                p.net.as_deref().unwrap_or("(unwired)"),
                p.summary
            );
        }
        for u in &mcu.uarts {
            let says = match (&u.first_line, u.bytes) {
                (_, 0) => "silent: no bytes sent".to_string(),
                (Some(line), n) => format!(
                    "{line:?} ({n} bytes, {:.0}% text)",
                    u.printable_ratio * 100.0
                ),
                (None, n) => format!("{n} bytes, no complete line"),
            };
            let _ = writeln!(
                s,
                "  {:<6} {:<24} {says}",
                u.port,
                u.net.as_deref().unwrap_or("(net unknown)")
            );
            if let Some(note) = &u.baud_note {
                let _ = writeln!(s, "         ({note})");
            }
        }
        if mcu.resets > 0 {
            let _ = writeln!(s, "  resets: {}", mcu.resets);
        }
    }
    if !report.events.is_empty() {
        let _ = writeln!(s, "\ntimeline:");
        for e in &report.events {
            let _ = writeln!(
                s,
                "  {:>9}  {:<4} {}",
                fmt_duration(e.t_s),
                e.mcu_ref,
                e.detail
            );
        }
    }
    if !report.bus_log.is_empty() {
        let (mut i2c, mut spi, mut uart) = (0, 0, 0);
        for t in &report.bus_log {
            match t.bus {
                BusKind::I2c => i2c += 1,
                BusKind::Spi => spi += 1,
                BusKind::Uart => uart += 1,
            }
        }
        let _ = writeln!(
            s,
            "\nbus: {i2c} I2C, {spi} SPI, {uart} serial line(s); --bus-log lists them, --vcd FILE draws them"
        );
    }
    if report.truncated {
        let _ = writeln!(
            s,
            "\nnote: the recorder hit its memory cap; counts are exact, the waveform and log are cut short"
        );
    }
    s
}

/// The full bus log, one transaction per line.
#[must_use]
pub fn render_bus_log(report: &BenchReport) -> String {
    let mut s = String::new();
    if report.bus_log.is_empty() {
        s.push_str("bus log: no I2C, SPI or serial transactions were recorded\n");
        return s;
    }
    let _ = writeln!(s, "bus log ({} entries):", report.bus_log.len());
    for t in &report.bus_log {
        let _ = writeln!(
            s,
            "  {:>9}  {:<4} {}",
            fmt_duration(t.t_s),
            t.bus.short(),
            t.summary
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_bind::digital::PinEdge;

    fn ctx(evidence: Option<ExecutionEvidence>) -> McuContext {
        McuContext {
            mcu_ref: "U1".into(),
            backend: "renode:stm32f103".into(),
            pin_nets: HashMap::from([(('C', 13), "PC13_LED".to_string())]),
            uart_net: Some("PA9_TX".into()),
            configured_outputs: BTreeSet::from([('A', 5)]),
            evidence,
            symbols: None,
        }
    }

    fn evidence(pc_samples: u64, distinct: u64, pc: u32) -> ExecutionEvidence {
        let probe = hauksbee_mcu::ExecutionProbe {
            reached_entry: Some(true),
            entry_symbol: Some("main".into()),
            pc_samples,
            distinct_pcs: distinct,
            last_pc: Some(pc),
            ..hauksbee_mcu::ExecutionProbe::default()
        };
        let board = crate::execution::BoardReadings {
            reset_held: crate::execution::Observed::No,
            supply_valid: crate::execution::Observed::Yes,
            supply_v: Some(3.3),
            clock: hauksbee_bind::mcu_clock::McuClock::default(),
        };
        ExecutionEvidence::new(
            "U1",
            "renode:stm32f103",
            probe,
            0,
            &std::collections::BTreeMap::new(),
            board,
        )
    }

    fn blinking() -> Observer {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        for k in 0..6_u32 {
            let t0 = f64::from(k) * 0.1;
            let edges = [PinEdge {
                cycle: 10,
                port: 'C',
                bit: 13,
                level: k % 2 == 0,
            }];
            obs.record_chunk(
                0,
                t0,
                0.1,
                (0, 100),
                &edges,
                if k == 1 { b"hi\n" } else { b"" },
            );
        }
        obs
    }

    #[test]
    fn a_blinking_pin_and_a_silent_output_get_rows() {
        let report = build_report(&blinking(), &[ctx(Some(evidence(40, 12, 0x0800_0100)))]);
        let mcu = &report.mcus[0];
        let led = mcu.pins.iter().find(|p| p.pin == "PC13").unwrap();
        assert_eq!(led.state, PinState::Toggling);
        assert_eq!(led.net.as_deref(), Some("PC13_LED"));
        assert!(led.freq_hz.unwrap() > 4.0, "{:?}", led.freq_hz);
        assert!(led.summary.starts_with("toggling"));
        let pa5 = mcu.pins.iter().find(|p| p.pin == "PA5").unwrap();
        assert_eq!(pa5.state, PinState::StaticLow);
        assert_eq!(mcu.core, CoreState::Running);
        assert_eq!(mcu.uarts[0].first_line.as_deref(), Some("hi"));
        assert!(report
            .events
            .iter()
            .any(|e| e.kind == EventKind::FirstUartByte));
        assert!(report.events.iter().any(|e| e.kind == EventKind::FirstEdge));
    }

    #[test]
    fn a_core_looping_in_error_handler_is_trapped() {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        obs.record_chunk(0, 0.0, 1.0, (0, 100), &[], &[]);
        let mut c = ctx(Some(evidence(64, 1, 0x0800_0202)));
        c.symbols = SymbolTable::from_image(&crate::observe::symbols::test_image(&[(
            "Error_Handler",
            0x0800_0201,
            8,
        )]));
        let report = build_report(&obs, &[c]);
        let mcu = &report.mcus[0];
        assert_eq!(mcu.core, CoreState::Trapped);
        assert_eq!(
            mcu.park.as_ref().and_then(|p| p.symbol.as_deref()),
            Some("Error_Handler")
        );
        assert!(report.events.iter().any(|e| e.kind == EventKind::Stall));
    }

    #[test]
    fn a_tight_loop_after_activity_is_parked_but_not_while_still_toggling() {
        // Toggled for the first 0.1 s of 1 s, then silence in a 1-PC loop.
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        let e = [PinEdge {
            cycle: 1,
            port: 'C',
            bit: 13,
            level: true,
        }];
        obs.record_chunk(0, 0.0, 0.1, (0, 10), &e, &[]);
        obs.record_chunk(0, 0.1, 0.9, (10, 20), &[], &[]);
        let parked = build_report(&obs, &[ctx(Some(evidence(64, 1, 0x0800_0300)))]);
        assert_eq!(parked.mcus[0].core, CoreState::Parked);
        assert!(parked.mcus[0].headline.starts_with("parked at 0x08000300"));

        // The same loop but the pin was still moving at the end: running.
        let mut busy = Observer::default();
        busy.ensure_mcu(0, "U1");
        for k in 0..10_u32 {
            let ev = [PinEdge {
                cycle: 1,
                port: 'C',
                bit: 13,
                level: k % 2 == 0,
            }];
            busy.record_chunk(0, f64::from(k) * 0.1, 0.1, (0, 10), &ev, &[]);
        }
        let running = build_report(&busy, &[ctx(Some(evidence(64, 1, 0x0800_0300)))]);
        assert_eq!(running.mcus[0].core, CoreState::Running);
    }

    #[test]
    fn bus_log_lines_read_like_an_analyser() {
        let t = RawBusTxn {
            t_s: 0.01,
            bus: BusKind::I2c,
            device: "U2".into(),
            addr: Some(0x48),
            read: true,
            bytes: vec![0x1a, 0x80],
            reply: vec![],
            acked: true,
        };
        assert_eq!(bus_summary(&t), "U2 0x48 read [0x1a 0x80] ACK");
        let nack = RawBusTxn {
            acked: false,
            bytes: vec![],
            ..t
        };
        assert!(bus_summary(&nack).ends_with("NACK (no device answered)"));
    }

    #[test]
    fn text_block_names_pins_nets_and_serial() {
        let report = build_report(&blinking(), &[ctx(Some(evidence(40, 12, 0x0800_0100)))]);
        let text = render_text(&report);
        assert!(text.contains("PC13"), "{text}");
        assert!(text.contains("PC13_LED"), "{text}");
        assert!(text.contains("toggling"), "{text}");
        assert!(text.contains("\"hi\""), "{text}");
        assert!(text.contains("timeline:"), "{text}");
    }
}
