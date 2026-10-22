//! An MCU's electrical behaviour as the card states it, applied to the live
//! co-simulation: the output resistance a pin drives with (per drive-strength
//! setting the firmware programmed, per level), and the reset the part holds
//! itself in when its reset pin is low or its supply is below the brown-out
//! threshold.
//!
//! Everything here is datasheet data from the card (`[[models.output_drive]]`,
//! `[models.drive_select]`, `[models.reset]`); a part whose card states none of
//! it keeps the generic 50 ohm driver and is never held in reset.
//!
//! # Reset
//!
//! The gate runs once per chunk per MCU, before the core is advanced, on the
//! voltages the digital side reads (the previous chunk's solve; a t = 0 DC
//! solve before the first). While the part is held its core is not advanced
//! (virtual time on the core stands still, as the silicon's does) and its pin
//! drivers float. When the hold ends the core is rebooted through
//! [`Mcu::reset`], so the firmware starts again from its reset vector with
//! every peripheral at its power-on state.

use super::{Circuit, LiveMcu};
use hauksbee_bind::drivers::DEFAULT_RO;
use std::collections::{HashMap, HashSet};

/// Chunks between refreshes of every enabled pin's drive-select field. A pin
/// that just produced an edge is always refreshed; this bounds how long a
/// drive-strength change made on an idle pin goes unnoticed.
const DRIVE_POLL_CHUNKS: u32 = 8;

/// What held a part in reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCause {
    /// The reset pin sat below its input-low threshold.
    ResetPin,
    /// The supply fell below the brown-out threshold after having been up.
    BrownOut,
    /// The supply had not yet risen to the release threshold (power-up).
    PowerUp,
}

impl ResetCause {
    /// A short phrase for a report line.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::ResetPin => "the reset pin held low",
            Self::BrownOut => "a supply brown-out",
            Self::PowerUp => "the supply still rising at power-up",
        }
    }
}

/// One stretch of time a part spent held in reset.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ResetEpisode {
    /// What held the part.
    pub cause: ResetCause,
    /// Simulated time (s) the hold began.
    pub start_s: f64,
    /// Simulated time (s) the part was released; `None` while still held at
    /// the end of the run.
    pub end_s: Option<f64>,
    /// The lowest voltage (V) seen on the offending pin or supply.
    pub low_v: f64,
    /// The datasheet threshold (V) it fell below.
    pub threshold_v: f64,
    /// Whether the core was rebooted on release. `false` means the backend
    /// cannot reboot its core, so the firmware carried on from where it was.
    pub rebooted: bool,
}

/// Hysteretic reset state for one MCU.
// Five independent latches (pin, supply, supply-seen-up, core-ran, reboot
// refused) that change on different events; an enum of their 32 combinations
// would be harder to read than the flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub(super) struct ResetGate {
    pin_low: bool,
    supply_low: bool,
    supply_ever_up: bool,
    /// Simulated time the supply-release delay ends, once the supply is up.
    release_deadline: Option<f64>,
    /// The core has been advanced since it was last reset (a reboot is only
    /// meaningful then).
    core_ran: bool,
    /// Backend refused a reboot; reported once, then not retried.
    reboot_unsupported: bool,
    pub(super) episodes: Vec<ResetEpisode>,
}

/// What the gate decided for one chunk.
#[derive(Debug, PartialEq, Eq)]
enum GateStep {
    /// Not held; the core may run.
    Run,
    /// Held; the core must not run.
    Hold,
    /// The hold just ended; reboot, then run.
    Release,
}

