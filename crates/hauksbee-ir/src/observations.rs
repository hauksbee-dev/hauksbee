//! What a co-sim run looked like from the bench: the words and the data shapes.
//!
//! A bench engineer looks at a board with a scope, a terminal and a logic
//! analyser. This module is the one place the run's *observations* are named:
//!
//! * the **wire data** ([`BenchReport`] and its parts), produced by the co-sim
//!   observer and carried by `hauksbee run --json` (`cosim.bench`), the web
//!   report (`cosim.bench`) and the VCD and bus-log writers;
//! * the **vocabulary** ([`observation_kinds`]): one row per pin state, core
//!   state, event kind, bus kind and probe-seed reason, each with an `id`, a
//!   display label, a one-line meaning and a docs anchor. The rows are
//!   registered in [`crate::labels`] (so the app's generated `labels.ts` carries
//!   them) and published through [`crate::vocabulary::vocabulary`], under the
//!   existing drift tests;
//! * the curated **trap symbols** ([`TRAP_SYMBOLS`]): function names that mean
//!   "the firmware gave up here".
//!
//! Every surface (CLI bench block, JSON, app strip, docs) asks this module for
//! a word. A renderer never spells its own.

use serde::{Deserialize, Serialize};

/// The `group` observation rows carry in the label table.
pub const GROUP_OBSERVATION: &str = "observation";

/// Edge samples kept per pin in a [`BenchReport`] (the VCD keeps more).
pub const MAX_EDGE_SAMPLES: usize = 400;

/// Function names that mean the firmware gave up: a `CubeMX` error trap, a
/// default or fault handler, an assert or panic. A core parked in one of
/// these is "trapped", not merely idle.
pub const TRAP_SYMBOLS: &[&str] = &[
    "Error_Handler",
    "Default_Handler",
    "HardFault_Handler",
    "MemManage_Handler",
    "BusFault_Handler",
    "UsageFault_Handler",
    "NMI_Handler",
    "abort",
    "_exit",
    "__assert_func",
    "__assert_fail",
    "panic",
    "rust_begin_unwind",
    "core::panicking::panic",
    "__stack_chk_fail",
];

/// Whether a symbol names a trap (case-sensitive exact match or a `panic*`
/// prefix).
#[must_use]
pub fn is_trap_symbol(symbol: &str) -> bool {
    TRAP_SYMBOLS.contains(&symbol) || symbol.starts_with("panic") || symbol.contains("::panic")
}

// ── The wire data ───────────────────────────────────────────────────

/// What one MCU pin did over the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum PinState {
    /// Driven high, no transitions after the first.
    #[serde(rename = "pin_static_high")]
    StaticHigh,
    /// Driven low, no transitions after the first.
    #[serde(rename = "pin_static_low")]
    StaticLow,
    /// Driven, changing level.
    #[serde(rename = "pin_toggling")]
    Toggling,
    /// Never driven by the firmware: an input or released (Hi-Z).
    #[serde(rename = "pin_input")]
    Input,
}

impl PinState {
    /// The vocabulary id (the wire word).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::StaticHigh => "pin_static_high",
            Self::StaticLow => "pin_static_low",
            Self::Toggling => "pin_toggling",
            Self::Input => "pin_input",
        }
    }
}

/// Where the core ended the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CoreState {
    /// Still doing things at the end of the run.
    #[serde(rename = "core_running")]
    Running,
    /// Sitting in a tight loop with no output since the loop began.
    #[serde(rename = "core_parked")]
    Parked,
    /// Parked in a curated trap symbol ([`TRAP_SYMBOLS`]).
    #[serde(rename = "core_trapped")]
    Trapped,
    /// A fault handler was entered or the core crashed.
    #[serde(rename = "core_faulted")]
    Faulted,
    /// The backend cannot say.
    #[serde(rename = "core_unknown")]
    Unknown,
}

impl CoreState {
    /// The vocabulary id (the wire word).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Running => "core_running",
            Self::Parked => "core_parked",
            Self::Trapped => "core_trapped",
            Self::Faulted => "core_faulted",
            Self::Unknown => "core_unknown",
        }
    }
}

