//! Execution evidence: did the firmware actually run on each co-simulated MCU?
//!
//! A co-sim run can look healthy while the firmware never executed: the image
//! did not match the core, the core sat in a fault handler, the board held the
//! part in reset, or its supply was never up. Net toggles alone cannot tell
//! these apart from a quiet but working firmware, so every run carries one
//! [`ExecutionEvidence`] row per MCU and a run with no evidence of execution
//! never counts as a firmware pass.
//!
//! The row combines what the backend saw inside the core
//! ([`hauksbee_mcu::ExecutionProbe`]: entry symbol reached, instructions
//! retired, PC samples, fault handlers entered) with what the scheduler saw at
//! the pins (UART bytes, GPIO toggles per pin) and on the board (reset line,
//! supply pads, clock source). Each board-side fact is tri-state
//! ([`Observed`]): "unknown" is a value of its own and never reads as a pass or
//! a failure.

use std::collections::{BTreeMap, HashMap};

use hauksbee_bind::mcu_clock::{ClockKind, McuClock};
use hauksbee_mcu::ExecutionProbe;

use crate::observe::symbols::SymbolTable;

/// Samples a parked-loop verdict needs before it is trusted.
pub const PARK_MIN_SAMPLES: u64 = 16;
/// A loop this tight (distinct program counters) is a park, not a program.
pub const PARK_MAX_DISTINCT_PCS: u64 = 3;
/// Lower-case fragments of function names that wait for a clock or oscillator
/// (`SystemInit`, `HAL_RCC_OscConfig`, `SystemClock_Config`, an RP2040 `xosc`).
const CLOCK_WAIT_MARKS: [&str; 7] = [
    "clock",
    "oscconfig",
    "systeminit",
    "rcc",
    "pll",
    "hserdy",
    "xosc",
];

/// The sentence every surface (CLI text, `--plain`, `--json` notes, stderr,
/// the web front door) uses when a run shows no evidence the firmware ran.
pub const NO_EXECUTION_MESSAGE: &str = "co-sim saw no evidence the firmware executed (entry \
     symbol not reached, no firmware GPIO drive, no UART output); this result cannot vouch \
     for firmware behaviour";

/// What a quiet run says instead when the board carries no processor and no
/// firmware was supplied.
///
/// There is no firmware to have executed, so the run says nothing about
/// firmware execution beyond this neutral line.
pub const NO_PROCESSOR_MESSAGE: &str = "no processor on this board: no firmware was run";

/// A fact the run either observed, observed false, or could not observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Observed {
    Yes,
    No,
    Unknown,
}

impl Observed {
    /// `Some(true)` is yes, `Some(false)` no, `None` unknown.
    #[must_use]
    pub const fn from_option(value: Option<bool>) -> Self {
        match value {
            Some(true) => Self::Yes,
            Some(false) => Self::No,
            None => Self::Unknown,
        }
    }

    /// The word every surface prints for this value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

/// Where in start-up a core stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StallPhase {
    /// Parked in a tight loop and `main` was never reached.
    BeforeMain,
    /// Parked in a function that waits for a clock or oscillator.
    ClockWait,
}

/// An early stall: the core sat in a tight loop at start-up, so it never ran
/// the firmware's own behaviour, whatever else the run recorded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct Stall {
    pub phase: StallPhase,
    /// Program counter the core was parked at.
    pub pc: u32,
    /// Function containing `pc`, when the firmware ELF has symbols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// One sentence saying where the core stuck and what to look at.
    pub detail: String,
}

/// How the blocker for a firmware stalled on a missing external oscillator
/// begins; the boot annotation recognises it by this.
pub const EXTERNAL_CLOCK_STALL_PREFIX: &str = "the board fits no crystal or oscillator";