impl ResetGate {
    fn step(
        &mut self,
        spec: &hauksbee_models::ResetSpec,
        t: f64,
        pin_v: Option<f64>,
        supply_v: Option<f64>,
    ) -> GateStep {
        let was_held = self.pin_low || self.supply_low;
        if let (Some(v), Some(vil)) = (pin_v, spec.pin_vil_v) {
            let vih = spec.pin_vih_v.unwrap_or(vil);
            if v < vil {
                self.pin_low = true;
            } else if v > vih {
                self.pin_low = false;
            }
        }
        let mut supply_threshold = None;
        if let (Some(v), Some(assert)) = (supply_v, spec.assert_below_v) {
            supply_threshold = Some(assert);
            let release = spec.release_above_v.unwrap_or(assert);
            if v < assert {
                self.supply_low = true;
                self.release_deadline = None;
            } else if v >= release {
                self.supply_ever_up = true;
                if self.supply_low {
                    let deadline = *self
                        .release_deadline
                        .get_or_insert_with(|| t + spec.release_delay_s.unwrap_or(0.0));
                    if t >= deadline {
                        self.supply_low = false;
                        self.release_deadline = None;
                    }
                }
            }
        }
        let held = self.pin_low || self.supply_low;
        if held && !was_held {
            let (cause, low_v, threshold_v) = if self.pin_low {
                (
                    ResetCause::ResetPin,
                    pin_v.unwrap_or(0.0),
                    spec.pin_vil_v.unwrap_or(0.0),
                )
            } else {
                (
                    if self.supply_ever_up {
                        ResetCause::BrownOut
                    } else {
                        ResetCause::PowerUp
                    },
                    supply_v.unwrap_or(0.0),
                    supply_threshold.unwrap_or(0.0),
                )
            };
            self.episodes.push(ResetEpisode {
                cause,
                start_s: t,
                end_s: None,
                low_v,
                threshold_v,
                rebooted: false,
            });
        }
        if held {
            if let Some(ep) = self.episodes.last_mut() {
                let v = if self.pin_low { pin_v } else { supply_v };
                if let Some(v) = v {
                    ep.low_v = ep.low_v.min(v);
                }
            }
            return GateStep::Hold;
        }
        if was_held {
            if let Some(ep) = self.episodes.last_mut() {
                ep.end_s = Some(t);
            }
            return GateStep::Release;
        }
        GateStep::Run
    }
}

impl LiveMcu {
    /// The driver's output resistance row for `pin`: the setting the firmware
    /// last programmed, else the card's reset default.
    fn drive_row(&self, pin: (char, u8)) -> Option<&hauksbee_bind::mcu_electrical::DriveRow> {
        let e = &self.binding.electrical;
        self.drive_rows
            .get(&pin)
            .and_then(|&i| e.drive.get(i))
            .or_else(|| e.reset_default())
    }

    /// Drive `pin` to `level` through its card output stage: the output
    /// resistance of the selected setting at that level, and a floating pin on
    /// a level the setting does not drive (an open-drain mode's high).
    pub(super) fn drive_pin(&mut self, circuit: &mut Circuit, pin: (char, u8), level: bool) {
        let stage = self.drive_row(pin).map(|row| row.drive(level, DEFAULT_RO));
        let high_v = self.logic_high_v;
        let Some(drv) = self.binding.gpio_drivers.get_mut(&pin) else {
            return;
        };
        if let Some((ohms, driven)) = stage {
            drv.set_ron(circuit, ohms);
            drv.set_enabled(circuit, driven);
        } else {
            drv.set_enabled(circuit, true);
        }
        drv.set_volts(circuit, if level { high_v } else { 0.0 });
    }

