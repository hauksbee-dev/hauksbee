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

use std::collections::BTreeMap;

use hauksbee_mcu::ExecutionProbe;

/// The sentence every surface (CLI text, `--plain`, `--json` notes, stderr,
/// the web front door) uses when a run shows no evidence the firmware ran.
pub const NO_EXECUTION_MESSAGE: &str = "co-sim saw no evidence the firmware executed (entry \
     symbol not reached, no firmware GPIO drive, no UART output); this result cannot vouch \
     for firmware behaviour";

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
    /// Any positive evidence the firmware ran: entry reached, UART bytes, or a
    /// GPIO toggle.
    pub executed: bool,
    /// Executed, no fault, not held in reset, and supply not invalid. Only an
    /// eligible MCU's firmware behaviour can count as a pass.
    pub firmware_pass_eligible: bool,
    /// Why the run is not eligible, one sentence per reason; empty when it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

/// Board-side readings for one MCU, gathered by the scheduler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoardReadings {
    /// Reset held for the whole run (see [`RailWatch::reset_held`]).
    pub reset_held: Observed,
    pub supply_valid: Observed,
    pub supply_v: Option<f64>,
    /// A crystal or oscillator is bound to the MCU's clock pins.
    pub external_clock: bool,
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
        let reached_entry = Observed::from_option(probe.reached_entry);
        let gpio_toggles: BTreeMap<String, u64> = pin_toggles
            .iter()
            .filter(|&(_, &n)| n > 0)
            .map(|(&(port, bit), &n)| (format!("P{port}{bit}"), n))
            .collect();
        let executed = reached_entry == Observed::Yes || uart_bytes > 0 || !gpio_toggles.is_empty();
        let mut blockers = Vec::new();
        if !executed {
            blockers.push(match reached_entry {
                Observed::No => format!(
                    "the core never reached {} and produced no UART or GPIO activity",
                    probe.entry_symbol.as_deref().unwrap_or("its entry symbol")
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
            reset_held: board.reset_held,
            supply_valid: board.supply_valid,
            supply_v: board.supply_v,
            clock_valid: if board.external_clock {
                Observed::Yes
            } else {
                Observed::Unknown
            },
            clock_source: if board.external_clock {
                "external crystal or oscillator bound".to_string()
            } else {
                "internal oscillator assumed; firmware that waits on an external clock stalls"
                    .to_string()
            },
            executed,
            firmware_pass_eligible: blockers.is_empty(),
            blockers,
        }
    }
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
    format!(
        "{} ({}): {}; reached {}: {}; {}; {} PC samples ({} distinct); faults: {}; \
         UART {} bytes; GPIO {} toggles on {} pins; reset held: {}; supply valid: {}; \
         clock valid: {}",
        e.mcu_ref,
        e.backend,
        if e.firmware_pass_eligible {
            "executed"
        } else if e.executed {
            "executed, NOT a firmware pass"
        } else {
            "NO EVIDENCE OF EXECUTION"
        },
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
    /// Highest reset-line voltage seen (the most-released moment).
    reset_peak_v: Option<f64>,
    /// Highest supply-pad voltage seen.
    supply_peak_v: Option<f64>,
    /// Lowest supply-pad voltage at the latest observation.
    supply_now_v: Option<f64>,
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
        match min_v {
            Some(min) => Observed::from_option(Some(now >= min)),
            None if now < 0.5 * logic_high_v => Observed::No,
            None => Observed::Unknown,
        }
    }

    /// The lowest supply-pad voltage at the latest observation.
    #[must_use]
    pub const fn supply_v(&self) -> Option<f64> {
        self.supply_now_v
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
            external_clock: false,
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
        }
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
}
