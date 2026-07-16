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
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use hauksbee_server::engine::Engine;

use crate::engine::HauksbeeEngine;

/// How many UART lines to keep in the rolling buffer shown in the pane.
const UART_TAIL_LINES: usize = 200;

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
    /// boot — i.e. the firmware visibly drove something. While this stays false
    /// past the boot window the pane shows the stall note.
    pub gpio_active: bool,
    /// True once the firmware has driven ANY GPIO output edge (from the
    /// scheduler's pin-change record), even one that is set high and HELD with no
    /// further movement. This is the honest "the firmware ran" signal: `gpio_active`
    /// (net moved off baseline) misses a drive-and-hold boot line, so the stall
    /// note must also consult this or it cries wolf on working boot firmware —
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
    /// report surface, so the TUI stops being the one place a diverged co-sim
    /// looks quiet. `Default` is `false`, so build every real snapshot through
    /// `build_update` (which sets it) rather than relying on the derive.
    pub analog_valid: bool,
    /// SPI buses still framed by the chunk-boundary heuristic (no CS pin
    /// resolved and the backend does not frame itself): their transaction
    /// boundaries are guessed, which the pane says out loud.
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
    /// sample the history of ANY net the user probes from the parts/nets list —
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
    /// True once this net's level has moved off its boot baseline — i.e. the MCU
    /// is (or was) actively driving it. Distinguishes a deliberately-driven line
    /// from one sitting at a static rail.
    pub driven: bool,
}

/// The handle the UI holds onto: a receiver for updates and a stop flag.
pub struct CosimHandle {
    pub rx: Receiver<CosimUpdate>,
    stop: Arc<AtomicBool>,
    /// Kept so the thread is joined on drop (best-effort).
    join: Option<std::thread::JoinHandle<()>>,
}

