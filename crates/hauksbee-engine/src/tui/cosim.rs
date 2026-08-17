//! The co-sim worker: runs the firmware co-simulation on a background thread and
//! streams incremental updates to the UI over a channel, so the TUI stays
//! responsive while QEMU/Renode grinds.
//!
//! The worker owns the [`HauksbeeEngine`] and steps the scheduler in small
//! chunks. After each chunk it sends a [`CosimUpdate`] snapshot (sim time, wall
//! time, the tail of the UART stream, and the level of key GPIO/LED nets). The
//! UI thread keeps the latest snapshot and renders it. A `stop` flag lets the UI
//! ask the worker to wind down cleanly.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hauksbee_frontdoor_api::engine::Engine;

use crate::engine::HauksbeeEngine;

/// Coverage fields change as firmware runs, but source-conflict discovery walks
/// the immutable circuit topology. Cache that part once per worker instead of
/// repeating the scan for every UI frame.
struct CoverageSampler {
    drive_conflicts: Vec<String>,
}

impl CoverageSampler {
    fn new(scheduler: &crate::scheduler::Scheduler) -> Self {
        Self::from_scan(|| scheduler.drive_conflicts())
    }

    fn from_scan(scan: impl FnOnce() -> Vec<String>) -> Self {
        Self {
            drive_conflicts: scan(),
        }
    }

    fn capture(
        &self,
        scheduler: &crate::scheduler::Scheduler,
    ) -> crate::reports::coverage::CoverageInputs {
        crate::reports::coverage::CoverageInputs::from_scheduler_with_drive_conflicts(
            scheduler,
            self.drive_conflicts.clone(),
        )
    }
}

/// How many UART lines to keep in the rolling buffer shown in the pane.
const UART_TAIL_LINES: usize = 200;
const MAX_INTERACTIVE_CHUNK_MS: f64 = 100.0;

/// An incremental snapshot streamed from the worker to the UI.
#[derive(Debug, Clone, Default)]
pub struct CosimUpdate {
    /// Simulated time elapsed (ms).
    pub sim_ms: f64,
    /// Wall-clock time elapsed since the run started (s).
    pub wall_s: f64,
    /// The chunk size in ms (so the pane can show "chunk=5 ms").
    pub chunk_ms: f64,
    /// The tail of the UART output, line-split.
    pub uart_lines: Vec<String>,
    /// Key control/LED GPIO nets and their level (V), sorted by name. Each entry
    /// carries whether the net is being actively driven (level moved off its
    /// boot baseline at some point during the run).
    pub gpio_nets: Vec<GpioNet>,
    /// True once at least one byte of UART output has been seen.
    pub uart_seen: bool,
    /// True once at least one watched GPIO/control net has changed level since
    /// boot, i.e. the firmware visibly drove something. While this stays false
    /// past the boot window the pane shows the stall note.
    pub gpio_active: bool,
    /// True once the firmware has driven ANY GPIO output edge (from the
    /// scheduler's pin-change record), even one that is set high and HELD with no
    /// further movement. This is the honest "the firmware ran" signal: `gpio_active`
    /// (net moved off baseline) misses a drive-and-hold boot line, so the stall
    /// note must also consult this or it cries wolf on working boot firmware,
    /// matching the CLI/web `Scheduler::any_gpio_driven()` behaviour.
    pub gpio_driven: bool,
    /// Chip-substitution caveat, if the firmware was emulated on a less-specific
    /// core (e.g. STM32F411 modelled as F407). Mirrors the CLI/web note so the
    /// TUI does not silently present substitute-chip results as exact.
    pub substitution: Option<String>,
    /// False once the co-sim's analog solve failed to converge on at least one
    /// chunk: the run held stale node voltages and cannot vouch for the GPIO/net
    /// levels shown in this pane (05 §3b). A clean run reports `true`. Read from
    /// `Scheduler::analog_valid()`, the same signal the CLI `--json` and the web
    /// report surface, so a diverged co-sim no longer looks quiet HERE.
    /// `Default` is `false`, so build every real snapshot through
    /// `build_update` (which sets it) rather than relying on the derive.
    pub analog_valid: bool,
    /// SPI buses still framed by the chunk-boundary heuristic (no CS pin
    /// resolved and the backend does not frame itself). Retained for source
    /// compatibility with the planned 0.1 public type; the TUI's complete
    /// caveat list travels through a private snapshot beside this update.
    pub heuristic_spi_buses: Vec<String>,
    /// Count of chunks whose analog solve failed this run. `0` on a clean run;
    /// non-zero drives the loud invalid line in the pane. Kept alongside
    /// `analog_valid` so the pane can say HOW MANY chunks were held stale.
    pub failed_chunk_count: u64,
    /// True once the run has finished (reached the target or was stopped).
    pub done: bool,
    /// Set on a hard error (e.g. firmware failed to load).
    pub error: Option<String>,
    /// The full per-net voltage snapshot at this chunk (the same
    /// `frame.net_voltages` the tracker reads). Carried so the scope pane can
    /// sample the history of ANY net the user probes from the parts/nets list,
    /// not just the watched GPIO subset in `gpio_nets`. This reuses the worker's
    /// existing per-chunk data; it is not a second co-sim path.
    pub net_voltages: HashMap<String, f64>,
}