/// Evidence that the firmware executed on one MCU during the run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct ExecutionEvidence {
    pub mcu_ref: String,
    pub backend: String,
    /// Whether the core reached the firmware's entry symbol (`main`).
    pub reached_entry: Observed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_symbol: Option<String>,
    /// Instructions retired (cycles on simavr), when the backend counts them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<u64>,
    /// Program-counter samples taken at chunk boundaries, and how many
    /// distinct values they held (one distinct value: a parked core).
    pub pc_samples: u64,
    pub distinct_pcs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pc: Option<u32>,
    /// Fault or exception handlers entered, or a crashed core.
    pub faults: Vec<String>,
    /// UART bytes the firmware transmitted.
    pub uart_bytes: u64,
    /// Output edges per GPIO pin (`"PA5"`), firmware-driven.
    pub gpio_toggles: BTreeMap<String, u64>,
    /// Whether the board held the reset line asserted for the whole run.
    pub reset_held: Observed,
    /// Whether the supply pads sat inside the part's operating range.
    pub supply_valid: Observed,
    /// Lowest supply-pad voltage at the end of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supply_v: Option<f64>,
    /// Whether the board supplies the clock the core runs from.
    pub clock_valid: Observed,
    /// Where the core's clock comes from, in words.
    pub clock_source: String,
    /// Set when the core stalled early (stuck before `main`, or in a clock
    /// wait): such a run is not "executed" unless the firmware also produced
    /// UART or GPIO output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall: Option<Stall>,
    /// Any positive evidence the firmware ran: entry reached, UART bytes, or a
    /// GPIO toggle. False for an early stall with no output of its own.
    pub executed: bool,
    /// Executed, no fault, not held in reset, supply not invalid and clock not
    /// broken. Only an eligible MCU's firmware behaviour can count as a pass.
    pub firmware_pass_eligible: bool,
    /// Why the run is not eligible, one sentence per reason; empty when it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// The boot mode the board's straps selected and why, for a chip whose
    /// card carries a boot table (hauksbee-ir's vocabulary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<hauksbee_ir::boot::BootDecision>,
    /// The firmware is probably waiting for an external oscillator the board
    /// does not fit (see [`BoardReadings::external_clock_modelled`]). The
    /// sentence is in [`Self::blockers`]; this flag lets the bench view say it
    /// in its one-line headline, so it is not repeated on the wire.
    #[serde(skip)]
    #[schemars(skip)]
    pub stalled_on_external_clock: bool,
}

/// Board-side readings for one MCU, gathered by the scheduler.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardReadings {
    /// Reset held for the whole run (see [`RailWatch::reset_held`]).
    pub reset_held: Observed,
    pub supply_valid: Observed,
    pub supply_v: Option<f64>,
    /// The clock the board gives the core, from the binder.
    pub clock: McuClock,
    /// Whether the backend gates the part's external-oscillator readiness on
    /// the board really fitting a crystal ([`hauksbee_mcu::Mcu::external_clock_modelled`]),
    /// so a firmware wait on the oscillator stalls on a board without one as on
    /// silicon. Without it a missing crystal is invisible in the run.
    pub external_clock_modelled: bool,
}

impl ExecutionEvidence {
    /// Combine the core's probe with the pin and board observations.
    #[must_use]
    pub fn new(
        mcu_ref: &str,
        backend: &str,
        probe: ExecutionProbe,
        uart_bytes: u64,
        pin_toggles: &BTreeMap<(char, u8), u64>,
        board: BoardReadings,
    ) -> Self {
        Self::with_symbols(
            mcu_ref,
            backend,
            probe,
            uart_bytes,
            pin_toggles,
            board,
            None,
        )
    }

