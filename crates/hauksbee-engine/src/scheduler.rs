//! The co-simulation scheduler.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/scheduler.md.
//!
//! Generalizes the Tarski-Emulator lockstep pattern. Each call to
//! [`Scheduler::step`] advances wall-clock `dt` in fixed sub-chunks (default
//! 100 µs). Per chunk:
//!
//! 1. **MCU**: run each emulated core for the chunk's cycles. GPIO output edges
//!    land in a shared queue via `on_pin_change`; UART output bytes are
//!    captured; the latest ADC voltages (from the *previous* chunk's solve) are
//!    injected continuously before the run.
//! 2. **Drivers**: apply captured GPIO edges to their Thevenin
//!    [`crate::drivers::PinDriver`]s,
//!    so the analog circuit sees the new pin states this chunk.
//! 3. **Analog**: solve a transient over the chunk and read the final node
//!    voltages for every net.
//! 4. **Sample back**: feed solved net voltages into MCU ADC channels (for the
//!    next chunk) and into the digital components' inputs.
//! 5. **Digital**: each behavioral IC processes its events and updates its
//!    output drivers (seen by the analog solve next chunk).
//!
//! Chunking is fixed and the solver is seeded deterministically, so a run is
//! reproducible given the same firmware and board.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
#[cfg(feature = "avr")]
use hauksbee_mcu::AvrMcu;
use hauksbee_mcu::{Mcu, PinId};
use hauksbee_solve::{Layout, SolverOptions, Transient};

use crate::behavioral::BehavioralDevice;
use crate::binder::{apin_gpio_of_role, gpio_of_role, BoundBoard, McuBinding};
use crate::digital::{DigitalComponent, PinEdge};
use crate::peripherals::{
    I2cBus, PeripheralSet, RegisterMapSensor, SpiBus, SpiFramingMode, TickCtx, TimelineEvent,
};
use crate::power_supply::{PowerSupply, SupplyLeg};
use crate::stress::{FaultEvent, StressMonitor};

/// Default co-sim chunk size (seconds).
pub const DEFAULT_CHUNK_S: f64 = 100e-6;

/// Resistance at or above which a resistor leg is too weak to count as
/// evidence that a net is really driven, for the plain digital-input sync. An
/// open pushbutton contact ([`crate::peripherals::controls`]' `CONTACT_ROFF`)
/// and a tri-stated [`PinDriver`] leg are both 1 GΩ, their ~0 V "drive" is a
/// numerical artifact, not a level a real input pin would see (on hardware the
/// pin's internal pull-up, unmodeled in the analog circuit, would win). A real
/// pull resistor (10 k–1 M) sits far below this. Checked against the LIVE ohms
/// each chunk, so a pressed button (contact swapped to ~100 Ω) becomes
/// evidence the moment it closes.
const WEAK_DIGITAL_DRIVE_OHMS: f64 = 1e8;

/// A strict headless run (`--strict`) or hauksbee-ci aborts (exit 3,
/// invalid-for-analysis) once the analog solve fails this many chunks in a row.
/// Three, not one: a single failed chunk is often a stiff step the warm-started
/// next chunk recovers from, so aborting on the first would be trigger-happy on a
/// board that self-heals within a chunk or two. Three back-to-back failures is a
/// solve that is genuinely stuck rather than a blip, and a run that reaches it is
/// reporting fiction (held stale voltages), so it must refuse rather than fake
/// (05 §3b, master doctrine §5).
pub const STRICT_CONSECUTIVE_FAILED_ABORT: u32 = 3;

/// Live chip-select framing hook installed by [`Scheduler::attach_spi_bus`] when
/// the binder resolved a slave's CS net to an MCU pin (05 §2.1). When `pin`
/// toggles, the `on_pin_change` closure frames `bus` from the REAL chip-select
/// edge, synchronously and in cycle order with the byte stream: on a push backend
/// (simavr) the SPI byte IRQ and the CS GPIO IRQ both fire inside `avr_run`, so
/// asserting/deasserting here interleaves the transaction boundaries exactly where
/// the firmware put them (mid-chunk included), instead of guessing at the chunk
/// boundary. SPI CS is active-low by convention: a falling edge (level=false)
/// asserts (begin transaction), a rising edge (level=true) deasserts (end).
struct CsFrame {
    pin: (char, u8),
    active_low: bool,
    bus: Arc<Mutex<SpiBus>>,
}

/// Captured state shared between an MCU's C callbacks and the scheduler.
#[derive(Default)]
struct McuShared {
    /// Pin edges since last drain: (port, bit) -> latest level. Still used by
    /// ordinary GPIO drivers (which only care about the final level a chunk
    /// settles to) and diagnostics / frame state.
    pin_edges: HashMap<(char, u8), bool>,
    /// Ordered, cycle-stamped log of EVERY pin transition since the last drain,
    /// in the order the firmware produced them. Unlike `pin_edges` (latest-level
    /// map, which collapses a sub-µs `shiftOut` SCLK pulse train to its final
    /// level), this preserves each edge AND its MCU cycle so the bit-banged
    /// digital layer replays at edge granularity in cycle order (FIX 1, 05 §1.1).
    pin_edge_log: Vec<PinEdge>,
    /// UART bytes the firmware emitted since last drain.
    uart_out: Vec<u8>,
    /// Live CS-framing hooks for SPI slaves whose CS net resolved to a pin on
    /// this MCU (05 §2.1). Consulted by the `on_pin_change` closure so a CS edge
    /// frames its bus in true cycle order with the byte transfers. Empty on the
    /// heuristic path (no resolved CS pin), so this is zero-overhead there.
    cs_frames: Vec<CsFrame>,
}

/// One live MCU core plus its binding and shared capture state.
struct LiveMcu {
    core: Box<dyn Mcu + Send>,
    binding: McuBinding,
    shared: Arc<Mutex<McuShared>>,
    /// Last known GPIO output levels, for diagnostics / frame state.
    last_levels: HashMap<(char, u8), bool>,
    /// The configured-output pin set reported by the core at the END of the
    /// previous chunk (`pins_configured_output`). Tracked so a pin the
    /// firmware switches from output back to input (DDR output→input, e.g.
    /// an open-drain bus hand-off) gets its Thevenin driver DISABLED again:
    /// without the release, a handed-off net stays clamped at its stale
    /// driven level; the latched-bus failure. Backends that cannot report
    /// direction always return an empty set, so this stays empty there and
    /// the release is a no-op (edge-enabled drivers are never torn down).
    configured_outputs: std::collections::HashSet<(char, u8)>,
    /// Logic-high output voltage for this MCU's GPIO drivers (rail-dependent:
    /// 5 V for classic AVR, 3.3 V for STM32-class parts).
    logic_high_v: f64,
    /// MCU *input* pins owned by a synchronous input responder (a 165 chain's
    /// MISO, a bit-banged SPI MISO, a soft-I2C SDA). These pins get their
    /// level at edge granularity from the responder INSIDE the MCU run loop,
    /// so the per-chunk plain digital-input sync must never also drive them:
    /// a chunk-boundary level would stomp the mid-transaction bit the
    /// responder just presented.
    responder_input_pins: std::collections::HashSet<(char, u8)>,
    /// Last logic level pushed into the core for each plain digital input pin;
    /// the hysteresis memory of the per-chunk node-voltage → digital-pin
    /// sync (no entry = never pushed; the core still holds its power-on
    /// level). Also the change filter: `set_digital_in` is only called when
    /// the decided level differs, so poll backends (Renode/QEMU, one socket
    /// round-trip per call) pay per *transition*, not per chunk.
    digital_in_levels: HashMap<(char, u8), bool>,
}

/// The scheduler driving one bound board.
pub struct Scheduler {
    pub circuit: Circuit,
    /// The bound circuit as it stood at construction, BEFORE any destructive
    /// fault mutated it (a blown resistor set to 1e12 Ω, a diode replaced by an
    /// open/short). Kept so `reset_run_state` can restore a pristine circuit for
    /// a replay: without it, a destructive run would leave the damage in place
    /// and the next run would either mis-solve the broken topology or (with the
    /// monitor tracks cleared) silently fail to re-raise the fault.
    original_circuit: Circuit,
    pub net_nodes: HashMap<String, NodeId>,
    pub digital: Vec<DigitalComponent>,
    /// MCU-bit-banged 74HC595 chains, clocked from the ordered pin-edge log at
    /// edge granularity (FIX 1). One controller PER physical chain (independent
    /// chains are not merged). The chips these drive are listed in `chain_chips`
    /// and are skipped by the once-per-chunk `digital` tick so they are not
    /// driven twice.
    chains: Vec<crate::digital::Hc595Chain>,
    /// Index into `mcus` of the MCU that clocks each chain (parallel to
    /// `chains`), so a chain only consumes its own MCU's edge log.
    chain_mcu: Vec<usize>,
    /// Indices into `digital` of every chip driven by `chains` (the edge path).
    chain_chips: std::collections::HashSet<usize>,
    mcus: Vec<LiveMcu>,
    /// Latest solved node voltages, indexed by `NodeId.0`.
    pub node_volts: Vec<f64>,
    /// Latest solved branch currents, indexed by branch unknown (after nodes).
    /// `branch_x[branch_index]`; map a device to its branch with [`Layout`].
    branch_x: Vec<f64>,
    /// Previous chunk's final unknown vector, used to warm-start the next chunk's
    /// DC operating point (skips cold-start homotopy on a stiff nonlinear board).
    /// Cleared on a solver failure or a structural relayout (size mismatch is
    /// also rejected safely by the solver).
    last_dc_seed: Option<Vec<f64>>,
    /// Frozen MNA unknown layout for the current circuit (branch lookup).
    layout: Layout,
    /// Configurable power supplies, updated between chunks (Feature 1).
    pub supplies: Vec<SupplyLeg>,
    /// Behavioural devices (power ICs), updated between chunks the same way the
    /// supplies are.
    pub behavioral: Vec<BehavioralDevice>,
    /// Fault / stress monitor, evaluated after each chunk (Feature 2).
    pub stress: StressMonitor,
    /// Faults raised since the last frame drain.
    faults_pending: Vec<FaultEvent>,
    pub chunk_s: f64,
    pub opts: SolverOptions,
    pub sim_time: f64,
    /// Sub-microsecond remainder carried between chunks. `run_micros` takes an
    /// integer microsecond count, so a chunk whose duration is not a whole
    /// number of microseconds (e.g. a 1.5 µs chunk, or any chunk under 0.5 µs
    /// that would otherwise round to zero and be clamped up to 1) would drift
    /// the firmware clock away from sim time. The truncated fraction is banked
    /// here and folded into the next chunk so the integer microseconds handed to
    /// the MCU sum to the true elapsed time.
    micros_carry: f64,
    /// Per-net toggle counters and min/max, for headless stats.
    pub stats: HashMap<String, NetStat>,
    /// Net-attached / output peripherals (controls, VCD sinks), ticked each
    /// chunk around the analog solve.
    pub peripherals: PeripheralSet,
    /// I2C bus slaves, shared with each MCU's `on_i2c` callback.
    i2c_buses: Vec<Arc<Mutex<I2cBus>>>,
    /// SPI bus slaves, shared with each MCU's `on_spi` callback.
    spi_buses: Vec<Arc<Mutex<SpiBus>>>,
    /// Per-controller SPI bus map (controller name -> bus). Populated by
    /// [`attach_spi_bus_on`]; not populated by the legacy [`attach_spi_bus`]
    /// path. Used for look-up by controller name after the run.
    spi_controller_map: HashMap<String, Arc<Mutex<SpiBus>>>,
    /// MCU chip-substitution events detected at build time (Track B): the board
    /// asked for a more specific part than the emulator core models. Surfaced as
    /// a co-sim warning + JSON note; never gates an exit code on its own.
    substitutions: Vec<McuSubstitution>,
    /// Bus peripherals (I2C/SPI slave models) attached on a board whose live
    /// MCU backends model NO matching bus controller; the firmware's bus
    /// traffic can never reach them, so they sit at their power-on defaults for
    /// the whole run. Recorded at attach time (mirroring `substitutions`) and
    /// surfaced as a co-sim coverage warning on every report surface; a CI
    /// `peripheral` assertion against one of these FAILS rather than passing on
    /// the slave's untouched default state (U3 finding 2).
    unexercised_buses: Vec<UnexercisedBus>,
    /// MCU-bit-banged 74HC165 read chains, resolved at GPIO-output edge
    /// granularity inside the owning MCU's run loop via its synchronous input
    /// responder (the read-direction analogue of `chains`). Wrapped in
    /// Arc<Mutex<>> because the responder closure owns a clone. Each shares the
    /// `input_volts` snapshot the scheduler refreshes from the last solve so the
    /// 165 captures the latest spike-latch states on its PL load.
    hc165_chains: Vec<Arc<Mutex<crate::digital::Hc165Chain>>>,
    /// Per-MCU synchronous input-responder registries (05 §1.5): the
    /// multiplexer that shares each MCU's single `on_input_responder` slot
    /// across every registered bit-banged input protocol (165 chains,
    /// bit-banged SPI MISO, soft-I2C). `None` until the first responder is
    /// registered for that MCU; the dispatch closure is installed lazily so a
    /// board with no responders keeps the backend hook empty (zero per-edge
    /// cost).
    responder_registries: Vec<Option<Arc<Mutex<crate::responders::ResponderRegistry>>>>,
    /// Latest solved node voltages, shared with the 165 read chains so their
    /// PL-load sampling (which fires inside the MCU run, before this chunk's
    /// solve) sees the previous chunk's settled latch voltages.
    input_volts: Arc<Mutex<Vec<f64>>>,
    /// Forced node-voltage overrides applied to `node_volts` AFTER each chunk's
    /// analog solve. Used by the firmware-driven Tarski inference to drive the 10
    /// output SPIKE nets from the EXACT feedforward decomposition (the monolith
    /// does not converge); the genuine per-column spikes the decomposition
    /// produces are presented on the SPIKE nets so the on-board 74HC02 NOR
    /// latches capture them and the firmware's 165 readback reflects them. Empty
    /// on every other board (no override). Keyed by `NodeId.0`. The value is
    /// `(high_volts, low_volts, t_start, t_end)`: the node is held at
    /// `high_volts` while `t_start <= sim_time < t_end`, else `low_volts`. A
    /// time-unbounded override uses `t_start=-inf, t_end=+inf` (always high). The
    /// time window lets the firmware-driven inference present each output column's
    /// SPIKE net HIGH for a sim-time fraction proportional to its decomposed spike
    /// count, so the firmware's per-sample RESET_SR-gated latch reads accumulate a
    /// count that tracks the decomposed RATE (not just a binary "spiked at all").
    forced_node_volts: HashMap<usize, (f64, f64, f64, f64)>,
    /// Per-run count of chunks whose analog transient solve failed to converge.
    /// A failed chunk holds/recovers stale voltages (see `solve_chunk`'s `Err`
    /// arm), so its operating point is not a real solve: it is excluded from the
    /// running net stats and the stress monitor and surfaced as
    /// `analog_valid: false` in coverage and the co-sim JSON, rather than being
    /// silently held and reported as a quiet run (05 §3b, refuse rather than fake).
    failed_chunks: u64,
    /// Sim-time windows `[start_s, end_s)` of the failed chunks, merged where
    /// consecutive so a diverged stretch reads as its true extent. Surfaced in the
    /// co-sim JSON so a consumer knows exactly which span cannot be trusted.
    failed_windows: Vec<(f64, f64)>,
    /// Current run of back-to-back failed chunks (reset to 0 by any converged
    /// chunk). Feeds the strict/CI abort threshold.
    consecutive_failed_chunks: u32,
    /// Worst back-to-back failed-chunk run seen this run. Retained even after a
    /// later chunk converges, so a strict post-run check still sees a streak that
    /// crossed the abort threshold mid-run.
    max_consecutive_failed_chunks: u32,
    /// Standalone GPIO-edge-driven digital components (indices into `digital`)
    /// advanced through the generalized micro-tick replay (05 §1.2), NOT through
    /// a 595 chain or the 165 responder. Empty on the current corpus (every GPIO
    /// 595 is a chain, every 165 a responder), so the generalized path is a no-op
    /// there and nothing regresses; a board with a standalone GPIO-clocked shift
    /// register populates it. Skipped by the once-per-chunk digital tick, exactly
    /// as `chain_chips` are, so they are not double-driven.
    replay_chips: Vec<usize>,
    /// Per-MCU map of GPIO `(port,bit)` -> the net node it drives, used by the
    /// generalized replay to overlay driven-pin levels while micro-ticking the
    /// `replay_chips`. Parallel to `mcus`.
    replay_pin_nets: Vec<HashMap<(char, u8), NodeId>>,
    /// Cycle-stamped GPIO edges each MCU produced in the LAST chunk (one entry
    /// per MCU that ran), for the analog PWL side. Rebuilt every chunk.
    last_chunk_edges: Vec<ChunkPinEdges>,
    /// Distinct cycle-groups replayed through the generalized digital path in the
    /// last chunk (a micro-tick each). A diagnostic that a `shiftOut` burst
    /// produced N ordered micro-ticks, not one collapsed level.
    last_replay_microticks: usize,
    /// Net node -> indices into `circuit.devices` of every device touching the
    /// node EXCEPT the MCU pin drivers' own Thevenin legs. This is the "is
    /// this net actually driven by the circuit?" evidence the per-chunk plain
    /// digital-input sync consults: a net whose only attachments are MCU pin
    /// legs is electrically floating (its ~0 V solve comes from the pins' own
    /// 1 GΩ tri-state legs, not a real driver), and pushing that fictional
    /// LOW into the core would defeat a firmware-enabled internal pull-up
    /// (which the analog circuit does not model). Rebuilt on every
    /// [`Scheduler::relayout`], so devices stamped later (peripheral controls,
    /// buttons) are included.
    digital_in_evidence: HashMap<u32, Vec<u32>>,
    /// Per-frame (one `step`) accumulators that capture INTRA-frame extremes a
    /// consumer reading only the frame's final chunk would miss. `step` runs
    /// many sub-chunks (default 10 per 1 ms frame) each overwriting `node_volts`;
    /// a current surge or voltage excursion that peaks mid-frame and subsides by
    /// the last chunk is invisible in `node_voltages()`. These are reset at the
    /// start of every `step` and folded per chunk, so the runner can read the
    /// true per-frame peak/extreme rather than the last-chunk snapshot.
    ///
    /// `frame_peak_current`: reference designator -> peak |current| (A) over the
    /// frame's chunks (resistors + diodes, matching the stress monitor's device
    /// current). `frame_v_extremes`: net name -> (min_v, max_v) over the frame.
    frame_peak_current: HashMap<String, f64>,
    frame_v_extremes: HashMap<String, (f64, f64)>,
    /// Net node -> references of TICK-evaluated sequential parts whose
    /// sequential inputs (register clocks / resets / loads / serial data, per
    /// the spec's own [`crate::logic::LogicComponent::sequential_pins`]) the
    /// net feeds. "Tick-evaluated" excludes every edge-exact path: 595 chain
    /// chips (`chain_chips`), generalized replay chips (`replay_chips`), and
    /// 165 read-chain chips (responder-owned). Built once at construction;
    /// consulted by [`Scheduler::detect_short_pulses`] (friction 1.16).
    tick_sequential_nets: HashMap<u32, Vec<String>>,
    /// Sub-chunk GPIO pulse warnings raised this run (friction 1.16), one per
    /// offending net. See [`ShortPulse`].
    short_pulses: Vec<ShortPulse>,
    /// Nets already warned about by `detect_short_pulses`, so a pulse train
    /// warns once per net per run, not once per chunk.
    short_pulse_nets: std::collections::HashSet<u32>,
    /// Runtime driver-contention findings raised this run (the model-vs-MCU
    /// half of the field failure the static lint documents as out of reach in
    /// `checks/contention.rs`), one per offending net. See [`DriverContention`].
    contentions: Vec<DriverContention>,
    /// Nets already reported by `detect_driver_contention` (once per net per
    /// run).
    contention_nets: std::collections::HashSet<u32>,
}

/// A chip-substitution event: the board asked for `requested_part` but the
/// available emulator core models a less-specific platform (`modelled_core`).
/// Recorded at scheduler-build time so every surface (CLI text, JSON, TUI) can
/// warn that co-sim results stand in for the requested silicon (Track B).
#[derive(Debug, Clone)]
pub struct McuSubstitution {
    /// The MCU reference designator (e.g. `"U1"`).
    pub reference: String,
    /// The backend string actually instantiated (e.g. `"renode:stm32f4"`).
    pub backend: String,
    /// The exact part the board asked for (e.g. `"STM32F411RET6"`).
    pub requested_part: String,
    /// Human label of the core that was actually modelled (e.g. `"STM32F407"`).
    pub modelled_core: String,
}

/// A bus peripheral bound on a platform that models no matching bus controller
/// (U3 finding 2). The device is on the board, but the emulated MCU has no
/// controller the bridge could attach to, so the firmware never talks to it,
/// a silent no-op unless surfaced.
#[derive(Debug, Clone)]
pub struct UnexercisedBus {
    /// The peripheral/bus id (the same id `peripheral` assertions target).
    pub id: String,
    /// `"I2C"` or `"SPI"`.
    pub bus: &'static str,
    /// The named SPI controller it was bound to, when the spec named one.
    pub controller: Option<String>,
}

impl UnexercisedBus {
    /// The one-line warning every surface (text, --plain, --json note, CI
    /// report) emits for this device, so they all name the same facts.
    pub fn message(&self) -> String {
        let on = match &self.controller {
            Some(c) => format!(" (bound to controller '{c}')"),
            None => String::new(),
        };
        format!(
            "co-sim: {} device '{}'{on} is on the board but this MCU platform \
             models no {} controller; the firmware's bus traffic can never \
             reach it, so it was NEVER exercised and its behaviour is \
             unverified (its state is the power-on default). Add [soc.{}] \
             controllers to the SoC descriptor to enable it (docs/cosim/MCU.md).",
            self.bus,
            self.id,
            self.bus,
            self.bus.to_ascii_lowercase(),
        )
    }
}

/// One ADC channel whose per-chunk injections the MCU backend DROPPED because
/// the platform has no injection map (U3 finding 1). The analog solve drove
/// the net; the firmware never received a single sample.
#[derive(Debug, Clone)]
pub struct AdcDrop {
    /// The MCU reference designator (e.g. `"U1"`).
    pub mcu_ref: String,
    /// The engine ADC channel index.
    pub channel: u8,
    /// The board net wired to the channel.
    pub net: String,
    /// Up to a few reference designators of the parts attached to the net
    /// (the analog source the firmware was supposed to read). Best-effort.
    pub parts: Vec<String>,
}

impl AdcDrop {
    /// The one-line warning every surface emits for this channel; same
    /// shared-wording discipline as [`UnexercisedBus::message`].
    pub fn message(&self) -> String {
        let parts = if self.parts.is_empty() {
            String::new()
        } else {
            format!(", parts {}", self.parts.join("/"))
        };
        format!(
            "co-sim: ADC channel {} on {} (net '{}'{parts}) was driven by the \
             analog solve but this platform has no ADC injection map; the \
             firmware NEVER received it, so analog readings on that pin are \
             meaningless. Add an [[soc.adc]] injection recipe to the SoC \
             descriptor to enable it (docs/cosim/MCU.md).",
            self.channel, self.mcu_ref, self.net,
        )
    }
}

/// A firmware GPIO pulse that rose AND fell inside a single solver chunk, on a
/// net that clocks a TICK-evaluated sequential part (cold-drive friction 1.16,
/// defect report 7). Chain-responder parts (74HC595/165 chains, bit-banged
/// SPI/I2C) resolve such edges synchronously inside the firmware's instruction
/// stream, but an ordinary sequential part (a 74HC74 latch, a ripple counter)
/// is evaluated once per chunk against the PREVIOUS solve, so a pulse contained
/// in one chunk is never observed: the part's state trails or misses events
/// while the rest of the board looks edge-exact. That asymmetry produces
/// plausible WRONG answers, not errors, so it must be surfaced loudly.
///
/// Detected from the cycle-stamped pin-edge log ([`ChunkPinEdges`]): two
/// consecutive opposite-level transitions of one pin inside one chunk are a
/// completed pulse, and its width is the cycle gap normalised over the chunk's
/// cycle span. Raised once per offending net per run.
#[derive(Debug, Clone)]
pub struct ShortPulse {
    /// The net carrying the pulse.
    pub net: String,
    /// The MCU whose pin drove it (reference designator).
    pub mcu_ref: String,
    /// Port letter of the driving pin.
    pub port: char,
    /// Bit index of the driving pin.
    pub bit: u8,
    /// Narrowest completed pulse observed on the net (seconds). On a poll
    /// backend (`cycle_exact == false`) this is coarse but the containment
    /// (both edges inside one chunk) still holds exactly.
    pub pulse_s: f64,
    /// The solver chunk the pulse fell inside (seconds).
    pub chunk_s: f64,
    /// References of the tick-evaluated sequential parts clocked by the net.
    pub parts: Vec<String>,
}

/// Compact human time: "2.0 us", "150 ns", "1.5 ms".
fn fmt_seconds(s: f64) -> String {
    if s >= 1e-3 {
        format!("{:.1} ms", s * 1e3)
    } else if s >= 1e-6 {
        format!("{:.1} us", s * 1e6)
    } else {
        format!("{:.0} ns", s * 1e9)
    }
}

impl ShortPulse {
    /// The one-line warning every surface (text, --plain, --json note, web)
    /// emits for this net, so they all name the same facts; same
    /// shared-wording discipline as [`UnexercisedBus::message`].
    pub fn message(&self) -> String {
        let suggest_us = (self.pulse_s * 1e6 / 2.0).max(0.1);
        format!(
            "co-sim: net '{}' carries a {} pulse from {} pin P{}{} that is shorter than \
             the {} solver chunk. Sequential part(s) {} clock from this net but are \
             evaluated once per chunk against the previous solve, so a pulse that rises \
             and falls inside one chunk is NEVER observed: their state lags or misses \
             events entirely, while chain-responder parts (74HC595/165 chains) on the \
             same board see every edge exactly. Results stay plausible-looking but \
             wrong. Rerun with --chunk-us {:.1} (a chunk no wider than half the pulse) \
             to make it visible, or widen the pulse in firmware. Edge-scheduling \
             sequential parts is the real fix and is recorded as a follow-up \
             (cold-drive friction 1.16).",
            self.net,
            fmt_seconds(self.pulse_s),
            self.mcu_ref,
            self.port,
            self.bit,
            fmt_seconds(self.chunk_s),
            self.parts.join("/"),
            suggest_us,
        )
    }
}

/// Runtime driver contention: the firmware configured an MCU pin as a push-pull
/// OUTPUT on a net where an ENABLED modelled push-pull output was already
/// driving. This is the model-vs-MCU half of the field failure whose
/// model-vs-model half the static lint catches
/// ([`crate::checks::contention`]): at lint time every MCU GPIO driver is
/// stamped high-impedance and only firmware sets direction, so "modelled output
/// shares a net with an MCU pad" is the most common HEALTHY topology there is,
/// and the static check documents this case as out of reach. The scheduler
/// learns real pin directions at runtime (pin-change edges and
/// `pins_configured_output` DDR sync), so it is the one place the fight is
/// observable.
///
/// What counts as a modelled push-pull output is shared with the static check
/// by construction, not by parallel reimplementation: the binder stamps a
/// [`crate::drivers::PinDriver`] on every connected output role from
/// [`crate::digital::output_roles`] (the same single source the static check's
/// `scan()` consults), and the spec's `[models.logic.tristate]` groups drive
/// the driver's live `enabled` flag (the same groups the static check expands
/// to exclude tri-stateable roles). A tri-stated (released) model output or a
/// tri-stated MCU pin therefore never fires here, matching the static
/// exclusions at runtime granularity. Raised once per net per run.
#[derive(Debug, Clone)]
pub struct DriverContention {
    /// The contended net.
    pub net: String,
    /// The MCU whose pin joined the fight (reference designator).
    pub mcu_ref: String,
    /// Port letter of the firmware-driven pin.
    pub port: char,
    /// Bit index of the firmware-driven pin.
    pub bit: u8,
    /// `"REF.role"` of every enabled modelled push-pull output on the net,
    /// sorted for determinism.
    pub parts: Vec<String>,
    /// Sim time (s) at which both sides were first seen driving together.
    pub t_s: f64,
}

impl DriverContention {
    /// The one-line finding every surface emits for this net; same
    /// shared-wording discipline as [`UnexercisedBus::message`].
    pub fn message(&self) -> String {
        format!(
            "co-sim: driver contention on net '{}' from t={:.6}s: firmware configured \
             {} pin P{}{} as a push-pull OUTPUT while modelled push-pull output(s) {} \
             were already driving the same net. Two push-pull drivers fighting one net \
             means both parts pass current well beyond their output ratings on real \
             hardware, and the simulation solves the fight to a voltage that looks like \
             data, so every waveform touching this net is untrustworthy from that time \
             on. Check the model pin mapping with `hauksbee models resolve` (a part \
             bound to the wrong pinout caused the field failure this check exists for) \
             and the firmware's pin-direction writes. The static output-contention lint \
             cannot see firmware pin directions, so this runtime monitor is the only \
             check that can catch the model-vs-MCU case.",
            self.net,
            self.t_s,
            self.mcu_ref,
            self.port,
            self.bit,
            self.parts.join(", "),
        )
    }
}

impl McuSubstitution {
    /// A one-line warning sentence suitable for stderr or a JSON note. Always
    /// ends with the actionable "here is how to model the real part exactly",
    /// adding a chip is a two-file, no-recompile recipe, so a substitution should
    /// point the user straight at it rather than leave them stuck on a substitute.
    pub fn message(&self) -> String {
        format!(
            "co-sim: {} requested {} but it is modelled as an {} core; \
             firmware behaviour is emulated on the substitute and may differ on \
             the real part (e.g. peripheral set, flash/RAM size, clock tree). \
             To model {} exactly, add a SoC descriptor + a [[models]] routing entry \
             (two TOML files, no recompile); see docs/extending/add-an-mcu-variant.md.",
            self.reference, self.requested_part, self.modelled_core, self.requested_part
        )
    }
}

/// The cycle-stamped GPIO edges one MCU produced during the most recent chunk,
/// exposed for the analog PWL side (05 §1.1/§1.3).
///
/// A `(port,bit)` maps to its ordered `(cycle, level)` series; `cycle_span` is
/// the chunk's `[start, end)` cycle counter so a consumer normalises an edge
/// cycle to a fraction of the chunk (`(cycle - start) / (end - start)`) and then
/// to seconds via `chunk_s`, driving a `SourceKind::Pwl` waveform on the net the
/// pin feeds. `cycle_exact` is false on poll backends (Renode/QEMU): the series
/// still orders correctly but sub-slice edge times are coarse, so the PWL side
/// must not claim cycle-exact corner times there.
#[derive(Debug, Clone)]
pub struct ChunkPinEdges {
    /// The MCU whose edges these are (its reference designator).
    pub mcu_reference: String,
    /// Per-pin ordered `(cycle, level)` transitions within the chunk.
    pub edges: HashMap<(char, u8), Vec<(u64, bool)>>,
    /// The chunk's `[start_cycle, end_cycle)` span, for time normalization.
    pub cycle_span: (u64, u64),
    /// Wall-clock duration of the chunk in seconds (maps normalized time to real time).
    pub chunk_s: f64,
    /// Whether the cycle stamps are cycle-exact (push backend) or coarse (poll).
    pub cycle_exact: bool,
}