    /// Re-read the drive-select field of the pins that need it and retune any
    /// whose setting changed. `edged` pins always refresh (firmware sets the
    /// drive before it drives); every enabled pin refreshes every
    /// [`DRIVE_POLL_CHUNKS`]th chunk.
    pub(super) fn refresh_drive_select(
        &mut self,
        circuit: &mut Circuit,
        edged: &HashSet<(char, u8)>,
    ) {
        let (Some(sel), true) = (
            self.binding.electrical.drive_select.clone(),
            self.binding.electrical.drive.len() > 1,
        ) else {
            return;
        };
        self.drive_poll = (self.drive_poll + 1) % DRIVE_POLL_CHUNKS;
        let sweep = self.drive_poll == 0;
        let pins: Vec<(char, u8)> = self
            .binding
            .gpio_drivers
            .iter()
            .filter(|(pin, drv)| drv.enabled && (sweep || edged.contains(pin)))
            .map(|(&pin, _)| pin)
            .collect();
        let mut words: HashMap<u32, Option<u32>> = HashMap::new();
        let mask = if sel.width >= 32 {
            u32::MAX
        } else {
            (1u32 << sel.width) - 1
        };
        for pin in pins {
            let Some((addr, shift)) = sel.locate(pin.0, pin.1) else {
                continue;
            };
            let word = *words
                .entry(addr)
                .or_insert_with(|| self.core.read_register(addr));
            let Some(word) = word else { continue };
            let field = (word >> shift) & mask;
            let Some(idx) = self
                .binding
                .electrical
                .drive
                .iter()
                .position(|r| r.selector == Some(field))
            else {
                continue;
            };
            if self.drive_rows.get(&pin) != Some(&idx) {
                self.drive_rows.insert(pin, idx);
                if let Some(&level) = self.last_levels.get(&pin) {
                    self.drive_pin(circuit, pin, level);
                }
            }
        }
    }

    /// Decide whether this MCU's core may run this chunk. Returns `true` while
    /// the part is held in reset: its core must not advance and its pins float.
    /// On release the core is rebooted.
    ///
    /// `volts` are the node voltages the digital side reads this chunk.
    pub(super) fn reset_gate_hold(&mut self, circuit: &mut Circuit, t: f64, volts: &[f64]) -> bool {
        let Some(spec) = self.binding.electrical.reset.as_ref() else {
            return false;
        };
        let at = |n: usize| volts.get(n).copied().filter(|v| v.is_finite());
        // Only nets something drives say anything: a floating reset or supply
        // net solves to ~0 V through the pins' own legs.
        let pin_v = self.rails.reset_nodes.first().and_then(|&n| at(n));
        let supply_v = spec
            .supply_pin
            .as_ref()
            .and_then(|role| self.binding.role_nets.get(role))
            .and_then(|n| usize::try_from(n.0).ok())
            .filter(|n| self.rails.supply_nodes.contains(n))
            .and_then(at);
        match self.reset_gate.step(spec, t, pin_v, supply_v) {
            GateStep::Run => {
                self.reset_gate.core_ran = true;
                false
            }
            GateStep::Hold => {
                self.float_pins(circuit);
                true
            }
            GateStep::Release => {
                self.float_pins(circuit);
                if self.reset_gate.core_ran && !self.reset_gate.reboot_unsupported {
                    match self.core.reset() {
                        Ok(()) => {
                            if let Some(ep) = self.reset_gate.episodes.last_mut() {
                                ep.rebooted = true;
                            }
                        }
                        Err(e) => {
                            self.reset_gate.reboot_unsupported = true;
                            hauksbee_ir::eprintln!(
                                "WARNING: MCU {} could not be rebooted after reset: {e:#}",
                                self.binding.reference
                            );
                        }
                    }
                } else if !self.reset_gate.core_ran {
                    // Never ran: it starts from reset anyway.
                    if let Some(ep) = self.reset_gate.episodes.last_mut() {
                        ep.rebooted = true;
                    }
                }
                // The reboot cleared the core's inputs and pin state.
                self.digital_in_levels.clear();
                self.reset_gate.core_ran = true;
                false
            }
        }
    }

    /// Every pin driver floats and the firmware-side pin state is forgotten.
    fn float_pins(&mut self, circuit: &mut Circuit) {
        for drv in self.binding.gpio_drivers.values_mut() {
            if drv.enabled {
                drv.set_enabled(circuit, false);
            }
        }
        self.configured_outputs.clear();
        self.last_levels.clear();
        self.drive_rows.clear();
    }
}

