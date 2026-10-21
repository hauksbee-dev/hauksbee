//! How many parts a batch extracts at once, and what that costs the plan.
//!
//! A subscription CLI has no per-call price, so a batch is judged by its share
//! of the plan's two limits: the 5-hour window and the weekly allowance. The
//! headroom is read from the CLI before the run (`extract_plan::read`: codex's
//! `account/rateLimits/read`, `claude -p /usage`), the cost per part comes from
//! measured priors below and is refined from the usage actually observed as the
//! run goes, and the result is one [`Plan`]: how many agents to run, whether the
//! run deserves a warning, and whether it will have to pause for a window
//! reset. All the numbers and thresholds live here; the words live in
//! `extract_messages` and are shown by the CLI and the app alike.
//!
//! Parallelism does not change the total usage, it only compresses it into
//! fewer windows, so the 5-hour window bounds the pool from above: the ceiling
//! is the largest pool whose run still fits the headroom left plus what refills
//! while it runs ([`recommend`]), and never above [`JOBS_DEFAULT`] (the burst a
//! service takes without refusing has not been measured for any backend, so the
//! default does not pretend to know). Where the plan cannot be read the ceiling
//! is a conservative [`JOBS_UNKNOWN`], never a guess dressed up as a reading.
//! The ceiling is not the pool: a batch ramps up to it ([`RAMP_START`], one
//! more every [`RAMP_EVERY`] while nothing is refused) and, after the service
//! rate-limits and the pool is halved, grows back one at a time once it has been
//! quiet for [`REGROW_AFTER`]. Usage headroom can only ever lower the ceiling.

use std::time::Duration;

use crate::datasheet::Backend;
use crate::extract_config::Resolved;
use crate::extract_plan::Reading;
use crate::extract_usage::Billing;

/// The most agents `--jobs` may ask for at once, whatever the headroom.
pub const JOBS_CAP: usize = 8;
/// The ceiling when `--jobs` is not given.
///
/// No backend has a measured concurrency figure yet: a codex plan refused eight
/// agents started together, so the default is what it is known to carry.
pub const JOBS_DEFAULT: usize = 4;
/// Agents started together when a batch begins; the pool ramps up from here.
pub const RAMP_START: usize = 2;
/// While nothing is refused, one more agent is let in this often.
pub const RAMP_EVERY: Duration = Duration::from_secs(10);
/// After a rate-limit refusal, the quiet stretch before the pool grows by one.
pub const REGROW_AFTER: Duration = Duration::from_secs(60);
/// The pool when the plan's headroom cannot be read.
pub const JOBS_UNKNOWN: usize = 2;
/// The pool for a metered (API key) backend, which has no plan to protect.
pub const JOBS_METERED: usize = 4;
/// Warn when a batch alone would use more than this share of the weekly allowance.
pub const WEEK_SHARE_WARN: f64 = 15.0;
/// Warn when either limit would pass this percentage.
pub const USAGE_WARN: f64 = 85.0;
/// A limit's whole allowance, in percent.
pub const FULL: f64 = 100.0;
/// The length of the 5-hour window, in minutes.
pub const WINDOW_MINUTES: f64 = 300.0;
/// Wall-clock minutes one part's extraction takes (a prior, for the run length).
pub const MINUTES_PER_PART: f64 = 6.0;
/// Parts that must finish before the observed usage replaces the prior.
pub const MIN_OBSERVED_PARTS: usize = 3;

/// A part's cost, in points (percent) of each limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prior {
    /// Of the 5-hour window.
    pub session: f64,
    /// Of the weekly allowance.
    pub week: f64,
}

/// Codex Plus, gpt-6-luna: measured about 0.8 points of the 5-hour window per
/// part, and about 4 points of the weekly allowance for roughly 90 parts.
pub const CODEX_LUNA: Prior = Prior {
    session: 0.8,
    week: 4.0 / 90.0,
};

/// Codex Plus, gpt-6.1-sol: measured about 6 points of the 5-hour window per
/// part; the weekly share scales with it from Luna's.
pub const CODEX_SOL: Prior = Prior {
    session: 6.0,
    week: 4.0 / 90.0 * (6.0 / 0.8),
};

/// The measured cost of a part on `backend` with `model`, if it has been
/// measured. Any other codex model is priced as the dearer one.
#[must_use]
pub fn prior(backend: Backend, model: &str) -> Option<Prior> {
    match backend {
        Backend::Codex if model.to_ascii_lowercase().contains("luna") => Some(CODEX_LUNA),
        Backend::Codex => Some(CODEX_SOL),
        _ => None,
    }
}

