//! `hauksbee run --serial-attach`: a live co-sim with a host-facing serial port,
//! so software on the user's own machine drives the emulated board the way it
//! would drive real hardware over USB serial.
//!
//! The transport lives in [`hauksbee_mcu::hostserial`] (a pty by default, so an
//! unmodified pyserial script or vendor tool works unmodified). This module is
//! the *session*: it owns the co-sim stepping loop, pumps bytes both ways once
//! per frame, and narrates what a user cannot otherwise see.
//!
//! Three decisions here are worth the ink:
//!
//! **The session is narrated.** A user who cannot tell whether their tool is
//! attached concludes the simulator is broken, so the device path, every attach,
//! every detach, and the final byte counts are printed. The endpoint line goes to
//! stderr in a paste-ready form because the interesting act is copying it into
//! another terminal.
//!
//! **Sim time is paced to wall-clock time by default.** A headless AVR co-sim
//! runs many times faster than real time, which is exactly wrong when a human or
//! a script with `time.sleep` is on the other end: the run would be over before
//! the peer's first write, and a firmware timeout measured in emulated
//! milliseconds would fire in wall microseconds. Pacing makes the emulated board
//! behave at the speed the host tool expects. `--serial-no-pace` free-runs only
//! while nobody is attached; see below for why it cannot free-run against a peer.
//!
//! **Host bytes land at load-independent simulated instants.** The peer is a
//! real process on the real clock, so the session has to define a mapping from
//! its wall-clock actions onto the sim clock, and the mapping must not depend
//! on how fast this machine happens to be running the sim. The earlier design
//! injected bytes at whatever sim time the loop had reached when `read`
//! returned, which made every arrival instant a function of how fast the loop
//! was spinning, i.e. of machine load: on an idle machine a host command
//! round-trip (a few wall milliseconds) landed simulated *seconds* apart,
//! under heavy load simulated *microseconds* apart, and timing-sensitive
//! verdicts (the NEP EEPROM gate) inverted with load.
//!
//! The mapping [`PacedInbox`] implements is *scaled reaction time*: an anchor
//! `(sim, wall)` is planted at each attach/detach transition and at every
//! firmware->host emission, and an inbound chunk observed at wall time `w` is
//! due at `anchor_sim + scale * (w - anchor_wall)` — the host's own reaction
//! gap, measured on its clock (which load does not stretch), scaled into sim
//! time. The chunk is delivered only when the sim clock reaches that instant,
//! and the sim is held at-or-behind the anchor's scaled schedule so an
//! arrival is never already in the past. Together: the same firmware and host
//! produce the same simulated byte-arrival schedule whether the sim is
//! keeping up or starved tenfold; a starved run just takes longer in wall
//! time to play the identical schedule out.
//!
//! `scale` is 1 for paced sessions (a human at a terminal gets a real-time
//! board). Unpaced sessions use [`SerialSessionConfig::unpaced_wall_scale`]
//! (default 1/20): a deliberate compression chosen because a co-sim that runs
//! slower than real time — this analog+MCU stack sustains roughly 0.3x — can
//! never keep a 1:1 schedule against a host whose serial timeouts are wall
//! seconds; at 1/20 the sim only has to sustain ~2% of real time before the
//! host notices anything, and the mapped gaps (a 2 s port-settle sleep
//! becomes 100 ms of sim) stay far above the millisecond-scale timing a
//! firmware or part model actually keys on. When the sim is starved below
//! even that, the failure is a loud host-side timeout, never a silently
//! shifted verdict. `--serial-no-pace` still free-runs while nobody is
//! attached, and a pure-firmware run (no session, no peer) is untouched.
//!
//! **Nothing is faked when no peer shows up.** `--serial-wait` fails loudly on
//! timeout rather than running a session with nobody on the far end, and the
//! summary states the attach count, so "my tool never connected" is a visible
//! outcome rather than a silently empty run.
//!
//! The loop deliberately does NOT reuse `reports::cosim::run_headless`: that
//! function owns its own `while t < seconds` loop with no per-frame hook, and a
//! serial session needs to read the peer, pace, and report attach transitions
//! between frames.