/// One watched GPIO / control / LED net in a co-sim snapshot.
#[derive(Debug, Clone)]
pub struct GpioNet {
    pub name: String,
    /// Current level (V).
    pub volts: f64,
    /// True once this net's level has moved off its boot baseline, i.e. the MCU
    /// is (or was) actively driving it. Distinguishes a deliberately-driven line
    /// from one sitting at a static rail.
    pub driven: bool,
}

/// The handle the UI holds onto: a receiver for updates and a stop flag.
pub struct CosimHandle {
    pub rx: Receiver<CosimUpdate>,
    coverage: Arc<Mutex<Vec<crate::reports::coverage::CoverageCaveat>>>,
    stop: Arc<AtomicBool>,
    /// Owned until the terminal has been restored. Engine construction and an
    /// external-emulator step are not interruptible wall-time operations, so
    /// the UI thread retires this handle nonblocking and joins it only after
    /// leaving raw/alternate-screen mode.
    join: Option<std::thread::JoinHandle<()>>,
}

impl CosimHandle {
    /// Ask the worker to stop at the next chunk boundary.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn latest_coverage(&self) -> Vec<crate::reports::coverage::CoverageCaveat> {
        self.coverage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for CosimHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Disconnect publication before joining. Otherwise a worker stopped
        // while a normal snapshot fills the capacity-one channel can block
        // forever trying to send its final update.
        let (_dummy_tx, dummy_rx) = std::sync::mpsc::sync_channel(1);
        let connected_rx = std::mem::replace(&mut self.rx, dummy_rx);
        drop(connected_rx);
        // A bare public handle owns cleanup too. The TUI avoids doing this while
        // raw mode is active by transferring its join handle into
        // `CosimWorkers::retired`; external callers still get safe teardown.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Owns the active and stopping workers across UI restarts. Retiring is
/// nonblocking, but a replacement cannot start until the old worker has exited,
/// and normal TUI shutdown joins every retained worker after terminal restore.
#[derive(Default)]
pub(crate) struct CosimWorkers {
    active: Option<CosimHandle>,
    retired: Vec<std::thread::JoinHandle<()>>,
}

impl CosimWorkers {
    pub(crate) fn active(&self) -> Option<&CosimHandle> {
        self.active.as_ref()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn is_stopping(&self) -> bool {
        !self.retired.is_empty()
    }

    pub(crate) fn reap_finished(&mut self) {
        let mut pending = Vec::new();
        for join in self.retired.drain(..) {
            if join.is_finished() {
                let _ = join.join();
            } else {
                pending.push(join);
            }
        }
        self.retired = pending;
    }

    pub(crate) fn can_start(&mut self) -> bool {
        self.reap_finished();
        self.active.is_none() && self.retired.is_empty()
    }

    pub(crate) fn try_start(&mut self, handle: CosimHandle) -> bool {
        if !self.can_start() {
            return false;
        }
        self.active = Some(handle);
        true
    }

    pub(crate) fn retire_active(&mut self) {
        if let Some(mut handle) = self.active.take() {
            handle.stop();
            if let Some(join) = handle.join.take() {
                self.retired.push(join);
            }
            // Drop the receiver now. A worker trying to publish a terminal
            // update to a full capacity-one channel gets Disconnected instead
            // of blocking the eventual shutdown join forever.
            drop(handle);
        }
    }

    pub(crate) fn finish_active(&mut self) {
        if let Some(mut handle) = self.active.take() {
            if let Some(join) = handle.join.take() {
                self.retired.push(join);
            }
            drop(handle);
        }
        self.reap_finished();
    }

    pub(crate) fn stop_and_join_all(&mut self) {
        self.retire_active();
        for join in self.retired.drain(..) {
            let _ = join.join();
        }
    }
}

impl Drop for CosimWorkers {
    fn drop(&mut self) {
        // Covers panic unwind and any future early-return path. The terminal's
        // panic hook restores raw mode first; this owner then keeps the process
        // alive until every external-emulator owner has run cleanup.
        self.stop_and_join_all();
    }
}

/// Spawn the co-sim worker. It builds the engine on the worker thread (so the UI
/// never blocks on QEMU boot), steps in chunks, and streams snapshots.
///
/// `board_text` is the board file's text (so the engine binds the same board the
/// static panes analysed); `firmware` is the optional ELF/HEX to co-sim;
/// `seconds` is the target simulated duration; `chunk_ms` is the exact scheduler
/// chunk selected by the caller.
pub fn spawn(
    board_text: String,
    firmware: Option<PathBuf>,
    board_name: String,
    seconds: f64,
    chunk_ms: f64,
) -> CosimHandle {
    // At most one full snapshot may wait behind the renderer. Intermediate
    // updates coalesce by being dropped while this slot is occupied; the final
    // done/error update waits only for that one slot to drain. This bounds both
    // memory and the amount the UI can drain before polling the keyboard.
    let (tx, rx): (SyncSender<CosimUpdate>, Receiver<CosimUpdate>) =
        std::sync::mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let coverage = Arc::new(Mutex::new(Vec::new()));
    let coverage_worker = Arc::clone(&coverage);

    let join = std::thread::spawn(move || {
        run_worker(
            board_text,
            firmware,
            &board_name,
            seconds,
            chunk_ms,
            &tx,
            &coverage_worker,
            &stop_worker,
        );
    });

    CosimHandle {
        rx,
        coverage,
        stop,
        join: Some(join),
    }
}

fn run_worker(
    board_text: String,
    firmware: Option<PathBuf>,
    board_name: &str,
    seconds: f64,
    chunk_ms: f64,
    tx: &SyncSender<CosimUpdate>,
    coverage_state: &Mutex<Vec<crate::reports::coverage::CoverageCaveat>>,
    stop: &AtomicBool,
) {
    if !(chunk_ms > 0.0 && chunk_ms.is_finite()) {
        publish_update(
            coverage_state,
            Vec::new(),
            tx,
            CosimUpdate {
                done: true,
                error: Some(format!(
                    "co-sim could not start: chunk must be positive and finite, got {chunk_ms} ms"
                )),
                chunk_ms,
                ..Default::default()
            },
        );
        return;
    }
    // Build the engine on the worker thread. A failure here (bad firmware arch,
    // unbindable board) is surfaced as an error update, never a silent hang.
    let board_url = format!("/boards/{board_name}");
    let mut engine =
        match HauksbeeEngine::from_board_file(&board_text, firmware.as_deref(), &board_url) {
            Ok(e) => e,
            Err(e) => {
                publish_update(
                    coverage_state,
                    Vec::new(),
                    tx,
                    CosimUpdate {
                        done: true,
                        error: Some(format!("co-sim could not start: {e}")),
                        chunk_ms,
                        ..Default::default()
                    },
                );
                return;
            }
        };
    // The requested solver chunk is exact. Rendering need not emit one channel
    // message per sub-100 us solve: `step(frame_dt)` subdivides internally at
    // `chunk_s`, so cap only the UI sampling cadence, never the solver setting.
    // This keeps a 1 us remediation from queuing two million snapshots during a
    // two-second run while still delivering the requested edge resolution.
    let (solver_chunk_s, frame_dt) = worker_durations(chunk_ms);
    engine.scheduler_mut().chunk_s = solver_chunk_s;
    let target_s = seconds.max(0.0);
    let start = Instant::now();
    let mut uart: VecDeque<String> = VecDeque::new();
    let mut uart_partial = String::new();
    let mut uart_seen = false;
    let mut t = 0.0;
    // The most recent per-net voltage snapshot, kept so the final `done` update
    // carries the last frame's voltages too (parity with the in-loop sends).
    let mut last_voltages: HashMap<String, f64> = HashMap::new();

    // Per-net activity tracker: remember each watched net's boot baseline level
    // and whether it has ever moved off it. A net that moves is one the firmware
    // is (or was) actively driving, that's the live observability the pane is
    // for, and it's also how we tell "stalled" from "running but quiet".
    let mut tracker = NetActivity::default();
    let coverage_sampler = CoverageSampler::new(engine.scheduler());

    loop {
        if stop.load(Ordering::Relaxed) || t >= target_s {
            break;
        }
        let step_dt = bounded_step_dt(frame_dt, target_s, t);
        let frame = engine.step(step_dt);
        t += step_dt;

        // Accumulate UART, split into lines. Iterate in sorted-by-MCU-key order
        // so a multi-MCU board's merged UART is deterministic run-to-run, not
        // HashMap iteration order, matching reports/cosim.rs and frontdoor.rs.
        let mut uart_entries: Vec<_> = frame.uart.iter().collect();
        uart_entries.sort_by(|a, b| a.0.cmp(b.0));
        for (_, bytes) in uart_entries {
            if !bytes.is_empty() {
                uart_seen = true;
            }
            let s = String::from_utf8_lossy(bytes);
            for ch in s.chars() {
                if ch == '\n' {
                    uart.push_back(std::mem::take(&mut uart_partial));
                    while uart.len() > UART_TAIL_LINES {
                        uart.pop_front();
                    }
                } else if ch != '\r' {
                    uart_partial.push(ch);
                }
            }
        }

        // Update the activity tracker and build the watched-net snapshot.
        tracker.observe(&frame.net_voltages);

        // Honest "firmware ran" signal + substitution caveat, read from the
        // scheduler (not just net movement), so a drive-and-hold boot line is not
        // mistaken for a stall.
        let gpio_driven = engine.scheduler().any_gpio_driven();
        let substitution = engine
            .scheduler()
            .substitutions()
            .first()
            .map(|s| s.message());
        // Analog-validity signal (05 §3b): once any chunk's analog solve fails,
        // the GPIO/net levels below are read off held-stale voltages. Surface it
        // so the pane can refuse rather than present them as trustworthy.
        let analog_valid = engine.scheduler().analog_valid();
        let failed_chunk_count = engine.scheduler().failed_chunk_count();
        // Coverage caveats through the shared enumeration, never a local rule:
        // the pane says what `hauksbee run --json` says because it reads the
        // same list.
        let coverage = coverage_sampler.capture(engine.scheduler()).caveats();

        // Move the frame's net voltages into the snapshot; keep a copy so the
        // final `done` update can carry the last frame's voltages too.
        let net_voltages = frame.net_voltages;
        // If the UI has gone away, stop.
        let update = build_update(
            t,
            &start,
            chunk_ms,
            &uart,
            &uart_partial,
            uart_seen,
            &tracker,
            gpio_driven,
            substitution,
            analog_valid,
            failed_chunk_count,
            coverage,
            false,
            net_voltages.clone(),
        );
        last_voltages = net_voltages;
        if !publish_snapshot(coverage_state, tx, update) {
            return;
        }
    }

    // Final snapshot marked done, keep the GPIO/UART state so the finished pane
    // still shows what the firmware drove (don't blank it on completion).
    let gpio_driven = engine.scheduler().any_gpio_driven();
    let substitution = engine
        .scheduler()
        .substitutions()
        .first()
        .map(|s| s.message());
    let analog_valid = engine.scheduler().analog_valid();
    let failed_chunk_count = engine.scheduler().failed_chunk_count();
    let coverage = coverage_sampler.capture(engine.scheduler()).caveats();
    let _ = publish_snapshot(
        coverage_state,
        tx,
        build_update(
            t,
            &start,
            chunk_ms,
            &uart,
            &uart_partial,
            uart_seen,
            &tracker,
            gpio_driven,
            substitution,
            analog_valid,
            failed_chunk_count,
            coverage,
            true,
            last_voltages,
        ),
    );
}

struct WorkerSnapshot {
    update: CosimUpdate,
    coverage: Vec<crate::reports::coverage::CoverageCaveat>,
}

fn publish_update(
    coverage_state: &Mutex<Vec<crate::reports::coverage::CoverageCaveat>>,
    coverage: Vec<crate::reports::coverage::CoverageCaveat>,
    tx: &SyncSender<CosimUpdate>,
    update: CosimUpdate,
) -> bool {
    *coverage_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = coverage;
    if update.done || update.error.is_some() {
        tx.send(update).is_ok()
    } else {
        match tx.try_send(update) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn publish_snapshot(
    coverage_state: &Mutex<Vec<crate::reports::coverage::CoverageCaveat>>,
    tx: &SyncSender<CosimUpdate>,
    snapshot: WorkerSnapshot,
) -> bool {
    publish_update(coverage_state, snapshot.coverage, tx, snapshot.update)
}

/// Build a [`CosimUpdate`] snapshot from the worker's live state. Both the
/// in-loop and final-`done` sends go through here so they can't drift apart.
#[allow(clippy::too_many_arguments)]
fn build_update(
    t: f64,
    start: &Instant,
    chunk_ms: f64,
    uart: &VecDeque<String>,
    uart_partial: &str,
    uart_seen: bool,
    tracker: &NetActivity,
    gpio_driven: bool,
    substitution: Option<String>,
    analog_valid: bool,
    failed_chunk_count: u64,
    coverage: Vec<crate::reports::coverage::CoverageCaveat>,
    done: bool,
    net_voltages: HashMap<String, f64>,
) -> WorkerSnapshot {
    let heuristic_spi_buses = coverage
        .iter()
        .filter(|caveat| {
            caveat.class == crate::reports::coverage::CoverageClass::HeuristicSpiFraming
        })
        .map(|caveat| caveat.subject.clone())
        .collect();
    WorkerSnapshot {
        update: CosimUpdate {
            sim_ms: t * 1000.0,
            wall_s: start.elapsed().as_secs_f64(),
            chunk_ms,
            uart_lines: collect_lines(uart, uart_partial),
            gpio_nets: tracker.snapshot(),
            uart_seen,
            gpio_active: tracker.any_driven(),
            gpio_driven,
            substitution,
            analog_valid,
            failed_chunk_count,
            heuristic_spi_buses,
            done,
            error: None,
            net_voltages,
        },
        coverage,
    }
}

/// Flatten the rolling UART buffer (plus any in-progress partial line) into a
/// vec of lines for the snapshot.
fn collect_lines(uart: &VecDeque<String>, partial: &str) -> Vec<String> {
    let mut lines: Vec<String> = uart.iter().cloned().collect();
    if !partial.is_empty() {
        lines.push(partial.to_string());
    }
    lines
}

/// Per-net state in the activity tracker: the boot baseline level, the latest
/// observed level, and whether the net has ever moved off its baseline.
struct NetState {
    baseline_v: f64,
    latest_v: f64,
    driven: bool,
}

/// Tracks the watched GPIO/control nets across the run: their boot baseline
/// level and whether each has ever moved off it (been actively driven).
#[derive(Default)]
struct NetActivity {
    nets: std::collections::HashMap<String, NetState>,
}

/// A net level is considered "moved" off baseline once it shifts by more than
/// this many volts, comfortably above solver noise, below a logic swing.
const MOVE_THRESHOLD_V: f64 = 0.3;

/// Cap on watched nets shown in the GPIO pane, so it stays readable. A deliberate
/// UI constraint, not an accident; the `snapshot_caps_at_twelve` test mirrors it.
const GPIO_PANE_MAX_NETS: usize = 12;

impl NetActivity {
    fn observe(&mut self, net_voltages: &std::collections::HashMap<String, f64>) {
        for (name, &v) in net_voltages {
            if !is_watch_net(name) {
                continue;
            }
            let e = self.nets.entry(name.clone()).or_insert(NetState {
                baseline_v: v,
                latest_v: v,
                driven: false,
            });
            e.latest_v = v;
            if (v - e.baseline_v).abs() > MOVE_THRESHOLD_V {
                e.driven = true;
            }
        }
    }

    fn any_driven(&self) -> bool {
        self.nets.values().any(|state| state.driven)
    }

    /// The watched nets, sorted by name, capped for a readable pane.
    fn snapshot(&self) -> Vec<GpioNet> {
        let mut out: Vec<GpioNet> = self
            .nets
            .iter()
            .map(|(name, state)| GpioNet {
                name: name.clone(),
                volts: state.latest_v,
                driven: state.driven,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.truncate(GPIO_PANE_MAX_NETS);
        out
    }
}

/// A net worth watching during co-sim: an LED, a boot/enable strap, a named
/// GPIO/PWM/control line, or an MCU pin net (Arduino `Dnn` / `Ann`, or an
/// STM32-style `Pxnn` port pin). These are the lines the firmware drives, which
/// is what an engineer wants to watch toggle.
fn is_watch_net(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    // Power/ground rails are never "the LED", exclude them explicitly so a
    // matched keyword inside a rail name doesn't pull a static rail in.
    let bare = upper
        .trim_start_matches(['/', '+'])
        .trim_start_matches("NET-(");
    if matches!(
        bare,
        "GND" | "VCC" | "VDD" | "VBUS" | "3V3" | "5V" | "+3V3" | "+5V"
    ) {
        return false;
    }
    if [
        "LED", "BOOT", "GPIO", "PWM", "MOTOR", "CTRL", "NRST", "RESET", "DSHOT",
    ]
    .iter()
    .any(|k| upper.contains(k))
    {
        return true;
    }
    // A bare "EN" / "*_EN" enable strap (but not a substring like "GREEN").
    if bare == "EN" || bare.ends_with("_EN") || bare.starts_with("EN_") {
        return true;
    }
    // An MCU pin net: Arduino Dnn/Ann, or an STM32-style Pxnn port pin.
    looks_like_mcu_pin(bare)
}

/// True for an MCU pin-net name like `D13`, `A0`, or `PA5` / `PC13`.
fn looks_like_mcu_pin(bare: &str) -> bool {
    let b = bare.trim_end_matches("_OUT").trim_end_matches("_IN");
    let bytes = b.as_bytes();
    match bytes {
        // Arduino digital/analog pin: D<digits> / A<digits>.
        [b'D' | b'A', rest @ ..] if !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) => true,
        // STM32 port pin: P<port letter><digits>, e.g. PA5, PC13.
        [b'P', port, rest @ ..]
            if port.is_ascii_alphabetic()
                && !rest.is_empty()
                && rest.iter().all(u8::is_ascii_digit) =>
        {
            true
        }
        _ => false,
    }
}

/// The default co-sim chunk (ms) for a backend. QEMU/Renode floor each step at a
/// few ms of wall time, so a coarse chunk keeps the run from appearing to hang;
/// the in-process AVR core is fine with a finer chunk.
pub fn default_chunk_ms(backend: Option<&str>) -> f64 {
    match backend {
        Some(b) if b.starts_with("simavr") => 1.0,
        _ => 5.0,
    }
}

/// Resolve the interactive worker chunk from the same `--chunk-us` value the
/// batch co-sim accepts. An explicit value always wins, including sub-100 us
/// values; silently widening it would make the TUI's remediation ineffective.
pub fn configured_chunk_ms(backend: Option<&str>, chunk_us: Option<f64>) -> anyhow::Result<f64> {
    match chunk_us {
        Some(us) => {
            anyhow::ensure!(
                us > 0.0 && us.is_finite(),
                "--chunk-us must be a positive number of microseconds, got {us}"
            );
            anyhow::ensure!(
                us <= MAX_INTERACTIVE_CHUNK_MS * 1000.0,
                "--chunk-us {us} is too coarse for the interactive dashboard; use at most {} us so stop/restart stays responsive, or use --headless for a coarser batch run",
                MAX_INTERACTIVE_CHUNK_MS * 1000.0
            );
            Ok(us / 1000.0)
        }
        None => Ok(default_chunk_ms(backend)),
    }
}

fn worker_durations(chunk_ms: f64) -> (f64, f64) {
    let solver_chunk_s = chunk_ms / 1000.0;
    (solver_chunk_s, solver_chunk_s.max(1e-4))
}

fn bounded_step_dt(frame_dt: f64, target_s: f64, elapsed_s: f64) -> f64 {
    frame_dt.min((target_s - elapsed_s).max(0.0))
}

/// Auto-detect a sibling firmware ELF next to a board file: look for a `.elf` in
/// the board's directory or common `build`/`Debug`/`Release` siblings.
pub fn autodetect_firmware(board_path: &std::path::Path) -> Option<PathBuf> {
    let dir = board_path.parent()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut search_dirs = vec![dir.to_path_buf()];
    for sub in ["build", "Debug", "Release", "Code/Debug", "Code/Release"] {
        search_dirs.push(dir.join(sub));
    }
    for d in search_dirs {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("elf") {
                    candidates.push(p);
                }
            }
        }
    }
    candidates.sort();
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn retiring_a_worker_is_nonblocking_but_shutdown_joins_it() {
        use std::time::Duration;

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(CosimUpdate::default()).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            while !stop_worker.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            // The normal snapshot already fills this capacity-one channel. A
            // retained receiver would make this terminal publication block
            // forever; retirement must disconnect it before the shutdown join.
            assert!(tx
                .send(CosimUpdate {
                    done: true,
                    ..Default::default()
                })
                .is_err());
            release_rx.recv().unwrap();
            finished_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let handle = CosimHandle {
            rx,
            coverage: Arc::new(Mutex::new(Vec::new())),
            stop,
            join: Some(join),
        };

        let mut workers = CosimWorkers::default();
        assert!(workers.try_start(handle));

        let started = Instant::now();
        workers.retire_active();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "terminal teardown waited for the worker: {:?}",
            started.elapsed()
        );
        assert!(
            !workers.can_start(),
            "a replacement worker must not overlap one still shutting down"
        );
        release_tx.send(()).unwrap();
        workers.stop_and_join_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("post-terminal shutdown joins the worker before returning");
    }

    #[test]
    fn bare_public_handle_drop_waits_for_owned_worker_cleanup() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(CosimUpdate::default()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let (clean_tx, clean_rx) = std::sync::mpsc::sync_channel(1);
        let join = std::thread::spawn(move || {
            while !stop_worker.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            assert!(tx
                .send(CosimUpdate {
                    done: true,
                    ..Default::default()
                })
                .is_err());
            clean_tx.send(()).unwrap();
        });
        let handle = CosimHandle {
            rx,
            coverage: Arc::new(Mutex::new(Vec::new())),
            stop,
            join: Some(join),
        };

        drop(handle);
        clean_rx
            .try_recv()
            .expect("public Drop returns only after worker-owned cleanup ran");
    }

    #[test]
    fn snapshot_transport_is_capacity_one_and_terminal_updates_are_retained() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let coverage = Mutex::new(Vec::new());
        let first = CosimUpdate {
            sim_ms: 1.0,
            ..Default::default()
        };
        let coalesced = CosimUpdate {
            sim_ms: 2.0,
            ..Default::default()
        };
        assert!(publish_update(&coverage, Vec::new(), &tx, first));
        assert!(
            publish_update(&coverage, Vec::new(), &tx, coalesced),
            "a full snapshot slot coalesces without stopping the worker"
        );
        assert_eq!(rx.try_iter().count(), 1, "the backlog is strictly bounded");

        let done = CosimUpdate {
            sim_ms: 3.0,
            done: true,
            ..Default::default()
        };
        assert!(publish_update(&coverage, Vec::new(), &tx, done));
        assert!(
            rx.recv().unwrap().done,
            "the terminal update is never coalesced away"
        );
    }

