//! The sleep-current account: current the circuit does not contain.
//!
//! A microcontroller's internal pull-up is part of the chip, not of the analog
//! circuit the solver sees. A firmware that reads a DIP switch with the
//! pull-ups on, and goes to sleep with them still on, puts `Vdd / R_pu` through
//! every closed contact for as long as it sleeps (66 to 165 uA per pin at 3.3 V
//! on an `ATmega328P`), orders of magnitude over the part's own microamp sleep
//! current, and nothing else in the co-sim notices.
//!
//! This module holds the account (types, arithmetic, thresholds, wording) and
//! the scheduler glue that feeds it. The scheduler asks the MCU core which
//! pull-ups are on ([`hauksbee_mcu::Mcu::internal_pull_ups`]) and what sleep
//! mode the firmware selected, looks each pad's net up in the solved circuit,
//! and keeps the worst sleeping chunk per MCU.
//!
//! Declared from `scheduler.rs` as a child module (so the glue reaches the
//! scheduler's private state) and re-exported from there.

use std::collections::BTreeMap;

use hauksbee_bind::mcu_electrical::SleepSpec;
use hauksbee_ir::{Device, NodeId};

use super::{
    backend_is_external,
    direct_supply_nodes,
    Scheduler,
    WEAK_DIGITAL_DRIVE_OHMS, //
};

/// A sleep current this many times the card's power-down current is flagged
/// even with no budget stated.
pub const CARD_RATIO_LIMIT: f64 = 10.0;

/// A net counts as held low when its solved voltage (the circuit has no
/// internal pull-up in it) is at most this fraction of the supply: the
/// `ATmega328P` datasheet reads anything under 0.3 VCC as logic low (Table 30-1,
/// VIL, VCC 2.4 to 5.5 V), so a pull-up fighting such a net is fighting a real
/// low.
pub const HELD_LOW_FRACTION: f64 = 0.3;

/// One pad whose internal pull-up is on and whose net a real low path holds low.
#[derive(Debug, Clone, PartialEq)]
pub struct LowPathPin {
    /// The port pin, `"PD2"`.
    pub pin: String,
    /// The package pad the card maps to that pin, when it does.
    pub pad: Option<String>,
    /// The board net on the pad.
    pub net: String,
    /// The net's solved voltage (V) without the pull-up.
    pub net_v: f64,
    /// The parts that hold the net low (a closed switch contact, a resistor to
    /// ground), by reference.
    pub low_path: Vec<String>,
    /// The low path's resistance (ohm): its legs in parallel. This is the
    /// first-hop resistance only; a chain of further resistors to ground adds
    /// to it, so the current computed from it is an upper bound.
    pub path_ohms: f64,
}

/// A [`LowPathPin`] with its pull-up current.
#[derive(Debug, Clone, PartialEq)]
pub struct PullUpPin {
    pub pin: LowPathPin,
    /// Smallest current (A), through the pull-up's largest resistance. `None`
    /// when the card does not state the resistance.
    pub min_a: Option<f64>,
    /// Largest current (A), through the pull-up's smallest resistance.
    pub max_a: Option<f64>,
}

/// One MCU's sleep-current account at its worst sleeping chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct SleepCurrent {
    pub mcu_ref: String,
    /// The part the board asked for, for the message (`"ATmega328P"`).
    pub part: String,
    /// The datasheet name of the selected sleep mode, `None` when the core
    /// could not report it.
    pub mode: Option<String>,
    /// Whether the card's power-down figure is the yardstick for the mode.
    pub power_down: bool,
    /// The MCU's supply pad voltage (V) in the solve.
    pub vdd_v: f64,
    /// Sim time (s) at the end of the chunk this account was taken from.
    pub t_s: f64,
    pub pins: Vec<PullUpPin>,
    /// Sum of the pins' smallest / largest currents (A); `None` when the card
    /// states no pull-up resistance, see [`Self::unknown`].
    pub total_min_a: Option<f64>,
    pub total_max_a: Option<f64>,
    /// The card's power-down current (A), typical and maximum.
    pub card_typ_a: Option<f64>,
    pub card_max_a: Option<f64>,
    /// The sleep budget (A) stated for the run, if any.
    pub budget_a: Option<f64>,
    /// Which card field is missing, when the current is unknown.
    pub unknown: Option<String>,
    /// Total above the stated budget.
    pub over_budget: bool,
    /// Total above [`CARD_RATIO_LIMIT`] times the card's power-down current.
    pub over_card: bool,
}

