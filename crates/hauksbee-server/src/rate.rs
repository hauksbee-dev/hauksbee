//! Honest realtime-rate accounting for the sim loop.
//!
//! Two distinct numbers live here, and the wire keeps them distinct:
//!
//! - **achieved**: sim seconds advanced per wall second, measured over a
//!   rolling window of what the loop actually delivered. This is what
//!   `SimFrame.realtime_factor` reports; it is never derived from the request.
//! - **sustainable ceiling**: the largest speed factor the engine could hold
//!   at the loop's tick budget, derived from the measured stepping cost (wall
//!   seconds spent inside `Engine::step` per sim second produced). The loop
//!   paces each tick at `min(requested, ceiling)` so an unachievable request
//!   degrades to a small step per tick (frames keep flowing at the tick rate,
//!   commands stay responsive) instead of one giant step that blocks the loop.
//!
//! The ceiling is computed from per-sim-second cost, not from the achieved
//! rate, so capping does not feed back into its own input (no downward
//! spiral): if a board costs 10 wall seconds per sim second, the ceiling is
//! 0.1x regardless of how hard the loop is currently pacing.
//!
//! All methods take explicit wall-clock seconds so tests are deterministic;
//! the sim loop feeds them from `Instant` measurements.

use std::collections::VecDeque;

/// Fraction of the measured ceiling the pacer actually requests. The margin
/// absorbs jitter in the cost estimate so the loop holds its tick cadence
/// instead of oscillating around 100% duty.
const HEADROOM: f64 = 0.9;

/// Rolling window length in wall seconds: long enough to be stable, short
/// enough that the display tracks a load change within a few seconds.
const WINDOW_WALL_S: f64 = 3.0;

/// Minimum wall span before the window's achieved estimate is trusted;
/// below it we fall back to the stepping-cost estimate.
const MIN_SPAN_S: f64 = 0.25;

/// Floor for the paced factor so a pathological cost estimate can never wedge
/// the sim at zero. Matches the `SetSpeed` clamp's lower bound.
const MIN_FACTOR: f64 = 0.001;

/// One progress sample: where the sim clock stood at a wall instant.
#[derive(Clone, Copy)]
struct Progress {
    wall_s: f64,
    sim_t: f64,
}

/// One stepping-cost sample: wall seconds spent inside `Engine::step` for the
/// sim seconds it produced.
#[derive(Clone, Copy)]
struct StepCost {
    wall_s: f64,
    step_wall: f64,
    sim_dt: f64,
}

/// Rolling measurement of achieved rate and sustainable ceiling.
pub struct RateMeter {
    progress: VecDeque<Progress>,
    costs: VecDeque<StepCost>,
}

impl Default for RateMeter {
    fn default() -> Self {
        RateMeter::new()
    }
}

impl RateMeter {
    pub fn new() -> RateMeter {
        RateMeter {
            progress: VecDeque::new(),
            costs: VecDeque::new(),
        }
    }

    /// Forget everything. Called on pause/reset so idle wall time is never
    /// counted against the achieved rate.
    pub fn clear(&mut self) {
        self.progress.clear();
        self.costs.clear();
    }

    /// Record one completed engine step: it consumed `step_wall` wall seconds
    /// to produce `sim_dt` sim seconds, and afterwards the sim clock stands at
    /// `sim_t` at wall time `wall_s` (seconds on any monotonic axis).
    pub fn record(&mut self, wall_s: f64, sim_t: f64, step_wall: f64, sim_dt: f64) {
        self.progress.push_back(Progress { wall_s, sim_t });
        if sim_dt > 0.0 {
            self.costs.push_back(StepCost {
                wall_s,
                step_wall,
                sim_dt,
            });
        }
        let cutoff = wall_s - WINDOW_WALL_S;
        // Keep one sample at or before the cutoff so the achieved span always
        // covers the full window rather than shrinking as samples expire.
        while self.progress.len() > 2 && self.progress[1].wall_s <= cutoff {
            self.progress.pop_front();
        }
        while self.costs.len() > 1 && self.costs[0].wall_s <= cutoff {
            self.costs.pop_front();
        }
    }

    /// Sim seconds advanced per wall second over the rolling window, i.e. the
    /// factor actually delivered. Falls back to the stepping-cost estimate
    /// while the window is too short to measure, and to `None` before any
    /// step has been recorded (a caller with nothing measured must not claim
    /// a rate).
    pub fn achieved(&self) -> Option<f64> {
        if let (Some(first), Some(last)) = (self.progress.front(), self.progress.back()) {
            let span = last.wall_s - first.wall_s;
            if span >= MIN_SPAN_S {
                return Some((last.sim_t - first.sim_t).max(0.0) / span);
            }
        }
        // Too early for a wall-window measurement: the per-sim-second stepping
        // cost is the best honest estimate of what a full-duty loop delivers.
        // May be infinite when steps were too fast to measure; the sim loop
        // clamps the reported value to its paced factor (which is what a
        // faster-than-budget engine actually delivers under tick pacing).
        self.ceiling().map(|c| c / HEADROOM)
    }