/// One level change of a pin: seconds of simulated time and the new level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EdgeSample {
    pub t_s: f64,
    pub high: bool,
}

/// One row of the bench view: a pin the board wires to a net.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PinRow {
    /// The pin as the firmware names it (`PC13`, `PB5`).
    pub pin: String,
    /// The board net it drives, when it is wired to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
    pub state: PinState,
    /// Transitions seen.
    pub edges: u64,
    /// Toggle rate (full cycles per second), for a toggling pin with at least
    /// three edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_hz: Option<f64>,
    /// Fraction of the observed time spent high, for a toggling pin (0..1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<f64>,
    /// Simulated time of the first edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_edge_s: Option<f64>,
    /// Level at the end of the run, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_high: Option<bool>,
    /// The row in words: `toggling 5.0 Hz, 50% duty`.
    pub summary: String,
    /// The first [`MAX_EDGE_SAMPLES`] edges, for the app's lanes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_samples: Vec<EdgeSample>,
    /// True when `edge_samples` is shorter than `edges`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub samples_truncated: bool,
}

/// One serial port's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UartRow {
    /// Always `UART` today; a port name when the backend names ports.
    pub port: String,
    /// The board net the TX pin drives, when the binder knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
    /// The baud rate the firmware configured. `None` until the peripheral
    /// snapshot decodes it; `baud_note` says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud_note: Option<String>,
    pub bytes: u64,
    /// Fraction of bytes that are printable text or whitespace (0..1).
    pub printable_ratio: f64,
    /// The first line the firmware printed, trimmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_line: Option<String>,
    /// Simulated time the first byte arrived (chunk resolution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_byte_s: Option<f64>,
}

/// Where a parked core sits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParkSite {
    pub pc: u32,
    /// The ELF function containing `pc`, when the image has a symbol table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Program-counter samples taken and how many distinct values they held.
    pub pc_samples: u64,
    pub distinct_pcs: u64,
    /// Simulated time of the last output (pin edge or UART byte); the core has
    /// produced none since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_since_s: Option<f64>,
}

/// The bench view of one MCU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McuBench {
    pub mcu_ref: String,
    pub backend: String,
    pub core: CoreState,
    /// One line: `running`, `parked at Error_Handler`, `restarted 2 times`.
    pub headline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<ParkSite>,
    /// Resets seen (reset-line pulses and watchdog bites).
    pub resets: u32,
    /// Simulated time of the first pin edge or UART byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_s: Option<f64>,
    pub pins: Vec<PinRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uarts: Vec<UartRow>,
}

/// Which bus a transaction crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum BusKind {
    #[serde(rename = "bus_i2c")]
    I2c,
    #[serde(rename = "bus_spi")]
    Spi,
    #[serde(rename = "bus_uart")]
    Uart,
}

impl BusKind {
    /// The vocabulary id (the wire word).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::I2c => "bus_i2c",
            Self::Spi => "bus_spi",
            Self::Uart => "bus_uart",
        }
    }

    /// The short name a log line prints.
    #[must_use]
    pub const fn short(self) -> &'static str {
        match self {
            Self::I2c => "I2C",
            Self::Spi => "SPI",
            Self::Uart => "UART",
        }
    }
}

/// One decoded bus transaction, as a logic analyser's protocol decoder
/// shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BusTransaction {
    /// Simulated time the transaction started (chunk resolution on poll
    /// backends).
    pub t_s: f64,
    pub bus: BusKind,
    /// The board part (or spec peripheral id) that answered, or that was
    /// addressed and did not.
    pub device: String,
    /// I2C 7-bit address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<u8>,
    /// True when the firmware read from the device.
    pub read: bool,
    /// I2C: the bytes written or read. SPI: the bytes on MOSI. UART: the line.
    pub bytes: Vec<u8>,
    /// SPI only: the bytes the device returned on MISO.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply: Vec<u8>,
    /// I2C: whether the address byte was acknowledged. SPI: whether the device
    /// was powered and answered.
    pub acked: bool,
    /// The transaction in words: `LM75 0x48 write [0x00] ACK`.
    pub summary: String,
}

