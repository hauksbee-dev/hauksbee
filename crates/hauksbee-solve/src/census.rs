//! End-of-march step census for the transient driver (HAUKSBEE_STEP_CENSUS=1).
//!
//! Pure readout, zero behaviour: every counter is written from values the
//! march already computed, and the whole estate is dead when the env var is
//! unset (one cached-bool branch per hook). The adaptive capture march on the
//! flagship costs ~1700 s and the accepted grid alone cannot attribute that
//! wall (rejected trials, event bisections, and Newton retries leave no trace
//! in the output waveform), so the march itself must count its discards. The
//! linear-algebra phase split (stamp / LU factor / back-substitution) lives in
//! thread-local accumulators because the phases execute inside `newton_solve`,
//! which has no census parameter and must keep its signature (and its
//! bit-identical default path) untouched.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;

/// Cached once per process: the census exists only when explicitly requested,
/// so the default path pays a single atomic load per hook and no timer reads.
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("HAUKSBEE_STEP_CENSUS").is_ok())
}

/// Linear-algebra phases timed inside the Newton iteration.
pub(crate) enum Phase {
    /// Device stamping (Jacobian + rhs assembly).
    Stamp,
    /// Numeric LU refactorization (frozen-order Gilbert-Peierls).
    Factor,
    /// Triangular back-substitution.
    Backsolve,
    /// One Armijo line-search residual evaluation (`residual_inf_norm_at`),
    /// which re-stamps the WHOLE system per alpha trial. Timed separately from
    /// Stamp because it is line-search overhead, not iteration assembly: the
    /// first census run left ~55% of the march wall unattributed and this call
    /// was the prime suspect.
    LineSearch,
}

thread_local! {
    static STAMP_NS: Cell<u64> = const { Cell::new(0) };
    static FACTOR_NS: Cell<u64> = const { Cell::new(0) };
    static FACTOR_CALLS: Cell<u64> = const { Cell::new(0) };
    static BACKSOLVE_NS: Cell<u64> = const { Cell::new(0) };
    static LS_NS: Cell<u64> = const { Cell::new(0) };
    static LS_CALLS: Cell<u64> = const { Cell::new(0) };
    /// Accepted line-search alpha per iteration, bucketed by halving: index k
    /// counts alpha = 2^-k (k = 0..=6, the Armijo ladder down to the 1/64
    /// floor); index 7 is a guard for anything off the ladder. The lever-1c
    /// decision (lazy arming) needs exactly this: how many iterations accept
    /// the full step immediately vs genuinely backtrack.
    static LS_ALPHA: Cell<[u64; 8]> = const { Cell::new([0; 8]) };
    /// Iterations whose line search ended by Armijo sufficient decrease.
    static LS_ARMIJO_OK: Cell<u64> = const { Cell::new(0) };
    /// Iterations that hit the alpha floor and fell back to the best trial
    /// seen (no alpha satisfied the Armijo test).
    static LS_FALLBACK: Cell<u64> = const { Cell::new(0) };
    /// Lazy-arming predictor cross-tab (lever 1c): [skip&pass, skip&fail,
    /// search&pass, search&fail], where "skip" is the step-norm predictor's
    /// decision (monotone-shrinking step skips the search) and "pass" is the
    /// ground truth (the alpha=1 trial satisfied Armijo on its first eval).
    /// skip&fail is the WRONG-SKIP count the safety argument must cover;
    /// search&pass is the saving the predictor leaves on the table.
    static LS_PRED: Cell<[u64; 4]> = const { Cell::new([0; 4]) };
}

/// Record one line-search iteration's predictor decision against the ground
/// truth of its first alpha=1 trial. Pure readout.
pub(crate) fn ls_predictor(would_skip: bool, first_trial_pass: bool) {
    if !enabled() {
        return;
    }
    let idx = match (would_skip, first_trial_pass) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    };
    LS_PRED.with(|c| {
        let mut a = c.get();
        a[idx] += 1;
        c.set(a);
    });
}