    /// The measured sustainable speed factor (with headroom applied): what the
    /// loop can hold given the observed stepping cost. `None` before any step
    /// has been measured.
    fn ceiling(&self) -> Option<f64> {
        let (wall, sim): (f64, f64) = self
            .costs
            .iter()
            .fold((0.0, 0.0), |(w, s), c| (w + c.step_wall, s + c.sim_dt));
        if sim <= 0.0 {
            return None;
        }
        // wall/sim = wall seconds per sim second; its inverse is the factor a
        // 100%-duty loop would deliver. A cost cheaper than the tick budget
        // means "faster than the request needs", so the ceiling can exceed 1.
        let cost = wall / sim;
        if cost <= 0.0 {
            // Steps too fast to measure: no evidence of a ceiling.
            return Some(f64::INFINITY);
        }
        Some((1.0 / cost) * HEADROOM)
    }

    /// The factor the loop should pace this tick: the requested factor, capped
    /// to the measured sustainable ceiling (floored so the sim never wedges).
    /// Returns the paced factor and whether the cap engaged.
    pub fn paced_factor(&self, requested: f64) -> (f64, bool) {
        match self.ceiling() {
            Some(ceiling) if ceiling < requested => (ceiling.max(MIN_FACTOR), true),
            _ => (requested, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the meter as a loop would: each tick advances `tick_s` of wall
    /// time, the engine produces `sim_dt` sim seconds at `cost` wall seconds
    /// per sim second.
    fn drive(meter: &mut RateMeter, ticks: usize, tick_s: f64, sim_dt: f64, cost: f64) -> f64 {
        let mut wall = 0.0;
        let mut sim_t = 0.0;
        for _ in 0..ticks {
            wall += tick_s;
            sim_t += sim_dt;
            meter.record(wall, sim_t, sim_dt * cost, sim_dt);
        }
        sim_t
    }

    #[test]
    fn empty_meter_claims_no_rate() {
        let meter = RateMeter::new();
        assert!(meter.achieved().is_none());
        // With nothing measured, the request passes through uncapped.
        assert_eq!(meter.paced_factor(1.0), (1.0, false));
    }

    #[test]
    fn achieved_tracks_a_known_slow_loop_within_tolerance() {
        // Engine costs 5 wall seconds per sim second; the loop ends up
        // delivering 0.2 sim seconds per wall second at full duty. Ticks of
        // 100 ms wall each advance 20 ms of sim time.
        let mut meter = RateMeter::new();
        drive(&mut meter, 50, 0.1, 0.02, 5.0);
        let achieved = meter.achieved().expect("measured");
        assert!(
            (achieved - 0.2).abs() < 0.02,
            "achieved {achieved} should be ~0.2"
        );
    }

    #[test]
    fn achieved_is_below_requested_when_the_engine_cannot_keep_up() {
        // Requested 1.0x but the engine only advances 10 sim ms per 100 ms of
        // wall time: achieved must report ~0.1, never the requested 1.0.
        let mut meter = RateMeter::new();
        drive(&mut meter, 40, 0.1, 0.01, 10.0);
        let achieved = meter.achieved().expect("measured");
        assert!(
            achieved < 0.15,
            "achieved {achieved} must be well under 1.0"
        );
        assert!(achieved > 0.05, "achieved {achieved} should be ~0.1");
    }

    #[test]
    fn cap_engages_at_the_measured_ceiling() {
        // Cost 10 wall s per sim s => sustainable 0.1x; with headroom the
        // paced factor lands at 0.09, flagged as limited.
        let mut meter = RateMeter::new();
        drive(&mut meter, 40, 0.1, 0.01, 10.0);
        let (paced, limited) = meter.paced_factor(1.0);
        assert!(limited);
        assert!(
            (paced - 0.1 * HEADROOM).abs() < 0.01,
            "paced {paced} should be ~{}",
            0.1 * HEADROOM
        );
    }

    #[test]
    fn cheap_engine_is_not_capped() {
        // Cost 0.1 wall s per sim s => ceiling 9x; a 2x request passes.
        let mut meter = RateMeter::new();
        drive(&mut meter, 40, 0.1, 0.2, 0.1);
        let (paced, limited) = meter.paced_factor(2.0);
        assert!(!limited);
        assert_eq!(paced, 2.0);
    }

    #[test]
    fn window_recovers_after_a_transient_stall() {
        let mut meter = RateMeter::new();
        // 3 seconds of slow stepping...
        drive(&mut meter, 30, 0.1, 0.01, 10.0);
        // ...then the meter is cleared (pause) and fast stepping resumes on a
        // fresh wall axis; the old samples must not drag the estimate down.
        meter.clear();
        let mut wall = 100.0;
        let mut sim_t = 0.0;
        for _ in 0..30 {
            wall += 0.1;
            sim_t += 0.1;
            meter.record(wall, sim_t, 0.01, 0.1);
        }
        let achieved = meter.achieved().expect("measured");
        assert!(
            (achieved - 1.0).abs() < 0.05,
            "achieved {achieved} should be ~1.0 after recovery"
        );
    }
}