impl SleepCurrent {
    /// Whether this account is worth a warning.
    #[must_use]
    pub const fn flagged(&self) -> bool {
        self.over_budget || self.over_card
    }
}

/// The current (A) one pull-up of `r_pu` ohms passes from `vdd` into a net the
/// low path holds at `net_v` through `path_ohms`.
///
/// A Thevenin source of `net_v` behind `path_ohms` against `vdd` behind
/// `r_pu`. Never negative: a net at or above the supply takes none.
#[must_use]
pub fn pin_current(vdd: f64, net_v: f64, path_ohms: f64, r_pu: f64) -> f64 {
    let total = r_pu + path_ohms.max(0.0);
    if total <= 0.0 || !vdd.is_finite() || !net_v.is_finite() {
        return 0.0;
    }
    ((vdd - net_v) / total).max(0.0)
}

/// Account the pull-up current of `pins` (all with their pull-ups on and a low
/// path) against the card's `spec` and the run's optional `budget_a`.
///
/// `power_down` says whether the card's power-down figures are the right
/// yardstick (the mode is power-down); only then is the card ratio checked.
#[must_use]
pub fn account(
    (mcu_ref, part): (&str, &str),
    mode: Option<(&str, bool)>,
    vdd_v: f64,
    t_s: f64,
    pins: Vec<LowPathPin>,
    spec: &SleepSpec,
    budget_a: Option<f64>,
) -> SleepCurrent {
    let (r_lo, r_hi) = (spec.pull_up_min_ohms, spec.pull_up_max_ohms);
    let unknown = match (r_lo, r_hi) {
        (Some(_), Some(_)) => None,
        (None, Some(_)) => Some("io_pull_up_min_ohms"),
        (Some(_), None) => Some("io_pull_up_max_ohms"),
        (None, None) => Some("io_pull_up_min_ohms and io_pull_up_max_ohms"),
    }
    .map(|missing| {
        format!(
            "the {part} card has no {missing}, so the pull-up current range is unknown; \
             add the datasheet's pull-up resistance to the card to size it"
        )
    });
    let rows: Vec<PullUpPin> = pins
        .into_iter()
        .map(|pin| {
            let at = |r: Option<f64>| r.map(|r| pin_current(vdd_v, pin.net_v, pin.path_ohms, r));
            // The smallest resistance gives the largest current.
            let (max_a, min_a) = (at(r_lo), at(r_hi));
            PullUpPin { pin, min_a, max_a }
        })
        .collect();
    let sum = |f: fn(&PullUpPin) -> Option<f64>| -> Option<f64> {
        rows.iter().map(f).sum::<Option<f64>>()
    };
    let (total_min_a, total_max_a) = if unknown.is_some() {
        (None, None)
    } else {
        (sum(|p| p.min_a), sum(|p| p.max_a))
    };
    let (mode_name, is_pd) = mode.map_or((None, false), |(n, pd)| (Some(n.to_string()), pd));
    // The upper end of the range is the claim: the part can pass up to this.
    let over_budget = matches!((total_max_a, budget_a), (Some(t), Some(b)) if t > b);
    let over_card = is_pd
        && matches!(
            (total_max_a, spec.power_down_max_a),
            (Some(t), Some(c)) if c > 0.0 && t > CARD_RATIO_LIMIT * c
        );
    SleepCurrent {
        mcu_ref: mcu_ref.to_string(),
        part: part.to_string(),
        mode: mode_name,
        power_down: is_pd,
        vdd_v,
        t_s,
        pins: rows,
        total_min_a,
        total_max_a,
        card_typ_a: spec.power_down_typ_a,
        card_max_a: spec.power_down_max_a,
        budget_a,
        unknown,
        over_budget,
        over_card,
    }
}

/// A current for a sentence: microamps below a milliamp, else milliamps.
#[must_use]
pub fn fmt_current(a: f64) -> String {
    if a.abs() < 1e-3 {
        let ua = a * 1e6;
        if ua.abs() < 10.0 {
            format!("{ua:.2} uA")
        } else {
            format!("{ua:.0} uA")
        }
    } else {
        format!("{:.2} mA", a * 1e3)
    }
}

/// `lo-hi unit`, collapsing to one figure when they agree to the printed digits.
fn fmt_range(lo: f64, hi: f64) -> String {
    let (l, h) = (fmt_current(lo), fmt_current(hi));
    if l == h {
        return l;
    }
    match (l.split_once(' '), h.split_once(' ')) {
        (Some((ln, lu)), Some((hn, hu))) if lu == hu => format!("{ln}-{hn} {hu}"),
        _ => format!("{l} to {h}"),
    }
}

fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [head @ .., last] => format!("{}, and {last}", head.join(", ")),
    }
}

impl SleepCurrent {
    /// The warning, in the words a person at the bench would use: what is
    /// wrong, why, and the fix. `None` when nothing is flagged.
    #[must_use]
    pub fn message(&self) -> Option<String> {
        if !self.flagged() {
            return None;
        }
        let (lo, hi) = (self.total_min_a?, self.total_max_a?);
        let mode = self.mode.as_deref().unwrap_or("sleep");
        let pads: Vec<String> = self
            .pins
            .iter()
            .map(|p| {
                let pad = p
                    .pin
                    .pad
                    .as_deref()
                    .map_or_else(String::new, |pad| format!(" (pad {pad})"));
                format!("{}{pad} on net {}", p.pin.pin, p.pin.net)
            })
            .collect();
        let mut parts: Vec<String> = self
            .pins
            .iter()
            .flat_map(|p| p.pin.low_path.iter().cloned())
            .collect();
        parts.sort();
        parts.dedup();
        let per_pin = self
            .pins
            .iter()
            .filter_map(|p| Some((p.min_a?, p.max_a?)))
            .fold(None, |acc: Option<(f64, f64)>, (a, b)| {
                Some(acc.map_or((a, b), |(x, y)| (x.min(a), y.max(b))))
            })
            .map_or_else(String::new, |(a, b)| {
                format!(" ({} per pin)", fmt_range(a, b))
            });
        let mut s = format!(
            "co-sim: {} ({}) went to sleep ({mode}) with the internal pull-up still enabled \
             on {}, and each of those pins is held low by {}. Each pull-up passes VCC / R_pu \
             ({:.2} V over the datasheet's pull-up resistance) through the closed path, \
             {} in total{per_pin}, for as long as the part sleeps",
            self.mcu_ref,
            self.part,
            join_names(&pads),
            join_names(&parts),
            self.vdd_v,
            fmt_range(lo, hi),
        );
        if let Some(c) = self.card_max_a {
            let typ = self
                .card_typ_a
                .map_or_else(String::new, |t| format!("{} typical, ", fmt_current(t)));
            let _ = std::fmt::Write::write_fmt(
                &mut s,
                format_args!(
                    ", against the part's own {typ}{} maximum power-down current",
                    fmt_current(c)
                ),
            );
        }
        if self.over_budget {
            if let Some(b) = self.budget_a {
                let _ = std::fmt::Write::write_fmt(
                    &mut s,
                    format_args!(" and over your sleep budget of {}", fmt_current(b)),
                );
            }
        }
        s.push_str(
            ". That current comes off the battery the whole time a switch is closed and the \
             part sleeps. Fix: disable the pull-ups (PORTxn = 0) or make the pins outputs \
             before sleep, and enable them again on wake-up before reading the switches.",
        );
        Some(s)
    }

    /// One summary line for the report whether or not it is flagged.
    #[must_use]
    pub fn summary(&self) -> String {
        let mode = self.mode.as_deref().unwrap_or("sleep");
        match (self.total_min_a, self.total_max_a) {
            (Some(lo), Some(hi)) => format!(
                "{}: asleep ({mode}) with {} internal pull-up(s) on into a low path, {} in total",
                self.mcu_ref,
                self.pins.len(),
                fmt_range(lo, hi)
            ),
            _ => format!(
                "{}: asleep ({mode}) with {} internal pull-up(s) on into a low path, current \
                 unknown ({})",
                self.mcu_ref,
                self.pins.len(),
                self.unknown
                    .as_deref()
                    .unwrap_or("the card states no pull-up resistance")
            ),
        }
    }
}

/// The worst sleeping chunk seen per MCU over the run.
#[derive(Debug, Clone, Default)]
pub struct SleepTracker {
    worst: BTreeMap<String, SleepCurrent>,
}

impl SleepTracker {
    /// Keep `now` when it is the first for its MCU or beats the kept one (a
    /// larger worst-case total; an unknown total never displaces a known one).
    pub fn record(&mut self, now: SleepCurrent) {
        match self.worst.get(&now.mcu_ref) {
            Some(kept) if kept.total_max_a.unwrap_or(0.0) >= now.total_max_a.unwrap_or(0.0) => {}
            _ => {
                self.worst.insert(now.mcu_ref.clone(), now);
            }
        }
    }