use anyhow::{bail, Result};
use hauksbee_mcu::hostserial::{HostSerial, HostSerialTransport, PeerEvent};

use crate::engine::HauksbeeEngine;

/// What the CLI flags resolve to for one serial session.
#[derive(Debug, Clone)]
pub struct SerialSessionConfig {
    /// pty (default) or loopback TCP.
    pub transport: HostSerialTransport,
    /// Hold the co-sim at t=0 until a peer attaches, for at most this many
    /// seconds. `None` starts immediately.
    pub wait_secs: Option<f64>,
    /// Hold sim time to wall-clock time from t=0 (default true). `false`
    /// free-runs only until the first peer attaches: from that point host
    /// bytes are delivered on the scaled load-independent schedule the module
    /// doc describes, because a co-sim free-running against a real process
    /// has no defined mapping between the peer's wall clock and sim time, and
    /// byte-arrival instants (hence timing verdicts) would vary with machine
    /// load.
    pub pace: bool,
    /// How a host's wall-clock reaction gaps map into sim time for an unpaced
    /// session (paced sessions are always 1:1). See the module doc for why
    /// the default compresses: a sub-real-time sim cannot honour a 1:1
    /// schedule against wall-second serial timeouts, and the scaled gaps stay
    /// far above the timing any firmware or part model keys on. Must be in
    /// (0, 1].
    pub unpaced_wall_scale: f64,
    /// End the session early once at least one peer has attached, every peer
    /// has detached, all delivered bytes are drained, and this many WALL
    /// seconds have passed with nobody re-attaching. Termination only — it
    /// never changes what any delivered byte did — so scripted sessions (the
    /// NEP acceptance pair) don't grind out their whole sim budget after the
    /// host is done. `None` (default) runs the full budget.
    pub end_after_idle_wall: Option<f64>,
    /// Which MCU's UART to bridge. `None`/empty means every MCU on the board,
    /// which is the right default for the single-MCU case and honest for the
    /// multi-MCU one (the summary says which references exist).
    pub mcu: Option<String>,
    /// Solver chunk override in microseconds (`--chunk-us`), applied the same way
    /// the headless report path applies it.
    pub chunk_us: Option<f64>,
}

impl Default for SerialSessionConfig {
    fn default() -> Self {
        Self {
            transport: HostSerialTransport::Pty,
            wait_secs: None,
            pace: true,
            unpaced_wall_scale: DEFAULT_UNPACED_WALL_SCALE,
            end_after_idle_wall: None,
            mcu: None,
            chunk_us: None,
        }
    }
}

/// Default [`SerialSessionConfig::unpaced_wall_scale`]: see the module doc's
/// third design note for the reasoning behind 1/20.
pub const DEFAULT_UNPACED_WALL_SCALE: f64 = 0.05;

/// Load-independent delivery schedule for inbound host bytes.
///
/// This is the whole determinism story of a serial session, so it is one small
/// object with three rules rather than logic spread through the loop:
///
/// 1. **Anchoring.** An anchor `(sim, wall)` is (re)planted at every
///    attach/detach transition and at every firmware->host emission, via
///    [`Self::re_anchor`]. Emissions matter because a request-response host
///    reacts to the last thing the firmware said: measuring its next bytes
///    from that emission makes the measured gap a property of the *host*
///    (which machine load does not stretch), not of how long the starved sim
///    took to produce the emission.
/// 2. **Stamping.** An inbound chunk observed at wall `w` is due at
///    `anchor_sim + scale * (w - anchor_wall)`, and [`Self::take_due`]
///    releases it only when the sim clock reaches that instant. A starved sim
///    delivers the same bytes at the same simulated instants as a fast one;
///    it just takes longer in wall time to get there.
/// 3. **Ceiling.** [`Self::sim_ceiling`] gives the sim time the loop may run
///    up to right now. Ahead of it, a byte arriving "now" would map to a sim
///    instant already in the past, which is rule 2's one failure mode, so the
///    loop sleeps the difference off instead of stepping.
///
/// Before engagement (no peer yet, `pace: false`) all rules are off: bytes
/// pass through at the current sim time and the ceiling is infinite, which is
/// the free-running pre-attach fast path.
///
/// Wall time is a caller-supplied `f64` of seconds rather than an owned
/// `Instant` so tests can drive an arbitrary wall/sim schedule and assert the
/// delivery instants exactly.
struct PacedInbox {
    /// Host wall gap -> sim gap factor (1 for paced sessions; see
    /// [`SerialSessionConfig::unpaced_wall_scale`]).
    scale: f64,
    /// `Some((sim_anchor, wall_anchor))` once engaged.
    anchor: Option<(f64, f64)>,
    /// Chunks not yet released: (target sim instant, bytes), FIFO.
    queue: std::collections::VecDeque<(f64, Vec<u8>)>,
}