/// What is known about the plan before the run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Headroom {
    /// A subscription, with its usage so far.
    Plan(Reading),
    /// Metered through an API key: there is no plan to run out of.
    Metered,
    /// Not readable.
    Unknown,
}

/// Read the plan's headroom for the backend `resolved` will run (spawns the
/// CLI; no model call).
#[must_use]
pub fn headroom(resolved: &Resolved) -> Headroom {
    if resolved.backend == Backend::Api {
        return Headroom::Metered;
    }
    let Some(agent) = resolved.agent(resolved.backend) else {
        return Headroom::Unknown;
    };
    match crate::datasheet::billing_for(resolved) {
        Billing::Plan => crate::extract_plan::read(resolved.backend.name(), &agent.command)
            .map_or(Headroom::Unknown, Headroom::Plan),
        Billing::Metered { .. } => Headroom::Metered,
        Billing::Unknown => Headroom::Unknown,
    }
}

/// What the run is projected to use, and from what.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Basis {
    /// Priced and read: the batch's use in points, and the usage now.
    Known {
        session_use: f64,
        week_use: f64,
        session_now: Option<f64>,
        week_now: Option<f64>,
    },
    /// Billed per call.
    Metered,
    /// The plan's headroom or the cost per part is not known.
    Unknown,
}

/// A reason to stop and look before a batch starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Warning {
    /// The batch alone is this many points of the weekly allowance.
    WeekShare(f64),
    /// Weekly usage would reach this percentage.
    WeekPast(f64),
    /// 5-hour usage would reach this percentage.
    SessionPast(f64),
    /// The batch needs more than the window has left: it will pause for a reset.
    WindowExceeded(f64),
}

/// The decision for one batch.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub parts: usize,
    /// The most agents that will run at once (the pool ramps up to this).
    pub jobs: usize,
    /// The default ceiling for the headroom and the backend.
    pub recommended: usize,
    /// `--jobs` asked for something other than the recommendation.
    pub overridden: bool,
    pub basis: Basis,
    pub warnings: Vec<Warning>,
    /// The projection is more than the 5-hour window has left.
    pub pause: bool,
}

