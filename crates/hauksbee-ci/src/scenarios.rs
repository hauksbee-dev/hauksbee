//! Transient scenario plumbing for hauksbee-ci.
//!
//! A `[[scenario]]` block attaches a dynamic [load profile](hauksbee_models::LoadProfile)
//! to a part on the board (stamped as a current sink on a supply net), optionally
//! turns the board's decoupling capacitors from ideal to honest (ESR/ESL), and
//! drives a transient window. The scenario-aware assertions
//! (`rail_window`, `protection_trip`) then judge the rail's behaviour over that
//! window: minimum/maximum voltage, how long it dipped below a threshold,
//! whether a battery protection cutoff fired, and how long recovery took.
//!
//! This module owns only the *spec types* and the small helpers that translate
//! them. The actual wiring (stamping loads, applying ESR/ESL, collecting the
//! window timeseries) lives in [`crate::runner`], which calls into here.

use serde::Deserialize;

/// One transient scenario: a profile attached to a part, plus optional
/// decoupling realism.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Stable id, referenced by `rail_window` / `protection_trip` assertions to
    /// scope them to this scenario's window (optional; a single-scenario spec
    /// can leave it unset).
    #[serde(default)]
    pub id: Option<String>,
    /// The part the load attaches to (reference designator, e.g. "U5").
    pub part: String,
    /// The named load profile to apply (built-in id, e.g. "esp32_boot_wifi", or
    /// one defined in `profiles` below).
    pub profile: String,
    /// The supply net the load current is drawn from. If omitted, the runner
    /// infers it from the part's power pins (VDD/VCC/VBAT/3V3/5V class).
    #[serde(default)]
    pub supply_net: Option<String>,
    /// When the profile's activity begins (ms into the run). Default 0.
    #[serde(default)]
    pub start_ms: f64,
    /// Deterministic seed for profile jitter. Default 0.
    #[serde(default)]
    pub seed: u64,
}

/// A user-defined load profile inline in the spec (mirrors the DB schema), so a
/// scenario can carry a bespoke profile without editing the model database.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineProfile {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "segment")]
    pub segments: Vec<InlineSegment>,
}

/// One segment of an inline profile (subset of the model-DB segment schema).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineSegment {
    pub level_a: f64,
    #[serde(default)]
    pub rise_s: f64,
    #[serde(default)]
    pub duration_s: f64,
    #[serde(default)]
    pub period_s: f64,
    #[serde(default)]
    pub idle_a: Option<f64>,
    #[serde(default)]
    pub jitter_s: f64,
}

impl InlineProfile {
    /// Convert to the model-layer [`hauksbee_models::LoadProfile`].
    pub fn to_profile(&self) -> hauksbee_models::LoadProfile {
        hauksbee_models::LoadProfile {
            id: self.id.clone(),
            description: self.description.clone(),
            match_rule: Default::default(),
            segments: self
                .segments
                .iter()
                .map(|s| hauksbee_models::Segment {
                    level_a: s.level_a,
                    rise_s: s.rise_s,
                    duration_s: s.duration_s,
                    period_s: s.period_s,
                    idle_a: s.idle_a,
                    jitter_s: s.jitter_s,
                })
                .collect(),
        }
    }
}

/// Capacitor-parasitics request for a scenario run. Opt-in: when `decoupling`
/// is unset the board's capacitors stay ideal (no behavioural change).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Decoupling {
    /// Turn on ESR/ESL on the board's decoupling capacitors using package /
    /// dielectric defaults inferred from each cap's footprint and value.
    #[serde(default)]
    pub parasitics: bool,
    /// Per-part ESR/ESL overrides (ohms / henries), keyed by capacitor ref.
    #[serde(default, rename = "override")]
    pub overrides: Vec<CapOverride>,
}

/// A per-capacitor ESR/ESL override.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapOverride {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub esr_ohms: Option<f64>,
    #[serde(default)]
    pub esl_henries: Option<f64>,
}

/// Window statistics for one rail over a scenario window, collected by the
/// runner each frame.
#[derive(Debug, Clone)]
pub struct RailWindow {
    /// Minimum voltage seen in the window (V).
    pub min_v: f64,
    /// Maximum voltage seen in the window (V).
    pub max_v: f64,
    /// Total time the rail was below the dip threshold (s), for the last
    /// `dip_threshold` queried (the runner tracks one threshold per assertion).
    pub samples: Vec<(f64, f64)>, // (t_s, v) timeseries within the window
}

