//! Dynamic load profiles: a declarative chip-activity current model.
//!
//! A [`LoadProfile`] describes the time-varying current a part draws on its
//! supply pin: a boot sequence, WiFi-TX bursts, deep sleep, a motor stall/run
//! envelope. The transient layer stamps it as a current sink (an `Isource`)
//! driven each chunk by [`LoadProfile::current_at`], so the rail sees the same
//! dI/dt a real chip imposes. DC analysis cannot see this; transient can.
//!
//! Profiles are authored in `db/load_profiles.toml` (cited to datasheets) and
//! embedded at compile time, queryable by id or by a value/MPN match rule.
//!
//! ## Waveform model
//!
//! A profile is an ordered list of [`Segment`]s. Each segment ramps (over
//! `rise_s`) from the previous level to its `level_a`, then holds for
//! `duration_s`. A segment with `period_s > 0` is a *burst train*: it fires for
//! `rise_s + duration_s`, idles at `idle_a` for the rest of the period, and
//! repeats, with optional deterministic `jitter_s` on the period. The last
//! non-periodic segment with `duration_s <= 0` holds to the end of the window.

use serde::{Deserialize, Serialize};

/// One piecewise / periodic current segment.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Segment {
    /// Steady current this segment holds (A).
    pub level_a: f64,
    /// Linear ramp time from the previous level to `level_a` (s). The ramp is
    /// the cheap stand-in for an L/R current-rise phase character.
    #[serde(default)]
    pub rise_s: f64,
    /// Time held at `level_a` after the ramp (s). `<= 0` on the last segment
    /// means "hold to the end of the window".
    #[serde(default)]
    pub duration_s: f64,
    /// If `> 0`, this segment repeats with this period (a burst train).
    #[serde(default)]
    pub period_s: f64,
    /// Between-bursts level for a periodic segment (default: the profile
    /// baseline = first segment's `level_a`).
    #[serde(default)]
    pub idle_a: Option<f64>,
    /// Deterministic jitter added to the period (s), seeded from
    /// `(scenario seed, segment index)`. Default 0.
    #[serde(default)]
    pub jitter_s: f64,
}

/// A named dynamic load profile.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LoadProfile {
    /// Stable id, referenced from a scenario spec.
    pub id: String,
    /// Human description for reports.
    #[serde(default)]
    pub description: String,
    /// Optional auto-binding match rule (value / MPN regex).
    #[serde(default, rename = "match")]
    pub match_rule: ProfileMatch,
    /// Ordered segments.
    #[serde(default, rename = "segment")]
    pub segments: Vec<Segment>,
}

/// Match rule for auto-binding a profile to a part.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ProfileMatch {
    #[serde(default)]
    pub value_re: Option<String>,
    #[serde(default)]
    pub mpn_re: Option<String>,
}

/// File container for `db/load_profiles.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct ProfileFile {
    #[serde(default, rename = "models")]
    profiles: Vec<LoadProfile>,
}

const EMBEDDED_PROFILES: &str = include_str!("../db/load_profiles.toml");

impl LoadProfile {
    /// All built-in profiles parsed from the embedded TOML.
    pub fn builtin() -> Vec<LoadProfile> {
        let file: ProfileFile = toml::from_str(EMBEDDED_PROFILES)
            .unwrap_or_else(|e| panic!("built-in load_profiles.toml failed to parse: {e}"));
        file.profiles
    }

    /// Look up a built-in profile by id.
    pub fn by_id(id: &str) -> Option<LoadProfile> {
        Self::builtin().into_iter().find(|p| p.id == id)
    }

    /// Parse a profile set from an arbitrary TOML string (user-supplied).
    pub fn from_toml_str(src: &str) -> Result<Vec<LoadProfile>, toml::de::Error> {
        let file: ProfileFile = toml::from_str(src)?;
        Ok(file.profiles)
    }

    /// The profile's baseline current (the first segment's level, or 0).
    pub fn baseline_a(&self) -> f64 {
        self.segments.first().map(|s| s.level_a).unwrap_or(0.0)
    }