    /// [`Self::new`], naming the function a stalled core sits in from the
    /// firmware ELF's symbols.
    #[must_use]
    pub fn with_symbols(
        mcu_ref: &str,
        backend: &str,
        probe: ExecutionProbe,
        uart_bytes: u64,
        pin_toggles: &BTreeMap<(char, u8), u64>,
        board: BoardReadings,
        symbols: Option<&SymbolTable>,
    ) -> Self {
        let reached_entry = Observed::from_option(probe.reached_entry);
        let gpio_toggles: BTreeMap<String, u64> = pin_toggles
            .iter()
            .filter(|&(_, &n)| n > 0)
            .map(|(&(port, bit), &n)| (format!("P{port}{bit}"), n))
            .collect();
        // A boot ROM that logs on the same UART talks before (and without) the
        // image, so its bytes are not firmware output.
        let firmware_uart = uart_bytes > 0 && !probe.boot_rom_console;
        let firmware_output = firmware_uart || !gpio_toggles.is_empty();
        let stall = stall_of(&probe, reached_entry, symbols);
        // A core parked at start-up has not run the firmware's behaviour; only
        // output of its own (UART or a driven pin) shows it got further.
        let executed = if stall.is_some() {
            firmware_output
        } else {
            reached_entry == Observed::Yes || firmware_output
        };
        let mut blockers = Vec::new();
        if let Some(stall) = &stall {
            blockers.push(stall.detail.clone());
        } else if !executed {
            blockers.push(match reached_entry {
                Observed::No => format!(
                    "the core never reached {} and produced no UART or GPIO activity",
                    probe.entry_symbol.as_deref().unwrap_or("its entry symbol")
                ),
                _ if uart_bytes > 0 => format!(
                    "no evidence the firmware executed: no {} seen and no GPIO toggles; \
                     the {uart_bytes} UART bytes may be the boot ROM's own log",
                    probe.entry_symbol.as_deref().unwrap_or("entry symbol")
                ),
                _ => "no evidence the firmware executed: no entry symbol seen, no UART \
                      bytes and no GPIO toggles"
                    .to_string(),
            });
        }
        if !probe.faults.is_empty() {
            blockers.push(format!("the core faulted: {}", probe.faults.join("; ")));
        }
        if board.reset_held == Observed::Yes {
            blockers.push(
                "the board held the reset line asserted for the whole run; the emulated core \
                 ran anyway, which silicon would not"
                    .to_string(),
            );
        }
        if board.supply_valid == Observed::No {
            blockers.push(board.supply_v.map_or_else(
                || "the supply pads are outside the part's operating range".to_string(),
                |v| {
                    format!(
                        "the supply pads read {v:.3} V, outside the part's operating range; \
                         the emulated core ran anyway, which silicon would not"
                    )
                },
            ));
        }
        let clock = board.clock;
        let (clock_valid, clock_source) = clock_reading(&clock);
        if clock_valid == Observed::No {
            blockers.push(format!(
                "the board's clock source is broken ({}); the emulated core ran anyway, \
                 which silicon would not",
                clock.detail
            ));
        }
        // On silicon the firmware's wait for the external oscillator never ends
        // when nothing is fitted; the emulator reproduces that, and the run
        // then shows a core that never got to `main` and nothing else. Say why.
        let stalled_on_external_clock =
            !executed && board.external_clock_modelled && !clock.external_present();
        if stalled_on_external_clock {
            blockers.push(format!(
                "{EXTERNAL_CLOCK_STALL_PREFIX} on the part's external clock pins \
                 ({}), and this backend models oscillator readiness as silicon does, so \
                 firmware that waits for the external oscillator to become ready (an SDK \
                 start-up loop, an HSE or XOSC ready flag) stalls here: fit the crystal \
                 or oscillator the part's reference design uses",
                if clock.detail.is_empty() {
                    "nothing is bound to them"
                } else {
                    &clock.detail
                }
            ));
        }
        Self {
            mcu_ref: mcu_ref.to_string(),
            backend: backend.to_string(),
            reached_entry,
            entry_symbol: probe.entry_symbol,
            instructions: probe.instructions,
            pc_samples: probe.pc_samples,
            distinct_pcs: probe.distinct_pcs,
            last_pc: probe.last_pc,
            faults: probe.faults,
            uart_bytes,
            gpio_toggles,
            stall,
            reset_held: board.reset_held,
            supply_valid: board.supply_valid,
            supply_v: board.supply_v,
            clock_valid,
            clock_source,
            executed,
            firmware_pass_eligible: blockers.is_empty(),
            blockers,
            boot: None,
            stalled_on_external_clock,
        }
    }
}

/// Whether the core is parked in a tight loop early in start-up, and where.
fn stall_of(
    probe: &ExecutionProbe,
    reached_entry: Observed,
    symbols: Option<&SymbolTable>,
) -> Option<Stall> {
    let pc = probe.last_pc?;
    if probe.pc_samples < PARK_MIN_SAMPLES || probe.distinct_pcs > PARK_MAX_DISTINCT_PCS {
        return None;
    }
    let symbol = symbols.and_then(|s| s.lookup(pc)).map(str::to_string);
    let at = symbol.as_ref().map_or_else(
        || format!("0x{pc:08x}"),
        |name| format!("{name} (0x{pc:08x})"),
    );
    let entry = probe.entry_symbol.as_deref().unwrap_or("main");
    let in_clock_wait = symbol.as_deref().is_some_and(|name| {
        let lower = name.to_ascii_lowercase();
        CLOCK_WAIT_MARKS.iter().any(|mark| lower.contains(mark))
    });
    let (phase, detail) = if in_clock_wait {
        (
            StallPhase::ClockWait,
            format!(
                "stalled in a clock wait at {at}: the core is spinning on a clock or oscillator \
                 that never became ready; check the crystal, its load capacitors and the \
                 oscillator pins"
            ),
        )
    } else if reached_entry == Observed::No {
        (
            StallPhase::BeforeMain,
            format!(
                "stalled before {entry} at {at}: the core is parked in a tight loop and never \
                 reached the firmware's entry point"
            ),
        )
    } else {
        return None;
    };
    Some(Stall {
        phase,
        pc,
        symbol,
        detail,
    })
}