/// Running statistics for one net across a run.
#[derive(Debug, Clone)]
pub struct NetStat {
    pub min_v: f64,
    pub max_v: f64,
    pub toggles: u64,
    last_logic: Option<bool>,
}

impl Default for NetStat {
    fn default() -> Self {
        NetStat {
            min_v: f64::INFINITY,
            max_v: f64::NEG_INFINITY,
            toggles: 0,
            last_logic: None,
        }
    }
}

impl NetStat {
    /// Test constructor: a net stat carrying a given toggle count (min/max at
    /// their empty sentinels). `last_logic` is private to this module, so a
    /// cross-module test (e.g. the web-report activity ranking) cannot build a
    /// `NetStat` literal directly; this exposes just enough for those tests.
    #[cfg(test)]
    pub(crate) fn with_toggles(toggles: u64) -> Self {
        NetStat {
            toggles,
            ..Default::default()
        }
    }

    /// Test constructor with an explicit voltage range, for the activity-ranking
    /// tie-break (equal toggles, differing swing) the web/CLI/JSON tables share.
    #[cfg(test)]
    pub(crate) fn with_toggles_and_range(toggles: u64, min_v: f64, max_v: f64) -> Self {
        NetStat {
            toggles,
            min_v,
            max_v,
            ..Default::default()
        }
    }
}

impl Scheduler {
    /// Build a scheduler from a bound board, instantiating MCU cores and
    /// loading firmware (one hex for all, or none).
    pub fn new(
        bound: BoundBoard,
        firmware: Option<&std::path::Path>,
        opts: SolverOptions,
    ) -> anyhow::Result<Self> {
        let BoundBoard {
            circuit,
            net_nodes,
            digital,
            mcus,
            supplies,
            behavioral,
            device_meta,
            dacs,
            ..
        } = bound;

        // Supply-rail absolute-maximum watches, built BEFORE the bindings are
        // consumed into live cores. An MCU/logic package has no whole-device
        // stress meta (its per-pin currents are covered by pin-driver metas),
        // so without these a rail driven far past the chip's abs-max Vcc
        // raised no fault at all while the model DB carried the rating. One
        // watch per distinct supply node per part; direct-supply roles only
        // (vin feeds a module's onboard regulator, its ceiling is different).
        let mut supply_watches = Vec::new();
        for binding in &mcus {
            if let Some(max_v) = binding.max_supply_v {
                let mut seen_nodes = std::collections::HashSet::new();
                for (role, &node) in &binding.role_nets {
                    let direct_supply =
                        matches!(role.as_str(), "vcc" | "avcc" | "vdd" | "vdda" | "5v");
                    if direct_supply && !node.is_ground() && seen_nodes.insert(node) {
                        supply_watches.push(crate::stress::SupplyWatch {
                            reference: binding.reference.clone(),
                            node,
                            max_v,
                        });
                    }
                }
            }
        }

        let mut live = Vec::new();
        let mut substitutions = Vec::new();
        for binding in mcus {
            // External emulator backends (renode/qemu) boot from a program
            // image; with no firmware given there is nothing to run, so the
            // MCU sits out and the board solves as a passive circuit (its pins
            // stay high-impedance). This keeps firmware-less analyses (lint,
            // DRC, stress, transient scenarios) working on boards whose MCU
            // happens to have an external backend mapping. The in-process AVR
            // core keeps its historical always-instantiated behaviour.
            let external = backend_is_external(&binding.backend);
            if external && firmware.is_none() {
                continue;
            }
            // Detect (and warn about) a chip substitution before the core is
            // consumed: the board asked for a more specific part than the
            // emulator models (e.g. STM32F411 -> the STM32F407 Discovery core).
            if let Some(sub) = detect_substitution(&binding) {
                eprintln!("WARNING: {}", sub.message());
                substitutions.push(sub);
            }
            let core = instantiate_mcu(&binding, firmware)?;
            live.push(core_with_hooks(core, binding));
        }

        // Build the MCU-bit-banged 74HC595 chain controllers (FIX 1). For each
        // live MCU, map the net node each GPIO driver pushes onto back to its
        // (port, bit), then identify the 595 daisy-chain(s) and bind their
        // broadcast control signals (SRCLK / RCLK / SRCLR_n) and head SER to
        // GPIO pins. A chain whose essential control pins are not bound to GPIO
        // is left to the once-per-chunk digital tick, which still models it,
        // just at chunk granularity.
        let (chains, chain_mcu, chain_chips) = build_595_chains(&digital, &live);

        // Standalone GPIO-edge-driven digital components (05 §1.2): shift/latch
        // parts clocked directly by an MCU pin that are NOT part of a 595 chain
        // or a 165 responder. On the current corpus this is empty; it is the
        // generalization hook so a lone GPIO-clocked 595/165 replays at edge
        // granularity through the same path as the chains.
        let (replay_chips, replay_pin_nets) =
            build_generic_replay_chips(&digital, &chain_chips, &live);

        let n_nodes = circuit.node_count();
        let layout = Layout::new(&circuit);
        let n_branch = layout.size.saturating_sub(layout.n_nodes);
        // Snapshot the pristine (post-bind, pre-run) circuit for destructive-mode
        // replay restoration in `reset_run_state`.
        let original_circuit = circuit.clone();
        let mut sched = Scheduler {
            circuit,
            original_circuit,
            net_nodes,
            digital,
            chains,
            chain_mcu,
            chain_chips,
            mcus: live,
            node_volts: vec![0.0; n_nodes],
            branch_x: vec![0.0; n_branch],
            last_dc_seed: None,
            layout,
            supplies,
            behavioral,
            stress: {
                let mut stress = StressMonitor::new(device_meta);
                stress.set_supply_watches(supply_watches);
                stress
            },
            faults_pending: Vec::new(),
            chunk_s: DEFAULT_CHUNK_S,
            opts,
            sim_time: 0.0,
            micros_carry: 0.0,
            stats: HashMap::new(),
            peripherals: PeripheralSet::new(),
            i2c_buses: Vec::new(),
            spi_buses: Vec::new(),
            spi_controller_map: HashMap::new(),
            substitutions,
            unexercised_buses: Vec::new(),
            hc165_chains: Vec::new(),
            responder_registries: Vec::new(),
            input_volts: Arc::new(Mutex::new(vec![0.0; n_nodes])),
            forced_node_volts: HashMap::new(),
            failed_chunks: 0,
            failed_windows: Vec::new(),
            consecutive_failed_chunks: 0,
            max_consecutive_failed_chunks: 0,
            replay_chips,
            replay_pin_nets,
            last_chunk_edges: Vec::new(),
            last_replay_microticks: 0,
            digital_in_evidence: HashMap::new(),
            frame_peak_current: HashMap::new(),
            frame_v_extremes: HashMap::new(),
            tick_sequential_nets: HashMap::new(),
            short_pulses: Vec::new(),
            short_pulse_nets: std::collections::HashSet::new(),
            contentions: Vec::new(),
            contention_nets: std::collections::HashSet::new(),
        };

        // One (initially absent) input-responder registry slot per live MCU;
        // `responder_registry` fills a slot on first registration.
        sched.responder_registries = (0..sched.mcus.len()).map(|_| None).collect();

        // Build the edge-driven 74HC165 read chains and register each with its
        // owning MCU's synchronous input-responder registry, so a firmware
        // readback (bit-banged SCLK + digitalRead(MISO)) resolves at edge
        // granularity.
        sched.build_and_install_165_chains();

        // Wire up the board's MCP4728 quad DACs: build one spec-driven I2C
        // slave per binding at its assigned address, attach them on a shared
        // bus (so firmware TWI writes reach them through `on_i2c`), with each
        // connected VOUT net's PinDriver bound to the matching spec output so
        // the slave drives the analog nets itself at every transaction end
        // (the ctx-bearing on_stop, 05 §3.1).
        if !dacs.is_empty() {
            sched.attach_mcp4728_dacs(dacs);
        }

        // Index the non-pin-driver devices touching each net, for the plain
        // digital-input sync's "is this net really driven?" check.
        sched.rebuild_digital_in_evidence();

        // Index the nets that clock TICK-evaluated sequential parts, for the
        // sub-chunk pulse warning (friction 1.16). Built after the 595 chains,
        // replay chips, and 165 read chains above, because those edge-exact
        // paths are exactly what the index must EXCLUDE.
        sched.rebuild_tick_sequential_nets();

        Ok(sched)
    }

    /// Rebuild [`Scheduler::tick_sequential_nets`]: net node -> references of
    /// sequential digital parts evaluated on the once-per-chunk tick path,
    /// keyed by the nets their spec-declared sequential inputs
    /// ([`DigitalComponent::sequential_pins`]: register clocks, resets, loads,
    /// enables, serial data) are wired to. Edge-exact parts are excluded: 595
    /// chain chips, generalized replay chips, and 165 read-chain chips all see
    /// every edge in cycle order, so a sub-chunk pulse is visible to them and
    /// warning about it would be a false positive (a bit-banged SRCLK train is
    /// the NORMAL way those parts are driven).
    fn rebuild_tick_sequential_nets(&mut self) {
        let mut edge_exact: std::collections::HashSet<usize> =
            self.chain_chips.iter().copied().collect();
        edge_exact.extend(self.replay_chips.iter().copied());
        for c in &self.hc165_chains {
            let c = c.lock().unwrap_or_else(|e| e.into_inner());
            edge_exact.extend(c.order.iter().copied());
        }
        let mut map: HashMap<u32, Vec<String>> = HashMap::new();
        for (i, d) in self.digital.iter().enumerate() {
            if edge_exact.contains(&i) || !d.is_sequential() {
                continue;
            }
            for pin in d.sequential_pins() {
                let Some(&node) = d.roles.get(pin) else {
                    continue;
                };
                let refs = map.entry(node.0).or_default();
                if !refs.iter().any(|r| r == &d.reference) {
                    refs.push(d.reference.clone());
                }
            }
        }
        for refs in map.values_mut() {
            refs.sort();
        }
        self.tick_sequential_nets = map;
    }