/// The one-line finding every surface emits for a part's reset episodes.
#[must_use]
pub fn reset_episode_message(mcu_ref: &str, ep: &ResetEpisode) -> String {
    let span = ep.end_s.map_or_else(
        || format!("from t={:.4} s to the end of the run", ep.start_s),
        |end| {
            format!(
                "from t={:.4} s to t={end:.4} s ({:.1} ms)",
                ep.start_s,
                (end - ep.start_s) * 1e3
            )
        },
    );
    let restart = match (ep.end_s, ep.rebooted) {
        (None, _) => ", and was still held when the run ended",
        (Some(_), true) => "; the firmware restarted from its reset vector on release",
        (Some(_), false) => {
            "; this backend cannot reboot the core, so the firmware carried on from where it was"
        }
    };
    format!(
        "MCU {mcu_ref}: held in reset by {} {span} (lowest {:.2} V against the datasheet \
         {:.2} V threshold){restart}",
        ep.cause.describe(),
        ep.low_v,
        ep.threshold_v,
    )
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use hauksbee_models::ResetSpec;

    fn spec() -> ResetSpec {
        ResetSpec {
            supply_pin: Some("vdd".into()),
            assert_below_v: Some(1.88),
            release_above_v: Some(1.92),
            release_delay_s: Some(0.0025),
            pin_vil_v: Some(0.8),
            pin_vih_v: Some(2.0),
            basis: "test".into(),
        }
    }

    #[test]
    fn a_reset_pin_is_held_below_vil_and_released_above_vih() {
        let mut g = ResetGate::default();
        let s = spec();
        assert_eq!(g.step(&s, 0.0, Some(3.3), Some(3.3)), GateStep::Run);
        assert_eq!(g.step(&s, 0.001, Some(0.2), Some(3.3)), GateStep::Hold);
        // Between VIL and VIH the previous state stands.
        assert_eq!(g.step(&s, 0.002, Some(1.4), Some(3.3)), GateStep::Hold);
        assert_eq!(g.step(&s, 0.003, Some(2.4), Some(3.3)), GateStep::Release);
        assert_eq!(g.episodes.len(), 1);
        assert_eq!(g.episodes[0].cause, ResetCause::ResetPin);
        assert_eq!(g.episodes[0].end_s, Some(0.003));
    }

    #[test]
    fn a_brown_out_holds_until_the_supply_is_back_and_the_delay_has_elapsed() {
        let mut g = ResetGate::default();
        let s = spec();
        assert_eq!(g.step(&s, 0.0, None, Some(3.3)), GateStep::Run);
        assert_eq!(g.step(&s, 0.001, None, Some(1.2)), GateStep::Hold);
        // Back above the rising threshold, but inside the temporization.
        assert_eq!(g.step(&s, 0.002, None, Some(3.3)), GateStep::Hold);
        assert_eq!(g.step(&s, 0.004, None, Some(3.3)), GateStep::Hold);
        assert_eq!(g.step(&s, 0.0046, None, Some(3.3)), GateStep::Release);
        assert_eq!(g.episodes[0].cause, ResetCause::BrownOut);
        assert!((g.episodes[0].low_v - 1.2).abs() < 1e-12);
    }

    #[test]
    fn a_supply_that_starts_low_is_a_power_up_not_a_brown_out() {
        let mut g = ResetGate::default();
        let mut s = spec();
        s.release_delay_s = None;
        assert_eq!(g.step(&s, 0.0, None, Some(0.0)), GateStep::Hold);
        assert_eq!(g.step(&s, 0.001, None, Some(3.3)), GateStep::Release);
        assert_eq!(g.episodes[0].cause, ResetCause::PowerUp);
    }

    #[test]
    fn a_part_with_no_thresholds_is_never_held() {
        let mut g = ResetGate::default();
        let s = ResetSpec {
            basis: "none".into(),
            ..ResetSpec::default()
        };
        assert_eq!(g.step(&s, 0.0, Some(0.0), Some(0.0)), GateStep::Run);
        assert!(g.episodes.is_empty());
    }
}
