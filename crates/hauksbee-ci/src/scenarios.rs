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

use schemars::JsonSchema;
use serde::Deserialize;

/// One transient scenario: a profile attached to a part, plus optional
/// decoupling realism.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Stable id, referenced by `rail_window` / `protection_trip` assertions to
    /// scope them to this scenario's window (optional; a single-scenario spec
    /// can leave it unset).
    #[serde(default)]
    pub id: Option<String>,
    /// The part the load attaches to (reference designator, e.g. "U5").
    pub part: String,
    /// The named load profile to apply (built-in id, e.g. "`esp32_boot_wifi`", or
    /// one defined in `profiles` below).
    pub profile: String,
    /// The supply net the load current is drawn from. If omitted, the runner
    /// infers it from the part's power pins (VDD/VCC/VBAT/3V3/5V class).
    #[serde(default)]
    pub supply_net: Option<String>,
    /// When the profile's activity begins (ms into the run). Default 0.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub start_ms: f64,
    /// Deterministic seed for profile jitter. Default 0.
    #[serde(default)]
    pub seed: u64,
}

/// A user-defined load profile inline in the spec (mirrors the DB schema), so a
/// scenario can carry a bespoke profile without editing the model database.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InlineProfile {
    /// The profile id a `[[scenario]]`'s `profile` field references. Shadows a
    /// built-in profile of the same name for this spec.
    pub id: String,
    /// Free-text note on what this load profile represents.
    #[serde(default)]
    pub description: String,
    /// The `[[profile.segment]]` blocks, applied in order, describing the
    /// current the part draws over time.
    #[serde(default, rename = "segment")]
    pub segments: Vec<InlineSegment>,
}

/// One segment of an inline profile (subset of the model-DB segment schema).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InlineSegment {
    /// Peak current drawn during this segment (A).
    pub level_a: f64,
    /// Ramp time from the previous level to `level_a` (s). 0 = a step edge,
    /// which is the worst case for inrush and the reason to model it.
    #[serde(default)]
    pub rise_s: f64,
    /// How long the segment holds `level_a` (s).
    #[serde(default)]
    pub duration_s: f64,
    /// Repeat period (s). Non-zero makes the segment a burst that fires every
    /// `period_s` (e.g. a radio TX slot); 0 runs it once.
    #[serde(default)]
    pub period_s: f64,
    /// Current drawn between bursts (A). Defaults to 0 (fully idle).
    #[serde(default)]
    pub idle_a: Option<f64>,
    /// Deterministic jitter (s) applied to the burst start, seeded from the
    /// scenario's `seed`, so repeats are not perfectly aligned.
    #[serde(default)]
    pub jitter_s: f64,
}

impl InlineProfile {
    /// Convert to the model-layer [`hauksbee_models::LoadProfile`].
    #[must_use]
    pub fn to_profile(&self) -> hauksbee_models::LoadProfile {
        hauksbee_models::LoadProfile {
            id: self.id.clone(),
            description: self.description.clone(),
            match_rule: hauksbee_models::profile::ProfileMatch::default(),
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
#[derive(Debug, Clone, Deserialize, Default, JsonSchema)]
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
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapOverride {
    /// The capacitor's reference designator, e.g. "C12".
    #[serde(rename = "ref")]
    pub reference: String,
    /// Equivalent series resistance (ohms), overriding the package/dielectric
    /// default inferred from the footprint.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub esr_ohms: Option<f64>,
    /// Equivalent series inductance (henries), overriding the inferred default.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
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
    /// Settled per-frame timeseries used to calculate dip/spike duration and
    /// recovery/settling time; threshold calculations are performed on demand.
    pub samples: Vec<(f64, f64)>, // (t_s, v) timeseries within the window
}

impl RailWindow {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            min_v: f64::INFINITY,
            max_v: f64::NEG_INFINITY,
            samples: Vec::new(),
        }
    }

    pub fn observe(&mut self, t_s: f64, v: f64) {
        self.fold(v);
        self.samples.push((t_s, v));
    }

    /// Whether a non-finite sample reached this window (see [`Self::fold`]).
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.min_v.is_nan() || self.max_v.is_nan()
    }

    /// Widen the window's min/max envelope with an intra-frame extreme WITHOUT
    /// recording a timeseries sample. Each frame is solved in ~10 sub-chunks and
    /// `observe` only sees the settled final-chunk voltage, so a load-step sag
    /// that bottoms out mid-frame and recovers by the last chunk would leave
    /// `min_v` blind to the excursion, a brownout-floor `rail_window` assertion
    /// would then false-pass the very sag it exists to catch. The scheduler's
    /// per-frame min/max is folded here so the min/max bounds match what the plain
    /// `voltage` assertion path already sees. (The `samples` timeseries, and hence
    /// `dip_duration/recovery` and `spike_duration/settling` still reflect settled
    /// per-frame values.)
    pub const fn fold(&mut self, v: f64) {
        // `f64::min`/`max` drop a NaN, so a non-finite sample would leave the
        // bounds looking untouched; poison both (sticky) so it reads as "no
        // measurement" and the assertions refuse to judge it.
        if v.is_finite() && !self.is_poisoned() {
            self.min_v = self.min_v.min(v);
            self.max_v = self.max_v.max(v);
        } else {
            self.min_v = f64::NAN;
            self.max_v = f64::NAN;
        }
    }