impl Plan {
    /// Whether an interactive run should ask before going on.
    #[must_use]
    pub const fn needs_confirmation(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// The largest pool (up to `cap`) whose run fits, and at least 1.
///
/// The batch's `session_use` points must not exceed the window's room now plus
/// what refills during the run, which is shorter the more agents there are.
#[must_use]
pub fn recommend(parts: usize, session_use: f64, session_now: f64, cap: usize) -> usize {
    let room = (FULL - session_now).max(0.0);
    let cap = cap.clamp(1, JOBS_CAP).min(parts.max(1));
    (1..=cap)
        .rev()
        .find(|&n| {
            let rounds = (count(parts) / count(n)).ceil();
            let minutes = rounds * MINUTES_PER_PART;
            session_use <= room + FULL * minutes / WINDOW_MINUTES
        })
        .unwrap_or(1)
}

/// Decide the pool and the warnings for `parts` parts.
///
/// `cpus` caps the pool by the machine, `requested` is `--jobs`, and
/// `assume_yes` is `--yes`: an unattended run that would overrun the window is
/// held to the recommendation.
#[must_use]
pub fn plan(
    parts: usize,
    headroom: Headroom,
    prior: Option<Prior>,
    cpus: usize,
    requested: Option<usize>,
    assume_yes: bool,
) -> Plan {
    let cap = cpus.clamp(1, JOBS_DEFAULT);
    let known = match (headroom, prior) {
        (Headroom::Plan(r), Some(p)) if r.session.is_some() || r.week.is_some() => Some((r, p)),
        _ => None,
    };
    let (basis, recommended) = match (headroom, known) {
        (_, Some((r, p))) => {
            let session_use = p.session * count(parts);
            let basis = Basis::Known {
                session_use,
                week_use: p.week * count(parts),
                session_now: r.session,
                week_now: r.week,
            };
            let jobs = r.session.map_or_else(
                || JOBS_UNKNOWN.min(cap),
                |now| recommend(parts, session_use, now, cap),
            );
            (basis, jobs)
        }
        (Headroom::Metered, None) => (Basis::Metered, JOBS_METERED.min(cap)),
        _ => (Basis::Unknown, JOBS_UNKNOWN.min(cap)),
    };
    let recommended = recommended.min(parts.max(1));
    let mut warnings = Vec::new();
    let mut pause = false;
    if let Basis::Known {
        session_use,
        week_use,
        session_now,
        week_now,
    } = basis
    {
        if week_use > WEEK_SHARE_WARN {
            warnings.push(Warning::WeekShare(week_use));
        }
        if let Some(after) = week_now.map(|n| n + week_use).filter(|a| *a > USAGE_WARN) {
            warnings.push(Warning::WeekPast(after));
        }
        if let Some(after) = session_now.map(|n| n + session_use) {
            if after > FULL {
                pause = true;
                warnings.push(Warning::WindowExceeded(after));
            } else if after > USAGE_WARN {
                warnings.push(Warning::SessionPast(after));
            }
        }
    }
    let wanted = requested.map(|n| n.clamp(1, JOBS_CAP));
    let mut jobs = wanted.unwrap_or(recommended).min(parts.max(1));
    if assume_yes && pause {
        jobs = jobs.min(recommended);
    }
    Plan {
        parts,
        jobs,
        recommended,
        overridden: wanted.is_some_and(|n| n != recommended),
        basis,
        warnings,
        pause,
    }
}

/// The 5-hour cost per part: the observed rise since the run began once
/// [`MIN_OBSERVED_PARTS`] have finished, else the prior. The flag says which.
#[must_use]
pub fn refined_per_part(
    prior: f64,
    start: Option<f64>,
    now: Option<f64>,
    done: usize,
) -> (f64, bool) {
    match (start, now) {
        (Some(s), Some(n)) if done >= MIN_OBSERVED_PARTS && n > s => ((n - s) / count(done), true),
        _ => (prior, false),
    }
}

/// The pool for the parts still to do, never above the current one: lowered when
/// the refined cost says the window will not last, never raised mid-run.
#[must_use]
pub fn relimit(remaining: usize, session_now: f64, per_part: f64, current: usize) -> usize {
    recommend(
        remaining,
        per_part * count(remaining),
        session_now,
        current.max(1),
    )
    .min(current.max(1))
}

/// A count as a float (counts here are far below where this loses anything).
fn count(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The pool one step up from `limit` towards `ceiling`; never above it.
#[must_use]
pub fn grown(limit: usize, ceiling: usize) -> usize {
    limit.saturating_add(1).min(ceiling)
}

/// The pool a batch begins with, for a planned `ceiling`.
#[must_use]
pub fn ramp_start(ceiling: usize) -> usize {
    ceiling.clamp(1, RAMP_START)
}

/// How a batch's pool moved: planned ceiling, most agents running at once,
/// the pool at the end, and the rate-limit refusals met on the way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub planned: usize,
    pub peak: usize,
    pub last: usize,
    pub rate_limits: usize,
}

/// Half the pool, for a backend that is rate limiting; never below 1.
#[must_use]
pub const fn halve(jobs: usize) -> usize {
    if jobs <= 1 {
        1
    } else {
        jobs / 2
    }
}

/// Whether a backend's error text is a rate limit (HTTP 429 or its wording).
#[must_use]
pub fn is_rate_limited(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("rate limit")
        || t.contains("rate-limit")
        || t.contains("too many requests")
        || t.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w == "429")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(session: f64, week: f64) -> Headroom {
        Headroom::Plan(Reading {
            week: Some(week),
            session: Some(session),
        })
    }

    #[test]
    fn plenty_of_headroom_runs_up_to_the_default_ceiling() {
        // 38 Luna parts: ~30 points of the 5-hour window, ~1.7 of the week.
        let p = plan(38, reading(10.0, 20.0), Some(CODEX_LUNA), 8, None, false);
        assert_eq!(
            (p.jobs, p.recommended),
            (JOBS_DEFAULT, JOBS_DEFAULT),
            "{p:?}"
        );
        assert!(p.warnings.is_empty() && !p.pause, "{p:?}");
        let Basis::Known {
            session_use,
            week_use,
            ..
        } = p.basis
        else {
            panic!("{p:?}")
        };
        assert!((session_use - 30.4).abs() < 1e-9 && (week_use - 38.0 * 4.0 / 90.0).abs() < 1e-9);
    }

    #[test]
    fn the_machine_and_the_part_count_bound_the_pool() {
        let few_cpus = plan(38, reading(0.0, 0.0), Some(CODEX_LUNA), 3, None, false);
        assert_eq!(few_cpus.jobs, 3);
        let few_parts = plan(2, reading(0.0, 0.0), Some(CODEX_LUNA), 8, None, false);
        assert_eq!(few_parts.jobs, 2);
    }

    #[test]
    fn a_tight_window_lowers_the_pool_because_parallelism_only_compresses_usage() {
        // 20 Sol parts = 120 points, only 100 left: more agents finish sooner,
        // so less of the window refills during the run; a pool that fits is
        // smaller than the cap, and the whole run overruns what is left now.
        let p = plan(20, reading(0.0, 0.0), Some(CODEX_SOL), 8, None, false);
        assert!(p.recommended < JOBS_DEFAULT, "{p:?}");
        assert!(p.pause, "{p:?}");
        // 6 Sol parts = 36 points fits with a full pool.
        let small = plan(6, reading(40.0, 0.0), Some(CODEX_SOL), 8, None, false);
        assert_eq!(small.recommended, JOBS_DEFAULT, "{small:?}");
        assert!(!small.pause);
    }

    #[test]
    fn warnings_fire_at_their_thresholds_and_only_then() {
        // Weekly share over 15 points.
        let p = plan(
            38,
            reading(0.0, 10.0),
            Some(Prior {
                session: 0.1,
                week: 0.5,
            }),
            8,
            None,
            false,
        );
        assert!(
            matches!(p.warnings[..], [Warning::WeekShare(w)] if (w - 19.0).abs() < 1e-9),
            "{p:?}"
        );
        // Weekly usage would pass 85.
        let p = plan(
            10,
            reading(0.0, 80.0),
            Some(Prior {
                session: 0.1,
                week: 0.6,
            }),
            8,
            None,
            false,
        );
        assert!(
            p.warnings
                .iter()
                .any(|w| matches!(w, Warning::WeekPast(a) if (*a - 86.0).abs() < 1e-9)),
            "{p:?}"
        );
        // 5-hour usage would pass 85 but not 100.
        let p = plan(
            10,
            reading(80.0, 0.0),
            Some(Prior {
                session: 1.0,
                week: 0.01,
            }),
            8,
            None,
            false,
        );
        assert!(
            matches!(p.warnings[..], [Warning::SessionPast(a)] if (a - 90.0).abs() < 1e-9),
            "{p:?}"
        );
        assert!(!p.pause);
        // Past 100 pauses.
        let p = plan(
            10,
            reading(95.0, 0.0),
            Some(Prior {
                session: 1.0,
                week: 0.01,
            }),
            8,
            None,
            false,
        );
        assert!(
            p.pause && matches!(p.warnings[..], [Warning::WindowExceeded(_)]),
            "{p:?}"
        );
        // Exactly at the thresholds is quiet.
        let p = plan(
            10,
            reading(75.0, 0.0),
            Some(Prior {
                session: 1.0,
                week: 1.5,
            }),
            8,
            None,
            false,
        );
        assert!(p.warnings.is_empty(), "{p:?}");
        assert!(!p.needs_confirmation());
    }

    #[test]
    fn yes_continues_through_a_warning_but_holds_an_overrunning_pool_to_the_recommendation() {
        let tight = reading(95.0, 0.0);
        let cost = Some(Prior {
            session: 1.0,
            week: 0.01,
        });
        let asked = plan(10, tight, cost, 8, Some(8), false);
        assert_eq!(asked.jobs, 8, "interactive --jobs is honoured: {asked:?}");
        assert!(asked.overridden && asked.needs_confirmation());
        let unattended = plan(10, tight, cost, 8, Some(8), true);
        assert_eq!(unattended.jobs, unattended.recommended, "{unattended:?}");
        assert!(unattended.pause);
        // A warning that is not an overrun does not cap an explicit --jobs under --yes.
        let share = plan(
            38,
            reading(0.0, 10.0),
            Some(Prior {
                session: 0.1,
                week: 0.5,
            }),
            8,
            Some(6),
            true,
        );
        assert_eq!(share.jobs, 6, "{share:?}");
        assert!(!share.warnings.is_empty() && !share.pause);
    }

    #[test]
    fn jobs_overrides_the_recommendation_and_is_bounded() {
        let p = plan(38, reading(0.0, 0.0), Some(CODEX_LUNA), 8, Some(3), false);
        assert_eq!(
            (p.jobs, p.recommended, p.overridden),
            (3, JOBS_DEFAULT, true)
        );
        let p = plan(38, reading(0.0, 0.0), Some(CODEX_LUNA), 8, Some(99), false);
        assert_eq!(p.jobs, JOBS_CAP);
    }

    #[test]
    fn unreadable_headroom_falls_back_to_a_conservative_pool_with_no_warnings() {
        let p = plan(38, Headroom::Unknown, Some(CODEX_LUNA), 8, None, false);
        assert_eq!((p.jobs, p.basis), (JOBS_UNKNOWN, Basis::Unknown));
        assert!(p.warnings.is_empty());
        // Readable plan but no measured price for the backend: still unknown.
        let p = plan(38, reading(0.0, 0.0), None, 8, None, false);
        assert_eq!((p.jobs, p.basis), (JOBS_UNKNOWN, Basis::Unknown));
        // Only the week readable: priced, but the pool is conservative.
        let weekly = Headroom::Plan(Reading {
            week: Some(50.0),
            session: None,
        });
        let p = plan(38, weekly, Some(CODEX_LUNA), 8, None, false);
        assert_eq!(p.jobs, JOBS_UNKNOWN);
        assert!(p.warnings.is_empty());
        // Metered is its own pool.
        let p = plan(38, Headroom::Metered, None, 8, None, false);
        assert_eq!((p.jobs, p.basis), (JOBS_METERED, Basis::Metered));
    }

    #[test]
    fn priors_follow_the_model_and_only_codex_has_them() {
        assert_eq!(prior(Backend::Codex, "gpt-6-luna"), Some(CODEX_LUNA));
        assert_eq!(prior(Backend::Codex, "gpt-6.1-sol"), Some(CODEX_SOL));
        assert_eq!(prior(Backend::Codex, "something-new"), Some(CODEX_SOL));
        assert_eq!(prior(Backend::ClaudeCode, "claude-sonnet-5-5"), None);
    }

    #[test]
    fn observed_usage_replaces_the_prior_after_a_few_parts_and_can_lower_the_pool() {
        assert_eq!(
            refined_per_part(0.8, Some(10.0), Some(12.0), 2),
            (0.8, false)
        );
        let (cost, observed) = refined_per_part(0.8, Some(10.0), Some(16.0), 3);
        assert!(observed && (cost - 2.0).abs() < 1e-9);
        assert_eq!(
            refined_per_part(0.8, Some(10.0), Some(10.0), 5),
            (0.8, false)
        );
        // 30 parts left at 2 points each from 40% used: 60 points, 60 left now;
        // a smaller pool than 8 gains refill time, but it never rises.
        let lowered = relimit(30, 40.0, 4.0, 8);
        assert!(lowered < 8, "{lowered}");
        assert_eq!(
            relimit(30, 0.0, 0.1, 4),
            4,
            "never raised past the current pool"
        );
    }

    #[test]
    fn the_default_ceiling_is_four_however_many_cpus_and_asking_goes_higher() {
        let p = plan(38, reading(0.0, 0.0), Some(CODEX_LUNA), 64, None, false);
        assert_eq!(p.jobs, 4, "{p:?}");
        let p = plan(38, Headroom::Metered, None, 64, None, false);
        assert_eq!(p.jobs, JOBS_METERED);
        let asked = plan(38, reading(0.0, 0.0), Some(CODEX_LUNA), 64, Some(7), false);
        assert_eq!((asked.jobs, asked.recommended), (7, 4), "{asked:?}");
    }

    #[test]
    fn a_pool_ramps_from_a_couple_and_grows_one_step_to_its_ceiling() {
        assert_eq!(
            (ramp_start(8), ramp_start(4), ramp_start(2), ramp_start(1)),
            (2, 2, 2, 1)
        );
        assert_eq!((grown(2, 4), grown(3, 4), grown(4, 4)), (3, 4, 4));
        assert_eq!(grown(5, 4), 4, "a lowered ceiling pulls it down, never up");
    }

    #[test]
    fn rate_limits_are_recognised_and_halve_the_pool() {
        for text in [
            "HTTP 429 Too Many Requests",
            "You are being rate limited",
            "rate-limit exceeded",
            "stream error: status 429",
        ] {
            assert!(is_rate_limited(text), "{text}");
        }
        for text in ["pin 4290 is open", "overloaded", "model refused"] {
            assert!(!is_rate_limited(text), "{text}");
        }
        assert_eq!((halve(8), halve(5), halve(2), halve(1)), (4, 2, 1, 1));
    }
}
