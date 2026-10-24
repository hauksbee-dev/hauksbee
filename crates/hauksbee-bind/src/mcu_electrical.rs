//! An MCU's electrical behaviour as its card states it.
//!
//! That is the output resistance of each pin per drive-strength setting, where
//! the firmware's drive-strength field lives, and the supply and reset-pin
//! thresholds at which the part holds itself in reset.
//!
//! Everything here is datasheet-derived card data. A part whose card states
//! nothing keeps [`crate::drivers::DEFAULT_RO`] and enforces no reset: the
//! empty [`McuElectrical`] is the "no data" answer, never a guess.

use hauksbee_models::{DriveSelect, ModelEntry, OutputDrive, OutputLevel, ResetSpec};

/// One drive-strength setting reduced to what the co-simulation stamps: an
/// output resistance per level (fitted from the card's stated VOH / VOL
/// points), or a level the setting does not drive at all.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveRow {
    /// The datasheet's name for the setting (`"12 mA"`, `"S0H1"`).
    pub setting: String,
    /// The drive-select field value that picks this setting.
    pub selector: Option<u32>,
    /// Whether the part comes out of reset in this setting.
    pub reset_default: bool,
    /// Output resistance (ohm) while the pin drives high.
    pub r_high: Option<f64>,
    /// Output resistance (ohm) while the pin drives low.
    pub r_low: Option<f64>,
    /// The pin floats instead of driving high (an open-drain mode).
    pub high_disconnected: bool,
    /// The pin floats instead of driving low (an open-source mode).
    pub low_disconnected: bool,
}

impl DriveRow {
    /// Reduce one card row.
    #[must_use]
    pub fn from_card(row: &OutputDrive) -> Self {
        Self {
            setting: row.setting.clone(),
            selector: row.selector,
            reset_default: row.reset_default,
            r_high: row.fit(OutputLevel::High),
            r_low: row.fit(OutputLevel::Low),
            high_disconnected: row.disconnected.contains(&OutputLevel::High),
            low_disconnected: row.disconnected.contains(&OutputLevel::Low),
        }
    }

    /// The resistance to stamp for a pin driving `high`, and whether the
    /// setting drives that level at all. A level the card gives no figure for
    /// falls back to the other level's, then to `fallback`.
    #[must_use]
    pub fn drive(&self, high: bool, fallback: f64) -> (f64, bool) {
        let (own, other, disconnected) = if high {
            (self.r_high, self.r_low, self.high_disconnected)
        } else {
            (self.r_low, self.r_high, self.low_disconnected)
        };
        (own.or(other).unwrap_or(fallback), !disconnected)
    }
}

/// What the card states about the part's internal pull-ups and its sleep
/// current. Every field is `None` unless the card prints it: an absent figure
/// is "unknown", never a default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SleepSpec {
    /// Smallest resistance (ohm) of an I/O pin's internal pull-up
    /// (`io_pull_up_min_ohms`); it sets the LARGEST pull-up current.
    pub pull_up_min_ohms: Option<f64>,
    /// Largest resistance (ohm) of an I/O pin's internal pull-up
    /// (`io_pull_up_max_ohms`); it sets the SMALLEST pull-up current.
    pub pull_up_max_ohms: Option<f64>,
    /// The part's power-down supply current (A), typical
    /// (`power_down_current_typ_a`).
    pub power_down_typ_a: Option<f64>,
    /// The part's power-down supply current (A), maximum
    /// (`power_down_current_max_a`).
    pub power_down_max_a: Option<f64>,
}

impl SleepSpec {
    /// Read the card's pull-up and power-down params.
    #[must_use]
    pub fn from_card(model: &ModelEntry) -> Self {
        let get = |key: &str| model.params.get_f64(key);
        Self {
            pull_up_min_ohms: get("io_pull_up_min_ohms"),
            pull_up_max_ohms: get("io_pull_up_max_ohms"),
            power_down_typ_a: get("power_down_current_typ_a"),
            power_down_max_a: get("power_down_current_max_a"),
        }
    }
}

