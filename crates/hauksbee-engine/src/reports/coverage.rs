//! The one enumeration of per-run co-sim coverage caveats: what a finished (or
//! in-flight) co-simulation could NOT do, in a form every surface can render.
//!
//! Why this module exists. Each coverage class already had exactly one wording
//! (`AdcDrop::message`, `UnexercisedBus::message`, `ShortPulse::message`,
//! `DriverContention::message`, `scheduler::watchdog_limitation_message`,
//! `scheduler::watchdog_reset_message`, `scheduler::timing_limitation_message`,
//! `scheduler::timing_coverage_line`, `Scheduler::drive_conflicts`,
//! `reports::cosim::heuristic_framing_warnings`) but no single place said WHICH
//! classes exist. Every surface re-listed them by hand, and the interactive
//! surfaces fell behind: `docs/cosim/MCU.md` measured the interactive TUI
//! carrying only a subset of the typed classes while batch surfaces carried
//! more of them. A user watching a
//! live co-sim saw a quiet pane over a run whose ADC channel was dropped.
//!
//! [`CoverageInputs::from_scheduler`] is the single extraction point from the
//! scheduler, and [`CoverageInputs::caveats`] the single enumeration. Every
//! sentence it emits is produced by the formatter that already owned that
//! wording; nothing here paraphrases one, so a surface reading this list says
//! what the batch surfaces say, in the same words.
//!
//! A consumer that renders the WHOLE list (the TUI pane, its coverage overlay)
//! cannot fall behind a new class. A consumer that picks classes out of it (the
//! web front door, which already carried five through its own findings) names
//! the ones it takes.

use crate::scheduler::{
    timing_coverage_line, timing_limitation_message, watchdog_limitation_message,
    watchdog_reset_message, AdcDrop, DriverContention, Scheduler, ShortPulse, TimingCoverage,
    UnexercisedBus,
};

/// Semantic tier of a co-sim disclosure. These are deliberately not collapsed
/// into one warning count: a strict-invalid timing refusal is stronger than a
/// bounded limitation, while a measured timing floor and a fallback-method
/// qualification are evidence about resolution rather than missing coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageDisposition {
    Limitation,
    TimingBound,
    StrictRefusal,
    FallbackQualification,
    /// A run-state transition that actually happened and changes how later
    /// observations must be read; it is evidence, not missing coverage.
    ObservedEvent,
    /// An electrical conflict the co-simulation actually observed.
    ElectricalFault,
}

/// One class of co-sim coverage disclosure. Twelve of them, which is the count
/// `docs/cosim/MCU.md`'s per-surface matrix rows are counted from;
/// [`CoverageClass::ALL`] is the list and `class_count_matches_the_matrix`
/// counts it rather than asserting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageClass {
    /// An ADC channel the analog solve drove whose injections the backend threw
    /// away: the firmware never received a sample.
    AdcDropped,
    /// A bus peripheral on a platform modelling no matching controller: the
    /// firmware's traffic could never reach it.
    UnexercisedBus,
    /// This backend's armed watchdog cannot bite, so hung firmware runs forever.
    WatchdogLimitation,
    /// The watchdog DID reboot the core, so behaviour after it belongs to a
    /// rebooted core.
    WatchdogReboot,
    /// A known systematic bias in this backend's virtual time.
    TimingLimitation,
    /// The measured edge-timestamp and minimum-observable-pulse resolution of a
    /// live core at the chunk actually run. The one class that is a resolution
    /// STATEMENT rather than a hole (see [`CoverageClass::is_hole`]).
    TimingCoverage,
    /// A runtime edge stream exceeded the replay implementation budget. The
    /// analog path retained only the final DC level, so strict evidence is invalid.
    TimingRefusal,
    /// A converged span produced by a second-class integration rung after the
    /// primary method failed, with its method and measured estimate attached.
    FallbackIntegration,
    /// A GPIO pulse that rose and fell inside one solver chunk, invisible to
    /// tick-evaluated sequential parts.
    ShortPulse,
    /// Firmware drove a net a modelled push-pull output was already driving.
    DriverContention,
    /// A requested drive lost to a co-located source or a post-solve override.
    DriveConflict,
    /// An SPI bus whose transaction boundaries are guessed at chunk edges.
    HeuristicSpiFraming,
}