    #[test]
    fn watch_net_matches_led_boot_gpio_and_pins() {
        // Keyword-matched control/LED nets (the "LED"/"BOOT"/"PWM"/"_EN" paths).
        assert!(is_watch_net("/LED1"));
        assert!(is_watch_net("/BOOT0"));
        assert!(is_watch_net("/MOTOR_PWM"));
        assert!(is_watch_net("EN"));
        assert!(is_watch_net("3V3_EN"));
        assert!(is_watch_net("LED_A"));
        assert!(is_watch_net("PC13_LED"));
        // MCU pin nets the firmware drives (the looks_like_mcu_pin path).
        assert!(is_watch_net("D13"));
        assert!(is_watch_net("A0"));
        assert!(is_watch_net("PA5"));
        assert!(is_watch_net("PA5_OUT"));
        // Rails and ground are not "the LED".
        assert!(!is_watch_net("/VBUS"));
        assert!(!is_watch_net("GND"));
        assert!(!is_watch_net("+5V"));
        assert!(!is_watch_net("+3V3"));
        // A keyword buried in a non-control name (GREEN contains "EN") shouldn't
        // match on the EN strap rule.
        assert!(!is_watch_net("GREEN_RAIL"));
    }

    #[test]
    fn net_activity_tracks_baseline_and_driven() {
        let mut tr = NetActivity::default();
        // Boot: LED_A at 0 V, PA5 at 0 V.
        let mut v = HashMap::new();
        v.insert("LED_A".to_string(), 0.0);
        v.insert("PA5".to_string(), 0.0);
        v.insert("GND".to_string(), 0.0);
        tr.observe(&v);
        assert!(!tr.any_driven(), "nothing moved yet");
        // Firmware drives LED_A high; PA5 stays put.
        v.insert("LED_A".to_string(), 2.0);
        tr.observe(&v);
        assert!(tr.any_driven(), "LED_A moved off baseline");
        let snap = tr.snapshot();
        let led = snap.iter().find(|n| n.name == "LED_A").unwrap();
        assert!(led.driven, "LED_A flagged driven");
        assert!((led.volts - 2.0).abs() < 1e-9);
        let pa5 = snap.iter().find(|n| n.name == "PA5").unwrap();
        assert!(!pa5.driven, "PA5 never moved");
        // GND was filtered out entirely.
        assert!(snap.iter().all(|n| n.name != "GND"));
    }