    /// The current (A) this profile draws at time `t` (seconds), under fuzz
    /// `seed` (for deterministic jitter). `t` is relative to the profile's
    /// start (the scenario applies any `start_ms` offset before calling this).
    ///
    /// The piecewise sweep walks segments in order. A non-periodic segment
    /// occupies `[t0, t0 + rise + duration)`. A periodic segment folds time into
    /// its period: inside `[0, rise+duration)` of the period it is active
    /// (ramping then holding `level_a`), and in the remainder it sits at
    /// `idle_a`. The final non-periodic segment with `duration_s <= 0` holds its
    /// level to the end of the window.
    pub fn current_at(&self, t: f64, seed: u64) -> f64 {
        if self.segments.is_empty() {
            return 0.0;
        }
        if t <= 0.0 {
            // Before the start, sit at the first level (DC seed value).
            return self.segments[0].level_a;
        }

        let baseline = self.baseline_a();
        let mut cursor = 0.0f64; // start time of the current segment
        let mut prev_level = self.segments[0].level_a;

        for (i, seg) in self.segments.iter().enumerate() {
            let idle = seg.idle_a.unwrap_or(baseline);

            if seg.period_s > 0.0 {
                // Periodic burst train: this segment owns the rest of the
                // window. Fold the elapsed time into the (jittered) period.
                let local0 = t - cursor;
                if local0 < 0.0 {
                    return prev_level;
                }
                let period = (seg.period_s + jitter(seed, i, seg.jitter_s)).max(1e-9);
                let phase = local0.rem_euclid(period);
                // Each period reproduces the SAME idle -> level -> idle burst,
                // so the per-period ramp must rise from `idle` (the between-burst
                // level), not from the frozen pre-train level. Only the FIRST
                // period ramps from `prev_level`, for continuity with the segment
                // that preceded the train. Anchoring every period to prev_level
                // re-injected a phantom recurring spike whenever the preceding
                // segment sat above idle, e.g. the esp32 cold-boot surge (1.2 A)
                // ahead of a 240 mA/40 mA-idle WiFi burst train re-spiked to
                // ~1.2 A at every period edge, corrupting the exact rail-dip /
                // protection-trip analysis the profile exists to drive.
                let from = if local0 < period { prev_level } else { idle };
                return ramp_hold(from, seg.level_a, idle, seg.rise_s, seg.duration_s, phase);
            }

            // Non-periodic segment. Its active span is rise + duration.
            let span = seg.rise_s.max(0.0) + seg.duration_s.max(0.0);
            let is_last = i + 1 == self.segments.len();

            if is_last && seg.duration_s <= 0.0 {
                // Last segment, no explicit duration: ramp in then hold forever.
                let local = t - cursor;
                return ramped(prev_level, seg.level_a, seg.rise_s, local);
            }

            if t < cursor + span {
                let local = t - cursor;
                return ramped(prev_level, seg.level_a, seg.rise_s, local);
            }

            cursor += span;
            prev_level = seg.level_a;
        }

        // Past every segment: hold the last level.
        prev_level
    }
}

/// Linear ramp from `from` to `to` over `rise` seconds, then hold `to`.
fn ramped(from: f64, to: f64, rise: f64, local: f64) -> f64 {
    if local <= 0.0 {
        return from;
    }
    if rise <= 0.0 || local >= rise {
        return to;
    }
    from + (to - from) * (local / rise)
}

/// One period of a burst train evaluated at phase `phase` in `[0, period)`:
/// ramp `from -> level` over `rise`, hold `level` for `duration`, then `idle`.
fn ramp_hold(from: f64, level: f64, idle: f64, rise: f64, duration: f64, phase: f64) -> f64 {
    let rise = rise.max(0.0);
    let duration = duration.max(0.0);
    if phase < rise {
        ramped(from.max(idle), level, rise, phase)
    } else if phase < rise + duration {
        level
    } else {
        idle
    }
}