impl CoverageClass {
    /// Every class, in the order the caveat list emits them.
    pub const ALL: [CoverageClass; 12] = [
        CoverageClass::AdcDropped,
        CoverageClass::UnexercisedBus,
        CoverageClass::WatchdogLimitation,
        CoverageClass::WatchdogReboot,
        CoverageClass::TimingLimitation,
        CoverageClass::TimingCoverage,
        CoverageClass::TimingRefusal,
        CoverageClass::FallbackIntegration,
        CoverageClass::ShortPulse,
        CoverageClass::DriverContention,
        CoverageClass::DriveConflict,
        CoverageClass::HeuristicSpiFraming,
    ];

    /// A short label for a cramped surface (the TUI pane is a quarter of a
    /// terminal wide, so the banner lists labels, not sentences).
    pub fn label(self) -> &'static str {
        match self {
            CoverageClass::AdcDropped => "ADC drop",
            CoverageClass::UnexercisedBus => "bus never exercised",
            CoverageClass::WatchdogLimitation => "watchdog cannot bite",
            CoverageClass::WatchdogReboot => "watchdog reboot",
            CoverageClass::TimingLimitation => "timing bias",
            CoverageClass::TimingCoverage => "timing resolution",
            CoverageClass::TimingRefusal => "timing replay invalid",
            CoverageClass::FallbackIntegration => "fallback-qualified span",
            CoverageClass::ShortPulse => "pulse too short to see",
            CoverageClass::DriverContention => "driver contention",
            CoverageClass::DriveConflict => "drive overridden",
            CoverageClass::HeuristicSpiFraming => "SPI framing guessed",
        }
    }

    /// True when the class names something the run could NOT do. False for a
    /// resolution statement that is present on every run with a live core
    /// ([`CoverageClass::TimingCoverage`]): counting it as a hole would put a
    /// permanent red number on a healthy run, which is how a caveat stops being
    /// read at all.
    pub fn is_hole(self) -> bool {
        matches!(self.disposition(), CoverageDisposition::Limitation)
    }

    pub fn disposition(self) -> CoverageDisposition {
        match self {
            CoverageClass::TimingCoverage => CoverageDisposition::TimingBound,
            CoverageClass::TimingRefusal => CoverageDisposition::StrictRefusal,
            CoverageClass::FallbackIntegration => CoverageDisposition::FallbackQualification,
            CoverageClass::WatchdogReboot | CoverageClass::DriveConflict => {
                CoverageDisposition::ObservedEvent
            }
            CoverageClass::DriverContention => CoverageDisposition::ElectricalFault,
            _ => CoverageDisposition::Limitation,
        }
    }
}

/// One caveat, ready to render on any surface.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageCaveat {
    pub class: CoverageClass,
    /// What it is about: an MCU reference, a net, or a bus/peripheral id.
    pub subject: String,
    /// A one-line headline for a card or a list row.
    pub headline: String,
    /// The whole sentence, verbatim from the formatter the batch surfaces use.
    /// Never paraphrased here.
    pub message: String,
    /// The next action appropriate to this disclosure: close a coverage gap,
    /// refine a bound, rerun a refusal, respond to an event, or fix a fault.
    /// Kept as `fix` for the existing report-card wire contract.
    pub fix: String,
}

impl CoverageCaveat {
    /// True when this caveat names something the run could not do.
    pub fn is_hole(&self) -> bool {
        self.class.is_hole()
    }
}

/// The scheduler signals the caveat list is built from, copied out so the
/// enumeration is testable without staging a whole board through the engine and
/// so a live TUI worker can snapshot them at a chunk boundary.
#[derive(Debug, Clone, Default)]
pub struct CoverageInputs {
    pub adc_dropped: Vec<AdcDrop>,
    pub unexercised_buses: Vec<UnexercisedBus>,
    pub watchdog_limitations: Vec<(String, String)>,
    pub watchdog_resets: Vec<(String, u64)>,
    pub timing_limitations: Vec<(String, String)>,
    pub timing_coverage: Vec<TimingCoverage>,
    pub timing_refusals: Vec<String>,
    pub fallback_windows: Vec<crate::result::CosimFallbackWindow>,
    pub short_pulses: Vec<ShortPulse>,
    pub driver_contentions: Vec<DriverContention>,
    pub drive_conflicts: Vec<String>,
    pub heuristic_spi_buses: Vec<String>,
}