    #[test]
    fn snapshot_caps_at_twelve() {
        let mut tr = NetActivity::default();
        let mut v = HashMap::new();
        for i in 0..20 {
            v.insert(format!("LED{i}"), 0.0);
        }
        tr.observe(&v);
        assert_eq!(tr.snapshot().len(), 12);
    }

    #[test]
    fn default_chunk_is_coarser_for_qemu_than_avr() {
        assert_eq!(default_chunk_ms(Some("simavr:atmega328p")), 1.0);
        assert_eq!(default_chunk_ms(Some("qemu:esp32c3")), 5.0);
        assert_eq!(default_chunk_ms(Some("renode:stm32f4")), 5.0);
        assert_eq!(default_chunk_ms(None), 5.0);
    }

    #[test]
    fn explicit_cli_chunk_overrides_tui_default_without_a_hidden_floor() {
        assert_eq!(
            configured_chunk_ms(Some("renode:stm32f4"), Some(250.0)).unwrap(),
            0.25
        );
        assert_eq!(
            configured_chunk_ms(Some("simavr:atmega328p"), Some(25.0)).unwrap(),
            0.025,
            "a requested sub-100 us chunk must not be silently widened"
        );
        assert_eq!(
            configured_chunk_ms(Some("renode:stm32f4"), None).unwrap(),
            default_chunk_ms(Some("renode:stm32f4"))
        );
        assert!(configured_chunk_ms(None, Some(0.0)).is_err());
        assert!(configured_chunk_ms(None, Some(f64::NAN)).is_err());
        assert!(configured_chunk_ms(None, Some(100_001.0)).is_err());

        let (solver_chunk_s, ui_frame_s) = worker_durations(0.025);
        assert!(
            (solver_chunk_s - 25e-6).abs() < f64::EPSILON,
            "the worker applies the exact solver chunk: {solver_chunk_s}"
        );
        assert_eq!(ui_frame_s, 1e-4, "only the UI sampling cadence is floored");
    }