/// Whether the board supplies the core's clock, and where it comes from, in words.
fn clock_reading(clock: &McuClock) -> (Observed, String) {
    let d = &clock.detail;
    match clock.kind {
        ClockKind::Crystal => (Observed::Yes, format!("external crystal or resonator: {d}")),
        ClockKind::External => (Observed::Yes, format!("external oscillator: {d}")),
        // The firmware's (or the fuses') clock selection is not checked, so an
        // internal-only board is not evidence of a valid clock.
        ClockKind::Internal => (
            Observed::Unknown,
            format!("internal oscillator only ({d}); firmware/fuse clock selection not checked"),
        ),
        ClockKind::Broken => (Observed::No, format!("clock source broken: {d}")),
        ClockKind::Unknown => {
            (
                Observed::Unknown,
                format!(
                "{}internal oscillator assumed; firmware that waits on an external clock stalls",
                if d.is_empty() { String::new() } else { format!("{d}; ") }
            ),
            )
        }
    }
}

/// One sentence per MCU that stalled early, `"U1: stalled before main at ..."`,
/// for the surfaces that explain a quiet run.
#[must_use]
pub fn stall_notes(rows: &[ExecutionEvidence]) -> Vec<String> {
    rows.iter()
        .filter_map(|e| {
            e.stall
                .as_ref()
                .map(|stall| format!("{}: {}", e.mcu_ref, stall.detail))
        })
        .collect()
}

/// The one-line execution statement every text surface prints for a row.
#[must_use]
pub fn execution_evidence_line(e: &ExecutionEvidence) -> String {
    let toggles: u64 = e.gpio_toggles.values().sum();
    let instructions = e.instructions.map_or_else(
        || "instructions unknown".to_string(),
        |n| format!("{n} instructions"),
    );
    let supply = e.supply_v.map_or_else(
        || e.supply_valid.as_str().to_string(),
        |v| format!("{} ({v:.3} V)", e.supply_valid.as_str()),
    );
    let verdict = match (&e.stall, e.firmware_pass_eligible, e.executed) {
        (Some(stall), _, _) => match stall.phase {
            StallPhase::BeforeMain => "STALLED BEFORE MAIN, not executed",
            StallPhase::ClockWait => "STALLED IN A CLOCK WAIT, not executed",
        },
        (None, true, _) => "executed",
        (None, false, true) => "executed, NOT a firmware pass",
        (None, false, false) => "NO EVIDENCE OF EXECUTION",
    };
    format!(
        "{} ({}): {}; reached {}: {}; {}; {} PC samples ({} distinct); faults: {}; \
         UART {} bytes; GPIO {} toggles on {} pins; reset held: {}; supply valid: {}; \
         clock valid: {}",
        e.mcu_ref,
        e.backend,
        verdict,
        e.entry_symbol.as_deref().unwrap_or("entry"),
        e.reached_entry.as_str(),
        instructions,
        e.pc_samples,
        e.distinct_pcs,
        if e.faults.is_empty() {
            "none".to_string()
        } else {
            e.faults.join("; ")
        },
        e.uart_bytes,
        toggles,
        e.gpio_toggles.len(),
        e.reset_held.as_str(),
        supply,
        e.clock_valid.as_str(),
    )
}

/// Per-MCU watch of the board's reset line and supply pads across a run.
#[derive(Debug, Clone, Default)]
pub struct RailWatch {
    /// Reset-role nodes that the board actually drives (a floating reset net
    /// solves to ~0 V through the pins' own legs and says nothing).
    pub reset_nodes: Vec<usize>,
    /// Direct-supply pad nodes.
    pub supply_nodes: Vec<usize>,
    /// A supply node's own datasheet minimum, when the card gives its pad's
    /// domain a `supply_range` row (the RP2040's 1.1 V DVDD next to its 3.3 V
    /// IOVDD). Nodes without one fall back to the part-wide minimum.
    pub supply_floors: HashMap<usize, f64>,
    /// Highest reset-line voltage seen (the most-released moment).
    reset_peak_v: Option<f64>,
    /// Highest supply-pad voltage seen.
    supply_peak_v: Option<f64>,
    /// Lowest supply-pad voltage at the latest observation.
    supply_now_v: Option<f64>,
    /// Lowest voltage at the latest observation among pads with no floor of
    /// their own (`None` when every watched pad has one).
    unfloored_now_v: Option<f64>,
    /// The deepest pad below its own floor at the latest observation, as
    /// (voltage, floor).
    under_floor_now: Option<(f64, f64)>,
}