impl PacedInbox {
    fn new(scale: f64) -> Self {
        Self {
            scale,
            anchor: None,
            queue: std::collections::VecDeque::new(),
        }
    }

    fn engaged(&self) -> bool {
        self.anchor.is_some()
    }

    /// Plant the first anchor: sim instant `sim_now` corresponds to wall
    /// instant `wall_now`. No-op once engaged (use [`Self::re_anchor`] for the
    /// per-event moves).
    fn engage(&mut self, sim_now: f64, wall_now: f64) {
        if self.anchor.is_none() {
            self.anchor = Some((sim_now, wall_now));
        }
    }

    /// Move the anchor to the present (attach/detach transition, or a
    /// firmware->host emission the peer will react to). Only ever moves it
    /// forward in sim time, so already-stamped targets stay valid.
    fn re_anchor(&mut self, sim_now: f64, wall_now: f64) {
        if self.anchor.is_some() {
            self.anchor = Some((sim_now, wall_now));
        }
    }

    /// The sim instant a chunk observed at `wall_now` is due.
    fn target_for(&self, sim_now: f64, wall_now: f64) -> f64 {
        match self.anchor {
            Some((sim_anchor, wall_anchor)) => {
                sim_anchor + self.scale * (wall_now - wall_anchor).max(0.0)
            }
            None => sim_now,
        }
    }

    /// Queue an inbound chunk observed at `wall_now`. Unengaged, it is due
    /// immediately at `sim_now`.
    fn push(&mut self, sim_now: f64, wall_now: f64, bytes: Vec<u8>) {
        let target = self.target_for(sim_now, wall_now);
        // Monotonic by construction (anchors and wall time only grow), but
        // never trust float arithmetic to keep FIFO release order on its own.
        let floor = self.queue.back().map(|(t, _)| *t).unwrap_or(f64::MIN);
        self.queue.push_back((target.max(floor), bytes));
    }

    /// Whether any chunk is still waiting for its instant.
    fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Every queued chunk whose simulated arrival instant has been reached,
    /// concatenated in arrival order.
    fn take_due(&mut self, sim_now: f64) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some((target, _)) = self.queue.front() {
            if *target > sim_now {
                break;
            }
            let (_, bytes) = self.queue.pop_front().expect("front checked above");
            out.extend(bytes);
        }
        out
    }

    /// The sim time the loop may advance to as of `wall_now`. Infinite while
    /// unengaged (free-run).
    fn sim_ceiling(&self, wall_now: f64) -> f64 {
        match self.anchor {
            Some(_) => self.target_for(f64::INFINITY, wall_now),
            None => f64::INFINITY,
        }
    }
}

/// What the session did, for the closing summary and for tests.
#[derive(Debug, Clone, Default)]
pub struct SerialSessionSummary {
    /// The device path / address the endpoint offered.
    pub endpoint: String,
    /// Bytes the host tool sent into the firmware's UART RX.
    pub bytes_to_mcu: u64,
    /// Bytes of firmware output handed to the host tool.
    pub bytes_to_peer: u64,
    /// Firmware output bytes dropped because the endpoint's backlog filled.
    pub dropped_to_peer: u64,
    /// How many host tools attached over the session. Zero means nobody did.
    pub attach_count: u64,
    /// Simulated seconds delivered.
    pub sim_seconds: f64,
    /// Wall seconds spent.
    pub wall_seconds: f64,
    /// Per-MCU host bytes the backend could not deliver to the firmware. Any
    /// non-zero entry means the firmware did not see everything the host sent.
    pub rx_overflow: Vec<(String, u64)>,
}