/// What kind of moment a timeline marker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EventKind {
    #[serde(rename = "event_reset")]
    Reset,
    #[serde(rename = "event_watchdog_reset")]
    WatchdogReset,
    #[serde(rename = "event_stall")]
    Stall,
    #[serde(rename = "event_first_uart_byte")]
    FirstUartByte,
    #[serde(rename = "event_first_edge")]
    FirstEdge,
    #[serde(rename = "event_fault")]
    Fault,
}

impl EventKind {
    /// The vocabulary id (the wire word).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Reset => "event_reset",
            Self::WatchdogReset => "event_watchdog_reset",
            Self::Stall => "event_stall",
            Self::FirstUartByte => "event_first_uart_byte",
            Self::FirstEdge => "event_first_edge",
            Self::Fault => "event_fault",
        }
    }
}

/// A dashed marker on the timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BenchEvent {
    pub t_s: f64,
    pub kind: EventKind,
    pub mcu_ref: String,
    /// One line: `reset line pulsed`, `first byte: hello`.
    pub detail: String,
}

/// The whole bench view of a run: what the pins, the serial port, the buses
/// and the reset line did, in time order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BenchReport {
    /// Simulated seconds the run covered.
    pub duration_s: f64,
    pub mcus: Vec<McuBench>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bus_log: Vec<BusTransaction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<BenchEvent>,
    /// True when the recorder hit its memory cap: counts stay exact, the
    /// waveform and log are cut short.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

// ── The vocabulary ──────────────────────────────────────────────────

/// One observation word: the shape every domain in [`crate::vocabulary`]
/// shares, plus where the docs explain it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservationEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub meaning: &'static str,
    /// Anchor in `docs/cosim/OBSERVATIONS.md`.
    pub docs_anchor: &'static str,
}

const fn row(
    id: &'static str,
    label: &'static str,
    meaning: &'static str,
    docs_anchor: &'static str,
) -> ObservationEntry {
    ObservationEntry {
        id,
        label,
        meaning,
        docs_anchor,
    }
}

/// The table behind [`observation_kinds`].
const KINDS: &[ObservationEntry] = &[
        row(
            "pin_static_high",
            "Static high",
            "The firmware drove this pin high and left it there.",
            "pin-states",
        ),
        row(
            "pin_static_low",
            "Static low",
            "The firmware drove this pin low and left it there.",
            "pin-states",
        ),
        row(
            "pin_toggling",
            "Toggling",
            "The firmware drives this pin and it changes level: the row gives the rate and duty.",
            "pin-states",
        ),
        row(
            "pin_input",
            "Input or Hi-Z",
            "The firmware never drove this pin: it is an input or released.",
            "pin-states",
        ),
        row(
            "core_running",
            "Running",
            "The core was still producing output at the end of the run.",
            "core-state",
        ),
        row(
            "core_parked",
            "Parked",
            "The core sat in a tight loop with no pin or serial output since the loop began.",
            "core-state",
        ),
        row(
            "core_trapped",
            "Trapped",
            "The core is parked in a function that means the firmware gave up (an error handler, abort or panic).",
            "core-state",
        ),
        row(
            "core_faulted",
            "Faulted",
            "A fault handler was entered or the core crashed.",
            "core-state",
        ),
        row(
            "core_unknown",
            "Core state unknown",
            "This backend cannot say where the core is.",
            "core-state",
        ),
        row(
            "bus_i2c",
            "I2C",
            "A transaction on an I2C bus, decoded from the controller.",
            "bus-log",
        ),
        row(
            "bus_spi",
            "SPI",
            "A chip-select-framed transfer on an SPI bus, decoded from the controller.",
            "bus-log",
        ),
        row(
            "bus_uart",
            "UART",
            "A line of serial output.",
            "bus-log",
        ),
        row(
            "event_reset",
            "Reset",
            "The board's reset line was pulsed and released.",
            "timeline",
        ),
        row(
            "event_watchdog_reset",
            "Watchdog reset",
            "An unserviced watchdog rebooted the core.",
            "timeline",
        ),
        row(
            "event_stall",
            "Stall",
            "The core stopped producing output and stayed quiet.",
            "timeline",
        ),
        row(
            "event_first_uart_byte",
            "First serial byte",
            "The first byte the firmware sent on its serial port.",
            "timeline",
        ),
        row(
            "event_first_edge",
            "First pin edge",
            "The first time the firmware changed an output pin.",
            "timeline",
        ),
        row(
            "event_fault",
            "Fault",
            "A fault handler was entered.",
            "timeline",
        ),
        row(
            "probe_seed_led",
            "an indicator LED",
            "An LED or indicator net is the first thing a person clips a probe to.",
            "seeded-probes",
        ),
        row(
            "probe_seed_uart_tx",
            "the serial transmit line",
            "The UART TX net shows the firmware's serial output as a waveform.",
            "seeded-probes",
        ),
        row(
            "probe_seed_bus",
            "a bus line",
            "An I2C or SPI line: the decoded transactions hang under it.",
            "seeded-probes",
        ),
        row(
            "probe_seed_pwm",
            "a pin that toggles",
            "The firmware drives this pin at a steady rate.",
            "seeded-probes",
        ),
        row(
            "probe_seed_reset",
            "the reset line",
            "The reset line shows when the board restarts the part.",
            "seeded-probes",
        ),
];

