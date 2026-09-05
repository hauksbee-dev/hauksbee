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
//!    [`hauksbee_bind::drivers::PinDriver`]s,
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

use hauksbee_ir::evidence::{Assumption, AssumptionSource};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
#[cfg(feature = "avr")]
use hauksbee_mcu::AvrMcu;
use hauksbee_mcu::{Mcu, PinId};
use hauksbee_models::{Bus, PeripheralSpec};
use hauksbee_solve::{Layout, SolverOptions, Transient, TransientDiagnostics};

use crate::peripherals::{
    CsProvenance, Eeprom24c, I2cBus, PeripheralSet, RegisterMapSensor, ResolvedCs, SpiBus,
    SpiFramingMode, SpiNorFlash, TickCtx, TimelineEvent,
};
use hauksbee_bind::behavioral::BehavioralDevice;
use hauksbee_bind::binder::{apin_gpio_of_role, gpio_of_role, BoundBoard, McuBinding};
use hauksbee_bind::digital::{DigitalComponent, PinEdge};
use hauksbee_bind::power_supply::{PowerSupply, SupplyLeg};
use hauksbee_bind::stress::{FaultEvent, StressMonitor};

/// Default co-sim chunk size (seconds).
pub const DEFAULT_CHUNK_S: f64 = 100e-6;

/// Smallest useful poll slice supported by the MCU bridge. `Mcu::run_micros`
/// accepts an integer microsecond count; asking Renode/QEMU (which observe GPIO
/// only after that call) to poll more finely would execute zero guest time and
/// manufacture precision the bridge does not have.
pub const POLL_BACKEND_QUANTUM_S: f64 = 1e-6;

/// A timing claim requested by a CI spec or another strict caller.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TimingRequirement {
    /// Narrowest pulse the caller needs guaranteed observable, in seconds.
    pub min_pulse_s: Option<f64>,
    /// Largest acceptable edge timestamp uncertainty, in seconds.
    pub max_edge_error_s: Option<f64>,
}

/// Measured timing capability of one live MCU backend at the configured chunk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct TimingCoverage {
    pub mcu_ref: String,
    pub backend: String,
    /// Push callbacks carry exact cycles; poll backends carry slice boundaries.
    pub cycle_exact: bool,
    /// Worst-case timestamp uncertainty: one MCU cycle or one poll chunk.
    pub timestamp_precision_s: f64,
    /// Pulse width guaranteed to include an observable event. Push callbacks
    /// see a one-cycle pulse. Polling uses two slices so at least one poll lies
    /// strictly inside the pulse even when an edge coincides with a boundary.
    pub minimum_guaranteed_pulse_s: f64,
    /// Actual solver/MCU slice selected for the run.
    pub chunk_s: f64,
}

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
/// reporting fiction (held stale voltages), so it must refuse rather than fake.
pub const STRICT_CONSECUTIVE_FAILED_ABORT: u32 = 3;

/// Per-chunk thermal integral over the solver's accepted steps: each monitored
/// device's dissipated energy (J, index-aligned with the stress monitor's
/// metas) plus the simulated time it covers. Filled by `march_chunk`'s
/// streaming sink (trapezoid between accepted steps), deposited into
/// [`hauksbee_bind::stress::StressMonitor::deposit_chunk_energy`] only for the march
/// the chunk actually adopts. This is what makes the junction-temperature
/// check duty-cycle-exact for waveforms that switch inside a chunk: the
/// endpoint sample reads peak or zero depending on PWM phase, the integral
/// reads the energy actually deposited.
#[derive(Debug, Clone, Default)]
struct ChunkThermalAccum {
    /// Integrated dissipation per monitored device (J).
    energy_j: Vec<f64>,
    /// Simulated seconds the integral covers.
    elapsed_s: f64,
}

impl ChunkThermalAccum {
    /// Discard the partial integral (a failed rung's fiction).
    fn clear(&mut self) {
        self.energy_j.clear();
        self.elapsed_s = 0.0;
    }
}

/// Which fallback rung produced a chunk's converged answer after the primary
/// integration failed (see `Scheduler::solve_chunk`'s ladder, tried in this
/// order). A number obtained by a more dissipative method is not the same
/// number, so the rung is recorded per window and surfaced with its accuracy
/// cost rather than the chunk passing as a first-class solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkFallbackMethod {
    /// The primary integration re-run with the maximum step bounded to a small
    /// fraction of the chunk (the LTE controller was striding a stiff feature).
    ReducedStep,
    /// Backward Euler at the bounded step: L-stable, damps the trapezoidal
    /// ringing that kills a stiff chunk, at first-order accuracy.
    BackwardEuler,
    /// Backward Euler from a cold start: the warm seed is dropped so the DC
    /// operating point re-runs the full gmin/source-stepping continuation
    /// before the march.
    ColdStartBackwardEuler,
    /// The chunk subdivided into quarters, each quarter marched with backward
    /// Euler at the bounded step and seeded from the previous quarter's end.
    SubdividedBackwardEuler,
}

impl ChunkFallbackMethod {
    /// Stable machine-readable name (serialized into the co-sim JSON).
    pub fn as_str(&self) -> &'static str {
        match self {
            ChunkFallbackMethod::ReducedStep => "reduced-step",
            ChunkFallbackMethod::BackwardEuler => "backward-euler",
            ChunkFallbackMethod::ColdStartBackwardEuler => "cold-start-backward-euler",
            ChunkFallbackMethod::SubdividedBackwardEuler => "subdivided-backward-euler",
        }
    }

    /// Qualitative numerical-fidelity context for this rung: it names the
    /// rung's algorithmic trade-off and points at the measured number, which
    /// lives on `FallbackWindow::error_estimate_v`.
    pub fn fidelity_note(&self) -> &'static str {
        match self {
            ChunkFallbackMethod::ReducedStep => {
                "primary integration at a smaller maximum step; \
                 error_estimate_v carries the worst per-chunk measured \
                 estimate of the error each chunk's march added to its end \
                 state, from a companion re-solve at a shifted accuracy dial \
                 (absent when no companion converged)"
            }
            ChunkFallbackMethod::BackwardEuler
            | ChunkFallbackMethod::ColdStartBackwardEuler
            | ChunkFallbackMethod::SubdividedBackwardEuler => {
                "first-order backward Euler: numerically dissipative, so fast \
                 transients and ringing inside this window are damped relative \
                 to the second-order primary solve; error_estimate_v carries \
                 the worst per-chunk measured estimate of the error each \
                 chunk's march added to its end state, from a companion \
                 re-solve at a shifted accuracy dial (absent when no \
                 companion converged)"
            }
        }
    }
}

/// One sim-time window `[start_s, end_s)` whose answer a fallback rung
/// produced after the primary analog solve failed there, with the rung and the
/// measured accuracy record attached.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FallbackWindow {
    pub start_s: f64,
    pub end_s: f64,
    /// The rung that produced this window's answer.
    pub method: ChunkFallbackMethod,
    /// Measured estimate, in volts, of the CHUNK-END node-voltage error each
    /// chunk's march ADDED relative to its own chunk-start seed: the adopted
    /// rung's end state differenced against a companion re-solve of the same
    /// window with the step bounds AND the LTE/Newton tolerances scaled 4x
    /// (tighter first; a 4x-coarser leg when the tight companion will not
    /// converge), scaled by the worst-case Richardson factor (see
    /// `fallback_error_estimate`), doubled as a safety factor, plus the
    /// solver's own convergence-tolerance floor (reltol·|v| + vntol, below
    /// which no march can vouch for a digit). Merged windows carry the worst
    /// (largest) of their chunks' estimates, not their sum. `None` when
    /// neither companion converged: the window is still a real converged
    /// solve, but no empirical estimate was established for it.
    pub error_estimate_v: Option<f64>,
}

/// Live chip-select framing hook installed by [`Scheduler::attach_spi_bus`] when
/// the binder resolved a slave's CS net to an MCU pin. When `pin` toggles, the
/// `on_pin_change` closure frames `bus` from the real chip-select edge,
/// synchronously and in cycle order with the byte stream: on a push backend the
/// SPI byte IRQ and the CS GPIO IRQ both fire inside `avr_run`, so the
/// transaction boundaries land exactly where the firmware put them (mid-chunk
/// included) instead of at the chunk boundary. SPI CS is active-low by
/// convention: a falling edge asserts, a rising edge deasserts.
struct CsFrame {
    pin: (char, u8),
    active_low: bool,
    bus: Arc<Mutex<SpiBus>>,
}

/// Captured state shared between an MCU's C callbacks and the scheduler.
#[derive(Default)]
struct McuShared {
    /// Pin edges since last drain: (port, bit) -> latest level. Used by
    /// ordinary GPIO drivers (which only care about the final level a chunk
    /// settles to) and diagnostics / frame state.
    pin_edges: HashMap<(char, u8), bool>,
    /// Ordered, cycle-stamped log of EVERY pin transition since the last drain,
    /// in the order the firmware produced them. Unlike `pin_edges` (a
    /// latest-level map, which collapses a sub-µs `shiftOut` SCLK pulse train to
    /// its final level), this preserves each edge and its MCU cycle so the
    /// bit-banged digital layer replays at edge granularity in cycle order.
    pin_edge_log: Vec<PinEdge>,
    /// UART bytes the firmware emitted since last drain.
    uart_out: Vec<u8>,
    /// Live CS-framing hooks for SPI slaves whose CS net resolved to a pin on
    /// this MCU. Consulted by the `on_pin_change` closure so a CS edge frames
    /// its bus in true cycle order with the byte transfers. Empty on the
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
    /// previous chunk (`pins_configured_output`). Tracked so a pin the firmware
    /// switches from output back to input (DDR output→input, e.g. an open-drain
    /// bus hand-off) gets its Thevenin driver disabled again; without the
    /// release a handed-off net stays clamped at its stale driven level.
    /// Backends that cannot report direction return an empty set, so this stays
    /// empty there and the release is a no-op.
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

/// Scheduler-side analogue projection of one edge-synchronous parallel
/// memory. Firmware reads from the responder inside its own instruction loop;
/// this handle applies the responder's final bus state to the circuit before
/// the analogue solve so reports and contention checks see the same device.
struct ParallelMemoryDrive {
    component: usize,
    outputs: Vec<String>,
    runtime: Arc<Mutex<crate::responders::ParallelMemoryRuntime>>,
}

/// A parallel memory whose provenance checks passed, waiting to be registered
/// with its owning MCU's responder registry.
struct PendingParallelMemory {
    mcu: usize,
    component: usize,
    outputs: Vec<String>,
    output_pins: Vec<(char, u8)>,
    runtime: Arc<Mutex<crate::responders::ParallelMemoryRuntime>>,
    responder: crate::responders::ParallelMemoryResponder,
}

/// One model-declared firmware peripheral's electrical supply projection.
///
/// The bus model remains the authority for protocol work; this leg only drains
/// its typed read/write activity once per analogue chunk and selects the
/// datasheet current envelope. Supply voltage from the previous converged
/// operating point gates both the bus and the load, so an unpowered EEPROM
/// cannot ACK while drawing an invented active current.
enum ModelPeripheralBus {
    I2c(Arc<Mutex<I2cBus>>),
    Spi(Arc<Mutex<SpiBus>>),
}

struct ModelPeripheralPowerLeg {
    reference: String,
    isource: DeviceId,
    supply_node: NodeId,
    return_node: NodeId,
    power_on_threshold_v: f64,
    idle_a: f64,
    read_a: f64,
    write_a: f64,
    low_power_a: Option<f64>,
    bus: ModelPeripheralBus,
    powered: bool,
    last_current_a: f64,
}

impl ModelPeripheralPowerLeg {
    /// Supply current with no transaction in flight: zero while the rail is
    /// below the power-on threshold, else the datasheet idle (or deep-power-down)
    /// envelope.
    fn quiescent_a(&self, low_power: bool) -> f64 {
        if !self.powered {
            0.0
        } else if low_power {
            self.low_power_a.unwrap_or(self.idle_a)
        } else {
            self.idle_a
        }
    }

    /// Gate the bus model with the rail: an unpowered part cannot ACK.
    fn set_bus_powered(&self, powered: bool) {
        match &self.bus {
            ModelPeripheralBus::I2c(bus) => bus
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_powered(powered),
            ModelPeripheralBus::Spi(bus) => bus
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_powered(powered),
        }
    }

    /// Whether the bus is a SPI slave currently in a deep-power-down state.
    fn low_power_mode(&self) -> bool {
        match &self.bus {
            ModelPeripheralBus::I2c(_) => false,
            ModelPeripheralBus::Spi(bus) => bus
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .low_power_mode(),
        }
    }

    /// Discard any protocol activity the bus recorded but has not been charged
    /// for, so a replay starts from an idle envelope.
    fn drain_activity(&self) {
        match &self.bus {
            ModelPeripheralBus::I2c(bus) => {
                let _ = bus
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take_activity();
            }
            ModelPeripheralBus::Spi(bus) => {
                let _ = bus
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take_activity();
            }
        }
    }
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
    /// edge granularity. One controller PER physical chain (independent
    /// chains are not merged). The chips these drive are listed in `chain_chips`
    /// and are skipped by the once-per-chunk `digital` tick so they are not
    /// driven twice.
    chains: Vec<hauksbee_bind::digital::Hc595Chain>,
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
    /// Configurable power supplies, updated between chunks.
    pub supplies: Vec<SupplyLeg>,
    /// Behavioural devices (power ICs), updated between chunks the same way the
    /// supplies are.
    pub behavioral: Vec<BehavioralDevice>,
    /// Fault / stress monitor, evaluated after each chunk.
    pub stress: StressMonitor,
    /// Faults raised since the last frame drain.
    faults_pending: Vec<FaultEvent>,
    pub chunk_s: f64,
    pub opts: SolverOptions,
    /// Stop a multi-chunk [`Scheduler::step`] early once the strict-abort
    /// streak trips. Live sessions set this (the server ends the session on
    /// the failure; grinding out the rest of the frame's chunks would only
    /// delay that answer and block Pause/Reset); headless/CI runs keep the
    /// default `false` so their failed-window record covers the whole
    /// requested span.
    pub stop_when_dead: bool,
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
    /// Firmware-output transitions observed by the MCU bridge, keyed by the
    /// bound net. Unlike `stats`, this consumes the ordered edge log and cannot
    /// lose a pulse that returns to its starting level within one analog chunk.
    firmware_edge_toggles: HashMap<String, u64>,
    /// Net-attached / output peripherals (controls, VCD sinks), ticked each
    /// chunk around the analog solve.
    pub peripherals: PeripheralSet,
    /// I2C bus slaves, shared with each MCU's `on_i2c` callback.
    i2c_buses: Vec<Arc<Mutex<I2cBus>>>,
    /// SPI bus slaves, shared with each MCU's `on_spi` callback.
    spi_buses: Vec<Arc<Mutex<SpiBus>>>,
    /// Model-owned bus peripherals whose protocol activity is projected into
    /// the analogue supply network using source-bound idle/read/write current
    /// envelopes and rail-voltage power gating.
    model_peripheral_power: Vec<ModelPeripheralPowerLeg>,
    /// Per-controller SPI bus map (controller name -> bus). Populated by
    /// [`attach_spi_bus_on`]; not populated by the legacy [`attach_spi_bus`]
    /// path. Used for look-up by controller name after the run.
    spi_controller_map: HashMap<String, Arc<Mutex<SpiBus>>>,
    /// MCU chip-substitution events detected at build time: the board
    /// asked for a more specific part than the emulator core models. Surfaced as
    /// a co-sim warning + JSON note; never gates an exit code on its own.
    substitutions: Vec<McuSubstitution>,
    /// Causally scoped counterparts of `substitutions`. The event and exact
    /// board occurrence stay paired so later evidence construction cannot zip
    /// two independently ordered collections or fall back to display refs.
    scoped_substitutions: Vec<ScopedMcuSubstitution>,
    /// Bus peripherals (I2C/SPI slave models) attached on a board whose live
    /// MCU backends model NO matching bus controller: the firmware's bus
    /// traffic can never reach them, so they sit at their power-on defaults for
    /// the whole run. Recorded at attach time and surfaced as a co-sim coverage
    /// warning; a CI `peripheral` assertion against one of these FAILS rather
    /// than passing on the slave's untouched default state.
    unexercised_buses: Vec<UnexercisedBus>,
    /// MCU-bit-banged 74HC165 read chains, resolved at GPIO-output edge
    /// granularity inside the owning MCU's run loop via its synchronous input
    /// responder (the read-direction analogue of `chains`). Wrapped in
    /// Arc<Mutex<>> because the responder closure owns a clone. Each shares the
    /// `input_volts` snapshot the scheduler refreshes from the last solve so the
    /// 165 captures the latest spike-latch states on its PL load.
    hc165_chains: Vec<Arc<Mutex<hauksbee_bind::digital::Hc165Chain>>>,
    /// Per-MCU synchronous input-responder registries: the
    /// multiplexer that shares each MCU's single `on_input_responder` slot
    /// across every registered bit-banged input protocol (165 chains,
    /// bit-banged SPI MISO, soft-I2C). `None` until the first responder is
    /// registered for that MCU; the dispatch closure is installed lazily so a
    /// board with no responders keeps the backend hook empty (zero per-edge
    /// cost).
    responder_registries: Vec<Option<Arc<Mutex<crate::responders::ResponderRegistry>>>>,
    /// Parallel-memory parts whose bidirectional buses are answered at MCU
    /// edge granularity. Their component indices are skipped by ordinary tick
    /// and replay paths to prevent a stale, second write decision.
    parallel_memory_drives: Vec<ParallelMemoryDrive>,
    parallel_memory_chips: std::collections::HashSet<usize>,
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
    /// silently held and reported as a quiet run.
    failed_chunks: u64,
    /// Sim-time windows `[start_s, end_s)` of the failed chunks, merged where
    /// consecutive so a diverged stretch reads as its true extent. Surfaced in the
    /// co-sim JSON so a consumer knows exactly which span cannot be trusted.
    failed_windows: Vec<(f64, f64)>,
    /// The solver's own refusal message for each entry in `failed_windows`,
    /// parallel and always the same length. Holds the FIRST failure's message
    /// in a merged window, which is the one that started the divergence and so
    /// the one worth naming. Carries the solver's blame clause (the net that
    /// refused to settle, the devices on it, any near-zero-ohm link poisoning
    /// the matrix), so a non-convergence names the smallest identifiable thing
    /// instead of only a chunk count.
    failed_window_reasons: Vec<String>,
    /// Per-run count of chunks whose PRIMARY analog solve failed but a fallback
    /// integration rung produced a real converged answer (see `solve_chunk`'s
    /// ladder). A fallback-solved chunk is a solved chunk, so it counts toward
    /// neither `failed_chunks` nor the strict abort streak, but its number came
    /// from a more dissipative method, a smaller step or a subdivided march.
    /// The count and windows reach the co-sim JSON as `fallback_windows`, each
    /// with its rung, that rung's fidelity note, and a MEASURED step-doubling
    /// estimate of the chunk-end output error.
    fallback_chunks: u64,
    /// Sim-time windows `[start_s, end_s)` solved by a fallback rung, with the
    /// rung that produced each and its measured error estimate, merged where
    /// consecutive with the same rung.
    fallback_windows: Vec<FallbackWindow>,
    /// TEST-ONLY tamper knob: when set, `solve_chunk` treats the primary march
    /// as failed and the fallback ladder starts at the named rung, so a test
    /// can put a board of its choosing on a rung of its choosing. Never set
    /// outside tests.
    #[doc(hidden)]
    pub debug_force_fallback_rung: Option<ChunkFallbackMethod>,
    /// TEST-ONLY tamper knob: when true, the fallback error estimator is
    /// deliberately BROKEN (returns 0.0 regardless of the measured
    /// difference), so the estimator's own two-sided test can prove it would
    /// catch a broken estimator. Never set outside tests.
    #[doc(hidden)]
    pub debug_zero_fallback_error_estimate: bool,
    /// TEST-ONLY tamper knob: when true, the error estimator's tight
    /// (refined) companion leg is skipped so a test can exercise the
    /// 4x-coarser leg deterministically. Never set outside tests.
    #[doc(hidden)]
    pub debug_skip_refined_companion: bool,
    /// True while the CURRENT chunk carries cycle-stamped within-chunk PWL
    /// drives (`apply_pwl_drives` installed at least one). Set around the
    /// `solve_chunk` call and cleared after; consulted by the fallback
    /// ladder's subdivision guard, which must not quarter a chunk whose
    /// forcing varies inside it (each quarter restarts chunk-local source
    /// time at 0 and would re-play only the waveform's first quarter).
    chunk_has_pwl_drives: bool,
    /// Largest measured final Newton residual among converged chunks this run.
    /// `None` means the active solver path did not measure one; it never means
    /// a zero residual.
    worst_residual: Option<(f64, usize)>,
    /// Current run of back-to-back failed chunks (reset to 0 by any converged
    /// chunk). Feeds the strict/CI abort threshold.
    consecutive_failed_chunks: u32,
    /// Worst back-to-back failed-chunk run seen this run. Retained even after a
    /// later chunk converges, so a strict post-run check still sees a streak that
    /// crossed the abort threshold mid-run.
    max_consecutive_failed_chunks: u32,
    /// The solver's message from the most recent failed chunk (e.g. "Newton
    /// failed at t=... even at dt_min=...", device-named when a behavioural
    /// fault caused it). Kept so the failure surfaces with its cause instead
    /// of being discarded; also printed once at the start of each failed
    /// streak, which is the breadcrumb a bisection needs.
    last_solve_error: Option<String>,
    /// Standalone GPIO-edge-driven digital components (indices into `digital`)
    /// advanced through the generalized micro-tick replay, NOT through
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
    /// the spec's own [`hauksbee_bind::logic::LogicComponent::sequential_pins`]) the
    /// net feeds. "Tick-evaluated" excludes every edge-exact path: 595 chain
    /// chips (`chain_chips`), generalized replay chips (`replay_chips`), and
    /// 165 read-chain chips (responder-owned). Built once at construction;
    /// consulted by [`Scheduler::detect_short_pulses`].
    tick_sequential_nets: HashMap<u32, Vec<String>>,
    /// Sub-chunk GPIO pulse warnings raised this run, one per
    /// offending net. See [`ShortPulse`].
    short_pulses: Vec<ShortPulse>,
    /// Nets already warned about by `detect_short_pulses`, so a pulse train
    /// warns once per net per run, not once per chunk.
    short_pulse_nets: std::collections::HashSet<u32>,
    /// Hard timing/replay limits reached during this run. A strict caller must
    /// invalidate its verdict rather than silently use the chunk-end DC level.
    timing_refusals: Vec<String>,
    timing_refusal_nets: std::collections::HashSet<u32>,
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
/// warn that co-sim results stand in for the requested silicon.
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

/// A substitution event paired with the exact board occurrence it describes.
///
/// The fields stay private so callers cannot manufacture a mismatched event,
/// subject, and assumption. [`McuSubstitution`] remains source-compatible for
/// presentation code and legacy public struct literals.
#[derive(Debug, Clone)]
pub struct ScopedMcuSubstitution {
    event: McuSubstitution,
    subject: String,
    assumption: Assumption,
}

impl ScopedMcuSubstitution {
    pub fn new(event: McuSubstitution, subject: String) -> Self {
        let assumption = Assumption::substitute_model_for_component(
            AssumptionSource::Scheduler,
            &subject,
            &event.reference,
            &event.requested_part,
            &event.modelled_core,
        );
        Self {
            event,
            subject,
            assumption,
        }
    }

    /// The presentation event paired with this exact occurrence.
    pub fn event(&self) -> &McuSubstitution {
        &self.event
    }

    /// The opaque board-occurrence subject used for causal traversal.
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The substitution assumption constructed from the paired event/subject.
    pub fn assumption(&self) -> &Assumption {
        &self.assumption
    }
}

/// A bus peripheral bound on a platform that models no matching bus controller
///. The device is on the board, but the emulated MCU has no
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
    /// The one-line warning every surface emits for this device.
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
             controllers to the SoC descriptor to enable it ({}).",
            self.bus,
            self.id,
            self.bus,
            self.bus.to_ascii_lowercase(),
            hauksbee_ir::docs_url("docs/cosim/MCU.md"),
        )
    }
}

/// One ADC channel whose per-chunk injections the MCU backend DROPPED because
/// the platform has no injection map. The analog solve drove
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
    /// The one-line warning every surface emits for this channel.
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
             descriptor to enable it ({}).",
            self.channel,
            self.mcu_ref,
            self.net,
            hauksbee_ir::docs_url("docs/cosim/MCU.md"),
        )
    }
}

/// The one-line warning every surface emits for an entry of
/// [`Scheduler::watchdog_limitations`], so they all name the same gap in the
/// same words.
///
/// `limitation` is the backend's own whole sentence and is passed through
/// UNCHANGED. Two surfaces wording one coverage hole differently is the failure
/// this shared formatter exists to prevent, so nothing here may paraphrase it;
/// the only thing added is which MCU it is about.
pub fn watchdog_limitation_message(mcu_ref: &str, limitation: &str) -> String {
    format!("MCU {mcu_ref}: {limitation}")
}

/// The same, for an entry of [`Scheduler::timing_limitations`]: the backend's
/// whole sentence, unchanged, prefixed only with which MCU it is about.
pub fn timing_limitation_message(mcu_ref: &str, limitation: &str) -> String {
    format!("MCU {mcu_ref}: {limitation}")
}

/// The one-line resolution statement for a [`TimingCoverage`] row: the edge
/// timestamp uncertainty, the narrowest pulse guaranteed observable, the chunk
/// actually run, and whether the stamps are cycle-exact or poll-boundary.
/// Shared so no surface words one core's resolution two ways. The leading
/// indent belongs to the text table and is added there, not here.
pub fn timing_coverage_line(t: &TimingCoverage) -> String {
    format!(
        "{} ({}): edge timestamps ±{:.3} us; pulses >= {:.3} us guaranteed; \
         {:.3} us chunk; {} stamps",
        t.mcu_ref,
        t.backend,
        t.timestamp_precision_s * 1e6,
        t.minimum_guaranteed_pulse_s * 1e6,
        t.chunk_s * 1e6,
        if t.cycle_exact {
            "cycle-exact"
        } else {
            "poll-boundary"
        },
    )
}

/// The one-line finding every surface emits for an entry of
/// [`Scheduler::watchdog_resets`].
pub fn watchdog_reset_message(mcu_ref: &str, resets: u64) -> String {
    let plural = if resets == 1 { "" } else { "s" };
    format!(
        "MCU {mcu_ref}: the watchdog rebooted the core {resets} time{plural} during this \
         run; behaviour observed after the first reboot belongs to a rebooted core"
    )
}

/// A firmware GPIO pulse that rose AND fell inside a single solver chunk, on a
/// net that clocks a TICK-evaluated sequential part. Chain-responder parts (74HC595/165 chains, bit-banged
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
    /// The one-line warning every surface emits for this net.
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
/// (`hauksbee_checks::checks::contention`): at lint time every MCU GPIO driver is
/// stamped high-impedance and only firmware sets direction, so "modelled output
/// shares a net with an MCU pad" is the most common HEALTHY topology there is,
/// and the static check documents this case as out of reach. The scheduler
/// learns real pin directions at runtime (pin-change edges and
/// `pins_configured_output` DDR sync), so it is the one place the fight is
/// observable.
///
/// What counts as a modelled push-pull output is shared with the static check
/// by construction, not by parallel reimplementation: the binder stamps a
/// [`hauksbee_bind::drivers::PinDriver`] on every connected output role from
/// [`hauksbee_bind::digital::output_roles`] (the same single source the static check's
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
    /// The one-line finding every surface but hauksbee-ci emits for this net.
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
             (two TOML files, no recompile); see {}.",
            self.reference,
            self.requested_part,
            self.modelled_core,
            self.requested_part,
            hauksbee_ir::docs_url("docs/extending/add-an-mcu-variant.md")
        )
    }
}

/// The cycle-stamped GPIO edges one MCU produced during the most recent chunk,
/// exposed for the analog PWL side.
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
    /// `NetStat` literal directly; this exposes just enough for those tests
    /// (they live in `hauksbee-engine`, so it cannot be `cfg(test)`).
    #[doc(hidden)]
    pub fn with_toggles(toggles: u64) -> Self {
        NetStat {
            toggles,
            ..Default::default()
        }
    }

    /// Test constructor with an explicit voltage range, for the activity-ranking
    /// tie-break (equal toggles, differing swing) the web/CLI/JSON tables share.
    #[doc(hidden)]
    pub fn with_toggles_and_range(toggles: u64, min_v: f64, max_v: f64) -> Self {
        NetStat {
            toggles,
            min_v,
            max_v,
            ..Default::default()
        }
    }
}

