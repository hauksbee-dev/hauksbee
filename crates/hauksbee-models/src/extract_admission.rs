//! Letting agent sessions in: spacing their starts and waiting out a refusal
//! without holding a place.
//!
//! The service refuses at the first request of a new session when too many
//! start close together (measured: every 429 came 1 to 3 seconds after a start,
//! none mid-run). Two things follow, and a batch installs a [`Gate`] to do both
//! for every session the process launches, wherever it launches it:
//!
//! - starts are spaced ([`StartPacer`]: one per [`START_SPACING`] and wider
//!   after a refusal), so they are not a burst;
//! - a refused start waits a few jittered seconds ([`pause`]) and the gate may
//!   give up the place the waiting part holds, so another part can use it.
//!
//! With no gate installed (a single `models extract`, the web app) starts are
//! not paced and a wait is a plain sleep. Whether a call may fork a shared
//! base session is decided per thread ([`with_solo`]).
//!
//! [`START_SPACING`]: crate::extract_capacity::START_SPACING

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use crate::extract_capacity as capacity;

/// What a batch does about session starts and refusals.
pub trait Gate: Send + Sync {
    /// A session is about to start: wait for its turn.
    fn before_start(&self);
    /// A session's first request was refused for rate limiting.
    fn refused(&self);
    /// Wait `how_long` before retrying a refused start; the caller's place in
    /// any pool may be given up meanwhile.
    fn pause(&self, how_long: Duration);
}

static GATE: RwLock<Option<Arc<dyn Gate>>> = RwLock::new(None);

/// The installed gate, removed when this is dropped.
#[must_use = "the gate is removed when this is dropped"]
pub struct Installed(());

