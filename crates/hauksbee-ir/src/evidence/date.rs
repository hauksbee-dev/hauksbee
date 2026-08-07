//! Shared, fail-closed waiver date policy.
//!
//! Waivers expire on a calendar date, which makes the run's own clock part of
//! the trust chain: a zeroed, pre-NTP, or backdated clock would silently
//! resurrect waivers that deliberately lapsed. [`RunDate`] closes that hole by
//! refusing to believe any reading before the day this policy shipped —
//! an untrustworthy clock reads as *unknown*, and an unknown date expires
//! every waiver rather than activating any. The same captured date feeds
//! waiver gating and evidence rendering, so the two surfaces can never
//! disagree about what "today" was. Calendar parsing is strict (`YYYY-MM-DD`,
//! real dates only) and converts through the standard civil-days algorithm,
//! with no timezone: expiry is end-of-day in epoch days, everywhere.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/evidence.md

use serde::{Deserialize, Serialize};

/// The run's date, captured once for waiver gating and evidence rendering.
///
/// A reading before [`RunDate::EARLIEST_CREDIBLE_DAY`] is untrustworthy rather
/// than an old but valid run date. Treating it as unknown fails closed: every
/// waiver is expired. This prevents a zeroed, pre-NTP, or backdated clock from
/// silently resurrecting waivers that deliberately lapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunDate(Option<i64>);

impl RunDate {
    /// Days since the Unix epoch for 2026-07-29, when this policy was added.
    pub const EARLIEST_CREDIBLE_DAY: i64 = 20_663;

    /// Capture the system clock and apply the credibility rule.
    pub fn from_system_clock() -> Self {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Self::from_epoch_days((d.as_secs() / 86_400) as i64),
            Err(_) => Self::unknown(),
        }
    }

    /// Construct from days since the Unix epoch. Pre-floor readings become
    /// unknown instead of being clamped into a date that could activate a waiver.
    pub fn from_epoch_days(days: i64) -> Self {
        Self((days >= Self::EARLIEST_CREDIBLE_DAY).then_some(days))
    }

    /// No trustworthy run date. Every waiver reads as expired.
    pub fn unknown() -> Self {
        Self(None)
    }

    /// Whether an end-of-day expiry still covers this run.
    pub fn is_covered_by(self, expiry_epoch_days: i64) -> bool {
        self.0.is_some_and(|today| expiry_epoch_days >= today)
    }

    /// Days since the Unix epoch, or `None` when the clock is untrustworthy.
    pub fn epoch_days(self) -> Option<i64> {
        self.0
    }

    /// Classify one waiver using the shared parser and fail-closed clock rule.
    pub fn waiver_state(self, until: &str) -> WaiverState {
        match parse_ymd_epoch_days(until) {
            Some(expiry) if self.is_covered_by(expiry) => WaiverState::Active,
            _ => WaiverState::Expired,
        }
    }
}

/// Whether a dated waiver covers the captured run date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaiverState {
    Active,
    Expired,
}

/// Parse a real `YYYY-MM-DD` calendar date into days since the Unix epoch.
pub fn parse_ymd_epoch_days(s: &str) -> Option<i64> {
    let mut parts = s.trim().split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || !(1..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || d < 1
        || d > days_in_month(y, m)
    {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_validation_and_epoch_conversion_are_stable() {
        assert_eq!(parse_ymd_epoch_days("1970-01-01"), Some(0));
        assert_eq!(parse_ymd_epoch_days("2024-02-29"), Some(19_782));
        assert_eq!(parse_ymd_epoch_days("2025-02-29"), None);
        assert_eq!(parse_ymd_epoch_days("99999999999999-12-31"), None);
    }

    #[test]
    fn untrustworthy_dates_expire_every_waiver() {
        assert_eq!(
            RunDate::unknown().waiver_state("9999-12-31"),
            WaiverState::Expired
        );
        assert_eq!(RunDate::from_epoch_days(0).epoch_days(), None);
    }
}