    /// Every MCU that slept with a pull-up into a low path, by reference.
    #[must_use]
    pub fn all(&self) -> Vec<SleepCurrent> {
        self.worst.values().cloned().collect()
    }
}

// ─── Scheduler glue ──────────────────────────────────────────────────────────

impl Scheduler {
    /// State a sleep-current budget (A) for the run: a total pull-up current
    /// above it is flagged. `None` (the default) leaves only the
    /// card-relative check. This is the one place the budget can be stated; no
    /// scenario or CLI field carries one yet.
    pub const fn set_sleep_budget_a(&mut self, budget_a: Option<f64>) {
        self.sleep_budget_a = budget_a;
    }

    /// The per-MCU sleep-current accounts of this run: every MCU that went to
    /// sleep with an internal pull-up on into a real low path, at its worst
    /// sleeping chunk. Empty when none did.
    #[must_use]
    pub fn sleep_currents(&self) -> Vec<SleepCurrent> {
        self.sleep_current.all()
    }

    /// The references of the in-process cores that are asleep right now.
    #[must_use]
    pub fn sleeping_mcus(&self) -> Vec<String> {
        self.mcus
            .iter()
            .filter(|m| !backend_is_external(&m.binding.backend) && m.core.state().sleeping)
            .map(|m| m.binding.reference.clone())
            .collect()
    }

    /// The warnings of [`Self::sleep_currents`] that are flagged.
    #[must_use]
    pub fn sleep_current_warnings(&self) -> Vec<String> {
        self.sleep_current
            .all()
            .iter()
            .filter_map(SleepCurrent::message)
            .collect()
    }

    /// The per-chunk hook: for each in-process core asleep at the end of the
    /// chunk, account the pull-ups the firmware left on against the nets the
    /// solve has held low. Runs after a converged solve only, so the voltages
    /// it reads are this chunk's.
    pub(super) fn observe_sleep_current(&mut self) {
        for mi in 0..self.mcus.len() {
            let m = &self.mcus[mi];
            // External cores answer `state()` over a socket; this account is
            // for the in-process core that reads its registers for free.
            if backend_is_external(&m.binding.backend) || !m.core.state().sleeping {
                continue;
            }
            let Some(pull_ups) = m.core.internal_pull_ups().filter(|p| !p.is_empty()) else {
                continue;
            };
            let volts = |n: NodeId| {
                if n.is_ground() {
                    0.0
                } else {
                    self.node_volts.get(n.0 as usize).copied().unwrap_or(0.0)
                }
            };
            let vdd = direct_supply_nodes(&m.binding)
                .into_iter()
                .map(volts)
                .fold(0.0_f64, f64::max);
            if vdd <= 0.0 {
                continue;
            }
            let mut pins = Vec::new();
            for p in pull_ups {
                let Some(driver) = m.binding.gpio_drivers.get(&(p.port, p.bit)) else {
                    continue; // the pad goes nowhere on this board
                };
                let net_v = volts(driver.net);
                if net_v > HELD_LOW_FRACTION * vdd {
                    continue;
                }
                let (low_path, path_ohms) = self.low_path_of(driver.net, vdd, &volts);
                if low_path.is_empty() {
                    continue; // nothing real holds the net: the pin reads high
                }
                let pad = m.binding.pad_roles.iter().find_map(|(pad, role)| {
                    (hauksbee_bind::binder::gpio_of_role(role, m.binding.module)
                        == Some((p.port, p.bit)))
                    .then(|| pad.clone())
                });
                pins.push(LowPathPin {
                    pin: format!("P{}{}", p.port, p.bit),
                    pad,
                    net: self.circuit.node_name(driver.net).to_string(),
                    net_v,
                    low_path,
                    path_ohms,
                });
            }
            if pins.is_empty() {
                continue;
            }
            pins.sort_by(|a, b| a.pin.cmp(&b.pin));
            let mode = m.core.sleep_mode();
            let part = if m.binding.requested_part.is_empty() {
                m.binding.backend.clone()
            } else {
                m.binding.requested_part.clone()
            };
            let account = account(
                (&m.binding.reference, &part),
                mode.as_ref().map(|s| (s.name(), s.is_power_down())),
                vdd,
                self.sim_time,
                pins,
                &m.binding.electrical.sleep,
                self.sleep_budget_a,
            );
            self.sleep_current.record(account);
        }
    }