impl Drop for Installed {
    fn drop(&mut self) {
        *GATE.write().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Install `gate` for every session this process starts, until the result drops.
pub fn install(gate: Arc<dyn Gate>) -> Installed {
    *GATE.write().unwrap_or_else(PoisonError::into_inner) = Some(gate);
    Installed(())
}

fn current() -> Option<Arc<dyn Gate>> {
    GATE.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Called before every agent session starts.
pub fn before_session() {
    if let Some(gate) = current() {
        gate.before_start();
    }
}

/// Called when a session's first request was refused for rate limiting.
pub fn refused() {
    if let Some(gate) = current() {
        gate.refused();
    }
}

/// Wait `how_long`: through the gate when there is one, else a plain sleep.
pub fn pause(how_long: Duration) {
    match current() {
        Some(gate) => gate.pause(how_long),
        None => std::thread::sleep(how_long),
    }
}

/// A number in `[0, 1)` to jitter a wait by. Not random enough for anything but
/// keeping parts that were refused together from coming back together.
#[must_use]
pub fn jitter_unit() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9, |d| u64::from(d.subsec_nanos()));
        x = now | 1;
    }
    // xorshift64: each caller steps the shared state; a lost update only repeats a value.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    #[allow(clippy::cast_precision_loss)] // the top 53 bits fit a double exactly
    {
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

thread_local! {
    static SOLO: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with calls on this thread told to run alone (`solo`): a codex call
/// then does not fork a shared base session, so a kind with one part in the
/// batch does not pay a warm-up nobody else will use.
pub fn with_solo<T>(solo: bool, f: impl FnOnce() -> T) -> T {
    let before = SOLO.replace(solo);
    let out = f();
    SOLO.set(before);
    out
}

/// Whether calls on this thread must run without a shared base ([`with_solo`]).
#[must_use]
pub fn solo() -> bool {
    SOLO.get()
}

/// Spaces session starts: the turns are handed out on a timeline, one
/// `spacing` apart, and refusals widen the spacing until things have been quiet.
///
/// The arithmetic is pure ([`claim`](Self::claim) takes the time and returns a
/// wait) so it is tested without a clock.
#[derive(Debug)]
pub struct StartPacer {
    base: Duration,
    widest: Duration,
    calm_after: Duration,
    state: Mutex<PacerState>,
}

#[derive(Debug)]
struct PacerState {
    /// The earliest moment the next start may happen.
    next: Duration,
    spacing: Duration,
    last_refusal: Option<Duration>,
}

impl StartPacer {
    #[must_use]
    pub const fn new(base: Duration, widest: Duration, calm_after: Duration) -> Self {
        Self {
            base,
            widest,
            calm_after,
            state: Mutex::new(PacerState {
                next: Duration::ZERO,
                spacing: base,
                last_refusal: None,
            }),
        }
    }

    /// The pacer the constants in [`crate::extract_capacity`] describe.
    #[must_use]
    pub const fn standard() -> Self {
        Self::new(
            capacity::START_SPACING,
            capacity::START_SPACING_MAX,
            capacity::START_CALM_AFTER,
        )
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PacerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the next turn at time `now`: how long to wait before starting.
    pub fn claim(&self, now: Duration) -> Duration {
        let mut s = self.state();
        if s.last_refusal
            .is_some_and(|t| now.saturating_sub(t) >= self.calm_after)
        {
            s.spacing = self.base;
            s.last_refusal = None;
        }
        let at = now.max(s.next);
        s.next = at + s.spacing;
        drop(s);
        at.saturating_sub(now)
    }

    /// A start was refused at `now`: spread the next ones further apart.
    pub fn pressure(&self, now: Duration) {
        let mut s = self.state();
        s.spacing = (s.spacing * 2).min(self.widest);
        s.last_refusal = Some(now);
    }

    /// The spacing now.
    #[must_use]
    pub fn spacing(&self) -> Duration {
        self.state().spacing
    }

    /// Wait for a turn on `clock`, sleeping with `sleep`.
    pub fn admit(&self, clock: &dyn Fn() -> Duration, sleep: &dyn Fn(Duration)) {
        let wait = self.claim(clock());
        if !wait.is_zero() {
            sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn starts_are_spaced_one_per_interval_however_many_arrive_together() {
        let pacer = StartPacer::new(secs(2), secs(8), secs(60));
        let waits: Vec<u64> = (0..5).map(|_| pacer.claim(secs(10)).as_secs()).collect();
        assert_eq!(
            waits,
            [0, 2, 4, 6, 8],
            "five at once go out two seconds apart"
        );
        assert_eq!(
            pacer.claim(secs(100)),
            Duration::ZERO,
            "a start after a quiet stretch goes at once"
        );
        assert_eq!(
            pacer.claim(secs(100)),
            secs(2),
            "and the next waits its turn"
        );
    }

    #[test]
    fn a_refusal_widens_the_spacing_and_quiet_restores_it() {
        let pacer = StartPacer::new(secs(2), secs(8), secs(60));
        pacer.pressure(secs(5));
        assert_eq!(pacer.spacing(), secs(4));
        pacer.pressure(secs(6));
        pacer.pressure(secs(7));
        assert_eq!(pacer.spacing(), secs(8), "never wider than the cap");
        assert_eq!(pacer.claim(secs(30)), Duration::ZERO);
        assert_eq!(
            pacer.claim(secs(30)),
            secs(8),
            "still wide 24 s after the last refusal"
        );
        assert_eq!(
            pacer.claim(secs(67)),
            Duration::ZERO,
            "a turn already at 67 s: free"
        );
        assert_eq!(
            pacer.spacing(),
            secs(2),
            "60 s without a refusal: back to the base"
        );
        assert_eq!(pacer.claim(secs(67)), secs(2));
    }

    #[test]
    fn admit_sleeps_the_claimed_wait_on_the_given_clock() {
        let pacer = StartPacer::standard();
        let slept = Mutex::new(Vec::new());
        let sleep = |d: Duration| slept.lock().unwrap().push(d);
        let clock = || secs(1);
        pacer.admit(&clock, &sleep);
        pacer.admit(&clock, &sleep);
        pacer.admit(&clock, &sleep);
        assert_eq!(
            *slept.lock().unwrap(),
            [secs(2), secs(4)],
            "the first is free"
        );
    }

    #[test]
    fn jitter_stays_in_the_unit_interval_and_moves() {
        let draws: Vec<f64> = (0..200).map(|_| jitter_unit()).collect();
        assert!(draws.iter().all(|u| (0.0..1.0).contains(u)), "{draws:?}");
        assert!(draws.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-9));
    }

    #[test]
    fn solo_is_per_thread_and_restored() {
        assert!(!solo());
        with_solo(true, || {
            assert!(solo());
            assert!(
                !std::thread::spawn(solo).join().unwrap(),
                "other threads are unaffected"
            );
            with_solo(false, || assert!(!solo()));
            assert!(solo(), "restored after the inner scope");
        });
        assert!(!solo());
    }
}