    /// Total time (s) the rail spent strictly past `threshold` on the `below`
    /// side, summed over the sampled window (rectangular integration on the
    /// sample grid). `dip_duration_s` (below) and `spike_duration_s` (above)
    /// are this same accounting mirrored across the threshold, so they share
    /// it here instead of each carrying their own copy of the loop.
    fn duration_past_s(&self, threshold: f64, below: bool) -> f64 {
        let past = |v: f64| if below { v < threshold } else { v > threshold };
        let mut total = 0.0;
        let mut last_dt = 0.0;
        for w in self.samples.windows(2) {
            let (t0, v0) = w[0];
            let (t1, _v1) = w[1];
            // Count the interval as counted when its leading sample is past.
            let dt = (t1 - t0).max(0.0);
            last_dt = dt;
            if past(v0) {
                total += dt;
            }
        }
        // The trailing sample owns one more frame interval; if it is still
        // past the threshold at the window end, count that dwell too
        // (otherwise a dip/spike that runs to the last frame is reported one
        // frame short).
        if let Some(&(_, vlast)) = self.samples.last() {
            if past(vlast) {
                total += last_dt;
            }
        }
        total
    }

    /// Total time (s) the rail spent strictly below `threshold` volts, summed
    /// over the sampled window (rectangular integration on the sample grid).
    #[must_use]
    pub fn dip_duration_s(&self, threshold: f64) -> f64 {
        self.duration_past_s(threshold, true)
    }

    /// Total time (s) the rail spent strictly above `threshold` volts, summed
    /// over the sampled window (rectangular integration on the sample grid).
    #[must_use]
    pub fn spike_duration_s(&self, threshold: f64) -> f64 {
        self.duration_past_s(threshold, false)
    }

    /// Shared engine for `recovery_s` (below) and `settling_s` (above): from
    /// the first sample past `threshold` to the moment the rail returned to
    /// `target` and stayed there; the FIRST sample recovered past which no
    /// later sample crosses back. Returns 0 if the rail never crossed
    /// `threshold`, and `+∞` if it crossed but never sustained a return to
    /// `target` (so a `recover_within_ms`/settling assertion FAILS loud
    /// rather than passing on the small `window_end − t_cross` value a late
    /// crossing produced).
    fn recovered_at_s(&self, threshold: f64, target: f64, below: bool) -> f64 {
        let past = |v: f64| if below { v < threshold } else { v > threshold };
        let recovered = |v: f64| if below { v >= target } else { v <= target };
        let first_cross = self.samples.iter().find(|(_, v)| past(*v)).map(|(t, _)| *t);
        let Some(t_cross) = first_cross else {
            return 0.0;
        };
        // Never recovered: the rail ended on the wrong side of `target`, so it
        // never sustained a crossing back. A late crossing would otherwise
        // report `window_end − t_cross` (a small value) and FALSELY pass a
        // recover_within_ms/settling bound. Fail loud with +∞.
        if let Some(&(_, vlast)) = self.samples.last() {
            if !recovered(vlast) {
                return f64::INFINITY;
            }
        }
        // The last time the rail was still on the wrong side of `target`
        // (after the crossing).
        let last_unrecovered = self
            .samples
            .iter()
            .filter(|(t, v)| *t >= t_cross && !recovered(*v))
            .map(|(t, _)| *t)
            .fold(t_cross, f64::max);
        // The rail does not actually reach `target` until the first sample
        // after `last_unrecovered`: every sample past it is recovered by
        // definition, so that sample is the moment it returned and stayed.
        // Returning `last_unrecovered` itself reported recovery a full frame
        // early, a silent false pass for a rail that only crosses back on the
        // following frame. The +∞ guard above already ensured the final
        // sample is recovered, so such a sample always exists.
        let recovered_at = self
            .samples
            .iter()
            .filter(|(t, v)| *t > last_unrecovered && recovered(*v))
            .map(|(t, _)| *t)
            .fold(f64::INFINITY, f64::min);
        (recovered_at - t_cross).max(0.0)
    }

    /// Recovery time (s): from the first sample below `threshold` to the moment
    /// the rail returned to `recover_to` and stayed there; the FIRST sample at or
    /// above `recover_to` past which no later sample drops back below it. Returns 0
    /// if the rail never dipped, and `+∞` if it dipped but never climbed back to
    /// `recover_to` (so a `recover_within_ms` assertion FAILS loud rather than
    /// passing on the small `window_end − t_dip` value a late dip produced).
    #[must_use]
    pub fn recovery_s(&self, threshold: f64, recover_to: f64) -> f64 {
        self.recovered_at_s(threshold, recover_to, true)
    }

