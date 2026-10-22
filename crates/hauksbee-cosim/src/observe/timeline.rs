//! The run as a waveform file: `hauksbee run --vcd run.vcd`.
//!
//! One VCD, openable in gtkwave, Surfer or `PulseView`, that carries what a logic
//! analyser would show for the whole board:
//!
//! * a scope per MCU with one wire per pin that moved (named `PC13:LED_NET`, so
//!   the pin and the board net it drives are on the same row), unknown until
//!   the pin's first edge;
//! * a `uart` string track per MCU: each line the firmware printed, at the time
//!   it arrived;
//! * a `bus` scope with a decoded string track per I2C/SPI bridge (the
//!   transaction in words), an address vector and a NACK wire for I2C;
//! * an `events` scope: a `timeline` string track and 1-bit markers for resets,
//!   watchdog bites, stalls and faults.
//!
//! Edge times follow the same honesty as the bench view: exact on simavr,
//! chunk resolution on Renode and QEMU. The decoded tracks come from the bus
//! bridges' own events, not from re-sampling the pins, so they are as exact as
//! the controller model.

use hauksbee_ir::observations::{BenchReport, BusKind, EventKind};

use super::bench::{pin_name, McuContext};
use super::vcd::{seconds_to_ps, Value, VarId, VarKind, VcdWriter};
use super::Observer;

/// Shortest width of a decoded-track pulse, in picoseconds (1 us).
const MIN_PULSE_PS: u64 = 1_000_000;
/// Gap kept between back-to-back entries on one text track, in picoseconds.
const TEXT_GAP_PS: u64 = 1_000;

/// A text track that never overlaps itself: entries that share a timestamp
/// are laid end to end.
struct TextTrack {
    var: VarId,
    free_at_ps: u64,
}

impl TextTrack {
    const fn new(var: VarId) -> Self {
        Self { var, free_at_ps: 0 }
    }

    /// Show `text` from `t_ps` for `width_ps`, then clear the track.
    fn show(&mut self, w: &mut VcdWriter, t_ps: u64, width_ps: u64, text: &str) {
        let start = t_ps.max(self.free_at_ps);
        let end = start + width_ps.max(MIN_PULSE_PS);
        w.change(start, self.var, Value::Text(text.to_string()));
        w.change(end, self.var, Value::Text(String::new()));
        self.free_at_ps = end + TEXT_GAP_PS;
    }
}

/// The decoded tracks of one bus bridge.
struct BusTrack {
    device: String,
    bus: BusKind,
    text: TextTrack,
    addr: Option<VarId>,
    nack: Option<VarId>,
}

/// Decoded bus tracks, one set per bridge.
fn write_bus_tracks(w: &mut VcdWriter, obs: &Observer, chunk_ps: u64) {
    if !obs.bus.is_empty() {
        w.begin_scope("bus");
        let mut tracks: Vec<BusTrack> = Vec::new();
        for txn in &obs.bus {
            let slot = tracks
                .iter()
                .position(|t| t.device == txn.device && t.bus == txn.bus);
            let slot = slot.unwrap_or_else(|| {
                let label = format!("{}_{}", txn.device, txn.bus.short());
                let text = TextTrack::new(w.var(VarKind::Text, &label));
                let (addr, nack) = if txn.bus == BusKind::I2c {
                    (
                        Some(w.var(VarKind::Vector(8), &format!("{label}_addr"))),
                        Some(w.var(VarKind::Bit, &format!("{label}_nack"))),
                    )
                } else {
                    (None, None)
                };
                tracks.push(BusTrack {
                    device: txn.device.clone(),
                    bus: txn.bus,
                    text,
                    addr,
                    nack,
                });
                tracks.len() - 1
            });
            let BusTrack {
                text, addr, nack, ..
            } = &mut tracks[slot];
            let t_ps = seconds_to_ps(txn.t_s);
            let summary = super::bench::bus_summary(txn);
            let start = t_ps.max(text.free_at_ps);
            text.show(w, t_ps, chunk_ps / 4, &summary);
            if let (Some(addr), Some(a)) = (*addr, txn.addr) {
                w.change(start, addr, Value::Vector(u64::from(a)));
            }
            if let Some(nack) = *nack {
                w.change(start, nack, Value::Bit(!txn.acked));
            }
        }
        w.end_scope();
    }
}