    /// The parts that hold `net` low and their parallel resistance: the
    /// non-MCU resistive legs under [`WEAK_DIGITAL_DRIVE_OHMS`] whose far end
    /// sits at a low voltage too. An open contact (a gigaohm) is not a path; a
    /// closed one (a few ohms) is.
    fn low_path_of(
        &self,
        net: NodeId,
        vdd: f64,
        volts: &dyn Fn(NodeId) -> f64,
    ) -> (Vec<String>, f64) {
        let mut parts = Vec::new();
        let mut conductance = 0.0_f64;
        for &di in self.digital_in_evidence.get(&net.0).into_iter().flatten() {
            let Some(Device::Resistor {
                name, a, b, ohms, ..
            }) = self.circuit.devices.get(di as usize)
            else {
                continue;
            };
            if *ohms >= WEAK_DIGITAL_DRIVE_OHMS {
                continue;
            }
            let far = if *a == net { *b } else { *a };
            if far == net || volts(far) > HELD_LOW_FRACTION * vdd {
                continue;
            }
            parts.push(part_label(name));
            conductance += 1.0 / ohms.max(1e-6);
        }
        parts.sort();
        parts.dedup();
        let ohms = if conductance > 0.0 {
            1.0 / conductance
        } else {
            f64::INFINITY
        };
        (parts, ohms)
    }
}

/// A stamped control's contact (`Rsw_SW1`) as the part the user knows.
fn part_label(device_name: &str) -> String {
    device_name.strip_prefix("Rsw_").map_or_else(
        || device_name.to_string(),
        |id| format!("{id} (closed switch contact)"),
    )
}

#[cfg(test)]
mod tests {
    use super::{account, fmt_current, pin_current, LowPathPin, SleepTracker, CARD_RATIO_LIMIT};
    use hauksbee_bind::mcu_electrical::SleepSpec;

    fn atmega() -> SleepSpec {
        SleepSpec {
            pull_up_min_ohms: Some(20_000.0),
            pull_up_max_ohms: Some(50_000.0),
            power_down_typ_a: Some(0.1e-6),
            power_down_max_a: Some(5e-6),
        }
    }

    fn pin(name: &str, net_v: f64, path_ohms: f64) -> LowPathPin {
        LowPathPin {
            pin: name.to_string(),
            pad: None,
            net: format!("DIP_{name}"),
            net_v,
            low_path: vec!["SW1".to_string()],
            path_ohms,
        }
    }

    fn acct(pins: Vec<LowPathPin>, spec: &SleepSpec, budget: Option<f64>) -> super::SleepCurrent {
        account(
            ("U1", "ATmega328P"),
            Some(("Power-down", true)),
            3.3,
            0.01,
            pins,
            spec,
            budget,
        )
    }

    #[test]
    fn a_pull_up_into_ground_passes_vdd_over_its_resistance() {
        assert!((pin_current(3.3, 0.0, 0.0, 20_000.0) - 165e-6).abs() < 1e-12);
        assert!((pin_current(3.3, 0.0, 0.0, 50_000.0) - 66e-6).abs() < 1e-12);
    }

    #[test]
    fn the_low_path_resistance_adds_to_the_pull_up() {
        // 10 kOhm to ground in series with 20 kOhm: 3.3 / 30k = 110 uA.
        assert!((pin_current(3.3, 0.0, 10_000.0, 20_000.0) - 110e-6).abs() < 1e-12);
        // A net the solve holds at 0.5 V drops that much from the numerator.
        assert!((pin_current(3.3, 0.5, 0.0, 20_000.0) - 140e-6).abs() < 1e-12);
    }