    /// Settling time (s): from the first sample above `threshold` to the first
    /// sample at or below `settle_to` after the last sample above `settle_to`.
    /// Returns 0 if the rail never spiked, and `+∞` if it spiked but the final
    /// sample is still above `settle_to`.
    #[must_use]
    pub fn settling_s(&self, threshold: f64, settle_to: f64) -> f64 {
        self.recovered_at_s(threshold, settle_to, false)
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

    fn window(samples: &[(f64, f64)]) -> RailWindow {
        let mut w = RailWindow::new();
        for &(t, v) in samples {
            w.observe(t, v);
        }
        w
    }

    #[test]
    fn dip_duration_covers_every_sub_threshold_sample_including_the_trailing_one() {
        // At 3.3, dips to 2.8 for the samples at 2, 3, 4 ms, recovers at 5 ms.
        let w = window(&[
            (0.000, 3.30),
            (0.001, 3.30),
            (0.002, 2.80),
            (0.003, 2.80),
            (0.004, 2.80),
            (0.005, 3.30),
            (0.006, 3.30),
        ]);
        assert!((w.min_v - 2.80).abs() < 1e-9 && (w.max_v - 3.30).abs() < 1e-9);
        let dip = w.dip_duration_s(3.0);
        assert!((dip - 0.003).abs() < 1e-9, "dip duration {dip}");
        // A dip running to the last sample owns one more frame, not one less.
        let trailing = window(&[
            (0.000, 3.30),
            (0.001, 3.30),
            (0.002, 2.80),
            (0.003, 2.80),
            (0.004, 2.80),
        ]);
        let dip = trailing.dip_duration_s(3.0);
        assert!(
            (dip - 0.003).abs() < 1e-9,
            "trailing-dwell dip duration {dip}"
        );
    }

    #[test]
    fn recovery_is_measured_to_the_instant_recover_to_is_reached() {
        // Dips at 0 ms, sits below 3.2 through 4 ms, reaches 3.3 at 6 ms: the
        // recovery time is 6 ms (not the last sub-recover sample at 4 ms), so
        // a 5 ms `recover_within_ms` bound fails.
        let w = window(&[(0.000, 2.80), (0.002, 2.80), (0.004, 2.80), (0.006, 3.30)]);
        let rec = w.recovery_s(3.0, 3.2);
        assert!((rec - 0.006).abs() < 1e-9, "recovery {rec}");
        // A rail that dips and never climbs back reports +inf, never a small
        // window-end remainder that would falsely pass.
        let never = window(&[(0.000, 3.30), (0.005, 3.30), (0.009, 2.50), (0.010, 2.50)]);
        assert!(never.recovery_s(3.0, 3.2).is_infinite());
    }

    #[test]
    fn window_stats_spike_and_settling() {
        let mut w = RailWindow::new();
        // A rail that sits at 3.3, spikes to 3.8 for 3 ms, then settles to 3.3.
        let pts = [
            (0.000, 3.30),
            (0.001, 3.30),
            (0.002, 3.80), // spike starts
            (0.003, 3.80),
            (0.004, 3.80),
            (0.005, 3.30), // settled
            (0.006, 3.30),
        ];
        for (t, v) in pts {
            w.observe(t, v);
        }
        let spike = w.spike_duration_s(3.5);
        assert!((spike - 0.003).abs() < 1e-9, "spike duration {spike}");
        let settle = w.settling_s(3.5, 3.4);
        assert!((settle - 0.003).abs() < 1e-9, "settling time {settle}");
    }

    #[test]
    fn spike_never_settles_is_infinite() {
        let mut w = RailWindow::new();
        for (t, v) in [
            (0.000, 3.30),
            (0.005, 3.80), // spikes near the window end
            (0.010, 3.60), // remains above settle_to at the final sample
        ] {
            w.observe(t, v);
        }
        let settle = w.settling_s(3.5, 3.4);
        assert!(
            settle.is_infinite(),
            "a rail that never settles to settle_to must report +inf, got {settle}"
        );
    }

    #[test]
    fn spike_and_settle_threshold_boundaries_are_strict_then_inclusive() {
        let mut w = RailWindow::new();
        // Exactly the spike threshold is not a spike; exactly settle_to counts
        // as settled once the rail has spiked.
        for (t, v) in [
            (0.000, 3.30),
            (0.001, 3.50), // equal to spike threshold: not above
            (0.002, 3.60), // actual spike
            (0.003, 3.40), // equal to settle_to: settled
        ] {
            w.observe(t, v);
        }
        assert_eq!(w.spike_duration_s(3.5).to_bits(), 0.001_f64.to_bits());
        assert!((w.settling_s(3.5, 3.4) - 0.001).abs() < 1e-9);
    }

    #[test]
    fn folded_intraframe_spike_updates_max_but_not_spike_duration() {
        let mut w = RailWindow::new();
        w.observe(0.000, 3.30);
        w.observe(0.001, 3.30);
        w.fold(4.20); // intra-frame spike, recovered by the settled sample
        assert_eq!(w.max_v.to_bits(), 4.20_f64.to_bits());
        assert_eq!(w.spike_duration_s(3.5).to_bits(), 0.0_f64.to_bits());
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