/// Render the whole bench view as a VCD document.
#[must_use]
pub fn render_vcd(obs: &Observer, contexts: &[McuContext], report: &BenchReport) -> String {
    let mut w = VcdWriter::new("hauksbee-bench");
    let chunk_ps = seconds_to_ps(obs.chunk_s).max(MIN_PULSE_PS);
    let end_ps = seconds_to_ps(obs.end_s);

    // Per-MCU pins and serial lines.
    for (i, ctx) in contexts.iter().enumerate() {
        let Some(mobs) = obs.mcus.get(i) else {
            continue;
        };
        w.begin_scope(&ctx.mcu_ref);
        for (&(port, bit), trace) in &mobs.pins {
            let pin = pin_name(port, bit);
            let name = ctx
                .pin_nets
                .get(&(port, bit))
                .map_or_else(|| pin.clone(), |net| format!("{pin}:{net}"));
            let var = w.var(VarKind::Bit, &name);
            for &(t, level) in &trace.edges {
                w.change(seconds_to_ps(t), var, Value::Bit(level));
            }
        }
        let lines = mobs.uart.all_lines();
        if !lines.is_empty() {
            w.begin_scope("uart");
            let name = ctx.uart_net.clone().unwrap_or_else(|| "TX".into());
            let mut track = TextTrack::new(w.var(VarKind::Text, &name));
            for line in &lines {
                track.show(&mut w, seconds_to_ps(line.t_s), chunk_ps, &line.text);
            }
            w.end_scope();
        }
        w.end_scope();
    }

    write_bus_tracks(&mut w, obs, chunk_ps);

    // Event markers.
    if !report.events.is_empty() {
        w.begin_scope("events");
        let mut timeline = TextTrack::new(w.var(VarKind::Text, "timeline"));
        let reset = w.var(VarKind::Bit, "reset");
        let watchdog = w.var(VarKind::Bit, "watchdog_reset");
        let stall = w.var(VarKind::Bit, "stall");
        let fault = w.var(VarKind::Bit, "fault");
        for v in [reset, watchdog, stall, fault] {
            w.change(0, v, Value::Bit(false));
        }
        for ev in &report.events {
            let t_ps = seconds_to_ps(ev.t_s);
            timeline.show(
                &mut w,
                t_ps,
                chunk_ps,
                &format!("{} {}: {}", ev.mcu_ref, ev.kind.id(), ev.detail),
            );
            match ev.kind {
                EventKind::Reset => pulse(&mut w, reset, t_ps, chunk_ps),
                EventKind::WatchdogReset => pulse(&mut w, watchdog, t_ps, chunk_ps),
                EventKind::Fault => pulse(&mut w, fault, t_ps, chunk_ps),
                // A stall stays asserted to the end of the run.
                EventKind::Stall => {
                    w.change(t_ps, stall, Value::Bit(true));
                    w.change(end_ps.max(t_ps + 1), stall, Value::Bit(false));
                }
                EventKind::FirstUartByte | EventKind::FirstEdge => {}
            }
        }
        w.end_scope();
    }
    w.render()
}