/// Record the alpha one line-search iteration ended on. Pure readout, called
/// from `newton_solve` only when the census is live.
pub(crate) fn ls_alpha(alpha: f64, armijo_ok: bool) {
    if !enabled() {
        return;
    }
    // The ladder is exact powers of two, so -log2(alpha) is integral there.
    let k = -alpha.log2();
    let idx = if (k - k.round()).abs() < 1e-9 && (0.0..=6.0).contains(&k) {
        k.round() as usize
    } else {
        7
    };
    LS_ALPHA.with(|c| {
        let mut a = c.get();
        a[idx] += 1;
        c.set(a);
    });
    if armijo_ok {
        LS_ARMIJO_OK.with(|c| c.set(c.get() + 1));
    } else {
        LS_FALLBACK.with(|c| c.set(c.get() + 1));
    }
}

/// Run one linear-algebra phase, accumulating its wall time when the census is
/// on. When off this is the plain call behind one predictable branch, so the
/// hot Newton loop keeps its cost (and its arithmetic, hence bit-exactness).
#[inline]
pub(crate) fn timed<T>(phase: Phase, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let t0 = Instant::now();
    let r = f();
    let ns = t0.elapsed().as_nanos() as u64;
    match phase {
        Phase::Stamp => STAMP_NS.with(|c| c.set(c.get() + ns)),
        Phase::Factor => {
            FACTOR_NS.with(|c| c.set(c.get() + ns));
            FACTOR_CALLS.with(|c| c.set(c.get() + 1));
        }
        Phase::Backsolve => BACKSOLVE_NS.with(|c| c.set(c.get() + ns)),
        Phase::LineSearch => {
            LS_NS.with(|c| c.set(c.get() + ns));
            LS_CALLS.with(|c| c.set(c.get() + 1));
        }
    }
    r
}

/// Drain the thread-local phase accumulators: (stamp_ns, factor_ns,
/// factor_calls, backsolve_ns, ls_ns, ls_calls, ls_alpha, ls_armijo_ok,
/// ls_fallback). Marches run their Newton solves on their own thread, so
/// draining at march start and end brackets exactly one march.
#[allow(clippy::type_complexity)]
fn take_phases() -> (u64, u64, u64, u64, u64, u64, [u64; 8], u64, u64, [u64; 4]) {
    (
        STAMP_NS.with(|c| c.replace(0)),
        FACTOR_NS.with(|c| c.replace(0)),
        FACTOR_CALLS.with(|c| c.replace(0)),
        BACKSOLVE_NS.with(|c| c.replace(0)),
        LS_NS.with(|c| c.replace(0)),
        LS_CALLS.with(|c| c.replace(0)),
        LS_ALPHA.with(|c| c.replace([0; 8])),
        LS_ARMIJO_OK.with(|c| c.replace(0)),
        LS_FALLBACK.with(|c| c.replace(0)),
        LS_PRED.with(|c| c.replace([0; 4])),
    )
}

/// Accepted-dt decade histogram bounds: buckets are `<1e-12`, one per decade
/// `[1e-12,1e-11) .. [1e-6,1e-5)`, then `>=1e-5`. The adaptive capture march
/// runs dt_min=1e-12, dt_max=2e-6, so every step it can legally take lands in
/// a labelled bucket; the two guards catch anything out of contract.
const N_DECADES: usize = 7;