impl CoverageInputs {
    /// The single extraction point: read every coverage signal off the live
    /// scheduler. Any surface that wants coverage caveats comes through here,
    /// so a surface cannot read four of the ten accessors and call it coverage.
    pub fn from_scheduler(sched: &Scheduler) -> Self {
        Self {
            adc_dropped: sched.adc_dropped(),
            unexercised_buses: sched.unexercised_buses().to_vec(),
            watchdog_limitations: sched.watchdog_limitations(),
            watchdog_resets: sched.watchdog_resets(),
            timing_limitations: sched.timing_limitations(),
            timing_coverage: sched.timing_coverage(),
            timing_refusals: sched.timing_refusals().to_vec(),
            fallback_windows: sched
                .fallback_windows()
                .iter()
                .map(|window| crate::result::CosimFallbackWindow {
                    start_s: window.start_s,
                    end_s: window.end_s,
                    method: window.method.as_str().to_string(),
                    fidelity_note: window.method.fidelity_note().to_string(),
                    error_estimate_v: window.error_estimate_v,
                })
                .collect(),
            short_pulses: sched.short_pulses().to_vec(),
            driver_contentions: sched.driver_contentions().to_vec(),
            drive_conflicts: sched.drive_conflicts(),
            heuristic_spi_buses: sched
                .spi_framing_modes()
                .into_iter()
                .filter(|(_, mode)| mode.as_str() == "heuristic")
                .map(|(bus, _)| bus)
                .collect(),
        }
    }