    #[test]
    fn a_net_at_or_above_the_supply_takes_no_current() {
        assert!(pin_current(3.3, 3.3, 0.0, 20_000.0).abs() < f64::EPSILON);
        assert!(pin_current(3.3, 4.0, 0.0, 20_000.0).abs() < f64::EPSILON);
        assert!(pin_current(f64::NAN, 0.0, 0.0, 20_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn two_closed_pins_range_over_the_cards_pull_up_resistance() {
        let a = acct(
            vec![pin("PD2", 0.0, 0.0), pin("PD3", 0.0, 0.0)],
            &atmega(),
            None,
        );
        assert!((a.total_max_a.unwrap() - 330e-6).abs() < 1e-12);
        assert!((a.total_min_a.unwrap() - 132e-6).abs() < 1e-12);
        assert!(a.unknown.is_none());
        // 330 uA against 5 uA max is 66x: over the card ratio.
        assert!(a.over_card && !a.over_budget && a.flagged());
        let text = a.message().expect("flagged");
        assert!(text.contains("PD2") && text.contains("PD3"), "{text}");
        assert!(text.contains("DIP_PD2") && text.contains("SW1"), "{text}");
        assert!(text.contains("PORTxn = 0"), "{text}");
        assert!(text.contains("132-330 uA"), "{text}");
    }

    #[test]
    fn the_card_ratio_threshold_is_ten_times_the_maximum() {
        assert!((CARD_RATIO_LIMIT - 10.0).abs() < f64::EPSILON);
        // One pin at 3.3 V over 50k..20k is 66..165 uA, over 10 x 5 uA = 50 uA.
        let one = acct(vec![pin("PD2", 0.0, 0.0)], &atmega(), None);
        assert!(one.over_card);
        // A 10 MOhm path: 3.3 / 10.02M = 0.33 uA, far under.
        let weak = acct(vec![pin("PD2", 0.0, 10e6)], &atmega(), None);
        assert!(!weak.over_card && !weak.flagged());
        assert!(weak.message().is_none());
        // Exactly at the limit is not over it.
        let mut spec = atmega();
        spec.power_down_max_a = Some(165e-6 / 10.0);
        assert!(!acct(vec![pin("PD2", 0.0, 0.0)], &spec, None).over_card);
    }

    #[test]
    fn the_card_ratio_only_applies_to_power_down() {
        let idle = account(
            ("U1", "ATmega328P"),
            Some(("Idle", false)),
            3.3,
            0.0,
            vec![pin("PD2", 0.0, 0.0)],
            &atmega(),
            None,
        );
        assert!(!idle.over_card && !idle.flagged());
        // The budget still applies in any mode.
        let idle_budget = account(
            ("U1", "ATmega328P"),
            Some(("Idle", false)),
            3.3,
            0.0,
            vec![pin("PD2", 0.0, 0.0)],
            &atmega(),
            Some(100e-6),
        );
        assert!(idle_budget.over_budget);
        assert!(idle_budget.message().unwrap().contains("budget of 100 uA"));
    }

    #[test]
    fn a_budget_above_the_worst_case_does_not_flag() {
        let mut spec = atmega();
        spec.power_down_max_a = None; // no card ratio, budget only
        let one = acct(vec![pin("PD2", 0.0, 0.0)], &spec, Some(200e-6));
        assert!(!one.flagged());
        let tight = acct(vec![pin("PD2", 0.0, 0.0)], &spec, Some(100e-6));
        assert!(tight.over_budget && tight.flagged());
    }

    #[test]
    fn a_card_without_the_pull_up_resistance_reports_unknown_and_never_flags() {
        let spec = SleepSpec {
            power_down_max_a: Some(5e-6),
            ..SleepSpec::default()
        };
        let a = acct(vec![pin("PD2", 0.0, 0.0)], &spec, Some(1e-6));
        assert_eq!(a.total_max_a, None);
        assert_eq!(a.total_min_a, None);
        assert!(!a.flagged());
        let why = a.unknown.as_deref().unwrap();
        assert!(why.contains("io_pull_up_min_ohms"), "{why}");
        assert!(why.contains("io_pull_up_max_ohms"), "{why}");
        let half = SleepSpec {
            pull_up_min_ohms: Some(20_000.0),
            ..SleepSpec::default()
        };
        let a = acct(vec![pin("PD2", 0.0, 0.0)], &half, None);
        assert!(a
            .unknown
            .as_deref()
            .unwrap()
            .contains("io_pull_up_max_ohms"));
        assert!(!a.flagged());
    }

    #[test]
    fn the_tracker_keeps_the_worst_chunk_per_mcu() {
        let mut t = SleepTracker::default();
        t.record(acct(vec![pin("PD2", 0.0, 0.0)], &atmega(), None));
        t.record(acct(
            vec![pin("PD2", 0.0, 0.0), pin("PD3", 0.0, 0.0)],
            &atmega(),
            None,
        ));
        t.record(acct(vec![pin("PD2", 0.0, 0.0)], &atmega(), None));
        let all = t.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].pins.len(), 2);
    }

    #[test]
    fn currents_print_in_the_unit_a_person_reads() {
        assert_eq!(fmt_current(165e-6), "165 uA");
        assert_eq!(fmt_current(0.1e-6), "0.10 uA");
        assert_eq!(fmt_current(2.5e-3), "2.50 mA");
    }
}