/// Every observation id, in display order.
#[must_use]
pub fn observation_kinds() -> Vec<ObservationEntry> {
    KINDS.to_vec()
}

/// The row for an id, if it is an observation id.
#[must_use]
pub fn observation_kind(id: &str) -> Option<ObservationEntry> {
    observation_kinds().into_iter().find(|e| e.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wire_id_is_in_the_vocabulary() {
        let ids: Vec<&str> = observation_kinds().iter().map(|e| e.id).collect();
        for id in [
            PinState::StaticHigh.id(),
            PinState::StaticLow.id(),
            PinState::Toggling.id(),
            PinState::Input.id(),
            CoreState::Running.id(),
            CoreState::Parked.id(),
            CoreState::Trapped.id(),
            CoreState::Faulted.id(),
            CoreState::Unknown.id(),
            BusKind::I2c.id(),
            BusKind::Spi.id(),
            BusKind::Uart.id(),
            EventKind::Reset.id(),
            EventKind::WatchdogReset.id(),
            EventKind::Stall.id(),
            EventKind::FirstUartByte.id(),
            EventKind::FirstEdge.id(),
            EventKind::Fault.id(),
        ] {
            assert!(ids.contains(&id), "{id} has no vocabulary row");
        }
    }

    #[test]
    fn serde_names_equal_the_vocabulary_ids() {
        let pin = serde_json::to_value(PinState::Toggling).unwrap();
        assert_eq!(pin, PinState::Toggling.id());
        let core = serde_json::to_value(CoreState::Trapped).unwrap();
        assert_eq!(core, CoreState::Trapped.id());
        let bus = serde_json::to_value(BusKind::Spi).unwrap();
        assert_eq!(bus, BusKind::Spi.id());
        let ev = serde_json::to_value(EventKind::FirstUartByte).unwrap();
        assert_eq!(ev, EventKind::FirstUartByte.id());
    }

    #[test]
    fn ids_are_unique_and_rows_are_complete() {
        let rows = observation_kinds();
        let mut seen = std::collections::HashSet::new();
        for r in &rows {
            assert!(seen.insert(r.id), "duplicate id {}", r.id);
            assert!(!r.label.is_empty() && !r.meaning.is_empty() && !r.docs_anchor.is_empty());
        }
    }

    #[test]
    fn trap_symbols_match_by_name_and_panic_prefix() {
        assert!(is_trap_symbol("Error_Handler"));
        assert!(is_trap_symbol("panic_halt"));
        assert!(is_trap_symbol("core::panicking::panic_fmt"));
        assert!(!is_trap_symbol("main"));
        assert!(!is_trap_symbol("delay_ms"));
    }
}