    #[test]
    fn final_worker_step_never_overshoots_the_requested_window() {
        assert!((bounded_step_dt(0.3, 2.0, 1.9) - 0.1).abs() < 1e-12);
        assert_eq!(bounded_step_dt(0.3, 2.0, 2.0), 0.0);
    }

    #[test]
    fn build_update_carries_analog_validity_flag() {
        // Finding 2 (05 §3b): a CosimUpdate must carry the analog-validity signal
        // the worker reads from the scheduler, so the pane can refuse rather than
        // present held-stale net levels as trustworthy. This is the terminal-free
        // state-level check: build an update through the same path the worker uses
        // and assert the two fields propagate.
        let start = Instant::now();
        let uart: VecDeque<String> = VecDeque::new();
        let tracker = NetActivity::default();

        // A clean run: valid, zero failed chunks.
        let clean = build_update(
            0.1,
            &start,
            1.0,
            &uart,
            "",
            false,
            &tracker,
            false,
            None,
            /*analog_valid*/ true,
            /*failed_chunk_count*/ 0,
            Vec::new(),
            false,
            HashMap::new(),
        );
        assert!(clean.update.analog_valid, "clean run stays analog-valid");
        assert_eq!(clean.update.failed_chunk_count, 0);

        // A diverged run: invalid, with the failed-chunk count carried through.
        let bad = build_update(
            0.1,
            &start,
            1.0,
            &uart,
            "",
            false,
            &tracker,
            false,
            None,
            /*analog_valid*/ false,
            /*failed_chunk_count*/ 3,
            Vec::new(),
            true,
            HashMap::new(),
        );
        assert!(
            !bad.update.analog_valid,
            "a failed chunk makes the update analog-invalid"
        );
        assert_eq!(
            bad.update.failed_chunk_count, 3,
            "the failed-chunk count reaches the UI snapshot"
        );
    }