    /// The caveats these signals carry, in [`CoverageClass::ALL`] order. Empty
    /// on a run with nothing to disclose and no live core, which is what makes a
    /// non-empty list worth reading.
    pub fn caveats(&self) -> Vec<CoverageCaveat> {
        let mut out: Vec<CoverageCaveat> = Vec::new();
        for d in &self.adc_dropped {
            out.push(CoverageCaveat {
                class: CoverageClass::AdcDropped,
                subject: d.mcu_ref.clone(),
                headline: format!(
                    "ADC channel {} on {} (net '{}') never reached the firmware",
                    d.channel, d.mcu_ref, d.net
                ),
                message: d.message(),
                fix: "Add an [[soc.adc]] injection recipe to this platform's SoC \
                      descriptor, or read the pin over a bus the platform does model."
                    .to_string(),
            });
        }
        for b in &self.unexercised_buses {
            out.push(CoverageCaveat {
                class: CoverageClass::UnexercisedBus,
                subject: b.id.clone(),
                headline: format!(
                    "{} device '{}' was never exercised (no modelled controller)",
                    b.bus, b.id
                ),
                message: b.message(),
                fix: format!(
                    "Add [soc.{}] controllers to this platform's SoC descriptor; until \
                     then treat this device's state as its power-on default.",
                    b.bus.to_ascii_lowercase()
                ),
            });
        }
        for (mcu_ref, limitation) in &self.watchdog_limitations {
            out.push(CoverageCaveat {
                class: CoverageClass::WatchdogLimitation,
                subject: mcu_ref.clone(),
                headline: format!("{mcu_ref}: the watchdog on this backend cannot bite"),
                message: watchdog_limitation_message(mcu_ref, limitation),
                fix: "Nothing this run can assert about hang recovery holds. Run the \
                      recovery path on a backend whose watchdog fires (simavr does) or \
                      on hardware."
                    .to_string(),
            });
        }
        for (mcu_ref, resets) in &self.watchdog_resets {
            out.push(CoverageCaveat {
                class: CoverageClass::WatchdogReboot,
                subject: mcu_ref.clone(),
                headline: format!("{mcu_ref}: the watchdog rebooted the core {resets}x"),
                message: watchdog_reset_message(mcu_ref, *resets),
                fix: "Read anything after the first reboot as a rebooted core's \
                      behaviour. Feed the watchdog in firmware, or shorten the window \
                      to before the first reset, to measure the run you meant."
                    .to_string(),
            });
        }
        for (mcu_ref, limitation) in &self.timing_limitations {
            out.push(CoverageCaveat {
                class: CoverageClass::TimingLimitation,
                subject: mcu_ref.clone(),
                headline: format!("{mcu_ref}: this backend's virtual time is biased"),
                message: timing_limitation_message(mcu_ref, limitation),
                fix: "Treat time-based assertions on this core as approximate; measure \
                      them on a backend without the bias, or on hardware."
                    .to_string(),
            });
        }
        for t in &self.timing_coverage {
            out.push(CoverageCaveat {
                class: CoverageClass::TimingCoverage,
                subject: t.mcu_ref.clone(),
                headline: format!(
                    "{}: pulses under {:.3} us are not guaranteed observable",
                    t.mcu_ref,
                    t.minimum_guaranteed_pulse_s * 1e6
                ),
                message: timing_coverage_line(t),
                fix: "Narrow the solver chunk (--chunk-us) if you need finer edges; \
                      this is the resolution the run actually delivered, not a defect."
                    .to_string(),
            });
        }
        for refusal in &self.timing_refusals {
            out.push(CoverageCaveat {
                class: CoverageClass::TimingRefusal,
                subject: String::new(),
                headline: "timing replay was refused; strict evidence is invalid".to_string(),
                message: format!("TIMING INVALID: {refusal}"),
                fix: "Reduce the transitions per solver chunk (for example with a narrower \
                      --chunk-us), then rerun the same firmware so the analog path can replay \
                      every edge instead of retaining only the final DC level."
                    .to_string(),
            });
        }
        for window in &self.fallback_windows {
            out.push(CoverageCaveat {
                class: CoverageClass::FallbackIntegration,
                subject: format!("{:.6}-{:.6}s", window.start_s, window.end_s),
                headline: format!(
                    "{:.3}-{:.3} ms used fallback integration",
                    window.start_s * 1e3,
                    window.end_s * 1e3
                ),
                message: fallback_qualification_message(window),
                fix: "Treat this span as numerically second-class. Narrow the chunk or fix the \
                      primary solve failure, then rerun if the fallback's damping or measured \
                      chunk-end error matters to the conclusion."
                    .to_string(),
            });
        }
        for p in &self.short_pulses {
            out.push(CoverageCaveat {
                class: CoverageClass::ShortPulse,
                subject: p.net.clone(),
                headline: format!(
                    "net '{}' carries a pulse shorter than the solver chunk",
                    p.net
                ),
                message: p.message(),
                fix: format!(
                    "Rerun with --chunk-us {:.1} (a chunk no wider than half the pulse), \
                     or widen the pulse in firmware.",
                    (p.pulse_s * 1e6 / 2.0).max(0.1)
                ),
            });
        }
        for c in &self.driver_contentions {
            out.push(CoverageCaveat {
                class: CoverageClass::DriverContention,
                subject: c.net.clone(),
                headline: format!("driver contention on net '{}'", c.net),
                message: c.message(),
                fix: "Check the model pin mapping (`hauksbee models resolve`) and the \
                      firmware's pin-direction writes; two push-pull drivers must never \
                      share a net without a series element."
                    .to_string(),
            });
        }
        for msg in &self.drive_conflicts {
            out.push(CoverageCaveat {
                class: CoverageClass::DriveConflict,
                subject: String::new(),
                headline: "a requested drive was overridden on its net".to_string(),
                message: msg.clone(),
                fix: "Remove the losing source, or suppress the rail that pins the net, \
                      so the drive you asked for is the one that takes effect."
                    .to_string(),
            });
        }
        // The canonical heuristic-framing sentence, from the formatter the
        // default text summary, `--plain` and the `--json` notes already share.
        let framing: Vec<(String, crate::peripherals::SpiFramingMode)> = self
            .heuristic_spi_buses
            .iter()
            .map(|bus| (bus.clone(), crate::peripherals::SpiFramingMode::Heuristic))
            .collect();
        for (bus, message) in self
            .heuristic_spi_buses
            .iter()
            .zip(crate::reports::cosim::heuristic_framing_warnings(&framing))
        {
            out.push(CoverageCaveat {
                class: CoverageClass::HeuristicSpiFraming,
                subject: bus.clone(),
                headline: format!("SPI bus '{bus}' framing is guessed at chunk edges"),
                message,
                fix: "Declare cs_net on the peripheral, or point it at a board component \
                      whose model maps a `cs` pin, for exact framing."
                    .to_string(),
            });
        }
        out
    }
}

/// How many of the caveats in `caveats` name something the run could not do
/// (as opposed to a resolution statement).
pub fn hole_count(caveats: &[CoverageCaveat]) -> usize {
    caveats.iter().filter(|c| c.is_hole()).count()
}

pub fn disposition_count(caveats: &[CoverageCaveat], disposition: CoverageDisposition) -> usize {
    caveats
        .iter()
        .filter(|caveat| caveat.class.disposition() == disposition)
        .count()
}