/// One march's census. Created (Some) only when [`enabled`]; the report prints
/// on Drop so an erroring march (early `return Err`) still accounts for its
/// discarded work.
pub(crate) struct StepCensus {
    started: Instant,
    tstop: f64,
    n_devices: usize,
    n_unknowns: usize,
    adaptive: bool,
    pub accepted: u64,
    pub lte_rejected: u64,
    pub newton_fail_cuts: u64,
    pub event_bisections: u64,
    pub event_resolved: u64,
    /// Accepted-step sizes by decade: [under, 1e-12.., .., 1e-6.., over].
    pub dt_hist: [u64; N_DECADES + 2],
    pub min_accepted_dt: f64,
    /// Trial-solve wall attributed by the trial's FATE, so the report answers
    /// "how much Newton work was thrown away and by which mechanism".
    pub ns_accepted: u64,
    pub ns_lte_rejected: u64,
    pub ns_bisected: u64,
    pub ns_newton_fail: u64,
    /// Wall inside the event-freeze retry (`newton_solve_event`), separate from
    /// the bare trial it followed.
    pub ns_event_retry: u64,
    pub ns_lte_estimate: u64,
    /// Bare-Newton iterations summed over every trial (the event retry's inner
    /// iterations are not reported by its API, hence the separate wall above).
    pub newton_iters: u64,
    pub newton_calls: u64,
    /// Crossing counts by device name, incremented every time a device's
    /// control straddles its threshold on a non-event trial step (whether or
    /// not the bisection was taken; the taken count is `event_bisections`).
    pub crossings: HashMap<String, u64>,
    /// FNV-1a over the raw bits of every accepted sample (time, then each
    /// unknown). Two marches print the same hash iff their accepted grids are
    /// BIT-IDENTICAL, which is the regression witness optimization work needs:
    /// "same physics" claims become one line to compare instead of a waveform
    /// dump. The t=0 emission is excluded (it precedes the loop and is the
    /// caller's seed, not the march's work).
    pub wf_hash: u64,
    /// Sample count folded into `wf_hash` (equal counts make a hash match
    /// meaningful at a glance).
    pub wf_samples: u64,
}

impl StepCensus {
    /// A live census when HAUKSBEE_STEP_CENSUS is set, else None (and every
    /// hook in the march loop stays behind `if let Some`).
    pub(crate) fn begin(
        tstop: f64,
        n_devices: usize,
        n_unknowns: usize,
        adaptive: bool,
    ) -> Option<StepCensus> {
        if !enabled() {
            return None;
        }
        // Drain phase counters left by any previous march on this thread so
        // the Drop report brackets exactly this march.
        let _ = take_phases();
        Some(StepCensus {
            started: Instant::now(),
            tstop,
            n_devices,
            n_unknowns,
            adaptive,
            accepted: 0,
            lte_rejected: 0,
            newton_fail_cuts: 0,
            event_bisections: 0,
            event_resolved: 0,
            dt_hist: [0; N_DECADES + 2],
            min_accepted_dt: f64::INFINITY,
            ns_accepted: 0,
            ns_lte_rejected: 0,
            ns_bisected: 0,
            ns_newton_fail: 0,
            ns_event_retry: 0,
            ns_lte_estimate: 0,
            newton_iters: 0,
            newton_calls: 0,
            crossings: HashMap::new(),
            wf_hash: 0xcbf2_9ce4_8422_2325, // FNV-1a offset basis
            wf_samples: 0,
        })
    }

    /// Fold one accepted sample (its time and full unknown vector) into the
    /// waveform hash.
    pub(crate) fn hash_sample(&mut self, t: f64, x: &[f64]) {
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = self.wf_hash;
        let mut fold = |v: f64| {
            for b in v.to_bits().to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(FNV_PRIME);
            }
        };
        fold(t);
        for &v in x {
            fold(v);
        }
        self.wf_hash = h;
        self.wf_samples += 1;
    }

    /// Record one accepted step of size `h`.
    pub(crate) fn accept(&mut self, h: f64) {
        self.accepted += 1;
        self.min_accepted_dt = self.min_accepted_dt.min(h);
        let idx = if h < 1e-12 {
            0
        } else if h >= 1e-5 {
            N_DECADES + 1
        } else {
            // 1e-12 -> 1, 1e-11 -> 2, ... 1e-6 -> 7. log10 is monotone and the
            // bucket edges are exact powers, so boundary values land on the
            // labelled side within f64 rounding (a one-bucket smear at an exact
            // edge is irrelevant to a decade histogram).
            (h.log10().floor() as i32 + 12).clamp(0, N_DECADES as i32 - 1) as usize + 1
        };
        self.dt_hist[idx] += 1;
    }
}