impl RailWindow {
    pub fn new() -> Self {
        RailWindow {
            min_v: f64::INFINITY,
            max_v: f64::NEG_INFINITY,
            samples: Vec::new(),
        }
    }

    pub fn observe(&mut self, t_s: f64, v: f64) {
        self.min_v = self.min_v.min(v);
        self.max_v = self.max_v.max(v);
        self.samples.push((t_s, v));
    }

    /// Total time (s) the rail spent strictly below `threshold` volts, summed
    /// over the sampled window (rectangular integration on the sample grid).
    pub fn dip_duration_s(&self, threshold: f64) -> f64 {
        let mut total = 0.0;
        let mut last_dt = 0.0;
        for w in self.samples.windows(2) {
            let (t0, v0) = w[0];
            let (t1, _v1) = w[1];
            // Count the interval as "dipped" when its leading sample is below.
            let dt = (t1 - t0).max(0.0);
            last_dt = dt;
            if v0 < threshold {
                total += dt;
            }
        }
        // The trailing sample owns one more frame interval; if it is still below
        // the threshold at the window end, count that dwell too (otherwise a dip
        // that runs to the last frame is reported one frame short).
        if let Some(&(_, vlast)) = self.samples.last() {
            if vlast < threshold {
                total += last_dt;
            }
        }
        total
    }

    /// Recovery time (s): from the first sample below `threshold` to the last
    /// time the rail crossed back above `recover_to` and stayed there. Returns 0
    /// if the rail never dipped, and the full window length if it never
    /// recovered.
    pub fn recovery_s(&self, threshold: f64, recover_to: f64) -> f64 {
        let first_dip = self
            .samples
            .iter()
            .find(|(_, v)| *v < threshold)
            .map(|(t, _)| *t);
        let Some(t_dip) = first_dip else {
            return 0.0;
        };
        // The last time the rail was still below recover_to.
        let last_below = self
            .samples
            .iter()
            .filter(|(t, v)| *t >= t_dip && *v < recover_to)
            .map(|(t, _)| *t)
            .fold(t_dip, f64::max);
        (last_below - t_dip).max(0.0)
    }
}

impl Default for RailWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_stats_dip_and_recovery() {
        let mut w = RailWindow::new();
        // A rail that sits at 3.3, dips to 2.8 for 3 ms, recovers to 3.3.
        let pts = [
            (0.000, 3.30),
            (0.001, 3.30),
            (0.002, 2.80), // dip starts
            (0.003, 2.80),
            (0.004, 2.80),
            (0.005, 3.30), // recovered
            (0.006, 3.30),
        ];
        for (t, v) in pts {
            w.observe(t, v);
        }
        assert!((w.min_v - 2.80).abs() < 1e-9);
        assert!((w.max_v - 3.30).abs() < 1e-9);
        // Below 3.0 V: samples at t=2,3,4 ms each cover a 1 ms forward interval.
        let dip = w.dip_duration_s(3.0);
        assert!((dip - 0.003).abs() < 1e-9, "dip duration {dip}");
        // Recovery: first dip at 2 ms, last below 3.2 at 4 ms => 2 ms recovery.
        let rec = w.recovery_s(3.0, 3.2);
        assert!((rec - 0.002).abs() < 1e-9, "recovery {rec}");
    }

    #[test]
    fn dip_duration_counts_trailing_dwell_at_window_end() {
        // A dip that runs to the last sample must count that final frame, not be
        // reported one frame short.
        let mut w = RailWindow::new();
        for (t, v) in [
            (0.000, 3.30),
            (0.001, 3.30),
            (0.002, 2.80), // dip starts and never recovers within the window
            (0.003, 2.80),
            (0.004, 2.80), // last sample, still below threshold
        ] {
            w.observe(t, v);
        }
        // Samples at 2,3 ms each own a 1 ms forward interval; the trailing 4 ms
        // sample owns one more 1 ms frame => 3 ms total below 3.0 V.
        let dip = w.dip_duration_s(3.0);
        assert!(
            (dip - 0.003).abs() < 1e-9,
            "trailing-dwell dip duration {dip}"
        );
    }

    #[test]
    fn inline_profile_converts() {
        let ip = InlineProfile {
            id: "x".into(),
            description: "d".into(),
            segments: vec![InlineSegment {
                level_a: 0.1,
                rise_s: 0.001,
                duration_s: 0.0,
                period_s: 0.0,
                idle_a: None,
                jitter_s: 0.0,
            }],
        };
        let p = ip.to_profile();
        assert_eq!(p.id, "x");
        assert!((p.current_at(1.0, 0) - 0.1).abs() < 1e-9);
    }
}