    /// Rebuild [`Scheduler::digital_in_evidence`]: net node -> device indices,
    /// excluding every MCU pin driver's own Thevenin legs (the hidden vsource
    /// and the series resistor). Those legs exist on EVERY wired pin, input
    /// pins included, tri-stated at 1 GΩ, so counting them would make every
    /// net look driven. A driven *output* pin of another MCU is still honored:
    /// the per-chunk sync separately treats any net with an ENABLED gpio
    /// driver as driven (see `run_chunk`), so an MCU-to-MCU GPIO link syncs.
    fn rebuild_digital_in_evidence(&mut self) {
        let mut pin_legs: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for m in &self.mcus {
            for drv in m.binding.gpio_drivers.values() {
                pin_legs.insert(drv.vsource.0);
                pin_legs.insert(drv.resistor.0);
            }
        }
        let mut evidence: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, d) in self.circuit.devices.iter().enumerate() {
            if pin_legs.contains(&(i as u32)) {
                continue;
            }
            for n in d.nodes() {
                if n == NodeId::GROUND {
                    continue;
                }
                evidence.entry(n.0).or_default().push(i as u32);
            }
        }
        self.digital_in_evidence = evidence;
    }

    /// Build and attach the MCP4728 DAC slaves discovered by the binder. One
    /// shared [`I2cBus`] holds all of them (addressed 0x60/0x61/0x62); the bus
    /// is registered as every MCU's `on_i2c` handler.
    ///
    /// Each slave is a [`RegisterMapSensor`] instance of the shipped MCP4728
    /// spec (05 §3.2: the DAC is data, not Rust) with the binder-resolved
    /// per-instance address / VREF / gain applied over the spec defaults and
    /// each connected VOUT channel's [`crate::drivers::PinDriver`] bound to
    /// the matching spec output. Net driving happens in the slave's own
    /// `on_stop(ctx)`, delivered by the chunk loop's `flush_stops`, no
    /// scheduler-side polling.
    fn attach_mcp4728_dacs(&mut self, dacs: Vec<crate::binder::DacBinding>) {
        /// The shipped declarative MCP4728 spec. Embedded (rather than loaded
        /// from disk at runtime) so an engine binary is self-contained; the
        /// unit fixtures include the same file, keeping one source of truth.
        const MCP4728_SPEC: &str = include_str!("../../../testdata/sensor-specs/mcp4728.toml");

        let mut bus = I2cBus::new("MCP4728_BUS");
        for d in dacs {
            let mut slave = RegisterMapSensor::from_toml(MCP4728_SPEC)
                .expect("shipped mcp4728.toml spec must validate");
            slave.set_i2c_address(d.address);
            for ch in 0..4 {
                slave.set_channel_state("vref", ch, d.vref);
                slave.set_channel_state("gain", ch, d.gain as f64);
            }
            for (ch, drv) in d.vout_drivers.into_iter().enumerate() {
                if let Some(drv) = drv {
                    slave.attach_output_driver_for_channel(ch, drv);
                }
            }
            bus.add_slave(Box::new(slave));
        }
        let bus = Arc::new(Mutex::new(bus));
        self.attach_i2c_bus(bus.clone());
        // Seed the analog nets with the DACs' power-on VOUT (code 0 -> ~0 V),
        // the way the first flush_stops otherwise would after a transaction.
        let volts = self.node_volts.clone();
        let mut ctx = TickCtx {
            circuit: &mut self.circuit,
            node_volts: &volts,
            t: self.sim_time,
            dt: self.chunk_s,
        };
        bus.lock()
            .unwrap_or_else(|e| e.into_inner())
            .drive_all(&mut ctx);
    }

    /// The synchronous input-responder registry for MCU `mi` (05 §1.5),
    /// creating it and installing its dispatch closure into the MCU's single
    /// `on_input_responder` slot on first use. Every bit-banged input protocol
    /// (165 chains, bit-banged SPI MISO, soft-I2C) registers here; the registry
    /// keys dispatch on the output pins each responder watches, so an edge on
    /// a non-protocol pin costs one map miss. Lazy install keeps the backend
    /// hook empty (its `None` fast path) on boards with no responders. On poll
    /// backends (Renode/QEMU) `on_input_responder` is a documented no-op; the
    /// registry exists but never fires, the deliberate coarse tier of 05 §1.5.
    fn responder_registry(
        &mut self,
        mi: usize,
    ) -> Arc<Mutex<crate::responders::ResponderRegistry>> {
        if let Some(reg) = &self.responder_registries[mi] {
            return reg.clone();
        }
        let reg = Arc::new(Mutex::new(crate::responders::ResponderRegistry::new()));
        let cb = reg.clone();
        self.mcus[mi].core.on_input_responder(Box::new(
            move |pin: PinId, high: bool| -> Vec<(PinId, bool)> {
                cb.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .dispatch((pin.port, pin.bit), high)
                    .into_iter()
                    .map(|((port, bit), level)| (PinId { port, bit }, level))
                    .collect()
            },
        ));
        self.responder_registries[mi] = Some(reg.clone());
        reg
    }

    /// Build the edge-driven 74HC165 read chains (one per physical chain whose
    /// PL / CLK / QH→MISO pins bind to an MCU's GPIO) and register each with
    /// that MCU's input-responder registry. The responder fires on every PL /
    /// SCLK edge during the MCU's run: the chain samples the spike-latch inputs
    /// on a PL load and presents the next QH bit on MISO, returning the (MISO
    /// pin, level) to drive immediately. This closes the readback inside the
    /// firmware's own bit-bang loop.
    fn build_and_install_165_chains(&mut self) {
        use crate::digital::{order_165_chains, Hc165Chain, LogicLevels};
        use crate::responders::Hc165Responder;

        // Per MCU: net-node -> (port,bit). Every wired digital-capable pin gets a
        // (possibly tri-stated) gpio driver, so this map covers both the control
        // outputs (PL/SCLK) and the MISO *input* pin (its driver stays disabled
        // because the firmware never drives it, but the mapping is what we need).
        let gpio_maps: Vec<HashMap<i64, (char, u8)>> = self
            .mcus
            .iter()
            .map(|m| {
                m.binding
                    .gpio_drivers
                    .iter()
                    .map(|(&(port, bit), drv)| (drv.net.0 as i64, (port, bit)))
                    .collect()
            })
            .collect();

        for order in order_165_chains(&self.digital) {
            for (mi, gpio_node) in gpio_maps.iter().enumerate() {
                // PL/CLK come from gpio_node; MISO is also in gpio_node (the
                // input pin's tri-stated driver carries the net mapping).
                let Some(chain) =
                    Hc165Chain::build(&self.digital, order.clone(), gpio_node, gpio_node)
                else {
                    continue;
                };
                let levels: LogicLevels = chain.levels(&self.digital);
                let miso = chain.miso;
                let chain = Arc::new(Mutex::new(chain));
                // Register with the owning MCU's responder registry: dispatch
                // is keyed on the chain's PL/SCLK pins, so every other edge is
                // ignored on a bare pin comparison. The
                // responder reads the shared voltage snapshot for the PL-load
                // sampling of the latch inputs.
                let responder =
                    Hc165Responder::new(chain.clone(), levels, self.input_volts.clone());
                let registry = self.responder_registry(mi);
                registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .register(Box::new(responder));
                // MISO is responder-owned from here on: the plain
                // digital-input sync must never also drive it.
                self.mcus[mi].responder_input_pins.insert(miso);
                self.hc165_chains.push(chain);
                break;
            }
        }
    }

    /// Chip-substitution events detected at build time (Track B). Empty when
    /// every instantiated MCU was modelled by its exact requested part.
    pub fn substitutions(&self) -> &[McuSubstitution] {
        &self.substitutions
    }

    /// Bus peripherals attached on a platform that models no matching bus
    /// controller (U3 finding 2), never exercised, recorded at attach time.
    pub fn unexercised_buses(&self) -> &[UnexercisedBus] {
        &self.unexercised_buses
    }

    /// Nets wired to an MCU pin whose drive this backend has NOT observed,
    /// on backends that cannot report drive direction. The honesty layer for
    /// the live-sim scope: on such a backend (the ESP32 QEMU RAM mailbox
    /// carries output LEVELS only, and the fork models no GPSPI/I2C
    /// controller, so hardware-peripheral traffic on a pin is invisible), a
    /// pin whose Thevenin driver is still tri-stated might be genuinely
    /// undriven OR driven in ways the backend cannot see; either way, the
    /// solved voltage on its net is the passive network's static level, not a
    /// measurement of MCU activity, and the UI must not present it as one.
    /// Direction-observable backends (simavr DDR hooks, dir-mapped Renode
    /// ports) are excluded: there a tri-stated driver IS the measured truth.
    /// A net some other, observed MCU driver is actively pushing on is also
    /// excluded: its reading is a real driven measurement. Recomputed per
    /// frame: the flag clears the moment the pin first reports a level.
    pub fn unobserved_drive_nets(&self) -> Vec<String> {
        let driven: std::collections::HashSet<u32> = self
            .mcus
            .iter()
            .flat_map(|m| m.binding.gpio_drivers.values())
            .filter(|d| d.enabled)
            .map(|d| d.net.0)
            .collect();
        let mut out: Vec<String> = self
            .mcus
            .iter()
            .filter(|m| !m.core.drive_direction_observable())
            .flat_map(|m| m.binding.gpio_drivers.values())
            .filter(|d| !d.enabled && !driven.contains(&d.net.0))
            .map(|d| self.circuit.node_name(d.net).to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// ADC channels whose injections the MCU backends DROPPED (no injection
    /// map), resolved to their board nets and nearby parts (U3 finding 1).
    /// Populated by the run itself (a drop is recorded when the scheduler's
    /// per-chunk push hits the backend's unmapped path), so query it after
    /// the co-sim, deterministic ordering by (mcu, channel).
    pub fn adc_dropped(&self) -> Vec<AdcDrop> {
        let node_names: HashMap<u32, &String> = self
            .net_nodes
            .iter()
            .map(|(name, node)| (node.0, name))
            .collect();
        let mut out = Vec::new();
        for m in &self.mcus {
            for ch in m.core.adc_dropped_channels() {
                let Some(&node) = m.binding.adc_nets.get(&ch) else {
                    continue;
                };
                let net = node_names
                    .get(&node.0)
                    .map(|s| (*s).clone())
                    .unwrap_or_else(|| format!("node {}", node.0));
                // Best-effort part naming: the devices on the net, excluding
                // MCU pin legs (the same exclusion the digital-in evidence
                // index applies), deduped by name and capped for readability.
                let mut parts: Vec<String> = self
                    .digital_in_evidence
                    .get(&node.0)
                    .into_iter()
                    .flatten()
                    .filter_map(|&di| self.circuit.devices.get(di as usize))
                    .map(|d| d.name().to_string())
                    .collect();
                parts.sort();
                parts.dedup();
                parts.truncate(3);
                out.push(AdcDrop {
                    mcu_ref: m.binding.reference.clone(),
                    channel: ch,
                    net,
                    parts,
                });
            }
        }
        out.sort_by(|a, b| a.mcu_ref.cmp(&b.mcu_ref).then(a.channel.cmp(&b.channel)));
        out
    }

    /// Record `bus`/`id` as unexercised when NO live MCU backend models a
    /// matching controller, and warn on stderr immediately (the same at-build
    /// loudness as a chip substitution). A board with no live MCUs stays
    /// silent: nothing co-simulates there at all, which the zero-activity /
    /// no-cosim surfaces already report.
    fn record_bus_if_unexercised(&mut self, id: &str, bus: &'static str, controller: Option<&str>) {
        if self.mcus.is_empty() {
            return;
        }
        let modeled = self.mcus.iter().any(|m| match bus {
            "I2C" => m.core.i2c_bus_modeled(),
            _ => m.core.spi_bus_modeled(controller),
        });
        if modeled {
            return;
        }
        let entry = UnexercisedBus {
            id: id.to_string(),
            bus,
            controller: controller.map(str::to_string),
        };
        eprintln!("WARNING: {}", entry.message());
        self.unexercised_buses.push(entry);
    }

    /// Whether any MCU produced at least one GPIO output edge, i.e. the firmware
    /// actually configured and drove a pin. This is the honest "the firmware did
    /// something" signal: unlike net `toggles` it survives a pin that is driven
    /// once and HELD (e.g. a boot-gate firmware that sets a control line high and
    /// leaves it), which contributes zero net transitions yet clearly ran. Keyed
    /// on `last_levels`, which is populated only from firmware pin-change edges.
    pub fn any_gpio_driven(&self) -> bool {
        self.mcus.iter().any(|m| !m.last_levels.is_empty())
    }

    /// Attach an I2C bus and register it as every live MCU's `on_i2c` handler.
    /// The bus is shared (Arc) so the same instance is both driven by the
    /// firmware's TWI activity and readable for assertions (EEPROM contents,
    /// sensor temperature).
    pub fn attach_i2c_bus(&mut self, bus: Arc<Mutex<I2cBus>>) {
        // Coverage honesty (U3 finding 2): a slave bound on a platform whose
        // backend models no I2C controller receives no traffic, ever. Record
        // it so every report surface says so instead of a silent green.
        let id = {
            use crate::peripherals::Peripheral as _;
            let g = bus.lock().unwrap_or_else(|e| e.into_inner());
            g.id().to_string()
        };
        self.record_bus_if_unexercised(&id, "I2C", None);
        self.i2c_buses.push(bus);
        // The AVR core's `on_i2c` closure and `set_i2c_slave_addresses` are
        // SINGLE-SLOT replacers, so a per-bus closure meant a second attach
        // silently overwrote the first bus's dispatcher AND dropped its addresses
        // from the TWI filter; the first bus went dead. Rebuild a MULTIPLEXING
        // dispatcher and the address UNION from the full bus list on every attach
        // (the last attach installs the complete handler). Each 7-bit address is
        // owned by at most one bus, so route by address and dispatch only to the
        // owner, never touching a sibling bus's state.
        let all: Vec<Arc<Mutex<I2cBus>>> = self.i2c_buses.clone();
        let addresses: Vec<u8> = all
            .iter()
            .flat_map(|b| b.lock().unwrap_or_else(|e| e.into_inner()).addresses())
            .collect();
        for m in &mut self.mcus {
            m.core.set_i2c_slave_addresses(&addresses);
            let buses = all.clone();
            m.core.on_i2c(Box::new(move |ev| {
                let addr = match ev {
                    hauksbee_mcu::I2cEvent::Start { addr, .. }
                    | hauksbee_mcu::I2cEvent::Write { addr, .. }
                    | hauksbee_mcu::I2cEvent::Read { addr }
                    | hauksbee_mcu::I2cEvent::Stop { addr } => addr,
                };
                for b in &buses {
                    let mut bus = b.lock().unwrap_or_else(|e| e.into_inner());
                    if bus.addresses().contains(&addr) {
                        return bus.dispatch(ev);
                    }
                }
                None
            }));
        }
    }

    /// Attach a SPI bus and register it as every live MCU's `on_spi` handler.
    ///
    /// `cs_pin` is the MCU pin `(port, bit)` that drives the slave's chip-select
    /// net, when the binder resolved it (05 §2.1). `Some` puts the bus on exact
    /// CS-edge framing: the CS GPIO edge stream frames each transaction at its
    /// true assert/deassert, so two transactions in one chunk are separated and a
    /// boundary-spanning transaction is not truncated. `None` leaves the bus on
    /// the chunk-boundary heuristic (the pre-05-§2 behaviour), reported honestly
    /// as `heuristic` in the co-sim coverage.
    pub fn attach_spi_bus(
        &mut self,
        bus: Arc<Mutex<SpiBus>>,
        cs_pin: Option<(char, u8)>,
        cs_net: Option<NodeId>,
    ) {
        let id = bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .id()
            .to_string();
        self.record_bus_if_unexercised(&id, "SPI", None);
        bus.lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cs_pin(cs_pin);
        self.register_cs_frame(&bus, cs_pin, cs_net);
        self.spi_buses.push(bus);
        // Rebuild a MULTIPLEXING `on_spi` across ALL attached buses. `on_spi` is a
        // single-slot replacer on the AVR core, so a per-bus closure meant a second
        // attach silently overwrote the first bus's transfer path, every byte then
        // went to the last-attached slave regardless of which chip-select was
        // asserted. Route each byte to the bus whose CS is currently asserted
        // (`is_selected`); a lone bus is always routed to, preserving the
        // single-slave path exactly (including before its first CS edge).
        let all: Vec<Arc<Mutex<SpiBus>>> = self.spi_buses.clone();
        for m in &mut self.mcus {
            let buses = all.clone();
            m.core.on_spi(Box::new(move |ev| dispatch_spi(&buses, ev)));
        }
    }

    /// Attach a SPI bus to a specific named SPI controller.
    ///
    /// Calls `on_spi_controller(controller, cb)` on each live MCU core so
    /// transfers from that controller route to this slave. On single-controller
    /// backends (AVR, QEMU), `on_spi_controller` falls back to `on_spi`, so
    /// calling this is safe even when there is only one physical SPI peripheral.
    ///
    /// The bus is also added to `spi_buses` so the chunk-boundary deselect
    /// loop (which is controller-agnostic) can reach it.
    ///
    /// `cs_pin` behaves exactly as in [`Self::attach_spi_bus`]: `Some` frames from the
    /// real CS edge, `None` falls back to the chunk-boundary heuristic.
    pub fn attach_spi_bus_on(
        &mut self,
        controller: &str,
        bus: Arc<Mutex<SpiBus>>,
        cs_pin: Option<(char, u8)>,
        cs_net: Option<NodeId>,
    ) {
        let id = bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .id()
            .to_string();
        self.record_bus_if_unexercised(&id, "SPI", Some(controller));
        bus.lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cs_pin(cs_pin);
        for m in &mut self.mcus {
            let b = bus.clone();
            let ctrl = controller.to_string();
            m.core.on_spi_controller(
                &ctrl,
                Box::new(move |ev| {
                    let mut guard = b.lock().unwrap_or_else(|e| e.into_inner());
                    if ev.deselect {
                        guard.note_backend_deselect();
                        0xFF
                    } else {
                        guard.transfer(ev.mosi)
                    }
                }),
            );
        }
        self.spi_controller_map
            .insert(controller.to_string(), bus.clone());
        self.register_cs_frame(&bus, cs_pin, cs_net);
        self.spi_buses.push(bus);
    }

    // (dispatch_spi is a free fn below.)

    /// Install the live CS-framing hook for `bus` on whichever MCU actually
    /// drives `cs_pin` (05 §2.1). Registers the hook on the SINGLE owning MCU, so a
    /// different MCU's identically-named pin cannot spuriously frame the bus. A
    /// `None` pin (unresolved CS) installs nothing and the bus stays on the
    /// chunk-boundary heuristic.
    ///
    /// `gpio_drivers` is keyed by chip-local `(port,bit)`, so on a multi-MCU board
    /// two MCUs can each own a driver for the SAME tuple on UNRELATED nets. Framing
    /// every such MCU let an unrelated MCU's toggle of its like-named pin
    /// spuriously select/deselect this bus, corrupting the decoded transaction. We
    /// install on only the FIRST MCU owning the pin, mirroring [`pin_driving_node`]
    /// (from which `cs_pin` was resolved), which returns the first match on the
    /// documented "a net is driven by at most one MCU" invariant.
    fn register_cs_frame(
        &mut self,
        bus: &Arc<Mutex<SpiBus>>,
        cs_pin: Option<(char, u8)>,
        cs_net: Option<NodeId>,
    ) {
        let Some(pin) = cs_pin else { return };
        // Install on the MCU that actually DRIVES the CS net, matching
        // pin_driving_node's net-based resolution, from which `cs_pin` was derived.
        // gpio_drivers is keyed by chip-local (port,bit), so on a multi-MCU board
        // the SAME tuple recurs on UNRELATED nets; keying only on the tuple installed
        // the frame on the first MCU that owns it, which need not drive the CS net,
        // so an unrelated MCU's like-named pin spuriously framed the bus. When the CS
        // net is known, require `drv.net == cs_net`; with no net (legacy callers) fall
        // back to the first tuple owner.
        let owner = self.mcus.iter().find(|m| {
            m.binding
                .gpio_drivers
                .get(&pin)
                .is_some_and(|drv| cs_net.map_or(true, |node| drv.net == node))
        });
        if let Some(m) = owner {
            let mut sh = m.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.cs_frames.push(CsFrame {
                pin,
                active_low: true,
                bus: bus.clone(),
            });
        }
    }

    /// Attach a bit-banged SPI slave (05 §1.5): the firmware toggles
    /// SCLK/MOSI/CS as plain GPIOs and reads MISO as a GPIO, and the
    /// [`crate::responders::BitBangSpiResponder`] bridges the bit stream to
    /// the byte-level slave in `bus`, answering MISO synchronously inside the
    /// firmware's own clock loop.
    ///
    /// The responder registers with the input-responder registry of the MCU
    /// whose binding owns the SCLK GPIO driver (the same ownership rule as
    /// `register_cs_frame`); all four pins must belong to that MCU. Pin tuples
    /// come from the board, resolve nets with [`Scheduler::mcu_pin_for_net`],
    /// the same net-to-pin trace the 165/595 chain discovery performs.
    ///
    /// The bus records `cs_n` as its CS pin so coverage reports `exact`
    /// framing and the chunk-boundary deselect heuristic stays off it
    /// (`frames_itself`). Deliberately NOT `register_cs_frame`: the responder
    /// owns select/deselect from the same CS edges, and registering both would
    /// double-deliver every CS event to the slave.
    ///
    /// Only meaningful on push backends (simavr): on poll backends the
    /// responder never fires (`on_input_responder` is a documented no-op) and
    /// a bit-banged read stays coarse, per the 05 §1.5 backend tier.
    pub fn attach_bitbang_spi(
        &mut self,
        bus: Arc<Mutex<SpiBus>>,
        pins: crate::responders::BitBangSpiPins,
    ) -> anyhow::Result<()> {
        let mi = self
            .mcus
            .iter()
            .position(|m| m.binding.gpio_drivers.contains_key(&pins.sclk))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "bit-banged SPI '{}': no live MCU drives SCLK pin {:?}",
                    bus.lock().unwrap_or_else(|e| e.into_inner()).id(),
                    pins.sclk
                )
            })?;
        for (name, pin) in [("MOSI", pins.mosi), ("MISO", pins.miso), ("CS", pins.cs_n)] {
            if !self.mcus[mi].binding.gpio_drivers.contains_key(&pin) {
                anyhow::bail!(
                    "bit-banged SPI '{}': {name} pin {:?} is not a wired GPIO of the MCU \
                     that drives SCLK ({})",
                    bus.lock().unwrap_or_else(|e| e.into_inner()).id(),
                    pin,
                    self.mcus[mi].binding.reference,
                );
            }
        }
        bus.lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cs_pin(Some(pins.cs_n));
        let responder = crate::responders::BitBangSpiResponder::new(bus.clone(), pins);
        self.responder_registry(mi)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(Box::new(responder));
        // MISO is responder-owned: the plain digital-input sync must never
        // also drive it.
        self.mcus[mi].responder_input_pins.insert(pins.miso);
        self.spi_buses.push(bus);
        Ok(())
    }

    /// Attach a soft-I2C slave bus (05 §1.5): the firmware bit-bangs SCL/SDA
    /// as plain GPIOs and the [`crate::responders::SoftI2cResponder`] protocol
    /// engine recovers the transaction from the pin edges, routing it to the
    /// existing [`I2cBus`] slave models and answering SDA synchronously inside
    /// the firmware's own clock loop. See the responder's docs for the honest
    /// waveform subset (single master, no clock stretching, push-pull master).
    ///
    /// The responder registers with the registry of the MCU whose binding owns
    /// the SCL GPIO driver; SDA must belong to the same MCU. The bus joins
    /// `i2c_buses` so the chunk loop's `flush_stops` delivers the ctx-bearing
    /// `on_stop` exactly like the hardware-TWI path, but deliberately WITHOUT
    /// `attach_i2c_bus`'s `on_i2c` registration: this bus lives on GPIO pins,
    /// not the TWI peripheral, and answering hardware-TWI traffic at these
    /// addresses would invent a device on the wrong pins.
    pub fn attach_soft_i2c(
        &mut self,
        bus: Arc<Mutex<I2cBus>>,
        scl: (char, u8),
        sda: (char, u8),
    ) -> anyhow::Result<()> {
        let mi = self
            .mcus
            .iter()
            .position(|m| m.binding.gpio_drivers.contains_key(&scl))
            .ok_or_else(|| anyhow::anyhow!("soft I2C: no live MCU drives SCL pin {scl:?}"))?;
        if !self.mcus[mi].binding.gpio_drivers.contains_key(&sda) {
            anyhow::bail!(
                "soft I2C: SDA pin {sda:?} is not a wired GPIO of the MCU that drives \
                 SCL ({})",
                self.mcus[mi].binding.reference,
            );
        }
        let responder = crate::responders::SoftI2cResponder::new(bus.clone(), scl, sda);
        self.responder_registry(mi)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(Box::new(responder));
        // SDA is bidirectional and responder-owned: the responder answers
        // ACKs/read bits on it inside the run loop, so the plain
        // digital-input sync must never also drive it.
        self.mcus[mi].responder_input_pins.insert(sda);
        self.i2c_buses.push(bus);
        Ok(())
    }

    /// Resolve a named net to the MCU GPIO pin wired to it. This is the
    /// net-to-pin trace the 165/595 chain discovery performs, exposed by net
    /// NAME so a caller wiring a bit-banged topology can go from the board's
    /// nets straight to responder pins. Input pins resolve too: every wired
    /// digital-capable pin gets a (possibly tri-stated) GPIO driver, so a
    /// MISO/SDA-style read pin carries the mapping even though the firmware
    /// never drives it.
    pub fn mcu_pin_for_net(&self, net: &str) -> Option<(char, u8)> {
        let node = *self.net_nodes.get(net)?;
        self.pin_driving_node(node)
    }

    /// Trace a net back to the MCU pin that drives it: the (port, bit) of the
    /// GPIO driver whose net is `node`, if any MCU drives it. This is the CS-net
    /// resolution the binder uses to populate `cs_pin` (05 §2.1): the same
    /// net-to-driving-pin trace the 74HC595 chain wiring performs to find its
    /// SRCLK/RCLK/SER pins. Returns the first match (a net is driven by at most
    /// one MCU push-pull output in a well-formed board).
    pub fn pin_driving_node(&self, node: NodeId) -> Option<(char, u8)> {
        // `gpio_drivers` is a HashMap with randomized iteration order, so when more
        // than one of an MCU's pins sits on `node`, a legitimate self-monitoring
        // topology, or two pins collapsed onto one net by a [[jumper]] bodge; the
        // first match, and hence the CS-framing pin, varied run to run. Pick the
        // lowest (port, bit) so the resolution is stable across process runs
        // (mirrors the sorted driver maps used for deterministic frame order).
        self.mcus
            .iter()
            .flat_map(|m| m.binding.gpio_drivers.iter())
            .filter(|(_, drv)| drv.net == node)
            .map(|(pin, _)| *pin)
            .min()
    }

    /// Per-slave SPI framing tier for the co-sim coverage: `(bus id, mode)` for
    /// every attached SPI bus (05 §2). A consumer reads this to know whether each
    /// slave's transaction boundaries are real (`exact`/`backend`) or guessed
    /// (`heuristic`).
    pub fn spi_framing_modes(&self) -> Vec<(String, SpiFramingMode)> {
        self.spi_buses
            .iter()
            .map(|b| {
                let g = b.lock().unwrap_or_else(|e| e.into_inner());
                (g.id().to_string(), g.framing_mode())
            })
            .collect()
    }

    /// Look up the SPI bus attached to a specific named controller.
    ///
    /// Returns `None` if no bus was attached to that controller via
    /// [`Self::attach_spi_bus_on`]. Buses attached via the controller-agnostic
    /// [`Self::attach_spi_bus`] are not findable by name (they carry no controller
    /// key in the map).
    pub fn spi_bus_for_controller(&self, controller: &str) -> Option<&Arc<Mutex<SpiBus>>> {
        self.spi_controller_map.get(controller)
    }

    /// Mutable access to the circuit so a caller can stamp a control's devices
    /// before attaching it. Call [`Scheduler::attach_peripheral`] afterwards,
    /// which relayouts the MNA system to pick up any new nodes/devices.
    pub fn circuit_mut(&mut self) -> &mut Circuit {
        &mut self.circuit
    }

    /// Attach a net/output peripheral (control, VCD sink). Relayouts the solver
    /// in case the peripheral stamped new circuit nodes or devices.
    pub fn attach_peripheral(&mut self, p: Box<dyn crate::peripherals::Peripheral>) {
        self.peripherals.push(p);
        self.relayout();
    }

    /// Schedule timeline events (press/release/set at time T).
    pub fn add_timeline(&mut self, events: Vec<TimelineEvent>) {
        self.peripherals.add_events(events);
    }

    /// Borrow the attached I2C buses (for assertions / sweeps).
    pub fn i2c_buses(&self) -> &[Arc<Mutex<I2cBus>>] {
        &self.i2c_buses
    }

    /// Borrow the attached SPI buses.
    pub fn spi_buses(&self) -> &[Arc<Mutex<SpiBus>>] {
        &self.spi_buses
    }

    /// Apply a live peripheral command (websocket SetInput onto a peripheral).
    /// Returns true if a peripheral with that id existed.
    pub fn set_peripheral(&mut self, id: &str, value: f64) -> bool {
        self.peripherals.set_value(id, value)
    }

    /// (id, kind) of every attached peripheral and bus, for board_info.
    pub fn peripheral_infos(&self) -> Vec<(String, String)> {
        use crate::peripherals::Peripheral as _;
        let mut out: Vec<(String, String)> = self
            .peripherals
            .peripherals
            .iter()
            .map(|p| (p.id().to_string(), p.kind().to_string()))
            .collect();
        for bus in &self.i2c_buses {
            let b = bus.lock().unwrap_or_else(|e| e.into_inner());
            out.push((b.id().to_string(), b.kind().to_string()));
        }
        for bus in &self.spi_buses {
            let b = bus.lock().unwrap_or_else(|e| e.into_inner());
            out.push((b.id().to_string(), b.kind().to_string()));
        }
        out
    }

    /// Peripheral state map, keyed by id, for component-state frames.
    pub fn peripheral_states(&self) -> HashMap<String, HashMap<String, f64>> {
        let mut m = self.peripherals.states();
        use crate::peripherals::Peripheral as _;
        for bus in &self.i2c_buses {
            let b = bus.lock().unwrap_or_else(|e| e.into_inner());
            m.insert(b.id().to_string(), b.state());
        }
        for bus in &self.spi_buses {
            let b = bus.lock().unwrap_or_else(|e| e.into_inner());
            m.insert(b.id().to_string(), b.state());
        }
        m
    }

    /// Number of live MCU cores.
    pub fn mcu_count(&self) -> usize {
        self.mcus.len()
    }

    /// Reference strings of the live MCUs (for serial routing).
    pub fn mcu_refs(&self) -> Vec<String> {
        self.mcus
            .iter()
            .map(|m| m.binding.reference.clone())
            .collect()
    }

    /// `(reference, backend, requested_part)` for each live MCU, in board order.
    /// The co-sim summary (Track B) reads this to report what part the board
    /// asked for alongside the backend that actually ran it.
    pub fn mcu_identities(&self) -> Vec<(String, String, String)> {
        self.mcus
            .iter()
            .map(|m| {
                (
                    m.binding.reference.clone(),
                    m.binding.backend.clone(),
                    m.binding.requested_part.clone(),
                )
            })
            .collect()
    }

    /// True if any live MCU runs on an external, wall-time-bounded emulator
    /// (Renode or QEMU). Those backends advance the guest clock over a TCP
    /// control socket with a per-chunk wall-time floor, so a fine analog
    /// `chunk_s` (the 100 us default that suits the in-process AVR core)
    /// multiplies into thousands of slow round-trips. A caller driving such a
    /// co-sim should coarsen `chunk_s` to a few milliseconds, the way the proven
    /// QEMU integration tests do, so the wall cost is the emulator's, not the
    /// chunk count's.
    pub fn has_external_backend(&self) -> bool {
        self.mcus
            .iter()
            .any(|m| backend_is_external(&m.binding.backend))
    }

    /// True when EVERY live MCU core can observe pin drive direction
    /// ([`Mcu::drive_direction_observable`]): the in-process AVR core (DDR
    /// hooks), and any Renode core whose SoC descriptor carries a verified
    /// direction-register map for each polled port. Conservative AND across
    /// cores: one direction-blind MCU makes the whole run's configured-output
    /// picture untrustworthy, so a boot-state check must then hedge ("undriven
    /// OR held LOW") rather than assert Hi-Z. Vacuously true with no MCUs
    /// (there is no pin whose direction could be misread), matching the old
    /// `!has_external_backend()` proxy this replaces.
    pub fn drive_direction_observable(&self) -> bool {
        self.mcus
            .iter()
            .all(|m| m.core.drive_direction_observable())
    }

    /// Advance the co-sim by `dt` seconds in fixed chunks.
    pub fn step(&mut self, dt: f64) -> StepResult {
        let mut uart: HashMap<String, Vec<u8>> = HashMap::new();
        let mut chunks = (dt / self.chunk_s).round() as u64;
        if chunks == 0 {
            chunks = 1;
        }
        let chunk = dt / chunks as f64;

        // Reset the per-frame extreme accumulators; run_chunk folds each chunk's
        // settled operating point in, so the runner reads true intra-frame peaks.
        self.frame_peak_current.clear();
        self.frame_v_extremes.clear();

        for _ in 0..chunks {
            self.run_chunk(chunk, &mut uart);
        }

        StepResult {
            sim_time: self.sim_time,
            uart,
        }
    }

    fn run_chunk(&mut self, chunk: f64, uart: &mut HashMap<String, Vec<u8>>) {
        // Integer microseconds for `run_micros`, carrying the sub-microsecond
        // remainder across chunks so the firmware clock does not drift from sim
        // time. A bare `(chunk * 1e6).round()` per chunk accumulates a rounding
        // error every chunk (and a chunk under 0.5 µs rounds to 0, then gets
        // clamped up to 1 µs, injecting time that never elapsed); banking the
        // truncated fraction makes the delivered microseconds sum to the true
        // elapsed time.
        //
        // Do NOT clamp the floored value up to 1: a persistent sub-1 µs chunk
        // (e.g. a fine `fixed_dt = 0.5e-6`) never reaches a whole banked
        // microsecond, so a `.max(1.0)` would deliver 1 µs every chunk while
        // banking unrepayable negative debt; the firmware clock races ahead of
        // sim time without bound. Instead the core advances 0 µs on a sub-µs
        // chunk and rolls forward once the banked fraction accrues a full
        // microsecond, which keeps `micros_carry` in [0, 1) and the delivered
        // microseconds tracking true elapsed time exactly. `run_micros(0)` is a
        // no-op, and a normal-size chunk (floor ≥ 1) is unaffected.
        let exact = chunk * 1e6 + self.micros_carry;
        let micros_f = exact.floor();
        self.micros_carry = exact - micros_f;
        let micros = micros_f as u64;
        // Set when any MCU refuses to advance this chunk (see the `run_micros`
        // Err handling below); folded into the chunk-failure accounting after
        // the analog solve so the run refuses to report a fake-quiet chunk.
        let mut mcu_run_failed = false;

        // Refresh the snapshot the edge-driven 74HC165 read chains sample on a
        // PL load (it fires inside the MCU run below, so it must reflect the
        // PREVIOUS chunk's settled spike-latch voltages). Cheap clone; only
        // taken when a 165 read chain is present.
        if !self.hc165_chains.is_empty() {
            let mut snap = self.input_volts.lock().unwrap_or_else(|e| e.into_inner());
            snap.clear();
            snap.extend_from_slice(&self.node_volts);
        }

        // Per-chunk accessor state, rebuilt as each MCU drains below.
        self.last_chunk_edges.clear();
        self.last_replay_microticks = 0;

        // Nets currently driven by ANY MCU's enabled GPIO driver, for the
        // plain digital-input sync below: an enabled driver is real drive
        // evidence even though pin-driver legs are excluded from the static
        // `digital_in_evidence` index (this is what makes a direct
        // MCU-to-MCU GPIO link readable on the receiving side).
        let mcu_driven_nets: std::collections::HashSet<u32> = self
            .mcus
            .iter()
            .flat_map(|m| m.binding.gpio_drivers.values())
            .filter(|d| d.enabled)
            .map(|d| d.net.0)
            .collect();

        // 1. MCU: inject latest ADC voltages, run the chunk, drain captures.
        for mi in 0..self.mcus.len() {
            let m = &mut self.mcus[mi];
            for (&ch, &node) in &m.binding.adc_nets {
                // Skip a pin the firmware has promoted to a GPIO output. An
                // analog-capable pin binds BOTH an ADC channel and a tri-stated
                // GPIO driver (dynamic promotion); once
                // that driver is enabled the pin is being DRIVEN, not read, so
                // injecting an ADC voltage for it is contradictory (a phantom
                // analog reading on a pin the firmware owns as an output).
                // Promotion is detected as THIS channel's OWN pin driver being
                // enabled, not merely any enabled driver sharing the net. Keying
                // on the net wrongly suppressed injection whenever a DIFFERENT
                // pin's output driver happened to sit on the same net (e.g. an
                // output pin wired directly to an ADC input to self-monitor it),
                // and it could never inject an ADC-ONLY channel (A6/A7 own no
                // driver, so they have no `adc_pin` entry and are never promoted).
                // A pin never driven keeps its driver disabled and is injected.
                if adc_channel_promoted(&m.binding, ch) {
                    continue;
                }
                let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
                m.core.set_analog_in(ch, v.max(0.0));
            }
            // 1a. Plain digital inputs: mirror the previous chunk's SOLVED net
            // voltage into the core's digital-in for every wired GPIO pin the
            // circuit (not the firmware) owns. This is the inbound direction
            // of the pin coupling: without it nothing calls `set_digital_in`,
            // and a pushbutton / limit switch / comparator output on a plain
            // input pin never reaches `digitalRead`. Symmetric to the
            // adc_nets loop above: injected before `run_micros`, so firmware
            // reads the level of the last settled operating point.
            //
            // A pin is synced only when ALL of these hold:
            //   * its driver is tri-stated (an enabled driver means the
            //     firmware owns the pin as an output, same promotion rule as
            //     the ADC skip above);
            //   * it is not responder-owned (165 MISO / bit-bang SPI MISO /
            //     soft-I2C SDA get edge-granularity drives from their
            //     responder inside the run loop; a chunk-boundary level would
            //     fight them);
            //   * its net shows real drive evidence: a non-pin device with
            //     live resistance under `WEAK_DIGITAL_DRIVE_OHMS` (or any
            //     non-R/C device), or another enabled GPIO driver. A floating
            //     net's ~0 V solve is the pins' own 1 GΩ legs talking, and
            //     pushing it would defeat an (unmodeled) internal pull-up.
            //
            // Levels use the 0.3/0.7-rail thresholds (the classic CMOS
            // Vil/Vih convention at the MCU's own rail) with the in-between
            // band as hysteresis: a mid-rail solve holds the pin's previous
            // level rather than chattering. `set_digital_in` fires only on a
            // level CHANGE, so poll backends pay per transition. Skipped on
            // the very first chunk (`sim_time == 0`): nothing has been solved
            // yet, and pushing the zero-filled seed would report fiction,
            // the core's power-on level is the honest state until a solve.
            if self.sim_time > 0.0 {
                let vih = 0.7 * m.logic_high_v;
                let vil = 0.3 * m.logic_high_v;
                for (&(port, bit), drv) in &m.binding.gpio_drivers {
                    if drv.enabled || m.responder_input_pins.contains(&(port, bit)) {
                        continue;
                    }
                    let net = drv.net.0;
                    let driven = mcu_driven_nets.contains(&net)
                        || self.digital_in_evidence.get(&net).is_some_and(|devs| {
                            devs.iter().any(|&di| {
                                match self.circuit.devices.get(di as usize) {
                                    Some(Device::Resistor { ohms, .. }) => {
                                        *ohms < WEAK_DIGITAL_DRIVE_OHMS
                                    }
                                    // A capacitor cannot decide a DC level.
                                    Some(Device::Capacitor { .. }) => false,
                                    Some(_) => true,
                                    None => false,
                                }
                            })
                        });
                    if !driven {
                        continue;
                    }
                    let v = self.node_volts.get(net as usize).copied().unwrap_or(0.0);
                    let prev = m.digital_in_levels.get(&(port, bit)).copied();
                    let level = if v >= vih {
                        true
                    } else if v <= vil {
                        false
                    } else {
                        match prev {
                            Some(p) => p,     // hysteresis: hold the last level
                            None => continue, // mid-band, no history: leave power-on
                        }
                    };
                    if prev != Some(level) {
                        m.core.set_digital_in(PinId { port, bit }, level);
                        m.digital_in_levels.insert((port, bit), level);
                    }
                }
            }
            // Push modeled I2C temperature-sensor readings into the backend's own
            // emulated device (the QEMU ESP32 tmp105). The simavr/Renode backends
            // ignore this (they answer I2C reads through the `on_i2c` byte
            // callback); QEMU runs the firmware against a real device, so it reads
            // the value through its own I2C controller. Done each chunk so a
            // temperature sweep tracks.
            for bus in &self.i2c_buses {
                let sensors = bus
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .temperature_sensors();
                for (addr, milli_c) in sensors {
                    m.core.set_i2c_device_temperature(addr, milli_c);
                }
            }
            // Cycle counter bracketing this run: the chunk's [start, end) span,
            // so the drained edge stamps normalize to a fraction of the chunk for
            // the analog PWL side (05 §1.1). Exact on simavr, coarse on poll
            // backends (flagged by `cycle_exact`).
            let cyc_start = m.core.current_cycle();
            if let Err(e) = m.core.run_micros(micros) {
                // The MCU backend refused to advance this chunk (a crashed core,
                // a backend transport error, a HALT). Do NOT swallow it: the
                // firmware side of this chunk did not run, so folding the
                // subsequent solve as a normal quiet chunk would report a fake
                // clean run. Flag it loudly and mark the chunk failed below so
                // strict/CI runs abort rather than trust it (05 §3b, refuse
                // rather than fake).
                eprintln!(
                    "WARNING: MCU {} refused to advance chunk at t={:.6}s ({micros} us): {e:#}",
                    m.binding.reference, self.sim_time,
                );
                mcu_run_failed = true;
            }
            let cyc_end = m.core.current_cycle();
            let cycle_exact = m.core.cycle_exact();
            let mcu_ref = m.binding.reference.clone();

            let (edges, edge_log, bytes) = {
                let mut sh = m.shared.lock().unwrap_or_else(|e| e.into_inner());
                (
                    std::mem::take(&mut sh.pin_edges),
                    std::mem::take(&mut sh.pin_edge_log),
                    std::mem::take(&mut sh.uart_out),
                )
            };
            if !bytes.is_empty() {
                uart.entry(mcu_ref.clone()).or_default().extend(bytes);
            }
            // Expose this MCU's cycle-stamped edges (per pin) for the analog side.
            self.last_chunk_edges.push(ChunkPinEdges {
                mcu_reference: mcu_ref,
                edges: crate::digital::pin_edges_by_pin(&edge_log),
                cycle_span: (cyc_start, cyc_end),
                chunk_s: chunk,
                cycle_exact,
            });
            // 1b. Generalized digital replay (05 §1.2): drain THIS MCU's ordered,
            // cycle-stamped log and replay it in cycle order through every
            // edge-driven digital element on one path; the 595 chains it owns AND
            // any standalone GPIO-clocked shift/latch (`replay_chips`). Each
            // edge-group sharing a cycle is one micro-tick, so a bit-banged
            // SRCLK/RCLK pulse train clocks the chain per edge instead of
            // collapsing to a level (FIX 1). Only chains whose owning MCU is `mi`
            // are replayed, so a different MCU's identically-named pin cannot
            // inject spurious clocks. Latched outputs reach the analog nets via the
            // digital tick / chain apply below.
            self.last_replay_microticks += self.replay_digital_edges(mi, &edge_log);
            // 2. Apply GPIO edges to drivers. An edge means the firmware has
            // configured the pin as a driven output, so enable the (initially
            // tri-stated) Thevenin leg before setting its level. This is also
            // the promotion path for a dual-bound analog-capable pin (dynamic
            // promotion): the first firmware drive of
            // an A-pin enables its driver exactly like any other GPIO, while a
            // pin never driven keeps its driver disabled and stays a pure ADC
            // input.
            let m = &mut self.mcus[mi];
            for ((port, bit), level) in edges {
                m.last_levels.insert((port, bit), level);
                if let Some(drv) = m.binding.gpio_drivers.get_mut(&(port, bit)) {
                    drv.set_enabled(&mut self.circuit, true);
                    let v = if level { m.logic_high_v } else { 0.0 };
                    drv.set_volts(&mut self.circuit, v);
                }
            }
            // 2b. Promotion AND release from the configured pin direction
            // (`pins_configured_output`); see `sync_configured_outputs`.
            // Backends that cannot report direction return an empty set,
            // making both halves a no-op there; the edge-driven enable above
            // remains the primary path.
            let configured: std::collections::HashSet<(char, u8)> = m
                .core
                .pins_configured_output()
                .into_iter()
                .map(|p| (p.port, p.bit))
                .collect();
            self.sync_configured_outputs(mi, configured);
        }

        // 1c. Sub-chunk pulse honesty (friction 1.16): a GPIO pulse that rose
        // and fell inside THIS chunk is invisible to every tick-evaluated
        // sequential part on its net (they sample once per chunk, against the
        // previous solve), while chain responders resolve the same edges
        // exactly. Warn once per offending net, from the cycle-stamped edge
        // log just drained.
        self.detect_short_pulses(chunk);

        // 5(prev). Digital components drive their outputs from current state,
        // sampling the previous chunk's solved node voltages. Chips clocked by an
        // edge path are SKIPPED here: chips owned by an edge-driven 595 chain
        // (`chain_chips`) and standalone GPIO-clocked shift/latch parts advanced
        // by the generalized replay (`replay_chips`) already ran at edge
        // granularity above, so ticking them once-per-chunk too would double-drive
        // them with a stale, pulse-collapsed sample.
        {
            let volts = self.node_volts.clone();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for (i, d) in self.digital.iter_mut().enumerate() {
                if self.chain_chips.contains(&i) || self.replay_chips.contains(&i) {
                    continue;
                }
                d.tick(&mut self.circuit, &node_v);
            }
        }
        // 5b(prev). Push the edge-driven chains' latched outputs onto the analog
        // nets (the latched switch-select levels the membrane solve will see).
        // Move the chains out to satisfy the borrow checker (apply needs &mut
        // self.digital and &mut self.circuit), then put them back.
        if !self.chains.is_empty() {
            let mut chains = std::mem::take(&mut self.chains);
            for chain in &mut chains {
                chain.apply(&mut self.digital, &mut self.circuit);
            }
            self.chains = chains;
        }

        // 5b'. Runtime driver-contention monitor (the model-vs-MCU half of the
        // field failure the static lint documents as out of static reach in
        // checks/contention.rs). Runs after the MCU edge/DDR sync (which sets
        // the firmware side's driver enables) AND after the digital tick /
        // chain apply (which set the model side's, tri-state included), so
        // both sides' live drive states are current for this chunk.
        self.detect_driver_contention();

        // 5b2(prev). Refresh every digital part's VCC supply draw for this
        // chunk: drain the output-transition accumulators (filled above by the
        // per-chunk ticks AND the edge-granularity replay/chain paths) and set
        // each part's supply Isource to static + n·Cpd_eff·VCC/dt. This runs
        // over ALL digital components, chain-owned chips are skipped by the
        // tick loop but still switch, and their supply legs are refreshed
        // here. Parts without supply params have no leg and no-op.
        {
            let volts = self.node_volts.clone();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for d in self.digital.iter_mut() {
                d.update_supply(&mut self.circuit, chunk, &node_v);
            }
        }

        // 5c(prev). Deliver the deferred I2C transaction-end hooks (05 §3.1):
        // every slave that saw a STOP during this chunk's MCU run gets
        // `on_stop(ctx)` so it can drive its output nets before this chunk's
        // solve; the write-side analogue of the 595 chain apply above (and
        // how a firmware MCP4728 write becomes a real VOUT net voltage). The
        // byte dispatch itself runs inside the MCU's `on_i2c` callback, where
        // no TickCtx can be built; the STOP is recorded there and delivered
        // here, the first point the circuit is borrowable and the earliest the
        // analog solve could see the result anyway.
        if !self.i2c_buses.is_empty() {
            let buses = self.i2c_buses.clone();
            let volts = self.node_volts.clone();
            let mut ctx = TickCtx {
                circuit: &mut self.circuit,
                node_volts: &volts,
                t: self.sim_time,
                dt: chunk,
            };
            for b in buses {
                b.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .flush_stops(&mut ctx);
            }
        }

        // 2b. Update configurable power supplies from the rail current measured
        // in the *previous* chunk, setting this chunk's commanded voltage (the
        // PinDriver pattern: behavioral source updated between solver chunks).
        self.update_supplies(chunk);

        // 2b'. Update behavioural devices (chargers/PMICs/balancers) from the
        // previous chunk's solved operating point: advance FSMs, recompute
        // converter regulation/limits, evaluate expression laws. Same cadence.
        self.update_behavioral(chunk);

        // 2c. Peripherals: fire any timeline events due by now, then let each
        // control push its commanded level onto its net before the solve.
        if !self.peripherals.is_empty() {
            self.peripherals.fire_due_events(self.sim_time);
            let volts = self.node_volts.clone();
            let mut ctx = TickCtx {
                circuit: &mut self.circuit,
                node_volts: &volts,
                t: self.sim_time,
                dt: chunk,
            };
            self.peripherals.pre_solve(&mut ctx);
        }

        // 3. Analog: solve a transient over the chunk; read final voltages and
        // branch currents. A false return means the solve did not converge and
        // this chunk is holding stale voltages (05 §3b): its operating point is
        // fiction, so the stats/stress fold below is skipped for it.
        // 2d. PWL edge drive (05 section 1.3). A pin that toggled more than
        // once this chunk collapsed to its final level in the driver path
        // above, which is electrically wrong for any net whose analog
        // response integrates the pulse train (an RC-loaded clock line, a
        // charge pump, a gate filter). For such pins, swap the driver's
        // source to the chunk's exact cycle-stamped PWL waveform for this one
        // solve; the solver's source-breakpoint table then lands the adaptive
        // integrator on every corner. Restored to the settled DC level right
        // after, so the digital tick and the next chunk see the final level.
        let pwl_restores = self.apply_pwl_drives(chunk);
        let chunk_converged = self.solve_chunk(chunk);
        self.restore_pwl_drives(&pwl_restores);
        // An MCU that refused to advance makes this chunk untrustworthy even if
        // the analog march converged. `solve_chunk` records an analog failure but
        // does NOT reset the consecutive-failure streak on its own; the streak
        // must reflect an MCU failure too. Fold the MCU failure into the same
        // accounting so the failed-window and consecutive-failure surfaces see
        // it. Only record here when the analog side converged, otherwise
        // `solve_chunk` already recorded this exact window and a second call
        // would double-count it. `sim_time` has not advanced yet, so the window
        // start matches `solve_chunk`'s.
        if mcu_run_failed && chunk_converged {
            self.record_failed_chunk(chunk);
        }
        // The consecutive-failure streak resets ONLY on a fully-successful chunk
        // (analog converged AND the MCU advanced). Doing this reset inside
        // `solve_chunk` on analog convergence alone let an MCU-failed-but-analog-
        // converged chunk zero the streak before `record_failed_chunk` bumped it
        // back to 1, capping `max_consecutive_failed_chunks` at 1 and defeating
        // the strict/CI abort (05 §3b) for a sustained MCU crash.
        if chunk_converged && !mcu_run_failed {
            self.consecutive_failed_chunks = 0;
        }

        // 3b. Peripherals: output sinks sample the freshly-solved voltages.
        //
        // Runs UNCONDITIONALLY, even when `chunk_converged` is false (05 §3b). We
        // deliberately do NOT gate this on the analog solve, for two reasons:
        //
        //   * The SPI/I2C slave state machines reached via `post_solve` (e.g. the
        //     SpiBus deselect) are DIGITAL frame-boundary resets. The byte
        //     transfers they frame happened during this chunk's `run_micros`
        //     (step 1) regardless of whether the analog march converged. Skipping
        //     the per-chunk deselect on a failed chunk would leave a slave stuck
        //     mid-command and desync the NEXT chunk's transaction (its bytes would
        //     append to stale command state). That is a real correctness bug, so
        //     the reset must fire every chunk.
        //   * The only voltage-sampling sink here is `VcdSink`, which emits a VCD
        //     change only when a net's level CROSSES a threshold. A failed chunk
        //     holds (or DC-recovers) the previous voltages, so a held net is at the
        //     same level and no spurious transition is recorded; a DC-recovered
        //     net records its bias, not a fabricated toggle. Either way the sample
        //     lands inside a window already surfaced as `analog_valid:false` with
        //     the exact `failed_windows` span (JSON/coverage), so a VCD consumer
        //     can mask it. What we DO gate on convergence is the stats/stress fold
        //     below (step 4/6): those manufacture analog findings and must not run
        //     on a solve that never happened.
        if !self.peripherals.is_empty() {
            let volts = self.node_volts.clone();
            let mut ctx = TickCtx {
                circuit: &mut self.circuit,
                node_volts: &volts,
                t: self.sim_time,
                dt: chunk,
            };
            self.peripherals.post_solve(&mut ctx);
        }

        // 3c. SPI bus chunk-boundary deselect: HEURISTIC-MODE BUSES ONLY.
        //
        // A chunk-boundary deselect stands in for a real chip-select edge,
        // which simavr's SPI IRQ never surfaces (it reports byte transfers and
        // nothing else). Applied to EVERY bus unconditionally it is wrong in
        // two documented ways (05 §2):
        //   * two CS-framed transactions inside one chunk are NOT separated:
        //     the second transaction's bytes append to the first slave's state,
        //     because no reset happens between them; and
        //   * a single transaction SPANNING a chunk boundary is reset mid-way:
        //     the slave deselects with bytes still pending, corrupting the reply
        //     (the debug guard below fires on exactly this case).
        //
        // Neither can bite a bus with a real CS source. When the binder
        // resolves the CS net to an MCU pin (`cs_pin`), the `on_pin_change`
        // closure frames transactions at the true active-low CS edges (mid-chunk
        // included) via the `CsFrame` hook (05 §2.1); and a backend that surfaces
        // CS itself (Renode hardware-NSS `FinishTransmission`) frames via the
        // `note_backend_deselect` path. For those buses (`frames_itself()`), a
        // chunk-boundary reset would CAUSE failure mode b (truncating a
        // legitimately boundary-spanning transaction), so we SKIP it and let the
        // real CS edges own framing (05 §2, failure mode b).
        //
        // Only buses still on the heuristic (no resolved CS pin, no backend CS
        // event, e.g. simavr with an unrouted CS, or Renode software-NSS) keep
        // the chunk-boundary deselect, and their coverage says `heuristic` so the
        // guess is surfaced rather than hidden.
        //
        // Runs UNCONDITIONALLY on a failed chunk (05 §3b), same reason as the 3b
        // post_solve deselects: this is a DIGITAL frame-boundary reset of the SPI
        // slave command state machine, not an analog sample. The byte transfers it
        // frames already happened in this chunk's `run_micros`, so whether the
        // analog march converged is irrelevant; skipping it would desync the next
        // transaction, so we keep it and surface the failed span via
        // `failed_windows` / `analog_valid:false` instead.
        for bus in &self.spi_buses {
            let mut guard = bus.lock().unwrap_or_else(|e| e.into_inner());
            // Real-CS buses frame themselves; the chunk boundary must not touch
            // them (05 §2, failure mode b). The debug warning below therefore also
            // stops firing for them: a spanning transaction is now correct, not a
            // truncation to warn about.
            if guard.frames_itself() {
                continue;
            }
            // Debug-only: a heuristic-mode slave still mid-transaction at the chunk
            // boundary means a transfer spanned the boundary, and the heuristic cannot
            // frame it. Warn loudly in debug builds rather than silently truncating.
            // (Not an assert/panic: spanning is a known limitation of the heuristic
            // path, not a bug to abort on.)
            #[cfg(debug_assertions)]
            if guard.slave_mid_transaction() {
                eprintln!(
                    "WARN: SPI bus '{}' deselected mid-transaction at chunk boundary \
                     (transfer spans the {:.3} ms chunk); reply may be truncated. \
                     Resolve the CS net (cs_pin) or use a backend that surfaces \
                     chip-select for exact framing.",
                    guard.id(),
                    chunk * 1e3,
                );
            }
            guard.slave_deselect();
        }

        // 4. Advance time (time passes even when the solve failed), then fold the
        // chunk into the running stats and the stress monitor, but ONLY if the
        // analog solve converged. A failed chunk holds the previous chunk's stale
        // voltages (solve_chunk's `Err` arm); folding that stale operating point
        // into the net stats or evaluating the stress monitor on it would
        // manufacture toggles and faults from a solve that never happened. That is
        // exactly the silent-hold defect 05 §3b refuses: the failed window is
        // recorded instead and surfaced as analog_valid:false, rather than an
        // analog-derived finding evaluated on stale state.
        self.sim_time += chunk;
        if chunk_converged {
            self.update_stats();
            self.accumulate_frame_peaks();

            // 6. Fault / stress monitor: evaluate every device against its ratings
            // using this chunk's solved operating point (may mutate the circuit in
            // destructive mode).
            self.evaluate_faults();
        }
    }

    /// Recompute each supply's commanded voltage from its last-measured rail
    /// current and write it onto the supply's `Vsource`.
    fn update_supplies(&mut self, chunk: f64) {
        if self.supplies.is_empty() {
            return;
        }
        let t = self.sim_time;
        for s in &mut self.supplies {
            let i = self
                .layout
                .branch(s.vsource)
                .and_then(|b| {
                    self.branch_x
                        .get(b.saturating_sub(self.layout.n_nodes))
                        .copied()
                })
                .unwrap_or(0.0);
            // Branch current of a Vsource flows p->n internally; the current
            // *delivered to the net* is the negative of that. Use magnitude.
            s.update(&mut self.circuit, i.abs(), t, chunk);
        }
    }

    /// Update behavioural devices from the previous chunk's solved operating
    /// point: node voltages and the converter output-source branch currents.
    /// Collects any faults each device raised (input overdraw, etc).
    fn update_behavioral(&mut self, chunk: f64) {
        if self.behavioral.is_empty() {
            return;
        }
        let t = self.sim_time;
        let volts = self.node_volts.clone();
        let branch = self.branch_x.clone();
        let layout = self.layout.clone();
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        let branch_current = |id: DeviceId| -> Option<f64> {
            layout
                .branch(id)
                .and_then(|b| branch.get(b.saturating_sub(layout.n_nodes)).copied())
        };
        for d in &mut self.behavioral {
            d.update(&mut self.circuit, &node_v, &branch_current, t, chunk);
            self.faults_pending.extend(d.drain_faults());
        }
    }

    /// Live readout per behavioural device: (reference, state, converter input
    /// current A, converter input limit A). For diagnostics / frames / tests.
    pub fn behavioral_states(&self) -> Vec<(String, String, Option<f64>, Option<f64>)> {
        self.behavioral
            .iter()
            .map(|d| {
                (
                    d.reference.clone(),
                    d.state().to_string(),
                    d.converter_iin(),
                    d.converter_iin_limit(),
                )
            })
            .collect()
    }

    /// Set an input-power budget (W) at a given input voltage on a named
    /// behavioural device's converter; raises an overpower fault when the
    /// reflected input draw exceeds the budget. Returns true if the device
    /// existed. The scheduler calls the per-device budget check each chunk once
    /// a budget is set.
    pub fn set_behavioral_input_budget(
        &mut self,
        reference: &str,
        vin: f64,
        budget_w: f64,
    ) -> bool {
        for d in &mut self.behavioral {
            if d.reference == reference {
                d.set_input_budget(vin, budget_w);
                return true;
            }
        }
        false
    }

    /// Mutable access to a behavioural device by reference (tests/sweeps).
    pub fn behavioral_device(&mut self, reference: &str) -> Option<&mut BehavioralDevice> {
        self.behavioral
            .iter_mut()
            .find(|d| d.reference == reference)
    }

    /// Read the current value (A) of a named current-law on a behavioural device
    /// (e.g. the LTC6803 balancer-leak current). `None` if absent.
    pub fn behavioral_law_value(&self, reference: &str, law: &str) -> Option<f64> {
        let d = self.behavioral.iter().find(|d| d.reference == reference)?;
        d.law_value(&self.circuit, law)
    }

    /// Evaluate the stress monitor over the chunk just solved.
    fn evaluate_faults(&mut self) {
        if self.stress.device_count() == 0 {
            return;
        }
        let volts = self.node_volts.clone();
        let branch = self.branch_x.clone();
        let layout = self.layout.clone();
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        let branch_current = |id: DeviceId| -> Option<f64> {
            layout
                .branch(id)
                .and_then(|b| branch.get(b.saturating_sub(layout.n_nodes)).copied())
        };
        let new = self
            .stress
            .evaluate(&mut self.circuit, &node_v, &branch_current, self.sim_time);
        self.faults_pending.extend(new);
    }

    /// Solve one chunk's transient. Returns `true` when the analog march
    /// converged, `false` when it failed and this chunk is holding recovered/
    /// stale voltages (the caller then excludes it from stats and stress, and the
    /// run reports `analog_valid: false` over the failed window; 05 §3b).
    fn solve_chunk(&mut self, chunk: f64) -> bool {
        // Keep temperature in sync with the circuit's global temp.
        self.circuit.temp_c = self.opts.temperature_c;
        let t = Transient::new(self.opts);
        let n_nodes = self.circuit.node_count();
        let mut final_x: Vec<f64> = Vec::new();
        // Run a short transient; capture the last accepted step's unknowns.
        // Warm-start this chunk's DC operating point from the previous chunk's
        // final unknowns: the operating point barely moves between 100 us chunks,
        // so plain Newton converges in ~1 iteration instead of re-running the
        // cold-start gmin/source-stepping homotopy on the full nonlinear board
        // every chunk. Exact (same root, fewer iters); a size-mismatched or
        // failing seed falls back to the cold solve inside the solver.
        let res = t.run_streaming_seeded(&self.circuit, chunk, self.last_dc_seed.as_deref(), |s| {
            final_x.clear();
            final_x.extend_from_slice(s.x);
        });
        let converged = match res {
            Ok(()) => {
                self.node_volts.resize(n_nodes, 0.0);
                self.node_volts[0] = 0.0;
                for node in 1..n_nodes {
                    self.node_volts[node] = final_x.get(node - 1).copied().unwrap_or(0.0);
                }
                // Branch currents follow the node block in layout order.
                let n_branch = self.layout.size.saturating_sub(self.layout.n_nodes);
                self.branch_x.resize(n_branch, 0.0);
                for b in 0..n_branch {
                    self.branch_x[b] = final_x.get(self.layout.n_nodes + b).copied().unwrap_or(0.0);
                }
                // Seed the next chunk's DC solve with this chunk's end state.
                self.last_dc_seed = Some(final_x);
                true
            }
            Err(_) => {
                // The transient march failed to advance. If the DC operating
                // point at t=0 was still captured (the streaming sink fires once
                // before the march loop), use it: a converged DC bias is a far
                // better state to report and to seed the next chunk from than a
                // hard zero, which would brown out the modelled MCU. This is what
                // lets a board whose stiff nonlinear march cannot progress (e.g.
                // the diode-laden Tarski synapse core) still hold its DC rails and
                // DAC/peripheral voltages instead of collapsing the whole co-sim.
                if !final_x.is_empty() {
                    self.node_volts.resize(n_nodes, 0.0);
                    self.node_volts[0] = 0.0;
                    for node in 1..n_nodes {
                        self.node_volts[node] = final_x.get(node - 1).copied().unwrap_or(0.0);
                    }
                    let n_branch = self.layout.size.saturating_sub(self.layout.n_nodes);
                    self.branch_x.resize(n_branch, 0.0);
                    for b in 0..n_branch {
                        self.branch_x[b] =
                            final_x.get(self.layout.n_nodes + b).copied().unwrap_or(0.0);
                    }
                    self.last_dc_seed = Some(final_x);
                } else {
                    // Nothing usable captured: hold previous voltages, cold-start
                    // the next chunk.
                    self.node_volts.resize(n_nodes, 0.0);
                    self.last_dc_seed = None;
                }
                false
            }
        };
        // A failed transient (either DC-recovered or held) is not a real solve of
        // this chunk. Record it so the run refuses to pass it off as quiet: the
        // failed-chunk count and window feed coverage/JSON (analog_valid:false),
        // and the consecutive streak drives the strict/CI abort (05 §3b). The
        // streak is NOT reset here on convergence: an MCU-failed chunk can still
        // reach this point analog-converged, and only `run_chunk`, which also
        // knows the MCU status, may reset the streak on a fully-successful chunk.
        if !converged {
            self.record_failed_chunk(chunk);
        }

        // Apply any forced node-voltage overrides (the firmware-driven Tarski
        // inference drives the output SPIKE nets from the exact feedforward
        // decomposition here, since the monolith does not converge). These land
        // in `node_volts` so the on-board NOR latches sample them next chunk.
        // Each override is time-gated to `sim_time` so a column is HIGH only for
        // its decomposed rate fraction of the window.
        if !self.forced_node_volts.is_empty() {
            let t = self.sim_time;
            for (&node, &(hi, lo, t0, t1)) in &self.forced_node_volts {
                let v = if t >= t0 && t < t1 { hi } else { lo };
                if let Some(slot) = self.node_volts.get_mut(node) {
                    *slot = v;
                }
            }
        }
        converged
    }

    /// Record one non-convergent chunk: bump the failed-chunk count and the
    /// consecutive streak, and extend/append the failed sim-time window. Called
    /// from `solve_chunk` before `sim_time` advances, so `[sim_time, sim_time +
    /// chunk)` is the window this failed chunk covers. Consecutive failed chunks
    /// are merged into one window so a diverged stretch reads as its true extent.
    fn record_failed_chunk(&mut self, chunk: f64) {
        self.failed_chunks += 1;
        self.consecutive_failed_chunks += 1;
        self.max_consecutive_failed_chunks = self
            .max_consecutive_failed_chunks
            .max(self.consecutive_failed_chunks);
        let start = self.sim_time;
        let end = self.sim_time + chunk;
        // Merge into the previous window when this chunk is contiguous with it (a
        // back-to-back failure). The tolerance is a small fraction of the chunk so
        // ordinary float drift in `sim_time` accumulation does not split a run.
        match self.failed_windows.last_mut() {
            Some(prev) if (start - prev.1).abs() <= chunk * 1e-6 => prev.1 = end,
            _ => self.failed_windows.push((start, end)),
        }
    }

    /// Force a net's voltage to `volts` AFTER each analog solve (until cleared),
    /// unconditionally (binary "always high"). Returns false if the net is absent.
    pub fn force_net_voltage(&mut self, net: &str, volts: f64) -> bool {
        self.force_net_voltage_windowed(net, volts, 0.0, f64::NEG_INFINITY, f64::INFINITY)
    }

    /// Force a net to `high_volts` while `t_start <= sim_time < t_end`, else
    /// `low_volts`. Returns false if the net does not exist. Used to drive an
    /// output SPIKE net HIGH for a sim-time window proportional to its decomposed
    /// spike count (rate-coded firmware-driven inference).
    pub fn force_net_voltage_windowed(
        &mut self,
        net: &str,
        high_volts: f64,
        low_volts: f64,
        t_start: f64,
        t_end: f64,
    ) -> bool {
        match self.net_nodes.get(net) {
            Some(&node) => {
                self.forced_node_volts
                    .insert(node.0 as usize, (high_volts, low_volts, t_start, t_end));
                // Reflect immediately so a same-chunk latch sample sees it too.
                let t = self.sim_time;
                let v = if t >= t_start && t < t_end {
                    high_volts
                } else {
                    low_volts
                };
                if let Some(slot) = self.node_volts.get_mut(node.0 as usize) {
                    *slot = v;
                }
                true
            }
            None => false,
        }
    }

    /// Current sim time (s).
    pub fn sim_time(&self) -> f64 {
        self.sim_time
    }

    /// Number of chunks this run whose analog transient solve failed to converge.
    /// Zero on a clean run. A non-zero count means at least one window held stale
    /// voltages and cannot vouch for analog-derived findings there (05 §3b).
    pub fn failed_chunk_count(&self) -> u64 {
        self.failed_chunks
    }

    /// The sim-time windows `[start_s, end_s)` where the analog solve failed,
    /// merged where consecutive. Empty on a clean run.
    pub fn failed_windows(&self) -> &[(f64, f64)] {
        &self.failed_windows
    }

    /// False once any chunk this run failed to solve faithfully: either the
    /// analog march diverged (held stale voltages over a window) or an MCU
    /// refused to advance (`run_micros` errored), so the digital side of that
    /// chunk never ran. Either way the co-sim cannot vouch for that window.
    /// Drives `analog_valid` in coverage and the co-sim JSON.
    pub fn analog_valid(&self) -> bool {
        self.failed_chunks == 0
    }

    /// True once the analog solve failed [`STRICT_CONSECUTIVE_FAILED_ABORT`]
    /// chunks in a row at any point this run. A strict headless run (`--strict`)
    /// or hauksbee-ci must abort (exit 3) rather than complete a fake-quiet run.
    pub fn analog_abort_tripped(&self) -> bool {
        self.max_consecutive_failed_chunks >= STRICT_CONSECUTIVE_FAILED_ABORT
    }

    /// Clear all forced node-voltage overrides.
    pub fn clear_forced_voltages(&mut self) {
        self.forced_node_volts.clear();
    }

    /// Restart the sim clock and drop every run-accumulated diagnostic so a
    /// re-run starts clean. Zeroing only `sim_time` would leave the
    /// failed-chunk count, failed-time windows, the consecutive-failure
    /// streak, the sub-microsecond clock carry, the running net stats, and the
    /// per-frame peak accumulators from the PREVIOUS run in place, so a fresh
    /// run would inherit a stale `analog_valid:false`, phantom failed windows,
    /// and a firmware clock already offset by the carry.
    /// These are all "since the run began" accumulators; a reset must clear
    /// them. Board topology, bindings, and forced overrides are left intact.
    pub fn reset_run_state(&mut self) {
        self.sim_time = 0.0;
        self.micros_carry = 0.0;
        self.failed_chunks = 0;
        self.failed_windows.clear();
        self.consecutive_failed_chunks = 0;
        self.max_consecutive_failed_chunks = 0;
        for st in self.stats.values_mut() {
            *st = Default::default();
        }
        self.frame_peak_current.clear();
        self.frame_v_extremes.clear();
        self.faults_pending.clear();
        // Run-accumulated co-sim findings: a replay must re-detect (and a
        // stale once-per-net guard would silently swallow a real re-fire).
        self.short_pulses.clear();
        self.short_pulse_nets.clear();
        self.contentions.clear();
        self.contention_nets.clear();
        // The stress monitor accumulates across a run (consecutive-over-limit
        // counters, already-raised faults, live stress, destroyed flags). Left
        // uncleared, a replay would silently drop a fault that fired last run
        // (still marked raised) or falsely early-trip one (over_chunks already
        // near the sustain threshold). Clear the tracks, and in destructive mode
        // restore the circuit the monitor damaged so the replay solves the same
        // pristine topology it did the first time.
        self.stress.reset_tracks();
        if self.stress.destructive {
            self.circuit = self.original_circuit.clone();
        }
    }

    fn update_stats(&mut self) {
        // Rail-relative logic thresholds for toggle counting. A fixed 3.0/2.0 V
        // band (a 2.5 V midpoint) is a 5 V-rail assumption: on a 3.3 V board
        // (every renode/qemu external MCU returns logic_high 3.3) a loaded high
        // output settles below 3.0 V through the driver's series R, so every high
        // sample lands in the hysteresis band, `last_logic` never establishes a
        // level, and `toggles` stays 0; the net reads as inactive in the activity
        // table and CI min_toggles/freq asserts fail falsely. Scale the band by
        // the board's logic-high rail (0.6/0.4·Vhigh = the original 3.0/2.0 at 5 V,
        // unchanged there), mirroring the rail-relative digital-IN thresholds.
        // Use the LOWEST MCU rail on the board, not the highest: a single global
        // band cannot be per-net, so on a mixed-rail board (a 5 V AVR + a 3.3 V
        // ESP32) the max rail (5.0 → vih 3.0) reintroduces the exact undercount the
        // rail scaling fixes, a 3.3 V net's loaded high (~2.8 V) sits in the
        // hysteresis band and never toggles. The min rail (3.3 → vih ~2.0) counts
        // toggles on BOTH domains: a clean 5 V push-pull edge still crosses the
        // lower threshold cleanly (5 V logic has no stable plateau in [vil,vih]),
        // and single-rail boards are unaffected (min == max == the one rail).
        let vhigh = {
            let m = self
                .mcus
                .iter()
                .map(|m| m.logic_high_v)
                .filter(|v| *v > 0.0)
                .fold(f64::INFINITY, f64::min);
            if m.is_finite() {
                m
            } else {
                5.0
            }
        };
        let (vih, vil) = (0.6 * vhigh, 0.4 * vhigh);
        for (name, &node) in &self.net_nodes {
            let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
            let st = self.stats.entry(name.clone()).or_default();
            st.min_v = st.min_v.min(v);
            st.max_v = st.max_v.max(v);
            // Logic level with a rail-scaled midpoint and hysteresis band.
            let logic = if v > vih {
                Some(true)
            } else if v < vil {
                Some(false)
            } else {
                st.last_logic
            };
            if let (Some(prev), Some(now)) = (st.last_logic, logic) {
                if prev != now {
                    st.toggles += 1;
                }
            }
            st.last_logic = logic;
        }
    }

    /// Fold this chunk's settled operating point into the per-frame extreme
    /// accumulators (peak device current, per-net voltage min/max). Called once
    /// per converged chunk so a mid-frame spike that has subsided by the frame's
    /// last chunk is still captured. The device-current formula mirrors the
    /// stress monitor's resistor/diode current so the peaks agree with what the
    /// fault checks see.
    fn accumulate_frame_peaks(&mut self) {
        let volts = &self.node_volts;
        let v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        for dev in &self.circuit.devices {
            let (name, i) = match dev {
                Device::Resistor {
                    name, a, b, ohms, ..
                } => {
                    let i = if *ohms > 0.0 {
                        ((v(*a) - v(*b)) / *ohms).abs()
                    } else {
                        0.0
                    };
                    (name, i)
                }
                Device::Diode { name, a, k, model } => {
                    let vt = hauksbee_ir::thermal_voltage_c(self.circuit.temp_c) * model.n;
                    let i = if vt > 0.0 {
                        (model.is_at(self.circuit.temp_c)
                            * (((v(*a) - v(*k)) / vt).clamp(-100.0, 200.0).exp() - 1.0))
                            .abs()
                    } else {
                        0.0
                    };
                    (name, i)
                }
                _ => continue,
            };
            if i.is_finite() {
                let e = self.frame_peak_current.entry(name.clone()).or_insert(0.0);
                if i > *e {
                    *e = i;
                }
            }
        }
        for (name, &node) in &self.net_nodes {
            let x = v(node);
            let e = self
                .frame_v_extremes
                .entry(name.clone())
                .or_insert((f64::INFINITY, f64::NEG_INFINITY));
            e.0 = e.0.min(x);
            e.1 = e.1.max(x);
        }
    }

    /// Per-device peak |current| (A) over the last completed frame's sub-chunks,
    /// keyed by reference designator. Captures intra-frame surges the final-chunk
    /// snapshot misses.
    pub fn frame_peak_current(&self) -> &HashMap<String, f64> {
        &self.frame_peak_current
    }

    /// Per-net (min_v, max_v) over the last completed frame's sub-chunks.
    pub fn frame_v_extremes(&self) -> &HashMap<String, (f64, f64)> {
        &self.frame_v_extremes
    }

    /// Inject serial bytes into a named MCU's UART RX.
    pub fn serial(&mut self, mcu_ref: &str, data: &[u8]) {
        for m in &mut self.mcus {
            if m.binding.reference == mcu_ref || mcu_ref.is_empty() {
                m.core.uart_write(data);
            }
        }
    }

    /// Override a named input source (any Vsource/Isource by device name).
    pub fn set_input(&mut self, source: &str, value: f64) {
        for dev in self.circuit.devices.iter_mut() {
            match dev {
                Device::Vsource { name, kind, .. } | Device::Isource { name, kind, .. }
                    if name == source =>
                {
                    *kind = hauksbee_ir::SourceKind::Dc(value);
                }
                _ => {}
            }
        }
    }

    /// Current voltage of a net by name.
    /// Reconcile one MCU's GPIO Thevenin drivers with the configured-output
    /// pin set its core reported at the end of a chunk (both halves of the
    /// dynamic promotion):
    ///
    /// * **Promotion**, a pin the firmware set as an OUTPUT but has so far
    ///   only held at its reset level (DDR write, no PORT toggle, e.g. an
    ///   active-low enable held low from boot) emits no pin-change edge, so
    ///   the edge loop never enables its driver and the net would float.
    ///   Enable such drivers at the pin's last known level (default low, the
    ///   AVR reset PORT state).
    /// * **Release**, a pin that dropped OUT of the set (DDR output→input,
    ///   e.g. an open-drain bus hand-off) gets its driver DISABLED again, so
    ///   the net is genuinely let go instead of staying clamped at its stale
    ///   driven level (the latched-bus failure). Only pins the core itself
    ///   previously reported as outputs are released: an edge-enabled driver
    ///   on a direction-blind backend (empty set both chunks) is never torn
    ///   down.
    fn sync_configured_outputs(
        &mut self,
        mi: usize,
        configured: std::collections::HashSet<(char, u8)>,
    ) {
        let m = &mut self.mcus[mi];
        for &(port, bit) in &configured {
            if let Some(drv) = m.binding.gpio_drivers.get_mut(&(port, bit)) {
                if !drv.enabled {
                    drv.set_enabled(&mut self.circuit, true);
                    let level = m.last_levels.get(&(port, bit)).copied().unwrap_or(false);
                    let v = if level { m.logic_high_v } else { 0.0 };
                    drv.set_volts(&mut self.circuit, v);
                }
            }
        }
        for &(port, bit) in m.configured_outputs.difference(&configured) {
            if let Some(drv) = m.binding.gpio_drivers.get_mut(&(port, bit)) {
                if drv.enabled {
                    drv.set_enabled(&mut self.circuit, false);
                }
            }
        }
        m.configured_outputs = configured;
    }

    pub fn net_voltage(&self, net: &str) -> Option<f64> {
        let node = self.net_nodes.get(net)?;
        self.node_volts.get(node.0 as usize).copied()
    }

    /// The name of the net on `node`, or `"node N"` for an unnamed one.
    fn net_name_of(&self, node: u32) -> String {
        self.net_nodes
            .iter()
            .find(|(_, n)| n.0 == node)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| format!("node {node}"))
    }

    /// Friction 1.16 detector: scan the chunk's drained cycle-stamped edge log
    /// for a pin that completed a pulse (two consecutive opposite-level
    /// transitions) INSIDE this chunk, driving a net that clocks a
    /// tick-evaluated sequential part. Containment in one chunk is exactly the
    /// invisible case: the once-per-chunk tick samples the settled end-of-chunk
    /// voltage, so a pulse that has already returned to its resting level is
    /// never seen (a pulse spanning a chunk boundary IS seen at the boundary
    /// sample, and correctly does not warn). Warns once per net per run.
    ///
    /// The pulse width is the cycle gap between the two transitions,
    /// normalised over the chunk's cycle span and scaled by the chunk
    /// duration; coarse (but still contained, hence still correct to warn) on
    /// poll backends whose stamps are not cycle-exact.
    fn detect_short_pulses(&mut self, chunk: f64) {
        if self.tick_sequential_nets.is_empty() {
            return;
        }
        let mut found: Vec<ShortPulse> = Vec::new();
        for (mi, ce) in self.last_chunk_edges.iter().enumerate() {
            let (c0, c1) = ce.cycle_span;
            if c1 <= c0 {
                continue;
            }
            let span = (c1 - c0) as f64;
            // Sorted pin order so the once-per-net record is deterministic
            // when several pins share a net (HashMap iteration is not).
            let mut pins: Vec<&(char, u8)> = ce.edges.keys().collect();
            pins.sort();
            for pin in pins {
                let transitions = &ce.edges[pin];
                if transitions.len() < 2 {
                    continue;
                }
                let Some(drv) = self.mcus[mi].binding.gpio_drivers.get(pin) else {
                    continue;
                };
                let node = drv.net.0;
                if self.short_pulse_nets.contains(&node) {
                    continue;
                }
                let Some(parts) = self.tick_sequential_nets.get(&node) else {
                    continue;
                };
                // Narrowest completed pulse: the smallest cycle gap between
                // consecutive opposite-level transitions.
                let mut width: Option<f64> = None;
                for w in transitions.windows(2) {
                    if w[0].1 == w[1].1 {
                        continue;
                    }
                    let g = (w[1].0.saturating_sub(w[0].0)) as f64 / span * chunk;
                    if width.map_or(true, |cur| g < cur) {
                        width = Some(g);
                    }
                }
                let Some(pulse_s) = width else { continue };
                self.short_pulse_nets.insert(node);
                found.push(ShortPulse {
                    net: self.net_name_of(node),
                    mcu_ref: ce.mcu_reference.clone(),
                    port: pin.0,
                    bit: pin.1,
                    pulse_s,
                    chunk_s: chunk,
                    parts: parts.clone(),
                });
            }
        }
        for p in found {
            eprintln!("WARNING: {}", p.message());
            self.short_pulses.push(p);
        }
    }

    /// Runtime driver-contention monitor: report every net where an ENABLED
    /// MCU GPIO driver (the firmware configured the pin as an output, seen via
    /// pin-change edges or the `pins_configured_output` DDR sync) coexists
    /// with an ENABLED modelled push-pull output driver of a digital part.
    /// Fires once per net per run.
    ///
    /// Classification is shared with the static lint
    /// ([`crate::checks::contention`]) by construction: the model drivers
    /// scanned here are exactly the [`crate::drivers::PinDriver`]s the binder
    /// stamped from [`crate::digital::output_roles`] (the static check's own
    /// single source of what counts as an output), and a driver's live
    /// `enabled` flag is set by the same `[models.logic.tristate]` groups the
    /// static check expands for its tri-state exclusion. So a tri-stated
    /// (released) model output never fires here, an OE-driven bus in its
    /// normal one-talker-at-a-time state never fires here, and a tri-stated
    /// MCU pin (driver disabled, the binder's stamped default) never fires
    /// here.
    ///
    /// Skipped on the very first chunk (`sim_time == 0`): the model drivers'
    /// tri-state enables have not yet been evaluated against a real solve
    /// there (the zero-filled voltage seed would read an active-low OE as
    /// asserted), and firing on that fiction would be a false positive. The
    /// same honesty rule the plain digital-input sync applies.
    fn detect_driver_contention(&mut self) {
        if self.sim_time <= 0.0 || self.digital.is_empty() || self.mcus.is_empty() {
            return;
        }
        // Enabled modelled push-pull outputs per net node, as "REF.role".
        let mut model_out: HashMap<u32, Vec<String>> = HashMap::new();
        for d in &self.digital {
            for (role, drv) in &d.drivers {
                if !drv.enabled {
                    continue;
                }
                model_out
                    .entry(drv.net.0)
                    .or_default()
                    .push(format!("{}.{role}", d.reference));
            }
        }
        if model_out.is_empty() {
            return;
        }
        let mut found: Vec<DriverContention> = Vec::new();
        for m in &self.mcus {
            // Sorted pin order for a deterministic once-per-net record.
            let mut pins: Vec<&(char, u8)> = m.binding.gpio_drivers.keys().collect();
            pins.sort();
            for &(port, bit) in pins {
                let drv = &m.binding.gpio_drivers[&(port, bit)];
                if !drv.enabled {
                    continue;
                }
                let node = drv.net.0;
                if self.contention_nets.contains(&node) {
                    continue;
                }
                let Some(parts) = model_out.get(&node) else {
                    continue;
                };
                let mut parts = parts.clone();
                parts.sort();
                self.contention_nets.insert(node);
                found.push(DriverContention {
                    net: self.net_name_of(node),
                    mcu_ref: m.binding.reference.clone(),
                    port,
                    bit,
                    parts,
                    t_s: self.sim_time,
                });
            }
        }
        for c in found {
            eprintln!("WARNING: {}", c.message());
            self.contentions.push(c);
        }
    }

    /// Sub-chunk GPIO pulse warnings raised this run (friction 1.16), one per
    /// offending net, in detection order.
    pub fn short_pulses(&self) -> &[ShortPulse] {
        &self.short_pulses
    }

    /// Runtime driver-contention findings raised this run (the model-vs-MCU
    /// case the static lint cannot reach), one per offending net.
    pub fn driver_contentions(&self) -> &[DriverContention] {
        &self.contentions
    }

    /// Snapshot every net's current voltage.
    pub fn net_voltages(&self) -> HashMap<String, f64> {
        self.net_nodes
            .iter()
            .map(|(name, node)| {
                (
                    name.clone(),
                    self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0),
                )
            })
            .collect()
    }

    /// MCU GPIO control nets the firmware drives HIGH and holds from boot: a
    /// settled, non-toggling logic-high. The power-up level of such a net is
    /// decided entirely by firmware, so if one controls a load that must be OFF
    /// at power-up (a MOSFET gate, relay / motor-driver enable, igniter), an
    /// unintended HIGH is a real hazard the netlist alone cannot adjudicate.
    ///
    /// This is honest heads-up *data*, never a fault on its own (a held-high
    /// enable that *should* be high is fine). The caller frames it for the user
    /// and may further narrow to nets with no static bias resistor; the case
    /// where there is no hardware fail-safe at all.
    pub fn firmware_held_high_nets(&self) -> Vec<String> {
        let name_of = |target: NodeId| -> Option<&str> {
            self.net_nodes
                .iter()
                .find(|(_, n)| n.0 == target.0)
                .map(|(name, _)| name.as_str())
        };
        let mut out = Vec::new();
        for m in &self.mcus {
            for (role, &node) in &m.binding.role_nets {
                // Include promoted analog pins (Nano A0..A5 = PC0..PC5): bind_mcu
                // stamps a real GPIO driver on these via the same apin fallback, so
                // a firmware-driven A-pin is modelled electrically and MUST be
                // visible to the boot-hazard panel too, else a held-high enable on
                // an A-pin is silently omitted from the hazard report.
                let Some((port, bit)) = gpio_of_role(role, m.binding.module)
                    .or_else(|| apin_gpio_of_role(role, m.binding.module))
                else {
                    continue;
                };
                // The firmware's most recent (and, for a held line, final) drive.
                if m.last_levels.get(&(port, bit)) != Some(&true) {
                    continue;
                }
                let Some(name) = name_of(node) else { continue };
                let Some(st) = self.stats.get(name) else {
                    continue;
                };
                // Held HIGH: reached a clear logic-high (>= 3.0 V, the same floor
                // update_stats uses) and is not a busy signal line (<=1 rising
                // edge = driven once and held; many edges = SPI / UART / PWM / a
                // blinking LED, which are not control holds). The analog check is
                // belt-and-suspenders: last_levels already confirms the firmware
                // drove the pin high; this confirms the net physically went high.
                if st.max_v >= 3.0 && st.toggles <= 1 {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// MCU GPIO nets the firmware drove to a *defined* level during the run,
    /// either it wrote the pin (a `last_levels` entry, high or low) or it
    /// configured the pin as an output (so an output-low-held pin counts as
    /// driven LOW, not floating). A net NOT in this set was never driven by any
    /// MCU: it floats. Used by the boot-state panel to classify each gate as
    /// driven-high / driven-low / floating without enabling any circuit driver.
    pub fn firmware_driven_nets(&self) -> Vec<String> {
        let name_of = |target: NodeId| -> Option<&str> {
            self.net_nodes
                .iter()
                .find(|(_, n)| n.0 == target.0)
                .map(|(name, _)| name.as_str())
        };
        let mut out = Vec::new();
        for m in &self.mcus {
            let configured: std::collections::HashSet<(char, u8)> = m
                .core
                .pins_configured_output()
                .into_iter()
                .map(|p| (p.port, p.bit))
                .collect();
            for (role, &node) in &m.binding.role_nets {
                let Some((port, bit)) = gpio_of_role(role, m.binding.module)
                    .or_else(|| apin_gpio_of_role(role, m.binding.module))
                else {
                    continue;
                };
                if !m.last_levels.contains_key(&(port, bit)) && !configured.contains(&(port, bit)) {
                    continue;
                }
                if let Some(name) = name_of(node) {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// MCU GPIO nets whose pin the firmware actively configured as an OUTPUT
    /// (DDR set, by latest direction). A net high in this set is a strong
    /// push-pull drive; a net high but NOT in it is a weak internal pull-up.
    /// Observation metadata (from `pins_configured_output`), never a drive.
    pub fn firmware_output_configured_nets(&self) -> Vec<String> {
        let name_of = |target: NodeId| -> Option<&str> {
            self.net_nodes
                .iter()
                .find(|(_, n)| n.0 == target.0)
                .map(|(name, _)| name.as_str())
        };
        let mut out = Vec::new();
        for m in &self.mcus {
            let configured: std::collections::HashSet<(char, u8)> = m
                .core
                .pins_configured_output()
                .into_iter()
                .map(|p| (p.port, p.bit))
                .collect();
            for (role, &node) in &m.binding.role_nets {
                let Some((port, bit)) = gpio_of_role(role, m.binding.module)
                    .or_else(|| apin_gpio_of_role(role, m.binding.module))
                else {
                    continue;
                };
                if configured.contains(&(port, bit)) {
                    if let Some(name) = name_of(node) {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Last GPIO levels per MCU, for component-state frames.
    pub fn mcu_states(&self) -> HashMap<String, HashMap<String, f64>> {
        let mut out = HashMap::new();
        for m in &self.mcus {
            let mut s = HashMap::new();
            s.insert("running".to_string(), 1.0);
            for ((port, bit), level) in &m.last_levels {
                s.insert(format!("P{port}{bit}"), if *level { 1.0 } else { 0.0 });
            }
            out.insert(m.binding.reference.clone(), s);
        }
        out
    }

    /// Generalized digital edge replay (05 §1.2): drain MCU `mi`'s ordered,
    /// cycle-stamped log and replay it in cycle order through every edge-driven
    /// digital element on ONE path; the 595 chains it owns AND any standalone
    /// GPIO-clocked shift/latch (`replay_chips`). One micro-tick per edge-group
    /// sharing a cycle. Returns the micro-tick count.
    ///
    /// One mechanism covers every edge-driven chip. The 595 chain
    /// stays byte-exact: replaying a cycle-group sub-slice evolves the chain's
    /// state identically to replaying the whole log (replay is a stateful
    /// sequential fold), so PATH B still latches its bytes exactly. The 74HC165
    /// read path is deliberately NOT here: it resolves synchronously inside
    /// `run_micros` through the input responder; this post-run replay reconciles
    /// only the write side.
    fn replay_digital_edges(&mut self, mi: usize, edge_log: &[PinEdge]) -> usize {
        if edge_log.is_empty() {
            return 0;
        }
        // Generic standalone components first, in cycle order, sharing the same
        // overlay semantics as the chains. No-op unless `replay_chips` is
        // populated (empty on the current corpus). Its own return is the
        // authoritative micro-tick count for those parts.
        let generic_ticks = if !self.replay_chips.is_empty() {
            if let Some(pin_nets) = self.replay_pin_nets.get(mi).cloned() {
                let high_v = self.mcus[mi].logic_high_v;
                let base = self.node_volts.clone();
                crate::digital::replay_components_on_edges(
                    &mut self.digital,
                    &self.replay_chips,
                    &pin_nets,
                    edge_log,
                    &base,
                    high_v,
                    0.0,
                    &mut self.circuit,
                )
            } else {
                0
            }
        } else {
            0
        };

        // Chains owned by this MCU, replayed cycle-group by cycle-group so the
        // ordering matches the generic path. The log is pushed in cycle order, so
        // equal cycles are contiguous: one group per distinct cycle.
        let mut group_ticks = 0usize;
        let mut i = 0;
        while i < edge_log.len() {
            let c = edge_log[i].cycle;
            let mut j = i;
            while j < edge_log.len() && edge_log[j].cycle == c {
                j += 1;
            }
            if !self.chains.is_empty() {
                let raw: Vec<(char, u8, bool)> = edge_log[i..j]
                    .iter()
                    .map(|e| (e.port, e.bit, e.level))
                    .collect();
                for (ci, chain) in self.chains.iter_mut().enumerate() {
                    if self.chain_mcu[ci] == mi {
                        chain.replay(&raw);
                    }
                }
            }
            group_ticks += 1;
            i = j;
        }
        // Prefer the generic count where a standalone part drove the replay
        // (it counts only cycle-groups that touched a watched pin); otherwise the
        // cycle-group count over the whole log is the chain replay's micro-ticks.
        if generic_ticks > 0 {
            generic_ticks
        } else {
            group_ticks
        }
    }

    /// The cycle-stamped GPIO edges each MCU produced in the most recent chunk,
    /// one entry per MCU that ran. The analog PWL side consumes this to translate
    /// each pin's ordered `(cycle, level)` series into a `SourceKind::Pwl`
    /// waveform on the driven net (05 §1.1/§1.3).
    /// The PWL edge drive of 05 section 1.3, and its policy.
    ///
    /// Eligibility, which IS the cadence negotiation: a pin gets a PWL drive
    /// only when (a) it toggled at least twice this chunk (a single edge is
    /// exactly represented by the final-level DC the driver path already
    /// applied), (b) its driver is enabled, and (c) its net feeds at least one
    /// device beyond the driver's own Thevenin pair (a pin wired to nothing
    /// analog pays nothing). Pins failing any test keep the cheap DC path.
    ///
    /// Coarse-stamped backends (cycle_exact == false) still get the drive:
    /// their per-poll ordering is preserved even though intra-poll spacing is
    /// approximate, and an approximately-timed pulse train integrates far
    /// closer to the truth than a collapsed level. The per-pin point cap
    /// guards against a pathological chunk (an edge storm beyond what any
    /// real bit-bang produces) blowing up the solve; beyond it the pin falls
    /// back to the DC level for this chunk, which is the pre-PWL behavior.
    fn apply_pwl_drives(&mut self, chunk: f64) -> Vec<(usize, f64)> {
        use hauksbee_ir::{PwlPoint, SourceKind};
        // Two PWL corners per transition plus the anchors; 10k transitions
        // per pin per chunk is far beyond any firmware bit-bang (a 720-clock
        // shiftOut is 1,440).
        const MAX_TRANSITIONS_PER_PIN: usize = 10_000;

        // Consumer count per net, beyond a driver's own Thevenin pair. Built
        // once per chunk, only if some pin actually multi-toggled.
        let mut consumers: Option<std::collections::HashMap<u32, usize>> = None;

        let mut restores = Vec::new();
        for (mi, ce) in self.last_chunk_edges.iter().enumerate() {
            let high_v = self.mcus[mi].logic_high_v;
            let (c0, c1) = ce.cycle_span;
            if c1 <= c0 {
                continue;
            }
            let span = (c1 - c0) as f64;
            for (pin, transitions) in &ce.edges {
                if transitions.len() < 2 || transitions.len() > MAX_TRANSITIONS_PER_PIN {
                    continue;
                }
                let Some(drv) = self.mcus[mi].binding.gpio_drivers.get(pin) else {
                    continue;
                };
                if !drv.enabled {
                    continue;
                }
                let net = drv.net;
                let counts = consumers.get_or_insert_with(|| {
                    let mut m = std::collections::HashMap::new();
                    for (_, dev) in self.circuit.iter() {
                        for n in dev.nodes() {
                            *m.entry(n.0).or_insert(0usize) += 1;
                        }
                    }
                    m
                });
                // The driver's own resistor touches the net once; anything
                // more means real circuitry hangs off this pin.
                if counts.get(&net.0).copied().unwrap_or(0) <= 1 {
                    continue;
                }

                // Build the waveform. Level BEFORE the first transition is
                // the complement of what that transition set.
                let level_v = |lv: bool| if lv { high_v } else { 0.0 };
                let t_of = |cycle: u64| {
                    ((cycle.saturating_sub(c0)) as f64 / span * chunk).clamp(0.0, chunk)
                };
                // Slew per corner: a tenth of the tightest edge spacing,
                // capped at 10 ns. Real drivers slew in ns; the exact figure
                // only needs to be shorter than anything the load can resolve
                // while keeping the PWL times strictly increasing.
                let mut min_gap = chunk;
                for w in transitions.windows(2) {
                    let g = t_of(w[1].0) - t_of(w[0].0);
                    if g > 0.0 && g < min_gap {
                        min_gap = g;
                    }
                }
                let t_edge = (min_gap / 10.0).min(10e-9).max(1e-12);

                let mut pts = Vec::with_capacity(transitions.len() * 2 + 2);
                let v_init = level_v(!transitions[0].1);
                pts.push(PwlPoint { t: 0.0, v: v_init });
                let mut prev_v = v_init;
                let mut last_t = 0.0f64;
                for &(cycle, lv) in transitions {
                    let mut tk = t_of(cycle);
                    // Strictly increasing times even under coarse stamps that
                    // collide: nudge past the previous corner.
                    if tk <= last_t {
                        tk = last_t + t_edge;
                    }
                    let v_new = level_v(lv);
                    pts.push(PwlPoint { t: tk, v: prev_v });
                    pts.push(PwlPoint {
                        t: tk + t_edge,
                        v: v_new,
                    });
                    prev_v = v_new;
                    last_t = tk + t_edge;
                }
                if last_t < chunk {
                    pts.push(PwlPoint {
                        t: chunk,
                        v: prev_v,
                    });
                }

                let vs = drv.vsource.0 as usize;
                if let Some(hauksbee_ir::Device::Vsource { kind, .. }) =
                    self.circuit.devices.get_mut(vs)
                {
                    *kind = SourceKind::Pwl(pts);
                    restores.push((vs, prev_v));
                }
            }
        }
        restores
    }

    /// Settle every PWL-driven source back to its final DC level after the
    /// chunk's solve, so the digital tick, the stats fold, and the next
    /// chunk's warm start all see the level the pin actually rests at.
    fn restore_pwl_drives(&mut self, restores: &[(usize, f64)]) {
        use hauksbee_ir::SourceKind;
        for &(vs, v) in restores {
            if let Some(hauksbee_ir::Device::Vsource { kind, .. }) =
                self.circuit.devices.get_mut(vs)
            {
                *kind = SourceKind::Dc(v);
            }
        }
    }

    pub fn last_chunk_pin_edges(&self) -> &[ChunkPinEdges] {
        &self.last_chunk_edges
    }

    /// Micro-ticks replayed through the generalized digital path in the last
    /// chunk (one per distinct edge-group cycle). A diagnostic that a bit-banged
    /// burst produced N ordered micro-ticks rather than one collapsed level.
    pub fn last_replay_microticks(&self) -> usize {
        self.last_replay_microticks
    }

    /// Per edge-driven 74HC595 chain, the MCU GPIO `(port, bit)` bound to its
    /// SRCLK, RCLK, optional SRCLR_n, optional OE_n, and head SER, plus the chip
    /// count. Empty when no chain is clocked by the MCU (the once-per-chunk
    /// path handles it). Exposed for diagnostics and co-sim tests of the chain
    /// wiring (FIX 1).
    #[allow(clippy::type_complexity)]
    pub fn hc595_chain_pins(
        &self,
    ) -> Vec<(
        (char, u8),
        (char, u8),
        Option<(char, u8)>,
        Option<(char, u8)>,
        (char, u8),
        usize,
    )> {
        self.chains
            .iter()
            .map(|c| (c.srclk, c.rclk, c.srclr_n, c.oe_n, c.ser, c.order.len()))
            .collect()
    }

    /// Per edge-driven 74HC165 read chain, the MCU GPIO `(port,bit)` bound to
    /// its PL, CLK, and MISO input, plus chip count. Empty when no MCU-clocked
    /// 165 chain was identified. For diagnostics and read-chain co-sim tests.
    #[allow(clippy::type_complexity)]
    pub fn hc165_chain_pins(&self) -> Vec<((char, u8), (char, u8), (char, u8), usize)> {
        self.hc165_chains
            .iter()
            .map(|c| {
                let c = c.lock().unwrap_or_else(|e| e.into_inner());
                (c.pl_n, c.clk, c.miso, c.order.len())
            })
            .collect()
    }

    /// The last word each 74HC165 read chain captured on its most recent PL
    /// load (MSB-first, as the firmware accumulates it). For diagnostics/tests.
    pub fn hc165_loaded_words(&self) -> Vec<u16> {
        self.hc165_chains
            .iter()
            .map(|c| c.lock().unwrap_or_else(|e| e.into_inner()).loaded_word())
            .collect()
    }

    /// Digital component register states, for component-state frames.
    pub fn digital_states(&self) -> HashMap<String, HashMap<String, f64>> {
        self.digital
            .iter()
            .map(|d| (d.reference.clone(), d.state_summary()))
            .collect()
    }

    /// Drain the faults raised since the last call (for SimFrame).
    pub fn drain_faults(&mut self) -> Vec<FaultEvent> {
        std::mem::take(&mut self.faults_pending)
    }

    /// Enable or disable destructive faulting (mutate the circuit on fault).
    pub fn set_destructive_faults(&mut self, on: bool) {
        self.stress.destructive = on;
    }

    /// Configure the power supply on a named supply net. Returns true if a
    /// supply leg for that net existed and was reconfigured.
    pub fn set_power_supply(&mut self, net: &str, supply: PowerSupply) -> bool {
        for s in &mut self.supplies {
            if s.net_name == net {
                s.reconfigure(&mut self.circuit, supply);
                return true;
            }
        }
        false
    }

    /// Names of the configurable supply nets, in stable order.
    pub fn supply_nets(&self) -> Vec<String> {
        self.supplies.iter().map(|s| s.net_name.clone()).collect()
    }

    /// Live supply readout per net: (kind label, last rail current A, SoC).
    pub fn supply_states(&self) -> HashMap<String, (String, f64, f64)> {
        self.supplies
            .iter()
            .map(|s| {
                (
                    s.net_name.clone(),
                    (
                        s.supply.kind_label().to_string(),
                        s.last_current_a,
                        s.supply.soc(),
                    ),
                )
            })
            .collect()
    }

    /// Live per-component stress fraction (0..1) for heat-mapping.
    pub fn stress_states(&self) -> HashMap<String, f64> {
        self.stress.stress_by_ref().clone()
    }

    /// Live per-component estimated junction temperature (C) for the thermal
    /// view. Only populated for dissipating devices.
    pub fn temp_states(&self) -> HashMap<String, f64> {
        self.stress.temp_by_ref().clone()
    }

    /// Set the ambient temperature (C) the thermal monitor's steady-state
    /// junction estimate sits on top of.
    pub fn set_ambient_c(&mut self, ambient_c: f64) {
        self.stress.ambient_c = ambient_c;
    }

    /// The configured ambient temperature (C).
    pub fn ambient_c(&self) -> f64 {
        self.stress.ambient_c
    }

    /// Short two nets by name, bridging them with a small resistance so the
    /// solver carries current between them and the stress monitor shows the
    /// fallout. The what-if "solder bridge" API. Returns true if the bridge was
    /// stamped (both nets exist, are distinct, and were not already bridged).
    ///
    /// A short to ground (one net is GND) bridges the live net straight to the
    /// ground reference, exactly the destructive case worth simulating.
    pub fn short_nets(&mut self, net_a: &str, net_b: &str) -> bool {
        let node_a = self.net_nodes.get(net_a).copied();
        let node_b = self.net_nodes.get(net_b).copied();
        let (Some(a), Some(b)) = (node_a, node_b) else {
            return false;
        };
        let Some(_name) = crate::shorts::stamp_bridge(&mut self.circuit, a, b, net_a, net_b) else {
            return false;
        };
        // The new device may add a branch unknown; rebuild the MNA layout and
        // resize the branch-current buffer so subsequent solves are consistent.
        self.relayout();
        self.faults_pending
            .push(crate::shorts::short_fault(net_a, net_b, self.sim_time));
        true
    }

    /// Apply every true overlap a DRC report found, bridging each shorted net
    /// pair. Clearance-only violations are not applied (they are near-short
    /// risks, not actual shorts). Returns the number of bridges stamped.
    pub fn apply_drc_shorts(&mut self, report: &hauksbee_extract::DrcReport) -> usize {
        let pairs = crate::shorts::shorted_name_pairs(report);
        let mut applied = 0;
        for (a, b) in pairs {
            if self.short_nets(&a, &b) {
                applied += 1;
            }
        }
        applied
    }

    /// Rebuild the frozen MNA layout from the current circuit and resize the
    /// node/branch state buffers to match. Called after structural edits (a
    /// stamped short bridge) so the solver and fault monitor stay consistent.
    fn relayout(&mut self) {
        self.layout = Layout::new(&self.circuit);
        let n_nodes = self.circuit.node_count();
        self.node_volts.resize(n_nodes, 0.0);
        let n_branch = self.layout.size.saturating_sub(self.layout.n_nodes);
        self.branch_x.resize(n_branch, 0.0);
        // Keep the 165 read-chain voltage snapshot sized to the node count.
        {
            let mut snap = self.input_volts.lock().unwrap_or_else(|e| e.into_inner());
            snap.resize(n_nodes, 0.0);
        }
        // The unknown vector changed shape; a prior warm seed no longer applies.
        self.last_dc_seed = None;
        // New devices may touch input-pin nets (peripheral controls, buttons):
        // refresh the plain digital-input sync's drive-evidence index.
        self.rebuild_digital_in_evidence();
    }
}

/// What one `step` produced (beyond the in-place voltage/stat updates).
pub struct StepResult {
    pub sim_time: f64,
    pub uart: HashMap<String, Vec<u8>>,
}

/// Instantiate an MCU core for a binding and load firmware if given.
///
/// The backend string (from the model db) selects the emulator:
///   - `simavr:<part>`  -> in-process AVR via libsimavr.
///   - `renode:<part>`  -> external headless Renode (STM32 / nRF52 / RISC-V).
///   - `qemu:<part>`    -> external Espressif QEMU (ESP32 / ESP32-S3 / ESP32-C3).
///
/// A `renode:` / `qemu:` backend on a build without the matching feature, or on
/// a host without the emulator installed, is a clear error rather than a silent
/// AVR fallback (that would run the wrong firmware against the circuit).
///
/// `renode:<part>` / `qemu:<part>` configs come from the SoC-descriptor
/// resolution path (`SocConfig::resolve`): `$HAUKSBEE_MCU_DIR` →
/// `~/.config/hauksbee/mcu` → the embedded builtin, so a user descriptor can
/// add a new part, or override a builtin, purely as data (06 §6.4).
///
/// For the QEMU backend the firmware path is the merged flash image, which QEMU
/// boots from at spawn; there is no separate load step (the trait's
/// `load_firmware` is a no-op for QEMU).
/// Route one SPI byte to the correct bus among all attached slaves (05 §2.3).
///
/// A single bus is always the target; this is the single-slave path and it must
/// stay byte-for-byte identical to the pre-multiplexing behaviour, even before
/// the bus has seen its first CS edge. With two or more buses, dispatch to the
/// first bus whose CS is currently asserted (`is_selected`); if none is selected
/// (all deasserted between transactions) the bus is idle and MISO floats high
/// (`0xFF`). At most one bus lock is held at a time, preserving the
/// McuShared→SpiBus lock order.
fn dispatch_spi(
    buses: &[std::sync::Arc<std::sync::Mutex<crate::peripherals::SpiBus>>],
    ev: hauksbee_mcu::SpiEvent,
) -> u8 {
    let apply = |bus: &std::sync::Arc<std::sync::Mutex<crate::peripherals::SpiBus>>| -> u8 {
        let mut g = bus.lock().unwrap_or_else(|e| e.into_inner());
        if ev.deselect {
            // A backend-surfaced CS deassert (Renode hardware-NSS
            // FinishTransmission): the backend frames CS itself, so record that
            // (coverage reports `backend`) and end the transaction.
            g.note_backend_deselect();
            0xFF
        } else {
            g.transfer(ev.mosi)
        }
    };
    if buses.len() == 1 {
        return apply(&buses[0]);
    }
    for b in buses {
        if b.lock().unwrap_or_else(|e| e.into_inner()).is_selected() {
            return apply(b);
        }
    }
    0xFF
}

fn instantiate_mcu(
    binding: &McuBinding,
    firmware: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    let backend = binding.backend.as_str();

    // Backstop: validate the path before it reaches ANY native loader (simavr
    // segfaults on a missing file; QEMU spawns from the flash image inside
    // instantiate_qemu, so the check must sit above the backend dispatch, and
    // it also closes QEMU's directory-path edge that a bare exists() check
    // lets through). Higher entry points (CLI, CI spec runner) validate
    // earlier with richer provenance; this guards any library caller that
    // reaches the scheduler directly.
    if let Some(fw) = firmware {
        hauksbee_mcu::validate_firmware_path(fw)?;
    }

    let mut core: Box<dyn Mcu + Send> = if let Some(part) = backend.strip_prefix("renode:") {
        instantiate_renode(part)?
    } else if let Some(part) = backend.strip_prefix("qemu:") {
        instantiate_qemu(part, firmware)?
    } else if let Some(family) = backend.strip_prefix("none:") {
        // The binder recognized the MCU family but knows no emulator models
        // it (e.g. ESP32-S2). Refuse rather than run the firmware on a
        // wrong-ISA core: everything the co-sim would report about the
        // circuit would be fiction.
        anyhow::bail!(
            "this board's MCU was recognized as {family}, which has no co-sim \
             platform (no supported emulator models it); firmware cannot run. \
             Firmware-less analyses (lint, report, DRC) still work. To force a \
             backend, override the part with a --models-dir entry that sets \
             `backend` explicitly."
        )
    } else if backend.starts_with("simavr:") {
        instantiate_avr(backend)?
    } else {
        // ALLOWLIST, mirroring backend_is_external: an unknown backend token
        // must fail loud, never drift into the AVR path (wrong ISA).
        anyhow::bail!(
            "unknown MCU backend '{backend}': expected 'simavr:<part>', \
             'renode:<part>', or 'qemu:<part>'"
        )
    };
    if let Some(fw) = firmware {
        core.load_firmware(fw)?;
    }
    Ok(core)
}

/// Build an Espressif-QEMU-backed core for a `qemu:<part>` backend string. The
/// firmware path is the merged flash image and is required (QEMU boots from it).
#[cfg(feature = "qemu")]
fn instantiate_qemu(
    part: &str,
    firmware: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    use hauksbee_mcu::QemuBackend;
    let config = resolve_qemu_config(part)?;
    let flash = firmware.ok_or_else(|| {
        anyhow::anyhow!(
            "the qemu:{part} backend needs a merged flash image as the firmware \
             path (build it with esp-idf + esptool merge_bin)"
        )
    })?;
    Ok(Box::new(QemuBackend::new(config, flash)?))
}

/// Resolve a `qemu:<part>` token to a `QemuConfig` through the descriptor
/// path, symmetric with [`resolve_renode_config`]: override dirs beat the
/// embedded builtin, an invalid override fails loudly with its named
/// validation error, and an unknown part's error enumerates the dirs
/// searched. The QEMU backend never had alias tokens, so there is no
/// canonical-part fallback.
#[cfg(feature = "qemu")]
fn resolve_qemu_config(part: &str) -> anyhow::Result<hauksbee_mcu::QemuConfig> {
    use hauksbee_mcu::SocConfig;
    let spec = format!("qemu:{part}");
    let resolved = SocConfig::resolve(&spec)
        .map_err(|e| anyhow::anyhow!("resolving MCU descriptor for '{spec}': {e}"))?;
    match resolved {
        SocConfig::Qemu(config) => Ok(config),
        // Unreachable: resolve() validates the declared backend against the
        // spec's `qemu:` half. Kept as a loud backstop.
        #[cfg(feature = "renode")]
        SocConfig::Renode(_) => anyhow::bail!(
            "descriptor for '{spec}' declares backend \"renode\" but was requested as qemu"
        ),
    }
}

#[cfg(not(feature = "qemu"))]
fn instantiate_qemu(
    _part: &str,
    _firmware: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    anyhow::bail!(
        "this build of hauksbee-engine was compiled without the `qemu` feature; \
         rebuild with --features qemu to run ESP32 firmware"
    )
}

/// Whether a backend string names an EXTERNAL co-sim core (Renode, QEMU, or
/// anything future) as opposed to the one full-stack in-process backend
/// (simavr), which models peripheral-slave coupling and exposes pin drive
/// direction. ALLOWLIST on purpose: an unknown future backend must fail SAFE
/// (classified external: capability-gated scaffolds, hedged diagnoses)
/// rather than inherit capabilities it does not have. Both hauksbee-ci's
/// scaffold gate and the runtime diagnosis route through this one predicate
/// so the two cannot drift (review finding on the ARM-honesty commit).
pub fn backend_is_external(backend: &str) -> bool {
    !backend.starts_with("simavr")
}

/// Build a simavr-backed AVR core for a `simavr:<part>` backend string.
#[cfg(feature = "avr")]
fn instantiate_avr(backend: &str) -> anyhow::Result<Box<dyn Mcu + Send>> {
    let mut avr = if backend.contains("atmega328p") {
        AvrMcu::atmega328p_16mhz()?
    } else {
        // Unknown simavr backend: fall back to atmega328p so the co-sim runs.
        AvrMcu::new("atmega328p", 16_000_000)?
    };
    avr.register_port_hooks(&['B', 'C', 'D']);
    Ok(Box::new(avr))
}

#[cfg(not(feature = "avr"))]
fn instantiate_avr(_backend: &str) -> anyhow::Result<Box<dyn Mcu + Send>> {
    anyhow::bail!(
        "this build of hauksbee-engine was compiled without the `avr` feature; \
         rebuild with --features avr to run AVR firmware"
    )
}

/// Detect whether the modelled core is less specific than the part the board
/// asked for. Returns `Some(McuSubstitution)` only when (a) we recognise the
/// backend's modelled core, (b) the board gave a non-empty requested part, and
/// (c) the requested part normalises to something OTHER than the modelled core.
/// Conservative by design: an unknown/empty requested part or an exact match
/// yields `None`, so a vanilla `STM32F407` board never spuriously warns.
fn detect_substitution(binding: &McuBinding) -> Option<McuSubstitution> {
    let requested = binding.requested_part.trim();
    if requested.is_empty() {
        return None;
    }
    // The canonical core each renode backend actually loads. The first element
    // is the normalised identity of the modelled part; the second is the human
    // label to print.
    let modelled: (&str, &str) = match binding.backend.as_str() {
        // The stm32f4 / stm32f4_discovery backend always loads the F407 Discovery
        // core, regardless of which F4 variant the board specified.
        "renode:stm32f4" | "renode:stm32f4_discovery" => ("STM32F407", "STM32F407"),
        "renode:stm32f103" => ("STM32F103", "STM32F103"),
        "renode:nrf52840" | "renode:nrf52" => ("NRF52840", "nRF52840"),
        "renode:sifive_fe310" | "renode:fe310" => ("FE310", "SiFive FE310"),
        // simavr / qemu backends model the requested part directly; no
        // family-collapse substitution applies.
        _ => return None,
    };

    let norm = normalise_part(requested);
    // The board asked for the exact modelled core (possibly with a package/temp
    // suffix, e.g. STM32F407VGT6): not a substitution.
    if norm.starts_with(modelled.0) {
        return None;
    }
    // Only warn when the requested part looks like the SAME family but a
    // different member (e.g. STM32F411 vs STM32F407). A requested part that does
    // not share the modelled family's stem is a binding the router should not
    // have produced; do not invent a substitution narrative for it.
    let family_stem = family_stem(modelled.0);
    if !norm.starts_with(family_stem) {
        return None;
    }
    Some(McuSubstitution {
        reference: binding.reference.clone(),
        backend: binding.backend.clone(),
        requested_part: requested.to_string(),
        modelled_core: modelled.1.to_string(),
    })
}

/// Uppercase + strip non-alphanumerics so "STM32F411RET6" and "stm32-f411"
/// compare equal up to a prefix.
fn normalise_part(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// The family stem shared by every member of a series: "STM32F407" -> "STM32F4",
/// "STM32F103" -> "STM32F1", "NRF52840" -> "NRF52", "FE310" -> "FE3".
fn family_stem(modelled_norm: &str) -> &str {
    if let Some(rest) = modelled_norm.strip_prefix("STM32F") {
        // Keep the series digit (the first char after STM32F).
        return &modelled_norm[..("STM32F".len() + rest.chars().next().map_or(0, |_| 1))];
    }
    if modelled_norm.starts_with("NRF52") {
        return "NRF52";
    }
    if modelled_norm.starts_with("FE3") {
        return "FE3";
    }
    modelled_norm
}

/// Build a Renode-backed core for a `renode:<part>` backend string.
#[cfg(feature = "renode")]
fn instantiate_renode(part: &str) -> anyhow::Result<Box<dyn Mcu + Send>> {
    use hauksbee_mcu::RenodeBackend;
    let config = resolve_renode_config(part)?;
    Ok(Box::new(RenodeBackend::new(config)?))
}

/// The canonical descriptor part for a legacy alias token, if `part` is one.
///
/// The pre-descriptor scheduler accepted these shorthand backend strings; the
/// descriptor files are named after the canonical parts, so an alias falls
/// back to its canonical descriptor when no `<alias>.soc.toml` exists. The
/// alias is still tried verbatim FIRST so an override dir can shadow it too.
#[cfg(feature = "renode")]
fn renode_part_alias(part: &str) -> Option<&'static str> {
    match part {
        "stm32f4" => Some("stm32f4_discovery"),
        "nrf52" => Some("nrf52840"),
        "fe310" => Some("sifive_fe310"),
        "pico" => Some("rp2040"),
        _ => None,
    }
}

/// Resolve a `renode:<part>` token to a `RenodeConfig` through the descriptor
/// path ([`hauksbee_mcu::SocConfig::resolve`]): `$HAUKSBEE_MCU_DIR` →
/// `~/.config/hauksbee/mcu` → the embedded builtin. This is the product-path
/// half of "add a Renode MCU purely as data" (06 §6.4): an override-dir
/// descriptor WINS over the embedded builtin of the same name, and an INVALID
/// override descriptor for the requested part fails loudly with its named
/// validation error; it is never silently skipped in favour of the builtin.
///
/// Only a genuine not-found on a legacy alias token (`stm32f4`, `nrf52`,
/// `fe310`, `pico`) falls back to the canonical part's descriptor.
#[cfg(feature = "renode")]
fn resolve_renode_config(part: &str) -> anyhow::Result<hauksbee_mcu::RenodeConfig> {
    use hauksbee_mcu::{SocConfig, SocError};
    let spec = format!("renode:{part}");
    let resolved = match SocConfig::resolve(&spec) {
        Ok(cfg) => cfg,
        // Not found under the alias name anywhere: try the canonical part.
        // Any OTHER error (an unreadable or invalid descriptor that DOES
        // exist for the alias) propagates, fail loud, never skip.
        Err(SocError::NotFound { .. }) if renode_part_alias(part).is_some() => {
            let canon = renode_part_alias(part).expect("guard checked");
            SocConfig::resolve(&format!("renode:{canon}")).map_err(|e| {
                anyhow::anyhow!(
                    "resolving MCU descriptor for '{spec}' (alias of 'renode:{canon}'): {e}"
                )
            })?
        }
        Err(e) => anyhow::bail!("resolving MCU descriptor for '{spec}': {e}"),
    };
    match resolved {
        SocConfig::Renode(config) => Ok(config),
        // Unreachable: resolve() validates the descriptor's declared backend
        // against the spec's `renode:` half. Kept as a loud backstop.
        #[cfg(feature = "qemu")]
        SocConfig::Qemu(_) => anyhow::bail!(
            "descriptor for '{spec}' declares backend \"qemu\" but was requested as renode"
        ),
    }
}

/// Whether a backend STRING names a core that can report pin drive direction,
/// decided from static data (no emulator is spawned). The scaffold-time
/// companion of [`Scheduler::drive_direction_observable`], for callers (like
/// `hauksbee-ci init`) that reason about a board before any co-sim runs:
///   - `simavr:*`, true, the in-process core reads DDR;
///   - `renode:<part>`, true iff the part's SoC descriptor resolves and every
///     GPIO port carries a direction-register map (`dir = {...}`, verified
///     per-part; see `db/mcu/*.soc.toml`);
///   - anything else (QEMU, unknown futures), false, fail-safe.
pub fn backend_reports_drive_direction(backend: &str) -> bool {
    if !backend_is_external(backend) {
        return true;
    }
    #[cfg(feature = "renode")]
    if let Some(part) = backend.strip_prefix("renode:") {
        return match resolve_renode_config(part) {
            Ok(cfg) => !cfg.ports.is_empty() && cfg.ports.iter().all(|p| p.dir.is_some()),
            Err(_) => false,
        };
    }
    false
}

#[cfg(not(feature = "renode"))]
fn instantiate_renode(_part: &str) -> anyhow::Result<Box<dyn Mcu + Send>> {
    anyhow::bail!(
        "this build of hauksbee-engine was compiled without the `renode` feature; \
         rebuild with --features renode to run non-AVR firmware"
    )
}

/// Build the edge-driven 74HC595 chain controllers, the owning-MCU index per
/// chain, and the set of chip indices they own.
///
/// Each PHYSICAL chain (recovered separately by `order_595_chains`, so two
/// chains fed by different SER sources are never merged) is matched against each
/// MCU's GPIO-net map: an MCU owns the chain when it drives the chain head's
/// SRCLK / RCLK / SER. The owning MCU's index is recorded so the chain only
/// consumes that MCU's edge log (a different MCU's identically-named pin, e.g.
/// PB5, must not inject spurious clocks). A chain no MCU drives is left to the
/// old once-per-chunk digital tick, so nothing regresses.
fn build_595_chains(
    digital: &[DigitalComponent],
    mcus: &[LiveMcu],
) -> (
    Vec<crate::digital::Hc595Chain>,
    Vec<usize>,
    std::collections::HashSet<usize>,
) {
    use crate::digital::{order_595_chains, Hc595Chain};

    let mut chains = Vec::new();
    let mut chain_mcu = Vec::new();
    let mut owned = std::collections::HashSet::new();

    // Precompute each MCU's net-node -> (port, bit) GPIO map once.
    let gpio_maps: Vec<HashMap<i64, (char, u8)>> = mcus
        .iter()
        .map(|m| {
            m.binding
                .gpio_drivers
                .iter()
                .map(|(&(port, bit), drv)| (drv.net.0 as i64, (port, bit)))
                .collect()
        })
        .collect();

    for order in order_595_chains(digital) {
        // Bind this chain to whichever MCU drives its head's control pins.
        for (mi, gpio_node) in gpio_maps.iter().enumerate() {
            if let Some(chain) = Hc595Chain::build(digital, order.clone(), gpio_node) {
                for &c in &chain.order {
                    owned.insert(c);
                }
                chains.push(chain);
                chain_mcu.push(mi);
                break;
            }
        }
    }
    (chains, chain_mcu, owned)
}

/// Identify standalone GPIO-edge-driven digital components for the generalized
/// replay (05 §1.2), and build each MCU's GPIO `(port,bit)` -> driven-net map.
///
/// A component qualifies when it is a shift/latch part (74HC595 / 74HC165) that
/// is NOT already owned by a 595 chain (`chain_chips`) nor by a 165 read chain
/// (the responder path), AND at least one of its clock/data input roles is wired
/// to a net an MCU GPIO drives. That last condition is what makes it truly
/// edge-driven: without a GPIO on its clock it can only change at solve
/// boundaries and stays on the once-per-chunk analog tick (§1.2 cadence case
/// (b)). On the current corpus every GPIO-clocked 595 is a chain and every 165 a
/// responder, so this returns an empty chip list; the generalization is a
/// no-op that regresses nothing and is exercised by the synthetic burst test.
fn build_generic_replay_chips(
    digital: &[DigitalComponent],
    chain_chips: &std::collections::HashSet<usize>,
    mcus: &[LiveMcu],
) -> (Vec<usize>, Vec<HashMap<(char, u8), NodeId>>) {
    use crate::digital::order_165_chains;

    let pin_nets: Vec<HashMap<(char, u8), NodeId>> = mcus
        .iter()
        .map(|m| {
            m.binding
                .gpio_drivers
                .iter()
                .map(|(&(port, bit), drv)| ((port, bit), drv.net))
                .collect()
        })
        .collect();
    let driven_nets: std::collections::HashSet<i64> = pin_nets
        .iter()
        .flat_map(|m| m.values().map(|n| n.0 as i64))
        .collect();

    // Chips owned by a 165 read chain (resolved via the synchronous responder).
    let mut responder_chips: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for order in order_165_chains(digital) {
        for c in order {
            responder_chips.insert(c);
        }
    }

    let mut chips = Vec::new();
    for (i, d) in digital.iter().enumerate() {
        if chain_chips.contains(&i) || responder_chips.contains(&i) {
            continue;
        }
        // A part qualifies when its spec declares at least one clocked
        // register (sequential, pulse trains on its pins would collapse at
        // chunk granularity) and one of those sequential pins (clock / reset /
        // load / enable / serial data, straight from the spec) is wired to a
        // GPIO-driven net. The test reads the spec rather than a part-kind
        // list, so a declarative 74HC74 clocked by a GPIO rides the same edge
        // path with no Rust change. Purely combinational parts (gates,
        // latches) stay on the once-per-chunk analog tick.
        if !d.is_sequential() {
            continue;
        }
        let seq_pins = d.sequential_pins();
        let gpio_clocked = d.roles.iter().any(|(role, n)| {
            seq_pins.contains(&role.as_str()) && driven_nets.contains(&(n.0 as i64))
        });
        if gpio_clocked {
            chips.push(i);
        }
    }
    (chips, pin_nets)
}

/// Wire `on_pin_change` / `on_uart` hooks into a shared capture buffer.
fn core_with_hooks(mut core: Box<dyn Mcu + Send>, binding: McuBinding) -> LiveMcu {
    let shared = Arc::new(Mutex::new(McuShared::default()));
    let pin_sink = shared.clone();
    // These callbacks fire from inside the MCU core's run loop, which for the
    // simavr backend is across an `extern "C"` FFI boundary where an unwind is
    // UB. So never panic on a poisoned lock: recover the guard and keep going
    // (the captured data is a simple accumulation buffer).
    core.on_pin_change(Box::new(move |pin: PinId, high: bool, cycle: u64| {
        let mut sh = pin_sink.lock().unwrap_or_else(|e| e.into_inner());
        sh.pin_edges.insert((pin.port, pin.bit), high);
        sh.pin_edge_log.push(PinEdge {
            cycle,
            port: pin.port,
            bit: pin.bit,
            level: high,
        });
        // Real SPI chip-select framing (05 §2.1): if this pin drives a slave's
        // CS net, frame that transaction NOW, interleaved in cycle order with the
        // byte transfers (which arrive through the separate `on_spi` closure).
        // Collect the matching buses while holding the McuShared lock, then RELEASE
        // it before taking a bus lock: the lock order is always McuShared -> SpiBus
        // (the `on_spi` closure only ever takes the bus lock), so never invert it.
        let mut frames: Vec<(Arc<Mutex<SpiBus>>, bool)> = Vec::new();
        for f in &sh.cs_frames {
            if f.pin == (pin.port, pin.bit) {
                // Active-low CS: falling edge (level=false) asserts, rising deasserts.
                let asserted = if f.active_low { !high } else { high };
                frames.push((f.bus.clone(), asserted));
            }
        }
        drop(sh);
        for (bus, asserted) in frames {
            let mut b = bus.lock().unwrap_or_else(|e| e.into_inner());
            if asserted {
                b.cs_assert();
            } else {
                b.cs_deassert();
            }
        }
    }));
    let uart_sink = shared.clone();
    core.on_uart(Box::new(move |b: u8| {
        uart_sink
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .uart_out
            .push(b);
    }));
    // Tell a polling backend which ports the board actually wired, so it only
    // reads those output registers each chunk (no effect on push backends).
    let mut active_ports: Vec<char> = binding.gpio_drivers.keys().map(|(p, _)| *p).collect();
    active_ports.sort_unstable();
    active_ports.dedup();
    core.set_active_ports(&active_ports);
    let logic_high_v = logic_high_for_backend(&binding.backend);
    LiveMcu {
        core,
        binding,
        shared,
        last_levels: HashMap::new(),
        configured_outputs: std::collections::HashSet::new(),
        logic_high_v,
        responder_input_pins: std::collections::HashSet::new(),
        digital_in_levels: HashMap::new(),
    }
}

/// GPIO logic-high voltage by backend: STM32-class parts and the ESP32 family
/// are 3.3 V rails, the classic AVR parts are 5 V.
fn logic_high_for_backend(backend: &str) -> f64 {
    if backend_is_external(backend) {
        3.3
    } else {
        5.0
    }
}

/// Whether ADC channel `ch` has been promoted to a GPIO OUTPUT by the firmware,
/// meaning the scheduler must NOT inject an analog reading for it. True only when
/// the channel's OWN pin (from `adc_pin`) carries an enabled driver. An ADC-only
/// channel (A6/A7, no `adc_pin` entry) is never promoted, and a driver belonging
/// to a DIFFERENT pin that merely shares the net does not count, keying on the
/// net alone wrongly suppressed a legitimate self-monitoring ADC topology.
fn adc_channel_promoted(binding: &McuBinding, ch: u8) -> bool {
    binding
        .adc_pin
        .get(&ch)
        .and_then(|pb| binding.gpio_drivers.get(pb))
        .is_some_and(|d| d.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_promotion_keys_on_the_channels_own_pin_not_the_net() {
        // Round-29: the promotion test asked whether ANY enabled driver sat on the
        // channel's net, so a DIFFERENT pin's output driver sharing the net (a
        // legitimate self-monitoring topology) falsely suppressed ADC injection,
        // and an ADC-only channel (A6/A7, no own pin) could be suppressed too. It
        // must key on the channel's OWN pin driver.
        use crate::binder::McuBinding;
        use crate::drivers::PinDriver;
        use hauksbee_ir::{DeviceId, NodeId};
        let drv = |net: u32, enabled: bool| PinDriver {
            vsource: DeviceId(0),
            net: NodeId(net),
            enabled,
            roff: 1e9,
            resistor: DeviceId(1),
            ron: 100.0,
        };
        let mut gpio_drivers = HashMap::new();
        gpio_drivers.insert(('C', 0), drv(5, false)); // ch0's OWN pin: still an input
        gpio_drivers.insert(('C', 1), drv(5, true)); // a NEIGHBOUR driving the same net
        let mut adc_nets = HashMap::new();
        adc_nets.insert(0u8, NodeId(5));
        adc_nets.insert(6u8, NodeId(5)); // A6: ADC-only, same net
        let mut adc_pin = HashMap::new();
        adc_pin.insert(0u8, ('C', 0)); // ch0 owns (C,0); ch6 (A6) has NO entry
        let mut binding = McuBinding {
            reference: "U1".to_string(),
            backend: String::new(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets,
            adc_pin,
            module: true,
            max_supply_v: None,
        };
        // ch0's own driver is DISABLED: not promoted, even though a neighbour on
        // the same net IS enabled (a net-keyed check would wrongly say promoted).
        assert!(
            !adc_channel_promoted(&binding, 0),
            "a neighbour's driver must not promote ch0"
        );
        // ch6 is ADC-only (no own pin): never promoted, whatever shares its net.
        assert!(
            !adc_channel_promoted(&binding, 6),
            "an ADC-only channel is never promoted"
        );
        // Enable ch0's OWN driver: now it is genuinely promoted to output.
        binding.gpio_drivers.get_mut(&('C', 0)).unwrap().enabled = true;
        assert!(
            adc_channel_promoted(&binding, 0),
            "ch0's own enabled driver promotes it"
        );
    }

    /// A Nano module driving net CLK from A2 (PC2), with CLK feeding an RC
    /// integrator (10k into 100 nF, tau = 1 ms): the load whose response
    /// depends on the WHOLE pulse train, not the final level.
    #[cfg(feature = "avr")]
    const RC_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "CLK")
  (net 4 "MID")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4 thru_hole circle (at 0 4) (size 1 1) (net 1 "GND"))
    (pad 27 thru_hole circle (at 0 27) (size 1 1) (net 2 "+5V"))
    (pad 21 thru_hole circle (at 0 21) (size 1 1) (net 3 "CLK"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 3 "CLK"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 4 "MID"))
  )
  (module Capacitor:C (layer F.Cu)
    (at 120 100)
    (fp_text reference C1 (at 0 0) (layer F.SilkS))
    (fp_text value 100nF (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 4 "MID"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

    /// A passive +5V → R1(100Ω) → MID → R2(300Ω) → GND divider, no MCU. The
    /// binder auto-rails +5V to 5 V; MID settles at 5·300/400 = 3.75 V, so each
    /// resistor carries 12.5 mA. Used to pin the per-frame accumulators to the
    /// stress monitor's own device-current formula and prove they reset per step.
    const DIVIDER_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "MID")
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 100 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 3 "MID"))
  )
  (module Resistor:R (layer F.Cu)
    (at 120 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 300 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 3 "MID"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

    #[test]
    fn frame_peak_accumulators_track_current_and_reset_per_step() {
        // The peak-current and voltage windows are the aggregates #2 rewired to
        // read from the scheduler's per-chunk accumulators instead of only the
        // frame's final chunk. This guards the wiring and the device-current
        // formula (must agree with the stress monitor), and, critically, that
        // the accumulators are CLEARED at the start of each `step`, so a peak
        // from one frame does not leak forward and inflate the next.
        let board = hauksbee_extract::ExtractedBoard::from_auto(DIVIDER_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        sched.step(1e-3);

        let r1 = sched
            .frame_peak_current()
            .get("R1")
            .copied()
            .expect("R1 tracked");
        let r2 = sched
            .frame_peak_current()
            .get("R2")
            .copied()
            .expect("R2 tracked");
        // 12.5 mA through both legs of the 100/300 divider off the 5 V rail.
        assert!(
            (r1 - 0.0125).abs() < 5e-4,
            "R1 current ~12.5 mA, got {r1:.5} A"
        );
        assert!(
            (r2 - 0.0125).abs() < 5e-4,
            "R2 current ~12.5 mA, got {r2:.5} A"
        );

        // Per-net voltage window captured MID at ~3.75 V (min == max in steady
        // state, and equal to the last-chunk voltage).
        let &(mn, mx) = sched.frame_v_extremes().get("MID").expect("MID tracked");
        let mid = sched.net_voltages().get("MID").copied().unwrap_or(0.0);
        assert!(
            (mn - 3.75).abs() < 0.05 && (mx - 3.75).abs() < 0.05,
            "MID ~3.75 V, got [{mn:.3},{mx:.3}]"
        );
        assert!(
            mn <= mid + 1e-9 && mid <= mx + 1e-9,
            "last-chunk MID must lie within the frame window"
        );

        // A second step must not inherit the first frame's peak; the reset makes
        // the accumulator report this frame's current, not the running max.
        sched.step(1e-3);
        let r1b = sched
            .frame_peak_current()
            .get("R1")
            .copied()
            .expect("R1 tracked");
        assert!(
            (r1b - 0.0125).abs() < 5e-4,
            "post-reset R1 current still ~12.5 mA, got {r1b:.5} A"
        );
    }

    #[cfg(feature = "avr")]
    fn rc_scheduler() -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(RC_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let mut bound = crate::binder::bind_board(&board, &lib);
        // Promote A2 exactly as the first firmware edge would.
        let drv = bound.mcus[0]
            .gpio_drivers
            .get_mut(&('C', 2))
            .expect("A2 driver");
        drv.set_enabled(&mut bound.circuit, true);
        drv.set_volts(&mut bound.circuit, 0.0);
        Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler")
    }

    /// The W4 acceptance gate (08 section 2): a
    /// firmware-shaped bit-bang latches a REAL bound 74HC595 through its
    /// electrical nets. The MCU pins drive SER/SRCLK/RCLK nets; the chip is
    /// bound from the models DB (not a hand-built fixture); the edge train is
    /// the exact shape shiftOut(MSBFIRST, 0xA6) emits; the assertion reads the
    /// latched byte back from the SOLVED node voltages of the output nets,
    /// which a latest-level collapse could never produce.
    #[cfg(feature = "avr")]
    const CHAIN_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "SER")
  (net 4 "SRCLK")
  (net 5 "RCLK")
  (net 6 "Q0")
  (net 7 "Q7")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4 thru_hole circle (at 0 4) (size 1 1) (net 1 "GND"))
    (pad 27 thru_hole circle (at 0 27) (size 1 1) (net 2 "+5V"))
    (pad 14 thru_hole circle (at 0 14) (size 1 1) (net 3 "SER"))
    (pad 16 thru_hole circle (at 0 16) (size 1 1) (net 4 "SRCLK"))
    (pad 15 thru_hole circle (at 0 15) (size 1 1) (net 5 "RCLK"))
  )
  (module Logic:SN74HC595 (layer F.Cu)
    (at 120 100)
    (fp_text reference U2 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC595 (at 0 2) (layer F.Fab))
    (pad 14 thru_hole circle (at 0 0) (size 1 1) (net 3 "SER"))
    (pad 11 thru_hole circle (at 0 1) (size 1 1) (net 4 "SRCLK"))
    (pad 12 thru_hole circle (at 0 2) (size 1 1) (net 5 "RCLK"))
    (pad 15 thru_hole circle (at 0 3) (size 1 1) (net 6 "Q0"))
    (pad 7 thru_hole circle (at 0 4) (size 1 1) (net 7 "Q7"))
    (pad 16 thru_hole circle (at 0 5) (size 1 1) (net 2 "+5V"))
    (pad 8 thru_hole circle (at 0 6) (size 1 1) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 130 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 6 "Q0"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
  (module Resistor:R2 (layer F.Cu)
    (at 140 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 7 "Q7"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

    // The Nano board binds `simavr:atmega328p`, whose in-process core is
    // always instantiated (even with no firmware), so this test needs the
    // GPL-gated `avr` feature and cannot run on the GPL-free renode/qemu
    // build.
    #[cfg(feature = "avr")]
    #[test]
    fn cosim_bitbang_595_latches_through_bound_nets() {
        let board = hauksbee_extract::ExtractedBoard::from_auto(CHAIN_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let mut bound = crate::binder::bind_board(&board, &lib);

        // The Nano drives SER on D11/PB3 (pad 14), SRCLK on D13/PB5 (pad 16),
        // RCLK on D12/PB4 (pad 15): the stock shiftOut wiring. Promote all
        // three exactly as their first firmware edges would.
        for (port, bit) in [('B', 3u8), ('B', 5), ('B', 4)] {
            let drv = bound.mcus[0]
                .gpio_drivers
                .get_mut(&(port, bit))
                .unwrap_or_else(|| panic!("driver for P{port}{bit}"));
            drv.set_enabled(&mut bound.circuit, true);
            drv.set_volts(&mut bound.circuit, 0.0);
        }

        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        // The exact edge shape of shiftOut(SER, SRCLK, MSBFIRST, 0xA6) then an
        // RCLK latch pulse, cycle-stamped like the simavr hook would: set SER,
        // pulse SRCLK high/low per bit, then pulse RCLK.
        let byte = 0xA6u8;
        let mut log = Vec::new();
        let mut cyc = 100u64;
        let mut ser_level = false;
        for i in (0..8).rev() {
            let bit_lv = (byte >> i) & 1 != 0;
            if bit_lv != ser_level {
                log.push(crate::digital::PinEdge {
                    cycle: cyc,
                    port: 'B',
                    bit: 3,
                    level: bit_lv,
                });
                ser_level = bit_lv;
            }
            cyc += 4;
            log.push(crate::digital::PinEdge {
                cycle: cyc,
                port: 'B',
                bit: 5,
                level: true,
            });
            cyc += 4;
            log.push(crate::digital::PinEdge {
                cycle: cyc,
                port: 'B',
                bit: 5,
                level: false,
            });
            cyc += 4;
        }
        log.push(crate::digital::PinEdge {
            cycle: cyc + 4,
            port: 'B',
            bit: 4,
            level: true,
        });
        log.push(crate::digital::PinEdge {
            cycle: cyc + 8,
            port: 'B',
            bit: 4,
            level: false,
        });

        // Replay through the generalized path (what run_chunk does with the
        // drained MCU log), then let the digital layer push latched outputs
        // and solve the chunk, mirroring run_chunk's order.
        let ticks = sched.replay_digital_edges(0, &log);
        assert!(ticks > 0, "the bound 595 must be clocked by the replay");
        // Mirror run_chunk's order: the chain's latched outputs are pushed
        // onto the analog nets by the chain-apply step before the solve.
        if !sched.chains.is_empty() {
            let mut chains = std::mem::take(&mut sched.chains);
            for chain in &mut chains {
                chain.apply(&mut sched.digital, &mut sched.circuit);
            }
            sched.chains = chains;
        }
        // Post-replay: apply final pin levels (all low) and solve so the
        // latched outputs appear on the electrical nets.
        assert!(sched.solve_chunk(100e-6), "chunk solve converges");

        // 0xA6 = 1010_0110: QA (bit 7 first shifted lands at QH... follow the
        // engine's convention: MSB-first shiftOut leaves the FIRST-sent bit in
        // QH (Q7) and the LAST-sent in QA (Q0). First-sent = MSB = 1 -> Q7
        // high; last-sent = LSB = 0 -> Q0 low.
        let q7 = sched.net_voltage("Q7").expect("Q7 solved");
        let q0 = sched.net_voltage("Q0").expect("Q0 solved");
        assert!(
            q7 > 3.0,
            "Q7 (first-shifted MSB of 0xA6 = 1) must be driven high electrically, got {q7:.2} V"
        );
        assert!(
            q0 < 1.0,
            "Q0 (last-shifted LSB of 0xA6 = 0) must rest low electrically, got {q0:.2} V"
        );
    }

    /// The electrical face of sub-chunk pulse fidelity: ten 5 us pulses
    /// inside one 100 us chunk end LOW, so a final-level DC drive leaves the
    /// RC integrator empty; the PWL drive integrates every pulse and pumps it
    /// to roughly the 50%-duty average. This is the analog half of sub-chunk
    /// fidelity, the digital half being the cycle-ordered replay.
    ///
    /// `rc_scheduler` binds an `simavr:atmega328p` Nano, whose in-process
    /// core is always instantiated, so this test needs the GPL-gated `avr`
    /// feature and cannot run on the GPL-free renode/qemu build.
    #[cfg(feature = "avr")]
    #[test]
    fn pwl_drive_integrates_a_pulse_train_the_dc_path_collapses() {
        let chunk = 100e-6;

        // Synthetic cycle-stamped edges: 16 MHz core, edge every 5 us
        // (80,000 cycles... 5 us at 16 MHz = 80 cycles-per-us * 5 = 400).
        // Ten high pulses at 50% duty across the chunk, ending low.
        let mut transitions = Vec::new();
        let cycles_per_chunk = 1600u64; // 100 us at 16 MHz
        for k in 0..10u64 {
            let up = k * 160;
            let down = up + 80;
            transitions.push((up, true));
            transitions.push((down, false));
        }
        let mut edges = HashMap::new();
        edges.insert(('C', 2u8), transitions);

        // Control run first: no chunk edges recorded, so the DC path rules
        // and the driver rests at its final level (low). The cap stays flat.
        let mut sched = rc_scheduler();
        assert!(sched.solve_chunk(chunk), "control solve converges");
        let mid_dc = sched.net_voltage("MID").expect("MID");
        assert!(
            mid_dc.abs() < 0.05,
            "control: a low-resting DC drive must leave the RC at ~0 V, got {mid_dc:.3}"
        );

        // PWL run: inject the chunk's stamped edges as the MCU drain would
        // have, apply the drive, solve, restore.
        let mut sched = rc_scheduler();
        sched.last_chunk_edges.push(ChunkPinEdges {
            mcu_reference: "A1".into(),
            edges,
            cycle_span: (0, cycles_per_chunk),
            chunk_s: chunk,
            cycle_exact: true,
        });
        let restores = sched.apply_pwl_drives(chunk);
        assert_eq!(restores.len(), 1, "exactly the CLK pin gets a PWL drive");
        assert!(sched.solve_chunk(chunk), "pwl solve converges");
        sched.restore_pwl_drives(&restores);

        let mid_pwl = sched.net_voltage("MID").expect("MID");
        // Ten 5 us high pulses = 50 us at 5 V through tau = 1 ms:
        // ~5 * (1 - exp(-0.05)) * duty-shape, roughly 0.2 V. The exact figure
        // is not the point; the discrimination from the collapsed 0 V is.
        assert!(
            mid_pwl > 0.15,
            "the PWL drive must pump the RC integrator, got {mid_pwl:.4} V"
        );

        // And the restore: the driver's source must be back at DC low.
        let drv = &sched.mcus[0].binding.gpio_drivers[&('C', 2)];
        match &sched.circuit.devices[drv.vsource.0 as usize] {
            Device::Vsource { kind, .. } => {
                assert!(matches!(kind, hauksbee_ir::SourceKind::Dc(v) if v.abs() < 1e-9));
            }
            _ => panic!("driver vsource missing"),
        }
    }

    /// A Nano driving net BUS from A2 (PC2) into a plain 10k pull-down: the
    /// minimal board on which a released (DDR output→input) pin is
    /// electrically distinguishable from a latched one.
    #[cfg(feature = "avr")]
    const RELEASE_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "BUS")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4 thru_hole circle (at 0 4) (size 1 1) (net 1 "GND"))
    (pad 27 thru_hole circle (at 0 27) (size 1 1) (net 2 "+5V"))
    (pad 21 thru_hole circle (at 0 21) (size 1 1) (net 3 "BUS"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 3 "BUS"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

    /// Regression for the driver-release half of `sync_configured_outputs`: a
    /// pin reported configured-output one chunk and gone the next (DDR
    /// output→input, the open-drain bus hand-off) must have its Thevenin
    /// driver disabled, so the net falls to its pull instead of staying
    /// clamped at the stale driven level (the latched-bus failure). The board
    /// binds `simavr:atmega328p`, so this needs the GPL-gated `avr` feature.
    #[cfg(feature = "avr")]
    #[test]
    fn sync_configured_outputs_releases_dropped_pins() {
        let board = hauksbee_extract::ExtractedBoard::from_auto(RELEASE_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        // Chunk 1: the core reports PC2 configured as an output, last driven
        // HIGH (the DDR-write-no-toggle promotion path). The driver enables
        // and the BUS net is clamped high through the 10k pull-down.
        sched.mcus[0].last_levels.insert(('C', 2), true);
        let mut configured = std::collections::HashSet::new();
        configured.insert(('C', 2u8));
        sched.sync_configured_outputs(0, configured);
        assert!(
            sched.mcus[0].binding.gpio_drivers[&('C', 2)].enabled,
            "a configured-output pin must have its driver enabled"
        );
        assert!(sched.solve_chunk(1e-3), "driven solve converges");
        let driven = sched.net_voltage("BUS").expect("BUS solved");
        assert!(
            driven > 3.0,
            "driven-high BUS must read ~5 V, got {driven:.2} V"
        );

        // Chunk 2: the pin drops out of the configured set (DDR back to
        // input, no PORT edge). The driver must be DISABLED and the pull-down
        // must win; leaving the driver enabled latches the net at 5 V.
        sched.sync_configured_outputs(0, std::collections::HashSet::new());
        assert!(
            !sched.mcus[0].binding.gpio_drivers[&('C', 2)].enabled,
            "a pin released back to input must have its driver disabled"
        );
        assert!(sched.solve_chunk(1e-3), "released solve converges");
        let released = sched.net_voltage("BUS").expect("BUS solved");
        assert!(
            released < 0.5,
            "released BUS must fall through its pull-down, got {released:.2} V (latched bus)"
        );
    }

    // ── Plain digital-input sync (BUG #17) ──────────────────────────────────

    /// A trait-level mock core recording every `set_digital_in` call, so the
    /// run_chunk digital-input sync is provable on the GPL-free build (no
    /// emulator backend needed; the sync is engine logic, not backend logic).
    struct RecordingCore {
        digital_ins: Arc<Mutex<Vec<((char, u8), bool)>>>,
    }

    impl Mcu for RecordingCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            Ok(n)
        }
        fn run_micros(&mut self, _us: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn frequency(&self) -> u64 {
            16_000_000
        }
        fn set_digital_in(&mut self, pin: PinId, high: bool) {
            self.digital_ins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(((pin.port, pin.bit), high));
        }
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, _cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {}
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: Box<dyn FnMut(u8) + Send>) {}
        fn on_i2c(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>) {}
        fn on_spi(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>) {}
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: 0,
                cycles: 0,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
    }

    /// A board with NO MCU module: two pulled nets (10 k to +5 V, 10 k to
    /// GND) plus a pulled-high net standing in for a responder-owned MISO.
    /// The test hand-wires a mock MCU onto these nets, exactly the shape the
    /// binder produces (tri-stated PinDriver per wired pin).
    const PLAIN_INPUT_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "BTN_HI")
  (net 4 "BTN_LO")
  (net 5 "RESP")

  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 3 "BTN_HI"))
  )
  (module Resistor:R2 (layer F.Cu)
    (at 120 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 4 "BTN_LO"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
  (module Resistor:R3 (layer F.Cu)
    (at 130 100)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 5 "RESP"))
  )
)
"#;

    /// Regression for BUG #17: `Mcu::set_digital_in` had ZERO engine callers,
    /// so a plain circuit-driven digital input (pushbutton, limit switch,
    /// comparator output) never reached firmware, `digitalRead` saw only the
    /// core's power-on level. Proves, through the real `step`/`run_chunk`
    /// path with real solved node voltages:
    ///   * a tri-stated input pin on a net pulled HIGH gets exactly one
    ///     `set_digital_in(pin, true)` (change-filtered, not per-chunk spam);
    ///   * one on a net pulled LOW gets exactly one `set_digital_in(pin, false)`;
    ///   * a responder-owned pin is NEVER driven by the sync, even though its
    ///     net is solidly pulled high (the responder alone owns it);
    ///   * a floating net (no device but the pin's own tri-state leg) is left
    ///     alone; its ~0 V solve is fiction, and pushing it would defeat an
    ///     internal pull-up.
    #[test]
    fn plain_digital_inputs_reach_the_core() {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        // Hand-wire a mock MCU: one tri-stated (input) driver per wired pin,
        // exactly what the binder stamps for a wired digital-capable pin the
        // firmware never drives.
        let hi_node = sched.net_nodes["BTN_HI"];
        let lo_node = sched.net_nodes["BTN_LO"];
        let resp_node = sched.net_nodes["RESP"];
        let float_node = sched.circuit.node("FLOATY"); // wired to nothing else
        let mut gpio_drivers = HashMap::new();
        for (pin, node, name) in [
            (('C', 0u8), hi_node, "BTN_HI"),
            (('C', 1), lo_node, "BTN_LO"),
            (('C', 2), resp_node, "RESP"),
            (('C', 3), float_node, "FLOATY"),
        ] {
            let mut drv = crate::drivers::PinDriver::stamp(
                &mut sched.circuit,
                node,
                name,
                &format!("t_{}{}", pin.0, pin.1),
                crate::drivers::DEFAULT_RO,
            );
            drv.set_enabled(&mut sched.circuit, false); // input: tri-stated
            gpio_drivers.insert(pin, drv);
        }
        let binding = McuBinding {
            reference: "U1".into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        let digital_ins = Arc::new(Mutex::new(Vec::new()));
        let core = RecordingCore {
            digital_ins: digital_ins.clone(),
        };
        sched.mcus.push(core_with_hooks(Box::new(core), binding));
        sched.responder_registries.push(None);
        // PC2 stands in for a responder-owned MISO/SDA pin.
        sched.mcus[0].responder_input_pins.insert(('C', 2));
        // Pick up the freshly stamped driver legs (and rebuild the
        // drive-evidence index) exactly as attach_peripheral would.
        sched.relayout();

        // Several chunks: chunk 1 has no solved voltages yet (no push);
        // chunk 2+ sees the solved levels; later chunks must not re-push.
        sched.step(5.0 * DEFAULT_CHUNK_S);

        let calls = digital_ins.lock().unwrap_or_else(|e| e.into_inner());
        let for_pin = |pin: (char, u8)| -> Vec<bool> {
            calls
                .iter()
                .filter(|(p, _)| *p == pin)
                .map(|&(_, l)| l)
                .collect()
        };
        assert_eq!(
            for_pin(('C', 0)),
            vec![true],
            "pulled-high plain input must be pushed HIGH exactly once, got {calls:?}"
        );
        assert_eq!(
            for_pin(('C', 1)),
            vec![false],
            "pulled-low plain input must be pushed LOW exactly once, got {calls:?}"
        );
        assert!(
            for_pin(('C', 2)).is_empty(),
            "responder-owned pin must never be driven by the chunk sync, got {calls:?}"
        );
        assert!(
            for_pin(('C', 3)).is_empty(),
            "floating net must not be pushed (its 0 V solve is the tri-state \
             legs talking), got {calls:?}"
        );
    }

    /// A CS net that resolves to a pin BOTH MCUs happen to have a driver for
    /// (e.g. a shared CS rail, or two parts each exposing the same port bit)
    /// must frame the SPI transaction on exactly ONE MCU; the first owner,
    /// mirroring the "a net is driven by at most one MCU" invariant. The old
    /// `for m in &mut self.mcus { if contains_key {...} }` installed a CsFrame
    /// on EVERY sharer, so a single CS edge framed the bus twice: a spurious
    /// double select/deselect that corrupts the transaction replay.
    #[test]
    fn cs_frame_installs_on_only_one_mcu_not_every_sharer_of_the_pin() {
        struct Opaque;
        impl crate::peripherals::spi::SpiSlave for Opaque {
            fn transfer(&mut self, _mosi: u8) -> u8 {
                0
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }

        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        let cs_pin = ('C', 2u8);
        let resp_node = sched.net_nodes["RESP"];

        // Two MCUs, BOTH owning a gpio driver for the same CS pin.
        for _ in 0..2 {
            let mut gpio_drivers = HashMap::new();
            let drv = crate::drivers::PinDriver::stamp(
                &mut sched.circuit,
                resp_node,
                "RESP",
                "t_cs",
                crate::drivers::DEFAULT_RO,
            );
            gpio_drivers.insert(cs_pin, drv);
            let binding = McuBinding {
                reference: "U1".into(),
                backend: "simavr:test".into(),
                requested_part: String::new(),
                pad_roles: HashMap::new(),
                role_nets: HashMap::new(),
                gpio_drivers,
                adc_nets: HashMap::new(),
                adc_pin: HashMap::new(),
                module: false,
                max_supply_v: None,
            };
            let core = RecordingCore {
                digital_ins: Arc::new(Mutex::new(Vec::new())),
            };
            sched.mcus.push(core_with_hooks(Box::new(core), binding));
            sched.responder_registries.push(None);
        }

        let bus = Arc::new(Mutex::new(SpiBus::new("U9", Box::new(Opaque))));
        sched.register_cs_frame(&bus, Some(cs_pin), None);

        let total: usize = sched
            .mcus
            .iter()
            .map(|m| {
                m.shared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cs_frames
                    .len()
            })
            .sum();
        assert_eq!(
            total, 1,
            "a shared CS pin must frame on exactly one MCU, not every sharer, got {total}"
        );
    }

    #[test]
    fn cs_frame_installs_on_the_mcu_driving_the_cs_net_not_the_first_tuple_owner() {
        struct Opaque;
        impl crate::peripherals::spi::SpiSlave for Opaque {
            fn transfer(&mut self, _mosi: u8) -> u8 {
                0
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }

        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        let cs_pin = ('C', 2u8);
        // R44: two MCUs own the SAME chip-local (C,2) pin, but on DIFFERENT nets.
        // MCU_0 (pushed first) drives it on an unrelated net; MCU_1 drives it on the
        // real CS net. Keying only on the tuple installed the frame on MCU_0 (first
        // owner). With the CS net threaded, it must land on MCU_1.
        let unrelated_net = sched.net_nodes["RESP"];
        let cs_net = sched.circuit.node("SPI_CS");

        for (idx, net) in [unrelated_net, cs_net].into_iter().enumerate() {
            let mut gpio_drivers = HashMap::new();
            let net_name = sched.circuit.node_name(net).to_string();
            let drv = crate::drivers::PinDriver::stamp(
                &mut sched.circuit,
                net,
                &net_name,
                "t_cs",
                crate::drivers::DEFAULT_RO,
            );
            gpio_drivers.insert(cs_pin, drv);
            let binding = McuBinding {
                reference: format!("U{idx}"),
                backend: "simavr:test".into(),
                requested_part: String::new(),
                pad_roles: HashMap::new(),
                role_nets: HashMap::new(),
                gpio_drivers,
                adc_nets: HashMap::new(),
                adc_pin: HashMap::new(),
                module: false,
                max_supply_v: None,
            };
            let core = RecordingCore {
                digital_ins: Arc::new(Mutex::new(Vec::new())),
            };
            sched.mcus.push(core_with_hooks(Box::new(core), binding));
            sched.responder_registries.push(None);
        }

        let bus = Arc::new(Mutex::new(SpiBus::new("U9", Box::new(Opaque))));
        // The CS net is cs_net (MCU_1's), NOT the first tuple owner MCU_0.
        sched.register_cs_frame(&bus, Some(cs_pin), Some(cs_net));

        let frames = |m: usize| {
            sched.mcus[m]
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cs_frames
                .len()
        };
        assert_eq!(
            frames(0),
            0,
            "the unrelated first tuple-owner MCU must NOT be framed"
        );
        assert_eq!(
            frames(1),
            1,
            "the MCU that drives the CS net must be framed"
        );
    }

    // ── Multi-bus on_i2c / on_spi dispatch (R47) ─────────────────────────────

    /// A mock core that CAPTURES the callbacks the scheduler installs (instead
    /// of discarding them like [`RecordingCore`]), so a test can drive them
    /// exactly the way firmware traffic would. Mirrors the AVR core's
    /// single-slot semantics: each `on_*` call REPLACES the stored callback,
    /// and `set_i2c_slave_addresses` replaces the recorded filter, which is
    /// precisely the overwrite behavior the multi-bus dispatch must survive.
    #[derive(Default)]
    struct CapturingCore {
        i2c_cb: Arc<Mutex<Option<Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>>>>,
        spi_cb: Arc<Mutex<Option<Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>>>>,
        pin_cb: Arc<Mutex<Option<Box<dyn FnMut(PinId, bool, u64) + Send>>>>,
        i2c_addrs: Arc<Mutex<Vec<u8>>>,
    }

    impl Mcu for CapturingCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            Ok(n)
        }
        fn run_micros(&mut self, _us: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn frequency(&self) -> u64 {
            16_000_000
        }
        fn set_digital_in(&mut self, _pin: PinId, _high: bool) {}
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {
            *self.pin_cb.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);
        }
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: Box<dyn FnMut(u8) + Send>) {}
        fn on_i2c(&mut self, cb: Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>) {
            *self.i2c_cb.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);
        }
        fn on_spi(&mut self, cb: Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>) {
            *self.spi_cb.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);
        }
        fn set_i2c_slave_addresses(&mut self, addresses: &[u8]) {
            *self.i2c_addrs.lock().unwrap_or_else(|e| e.into_inner()) = addresses.to_vec();
        }
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: 0,
                cycles: 0,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
    }

    /// Wire one `CapturingCore` MCU (with GPIO drivers for the given pins, on
    /// per-pin fresh nets) onto a solvable board. Returns the scheduler plus
    /// the captured-callback handles.
    fn sched_with_capturing_core(pins: &[(char, u8)]) -> (Scheduler, CapturingCore) {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let mut gpio_drivers = HashMap::new();
        for &pin in pins {
            let name = format!("CS_{}{}", pin.0, pin.1);
            let node = sched.circuit.node(&name);
            let drv = crate::drivers::PinDriver::stamp(
                &mut sched.circuit,
                node,
                &name,
                &format!("t_{}{}", pin.0, pin.1),
                crate::drivers::DEFAULT_RO,
            );
            gpio_drivers.insert(pin, drv);
        }
        let binding = McuBinding {
            reference: "U1".into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        let core = CapturingCore::default();
        let handles = CapturingCore {
            i2c_cb: core.i2c_cb.clone(),
            spi_cb: core.spi_cb.clone(),
            pin_cb: core.pin_cb.clone(),
            i2c_addrs: core.i2c_addrs.clone(),
        };
        sched.mcus.push(core_with_hooks(Box::new(core), binding));
        sched.responder_registries.push(None);
        (sched, handles)
    }

    /// Regression (R54): update_stats classified logic level with a fixed 3.0/2.0
    /// V band (a 5 V-rail assumption). On a 3.3 V board a loaded GPIO high output
    /// settles below 3.0 V, so every high sample landed mid-band, `last_logic`
    /// never established a level, and `toggles` stayed 0, a blinking net read as
    /// inactive. The band must scale with the board's logic-high rail.
    #[test]
    fn toggle_counting_is_rail_relative_on_a_3v3_board() {
        let (mut sched, _h) = sched_with_capturing_core(&[]);
        // A 3.3 V logic rail (the external-MCU class: renode/qemu).
        sched.mcus[0].logic_high_v = 3.3;

        let node = sched.circuit.node("BLINK");
        sched.net_nodes.insert("BLINK".to_string(), node);
        let idx = node.0 as usize;
        if sched.node_volts.len() <= idx {
            sched.node_volts.resize(idx + 1, 0.0);
        }

        // A loaded 3.3 V GPIO output swings 0 V <-> 2.7 V, below the 3.0 V
        // high threshold a fixed 5 V band would impose. Drive
        // high/low/high/low, three transitions.
        for v in [2.7_f64, 0.0, 2.7, 0.0] {
            sched.node_volts[idx] = v;
            sched.update_stats();
        }
        let toggles = sched.stats.get("BLINK").map(|s| s.toggles).unwrap_or(0);
        assert!(
            toggles >= 3,
            "a 3.3 V net swinging to 2.7 V must register toggles, got {toggles}"
        );
    }

    /// Regression (R55): the toggle band uses ONE global rail. On a mixed-rail
    /// board (a 5 V AVR + a 3.3 V ESP32) taking the MAX rail (5.0 → vih 3.0)
    /// reintroduced the R54 undercount for the 3.3 V domain. It must use the
    /// LOWEST rail so both domains' nets toggle.
    #[test]
    fn toggle_counting_uses_the_min_rail_on_a_mixed_rail_board() {
        let (mut sched, _h) = sched_with_capturing_core(&[]);
        // MCU 0 is a 5 V AVR (simavr backend default).
        sched.mcus[0].logic_high_v = 5.0;
        // Add a second MCU on a 3.3 V rail (renode external backend → 3.3 V).
        let binding = McuBinding {
            reference: "U2".into(),
            backend: "renode:stm32f4".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers: HashMap::new(),
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        sched
            .mcus
            .push(core_with_hooks(Box::new(CapturingCore::default()), binding));
        sched.responder_registries.push(None);
        assert!(
            (sched.mcus[1].logic_high_v - 3.3).abs() < 1e-6,
            "the second MCU should be on a 3.3 V rail"
        );

        let node = sched.circuit.node("BLINK33");
        sched.net_nodes.insert("BLINK33".to_string(), node);
        let idx = node.0 as usize;
        if sched.node_volts.len() <= idx {
            sched.node_volts.resize(idx + 1, 0.0);
        }
        // A 3.3 V-domain net whose loaded high settles at ~2.7 V (< the 3.0 V band
        // the MAX rail would produce). It must still toggle.
        for v in [2.7_f64, 0.0, 2.7, 0.0] {
            sched.node_volts[idx] = v;
            sched.update_stats();
        }
        let toggles = sched.stats.get("BLINK33").map(|s| s.toggles).unwrap_or(0);
        assert!(
            toggles >= 3,
            "a 3.3 V net on a mixed-rail board must toggle (min-rail band), got {toggles}"
        );
    }

    /// A mock core for the co-sim coverage honesty tests (U3): reports NO
    /// modeled bus controllers and a dropped ADC channel 0; the shape of a
    /// Renode platform whose descriptor carries empty controller lists and no
    /// `[[soc.adc]]` map.
    struct BusBlindCore;

    impl Mcu for BusBlindCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            Ok(n)
        }
        fn run_micros(&mut self, _us: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn frequency(&self) -> u64 {
            64_000_000
        }
        fn set_digital_in(&mut self, _pin: PinId, _high: bool) {}
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, _cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {}
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: Box<dyn FnMut(u8) + Send>) {}
        fn on_i2c(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>) {}
        fn on_spi(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>) {}
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: 0,
                cycles: 0,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
        fn i2c_bus_modeled(&self) -> bool {
            false
        }
        fn spi_bus_modeled(&self, _controller: Option<&str>) -> bool {
            false
        }
        fn adc_dropped_channels(&self) -> Vec<u8> {
            vec![0]
        }
    }

    /// A scheduler whose single live MCU is a [`BusBlindCore`], with ADC
    /// channel 0 bound to the named net.
    fn sched_with_bus_blind_core(adc_net: &str) -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let node = sched.circuit.node(adc_net);
        sched.net_nodes.insert(adc_net.to_string(), node);
        let mut adc_nets = HashMap::new();
        adc_nets.insert(0u8, node);
        let binding = McuBinding {
            reference: "U1".into(),
            backend: "renode:nrf52840".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers: HashMap::new(),
            adc_nets,
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        sched
            .mcus
            .push(core_with_hooks(Box::new(BusBlindCore), binding));
        sched.responder_registries.push(None);
        sched
    }

    // U3 finding 2: a bus slave attached on a platform whose backend models no
    // matching controller must be RECORDED as unexercised; the raw fact every
    // report surface (text, --plain, --json note, CI) is built from.
    #[test]
    fn bus_slaves_on_an_unmodeled_controller_are_recorded_as_unexercised() {
        let mut sched = sched_with_bus_blind_core("TEMP_SENSE");
        assert!(sched.unexercised_buses().is_empty());

        let i2c = Arc::new(Mutex::new(
            I2cBus::new("TEMP1").with_slave(Box::new(crate::Lm75::new(0x48, 25.0))),
        ));
        sched.attach_i2c_bus(i2c);
        let spi = Arc::new(Mutex::new(SpiBus::new(
            "FLASH1",
            Box::new(crate::Spi25Eeprom::new(256)),
        )));
        sched.attach_spi_bus(spi, None, None);
        let spi2 = Arc::new(Mutex::new(SpiBus::new(
            "IMU1",
            Box::new(crate::Spi25Eeprom::new(256)),
        )));
        sched.attach_spi_bus_on("spi9", spi2, None, None);

        let rec = sched.unexercised_buses();
        assert_eq!(
            rec.len(),
            3,
            "all three bound slaves are unexercised: {rec:?}"
        );
        assert_eq!((rec[0].id.as_str(), rec[0].bus), ("TEMP1", "I2C"));
        assert_eq!((rec[1].id.as_str(), rec[1].bus), ("FLASH1", "SPI"));
        assert_eq!(rec[2].controller.as_deref(), Some("spi9"));
        // The canonical message names the device and says it never ran.
        let msg = rec[0].message();
        assert!(
            msg.contains("TEMP1")
                && msg.contains("NEVER exercised")
                && msg.contains("models no I2C controller"),
            "{msg}"
        );
    }

    // The negative: a core that DOES model its buses records nothing (the
    // capturing core keeps the trait defaults, the AVR/QEMU shape).
    #[test]
    fn bus_slaves_on_a_modeled_controller_are_not_recorded() {
        let (mut sched, _h) = sched_with_capturing_core(&[]);
        let i2c = Arc::new(Mutex::new(
            I2cBus::new("TEMP1").with_slave(Box::new(crate::Lm75::new(0x48, 25.0))),
        ));
        sched.attach_i2c_bus(i2c);
        let spi = Arc::new(Mutex::new(SpiBus::new(
            "FLASH1",
            Box::new(crate::Spi25Eeprom::new(256)),
        )));
        sched.attach_spi_bus(spi, None, None);
        assert!(
            sched.unexercised_buses().is_empty(),
            "modeled buses must not be flagged"
        );
    }

    // U3 finding 1: a backend-reported dropped ADC channel resolves to its
    // board net and MCU, and the canonical message says the firmware never
    // received the injection.
    #[test]
    fn dropped_adc_channels_resolve_to_their_net_and_warn() {
        let sched = sched_with_bus_blind_core("TEMP_SENSE");
        let drops = sched.adc_dropped();
        assert_eq!(drops.len(), 1, "{drops:?}");
        assert_eq!(drops[0].mcu_ref, "U1");
        assert_eq!(drops[0].channel, 0);
        assert_eq!(drops[0].net, "TEMP_SENSE");
        let msg = drops[0].message();
        assert!(
            msg.contains("ADC channel 0")
                && msg.contains("TEMP_SENSE")
                && msg.contains("NEVER received")
                && msg.contains("[[soc.adc]]"),
            "{msg}"
        );
    }

    /// Regression (R52): `pin_driving_node` iterated a randomized HashMap and
    /// returned the FIRST driver on the net, so when >1 of an MCU's pins share a
    /// net node (a self-monitoring topology, or two pins bodge-merged) the
    /// resolved CS-framing pin varied run-to-run. It must return a deterministic
    /// pin; the lowest (port, bit), regardless of HashMap iteration order.
    #[test]
    fn pin_driving_node_is_deterministic_when_multiple_pins_share_a_net() {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        // Six of the MCU's pins all wired to ONE shared net node, inserted in a
        // deliberately non-sorted order so the HashMap cannot accidentally yield
        // the minimum first.
        let node = sched.circuit.node("SHARED_CS");
        let mut gpio_drivers = HashMap::new();
        for &pin in &[('D', 7), ('B', 5), ('C', 2), ('B', 4), ('D', 0), ('C', 9)] {
            let drv = crate::drivers::PinDriver::stamp(
                &mut sched.circuit,
                node,
                "SHARED_CS",
                &format!("t_{}{}", pin.0, pin.1),
                crate::drivers::DEFAULT_RO,
            );
            gpio_drivers.insert(pin, drv);
        }
        let binding = McuBinding {
            reference: "U1".into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        sched
            .mcus
            .push(core_with_hooks(Box::new(CapturingCore::default()), binding));
        sched.responder_registries.push(None);

        // The lowest (port, bit) among the six is ('B', 4).
        assert_eq!(
            sched.pin_driving_node(node),
            Some(('B', 4)),
            "pin_driving_node must return the deterministic lowest pin on a shared net"
        );
    }

    /// Regression (R47): `Mcu::on_i2c` and `set_i2c_slave_addresses` are
    /// SINGLE-SLOT on the core, so a per-bus closure would let a second
    /// `attach_i2c_bus` silently disconnect the first bus; its slave would
    /// never see firmware traffic and its addresses would vanish from the TWI
    /// filter.
    /// The dispatcher must route each event by its 7-bit address to the bus
    /// that owns it, and the filter must be the union across attached buses.
    #[test]
    fn attach_two_i2c_buses_routes_firmware_bytes_by_address() {
        use crate::peripherals::i2c::Eeprom24c;
        use hauksbee_mcu::I2cEvent as E;

        let (mut sched, h) = sched_with_capturing_core(&[]);
        let bus1 = Arc::new(Mutex::new(
            I2cBus::new("U2").with_slave(Box::new(Eeprom24c::new(0x50, 64))),
        ));
        let bus2 = Arc::new(Mutex::new(
            I2cBus::new("U3").with_slave(Box::new(Eeprom24c::new(0x48, 64))),
        ));
        sched.attach_i2c_bus(bus1.clone());
        sched.attach_i2c_bus(bus2.clone());

        // TWI address filter: the LAST install (the one the core keeps) must
        // carry BOTH buses' addresses, not just the last-attached bus's.
        let addrs = h
            .i2c_addrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(
            addrs.contains(&0x50) && addrs.contains(&0x48),
            "TWI filter must be the union of all attached buses' addresses, got {addrs:#04x?}"
        );

        // Firmware writes 0xAB at word address 0 of the FIRST bus's EEPROM.
        let mut slot = h.i2c_cb.lock().unwrap_or_else(|e| e.into_inner());
        let cb = slot.as_mut().expect("on_i2c handler installed");
        cb(E::Start {
            addr: 0x50,
            read: false,
        });
        cb(E::Write {
            addr: 0x50,
            data: 0x00,
        });
        cb(E::Write {
            addr: 0x50,
            data: 0x00,
        });
        cb(E::Write {
            addr: 0x50,
            data: 0xAB,
        });
        cb(E::Stop { addr: 0x50 });
        // ... and reads it back (repeated START read).
        cb(E::Start {
            addr: 0x50,
            read: false,
        });
        cb(E::Write {
            addr: 0x50,
            data: 0x00,
        });
        cb(E::Write {
            addr: 0x50,
            data: 0x00,
        });
        cb(E::Start {
            addr: 0x50,
            read: true,
        });
        let read_back = cb(E::Read { addr: 0x50 });
        cb(E::Stop { addr: 0x50 });
        drop(slot);

        let b1 = bus1.lock().unwrap_or_else(|e| e.into_inner());
        let b2 = bus2.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            b1.slave::<Eeprom24c>(0x50).expect("eeprom@0x50").contents()[0],
            0xAB,
            "the FIRST-attached bus must receive firmware dispatch for its address"
        );
        assert_eq!(
            read_back,
            Some(0xAB),
            "a firmware READ from the first bus's slave must answer"
        );
        assert!(
            b2.slave::<Eeprom24c>(0x48)
                .expect("eeprom@0x48")
                .contents()
                .iter()
                .all(|&b| b == 0xFF),
            "the second bus's slave must be untouched by traffic addressed to the first"
        );
    }

    /// Regression (R47): `Mcu::on_spi` is SINGLE-SLOT, so a per-bus closure
    /// would let a second `attach_spi_bus` silently disconnect the first, and
    /// every firmware byte would go to the LAST-attached slave regardless
    /// of which chip-select was asserted. The dispatcher must forward each
    /// byte to the bus whose CS is currently asserted (per its cs_frame).
    #[test]
    fn attach_two_spi_buses_routes_bytes_to_the_cs_selected_bus() {
        struct RecSlave {
            got: Arc<Mutex<Vec<u8>>>,
            miso: u8,
        }
        impl crate::peripherals::spi::SpiSlave for RecSlave {
            fn transfer(&mut self, mosi: u8) -> u8 {
                self.got
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(mosi);
                self.miso
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }

        let cs1 = ('C', 0u8);
        let cs2 = ('C', 1u8);
        let (mut sched, h) = sched_with_capturing_core(&[cs1, cs2]);
        let got1 = Arc::new(Mutex::new(Vec::new()));
        let got2 = Arc::new(Mutex::new(Vec::new()));
        let bus1 = Arc::new(Mutex::new(SpiBus::new(
            "U2",
            Box::new(RecSlave {
                got: got1.clone(),
                miso: 0x5A,
            }),
        )));
        let bus2 = Arc::new(Mutex::new(SpiBus::new(
            "U3",
            Box::new(RecSlave {
                got: got2.clone(),
                miso: 0xA5,
            }),
        )));
        sched.attach_spi_bus(bus1, Some(cs1), None);
        sched.attach_spi_bus(bus2, Some(cs2), None);

        let edge = |pin: (char, u8), high: bool| {
            let mut slot = h.pin_cb.lock().unwrap_or_else(|e| e.into_inner());
            (slot.as_mut().expect("on_pin_change installed"))(
                PinId {
                    port: pin.0,
                    bit: pin.1,
                },
                high,
                0,
            );
        };
        let xfer = |mosi: u8| -> u8 {
            let mut slot = h.spi_cb.lock().unwrap_or_else(|e| e.into_inner());
            (slot.as_mut().expect("on_spi handler installed"))(hauksbee_mcu::SpiEvent {
                mosi,
                deselect: false,
                cycle: 0,
            })
        };
        let bytes = |g: &Arc<Mutex<Vec<u8>>>| g.lock().unwrap_or_else(|e| e.into_inner()).clone();

        // Firmware asserts the FIRST slave's CS (active-low falling edge) and
        // clocks a byte: it must reach slave 1, not slave 2, and slave 1's
        // MISO byte must come back.
        edge(cs1, false);
        let miso = xfer(0x42);
        assert_eq!(
            bytes(&got1),
            vec![0x42],
            "the byte must reach the FIRST bus's slave (its CS is asserted)"
        );
        assert!(
            bytes(&got2).is_empty(),
            "the deselected second slave must see no traffic, got {:02x?}",
            bytes(&got2)
        );
        assert_eq!(miso, 0x5A, "MISO must come from the selected (first) slave");

        // Deselect the first, select the second: bytes now route to slave 2.
        edge(cs1, true);
        edge(cs2, false);
        let miso = xfer(0x99);
        assert_eq!(
            bytes(&got2),
            vec![0x99],
            "byte must follow the newly asserted CS"
        );
        assert_eq!(
            bytes(&got1),
            vec![0x42],
            "the deselected first slave must see nothing more"
        );
        assert_eq!(
            miso, 0xA5,
            "MISO must come from the selected (second) slave"
        );
    }

    // ── run_micros integer-carry + failure accounting (SCHED-1 / SCHED-2) ────

    /// A mock core that records every integer-microsecond count handed to
    /// `run_micros`, and can be told to refuse the advance (return `Err`).
    struct MicrosCore {
        micros: Arc<Mutex<Vec<u64>>>,
        fail: bool,
    }

    impl Mcu for MicrosCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            Ok(n)
        }
        fn run_micros(&mut self, us: u64) -> anyhow::Result<()> {
            self.micros
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(us);
            if self.fail {
                anyhow::bail!("mock core refuses to advance");
            }
            Ok(())
        }
        fn frequency(&self) -> u64 {
            16_000_000
        }
        fn set_digital_in(&mut self, _pin: PinId, _high: bool) {}
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, _cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {}
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: Box<dyn FnMut(u8) + Send>) {}
        fn on_i2c(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>) {}
        fn on_spi(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>) {}
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: 0,
                cycles: 0,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
    }

    /// Wire a bare `MicrosCore` (no drivers) onto a solvable board and return
    /// the scheduler plus the shared micros-log handle.
    fn sched_with_micros_core(fail: bool) -> (Scheduler, Arc<Mutex<Vec<u64>>>) {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let micros = Arc::new(Mutex::new(Vec::new()));
        let core = MicrosCore {
            micros: micros.clone(),
            fail,
        };
        let binding = McuBinding {
            reference: "U1".into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers: HashMap::new(),
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        sched.mcus.push(core_with_hooks(Box::new(core), binding));
        sched.responder_registries.push(None);
        sched.relayout();
        (sched, micros)
    }

    /// Trait-level mock whose only interesting property is whether it claims
    /// pin drive direction is observable, for proving the scheduler's
    /// conservative-AND aggregation without any emulator backend.
    struct DirCore {
        observable: bool,
    }

    impl Mcu for DirCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            Ok(n)
        }
        fn run_micros(&mut self, _us: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn frequency(&self) -> u64 {
            16_000_000
        }
        fn set_digital_in(&mut self, _pin: PinId, _high: bool) {}
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, _cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {}
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: Box<dyn FnMut(u8) + Send>) {}
        fn on_i2c(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>) {}
        fn on_spi(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>) {}
        fn drive_direction_observable(&self) -> bool {
            self.observable
        }
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: 0,
                cycles: 0,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
    }

    /// `Scheduler::drive_direction_observable` is the conservative AND across
    /// live cores: vacuously true with no MCUs (matching the old
    /// `!has_external_backend()` proxy), true while every core reports
    /// direction, and false the moment ONE direction-blind core joins, a
    /// boot-state check must then hedge rather than assert Hi-Z.
    #[test]
    fn drive_direction_observable_ands_across_cores() {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let binding = |reference: &str| McuBinding {
            reference: reference.into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers: HashMap::new(),
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };

        assert!(
            sched.drive_direction_observable(),
            "no MCUs: vacuously observable (no pin whose direction could be misread)"
        );

        sched.mcus.push(core_with_hooks(
            Box::new(DirCore { observable: true }),
            binding("U1"),
        ));
        sched.responder_registries.push(None);
        assert!(
            sched.drive_direction_observable(),
            "one direction-reporting core keeps the run observable"
        );

        sched.mcus.push(core_with_hooks(
            Box::new(DirCore { observable: false }),
            binding("U2"),
        ));
        sched.responder_registries.push(None);
        assert!(
            !sched.drive_direction_observable(),
            "one direction-blind core must make the whole run unobservable"
        );
    }

    /// The live-scope honesty flag: on a direction-blind core, a net whose
    /// MCU pin driver never reported a level must be listed as unobserved
    /// (its shown voltage is the passive network's static level, not a
    /// measured drive); the flag clears the moment the driver is enabled,
    /// and a direction-reporting core never populates it (there, an undriven
    /// pin's level IS a real measurement).
    #[test]
    fn unobserved_drive_nets_flags_only_direction_blind_undriven_pins() {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PLAIN_INPUT_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let node = *sched.net_nodes.get("BTN_HI").expect("net exists");
        let make_binding = |sched: &mut Scheduler, observable: bool| {
            let mut drv =
                crate::drivers::PinDriver::stamp(&mut sched.circuit, node, "BTN_HI", "t", 50.0);
            // Mirror bind_mcu: every driver starts tri-stated until the
            // firmware's first observed drive enables it.
            drv.set_enabled(&mut sched.circuit, false);
            let mut gpio_drivers = HashMap::new();
            gpio_drivers.insert(('0', 1u8), drv);
            McuBinding {
                reference: if observable { "U1" } else { "U2" }.into(),
                backend: "test".into(),
                requested_part: String::new(),
                pad_roles: HashMap::new(),
                role_nets: HashMap::new(),
                gpio_drivers,
                adc_nets: HashMap::new(),
                adc_pin: HashMap::new(),
                module: false,
                max_supply_v: None,
            }
        };

        // Direction-reporting core: nothing is unobserved, tri-stated or not.
        let binding = make_binding(&mut sched, true);
        sched.mcus.push(core_with_hooks(
            Box::new(DirCore { observable: true }),
            binding,
        ));
        sched.responder_registries.push(None);
        assert!(
            sched.unobserved_drive_nets().is_empty(),
            "a direction-observable backend's undriven pin is a real measurement"
        );

        // Direction-blind core with a never-driven pin: its net is disclosed.
        let binding = make_binding(&mut sched, false);
        sched.mcus.push(core_with_hooks(
            Box::new(DirCore { observable: false }),
            binding,
        ));
        sched.responder_registries.push(None);
        assert_eq!(
            sched.unobserved_drive_nets(),
            vec!["BTN_HI".to_string()],
            "a direction-blind, never-driven pin's net must be disclosed"
        );

        // The moment ANY driver on the net is enabled (an observed drive),
        // the reading is a driven measurement and the flag clears.
        {
            let m = sched.mcus.last_mut().expect("mcu");
            let drv = m.binding.gpio_drivers.get_mut(&('0', 1u8)).expect("driver");
            drv.enabled = true;
        }
        assert!(
            sched.unobserved_drive_nets().is_empty(),
            "an enabled driver makes the net a driven measurement"
        );
    }

    /// Regression for SCHED-1: `run_micros` takes integer microseconds, so a
    /// chunk whose duration is a fractional number of microseconds must carry
    /// the truncated remainder into the next chunk, otherwise the firmware
    /// clock drifts from sim time by up to ~1 µs per chunk. Ten 1.3 µs chunks
    /// are 13.0 µs of true time; the delivered microseconds must sum to within
    /// one microsecond of that. Rounding `(chunk * 1e6)` per chunk with no
    /// carry delivers 1 µs each (10 µs total), 3 µs, ~23 %, of pure drift.
    #[test]
    fn fractional_microsecond_chunks_do_not_drift_the_mcu_clock() {
        let (mut sched, micros) = sched_with_micros_core(false);
        let mut uart = HashMap::new();
        let chunk = 1.3e-6; // 1.3 µs, not a whole number of microseconds
        for _ in 0..10 {
            sched.run_chunk(chunk, &mut uart);
        }
        let delivered: u64 = micros
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .sum();
        let true_us = 10.0 * 1.3; // 13.0 µs
        assert!(
            (delivered as f64 - true_us).abs() <= 1.0,
            "carried integer microseconds must track true elapsed time within 1 µs: \
             delivered {delivered} µs vs {true_us} µs true"
        );
        // And it must NOT be the naive per-chunk round (which loses time every
        // chunk): 10 chunks rounding to 1 µs would deliver only 10 µs.
        assert!(
            delivered >= 12,
            "per-chunk rounding without carry systematically undercounts: got {delivered} µs"
        );
    }

    /// Regression for SCHED-1b: a PERSISTENTLY sub-microsecond chunk (a fine
    /// `fixed_dt = 0.5e-6`) must not race the firmware clock ahead of sim time.
    /// A `.floor().max(1.0)` delivers 1 µs every chunk while sim time
    /// advances only 0.5 µs, banking unrepayable negative debt, an unbounded 2x
    /// drift. Twenty 0.5 µs chunks are 10.0 µs of true time; the delivered
    /// microseconds must sum to within one microsecond of that, NOT the 20 µs
    /// the min-1 clamp would inject.
    #[test]
    fn sub_microsecond_chunks_do_not_race_the_mcu_clock_ahead() {
        let (mut sched, micros) = sched_with_micros_core(false);
        let mut uart = HashMap::new();
        let chunk = 0.5e-6; // 0.5 µs, persistently under one microsecond
        for _ in 0..20 {
            sched.run_chunk(chunk, &mut uart);
        }
        let delivered: u64 = micros
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .sum();
        let true_us = 20.0 * 0.5; // 10.0 µs
        assert!(
            (delivered as f64 - true_us).abs() <= 1.0,
            "sub-µs chunks must not inject time the sim never elapsed: \
             delivered {delivered} µs vs {true_us} µs true"
        );
        // A min-1 clamp delivers one microsecond per chunk (20 µs total),
        // double the true elapsed time. Guard against that regression.
        assert!(
            delivered <= 11,
            "min-1 clamp races the firmware clock ahead: got {delivered} µs for {true_us} µs true"
        );
    }

    /// Regression for SCHED-2: swallowing a `run_micros` error with
    /// `let _ = ...` leaves an MCU that refused to advance (crashed core,
    /// backend transport error) looking like a normal quiet chunk even
    /// though the firmware side never executed. The failure must feed the same
    /// failed-chunk / `analog_valid` surface the analog march uses, so strict
    /// and CI consumers refuse to trust the window.
    #[test]
    fn mcu_refusing_to_advance_marks_the_chunk_failed() {
        let (mut sched, _micros) = sched_with_micros_core(true);
        assert!(sched.analog_valid(), "clean before any chunk runs");
        let mut uart = HashMap::new();
        sched.run_chunk(1e-4, &mut uart);
        assert!(
            sched.failed_chunk_count() >= 1,
            "an MCU that errored out of run_micros must record a failed chunk, \
             not report a fake-quiet run"
        );
        assert!(
            !sched.analog_valid(),
            "a swallowed MCU failure must flip analog_valid so CI/strict refuse it"
        );
        assert!(
            !sched.failed_windows().is_empty(),
            "the failed chunk's sim-time window must be surfaced for consumers"
        );
    }

    /// R15: a sustained MCU crash (run_micros Err) while the analog march keeps
    /// converging must trip the strict/CI abort. Resetting the
    /// consecutive-failure streak inside solve_chunk on analog convergence,
    /// BEFORE run_chunk re-records the MCU failure, makes each such chunk zero
    /// the streak and then bump it back to 1, capping
    /// max_consecutive_failed_chunks at 1 so the abort threshold is never
    /// reached. The reset therefore happens only on a fully-successful chunk
    /// (analog converged AND MCU advanced).
    #[test]
    fn sustained_mcu_failure_trips_the_strict_abort() {
        let (mut sched, _micros) = sched_with_micros_core(true);
        let mut uart = HashMap::new();
        // The board's passive analog solve converges every chunk; only the MCU
        // refuses to advance. Run past the abort threshold.
        for _ in 0..STRICT_CONSECUTIVE_FAILED_ABORT {
            sched.run_chunk(1e-4, &mut uart);
        }
        assert!(
            sched.analog_abort_tripped(),
            "an MCU crashing for {} consecutive chunks must trip the strict/CI abort, \
             not be capped at a streak of 1",
            STRICT_CONSECUTIVE_FAILED_ABORT
        );
    }

    /// Regression for the engine `reset` bug: a reset that zeroed only
    /// `sim_time` left the previous run's failed-chunk count, failed windows,
    /// consecutive streak, and clock carry in place, so a re-run inherited a
    /// stale `analog_valid:false` and phantom failed windows. `reset_run_state`
    /// must wipe every run-accumulated diagnostic back to a clean-run state.
    #[test]
    fn reset_run_state_clears_stale_failure_accounting() {
        let (mut sched, _micros) = sched_with_micros_core(true);
        let mut uart = HashMap::new();
        sched.run_chunk(1e-4, &mut uart);
        assert!(
            sched.failed_chunk_count() >= 1,
            "first run recorded a failure"
        );
        assert!(!sched.analog_valid(), "first run is not clean");

        sched.reset_run_state();
        assert_eq!(sched.sim_time, 0.0, "reset restarts the sim clock");
        assert_eq!(
            sched.failed_chunk_count(),
            0,
            "reset must clear the stale failed-chunk count"
        );
        assert!(
            sched.failed_windows().is_empty(),
            "reset must clear the stale failed windows"
        );
        assert!(
            sched.analog_valid(),
            "a reset scheduler must report clean until the NEXT run fails"
        );
        assert!(
            !sched.analog_abort_tripped(),
            "reset must clear the consecutive-failure streak"
        );
    }
}

/// The product-path wiring the fresh-context critic proved missing: backend
/// instantiation for `renode:<part>` consults the SoC-descriptor override
/// dirs through `SocConfig::resolve`, override beats builtin, an invalid
/// override for the requested part fails loudly, aliases fall back to their
/// canonical descriptors, and an unknown part's error enumerates the dirs
/// searched. One test fn: it mutates HAUKSBEE_MCU_DIR, which must not be
/// visible to a concurrently running test.
#[cfg(all(test, feature = "renode"))]
mod soc_wiring_tests {
    use super::resolve_renode_config;

    #[test]
    fn renode_instantiation_resolves_through_override_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "hauksbee-engine-socwire-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // A brand-new part, purely as data (an F101 sibling of the F103).
        let f101 = include_str!("../../hauksbee-mcu/db/mcu/stm32f103.soc.toml").replace(
            "mcu_label = \"STM32F103 (ARM Cortex-M3)\"",
            "mcu_label = \"STM32F101 (ARM Cortex-M3)\"",
        );
        std::fs::write(dir.join("stm32f101.soc.toml"), &f101).unwrap();

        // An INVALID override for the BUILTIN part sifive_fe310 (typo'd
        // field). rp2040 is left unshadowed so the `pico` alias below
        // exercises the clean canonical-descriptor fallback.
        let broken = include_str!("../../hauksbee-mcu/db/mcu/sifive_fe310.soc.toml")
            .replace("platform_repl =", "platform_rep =");
        std::fs::write(dir.join("sifive_fe310.soc.toml"), &broken).unwrap();

        // SAFETY (edition 2021): set_var is safe; no other test in this binary
        // reads HAUKSBEE_MCU_DIR or resolves these parts.
        std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
        let new_part = resolve_renode_config("stm32f101");
        let invalid_override = resolve_renode_config("sifive_fe310");
        let alias = resolve_renode_config("pico");
        let missing = resolve_renode_config("stm32f199");
        std::env::remove_var("HAUKSBEE_MCU_DIR");
        std::fs::remove_dir_all(&dir).ok();

        // The new part came from the override dir.
        assert_eq!(
            new_part.expect("new part resolves").mcu_label,
            "STM32F101 (ARM Cortex-M3)"
        );

        // The invalid override for a builtin name FAILS LOUDLY (never falls
        // back to the embedded fe310), naming the file and the typo'd field.
        let err = invalid_override
            .expect_err("invalid override must fail")
            .to_string();
        assert!(err.contains("sifive_fe310.soc.toml"), "err: {err}");
        assert!(err.contains("platform_rep"), "err: {err}");

        // The legacy alias reaches its canonical descriptor... but only after
        // trying `pico` verbatim, which the override dir did not shadow.
        assert_eq!(alias.expect("alias resolves").machine, "rp2040");

        // Unknown part with no descriptor anywhere: the error enumerates the
        // dirs searched.
        let err = missing.expect_err("unknown part must fail").to_string();
        assert!(err.contains("no SoC descriptor found"), "err: {err}");
        assert!(err.contains("renode:stm32f199"), "err: {err}");
    }
}

#[cfg(test)]
mod pulse_and_contention_tests {
    use super::*;
    use hauksbee_mcu::{Mcu, PinId};
    use hauksbee_solve::SolverOptions;

    // ── Sub-chunk pulse warning (friction 1.16) + runtime driver contention ──

    /// A mock core whose cycle counter advances at 16 MHz per `run_micros`, so
    /// a drained edge log normalises over a REAL cycle span. (The
    /// [`RecordingCore`]'s default `current_cycle` is a constant 0, which
    /// yields an empty span and would suppress the pulse-width math entirely.)
    struct CycleCore {
        cycles: u64,
    }

    impl Mcu for CycleCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            self.cycles += n;
            Ok(n)
        }
        fn run_micros(&mut self, us: u64) -> anyhow::Result<()> {
            self.cycles += 16 * us;
            Ok(())
        }
        fn frequency(&self) -> u64 {
            16_000_000
        }
        fn current_cycle(&self) -> u64 {
            self.cycles
        }
        fn set_digital_in(&mut self, _pin: PinId, _high: bool) {}
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, _cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {}
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: Box<dyn FnMut(u8) + Send>) {}
        fn on_i2c(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>) {}
        fn on_spi(&mut self, _cb: Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>) {}
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: 0,
                cycles: self.cycles,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
    }

    /// A 74HC74 dual flip-flop whose clock (pad 3 = `clk1`) sits on STROBE and
    /// data (pad 2 = `d1`) on DATA, plus a FREE net wired only to a pull-down:
    /// the minimal board on which a sub-chunk pulse can hit (a) a net that
    /// clocks a tick-evaluated sequential part and (b) a net that clocks
    /// nothing.
    const PULSE_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "STROBE")
  (net 4 "DATA")
  (net 5 "FREE")

  (module Logic:74HC74 (layer F.Cu)
    (at 100 100)
    (fp_text reference U5 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC74 (at 0 2) (layer F.Fab))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 4 "DATA"))
    (pad 3 thru_hole circle (at 0 3) (size 1 1) (net 3 "STROBE"))
    (pad 7 thru_hole circle (at 0 7) (size 1 1) (net 1 "GND"))
    (pad 14 thru_hole circle (at 0 14) (size 1 1) (net 2 "+5V"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 5 "FREE"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

    /// Build the PULSE_BOARD scheduler with one hand-wired mock MCU ("A1",
    /// [`CycleCore`]) owning a tri-stated GPIO driver per (pin, net) pair,
    /// exactly the shape the binder stamps.
    fn pulse_scheduler(pins: &[((char, u8), &str)]) -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(PULSE_BOARD).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let mut gpio_drivers = HashMap::new();
        for &(pin, net) in pins {
            let node = sched.net_nodes[net];
            let mut drv = crate::drivers::PinDriver::stamp(
                &mut sched.circuit,
                node,
                net,
                &format!("t_{}{}", pin.0, pin.1),
                crate::drivers::DEFAULT_RO,
            );
            drv.set_enabled(&mut sched.circuit, false);
            gpio_drivers.insert(pin, drv);
        }
        let binding = McuBinding {
            reference: "A1".into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        sched
            .mcus
            .push(core_with_hooks(Box::new(CycleCore { cycles: 0 }), binding));
        sched.responder_registries.push(None);
        sched.relayout();
        sched
    }

    /// Preload the mock MCU's shared capture state with an already-happened
    /// GPIO transition sequence, the exact shape `on_pin_change` accumulates;
    /// the next `step` drains it like a real firmware chunk.
    fn preload_edges(sched: &Scheduler, pin: (char, u8), transitions: &[(u64, bool)]) {
        let mut sh = sched.mcus[0]
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for &(cycle, level) in transitions {
            sh.pin_edges.insert(pin, level);
            sh.pin_edge_log.push(PinEdge {
                cycle,
                port: pin.0,
                bit: pin.1,
                level,
            });
        }
    }

    /// THE FRICTION-1.16 CASE: a 2 us GPIO pulse (rise + fall inside one
    /// 100 us chunk) on the net clocking a tick-evaluated 74HC74 must warn,
    /// naming the net, the measured width, the chunk, and the part at risk;
    /// and it must warn ONCE per net per run, not once per chunk.
    #[test]
    fn subchunk_pulse_on_a_tick_sequential_clock_net_warns_once() {
        let mut sched = pulse_scheduler(&[(('B', 1), "STROBE")]);

        // 2 us pulse at 16 MHz = 32 cycles, wholly inside the chunk's
        // [0, 1600) cycle span.
        preload_edges(&sched, ('B', 1), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);

        let pulses = sched.short_pulses();
        assert_eq!(pulses.len(), 1, "exactly one warning: {pulses:?}");
        let p = &pulses[0];
        assert_eq!(p.net, "STROBE");
        assert_eq!(p.mcu_ref, "A1");
        assert_eq!((p.port, p.bit), ('B', 1));
        assert_eq!(p.parts, vec!["U5".to_string()]);
        assert!(
            (p.pulse_s - 2e-6).abs() < 1e-9,
            "measured width must be the 2 us cycle gap, got {}",
            p.pulse_s
        );
        assert!((p.chunk_s - DEFAULT_CHUNK_S).abs() < 1e-12);
        let msg = p.message();
        for needle in [
            "STROBE",
            "U5",
            "2.0 us",
            "100.0 us",
            "--chunk-us",
            "follow-up",
        ] {
            assert!(msg.contains(needle), "message must name '{needle}': {msg}");
        }

        // A second pulse train on the same net must NOT append a second record.
        preload_edges(&sched, ('B', 1), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert_eq!(sched.short_pulses().len(), 1, "once per net per run");
    }

    /// The same pulse stretched to ~1 ms (rise in one chunk, fall ten chunks
    /// later) is OBSERVED by the chunk-boundary sample, so it must NOT warn;
    /// and a sub-chunk pulse on a net that clocks nothing sequential must not
    /// warn either. Zero-false-positive discipline for the 1.16 warning.
    #[test]
    fn spanning_pulse_and_non_clock_net_stay_silent() {
        // Rise in chunk 0, fall in chunk 10: every chunk carries at most ONE
        // transition, so no completed pulse ever falls inside a chunk.
        let mut sched = pulse_scheduler(&[(('B', 1), "STROBE")]);
        preload_edges(&sched, ('B', 1), &[(100, true)]);
        sched.step(DEFAULT_CHUNK_S);
        for _ in 0..9 {
            sched.step(DEFAULT_CHUNK_S);
        }
        preload_edges(&sched, ('B', 1), &[(100, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert!(
            sched.short_pulses().is_empty(),
            "a chunk-spanning pulse is observed and must not warn: {:?}",
            sched.short_pulses()
        );

        // A 2 us pulse on FREE (a pull-down only, no sequential part) is
        // electrically fine at any width: silent.
        let mut sched = pulse_scheduler(&[(('B', 2), "FREE")]);
        preload_edges(&sched, ('B', 2), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert!(
            sched.short_pulses().is_empty(),
            "a pulse on a net clocking nothing must not warn: {:?}",
            sched.short_pulses()
        );
    }

    /// THE FIELD CASE, runtime half: a 74HC08 output (pad 3 = `y1`) on net
    /// SHARED that the firmware also drives as a GPIO output. The static lint
    /// proves this unreachable statically
    /// (`checks::contention::tests::field_case_model_vs_mcu_gpio_is_out_of_static_reach`);
    /// here the scheduler catches it the moment the pin becomes a DRIVING
    /// output, once per net per run. Before the firmware drives the pin
    /// (tri-stated MCU driver, the binder's stamped default) it must stay
    /// silent: a gate output feeding an MCU input is the most common healthy
    /// topology there is.
    const CONTENTION_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "SHARED")
  (net 4 "INA")
  (net 5 "INB")

  (module Logic:74HC08 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC08 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 1) (size 1 1) (net 4 "INA"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 5 "INB"))
    (pad 3 thru_hole circle (at 0 3) (size 1 1) (net 3 "SHARED"))
    (pad 7 thru_hole circle (at 0 7) (size 1 1) (net 1 "GND"))
    (pad 14 thru_hole circle (at 0 14) (size 1 1) (net 2 "+5V"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 4 "INA"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
  (module Resistor:R2 (layer F.Cu)
    (at 120 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 5 "INB"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

    fn contention_scheduler(board_text: &str, pin: (char, u8), net: &str) -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(board_text).expect("board");
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = crate::binder::bind_board(&board, &lib);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let node = sched.net_nodes[net];
        let mut gpio_drivers = HashMap::new();
        let mut drv = crate::drivers::PinDriver::stamp(
            &mut sched.circuit,
            node,
            net,
            &format!("t_{}{}", pin.0, pin.1),
            crate::drivers::DEFAULT_RO,
        );
        drv.set_enabled(&mut sched.circuit, false);
        gpio_drivers.insert(pin, drv);
        let binding = McuBinding {
            reference: "A1".into(),
            backend: "simavr:test".into(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        };
        sched
            .mcus
            .push(core_with_hooks(Box::new(CycleCore { cycles: 0 }), binding));
        sched.responder_registries.push(None);
        sched.relayout();
        sched
    }

    #[test]
    fn firmware_output_fighting_an_enabled_model_output_fires_once() {
        let mut sched = contention_scheduler(CONTENTION_BOARD, ('B', 1), "SHARED");

        // Tri-stated MCU pin (firmware never drove it): the 74HC08 driving
        // SHARED alone is the healthy gate-feeds-MCU-input topology. Silent.
        sched.step(2.0 * DEFAULT_CHUNK_S);
        assert!(
            sched.driver_contentions().is_empty(),
            "a tri-stated MCU pin is not contention: {:?}",
            sched.driver_contentions()
        );

        // The firmware drives PB1 (a pin-change edge enables the driver):
        // two push-pull drivers now share SHARED. Fires, once.
        preload_edges(&sched, ('B', 1), &[(10, true)]);
        sched.step(DEFAULT_CHUNK_S);
        let found = sched.driver_contentions();
        assert_eq!(found.len(), 1, "exactly one finding: {found:?}");
        let c = &found[0];
        assert_eq!(c.net, "SHARED");
        assert_eq!(c.mcu_ref, "A1");
        assert_eq!((c.port, c.bit), ('B', 1));
        assert_eq!(c.parts, vec!["U1.y1".to_string()]);
        assert!(
            c.t_s > 0.0,
            "detection is skipped on the unsolved first chunk"
        );
        let msg = c.message();
        for needle in ["SHARED", "U1.y1", "PB1", "models resolve", "pin-direction"] {
            assert!(msg.contains(needle), "message must name '{needle}': {msg}");
        }

        // Still fighting next chunk: once per net per run, no growth.
        sched.step(2.0 * DEFAULT_CHUNK_S);
        assert_eq!(sched.driver_contentions().len(), 1, "once per net per run");
    }

    /// A 74HC125 whose `y1` shares a net with a firmware-driven pin but whose
    /// own `oe_n_1` is tied HIGH (tri-stated, released): the model output is
    /// NOT driving, so there is no fight and the monitor must stay silent.
    /// This is the runtime mirror of the static check's tri-state exclusion,
    /// derived from the same `[models.logic.tristate]` spec via the driver's
    /// live `enabled` flag.
    #[test]
    fn tristated_model_output_is_not_contention() {
        const TRISTATE_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "BUS")
  (net 4 "INA")

  (module Logic:74HC125 (layer F.Cu)
    (at 100 100)
    (fp_text reference U2 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC125 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 1) (size 1 1) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 4 "INA"))
    (pad 3 thru_hole circle (at 0 3) (size 1 1) (net 3 "BUS"))
    (pad 7 thru_hole circle (at 0 7) (size 1 1) (net 1 "GND"))
    (pad 14 thru_hole circle (at 0 14) (size 1 1) (net 2 "+5V"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 4 "INA"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;
        let mut sched = contention_scheduler(TRISTATE_BOARD, ('B', 1), "BUS");

        // Chunk 0 solves the rails; from chunk 1 on the tick reads OE_n at
        // ~5 V and keeps y1 released. Then the firmware drives PB1: an output
        // into a released 3-state pin, the intended arrangement. Silent.
        sched.step(2.0 * DEFAULT_CHUNK_S);
        preload_edges(&sched, ('B', 1), &[(10, true)]);
        sched.step(3.0 * DEFAULT_CHUNK_S);
        assert!(
            sched.driver_contentions().is_empty(),
            "a tri-stated (OE-released) model output is not contention: {:?}",
            sched.driver_contentions()
        );
    }
}