/// Deterministic jitter in `[-amp, amp]` from `(seed, index)` (splitmix64).
/// Zero amplitude yields exactly zero so default profiles are jitter-free.
fn jitter(seed: u64, index: usize, amp: f64) -> f64 {
    if amp == 0.0 {
        return 0.0;
    }
    let mut x = seed
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(index as u64);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    let u = (x as f64 / u64::MAX as f64) * 2.0 - 1.0;
    u * amp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_parse_and_are_nonempty() {
        let ps = LoadProfile::builtin();
        assert!(
            ps.len() >= 6,
            "expected at least 6 builtin profiles, got {}",
            ps.len()
        );
        for p in &ps {
            assert!(!p.segments.is_empty(), "profile {} has no segments", p.id);
        }
        // The three hand-authored classes the task names.
        assert!(LoadProfile::by_id("esp32_boot_wifi").is_some());
        assert!(LoadProfile::by_id("mcu_generic").is_some());
        assert!(
            LoadProfile::by_id("servo_sg90").is_some()
                || LoadProfile::by_id("bldc_phase_burst").is_some()
        );
    }

    #[test]
    fn single_level_profile_holds() {
        let p = LoadProfile::by_id("esp32_deep_sleep").unwrap();
        // Deep sleep ~10 uA, held flat after the ramp.
        let i = p.current_at(1.0, 0);
        assert!((i - 0.00001).abs() < 1e-7, "deep sleep current {i}");
    }

    #[test]
    fn esp32_boot_wifi_bursts_between_baseline_and_tx() {
        let p = LoadProfile::by_id("esp32_boot_wifi").unwrap();
        // Inside the first burst hold (just after the 100 ms train starts) the
        // current should be at the 240 mA TX level; between bursts at 40 mA.
        // Segment 0 (baseline) has span = rise(1ms)+dur(0) = 1ms, so the burst
        // train starts ~1 ms in. Peak hold lands around t = 1ms + 0.5ms + a bit.
        let baseline = p.current_at(0.0005, 0);
        assert!((baseline - 0.040).abs() < 1e-6, "baseline {baseline}");

        // Find the max over the first full period.
        let mut peak = 0.0f64;
        let t0 = 0.001;
        let mut t = t0;
        while t < t0 + 0.100 {
            peak = peak.max(p.current_at(t, 0));
            t += 0.0002;
        }
        assert!(
            (peak - 0.240).abs() < 1e-6,
            "burst peak {peak} should hit 240 mA"
        );

        // Late in the period (after the 10 ms burst) it idles at 40 mA.
        let idle = p.current_at(t0 + 0.050, 0);
        assert!((idle - 0.040).abs() < 1e-6, "between-burst idle {idle}");
    }

    #[test]
    fn periodic_segment_repeats() {
        let p = LoadProfile::by_id("esp32_boot_wifi").unwrap();
        // The burst at the start of period N equals the burst at period N+1.
        let a = p.current_at(0.001 + 0.0008, 0); // inside burst 1
        let b = p.current_at(0.001 + 0.100 + 0.0008, 0); // inside burst 2
        assert!((a - b).abs() < 1e-9, "periodic bursts differ: {a} vs {b}");
    }

    #[test]
    fn cold_boot_burst_train_ramps_from_idle_not_the_surge() {
        // R19: esp32_cold_boot_inrush has a 1.2 A cold-boot surge (seg1) directly
        // before a 240 mA / 40 mA-idle WiFi burst train (seg2, 100 ms period).
        // The train starts at cursor = 0.0025 + 0.0065 = 0.009 s. Every period
        // after the first must ramp from the 40 mA idle, NOT re-spike to the
        // frozen 1.2 A pre-train level. Sample the SECOND period's leading edge.
        let p = LoadProfile::by_id("esp32_cold_boot_inrush").unwrap();
        let edge2 = p.current_at(0.109, 0); // local0 = 0.100, phase 0
        let ramp2 = p.current_at(0.1092, 0); // 0.2 ms into period 2's ramp
        assert!(
            edge2 < 0.1,
            "period-2 edge must sit near idle (0.04 A), not re-spike to 1.2 A: {edge2}"
        );
        assert!(
            (edge2 - 0.040).abs() < 1e-6,
            "period-2 edge is the 40 mA idle: {edge2}"
        );
        assert!(
            (ramp2 - 0.080).abs() < 1e-6,
            "period-2 ramp rises idle->level (0.04 -> 0.24): {ramp2}"
        );
        // The burst itself is unchanged, and every period repeats identically.
        let hold2 = p.current_at(0.114, 0); // period 2 hold
        let hold3 = p.current_at(0.214, 0); // period 3 hold
        assert!(
            (hold2 - 0.240).abs() < 1e-9 && (hold3 - 0.240).abs() < 1e-9,
            "burst hold is 240 mA"
        );
        assert!(
            (p.current_at(0.1092, 0) - p.current_at(0.2092, 0)).abs() < 1e-9,
            "period 2 and 3 ramps are identical"
        );
        // First-period continuity with the surge is preserved (ramps from 1.2 A).
        assert!(
            (p.current_at(0.009, 0) - 1.200).abs() < 1e-9,
            "first period ramps from the preceding surge for continuity"
        );
    }

    #[test]
    fn jitter_is_deterministic_and_seed_dependent() {
        let seg = Segment {
            level_a: 1.0,
            rise_s: 0.0,
            duration_s: 0.001,
            period_s: 0.010,
            idle_a: Some(0.0),
            jitter_s: 0.002,
        };
        let p = LoadProfile {
            id: "j".into(),
            description: String::new(),
            match_rule: ProfileMatch::default(),
            segments: vec![seg],
        };
        // Same seed => identical waveform; different seed => may differ.
        assert_eq!(p.current_at(0.05, 7), p.current_at(0.05, 7));
        let s0 = (0..200)
            .map(|k| p.current_at(k as f64 * 0.0005, 0))
            .sum::<f64>();
        let s1 = (0..200)
            .map(|k| p.current_at(k as f64 * 0.0005, 1))
            .sum::<f64>();
        assert!((s0 - s1).abs() > 0.0, "jitter should make seeds differ");
    }
}