/// Run a co-sim with a host-facing serial endpoint attached, until `seconds` of
/// simulated time have elapsed.
///
/// `announce` receives every human-facing line (already prefixed), so the CLI can
/// send them to stderr while a test can collect them.
pub fn run_session(
    engine: &mut HauksbeeEngine,
    seconds: f64,
    cfg: &SerialSessionConfig,
    announce: &mut dyn FnMut(&str),
) -> Result<SerialSessionSummary> {
    use hauksbee_server::engine::Engine;

    if engine.scheduler().mcu_identities().is_empty() {
        bail!(
            "--serial-attach bridges a host serial port to an emulated MCU's UART, but no MCU \
             was instantiated for this board (nothing would ever answer). Pass --firmware for a \
             board with a supported processor."
        );
    }
    anyhow::ensure!(
        seconds > 0.0 && seconds.is_finite(),
        "--seconds must be a positive number of simulated seconds for a serial session, got {seconds}"
    );

    // External emulators advance over a socket, so a 1 ms frame is thousands of
    // round-trips; the headless path makes the same 10 ms choice for them.
    let external = engine
        .scheduler()
        .mcu_identities()
        .iter()
        .any(|(_, backend, _)| backend.starts_with("renode:") || backend.starts_with("qemu:"));
    let mut frame_dt = if external {
        10.0 / 1000.0
    } else {
        1.0 / 1000.0
    };
    if external {
        engine.scheduler_mut().chunk_s = frame_dt;
    }
    if let Some(us) = cfg.chunk_us {
        anyhow::ensure!(
            us > 0.0 && us.is_finite(),
            "--chunk-us must be a positive number of microseconds, got {us}"
        );
        let chunk_s = us * 1e-6;
        engine.scheduler_mut().chunk_s = chunk_s;
        frame_dt = frame_dt.max(chunk_s);
    }

    let mut endpoint = HostSerial::open(cfg.transport)?;
    let mcu_ref = cfg.mcu.clone().unwrap_or_default();
    let mut summary = SerialSessionSummary {
        endpoint: endpoint.endpoint().to_string(),
        ..Default::default()
    };

    announce(&format!(
        "host serial: {} on {}",
        cfg.transport.as_str(),
        endpoint.endpoint()
    ));
    announce("host serial: attach your own software with one of:");
    for hint in endpoint.attach_hint() {
        announce(&format!("host serial:   {hint}"));
    }
    let mcus: Vec<String> = engine
        .scheduler()
        .mcu_identities()
        .iter()
        .map(|(reference, _, _)| reference.clone())
        .collect();
    announce(&format!(
        "host serial: wired to the UART of {}{}",
        if mcu_ref.is_empty() {
            mcus.join(", ")
        } else {
            mcu_ref.clone()
        },
        if mcu_ref.is_empty() && mcus.len() > 1 {
            " (every MCU on the board; use --serial-mcu REF to pick one)"
        } else {
            ""
        }
    ));
    announce(&format!(
        "host serial: baud is whatever the firmware configures; the endpoint is \
         transparent and does not rate-limit{}",
        if cfg.pace {
            ", and sim time is paced to wall-clock time"
        } else {
            ", and the co-sim free-runs until a peer attaches (--serial-no-pace), \
             then holds to wall-clock time so byte timing is load-independent"
        }
    ));

    // Waiting is opt-in because a fast run can finish before a human gets their
    // tool started, and a session nobody attached to is worthless.
    if let Some(limit) = cfg.wait_secs {
        announce(&format!(
            "host serial: waiting up to {limit:.0}s for a peer to open {} ...",
            endpoint.endpoint()
        ));
        let started = std::time::Instant::now();
        loop {
            report_events(&mut endpoint, &mut summary, announce);
            if endpoint.peer_attached() {
                break;
            }
            if started.elapsed().as_secs_f64() >= limit {
                bail!(
                    "no host tool attached to {} within {limit:.0}s (--serial-wait). Nothing \
                     would have driven the firmware, so the run is refused rather than reported \
                     as a quiet session.",
                    endpoint.endpoint()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    anyhow::ensure!(
        cfg.unpaced_wall_scale > 0.0 && cfg.unpaced_wall_scale <= 1.0,
        "unpaced_wall_scale must be in (0, 1], got {}",
        cfg.unpaced_wall_scale
    );
    let scale = if cfg.pace {
        1.0
    } else {
        // TEMP-DEBUG: allow overriding the unpaced scale from the environment
        // so heavily instrumented builds (ASan) can compress host gaps enough
        // to keep the protocol alive at their sim speeds.
        std::env::var("HAUKSBEE_WALL_SCALE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0 && *v <= 1.0)
            .unwrap_or(cfg.unpaced_wall_scale)
    };

    let run_started = std::time::Instant::now();
    let mut inbox = PacedInbox::new(scale);
    if cfg.pace {
        // Paced sessions are anchored from t=0; unpaced ones free-run until
        // the first attach plants the anchor inside the loop.
        inbox.engage(0.0, 0.0);
    }
    let mut t = 0.0f64;
    let mut was_attached = endpoint.peer_attached();
    // Wall instant of the last observable host activity, for
    // `end_after_idle_wall`.
    let mut last_activity_wall = 0.0f64;
    while t < seconds {
        report_events(&mut endpoint, &mut summary, announce);

        let wall = run_started.elapsed().as_secs_f64();
        let attached = endpoint.peer_attached();
        if !inbox.engaged() && endpoint.stats().attach_count > 0 {
            // "Has ever attached", not "is attached": a peer that attached and
            // detached within one frame still turns the timing discipline on,
            // and its bytes (readable after the hangup) get honest stamps.
            inbox.engage(t, wall);
        }
        if attached != was_attached {
            // A fresh peer's first gap is measured from its own attach, and a
            // detach closes the old peer's schedule, so both transitions move
            // the anchor.
            inbox.re_anchor(t, wall);
            was_attached = attached;
            last_activity_wall = wall;
        }

        // Host -> firmware, through the inbox so each byte lands at the sim
        // instant its scaled reaction gap maps to (see PacedInbox).
        // `uart_write` queues and meters, so a host record far longer than the
        // emulated RX fifo is delivered whole; that was the fifo-truncation
        // defect and this path must not reintroduce it by chunking or
        // dropping here.
        let inbound = endpoint.read_from_peer();
        if !inbound.is_empty() {
            inbox.push(t, wall, inbound);
            last_activity_wall = wall;
        }
        let due = inbox.take_due(t);
        if !due.is_empty() {
            engine.scheduler_mut().serial(&mcu_ref, &due);
        }

        // Hold the sim at-or-behind the anchored schedule (a byte arriving now
        // must not map to a sim instant already passed). Only needed while
        // bytes can still arrive against the current anchor: paced sessions
        // always, unpaced ones while a peer is attached or stamped bytes are
        // still queued — an unpaced session whose peer is gone free-runs.
        //
        // The endpoint is polled BETWEEN naps, with the naps kept short. A
        // scaled schedule can spend tens of wall milliseconds per frame
        // sleeping, and a multi-process host's close-then-reopen gap (NEP's
        // is 50 ms) fits inside one long nap: the session then never observes
        // the detach/attach pair, undercounting attaches and mis-anchoring
        // the new peer. Polling here also stamps bytes closer to their true
        // arrival than a once-per-frame read would.
        if cfg.pace || attached || inbox.pending() {
            loop {
                let ahead = (t + frame_dt) - inbox.sim_ceiling(run_started.elapsed().as_secs_f64());
                if ahead <= 0.0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs_f64(ahead.min(0.005)));
                report_events(&mut endpoint, &mut summary, announce);
                let wall_nap = run_started.elapsed().as_secs_f64();
                if endpoint.peer_attached() != was_attached {
                    inbox.re_anchor(t, wall_nap);
                    was_attached = endpoint.peer_attached();
                    last_activity_wall = wall_nap;
                }
                let inbound = endpoint.read_from_peer();
                if !inbound.is_empty() {
                    inbox.push(t, wall_nap, inbound);
                    last_activity_wall = wall_nap;
                }
            }
        }

        let frame = engine.step(frame_dt);
        t += frame_dt;

        // Firmware -> host, in a stable per-MCU order so a multi-MCU board's
        // merged stream is deterministic run to run.
        let mut entries: Vec<_> = frame.uart.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut emitted = false;
        for (_, bytes) in entries {
            if !bytes.is_empty() {
                endpoint.write_to_peer(bytes);
                emitted = true;
            }
        }
        if emitted && was_attached {
            // The peer reacts to what it just heard: measuring its next bytes
            // from this emission keeps the measured gap a host property
            // rather than a sim-starvation artifact (PacedInbox rule 1).
            // `was_attached` and not the loop-top `attached`: the nap loop
            // above may have observed a transition since.
            let wall_after = run_started.elapsed().as_secs_f64();
            inbox.re_anchor(t, wall_after);
            last_activity_wall = wall_after;
        }

        // Optional early end: the host came, spoke, and left. Termination
        // only — no delivered byte behaves differently — so a scripted
        // session doesn't grind out a large sim budget after its host exits.
        if let Some(idle_limit) = cfg.end_after_idle_wall {
            let wall_now = run_started.elapsed().as_secs_f64();
            if endpoint.stats().attach_count > 0
                && !endpoint.peer_attached()
                && !inbox.pending()
                && wall_now - last_activity_wall >= idle_limit
            {
                announce(&format!(
                    "host serial: every peer detached and {idle_limit:.1}s wall passed with \
                     no re-attach; ending the session early at t={t:.3}s of the {seconds:.1}s \
                     budget"
                ));
                break;
            }
        }
    }

    // A final pass so output produced by the last frame reaches an attached peer,
    // and so a detach during that frame is reported before the summary.
    report_events(&mut endpoint, &mut summary, announce);
    endpoint.flush();

    summary.sim_seconds = engine.scheduler().sim_time;
    summary.wall_seconds = run_started.elapsed().as_secs_f64();
    let stats = endpoint.stats();
    summary.bytes_to_mcu = stats.to_mcu;
    summary.bytes_to_peer = stats.to_peer;
    summary.dropped_to_peer = stats.dropped_to_peer;
    summary.attach_count = stats.attach_count;
    summary.rx_overflow = engine
        .scheduler()
        .uart_rx_overflow()
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .collect();
    Ok(summary)
}

/// Drain the endpoint's attach/detach transitions into printed lines.
fn report_events(
    endpoint: &mut HostSerial,
    summary: &mut SerialSessionSummary,
    announce: &mut dyn FnMut(&str),
) {
    for ev in endpoint.poll_peer() {
        match ev {
            PeerEvent::Attached => announce(&format!(
                "host serial: peer ATTACHED on {}",
                endpoint.endpoint()
            )),
            PeerEvent::Detached => {
                let st = endpoint.stats();
                announce(&format!(
                    "host serial: peer DETACHED ({} byte(s) in, {} byte(s) out so far); \
                     the co-sim keeps running and a new peer may attach",
                    st.to_mcu, st.to_peer
                ))
            }
        }
    }
    summary.attach_count = endpoint.stats().attach_count;
}

/// Render the closing summary. Separate from the loop so the wording is one
/// place, and so the honesty checks (nobody attached, bytes dropped, host bytes
/// the backend could not deliver) cannot be forgotten by a caller.
pub fn summary_lines(s: &SerialSessionSummary) -> Vec<String> {
    let mut out = vec![format!(
        "host serial: session over {} ({:.3}s simulated in {:.2}s wall): {} byte(s) host->MCU, \
         {} byte(s) MCU->host, {} peer attach(es)",
        s.endpoint, s.sim_seconds, s.wall_seconds, s.bytes_to_mcu, s.bytes_to_peer, s.attach_count
    )];
    if s.attach_count == 0 {
        out.push(
            "host serial: WARNING no host tool ever attached, so nothing exercised the \
             firmware over serial. The endpoint is only live while the run is; start the \
             run first, then attach."
                .to_string(),
        );
    }
    if s.dropped_to_peer > 0 {
        out.push(format!(
            "host serial: WARNING {} byte(s) of firmware output were DROPPED because the \
             endpoint's backlog filled (no peer attached, or the peer stopped reading). The \
             host tool did not see them.",
            s.dropped_to_peer
        ));
    }
    for (reference, n) in &s.rx_overflow {
        out.push(format!(
            "host serial: WARNING {n} host byte(s) never reached {reference}'s firmware: its \
             pending UART buffer overflowed, which means the firmware was not draining its \
             receiver. Findings that depend on those bytes are not trustworthy."
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The determinism invariant itself: the same wall-clock arrival schedule
    /// must produce byte-for-byte identical simulated delivery instants no
    /// matter how fast the sim is able to step. One "run" steps the sim 1 ms
    /// per wall ms (keeping up); the other is starved fivefold (a loaded
    /// machine). This is the two-sided guard on the NEP verdict-inversion
    /// defect: any change that lets wall speed leak back into arrival instants
    /// shifts the starved run's deliveries and fails here.
    #[test]
    fn delivery_instants_are_identical_regardless_of_sim_rate() {
        // (wall_arrival_s, payload): a command byte, a burst, a late chunk.
        let arrivals: [(f64, &[u8]); 3] = [(0.0031, b"W"), (0.0102, b"abc"), (0.0507, b"zz")];
        let mut runs = Vec::new();
        for wall_per_sim_frame in [0.001, 0.005] {
            let mut inbox = PacedInbox::new(1.0);
            inbox.engage(0.0, 0.0);
            let mut delivered = Vec::new();
            let mut next = 0;
            let mut t = 0.0f64;
            for frame in 0..200 {
                let wall = frame as f64 * wall_per_sim_frame;
                while next < arrivals.len() && arrivals[next].0 <= wall {
                    inbox.push(t, arrivals[next].0, arrivals[next].1.to_vec());
                    next += 1;
                }
                let due = inbox.take_due(t);
                if !due.is_empty() {
                    delivered.push((format!("{t:.6}"), due));
                }
                // The ceiling only permits stepping once the wall clock has
                // caught up, exactly as the session loop enforces by sleeping.
                if t + 0.001 <= inbox.sim_ceiling(wall) {
                    t += 0.001;
                }
            }
            assert_eq!(next, arrivals.len(), "every chunk was offered");
            runs.push(delivered);
        }
        assert!(
            !runs[0].is_empty(),
            "the schedule must actually deliver something"
        );
        assert_eq!(
            runs[0], runs[1],
            "a starved sim must deliver every host byte at the same simulated \
             instant as one that keeps up"
        );
    }

    /// A chunk arriving while the sim is behind its wall-mapped instant is
    /// held, not injected early; it is released the moment the sim clock
    /// reaches the instant.
    #[test]
    fn starved_sim_never_receives_bytes_early() {
        let mut inbox = PacedInbox::new(1.0);
        inbox.engage(0.0, 0.0);
        // Wall is at 0.5 s but the sim has only reached 0.1 s.
        inbox.push(0.1, 0.5, b"late".to_vec());
        assert!(inbox.take_due(0.1).is_empty());
        assert!(inbox.take_due(0.4999).is_empty());
        assert_eq!(inbox.take_due(0.5), b"late");
        assert!(inbox.take_due(10.0).is_empty(), "delivered exactly once");
    }

    /// Before a peer engages the inbox, bytes pass through at the current sim
    /// time and the ceiling is infinite: the free-running pre-attach path.
    #[test]
    fn unengaged_inbox_is_free_running_passthrough() {
        let mut inbox = PacedInbox::new(1.0);
        assert_eq!(inbox.sim_ceiling(0.0), f64::INFINITY);
        inbox.push(42.0, 3.0, b"x".to_vec());
        assert_eq!(inbox.take_due(42.0), b"x");
    }

    /// Engagement mid-run maps wall gaps onto sim gaps from the anchor, and a
    /// second engage must not move the anchor (only `re_anchor` may).
    #[test]
    fn anchor_is_planted_once_and_maps_wall_gaps_one_to_one() {
        let mut inbox = PacedInbox::new(1.0);
        // Free-ran to sim 5.0 by wall 0.25, then a peer attached.
        inbox.engage(5.0, 0.25);
        inbox.engage(99.0, 99.0); // must be ignored
        inbox.push(5.0, 0.35, b"a".to_vec()); // 100 ms after attach
        assert!(inbox.take_due(5.099).is_empty());
        assert_eq!(inbox.take_due(5.1), b"a");
        assert!((inbox.sim_ceiling(1.25) - 6.0).abs() < 1e-12);
    }

    /// The unpaced scale compresses a host's wall gaps into sim time by a
    /// fixed factor, so a sub-real-time sim can honour the schedule: a 2 s
    /// port-settle sleep at scale 1/20 becomes 100 ms of sim, and the ceiling
    /// grows at the same compressed rate.
    #[test]
    fn scaled_inbox_compresses_host_gaps_by_a_fixed_factor() {
        let mut inbox = PacedInbox::new(0.05);
        inbox.engage(1.0, 10.0);
        inbox.push(1.0, 12.0, b"s".to_vec()); // 2 s of host wall gap
        assert!(inbox.take_due(1.0999).is_empty());
        assert_eq!(inbox.take_due(1.1), b"s", "2 s * 0.05 = 100 ms of sim");
        assert!((inbox.sim_ceiling(14.0) - 1.2).abs() < 1e-12);
    }

    /// Re-anchoring at a firmware emission measures the host's next gap from
    /// that emission, so a starved sim's slow production of the emission does
    /// not stretch the measured gap; and re-anchoring never rewinds already
    /// stamped targets.
    #[test]
    fn re_anchor_measures_reaction_gaps_from_the_emission() {
        let mut inbox = PacedInbox::new(0.05);
        inbox.engage(0.0, 0.0);
        // The sim took 8 wall seconds to reach sim 0.4 (badly starved), where
        // the firmware emitted its response; the host replies 1 s later.
        inbox.re_anchor(0.4, 8.0);
        inbox.push(0.4, 9.0, b"r".to_vec());
        assert!(inbox.take_due(0.4499).is_empty());
        assert_eq!(
            inbox.take_due(0.45),
            b"r",
            "the reply lands 1 s * 0.05 = 50 ms of sim after the emission, \
             independent of the 8 wall seconds the sim needed to get there"
        );
    }

    /// A session nobody attached to must SAY so. Silence here is the failure
    /// mode that makes a user debug their firmware instead of their command line.
    #[test]
    fn zero_attach_session_is_reported_loudly() {
        let s = SerialSessionSummary {
            endpoint: "/dev/ttys009".into(),
            attach_count: 0,
            ..Default::default()
        };
        let lines = summary_lines(&s);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("no host tool ever attached")),
            "lines: {lines:?}"
        );
    }

    /// Dropped output and undelivered host bytes are separate failures and each
    /// gets its own warning; neither may hide behind the byte-count line.
    #[test]
    fn dropped_and_undelivered_bytes_each_warn() {
        let s = SerialSessionSummary {
            endpoint: "/dev/ttys009".into(),
            attach_count: 1,
            dropped_to_peer: 12,
            rx_overflow: vec![("U1".into(), 7)],
            ..Default::default()
        };
        let lines = summary_lines(&s);
        assert!(
            lines.iter().any(|l| l.contains("DROPPED")),
            "lines: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("never reached U1's firmware")),
            "lines: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("no host tool ever attached")),
            "a session with a peer must not claim nobody attached: {lines:?}"
        );
    }
}