fn pulse(w: &mut VcdWriter, var: VarId, t_ps: u64, width_ps: u64) {
    w.change(t_ps, var, Value::Bit(true));
    w.change(t_ps + width_ps, var, Value::Bit(false));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::bench::build_report;
    use crate::observe::RawBusTxn;
    use hauksbee_bind::digital::PinEdge;
    use std::collections::HashMap;

    fn scenario() -> (Observer, Vec<McuContext>) {
        let mut obs = Observer::default();
        obs.ensure_mcu(0, "U1");
        for k in 0..6_u32 {
            let edges = [PinEdge {
                cycle: 50,
                port: 'C',
                bit: 13,
                level: k % 2 == 0,
            }];
            let uart: &[u8] = if k == 2 { b"boot ok\n" } else { b"" };
            obs.record_chunk(0, f64::from(k) * 0.1, 0.1, (0, 100), &edges, uart);
        }
        obs.push_bus(vec![
            RawBusTxn {
                t_s: 0.25,
                bus: BusKind::I2c,
                device: "U2".into(),
                addr: Some(0x48),
                read: false,
                bytes: vec![0x00],
                reply: vec![],
                acked: true,
            },
            RawBusTxn {
                t_s: 0.25,
                bus: BusKind::I2c,
                device: "U2".into(),
                addr: Some(0x48),
                read: true,
                bytes: vec![0x19, 0x80],
                reply: vec![],
                acked: true,
            },
            RawBusTxn {
                t_s: 0.35,
                bus: BusKind::I2c,
                device: "U2".into(),
                addr: Some(0x49),
                read: false,
                bytes: vec![],
                reply: vec![],
                acked: false,
            },
        ]);
        let ctx = McuContext {
            mcu_ref: "U1".into(),
            backend: "test".into(),
            pin_nets: HashMap::from([(('C', 13), "PC13_LED".to_string())]),
            uart_net: Some("PA9_TX".into()),
            ..McuContext::default()
        };
        (obs, vec![ctx])
    }

    #[test]
    fn vcd_round_trips_through_an_independent_parser() {
        let (obs, ctxs) = scenario();
        let report = build_report(&obs, &ctxs);
        let text = render_vcd(&obs, &ctxs, &report);

        let mut parser = vcd::Parser::new(text.as_bytes());
        let header = parser.parse_header().expect("header parses");
        let pin = header
            .find_var(&["U1", "PC13:PC13_LED"])
            .expect("the pin is declared as pin:net");
        let uart = header
            .find_var(&["U1", "uart", "PA9_TX"])
            .expect("uart track");
        let bus = header.find_var(&["bus", "U2_I2C"]).expect("i2c track");
        let nack = header.find_var(&["bus", "U2_I2C_nack"]).expect("nack wire");
        let addr = header
            .find_var(&["bus", "U2_I2C_addr"])
            .expect("addr vector");
        assert_eq!(addr.size, 8);
        assert!(
            header.find_var(&["events", "timeline"]).is_some(),
            "first-edge and first-byte markers put a timeline track in the file"
        );

        let (pin_code, uart_code, bus_code, nack_code, addr_code) =
            (pin.code, uart.code, bus.code, nack.code, addr.code);
        let mut now = 0_u64;
        let mut pin_changes: Vec<(u64, bool)> = Vec::new();
        let mut uart_lines = Vec::new();
        let mut bus_lines = Vec::new();
        let mut nacks = Vec::new();
        let mut addrs = Vec::new();
        for cmd in parser {
            match cmd.expect("every command parses") {
                vcd::Command::Timestamp(t) => now = t,
                vcd::Command::ChangeScalar(c, v) if c == pin_code => {
                    pin_changes.push((now, v == vcd::Value::V1));
                }
                vcd::Command::ChangeScalar(c, v) if c == nack_code => {
                    nacks.push((now, v == vcd::Value::V1));
                }
                vcd::Command::ChangeString(c, s) if c == uart_code && s != "-" => {
                    uart_lines.push((now, s));
                }
                vcd::Command::ChangeString(c, s) if c == bus_code && s != "-" => {
                    bus_lines.push((now, s));
                }
                vcd::Command::ChangeVector(c, v) if c == addr_code && now > 0 => addrs.push(v),
                _ => {}
            }
        }
        // Unknown until the first edge (t=0 dump), then six edges at 0.05,
        // 0.15, ... 0.55 s alternating high/low.
        assert_eq!(pin_changes[0], (0, false), "x is not V1");
        pin_changes.remove(0);
        assert_eq!(pin_changes.len(), 6, "{pin_changes:?}");
        assert_eq!(pin_changes[0], (50_000_000_000, true));
        assert_eq!(pin_changes[1], (150_000_000_000, false));
        assert_eq!(uart_lines.len(), 1);
        assert_eq!(uart_lines[0].1, "boot_ok");
        assert_eq!(uart_lines[0].0, 200_000_000_000);
        assert_eq!(bus_lines.len(), 3, "{bus_lines:?}");
        assert!(bus_lines[0].1.starts_with("U2_0x48_write_[0x00]_ACK"));
        // Two transactions share t=0.25 s: laid end to end, not overwritten.
        assert!(bus_lines[1].0 > bus_lines[0].0);
        assert!(bus_lines[2].1.contains("NACK"));
        assert!(nacks.contains(&(bus_lines[2].0, true)));
        assert_eq!(addrs.len(), 3);
    }

    #[test]
    fn events_become_markers() {
        let (mut obs, ctxs) = scenario();
        obs.record_reset_level(0, 0.0, Some(3.3), 3.3);
        obs.record_reset_level(0, 0.2, Some(0.0), 3.3);
        obs.record_reset_level(0, 0.3, Some(3.3), 3.3);
        let report = build_report(&obs, &ctxs);
        let text = render_vcd(&obs, &ctxs, &report);
        let mut parser = vcd::Parser::new(text.as_bytes());
        let header = parser.parse_header().unwrap();
        let reset = header.find_var(&["events", "reset"]).expect("reset marker");
        let mut highs = 0;
        for cmd in parser {
            if let vcd::Command::ChangeScalar(c, vcd::Value::V1) = cmd.unwrap() {
                if c == reset.code {
                    highs += 1;
                }
            }
        }
        assert_eq!(highs, 1);
    }
}