impl CosimHandle {
    /// Ask the worker to stop at the next chunk boundary.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for CosimHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spawn the co-sim worker. It builds the engine on the worker thread (so the UI
/// never blocks on QEMU boot), steps in chunks, and streams snapshots.
///
/// `board_text` is the board file's text (so the engine binds the same board the
/// static panes analysed); `firmware` is the optional ELF/HEX to co-sim;
/// `seconds` is the target simulated duration; `chunk_ms` is the scheduler chunk
/// (coarsened for QEMU backends so the run doesn't appear to hang).
pub fn spawn(
    board_text: String,
    firmware: Option<PathBuf>,
    board_name: String,
    seconds: f64,
    chunk_ms: f64,
) -> CosimHandle {
    let (tx, rx): (Sender<CosimUpdate>, Receiver<CosimUpdate>) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();

    let join = std::thread::spawn(move || {
        run_worker(
            board_text,
            firmware,
            &board_name,
            seconds,
            chunk_ms,
            &tx,
            &stop_worker,
        );
    });

    CosimHandle {
        rx,
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
    tx: &Sender<CosimUpdate>,
    stop: &AtomicBool,
) {
    // Build the engine on the worker thread. A failure here (bad firmware arch,
    // unbindable board) is surfaced as an error update, never a silent hang.
    let board_url = format!("/boards/{board_name}");
    let mut engine = match HauksbeeEngine::from_board_file(
        &board_text,
        firmware.as_deref(),
        &board_url,
    ) {
        Ok(e) => e,
        Err(e) => {
            let _ = tx.send(CosimUpdate {
                done: true,
                error: Some(format!("co-sim could not start: {e}")),
                chunk_ms,
                ..Default::default()
            });
            return;
        }
    };
    // Coarsen the scheduler chunk so QEMU/Renode backends step in viable jumps
    // (the integration tests use 5 ms for exactly this reason; the 100 us
    // default would make the run appear to hang). The same value drives both the
    // scheduler field and the per-frame step below — derive it once.
    let frame_dt = (chunk_ms / 1000.0).max(1e-4);
    engine.scheduler_mut().chunk_s = frame_dt;
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
    // is (or was) actively driving — that's the live observability the pane is
    // for, and it's also how we tell "stalled" from "running but quiet".
    let mut tracker = NetActivity::default();

    loop {
        if stop.load(Ordering::Relaxed) || t >= target_s {
            break;
        }
        let frame = engine.step(frame_dt);
        t += frame_dt;

        // Accumulate UART, split into lines. Iterate in sorted-by-MCU-key order
        // so a multi-MCU board's merged UART is deterministic run-to-run, not
        // HashMap iteration order — matching reports/cosim.rs and frontdoor.rs.
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
        let heuristic_spi_buses: Vec<String> = engine
            .scheduler()
            .spi_framing_modes()
            .into_iter()
            .filter(|(_, m)| matches!(m, crate::peripherals::spi::SpiFramingMode::Heuristic))
            .map(|(b, _)| b)
            .collect();

        // Move the frame's net voltages into the snapshot; keep a copy so the
        // final `done` update can carry the last frame's voltages too.
        let net_voltages = frame.net_voltages;
        // If the UI has gone away, stop.
        let update = build_update(
            t, &start, chunk_ms, &uart, &uart_partial, uart_seen, &tracker, gpio_driven,
            substitution, analog_valid, failed_chunk_count, heuristic_spi_buses, false,
            net_voltages.clone(),
        );
        last_voltages = net_voltages;
        if tx.send(update).is_err() {
            return;
        }
    }

    // Final snapshot marked done — keep the GPIO/UART state so the finished pane
    // still shows what the firmware drove (don't blank it on completion).
    let gpio_driven = engine.scheduler().any_gpio_driven();
    let substitution = engine
        .scheduler()
        .substitutions()
        .first()
        .map(|s| s.message());
    let analog_valid = engine.scheduler().analog_valid();
    let failed_chunk_count = engine.scheduler().failed_chunk_count();
    let heuristic_spi_buses: Vec<String> = engine
        .scheduler()
        .spi_framing_modes()
        .into_iter()
        .filter(|(_, m)| matches!(m, crate::peripherals::spi::SpiFramingMode::Heuristic))
        .map(|(b, _)| b)
        .collect();
    let _ = tx.send(build_update(
        t, &start, chunk_ms, &uart, &uart_partial, uart_seen, &tracker, gpio_driven, substitution,
        analog_valid, failed_chunk_count, heuristic_spi_buses, true, last_voltages,
    ));
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
    heuristic_spi_buses: Vec<String>,
    done: bool,
    net_voltages: HashMap<String, f64>,
) -> CosimUpdate {
    CosimUpdate {
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
/// this many volts — comfortably above solver noise, below a logic swing.
const MOVE_THRESHOLD_V: f64 = 0.3;

/// Cap on watched nets shown in the GPIO pane, so it stays readable. A deliberate
/// UI constraint, not an accident — the `snapshot_caps_at_twelve` test mirrors it.
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
    // Power/ground rails are never "the LED" — exclude them explicitly so a
    // matched keyword inside a rail name doesn't pull a static rail in.
    let bare = upper.trim_start_matches(['/', '+']).trim_start_matches("NET-(");
    if matches!(bare, "GND" | "VCC" | "VDD" | "VBUS" | "3V3" | "5V" | "+3V3" | "+5V") {
        return false;
    }
    if ["LED", "BOOT", "GPIO", "PWM", "MOTOR", "CTRL", "NRST", "RESET", "DSHOT"]
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
            0.1, &start, 1.0, &uart, "", false, &tracker, false, None, /*analog_valid*/ true,
            /*failed_chunk_count*/ 0, Vec::new(), false, HashMap::new(),
        );
        assert!(clean.analog_valid, "clean run stays analog-valid");
        assert_eq!(clean.failed_chunk_count, 0);

        // A diverged run: invalid, with the failed-chunk count carried through.
        let bad = build_update(
            0.1, &start, 1.0, &uart, "", false, &tracker, false, None, /*analog_valid*/ false,
            /*failed_chunk_count*/ 3, Vec::new(), true, HashMap::new(),
        );
        assert!(!bad.analog_valid, "a failed chunk makes the update analog-invalid");
        assert_eq!(
            bad.failed_chunk_count, 3,
            "the failed-chunk count reaches the UI snapshot"
        );
    }
}