/// The card's electrical data for one MCU.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McuElectrical {
    /// Drive-strength settings, in card order.
    pub drive: Vec<DriveRow>,
    /// Where the firmware's drive-strength field lives.
    pub drive_select: Option<DriveSelect>,
    /// Brown-out and reset-pin thresholds.
    pub reset: Option<ResetSpec>,
    /// Internal pull-up resistance and power-down supply current.
    pub sleep: SleepSpec,
}

impl McuElectrical {
    /// Read the card's `output_drive`, `drive_select` and `reset` tables.
    #[must_use]
    pub fn from_card(model: &ModelEntry) -> Self {
        Self {
            drive: model.output_drive.iter().map(DriveRow::from_card).collect(),
            drive_select: model.drive_select.clone(),
            reset: model.reset.clone(),
            sleep: SleepSpec::from_card(model),
        }
    }

    /// The setting the part comes out of reset in: the card's `reset_default`
    /// row, else the only row when there is just one.
    #[must_use]
    pub fn reset_default(&self) -> Option<&DriveRow> {
        self.drive
            .iter()
            .find(|r| r.reset_default)
            .or_else(|| (self.drive.len() == 1).then(|| &self.drive[0]))
    }

    /// The setting a drive-select field value picks.
    #[must_use]
    pub fn by_selector(&self, selector: u32) -> Option<&DriveRow> {
        self.drive.iter().find(|r| r.selector == Some(selector))
    }

    /// The output resistance to stamp before firmware has run: the reset
    /// default's high-side figure, else `fallback`.
    #[must_use]
    pub fn initial_ohms(&self, fallback: f64) -> f64 {
        self.reset_default()
            .map_or(fallback, |row| row.drive(true, fallback).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_models::OutputPoint;

    fn row(selector: u32, ma: f64) -> OutputDrive {
        OutputDrive {
            setting: format!("{ma} mA"),
            selector: Some(selector),
            vdd: 3.3,
            points: vec![
                OutputPoint {
                    level: OutputLevel::High,
                    current_a: ma / 1000.0,
                    volts: 2.62,
                },
                OutputPoint {
                    level: OutputLevel::Low,
                    current_a: ma / 1000.0,
                    volts: 0.5,
                },
            ],
            ..OutputDrive::default()
        }
    }

    #[test]
    fn a_stronger_setting_has_a_lower_output_resistance() {
        let weak = DriveRow::from_card(&row(0, 2.0));
        let strong = DriveRow::from_card(&row(3, 12.0));
        assert!((weak.r_high.unwrap() - 340.0).abs() < 1e-6);
        assert!((strong.r_high.unwrap() - 56.666_666).abs() < 1e-3);
        assert!((weak.r_low.unwrap() - 250.0).abs() < 1e-6);
        assert!(strong.r_low.unwrap() < weak.r_low.unwrap());
    }

    #[test]
    fn a_disconnected_level_is_not_driven() {
        let mut open_drain = row(6, 1.0);
        open_drain.points.retain(|p| p.level == OutputLevel::Low);
        open_drain.disconnected = vec![OutputLevel::High];
        let r = DriveRow::from_card(&open_drain);
        assert!(r.high_disconnected && r.r_high.is_none());
        assert!(!r.drive(true, 50.0).1, "the high level floats");
        assert!(r.drive(false, 50.0).1, "the low level is driven");
    }

    #[test]
    fn no_card_data_keeps_the_generic_resistance() {
        let none = McuElectrical::default();
        assert!((none.initial_ohms(50.0) - 50.0).abs() < f64::EPSILON);
        assert!(none.reset_default().is_none());
    }

    #[test]
    fn the_reset_default_row_sets_the_initial_resistance() {
        let mut four = row(1, 4.0);
        four.reset_default = true;
        let e = McuElectrical {
            drive: vec![
                DriveRow::from_card(&row(0, 2.0)),
                DriveRow::from_card(&four),
            ],
            ..McuElectrical::default()
        };
        assert!((e.initial_ohms(50.0) - 170.0).abs() < 1e-6);
        assert_eq!(e.by_selector(0).unwrap().setting, "2 mA");
    }
}