impl Drop for StepCensus {
    fn drop(&mut self) {
        let wall = self.started.elapsed().as_secs_f64();
        let (
            stamp_ns,
            factor_ns,
            factor_calls,
            backsolve_ns,
            ls_ns,
            ls_calls,
            ls_alpha,
            ls_armijo_ok,
            ls_fallback,
            ls_pred,
        ) = take_phases();
        let s = |ns: u64| ns as f64 / 1e9;
        let attempts =
            self.accepted + self.lte_rejected + self.newton_fail_cuts + self.event_bisections;
        eprintln!(
            "[step-census] march tstop={:.3e}s devices={} unknowns={} mode={} wall={:.2}s",
            self.tstop,
            self.n_devices,
            self.n_unknowns,
            if self.adaptive { "adaptive" } else { "fixed" },
            wall,
        );
        eprintln!(
            "[step-census]   accepted={} lte_rejected={} newton_fail_cuts={} event_bisections={} event_resolved={} (attempts={})",
            self.accepted,
            self.lte_rejected,
            self.newton_fail_cuts,
            self.event_bisections,
            self.event_resolved,
            attempts,
        );
        let labels = ["<1e-12", "1e-12", "1e-11", "1e-10", "1e-9", "1e-8", "1e-7", "1e-6", ">=1e-5"];
        let hist: Vec<String> = labels
            .iter()
            .zip(self.dt_hist.iter())
            .map(|(l, c)| format!("{l}:{c}"))
            .collect();
        eprintln!(
            "[step-census]   accepted dt decades [{}] min_dt={:.2e}",
            hist.join(" "),
            self.min_accepted_dt,
        );
        eprintln!(
            "[step-census]   waveform fnv1a=0x{:016x} over {} accepted samples",
            self.wf_hash, self.wf_samples,
        );
        eprintln!(
            "[step-census]   trial wall by fate: accepted={:.2}s lte_rejected={:.2}s bisected={:.2}s newton_fail={:.2}s event_retry={:.2}s lte_estimate={:.2}s",
            s(self.ns_accepted),
            s(self.ns_lte_rejected),
            s(self.ns_bisected),
            s(self.ns_newton_fail),
            s(self.ns_event_retry),
            s(self.ns_lte_estimate),
        );
        eprintln!(
            "[step-census]   newton: calls={} iters={} ({:.2} iters/call); la phases: stamp={:.2}s factor={:.2}s ({} calls, {:.2}ms/call) backsolve={:.2}s",
            self.newton_calls,
            self.newton_iters,
            if self.newton_calls > 0 {
                self.newton_iters as f64 / self.newton_calls as f64
            } else {
                0.0
            },
            s(stamp_ns),
            s(factor_ns),
            factor_calls,
            if factor_calls > 0 {
                factor_ns as f64 / 1e6 / factor_calls as f64
            } else {
                0.0
            },
            s(backsolve_ns),
        );
        if ls_calls > 0 {
            eprintln!(
                "[step-census]   line-search residual evals: {} ({:.2}s, {:.2}ms/eval; each is a full re-stamp)",
                ls_calls,
                s(ls_ns),
                ls_ns as f64 / 1e6 / ls_calls as f64,
            );
            let ladder = ["1", "1/2", "1/4", "1/8", "1/16", "1/32", "1/64", "other"];
            let alpha_hist: Vec<String> = ladder
                .iter()
                .zip(ls_alpha.iter())
                .map(|(l, c)| format!("{l}:{c}"))
                .collect();
            eprintln!(
                "[step-census]   line-search accepted alpha [{}] armijo_ok={} best_fallback={}",
                alpha_hist.join(" "),
                ls_armijo_ok,
                ls_fallback,
            );
            if ls_pred.iter().any(|&c| c > 0) {
                eprintln!(
                    "[step-census]   ls predictor: skip&pass={} skip&fail={} search&pass={} search&fail={}",
                    ls_pred[0], ls_pred[1], ls_pred[2], ls_pred[3],
                );
            }
        }
        if !self.crossings.is_empty() {
            let mut by_count: Vec<(&String, &u64)> = self.crossings.iter().collect();
            by_count.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let top: Vec<String> = by_count
                .iter()
                .take(10)
                .map(|(name, n)| format!("{name} x{n}"))
                .collect();
            eprintln!(
                "[step-census]   top crossings ({} devices crossed): {}",
                self.crossings.len(),
                top.join(", "),
            );
        }
    }
}
