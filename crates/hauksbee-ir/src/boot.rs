//! What a chip does at reset, decided by the levels its boot-strap pins hold.
//!
//! A microcontroller picks its boot source from a few pins sampled as reset is
//! released (an STM32's BOOT0, an ESP32's GPIO0 and GPIO2, an RP2040's
//! `QSPI_SS`). The model cards carry the vendor's table; the co-simulator
//! resolves it against the solved board and produces a [`BootDecision`]. This
//! module is the vocabulary of that decision: the outcome words, the reading of
//! each strap and the one sentence every surface (run JSON, CLI, app) shows, so
//! no renderer words it again.

use serde::{Deserialize, Serialize};

/// Where the chip's reset sequence sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BootOutcome {
    /// The application in the normal flash runs.
    UserFlash,
    /// The vendor's ROM bootloader (or download mode) runs; the user firmware
    /// does not.
    RomBootloader,
    /// The chip jumps into SRAM; user firmware runs only if something loaded
    /// it there.
    Sram,
    /// The reset line is asserted, so the chip never starts.
    HeldInReset,
    /// The strap levels fall in a combination the vendor marks reserved, or the
    /// card has no row for them.
    Undefined,
}

impl BootOutcome {
    /// Every outcome, in the order the table lists them.
    pub const ALL: [Self; 5] = [
        Self::UserFlash,
        Self::RomBootloader,
        Self::Sram,
        Self::HeldInReset,
        Self::Undefined,
    ];

    /// The machine id (the serialised word).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserFlash => "user_flash",
            Self::RomBootloader => "rom_bootloader",
            Self::Sram => "sram",
            Self::HeldInReset => "held_in_reset",
            Self::Undefined => "undefined",
        }
    }

    /// Parse the machine id.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == s)
    }

    /// The words a reader sees.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UserFlash => "Runs the user firmware",
            Self::RomBootloader => "Enters the ROM bootloader",
            Self::Sram => "Boots from SRAM",
            Self::HeldInReset => "Held in reset",
            Self::Undefined => "Boot mode undefined",
        }
    }

    /// One line saying what it means, for a tooltip.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::UserFlash => "The chip starts the application stored in its normal flash memory.",
            Self::RomBootloader => {
                "The chip runs the manufacturer's built-in loader or download mode instead of your firmware."
            }
            Self::Sram => "The chip starts from its on-chip RAM, which holds code only if something loaded it.",
            Self::HeldInReset => "The reset pin is held active, so the chip never starts running.",
            Self::Undefined => {
                "The boot pins are in a combination the manufacturer marks reserved, or are not at a defined level."
            }
        }
    }

    /// Whether the application firmware gets to run in this outcome.
    #[must_use]
    pub const fn runs_user_firmware(self) -> bool {
        matches!(self, Self::UserFlash)
    }
}

/// The level a strap pin was read at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StrapReadingLevel {
    Low,
    High,
    /// Between the chip's low and high thresholds, or no voltage was solved.
    Indeterminate,
}

impl StrapReadingLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// One strap pin as the chip saw it at reset release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StrapReading {
    /// The pin role in the model card (`boot0`, `gpio0`).
    pub role: String,
    /// The pin's name for a reader (`BOOT0`, `GPIO0`).
    pub pin: String,
    /// The board net on the pin, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
    /// The solved voltage on the pin, when the board defines one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volts: Option<f64>,
    pub level: StrapReadingLevel,
    /// Why the pin reads that level when the voltage alone does not say: an
    /// internal pull on an undriven pad, or the net left unconnected.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub basis: String,
}

impl StrapReading {
    /// `BOOT0 = 3.30 V (high)`, or `GPIO0 floating, internal pull-up (high)`.
    #[must_use]
    pub fn describe(&self) -> String {
        let level = self.level.as_str();
        match (self.volts, self.basis.is_empty()) {
            (Some(v), true) => format!("{} = {v:.2} V ({level})", self.pin),
            (Some(v), false) => format!("{} = {v:.2} V ({level}; {})", self.pin, self.basis),
            (None, false) => format!("{} {} ({level})", self.pin, self.basis),
            (None, true) => format!("{} ({level})", self.pin),
        }
    }
}