    #[test]
    fn build_update_carries_the_coverage_caveats() {
        // The worker reads the caveats off the scheduler through the shared
        // enumeration; this is the transport check, that the list survives into
        // the snapshot the UI thread renders. Both sides: a run with a caveat
        // carries it, a clean run carries an empty list rather than a stale one.
        use crate::reports::coverage::{CoverageClass, CoverageInputs};
        use crate::scheduler::AdcDrop;

        let start = Instant::now();
        let uart: VecDeque<String> = VecDeque::new();
        let tracker = NetActivity::default();
        let build = |coverage: Vec<crate::reports::coverage::CoverageCaveat>| {
            build_update(
                0.1,
                &start,
                1.0,
                &uart,
                "",
                false,
                &tracker,
                false,
                None,
                true,
                0,
                coverage,
                false,
                HashMap::new(),
            )
        };

        let caveats = CoverageInputs {
            adc_dropped: vec![AdcDrop {
                mcu_ref: "U1".to_string(),
                channel: 4,
                net: "/VSENSE".to_string(),
                parts: Vec::new(),
            }],
            heuristic_spi_buses: vec!["SPI1".to_string()],
            ..Default::default()
        }
        .caveats();
        let carried = build(caveats.clone());
        assert_eq!(carried.coverage, caveats, "the caveat list reaches the UI");
        assert_eq!(carried.coverage[0].class, CoverageClass::AdcDropped);
        assert_eq!(
            carried.update.heuristic_spi_buses,
            ["SPI1"],
            "the compatible public field remains populated from the canonical caveat list"
        );

        assert!(
            build(Vec::new()).coverage.is_empty(),
            "a run with nothing to disclose carries an empty list"
        );
    }