pub fn fallback_qualification_message(window: &crate::result::CosimFallbackWindow) -> String {
    let estimate = window
        .error_estimate_v
        .map(|value| format!("measured chunk-end error estimate {value:.3e} V"))
        .unwrap_or_else(|| "no measured error estimate (no companion re-solve converged)".into());
    format!(
        "[{:.6}s .. {:.6}s) via {}; {}; {}",
        window.start_s, window.end_s, window.method, estimate, window.fidelity_note
    )
}

/// The distinct classes present in `caveats`, in [`CoverageClass::ALL`] order,
/// so a cramped surface can list "which kinds" without repeating a class once
/// per offending net.
pub fn classes_present(caveats: &[CoverageCaveat]) -> Vec<CoverageClass> {
    CoverageClass::ALL
        .into_iter()
        .filter(|class| caveats.iter().any(|c| c.class == *class))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adc_drop(mcu: &str, channel: u8, net: &str) -> AdcDrop {
        AdcDrop {
            mcu_ref: mcu.to_string(),
            channel,
            net: net.to_string(),
            parts: vec!["R1".to_string()],
        }
    }

    fn timing_row(mcu: &str, cycle_exact: bool) -> TimingCoverage {
        TimingCoverage {
            mcu_ref: mcu.to_string(),
            backend: if cycle_exact {
                "simavr:atmega328p".to_string()
            } else {
                "renode:stm32f103".to_string()
            },
            cycle_exact,
            timestamp_precision_s: 6.25e-8,
            minimum_guaranteed_pulse_s: 1.25e-7,
            chunk_s: 1e-3,
        }
    }

    /// The matrix in `docs/cosim/MCU.md` is written against a counted number of
    /// classes, not an asserted one. If a class is added here without the doc
    /// being recounted, this fails.
    #[test]
    fn class_count_matches_the_matrix() {
        assert_eq!(CoverageClass::ALL.len(), 12);
        assert_eq!(
            CoverageClass::ALL
                .into_iter()
                .filter(|class| class.is_hole())
                .count(),
            6,
            "only missing observability/model support is a hole; observed events and faults are not"
        );
        assert!(!CoverageClass::WatchdogReboot.is_hole());
        assert!(!CoverageClass::DriverContention.is_hole());
        assert!(!CoverageClass::DriveConflict.is_hole());
        assert_eq!(
            CoverageClass::TimingCoverage.disposition(),
            CoverageDisposition::TimingBound
        );
        assert_eq!(
            CoverageClass::TimingRefusal.disposition(),
            CoverageDisposition::StrictRefusal
        );
        assert_eq!(
            CoverageClass::FallbackIntegration.disposition(),
            CoverageDisposition::FallbackQualification
        );
    }

    /// Every class must be reachable from `caveats()`. A class that exists in
    /// the enum but is never emitted would read as covered on a surface that
    /// renders the whole list.
    #[test]
    fn every_class_is_emitted_when_its_signal_is_present() {
        let inputs = CoverageInputs {
            adc_dropped: vec![adc_drop("U1", 4, "/VSENSE")],
            unexercised_buses: vec![UnexercisedBus {
                id: "IMU1".to_string(),
                bus: "I2C",
                controller: None,
            }],
            watchdog_limitations: vec![("U1".to_string(), "the watchdog never fires".to_string())],
            watchdog_resets: vec![("U1".to_string(), 3)],
            timing_limitations: vec![("U1".to_string(), "virtual time runs slow".to_string())],
            timing_coverage: vec![timing_row("U1", false)],
            timing_refusals: vec![
                "PWL replay refused on net /CLK: transition budget exceeded".to_string()
            ],
            fallback_windows: vec![crate::result::CosimFallbackWindow {
                start_s: 0.001,
                end_s: 0.002,
                method: "backward-euler".to_string(),
                fidelity_note: "first-order and numerically dissipative".to_string(),
                error_estimate_v: Some(0.012),
            }],
            short_pulses: vec![ShortPulse {
                net: "/STROBE".to_string(),
                mcu_ref: "U1".to_string(),
                port: 'B',
                bit: 5,
                pulse_s: 2e-6,
                chunk_s: 1e-4,
                parts: vec!["U7".to_string()],
            }],
            driver_contentions: vec![DriverContention {
                net: "/LED".to_string(),
                mcu_ref: "U1".to_string(),
                port: 'B',
                bit: 5,
                parts: vec!["U9.out".to_string()],
                t_s: 0.01,
            }],
            drive_conflicts: vec!["net '/VBUS' is overridden to 20.000 V".to_string()],
            heuristic_spi_buses: vec!["ADC1".to_string()],
        };
        let caveats = inputs.caveats();
        for class in CoverageClass::ALL {
            assert!(
                caveats.iter().any(|c| c.class == class),
                "class {class:?} is never emitted"
            );
        }
        assert_eq!(caveats.len(), 12, "one caveat per class in this fixture");
        assert_eq!(hole_count(&caveats), 6);
        assert_eq!(classes_present(&caveats), CoverageClass::ALL.to_vec());
        // Every disclosure names a concrete next action.
        for c in &caveats {
            assert!(!c.fix.trim().is_empty(), "{:?} has no fix", c.class);
            assert!(!c.message.trim().is_empty(), "{:?} has no message", c.class);
        }
    }

    /// The silence control: nothing to disclose emits nothing, so a non-empty
    /// list on a surface means a real signal fired.
    #[test]
    fn a_clean_run_emits_no_caveats() {
        let caveats = CoverageInputs::default().caveats();
        assert!(caveats.is_empty(), "{caveats:?}");
        assert_eq!(hole_count(&caveats), 0);
        assert!(classes_present(&caveats).is_empty());
    }

    /// Wording is not re-invented here: each caveat's `message` is the sentence
    /// the batch surfaces print, character for character.
    #[test]
    fn messages_come_from_the_shared_formatters_verbatim() {
        let drop = adc_drop("U2", 7, "/NTC");
        let row = timing_row("U2", false);
        let inputs = CoverageInputs {
            adc_dropped: vec![drop.clone()],
            watchdog_resets: vec![("U2".to_string(), 1)],
            timing_coverage: vec![row.clone()],
            ..Default::default()
        };
        let caveats = inputs.caveats();
        let by = |class: CoverageClass| {
            caveats
                .iter()
                .find(|c| c.class == class)
                .expect("class present")
                .message
                .clone()
        };
        assert_eq!(by(CoverageClass::AdcDropped), drop.message());
        assert_eq!(
            by(CoverageClass::WatchdogReboot),
            watchdog_reset_message("U2", 1)
        );
        assert_eq!(
            by(CoverageClass::TimingCoverage),
            timing_coverage_line(&row)
        );
    }

    /// The timing-resolution statement tracks the backend it came from: a poll
    /// backend says poll-boundary, a cycle-exact one says cycle-exact. Both
    /// sides, so the line is a measurement rather than boilerplate.
    #[test]
    fn timing_resolution_reports_the_measured_tier_on_both_sides() {
        let poll = CoverageInputs {
            timing_coverage: vec![timing_row("U1", false)],
            ..Default::default()
        }
        .caveats();
        assert!(poll[0].message.contains("poll-boundary"), "{:?}", poll[0]);
        let exact = CoverageInputs {
            timing_coverage: vec![timing_row("U1", true)],
            ..Default::default()
        }
        .caveats();
        assert!(exact[0].message.contains("cycle-exact"), "{:?}", exact[0]);
        // And with no live core there is no row at all.
        assert!(CoverageInputs::default().caveats().is_empty());
    }

    /// One SPI caveat per heuristic bus, each naming its own bus, so a
    /// two-bus board cannot report one bus's guess twice.
    #[test]
    fn one_spi_framing_caveat_per_heuristic_bus() {
        let caveats = CoverageInputs {
            heuristic_spi_buses: vec!["ADC1".to_string(), "FLASH1".to_string()],
            ..Default::default()
        }
        .caveats();
        assert_eq!(caveats.len(), 2);
        assert_eq!(caveats[0].subject, "ADC1");
        assert!(caveats[0].message.contains("ADC1"), "{:?}", caveats[0]);
        assert_eq!(caveats[1].subject, "FLASH1");
        assert!(caveats[1].message.contains("FLASH1"), "{:?}", caveats[1]);
    }

    #[test]
    fn web_matrix_marks_external_only_disclosures_as_not_run() {
        let docs = include_str!("../../../../docs/cosim/MCU.md");
        for label in [
            "dropped ADC injections",
            "unexercised buses",
            "watchdog limitation",
            "timing limitation",
        ] {
            let row = docs
                .lines()
                .find(|line| line.starts_with(&format!("| {label} |")))
                .unwrap_or_else(|| panic!("missing matrix row for {label}"));
            let web_cell = row
                .trim_matches('|')
                .split('|')
                .next_back()
                .expect("web-frontdoor cell")
                .trim();
            assert_eq!(
                web_cell, "not run (external backend)",
                "the synchronous web front door returns before these scheduler signals exist: {row}"
            );
        }
    }
}