/// The boot mode the chip reached and why. Carried in the run JSON as
/// `cosim.execution[].boot` and printed by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BootDecision {
    pub outcome: BootOutcome,
    /// The vendor's name for the mode (`System memory`, `SPI Boot`).
    pub mode: String,
    /// Each strap pin the table reads, as the chip saw it.
    pub straps: Vec<StrapReading>,
    /// Whether the application firmware ran, or was allowed to.
    pub user_firmware_runs: bool,
    /// The whole finding in one sentence (see [`BootDecision::sentence`]).
    pub summary: String,
    /// The vendor document and table the decision comes from.
    pub cite: String,
    /// Things the reader should weigh: a strap that moved between the solved
    /// start-up point and reset release, a level taken from an internal pull.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

impl BootDecision {
    /// Build a decision and its sentence.
    #[must_use]
    pub fn new(
        outcome: BootOutcome,
        mode: &str,
        straps: Vec<StrapReading>,
        cite: &str,
        caveats: Vec<String>,
    ) -> Self {
        let summary = Self::sentence(outcome, mode, &straps);
        Self {
            outcome,
            mode: mode.to_string(),
            user_firmware_runs: outcome.runs_user_firmware(),
            straps,
            summary,
            cite: cite.to_string(),
            caveats,
        }
    }

    /// `BOOT0 = 3.30 V (high) at reset release -> System memory; user firmware
    /// not run.` The one wording of a boot decision.
    #[must_use]
    pub fn sentence(outcome: BootOutcome, mode: &str, straps: &[StrapReading]) -> String {
        let pins = if straps.is_empty() {
            "no boot-strap pin".to_string()
        } else {
            straps
                .iter()
                .map(StrapReading::describe)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mode = if mode.is_empty() {
            outcome.label().to_lowercase()
        } else {
            mode.to_string()
        };
        let tail = match outcome {
            BootOutcome::UserFlash => "user firmware runs",
            BootOutcome::RomBootloader => "user firmware not run",
            BootOutcome::Sram => "user firmware runs only if it was loaded into RAM",
            BootOutcome::HeldInReset => "the chip never starts",
            BootOutcome::Undefined => "the vendor does not define this combination",
        };
        format!("{pins} at reset release -> {mode}; {tail}")
    }
}

/// The label-table rows for the outcomes: `(id, label, explanation)`.
#[must_use]
pub fn outcome_rows() -> Vec<(&'static str, &'static str, &'static str)> {
    BootOutcome::ALL
        .iter()
        .map(|o| (o.as_str(), o.label(), o.meaning()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot0(volts: f64, level: StrapReadingLevel) -> StrapReading {
        StrapReading {
            role: "boot0".into(),
            pin: "BOOT0".into(),
            net: Some("BOOT0".into()),
            volts: Some(volts),
            level,
            basis: String::new(),
        }
    }

    #[test]
    fn the_sentence_names_the_pin_the_level_the_mode_and_the_consequence() {
        let d = BootDecision::new(
            BootOutcome::RomBootloader,
            "System memory bootloader",
            vec![boot0(3.3, StrapReadingLevel::High)],
            "RM0008 Table 9",
            vec![],
        );
        assert_eq!(
            d.summary,
            "BOOT0 = 3.30 V (high) at reset release -> System memory bootloader; user firmware not run"
        );
        assert!(!d.user_firmware_runs);
    }

    #[test]
    fn an_internal_pull_is_named_as_the_basis() {
        let r = StrapReading {
            role: "gpio0".into(),
            pin: "GPIO0".into(),
            net: None,
            volts: None,
            level: StrapReadingLevel::High,
            basis: "left floating, internal pull-up".into(),
        };
        assert_eq!(r.describe(), "GPIO0 left floating, internal pull-up (high)");
    }

    #[test]
    fn outcome_ids_round_trip() {
        for o in BootOutcome::ALL {
            assert_eq!(BootOutcome::parse(o.as_str()), Some(o));
        }
        assert!(BootOutcome::UserFlash.runs_user_firmware());
        assert!(!BootOutcome::HeldInReset.runs_user_firmware());
    }
}