    #[cfg(feature = "avr")]
    #[test]
    fn real_worker_rerun_replaces_watchdog_coverage_with_the_control_run() {
        use crate::reports::coverage::CoverageClass;
        use std::time::Duration;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let board_path = root.join("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb");
        let board = std::fs::read_to_string(&board_path)
            .unwrap_or_else(|error| panic!("tracked board fixture {board_path:?}: {error}"));
        let run = |name: &str| {
            let firmware = root.join(format!("testdata/firmware/avr_watchdog/{name}.elf"));
            assert!(firmware.exists(), "tracked required fixture {firmware:?}");
            let handle = spawn(
                board.clone(),
                Some(firmware),
                "blinky.kicad_pcb".into(),
                0.05,
                1.0,
            );
            loop {
                let update = handle
                    .rx
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap_or_else(|error| panic!("worker {name} did not finish: {error}"));
                assert!(update.error.is_none(), "worker {name}: {:?}", update.error);
                if update.done {
                    break handle.latest_coverage();
                }
            }
        };

        let watchdog = run("wdt");
        assert!(
            watchdog
                .iter()
                .any(|caveat| caveat.class == CoverageClass::WatchdogReboot),
            "the real worker must carry the scheduler's reboot disclosure: {watchdog:?}"
        );
        let control = run("nowdt");
        assert!(
            control
                .iter()
                .all(|caveat| caveat.class != CoverageClass::WatchdogReboot),
            "a rerun must carry only its own coverage, never the prior worker's: {control:?}"
        );
    }

    #[test]
    fn worker_coverage_sampler_caches_topology_conflicts_across_frames() {
        use std::cell::Cell;
        let scans = Cell::new(0);
        let sampler = CoverageSampler::from_scan(|| {
            scans.set(scans.get() + 1);
            vec!["V1 and V2 contest /RAIL".to_string()]
        });
        assert_eq!(scans.get(), 1);
        assert_eq!(sampler.drive_conflicts, ["V1 and V2 contest /RAIL"]);
        // Reading the cached value for many frames cannot invoke the topology
        // closure again; live fields are captured separately by `capture`.
        for _ in 0..20_000 {
            assert_eq!(sampler.drive_conflicts.len(), 1);
        }
        assert_eq!(scans.get(), 1);
    }
}