impl RailWatch {
    /// A watch over the given reset-role and direct-supply nodes.
    #[must_use]
    pub fn new(reset_nodes: Vec<usize>, supply_nodes: Vec<usize>) -> Self {
        Self {
            reset_nodes,
            supply_nodes,
            ..Self::default()
        }
    }

    /// Give supply nodes their own datasheet minimum (see `supply_floors`).
    #[must_use]
    pub fn with_supply_floors(mut self, floors: HashMap<usize, f64>) -> Self {
        self.supply_floors = floors;
        self
    }

    /// Record one solved chunk's node voltages.
    pub fn observe(&mut self, node_volts: &[f64]) {
        let read = |nodes: &[usize]| -> Vec<f64> {
            nodes
                .iter()
                .filter_map(|&n| node_volts.get(n).copied())
                .filter(|v| v.is_finite())
                .collect()
        };
        let reset = read(&self.reset_nodes);
        if let Some(lowest) = reset.iter().copied().reduce(f64::min) {
            self.reset_peak_v = Some(self.reset_peak_v.map_or(lowest, |p| p.max(lowest)));
        }
        let supply = read(&self.supply_nodes);
        if let Some(highest) = supply.iter().copied().reduce(f64::max) {
            self.supply_peak_v = Some(self.supply_peak_v.map_or(highest, |p| p.max(highest)));
        }
        self.supply_now_v = supply.iter().copied().reduce(f64::min);
        let pads = self.supply_nodes.iter().filter_map(|n| {
            let v = node_volts.get(*n).copied().filter(|v| v.is_finite())?;
            Some((v, self.supply_floors.get(n).copied()))
        });
        let (mut unfloored, mut under) = (None::<f64>, None::<(f64, f64)>);
        for (v, floor) in pads {
            match floor {
                None => unfloored = Some(unfloored.map_or(v, |u| u.min(v))),
                Some(f) if v < f && under.is_none_or(|(uv, uf)| f - v > uf - uv) => {
                    under = Some((v, f));
                }
                Some(_) => {}
            }
        }
        self.unfloored_now_v = unfloored;
        self.under_floor_now = under;
    }

    /// Whether the reset line stayed below `vil` for the whole run. Unknown
    /// with no driven reset node, or when it never cleared `vih` either.
    #[must_use]
    pub fn reset_held(&self, logic_high_v: f64) -> Observed {
        match self.reset_peak_v {
            Some(v) if v < 0.3 * logic_high_v => Observed::Yes,
            Some(v) if v >= 0.7 * logic_high_v => Observed::No,
            _ => Observed::Unknown,
        }
    }

    /// Whether the supply pads sit inside `[min_v, max_v]`. Without a
    /// datasheet minimum, only a pad below half the logic rail is called
    /// invalid (clearly unpowered); anything else stays unknown.
    #[must_use]
    pub fn supply_valid(
        &self,
        min_v: Option<f64>,
        max_v: Option<f64>,
        logic_high_v: f64,
    ) -> Observed {
        let Some(now) = self.supply_now_v else {
            return Observed::Unknown;
        };
        let over = max_v
            .zip(self.supply_peak_v)
            .is_some_and(|(max, peak)| peak > max);
        if over {
            return Observed::No;
        }
        if self.under_floor_now.is_some() {
            return Observed::No;
        }
        if self.supply_floors.is_empty() {
            return Self::judge(now, min_v, logic_high_v);
        }
        // Every floored pad is inside its own row; the rest take the
        // part-wide minimum.
        self.unfloored_now_v
            .map_or(Observed::Yes, |v| Self::judge(v, min_v, logic_high_v))
    }

    fn judge(now: f64, min_v: Option<f64>, logic_high_v: f64) -> Observed {
        match min_v {
            Some(min) => Observed::from_option(Some(now >= min)),
            None if now < 0.5 * logic_high_v => Observed::No,
            None => Observed::Unknown,
        }
    }