fn mcu_occurrence_subjects(report: &hauksbee_bind::bind_report::BindReport) -> Vec<String> {
    let component_rows: Vec<_> = report
        .rows
        .iter()
        .filter(|row| {
            !matches!(
                &row.outcome,
                hauksbee_bind::bind_report::BindOutcome::PowerRail { .. }
            )
        })
        .collect();
    let component_subjects =
        hauksbee_bind::occurrence::component_occurrence_subjects_for_references(
            component_rows.iter().map(|row| row.reference.as_str()),
        );
    component_rows
        .iter()
        .zip(component_subjects)
        .filter_map(|(row, subject)| {
            matches!(
                &row.outcome,
                hauksbee_bind::bind_report::BindOutcome::Mcu { .. }
            )
            .then_some(subject)
        })
        .collect()
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
            peripherals,
            report,
            ..
        } = bound;

        let mut mcu_subjects = mcu_occurrence_subjects(&report).into_iter();

        // Supply-rail absolute-maximum watches, built BEFORE the bindings are
        // consumed into live cores. An MCU/logic package has no whole-device
        // stress meta (its per-pin currents are covered by pin-driver metas),
        // so without these a rail driven far past the chip's abs-max Vcc
        // raised no fault at all while the model DB carried the rating. One
        // watch per distinct supply node per part; direct-supply roles only
        // (vin and raw feed a module's onboard regulator, and their ceiling is a
        // different number from the core's).
        let mut supply_watches = Vec::new();
        for binding in &mcus {
            if let Some(max_v) = binding.max_supply_v {
                let mut seen_nodes = std::collections::HashSet::new();
                for (role, &node) in &binding.role_nets {
                    // A numbered pad of the same supply domain must be stripped
                    // to its domain name before the match: dedup is by NODE, but
                    // `seen_nodes.insert` runs inside the `direct_supply` arm, so
                    // a pad named `dvdd2` would match nothing and a rail reaching
                    // ONLY numbered pads would get no watch at all. Stripping a
                    // trailing digit run makes `vcc2`, `gnd2`, `dvdd3` and
                    // `iovdd2` read as their domain, and cannot collide with a
                    // rail whose name legitimately ends in a digit (`5v` is in
                    // the list verbatim and matches before the strip applies).
                    let bare = role.trim_end_matches(|c: char| c.is_ascii_digit());
                    let domain = if is_direct_supply_role(role) {
                        role.as_str()
                    } else {
                        bare
                    };
                    let direct_supply = is_direct_supply_role(domain);
                    if direct_supply && !node.is_ground() && seen_nodes.insert(node) {
                        supply_watches.push(hauksbee_bind::stress::SupplyWatch {
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
        let mut scoped_substitutions = Vec::new();
        for binding in mcus {
            let evidence_subject = mcu_subjects
                .next()
                .unwrap_or_else(|| binding.reference.trim().to_string());
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
                scoped_substitutions
                    .push(ScopedMcuSubstitution::new(sub.clone(), evidence_subject));
                substitutions.push(sub);
            }
            let core = instantiate_mcu(&binding, firmware)?;
            live.push(core_with_hooks(core, binding));
        }

        // Build the MCU-bit-banged 74HC595 chain controllers. For each
        // live MCU, map the net node each GPIO driver pushes onto back to its
        // (port, bit), then identify the 595 daisy-chain(s) and bind their
        // broadcast control signals (SRCLK / RCLK / SRCLR_n) and head SER to
        // GPIO pins. A chain whose essential control pins are not bound to GPIO
        // is left to the once-per-chunk digital tick, which still models it,
        // just at chunk granularity.
        let (chains, chain_mcu, chain_chips) = build_595_chains(&digital, &live);

        // Standalone GPIO-edge-driven digital components: shift/latch
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
            stop_when_dead: false,
            opts,
            sim_time: 0.0,
            micros_carry: 0.0,
            stats: HashMap::new(),
            firmware_edge_toggles: HashMap::new(),
            peripherals: PeripheralSet::new(),
            i2c_buses: Vec::new(),
            spi_buses: Vec::new(),
            model_peripheral_power: Vec::new(),
            spi_controller_map: HashMap::new(),
            substitutions,
            scoped_substitutions,
            unexercised_buses: Vec::new(),
            hc165_chains: Vec::new(),
            responder_registries: Vec::new(),
            parallel_memory_drives: Vec::new(),
            parallel_memory_chips: std::collections::HashSet::new(),
            input_volts: Arc::new(Mutex::new(vec![0.0; n_nodes])),
            forced_node_volts: HashMap::new(),
            failed_chunks: 0,
            failed_windows: Vec::new(),
            failed_window_reasons: Vec::new(),
            fallback_chunks: 0,
            fallback_windows: Vec::new(),
            debug_force_fallback_rung: None,
            debug_zero_fallback_error_estimate: false,
            debug_skip_refined_companion: false,
            chunk_has_pwl_drives: false,
            worst_residual: None,
            consecutive_failed_chunks: 0,
            max_consecutive_failed_chunks: 0,
            last_solve_error: None,
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
            timing_refusals: Vec::new(),
            timing_refusal_nets: std::collections::HashSet::new(),
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
        sched.build_and_install_parallel_memories();

        // Wire up the board's MCP4728 quad DACs: build one spec-driven I2C
        // slave per binding at its assigned address, attach them on a shared
        // bus (so firmware TWI writes reach them through `on_i2c`), with each
        // connected VOUT net's PinDriver bound to the matching spec output so
        // the slave drives the analog nets itself at every transaction end
        // (the ctx-bearing on_stop).
        if !dacs.is_empty() {
            sched.attach_mcp4728_dacs(dacs);
        }

        // A fitted exact model card can carry its firmware-visible peripheral
        // contract. Instantiate those slaves directly from the bound board so
        // every CLI/co-sim surface gets the same EEPROM/flash behavior without
        // requiring the user to repeat datasheet geometry in a CI-only spec.
        sched.attach_model_peripherals(peripherals);

        // Peripheral/DAC attachment can stamp real analogue devices after the
        // initial constructor snapshot. A destructive-fault reset must restore
        // those devices too; otherwise their retained DeviceIds point into a
        // shorter circuit and the first post-reset update silently disappears.
        sched.original_circuit = sched.circuit.clone();

        // Index the non-pin-driver devices touching each net, for the plain
        // digital-input sync's "is this net really driven?" check.
        sched.rebuild_digital_in_evidence();

        // Index the nets that clock TICK-evaluated sequential parts, for the
        // sub-chunk pulse warning. Built after the 595 chains,
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
        edge_exact.extend(self.parallel_memory_chips.iter().copied());
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
    /// shared [`I2cBus`] holds all of them, registered as every MCU's `on_i2c`
    /// handler.
    ///
    /// Each slave is a [`RegisterMapSensor`] instance of the shipped MCP4728
    /// spec (the DAC is data, not Rust), with the binder-resolved per-instance
    /// address / VREF / gain over the spec defaults and each connected VOUT
    /// channel's [`hauksbee_bind::drivers::PinDriver`] bound to the matching spec
    /// output. Net driving happens in the slave's own `on_stop(ctx)`, delivered
    /// by the chunk loop's `flush_stops`: no scheduler-side polling.
    fn attach_mcp4728_dacs(&mut self, dacs: Vec<hauksbee_bind::binder::DacBinding>) {
        /// The shipped declarative MCP4728 spec. Embedded (rather than loaded
        /// from disk at runtime) so an engine binary is self-contained.
        /// Embedded from THIS crate's assets/ mirror because `cargo package`
        /// ships only files under the crate directory; the AUTHORITATIVE copy
        /// is testdata/sensor-specs/mcp4728.toml (the unit fixtures load it),
        /// guarded by tests/packaged_asset_sync.rs and refreshed by
        /// scripts/sync-crate-assets.sh.
        const MCP4728_SPEC: &str = include_str!("../assets/sensor-specs/mcp4728.toml");

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

    /// Instantiate I2C/SPI slaves declared by exact resolved model cards.
    fn attach_model_peripherals(
        &mut self,
        peripherals: Vec<hauksbee_bind::binder::PeripheralBinding>,
    ) {
        for peripheral in peripherals {
            let reference = peripheral.reference;
            let power = peripheral.power;
            let cs_net = peripheral.cs_net;
            match peripheral.spec {
                PeripheralSpec::I2cEeprom {
                    address,
                    size_bytes,
                    page_size,
                    word_address_bytes,
                } => {
                    let slave = Eeprom24c::new(address, size_bytes)
                        .with_word_address_bytes(word_address_bytes)
                        .with_page_size(page_size);
                    let bus = I2cBus::new(&reference).with_slave(Box::new(slave));
                    self.attach_powered_i2c(&reference, power, bus);
                }
                PeripheralSpec::SpiNorFlash {
                    size_bytes,
                    page_size,
                    sector_size,
                    jedec_id,
                    spi_mode,
                    ..
                } => {
                    // Model validation guarantees exactly three JEDEC bytes.
                    let id: [u8; 3] = jedec_id
                        .try_into()
                        .expect("validated SPI NOR JEDEC ID has exactly three bytes");
                    let flash = SpiNorFlash::new(size_bytes, page_size, sector_size, id, spi_mode);
                    let bus = SpiBus::new(&reference, Box::new(flash));
                    self.attach_powered_spi(&reference, power, bus, cs_net, None);
                }
                PeripheralSpec::RegisterMap {
                    spec_toml,
                    controller,
                    ..
                } => {
                    let mut sensor = RegisterMapSensor::from_toml(&spec_toml)
                        .expect("model validation guarantees an executable register-map spec");
                    if let Some(address) = peripheral.i2c_address_override {
                        sensor.set_i2c_address(address);
                    }
                    match sensor.bus() {
                        Bus::I2c => {
                            let bus = I2cBus::new(&reference).with_slave(Box::new(sensor));
                            self.attach_powered_i2c(&reference, power, bus);
                        }
                        Bus::Spi => {
                            let bus = SpiBus::new(&reference, Box::new(sensor));
                            self.attach_powered_spi(
                                &reference,
                                power,
                                bus,
                                cs_net,
                                controller.as_deref(),
                            );
                        }
                    }
                }
            }
        }
        if !self.model_peripheral_power.is_empty() {
            self.relayout();
        }
    }

    /// Attach one model-declared I2C slave bus, starting it electrically off
    /// when the model card carries a supply spec (the first converged operating
    /// point decides whether the rail clears the power-on threshold).
    fn attach_powered_i2c(
        &mut self,
        reference: &str,
        power: Option<hauksbee_bind::binder::BoundPeripheralPower>,
        bus: I2cBus,
    ) {
        let bus = Arc::new(Mutex::new(bus));
        if power.is_some() {
            bus.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_powered(false);
        }
        self.attach_i2c_bus(bus.clone());
        if let Some(power) = power {
            self.attach_model_peripheral_power(reference, power, ModelPeripheralBus::I2c(bus));
        }
    }

    /// The SPI counterpart of [`Scheduler::attach_powered_i2c`], resolving the
    /// model's declared CS net to an MCU pin and routing to a named controller
    /// when the card asked for one.
    fn attach_powered_spi(
        &mut self,
        reference: &str,
        power: Option<hauksbee_bind::binder::BoundPeripheralPower>,
        bus: SpiBus,
        cs_net: Option<NodeId>,
        controller: Option<&str>,
    ) {
        let bus = Arc::new(Mutex::new(bus));
        if power.is_some() {
            bus.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_powered(false);
        }
        let cs = cs_net.and_then(|net| {
            self.pin_driving_node(net).map(|pin| ResolvedCs {
                pin,
                net: Some(net),
                provenance: CsProvenance::ModelRoles,
            })
        });
        match controller {
            Some(controller) => self.attach_spi_bus_on(controller, bus.clone(), cs),
            None => self.attach_spi_bus(bus.clone(), cs),
        }
        if let Some(power) = power {
            self.attach_model_peripheral_power(reference, power, ModelPeripheralBus::Spi(bus));
        }
    }

    fn attach_model_peripheral_power(
        &mut self,
        reference: &str,
        power: hauksbee_bind::binder::BoundPeripheralPower,
        bus: ModelPeripheralBus,
    ) {
        let isource = self.circuit.add(Device::Isource {
            name: format!("Iperipheral_{reference}"),
            p: power.supply_node,
            n: power.return_node,
            // Begin electrically off. The first converged board operating
            // point establishes whether the rail actually clears the source-
            // bound power-on threshold; until then no transaction is accepted.
            kind: SourceKind::Dc(0.0),
        });
        self.model_peripheral_power.push(ModelPeripheralPowerLeg {
            reference: reference.to_string(),
            isource,
            supply_node: power.supply_node,
            return_node: power.return_node,
            power_on_threshold_v: power.spec.power_on_threshold_v,
            idle_a: power.spec.idle_a,
            read_a: power.spec.read_a,
            write_a: power.spec.write_a,
            low_power_a: power.spec.low_power_a,
            bus,
            powered: false,
            last_current_a: 0.0,
        });
    }

    /// Apply rail-voltage power gating before firmware runs this chunk.
    /// `node_volts` is the previous converged operating point; the all-zero
    /// construction/reset snapshot deliberately leaves each bus off for the
    /// first chunk, so firmware cannot talk to a part before the board has
    /// established a powered rail.
    fn gate_model_peripherals_from_rails(&mut self) {
        for leg in &mut self.model_peripheral_power {
            let supply_v = self
                .node_volts
                .get(leg.supply_node.0 as usize)
                .copied()
                .unwrap_or(0.0)
                - self
                    .node_volts
                    .get(leg.return_node.0 as usize)
                    .copied()
                    .unwrap_or(0.0);
            let powered = supply_v.is_finite() && supply_v >= leg.power_on_threshold_v;
            leg.powered = powered;
            leg.set_bus_powered(powered);
            let current_a = leg.quiescent_a(leg.low_power_mode());
            leg.last_current_a = current_a;
            set_isource_dc(&mut self.circuit, leg.isource, current_a);
        }
    }

    /// Drain model-owned bus work after firmware ran and project the highest
    /// source-bound current class observed in this chunk onto the supply rail.
    /// This is intentionally a conservative per-chunk envelope rather than an
    /// invented sub-byte waveform or duty-cycle average.
    fn apply_model_peripheral_activity(&mut self) {
        for leg in &mut self.model_peripheral_power {
            let (activity, low_power) = match &leg.bus {
                ModelPeripheralBus::I2c(bus) => (
                    bus.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take_activity(),
                    false,
                ),
                ModelPeripheralBus::Spi(bus) => {
                    let mut bus = bus.lock().unwrap_or_else(|e| e.into_inner());
                    (bus.take_activity(), bus.low_power_mode())
                }
            };
            let mut current_a = leg.quiescent_a(low_power);
            if leg.powered && activity.read_units > 0 {
                current_a = current_a.max(leg.read_a);
            }
            if leg.powered && activity.write_units > 0 {
                current_a = current_a.max(leg.write_a);
            }
            if leg.powered && activity.other_units > 0 {
                current_a = current_a.max(leg.read_a.max(leg.write_a));
            }
            leg.last_current_a = current_a;
            set_isource_dc(&mut self.circuit, leg.isource, current_a);
        }
    }

    /// The synchronous input-responder registry for MCU `mi`, creating it and
    /// installing its dispatch closure into the MCU's single
    /// `on_input_responder` slot on first use. Every bit-banged input protocol
    /// (165 chains, bit-banged SPI MISO, soft-I2C) registers here; dispatch is
    /// keyed on the output pins each responder watches, so an edge on a
    /// non-protocol pin costs one map miss, and the lazy install keeps the
    /// backend hook empty on boards with no responders. On poll backends
    /// `on_input_responder` is a documented no-op, so the registry exists but
    /// never fires: the deliberate coarse tier.
    fn responder_registry(
        &mut self,
        mi: usize,
    ) -> Arc<Mutex<crate::responders::ResponderRegistry>> {
        if let Some(reg) = &self.responder_registries[mi] {
            return reg.clone();
        }
        let reg = Arc::new(Mutex::new(crate::responders::ResponderRegistry::new()));
        let cb = reg.clone();
        self.mcus[mi].core.on_input_responder_batch(Box::new(
            move |edges: &[(PinId, bool)], cycle: u64| -> Vec<hauksbee_mcu::PinDrive> {
                cb.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .dispatch_batch_at(
                        &edges
                            .iter()
                            .map(|&(pin, high)| ((pin.port, pin.bit), high))
                            .collect::<Vec<_>>(),
                        cycle,
                    )
                    .into_iter()
                    .map(|update| {
                        let pin = PinId {
                            port: update.pin.0,
                            bit: update.pin.1,
                        };
                        match update.level {
                            Some(high) => hauksbee_mcu::PinDrive::drive(pin, high),
                            None => hauksbee_mcu::PinDrive::release(pin),
                        }
                    })
                    .collect()
            },
        ));
        let direction_registry = reg.clone();
        self.mcus[mi].core.on_input_responder_direction(Box::new(
            move |changes: &[(PinId, bool, bool)], cycle: u64| {
                direction_registry
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .dispatch_direction_at(
                        &changes
                            .iter()
                            .map(|&(pin, output, port_high)| {
                                ((pin.port, pin.bit), output, port_high)
                            })
                            .collect::<Vec<_>>(),
                        cycle,
                    )
                    .into_iter()
                    .map(|update| match update.level {
                        Some(high) => hauksbee_mcu::PinDrive::drive(
                            PinId::new(update.pin.0, update.pin.1),
                            high,
                        ),
                        None => {
                            hauksbee_mcu::PinDrive::release(PinId::new(update.pin.0, update.pin.1))
                        }
                    })
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
        use crate::responders::Hc165Responder;
        use hauksbee_bind::digital::{order_165_chains, Hc165Chain, LogicLevels};

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

    /// Discover declarative parallel memories whose data bus reaches one live
    /// MCU and install an edge-synchronous responder. Address bits may be
    /// direct GPIO, settled/static nodes, or outputs of any MCU-owned 74HC595
    /// chain (including several independent chains sharing SER/RCLK, as on the
    /// Nano EEPROM Programmer). The responder owns write timing for installed
    /// parts; ordinary tick/replay paths are disabled for those components.
    fn build_and_install_parallel_memories(&mut self) {
        let mut pending = Vec::new();
        for component in 0..self.digital.len() {
            for port in self.digital[component].memory_ports() {
                // Installing a responder suppresses this component's whole
                // once-per-chunk tick. Refuse a partial takeover: unrelated
                // combinational/register/output behavior must keep running.
                if !self.digital[component].has_exclusive_memory_port(&port.name) {
                    continue;
                }
                if let Some(bound) = (0..self.mcus.len())
                    .find_map(|mcu| self.bind_parallel_memory(component, &port, mcu))
                {
                    pending.push(bound);
                }
            }
        }

        for item in pending {
            self.responder_registry(item.mcu)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .register(Box::new(item.responder));
            self.mcus[item.mcu]
                .responder_input_pins
                .extend(item.output_pins);
            self.parallel_memory_chips.insert(item.component);
            self.parallel_memory_drives.push(ParallelMemoryDrive {
                component: item.component,
                outputs: item.outputs,
                runtime: item.runtime,
            });
        }
        self.replay_chips
            .retain(|chip| !self.parallel_memory_chips.contains(chip));
    }

    /// Try to resolve one memory port of `component` against MCU `mcu`,
    /// returning the responder to install when every signal the port needs is
    /// provably owned by that MCU (directly, or through a 74HC595 chain it
    /// clocks). Returns `None` — leaving the part on the ordinary once-per-chunk
    /// tick path — whenever provenance cannot be established, which is the
    /// conservative answer in every ambiguous case below.
    fn bind_parallel_memory(
        &self,
        component: usize,
        port: &hauksbee_bind::logic::ParallelMemoryPort,
        mcu: usize,
    ) -> Option<PendingParallelMemory> {
        use crate::responders::{
            ParallelMemoryResponder, ParallelMemoryRuntime, ParallelMemoryWrite, ParallelSignal,
        };

        // Poll backends cannot answer between guest instructions.
        if !self.mcus[mcu].core.cycle_exact() || !self.mcus[mcu].core.input_responder_synchronous()
        {
            return None;
        }
        let gpio: HashMap<i64, (char, u8)> = self.mcus[mcu]
            .binding
            .gpio_drivers
            .iter()
            .map(|(&(port, bit), drv)| (drv.net.0 as i64, (port, bit)))
            .collect();
        let chains: Vec<hauksbee_bind::digital::Hc595Chain> = self
            .chains
            .iter()
            .zip(&self.chain_mcu)
            .filter(|(_, owner)| **owner == mcu)
            .map(|(chain, _)| chain.clone())
            .collect();

        // Resolve physical provenance before choosing a fast-path
        // representation. A node does not become MCU-owned merely because the
        // candidate MCU has one pin on it: another MCU pin or digital output on
        // the same copper can change the level inside the chunk and would be
        // invisible to this responder. Count every producer, including duplicate
        // pins on one MCU and 595 outputs. The memory component's own
        // bidirectional data drivers are excluded because the responder is
        // precisely the replacement for those.
        let mut producer_counts: HashMap<NodeId, usize> = HashMap::new();
        for live in &self.mcus {
            for driver in live.binding.gpio_drivers.values() {
                *producer_counts.entry(driver.net).or_default() += 1;
            }
        }
        for (digital_i, digital) in self.digital.iter().enumerate() {
            for role in digital.drivers.keys() {
                if digital_i == component && port.data_out.contains(role) {
                    continue;
                }
                if let Some(&node) = digital.roles.get(role) {
                    *producer_counts.entry(node).or_default() += 1;
                }
            }
        }
        let permitted_driver_resistors: std::collections::HashSet<DeviceId> = self.mcus[mcu]
            .binding
            .gpio_drivers
            .values()
            .map(|driver| driver.resistor)
            .chain(port.data_out.iter().filter_map(|role| {
                self.digital[component]
                    .drivers
                    .get(role)
                    .map(|driver| driver.resistor)
            }))
            .chain(chains.iter().flat_map(|chain| {
                chain.order.iter().flat_map(|&digital_i| {
                    self.digital[digital_i]
                        .drivers
                        .values()
                        .map(|driver| driver.resistor)
                })
            }))
            .collect();
        let levels = self.digital[component].levels;
        let mut fixed_volts = HashMap::from([(NodeId::GROUND, 0.0)]);
        for supply in &self.supplies {
            fixed_volts.insert(supply.net, supply.supply.nominal_volts());
        }
        for device in &self.circuit.devices {
            if let Device::Vsource {
                p,
                n,
                kind: SourceKind::Dc(volts),
                ..
            } = device
            {
                if *n == NodeId::GROUND {
                    fixed_volts.insert(*p, *volts);
                }
            }
        }
        let mut pull_candidates: HashMap<NodeId, Vec<(DeviceId, bool)>> = HashMap::new();
        for (index, device) in self.circuit.devices.iter().enumerate() {
            if permitted_driver_resistors.contains(&DeviceId(index as u32)) {
                continue;
            }
            let Device::Resistor { a, b, ohms, .. } = device else {
                continue;
            };
            if !ohms.is_finite() || *ohms < 1_000.0 {
                continue;
            }
            let pulled = fixed_volts
                .get(a)
                .map(|&volts| (*b, volts))
                .or_else(|| fixed_volts.get(b).map(|&volts| (*a, volts)))
                .and_then(|(node, volts)| {
                    if volts >= levels.vih {
                        Some((node, true))
                    } else if volts <= levels.vil {
                        Some((node, false))
                    } else {
                        None
                    }
                });
            if let Some((node, level)) = pulled {
                pull_candidates
                    .entry(node)
                    .or_default()
                    .push((DeviceId(index as u32), level));
            }
        }
        let mut passive_pull_devices = std::collections::HashSet::new();
        let mut pulled_levels = HashMap::new();
        for (node, candidates) in pull_candidates {
            if let [(device, level)] = candidates.as_slice() {
                passive_pull_devices.insert(*device);
                pulled_levels.insert(node, *level);
            }
        }
        let externally_coupled_nodes: std::collections::HashSet<NodeId> = self
            .circuit
            .devices
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                let id = DeviceId(*index as u32);
                !permitted_driver_resistors.contains(&id) && !passive_pull_devices.contains(&id)
            })
            .flat_map(|(_, device)| device.nodes())
            .filter(|node| *node != NodeId::GROUND)
            .collect();

        let signal_for_node = |node: NodeId| -> Option<ParallelSignal> {
            if node == NodeId::GROUND {
                return Some(ParallelSignal::Node(NodeId::GROUND));
            }
            if producer_counts.get(&node).copied().unwrap_or(0) > 1
                || externally_coupled_nodes.contains(&node)
            {
                return None;
            }
            if let Some(&pin) = gpio.get(&(node.0 as i64)) {
                return Some(ParallelSignal::Mcu(pin));
            }
            for (chain_i, chain) in chains.iter().enumerate() {
                for (chip_i, &digital_i) in chain.order.iter().enumerate() {
                    for (bit, role) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"]
                        .iter()
                        .enumerate()
                    {
                        if self.digital[digital_i].roles.get(*role) == Some(&node) {
                            return Some(ParallelSignal::Hc595 {
                                chain: chain_i,
                                chip: chip_i,
                                bit: bit as u8,
                            });
                        }
                    }
                }
            }
            Some(ParallelSignal::Node(node))
        };
        let role_signal =
            |role: &str| signal_for_node(self.digital[component].roles.get(role).copied()?);
        let signals = |roles: &[String]| {
            roles
                .iter()
                .map(|role| role_signal(role))
                .collect::<Option<Vec<_>>>()
        };
        let gate_signals = |gates: &[(String, hauksbee_models::logic_spec::Level)]| {
            gates
                .iter()
                .map(|(role, active)| role_signal(role).map(|s| (s, *active)))
                .collect::<Option<Vec<_>>>()
        };

        let initial_pin_levels: HashMap<(char, u8), bool> = gpio
            .iter()
            .filter_map(|(&node, &pin)| {
                pulled_levels
                    .get(&NodeId(node as u32))
                    .map(|&level| (pin, level))
            })
            .collect();
        if !initial_pin_levels.is_empty() && !self.mcus[mcu].core.input_responder_tracks_direction()
        {
            return None;
        }

        let address = signals(&port.address)?;
        let writes = port
            .writes
            .iter()
            .map(|write| {
                let signal = role_signal(&write.pin)?;
                let gates = gate_signals(&write.gates)?;
                Some(ParallelMemoryWrite::new(signal, write.edge, gates))
            })
            .collect::<Option<Vec<_>>>()?;
        let read_gates = gate_signals(&port.read_gates)?;
        let data_in = signals(&port.data_in)?;

        // A legacy singleton callback remains exact when one MCU output is the
        // only mutable input to this memory. With two MCU inputs (or an
        // MCU-clocked 595 feeding it), one hardware port write can change a
        // write edge and a gate/address/data bit together; only an atomic batch
        // exposes the final state.
        let mut mcu_inputs = std::collections::HashSet::new();
        let mut referenced_595_chains = std::collections::HashSet::new();
        let mut has_unproven_node = false;
        for signal in address
            .iter()
            .chain(writes.iter().flat_map(|write| {
                std::iter::once(&write.signal).chain(write.gates.iter().map(|(signal, _)| signal))
            }))
            .chain(read_gates.iter().map(|(signal, _)| signal))
            .chain(data_in.iter())
        {
            match signal {
                ParallelSignal::Mcu(pin) => {
                    mcu_inputs.insert(*pin);
                }
                ParallelSignal::Hc595 { chain, .. } => {
                    referenced_595_chains.insert(*chain);
                }
                // The responder runs before the first analogue solve and
                // therefore cannot trust a pulled or supplied node's all-zero
                // voltage snapshot. Ground is the sole node whose LOW level is
                // an identity, independent of a solve.
                ParallelSignal::Node(node) => has_unproven_node |= *node != NodeId::GROUND,
            }
        }
        if has_unproven_node {
            return None;
        }
        let chain_controls = chains
            .iter()
            .map(|chain| {
                let mut pins = vec![chain.srclk, chain.rclk, chain.ser];
                pins.extend(chain.srclr_n);
                pins.extend(chain.oe_n);
                (chain.oe_n, pins)
            })
            .collect::<Vec<_>>();
        let pulled_pins = initial_pin_levels.keys().copied().collect();
        // ParallelSignal::Hc595 carries a stored bit, not drive ownership or
        // externally-biased control state. A referenced chain with mutable OE
        // can be Hi-Z, and a pulled clock/data/clear control does not start at
        // Hc595Chain's built-in default; both stay on the analogue tick path.
        if !referenced_595_controls_are_proven(
            &referenced_595_chains,
            &chain_controls,
            &pulled_pins,
        ) {
            return None;
        }
        let has_shifted_input = !referenced_595_chains.is_empty();
        let has_callback_trigger = !mcu_inputs.is_empty() || has_shifted_input;
        let initial_level = |signal: &ParallelSignal| match signal {
            ParallelSignal::Mcu(pin) => initial_pin_levels.get(pin).copied().unwrap_or(false),
            ParallelSignal::Hc595 { .. } | ParallelSignal::Node(_) => false,
        };
        let power_on_read_is_inhibited = read_gates
            .iter()
            .any(|(signal, active): &(ParallelSignal, _)| !active.is_active(initial_level(signal)));
        // Responders have no trustworthy solved analogue snapshot at
        // construction and start with MCU/595 levels LOW. A memory with no
        // watched source never runs at all; one whose read is already active
        // would need an initial drive before the first guest instruction. At
        // least one dynamic read gate must be inactive under the proven initial
        // MCU/595/pull state, so the enabling edge synchronously establishes the
        // drive.
        if !has_callback_trigger || !power_on_read_is_inhibited {
            return None;
        }
        let needs_atomic_batch = has_shifted_input || mcu_inputs.len() > 1;
        if needs_atomic_batch && !self.mcus[mcu].core.input_responder_batches_atomic() {
            return None;
        }
        let output_pins = port
            .data_out
            .iter()
            .map(|role| {
                let node = self.digital[component].roles.get(role)?;
                if *node == NodeId::GROUND || producer_counts.get(node).copied().unwrap_or(0) != 1 {
                    return None;
                }
                gpio.get(&(node.0 as i64)).copied()
            })
            .collect::<Option<Vec<_>>>()?;

        let runtime = Arc::new(Mutex::new(ParallelMemoryRuntime::default()));
        let responder = ParallelMemoryResponder::new(
            format!("{}.{}", self.digital[component].reference, port.name),
            port.clone(),
            self.digital[component].levels,
            self.mcus[mcu].core.frequency(),
            self.input_volts.clone(),
            address,
            writes,
            read_gates,
            data_in,
            output_pins.clone(),
            chains,
            runtime.clone(),
        )
        .with_initial_pin_levels(initial_pin_levels);
        Some(PendingParallelMemory {
            mcu,
            component,
            outputs: port.data_out.clone(),
            output_pins,
            runtime,
            responder,
        })
    }

    fn apply_parallel_memory_outputs(&mut self) {
        for binding in &self.parallel_memory_drives {
            let runtime = binding.runtime.lock().unwrap_or_else(|e| e.into_inner());
            let component = &mut self.digital[binding.component];
            for (bit, role) in binding.outputs.iter().enumerate() {
                let Some(driver) = component.drivers.get_mut(role) else {
                    continue;
                };
                driver.set_enabled(&mut self.circuit, runtime.read_enabled);
                if runtime.read_enabled {
                    driver.set_volts(
                        &mut self.circuit,
                        component.levels.drive_volts(runtime.word & (1 << bit) != 0),
                    );
                }
            }
        }
    }

    /// Chip-substitution events detected at build time. Empty when every
    /// instantiated MCU was modelled by its exact requested part.
    pub fn substitutions(&self) -> &[McuSubstitution] {
        &self.substitutions
    }

    /// Substitution events paired with their exact board-occurrence evidence.
    pub fn scoped_substitutions(&self) -> &[ScopedMcuSubstitution] {
        &self.scoped_substitutions
    }

    /// Loud notes for every net whose voltage is decided by something other
    /// than the thing the user asked for. Two shapes:
    ///
    ///   - Two ideal sources pinning one net. Forcing a net to 20 V beside a
    ///     3.3 V rail leaves it reading 3.300 V; the note names both sources
    ///     and, by reading the settled node voltage rather than guessing at
    ///     pivot order, says which one won.
    ///   - A post-solve `force_net_voltage` override on a net that a stamped
    ///     ideal source already pins. The override wins in the reported
    ///     voltage, so the stamped source's contribution is invisible.
    ///
    /// Empty on any board where nothing contests a net, which is every ordinary
    /// board, so this costs one allocation and no notes in the common case.
    pub fn drive_conflicts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in hauksbee_solve::blame::source_conflicts(&self.circuit) {
            let settled = self.node_volts.get(c.node.0 as usize).copied();
            out.push(c.describe(settled));
        }
        for (&node, &(hi, lo, t0, t1)) in &self.forced_node_volts {
            let stamped = hauksbee_solve::blame::sources_on_node(
                &self.circuit,
                hauksbee_ir::NodeId(node as u32),
            );
            if stamped.is_empty() {
                continue;
            }
            let names = stamped
                .iter()
                .map(|s| format!("{} ({:.3} V)", s.name, s.volts))
                .collect::<Vec<_>>()
                .join(", ");
            let net = self
                .net_nodes
                .iter()
                .find(|(_, id)| id.0 as usize == node)
                .map(|(n, _)| n.clone())
                .unwrap_or_else(|| format!("node #{node}"));
            let window = if t0.is_finite() || t1.is_finite() {
                format!(" over [{t0:.6}, {t1:.6}) s")
            } else {
                String::new()
            };
            out.push(format!(
                "net '{net}' is overridden to {hi:.3} V (else {lo:.3} V){window} AFTER each \
                 solve, on top of stamped ideal source(s) {names}: the override wins and the \
                 stamped source has no effect on the reported voltage"
            ));
        }
        out
    }

    /// Bus peripherals attached on a platform that models no matching bus
    /// controller: never exercised, recorded at attach time.
    pub fn unexercised_buses(&self) -> &[UnexercisedBus] {
        &self.unexercised_buses
    }

    /// Nets wired to an MCU pin whose drive this backend has NOT observed, on
    /// backends that cannot report drive direction. There (the ESP32 QEMU RAM
    /// mailbox carries output LEVELS only, and the fork models no GPSPI/I2C
    /// controller) a pin whose Thevenin driver is still tri-stated might be
    /// genuinely undriven OR driven in ways the backend cannot see; either way
    /// its net's solved voltage is the passive network's static level, not a
    /// measurement of MCU activity, and the UI must not present it as one.
    ///
    /// Direction-observable backends (simavr DDR hooks, dir-mapped Renode ports)
    /// are excluded: there a tri-stated driver IS the measured truth. A net some
    /// other, observed MCU driver is actively pushing on is excluded too: that
    /// reading is a real driven measurement. Recomputed per frame, so the flag
    /// clears the moment the pin first reports a level.
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
    /// map), resolved to their board nets and nearby parts.
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

    /// Host serial bytes not delivered to each MCU's UART, whether due to a
    /// pending-buffer overflow or an external backend transport/configuration
    /// failure. Zero everywhere on a healthy run. Ordered by MCU reference.
    pub fn uart_rx_overflow(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .mcus
            .iter()
            .map(|m| (m.binding.reference.clone(), m.core.uart_rx_overflow()))
            .filter(|(_, n)| *n > 0)
            .collect();
        out.sort();
        out
    }

    /// Host serial bytes accepted by a backend but not yet presented to its
    /// emulated UART. A non-empty result at session teardown is undelivered
    /// input, even when the backend's overflow count is zero.
    pub fn uart_rx_pending(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .mcus
            .iter()
            .map(|m| (m.binding.reference.clone(), m.core.uart_rx_pending()))
            .filter(|(_, n)| *n > 0)
            .collect();
        out.sort();
        out
    }

    /// Per-MCU statements of how the backend's watchdog fidelity falls short of
    /// the part, keyed by MCU reference. Same class of finding as `adc_dropped`:
    /// the run happened, but something the board would have done did not, so a
    /// green result on the recovery path means less than it looks. A firmware
    /// that HANGS runs forever here, so every assertion about behaviour after a
    /// hang is fiction.
    ///
    /// The value is the backend's whole sentence, rendered verbatim through
    /// [`watchdog_limitation_message`]. Backends whose armed, never-fed watchdog
    /// reboots the core the way silicon does (simavr) are absent: the silence is
    /// what makes the warning mean something. Ordered by MCU reference.
    pub fn watchdog_limitations(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .mcus
            .iter()
            .filter_map(|m| {
                m.core
                    .watchdog_limitation()
                    .map(|l| (m.binding.reference.clone(), l))
            })
            .collect();
        out.sort();
        out
    }

    /// Per-MCU statements of how the backend's TIMING fidelity falls short of
    /// the part. Same coverage class as [`Scheduler::watchdog_limitations`]:
    /// time-based results on these cores carry a known systematic bias
    /// (wall-clock-paced virtual time on the QEMU family, the F103's deliberate
    /// TIMx-at-72MHz divergence), so a green time-based assertion means less
    /// than it looks. The value is the backend's whole sentence, rendered
    /// verbatim through [`timing_limitation_message`]. Clock-truth-gated
    /// backends are absent: the silence is the claim the gate measures. Ordered
    /// by MCU reference.
    pub fn timing_limitations(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .mcus
            .iter()
            .filter_map(|m| {
                m.core
                    .timing_limitation()
                    .map(|l| (m.binding.reference.clone(), l))
            })
            .collect();
        out.sort();
        out
    }

    /// Times an unserviced watchdog rebooted each MCU's core during this run,
    /// filtered to the nonzero. Not an error, a FINDING: an assertion that
    /// passed across a reboot was not measuring the run it claimed, because the
    /// behaviour it observed belongs to a rebooted core. Read together with
    /// [`Scheduler::watchdog_limitations`], since a backend that cannot reboot
    /// at all reports zero here and says so there. Ordered by MCU reference.
    pub fn watchdog_resets(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .mcus
            .iter()
            .map(|m| (m.binding.reference.clone(), m.core.watchdog_resets()))
            .filter(|(_, n)| *n > 0)
            .collect();
        out.sort();
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
        // A slave bound on a platform whose backend models no I2C controller
        // receives no traffic, ever. Record it so every report surface says so
        // instead of a silent green.
        let id = {
            use crate::peripherals::Peripheral as _;
            let g = bus.lock().unwrap_or_else(|e| e.into_inner());
            g.id().to_string()
        };
        self.record_bus_if_unexercised(&id, "I2C", None);
        self.i2c_buses.push(bus);
        // The AVR core's `on_i2c` closure and `set_i2c_slave_addresses` are
        // SINGLE-SLOT replacers, so a per-bus closure would let a second attach
        // overwrite the first bus's dispatcher and drop its addresses from the
        // TWI filter. Rebuild a MULTIPLEXING dispatcher and the address union
        // from the full bus list on every attach. Each 7-bit address is owned by
        // at most one bus, so route by address and dispatch only to the owner,
        // never touching a sibling bus's state.
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
    /// `cs` is the resolved chip-select. `Some` puts the bus on exact CS-edge
    /// framing: the CS GPIO edge stream frames each transaction at its true
    /// assert/deassert, so two transactions in one chunk are separated and a
    /// boundary-spanning transaction is not truncated. `None` leaves the bus on
    /// the chunk-boundary heuristic, reported as `heuristic` in the co-sim
    /// coverage.
    pub fn attach_spi_bus(&mut self, bus: Arc<Mutex<SpiBus>>, cs: Option<ResolvedCs<NodeId>>) {
        let id = bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .id()
            .to_string();
        self.record_bus_if_unexercised(&id, "SPI", None);
        bus.lock().unwrap_or_else(|e| e.into_inner()).set_cs_pin(
            cs.map(|c| c.pin),
            cs.map_or(CsProvenance::SpecDeclared, |c| c.provenance),
        );
        self.register_cs_frame(&bus, cs.map(|c| c.pin), cs.and_then(|c| c.net));
        self.spi_buses.push(bus);
        // Rebuild a MULTIPLEXING `on_spi` across ALL attached buses. `on_spi` is
        // a single-slot replacer on the AVR core, so a per-bus closure would let
        // a second attach overwrite the first bus's transfer path and send every
        // byte to the last-attached slave whatever chip-select was asserted.
        // Route each byte to the bus whose CS is currently asserted
        // (`is_selected`); a lone bus is always routed to, which preserves the
        // single-slave path exactly, including before its first CS edge.
        let all: Vec<Arc<Mutex<SpiBus>>> = self.spi_buses.clone();
        for m in &mut self.mcus {
            let buses = all.clone();
            m.core.on_spi(Box::new(move |ev| dispatch_spi(&buses, ev)));
        }
    }

    /// Attach a SPI bus to a specific named SPI controller, so transfers from
    /// that controller route to this slave. On single-controller backends (AVR,
    /// QEMU) `on_spi_controller` falls back to `on_spi`, so this is safe even
    /// with one physical SPI peripheral. The bus also joins `spi_buses` so the
    /// controller-agnostic chunk-boundary deselect can reach it. `cs` behaves
    /// exactly as in [`Self::attach_spi_bus`].
    pub fn attach_spi_bus_on(
        &mut self,
        controller: &str,
        bus: Arc<Mutex<SpiBus>>,
        cs: Option<ResolvedCs<NodeId>>,
    ) {
        let id = bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .id()
            .to_string();
        self.record_bus_if_unexercised(&id, "SPI", Some(controller));
        bus.lock().unwrap_or_else(|e| e.into_inner()).set_cs_pin(
            cs.map(|c| c.pin),
            cs.map_or(CsProvenance::SpecDeclared, |c| c.provenance),
        );
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
        self.register_cs_frame(&bus, cs.map(|c| c.pin), cs.and_then(|c| c.net));
        self.spi_buses.push(bus);
    }

    /// Install the live CS-framing hook for `bus` on whichever MCU actually
    /// drives `cs_pin`. A `None` pin (unresolved CS) installs nothing and the
    /// bus stays on the chunk-boundary heuristic.
    ///
    /// `gpio_drivers` is keyed by chip-local `(port,bit)`, so on a multi-MCU
    /// board two MCUs can each own a driver for the SAME tuple on UNRELATED
    /// nets; framing every such MCU would let an unrelated MCU's toggle of its
    /// like-named pin select/deselect this bus and corrupt the decoded
    /// transaction. The hook goes on the single owning MCU only.
    fn register_cs_frame(
        &mut self,
        bus: &Arc<Mutex<SpiBus>>,
        cs_pin: Option<(char, u8)>,
        cs_net: Option<NodeId>,
    ) {
        let Some(pin) = cs_pin else { return };
        // When the CS net is known, require `drv.net == cs_net`, matching
        // `pin_driving_node`'s net-based resolution (from which `cs_pin` was
        // derived); with no net, fall back to the first tuple owner.
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

    /// Attach a bit-banged SPI slave: the firmware toggles SCLK/MOSI/CS as
    /// plain GPIOs and reads MISO as a GPIO, and the
    /// [`crate::responders::BitBangSpiResponder`] bridges the bit stream to the
    /// byte-level slave in `bus`, answering MISO synchronously inside the
    /// firmware's own clock loop.
    ///
    /// The responder registers with the registry of the MCU whose binding owns
    /// the SCLK GPIO driver; all four pins must belong to that MCU (resolve nets
    /// to pins with [`Scheduler::mcu_pin_for_net`]).
    ///
    /// The bus records `cs_n` as its CS pin so coverage reports `exact` framing
    /// and the chunk-boundary deselect stays off it. Deliberately NOT
    /// `register_cs_frame`: the responder owns select/deselect from the same CS
    /// edges, and registering both would double-deliver every CS event.
    ///
    /// Only meaningful on push backends (simavr): on poll backends the
    /// responder never fires (`on_input_responder` is a documented no-op) and a
    /// bit-banged read stays coarse.
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
        // This CS came from the bit-bang wiring, not from a spec `cs_net` and not
        // from a model pad map, and says so.
        bus.lock().unwrap_or_else(|e| e.into_inner()).set_cs_pin(
            Some(pins.cs_n),
            crate::peripherals::CsProvenance::BitBangPins,
        );
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

    /// Attach a soft-I2C slave bus: the firmware bit-bangs SCL/SDA as plain
    /// GPIOs and the [`crate::responders::SoftI2cResponder`] recovers the
    /// transaction from the pin edges, routing it to the existing [`I2cBus`]
    /// slave models and answering SDA synchronously inside the firmware's own
    /// clock loop. See the responder's docs for the honest waveform subset.
    ///
    /// The responder registers with the registry of the MCU whose binding owns
    /// the SCL GPIO driver; SDA must belong to the same MCU. The bus joins
    /// `i2c_buses` so `flush_stops` delivers the ctx-bearing `on_stop` exactly
    /// like the hardware-TWI path, but deliberately WITHOUT `attach_i2c_bus`'s
    /// `on_i2c` registration: this bus lives on GPIO pins, not the TWI
    /// peripheral, and answering hardware-TWI traffic at these addresses would
    /// invent a device on the wrong pins.
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

    /// Resolve a named net to the MCU GPIO pin wired to it, so a caller wiring
    /// a bit-banged topology can go from the board's nets straight to responder
    /// pins. Input pins resolve too: every wired digital-capable pin gets a
    /// (possibly tri-stated) GPIO driver, so a MISO/SDA-style read pin carries
    /// the mapping even though the firmware never drives it.
    pub fn mcu_pin_for_net(&self, net: &str) -> Option<(char, u8)> {
        let node = *self.net_nodes.get(net)?;
        self.pin_driving_node(node)
    }

    /// Trace a net back to the MCU pin that drives it: the (port, bit) of the
    /// GPIO driver whose net is `node`, if any MCU drives it. This is the CS-net
    /// resolution the binder uses to populate `cs_pin`, and the same
    /// net-to-driving-pin trace the 74HC595 chain wiring performs to find its
    /// SRCLK/RCLK/SER pins.
    pub fn pin_driving_node(&self, node: NodeId) -> Option<(char, u8)> {
        // `gpio_drivers` is a HashMap with randomized iteration order, and more
        // than one of an MCU's pins can legitimately sit on `node` (a
        // self-monitoring topology, or two pins collapsed onto one net by a
        // [[jumper]] bodge). Pick the lowest (port, bit) so the resolution is
        // stable across process runs.
        self.mcus
            .iter()
            .flat_map(|m| m.binding.gpio_drivers.iter())
            .filter(|(_, drv)| drv.net == node)
            .map(|(pin, _)| *pin)
            .min()
    }

    /// Per-slave SPI framing tier for the co-sim coverage: `(bus id, mode)` for
    /// every attached SPI bus. A consumer reads this to know whether each
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
        for leg in &self.model_peripheral_power {
            let supply_v = self
                .node_volts
                .get(leg.supply_node.0 as usize)
                .copied()
                .unwrap_or(0.0)
                - self
                    .node_volts
                    .get(leg.return_node.0 as usize)
                    .copied()
                    .unwrap_or(0.0);
            let fields = m.entry(leg.reference.clone()).or_default();
            fields.insert("supply_v".into(), supply_v);
            fields.insert("supply_current_a".into(), leg.last_current_a);
            fields.insert("power_on_threshold_v".into(), leg.power_on_threshold_v);
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
    /// The co-sim summary reads this to report what part the board
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

    /// Report the timing accuracy actually available at the current chunk.
    /// Values come from the live core's frequency/callback tier and the chunk
    /// the scheduler will really execute; no part-family limits are guessed.
    pub fn timing_coverage(&self) -> Vec<TimingCoverage> {
        self.mcus
            .iter()
            .map(|m| {
                let cycle_exact = m.core.cycle_exact();
                let cycle_s = 1.0 / m.core.frequency().max(1) as f64;
                let timestamp_precision_s = if cycle_exact { cycle_s } else { self.chunk_s };
                TimingCoverage {
                    mcu_ref: m.binding.reference.clone(),
                    backend: m.binding.backend.clone(),
                    cycle_exact,
                    timestamp_precision_s,
                    minimum_guaranteed_pulse_s: if cycle_exact {
                        cycle_s
                    } else {
                        2.0 * self.chunk_s
                    },
                    chunk_s: self.chunk_s,
                }
            })
            .collect()
    }

    /// Per-net toggle counts suitable for assertions and reports.
    ///
    /// Firmware-driven nets use at least the ordered MCU edge count, preserving
    /// sub-chunk pulses. Analog-only nets retain the settled-voltage count. The
    /// maximum avoids double-counting an edge also seen at a chunk boundary.
    pub fn toggle_counts(&self) -> HashMap<String, u64> {
        let mut counts: HashMap<String, u64> = self
            .stats
            .iter()
            .map(|(net, stat)| (net.clone(), stat.toggles))
            .collect();
        for (net, edge_count) in &self.firmware_edge_toggles {
            let count = counts.entry(net.clone()).or_default();
            *count = (*count).max(*edge_count);
        }
        counts
    }

    /// Runtime timing/replay limits reached by this scheduler run.
    pub fn timing_refusals(&self) -> &[String] {
        &self.timing_refusals
    }

    /// Negotiate a strict timing request against the live backends.
    ///
    /// Exact push backends need no smaller analog chunk to preserve GPIO edges:
    /// their callback records every transition and the PWL/replay paths consume
    /// the cycle stamps. Poll backends can only improve by shrinking the real
    /// poll slice, so this adaptively does so. A request below the integer-
    /// microsecond MCU bridge quantum is refused before mutating the scheduler.
    pub fn configure_timing(&mut self, req: TimingRequirement) -> anyhow::Result<()> {
        for (label, value) in [
            ("minimum pulse", req.min_pulse_s),
            ("maximum edge error", req.max_edge_error_s),
        ] {
            if let Some(v) = value {
                anyhow::ensure!(
                    v.is_finite() && v > 0.0,
                    "{label} must be positive and finite"
                );
            }
        }

        // Exact cores have a measured one-cycle floor. Check it before changing
        // a shared chunk for any coarse core, so a mixed-backend refusal is
        // transactional.
        for m in &self.mcus {
            if !m.core.cycle_exact() {
                continue;
            }
            let cycle_s = 1.0 / m.core.frequency().max(1) as f64;
            if let Some(pulse) = req.min_pulse_s {
                anyhow::ensure!(
                    pulse + f64::EPSILON >= cycle_s,
                    "timing request refused for {} ({}): minimum pulse {:.3} us is below its measured one-cycle resolution {:.3} us",
                    m.binding.reference,
                    m.binding.backend,
                    pulse * 1e6,
                    cycle_s * 1e6,
                );
            }
            if let Some(error) = req.max_edge_error_s {
                anyhow::ensure!(
                    error + f64::EPSILON >= cycle_s,
                    "timing request refused for {} ({}): maximum edge error {:.3} us is below its measured one-cycle resolution {:.3} us",
                    m.binding.reference,
                    m.binding.backend,
                    error * 1e6,
                    cycle_s * 1e6,
                );
            }
        }

        if self.mcus.iter().any(|m| !m.core.cycle_exact()) {
            let mut requested_chunk = self.chunk_s;
            if let Some(pulse) = req.min_pulse_s {
                requested_chunk = requested_chunk.min(pulse / 2.0);
            }
            if let Some(error) = req.max_edge_error_s {
                requested_chunk = requested_chunk.min(error);
            }
            anyhow::ensure!(
                requested_chunk + f64::EPSILON >= POLL_BACKEND_QUANTUM_S,
                "timing request refused for poll backend(s): {} requires a {:.3} us slice, below the measured 1.000 us run_micros bridge quantum",
                match (req.min_pulse_s, req.max_edge_error_s) {
                    (Some(p), Some(e)) => format!("minimum pulse {:.3} us / maximum edge error {:.3} us", p * 1e6, e * 1e6),
                    (Some(p), None) => format!("minimum pulse {:.3} us", p * 1e6),
                    (None, Some(e)) => format!("maximum edge error {:.3} us", e * 1e6),
                    (None, None) => "empty requirement".to_string(),
                },
                requested_chunk * 1e6,
            );
            self.chunk_s = requested_chunk;
        }
        Ok(())
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
        // `chunk_s` is a maximum cadence, not a rounded target: strict timing
        // negotiation may have chosen it as the largest acceptable poll error.
        // Ceiling guarantees the actual `dt / chunks` slice never exceeds it.
        let mut chunks = (dt / self.chunk_s).ceil() as u64;
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
            // Live sessions opt in to stopping a DEAD solve mid-step: once
            // the strict-abort streak trips, every further chunk is another
            // full rescue ladder ground for nothing (a 30 Hz frame is ~334
            // chunks; a manual 1 s step is 10,000), and the session cannot
            // even process its Pause/Reset until the step returns. Headless
            // and CI runs keep the complete march: their failed-window
            // record over the WHOLE requested span is the product.
            if self.stop_when_dead && self.analog_abort_tripped() {
                break;
            }
        }

        StepResult {
            sim_time: self.sim_time,
            uart,
        }
    }

    fn run_chunk(&mut self, chunk: f64, uart: &mut HashMap<String, Vec<u8>>) {
        self.gate_model_peripherals_from_rails();

        // Integer microseconds for `run_micros`, banking the sub-microsecond
        // remainder across chunks so the firmware clock does not drift from sim
        // time. The floored value is deliberately NOT clamped up to 1: a
        // persistent sub-1 µs chunk (a fine `fixed_dt = 0.5e-6`) never reaches a
        // whole banked microsecond, so a `.max(1.0)` would deliver 1 µs every
        // chunk while banking unrepayable negative debt and racing the firmware
        // clock ahead of sim time. Instead the core advances 0 µs on a sub-µs
        // chunk and rolls forward once the banked fraction accrues a full
        // microsecond, keeping `micros_carry` in [0, 1).
        let exact = chunk * 1e6 + self.micros_carry;
        let micros_f = exact.floor();
        self.micros_carry = exact - micros_f;
        let micros = micros_f as u64;
        // Set when any MCU refuses to advance this chunk; folded into the
        // chunk-failure accounting after the analog solve so the run refuses to
        // report a fake-quiet chunk.
        let mut mcu_run_failed = false;

        // Refresh the snapshot the edge-driven 74HC165 read chains sample on a
        // PL load (it fires inside the MCU run below, so it must reflect the
        // PREVIOUS chunk's settled spike-latch voltages).
        if !self.hc165_chains.is_empty() {
            let mut snap = self.input_volts.lock().unwrap_or_else(|e| e.into_inner());
            snap.clear();
            snap.extend_from_slice(&self.node_volts);
        }

        // Per-chunk accessor state, rebuilt as each MCU drains below.
        self.last_chunk_edges.clear();
        self.last_replay_microticks = 0;

        // Nets currently driven by ANY MCU's enabled GPIO driver, for the plain
        // digital-input sync below: an enabled driver is real drive evidence
        // even though pin-driver legs are excluded from the static
        // `digital_in_evidence` index (this is what makes a direct MCU-to-MCU
        // GPIO link readable on the receiving side).
        let mcu_driven_nets: std::collections::HashSet<u32> = self
            .mcus
            .iter()
            .flat_map(|m| m.binding.gpio_drivers.values())
            .filter(|d| d.enabled)
            .map(|d| d.net.0)
            .collect();

        // 1. MCU: inject latest ADC voltages, run the chunk, drain captures.
        for mi in 0..self.mcus.len() {
            self.inject_mcu_inputs(mi, &mcu_driven_nets);
            let m = &mut self.mcus[mi];
            // Cycle counter bracketing this run: the chunk's [start, end) span,
            // so the drained edge stamps normalize to a fraction of the chunk
            // for the analog PWL side. Exact on simavr, coarse on poll backends
            // (flagged by `cycle_exact`).
            let cyc_start = m.core.current_cycle();
            if let Err(e) = m.core.run_micros(micros) {
                // The MCU backend refused to advance (a crashed core, a
                // transport error, a HALT). The firmware side of this chunk did
                // not run, so folding the subsequent solve as a normal quiet
                // chunk would report a fake clean run: flag it and mark the
                // chunk failed below so strict/CI runs abort rather than trust
                // it.
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
                edges: hauksbee_bind::digital::pin_edges_by_pin(&edge_log),
                cycle_span: (cyc_start, cyc_end),
                chunk_s: chunk,
                cycle_exact,
            });
            self.record_firmware_edge_toggles(mi, &edge_log);
            // 1b. Generalized digital replay: drain THIS MCU's ordered,
            // cycle-stamped log and replay it in cycle order through every
            // edge-driven digital element on one path (the 595 chains it owns
            // and any standalone GPIO-clocked shift/latch). Each edge-group
            // sharing a cycle is one micro-tick, so a bit-banged SRCLK/RCLK
            // pulse train clocks the chain per edge instead of collapsing to a
            // level. Only chains whose owning MCU is `mi` are replayed, so a
            // different MCU's identically-named pin cannot inject spurious
            // clocks.
            self.last_replay_microticks += self.replay_digital_edges(mi, &edge_log);
            // 2. Apply GPIO edges to drivers. An edge means the firmware has
            // configured the pin as a driven output, so enable the (initially
            // tri-stated) Thevenin leg before setting its level. This is also
            // the promotion path for a dual-bound analog-capable pin: the first
            // firmware drive of an A-pin enables its driver exactly like any
            // other GPIO, while a pin never driven keeps its driver disabled and
            // stays a pure ADC input.
            let m = &mut self.mcus[mi];
            let edge_pins: std::collections::HashSet<(char, u8)> = edges.keys().copied().collect();
            for ((port, bit), level) in edges {
                m.last_levels.insert((port, bit), level);
                if let Some(drv) = m.binding.gpio_drivers.get_mut(&(port, bit)) {
                    drv.set_enabled(&mut self.circuit, true);
                    let v = if level { m.logic_high_v } else { 0.0 };
                    drv.set_volts(&mut self.circuit, v);
                }
            }
            // 2b. Promotion AND release from the configured pin direction; see
            // `sync_configured_outputs`. On a direction-blind backend the set is
            // empty and the edge-evidence release arm is gated off, so nothing
            // is promoted or torn down there.
            let configured: std::collections::HashSet<(char, u8)> = m
                .core
                .pins_configured_output()
                .into_iter()
                .map(|p| (p.port, p.bit))
                .collect();
            self.sync_configured_outputs(mi, configured, &edge_pins);
        }

        // 1c. Sub-chunk pulse honesty: a GPIO pulse that rose and fell inside
        // THIS chunk is invisible to every tick-evaluated sequential part on its
        // net (they sample once per chunk, against the previous solve), while
        // chain responders resolve the same edges exactly.
        self.detect_short_pulses(chunk);

        // 5(prev). Digital components drive their outputs from current state,
        // sampling the previous chunk's solved node voltages. Chips clocked by
        // an edge path are SKIPPED here: they already ran at edge granularity
        // above, so ticking them once per chunk too would double-drive them with
        // a stale, pulse-collapsed sample.
        {
            let volts = self.node_volts.clone();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for (i, d) in self.digital.iter_mut().enumerate() {
                if self.chain_chips.contains(&i)
                    || self.replay_chips.contains(&i)
                    || self.parallel_memory_chips.contains(&i)
                {
                    continue;
                }
                d.tick(&mut self.circuit, &node_v);
            }
        }
        // 5b(prev). Push the edge-driven chains' latched outputs onto the analog
        // nets. Move the chains out to satisfy the borrow checker (apply needs
        // &mut self.digital and &mut self.circuit), then put them back.
        if !self.chains.is_empty() {
            let mut chains = std::mem::take(&mut self.chains);
            for chain in &mut chains {
                chain.apply(&mut self.digital, &mut self.circuit);
            }
            self.chains = chains;
        }
        self.apply_parallel_memory_outputs();

        // 5b'. Runtime driver-contention monitor. Runs after the MCU edge/DDR
        // sync (which sets the firmware side's driver enables) AND after the
        // digital tick / chain apply (which set the model side's, tri-state
        // included), so both sides' live drive states are current.
        self.detect_driver_contention();

        // 5b2(prev). Refresh every digital part's VCC supply draw for this
        // chunk: drain the output-transition accumulators (filled by the
        // per-chunk ticks AND the edge-granularity replay/chain paths) and set
        // each part's supply Isource to static + n·Cpd_eff·VCC/dt. Chain-owned
        // chips are skipped by the tick loop but still switch, so this runs over
        // ALL digital components. Parts without supply params no-op.
        {
            let volts = self.node_volts.clone();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for d in self.digital.iter_mut() {
                d.update_supply(&mut self.circuit, chunk, &node_v);
            }
        }

        // 5c(prev). Deliver the deferred I2C transaction-end hooks: every slave
        // that saw a STOP during this chunk's MCU run gets `on_stop(ctx)` so it
        // can drive its output nets before this chunk's solve (this is how a
        // firmware MCP4728 write becomes a real VOUT net voltage). The byte
        // dispatch itself runs inside the MCU's `on_i2c` callback, where no
        // TickCtx can be built; the STOP is recorded there and delivered here,
        // the first point the circuit is borrowable.
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

        // Model-owned bus activity is now complete for this chunk. Convert it
        // to the datasheet current envelope before the analogue solve so the
        // same transaction that firmware observed loads the physical rail.
        self.apply_model_peripheral_activity();

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

        // 2d. PWL edge drive. A pin that toggled more than once this chunk
        // collapsed to its final level in the driver path above, which is
        // electrically wrong for any net whose analog response integrates the
        // pulse train (an RC-loaded clock line, a charge pump, a gate filter).
        // For such pins, swap the driver's source to the chunk's exact
        // cycle-stamped PWL waveform for this one solve; the solver's
        // source-breakpoint table then lands the adaptive integrator on every
        // corner. Restored to the settled DC level right after, so the digital
        // tick and the next chunk see the final level.
        //
        // 3. Analog: solve a transient over the chunk; read final voltages and
        // branch currents. A false return means the solve did not converge and
        // this chunk is holding stale voltages: its operating point is fiction,
        // so the stats/stress fold below is skipped for it.
        let pwl_restores = self.apply_pwl_drives(chunk);
        self.chunk_has_pwl_drives = !pwl_restores.is_empty();
        let chunk_converged = self.solve_chunk(chunk);
        self.chunk_has_pwl_drives = false;
        self.restore_pwl_drives(&pwl_restores);
        // An MCU that refused to advance makes this chunk untrustworthy even if
        // the analog march converged, so fold the MCU failure into the same
        // accounting the failed-window and consecutive-failure surfaces read.
        // Only record here when the analog side converged; otherwise
        // `solve_chunk` already recorded this exact window and a second call
        // would double-count it (`sim_time` has not advanced yet, so the window
        // start matches `solve_chunk`'s).
        if mcu_run_failed && chunk_converged {
            self.record_failed_chunk(
                chunk,
                Some(
                    "MCU refused to advance; the digital side of this chunk never ran".to_string(),
                ),
            );
        }
        // The consecutive-failure streak resets ONLY on a fully-successful chunk
        // (analog converged AND the MCU advanced). Resetting on analog
        // convergence alone would let an MCU-failed-but-analog-converged chunk
        // zero the streak before `record_failed_chunk` bumped it back to 1,
        // capping `max_consecutive_failed_chunks` at 1 and defeating the
        // strict/CI abort for a sustained MCU crash.
        if chunk_converged && !mcu_run_failed {
            self.consecutive_failed_chunks = 0;
        }

        // 3b. Peripherals: output sinks sample the freshly-solved voltages.
        //
        // Runs UNCONDITIONALLY, even when `chunk_converged` is false, for two
        // reasons:
        //
        //   * The SPI/I2C slave state machines reached via `post_solve` (e.g.
        //     the SpiBus deselect) are DIGITAL frame-boundary resets. The byte
        //     transfers they frame happened during this chunk's `run_micros`
        //     regardless of whether the analog march converged. Skipping the
        //     per-chunk deselect on a failed chunk would leave a slave stuck
        //     mid-command and desync the NEXT chunk's transaction.
        //   * The only voltage-sampling sink here is `VcdSink`, which emits a
        //     change only when a net's level CROSSES a threshold. A failed chunk
        //     holds (or DC-recovers) the previous voltages, so no spurious
        //     transition is recorded, and the sample lands inside a window
        //     already surfaced as `analog_valid:false` with its exact span.
        //
        // What IS gated on convergence is the stats/stress fold below: those
        // manufacture analog findings and must not run on a solve that never
        // happened.
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

        self.deselect_heuristic_spi_buses(chunk);

        // 4. Advance time (time passes even when the solve failed), then fold
        // the chunk into the running stats and the stress monitor, but ONLY if
        // the analog solve converged. A failed chunk holds the previous chunk's
        // stale voltages, and folding that operating point into the net stats or
        // the stress monitor would manufacture toggles and faults from a solve
        // that never happened; the failed window is recorded and surfaced as
        // `analog_valid:false` instead.
        self.sim_time += chunk;
        if chunk_converged {
            self.update_stats();
            self.accumulate_frame_peaks();

            // 6. Fault / stress monitor: evaluate every device against its
            // ratings using this chunk's solved operating point (may mutate the
            // circuit in destructive mode).
            self.evaluate_faults();
        }
    }

    /// Inject the previous chunk's solved operating point into one MCU core
    /// before it runs: ADC channel voltages, plain digital-input levels, and the
    /// modelled I2C temperature-sensor readings a backend answers itself.
    ///
    /// An ADC channel is skipped when the firmware has promoted its pin to a
    /// GPIO output: an analog-capable pin binds BOTH an ADC channel and a
    /// tri-stated GPIO driver, and once that driver is enabled the pin is being
    /// driven, not read, so injecting a voltage for it would manufacture a
    /// phantom analog reading. Promotion keys on THIS channel's own pin driver,
    /// not on any enabled driver sharing the net (an output pin wired to an ADC
    /// input to self-monitor it must still be injected, and an ADC-only channel
    /// such as A6/A7 owns no driver at all).
    ///
    /// A plain digital input is synced only when its driver is tri-stated, it is
    /// not responder-owned (165 MISO / bit-bang SPI MISO / soft-I2C SDA take
    /// edge-granularity drives from their responder inside the run loop, which a
    /// chunk-boundary level would fight), and its net shows real drive evidence:
    /// a non-pin device with live resistance under [`WEAK_DIGITAL_DRIVE_OHMS`]
    /// (or any non-R/C device), or another enabled GPIO driver. A floating net's
    /// ~0 V solve is the pins' own 1 GΩ legs talking, and pushing it into the
    /// core would defeat an unmodeled internal pull-up.
    ///
    /// Levels use the 0.3/0.7-rail CMOS Vil/Vih convention at the MCU's own
    /// rail, with the in-between band as hysteresis so a mid-rail solve holds
    /// the previous level rather than chattering. `set_digital_in` fires only on
    /// a level CHANGE, so poll backends pay per transition. The whole digital
    /// sync is skipped on the first chunk (`sim_time == 0`): nothing has been
    /// solved yet, and the core's power-on level is the honest state.
    fn inject_mcu_inputs(&mut self, mi: usize, mcu_driven_nets: &std::collections::HashSet<u32>) {
        let m = &mut self.mcus[mi];
        for (&ch, &node) in &m.binding.adc_nets {
            if adc_channel_promoted(&m.binding, ch) {
                continue;
            }
            let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
            m.core.set_analog_in(ch, v.max(0.0));
        }
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
                        devs.iter()
                            .any(|&di| match self.circuit.devices.get(di as usize) {
                                Some(Device::Resistor { ohms, .. }) => {
                                    *ohms < WEAK_DIGITAL_DRIVE_OHMS
                                }
                                // A capacitor cannot decide a DC level.
                                Some(Device::Capacitor { .. }) => false,
                                Some(_) => true,
                                None => false,
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
        // Push modelled I2C temperature-sensor readings into the backend's own
        // emulated device (the QEMU ESP32 tmp105). The simavr/Renode backends
        // ignore this (they answer I2C reads through the `on_i2c` byte
        // callback); QEMU runs the firmware against a real device, so it reads
        // the value through its own I2C controller.
        for bus in &self.i2c_buses {
            let sensors = bus
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .temperature_sensors();
            for (addr, milli_c) in sensors {
                m.core.set_i2c_device_temperature(addr, milli_c);
            }
        }
    }

    /// Chunk-boundary SPI deselect, for HEURISTIC-MODE BUSES ONLY.
    ///
    /// A chunk-boundary deselect stands in for a real chip-select edge, which
    /// simavr's SPI IRQ never surfaces (it reports byte transfers and nothing
    /// else). Applied to every bus unconditionally it is wrong in two ways: two
    /// CS-framed transactions inside one chunk are not separated (the second's
    /// bytes append to the first slave's state), and a single transaction
    /// spanning a chunk boundary is reset mid-way, corrupting the reply.
    ///
    /// Neither can bite a bus with a real CS source. When the binder resolves
    /// the CS net to an MCU pin, the `on_pin_change` closure frames transactions
    /// at the true active-low CS edges (mid-chunk included) via [`CsFrame`]; a
    /// backend that surfaces CS itself (Renode hardware-NSS
    /// `FinishTransmission`) frames via `note_backend_deselect`. For those buses
    /// (`frames_itself()`) a chunk-boundary reset would itself truncate a
    /// legitimately boundary-spanning transaction, so it is skipped and the real
    /// CS edges own framing. Only buses still on the heuristic keep the
    /// boundary deselect, and their coverage says `heuristic` so the guess is
    /// surfaced rather than hidden.
    ///
    /// Runs unconditionally on a failed chunk, same reason as the `post_solve`
    /// deselects: this is a digital frame-boundary reset of the slave command
    /// state machine, not an analog sample.
    fn deselect_heuristic_spi_buses(&mut self, chunk: f64) {
        for bus in &self.spi_buses {
            let mut guard = bus.lock().unwrap_or_else(|e| e.into_inner());
            if guard.frames_itself() {
                continue;
            }
            // Debug-only: a heuristic-mode slave still mid-transaction at the
            // chunk boundary means a transfer spanned the boundary, and the
            // heuristic cannot frame it. Warn rather than silently truncating;
            // spanning is a known limitation of the heuristic path, not a bug to
            // abort on.
            #[cfg(debug_assertions)]
            if guard.slave_mid_transaction() {
                eprintln!(
                    "WARN: SPI bus '{}' deselected mid-transaction at chunk boundary \
                     (transfer spans the {:.3} ms chunk); reply may be truncated. \
                     Declare the CS net, bind a model that maps a `cs` pin, or use \
                     a backend that surfaces chip-select, for exact framing.",
                    guard.id(),
                    chunk * 1e3,
                );
            }
            guard.slave_deselect();
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
    /// (e.g. the LTC6803 balancer-leak current). `None` if absent. An implicit
    /// law is evaluated at the last solved node voltages, which is the current
    /// the solver stamped there.
    pub fn behavioral_law_value(&self, reference: &str, law: &str) -> Option<f64> {
        let d = self.behavioral.iter().find(|d| d.reference == reference)?;
        let node_v = |n: NodeId| self.node_volts.get(n.0 as usize).copied().unwrap_or(0.0);
        d.law_value(&self.circuit, law, &node_v, self.sim_time)
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

    /// One transient march over `[0, tstop)` of this chunk's circuit with the
    /// given options and DC seed. `Ok` carries the last accepted step's
    /// unknowns; `Err` carries whatever the streaming sink captured before the
    /// march failed (the t=0 DC point when the DC solve itself succeeded),
    /// which the refusal path uses as a recovery state.
    ///
    /// `thermal` accumulates (never clears) each monitored device's dissipated
    /// energy integrated over THIS march's accepted steps — the time-weighted
    /// thermal input the stress monitor needs, because a firmware PWM waveform
    /// switching inside the chunk is invisible to the endpoint sample (the
    /// endpoint reads peak or zero depending on phase; the junction heats on
    /// the duty-cycle average). Trapezoid between consecutive accepted steps;
    /// the caller deposits it into the monitor only for the march it adopts.
    ///
    /// Residual: the trapezoid is exact only when dissipation is smooth over
    /// each accepted interval. Firmware/PWL edges land on solver breakpoints
    /// (their corners ARE accepted samples), but an event the integrator
    /// resolves without splitting the interval at the exact corner charges
    /// that one interval at the average of its two sides. The error is
    /// bounded by the event-resolution step width, second-order against the
    /// whole-chunk endpoint sampling this replaced.
    fn march_chunk(
        &self,
        tstop: f64,
        opts: SolverOptions,
        seed: Option<&[f64]>,
        thermal: &mut ChunkThermalAccum,
    ) -> Result<(Vec<f64>, TransientDiagnostics), (String, Vec<f64>)> {
        let t = Transient::new(opts);
        let mut final_x: Vec<f64> = Vec::new();
        // Trapezoid state: the previous accepted step's (time, per-device
        // powers). Local to this march on purpose: a fallback rung or a
        // subdivided quarter restarts chunk-local time at 0, and an interval
        // must never straddle two marches.
        let integrate = self.stress.device_count() > 0;
        let mut prev: Option<(f64, Vec<f64>)> = None;
        let layout = &self.layout;
        let circuit = &self.circuit;
        let stress = &self.stress;
        let res = t.run_streaming_seeded_with_diagnostics(&self.circuit, tstop, seed, |s| {
            final_x.clear();
            final_x.extend_from_slice(s.x);
            if !integrate {
                return;
            }
            // Solver unknown vector -> the monitor's view of this step: node k
            // (non-ground) lives at x[k-1], a device's branch current at
            // x[layout.branch(id)] (same mapping `adopt_chunk_state` publishes).
            let node_v = |n: NodeId| {
                if n.is_ground() {
                    0.0
                } else {
                    s.x.get(n.0 as usize - 1).copied().unwrap_or(0.0)
                }
            };
            let branch_current = |id: DeviceId| layout.branch(id).and_then(|b| s.x.get(b).copied());
            let powers = stress.step_powers(circuit, &node_v, &branch_current);
            if let Some((t0, p0)) = prev.take() {
                let dt = s.time - t0;
                if dt > 0.0 {
                    thermal
                        .energy_j
                        .resize(powers.len().max(thermal.energy_j.len()), 0.0);
                    for (slot, (pa, pb)) in thermal.energy_j.iter_mut().zip(p0.iter().zip(&powers))
                    {
                        *slot += 0.5 * (pa + pb) * dt;
                    }
                    thermal.elapsed_s += dt;
                }
            }
            prev = Some((s.time, powers));
        });
        match res {
            Ok(diagnostics) => Ok((final_x, diagnostics)),
            // The message travels with the recovered state. It carries the
            // solver's blame clause (the net that refused to settle, the devices
            // on it, any near-zero-ohm link poisoning the matrix), which is the
            // difference between a diagnosable refusal and a chunk count.
            // A fallback rung's own failure message is discarded by the ladder:
            // the primary's is the one that describes the board.
            Err(msg) => Err((msg.to_string(), final_x)),
        }
    }

    /// `self.opts` with the maximum step bounded to a small fraction of the
    /// chunk (and the initial step below it), the first fallback lever: a
    /// stiff feature the LTE controller strides at a large step is resolved
    /// instead of fought.
    fn reduced_step_opts(&self, chunk: f64) -> SolverOptions {
        let mut opts = self.opts;
        opts.step = match opts.step {
            hauksbee_solve::StepControl::Adaptive {
                dt_initial,
                dt_min,
                dt_max,
            } => hauksbee_solve::StepControl::Adaptive {
                dt_initial: dt_initial.min(chunk / 4096.0).max(dt_min),
                dt_min,
                dt_max: dt_max.min(chunk / 1024.0),
            },
            hauksbee_solve::StepControl::Fixed { dt } => hauksbee_solve::StepControl::Fixed {
                dt: (dt / 16.0).min(chunk / 1024.0),
            },
        };
        opts
    }

    /// The per-chunk FALLBACK LADDER, tried only after the primary march
    /// failed, in order of increasing desperation and decreasing accuracy.
    /// `thermal` is cleared at the start of every rung so only the adopted
    /// rung's accepted-step energy survives (a failed rung's partial integral
    /// is fiction; rung 4's quarters accumulate additively into one chunk).
    /// Returns the rung that produced a converged end state, with that state,
    /// or `None` when no rung could rescue the chunk (the caller then refuses
    /// exactly as before: stale-voltage holding stays the last resort and
    /// stays loud). This exists to SHRINK the set of windows the run cannot
    /// vouch for, never to manufacture a plausible number for one: every rung
    /// is a real converged solve of the chunk, just by a second-class method,
    /// and the rung is recorded per window, WITH a measured step-doubling
    /// error estimate, so the consumer knows which method produced
    /// which answer and how far the chunk-end state can be trusted.
    fn fallback_ladder(
        &self,
        chunk: f64,
        thermal: &mut ChunkThermalAccum,
    ) -> Option<(
        ChunkFallbackMethod,
        Vec<f64>,
        TransientDiagnostics,
        Option<f64>,
    )> {
        let seed = self.last_dc_seed.as_deref();
        let reduced = self.reduced_step_opts(chunk);
        // Rungs below the forced one are skipped (test hook; 0 in production,
        // so every rung runs).
        let min_rung = self.debug_force_fallback_rung.map(|m| m as u8).unwrap_or(0);
        // Backward Euler at the bounded step: L-stable, so the trapezoidal
        // ringing that kills a stiff chunk is damped; costs the integration
        // order, which the record discloses.
        let mut be = reduced;
        be.integration = hauksbee_solve::Integration::BackwardEuler;

        // Rung 1: the primary integration at a bounded step.
        // Rung 2: backward Euler at the bounded step.
        // Rung 3: backward Euler from a COLD start. Dropping the warm seed
        //   forces the solver's own DC continuation ladder (gmin stepping,
        //   source stepping, the staged rescue) to re-derive this chunk's
        //   operating point from scratch: a warm seed that has drifted onto a
        //   bad basin is exactly the state a continuation restart escapes.
        for (method, opts, seed) in [
            (ChunkFallbackMethod::ReducedStep, reduced, seed),
            (ChunkFallbackMethod::BackwardEuler, be, seed),
            (ChunkFallbackMethod::ColdStartBackwardEuler, be, None),
        ] {
            if min_rung > method as u8 {
                continue;
            }
            if let Some(rescued) = self.try_fallback_rung(chunk, method, opts, seed, thermal) {
                return Some(rescued);
            }
        }

        // Rung 4: subdivide the chunk into quarters and march each with
        // backward Euler at the bounded step, seeding each quarter from the
        // previous quarter's end. Every quarter must converge; a partial
        // chunk is not an answer. The quarters' thermal integrals add up
        // to the one chunk's energy, so the accumulator is cleared once here
        // and shared across all four sub-marches.
        //
        // GUARD: chunk-local source time restarts per sub-march, so the
        // quartering is faithful ONLY when every source the chunk sees is
        // constant across it. A chunk carrying a cycle-stamped within-chunk
        // PWL drive, or any time-varying board source (Pulse/Sin/Pwl/Ramped
        // evaluate against the march's local clock), would have each quarter
        // re-play only the waveform's first quarter, four times: a converged
        // solve of the WRONG forcing. The ladder refuses that rung rather
        // than adopt it; a mis-forced trajectory presented as a rescue is
        // exactly the manufactured number this ladder promises never to
        // produce.
        if !self.chunk_forcing_is_constant() {
            return None;
        }
        thermal.clear();
        let (x, diagnostics) =
            self.march_chunk_subdivided(chunk, 4, be, self.last_dc_seed.clone(), thermal)?;
        if self.insane_node_state(&x).is_some() {
            return None;
        }
        let method = ChunkFallbackMethod::SubdividedBackwardEuler;
        let err = self.fallback_error_estimate(chunk, 4, be, self.last_dc_seed.as_deref(), &x);
        Some((method, x, diagnostics, err))
    }

    /// One rung of [`Scheduler::fallback_ladder`]: discard the previous rung's
    /// partial thermal integral (a failed rung's is fiction), march the chunk
    /// with `opts` from `seed`, and accept the result only when it both
    /// converged AND is physical.
    ///
    /// A rung that "converged" onto a non-physical state (a node beyond the
    /// voltage sanity bound) has not rescued anything: it must not be adopted,
    /// and it must not end the ladder either, because a later, more robust rung
    /// can still produce the real answer. Hence the check per rung rather than
    /// on the ladder's result.
    fn try_fallback_rung(
        &self,
        chunk: f64,
        method: ChunkFallbackMethod,
        opts: SolverOptions,
        seed: Option<&[f64]>,
        thermal: &mut ChunkThermalAccum,
    ) -> Option<(
        ChunkFallbackMethod,
        Vec<f64>,
        TransientDiagnostics,
        Option<f64>,
    )> {
        thermal.clear();
        let (x, diagnostics) = self.march_chunk(chunk, opts, seed, thermal).ok()?;
        if self.insane_node_state(&x).is_some() {
            return None;
        }
        let error_estimate_v = self.fallback_error_estimate(chunk, 1, opts, seed, &x);
        Some((method, x, diagnostics, error_estimate_v))
    }

    /// Whether every source the current chunk sees holds a constant value
    /// across the whole chunk: no within-chunk PWL drive installed and no
    /// time-varying board source. Only then may the chunk be subdivided into
    /// sub-marches (each restarts chunk-local source time at 0).
    fn chunk_forcing_is_constant(&self) -> bool {
        if self.chunk_has_pwl_drives {
            return false;
        }
        !self.circuit.devices.iter().any(|d| {
            matches!(
                d,
                hauksbee_ir::Device::Vsource { kind, .. }
                | hauksbee_ir::Device::Isource { kind, .. }
                    if !matches!(kind, hauksbee_ir::SourceKind::Dc(_))
            )
        })
    }

    /// March `chunk` as `n` equal back-to-back sub-marches with the given
    /// options, each seeded from the previous one's end state (the first from
    /// `seed`). Every sub-march must converge; a partial chunk is not an
    /// answer, so any failure returns `None`. Diagnostics keep the worst
    /// sub-march residual; accepted-step thermal energy accumulates additively
    /// into the one shared accumulator.
    fn march_chunk_subdivided(
        &self,
        chunk: f64,
        n: usize,
        opts: SolverOptions,
        seed: Option<Vec<f64>>,
        thermal: &mut ChunkThermalAccum,
    ) -> Option<(Vec<f64>, TransientDiagnostics)> {
        let sub = chunk / n as f64;
        let mut carry: Option<Vec<f64>> = seed;
        let mut diagnostics = TransientDiagnostics::default();
        for _ in 0..n {
            match self.march_chunk(sub, opts, carry.as_deref(), thermal) {
                Ok((x, sub_diagnostics)) => {
                    // Every quarter must be physical, not just the final one:
                    // an insane intermediate would seed the next quarter (and
                    // deposit its thermal integral) and could wander back to
                    // a sane-looking endpoint, laundering the divergence.
                    if self.insane_node_state(&x).is_some() {
                        return None;
                    }
                    if let Some(candidate) = sub_diagnostics.final_residual {
                        if diagnostics
                            .final_residual
                            .is_none_or(|current| candidate.0 > current.0)
                        {
                            diagnostics.final_residual = Some(candidate);
                        }
                    }
                    carry = Some(x);
                }
                Err(_) => return None,
            }
        }
        carry.map(|x| (x, diagnostics))
    }

    /// The adopted rung's options with the accuracy dial scaled by `factor`
    /// (< 1 tighter, > 1 coarser), for the step-doubling companion: BOTH the
    /// step bounds and the LTE/Newton tolerances scale. Scaling only the step
    /// bound is not enough: on a window whose error the LTE controller (not
    /// the bound) dominates, a bound-only companion re-walks essentially the
    /// same grid and the difference measures nothing (found by the RLC
    /// bracket test, which it failed by 7x).
    fn companion_opts(mut opts: SolverOptions, factor: f64) -> SolverOptions {
        opts.step = match opts.step {
            hauksbee_solve::StepControl::Adaptive {
                dt_initial,
                dt_min,
                dt_max,
            } => hauksbee_solve::StepControl::Adaptive {
                dt_initial: (dt_initial * factor).max(dt_min),
                dt_min,
                dt_max: (dt_max * factor).max(dt_min),
            },
            hauksbee_solve::StepControl::Fixed { dt } => {
                hauksbee_solve::StepControl::Fixed { dt: dt * factor }
            }
        };
        opts.reltol *= factor;
        opts.abstol *= factor;
        opts.vntol *= factor;
        opts.chgtol *= factor;
        opts
    }

    /// MEASURED per-window error estimate for a fallback rung, by the
    /// step-doubling family of constructions: the adopted rung solved the
    /// chunk at accuracy dial h (step bounds and tolerances together);
    /// re-solve the same window (same rung structure, same seed,
    /// `subdivisions` sub-marches) at dial h/4 and difference the end
    /// states.
    ///
    /// The Richardson factors assume the WORST-CASE error ratio between the
    /// two legs, not the nominal dial ratio. A 4x tolerance shift moves the
    /// accepted step only ~2x where the LTE controller dominates (acceptance
    /// goes as h²/tol, so h ∝ √tol), and moves it the full 4x where the step
    /// bound dominates; the error ratio r between the legs is therefore
    /// somewhere in [2, 4] for an effective order >= 1 march. Both factors
    /// below are computed at r = 2, the conservative end:
    ///   refined leg: err = diff·r/(r-1)  -> factor 2   (r=4 would give 4/3)
    ///   coarse leg:  err = diff/(r-1)    -> factor 1   (r=4 would give 1/3)
    /// so when the dial delivers more than 2x the estimate simply
    /// over-covers. (Assuming r = 4 was measured UNSOUND: the coarse leg
    /// under-reported exactly on the stiff path that uses it.) The coarse
    /// leg exists because stiff boards land on the fallback ladder precisely
    /// when tighter marches struggle; it is tried only after the refined
    /// companion fails to converge.
    ///
    /// The recorded estimate doubles the Richardson value (a documented
    /// safety factor: the power-law error model is asymptotic) and adds the
    /// solver's own convergence-tolerance floor reltol·|v| + vntol, below
    /// which no march can vouch for a digit anyway.
    ///
    /// What it estimates: the error THIS CHUNK'S MARCH ADDED to its end
    /// state, relative to an exact solve from the same chunk-start seed (the
    /// state the window publishes and the next chunk is seeded from). Error
    /// carried into the chunk by an earlier fallback chunk's seed is that
    /// chunk's estimate, not this one's; across a merged multi-chunk window
    /// the recorded value is the worst per-chunk estimate, not their sum.
    /// Returns `None` when neither companion converges: no estimate is
    /// invented.
    fn fallback_error_estimate(
        &self,
        chunk: f64,
        subdivisions: usize,
        opts: SolverOptions,
        seed: Option<&[f64]>,
        x_end: &[f64],
    ) -> Option<f64> {
        // Deliberately-broken-estimator test hook: prove the two-sided test
        // catches an estimator that stops measuring. Never set in production.
        if self.debug_zero_fallback_error_estimate {
            return Some(0.0);
        }
        // Worst node-voltage disagreement at the chunk end. `layout.n_nodes`
        // counts the NON-ground node unknowns, which occupy x[0..n_nodes]
        // (the branch-current block follows, exactly as `adopt_chunk_state`
        // reads it); branch currents are excluded from the diff, they are
        // not volts.
        let n_v = self.layout.n_nodes;
        let end_state_diff = |x_ref: &[f64]| {
            let mut diff = 0.0f64;
            let mut v_max = 0.0f64;
            for i in 0..n_v.min(x_end.len()).min(x_ref.len()) {
                diff = diff.max((x_end[i] - x_ref[i]).abs());
                v_max = v_max.max(x_end[i].abs()).max(x_ref[i].abs());
            }
            (diff, v_max)
        };
        // The companion's thermal integral is scratch either way: the adopted
        // rung's energy deposit is the real one.
        let mut scratch = ChunkThermalAccum::default();
        // Worst-case Richardson factors at error ratio r = 2 (see above).
        const REFINED_FACTOR: f64 = 2.0;
        const COARSE_FACTOR: f64 = 1.0;
        const REFINED_TOL_SCALE: f64 = 0.25;
        const COARSE_TOL_SCALE: f64 = 4.0;
        let refined = Self::companion_opts(opts, REFINED_TOL_SCALE);
        let companion = if self.debug_skip_refined_companion {
            None
        } else {
            self.march_chunk_subdivided(
                chunk,
                subdivisions,
                refined,
                seed.map(<[f64]>::to_vec),
                &mut scratch,
            )
            // A companion the sanity policy would reject as a MAIN result
            // measures nothing as a reference either.
            .filter(|(x_ref, _)| self.insane_node_state(x_ref).is_none())
            .map(|(x_ref, _)| (x_ref, REFINED_FACTOR, REFINED_TOL_SCALE))
        };
        let companion = companion.or_else(|| {
            let coarse = Self::companion_opts(opts, COARSE_TOL_SCALE);
            self.march_chunk_subdivided(
                chunk,
                subdivisions,
                coarse,
                seed.map(<[f64]>::to_vec),
                &mut scratch,
            )
            .filter(|(x_ref, _)| self.insane_node_state(x_ref).is_none())
            .map(|(x_ref, _)| (x_ref, COARSE_FACTOR, COARSE_TOL_SCALE))
        })?;
        let (x_ref, richardson, tol_scale) = companion;
        let (diff, v_max) = end_state_diff(&x_ref);
        if std::env::var("HAUKSBEE_DEBUG_FALLBACK_EST").is_ok() {
            eprintln!(
                "[fallback-est] richardson={richardson} diff={diff:.6e} v_max={v_max:.3} \
                 x_end={:?} x_ref={:?}",
                &x_end[..n_v.min(x_end.len())],
                &x_ref[..n_v.min(x_ref.len())]
            );
        }
        if !diff.is_finite() {
            return None;
        }
        // The floor covers BOTH marches' Newton acceptance slack: the adopted
        // march's at the run tolerances (x1) plus the companion's at its
        // scaled tolerances (x tol_scale; the 4x-coarser leg admits up to 4x
        // the slack into `diff`, so it must widen the floor accordingly).
        let floor = (1.0 + tol_scale) * (self.opts.reltol * v_max + self.opts.vntol);
        Some(2.0 * richardson * diff + floor)
    }

    /// A node-voltage magnitude no physical board reaches. A converged solve
    /// whose state exceeds this is a numerically consistent answer to a
    /// question the board never asked (measured: a chunk-updated clamp law on
    /// a floating USB net "converged" at 9.86e11 V, because 1.97 A into a
    /// gmin-only node balances KCL exactly at I/gmin volts), and adopting it
    /// poisons every later chunk's seed. Kilovolt-class inductive spikes and
    /// HV supplies sit orders of magnitude below this bound, so it never bites
    /// a real answer.
    const NODE_VOLTAGE_SANITY_V: f64 = 1e9;

    /// The first node whose solved voltage is non-finite or beyond
    /// [`Self::NODE_VOLTAGE_SANITY_V`], with that voltage. `None` for a sane
    /// state. Only the node block is judged: the branch entries are currents.
    fn insane_node_state(&self, x: &[f64]) -> Option<(usize, f64)> {
        x.iter()
            .take(self.layout.n_nodes.min(x.len()))
            .enumerate()
            .find(|(_, v)| !v.is_finite() || v.abs() > Self::NODE_VOLTAGE_SANITY_V)
            .map(|(i, &v)| (i, v))
    }

    /// The honest refusal for an insane converged state: name the worst net
    /// and the voltage, so the failure reads as the modelling problem it is
    /// rather than a bare "did not converge".
    fn insane_state_reason(&self, unknown: usize, volts: f64) -> String {
        format!(
            "converged state is not board reality: {} at {volts:.3e} V \
             (beyond the {:.0e} V sanity bound); refusing to adopt it",
            hauksbee_solve::blame::name_node_unknown(&self.circuit, unknown),
            Self::NODE_VOLTAGE_SANITY_V
        )
    }

    /// Adopt a converged end state: publish node voltages and branch currents,
    /// and seed the next chunk's DC solve from it.
    fn adopt_chunk_state(&mut self, final_x: Vec<f64>) {
        let n_nodes = self.circuit.node_count();
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
        self.last_dc_seed = Some(final_x);
    }

    /// Solve one chunk's transient. Returns `true` when the analog march
    /// converged (on the primary path or on a RECORDED fallback rung), `false`
    /// when every rung failed and this chunk is holding recovered/stale
    /// voltages (the caller then excludes it from stats and stress, and the
    /// run reports `analog_valid: false` over the failed window).
    fn solve_chunk(&mut self, chunk: f64) -> bool {
        // Keep temperature in sync with the circuit's global temp.
        self.circuit.temp_c = self.opts.temperature_c;
        // Run a short transient; capture the last accepted step's unknowns.
        // Warm-start this chunk's DC operating point from the previous chunk's
        // final unknowns: the operating point barely moves between 100 us chunks,
        // so plain Newton converges in ~1 iteration instead of re-running the
        // cold-start gmin/source-stepping homotopy on the full nonlinear board
        // every chunk. Exact (same root, fewer iters); a size-mismatched or
        // failing seed falls back to the cold solve inside the solver.
        let mut thermal = ChunkThermalAccum::default();
        let primary = if self.debug_force_fallback_rung.is_some() {
            // Test hook: pretend the primary failed so the ladder runs on a
            // board of the test's choosing. Never set in production.
            Err(("primary march skipped by test hook".to_string(), Vec::new()))
        } else {
            self.march_chunk(chunk, self.opts, self.last_dc_seed.as_deref(), &mut thermal)
        };
        // A "converged" state that is not board reality (a net at 1e12 V, an
        // inf/NaN) must never be adopted: it would seed every later chunk and
        // turn one bad answer into a diverged run. Demote it to the failure
        // path with the net named, where the ladder/refusal machinery already
        // tells the truth about unsolved windows.
        let primary = match primary {
            Ok((x, diagnostics)) => match self.insane_node_state(&x) {
                None => Ok((x, diagnostics)),
                Some((unknown, volts)) => {
                    Err((self.insane_state_reason(unknown, volts), Vec::new()))
                }
            },
            err => err,
        };
        let mut failure_reason: Option<String> = None;
        let converged = match primary {
            Ok((x, diagnostics)) => {
                self.adopt_chunk_state(x);
                self.record_residual(diagnostics);
                self.stress
                    .deposit_chunk_energy(&thermal.energy_j, thermal.elapsed_s);
                true
            }
            Err((err, recovered)) => {
                // PRIMARY FAILED. Before giving up on the window, walk the
                // fallback ladder: a chunk rescued by a more robust path is a
                // real solved chunk whose method is recorded, not a stale
                // window. Only when every rung also fails does the refusal
                // path below run, unchanged: this feature shrinks the set of
                // windows the run cannot vouch for, it never papers over one.
                // Every ladder rung applies the same sanity bar internally
                // (an insane "rescue" neither ends the ladder nor gets
                // adopted), so a `Some` here is a converged AND physical
                // answer.
                if let Some((method, x, diagnostics, error_estimate_v)) =
                    self.fallback_ladder(chunk, &mut thermal)
                {
                    self.adopt_chunk_state(x);
                    self.record_residual(diagnostics);
                    // Only the adopted rung's accepted-step thermal integral is
                    // real; the ladder cleared every failed attempt's partial.
                    self.stress
                        .deposit_chunk_energy(&thermal.energy_j, thermal.elapsed_s);
                    self.record_fallback_chunk(chunk, method, error_estimate_v);
                    true
                } else {
                    // Every rung failed. Keep the solver's own refusal message:
                    // it carries the blame clause naming the net that refused to
                    // settle and the offending element(s). Throwing it away is
                    // what left a 259-part board diagnosable only by bisection
                    //. Said once per failed streak rather than per chunk.
                    if self.consecutive_failed_chunks == 0 {
                        eprintln!(
                            "WARNING: analog solve failed at t={:.6e}s: {err}",
                            self.sim_time
                        );
                    }
                    failure_reason = Some(err.clone());
                    self.last_solve_error = Some(err);
                    // A recovered DC bias is only usable if it is itself sane;
                    // a poisoned bias would re-seed the very state this guard
                    // exists to keep out.
                    let recovered = if self.insane_node_state(&recovered).is_some() {
                        Vec::new()
                    } else {
                        recovered
                    };
                    if !recovered.is_empty() {
                        // If the DC operating point at t=0 was still captured
                        // (the streaming sink fires once before the march loop),
                        // use it: a converged DC bias is a far better state to
                        // report and to seed the next chunk from than a hard
                        // zero, which would brown out the modelled MCU. This is
                        // what lets a board whose stiff nonlinear march cannot
                        // progress still hold its DC rails and DAC/peripheral
                        // voltages instead of collapsing the whole co-sim. Still
                        // recorded as a failed chunk below: a recovered bias is
                        // not a solved window.
                        self.adopt_chunk_state(recovered);
                        false
                    } else {
                        // Nothing usable captured: hold previous voltages, and
                        // cold-start the next chunk.
                        self.node_volts.resize(self.circuit.node_count(), 0.0);
                        self.last_dc_seed = None;
                        false
                    }
                }
            }
        };
        // A failed transient (either DC-recovered or held) is not a real solve of
        // this chunk. Record it so the run refuses to pass it off as quiet: the
        // failed-chunk count and window feed coverage/JSON (analog_valid:false),
        // and the consecutive streak drives the strict/CI abort. The
        // streak is NOT reset here on convergence: an MCU-failed chunk can still
        // reach this point analog-converged, and only `run_chunk`, which also
        // knows the MCU status, may reset the streak on a fully-successful chunk.
        if !converged {
            self.record_failed_chunk(chunk, failure_reason);
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

    /// Record one FALLBACK-solved chunk: bump the count and extend/append the
    /// fallback window, merging contiguous windows solved by the SAME rung so
    /// a stretch reads as its true extent (a method change starts a new window,
    /// since the two spans carry different accuracy). A merged window keeps the
    /// WORST (largest) chunk-end error estimate of its chunks; one chunk
    /// without an estimate makes the whole window's estimate honest-absent.
    /// Called from `solve_chunk` before `sim_time` advances, mirroring
    /// `record_failed_chunk`.
    fn record_fallback_chunk(
        &mut self,
        chunk: f64,
        method: ChunkFallbackMethod,
        error_estimate_v: Option<f64>,
    ) {
        self.fallback_chunks += 1;
        let start = self.sim_time;
        let end = self.sim_time + chunk;
        match self.fallback_windows.last_mut() {
            Some(prev) if prev.method == method && (start - prev.end_s).abs() <= chunk * 1e-6 => {
                prev.end_s = end;
                prev.error_estimate_v = match (prev.error_estimate_v, error_estimate_v) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                };
            }
            _ => self.fallback_windows.push(FallbackWindow {
                start_s: start,
                end_s: end,
                method,
                error_estimate_v,
            }),
        }
    }

    fn record_residual(&mut self, diagnostics: TransientDiagnostics) {
        let Some(candidate) = diagnostics.final_residual else {
            return;
        };
        if !candidate.0.is_finite() {
            return;
        }
        if self
            .worst_residual
            .is_none_or(|current| candidate.0 > current.0)
        {
            self.worst_residual = Some(candidate);
        }
    }

    /// Record one non-convergent chunk: bump the failed-chunk count and the
    /// consecutive streak, and extend/append the failed sim-time window. Called
    /// from `solve_chunk` before `sim_time` advances, so `[sim_time, sim_time +
    /// chunk)` is the window this failed chunk covers. Consecutive failed chunks
    /// are merged into one window so a diverged stretch reads as its true extent.
    fn record_failed_chunk(&mut self, chunk: f64, reason: Option<String>) {
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
            _ => {
                self.failed_windows.push((start, end));
                self.failed_window_reasons
                    .push(reason.unwrap_or_else(|| "analog march did not advance".to_string()));
            }
        }
        debug_assert_eq!(self.failed_windows.len(), self.failed_window_reasons.len());
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
        // A forced level bypasses the solver AND the converged-state sanity
        // guard (it is written into `node_volts` after every solve), so it
        // must clear the same bar here: a NaN/inf/absurd force would be the
        // one remaining door for a non-physical published voltage.
        if !high_volts.is_finite()
            || !low_volts.is_finite()
            || high_volts.abs() > Self::NODE_VOLTAGE_SANITY_V
            || low_volts.abs() > Self::NODE_VOLTAGE_SANITY_V
        {
            eprintln!(
                "refusing to force net '{net}' to {high_volts:.3e}/{low_volts:.3e} V: \
                 not board reality (beyond the {:.0e} V sanity bound)",
                Self::NODE_VOLTAGE_SANITY_V
            );
            return false;
        }
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
    /// voltages and cannot vouch for analog-derived findings there.
    pub fn failed_chunk_count(&self) -> u64 {
        self.failed_chunks
    }

    /// The sim-time windows `[start_s, end_s)` where the analog solve failed,
    /// merged where consecutive. Empty on a clean run.
    pub fn failed_windows(&self) -> &[(f64, f64)] {
        &self.failed_windows
    }

    /// The solver's message from the most recent non-convergent chunk, or
    /// `None` if every chunk this run solved. Pairs with `failed_windows`, so
    /// a failed span carries its cause, not just its extent.
    pub fn last_solve_error(&self) -> Option<&str> {
        self.last_solve_error.as_deref()
    }

    /// The solver's refusal message for each failed window, parallel to
    /// [`Self::failed_windows`]. Each carries the blame clause naming the net
    /// that refused to settle, the devices on it, and any element whose
    /// conductance is outside the board's own distribution. Empty on a
    /// clean run.
    pub fn failed_window_reasons(&self) -> &[String] {
        &self.failed_window_reasons
    }

    /// The failed windows with their diagnoses, formatted one per line, ready
    /// to print. Empty on a clean run.
    pub fn failed_window_diagnoses(&self) -> Vec<String> {
        self.failed_windows
            .iter()
            .zip(self.failed_window_reasons.iter())
            .map(|((a, b), why)| format!("{:.3}-{:.3} ms: {}", a * 1e3, b * 1e3, why))
            .collect()
    }

    /// Number of chunks this run whose PRIMARY analog solve failed but a
    /// fallback integration rung produced a converged answer. Zero on a run
    /// the primary path carried whole. These chunks are solved (they count
    /// toward neither `failed_chunk_count` nor the strict abort), but their
    /// numbers were produced by a second-class method whose known numerical
    /// trade-off is recorded per window (`fallback_windows`).
    pub fn fallback_chunk_count(&self) -> u64 {
        self.fallback_chunks
    }

    /// The sim-time windows `[start_s, end_s)` solved by a fallback rung, with
    /// the rung that produced each and its measured chunk-end error estimate,
    /// merged where consecutive with the same rung. Empty on a run the primary
    /// path carried whole.
    pub fn fallback_windows(&self) -> &[FallbackWindow] {
        &self.fallback_windows
    }

    /// The machine-readable numerical qualification for the run so far.
    /// Tolerances and methods are the options actually used. Failed spans are
    /// explicit invalid windows; an absent residual means unmeasured.
    pub fn error_budget(
        &self,
    ) -> Result<hauksbee_ir::evidence::ErrorBudget, hauksbee_ir::evidence::EvidenceError> {
        let fallback: Vec<(f64, f64, &str)> = self
            .fallback_windows
            .iter()
            .map(|w| (w.start_s, w.end_s, w.method.as_str()))
            .collect();
        let mut budget = crate::error_budget::transient_error_budget(
            &self.opts,
            0.0,
            self.sim_time,
            self.chunk_s,
            &self.failed_windows,
            &fallback,
        )?;
        // A post-solve force changes reported node values without re-solving
        // the equations. The measured residual belongs to the pre-override
        // solve and must not be presented as qualification of those values.
        if self.forced_node_volts.is_empty() {
            if let Some((max_abs, unknown)) = self.worst_residual {
                let at = (1..self.circuit.node_count())
                    .map(|node| NodeId(node as u32))
                    .find(|node| self.layout.node(*node) == Some(unknown))
                    .map(|node| self.circuit.node_name(node).to_string())
                    .unwrap_or_else(|| format!("unknown #{unknown}"));
                budget = budget.with_residual(hauksbee_ir::evidence::Residual::new(max_abs, at)?);
            }
        }
        Ok(budget)
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

    /// The net names currently carrying a forced post-solve voltage override,
    /// sorted for deterministic output. Empty in every ordinary run.
    pub fn forced_net_names(&self) -> Vec<String> {
        // Keys are NodeId indices (see force_net_voltage_windowed), so the
        // circuit's own reverse map is the resolution.
        let mut names: Vec<String> = self
            .forced_node_volts
            .keys()
            .map(|&node| self.circuit.node_name(NodeId(node as u32)).to_string())
            .collect();
        names.sort();
        names
    }

    /// A forced run must be SELF-DECLARING in the published evidence. The
    /// residual suppression in [`Self::error_budget`] is honest about what the
    /// residual can vouch for, but an absent residual is indistinguishable
    /// from an unmeasured one; a reader of the evidence JSON must be told,
    /// in so many words, that these nets present an override rather than a
    /// converged solution. Returns at most one assumption, scoped to the
    /// forced nets; empty when nothing is forced.
    pub fn forced_voltage_assumptions(
        &self,
    ) -> Result<Vec<hauksbee_ir::evidence::Assumption>, hauksbee_ir::evidence::EvidenceError> {
        use hauksbee_ir::evidence::{Assumption, AssumptionSource, NetScope, Scope, Subject};
        let nets = self.forced_net_names();
        if nets.is_empty() {
            return Ok(Vec::new());
        }
        let list = nets.join(", ");
        Ok(vec![Assumption::reduced_fidelity(
            AssumptionSource::Scheduler,
            Subject::new(
                "scheduler/forced-voltages",
                "post-solve forced node voltages",
            ),
            Scope::Nets(NetScope::new(nets.clone(), None)?),
            &format!(
                "the reported voltages on {list} were written by a forced override after \
                 the solve, not converged in place, and the run's residual is withheld \
                 because it belongs to the pre-override solution"
            ),
            "remove the force, or validate the forced trajectory independently and cite \
             that validation as the evidence for these nets",
        )])
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
    ///
    /// The MCU cores are REBOOTED (reset vector, power-on registers, firmware
    /// kept), not merely re-clocked: rewinding time while the firmware kept
    /// its wedged PC made Reset a lie for the one part users press it for.
    pub fn reset_run_state(&mut self) {
        self.sim_time = 0.0;
        self.micros_carry = 0.0;
        self.failed_chunks = 0;
        self.failed_windows.clear();
        self.failed_window_reasons.clear();
        self.fallback_chunks = 0;
        self.fallback_windows.clear();
        self.worst_residual = None;
        self.consecutive_failed_chunks = 0;
        self.max_consecutive_failed_chunks = 0;
        self.last_solve_error = None;
        for st in self.stats.values_mut() {
            *st = Default::default();
        }
        self.firmware_edge_toggles.clear();
        self.frame_peak_current.clear();
        self.frame_v_extremes.clear();
        self.faults_pending.clear();
        // Run-accumulated co-sim findings: a replay must re-detect (and a
        // stale once-per-net guard would silently swallow a real re-fire).
        self.short_pulses.clear();
        self.short_pulse_nets.clear();
        self.timing_refusals.clear();
        self.timing_refusal_nets.clear();
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
        // Reboot the MCU cores: restarting the sim clock while the firmware
        // keeps its old PC and SRAM would leave a wedged firmware wedged
        // forever, with no recovery from a stuck serial protocol short of
        // killing the server. A reset that rewinds time must also pulse the
        // cores' RESET line.
        for mi in 0..self.mcus.len() {
            match self.mcus[mi].core.reset() {
                Ok(()) => {
                    let m = &mut self.mcus[mi];
                    // The core's IO registers are back at power-on, so the
                    // coupling caches must match: stale levels would replay
                    // the wedged run's pin state onto the rebooted firmware.
                    m.last_levels.clear();
                    m.configured_outputs.clear();
                    m.digital_in_levels.clear();
                    if let Ok(mut sh) = m.shared.lock() {
                        sh.pin_edges.clear();
                        sh.pin_edge_log.clear();
                        sh.uart_out.clear();
                    }
                    // GPIO drivers back to Hi-Z until the rebooted firmware
                    // configures its pins again (the promotion/edge paths
                    // re-enable them), matching real reset-line behaviour.
                    for drv in self.mcus[mi].binding.gpio_drivers.values_mut() {
                        drv.set_enabled(&mut self.circuit, false);
                    }
                }
                // A backend that cannot reboot keeps ALL its coupling state:
                // clearing caches for a core that did not actually restart
                // would tri-state its drivers with no firmware edges coming
                // to re-enable them. Loud, not silent, so the user knows the
                // firmware survived the reset.
                Err(e) => eprintln!(
                    "WARNING: {}: reset did not reboot the MCU core ({e}); \
                     firmware state persists across this reset",
                    self.mcus[mi].binding.reference
                ),
            }
        }
        // The analog operating point restarts with the story: drop the warm
        // start and the previous run's solved node state so the first chunk
        // after reset re-solves from the same cold start as construction.
        self.last_dc_seed = None;
        self.node_volts.iter_mut().for_each(|v| *v = 0.0);
        self.branch_x.iter_mut().for_each(|v| *v = 0.0);
        for leg in &mut self.model_peripheral_power {
            leg.powered = false;
            leg.last_current_a = 0.0;
            leg.set_bus_powered(false);
            leg.drain_activity();
            set_isource_dc(&mut self.circuit, leg.isource, 0.0);
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
    /// * **Release**, a pin the report does not list as an output gets its
    ///   driver DISABLED again, so the net is genuinely let go instead of
    ///   staying clamped at its stale driven level (the latched-bus failure).
    ///   Released pins are those the report contradicts with fresh evidence:
    ///   in last chunk's set, or carrying PORT edges from this chunk (a pin
    ///   that flipped OUTPUT, wrote, and flipped back to INPUT inside one
    ///   chunk appears in neither chunk-end set; see the inline comment). An
    ///   externally promoted driver with neither is left alone, and a
    ///   direction-blind backend never releases anything.
    fn sync_configured_outputs(
        &mut self,
        mi: usize,
        configured: std::collections::HashSet<(char, u8)>,
        edge_pins: &std::collections::HashSet<(char, u8)>,
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
        // Release: a pin is torn down when the direction report contradicts
        // its enabled driver AND there is fresh evidence the report covers it:
        // either it was in the PREVIOUS chunk's set (the ordinary
        // output->input hand-off), or it produced PORT edges THIS chunk (the
        // step-2 enable ran against a chunk-end direction of INPUT). The
        // second clause closes the within-one-chunk OUTPUT->write->INPUT
        // window the difference rule alone misses: the NEP EEPROM data bus
        // does an 87 us write-then-/OE-poll cycle inside a 1 ms chunk, its
        // pins appear in NEITHER chunk-end set, and the stale edge-enabled
        // driver then fought the EEPROM's legitimate read drive whenever a
        // boundary landed in a poll's /OE-low window (the driver-contention
        // monitor correctly refused those runs). A driver enabled by neither
        // path (external promotion through the binding, e.g. a test or a
        // future set-pin API) is deliberately left alone, and a
        // direction-blind backend (empty set every chunk, no DDR hook) never
        // reaches either clause with a live pin, so its edge-enabled drivers
        // are never torn down.
        let mut release: Vec<(char, u8)> = m
            .configured_outputs
            .difference(&configured)
            .copied()
            .collect();
        if m.core.drive_direction_observable() {
            // The edge-evidence arm needs a trustworthy direction report: on
            // a direction-blind (or partially covered) backend, "edged but
            // not in the set" describes every driven pin, and releasing them
            // would tear down the edge-driven enables that are those
            // backends' only signal. The dropped-from-last-chunk arm above
            // stays unconditional: it only ever names pins the core itself
            // reported as outputs in the previous chunk.
            release.extend(edge_pins.difference(&configured).copied());
        }
        for (port, bit) in release {
            if let Some(drv) = m.binding.gpio_drivers.get_mut(&(port, bit)) {
                if drv.enabled {
                    drv.set_enabled(&mut self.circuit, false);
                }
            }
        }
        m.configured_outputs = configured;
    }

    /// Current voltage of a net by name.
    pub fn net_voltage(&self, net: &str) -> Option<f64> {
        let node = self.net_nodes.get(net)?;
        self.node_volts.get(node.0 as usize).copied()
    }

    /// The name of the net on `node`, or `"node N"` for an unnamed one.
    fn net_name_of(&self, node: u32) -> String {
        self.net_name_for(NodeId(node))
            .map(str::to_string)
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
    /// (`hauksbee_checks::checks::contention`) by construction: the model drivers
    /// scanned here are exactly the [`hauksbee_bind::drivers::PinDriver`]s the binder
    /// stamped from [`hauksbee_bind::digital::output_roles`] (the static check's own
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

    /// Sub-chunk GPIO pulse warnings raised this run, one per
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

    /// The board net name a node interns as, when the board named it.
    fn net_name_for(&self, target: NodeId) -> Option<&str> {
        self.net_nodes
            .iter()
            .find(|(_, n)| n.0 == target.0)
            .map(|(name, _)| name.as_str())
    }

    /// Per-MCU sets of the pins the firmware currently has configured as
    /// outputs, read once per query (each read can be a backend round-trip).
    fn configured_output_pins(&self) -> Vec<std::collections::HashSet<(char, u8)>> {
        self.mcus
            .iter()
            .map(|m| {
                m.core
                    .pins_configured_output()
                    .into_iter()
                    .map(|p| (p.port, p.bit))
                    .collect()
            })
            .collect()
    }

    /// Sorted, deduped board-net names for every MCU GPIO pin `keep` accepts,
    /// called with the MCU's index, the MCU, the pin and the net's name.
    ///
    /// Promoted analog pins (Nano A0..A5 = PC0..PC5) are included: `bind_mcu`
    /// stamps a real GPIO driver on them through the same apin fallback, so a
    /// firmware-driven A-pin is modelled electrically and must be visible to
    /// every caller below (else a held-high enable on an A-pin is silently
    /// omitted from the boot-hazard report).
    fn gpio_nets_where(
        &self,
        mut keep: impl FnMut(usize, &LiveMcu, (char, u8), &str) -> bool,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (mi, m) in self.mcus.iter().enumerate() {
            for (role, &node) in &m.binding.role_nets {
                let Some(pin) = gpio_of_role(role, m.binding.module)
                    .or_else(|| apin_gpio_of_role(role, m.binding.module))
                else {
                    continue;
                };
                let Some(name) = self.net_name_for(node) else {
                    continue;
                };
                if keep(mi, m, pin, name) {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
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
    ///
    /// "Held HIGH" is the firmware's most recent drive being high AND the net
    /// physically reaching a clear logic-high (>= 3.0 V, the floor
    /// `update_stats` uses) with at most one rising edge: driven once and held.
    /// Many edges is SPI / UART / PWM / a blinking LED, not a control hold.
    pub fn firmware_held_high_nets(&self) -> Vec<String> {
        self.gpio_nets_where(|_, m, pin, name| {
            m.last_levels.get(&pin) == Some(&true)
                && self
                    .stats
                    .get(name)
                    .is_some_and(|st| st.max_v >= 3.0 && st.toggles <= 1)
        })
    }

    /// How a net arrived at the level it is sitting at.
    ///
    /// The distinction boot coverage lives or dies on: a control net that
    /// reaches its level because a pull-up holds it there has NOT been shown to
    /// boot, and calling that a pass vouches for firmware behaviour that never
    /// ran.
    pub fn level_provenance(&self, net: &str) -> LevelProvenance {
        if self.firmware_driven_nets().iter().any(|n| n == net) {
            return LevelProvenance::FirmwareDriven;
        }
        if self.mcus.is_empty() {
            // No MCU is modelled at all, so nothing digital could have driven
            // anything. The level is the passive network's, with certainty; pin
            // direction observability does not enter into it.
            return LevelProvenance::Passive;
        }
        if !self.drive_direction_observable() {
            // A levels-only backend cannot tell a held-low output from a
            // floating input, so neither "firmware drove it" nor "the network
            // did" is a claim this run can support.
            return LevelProvenance::Unobservable;
        }
        LevelProvenance::Passive
    }

    /// One clause stating how `net` arrived at `volts` at `t_ms`, for any caller
    /// about to report that a control net reached a level.
    ///
    /// The wording is the point. "Driven to 3.3 V at 1.00 ms" is a claim about
    /// firmware; when no firmware drove the net, the honest sentence names the
    /// passive network instead, and when the backend cannot tell, it says that.
    pub fn level_reached_clause(&self, net: &str, volts: f64, t_ms: f64) -> String {
        match self.level_provenance(net) {
            LevelProvenance::FirmwareDriven => format!(
                "control net '{net}' was driven to {volts:.3} V by firmware at {t_ms:.2} ms"
            ),
            LevelProvenance::Passive => format!(
                "control net '{net}' reached {volts:.3} V at {t_ms:.2} ms PASSIVELY: no \
                 firmware drove it, so this is the passive network's level (a pull resistor \
                 or a rail), not a firmware action"
            ),
            LevelProvenance::Unobservable => format!(
                "control net '{net}' reached {volts:.3} V at {t_ms:.2} ms, but this MCU \
                 backend cannot report pin direction, so whether firmware drove it or the \
                 passive network held it there is UNKNOWN"
            ),
        }
    }

    /// MCU GPIO nets the firmware drove to a *defined* level during the run:
    /// either it wrote the pin (a `last_levels` entry, high or low) or it
    /// configured the pin as an output (so an output-low-held pin counts as
    /// driven LOW, not floating). A net NOT in this set was never driven by any
    /// MCU: it floats. Used by the boot-state panel to classify each gate as
    /// driven-high / driven-low / floating without enabling any circuit driver.
    pub fn firmware_driven_nets(&self) -> Vec<String> {
        let configured = self.configured_output_pins();
        self.gpio_nets_where(|mi, m, pin, _| {
            m.last_levels.contains_key(&pin) || configured[mi].contains(&pin)
        })
    }

    /// MCU GPIO nets whose pin the firmware actively configured as an OUTPUT
    /// (DDR set, by latest direction). A net high in this set is a strong
    /// push-pull drive; a net high but NOT in it is a weak internal pull-up.
    /// Observation metadata (from `pins_configured_output`), never a drive.
    pub fn firmware_output_configured_nets(&self) -> Vec<String> {
        let configured = self.configured_output_pins();
        self.gpio_nets_where(|mi, _, pin, _| configured[mi].contains(&pin))
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

    /// Generalized digital edge replay: drain MCU `mi`'s ordered,
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
                hauksbee_bind::digital::replay_components_on_edges(
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

    /// Fold every callback edge into the assertion/report count for its bound
    /// net before the chunk-end analog sample can collapse a pulse.
    fn record_firmware_edge_toggles(&mut self, mi: usize, edge_log: &[PinEdge]) {
        for edge in edge_log {
            let Some(driver) = self.mcus[mi]
                .binding
                .gpio_drivers
                .get(&(edge.port, edge.bit))
            else {
                continue;
            };
            let net = self.circuit.node_name(driver.net);
            *self
                .firmware_edge_toggles
                .entry(net.to_string())
                .or_default() += 1;
        }
    }

    /// The PWL edge drive, and its policy.
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
    /// bounds a pathological edge storm before it can blow up the solve. If it
    /// is exceeded the scheduler records a hard timing refusal; strict/CI
    /// callers invalidate the result instead of silently trusting the DC
    /// fallback.
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
                if transitions.len() < 2 {
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
                if transitions.len() > MAX_TRANSITIONS_PER_PIN {
                    if self.timing_refusal_nets.insert(net.0) {
                        let net_name = self.circuit.node_name(net);
                        self.timing_refusals.push(format!(
                            "PWL replay refused on net {net_name}: {} GPIO transitions in one chunk exceed the implementation budget of {MAX_TRANSITIONS_PER_PIN}; the analog path retained only the final DC level",
                            transitions.len()
                        ));
                    }
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
    /// wiring.
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
        self.short_nets_classified(net_a, net_b, true)
    }

    /// Stamp one physical bridge, optionally recording it as a defect. A
    /// schematic-declared net tie needs the same milliohm bridge in the circuit
    /// as any other copper contact, but must not manufacture a `short` fault.
    fn short_nets_classified(&mut self, net_a: &str, net_b: &str, defect: bool) -> bool {
        let node_a = self.net_nodes.get(net_a).copied();
        let node_b = self.net_nodes.get(net_b).copied();
        let (Some(a), Some(b)) = (node_a, node_b) else {
            return false;
        };
        let Some(_name) =
            hauksbee_bind::shorts::stamp_bridge(&mut self.circuit, a, b, net_a, net_b)
        else {
            return false;
        };
        // The new device may add a branch unknown; rebuild the MNA layout and
        // resize the branch-current buffer so subsequent solves are consistent.
        self.relayout();
        if defect {
            self.faults_pending.push(hauksbee_bind::shorts::short_fault(
                net_a,
                net_b,
                self.sim_time,
            ));
        }
        true
    }

    /// Apply every true overlap a DRC report found, bridging each shorted net
    /// pair. Clearance-only violations are not applied (they are near-short
    /// risks, not actual shorts). Returns the number of bridges stamped.
    pub fn apply_drc_shorts(&mut self, report: &hauksbee_extract::DrcReport) -> usize {
        self.apply_drc_shorts_with_qualification(report, None)
    }

    /// Apply every physical DRC contact while recording a `short` fault only
    /// for net pairs that retain at least one unqualified contact. A net pair is
    /// one electrical bridge even when the geometry reports it on several
    /// layers; if any spatial contact remains undeclared, the pair remains a
    /// defect.
    pub fn apply_drc_shorts_with_qualification(
        &mut self,
        report: &hauksbee_extract::DrcReport,
        qualification: Option<&hauksbee_extract::DrcTieQualification>,
    ) -> usize {
        let pairs = hauksbee_bind::shorts::shorted_name_pairs(report);
        let mut applied = 0;
        for (a, b) in pairs {
            let defect = report.shorts().any(|finding| {
                let same_pair = (finding.net_a_name == a && finding.net_b_name == b)
                    || (finding.net_a_name == b && finding.net_b_name == a);
                same_pair && qualification.is_none_or(|ties| ties.tie_for(finding).is_none())
            });
            if self.short_nets_classified(&a, &b, defect) {
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

/// Point one model-peripheral supply leg's Isource at `amps`.
fn set_isource_dc(circuit: &mut Circuit, id: DeviceId, amps: f64) {
    match circuit.devices.get_mut(id.0 as usize) {
        Some(Device::Isource { kind, .. }) => *kind = SourceKind::Dc(amps),
        Some(other) => panic!(
            "model peripheral supply leg {:?} changed type: {other:?}",
            id
        ),
        None => panic!("model peripheral supply leg {:?} disappeared", id),
    }
}

/// Route one SPI byte to the correct bus among all attached slaves.
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
/// add a new part, or override a builtin, purely as data.
///
/// For the QEMU backend the firmware path is either the app `.elf` (the
/// backend builds the bootable merged image from it in-process) or an
/// esptool-merged flash image; QEMU boots it at spawn, so there is no separate
/// load step (the trait's `load_firmware` is a no-op for QEMU).
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
        instantiate_renode(part, binding.external_clock_present)?
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
/// firmware path (required; QEMU boots from it) is the app `.elf` or a merged
/// flash image; see `QemuBackend::new` for the two accepted shapes.
#[cfg(feature = "qemu")]
fn instantiate_qemu(
    part: &str,
    firmware: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    use hauksbee_mcu::QemuBackend;
    let config = resolve_qemu_config(part)?;
    let flash = firmware.ok_or_else(|| {
        anyhow::anyhow!(
            "the qemu:{part} backend needs firmware to boot: pass the app .elf \
             your build produced (a merged flash image from esptool merge_bin \
             also works)"
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

/// Whether a pin role names a pad fed DIRECTLY from a board rail, so the part's
/// abs-max supply voltage is the ceiling for that node.
///
/// A module's `vin`/`raw` is deliberately absent: those feed an on-board regulator
/// and their ceiling is a different number from the core's.
fn is_direct_supply_role(role: &str) -> bool {
    matches!(
        role,
        "vcc" | "avcc" | "vdd" | "vdda" | "dvdd" | "avdd" | "iovdd" | "vddio" | "vregvdd" | "5v"
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
///
/// The part token is passed STRAIGHT to simavr, which owns the list of cores it
/// has and returns a null MCU for anything else (`AvrMcu::new` turns that into a
/// named error). Silently substituting an ATmega328P for an unrecognised token
/// would be the wrong-ISA failure the QEMU and Renode paths above refuse by
/// name: an ATmega1284P board has ports A-D, 16 KB of SRAM and two USARTs, and
/// running its firmware on a 2 KB three-port core produces a plausible-looking
/// trace of a chip that is not on the board. A board asking for a core simavr
/// does not have fails loudly and says which cores it does have.
///
/// Port hooks are registered per part because the hook set is what connects
/// firmware GPIO writes to the analog solve; a port the part does not have is
/// skipped by `register_port_hooks` (its IRQ is null), so the lists below are
/// upper bounds, not assertions.
///
/// NOT handled here: the clock. Every core is instantiated at 16 MHz regardless
/// of the crystal the board fits, which is a pre-existing simplification (the
/// binder records `external_clock_present` but no frequency), and it makes
/// firmware-timed results wrong by the ratio of the two clocks on any board that
/// is not a 16 MHz one. Carrying the crystal frequency through `McuBinding` is
/// the fix; it is not this function's to make up.
#[cfg(feature = "avr")]
fn instantiate_avr(backend: &str) -> anyhow::Result<Box<dyn Mcu + Send>> {
    let part = backend.strip_prefix("simavr:").unwrap_or(backend);
    let mut avr = AvrMcu::new(part, 16_000_000).map_err(|e| {
        anyhow::anyhow!(
            "{e}. The AVR co-sim runs whatever core simavr has, and will not \
             substitute a different one: an ATmega328P standing in for the part \
             the board fits would report a chip that is not on the board. \
             simavr's AVR cores include atmega8/16/32/48/88/168/328/328p/328pb, \
             atmega164/324/644/1284/1284p, atmega128/1280/1281/2560/2561, \
             atmega16m1/32u4/64m1, at90usb162 and the attiny family. Override \
             the part with a --models-dir entry that sets `backend` explicitly \
             if the board's value string names a core by a different spelling."
        )
    })?;
    // One superset list per family SHAPE, not per part. Registering a port the
    // part does not have is a no-op (its IRQ is null), so a list may be generous;
    // what it must not be is short, because a port left unhooked is a pin whose
    // firmware writes never reach the analog solve.
    //
    // The lists below are honest about their coverage rather than complete:
    //   - The megaX4 arm covers A-D, which is right for 164/324/644/1284. It also
    //     claims A for the 16m1/64m1, which have no PORTA and do have PORTE; those
    //     two parts therefore lose PORTE here.
    //   - The mega1280/2560 arm stops at H because `port_hook_fn` has no J/K/L
    //     hooks to register, so those ports are unreachable from this layer either
    //     way.
    //   - The default is B/C/D, which is right for the mega48/88/168/328 line and
    //     WRONG for the attiny family the error message above advertises: an
    //     ATtiny85 has only PORTB (harmless), but an ATtiny84 or 2313 has PORTA
    //     and would lose it. No board in the corpus fits one; when one does, it
    //     needs an arm here rather than the default.
    let ports: &[char] = match part {
        "atmega1280" | "atmega1281" | "atmega2560" | "atmega2561" | "atmega128" | "atmega128L"
        | "atmega128rfa1" | "atmega128rfr2" => &['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H'],
        "atmega32u4" | "at90usb162" => &['B', 'C', 'D', 'E', 'F'],
        "atmega164" | "atmega164p" | "atmega164pa" | "atmega324" | "atmega324a" | "atmega324p"
        | "atmega324pa" | "atmega644" | "atmega644p" | "atmega1284" | "atmega1284p"
        | "atmega16" | "atmega32" | "atmega16m1" | "atmega64m1" => &['A', 'B', 'C', 'D'],
        _ => &['B', 'C', 'D'],
    };
    avr.register_port_hooks(ports);
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
fn instantiate_renode(
    part: &str,
    external_clock_present: bool,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    use hauksbee_mcu::RenodeBackend;
    let config = resolve_renode_config(part)?.with_external_clock_present(external_clock_present);
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
/// half of "add a Renode MCU purely as data": an override-dir
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

/// How a net arrived at the level it is sitting at.
///
/// Reported by [`Scheduler::level_provenance`]. A caller about to claim a
/// control net "was driven" to a level must consult this first: a level reached
/// passively is a fact about the passive network, not about firmware, and
/// presenting it as the latter would be a false claim of coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelProvenance {
    /// An MCU drove the net to a defined level this run.
    FirmwareDriven,
    /// Nothing drove it: the level is the passive network's, a pull resistor or
    /// a rail. Reported only when the run can actually support the claim, i.e.
    /// there is no MCU at all, or every MCU's backend reports pin direction.
    Passive,
    /// The MCU backend cannot report pin direction, so neither claim is honest.
    Unobservable,
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
fn instantiate_renode(
    _part: &str,
    _external_clock_present: bool,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
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
    Vec<hauksbee_bind::digital::Hc595Chain>,
    Vec<usize>,
    std::collections::HashSet<usize>,
) {
    use hauksbee_bind::digital::{order_595_chains, Hc595Chain};

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

/// Whether every 595 chain whose parallel outputs feed a synchronous memory
/// has a control state the responder can prove before the first analogue solve.
/// Mutable OE can make the stored output byte electrically absent, and an
/// externally pulled control pin invalidates the chain controller's built-in
/// reset defaults. Chains not referenced by this memory are deliberately
/// ignored.
fn referenced_595_controls_are_proven(
    referenced: &std::collections::HashSet<usize>,
    controls: &[(Option<(char, u8)>, Vec<(char, u8)>)],
    pulled_pins: &std::collections::HashSet<(char, u8)>,
) -> bool {
    referenced.iter().all(|&index| {
        controls.get(index).is_some_and(|(oe_n, pins)| {
            oe_n.is_none() && pins.iter().all(|pin| !pulled_pins.contains(pin))
        })
    })
}

/// Identify standalone GPIO-edge-driven digital components for the generalized
/// replay, and build each MCU's GPIO `(port,bit)` -> driven-net map.
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
    use hauksbee_bind::digital::order_165_chains;

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
        // Real SPI chip-select framing: if this pin drives a slave's
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
    use hauksbee_ir::SourceKind;

    const POWERED_EEPROM_BOARD: &str = r#"(kicad_pcb (version 20240108) (generator pcbnew)
  (general (thickness 1.6))
  (paper "A4")
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (36 "B.SilkS" user "b.silkscreen") (37 "F.SilkS" user "f.silkscreen") (44 "Edge.Cuts" user))
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "SCL")
  (net 4 "SDA")
  (footprint "Package_TO_SOT_SMD:SOT-23-5" (layer "F.Cu")
    (at 100 100)
    (property "Reference" "U1" (at 0 -2 0) (layer "F.SilkS"))
    (property "Value" "AT24CS01-STUM" (at 0 2 0) (layer "F.Fab"))
    (pad "1" smd roundrect (at -0.95 -1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (roundrect_rratio 0.25) (net 3 "SCL"))
    (pad "2" smd roundrect (at -0.95 0) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (roundrect_rratio 0.25) (net 1 "GND"))
    (pad "3" smd roundrect (at -0.95 1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (roundrect_rratio 0.25) (net 4 "SDA"))
    (pad "4" smd roundrect (at 0.95 1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (roundrect_rratio 0.25) (net 2 "+3V3"))
    (pad "5" smd roundrect (at 0.95 -1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (roundrect_rratio 0.25) (net 1 "GND"))))"#;

    const POWERED_FLASH_BOARD: &str = r#"(kicad_pcb (version 20240108) (generator pcbnew)
  (general (thickness 1.6))
  (paper "A4")
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (36 "B.SilkS" user "b.silkscreen") (37 "F.SilkS" user "f.silkscreen") (44 "Edge.Cuts" user))
  (net 0 "") (net 1 "GND") (net 2 "+3V3") (net 3 "CS") (net 4 "MISO") (net 5 "MOSI") (net 6 "SCK")
  (footprint "Pedalboard Library:SOIC-8_5.23x5.23mm_P1.27mm" (layer "F.Cu")
    (at 100 100)
    (property "Reference" "U1" (at 0 -2 0) (layer "F.SilkS"))
    (property "Value" "W25Q128JVS" (at 0 2 0) (layer "F.Fab"))
    (pad "1" smd rect (at -2 -1.9) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 3 "CS"))
    (pad "2" smd rect (at -2 -0.63) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 4 "MISO"))
    (pad "3" smd rect (at -2 0.63) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 2 "+3V3"))
    (pad "4" smd rect (at -2 1.9) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 1 "GND"))
    (pad "5" smd rect (at 2 1.9) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 5 "MOSI"))
    (pad "6" smd rect (at 2 0.63) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 6 "SCK"))
    (pad "7" smd rect (at 2 -0.63) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 2 "+3V3"))
    (pad "8" smd rect (at 2 -1.9) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 2 "+3V3"))))"#;

    const REGISTER_MAP_BOARD: &str = r#"(kicad_pcb (version 20240108) (generator pcbnew)
  (general (thickness 1.6))
  (paper "A4")
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (36 "B.SilkS" user "b.silkscreen") (37 "F.SilkS" user "f.silkscreen") (44 "Edge.Cuts" user))
  (net 0 "") (net 1 "GND") (net 2 "+3V3") (net 3 "SCL") (net 4 "SDA")
  (footprint "Package_LGA:LGA-4" (layer "F.Cu")
    (at 100 100)
    (property "Reference" "U1" (at 0 -2 0) (layer "F.SilkS"))
    (property "Value" "ACME-WHOAMI-13" (at 0 2 0) (layer "F.Fab"))
    (pad "1" smd rect (at -1 -1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 3 "SCL"))
    (pad "2" smd rect (at -1 1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 1 "GND"))
    (pad "3" smd rect (at 1 1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 4 "SDA"))
    (pad "4" smd rect (at 1 -1) (size 1 1) (layers "F.Cu" "F.Paste" "F.Mask") (net 2 "+3V3"))))"#;

    const REGISTER_MAP_MODEL: &str = r#"[[models]]
id = "acme_whoami_13"
kind = "digital"
description = "synthetic exact register-map attachment proof"

[models.match]
value_re = "^ACME-WHOAMI-13$"

[models.pins]
"1" = "scl"
"2" = "gnd"
"3" = "sda"
"4" = "vcc"

[models.peripheral]
kind = "register_map"
spec_toml = '''
[sensor]
name = "ACME WHOAMI"
bus = "i2c"
i2c_address = 0x18
[[sensor.register]]
addr = 0x00
const = [0x13]
[sensor.protocol]
style = "i2c_pointer"
'''

[models.coverage]
implements = ["i2c_chip_id"]
missing = ["measurement_registers"]
"#;

    /// A passive +5V → R1(100Ω) → MID → R2(300Ω) → GND divider, no MCU.
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

    /// A board with NO MCU module: two pulled nets (10 k to +5 V, 10 k to
    /// GND) plus a pulled-high net standing in for a responder-owned MISO.
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

    /// A 74HC74 whose clock (pad 3 = `clk1`) sits on STROBE and data (pad 2 =
    /// `d1`) on DATA, plus a FREE net wired only to a pull-down.
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

    /// A 74HC08 output (pad 3 = `y1`) on net SHARED that firmware may also
    /// drive as a GPIO output.
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

    /// A 74HC125 whose `y1` shares BUS with a firmware pin but whose `oe_n_1`
    /// is tied HIGH (released).
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

    // ── Shared fixtures ──────────────────────────────────────────────────────

    type EdgeResponder = Box<dyn FnMut(PinId, bool, u64) -> Vec<hauksbee_mcu::PinDrive> + Send>;
    type DirectionResponder =
        Box<dyn FnMut(&[(PinId, bool, bool)], u64) -> Vec<hauksbee_mcu::PinDrive> + Send>;

    /// One configurable trait-level core standing in for every backend shape
    /// the scheduler has to handle. Every field is a knob; the default is a
    /// faithful, silent, 16 MHz core that records nothing.
    #[derive(Default)]
    struct MockCore {
        digital_ins: Arc<Mutex<Vec<((char, u8), bool)>>>,
        micros: Arc<Mutex<Vec<u64>>>,
        fail_micros: bool,
        /// Advance a cycle counter at 16 cycles/us so drained edge logs
        /// normalise over a real span (`current_cycle` is 0 otherwise).
        count_cycles: bool,
        cycles: u64,
        /// A poll-only backend: `cycle_exact()` is false and `state().pc`
        /// counts `run_micros` calls.
        coarse: bool,
        calls: u32,
        /// Report pin drive direction (the trait default is blind).
        direction_observable: bool,
        was_reset: Arc<Mutex<bool>>,
        i2c_cb: Arc<Mutex<Option<Box<dyn FnMut(hauksbee_mcu::I2cEvent) -> Option<u8> + Send>>>>,
        spi_cb: Arc<Mutex<Option<Box<dyn FnMut(hauksbee_mcu::SpiEvent) -> u8 + Send>>>>,
        pin_cb: Arc<Mutex<Option<Box<dyn FnMut(PinId, bool, u64) + Send>>>>,
        i2c_addrs: Arc<Mutex<Vec<u8>>>,
        /// Models no bus controller, drops ADC channel 0 and carries a
        /// watchdog and timing limitation (the nRF52/ESP32 Renode/QEMU shape).
        bus_blind: bool,
        watchdog_resets: u64,
        /// Implements the synchronous single-slot input responder hooks.
        legacy: bool,
        responder_installs: Arc<std::sync::atomic::AtomicUsize>,
        responder: Arc<Mutex<Option<EdgeResponder>>>,
        direction_responder: Arc<Mutex<Option<DirectionResponder>>>,
    }

    const WATCHDOG_LIMITATION: &str =
        "The nRF52840 watchdog arms in this co-simulator (it reads back as running, \
         with a correct 32768 Hz reload) but never fires: an unserviced watchdog will \
         NOT reset the core, so watchdog recovery is untested on this run.";
    const TIMING_LIMITATION: &str =
        "ESP32 virtual time is paced by the host wall clock in this co-simulator, \
         so simulated time is approximate and host-load dependent.";

    impl MockCore {
        fn handles(&self) -> MockCore {
            MockCore {
                digital_ins: self.digital_ins.clone(),
                micros: self.micros.clone(),
                was_reset: self.was_reset.clone(),
                i2c_cb: self.i2c_cb.clone(),
                spi_cb: self.spi_cb.clone(),
                pin_cb: self.pin_cb.clone(),
                i2c_addrs: self.i2c_addrs.clone(),
                responder_installs: self.responder_installs.clone(),
                responder: self.responder.clone(),
                direction_responder: self.direction_responder.clone(),
                ..Default::default()
            }
        }
    }

    impl Mcu for MockCore {
        fn load_firmware(&mut self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_cycles(&mut self, n: u64) -> anyhow::Result<u64> {
            self.cycles += n;
            Ok(n)
        }
        fn run_micros(&mut self, us: u64) -> anyhow::Result<()> {
            self.micros
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(us);
            self.cycles += 16 * us;
            self.calls += 1;
            if self.fail_micros {
                anyhow::bail!("mock core refuses to advance");
            }
            Ok(())
        }
        fn frequency(&self) -> u64 {
            16_000_000
        }
        fn current_cycle(&self) -> u64 {
            if self.count_cycles || self.coarse {
                self.cycles
            } else {
                0
            }
        }
        fn cycle_exact(&self) -> bool {
            !self.coarse
        }
        fn set_digital_in(&mut self, pin: PinId, high: bool) {
            self.digital_ins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(((pin.port, pin.bit), high));
        }
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {
            *self.pin_cb.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);
        }
        fn on_input_responder(&mut self, responder: EdgeResponder) {
            if self.legacy {
                self.responder_installs
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.responder.lock().unwrap_or_else(|e| e.into_inner()) = Some(responder);
            }
        }
        fn input_responder_synchronous(&self) -> bool {
            self.legacy
        }
        fn input_responder_tracks_direction(&self) -> bool {
            self.legacy
        }
        fn on_input_responder_direction(&mut self, responder: DirectionResponder) {
            if self.legacy {
                *self
                    .direction_responder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(responder);
            }
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
        fn reset(&mut self) -> anyhow::Result<()> {
            *self.was_reset.lock().unwrap() = true;
            Ok(())
        }
        fn drive_direction_observable(&self) -> bool {
            self.direction_observable
        }
        fn i2c_bus_modeled(&self) -> bool {
            !self.bus_blind
        }
        fn spi_bus_modeled(&self, _controller: Option<&str>) -> bool {
            !self.bus_blind
        }
        fn adc_dropped_channels(&self) -> Vec<u8> {
            if self.bus_blind {
                vec![0]
            } else {
                Vec::new()
            }
        }
        fn watchdog_limitation(&self) -> Option<String> {
            self.bus_blind.then(|| WATCHDOG_LIMITATION.to_string())
        }
        fn timing_limitation(&self) -> Option<String> {
            self.bus_blind.then(|| TIMING_LIMITATION.to_string())
        }
        fn watchdog_resets(&self) -> u64 {
            self.watchdog_resets
        }
        fn state(&self) -> hauksbee_mcu::McuState {
            hauksbee_mcu::McuState {
                pc: self.calls,
                cycles: self.cycles,
                sleeping: false,
                done: false,
                crashed: false,
            }
        }
    }

    fn binding(
        reference: &str,
        backend: &str,
        gpio_drivers: HashMap<(char, u8), hauksbee_bind::drivers::PinDriver>,
    ) -> McuBinding {
        McuBinding {
            reference: reference.into(),
            backend: backend.into(),
            requested_part: String::new(),
            external_clock_present: false,
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
        }
    }

    fn board_scheduler(text: &str) -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(text).expect("board");
        let bound =
            hauksbee_bind::binder::bind_board(&board, &hauksbee_models::ModelLibrary::builtin());
        Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler")
    }

    fn bound_board(
        name: &str,
        circuit: Circuit,
        net_nodes: HashMap<String, NodeId>,
        digital: Vec<hauksbee_bind::digital::DigitalComponent>,
        device_meta: Vec<hauksbee_bind::stress::DeviceMeta>,
    ) -> hauksbee_bind::binder::BoundBoard {
        hauksbee_bind::binder::BoundBoard {
            name: name.into(),
            circuit,
            net_names: net_nodes.keys().cloned().collect(),
            net_nodes,
            digital,
            mcus: Vec::new(),
            dnp_mcus: Vec::new(),
            component_kinds: HashMap::new(),
            input_sources: HashMap::new(),
            supplies: Vec::new(),
            behavioral: Vec::new(),
            device_meta,
            dacs: Vec::new(),
            peripherals: Vec::new(),
            report: hauksbee_bind::bind_report::BindReport::default(),
        }
    }

    /// Stamp a tri-stated (input) driver per (pin, node), the shape the binder
    /// stamps for a wired digital-capable pin the firmware never drives.
    fn tristated_drivers(
        sched: &mut Scheduler,
        pins: &[((char, u8), NodeId)],
    ) -> HashMap<(char, u8), hauksbee_bind::drivers::PinDriver> {
        let mut drivers = HashMap::new();
        for &(pin, node) in pins {
            let name = sched.circuit.node_name(node).to_string();
            let mut drv = hauksbee_bind::drivers::PinDriver::stamp(
                &mut sched.circuit,
                node,
                &name,
                &format!("t_{}{}", pin.0, pin.1),
                hauksbee_bind::drivers::DEFAULT_RO,
            );
            drv.set_enabled(&mut sched.circuit, false);
            drivers.insert(pin, drv);
        }
        drivers
    }

    fn push_core(sched: &mut Scheduler, core: MockCore, binding: McuBinding) {
        sched.mcus.push(core_with_hooks(Box::new(core), binding));
        sched.responder_registries.push(None);
    }

    /// A PLAIN_INPUT_BOARD scheduler with one mock MCU whose GPIO drivers sit
    /// on fresh per-pin nets. Returns the core's shared handles.
    fn sched_with_core(core: MockCore, pins: &[(char, u8)]) -> (Scheduler, MockCore) {
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let nodes: Vec<_> = pins
            .iter()
            .map(|&pin| (pin, sched.circuit.node(&format!("CS_{}{}", pin.0, pin.1))))
            .collect();
        let mut drivers = tristated_drivers(&mut sched, &nodes);
        for drv in drivers.values_mut() {
            drv.set_enabled(&mut sched.circuit, true);
        }
        let handles = core.handles();
        push_core(&mut sched, core, binding("U1", "simavr:test", drivers));
        (sched, handles)
    }

    /// A PLAIN_INPUT_BOARD scheduler whose single live MCU is a bus-blind core
    /// with ADC channel 0 bound to `adc_net`.
    fn sched_with_bus_blind_core(adc_net: &str) -> Scheduler {
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let node = sched.circuit.node(adc_net);
        sched.net_nodes.insert(adc_net.to_string(), node);
        let mut b = binding("U1", "renode:nrf52840", HashMap::new());
        b.adc_nets.insert(0u8, node);
        push_core(
            &mut sched,
            MockCore {
                bus_blind: true,
                ..Default::default()
            },
            b,
        );
        sched
    }

    fn cycle_core() -> MockCore {
        MockCore {
            count_cycles: true,
            ..Default::default()
        }
    }

    /// Build a board scheduler with one hand-wired cycle-counting mock MCU
    /// ("A1") owning a tri-stated GPIO driver per (pin, net) pair.
    fn pulse_scheduler(board: &str, pins: &[((char, u8), &str)]) -> Scheduler {
        let mut sched = board_scheduler(board);
        let nodes: Vec<_> = pins
            .iter()
            .map(|&(pin, net)| (pin, sched.net_nodes[net]))
            .collect();
        let drivers = tristated_drivers(&mut sched, &nodes);
        push_core(
            &mut sched,
            cycle_core(),
            binding("A1", "simavr:test", drivers),
        );
        sched.relayout();
        sched
    }

    /// Preload the mock MCU's shared capture state with an already-happened
    /// GPIO transition sequence; the next `step` drains it like a firmware chunk.
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

    fn i2c_read_after_pointer(bus: &mut I2cBus, addr: u8, pointer: u8) -> Option<u8> {
        use hauksbee_mcu::I2cEvent as E;
        bus.dispatch(E::Start { addr, read: false });
        bus.dispatch(E::Write {
            addr,
            data: pointer,
        });
        bus.dispatch(E::Stop { addr });
        bus.dispatch(E::Start { addr, read: true });
        bus.dispatch(E::Read { addr })
    }

    // ── Model-card peripherals ───────────────────────────────────────────────

    #[test]
    fn model_card_register_map_attaches_without_scenario_duplication() {
        let dir = tempfile::tempdir().expect("temporary model dir");
        std::fs::write(dir.path().join("sensor.toml"), REGISTER_MAP_MODEL).unwrap();
        let library = hauksbee_models::ModelLibrary::empty()
            .with_user_dir(dir.path())
            .expect("load validated register-map model");
        let board = hauksbee_extract::ExtractedBoard::from_auto(REGISTER_MAP_BOARD).unwrap();
        let bound = hauksbee_bind::binder::bind_board(&board, &library);
        assert_eq!(bound.peripherals.len(), 1, "exact model attaches itself");
        assert!(matches!(
            bound.report.rows[0].outcome,
            hauksbee_bind::bind_report::BindOutcome::Behavioral { .. }
        ));

        let sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let mut bus = sched.i2c_buses()[0]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(i2c_read_after_pointer(&mut bus, 0x18, 0x00), Some(0x13));
    }

    #[test]
    fn model_peripheral_power_is_rail_gated_and_transaction_coupled() {
        let mut sched = board_scheduler(POWERED_EEPROM_BOARD);
        assert_eq!(
            sched.i2c_buses().len(),
            1,
            "exact model attaches one bus slave"
        );

        // The first chunk establishes the 3.3 V operating point; the second
        // observes it and applies standby.
        sched.step(2.0 * DEFAULT_CHUNK_S);
        let idle = sched.peripheral_states()["U1"].clone();
        assert_eq!(idle["powered"], 1.0);
        assert!((idle["supply_v"] - 3.3).abs() < 1e-6, "{idle:?}");
        assert!((idle["supply_current_a"] - 6e-6).abs() < 1e-12, "{idle:?}");

        {
            let mut bus = sched.i2c_buses()[0]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            bus.dispatch(hauksbee_mcu::I2cEvent::Start {
                addr: 0x50,
                read: true,
            });
            assert_eq!(
                bus.dispatch(hauksbee_mcu::I2cEvent::Read { addr: 0x50 }),
                Some(0xff)
            );
        }
        sched.step(DEFAULT_CHUNK_S);
        let read = sched.peripheral_states()["U1"].clone();
        assert!((read["supply_current_a"] - 1e-3).abs() < 1e-12, "{read:?}");

        // A brown rail disables both electrical draw and protocol response.
        let vcc = sched.net_nodes["+3V3"].0 as usize;
        sched.node_volts[vcc] = 1.0;
        sched.gate_model_peripherals_from_rails();
        let off = sched.peripheral_states()["U1"].clone();
        assert_eq!(off["powered"], 0.0);
        assert_eq!(off["supply_current_a"], 0.0);
        let mut bus = sched.i2c_buses()[0]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            bus.dispatch(hauksbee_mcu::I2cEvent::Start {
                addr: 0x50,
                read: true
            }),
            None
        );
        assert_eq!(
            bus.dispatch(hauksbee_mcu::I2cEvent::Read { addr: 0x50 }),
            None
        );
    }

    #[test]
    fn spi_deep_power_down_selects_the_datasheet_current_state() {
        let mut sched = board_scheduler(POWERED_FLASH_BOARD);
        assert_eq!(sched.spi_buses().len(), 1, "exact flash model attaches");
        let current = |sched: &Scheduler| sched.peripheral_states()["U1"]["supply_current_a"];
        let command = |sched: &Scheduler, byte: u8| {
            let mut bus = sched.spi_buses()[0]
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            bus.cs_assert();
            bus.transfer(byte);
            bus.cs_deassert();
        };
        sched.step(2.0 * DEFAULT_CHUNK_S);
        assert!((current(&sched) - 60e-6).abs() < 1e-12);
        command(&sched, 0xb9);
        sched.step(DEFAULT_CHUNK_S);
        assert!((current(&sched) - 20e-6).abs() < 1e-12);
        command(&sched, 0xab);
        sched.step(DEFAULT_CHUNK_S);
        assert!((current(&sched) - 60e-6).abs() < 1e-12);
    }

    // ── Bindings, substitution and 595 control proofs ────────────────────────

    #[test]
    fn referenced_595_controls_are_proven_only_when_unpulled_and_static() {
        let controls = [
            (None, vec![('B', 0), ('B', 1), ('B', 2)]),
            (Some(('C', 3)), vec![('C', 0), ('C', 1), ('C', 2), ('C', 3)]),
        ];
        let set = |v: &[usize]| v.iter().copied().collect::<std::collections::HashSet<_>>();
        let none = std::collections::HashSet::new();
        assert!(referenced_595_controls_are_proven(
            &set(&[0]),
            &controls,
            &none
        ));
        assert!(!referenced_595_controls_are_proven(
            &set(&[1]),
            &controls,
            &none
        ));

        let controls = [
            (None, vec![('B', 0), ('B', 1), ('B', 2)]),
            (None, vec![('C', 0), ('C', 1), ('C', 2)]),
        ];
        let pulled = |pin| std::collections::HashSet::from([pin]);
        assert!(!referenced_595_controls_are_proven(
            &set(&[0]),
            &controls,
            &pulled(('B', 1))
        ));
        assert!(referenced_595_controls_are_proven(
            &set(&[0]),
            &controls,
            &pulled(('C', 1))
        ));
    }

    #[cfg(feature = "avr")]
    #[test]
    #[ignore = "requires the private NEP board path"]
    fn private_nep_topology_installs_edge_exact_parallel_memory() {
        use sha2::{Digest, Sha256};

        const BOARD_SHA256: &str =
            "b7d2a7c3d7ea193bb394bcb3987cfb141230d3db0e294744a8a0b263f9b93673";
        let path = std::env::var("HAUKSBEE_NEP_BOARD").expect("HAUKSBEE_NEP_BOARD");
        let board_bytes = std::fs::read(path).expect("read private board");
        let digest = Sha256::digest(&board_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(digest, BOARD_SHA256);
        let board_text = String::from_utf8(board_bytes).expect("private board is UTF-8");
        let scheduler = board_scheduler(&board_text);

        assert_eq!(scheduler.parallel_memory_chips.len(), 1);
        let memory = *scheduler.parallel_memory_chips.iter().next().unwrap();
        assert_eq!(scheduler.digital[memory].reference, "U1");
        let chain_refs = scheduler
            .chains
            .iter()
            .flat_map(|chain| {
                chain
                    .order
                    .iter()
                    .map(|&index| scheduler.digital[index].reference.as_str())
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(chain_refs, std::collections::BTreeSet::from(["U2", "U3"]));

        let rail = scheduler
            .supplies
            .iter()
            .find(|supply| supply.net_name == "+5V")
            .expect("private +5V supply")
            .net;
        for role in ["we_n", "oe_n"] {
            let node = scheduler.digital[memory].roles[role];
            let pulls = scheduler
                .circuit
                .devices
                .iter()
                .filter(|device| {
                    matches!(device, Device::Resistor { a, b, ohms, .. }
                        if ((*a == node && *b == rail) || (*b == node && *a == rail))
                            && (*ohms - 10_000.0).abs() < f64::EPSILON)
                })
                .count();
            assert_eq!(pulls, 1, "{role} must retain its exact 10k pull-up");
        }
    }

    #[test]
    fn public_mcu_binding_literal_remains_source_compatible() {
        let mut b = binding("U1", "renode:stm32f4", HashMap::new());
        b.requested_part = "STM32F411RET6".into();
        let substitution = detect_substitution(&b).expect("F411 uses the F407 stand-in");
        let public_event = McuSubstitution {
            reference: substitution.reference,
            backend: substitution.backend,
            requested_part: substitution.requested_part,
            modelled_core: substitution.modelled_core,
        };
        assert_eq!(public_event.reference, "U1");
    }

    #[test]
    fn scheduler_occurrence_identity_ignores_synthetic_rail_rows() {
        let mut report = hauksbee_bind::bind_report::BindReport::default();
        let row =
            |value: &str, model_id: Option<&str>, outcome| hauksbee_bind::bind_report::BindRow {
                reference: "RAIL:+5V".into(),
                value: value.into(),
                model_id: model_id.map(str::to_string),
                confidence: hauksbee_models::Confidence::Exact,
                source: None,
                outcome,
                warning: None,
                guesses: Vec::new(),
            };
        report.push(row(
            "STM32F411",
            Some("stm32f4"),
            hauksbee_bind::bind_report::BindOutcome::Mcu {
                backend: "renode:stm32f4".into(),
            },
        ));
        report.push(row(
            "5 V ideal rail",
            None,
            hauksbee_bind::bind_report::BindOutcome::PowerRail { volts: 5.0 },
        ));
        assert_eq!(mcu_occurrence_subjects(&report), ["RAIL:+5V"]);
    }

    /// A part simavr has is built as itself; one it lacks fails loudly with
    /// the available cores named, never silently substituted by an ATmega328P.
    #[cfg(feature = "avr")]
    #[test]
    fn the_avr_backend_builds_the_part_the_board_asked_for_or_refuses() {
        for part in ["atmega328p", "atmega1284p", "atmega1284", "atmega32u4"] {
            assert!(
                instantiate_avr(&format!("simavr:{part}")).is_ok(),
                "simavr knows {part}"
            );
        }
        let err = instantiate_avr("simavr:atmega4809")
            .err()
            .expect("an unmodelled AVR core must refuse, not substitute")
            .to_string();
        assert!(err.contains("atmega4809"), "{err}");
        assert!(err.contains("1284p") && err.contains("32u4"), "{err}");
    }

    #[test]
    fn adc_promotion_keys_on_the_channels_own_pin_not_the_net() {
        use hauksbee_bind::drivers::PinDriver;
        use hauksbee_ir::DeviceId;
        let drv = |net: u32, enabled: bool| PinDriver {
            vsource: DeviceId(0),
            net: NodeId(net),
            enabled,
            roff: 1e9,
            resistor: DeviceId(1),
            ron: 100.0,
        };
        let mut b = binding(
            "U1",
            "",
            HashMap::from([(('C', 0), drv(5, false)), (('C', 1), drv(5, true))]),
        );
        b.adc_nets = HashMap::from([(0u8, NodeId(5)), (6u8, NodeId(5))]);
        b.adc_pin = HashMap::from([(0u8, ('C', 0))]);
        b.module = true;
        // ch0's own driver is disabled: a neighbour on the same net must not
        // promote it; an ADC-only channel (no own pin) is never promoted.
        assert!(!adc_channel_promoted(&b, 0));
        assert!(!adc_channel_promoted(&b, 6));
        b.gpio_drivers.get_mut(&('C', 0)).unwrap().enabled = true;
        assert!(adc_channel_promoted(&b, 0));
    }

    #[test]
    fn frame_peak_accumulators_track_current_and_reset_per_step() {
        let mut sched = board_scheduler(DIVIDER_BOARD);
        let peak = |sched: &Scheduler, r: &str| sched.frame_peak_current()[r];

        sched.step(1e-3);
        // 12.5 mA through both legs of the 100/300 divider off the 5 V rail.
        assert!((peak(&sched, "R1") - 0.0125).abs() < 5e-4);
        assert!((peak(&sched, "R2") - 0.0125).abs() < 5e-4);
        let &(mn, mx) = sched.frame_v_extremes().get("MID").expect("MID tracked");
        let mid = sched.net_voltages()["MID"];
        assert!((mn - 3.75).abs() < 0.05 && (mx - 3.75).abs() < 0.05);
        assert!(mn <= mid + 1e-9 && mid <= mx + 1e-9);

        // A second step must not inherit the first frame's peak.
        sched.step(1e-3);
        assert!((peak(&sched, "R1") - 0.0125).abs() < 5e-4);
    }

    // ── AVR-bound electrical proofs (need the in-process simavr core) ────────

    /// A Nano module driving net CLK from A2 (PC2) into an RC integrator
    /// (10k into 100 nF, tau = 1 ms).
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

    /// A Nano driving SER/SRCLK/RCLK of a real bound 74HC595 whose Q0/Q7 are
    /// pulled down.
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

    /// A Nano driving net BUS from A2 (PC2) into a plain 10k pull-down.
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

    /// Bind `board` and promote the given Nano pins to driven-low outputs,
    /// exactly as their first firmware edges would.
    #[cfg(feature = "avr")]
    fn nano_scheduler(board: &str, promote: &[(char, u8)]) -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(board).expect("board");
        let mut bound =
            hauksbee_bind::binder::bind_board(&board, &hauksbee_models::ModelLibrary::builtin());
        for pin in promote {
            let drv = bound.mcus[0]
                .gpio_drivers
                .get_mut(pin)
                .unwrap_or_else(|| panic!("driver for P{}{}", pin.0, pin.1));
            drv.set_enabled(&mut bound.circuit, true);
            drv.set_volts(&mut bound.circuit, 0.0);
        }
        Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler")
    }

    /// A firmware-shaped shiftOut(MSBFIRST, 0xA6) bit-bang latches a REAL
    /// bound 74HC595 through its electrical nets: the latched byte is read
    /// back from the solved node voltages of the output nets.
    #[cfg(feature = "avr")]
    #[test]
    fn cosim_bitbang_595_latches_through_bound_nets() {
        // SER on D11/PB3, SRCLK on D13/PB5, RCLK on D12/PB4.
        let mut sched = nano_scheduler(CHAIN_BOARD, &[('B', 3), ('B', 5), ('B', 4)]);

        let byte = 0xA6u8;
        let mut log = Vec::new();
        let mut cyc = 100u64;
        let mut ser_level = false;
        let mut edge = |cycle: u64, bit: u8, level: bool| {
            log.push(hauksbee_bind::digital::PinEdge {
                cycle,
                port: 'B',
                bit,
                level,
            });
        };
        for i in (0..8).rev() {
            let bit_lv = (byte >> i) & 1 != 0;
            if bit_lv != ser_level {
                edge(cyc, 3, bit_lv);
                ser_level = bit_lv;
            }
            cyc += 4;
            edge(cyc, 5, true);
            cyc += 4;
            edge(cyc, 5, false);
            cyc += 4;
        }
        edge(cyc + 4, 4, true);
        edge(cyc + 8, 4, false);

        // Mirror run_chunk's order: replay, chain-apply, solve.
        let ticks = sched.replay_digital_edges(0, &log);
        assert!(ticks > 0, "the bound 595 must be clocked by the replay");
        let mut chains = std::mem::take(&mut sched.chains);
        for chain in &mut chains {
            chain.apply(&mut sched.digital, &mut sched.circuit);
        }
        sched.chains = chains;
        assert!(sched.solve_chunk(100e-6), "chunk solve converges");

        // MSB-first shiftOut leaves the first-sent bit (MSB = 1) in Q7 and
        // the last-sent (LSB = 0) in Q0.
        let q7 = sched.net_voltage("Q7").expect("Q7 solved");
        let q0 = sched.net_voltage("Q0").expect("Q0 solved");
        assert!(q7 > 3.0, "Q7 must be driven high, got {q7:.2} V");
        assert!(q0 < 1.0, "Q0 must rest low, got {q0:.2} V");
    }

    /// Ten 5 us pulses inside one 100 us chunk end LOW: a final-level DC drive
    /// leaves the RC integrator empty, the PWL drive pumps it.
    #[cfg(feature = "avr")]
    #[test]
    fn pwl_drive_integrates_a_pulse_train_the_dc_path_collapses() {
        let chunk = 100e-6;
        let mut transitions = Vec::new();
        for k in 0..10u64 {
            transitions.push((k * 160, true));
            transitions.push((k * 160 + 80, false));
        }
        let mut edges = HashMap::new();
        edges.insert(('C', 2u8), transitions);

        let mut sched = nano_scheduler(RC_BOARD, &[('C', 2)]);
        assert!(sched.solve_chunk(chunk), "control solve converges");
        let mid_dc = sched.net_voltage("MID").expect("MID");
        assert!(
            mid_dc.abs() < 0.05,
            "DC control must leave the RC at ~0 V, got {mid_dc:.3}"
        );

        let mut sched = nano_scheduler(RC_BOARD, &[('C', 2)]);
        sched.last_chunk_edges.push(ChunkPinEdges {
            mcu_reference: "A1".into(),
            edges,
            cycle_span: (0, 1600),
            chunk_s: chunk,
            cycle_exact: true,
        });
        let restores = sched.apply_pwl_drives(chunk);
        assert_eq!(restores.len(), 1, "exactly the CLK pin gets a PWL drive");
        assert!(sched.solve_chunk(chunk), "pwl solve converges");
        sched.restore_pwl_drives(&restores);

        let mid_pwl = sched.net_voltage("MID").expect("MID");
        assert!(
            mid_pwl > 0.15,
            "the PWL drive must pump the RC, got {mid_pwl:.4} V"
        );
        let drv = &sched.mcus[0].binding.gpio_drivers[&('C', 2)];
        match &sched.circuit.devices[drv.vsource.0 as usize] {
            Device::Vsource { kind, .. } => {
                assert!(matches!(kind, SourceKind::Dc(v) if v.abs() < 1e-9));
            }
            _ => panic!("driver vsource missing"),
        }
    }

    /// A pin reported configured-output one chunk and gone the next (DDR
    /// output→input) must have its driver disabled so the net falls to its
    /// pull; fresh edge evidence with an INPUT chunk-end report must release
    /// it too.
    #[cfg(feature = "avr")]
    #[test]
    fn sync_configured_outputs_releases_dropped_pins() {
        let mut sched = nano_scheduler(RELEASE_BOARD, &[]);
        let enabled = |sched: &Scheduler| sched.mcus[0].binding.gpio_drivers[&('C', 2)].enabled;
        let no_edges = std::collections::HashSet::new();

        sched.mcus[0].last_levels.insert(('C', 2), true);
        sched.sync_configured_outputs(0, std::collections::HashSet::from([('C', 2u8)]), &no_edges);
        assert!(enabled(&sched));
        assert!(sched.solve_chunk(1e-3));
        let driven = sched.net_voltage("BUS").expect("BUS solved");
        assert!(
            driven > 3.0,
            "driven-high BUS must read ~5 V, got {driven:.2} V"
        );

        sched.sync_configured_outputs(0, std::collections::HashSet::new(), &no_edges);
        assert!(!enabled(&sched));
        assert!(sched.solve_chunk(1e-3));
        let released = sched.net_voltage("BUS").expect("BUS solved");
        assert!(
            released < 0.5,
            "released BUS must fall to its pull-down, got {released:.2} V"
        );

        sched.mcus[0]
            .binding
            .gpio_drivers
            .get_mut(&('C', 2))
            .unwrap()
            .set_enabled(&mut sched.circuit, true);
        let edge_pins = std::collections::HashSet::from([('C', 2u8)]);
        sched.sync_configured_outputs(0, std::collections::HashSet::new(), &edge_pins);
        assert!(
            !enabled(&sched),
            "a pin that edged but reads INPUT at chunk end is released"
        );
    }

    // ── Parallel-memory fast path claiming ───────────────────────────────────

    struct LegacyMemory {
        sched: Scheduler,
        installs: Arc<std::sync::atomic::AtomicUsize>,
        edge: Arc<Mutex<Option<EdgeResponder>>>,
        direction: Arc<Mutex<Option<DirectionResponder>>>,
    }

    /// A hand-built board holding one compiled memory `spec` on U1 and a
    /// legacy synchronous-responder MCU "M1" whose ('B', i) drivers sit on the
    /// `gpio_roles` nets.
    fn scheduler_with_legacy_memory(
        spec: &str,
        role_names: &[&str],
        gpio_roles: &[&str],
    ) -> LegacyMemory {
        let mut circuit = Circuit::new();
        let mut roles = HashMap::new();
        let mut gpio_drivers = HashMap::new();
        for &role in role_names {
            let node = circuit.node(role);
            roles.insert(role.to_string(), node);
            if let Some(index) = gpio_roles.iter().position(|candidate| *candidate == role) {
                gpio_drivers.insert(
                    ('B', index as u8),
                    hauksbee_bind::drivers::PinDriver::stamp(
                        &mut circuit,
                        node,
                        role,
                        "legacy",
                        50.0,
                    ),
                );
            }
        }
        if let Some(&vcc) = roles.get("vcc") {
            circuit.add(Device::Vsource {
                name: "VSTATIC".into(),
                p: vcc,
                n: NodeId::GROUND,
                kind: SourceKind::Dc(5.0),
            });
        }
        let parsed: hauksbee_models::logic_spec::Logic =
            toml::from_str(spec).expect("parse memory");
        let logic = hauksbee_bind::logic::LogicComponent::compile("legacy-memory", &parsed)
            .expect("compile memory");
        let digital = hauksbee_bind::digital::DigitalComponent {
            reference: "U1".into(),
            levels: hauksbee_bind::digital::LogicLevels {
                voh: 4.4,
                vol: 0.1,
                vih: 2.0,
                vil: 0.8,
                ro: 50.0,
            },
            roles,
            drivers: HashMap::new(),
            logic: Some(logic),
            supply: None,
        };
        let net_nodes = role_names
            .iter()
            .map(|name| ((*name).to_string(), circuit.node(name)))
            .collect();
        let bound = bound_board(
            "legacy-memory",
            circuit,
            net_nodes,
            vec![digital],
            Vec::new(),
        );
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let core = MockCore {
            legacy: true,
            ..Default::default()
        };
        let h = core.handles();
        push_core(&mut sched, core, binding("M1", "legacy:test", gpio_drivers));
        LegacyMemory {
            sched,
            installs: h.responder_installs,
            edge: h.responder,
            direction: h.direction_responder,
        }
    }

    /// A second MCU "M2" whose ('C', 0) driver is a clone of M1's ('B', 0)
    /// driver moved onto `node`.
    fn add_second_mcu_on(m: &mut LegacyMemory, node: NodeId) {
        let mut driver = m.sched.mcus[0].binding.gpio_drivers[&('B', 0)].clone();
        driver.net = node;
        push_core(
            &mut m.sched,
            MockCore::default(),
            binding("M2", "other:test", HashMap::from([(('C', 0), driver)])),
        );
    }

    const ONE_PIN_MEMORY: &str = r#"
inputs = ["gnd", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "we_n", edge = "rising" }
read_gates = [{ pin = "we_n", active = "high" }]
data_in = ["gnd"]
data_out = ["io0"]
"#;

    #[test]
    fn legacy_singleton_responder_keeps_unambiguous_parallel_memory_edge_exact() {
        let mut m =
            scheduler_with_legacy_memory(ONE_PIN_MEMORY, &["gnd", "we_n", "io0"], &["we_n", "io0"]);
        m.sched.build_and_install_parallel_memories();

        assert_eq!(m.installs.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(m.sched.parallel_memory_chips.contains(&0));

        let mut slot = m.edge.lock().unwrap_or_else(|e| e.into_inner());
        let callback = slot.as_mut().expect("legacy singleton callback installed");
        m.direction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .expect("direction callback installed")(&[(PinId::new('B', 0), true, false)], 9);
        let _ = callback(PinId::new('B', 0), false, 10);
        assert_eq!(
            callback(PinId::new('B', 0), true, 11),
            vec![hauksbee_mcu::PinDrive::drive(PinId::new('B', 1), false)],
            "a sub-chunk pulse must synchronously write and return the new memory bit"
        );
    }

    #[test]
    fn legacy_singleton_responder_does_not_claim_cross_pin_memory() {
        const SPEC: &str = r#"
inputs = ["a0", "ce_n", "oe_n", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["a0"]
write = { pin = "we_n", edge = "rising" }
write_gates = [
  { pin = "ce_n", active = "low" },
  { pin = "oe_n", active = "high" },
]
read_gates = [
  { pin = "ce_n", active = "low" },
  { pin = "oe_n", active = "low" },
  { pin = "we_n", active = "high" },
]
data_in = ["io0"]
data_out = ["io0"]
"#;
        let names = ["a0", "ce_n", "oe_n", "we_n", "io0"];
        let mut m = scheduler_with_legacy_memory(SPEC, &names, &names);
        m.sched.build_and_install_parallel_memories();
        assert_eq!(m.installs.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(m.sched.parallel_memory_chips.is_empty());
        assert!(m.edge.lock().unwrap_or_else(|e| e.into_inner()).is_none());
    }

    #[test]
    fn cycle_exact_backend_with_no_synchronous_responder_does_not_claim_memory() {
        let mut m =
            scheduler_with_legacy_memory(ONE_PIN_MEMORY, &["gnd", "we_n", "io0"], &["we_n", "io0"]);
        let binding = m.sched.mcus.pop().expect("legacy MCU").binding;
        m.sched.responder_registries.pop();
        push_core(&mut m.sched, MockCore::default(), binding);
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    #[test]
    fn memory_with_another_mcus_mutable_node_is_not_claimed() {
        const SPEC: &str = r#"
inputs = ["gnd", "data", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "we_n", edge = "rising" }
read_gates = [{ pin = "we_n", active = "high" }]
data_in = ["data"]
data_out = ["io0"]
"#;
        let mut m =
            scheduler_with_legacy_memory(SPEC, &["gnd", "data", "we_n", "io0"], &["we_n", "io0"]);
        let data_node = m.sched.net_nodes["data"];
        add_second_mcu_on(&mut m, data_node);
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    #[test]
    fn memory_node_shared_with_a_second_mcu_or_a_595_output_is_not_claimed() {
        let mut m =
            scheduler_with_legacy_memory(ONE_PIN_MEMORY, &["gnd", "we_n", "io0"], &["we_n", "io0"]);
        let we_node = m.sched.net_nodes["we_n"];
        add_second_mcu_on(&mut m, we_node);
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());

        let mut m =
            scheduler_with_legacy_memory(ONE_PIN_MEMORY, &["gnd", "we_n", "io0"], &["we_n", "io0"]);
        let we_node = m.sched.net_nodes["we_n"];
        let output = hauksbee_bind::drivers::PinDriver::stamp(
            &mut m.sched.circuit,
            we_node,
            "qa",
            "U595",
            50.0,
        );
        m.sched
            .digital
            .push(hauksbee_bind::digital::DigitalComponent {
                reference: "U595".into(),
                levels: m.sched.digital[0].levels,
                roles: HashMap::from([("qa".to_string(), we_node)]),
                drivers: HashMap::from([("qa".to_string(), output)]),
                logic: None,
                supply: None,
            });
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    #[test]
    fn another_output_on_the_memory_component_is_not_hidden_from_provenance() {
        let mut m = scheduler_with_legacy_memory(
            ONE_PIN_MEMORY,
            &["gnd", "we_n", "io0", "status"],
            &["we_n", "io0"],
        );
        let we_n = m.sched.net_nodes["we_n"];
        let status = hauksbee_bind::drivers::PinDriver::stamp(
            &mut m.sched.circuit,
            we_n,
            "we_n",
            "U1_status",
            50.0,
        );
        m.sched.digital[0].roles.insert("status".into(), we_n);
        m.sched.digital[0].drivers.insert("status".into(), status);
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    /// Claiming one memory port skips the component's whole tick, so a
    /// component with unrelated comb logic must stay on the ordinary path.
    #[test]
    fn memory_fast_path_does_not_suppress_unrelated_logic_on_the_same_component() {
        const SPEC: &str = r#"
inputs = ["gnd", "we_n"]
outputs = ["io0", "status"]
comb = { status = "we_n" }
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "we_n", edge = "rising" }
read_gates = [{ pin = "we_n", active = "high" }]
data_in = ["gnd"]
data_out = ["io0"]
"#;
        let mut m =
            scheduler_with_legacy_memory(SPEC, &["gnd", "we_n", "io0", "status"], &["we_n", "io0"]);
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    /// Specs with no usable firmware trigger: a write pin shorted to ground,
    /// an all-static responder, and a power-on-active read the responder
    /// cannot seed. None may be claimed.
    #[test]
    fn memories_without_a_reachable_trigger_are_not_claimed() {
        const GROUNDED_WRITE: &str = r#"
inputs = ["gnd"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "gnd", edge = "rising" }
read_gates = [{ pin = "gnd", active = "high" }]
data_in = ["gnd"]
data_out = ["io0"]
"#;
        const ALL_STATIC: &str = r#"
inputs = ["gnd"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "gnd", edge = "rising" }
read_gates = [{ pin = "gnd", active = "low" }]
data_in = ["gnd"]
data_out = ["io0"]
"#;
        const POWER_ON_READ: &str = r#"
inputs = ["gnd", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "we_n", edge = "rising" }
read_gates = [{ pin = "we_n", active = "low" }]
data_in = ["gnd"]
data_out = ["io0"]
"#;
        for (spec, roles, gpio) in [
            (GROUNDED_WRITE, &["gnd", "io0"][..], &["gnd", "io0"][..]),
            (ALL_STATIC, &["gnd", "io0"], &["io0"]),
            (POWER_ON_READ, &["gnd", "we_n", "io0"], &["we_n", "io0"]),
        ] {
            let mut m = scheduler_with_legacy_memory(spec, roles, gpio);
            m.sched.build_and_install_parallel_memories();
            assert!(m.sched.parallel_memory_chips.is_empty(), "{spec}");
        }
    }

    /// A real 5 V static node is still zero in the pre-solve snapshot and must
    /// not be responder-owned.
    #[test]
    fn static_high_node_is_not_claimed_from_the_unsolved_zero_snapshot() {
        const SPEC: &str = r#"
inputs = ["vcc", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["vcc"]
write = { pin = "we_n", edge = "rising" }
read_gates = [{ pin = "we_n", active = "high" }]
data_in = ["vcc"]
data_out = ["io0"]
"#;
        let mut m = scheduler_with_legacy_memory(SPEC, &["vcc", "we_n", "io0"], &["we_n", "io0"]);
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    fn pulled_memory(spec: &str, pulls: &[(bool, f64)]) -> Scheduler {
        let mut m =
            scheduler_with_legacy_memory(spec, &["gnd", "vcc", "we_n", "io0"], &["we_n", "io0"]);
        for (index, &(high, ohms)) in pulls.iter().enumerate() {
            let a = if high {
                m.sched.net_nodes["vcc"]
            } else {
                NodeId::GROUND
            };
            m.sched.circuit.add(Device::Resistor {
                name: format!("Rbias{index}"),
                a,
                b: m.sched.net_nodes["we_n"],
                ohms,
                tc1: None,
            });
        }
        m.sched.build_and_install_parallel_memories();
        m.sched
    }

    /// A single weak static pull is a trustworthy initial GPIO level and seeds
    /// the port shadow; a strong or conflicting bias is not; and a pulled-high
    /// power-on-active read still needs an initial bus drive the responder
    /// does not provide.
    #[test]
    fn only_one_unambiguous_weak_pull_seeds_a_responder_input() {
        const SEEDED: &str = r#"
inputs = ["gnd", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["gnd"]
write = { pin = "we_n", edge = "rising" }
read_gates = [
  { pin = "we_n", active = "high" },
  { pin = "gnd", active = "high" },
]
data_in = ["gnd"]
data_out = ["io0"]
"#;
        assert!(pulled_memory(SEEDED, &[(true, 10_000.0)])
            .parallel_memory_chips
            .contains(&0));
        assert!(pulled_memory(ONE_PIN_MEMORY, &[(true, 10_000.0)])
            .parallel_memory_chips
            .is_empty());
        assert!(pulled_memory(ONE_PIN_MEMORY, &[(true, 100.0)])
            .parallel_memory_chips
            .is_empty());
        assert!(
            pulled_memory(ONE_PIN_MEMORY, &[(true, 10_000.0), (false, 10_000.0)])
                .parallel_memory_chips
                .is_empty()
        );
    }

    // ── Plain digital-input sync and CS framing ──────────────────────────────

    /// Through the real `step`/`run_chunk` path: a tri-stated input on a net
    /// pulled HIGH gets exactly one `set_digital_in(true)`, one pulled LOW
    /// exactly one `false`, a responder-owned pin is never pushed, and a
    /// floating net (no device but the pin's own tri-state leg) is left alone.
    #[test]
    fn plain_digital_inputs_reach_the_core() {
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let float_node = sched.circuit.node("FLOATY");
        let pins = [
            (('C', 0u8), sched.net_nodes["BTN_HI"]),
            (('C', 1), sched.net_nodes["BTN_LO"]),
            (('C', 2), sched.net_nodes["RESP"]),
            (('C', 3), float_node),
        ];
        let drivers = tristated_drivers(&mut sched, &pins);
        let core = MockCore::default();
        let digital_ins = core.digital_ins.clone();
        push_core(&mut sched, core, binding("U1", "simavr:test", drivers));
        sched.mcus[0].responder_input_pins.insert(('C', 2));
        sched.relayout();

        sched.step(5.0 * DEFAULT_CHUNK_S);

        let calls = digital_ins.lock().unwrap_or_else(|e| e.into_inner());
        let for_pin = |pin: (char, u8)| -> Vec<bool> {
            calls
                .iter()
                .filter(|(p, _)| *p == pin)
                .map(|&(_, l)| l)
                .collect()
        };
        assert_eq!(for_pin(('C', 0)), vec![true], "{calls:?}");
        assert_eq!(for_pin(('C', 1)), vec![false], "{calls:?}");
        assert!(for_pin(('C', 2)).is_empty(), "{calls:?}");
        assert!(for_pin(('C', 3)).is_empty(), "{calls:?}");
    }

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

    fn cs_frames(sched: &Scheduler, m: usize) -> usize {
        sched.mcus[m]
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cs_frames
            .len()
    }

    /// A CS pin two MCUs both own frames on exactly one of them: the MCU that
    /// drives the CS net when that is known, else the first owner.
    #[test]
    fn cs_frame_installs_on_exactly_one_mcu_preferring_the_cs_net_driver() {
        let cs_pin = ('C', 2u8);
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let resp_node = sched.net_nodes["RESP"];
        for _ in 0..2 {
            let drivers = tristated_drivers(&mut sched, &[(cs_pin, resp_node)]);
            push_core(
                &mut sched,
                MockCore::default(),
                binding("U1", "simavr:test", drivers),
            );
        }
        let bus = Arc::new(Mutex::new(SpiBus::new("U9", Box::new(Opaque))));
        sched.register_cs_frame(&bus, Some(cs_pin), None);
        assert_eq!(cs_frames(&sched, 0) + cs_frames(&sched, 1), 1);

        // Same pin on DIFFERENT nets: MCU_0 on an unrelated net, MCU_1 on the
        // real CS net. The frame must land on MCU_1.
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let unrelated_net = sched.net_nodes["RESP"];
        let cs_net = sched.circuit.node("SPI_CS");
        for (idx, net) in [unrelated_net, cs_net].into_iter().enumerate() {
            let drivers = tristated_drivers(&mut sched, &[(cs_pin, net)]);
            push_core(
                &mut sched,
                MockCore::default(),
                binding(&format!("U{idx}"), "simavr:test", drivers),
            );
        }
        let bus = Arc::new(Mutex::new(SpiBus::new("U9", Box::new(Opaque))));
        sched.register_cs_frame(&bus, Some(cs_pin), Some(cs_net));
        assert_eq!(cs_frames(&sched, 0), 0);
        assert_eq!(cs_frames(&sched, 1), 1);
    }

    /// `pin_driving_node` must return the lowest (port, bit) on a shared net
    /// regardless of HashMap iteration order.
    #[test]
    fn pin_driving_node_is_deterministic_when_multiple_pins_share_a_net() {
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let node = sched.circuit.node("SHARED_CS");
        let pins: Vec<_> = [('D', 7), ('B', 5), ('C', 2), ('B', 4), ('D', 0), ('C', 9)]
            .into_iter()
            .map(|pin| (pin, node))
            .collect();
        let drivers = tristated_drivers(&mut sched, &pins);
        push_core(
            &mut sched,
            MockCore::default(),
            binding("U1", "simavr:test", drivers),
        );
        assert_eq!(sched.pin_driving_node(node), Some(('B', 4)));
    }

    // ── Toggle statistics ────────────────────────────────────────────────────

    fn count_toggles(sched: &mut Scheduler, net: &str, swing: &[f64]) -> u64 {
        let node = sched.circuit.node(net);
        sched.net_nodes.insert(net.to_string(), node);
        let idx = node.0 as usize;
        if sched.node_volts.len() <= idx {
            sched.node_volts.resize(idx + 1, 0.0);
        }
        for &v in swing {
            sched.node_volts[idx] = v;
            sched.update_stats();
        }
        sched.stats.get(net).map(|s| s.toggles).unwrap_or(0)
    }

    /// The logic-level band scales with the LOWEST logic rail on the board: a
    /// loaded 3.3 V output swinging to 2.7 V must register toggles, on a pure
    /// 3.3 V board and on a mixed 5 V / 3.3 V one.
    #[test]
    fn toggle_counting_uses_the_min_logic_rail() {
        let (mut sched, _h) = sched_with_core(MockCore::default(), &[]);
        sched.mcus[0].logic_high_v = 3.3;
        assert!(count_toggles(&mut sched, "BLINK", &[2.7, 0.0, 2.7, 0.0]) >= 3);

        let (mut sched, _h) = sched_with_core(MockCore::default(), &[]);
        sched.mcus[0].logic_high_v = 5.0;
        push_core(
            &mut sched,
            MockCore::default(),
            binding("U2", "renode:stm32f4", HashMap::new()),
        );
        assert!((sched.mcus[1].logic_high_v - 3.3).abs() < 1e-6);
        assert!(count_toggles(&mut sched, "BLINK33", &[2.7, 0.0, 2.7, 0.0]) >= 3);
    }

    // ── Backend limitation and coverage honesty ──────────────────────────────

    #[test]
    fn backend_limitations_are_reported_per_mcu_verbatim() {
        let sched = sched_with_bus_blind_core("TEMP_SENSE");
        let limits = sched.watchdog_limitations();
        assert_eq!(
            limits,
            vec![("U1".to_string(), WATCHDOG_LIMITATION.to_string())]
        );
        assert_eq!(
            watchdog_limitation_message(&limits[0].0, &limits[0].1),
            format!("MCU U1: {WATCHDOG_LIMITATION}")
        );
        assert!(sched.watchdog_resets().is_empty());
        let limits = sched.timing_limitations();
        assert_eq!(
            limits,
            vec![("U1".to_string(), TIMING_LIMITATION.to_string())]
        );
        assert_eq!(
            timing_limitation_message(&limits[0].0, &limits[0].1),
            format!("MCU U1: {TIMING_LIMITATION}")
        );
    }

    #[test]
    fn watchdog_reboots_are_counted_per_mcu() {
        let (sched, _h) = sched_with_core(
            MockCore {
                watchdog_resets: 12,
                ..Default::default()
            },
            &[],
        );
        assert_eq!(sched.watchdog_resets(), vec![("U1".to_string(), 12)]);
        assert!(watchdog_reset_message("U1", 12).contains("12 times"));
        assert!(watchdog_reset_message("U1", 1).contains("1 time during this run"));
        assert!(sched.watchdog_limitations().is_empty());
    }

    #[test]
    fn a_faithful_backend_with_no_reboots_reports_nothing_at_all() {
        let (sched, _h) = sched_with_core(MockCore::default(), &[]);
        assert!(sched.watchdog_limitations().is_empty());
        assert!(sched.watchdog_resets().is_empty());
        assert!(sched.timing_limitations().is_empty());
    }

    fn attach_test_buses(sched: &mut Scheduler) {
        let i2c = Arc::new(Mutex::new(
            I2cBus::new("TEMP1").with_slave(Box::new(crate::Lm75::new(0x48, 25.0))),
        ));
        sched.attach_i2c_bus(i2c);
        let spi = Arc::new(Mutex::new(SpiBus::new(
            "FLASH1",
            Box::new(crate::Spi25Eeprom::new(256)),
        )));
        sched.attach_spi_bus(spi, None);
    }

    #[test]
    fn bus_slaves_are_recorded_as_unexercised_only_on_an_unmodeled_controller() {
        let mut sched = sched_with_bus_blind_core("TEMP_SENSE");
        assert!(sched.unexercised_buses().is_empty());
        attach_test_buses(&mut sched);
        let spi2 = Arc::new(Mutex::new(SpiBus::new(
            "IMU1",
            Box::new(crate::Spi25Eeprom::new(256)),
        )));
        sched.attach_spi_bus_on("spi9", spi2, None);
        let rec = sched.unexercised_buses();
        assert_eq!(rec.len(), 3, "{rec:?}");
        assert_eq!((rec[0].id.as_str(), rec[0].bus), ("TEMP1", "I2C"));
        assert_eq!((rec[1].id.as_str(), rec[1].bus), ("FLASH1", "SPI"));
        assert_eq!(rec[2].controller.as_deref(), Some("spi9"));
        assert!(rec[0].message().contains("TEMP1"));

        let (mut sched, _h) = sched_with_core(MockCore::default(), &[]);
        attach_test_buses(&mut sched);
        assert!(sched.unexercised_buses().is_empty());
    }

    #[test]
    fn dropped_adc_channels_resolve_to_their_net_and_warn() {
        let sched = sched_with_bus_blind_core("TEMP_SENSE");
        let drops = sched.adc_dropped();
        assert_eq!(drops.len(), 1, "{drops:?}");
        assert_eq!(drops[0].mcu_ref, "U1");
        assert_eq!(drops[0].channel, 0);
        assert_eq!(drops[0].net, "TEMP_SENSE");
        assert!(drops[0].message().contains("TEMP_SENSE"));
    }

    // ── Multi-bus dispatch through single-slot core hooks ────────────────────

    /// `on_i2c`/`set_i2c_slave_addresses` are single-slot on the core: the
    /// dispatcher must route each event by address to the bus that owns it
    /// and the filter must be the union across attached buses.
    #[test]
    fn attach_two_i2c_buses_routes_firmware_bytes_by_address() {
        use crate::peripherals::i2c::Eeprom24c;
        use hauksbee_mcu::I2cEvent as E;

        let (mut sched, h) = sched_with_core(MockCore::default(), &[]);
        let bus1 = Arc::new(Mutex::new(
            I2cBus::new("U2").with_slave(Box::new(Eeprom24c::new(0x50, 64))),
        ));
        let bus2 = Arc::new(Mutex::new(
            I2cBus::new("U3").with_slave(Box::new(Eeprom24c::new(0x48, 64))),
        ));
        sched.attach_i2c_bus(bus1.clone());
        sched.attach_i2c_bus(bus2.clone());

        let addrs = h
            .i2c_addrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(
            addrs.contains(&0x50) && addrs.contains(&0x48),
            "{addrs:#04x?}"
        );

        let mut slot = h.i2c_cb.lock().unwrap_or_else(|e| e.into_inner());
        let cb = slot.as_mut().expect("on_i2c handler installed");
        let addr = 0x50;
        cb(E::Start { addr, read: false });
        for data in [0x00, 0x00, 0xAB] {
            cb(E::Write { addr, data });
        }
        cb(E::Stop { addr });
        cb(E::Start { addr, read: false });
        cb(E::Write { addr, data: 0x00 });
        cb(E::Write { addr, data: 0x00 });
        cb(E::Start { addr, read: true });
        let read_back = cb(E::Read { addr });
        cb(E::Stop { addr });
        drop(slot);

        let b1 = bus1.lock().unwrap_or_else(|e| e.into_inner());
        let b2 = bus2.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(b1.slave::<Eeprom24c>(0x50).unwrap().contents()[0], 0xAB);
        assert_eq!(read_back, Some(0xAB));
        assert!(b2
            .slave::<Eeprom24c>(0x48)
            .unwrap()
            .contents()
            .iter()
            .all(|&b| b == 0xFF));
    }

    /// `on_spi` is single-slot: each byte must go to the bus whose CS is
    /// currently asserted.
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
        let (mut sched, h) = sched_with_core(MockCore::default(), &[cs1, cs2]);
        let got1 = Arc::new(Mutex::new(Vec::new()));
        let got2 = Arc::new(Mutex::new(Vec::new()));
        let bus = |got: &Arc<Mutex<Vec<u8>>>, miso| {
            Arc::new(Mutex::new(SpiBus::new(
                "U",
                Box::new(RecSlave {
                    got: got.clone(),
                    miso,
                }),
            )))
        };
        let resolved = |pin| {
            Some(ResolvedCs {
                pin,
                net: None,
                provenance: CsProvenance::SpecDeclared,
            })
        };
        sched.attach_spi_bus(bus(&got1, 0x5A), resolved(cs1));
        sched.attach_spi_bus(bus(&got2, 0xA5), resolved(cs2));

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

        edge(cs1, false);
        assert_eq!(xfer(0x42), 0x5A);
        assert_eq!(bytes(&got1), vec![0x42]);
        assert!(bytes(&got2).is_empty());

        edge(cs1, true);
        edge(cs2, false);
        assert_eq!(xfer(0x99), 0xA5);
        assert_eq!(bytes(&got2), vec![0x99]);
        assert_eq!(bytes(&got1), vec![0x42]);
    }

    // ── run_micros integer carry and failure accounting ──────────────────────

    fn sched_with_micros_core(fail: bool) -> (Scheduler, Arc<Mutex<Vec<u64>>>) {
        let (mut sched, h) = sched_with_core(
            MockCore {
                fail_micros: fail,
                ..Default::default()
            },
            &[],
        );
        sched.relayout();
        (sched, h.micros)
    }

    fn delivered_micros(chunk: f64, chunks: usize) -> u64 {
        let (mut sched, micros) = sched_with_micros_core(false);
        let mut uart = HashMap::new();
        for _ in 0..chunks {
            sched.run_chunk(chunk, &mut uart);
        }
        let total = micros
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .sum();
        total
    }

    /// `run_micros` takes integer microseconds: fractional and sub-microsecond
    /// chunks must carry their remainder so the firmware clock neither drifts
    /// behind (per-chunk rounding) nor races ahead (a min-1 clamp).
    #[test]
    fn fractional_and_sub_microsecond_chunks_track_true_elapsed_time() {
        let delivered = delivered_micros(1.3e-6, 10);
        assert!(
            (delivered as f64 - 13.0).abs() <= 1.0,
            "{delivered} us vs 13.0 true"
        );
        assert!(
            delivered >= 12,
            "per-chunk rounding undercounts: {delivered}"
        );

        let delivered = delivered_micros(0.5e-6, 20);
        assert!(
            (delivered as f64 - 10.0).abs() <= 1.0,
            "{delivered} us vs 10.0 true"
        );
        assert!(delivered <= 11, "min-1 clamp races ahead: {delivered}");
    }

    #[test]
    fn scheduler_error_budget_uses_measured_run_not_defaults() {
        let (mut sched, _micros) = sched_with_micros_core(false);
        sched.opts.reltol = 2.5e-4;
        sched.opts.vntol = 7.5e-7;
        sched.run_chunk(1e-4, &mut HashMap::new());

        let budget = sched.error_budget().expect("valid scheduler budget");
        assert_eq!(budget.tolerance().reltol(), 2.5e-4);
        assert_eq!(budget.tolerance().vntol(), 7.5e-7);
        let residual = budget
            .residual()
            .expect("a converged chunk measures its residual");
        assert!(residual.max_abs().is_finite());
        assert!(!residual.at().is_empty());
        assert_eq!(budget.failed_windows().len(), 0);
    }

    /// An MCU that refuses to advance marks the chunk failed (feeding the same
    /// `analog_valid` surface the analog march uses), a sustained refusal
    /// trips the strict abort even though the analog solve converges, and
    /// `reset_run_state` wipes all of it.
    #[test]
    fn mcu_failures_are_accounted_and_cleared_by_reset() {
        let (mut sched, _micros) = sched_with_micros_core(true);
        assert!(sched.analog_valid(), "clean before any chunk runs");
        let mut uart = HashMap::new();
        sched.run_chunk(1e-4, &mut uart);
        assert!(sched.failed_chunk_count() >= 1);
        assert!(!sched.analog_valid());
        assert!(!sched.failed_windows().is_empty());

        for _ in 1..STRICT_CONSECUTIVE_FAILED_ABORT {
            sched.run_chunk(1e-4, &mut uart);
        }
        assert!(sched.analog_abort_tripped());

        sched.reset_run_state();
        assert_eq!(sched.sim_time, 0.0);
        assert_eq!(sched.failed_chunk_count(), 0);
        assert!(sched.failed_windows().is_empty());
        assert!(sched.analog_valid());
        assert!(!sched.analog_abort_tripped());
    }

    #[test]
    fn reset_run_state_reboots_mcu_cores_and_clears_coupling_caches() {
        let (mut sched, h) = sched_with_core(MockCore::default(), &[]);
        sched.relayout();
        {
            let m = sched.mcus.last_mut().unwrap();
            m.last_levels.insert(('B', 5), true);
            m.configured_outputs.insert(('B', 5));
            m.digital_in_levels.insert(('D', 2), true);
            let mut sh = m.shared.lock().unwrap();
            sh.pin_edges.insert(('B', 5), true);
            sh.pin_edge_log.push(PinEdge {
                cycle: 123,
                port: 'B',
                bit: 5,
                level: true,
            });
            sh.uart_out.extend_from_slice(b"stale");
        }

        sched.reset_run_state();

        assert!(
            *h.was_reset.lock().unwrap(),
            "the core's reset must be pulsed"
        );
        let m = sched.mcus.last().unwrap();
        assert!(m.last_levels.is_empty());
        assert!(m.configured_outputs.is_empty());
        assert!(m.digital_in_levels.is_empty());
        let sh = m.shared.lock().unwrap();
        assert!(sh.pin_edges.is_empty() && sh.pin_edge_log.is_empty() && sh.uart_out.is_empty());
    }

    // ── Drive-direction observability ────────────────────────────────────────

    fn dir_core(observable: bool) -> MockCore {
        MockCore {
            direction_observable: observable,
            ..Default::default()
        }
    }

    /// `drive_direction_observable` is the conservative AND across live cores.
    #[test]
    fn drive_direction_observable_ands_across_cores() {
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        assert!(
            sched.drive_direction_observable(),
            "no MCUs: vacuously observable"
        );
        push_core(
            &mut sched,
            dir_core(true),
            binding("U1", "simavr:test", HashMap::new()),
        );
        assert!(sched.drive_direction_observable());
        push_core(
            &mut sched,
            dir_core(false),
            binding("U2", "simavr:test", HashMap::new()),
        );
        assert!(!sched.drive_direction_observable());
    }

    /// On a direction-blind core, a net whose MCU pin driver never reported a
    /// level is listed as unobserved; the flag clears once the driver is
    /// enabled, and a direction-reporting core never populates it.
    #[test]
    fn unobserved_drive_nets_flags_only_direction_blind_undriven_pins() {
        let mut sched = board_scheduler(PLAIN_INPUT_BOARD);
        let node = sched.net_nodes["BTN_HI"];
        for (observable, reference) in [(true, "U1"), (false, "U2")] {
            let drivers = tristated_drivers(&mut sched, &[(('0', 1u8), node)]);
            push_core(
                &mut sched,
                dir_core(observable),
                binding(reference, "test", drivers),
            );
            if observable {
                assert!(sched.unobserved_drive_nets().is_empty());
            }
        }
        assert_eq!(sched.unobserved_drive_nets(), vec!["BTN_HI".to_string()]);
        sched.mcus[1]
            .binding
            .gpio_drivers
            .get_mut(&('0', 1u8))
            .unwrap()
            .enabled = true;
        assert!(sched.unobserved_drive_nets().is_empty());
    }

    /// A control net that reaches its level with nothing driving it is
    /// reported as passively reached; a direction-blind backend makes that
    /// unknowable; only a core that actually wrote the pin is firmware-driven.
    #[test]
    fn a_passively_reached_level_is_not_reported_as_firmware_driven() {
        let mut circuit = Circuit::new();
        let res = circuit.node("RES");
        circuit.add(Device::Vsource {
            name: "Vsupply_VCC".into(),
            p: res,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.3),
        });
        let bound = bound_board(
            "passive_res",
            circuit,
            HashMap::from([("RES".to_string(), res)]),
            Vec::new(),
            Vec::new(),
        );
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

        assert_eq!(sched.level_provenance("RES"), LevelProvenance::Passive);
        let clause = sched.level_reached_clause("RES", 3.3, 1.0);
        assert!(
            clause.contains("PASSIVELY") && !clause.contains("was driven to"),
            "{clause}"
        );

        let with_role = || {
            let mut b = binding("U1", "qemu:test", HashMap::new());
            b.role_nets.insert("pb5".to_string(), res);
            b
        };
        push_core(&mut sched, dir_core(false), with_role());
        assert_eq!(sched.level_provenance("RES"), LevelProvenance::Unobservable);
        assert!(sched
            .level_reached_clause("RES", 3.3, 1.0)
            .contains("UNKNOWN"));

        sched.mcus.clear();
        sched.responder_registries.clear();
        push_core(&mut sched, dir_core(true), with_role());
        assert_eq!(sched.level_provenance("RES"), LevelProvenance::Passive);
        sched.mcus[0].last_levels.insert(('B', 5), true);
        assert_eq!(
            sched.level_provenance("RES"),
            LevelProvenance::FirmwareDriven
        );
        let clause = sched.level_reached_clause("RES", 3.3, 1.0);
        assert!(
            clause.contains("was driven to") && !clause.contains("PASSIVELY"),
            "{clause}"
        );
    }

    // ── Thermal integration through the real march ───────────────────────────

    /// The trapezoid streaming sink and per-chunk energy deposit must reproduce
    /// the duty cycle of a PULSE waveform switching INSIDE the chunk, with the
    /// pulse phase adversarial against the chunk endpoint in both directions.
    #[test]
    fn production_thermal_path_integrates_sub_chunk_pwm() {
        use hauksbee_bind::stress::DeviceMeta;
        use hauksbee_models::schema::{ComponentKind, Ratings};

        // One 1 Ω load across a chunk-local PULSE source: 1 V on ⇒ 1 W.
        // theta_JA 100 C/W, ambient 25 C, Tj limit 90 C.
        let build = |delay: f64, width: f64| -> Scheduler {
            let mut circuit = Circuit::new();
            let a = circuit.node("PWM");
            circuit.add(Device::Vsource {
                name: "V1".into(),
                p: a,
                n: NodeId::GROUND,
                kind: SourceKind::Pulse {
                    v1: 0.0,
                    v2: 1.0,
                    delay,
                    rise: 1e-7,
                    fall: 1e-7,
                    width,
                    period: 0.0,
                },
            });
            let q = circuit.add(Device::Resistor {
                name: "Q1".into(),
                a,
                b: NodeId::GROUND,
                ohms: 1.0,
                tc1: None,
            });
            let meta = DeviceMeta {
                reference: "Q1".into(),
                device: q,
                kind: ComponentKind::Nmos,
                footprint: "Package_TO_SOT_SMD:SOT-23".into(),
                ratings: Ratings {
                    max_power_w: Some(2.0),
                    theta_ja_c_per_w: Some(100.0),
                    max_junction_temp_c: Some(90.0),
                    ..Default::default()
                },
            };
            let bound = bound_board(
                "pwm_thermal",
                circuit,
                HashMap::from([("PWM".to_string(), a)]),
                Vec::new(),
                vec![meta],
            );
            let mut sched =
                Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
            sched.chunk_s = 1.0e-3;
            sched.set_ambient_c(25.0);
            sched
        };
        let tj = |sched: &Scheduler| sched.temp_states()["Q1"];
        let overtemp = |sched: &mut Scheduler| {
            sched
                .drain_faults()
                .into_iter()
                .filter(|f| f.kind == hauksbee_bind::stress::FaultKind::Overtemperature)
                .collect::<Vec<_>>()
        };

        // 75% duty, endpoint OFF: integral 0.75 W ⇒ Tj ≈ 100 C > 90 C.
        let mut hot = build(0.0, 0.75e-3);
        hot.step(8.0e-3);
        assert!((tj(&hot) - 100.0).abs() < 1.0, "got {:.3}", tj(&hot));
        let faults = overtemp(&mut hot);
        assert_eq!(faults.len(), 1);
        assert!((faults[0].value - 100.0).abs() < 1.0);

        // ~10% duty, endpoint ON: integral ~0.1 W ⇒ Tj ≈ 35 C, silent.
        let mut cool = build(0.9e-3, 0.2e-3);
        cool.step(8.0e-3);
        assert!((tj(&cool) - 35.0).abs() < 1.0, "got {:.3}", tj(&cool));
        assert!(overtemp(&mut cool).is_empty());
    }

    // ── Sub-chunk pulses, timing policy and runtime contention ───────────────

    /// A 2 us GPIO pulse inside one 100 us chunk on the net clocking a
    /// tick-evaluated 74HC74 warns once per net per run, naming the net, the
    /// measured width and the part at risk; the pulse still counts two toggles.
    #[test]
    fn subchunk_pulse_on_a_tick_sequential_clock_net_warns_once() {
        let mut sched = pulse_scheduler(PULSE_BOARD, &[(('B', 1), "STROBE")]);
        preload_edges(&sched, ('B', 1), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);

        let pulses = sched.short_pulses();
        assert_eq!(pulses.len(), 1, "{pulses:?}");
        let p = &pulses[0];
        assert_eq!(p.net, "STROBE");
        assert_eq!(p.mcu_ref, "A1");
        assert_eq!((p.port, p.bit), ('B', 1));
        assert_eq!(p.parts, vec!["U5".to_string()]);
        assert!((p.pulse_s - 2e-6).abs() < 1e-9, "got {}", p.pulse_s);
        assert!((p.chunk_s - DEFAULT_CHUNK_S).abs() < 1e-12);
        assert!(p.message().contains("STROBE") && p.message().contains("U5"));
        assert_eq!(sched.toggle_counts().get("STROBE"), Some(&2));

        preload_edges(&sched, ('B', 1), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert_eq!(sched.short_pulses().len(), 1, "once per net per run");
    }

    #[test]
    fn pwl_transition_budget_is_an_explicit_timing_refusal() {
        let mut sched = pulse_scheduler(PULSE_BOARD, &[(('B', 1), "STROBE")]);
        let strobe = sched.net_nodes["STROBE"];
        sched.circuit.add(Device::Resistor {
            name: "Rload".into(),
            a: strobe,
            b: NodeId::GROUND,
            ohms: 10_000.0,
            tc1: None,
        });
        sched.relayout();
        let transitions: Vec<(u64, bool)> =
            (0..=10_000).map(|cycle| (cycle, cycle % 2 == 0)).collect();
        preload_edges(&sched, ('B', 1), &transitions);
        sched.step(DEFAULT_CHUNK_S);

        let refusals = sched.timing_refusals();
        assert_eq!(refusals.len(), 1, "one refusal per affected net");
        assert!(refusals[0].contains("STROBE") && refusals[0].contains("PWL"));
    }

    /// A pulse spanning chunks is observed by the boundary sample, and a pulse
    /// on a net clocking nothing sequential is fine at any width: silent.
    #[test]
    fn spanning_pulse_and_non_clock_net_stay_silent() {
        let mut sched = pulse_scheduler(PULSE_BOARD, &[(('B', 1), "STROBE")]);
        preload_edges(&sched, ('B', 1), &[(100, true)]);
        for _ in 0..10 {
            sched.step(DEFAULT_CHUNK_S);
        }
        preload_edges(&sched, ('B', 1), &[(100, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert!(
            sched.short_pulses().is_empty(),
            "{:?}",
            sched.short_pulses()
        );

        let mut sched = pulse_scheduler(PULSE_BOARD, &[(('B', 2), "FREE")]);
        preload_edges(&sched, ('B', 2), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert!(
            sched.short_pulses().is_empty(),
            "{:?}",
            sched.short_pulses()
        );
    }

    fn coarse_scheduler() -> Scheduler {
        let mut coarse = pulse_scheduler(PULSE_BOARD, &[(('B', 1), "STROBE")]);
        coarse.mcus[0].core = Box::new(MockCore {
            coarse: true,
            ..Default::default()
        });
        coarse
    }

    #[test]
    fn timing_policy_uses_measured_backend_resolution_and_adapts_poll_chunks() {
        let mut exact = pulse_scheduler(PULSE_BOARD, &[(('B', 1), "STROBE")]);
        let exact_cov = exact.timing_coverage();
        assert_eq!(exact_cov.len(), 1);
        assert!(exact_cov[0].cycle_exact);
        assert!((exact_cov[0].timestamp_precision_s - 1.0 / 16_000_000.0).abs() < 1e-15);
        assert!((exact_cov[0].minimum_guaranteed_pulse_s - 1.0 / 16_000_000.0).abs() < 1e-15);
        exact
            .configure_timing(TimingRequirement {
                min_pulse_s: Some(2e-6),
                max_edge_error_s: Some(100e-9),
            })
            .expect("a 16 MHz push backend resolves both budgets without chunking");
        assert_eq!(exact.chunk_s, DEFAULT_CHUNK_S);

        let mut coarse = coarse_scheduler();
        coarse
            .configure_timing(TimingRequirement {
                min_pulse_s: Some(20e-6),
                max_edge_error_s: Some(4e-6),
            })
            .expect("poll chunk can be refined to the requested measured budget");
        assert!((coarse.chunk_s - 4e-6).abs() < 1e-15);
        let coarse_cov = coarse.timing_coverage();
        assert!(!coarse_cov[0].cycle_exact);
        assert!((coarse_cov[0].timestamp_precision_s - 4e-6).abs() < 1e-15);
        assert!((coarse_cov[0].minimum_guaranteed_pulse_s - 8e-6).abs() < 1e-15);
    }

    #[test]
    fn timing_policy_refuses_poll_precision_below_the_bridge_quantum() {
        let mut coarse = coarse_scheduler();
        let err = coarse
            .configure_timing(TimingRequirement {
                min_pulse_s: Some(1e-6),
                max_edge_error_s: None,
            })
            .expect_err("a 0.5 us poll slice cannot be represented by run_micros(u64)");
        assert!(err.to_string().contains("1.000 us"), "{err}");
        assert_eq!(
            coarse.chunk_s, DEFAULT_CHUNK_S,
            "refusal must not half-apply policy"
        );
    }

    #[test]
    fn adaptive_chunk_is_a_ceiling_not_a_rounded_target() {
        let mut coarse = coarse_scheduler();
        coarse
            .configure_timing(TimingRequirement {
                min_pulse_s: None,
                max_edge_error_s: Some(3e-6),
            })
            .expect("3 us polls are representable");
        coarse.step(10e-6);
        assert_eq!(
            coarse.mcus[0].core.state().pc,
            4,
            "10 us = ceil(10/3) slices"
        );
    }

    /// A tri-stated MCU pin sharing a net with a 74HC08 output is the healthy
    /// gate-feeds-input topology; once the firmware drives the pin two
    /// push-pull drivers fight and the monitor fires, once per net per run.
    #[test]
    fn firmware_output_fighting_an_enabled_model_output_fires_once() {
        let mut sched = pulse_scheduler(CONTENTION_BOARD, &[(('B', 1), "SHARED")]);
        sched.step(2.0 * DEFAULT_CHUNK_S);
        assert!(
            sched.driver_contentions().is_empty(),
            "{:?}",
            sched.driver_contentions()
        );

        preload_edges(&sched, ('B', 1), &[(10, true)]);
        sched.step(DEFAULT_CHUNK_S);
        let found = sched.driver_contentions();
        assert_eq!(found.len(), 1, "{found:?}");
        let c = &found[0];
        assert_eq!(c.net, "SHARED");
        assert_eq!(c.mcu_ref, "A1");
        assert_eq!((c.port, c.bit), ('B', 1));
        assert_eq!(c.parts, vec!["U1.y1".to_string()]);
        assert!(
            c.t_s > 0.0,
            "detection is skipped on the unsolved first chunk"
        );
        assert!(c.message().contains("SHARED") && c.message().contains("U1.y1"));

        sched.step(2.0 * DEFAULT_CHUNK_S);
        assert_eq!(sched.driver_contentions().len(), 1, "once per net per run");
    }

    /// A 74HC125 output released by its tied-high OE is not driving, so a
    /// firmware output on the same net is not contention.
    #[test]
    fn tristated_model_output_is_not_contention() {
        let mut sched = pulse_scheduler(TRISTATE_BOARD, &[(('B', 1), "BUS")]);
        sched.step(2.0 * DEFAULT_CHUNK_S);
        preload_edges(&sched, ('B', 1), &[(10, true)]);
        sched.step(3.0 * DEFAULT_CHUNK_S);
        assert!(
            sched.driver_contentions().is_empty(),
            "{:?}",
            sched.driver_contentions()
        );
    }
}

/// `renode:<part>` instantiation consults the SoC-descriptor override dirs:
/// override beats builtin, an invalid override fails loudly, aliases fall back
/// to their canonical descriptors, and an unknown part names the dirs searched.
/// One test fn: it mutates HAUKSBEE_MCU_DIR.
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

        let f101 = include_str!("../../hauksbee-mcu/db/mcu/stm32f103.soc.toml").replace(
            "mcu_label = \"STM32F103 (ARM Cortex-M3)\"",
            "mcu_label = \"STM32F101 (ARM Cortex-M3)\"",
        );
        std::fs::write(dir.join("stm32f101.soc.toml"), &f101).unwrap();
        let broken = include_str!("../../hauksbee-mcu/db/mcu/sifive_fe310.soc.toml")
            .replace("platform_repl =", "platform_rep =");
        std::fs::write(dir.join("sifive_fe310.soc.toml"), &broken).unwrap();

        std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
        let new_part = resolve_renode_config("stm32f101");
        let invalid_override = resolve_renode_config("sifive_fe310");
        let alias = resolve_renode_config("pico");
        let missing = resolve_renode_config("stm32f199");
        std::env::remove_var("HAUKSBEE_MCU_DIR");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            new_part.expect("new part resolves").mcu_label,
            "STM32F101 (ARM Cortex-M3)"
        );
        let err = invalid_override
            .expect_err("invalid override must fail")
            .to_string();
        assert!(
            err.contains("sifive_fe310.soc.toml") && err.contains("platform_rep"),
            "{err}"
        );
        assert_eq!(alias.expect("alias resolves").machine, "rp2040");
        let err = missing.expect_err("unknown part must fail").to_string();
        assert!(
            err.contains("no SoC descriptor found") && err.contains("renode:stm32f199"),
            "{err}"
        );
    }
}