    /// The supply-pad voltage the verdict rests on at the latest observation:
    /// a pad below its own floor when there is one, else the lowest pad.
    #[must_use]
    pub fn supply_v(&self) -> Option<f64> {
        self.under_floor_now.map(|(v, _)| v).or(self.supply_now_v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(reset_held: Observed, supply_valid: Observed) -> BoardReadings {
        BoardReadings {
            reset_held,
            supply_valid,
            supply_v: Some(3.3),
            clock: McuClock::default(),
            external_clock_modelled: false,
        }
    }

    fn probe(reached: Option<bool>, faults: &[&str]) -> ExecutionProbe {
        ExecutionProbe {
            reached_entry: reached,
            entry_symbol: reached.map(|_| "main".to_string()),
            instructions: Some(1_000),
            pc_samples: 4,
            distinct_pcs: 3,
            last_pc: Some(0x0800_0100),
            faults: faults.iter().map(ToString::to_string).collect(),
            boot_rom_console: false,
        }
    }

    #[test]
    fn boot_rom_log_bytes_alone_are_not_execution() {
        // An ESP mask ROM prints its boot log whatever the image holds.
        let rom = ExecutionProbe {
            boot_rom_console: true,
            ..probe(None, &[])
        };
        let e = ExecutionEvidence::new(
            "U1",
            "qemu:esp32",
            rom.clone(),
            907,
            &BTreeMap::new(),
            board(Observed::Unknown, Observed::Unknown),
        );
        assert!(!e.executed && !e.firmware_pass_eligible, "{e:?}");
        assert!(e.blockers[0].contains("boot ROM's own log"), "{e:?}");
        // A PC sample in the image's code is.
        let ran = ExecutionProbe {
            reached_entry: Some(true),
            ..rom
        };
        let e = ExecutionEvidence::new(
            "U1",
            "qemu:esp32",
            ran,
            907,
            &BTreeMap::new(),
            board(Observed::Unknown, Observed::Unknown),
        );
        assert!(e.executed && e.firmware_pass_eligible, "{e:?}");
    }

    /// A core that never reached `main` on a board with no crystal, on a backend
    /// that gates oscillator readiness, is reported as waiting for the
    /// oscillator; with readiness not modelled, or a crystal fitted, or `main`
    /// reached, it is not.
    #[test]
    fn a_core_that_never_started_without_a_crystal_is_reported_as_waiting_for_it() {
        let run = |reached: bool, modelled: bool, clock: McuClock| {
            ExecutionEvidence::new(
                "U1",
                "renode:rp2040",
                probe(Some(reached), &[]),
                0,
                &BTreeMap::new(),
                BoardReadings {
                    clock,
                    external_clock_modelled: modelled,
                    ..board(Observed::No, Observed::Yes)
                },
            )
        };
        let e = run(false, true, McuClock::default());
        assert!(
            e.stalled_on_external_clock && !e.firmware_pass_eligible,
            "{e:?}"
        );
        assert!(
            e.blockers
                .iter()
                .any(|b| b.starts_with(EXTERNAL_CLOCK_STALL_PREFIX)),
            "{e:?}"
        );
        assert!(!run(false, false, McuClock::default()).stalled_on_external_clock);
        assert!(!run(true, true, McuClock::default()).stalled_on_external_clock);
        let crystal = McuClock {
            kind: ClockKind::Crystal,
            ..McuClock::default()
        };
        assert!(!run(false, true, crystal).stalled_on_external_clock);
    }

    #[test]
    fn a_clean_run_that_reached_main_is_eligible() {
        let e = ExecutionEvidence::new(
            "U1",
            "renode:stm32g4",
            probe(Some(true), &[]),
            0,
            &BTreeMap::new(),
            board(Observed::No, Observed::Yes),
        );
        assert!(e.executed && e.firmware_pass_eligible, "{e:?}");
        assert!(execution_evidence_line(&e).contains("U1 (renode:stm32g4): executed;"));
    }

    #[test]
    fn no_evidence_is_never_a_pass_even_when_nothing_is_wrong() {
        let e = ExecutionEvidence::new(
            "U1",
            "qemu:esp32",
            probe(None, &[]),
            0,
            &BTreeMap::new(),
            board(Observed::Unknown, Observed::Unknown),
        );
        assert!(!e.executed && !e.firmware_pass_eligible);
        assert!(execution_evidence_line(&e).contains("NO EVIDENCE OF EXECUTION"));
        // A single UART byte or toggle is evidence.
        let toggles = BTreeMap::from([(('A', 5), 2)]);
        let e = ExecutionEvidence::new(
            "U1",
            "qemu:esp32",
            probe(None, &[]),
            0,
            &toggles,
            board(Observed::Unknown, Observed::Unknown),
        );
        assert!(e.executed && e.firmware_pass_eligible);
        assert_eq!(e.gpio_toggles.get("PA5"), Some(&2));
    }

    #[test]
    fn fault_reset_or_dead_supply_blocks_a_pass_despite_activity() {
        let uart = 12;
        for (p, b, needle) in [
            (
                probe(Some(true), &["HardFault handler entered x1"]),
                board(Observed::No, Observed::Yes),
                "faulted",
            ),
            (
                probe(Some(true), &[]),
                board(Observed::Yes, Observed::Yes),
                "reset",
            ),
            (
                probe(Some(true), &[]),
                board(Observed::No, Observed::No),
                "supply",
            ),
        ] {
            let e = ExecutionEvidence::new("U1", "renode:stm32f7", p, uart, &BTreeMap::new(), b);
            assert!(e.executed && !e.firmware_pass_eligible, "{e:?}");
            assert!(e.blockers.iter().any(|r| r.contains(needle)), "{e:?}");
        }
    }

    #[test]
    fn clock_verdict_sets_clock_valid_and_only_a_broken_clock_blocks() {
        let run = |kind, detail: &str| {
            let mut b = board(Observed::No, Observed::Yes);
            b.clock = McuClock {
                kind,
                detail: detail.to_string(),
                ..McuClock::default()
            };
            ExecutionEvidence::new(
                "U1",
                "renode:stm32f103",
                probe(Some(true), &[]),
                0,
                &BTreeMap::new(),
                b,
            )
        };
        for (kind, valid, word) in [
            (ClockKind::Crystal, Observed::Yes, "crystal"),
            (ClockKind::External, Observed::Yes, "oscillator"),
            (
                ClockKind::Internal,
                Observed::Unknown,
                "selection not checked",
            ),
            (ClockKind::Unknown, Observed::Unknown, "assumed"),
        ] {
            let e = run(kind, "RC");
            assert_eq!(e.clock_valid, valid, "{kind:?}");
            assert!(e.clock_source.contains(word), "{}", e.clock_source);
            assert!(e.firmware_pass_eligible, "{kind:?}: {e:?}");
        }
        let e = run(ClockKind::Broken, "XIN is tied to GND");
        assert_eq!(e.clock_valid, Observed::No);
        assert!(e.executed && !e.firmware_pass_eligible, "{e:?}");
        assert!(e.blockers.iter().any(|r| r.contains("XIN is tied to GND")));
        assert!(execution_evidence_line(&e).contains("clock valid: no"));
    }

    #[test]
    fn rail_watch_reads_reset_and_supply_over_the_run() {
        let mut w = RailWatch {
            reset_nodes: vec![1],
            supply_nodes: vec![2, 3],
            ..RailWatch::default()
        };
        assert_eq!(w.reset_held(3.3), Observed::Unknown);
        assert_eq!(
            w.supply_valid(Some(1.71), Some(4.0), 3.3),
            Observed::Unknown
        );
        w.observe(&[0.0, 0.1, 3.3, 3.2]);
        assert_eq!(w.reset_held(3.3), Observed::Yes);
        assert_eq!(w.supply_valid(Some(1.71), Some(4.0), 3.3), Observed::Yes);
        assert_eq!(w.supply_v(), Some(3.2));
        // Reset released later in the run: not held.
        w.observe(&[0.0, 3.3, 3.3, 3.3]);
        assert_eq!(w.reset_held(3.3), Observed::No);
        // An overvoltage peak anywhere in the run invalidates the supply.
        w.observe(&[0.0, 3.3, 4.5, 3.3]);
        assert_eq!(w.supply_valid(Some(1.71), Some(4.0), 3.3), Observed::No);
        // Below the datasheet minimum at the end is invalid; without a
        // minimum only a clearly unpowered pad is.
        let mut low = RailWatch {
            supply_nodes: vec![0],
            ..RailWatch::default()
        };
        low.observe(&[1.5]);
        assert_eq!(low.supply_valid(Some(1.71), None, 3.3), Observed::No);
        assert_eq!(low.supply_valid(None, None, 3.3), Observed::No);
        low.observe(&[2.5]);
        assert_eq!(low.supply_valid(None, None, 3.3), Observed::Unknown);
    }

    /// The RP2040's 1.1 V DVDD next to its 3.3 V IOVDD: each pad is judged
    /// against its own row, so the core rail is not an unpowered I/O rail.
    #[test]
    fn each_supply_domain_is_judged_against_its_own_floor() {
        let mut w = RailWatch::new(vec![], vec![0, 1])
            .with_supply_floors(HashMap::from([(0, 1.62), (1, 1.05)]));
        w.observe(&[3.3, 1.1]);
        assert_eq!(w.supply_valid(Some(1.05), Some(3.63), 3.3), Observed::Yes);
        assert_eq!(w.supply_v(), Some(1.1));
        // IOVDD sagging to 1.2 V is under its own 1.62 V floor even though
        // it clears the part-wide (DVDD) minimum.
        w.observe(&[1.2, 1.1]);
        assert_eq!(w.supply_valid(Some(1.05), Some(3.63), 3.3), Observed::No);
        assert_eq!(w.supply_v(), Some(1.2));
        // A pad with no row of its own still takes the part-wide rule.
        let mut mixed =
            RailWatch::new(vec![], vec![0, 1]).with_supply_floors(HashMap::from([(1, 1.05)]));
        mixed.observe(&[1.0, 1.1]);
        assert_eq!(mixed.supply_valid(None, None, 3.3), Observed::No);
        mixed.observe(&[3.3, 1.1]);
        assert_eq!(mixed.supply_valid(None, None, 3.3), Observed::Unknown);
    }

    fn parked(reached: Option<bool>, pc: u32) -> ExecutionProbe {
        ExecutionProbe {
            pc_samples: 64,
            distinct_pcs: 1,
            last_pc: Some(pc),
            ..probe(reached, &[])
        }
    }

    fn table() -> SymbolTable {
        SymbolTable::from_image(&crate::observe::symbols::test_image(&[
            ("Reset_Handler", 0x0800_0100, 0x20),
            ("HAL_RCC_OscConfig", 0x0800_0200, 0x80),
            ("main", 0x0800_0400, 0x40),
        ]))
        .unwrap()
    }

    fn stalled(p: ExecutionProbe, uart: u64) -> ExecutionEvidence {
        ExecutionEvidence::with_symbols(
            "U1",
            "renode:stm32f103",
            p,
            uart,
            &BTreeMap::new(),
            board(Observed::No, Observed::Yes),
            Some(&table()),
        )
    }

    #[test]
    fn a_core_stuck_before_main_is_stalled_not_executed() {
        let e = stalled(parked(Some(false), 0x0800_0108), 0);
        let stall = e.stall.as_ref().expect("stalled");
        assert_eq!(stall.phase, StallPhase::BeforeMain);
        assert_eq!(stall.symbol.as_deref(), Some("Reset_Handler"));
        assert!(!e.executed && !e.firmware_pass_eligible, "{e:?}");
        assert!(
            e.blockers[0].contains("before main at Reset_Handler"),
            "{e:?}"
        );
        assert!(execution_evidence_line(&e).contains("STALLED BEFORE MAIN, not executed"));
    }

    #[test]
    fn a_core_waiting_on_a_clock_names_where_even_after_reset_handler() {
        for reached in [Some(false), Some(true)] {
            let e = stalled(parked(reached, 0x0800_0210), 0);
            let stall = e.stall.as_ref().expect("stalled");
            assert_eq!(stall.phase, StallPhase::ClockWait, "{reached:?}");
            assert!(stall.detail.contains("HAL_RCC_OscConfig"), "{stall:?}");
            assert!(!e.executed, "{e:?}");
            assert!(execution_evidence_line(&e).contains("STALLED IN A CLOCK WAIT"));
        }
    }

    #[test]
    fn a_stall_without_symbols_still_reports_the_address() {
        let e = ExecutionEvidence::new(
            "U1",
            "renode:stm32f103",
            parked(Some(false), 0x0800_0108),
            0,
            &BTreeMap::new(),
            board(Observed::No, Observed::Yes),
        );
        let stall = e.stall.as_ref().expect("stalled");
        assert!(
            stall.symbol.is_none() && stall.detail.contains("0x08000108"),
            "{stall:?}"
        );
    }

    #[test]
    fn output_of_its_own_keeps_a_later_stall_executed_but_not_a_pass() {
        let e = stalled(parked(Some(true), 0x0800_0210), 12);
        assert!(e.stall.is_some() && e.executed, "{e:?}");
        assert!(!e.firmware_pass_eligible, "{e:?}");
    }

    #[test]
    fn a_quiet_firmware_parked_in_main_is_not_a_stall() {
        let e = stalled(parked(Some(true), 0x0800_0410), 0);
        assert!(
            e.stall.is_none() && e.executed && e.firmware_pass_eligible,
            "{e:?}"
        );
        // Too few samples, or a core that is still moving, is not a park.
        let few = ExecutionProbe {
            pc_samples: 4,
            ..parked(Some(false), 0x0800_0108)
        };
        assert!(stalled(few, 0).stall.is_none());
        let moving = ExecutionProbe {
            distinct_pcs: 40,
            ..parked(Some(false), 0x0800_0108)
        };
        assert!(stalled(moving, 0).stall.is_none());
        // A backend that cannot say whether main was reached claims nothing.
        assert!(stalled(parked(None, 0x0800_0108), 0).stall.is_none());
    }
}
