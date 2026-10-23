//! The co-simulation scheduler.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/scheduler.md.
//!
//! Generalizes an earlier board-specific emulator's lockstep pattern. Each call to
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

use hauksbee_ir::eprintln;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hauksbee_ir::evidence::{Assumption, AssumptionSource};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_mcu::backend_id::BackendId;
#[cfg(feature = "avr")]
use hauksbee_mcu::AvrMcu;
use hauksbee_mcu::{Mcu, PinId};
use hauksbee_models::{Bus, PeripheralSpec};
use hauksbee_solve::{
    dc_operating_point_seeded, DcInit, Layout, MarchCarry, OperatingPointReport, SolverOptions,
    StepSample, Transient, TransientDiagnostics, Workspace,
};

use crate::peripherals::{
    CsProvenance, Eeprom24c, I2cBus, PeripheralSet, RegisterMapSensor, ResolvedCs, SpiBus,
    SpiFramingMode, SpiNorFlash, TickCtx, TimelineEvent,
};
use crate::regulation;
pub use crate::regulation::{open_driver_reason, RegulationTaint, OPEN_DRIVER};
use crate::responders::{GpioPin, Hc595ChainPins};
use crate::MutexExt;
#[path = "scheduler/electrics.rs"]
mod electrics;
pub use electrics::{reset_episode_message, ResetCause, ResetEpisode};
use hauksbee_bind::behavioral::BehavioralDevice;
use hauksbee_bind::bind_report::SimpleRegulator;
use hauksbee_bind::binder::{apin_gpio_of_role, gpio_of_role, BoundBoard, McuBinding};
use hauksbee_bind::digital::{DigitalComponent, PinEdge};
use hauksbee_bind::power_supply::{PowerSupply, SupplyLeg};
use hauksbee_bind::stress::{FaultEvent, RegulatorHeat, StressMonitor};

/// Default co-sim chunk size (seconds).
pub const DEFAULT_CHUNK_S: f64 = 100e-6;

/// Smallest useful poll slice supported by the MCU bridge.
///
/// `Mcu::run_micros` accepts an integer microsecond count; asking Renode/QEMU
/// (which observe GPIO only after that call) to poll more finely would execute
/// zero guest time and manufacture precision the bridge does not have.
pub const POLL_BACKEND_QUANTUM_S: f64 = 1e-6;

/// [`TimingCoverage::backend`] of the chunk-sampled digital logic row.
pub const CHUNK_SAMPLED_LOGIC: &str = "chunk-sampled logic, tpd not applied";

/// A timing claim requested by a CI spec or another strict caller.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TimingRequirement {
    /// Narrowest pulse the caller needs guaranteed observable, in seconds.
    pub min_pulse_s: Option<f64>,
    /// Largest acceptable edge timestamp uncertainty, in seconds.
    pub max_edge_error_s: Option<f64>,
}

/// Measured timing capability of one live MCU backend, or of the digital
/// logic evaluated once per chunk, at the configured chunk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct TimingCoverage {
    /// The MCU reference, or the comma-joined chunk-sampled logic references.
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

/// A model output this many times the MCU pin's resistance is overdriven, not
/// fought: the pin holds the net within 1/(1 + ratio), under 5 %, of its own
/// level. A `TXB010x` DC driver (4.5 kOhm, SCES639F 7.2.2) against a 50 Ohm pin
/// is 90; a CMOS logic output (tens of Ohms) is about 1.
const OVERDRIVE_RATIO: f64 = 20.0;

/// A strict headless run (`--strict`) or hauksbee-ci aborts (exit 3,
/// invalid-for-analysis) once the analog solve fails this many chunks in a row.
///
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
    /// Charge each regulator's source delivered into its output (C), in
    /// `Scheduler::regulators` order, for its pass draw's next level.
    pass_charge: Vec<f64>,
    /// Simulated seconds `pass_charge` covers.
    pass_s: f64,
    /// Charge each routed pin driver sourced and sunk (C), in
    /// `Scheduler::driver_draws` order: `(positive part, negative part)` of
    /// its delivered current, integrated separately so an alternating output
    /// still draws from the supply.
    drive_charge: Vec<(f64, f64)>,
    /// Simulated seconds `drive_charge` covers.
    drive_s: f64,
    /// The march's own t = 0 state (the first streamed sample): the carried
    /// state on a continued march, a fresh operating point on a cold one. The
    /// cold-start rung measures its state discontinuity against it.
    start_x: Vec<f64>,
    /// Every accepted step of the march at the watched nodes (see
    /// [`Scheduler::watch_steps`]): chunk-local time and one voltage per node.
    steps: Vec<(f64, Vec<f64>)>,
}

impl ChunkThermalAccum {
    /// Discard the partial integral (a failed rung's fiction).
    fn clear(&mut self) {
        self.energy_j.clear();
        self.elapsed_s = 0.0;
        self.pass_charge.clear();
        self.pass_s = 0.0;
        self.drive_charge.clear();
        self.drive_s = 0.0;
        self.start_x.clear();
        self.steps.clear();
    }

    /// Take a streamed sample as the march's `start_x` if it is the first,
    /// and say whether it is a stand-in ([`StepSample::stand_in`]): the
    /// relaxed seed of a DC with no root, where the march left from but no
    /// state of the board. Nothing is read off one: no step series, no
    /// published or recovered state, no charge or power, so every trapezoid
    /// opens at the first solved step and a march that fails before one
    /// recovers nothing.
    fn open_at(&mut self, s: &StepSample) -> bool {
        if self.start_x.is_empty() {
            self.start_x.extend_from_slice(s.x);
        }
        s.stand_in
    }

    /// Record one accepted step at the watched nodes (ground reads 0 V).
    fn record_step(&mut self, watch: &[(String, NodeId)], time: f64, x: &[f64]) {
        if watch.is_empty() {
            return;
        }
        let at = |n: NodeId| x.get((n.0 as usize).wrapping_sub(1)).copied();
        let volts = watch.iter().map(|(_, n)| at(*n).unwrap_or(0.0));
        self.steps.push((time, volts.collect()));
    }
}

/// Which fallback rung produced a chunk's converged answer after the primary
/// integration failed (see `Scheduler::solve_chunk`'s ladder, tried in this
/// order).
///
/// A number obtained by a more dissipative method is not the same
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
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ReducedStep => "reduced-step",
            Self::BackwardEuler => "backward-euler",
            Self::ColdStartBackwardEuler => "cold-start-backward-euler",
            Self::SubdividedBackwardEuler => "subdivided-backward-euler",
        }
    }

    /// Qualitative numerical-fidelity context for this rung: it names the
    /// rung's algorithmic trade-off and points at the measured number, which
    /// lives on `FallbackWindow::error_estimate_v`.
    #[must_use]
    pub const fn fidelity_note(&self) -> &'static str {
        match self {
            Self::ReducedStep => {
                "primary integration at a smaller maximum step; \
                 error_estimate_v carries the worst per-chunk measured \
                 estimate of the error each chunk's march added to its end \
                 state, from a companion re-solve at a shifted accuracy dial \
                 (absent when no companion converged)"
            }
            Self::ColdStartBackwardEuler => {
                "first-order backward Euler from a FRESH DC operating point: \
                 the state carried from the previous chunk (capacitor \
                 voltages, inductor currents, latches) was DISCARDED, so the \
                 trajectory is discontinuous at the window start; \
                 state_jump_v carries the largest node-voltage jump that \
                 restart introduced, and error_estimate_v only the error each \
                 chunk's march added after it, from a companion re-solve at a \
                 shifted accuracy dial (absent when no companion converged)"
            }
            Self::BackwardEuler | Self::SubdividedBackwardEuler => {
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
    /// For a rung that DISCARDS the carried chunk-start state
    /// ([`ChunkFallbackMethod::ColdStartBackwardEuler`]): the largest
    /// node-voltage jump, in volts, between the state the previous chunk
    /// ended in and the fresh operating point the rung marched from. The run's
    /// trajectory is discontinuous by this much at the start of each such
    /// chunk, which `error_estimate_v` (measured against the rung's own seed)
    /// cannot see. Merged windows carry the worst chunk's jump. `None` for a
    /// rung that continues the carried state, or when no carried state
    /// existed to jump from.
    pub state_jump_v: Option<f64>,
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
    /// UART bytes this core transmitted over the run (execution evidence).
    uart_bytes: u64,
    /// The drive-strength row each pin's firmware selected (index into the
    /// card's `output_drive`); a pin absent here runs the card's reset default.
    drive_rows: HashMap<(char, u8), usize>,
    /// Chunk counter for the periodic drive-select sweep.
    drive_poll: u32,
    /// Reset-pin / brown-out state (see `electrics`).
    reset_gate: electrics::ResetGate,
    /// Hardware I2C events (start, address, byte, stop) this core's backend
    /// delivered to the bus handler over the run. Shared with the `on_i2c`
    /// closure, which counts every event whether or not a modelled slave
    /// answers it, because the TWI block drove SDA and SCL either way.
    i2c_events: Arc<AtomicU64>,
    /// Hardware SPI events (byte transfers, chip-select deasserts) this core's
    /// backend delivered to the bus handler over the run; shared with the
    /// `on_spi` closure like [`Self::i2c_events`].
    spi_events: Arc<AtomicU64>,
    /// Firmware output edges per pin over the run (execution evidence).
    pin_toggles: std::collections::BTreeMap<(char, u8), u64>,
    /// Reset line and supply pads, watched across the run.
    rails: crate::execution::RailWatch,
    /// Boot-strap resolution and reset gating, for a chip whose card carries a
    /// boot table.
    boot: Option<crate::boot::BootState>,
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
    /// Supply-minus-return rail voltage at the `node_volts` operating point
    /// (an out-of-range node reads 0 V).
    fn supply_v(&self, node_volts: &[f64]) -> f64 {
        let v = |node: NodeId| node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
        v(self.supply_node) - v(self.return_node)
    }

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
            ModelPeripheralBus::I2c(bus) => bus.lock_or_recover().set_powered(powered),
            ModelPeripheralBus::Spi(bus) => bus.lock_or_recover().set_powered(powered),
        }
    }

    /// Whether the bus is a SPI slave currently in a deep-power-down state.
    fn low_power_mode(&self) -> bool {
        match &self.bus {
            ModelPeripheralBus::I2c(_) => false,
            ModelPeripheralBus::Spi(bus) => bus.lock_or_recover().low_power_mode(),
        }
    }

    /// Discard any protocol activity the bus recorded but has not been charged
    /// for, so a replay starts from an idle envelope.
    fn drain_activity(&self) {
        match &self.bus {
            ModelPeripheralBus::I2c(bus) => {
                let _ = bus.lock_or_recover().take_activity();
            }
            ModelPeripheralBus::Spi(bus) => {
                let _ = bus.lock_or_recover().take_activity();
            }
        }
    }
}

/// The scheduler driving one bound board.
#[allow(clippy::struct_excessive_bools)] // independent run flags and debug hooks, not a state machine
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
    /// Simple regulators, whether each currently has independent input headroom
    /// (never again once tainted), and whether that must be re-established.
    regulators: Vec<SimpleRegulator>,
    /// Every pin driver whose output current is drawn from its part's supply
    /// pins: its `Vsource` (the current it delivers is read there) and the legs
    /// set once per chunk (see `hauksbee_bind::drivers::DriverSupply`).
    driver_draws: Vec<(DeviceId, hauksbee_bind::drivers::DriverSupply)>,
    open_parts: Vec<hauksbee_bind::bind_report::OpenPart>,
    reg_supported: Vec<bool>,
    reg_dirty: bool,
    /// Append-only, and deliberately NOT cleared by [`Self::reset_run_state`].
    regulation: Vec<RegulationTaint>,
    /// Latest solved branch currents, indexed by branch unknown (after nodes).
    /// `branch_x[branch_index]`; map a device to its branch with [`Layout`].
    branch_x: Vec<f64>,
    /// Previous chunk's final unknown vector, used to warm-start the next chunk's
    /// DC operating point (skips cold-start homotopy on a stiff nonlinear board).
    /// Cleared on a solver failure or a structural relayout (size mismatch is
    /// also rejected safely by the solver).
    last_dc_seed: Option<Vec<f64>>,
    /// The rest of the state the previous converged chunk's march ENDED on
    /// (relay latch, staged-regularizer arming, reactive histories), carried
    /// with `last_dc_seed` so the next chunk continues one march rather than
    /// re-solving a DC point per window (see [`MarchCarry`]). Cleared with
    /// the seed, and on any chunk that did not converge.
    last_carry: Option<MarchCarry>,
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
    /// When the board is powered: a cold power-on ramp ends here, so a net's
    /// [`Self::net_range`] floor counts only samples from then on.
    pub powered_from_s: f64,
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
    /// The bench recorder: pin edges with times, serial lines, bus
    /// transactions, reset pulses. See [`crate::observe`].
    observer: crate::observe::Observer,
    /// Function symbols of the firmware ELF, to name where a parked core sits.
    firmware_symbols: Option<crate::observe::symbols::SymbolTable>,
    /// Net-attached / output peripherals (controls, VCD sinks), ticked each
    /// chunk around the analog solve.
    pub peripherals: PeripheralSet,
    /// I2C bus slaves, shared with each MCU's `on_i2c` callback.
    i2c_buses: Vec<Arc<Mutex<I2cBus>>>,
    /// SPI bus slaves, shared with each MCU's `on_spi` callback.
    spi_buses: Vec<Arc<Mutex<SpiBus>>>,
    /// The subset of `i2c_buses` reached through a backend's hardware I2C
    /// handler (`attach_i2c_bus`), as opposed to a soft bus on GPIO pins.
    hw_i2c_buses: Vec<Arc<Mutex<I2cBus>>>,
    /// The subset of `spi_buses` reached through a backend's hardware SPI
    /// handler (`attach_spi_bus`, `attach_spi_bus_on`).
    hw_spi_buses: Vec<Arc<Mutex<SpiBus>>>,
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
    /// AVR fuse/board clock inconsistencies found at build time (see
    /// [`avr_clock_plan`]); each was also printed as a WARNING.
    clock_fuse_findings: Vec<AvrClockFinding>,
    /// One sentence per AVR whose core clock is a GUESS (the card's
    /// `default_clock_hz`, with no fuses or crystal frequency to say otherwise);
    /// each was also printed as a WARNING. Every result that rests on the run
    /// carries them as conditions: a guessed clock is never silent.
    clock_assumptions: Vec<String>,
    /// Bus peripherals (I2C/SPI slave models) attached on a board whose live
    /// MCU backends model NO matching bus controller: the firmware's bus
    /// traffic can never reach them, so they sit at their power-on defaults for
    /// the whole run. Recorded at attach time and surfaced as a co-sim coverage
    /// warning; a CI `peripheral` assertion against one of these FAILS rather
    /// than passing on the slave's untouched default state.
    unexercised_buses: Vec<UnexercisedBus>,
    /// MCUs the board carries that were left out of this run because no
    /// firmware was given, each with the nodes of its GPIO pads. They never
    /// execute, so nothing in the run is a reading of what they would do.
    idle_mcus: Vec<IdleMcu>,
    /// One line per MCU footprint left without a processor because it is an
    /// alternate footprint of another (see [`alternate_footprints`]).
    alternate_footprint_notes: Vec<String>,
    /// MCU-bit-banged 74HC165 read chains, resolved at GPIO-output edge
    /// granularity inside the owning MCU's run loop via its synchronous input
    /// responder (the read-direction analogue of `chains`). Wrapped in
    /// Arc<Mutex<>> because the responder closure owns a clone. Each shares the
    /// `input_volts` snapshot the scheduler refreshes from the last solve so the
    /// 165 captures the latest pulse-latch states on its PL load.
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
    /// analog solve. Used by firmware-driven inference runs to drive output
    /// nets from an EXACT feedforward decomposition (when the monolith
    /// does not converge); the pulses the decomposition
    /// produces are presented on those nets so on-board NOR
    /// latches capture them and the firmware's 165 readback reflects them. Empty
    /// on every other board (no override). Keyed by `NodeId.0`. The value is
    /// `(high_volts, low_volts, t_start, t_end)`: the node is held at
    /// `high_volts` while `t_start <= sim_time < t_end`, else `low_volts`. A
    /// time-unbounded override uses `t_start=-inf, t_end=+inf` (always high). The
    /// time window lets the firmware-driven inference present each output
    /// net HIGH for a sim-time fraction proportional to its decomposed pulse
    /// count, so the firmware's per-sample reset-gated latch reads accumulate a
    /// count that tracks the decomposed RATE (not just a binary "fired at all").
    forced_node_volts: HashMap<NodeId, (f64, f64, f64, f64)>,
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
    /// How the last chunk that solved its own t = 0 operating point (rather
    /// than continuing carried state) reached it; see [`Self::operating_point`].
    operating_point: Option<OperatingPointReport>,
    /// The relay latch the last converged chunk ENDED on, carried into the
    /// next. A chunk builds a fresh solver workspace, so without this every
    /// latching switch resets at every chunk boundary -- the same defect that
    /// made a Schmitt oscillator free-run at one toggle per chunk, but on the
    /// discrete state instead of the reactive one.
    last_switch_latch: Option<std::collections::HashMap<hauksbee_ir::DeviceId, bool>>,
    /// This board stamps a LATCH out of hysteretic switches (a 555's SR latch),
    /// which changes two things about how its chunks are marched; see
    /// `march_chunk`.
    latching: bool,
    /// Current run of back-to-back failed chunks (reset to 0 by any converged
    /// chunk). Feeds the strict/CI abort threshold.
    consecutive_failed_chunks: u32,
    /// Worst back-to-back failed-chunk run seen this run. Retained even after a
    /// later chunk converges, so a strict post-run check still sees a streak that
    /// crossed the abort threshold mid-run.
    max_consecutive_failed_chunks: u32,
    /// True once any chunk's analog march converged this run. A run that has
    /// never converged and has failed a whole abort streak from t = 0 is
    /// re-solving the same cold start: see [`Self::dead_from_start`].
    analog_ever_converged: bool,
    /// The rails as the board's DC operating point at t = 0 holds them, solved
    /// once for the digital parts' first look at their supplies (before any
    /// chunk has solved); `None` when that solve did not converge.
    /// See [`Self::digital_view_volts`].
    initial_rail_view: Option<Vec<f64>>,
    /// The t = 0 DC solve behind [`Self::initial_rail_view`] has been tried.
    initial_rail_view_tried: bool,
    /// The dead-from-start notice has been printed for this run.
    dead_from_start_noted: bool,
    /// The solver's message from the most recent failed chunk (e.g. "Newton
    /// failed at t=... even at `dt_min`=...", device-named when a behavioural
    /// fault caused it). Kept so the failure surfaces with its cause instead
    /// of being discarded; also printed once at the start of each failed
    /// streak, which is the breadcrumb a bisection needs.
    last_solve_error: Option<String>,
    /// A reset may mutate even its first core before failing; never march it.
    reset_failure: Option<String>,
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
    /// current). `frame_v_extremes`: net name -> (`min_v`, `max_v`) over the frame.
    frame_peak_current: HashMap<String, f64>,
    frame_v_extremes: HashMap<String, (f64, f64)>,
    /// Nets whose voltage is recorded at EVERY accepted transient step, not
    /// once per chunk (see [`Scheduler::watch_steps`]), and the series so far.
    step_watch: Vec<(String, NodeId)>,
    step_series: HashMap<String, Vec<(f64, f64)>>,
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
    /// Per-MCU execution evidence, as of the last
    /// [`Scheduler::refresh_execution_evidence`].
    execution: Vec<crate::execution::ExecutionEvidence>,
}

/// A chip-substitution event: the board asked for `requested_part` but the
/// available emulator core models a less-specific platform (`modelled_core`).
///
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
    #[must_use]
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
    #[must_use]
    pub const fn event(&self) -> &McuSubstitution {
        &self.event
    }

    /// The opaque board-occurrence subject used for causal traversal.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The substitution assumption constructed from the paired event/subject.
    #[must_use]
    pub const fn assumption(&self) -> &Assumption {
        &self.assumption
    }
}

/// An MCU the board carries that no firmware was given for, so it never ran.
#[derive(Debug, Clone)]
struct IdleMcu {
    reference: String,
    gpio_nodes: Vec<NodeId>,
}

/// A bus peripheral bound on a platform that models no matching bus controller.
///
/// The device is on the board, but the emulated MCU has no controller the
/// bridge could attach to, so the firmware never talks to it, a silent no-op
/// unless surfaced.
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
    #[must_use]
    pub fn message(&self) -> String {
        let on = self
            .controller
            .as_ref()
            .map_or_else(String::new, |c| format!(" (bound to controller '{c}')"));
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
    #[must_use]
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
#[must_use]
pub fn watchdog_limitation_message(mcu_ref: &str, limitation: &str) -> String {
    format!("MCU {mcu_ref}: {limitation}")
}

/// The same, for an entry of [`Scheduler::timing_limitations`]: the backend's
/// whole sentence, unchanged, prefixed only with which MCU it is about.
#[must_use]
pub fn timing_limitation_message(mcu_ref: &str, limitation: &str) -> String {
    format!("MCU {mcu_ref}: {limitation}")
}

/// The one-line resolution statement for a [`TimingCoverage`] row.
///
/// It gives the edge timestamp uncertainty, the narrowest pulse guaranteed
/// observable, the chunk actually run, and whether the stamps are cycle-exact
/// or poll-boundary.
///
/// Shared so no surface words one core's resolution two ways. The leading
/// indent belongs to the text table and is added there, not here.
#[must_use]
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
#[must_use]
pub fn watchdog_reset_message(mcu_ref: &str, resets: u64) -> String {
    let plural = if resets == 1 { "" } else { "s" };
    format!(
        "MCU {mcu_ref}: the watchdog rebooted the core {resets} time{plural} during this \
         run; behaviour observed after the first reboot belongs to a rebooted core"
    )
}

const UNVERIFIED_HEADROOM: &str = "; UNVERIFIED HEADROOM: the model has no dropout figure; \
                                   only Vin >= Vout was checked";

/// Watchdog limitations, watchdog resets, timing limitations, then regulation
/// refusals, in the one order and wording every surface carries them.
///
/// Both the CI runner's coverage warnings and the `run` binary's shared
/// coverage messages use this, so those two cannot drift apart.
///
/// Each is a claim the run cannot fully back: a systematic time bias on a core,
/// or an ideal setpoint with no proven input headroom standing in for a
/// simulated result.
#[must_use]
pub fn coverage_limitation_messages(scheduler: &Scheduler) -> Vec<String> {
    let limitations = scheduler.watchdog_limitations();
    let resets = scheduler.watchdog_resets();
    let timing = scheduler.timing_limitations();
    limitations
        .iter()
        .map(|(mcu_ref, l)| watchdog_limitation_message(mcu_ref, l))
        .chain(resets.iter().map(|(m, r)| watchdog_reset_message(m, *r)))
        .chain(
            scheduler
                .reset_episodes()
                .iter()
                .map(|(m, e)| reset_episode_message(m, e)),
        )
        .chain(timing.iter().map(|(m, l)| timing_limitation_message(m, l)))
        .chain(
            scheduler
                .regulation_taint()
                .iter()
                .map(|t| t.reason.clone()),
        )
        .chain(scheduler.alternate_footprint_notes().iter().cloned())
        .collect()
}

/// A firmware GPIO pulse that rose AND fell inside a single solver chunk, on a
/// net that clocks a TICK-evaluated sequential part.
///
/// Chain-responder parts (74HC595/165 chains, bit-banged
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
    #[must_use]
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
/// driving.
///
/// This is the model-vs-MCU half of the field failure whose
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
    #[must_use]
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
    #[must_use]
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
    /// The minimum once the board is powered (see `Scheduler::powered_from_s`).
    pub powered_min_v: f64,
    pub toggles: u64,
    last_logic: Option<bool>,
}

impl Default for NetStat {
    fn default() -> Self {
        Self {
            min_v: f64::INFINITY,
            max_v: f64::NEG_INFINITY,
            powered_min_v: f64::INFINITY,
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
    #[must_use]
    pub fn with_toggles(toggles: u64) -> Self {
        Self {
            toggles,
            ..Default::default()
        }
    }

    /// Test constructor with an explicit voltage range, for the activity-ranking
    /// tie-break (equal toggles, differing swing) the web/CLI/JSON tables share.
    #[doc(hidden)]
    #[must_use]
    pub fn with_toggles_and_range(toggles: u64, min_v: f64, max_v: f64) -> Self {
        Self {
            min_v,
            max_v,
            toggles,
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
    ///
    /// # Errors
    ///
    /// Returns an error if firmware is given and an MCU's active-low reset role is
    /// wired directly to ground, or if an MCU core cannot be instantiated or loaded
    /// with the firmware.
    #[allow(clippy::too_many_lines)] // constructor wiring every subsystem in dependency order
    pub fn new(
        bound: BoundBoard,
        firmware: Option<&std::path::Path>,
        mut opts: SolverOptions,
    ) -> anyhow::Result<Self> {
        // A part whose stamp uses a VSwitch as a LATCH (a 555's SR latch) has no
        // memory at all under SwitchModel::Smooth, so it would free-run and
        // report a plausible wrong frequency rather than fail. Nothing sets
        // Smooth on the board path today; this makes that a property of the
        // board rather than of who happened to build the options.
        let bound_latching = bound.latching_switches;
        if bound_latching {
            opts.effects.switch_model = hauksbee_solve::SwitchModel::Hysteretic;
        }
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
            mcu_boot,
            ..
        } = bound;

        // Preflight every binding before starting any emulator. These mapped
        // roles are active-low reset inputs; this checks hard wiring, not an
        // analogue threshold or a claim about an unmodelled reset network.
        if firmware.is_some() {
            for binding in &mcus {
                if let Some(role) = RESET_ROLES
                    .into_iter()
                    .find(|role| binding.role_nets.get(*role) == Some(&NodeId::GROUND))
                {
                    anyhow::bail!(
                        "{}: active-low reset role '{role}' is wired directly to ground; \
                         firmware co-simulation cannot run with this reset binding. \
                         Check the reset wiring/pin model (reset-disable fuses are not evaluated); \
                         firmware-less board analysis is still available.",
                        binding.reference
                    );
                }
            }
        }
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
                for node in direct_supply_nodes(binding) {
                    supply_watches.push(hauksbee_bind::stress::SupplyWatch {
                        reference: binding.reference.clone(),
                        node,
                        max_v,
                        stand_in: binding.supply_stand_in.clone(),
                    });
                }
            }
        }

        // Every routed pin driver, MCU, logic part or DAC, whether or not a core
        // runs behind it (a firmware-less run leaves the MCU's drivers disabled
        // but a caller may still enable one).
        let mut driver_draws: Vec<_> = (digital.iter().flat_map(|d| d.drivers.values()))
            .chain(mcus.iter().flat_map(|m| m.gpio_drivers.values()))
            .chain(dacs.iter().flat_map(|d| d.vout_drivers.iter().flatten()))
            .filter_map(|d| Some((d.vsource, d.supply?)))
            .collect();
        driver_draws.sort_unstable_by_key(|(source, _)| *source);
        let mut live = Vec::new();
        let mut substitutions = Vec::new();
        let mut scoped_substitutions = Vec::new();
        let mut clock_fuse_findings = Vec::new();
        let mut clock_assumptions = Vec::new();
        let mut idle_mcus = Vec::new();
        // What each chip's boot straps select, read off the board's start-up
        // DC point (empty for a firmware-less run and for cards without a
        // boot table).
        let mut boot_plan = if firmware.is_some() {
            crate::boot::plan_boots(&mcus, &mcu_boot, &circuit, &opts)
        } else {
            HashMap::new()
        };
        // Alternate footprints of one processor get one core between them
        // (two cores running one firmware would fight on every shared pin).
        let alternates = firmware.map_or_else(AlternateFootprints::default, |fw| {
            alternate_footprints(&mcus, &circuit, fw)
        });
        for note in &alternates.notes {
            eprintln!("WARNING: {note}");
        }
        for binding in mcus {
            let evidence_subject = mcu_subjects
                .next()
                .unwrap_or_else(|| binding.reference.trim().to_string());
            if alternates.dropped.contains(&binding.reference) {
                continue;
            }
            // A firmware-less run is a board analysis, not a co-simulation.
            // Leave every MCU backend out of the live scheduler (including
            // in-process AVR), so a board can be analysed as a passive circuit
            // even when this binary was built without the optional AVR
            // backend. Firmware-backed runs still instantiate the selected
            // backend below and retain their fail-closed feature checks.
            if firmware.is_none() {
                let gpio_nodes = binding
                    .role_nets
                    .iter()
                    .filter(|(role, _)| {
                        gpio_of_role(role, binding.module)
                            .or_else(|| apin_gpio_of_role(role, binding.module))
                            .is_some()
                    })
                    .map(|(_, &node)| node)
                    .collect();
                idle_mcus.push(IdleMcu {
                    reference: binding.reference.clone(),
                    gpio_nodes,
                });
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
            let clock = avr_clock_plan(&binding, firmware);
            if let Some(finding) = clock.finding.clone() {
                eprintln!("WARNING: {}", finding.message);
                clock_fuse_findings.push(finding);
            }
            if let Some(guess) = clock.guess.clone().filter(|_| clock.frequency_hz.is_some()) {
                eprintln!("WARNING: {guess}");
                clock_assumptions.push(guess);
            }
            let boot = boot_plan.remove(&binding.reference).and_then(|resolved| {
                mcu_boot
                    .get(&binding.reference)
                    .map(|spec| (spec.clone(), resolved))
            });
            let (core, boot_state) = match boot {
                None => (instantiate_mcu(&binding, firmware, &clock)?, None),
                Some((spec, resolved)) => {
                    // Owned, because a chip held in reset builds its core later.
                    let backend = binding.backend.clone();
                    let reference = binding.reference.clone();
                    let requested_part = binding.requested_part.clone();
                    let external_clock = binding.clock.external_present();
                    let firmware_path = firmware.map(std::path::Path::to_path_buf);
                    let clock = clock.clone();
                    let (core, state) = crate::boot::build_core(&spec, &resolved, move |strap| {
                        instantiate_core(
                            &backend,
                            (&reference, &requested_part),
                            external_clock,
                            firmware_path.as_deref(),
                            &clock,
                            strap.as_ref(),
                        )
                    })?;
                    (core, Some(state))
                }
            };
            let mut live_mcu = core_with_hooks(core, binding);
            live_mcu.boot = boot_state;
            live.push(live_mcu);
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

        let mut stress = StressMonitor::new(device_meta);
        stress.set_supply_watches(supply_watches);
        for r in &report.regulators {
            stress.watch_regulator_heat(r.source, RegulatorHeat::Linear { input: r.input });
        }
        for &(source, input) in &report.linear_heat {
            stress.watch_regulator_heat(source, RegulatorHeat::Linear { input });
        }
        for &(source, input, port) in &report.port_heat {
            stress.watch_regulator_heat(source, RegulatorHeat::LinearWithPort { input, port });
        }
        for (source, heat) in behavioral
            .iter()
            .filter_map(BehavioralDevice::converter_heat)
        {
            stress.watch_regulator_heat(source, heat);
        }
        let n_nodes = circuit.node_count();
        let layout = Layout::new(&circuit);
        let n_branch = layout.size.saturating_sub(layout.n_nodes);
        // Snapshot the pristine (post-bind, pre-run) circuit for destructive-mode
        // replay restoration in `reset_run_state`.
        let original_circuit = circuit.clone();
        let mut sched = Self {
            circuit,
            original_circuit,
            net_nodes,
            digital,
            chains,
            chain_mcu,
            chain_chips,
            mcus: live,
            node_volts: vec![0.0; n_nodes],
            reg_supported: vec![false; report.regulators.len()],
            regulators: report.regulators,
            driver_draws,
            open_parts: report.open_parts,
            reg_dirty: true,
            regulation: Vec::new(),
            branch_x: vec![0.0; n_branch],
            last_dc_seed: None,
            last_carry: None,
            layout,
            supplies,
            behavioral,
            stress,
            faults_pending: Vec::new(),
            chunk_s: DEFAULT_CHUNK_S,
            stop_when_dead: false,
            opts,
            sim_time: 0.0,
            powered_from_s: 0.0,
            micros_carry: 0.0,
            stats: HashMap::new(),
            firmware_edge_toggles: HashMap::new(),
            observer: crate::observe::Observer::default(),
            firmware_symbols: firmware.and_then(crate::observe::symbols::SymbolTable::from_elf),
            peripherals: PeripheralSet::new(),
            i2c_buses: Vec::new(),
            spi_buses: Vec::new(),
            hw_i2c_buses: Vec::new(),
            hw_spi_buses: Vec::new(),
            model_peripheral_power: Vec::new(),
            spi_controller_map: HashMap::new(),
            substitutions,
            scoped_substitutions,
            clock_fuse_findings,
            clock_assumptions,
            unexercised_buses: Vec::new(),
            idle_mcus,
            alternate_footprint_notes: alternates.notes,
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
            operating_point: None,
            last_switch_latch: None,
            latching: bound_latching,
            consecutive_failed_chunks: 0,
            max_consecutive_failed_chunks: 0,
            analog_ever_converged: false,
            initial_rail_view: None,
            initial_rail_view_tried: false,
            dead_from_start_noted: false,
            last_solve_error: None,
            reset_failure: None,
            replay_chips,
            replay_pin_nets,
            last_chunk_edges: Vec::new(),
            last_replay_microticks: 0,
            digital_in_evidence: HashMap::new(),
            frame_peak_current: HashMap::new(),
            frame_v_extremes: HashMap::new(),
            step_watch: Vec::new(),
            step_series: HashMap::new(),
            tick_sequential_nets: HashMap::new(),
            short_pulses: Vec::new(),
            short_pulse_nets: std::collections::HashSet::new(),
            timing_refusals: Vec::new(),
            timing_refusal_nets: std::collections::HashSet::new(),
            execution: Vec::new(),
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
        sched.drop_undriven_rail_nodes();

        // Index the nets that clock TICK-evaluated sequential parts, for the
        // sub-chunk pulse warning. Built after the 595 chains,
        // replay chips, and 165 read chains above, because those edge-exact
        // paths are exactly what the index must EXCLUDE.
        sched.rebuild_tick_sequential_nets();

        Ok(sched)
    }

    /// Digital parts that see every MCU edge in cycle order: 595 chain chips,
    /// generalized replay chips, parallel memories and 165 read-chain chips.
    /// Every other part is evaluated once per chunk.
    fn edge_exact_chips(&self) -> std::collections::HashSet<usize> {
        let mut edge_exact: std::collections::HashSet<usize> =
            self.chain_chips.iter().copied().collect();
        edge_exact.extend(self.replay_chips.iter().copied());
        edge_exact.extend(self.parallel_memory_chips.iter().copied());
        for c in &self.hc165_chains {
            edge_exact.extend(c.lock_or_recover().order.iter().copied());
        }
        edge_exact
    }

    /// References of the digital parts evaluated once per chunk.
    fn chunk_sampled_logic(&self) -> Vec<&str> {
        let edge_exact = self.edge_exact_chips();
        (0..self.digital.len())
            .filter(|i| !edge_exact.contains(i))
            .map(|i| self.digital[i].reference.as_str())
            .collect()
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
        let edge_exact = self.edge_exact_chips();
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
        let bindings: Vec<&McuBinding> = self.mcus.iter().map(|m| &m.binding).collect();
        self.digital_in_evidence = drive_evidence_for(&self.circuit, &bindings);
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
        /// is the server crate's bundled assets/sensor-specs/mcp4728.toml,
        /// guarded by `tests/packaged_asset_sync.rs` and refreshed by
        /// scripts/sync-crate-assets.sh.
        const MCP4728_SPEC: &str = include_str!("../assets/sensor-specs/mcp4728.toml");

        let mut bus = I2cBus::new("MCP4728_BUS");
        for d in dacs {
            bus.add_signal_nodes(d.bus_nodes.iter().copied());
            let mut slave = RegisterMapSensor::from_toml(MCP4728_SPEC)
                .expect("shipped mcp4728.toml spec must validate");
            slave.set_i2c_address(d.address);
            for ch in 0..4 {
                slave.set_channel_state("vref", ch, d.vref);
                slave.set_channel_state("gain", ch, f64::from(d.gain));
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
        let mut ctx = TickCtx::new(&mut self.circuit, &volts, self.sim_time, self.chunk_s);
        bus.lock_or_recover().drive_all(&mut ctx);
    }

    /// Instantiate I2C/SPI slaves declared by exact resolved model cards.
    fn attach_model_peripherals(
        &mut self,
        peripherals: Vec<hauksbee_bind::binder::PeripheralBinding>,
    ) {
        for peripheral in peripherals {
            let reference = peripheral.reference;
            let power = peripheral.power.as_ref();
            let cs_net = peripheral.cs_net;
            let bus_nodes = peripheral.bus_nodes.clone();
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
                    let mut bus = I2cBus::new(&reference).with_slave(Box::new(slave));
                    bus.add_signal_nodes(bus_nodes);
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
                    let mut bus = SpiBus::new(&reference, Box::new(flash));
                    bus.add_signal_nodes(bus_nodes);
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
                            let mut bus = I2cBus::new(&reference).with_slave(Box::new(sensor));
                            bus.add_signal_nodes(bus_nodes);
                            self.attach_powered_i2c(&reference, power, bus);
                        }
                        Bus::Spi => {
                            let mut bus = SpiBus::new(&reference, Box::new(sensor));
                            bus.add_signal_nodes(bus_nodes);
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
        power: Option<&hauksbee_bind::binder::BoundPeripheralPower>,
        bus: I2cBus,
    ) {
        let bus = Arc::new(Mutex::new(bus));
        if power.is_some() {
            bus.lock_or_recover().set_powered(false);
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
        power: Option<&hauksbee_bind::binder::BoundPeripheralPower>,
        bus: SpiBus,
        cs_net: Option<NodeId>,
        controller: Option<&str>,
    ) {
        let bus = Arc::new(Mutex::new(bus));
        if power.is_some() {
            bus.lock_or_recover().set_powered(false);
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
        power: &hauksbee_bind::binder::BoundPeripheralPower,
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
            let supply_v = leg.supply_v(&self.node_volts);
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
                ModelPeripheralBus::I2c(bus) => (bus.lock_or_recover().take_activity(), false),
                ModelPeripheralBus::Spi(bus) => {
                    let mut bus = bus.lock_or_recover();
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
                cb.lock_or_recover()
                    .dispatch_batch_at(
                        &edges
                            .iter()
                            .map(|&(pin, high)| ((pin.port, pin.bit), high))
                            .collect::<Vec<_>>(),
                        cycle,
                    )
                    .into_iter()
                    .map(pin_drive)
                    .collect()
            },
        ));
        let direction_registry = reg.clone();
        self.mcus[mi].core.on_input_responder_direction(Box::new(
            move |changes: &[(PinId, bool, bool)], cycle: u64| {
                direction_registry
                    .lock_or_recover()
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
                    .map(pin_drive)
                    .collect()
            },
        ));
        self.responder_registries[mi] = Some(reg.clone());
        reg
    }

    /// Build the edge-driven 74HC165 read chains (one per physical chain whose
    /// PL / CLK / QH→MISO pins bind to an MCU's GPIO) and register each with
    /// that MCU's input-responder registry. The responder fires on every PL /
    /// SCLK edge during the MCU's run: the chain samples the pulse-latch inputs
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
        let gpio_maps = gpio_net_maps(&self.mcus);

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
                registry.lock_or_recover().register(Box::new(responder));
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
                .lock_or_recover()
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
    #[allow(clippy::too_many_lines)] // one conservative classification pass
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // gpio keys are widened u32 ids
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
            .map(|(&(port, bit), drv)| (i64::from(drv.net.0), (port, bit)))
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
        let levels = self.digital[component]
            .levels
            .at(&|n| fixed_volts.get(&n).copied().unwrap_or(0.0));
        let mut pull_candidates: HashMap<NodeId, Vec<(DeviceId, bool)>> = HashMap::new();
        for (index, device) in (0u32..).zip(&self.circuit.devices) {
            if permitted_driver_resistors.contains(&DeviceId(index)) {
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
                    .push((DeviceId(index), level));
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
        let externally_coupled_nodes: std::collections::HashSet<NodeId> = (0u32..)
            .zip(&self.circuit.devices)
            .filter(|(index, _)| {
                let id = DeviceId(*index);
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
            if let Some(&pin) = gpio.get(&i64::from(node.0)) {
                return Some(ParallelSignal::Mcu(pin));
            }
            for (chain_i, chain) in chains.iter().enumerate() {
                for (chip_i, &digital_i) in chain.order.iter().enumerate() {
                    for (bit, role) in (0u8..).zip(["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"])
                    {
                        if self.digital[digital_i].roles.get(role) == Some(&node) {
                            return Some(ParallelSignal::Hc595 {
                                chain: chain_i,
                                chip: chip_i,
                                bit,
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
                gpio.get(&i64::from(node.0)).copied()
            })
            .collect::<Option<Vec<_>>>()?;

        let runtime = Arc::new(Mutex::new(ParallelMemoryRuntime::default()));
        let responder = ParallelMemoryResponder::new(
            format!("{}.{}", self.digital[component].reference, port.name),
            port.clone(),
            self.digital[component].levels.clone(),
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
            let runtime = binding.runtime.lock_or_recover();
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
    #[must_use]
    pub fn substitutions(&self) -> &[McuSubstitution] {
        &self.substitutions
    }

    /// Substitution events paired with their exact board-occurrence evidence.
    #[must_use]
    pub fn scoped_substitutions(&self) -> &[ScopedMcuSubstitution] {
        &self.scoped_substitutions
    }

    /// AVR fuse/board clock inconsistencies found when the cores were built.
    #[must_use]
    pub fn clock_fuse_findings(&self) -> &[AvrClockFinding] {
        &self.clock_fuse_findings
    }

    /// The AVR clocks this run guessed (see [`AvrClockPlan::guess`]). Empty when
    /// every AVR's clock came from its fuses or a crystal the board states.
    #[must_use]
    pub fn clock_assumptions(&self) -> &[String] {
        &self.clock_assumptions
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
    #[must_use]
    pub fn drive_conflicts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in hauksbee_solve::blame::source_conflicts(&self.circuit) {
            let settled = self.node_volts.get(c.node.0 as usize).copied();
            out.push(c.describe(settled));
        }
        for (&node, &(hi, lo, t0, t1)) in &self.forced_node_volts {
            let stamped = hauksbee_solve::blame::sources_on_node(&self.circuit, node);
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
                .find(|(_, id)| **id == node)
                .map_or_else(|| format!("node #{}", node.0), |(n, _)| n.clone());
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
                    .map_or_else(|| format!("node {}", node.0), |s| (*s).clone());
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
    #[must_use]
    pub fn uart_rx_overflow(&self) -> Vec<(String, u64)> {
        self.per_mcu(|core| Some(core.uart_rx_overflow()).filter(|&n| n > 0))
    }

    /// Host serial bytes accepted by a backend but not yet presented to its
    /// emulated UART. A non-empty result at session teardown is undelivered
    /// input, even when the backend's overflow count is zero.
    #[must_use]
    pub fn uart_rx_pending(&self) -> Vec<(String, usize)> {
        self.per_mcu(|core| Some(core.uart_rx_pending()).filter(|&n| n > 0))
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
    #[must_use]
    pub fn watchdog_limitations(&self) -> Vec<(String, String)> {
        self.per_mcu(|core| core.watchdog_limitation())
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
    #[must_use]
    pub fn timing_limitations(&self) -> Vec<(String, String)> {
        self.per_mcu(|core| core.timing_limitation())
    }

    /// References of the live MCUs whose simulated time is credited from host
    /// wall-clock spans ([`Mcu::clock_host_paced`]), ordered by reference. A
    /// firmware time on one of these depends on the machine that ran it, so a
    /// caller judging such a time against a deadline must not issue a hard
    /// verdict. Empty when every core counts its own time (simavr, Renode).
    #[must_use]
    pub fn host_paced_mcus(&self) -> Vec<String> {
        self.per_mcu(|core| core.clock_host_paced().then_some(()))
            .into_iter()
            .map(|(reference, ())| reference)
            .collect()
    }

    /// Times an unserviced watchdog rebooted each MCU's core during this run,
    /// filtered to the nonzero. Not an error, a FINDING: an assertion that
    /// passed across a reboot was not measuring the run it claimed, because the
    /// behaviour it observed belongs to a rebooted core. Read together with
    /// [`Scheduler::watchdog_limitations`], since a backend that cannot reboot
    /// at all reports zero here and says so there. Ordered by MCU reference.
    #[must_use]
    pub fn watchdog_resets(&self) -> Vec<(String, u64)> {
        self.per_mcu(|core| Some(core.watchdog_resets()).filter(|&n| n > 0))
    }

    /// Every stretch of time an MCU was held in reset (reset pin low, supply
    /// below its brown-out threshold, or the supply still rising at power-up),
    /// by MCU reference then time. A finding, not an error: a run that crossed
    /// one measured a rebooted core.
    #[must_use]
    pub fn reset_episodes(&self) -> Vec<(String, ResetEpisode)> {
        let mut out: Vec<(String, ResetEpisode)> = self
            .mcus
            .iter()
            .flat_map(|m| {
                m.reset_gate
                    .episodes
                    .iter()
                    .map(|e| (m.binding.reference.clone(), e.clone()))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start_s.total_cmp(&b.1.start_s)));
        out
    }

    /// `(MCU reference, value)` for every live MCU whose core yields a value,
    /// ordered by MCU reference: the shape every per-MCU finding above shares.
    fn per_mcu<T: Ord>(&self, value: impl Fn(&dyn Mcu) -> Option<T>) -> Vec<(String, T)> {
        let mut out: Vec<(String, T)> = self
            .mcus
            .iter()
            .filter_map(|m| value(m.core.as_ref()).map(|v| (m.binding.reference.clone(), v)))
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
    #[must_use]
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
            let g = bus.lock_or_recover();
            g.id().to_string()
        };
        self.record_bus_if_unexercised(&id, "I2C", None);
        self.i2c_buses.push(bus.clone());
        self.hw_i2c_buses.push(bus);
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
            .flat_map(|b| b.lock_or_recover().addresses())
            .collect();
        for m in &mut self.mcus {
            m.core.set_i2c_slave_addresses(&addresses);
            let buses = all.clone();
            let events = m.i2c_events.clone();
            m.core.on_i2c(Box::new(move |ev| {
                events.fetch_add(1, Ordering::Relaxed);
                let addr = match ev {
                    hauksbee_mcu::I2cEvent::Start { addr, .. }
                    | hauksbee_mcu::I2cEvent::Write { addr, .. }
                    | hauksbee_mcu::I2cEvent::Read { addr }
                    | hauksbee_mcu::I2cEvent::Stop { addr } => addr,
                };
                for b in &buses {
                    let mut bus = b.lock_or_recover();
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
        let id = bus.lock_or_recover().id().to_string();
        self.record_bus_if_unexercised(&id, "SPI", None);
        bus.lock_or_recover().set_cs_pin(
            cs.map(|c| c.pin),
            cs.map_or(CsProvenance::SpecDeclared, |c| c.provenance),
        );
        self.register_cs_frame(&bus, cs.map(|c| c.pin), cs.and_then(|c| c.net));
        self.spi_buses.push(bus.clone());
        self.hw_spi_buses.push(bus);
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
            let events = m.spi_events.clone();
            m.core.on_spi(Box::new(move |ev| {
                events.fetch_add(1, Ordering::Relaxed);
                dispatch_spi(&buses, &ev)
            }));
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
        let id = bus.lock_or_recover().id().to_string();
        self.record_bus_if_unexercised(&id, "SPI", Some(controller));
        bus.lock_or_recover().set_cs_pin(
            cs.map(|c| c.pin),
            cs.map_or(CsProvenance::SpecDeclared, |c| c.provenance),
        );
        for m in &mut self.mcus {
            let b = bus.clone();
            let ctrl = controller.to_string();
            let events = m.spi_events.clone();
            m.core.on_spi_controller(
                &ctrl,
                Box::new(move |ev| {
                    events.fetch_add(1, Ordering::Relaxed);
                    let mut guard = b.lock_or_recover();
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
        self.spi_buses.push(bus.clone());
        self.hw_spi_buses.push(bus);
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
        &self,
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
                .is_some_and(|drv| cs_net.is_none_or(|node| drv.net == node))
        });
        if let Some(m) = owner {
            let mut sh = m.shared.lock_or_recover();
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
    ///
    /// # Errors
    ///
    /// Returns an error if no live MCU drives the SCLK pin, or if any of the other
    /// pins is not a wired GPIO of that same MCU.
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
                    bus.lock_or_recover().id(),
                    pins.sclk
                )
            })?;
        for (name, pin) in [("MOSI", pins.mosi), ("MISO", pins.miso), ("CS", pins.cs_n)] {
            if !self.mcus[mi].binding.gpio_drivers.contains_key(&pin) {
                anyhow::bail!(
                    "bit-banged SPI '{}': {name} pin {:?} is not a wired GPIO of the MCU \
                     that drives SCLK ({})",
                    bus.lock_or_recover().id(),
                    pin,
                    self.mcus[mi].binding.reference,
                );
            }
        }
        // This CS came from the bit-bang wiring, not from a spec `cs_net` and not
        // from a model pad map, and says so.
        bus.lock_or_recover().set_cs_pin(
            Some(pins.cs_n),
            crate::peripherals::CsProvenance::BitBangPins,
        );
        let nets = [pins.sclk, pins.mosi, pins.miso, pins.cs_n]
            .map(|pin| self.mcus[mi].binding.gpio_drivers[&pin].net);
        bus.lock_or_recover().add_signal_nodes(nets);
        let responder = crate::responders::BitBangSpiResponder::new(bus.clone(), pins);
        self.responder_registry(mi)
            .lock_or_recover()
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
    ///
    /// # Errors
    ///
    /// Returns an error if no live MCU drives the SCL pin, or if the SDA pin is not
    /// a wired GPIO of that same MCU.
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
        let nets = [scl, sda].map(|pin| self.mcus[mi].binding.gpio_drivers[&pin].net);
        bus.lock_or_recover().add_signal_nodes(nets);
        let responder = crate::responders::SoftI2cResponder::new(bus.clone(), scl, sda);
        self.responder_registry(mi)
            .lock_or_recover()
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
    #[must_use]
    pub fn mcu_pin_for_net(&self, net: &str) -> Option<(char, u8)> {
        let node = *self.net_nodes.get(net)?;
        self.pin_driving_node(node)
    }

    /// Trace a net back to the MCU pin that drives it: the (port, bit) of the
    /// GPIO driver whose net is `node`, if any MCU drives it. This is the CS-net
    /// resolution the binder uses to populate `cs_pin`, and the same
    /// net-to-driving-pin trace the 74HC595 chain wiring performs to find its
    /// SRCLK/RCLK/SER pins.
    #[must_use]
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
    #[must_use]
    pub fn spi_framing_modes(&self) -> Vec<(String, SpiFramingMode)> {
        self.spi_buses
            .iter()
            .map(|b| {
                let g = b.lock_or_recover();
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
    #[must_use]
    pub fn spi_bus_for_controller(&self, controller: &str) -> Option<&Arc<Mutex<SpiBus>>> {
        self.spi_controller_map.get(controller)
    }

    /// Mutable access to the circuit so a caller can stamp a control's devices
    /// before attaching it. Call [`Scheduler::attach_peripheral`] afterwards,
    /// which relayouts the MNA system to pick up any new nodes/devices.
    pub const fn circuit_mut(&mut self) -> &mut Circuit {
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
    #[must_use]
    pub fn i2c_buses(&self) -> &[Arc<Mutex<I2cBus>>] {
        &self.i2c_buses
    }

    /// Borrow the attached SPI buses.
    #[must_use]
    pub fn spi_buses(&self) -> &[Arc<Mutex<SpiBus>>] {
        &self.spi_buses
    }

    /// Apply a live peripheral command (websocket `SetInput` onto a peripheral).
    /// Returns true if a peripheral with that id existed.
    pub fn set_peripheral(&mut self, id: &str, value: f64) -> bool {
        self.peripherals.set_value(id, value)
    }

    /// (id, kind) of every attached peripheral and bus, for `board_info`.
    #[must_use]
    pub fn peripheral_infos(&self) -> Vec<(String, String)> {
        use crate::peripherals::Peripheral as _;
        let mut out: Vec<(String, String)> = self
            .peripherals
            .peripherals
            .iter()
            .map(|p| (p.id().to_string(), p.kind().to_string()))
            .collect();
        for bus in &self.i2c_buses {
            let b = bus.lock_or_recover();
            out.push((b.id().to_string(), b.kind().to_string()));
        }
        for bus in &self.spi_buses {
            let b = bus.lock_or_recover();
            out.push((b.id().to_string(), b.kind().to_string()));
        }
        out
    }

    /// Peripheral state map, keyed by id, for component-state frames.
    #[must_use]
    pub fn peripheral_states(&self) -> HashMap<String, HashMap<String, f64>> {
        use crate::peripherals::Peripheral as _;
        let mut m = self.peripherals.states();
        for bus in &self.i2c_buses {
            let b = bus.lock_or_recover();
            m.insert(b.id().to_string(), b.state());
        }
        for bus in &self.spi_buses {
            let b = bus.lock_or_recover();
            m.insert(b.id().to_string(), b.state());
        }
        for leg in &self.model_peripheral_power {
            let supply_v = leg.supply_v(&self.node_volts);
            let fields = m.entry(leg.reference.clone()).or_default();
            fields.insert("supply_v".into(), supply_v);
            fields.insert("supply_current_a".into(), leg.last_current_a);
            fields.insert("power_on_threshold_v".into(), leg.power_on_threshold_v);
        }
        m
    }

    /// Whether the board carries any processor the run bound: a live core, or
    /// one left idle because no firmware was given. A board with none has no
    /// firmware to run, so nothing about firmware execution is worth saying.
    #[must_use]
    pub const fn has_processor(&self) -> bool {
        !self.mcus.is_empty() || !self.idle_mcus.is_empty()
    }

    /// Number of live MCU cores.
    #[must_use]
    pub const fn mcu_count(&self) -> usize {
        self.mcus.len()
    }

    /// References of the MCUs the board carries that never ran because the run
    /// was given no firmware.
    #[must_use]
    pub fn idle_mcu_refs(&self) -> Vec<String> {
        self.idle_mcus.iter().map(|m| m.reference.clone()).collect()
    }

    /// One sentence per MCU footprint that got no processor because it is an
    /// alternate footprint of another one the run kept (empty when the board
    /// has none).
    #[must_use]
    pub fn alternate_footprint_notes(&self) -> &[String] {
        &self.alternate_footprint_notes
    }

    /// Reference strings of the live MCUs (for serial routing).
    #[must_use]
    pub fn mcu_refs(&self) -> Vec<String> {
        self.mcus
            .iter()
            .map(|m| m.binding.reference.clone())
            .collect()
    }

    /// `(reference, backend, requested_part)` for each live MCU, in board order.
    /// The co-sim summary reads this to report what part the board
    /// asked for alongside the backend that actually ran it.
    #[must_use]
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
    #[must_use]
    pub fn has_external_backend(&self) -> bool {
        self.mcus
            .iter()
            .any(|m| backend_is_external(&m.binding.backend))
    }

    /// Report the timing accuracy actually available at the current chunk.
    /// Values come from the live core's frequency/callback tier and the chunk
    /// the scheduler will really execute; no part-family limits are guessed.
    /// Chunk-sampled logic gets one row: its outputs follow inputs sampled at
    /// the previous chunk boundary, so an edge lands up to one chunk late and
    /// the datasheet propagation delay is not applied.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // clock frequencies are far below 2^52 Hz
    pub fn timing_coverage(&self) -> Vec<TimingCoverage> {
        let logic = self.chunk_sampled_logic();
        let logic_row = (!logic.is_empty()).then(|| TimingCoverage {
            mcu_ref: logic.join(", "),
            backend: CHUNK_SAMPLED_LOGIC.to_string(),
            cycle_exact: false,
            timestamp_precision_s: self.chunk_s,
            minimum_guaranteed_pulse_s: 2.0 * self.chunk_s,
            chunk_s: self.chunk_s,
        });
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
            .chain(logic_row)
            .collect()
    }

    /// Per-net toggle counts suitable for assertions and reports.
    ///
    /// Firmware-driven nets use at least the ordered MCU edge count, preserving
    /// sub-chunk pulses. Analog-only nets retain the settled-voltage count. The
    /// maximum avoids double-counting an edge also seen at a chunk boundary.
    #[must_use]
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
    #[must_use]
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
    ///
    /// # Errors
    ///
    /// Returns an error if a requested bound is not positive and finite, or if it
    /// is below the one-cycle resolution of a cycle-exact core, or below what a poll
    /// backend's integer-microsecond slice can resolve.
    #[allow(clippy::cast_precision_loss)] // clock frequencies are far below 2^52 Hz
    pub fn configure_timing(&mut self, req: TimingRequirement) -> anyhow::Result<()> {
        let limits = [
            ("minimum pulse", req.min_pulse_s),
            ("maximum edge error", req.max_edge_error_s),
        ];
        for (label, value) in limits {
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
        for m in self.mcus.iter().filter(|m| m.core.cycle_exact()) {
            let cycle_s = 1.0 / m.core.frequency().max(1) as f64;
            for (label, value) in limits {
                let Some(v) = value else { continue };
                anyhow::ensure!(
                    v + f64::EPSILON >= cycle_s,
                    "timing request refused for {} ({}): {label} {:.3} us is below its measured one-cycle resolution {:.3} us",
                    m.binding.reference,
                    m.binding.backend,
                    v * 1e6,
                    cycle_s * 1e6,
                );
            }
        }

        let polled = self.mcus.iter().any(|m| !m.core.cycle_exact());
        if polled || !self.chunk_sampled_logic().is_empty() {
            let mut requested_chunk = self.chunk_s;
            if let Some(pulse) = req.min_pulse_s {
                requested_chunk = requested_chunk.min(pulse / 2.0);
            }
            if let Some(error) = req.max_edge_error_s {
                requested_chunk = requested_chunk.min(error);
            }
            anyhow::ensure!(
                !polled || requested_chunk + f64::EPSILON >= POLL_BACKEND_QUANTUM_S,
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
    /// hooks), and any Renode core whose system-on-chip descriptor carries a verified
    /// direction-register map for each polled port. Conservative AND across
    /// cores: one direction-blind MCU makes the whole run's configured-output
    /// picture untrustworthy, so a boot-state check must then hedge ("undriven
    /// OR held LOW") rather than assert Hi-Z. Vacuously true with no MCUs
    /// (there is no pin whose direction could be misread), matching the old
    /// `!has_external_backend()` proxy this replaces.
    #[must_use]
    pub fn drive_direction_observable(&self) -> bool {
        self.mcus
            .iter()
            .all(|m| m.core.drive_direction_observable())
    }

    /// Advance the co-sim by `dt` seconds in fixed chunks. After a failed reset,
    /// returns the unchanged clock without a sample; consult `reset_failure`.
    /// Retained diagnostics still describe the old run, not a new observation.
    #[allow(clippy::cast_precision_loss)] // the chunk count is a small whole number
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // ceil of a positive ratio
    pub fn step(&mut self, dt: f64) -> StepResult {
        let mut uart: HashMap<String, Vec<u8>> = HashMap::new();
        if self.reset_failure.is_some() {
            return StepResult {
                sim_time: self.sim_time,
                uart,
            };
        }
        // `chunk_s` is a maximum cadence, not a rounded target: strict timing
        // negotiation may have chosen it as the largest acceptable poll error.
        // Ceiling guarantees the actual `dt / chunks` slice never exceeds it.
        let chunks = ((dt / self.chunk_s).ceil() as u64).max(1);
        let chunk = dt / chunks as f64;

        // Reset the per-frame extreme accumulators; run_chunk folds each chunk's
        // settled operating point in, so the runner reads true intra-frame peaks.
        self.frame_peak_current.clear();
        self.frame_v_extremes.clear();

        for _ in 0..chunks {
            if self.dead_from_start() {
                if self.stop_when_dead {
                    break;
                }
                // The analog side has never solved: every chunk so far
                // re-solved the same cold start and was refused. Marching on
                // only regrinds that refusal (a full rescue ladder per chunk),
                // so record the rest of the span as unsolved and advance.
                self.skip_dead_chunk(chunk);
                continue;
            }
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

    #[allow(clippy::too_many_lines)] // one chunk's ordered MCU and analog phases
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // micros is floored, >= 0
    fn run_chunk(&mut self, chunk: f64, uart: &mut HashMap<String, Vec<u8>>) {
        self.settle_converters();
        self.settle_pass_draws();
        // Before any MCU advances: firmware must never run unflagged on a rail
        // whose regulation is unsupported.
        self.establish_regulation();
        self.gate_model_peripherals_from_rails();

        // Every digital part, chain-owned and edge-replayed ones included,
        // re-reads its supply BEFORE anything drives an output this chunk (MCU
        // edge replay, tick, chain apply, parallel memory), so no path drives a
        // level read at a stale or never-resolved rail.
        {
            let volts = self.digital_view_volts();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for d in &mut self.digital {
                d.refresh_levels(&node_v);
            }
        }

        // Integer microseconds for `run_micros`, banking the sub-microsecond
        // remainder across chunks so the firmware clock does not drift from sim
        // time. The floored value is deliberately NOT clamped up to 1: a
        // persistent sub-1 µs chunk (a fine `fixed_dt = 0.5e-6`) never reaches a
        // whole banked microsecond, so a `.max(1.0)` would deliver 1 µs every
        // chunk while banking unrepayable negative debt and racing the firmware
        // clock ahead of sim time. Instead the core advances 0 µs on a sub-µs
        // chunk and rolls forward once the banked fraction accrues a full
        // microsecond, keeping `micros_carry` in [0, 1).
        let exact = chunk.mul_add(1e6, self.micros_carry);
        let micros_f = exact.floor();
        self.micros_carry = exact - micros_f;
        let micros = micros_f as u64;
        // Set when any MCU refuses to advance this chunk; folded into the
        // chunk-failure accounting after the analog solve so the run refuses to
        // report a fake-quiet chunk.
        let mut mcu_run_failed = false;
        // The first backend error of this chunk, so the failed-chunk record
        // names what the emulator reported, not only that it failed.
        let mut mcu_run_error: Option<String> = None;

        // Refresh the snapshot the edge-driven 74HC165 read chains sample on a
        // PL load (it fires inside the MCU run below, so it must reflect the
        // PREVIOUS chunk's settled pulse-latch voltages).
        if !self.hc165_chains.is_empty() {
            let mut snap = self.input_volts.lock_or_recover();
            snap.clear();
            snap.extend_from_slice(&self.node_volts);
        }

        // Per-chunk accessor state, rebuilt as each MCU drains below.
        self.last_chunk_edges.clear();
        self.last_replay_microticks = 0;

        // The bench recorder stamps each bus transaction with the chunk it
        // began in; tell every bridge what time this chunk starts at.
        let chunk_t0 = self.sim_time;
        for bus in &self.i2c_buses {
            bus.lock_or_recover().set_observed_time(chunk_t0);
        }
        for bus in &self.spi_buses {
            bus.lock_or_recover().set_observed_time(chunk_t0);
        }

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

        // Execution evidence: the board's reset line and supply pads as the
        // previous chunk solved them.
        for m in &mut self.mcus {
            m.rails.observe(&self.node_volts);
        }

        // Voltages the reset gate reads (the t = 0 DC solve before any chunk
        // has solved); only taken when some part states reset thresholds.
        let gate_volts = if self
            .mcus
            .iter()
            .any(|m| m.binding.electrical.reset.is_some())
        {
            self.digital_view_volts()
        } else {
            Vec::new()
        };

        // 1. MCU: inject latest ADC voltages, run the chunk, drain captures.
        for mi in 0..self.mcus.len() {
            self.inject_mcu_inputs(mi, &mcu_driven_nets);
            let m = &mut self.mcus[mi];
            // Held in reset (reset pin low / brown-out): the core does not
            // advance and its pins float; on release it is rebooted.
            let held = m.reset_gate_hold(&mut self.circuit, self.sim_time, &gate_volts);
            // Cycle counter bracketing this run: the chunk's [start, end) span,
            // so the drained edge stamps normalize to a fraction of the chunk
            // for the analog PWL side. Exact on simavr, coarse on poll backends
            // (flagged by `cycle_exact`).
            let cyc_start = m.core.current_cycle();
            // Bench recorder: the reset line as the previous chunk solved it.
            let reset_v = m
                .rails
                .reset_nodes
                .iter()
                .filter_map(|&n| self.node_volts.get(n).copied())
                .filter(|v| v.is_finite())
                .reduce(f64::min);
            self.observer.ensure_mcu(mi, &m.binding.reference);
            self.observer
                .record_reset_level(mi, chunk_t0, reset_v, m.logic_high_v);
            // Boot straps: a chip held in reset does not run; its core waits, and the
            // straps are read again when reset releases.
            let boot_held = m.boot.as_mut().is_some_and(|b| {
                let name = |n: NodeId| self.circuit.node_name(n).to_string();
                let driven = |n: NodeId| {
                    net_has_drive_evidence(&self.digital_in_evidence, &self.circuit.devices, n.0)
                };
                let snap = crate::boot::BoardSnapshot {
                    role_nets: &m.binding.role_nets,
                    volts: &self.node_volts,
                    vdd: m.logic_high_v,
                    net_name: &name,
                    driven: &driven,
                };
                b.hold_core(
                    &snap,
                    self.sim_time,
                    f64::from(u32::try_from(micros).unwrap_or(u32::MAX)) * 1e-6,
                )
            });
            let run = m
                .core
                .run_micros(if held || boot_held { 0 } else { micros });
            if let Err(e) = run {
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
                mcu_run_error.get_or_insert_with(|| format!("{}: {e:#}", m.binding.reference));
            }
            let cyc_end = m.core.current_cycle();
            let cycle_exact = m.core.cycle_exact();
            let mcu_ref = m.binding.reference.clone();

            let (edges, edge_log, bytes) = {
                let mut sh = m.shared.lock_or_recover();
                (
                    std::mem::take(&mut sh.pin_edges),
                    std::mem::take(&mut sh.pin_edge_log),
                    std::mem::take(&mut sh.uart_out),
                )
            };
            m.uart_bytes += bytes.len() as u64;
            for edge in &edge_log {
                *m.pin_toggles.entry((edge.port, edge.bit)).or_default() += 1;
            }
            self.observer.record_chunk(
                mi,
                chunk_t0,
                chunk,
                (cyc_start, cyc_end),
                &edge_log,
                &bytes,
            );
            // In-process cores answer the watchdog count for free; the
            // emulators are asked once, after the run.
            if !backend_is_external(&m.binding.backend) {
                self.observer
                    .record_watchdog(mi, chunk_t0, m.core.watchdog_resets(), true);
            }
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
                m.drive_pin(&mut self.circuit, (port, bit), level);
            }
            // 2b. Promotion AND release from the configured pin direction; see
            // `sync_configured_outputs`. On a direction-blind backend the set is
            // empty and the edge-evidence release arm is gated off, so nothing
            // is promoted or torn down there.
            let configured: std::collections::HashSet<(char, u8)> = if held {
                std::collections::HashSet::new()
            } else {
                m.core
                    .pins_configured_output()
                    .into_iter()
                    .map(|p| (p.port, p.bit))
                    .collect()
            };
            self.sync_configured_outputs(mi, configured, &edge_pins);
            self.mcus[mi].refresh_drive_select(&mut self.circuit, &edge_pins);
        }

        // Bench recorder: the bus transactions this chunk's firmware drove.
        self.drain_bus_transactions(false);

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
            let volts = self.digital_view_volts();
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
            let volts = self.digital_view_volts();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for d in &mut self.digital {
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
            let mut ctx = TickCtx::new(&mut self.circuit, &volts, self.sim_time, chunk);
            for b in buses {
                b.lock_or_recover().flush_stops(&mut ctx);
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
            let mut ctx = TickCtx::new(&mut self.circuit, &volts, self.sim_time, chunk);
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
        self.analog_ever_converged |= chunk_converged;
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
                Some(format!(
                    "MCU refused to advance ({}); the digital side of this chunk never ran",
                    mcu_run_error.as_deref().unwrap_or("no error text")
                )),
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
            let mut ctx = TickCtx::new(&mut self.circuit, &volts, self.sim_time, chunk);
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

            // Revoke-only monitor on the full solve; granting is `evaluate`'s job.
            for i in 0..self.regulators.len() {
                let reg = &self.regulators[i];
                let (_, ret, vout) = regulation::terminals(&self.circuit, reg);
                let volts = |n: NodeId| self.node_volts.get(n.0 as usize).map_or(f64::NAN, |v| *v);
                let headroom = vout.signum() * (volts(reg.input) - volts(ret));
                if self.reg_supported[i] && !regulation::headroom_ok(reg, headroom) {
                    self.taint_regulator(i, &Ok(headroom), Some(self.sim_time));
                }
            }
        }
    }

    /// Before the first chunk, arm every behavioural converter at the initial
    /// operating point and settle its arming (the regulation loop and its
    /// input draw are in-circuit, so each DC solve closes them). Supplies stand
    /// at their t=0 values, so the board has been powered forever: a converter
    /// left unarmed until its first update would invent a cold start (and an
    /// inrush) that the stimulus never asked for. A ramped or absent input
    /// settles to 0 V and keeps the cold start. The settled point becomes the
    /// t=0 state the first chunk's behavioural update reads (not the solver's
    /// DC seed). An arming that never settles keeps the cold start it had.
    fn settle_converters(&mut self) {
        let has_converter = self
            .behavioral
            .iter()
            .any(|d| d.converter_out_node().is_some());
        if self.sim_time > 0.0 || self.last_dc_seed.is_some() || !has_converter {
            return;
        }
        let cold = self.circuit.clone();
        self.circuit.temp_c = self.opts.temperature_c;
        let mut ws = Workspace::new(&self.circuit);
        for _ in 0..40 {
            let seed = ws.x.clone();
            if dc_operating_point_seeded(&mut ws, &self.circuit, &self.opts, Some(&seed)).is_err() {
                break;
            }
            let (x, layout) = (&ws.x, &ws.layout);
            let node_v = |n: NodeId| layout.node(n).map_or(0.0, |i| x[i]);
            let circuit = &mut self.circuit;
            let settle = |d: &mut BehavioralDevice| d.update_converter(circuit, &node_v);
            if self.behavioral.iter_mut().map(settle).fold(0.0, f64::max) < 1e-6 {
                for (node, v) in self.node_volts.iter_mut().enumerate().skip(1) {
                    let id = u32::try_from(node).ok().map(NodeId);
                    *v = id.and_then(|n| layout.node(n)).map_or(0.0, |i| x[i]);
                }
                return;
            }
        }
        self.circuit = cold;
    }

    /// Re-decide regulator support when supplies, inputs or shorts changed. Works
    /// on a clone: `node_volts` and the DC seed are never written from it.
    fn establish_regulation(&mut self) {
        if !std::mem::take(&mut self.reg_dirty) {
            return;
        }
        // A regulator that was ever unsupported stays so, however it reads now.
        let tainted =
            |r: &SimpleRegulator| self.regulation.iter().any(|t| t.refs[0] == r.reference);
        let barred: Vec<bool> = self.regulators.iter().map(tainted).collect();
        let (regs, ok) = (&mut self.regulators, &mut self.reg_supported);
        let unsupported = if regs.is_empty() {
            Vec::new()
        } else {
            regulation::evaluate(&self.circuit, regs, ok, &barred, &self.opts)
        };
        for (i, verdict) in unsupported {
            self.taint_regulator(i, &verdict, None);
        }
        self.taint_open_drivers();
    }

    /// The declared supply nets and the reachability walk over them. Both taint
    /// paths need the same pair, and passing different `supply_nets` to the two
    /// would make a net a stop node for one taint and not the other.
    fn taint_walk(&self) -> (Vec<NodeId>, regulation::Walk) {
        let supply_nets: Vec<NodeId> = self.supplies.iter().map(|s| s.net).collect();
        let (regs, ok) = (&self.regulators, &self.reg_supported);
        let walk = regulation::Walk::new(&self.circuit, regs, ok, &supply_nets);
        (supply_nets, walk)
    }

    /// Record every unmodelled part that is powered and could DRIVE a net: what
    /// the solver prints there is the board without it, not a result. Repeats
    /// until nothing is added, so a part powered from a tainted net is caught.
    fn taint_open_drivers(&mut self) {
        let (mut rails, walk) = self.taint_walk();
        let (regs, ok) = (&self.regulators, &self.reg_supported);
        let trusted = regs.iter().zip(ok).filter(|(_, ok)| **ok);
        rails.extend(trusted.map(|(r, _)| regulation::terminals(&self.circuit, r).0));
        rails.extend(
            self.behavioral
                .iter()
                .filter_map(hauksbee_bind::BehavioralDevice::converter_out_node),
        );
        // Indexed once per call: the search below restarts after every taint
        // it adds, and a linear name lookup per pin per pass made this
        // start-up step superlinear in the board's parts and nets.
        let rails: HashSet<NodeId> = rails.into_iter().collect();
        let names = self.node_name_index();
        let mut tainted_nets: HashSet<String> = (self.regulation.iter())
            .flat_map(|t| t.nets.iter().cloned())
            .collect();
        let mut taken: HashSet<String> = (self.regulation.iter())
            .map(|t| t.refs[0].clone())
            .collect();
        loop {
            let tainted = |n: NodeId| tainted_nets.contains(&*names.name(n.0));
            let powered = |n: NodeId| !n.is_ground() && (rails.contains(&n) || tainted(n));
            let found = self.open_parts.iter().find_map(|part| {
                let fresh = !taken.contains(&part.reference);
                let (from, nodes) = fresh.then(|| walk.open_driver(part, &powered))??;
                Some((part.clone(), names.name(from.0).into_owned(), nodes))
            });
            let Some((part, from, nodes)) = found else {
                return;
            };
            let (r, v) = (&part.reference, &part.value);
            let reason = |nets: &[String]| regulation::open_driver_reason(r, v, &from, nets);
            self.push_taint_named(&names, part.reference.clone(), nodes, reason);
            let added = self
                .regulation
                .last()
                .expect("push_taint_named pushes one taint");
            tainted_nets.extend(added.nets.iter().cloned());
            taken.insert(added.refs[0].clone());
        }
    }

    /// Record regulator `i` as unsupported, once: the shared reason, every net
    /// its output can power, and every MCU whose direct supply is among them
    /// (with that MCU's GPIO nets, which it drives from the tainted rail).
    fn taint_regulator(&mut self, i: usize, verdict: &regulation::Verdict, revoked: Option<f64>) {
        self.reg_supported[i] = false;
        let reg = &self.regulators[i];
        if self.regulation.iter().any(|t| t.refs[0] == reg.reference) {
            return;
        }
        // What a revoked regulator feeds must be re-decided without it.
        self.reg_dirty |= revoked.is_some();
        let nodes = self.taint_walk().1.regulator(&self.circuit, reg);
        let (reference, circuit) = (reg.reference.clone(), &self.circuit);
        let reason = regulation::reason(circuit, reg, verdict, revoked);
        self.push_taint(reference, nodes, |_| reason);
    }

    /// The shared tail of both taints: every MCU whose direct supply is among
    /// `nodes` joins `refs`, with the GPIO nets it drives from the tainted rail.
    fn push_taint(
        &mut self,
        reference: String,
        nodes: Vec<NodeId>,
        reason: impl FnOnce(&[String]) -> String,
    ) {
        let names = self.node_name_index();
        self.push_taint_named(&names, reference, nodes, reason);
    }

    /// [`Self::push_taint`] with the node names already indexed.
    fn push_taint_named(
        &mut self,
        names: &NodeNames,
        reference: String,
        mut nodes: Vec<NodeId>,
        reason: impl FnOnce(&[String]) -> String,
    ) {
        let mut refs = vec![reference];
        for m in &self.mcus {
            let fed =
                |(role, n): (&String, &NodeId)| is_direct_supply_role(role) && nodes.contains(n);
            if m.binding.role_nets.iter().any(fed) {
                refs.push(m.binding.reference.clone());
                nodes.extend(m.binding.gpio_drivers.values().map(|d| d.net));
            }
        }
        let mut nets: Vec<String> = nodes.iter().map(|n| names.name(n.0).into_owned()).collect();
        nets.sort();
        nets.dedup();
        self.regulation.push(RegulationTaint {
            reason: reason(&nets),
            nets,
            refs,
            since_s: self.sim_time,
        });
    }

    /// Every regulator whose output (and what it powers) is invalid this run.
    #[must_use]
    pub fn regulation_taint(&self) -> &[RegulationTaint] {
        &self.regulation
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
                    || net_has_drive_evidence(
                        &self.digital_in_evidence,
                        &self.circuit.devices,
                        net,
                    );
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
            let sensors = bus.lock_or_recover().temperature_sensors();
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
    // `chunk` is only read by the debug-only warning below, so a release build
    // (which the Windows release leg compiles with warnings as errors) would
    // otherwise see it unused.
    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    fn deselect_heuristic_spi_buses(&self, chunk: f64) {
        for bus in &self.spi_buses {
            let mut guard = bus.lock_or_recover();
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
            let i = branch_current_of(&self.layout, &self.branch_x, s.vsource).unwrap_or(0.0);
            // Branch current of a Vsource flows p->n internally; the current
            // *delivered to the net* is the negative of that. Signed: current
            // pushed back into a supply is not load.
            s.update(&mut self.circuit, -i, t, chunk);
        }
    }

    /// The node voltages the digital parts read their supplies and inputs
    /// from: the previous chunk's solved operating point. Before ANY chunk has
    /// solved there is none, and the all-zero placeholder would read every rail
    /// as dead, so the first look takes the board's DC operating point at t = 0
    /// (supplies standing at their t = 0 values, the same "powered forever"
    /// reading the converters settle with; a ramped or absent supply solves to
    /// 0 V and stays unpowered). That solve is made once; if it does not
    /// converge the rails an ideal DC source holds directly stand at that value
    /// and every other rail stays at zero: unresolved, so its parts drive
    /// nothing yet.
    fn digital_view_volts(&mut self) -> Vec<f64> {
        if self.analog_ever_converged {
            return self.node_volts.clone();
        }
        if !self.initial_rail_view_tried {
            self.initial_rail_view = self.solve_initial_rail_view();
            self.initial_rail_view_tried = true;
        }
        let mut volts = self.node_volts.clone();
        if let Some(dc) = &self.initial_rail_view {
            for (slot, &v) in volts.iter_mut().zip(dc).skip(1) {
                if *slot == 0.0 {
                    *slot = v;
                }
            }
        } else {
            self.seed_ideal_sources(&mut volts);
        }
        volts
    }

    /// The circuit's DC operating point as it stands (node voltages, indexed by
    /// node), or `None` when the solve does not converge.
    fn solve_initial_rail_view(&self) -> Option<Vec<f64>> {
        dc_node_volts(&self.circuit, &self.opts)
    }

    /// Stand each net an ideal DC source (or a configured supply's source)
    /// holds directly at that source's value, where `volts` still reads zero.
    fn seed_ideal_sources(&self, volts: &mut [f64]) {
        let leg_sources = self.supplies.iter().filter_map(|leg| {
            match self.circuit.devices.get(leg.vsource.0 as usize)? {
                Device::Vsource {
                    kind: SourceKind::Dc(v),
                    ..
                } => Some((leg.net, *v)),
                _ => None,
            }
        });
        let direct = self
            .circuit
            .devices
            .iter()
            .filter_map(|device| match device {
                Device::Vsource {
                    p,
                    n,
                    kind: SourceKind::Dc(v),
                    ..
                } if *n == NodeId::GROUND => Some((*p, *v)),
                _ => None,
            });
        for (net, v) in leg_sources.chain(direct) {
            if let Some(slot) = volts.get_mut(net.0 as usize) {
                if *slot == 0.0 {
                    *slot = v;
                }
            }
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
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        for d in &mut self.behavioral {
            d.update(&mut self.circuit, &node_v, t, chunk);
            self.faults_pending.extend(d.drain_faults());
        }
    }

    /// Live readout per behavioural device: (reference, state, converter input
    /// current A, converter input limit A). For diagnostics / frames / tests.
    #[must_use]
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
        self.behavioral_device(reference)
            .map(|d| d.set_input_budget(vin, budget_w))
            .is_some()
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
    #[must_use]
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
        let (volts, branch, layout) = (&self.node_volts, &self.branch_x, &self.layout);
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        let branch_current = |id: DeviceId| branch_current_of(layout, branch, id);
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
        (seed, carry): (Option<&[f64]>, Option<&MarchCarry>),
        thermal: &mut ChunkThermalAccum,
    ) -> Result<(Vec<f64>, TransientDiagnostics), (String, Vec<f64>)> {
        // A chunk is a WINDOW of one continuous march, so a seeded chunk starts
        // where the last one ended rather than at a fresh operating point. Left
        // as a mere Newton warm start, the DC point re-opened every capacitor at
        // the board's steady state each chunk, deleting any time constant longer
        // than a chunk: a 74HC14 Schmitt oscillator then free-ran at one toggle
        // per chunk with its timing capacitor decorative (multiplying C by ten
        // left the toggle count identical). The first chunk has nothing to carry
        // and keeps its power-on solve; `FromZero` states its own start.
        let mut opts = opts;
        // A march that keeps failing Newton hands the chunk to the ladder
        // instead of grinding (see `SolverOptions::max_failed_steps`).
        opts.max_failed_steps = Some(64);
        if seed.is_some() && opts.dc_init == DcInit::Solve {
            opts.dc_init = DcInit::Continue;
        } else if self.latching && self.last_switch_latch.is_none() && opts.dc_init == DcInit::Solve
        {
            // FIRST chunk of a latching board: a latch fed by its own output
            // has no DC operating point to enter from (a free-running 555: open
            // drives THRES up, which sets it; closed drives it down, which
            // resets it). Power-on from rest is the state such a board starts
            // in, and what the paired ngspice deck states with `uic`. Later
            // cold starts (the ladder's) must not restart the board from rest.
            opts.dc_init = DcInit::FromZero;
        }
        let t = Transient::new(opts)
            .with_switch_latch(self.last_switch_latch.clone())
            .with_carry(carry.cloned());
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
        // Each pass draw's source branch, and the previous step's (time,
        // delivered currents) for its trapezoid, chunk-local like `prev`.
        let pass: Vec<Option<usize>> = (self.regulators.iter())
            .map(|r| r.pass_level.and_then(|_| layout.branch(r.source)))
            .collect();
        let mut pass_prev: Option<(f64, Vec<f64>)> = None;
        // The same for each routed pin driver's `Vsource`.
        let drive: Vec<Option<usize>> = (self.driver_draws.iter())
            .map(|(source, _)| layout.branch(*source))
            .collect();
        let mut drive_prev: Option<(f64, Vec<f64>)> = None;
        let watch = &self.step_watch;
        let res = t.run_streaming_seeded_with_diagnostics(&self.circuit, tstop, seed, |s| {
            if thermal.open_at(&s) {
                return;
            }
            thermal.record_step(watch, s.time, s.x);
            final_x.clear();
            final_x.extend_from_slice(s.x);
            if !drive.is_empty() {
                let now: Vec<f64> = (drive.iter())
                    .map(|b| b.and_then(|b| s.x.get(b)).map_or(0.0, |i| -i))
                    .collect();
                if let Some((t0, i0)) = drive_prev.take() {
                    let dt = s.time - t0;
                    if dt > 0.0 {
                        thermal.drive_charge.resize(now.len(), (0.0, 0.0));
                        for (q, (a, b)) in thermal.drive_charge.iter_mut().zip(i0.iter().zip(&now))
                        {
                            q.0 += 0.5 * (a.max(0.0) + b.max(0.0)) * dt;
                            q.1 += 0.5 * (a.min(0.0) + b.min(0.0)) * dt;
                        }
                        thermal.drive_s += dt;
                    }
                }
                drive_prev = Some((s.time, now));
            }
            if pass.iter().any(Option::is_some) {
                let now: Vec<f64> = (pass.iter())
                    .map(|b| b.and_then(|b| s.x.get(b)).map_or(0.0, |i| -i))
                    .collect();
                if let Some((t0, i0)) = pass_prev.take() {
                    let dt = s.time - t0;
                    if dt > 0.0 {
                        thermal.pass_charge.resize(now.len(), 0.0);
                        for (q, (a, b)) in thermal.pass_charge.iter_mut().zip(i0.iter().zip(&now)) {
                            *q += 0.5 * (a + b) * dt;
                        }
                        thermal.pass_s += dt;
                    }
                }
                pass_prev = Some((s.time, now));
            }
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
    fn fallback_ladder(&self, chunk: f64, thermal: &mut ChunkThermalAccum) -> Option<RungOutcome> {
        let seed = (self.last_dc_seed.as_deref(), self.last_carry.as_ref());
        let reduced = self.reduced_step_opts(chunk);
        // Rungs below the forced one are skipped (test hook; 0 in production,
        // so every rung runs).
        let min_rung = self.debug_force_fallback_rung.map_or(0, |m| m as u8);
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
            (
                ChunkFallbackMethod::ColdStartBackwardEuler,
                be,
                (None, None),
            ),
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
        let (x, diagnostics) = self.march_chunk_subdivided(chunk, 4, be, seed, thermal)?;
        if self.insane_node_state(&x).is_some() {
            return None;
        }
        let error_estimate_v = self.fallback_error_estimate(chunk, 4, be, seed, &x);
        Some(RungOutcome {
            method: ChunkFallbackMethod::SubdividedBackwardEuler,
            x,
            diagnostics,
            error_estimate_v,
            state_jump: None,
        })
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
        seed: (Option<&[f64]>, Option<&MarchCarry>),
        thermal: &mut ChunkThermalAccum,
    ) -> Option<RungOutcome> {
        thermal.clear();
        let (x, diagnostics) = self.march_chunk(chunk, opts, seed, thermal).ok()?;
        if self.insane_node_state(&x).is_some() {
            return None;
        }
        // A rung that marched from a cold start did not continue the carried
        // state: measure the jump it introduced (before the error estimate's
        // companion marches overwrite `thermal`'s start sample).
        let state_jump = (seed.0.is_none())
            .then(|| self.state_jump(&thermal.start_x))
            .flatten();
        let error_estimate_v = self.fallback_error_estimate(chunk, 1, opts, seed, &x);
        Some(RungOutcome {
            method,
            x,
            diagnostics,
            error_estimate_v,
            state_jump,
        })
    }

    /// The largest node-voltage difference between the state the previous
    /// chunk ended in (`last_dc_seed`) and `start`, a march's t = 0 state, with
    /// the unknown it occurs on. `None` when there is no carried state to
    /// compare against (the first chunk, or one after a refusal), or no start
    /// sample was captured.
    fn state_jump(&self, start: &[f64]) -> Option<(f64, usize)> {
        let carried = self.last_dc_seed.as_deref()?;
        (carried.iter().zip(start).take(self.layout.n_nodes))
            .map(|(a, b)| (a - b).abs())
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, v)| (v, i))
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
    #[allow(clippy::cast_precision_loss)] // the subdivision count is small
    fn march_chunk_subdivided(
        &self,
        chunk: f64,
        n: usize,
        opts: SolverOptions,
        (seed, state): (Option<&[f64]>, Option<&MarchCarry>),
        thermal: &mut ChunkThermalAccum,
    ) -> Option<(Vec<f64>, TransientDiagnostics)> {
        let sub = chunk / n as f64;
        let mut carry: Option<Vec<f64>> = seed.map(<[f64]>::to_vec);
        let mut carried: Option<MarchCarry> = state.cloned();
        let mut diagnostics = TransientDiagnostics::default();
        for _ in 0..n {
            match self.march_chunk(sub, opts, (carry.as_deref(), carried.as_ref()), thermal) {
                Ok((x, mut sub_diagnostics)) => {
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
                    // Each quarter continues the previous one's march whole,
                    // and the last one's end state is the chunk's.
                    carried = sub_diagnostics.carry.take();
                    diagnostics.switch_latch = sub_diagnostics.switch_latch;
                    carry = Some(x);
                }
                Err(_) => return None,
            }
        }
        diagnostics.carry = carried;
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
        seed: (Option<&[f64]>, Option<&MarchCarry>),
        x_end: &[f64],
    ) -> Option<f64> {
        // Worst-case Richardson factors at error ratio r = 2 (see above).
        const REFINED_FACTOR: f64 = 2.0;
        const COARSE_FACTOR: f64 = 1.0;
        const REFINED_TOL_SCALE: f64 = 0.25;
        const COARSE_TOL_SCALE: f64 = 4.0;
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
        let refined = Self::companion_opts(opts, REFINED_TOL_SCALE);
        let companion = if self.debug_skip_refined_companion {
            None
        } else {
            self.march_chunk_subdivided(chunk, subdivisions, refined, seed, &mut scratch)
                // A companion the sanity policy would reject as a MAIN result
                // measures nothing as a reference either.
                .filter(|(x_ref, _)| self.insane_node_state(x_ref).is_none())
                .map(|(x_ref, _)| (x_ref, REFINED_FACTOR, REFINED_TOL_SCALE))
        };
        let companion = companion.or_else(|| {
            let coarse = Self::companion_opts(opts, COARSE_TOL_SCALE);
            self.march_chunk_subdivided(chunk, subdivisions, coarse, seed, &mut scratch)
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
        let floor = (1.0 + tol_scale) * self.opts.reltol.mul_add(v_max, self.opts.vntol);
        Some((2.0 * richardson).mul_add(diff, floor))
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

    /// Point every regulator's pass draw at the mean current its source
    /// delivered over the chunk just adopted (see the binder's `pass_draw`),
    /// or at the end state's when no step was integrated.
    fn set_pass_draws(&mut self, chunk: &ChunkThermalAccum) {
        for (i, reg) in self.regulators.iter().enumerate() {
            let Some(level) = reg.pass_level else {
                continue;
            };
            let amps = match chunk.pass_charge.get(i) {
                Some(q) if chunk.pass_s > 0.0 => q / chunk.pass_s,
                _ => -branch_current_of(&self.layout, &self.branch_x, reg.source).unwrap_or(0.0),
            };
            set_vsource_dc(&mut self.circuit, level, amps);
        }
    }

    /// Point every routed pin driver's supply legs at the mean current it
    /// delivered over the chunk just adopted, positive part from `hi` and
    /// negative part into `lo`, or at the end state's when no step was
    /// integrated. One chunk of lag, no `C*dV/dt` noise (see `pass_draw`).
    fn set_driver_draws(&mut self, chunk: &ChunkThermalAccum) {
        for (i, (source, supply)) in self.driver_draws.iter().enumerate() {
            let (sourced, sunk) = match chunk.drive_charge.get(i) {
                Some((qp, qn)) if chunk.drive_s > 0.0 => (qp / chunk.drive_s, qn / chunk.drive_s),
                _ => {
                    let i =
                        -branch_current_of(&self.layout, &self.branch_x, *source).unwrap_or(0.0);
                    (i.max(0.0), i.min(0.0))
                }
            };
            supply.set(&mut self.circuit, sourced, sunk);
        }
    }

    /// Before the first chunk, point every pass draw at its regulator's
    /// operating-point output current, so the first chunk's march starts from
    /// the draw its DC solve (which reads the branch live) already settled.
    fn settle_pass_draws(&mut self) {
        let drawn = self.regulators.iter().any(|r| r.pass_level.is_some());
        if self.sim_time > 0.0 || self.last_dc_seed.is_some() || !drawn {
            return;
        }
        self.circuit.temp_c = self.opts.temperature_c;
        let mut ws = Workspace::new(&self.circuit);
        if dc_operating_point_seeded(&mut ws, &self.circuit, &self.opts, None).is_err() {
            return;
        }
        for reg in &self.regulators {
            let (Some(level), Some(b)) = (reg.pass_level, ws.layout.branch(reg.source)) else {
                continue;
            };
            let amps = ws.x.get(b).map_or(0.0, |i| -i);
            set_vsource_dc(&mut self.circuit, level, amps);
        }
    }

    /// Adopt a converged end state: publish node voltages and branch currents,
    /// and seed the next chunk's DC solve from it.
    /// Record every accepted step from now on at `nets` (unknown names are
    /// skipped), for measurements a chunk-end sample would alias: the peak of
    /// a 100 ns spike, or the input level at which a comparator decided.
    ///
    /// Only a chunk the PRIMARY march solved contributes steps. A fallback
    /// rung restarts chunk-local time (a subdivided rung once per quarter), so
    /// its steps cannot be placed on the run's clock honestly; such a chunk
    /// contributes its chunk-end level through the ordinary frame extremes
    /// only.
    pub fn watch_steps(&mut self, nets: &[String]) {
        for net in nets {
            let known = self.step_watch.iter().any(|(n, _)| n == net);
            if let (false, Some(&node)) = (known, self.net_nodes.get(net)) {
                self.step_watch.push((net.clone(), node));
            }
        }
    }

    /// The per-step series of every watched net, `(t_s, volts)` on the run's
    /// clock: see [`Scheduler::watch_steps`].
    #[must_use]
    pub const fn step_series(&self) -> &HashMap<String, Vec<(f64, f64)>> {
        &self.step_series
    }

    /// Place one primary-solved chunk's watched steps on the run's clock.
    fn commit_steps(&mut self, steps: &[(f64, Vec<f64>)]) {
        for (t, volts) in steps {
            for ((net, node), &v) in self.step_watch.iter().zip(volts) {
                let v = if node.is_ground() { 0.0 } else { v };
                let series = self.step_series.entry(net.clone()).or_default();
                series.push((self.sim_time + t, v));
            }
        }
    }

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
        // A chunk after the first continues the previous chunk's march from its
        // end state (unknowns plus `last_carry`) and solves no DC point; the
        // first chunk, or one after a failure, solves its own.
        let mut thermal = ChunkThermalAccum::default();
        let primary = if self.debug_force_fallback_rung.is_some() {
            // Test hook: pretend the primary failed so the ladder runs on a
            // board of the test's choosing. Never set in production.
            Err(("primary march skipped by test hook".to_string(), Vec::new()))
        } else {
            let seed = (self.last_dc_seed.as_deref(), self.last_carry.as_ref());
            self.march_chunk(chunk, self.opts, seed, &mut thermal)
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
                self.commit_steps(&thermal.steps);
                self.adopt_chunk_state(x);
                self.set_pass_draws(&thermal);
                self.set_driver_draws(&thermal);
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
                if let Some(RungOutcome {
                    method,
                    x,
                    diagnostics,
                    error_estimate_v,
                    state_jump,
                }) = self.fallback_ladder(chunk, &mut thermal)
                {
                    // A rung that restarted from a fresh operating point broke
                    // the trajectory: say so where it happens (with the net),
                    // and carry the jump on the window record, never let it
                    // pass as a continuous solve.
                    if let Some((volts, unknown)) = state_jump {
                        let net = hauksbee_solve::blame::name_node_unknown(&self.circuit, unknown);
                        eprintln!(
                            "WARNING: analog chunk at t={:.6e}s rescued by a cold start: \
                             the carried state was discarded, a {volts:.3e} V jump on {net}",
                            self.sim_time
                        );
                    }
                    self.adopt_chunk_state(x);
                    self.set_pass_draws(&thermal);
                    self.set_driver_draws(&thermal);
                    self.record_residual(diagnostics);
                    // Only the adopted rung's accepted-step thermal integral is
                    // real; the ladder cleared every failed attempt's partial.
                    self.stress
                        .deposit_chunk_energy(&thermal.energy_j, thermal.elapsed_s);
                    self.record_fallback_chunk(
                        chunk,
                        method,
                        error_estimate_v,
                        state_jump.map(|(volts, _)| volts),
                    );
                    true
                } else {
                    // Every rung failed. Keep the solver's own refusal message:
                    // it carries the blame clause naming the net that refused to
                    // settle and the offending element(s). Throwing it away is
                    // what left a 259-part board diagnosable only by bisection.
                    // Said once per failed streak rather than per chunk.
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
                    // A recovered state is not where a march ended, so there
                    // is no march to continue: the next chunk re-derives its
                    // state from the seed (or cold) as a fresh run would.
                    self.last_carry = None;
                    if recovered.is_empty() {
                        // Nothing usable captured: hold previous voltages, and
                        // cold-start the next chunk.
                        self.node_volts.resize(self.circuit.node_count(), 0.0);
                        self.last_dc_seed = None;
                    } else {
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
                    }
                    false
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

        // Apply any forced node-voltage overrides (a firmware-driven inference
        // run drives output nets from an exact feedforward
        // decomposition here, when the monolith does not converge). These land
        // in `node_volts` so the on-board NOR latches sample them next chunk.
        // Each override is time-gated to `sim_time` so a net is HIGH only for
        // its decomposed rate fraction of the window.
        if !self.forced_node_volts.is_empty() {
            let t = self.sim_time;
            for (&node, &(hi, lo, t0, t1)) in &self.forced_node_volts {
                let v = if t >= t0 && t < t1 { hi } else { lo };
                if let Some(slot) = self.node_volts.get_mut(node.0 as usize) {
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
        state_jump_v: Option<f64>,
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
                prev.state_jump_v = match (prev.state_jump_v, state_jump_v) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            _ => self.fallback_windows.push(FallbackWindow {
                start_s: start,
                end_s: end,
                method,
                error_estimate_v,
                state_jump_v,
            }),
        }
    }

    fn record_residual(&mut self, diagnostics: TransientDiagnostics) {
        // Both converged paths funnel through here, so this is the one place
        // the carried latch is adopted -- on a latching board only, so every
        // other board enters each chunk exactly as it always did. `None` (no
        // latch tracked) keeps the previous chunk's rather than clearing it.
        if self.latching && diagnostics.switch_latch.is_some() {
            self.last_switch_latch = diagnostics.switch_latch;
        }
        // Every board carries the rest of its march state into the next chunk.
        // A path that keeps none (the partitioned engine) leaves `None`, and the
        // next chunk re-derives it as before.
        self.last_carry = diagnostics.carry;
        if diagnostics.operating_point.is_some() {
            self.operating_point = diagnostics.operating_point;
        }
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
    /// output net HIGH for a sim-time window proportional to its decomposed
    /// pulse count (rate-coded firmware-driven inference).
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
                    .insert(node, (high_volts, low_volts, t_start, t_end));
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
    #[must_use]
    pub const fn sim_time(&self) -> f64 {
        self.sim_time
    }

    /// Number of chunks this run whose analog transient solve failed to converge.
    /// Zero on a clean run. A non-zero count means at least one window held stale
    /// voltages and cannot vouch for analog-derived findings there.
    #[must_use]
    pub const fn failed_chunk_count(&self) -> u64 {
        self.failed_chunks
    }

    /// The sim-time windows `[start_s, end_s)` where the analog solve failed,
    /// merged where consecutive. Empty on a clean run.
    #[must_use]
    pub fn failed_windows(&self) -> &[(f64, f64)] {
        &self.failed_windows
    }

    /// How the last chunk that solved its own t = 0 DC operating point
    /// reached it: whether that is a true root or the staged ladder's relaxed
    /// seed, and which comparators it holds pinned because they have no
    /// self-consistent DC state. `None` until such a chunk converges.
    #[must_use]
    pub const fn operating_point(&self) -> Option<&OperatingPointReport> {
        self.operating_point.as_ref()
    }

    /// The solver's message from the most recent non-convergent chunk, or
    /// `None` if every chunk this run solved. Pairs with `failed_windows`, so
    /// a failed span carries its cause, not just its extent.
    #[must_use]
    pub fn last_solve_error(&self) -> Option<&str> {
        self.last_solve_error.as_deref()
    }

    /// The solver's refusal message for each failed window, parallel to
    /// [`Self::failed_windows`]. Each carries the blame clause naming the net
    /// that refused to settle, the devices on it, and any element whose
    /// conductance is outside the board's own distribution. Empty on a
    /// clean run.
    #[must_use]
    pub fn failed_window_reasons(&self) -> &[String] {
        &self.failed_window_reasons
    }

    /// The failed windows with their diagnoses, formatted one per line, ready
    /// to print. Empty on a clean run.
    #[must_use]
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
    #[must_use]
    pub const fn fallback_chunk_count(&self) -> u64 {
        self.fallback_chunks
    }

    /// The sim-time windows `[start_s, end_s)` solved by a fallback rung, with
    /// the rung that produced each and its measured chunk-end error estimate,
    /// merged where consecutive with the same rung. Empty on a run the primary
    /// path carried whole.
    #[must_use]
    pub fn fallback_windows(&self) -> &[FallbackWindow] {
        &self.fallback_windows
    }

    /// The machine-readable numerical qualification for the run so far.
    /// Tolerances and methods are the options actually used. Failed spans are
    /// explicit invalid windows; an absent residual means unmeasured.
    ///
    /// # Errors
    ///
    /// Returns an [`EvidenceError`](hauksbee_ir::evidence::EvidenceError) if the
    /// run window, solver tolerances, failed or fallback windows, or the measured
    /// residual do not form a valid budget.
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
                let at = (1u32..)
                    .map(NodeId)
                    .take(self.circuit.node_count().saturating_sub(1))
                    .find(|node| self.layout.node(*node) == Some(unknown))
                    .map_or_else(
                        || format!("unknown #{unknown}"),
                        |node| self.circuit.node_name(node).to_string(),
                    );
                budget = budget.with_residual(hauksbee_ir::evidence::Residual::new(max_abs, at)?);
            }
        }
        Ok(budget)
    }

    /// Stop watching reset and supply nodes nothing drives. A floating net (a
    /// reset pad with only an RC to nothing, a VDDA fed through an unmodelled
    /// ferrite bead) solves to ~0 V through the pins' own legs and would read
    /// as "held in reset" or "supply dead": a modelling gap, not a board fact.
    /// A dropped node leaves its reading unknown rather than wrong.
    fn drop_undriven_rail_nodes(&mut self) {
        let (evidence, devices) = (&self.digital_in_evidence, &self.circuit.devices);
        let driven = |n: &usize| {
            u32::try_from(*n).is_ok_and(|net| net_has_drive_evidence(evidence, devices, net))
        };
        for m in &mut self.mcus {
            m.rails.reset_nodes.retain(driven);
            m.rails.supply_nodes.retain(driven);
        }
    }

    /// Re-read every live MCU's execution evidence: the backend's probe (a few
    /// Monitor round trips on Renode, free on simavr) plus the run's UART,
    /// GPIO, reset, supply and clock observations. Call once after the run and
    /// before reporting; [`Scheduler::execution_evidence`] returns the result.
    pub fn refresh_execution_evidence(&mut self) {
        let volts = &self.node_volts;
        let symbols = self.firmware_symbols.as_ref();
        self.execution = self
            .mcus
            .iter_mut()
            .map(|m| {
                m.rails.observe(volts);
                let board = crate::execution::BoardReadings {
                    reset_held: m.rails.reset_held(m.logic_high_v),
                    supply_valid: m.rails.supply_valid(
                        m.binding.min_supply_v,
                        m.binding.max_supply_v,
                        m.logic_high_v,
                    ),
                    supply_v: m.rails.supply_v(),
                    clock: m.binding.clock.clone(),
                    external_clock_modelled: m.core.external_clock_modelled(),
                };
                let mut evidence = crate::execution::ExecutionEvidence::with_symbols(
                    &m.binding.reference,
                    &m.binding.backend,
                    m.core.execution_probe(),
                    m.uart_bytes,
                    &m.pin_toggles,
                    board,
                    symbols,
                );
                if let Some(boot) = &m.boot {
                    crate::boot::annotate(&mut evidence, boot);
                }
                evidence
            })
            .collect();
        self.execution.sort_by(|a, b| a.mcu_ref.cmp(&b.mcu_ref));
        self.finalize_observation();
    }

    /// Move every completed (and, at the end of a run, in-flight) bus
    /// transaction from the bridges into the recorder.
    fn drain_bus_transactions(&mut self, include_open: bool) {
        let mut txns = Vec::new();
        for bus in &self.i2c_buses {
            txns.extend(bus.lock_or_recover().take_transactions(include_open));
        }
        for bus in &self.spi_buses {
            txns.extend(bus.lock_or_recover().take_transactions(include_open));
        }
        if !txns.is_empty() {
            txns.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
            self.observer.push_bus(txns);
        }
    }

    /// End-of-run bookkeeping for the bench recorder: close in-flight bus
    /// transactions and book the emulators' watchdog reboots (their count is
    /// only read here, so the event carries no time).
    fn finalize_observation(&mut self) {
        self.drain_bus_transactions(true);
        let t = self.sim_time;
        for (mi, m) in self.mcus.iter().enumerate() {
            if backend_is_external(&m.binding.backend) {
                self.observer.ensure_mcu(mi, &m.binding.reference);
                self.observer
                    .record_watchdog(mi, t, m.core.watchdog_resets(), false);
            }
        }
    }

    /// What the bench recorder needs from the scheduler to name things: each
    /// MCU's pin nets, serial net, execution evidence and ELF symbols.
    #[must_use]
    pub fn bench_contexts(&self) -> Vec<crate::observe::bench::McuContext> {
        self.mcus
            .iter()
            .map(|m| {
                let net_name = |n: NodeId| self.circuit.node_name(n).to_string();
                crate::observe::bench::McuContext {
                    mcu_ref: m.binding.reference.clone(),
                    backend: m.binding.backend.clone(),
                    pin_nets: m
                        .binding
                        .gpio_drivers
                        .iter()
                        .map(|(&pin, d)| (pin, net_name(d.net)))
                        .collect(),
                    uart_net: m
                        .binding
                        .role_nets
                        .iter()
                        .filter(|(role, net)| is_uart_tx_net_role(role) && !net.is_ground())
                        .map(|(_, &net)| net_name(net))
                        .min(),
                    configured_outputs: m.configured_outputs.iter().copied().collect(),
                    evidence: self
                        .execution
                        .iter()
                        .find(|e| e.mcu_ref == m.binding.reference)
                        .cloned(),
                    symbols: self.firmware_symbols.clone(),
                }
            })
            .collect()
    }

    /// The bench view of the run so far: what each pin, serial port and bus did.
    /// Call [`Scheduler::refresh_execution_evidence`] first so the core state
    /// and the in-flight bus transactions are final.
    #[must_use]
    pub fn bench_report(&self) -> hauksbee_ir::observations::BenchReport {
        crate::observe::bench::build_report(&self.observer, &self.bench_contexts())
    }

    /// The bench recorder, for exporters.
    #[must_use]
    pub const fn observer(&self) -> &crate::observe::Observer {
        &self.observer
    }

    /// The run as a VCD waveform: MCU pins, serial lines, decoded bus tracks
    /// and event markers. See [`crate::observe::timeline`].
    #[must_use]
    pub fn bench_vcd(&self) -> String {
        let contexts = self.bench_contexts();
        let report = crate::observe::bench::build_report(&self.observer, &contexts);
        crate::observe::timeline::render_vcd(&self.observer, &contexts, &report)
    }

    /// Per-MCU execution evidence from the last
    /// [`Scheduler::refresh_execution_evidence`], ordered by MCU reference.
    /// Empty when no MCU co-simulated or the evidence was never refreshed.
    #[must_use]
    pub fn execution_evidence(&self) -> &[crate::execution::ExecutionEvidence] {
        &self.execution
    }

    /// Whether the firmware demonstrably executed: `Some(true)` when at least
    /// one MCU shows execution evidence, `Some(false)` when none does, `None`
    /// when there is no evidence to judge (no MCU, or never refreshed).
    #[must_use]
    pub fn firmware_executed(&self) -> Option<bool> {
        (!self.execution.is_empty()).then(|| self.execution.iter().any(|e| e.executed))
    }

    /// The one "did the firmware do nothing?" predicate every surface asks
    /// (CLI strict refusal, boot advisory, web front door). UART output or a
    /// firmware-driven GPIO is always activity. Past that, refreshed execution
    /// evidence decides: net toggles alone can come from the analog side (an
    /// oscillator, a responder) and never prove the firmware ran. Without
    /// evidence (no MCU, never refreshed) the older net-toggle test stands.
    #[must_use]
    pub fn firmware_quiet(&self, total_toggles: u64, uart_seen: bool) -> bool {
        // A ROM bootloader's own UART traffic is not the firmware running.
        if crate::boot::all_stopped(&self.execution) {
            return true;
        }
        if uart_seen || self.any_gpio_driven() {
            return false;
        }
        self.firmware_executed()
            .map_or(total_toggles == 0, |ran| !ran)
    }

    /// Which MCUs the boot straps kept from the user firmware, one sentence
    /// each (empty when every MCU boots its firmware).
    #[must_use]
    pub fn boot_stops(&self) -> Vec<String> {
        crate::boot::stop_lines(&self.execution)
    }

    /// Why each MCU that DID execute still cannot count as a firmware pass (a
    /// fault handler entered, reset held, supply out of range), one line per
    /// MCU. Empty when every executing MCU is eligible; a core with no
    /// execution evidence at all is [`Scheduler::firmware_quiet`]'s case.
    #[must_use]
    pub fn firmware_pass_blockers(&self) -> Vec<String> {
        self.execution
            .iter()
            .filter(|e| e.executed && !e.firmware_pass_eligible)
            .map(|e| format!("{}: {}", e.mcu_ref, e.blockers.join("; ")))
            .collect()
    }

    /// False once any chunk this run failed to solve faithfully: either the
    /// analog march diverged (held stale voltages over a window) or an MCU
    /// refused to advance (`run_micros` errored), so the digital side of that
    /// chunk never ran. Either way the co-sim cannot vouch for that window.
    /// Drives `analog_valid` in coverage and the co-sim JSON.
    #[must_use]
    pub const fn analog_valid(&self) -> bool {
        self.failed_chunks == 0
    }

    /// True once the analog solve failed [`STRICT_CONSECUTIVE_FAILED_ABORT`]
    /// chunks in a row at any point this run. A strict headless run (`--strict`)
    /// or hauksbee-ci must abort (exit 3) rather than complete a fake-quiet run.
    #[must_use]
    pub const fn analog_abort_tripped(&self) -> bool {
        self.max_consecutive_failed_chunks >= STRICT_CONSECUTIVE_FAILED_ABORT
    }

    /// True when the analog solve has been refused from t = 0 for a whole
    /// abort streak and has never converged a chunk. Nothing a later chunk
    /// does differs from the cold start every one of those chunks already
    /// re-solved, so the run is invalid for analysis and further marching
    /// cannot change that: the remaining span is recorded, not re-solved.
    #[must_use]
    pub const fn dead_from_start(&self) -> bool {
        !self.analog_ever_converged
            && self.consecutive_failed_chunks >= STRICT_CONSECUTIVE_FAILED_ABORT
    }

    /// Account one chunk of a run that is [`Self::dead_from_start`] without
    /// solving it: the chunk joins the failed window (so the record still
    /// spans everything requested) and time advances.
    fn skip_dead_chunk(&mut self, chunk: f64) {
        if !self.dead_from_start_noted {
            self.dead_from_start_noted = true;
            eprintln!(
                "WARNING: the analog solve has failed every chunk since t=0 \
                 ({STRICT_CONSECUTIVE_FAILED_ABORT} in a row, never converged); the run is \
                 invalid for analysis and the rest of the span is recorded as unsolved \
                 without re-marching it"
            );
        }
        self.record_failed_chunk(chunk, None);
        self.sim_time += chunk;
    }

    /// Clear all forced node-voltage overrides.
    pub fn clear_forced_voltages(&mut self) {
        self.forced_node_volts.clear();
    }

    /// The net names currently carrying a forced post-solve voltage override,
    /// sorted for deterministic output. Empty in every ordinary run.
    #[must_use]
    pub fn forced_net_names(&self) -> Vec<String> {
        // Keys are NodeId indices (see force_net_voltage_windowed), so the
        // circuit's own reverse map is the resolution.
        let mut names: Vec<String> = self
            .forced_node_volts
            .keys()
            .map(|&node| self.circuit.node_name(node).to_string())
            .collect();
        names.sort();
        names
    }

    /// A forced run must be SELF-DECLARING in the published evidence. The
    /// residual suppression in [`Self::error_budget`] is honest about what the
    /// residual can vouch for, but an absent residual is indistinguishable
    /// from an unmeasured one; a reader of the evidence JSON must be told,
    /// in so many words, that these nets present an override rather than a
    /// converged solution. Returns one assumption per simple regulator, then at
    /// most one scoped to the forced nets.
    ///
    /// # Errors
    ///
    /// Returns an [`EvidenceError`](hauksbee_ir::evidence::EvidenceError) if a
    /// regulator or forced-net scope cannot be built (for example an empty net list).
    pub fn forced_voltage_assumptions(
        &self,
    ) -> Result<Vec<hauksbee_ir::evidence::Assumption>, hauksbee_ir::evidence::EvidenceError> {
        use hauksbee_ir::evidence::{Assumption, AssumptionSource, NetScope, Scope, Subject};
        // One per simple regulator: its rail is an ideal source's setpoint,
        // qualified while its input headroom held, undermined otherwise.
        let mut out = Vec::new();
        for (reg, &supported) in self.regulators.iter().zip(&self.reg_supported) {
            let (p, n, _) = regulation::terminals(&self.circuit, reg);
            let rail = self.net_name_of(if p.is_ground() { n.0 } else { p.0 });
            let (key, text) = (
                format!("regulator/{}", reg.reference),
                format!("the {rail} regulator {}", reg.reference),
            );
            let subject = Subject::new(&key, &text);
            let taint = self.regulation.iter().find(|t| t.refs[0] == reg.reference);
            let never = (!supported).then_some("its input headroom was never evaluated");
            let (taint_nets, because) = taint.map_or_else(
                || (vec![rail], never),
                |t| (t.nets.clone(), Some(t.reason.as_str())),
            );
            let scope = Scope::Nets(NetScope::new(taint_nets, None)?);
            out.push(match because {
                Some(because) => {
                    let a = Assumption::not_exercised(
                        AssumptionSource::Scheduler,
                        subject,
                        scope,
                        because,
                        "give the regulator input its headroom from a declared supply and start a fresh run",
                    );
                    // An unpowered input is closed by a declared supply; a sag or an
                    // over-rating is a design question the prose answers. An input
                    // that is another regulator's output rail is powered by powering
                    // that one, so only the first stage of a cascade is asked for.
                    let net = self.net_name_of(reg.input.0);
                    let fed_by_regulator = self.regulators.iter().any(|other| {
                        let (p, n, _) = regulation::terminals(&self.circuit, other);
                        other.reference != reg.reference
                            && self.net_name_of(if p.is_ground() { n.0 } else { p.0 }) == net
                    });
                    if !fed_by_regulator
                        && taint.is_none_or(|t| t.reason.contains("no independent DC support"))
                    {
                        let volts_hint = (reg.adj_return.is_none() && reg.min_in_v.is_finite())
                            .then_some(reg.min_in_v);
                        a.with_remedy(hauksbee_ir::evidence::Remedy::AddSupply { net, volts_hint })
                    } else {
                        a
                    }
                }
                None => Assumption::reduced_fidelity(
                    AssumptionSource::Scheduler,
                    subject,
                    scope,
                    &format!(
                        // The enable pin is not in this list: the binder gates the
                        // rail on it, and discloses the variants it cannot read
                        // on the part itself.
                        "an ideal source held at its setpoint (input headroom checked against \
                         declared dropout only; input current, current limit, thermal and \
                         startup are not modelled){}",
                        if reg.dropout_v.is_some() { "" } else { UNVERIFIED_HEADROOM }
                    ),
                    "validate this rail on hardware or with a behavioural converter model",
                )
                // The part, as a typed target: the id's `regulator/` namespace
                // is not a part name.
                .with_remedy(hauksbee_ir::evidence::Remedy::AddModel {
                    reference: reg.reference.clone(),
                }),
            });
        }
        let nets = self.forced_net_names();
        if nets.is_empty() {
            return Ok(out);
        }
        let list = nets.join(", ");
        out.push(Assumption::reduced_fidelity(
            AssumptionSource::Scheduler,
            Subject::new(
                "scheduler/forced-voltages",
                "post-solve forced node voltages",
            ),
            Scope::Nets(NetScope::new(nets, None)?),
            &format!(
                "the reported voltages on {list} were written by a forced override after \
                 the solve, not converged in place, and the run's residual is withheld \
                 because it belongs to the pre-override solution"
            ),
            "remove the force, or validate the forced trajectory independently and cite \
             that validation as the evidence for these nets",
        ));
        Ok(out)
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
    /// A refusal preserves the old diagnostics; a possibly partial core reset
    /// also latches execution off. This does not reset external bus devices.
    ///
    /// # Errors
    ///
    /// Returns a message if any MCU backend cannot reset, or if a core reset fails;
    /// after a failed core reset execution stays latched off until a retry succeeds.
    pub fn reset_run_state(&mut self) -> Result<(), String> {
        for m in &self.mcus {
            if !m.core.supports_reset() {
                return Err(format!(
                    "{}: this MCU backend cannot reset; relaunch the session",
                    m.binding.reference
                ));
            }
        }
        // No rollback is promised: even the first failing core may have changed.
        // Keep the old history and coupling state until every reset succeeds.
        for m in &mut self.mcus {
            if let Err(error) = m.core.reset() {
                let reason = format!(
                    "{}: MCU reset failed: {error}; relaunch or retry reset",
                    m.binding.reference
                );
                self.reset_failure = Some(reason.clone());
                return Err(reason);
            }
        }
        self.reset_failure = None;
        self.sim_time = 0.0;
        self.micros_carry = 0.0;
        self.failed_chunks = 0;
        self.failed_windows.clear();
        self.failed_window_reasons.clear();
        self.fallback_chunks = 0;
        self.fallback_windows.clear();
        self.worst_residual = None;
        self.operating_point = None;
        self.last_switch_latch = None;
        self.consecutive_failed_chunks = 0;
        self.max_consecutive_failed_chunks = 0;
        self.analog_ever_converged = false;
        self.initial_rail_view = None;
        self.initial_rail_view_tried = false;
        self.dead_from_start_noted = false;
        self.last_solve_error = None;
        for st in self.stats.values_mut() {
            *st = NetStat::default();
        }
        self.firmware_edge_toggles.clear();
        self.observer = crate::observe::Observer::default();
        self.frame_peak_current.clear();
        self.frame_v_extremes.clear();
        self.step_series.clear();
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
        // All cores rebooted successfully; now discard their stale coupling.
        for m in &mut self.mcus {
            m.last_levels.clear();
            m.configured_outputs.clear();
            m.digital_in_levels.clear();
            if let Ok(mut sh) = m.shared.lock() {
                sh.pin_edges.clear();
                sh.pin_edge_log.clear();
                sh.uart_out.clear();
            }
            // Hi-Z until firmware configures its pins again.
            for drv in m.binding.gpio_drivers.values_mut() {
                drv.set_enabled(&mut self.circuit, false);
            }
        }
        // The analog operating point restarts with the story: drop the warm
        // start and the previous run's solved node state so the first chunk
        // after reset re-solves from the same cold start as construction.
        self.last_dc_seed = None;
        self.last_carry = None;
        self.node_volts.iter_mut().for_each(|v| *v = 0.0);
        self.branch_x.iter_mut().for_each(|v| *v = 0.0);
        for leg in &mut self.model_peripheral_power {
            leg.powered = false;
            leg.last_current_a = 0.0;
            leg.set_bus_powered(false);
            leg.drain_activity();
            set_isource_dc(&mut self.circuit, leg.isource, 0.0);
        }
        Ok(())
    }

    /// Digital reset failure, separate from the retained analog evidence.
    #[must_use]
    pub fn reset_failure(&self) -> Option<&str> {
        self.reset_failure.as_deref()
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
        let powered = self.sim_time >= self.powered_from_s;
        for (name, &node) in &self.net_nodes {
            let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
            let st = self.stats.entry(name.clone()).or_default();
            st.min_v = st.min_v.min(v);
            st.max_v = st.max_v.max(v);
            if powered {
                st.powered_min_v = st.powered_min_v.min(v);
            }
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
                // The terminals read the junction plus the `rs` drop, so the
                // current comes from the junction the monitor recovers, not
                // Shockley at the terminal voltage: that read a 24.5 mA LED
                // (rs = 6 ohm) as a 0.58 A peak.
                Device::Diode { name, a, k, model } => (
                    name,
                    hauksbee_bind::stress::diode_current(model, v(*a) - v(*k), self.circuit.temp_c)
                        .abs(),
                ),
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
    #[must_use]
    pub const fn frame_peak_current(&self) -> &HashMap<String, f64> {
        &self.frame_peak_current
    }

    /// Per-net (`min_v`, `max_v`) over the last completed frame's sub-chunks.
    #[must_use]
    pub const fn frame_v_extremes(&self) -> &HashMap<String, (f64, f64)> {
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
        self.reg_dirty = true;
        for dev in &mut self.circuit.devices {
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
            let idle = m
                .binding
                .gpio_drivers
                .get(&(port, bit))
                .is_some_and(|drv| !drv.enabled);
            if idle {
                let level = m.last_levels.get(&(port, bit)).copied().unwrap_or(false);
                m.drive_pin(&mut self.circuit, (port, bit), level);
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
    #[must_use]
    pub fn net_voltage(&self, net: &str) -> Option<f64> {
        let node = self.net_nodes.get(net)?;
        self.node_volts.get(node.0 as usize).copied()
    }

    /// The (min, max) a net covered across the run's converged chunks, the
    /// floor counted once the board is powered and the ceiling throughout (a
    /// power-on overshoot is real). `None` before any powered sample.
    #[must_use]
    pub fn net_range(&self, net: &str) -> Option<(f64, f64)> {
        let st = self.stats.get(net)?;
        (st.powered_min_v <= st.max_v).then_some((st.powered_min_v, st.max_v))
    }

    /// Every named node's net name, the same one [`Self::net_name_for`] finds
    /// (the first in `net_nodes` order), for callers that look up many nodes.
    fn node_name_index(&self) -> NodeNames {
        let mut by_node: HashMap<u32, String> = HashMap::with_capacity(self.net_nodes.len());
        for (name, node) in &self.net_nodes {
            by_node.entry(node.0).or_insert_with(|| name.clone());
        }
        NodeNames(by_node)
    }

    /// The name of the net on `node`, or `"node N"` for an unnamed one.
    fn net_name_of(&self, node: u32) -> String {
        self.net_name_for(NodeId(node))
            .map_or_else(|| format!("node {node}"), str::to_string)
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
    #[allow(clippy::cast_precision_loss)] // cycle counts within one chunk are far below 2^52
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
                    if width.is_none_or(|cur| g < cur) {
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
    ///
    /// A model output at least [`OVERDRIVE_RATIO`] times weaker than the MCU
    /// pin is not a fight: the pin holds the net within 1/(1 + ratio) of its
    /// own level, which is how an auto-direction translator's deliberately
    /// weak DC drivers are meant to be overdriven (TI SCES639F 6.3.1).
    fn detect_driver_contention(&mut self) {
        if self.sim_time <= 0.0 || self.digital.is_empty() || self.mcus.is_empty() {
            return;
        }
        // Enabled modelled push-pull outputs per net node, as "REF.role" with
        // the driver's output resistance.
        let mut model_out: HashMap<u32, Vec<(String, f64)>> = HashMap::new();
        for d in &self.digital {
            for (role, drv) in &d.drivers {
                if !drv.enabled {
                    continue;
                }
                model_out
                    .entry(drv.net.0)
                    .or_default()
                    .push((format!("{}.{role}", d.reference), drv.ron));
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
                let fights = |(_, ron): &&(String, f64)| *ron < OVERDRIVE_RATIO * drv.ron;
                let mut parts: Vec<String> = parts
                    .iter()
                    .filter(fights)
                    .map(|(p, _)| p.clone())
                    .collect();
                if parts.is_empty() {
                    continue;
                }
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
    #[must_use]
    pub fn short_pulses(&self) -> &[ShortPulse] {
        &self.short_pulses
    }

    /// Runtime driver-contention findings raised this run (the model-vs-MCU
    /// case the static lint cannot reach), one per offending net.
    #[must_use]
    pub fn driver_contentions(&self) -> &[DriverContention] {
        &self.contentions
    }

    /// Snapshot every net's current voltage.
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    /// driven LOW, not floating), or the backend reports a peripheral output
    /// (PWM, timer, RMT) routed onto the pad. A net NOT in this set was never driven by any
    /// MCU: it floats. Used by the boot-state panel to classify each gate as
    /// driven-high / driven-low / floating without enabling any circuit driver.
    #[must_use]
    pub fn firmware_driven_nets(&self) -> Vec<String> {
        let configured = self.configured_output_pins();
        // Pads a peripheral output (PWM, timer, RMT) is routed onto through a
        // pin-mux: driven by the peripheral, so no pin write or direction
        // report need exist for them. Observation only, never a level.
        let routed: Vec<std::collections::HashSet<(char, u8)>> = self
            .mcus
            .iter()
            .map(|m| {
                m.core
                    .pins_routed_to_peripheral()
                    .into_iter()
                    .map(|p| (p.port, p.bit))
                    .collect()
            })
            .collect();
        let mut out = self.gpio_nets_where(|mi, m, pin, _| {
            m.last_levels.contains_key(&pin)
                || configured[mi].contains(&pin)
                || routed[mi].contains(&pin)
        });
        out.extend(self.peripheral_driven_nets());
        out.sort();
        out.dedup();
        out
    }

    /// The nets of pins a peripheral block drives, for peripherals the core
    /// reported activity on: a UART transmit line on a core that sent bytes, an
    /// I2C clock and data line on a core whose backend delivered I2C events, an
    /// SPI clock and data-out line on a core that delivered SPI events.
    ///
    /// The peripheral, not the GPIO output register, drives such a pin, so a
    /// backend that reports no pin write or direction for it (a pin-muxed UART,
    /// the TWI block's open-drain override, an alternate-function SPI pad)
    /// would otherwise leave a line the firmware demonstrably drove out of the
    /// driven set, and with it everything an RC or level shifter takes from
    /// that line. Two sources feed each peripheral: the pin roles the binder
    /// tagged with the signal (`_txd`, `_sda`, `_scl`, `_sck`, `_mosi`), and
    /// the signal nets of the hardware buses attached to the board (which names
    /// the nets on a part whose pins carry no signal suffix). Counts are per
    /// core, not per peripheral instance, so a second, silent UART, I2C or SPI
    /// block's lines are listed too, which errs toward judging a time bound
    /// there over the whole run.
    fn peripheral_driven_nets(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push_roles = |m: &LiveMcu, wanted: &dyn Fn(&str) -> bool| {
            for (role, &node) in &m.binding.role_nets {
                if wanted(role) && gpio_of_role(role, m.binding.module).is_some() {
                    if let Some(name) = self.net_name_for(node) {
                        out.push(name.to_string());
                    }
                }
            }
        };
        let busy = |counter: &Arc<AtomicU64>| counter.load(Ordering::Relaxed) > 0;
        for m in &self.mcus {
            if m.uart_bytes > 0 {
                push_roles(m, &is_uart_tx_role);
            }
            if busy(&m.i2c_events) {
                push_roles(m, &|r| role_has_token(r, &["sda", "scl"]));
            }
            if busy(&m.spi_events) {
                push_roles(m, &|r| role_has_token(r, &["sck", "mosi"]));
            }
        }
        if self.mcus.iter().any(|m| busy(&m.i2c_events)) {
            for bus in &self.hw_i2c_buses {
                let nodes = bus.lock_or_recover().signal_nodes().to_vec();
                out.extend(
                    nodes
                        .into_iter()
                        .filter_map(|n| self.net_name_for(n).map(str::to_string)),
                );
            }
        }
        if self.mcus.iter().any(|m| busy(&m.spi_events)) {
            for bus in &self.hw_spi_buses {
                let nodes = bus.lock_or_recover().signal_nodes().to_vec();
                out.extend(
                    nodes
                        .into_iter()
                        .filter_map(|n| self.net_name_for(n).map(str::to_string)),
                );
            }
        }
        out
    }

    /// Board nets no MCU pin drives whose level the firmware can still move,
    /// each paired with the firmware-driven net (see
    /// [`Self::firmware_driven_nets`]) it is downstream of, sorted by net.
    ///
    /// A directed walk from the driven nets through the bound circuit (a gate
    /// into its channel, a switch's control into its contacts, a passive end to
    /// end, an amplifier's inputs into its output), through the logic parts the
    /// scheduler evaluates (any pin into its outputs), through a behavioural
    /// power device (any pin into its other pins and the rails it holds) and
    /// an unmodelled part (any pin into its drive-capable pins), and from a
    /// simple regulator's enable or shutdown pin into its output: the stamp
    /// does not read the enable, but on the real board whatever drives it
    /// switches the rail. The outputs a bus peripheral sets when the firmware
    /// writes it (a DAC's output nets, from `I2cBus::driven_nodes` and its SPI
    /// counterpart) are seeds of their own, named after the bus's signal net
    /// the firmware drove (see `bus_driven_outputs`). The walk stops where the
    /// regulation taint walk stops (ground, declared supplies, the terminals
    /// of trusted voltage sources), except that a 0 V source in series
    /// between two live nodes (a current-sense ammeter) is a wire to it, so a
    /// rail an independent source holds is not reached through a pull-up
    /// beside it, and it never crosses an MCU: a supply pin's draw is a load
    /// on the rail, not a way to move it.
    #[must_use]
    pub fn firmware_influenced_nets(&self) -> Vec<(String, String)> {
        let driven = self.firmware_driven_nets();
        // Each driven node once, under the first driven net name on it.
        let mut seeds: Vec<(NodeId, String)> = Vec::new();
        for name in &driven {
            if let Some(&node) = self.net_nodes.get(name) {
                if !seeds.iter().any(|(n, _)| *n == node) {
                    seeds.push((node, name.clone()));
                }
            }
        }
        // Outputs a bus peripheral sets in response to firmware writes join
        // the walk as seeds of their own, and are listed in the result.
        let bus_outputs = self.bus_driven_outputs(&driven, &seeds);
        for (node, from) in &bus_outputs {
            seeds.push((*node, from.clone()));
        }
        if seeds.is_empty() {
            return Vec::new();
        }
        let starts: Vec<NodeId> = seeds.iter().map(|(n, _)| *n).collect();
        let reached: HashMap<NodeId, NodeId> =
            self.influence_graph().reach(&starts).into_iter().collect();
        let origin = |seed: NodeId| {
            seeds
                .iter()
                .find(|(n, _)| *n == seed)
                .map(|(_, s)| s.clone())
        };
        // Every board name on a reached node: two names can share one node.
        let mut out: Vec<(String, String)> = self
            .net_nodes
            .iter()
            .filter_map(|(name, node)| {
                let of = match reached.get(node) {
                    Some(seed) => origin(*seed)?,
                    None => bus_outputs.iter().find(|(n, _)| n == node)?.1.clone(),
                };
                Some((name.clone(), of))
            })
            .collect();
        out.sort();
        out
    }

    /// Every net an MCU GPIO pad sits on, plus every net a pad could move
    /// through the board (the same walk as [`Self::firmware_influenced_nets`]),
    /// whether or not any firmware ran. This is the topological answer to
    /// "could firmware have driven this net?", used to tell a pin-toggle claim
    /// that rests on firmware execution from one a spec stimulus can satisfy.
    #[must_use]
    pub fn mcu_reachable_nets(&self) -> Vec<String> {
        let live = self
            .gpio_nets_where(|_, _, _, _| true)
            .into_iter()
            .filter_map(|name| self.net_nodes.get(&name).copied());
        let idle = self
            .idle_mcus
            .iter()
            .flat_map(|m| m.gpio_nodes.iter().copied());
        let seeds: Vec<NodeId> = live.chain(idle).collect();
        if seeds.is_empty() {
            return Vec::new();
        }
        let reached: std::collections::HashSet<NodeId> = self
            .influence_graph()
            .reach(&seeds)
            .into_iter()
            .map(|(node, _)| node)
            .chain(seeds.iter().copied())
            .collect();
        let mut out: Vec<String> = self
            .net_nodes
            .iter()
            .filter(|(_, node)| reached.contains(node))
            .map(|(name, _)| name.clone())
            .collect();
        out.sort();
        out
    }

    /// MCU pads a peripheral output drives, or may drive, with a waveform the
    /// co-sim does not model, and every net that follows one through the
    /// board, keyed by net (sorted).
    ///
    /// Two kinds of pad. One the backend reports a peripheral output routed
    /// onto ([`Mcu::pins_routed_to_peripheral`]): already firmware-driven, it
    /// carries no GPIO level (the backend drops its stale latch), and the nets
    /// behind it are in [`Self::firmware_influenced_nets`]. And, on a
    /// host-paced core, an output-capable pad with no drive evidence whose
    /// backend cannot observe peripheral routing for it
    /// ([`Mcu::peripheral_routing_unobserved`]): a PWM, timer, RMT or bus
    /// output could move it with nothing reported, so its absence from the
    /// driven set proves nothing. Pads that cannot drive
    /// ([`Mcu::pin_can_drive`]) and pads wired to nothing but MCU pins are left
    /// out, and the rest seed the influence walk, so an RC filter behind one
    /// is listed under it. A clock-exact core keeps its hard verdicts, so it
    /// adds no unobserved pad.
    #[must_use]
    pub fn peripheral_pad_nets(&self) -> Vec<(String, PeripheralPad)> {
        let driven = self.firmware_driven_nets();
        let wired = self.nodes_wired_beyond_mcu_pins();
        let mut pads: Vec<(NodeId, PeripheralPad)> = Vec::new();
        for m in &self.mcus {
            let routed = m.core.pins_routed_to_peripheral();
            let paced = m.core.clock_host_paced();
            for (role, &node) in &m.binding.role_nets {
                let Some((port, bit)) = gpio_of_role(role, m.binding.module)
                    .or_else(|| apin_gpio_of_role(role, m.binding.module))
                else {
                    continue;
                };
                let (Some(net), pin) = (self.net_name_for(node), PinId { port, bit }) else {
                    continue;
                };
                let unobserved = if routed.contains(&pin) {
                    None
                } else if paced
                    && m.core.pin_can_drive(pin)
                    && wired.contains(&node)
                    && !driven.iter().any(|d| d == net)
                {
                    let Some(why) = m.core.peripheral_routing_unobserved(pin) else {
                        continue;
                    };
                    Some(why)
                } else {
                    continue;
                };
                let pin = role.clone();
                let mcu = m.binding.reference.clone();
                let net = net.to_string();
                pads.push((
                    node,
                    PeripheralPad {
                        net,
                        mcu,
                        pin,
                        unobserved,
                    },
                ));
            }
        }
        pads.sort_by(|a, b| (&a.1.net, &a.1.pin).cmp(&(&b.1.net, &b.1.pin)));
        let blind: Vec<NodeId> = pads
            .iter()
            .filter(|(_, p)| p.unobserved.is_some())
            .map(|(n, _)| *n)
            .collect();
        let reached: HashMap<NodeId, NodeId> = if blind.is_empty() {
            HashMap::new()
        } else {
            self.influence_graph().reach(&blind).into_iter().collect()
        };
        let mut out: Vec<(String, PeripheralPad)> = self
            .net_nodes
            .iter()
            .filter_map(|(name, node)| {
                let seed = reached.get(node).unwrap_or(node);
                let pad = pads.iter().find(|(n, _)| n == seed)?;
                Some((name.clone(), pad.1.clone()))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Nodes something other than an MCU pin driver touches: a circuit
    /// device, or a pin of a logic, behavioural or unmodelled part.
    fn nodes_wired_beyond_mcu_pins(&self) -> std::collections::HashSet<NodeId> {
        let legs: std::collections::HashSet<DeviceId> = self
            .mcus
            .iter()
            .flat_map(|m| m.binding.gpio_drivers.values())
            .flat_map(|d| [d.vsource, d.resistor])
            .collect();
        let mut out: std::collections::HashSet<NodeId> = self
            .circuit
            .iter()
            .filter(|(id, _)| !legs.contains(id))
            .flat_map(|(_, d)| d.nodes())
            .collect();
        out.extend(self.digital.iter().flat_map(|d| d.roles.values().copied()));
        out.extend(
            self.behavioral
                .iter()
                .flat_map(hauksbee_bind::BehavioralDevice::role_nodes),
        );
        out.extend(
            self.open_parts
                .iter()
                .flat_map(|p| p.pins.iter().map(|(n, _)| *n)),
        );
        out
    }

    /// The graph [`Self::firmware_influenced_nets`] walks: the bound
    /// circuit's devices, the logic, behavioural and unmodelled parts the
    /// scheduler evaluates, and each simple regulator's enable into its
    /// output, stopping where the regulation taint walk stops.
    fn influence_graph(&self) -> crate::influence::Influence {
        let (_, walk) = self.taint_walk();
        let helpers: Vec<DeviceId> = self
            .regulators
            .iter()
            .flat_map(|r| r.helpers.iter().copied())
            .collect();
        let mut graph = crate::influence::Influence::new(&self.circuit, &helpers, walk.stops());
        for reg in &self.regulators {
            let (out, _, _) = regulation::terminals(&self.circuit, reg);
            for en in &reg.enable {
                graph.force(*en, out);
            }
        }
        for d in &self.digital {
            for from in d.roles.values() {
                for drv in d.drivers.values() {
                    graph.link(*from, drv.net);
                }
            }
        }
        for b in &self.behavioral {
            // Unknown pin roles err toward influence: any pin but the one it
            // takes power from can steer its state machine, and with it the
            // rails it holds.
            let input = b.converter_in_node();
            let held = b.driven_nodes();
            for from in b.role_nodes().filter(|n| Some(*n) != input) {
                for to in b.role_nodes() {
                    graph.link(from, to);
                }
                for to in &held {
                    graph.force(from, *to);
                }
            }
        }
        for part in &self.open_parts {
            for (from, _) in &part.pins {
                for (to, drives) in &part.pins {
                    if *drives {
                        graph.link(*from, *to);
                    }
                }
            }
        }
        graph
    }

    /// The nodes the attached bus peripherals drive in response to firmware
    /// writes (a DAC's output nets), each with the net it is attributed to:
    /// the bus's own signal net the firmware drove, else its first signal net,
    /// else the first firmware-driven net, else the bus's id. A node an MCU pin
    /// already drives (`seeds`) is left out, and so is everything when no MCU
    /// runs, since nothing then writes to a bus.
    fn bus_driven_outputs(
        &self,
        driven: &[String],
        seeds: &[(NodeId, String)],
    ) -> Vec<(NodeId, String)> {
        use crate::peripherals::Peripheral as _;
        if self.mcus.is_empty() {
            return Vec::new();
        }
        // (id, signal nodes, driven nodes) of every attached bus.
        let mut buses: Vec<(String, Vec<NodeId>, Vec<NodeId>)> = Vec::new();
        for bus in &self.i2c_buses {
            let b = bus.lock_or_recover();
            let id = b.id().to_string();
            buses.push((id, b.signal_nodes().to_vec(), b.driven_nodes()));
        }
        for bus in &self.spi_buses {
            let b = bus.lock_or_recover();
            let id = b.id().to_string();
            buses.push((id, b.signal_nodes().to_vec(), b.driven_nodes()));
        }
        let mut out: Vec<(NodeId, String)> = Vec::new();
        for (id, signals, outputs) in buses {
            let signal_net = |n: &NodeId| self.net_name_for(*n).map(str::to_string);
            let by_firmware = signals
                .iter()
                .find(|n| seeds.iter().any(|(s, _)| s == *n))
                .and_then(signal_net);
            let from = by_firmware
                .or_else(|| signals.first().and_then(signal_net))
                .or_else(|| driven.first().cloned())
                .unwrap_or(id);
            for node in outputs {
                let taken =
                    seeds.iter().any(|(s, _)| *s == node) || out.iter().any(|(s, _)| *s == node);
                if !node.is_ground() && !taken {
                    out.push((node, from.clone()));
                }
            }
        }
        out
    }

    /// MCU GPIO nets whose pin the firmware actively configured as an OUTPUT
    /// (DDR set, by latest direction). A net high in this set is a strong
    /// push-pull drive; a net high but NOT in it is a weak internal pull-up.
    /// Observation metadata (from `pins_configured_output`), never a drive.
    #[must_use]
    pub fn firmware_output_configured_nets(&self) -> Vec<String> {
        let configured = self.configured_output_pins();
        self.gpio_nets_where(|mi, _, pin, _| configured[mi].contains(&pin))
    }

    /// Last GPIO levels per MCU, for component-state frames.
    #[must_use]
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
        let generic_ticks = if self.replay_chips.is_empty() {
            0
        } else if let Some(pin_nets) = self.replay_pin_nets.get(mi).cloned() {
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
    /// Coarse-stamped backends (`cycle_exact` == false) still get the drive:
    /// their per-poll ordering is preserved even though intra-poll spacing is
    /// approximate, and an approximately-timed pulse train integrates far
    /// closer to the truth than a collapsed level. The per-pin point cap
    /// bounds a pathological edge storm before it can blow up the solve. If it
    /// is exceeded the scheduler records a hard timing refusal; strict/CI
    /// callers invalidate the result instead of silently trusting the DC
    /// fallback.
    // min/max keeps a NaN edge time bounded; clamp would propagate the NaN.
    #[allow(clippy::manual_clamp, clippy::cast_precision_loss)] // cycle counts within one chunk are far below 2^52
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
    #[must_use]
    pub const fn last_replay_microticks(&self) -> usize {
        self.last_replay_microticks
    }

    /// Per edge-driven 74HC595 chain, the MCU GPIO `(port, bit)` bound to its
    /// SRCLK, RCLK, optional `SRCLR_n`, optional `OE_n`, and head SER, plus the chip
    /// count. Empty when no chain is clocked by the MCU (the once-per-chunk
    /// path handles it). Exposed for diagnostics and co-sim tests of the chain
    /// wiring.
    #[must_use]
    pub fn hc595_chain_pins(&self) -> Vec<Hc595ChainPins> {
        self.chains
            .iter()
            .map(|c| (c.srclk, c.rclk, c.srclr_n, c.oe_n, c.ser, c.order.len()))
            .collect()
    }

    /// Per edge-driven 74HC165 read chain, the MCU GPIO `(port,bit)` bound to
    /// its PL, CLK, and MISO input, plus chip count. Empty when no MCU-clocked
    /// 165 chain was identified. For diagnostics and read-chain co-sim tests.
    #[must_use]
    pub fn hc165_chain_pins(&self) -> Vec<(GpioPin, GpioPin, GpioPin, usize)> {
        self.hc165_chains
            .iter()
            .map(|c| {
                let c = c.lock_or_recover();
                (c.pl_n, c.clk, c.miso, c.order.len())
            })
            .collect()
    }

    /// The last word each 74HC165 read chain captured on its most recent PL
    /// load (MSB-first, as the firmware accumulates it). For diagnostics/tests.
    #[must_use]
    pub fn hc165_loaded_words(&self) -> Vec<u16> {
        self.hc165_chains
            .iter()
            .map(|c| c.lock_or_recover().loaded_word())
            .collect()
    }

    /// Digital component register states, for component-state frames.
    #[must_use]
    pub fn digital_states(&self) -> HashMap<String, HashMap<String, f64>> {
        self.digital
            .iter()
            .map(|d| (d.reference.clone(), d.state_summary()))
            .collect()
    }

    /// Drain the faults raised since the last call (for `SimFrame`).
    pub fn drain_faults(&mut self) -> Vec<FaultEvent> {
        std::mem::take(&mut self.faults_pending)
    }

    /// Enable or disable destructive faulting (mutate the circuit on fault).
    pub const fn set_destructive_faults(&mut self, on: bool) {
        self.stress.destructive = on;
    }

    /// Configure the power supply on a named supply net. Returns true if a
    /// supply leg for that net existed and was reconfigured.
    pub fn set_power_supply(&mut self, net: &str, supply: PowerSupply) -> bool {
        let Some(leg) = self.supplies.iter_mut().find(|s| s.net_name == net) else {
            return false;
        };
        let devices = self.circuit.devices.len();
        leg.reconfigure(&mut self.circuit, supply);
        if self.circuit.devices.len() != devices {
            self.relayout(); // a first current limit stamps its fold
        }
        self.reg_dirty = true;
        true
    }

    /// Live supply readout per net: (kind label, last rail current A, state of charge).
    #[must_use]
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
    #[must_use]
    pub fn stress_states(&self) -> HashMap<String, f64> {
        self.stress.stress_by_ref().clone()
    }

    /// Live per-component estimated junction temperature (C) for the thermal
    /// view. Only populated for dissipating devices.
    #[must_use]
    pub fn temp_states(&self) -> HashMap<String, f64> {
        self.stress.temp_by_ref().clone()
    }

    /// Set the ambient temperature (C) the thermal monitor's steady-state
    /// junction estimate sits on top of.
    pub const fn set_ambient_c(&mut self, ambient_c: f64) {
        self.stress.ambient_c = ambient_c;
        self.stress.ambient_given = true;
    }

    /// The configured ambient temperature (C).
    #[must_use]
    pub const fn ambient_c(&self) -> f64 {
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
        self.reg_dirty = true;
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
            let mut snap = self.input_volts.lock_or_recover();
            snap.resize(n_nodes, 0.0);
        }
        // The unknown vector changed shape; a prior warm seed no longer applies.
        self.last_dc_seed = None;
        self.last_carry = None;
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

/// A fallback rung that rescued a chunk: its method, end state, diagnostics,
/// step-doubling error estimate, and (for a rung that discarded the carried
/// state) the largest node-voltage jump it introduced with the unknown it
/// occurred on.
struct RungOutcome {
    method: ChunkFallbackMethod,
    x: Vec<f64>,
    diagnostics: TransientDiagnostics,
    error_estimate_v: Option<f64>,
    state_jump: Option<(f64, usize)>,
}

/// The solved branch current of device `id`, when it owns a branch unknown.
fn branch_current_of(layout: &Layout, branch_x: &[f64], id: DeviceId) -> Option<f64> {
    layout
        .branch(id)
        .and_then(|b| branch_x.get(b.saturating_sub(layout.n_nodes)).copied())
}

/// Point a regulator's hidden pass-level source at `amps` (1 V per A).
fn set_vsource_dc(circuit: &mut Circuit, id: DeviceId, amps: f64) {
    match circuit.devices.get_mut(id.0 as usize) {
        Some(Device::Vsource { kind, .. }) => *kind = SourceKind::Dc(amps),
        other => panic!("pass level {id:?} is not a voltage source: {other:?}"),
    }
}

/// Point one model-peripheral supply leg's Isource at `amps`.
fn set_isource_dc(circuit: &mut Circuit, id: DeviceId, amps: f64) {
    match circuit.devices.get_mut(id.0 as usize) {
        Some(Device::Isource { kind, .. }) => *kind = SourceKind::Dc(amps),
        Some(other) => panic!("model peripheral supply leg {id:?} changed type: {other:?}"),
        None => panic!("model peripheral supply leg {id:?} disappeared"),
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
    ev: &hauksbee_mcu::SpiEvent,
) -> u8 {
    let apply = |bus: &std::sync::Arc<std::sync::Mutex<crate::peripherals::SpiBus>>| -> u8 {
        let mut g = bus.lock_or_recover();
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
        if b.lock_or_recover().is_selected() {
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
    clock: &AvrClockPlan,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    instantiate_mcu_with_strap(binding, firmware, clock, None)
}

/// [`instantiate_mcu`] with the board's boot-strap levels handed to a backend
/// that latches them itself (Espressif's QEMU).
fn instantiate_mcu_with_strap(
    binding: &McuBinding,
    firmware: Option<&std::path::Path>,
    clock: &AvrClockPlan,
    strap: Option<&hauksbee_mcu::boot_mode::QemuStrapMode>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    instantiate_core(
        &binding.backend,
        (&binding.reference, &binding.requested_part),
        binding.clock.external_present(),
        firmware,
        clock,
        strap,
    )
}

/// [`instantiate_mcu_with_strap`] from the few binding fields it reads, so a
/// chip held in reset can carry them (owned) until its core is first needed.
fn instantiate_core(
    backend: &str,
    (reference, requested_part): (&str, &str),
    external_clock: bool,
    firmware: Option<&std::path::Path>,
    clock: &AvrClockPlan,
    strap: Option<&hauksbee_mcu::boot_mode::QemuStrapMode>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
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

    let mut core: Box<dyn Mcu + Send> = match BackendId::parse(backend) {
        Some(BackendId::Renode(part)) => instantiate_renode(part, external_clock)?,
        Some(BackendId::Qemu(part)) => instantiate_qemu(part, firmware, strap)?,
        Some(BackendId::None(family)) => {
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
        }
        Some(BackendId::Simavr(_)) => {
            let Some(hz) = clock.frequency_hz else {
                anyhow::bail!(
                    "{}",
                    clock
                        .guess
                        .clone()
                        .unwrap_or_else(|| format!("{reference}: no AVR core clock is known"))
                )
            };
            instantiate_avr(backend, hz)?
        }
        // ALLOWLIST, mirroring backend_is_external: an unknown backend token
        // must fail loud, never drift into the AVR path (wrong ISA).
        None => anyhow::bail!(
            "unknown MCU backend '{backend}': expected 'simavr:<part>', \
             'renode:<part>', or 'qemu:<part>'"
        ),
    };
    if let Some(fw) = firmware {
        // Name the footprint: the loader knows only the core it was given.
        core.load_firmware(fw).map_err(|e| {
            anyhow::anyhow!("{reference} ({requested_part}, simulated as {backend}): {e:#}")
        })?;
    }
    Ok(core)
}

/// MCU footprints left without a processor because each is an alternate
/// footprint of another, and why.
#[derive(Debug, Default)]
struct AlternateFootprints {
    /// References that get no core.
    dropped: HashSet<String>,
    /// One sentence per dropped footprint: which footprint it is an alternate
    /// of, which one runs the firmware, and how to choose the other.
    notes: Vec<String>,
}

/// GPIO pins (port, bit) of `binding` and the net each lands on, leaving out
/// ground and pads whose net reaches nothing but this MCU's own pin driver (an
/// unconnected pad, which KiCad gives a net of its own).
fn connected_gpio_nets(
    binding: &McuBinding,
    devices_on: &HashMap<NodeId, Vec<&str>>,
) -> HashMap<(char, u8), NodeId> {
    let own = [
        format!("Rdrv_{}_", binding.reference),
        format!("Vdrv_{}_", binding.reference),
    ];
    binding
        .gpio_drivers
        .iter()
        .filter(|(_, d)| !d.net.is_ground())
        .filter(|(_, d)| {
            devices_on.get(&d.net).is_some_and(|names| {
                names
                    .iter()
                    .any(|n| !own.iter().any(|prefix| n.starts_with(prefix.as_str())))
            })
        })
        .map(|(pin, d)| (*pin, d.net))
        .collect()
}

/// Whether two MCU bindings are footprints of ONE processor: the same emulator
/// family, and every GPIO both connect lands on the same net in both (at least
/// three of them, and at least half of the smaller one's connected GPIO). Two
/// separate processors share a bus at most; an alternate footprint (a
/// smaller-flash variant's pad pattern beside the bigger part's, a DIP beside
/// a TQFP) repeats the whole pin-to-net map, because only one of them is ever
/// assembled.
fn is_alternate_pair(a: &HashMap<(char, u8), NodeId>, b: &HashMap<(char, u8), NodeId>) -> bool {
    let mut same = 0usize;
    for (pin, net) in a {
        match b.get(pin) {
            Some(other) if other == net => same += 1,
            Some(_) => return false,
            None => {}
        }
    }
    same >= 3 && same * 2 >= a.len().min(b.len())
}

/// Group a board's MCU footprints that are alternates of one processor (see
/// [`is_alternate_pair`]) and keep one core per group: the first footprint, in
/// board order, whose part's flash holds `firmware` (a simavr part's flash is
/// read from the emulator; any other part counts as holding it), else the
/// first footprint, whose load then refuses the image with both sizes named.
/// A footprint the board marks do-not-fit never reaches the binder, so this
/// only meets footprints that are both nominally fitted.
fn alternate_footprints(
    mcus: &[McuBinding],
    circuit: &Circuit,
    firmware: &std::path::Path,
) -> AlternateFootprints {
    let mut out = AlternateFootprints::default();
    if mcus.len() < 2 {
        return out;
    }
    let mut devices_on: HashMap<NodeId, Vec<&str>> = HashMap::new();
    for device in &circuit.devices {
        for node in device.nodes() {
            devices_on.entry(node).or_default().push(device.name());
        }
    }
    let family = |b: &McuBinding| b.backend.split(':').next().unwrap_or("").to_string();
    let nets: Vec<_> = mcus
        .iter()
        .map(|b| connected_gpio_nets(b, &devices_on))
        .collect();
    // Union-find over alternate pairs, rooted at the earliest footprint.
    let mut root: Vec<usize> = (0..mcus.len()).collect();
    let find = |root: &[usize], mut i: usize| {
        while root[i] != i {
            i = root[i];
        }
        i
    };
    for i in 0..mcus.len() {
        for j in i + 1..mcus.len() {
            if family(&mcus[i]) == family(&mcus[j]) && is_alternate_pair(&nets[i], &nets[j]) {
                let (ri, rj) = (find(&root, i), find(&root, j));
                root[ri.max(rj)] = ri.min(rj);
            }
        }
    }
    let image_flash = image_flash_bytes(firmware);
    let holds = |b: &McuBinding| match (BackendId::parse(&b.backend), image_flash) {
        (Some(BackendId::Simavr(part)), Some(need)) => {
            simavr_flash_bytes(part).is_none_or(|have| need <= have)
        }
        _ => true,
    };
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in 0..mcus.len() {
        let r = find(&root, i);
        match groups.iter_mut().find(|g| g[0] == r) {
            Some(group) => group.push(i),
            None => groups.push(vec![i]),
        }
    }
    for group in groups.into_iter().filter(|g| g.len() > 1) {
        let keep = group
            .iter()
            .copied()
            .find(|&i| holds(&mcus[i]))
            .unwrap_or(group[0]);
        let kept = &mcus[keep];
        for &i in group.iter().filter(|&&i| i != keep) {
            let dropped = &mcus[i];
            let size = match image_flash {
                Some(need) if !holds(dropped) => format!(
                    " The {need}-byte firmware image does not fit the {}'s flash, so it                      could not run there anyway.",
                    dropped.requested_part
                ),
                _ => String::new(),
            };
            out.notes.push(format!(
                "{} ({}) is an alternate footprint of {} ({}): every GPIO both connect                  sits on the same net, so only one of them is assembled. {} runs the                  firmware and {} gets no processor in this run.{size} If {} is the part                  fitted, mark {} do-not-fit (DNP) and give firmware built for it.",
                dropped.reference,
                dropped.requested_part,
                kept.reference,
                kept.requested_part,
                kept.reference,
                dropped.reference,
                dropped.reference,
                kept.reference,
            ));
            out.dropped.insert(dropped.reference.clone());
        }
    }
    out
}

/// The flash an AVR firmware image needs, or `None` when it is not a readable
/// AVR image (another family's firmware, or a file its loader will refuse
/// with its own message).
fn image_flash_bytes(firmware: &std::path::Path) -> Option<u64> {
    hauksbee_mcu::avr_image::AvrImage::read(firmware)
        .ok()
        .map(|image| image.flash_bytes())
}

/// A simavr part's flash, read from the emulator's own part table.
#[cfg(feature = "avr")]
fn simavr_flash_bytes(part: &str) -> Option<u64> {
    hauksbee_mcu::avr_part_capacity(part).map(|c| c.flash)
}

/// Without the AVR backend no simavr part runs at all, so none is preferred.
#[cfg(not(feature = "avr"))]
const fn simavr_flash_bytes(_part: &str) -> Option<u64> {
    None
}

/// Build an Espressif-QEMU-backed core for a `qemu:<part>` backend string. The
/// firmware path (required; QEMU boots from it) is the app `.elf` or a merged
/// flash image; see `QemuBackend::new` for the two accepted shapes.
#[cfg(feature = "qemu")]
fn instantiate_qemu(
    part: &str,
    firmware: Option<&std::path::Path>,
    strap: Option<&hauksbee_mcu::boot_mode::QemuStrapMode>,
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
    Ok(Box::new(QemuBackend::new_with_strap(config, flash, strap)?))
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
    _strap: Option<&hauksbee_mcu::boot_mode::QemuStrapMode>,
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

/// Active-low reset roles an MCU card maps (`reset`, `run` on the RP2040...).
const RESET_ROLES: [&str; 4] = ["reset", "reset2", "pc6_reset", "run"];

/// The distinct non-ground nodes on an MCU's direct-supply pads.
///
/// A numbered pad of the same supply domain is stripped to its domain name
/// before the match: `vcc2`, `dvdd3` and `iovdd2` read as their domain, so a
/// rail reaching ONLY numbered pads is still found. A rail whose name
/// legitimately ends in a digit (`5v`) is in the list verbatim and matches
/// before the strip applies. `vin` and `raw` feed a module's onboard regulator
/// and are not the core's supply.
fn direct_supply_nodes(binding: &McuBinding) -> Vec<NodeId> {
    let mut nodes: Vec<NodeId> = binding
        .role_nets
        .iter()
        .filter(|(role, _)| {
            is_direct_supply_role(role)
                || is_direct_supply_role(role.trim_end_matches(|c: char| c.is_ascii_digit()))
        })
        .map(|(_, &node)| node)
        .filter(|node| !node.is_ground())
        .collect();
    nodes.sort_unstable_by_key(|n| n.0);
    nodes.dedup();
    nodes
}

/// Each direct-supply node's own datasheet floor: the strictest
/// `supply_range` row among the supply roles on it (`iovdd2` reads its
/// `iovdd` row). A node none of whose roles has a row is absent.
fn supply_floor_nodes(binding: &McuBinding) -> HashMap<NodeId, f64> {
    let mut floors: HashMap<NodeId, f64> = HashMap::new();
    for (role, &node) in &binding.role_nets {
        if node.is_ground() {
            continue;
        }
        let floor = binding.supply_floors.get(role.as_str()).or_else(|| {
            binding
                .supply_floors
                .get(role.trim_end_matches(|c: char| c.is_ascii_digit()))
        });
        if let Some(&f) = floor {
            floors
                .entry(node)
                .and_modify(|v| *v = v.max(f))
                .or_insert(f);
        }
    }
    floors
}

/// The circuit's DC operating point (node voltages, indexed by node), or `None`
/// when the solve does not converge.
pub(crate) fn dc_node_volts(circuit: &Circuit, opts: &SolverOptions) -> Option<Vec<f64>> {
    let mut circuit = circuit.clone();
    circuit.temp_c = opts.temperature_c;
    let mut ws = Workspace::new(&circuit);
    let seed = ws.x.clone();
    dc_operating_point_seeded(&mut ws, &circuit, opts, Some(&seed)).ok()?;
    let (x, layout) = (&ws.x, &ws.layout);
    Some(
        (0..circuit.node_count())
            .map(|node| {
                u32::try_from(node)
                    .ok()
                    .map(NodeId)
                    .and_then(|n| layout.node(n))
                    .map_or(0.0, |i| x[i])
            })
            .collect(),
    )
}

/// Net -> the devices touching it that are not an MCU pin's own driver legs:
/// the index [`net_has_drive_evidence`] reads.
pub(crate) fn drive_evidence_for(
    circuit: &Circuit,
    mcus: &[&McuBinding],
) -> HashMap<u32, Vec<u32>> {
    let mut pin_legs: HashSet<u32> = HashSet::new();
    let mut pad_nets: HashSet<u32> = HashSet::new();
    for m in mcus {
        for drv in m.gpio_drivers.values() {
            pin_legs.insert(drv.vsource.0);
            pin_legs.insert(drv.resistor.0);
            pad_nets.insert(drv.net.0);
        }
    }
    let mut evidence: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, d) in (0u32..).zip(&circuit.devices) {
        if pin_legs.contains(&i) {
            continue;
        }
        for n in d.nodes() {
            if n == NodeId::GROUND {
                continue;
            }
            evidence.entry(n.0).or_default().push(i);
        }
    }
    prune_dead_end_resistors(&circuit.devices, &pad_nets, &mut evidence);
    evidence
}

/// Drop the resistor evidence of a net whose resistive neighbourhood leads
/// nowhere. A resistor to a node nothing else touches (a series resistor to a
/// push-button that is open, a stub) carries no current, so the net it sits on
/// still floats and its ~0 V solve is the pins' own legs talking, not a drive.
///
/// The resistors under [`WEAK_DIGITAL_DRIVE_OHMS`] join their nodes into one
/// DC component. A component is anchored when it holds ground, a node touched
/// by a non-R/C device (a rail source, a diode, an inductor, a switch), or the pads of two
/// or more MCU pins (one MCU pin can drive another through a series resistor).
/// Only an anchored component keeps its resistors as drive evidence.
fn prune_dead_end_resistors(
    devices: &[Device],
    pad_nets: &HashSet<u32>,
    evidence: &mut HashMap<u32, Vec<u32>>,
) {
    fn find(parent: &mut HashMap<u32, u32>, x: u32) -> u32 {
        let mut root = x;
        while let Some(&p) = parent.get(&root) {
            if p == root {
                break;
            }
            root = p;
        }
        parent.insert(x, root);
        root
    }
    let low_ohm =
        |d: &Device| matches!(d, Device::Resistor { ohms, .. } if *ohms < WEAK_DIGITAL_DRIVE_OHMS);
    let mut parent: HashMap<u32, u32> = HashMap::new();
    let mut seen: HashSet<u32> = HashSet::new();
    for devs in evidence.values() {
        for &di in devs {
            if !seen.insert(di) {
                continue;
            }
            if let Some(Device::Resistor { a, b, .. }) =
                devices.get(di as usize).filter(|d| low_ohm(d))
            {
                parent.entry(a.0).or_insert(a.0);
                parent.entry(b.0).or_insert(b.0);
                let (ra, rb) = (find(&mut parent, a.0), find(&mut parent, b.0));
                if ra != rb {
                    parent.insert(ra, rb);
                }
            }
        }
    }
    let mut anchored: HashSet<u32> = HashSet::new();
    let mut pad_count: HashMap<u32, u32> = HashMap::new();
    let ground_root = parent.contains_key(&0).then(|| find(&mut parent, 0));
    anchored.extend(ground_root);
    for (&net, devs) in evidence.iter() {
        if !parent.contains_key(&net) {
            continue;
        }
        let root = find(&mut parent, net);
        if pad_nets.contains(&net) {
            *pad_count.entry(root).or_default() += 1;
        }
        if devs.iter().any(|&di| {
            devices
                .get(di as usize)
                .is_some_and(|d| !matches!(d, Device::Resistor { .. } | Device::Capacitor { .. }))
        }) {
            anchored.insert(root);
        }
    }
    anchored.extend(pad_count.iter().filter(|(_, &c)| c >= 2).map(|(&r, _)| r));
    for (&net, devs) in evidence.iter_mut() {
        if !parent.contains_key(&net) || anchored.contains(&find(&mut parent, net)) {
            continue;
        }
        devs.retain(|&di| !devices.get(di as usize).is_some_and(low_ohm));
    }
}

/// [`is_direct_supply_role`] for a role that may carry a domain number.
pub(crate) fn is_direct_supply_role_name(role: &str) -> bool {
    is_direct_supply_role(role)
        || is_direct_supply_role(role.trim_end_matches(|c: char| c.is_ascii_digit()))
}

/// Whether `net` has real drive evidence: a non-pin device with live
/// resistance under [`WEAK_DIGITAL_DRIVE_OHMS`], or any non-R/C device. A
/// floating net's ~0 V solve is the pins' own 1 GΩ legs talking.
pub(crate) fn net_has_drive_evidence(
    evidence: &HashMap<u32, Vec<u32>>,
    devices: &[Device],
    net: u32,
) -> bool {
    evidence.get(&net).is_some_and(|devs| {
        devs.iter().any(|&di| match devices.get(di as usize) {
            Some(Device::Resistor { ohms, .. }) => *ohms < WEAK_DIGITAL_DRIVE_OHMS,
            // A capacitor cannot decide a DC level.
            Some(Device::Capacitor { .. }) | None => false,
            Some(_) => true,
        })
    })
}

/// Whether a backend string names an EXTERNAL co-sim core (Renode, QEMU, or
/// anything future) rather than the in-process simavr backend.
///
/// simavr is the one full-stack backend: it models peripheral-slave coupling
/// and exposes pin drive direction. ALLOWLIST on purpose: an unknown future backend must fail SAFE
/// (classified external: capability-gated scaffolds, hedged diagnoses)
/// rather than inherit capabilities it does not have. Both hauksbee-ci's
/// scaffold gate and the runtime diagnosis route through this one predicate
/// so the two cannot drift (review finding on the ARM-honesty commit).
#[must_use]
pub fn backend_is_external(backend: &str) -> bool {
    hauksbee_mcu::backend_id::simavr_part(backend).is_none()
}

/// A fuse/board clock inconsistency on one AVR (see [`avr_clock_plan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvrClockFinding {
    /// The MCU's reference designator.
    pub reference: String,
    /// The full sentence, reference and fuse byte included.
    pub message: String,
}

/// Where an AVR core's clock came from, strongest evidence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvrClockBasis {
    /// Not an AVR backend: there is no clock to plan.
    NotAvr,
    /// The firmware's fuses, decoded against the board's crystal.
    Fuses,
    /// A crystal bound to XTAL1/XTAL2 whose value states its frequency, with no
    /// fuses to say otherwise.
    BoardCrystal,
    /// The card's `default_clock_hz`: a guess about the real chip.
    CardDefault,
    /// Nothing states a clock: no fuses, no crystal frequency, no card value.
    Unknown,
}

/// How an AVR core is clocked for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvrClockPlan {
    /// The core clock the co-sim runs at; `None` when nothing states one.
    pub frequency_hz: Option<u64>,
    /// Where `frequency_hz` came from.
    pub basis: AvrClockBasis,
    /// The fuses' clock selection disagrees with the board's XTAL pins.
    pub finding: Option<AvrClockFinding>,
    /// The sentence that discloses a guessed (or missing) clock; set exactly
    /// when `basis` is [`AvrClockBasis::CardDefault`] or [`AvrClockBasis::Unknown`].
    /// Every result that rests on the run carries it as a condition.
    pub guess: Option<String>,
}

/// Decide an AVR's co-sim clock. The one place the choice is made.
///
/// Precedence, first that states a frequency:
/// 1. the low fuse in the ELF `.fuse` section ([`hauksbee_mcu::avr_fuses`]): it
///    selects the clock source and the CKDIV8 prescaler, so the core runs at what
///    that produces on THIS board (the internal RC rate, or the fitted crystal's
///    parsed value ([`McuClock::crystal_hz`]), each divided by 8 under CKDIV8). A
///    selection that contradicts the board (a crystal source with no crystal or
///    oscillator on XTAL1/XTAL2, or a fitted one the fuses ignore for the
///    internal RC) is returned as a finding;
/// 2. with no usable fuses, the crystal the board binds to XTAL1/XTAL2;
/// 3. the card's `default_clock_hz` ([`ModelLibrary::mcu_default_clock_hz`]),
///    returned with a `guess` sentence;
/// 4. nothing: `frequency_hz` is `None`, `guess` says why, and the core is not
///    built. No frequency is invented.
///
/// [`McuClock::crystal_hz`]: hauksbee_bind::mcu_clock::McuClock::crystal_hz
/// [`ModelLibrary::mcu_default_clock_hz`]: hauksbee_models::ModelLibrary::mcu_default_clock_hz
#[must_use]
pub fn avr_clock_plan(binding: &McuBinding, firmware: Option<&std::path::Path>) -> AvrClockPlan {
    use hauksbee_bind::mcu_clock::ClockKind;
    use hauksbee_mcu::avr_fuses::{decode_clock, fmt_hz, read_elf_fuses};
    let plan = |frequency_hz, basis, finding, guess| AvrClockPlan {
        frequency_hz,
        basis,
        finding,
        guess,
    };
    let Some(part) = hauksbee_mcu::backend_id::simavr_part(&binding.backend) else {
        return plan(None, AvrClockBasis::NotAvr, None, None);
    };
    let fuses = firmware.and_then(|fw| read_elf_fuses(fw).ok().flatten());
    let select = fuses.and_then(|f| decode_clock(part, f.low).map(|select| (f, select)));
    // Only a decided clock verdict is checked: an unknown one (parts on the
    // XTAL pins the binder cannot read) or a broken one (already reported by
    // the clock evidence) says nothing about which source the fuses should
    // pick.
    let external = match binding.clock.kind {
        ClockKind::Crystal | ClockKind::External => Some(true),
        ClockKind::Internal => Some(false),
        // A module's own crystal is not a board part the fuses can be checked
        // against: the check stays silent, as it does for an unknown clock.
        ClockKind::Unknown | ClockKind::Broken | ClockKind::Module => None,
    };
    let finding = select.and_then(|(fuses, select)| {
        let why = external.and_then(|present| select.board_mismatch(present))?;
        Some(AvrClockFinding {
            reference: binding.reference.clone(),
            message: format!(
                "{} ({part}): low fuse 0x{:02X}: {why}",
                binding.reference, fuses.low
            ),
        })
    });
    let crystal_hz = binding
        .clock
        .crystal_hz
        .filter(|_| binding.clock.kind == ClockKind::Crystal);
    if let Some(hz) = select.and_then(|(_, select)| select.core_hz(crystal_hz)) {
        return plan(Some(hz), AvrClockBasis::Fuses, finding, None);
    }
    // Fuses that are absent let the board's crystal speak. Fuses that decode but
    // name a source whose frequency the board does not state leave the clock open.
    if select.is_none() {
        if let Some(hz) = crystal_hz {
            return plan(Some(hz), AvrClockBasis::BoardCrystal, finding, None);
        }
    }
    let gap = match (select, binding.clock.kind) {
        (Some(_), _) => {
            "the firmware's fuses select a clock source whose frequency the board does not state"
        }
        (None, ClockKind::Crystal) => {
            "the firmware carries no decodable fuses, and the board's crystal value states no frequency"
        }
        (None, ClockKind::External) => {
            "the firmware carries no decodable fuses, and the board's external oscillator states no frequency"
        }
        (None, _) => {
            "the firmware carries no decodable fuses and no crystal is bound to XTAL1/XTAL2"
        }
    };
    let reference = &binding.reference;
    let default = hauksbee_models::ModelLibrary::builtin_shared().mcu_default_clock_hz(
        &binding.backend,
        &binding.requested_part,
        binding.module,
    );
    let Some(hz) = default else {
        let guess = format!(
            "{reference} ({part}): {gap}, and no card states a default clock for it, so no \
             core clock is known and the core is not built. Bind a crystal whose value states \
             its frequency, set FUSES in the firmware ELF, or add `default_clock_hz` to the \
             part's card."
        );
        return plan(None, AvrClockBasis::Unknown, finding, Some(guess));
    };
    let prescaler = if binding.module {
        ""
    } else {
        " CKDIV8 is not assumed; a factory-fused part, which ships with it programmed, runs 8x slower."
    };
    let guess = format!(
        "{reference} ({part}): the core clock is a guess: {gap}, so it runs at the card's \
         default {}.{prescaler} Firmware timing (delays, baud rates, PWM periods) is off by \
         the ratio if the real part runs at another clock; bind a crystal whose value states \
         its frequency, or set FUSES in the firmware ELF, to remove this assumption.",
        fmt_hz(hz),
    );
    plan(Some(hz), AvrClockBasis::CardDefault, finding, Some(guess))
}

/// Build a simavr-backed AVR core for a `simavr:<part>` backend string.
///
/// The part token is passed STRAIGHT to simavr, which owns the list of cores it
/// has and returns a null MCU for anything else (`AvrMcu::new` turns that into a
/// named error). Silently substituting an `ATmega328P` for an unrecognised token
/// would be the wrong-ISA failure the QEMU and Renode paths above refuse by
/// name: an `ATmega1284P` board has ports A-D, 16 KB of SRAM and two USARTs, and
/// running its firmware on a 2 KB three-port core produces a plausible-looking
/// trace of a chip that is not on the board. A board asking for a core simavr
/// does not have fails loudly and says which cores it does have.
///
/// Every port the core has is hooked ([`AvrMcu::register_all_port_hooks`]): the
/// hook set is what connects firmware GPIO writes to the analog solve, and a
/// port the part lacks has a null IRQ that is skipped, so there is no per-part
/// port list to drift from the silicon.
///
/// The clock is the caller's: [`avr_clock_plan`] decides it, this function takes
/// it and keeps no default of its own.
#[cfg(feature = "avr")]
fn instantiate_avr(backend: &str, frequency_hz: u64) -> anyhow::Result<Box<dyn Mcu + Send>> {
    let part = hauksbee_mcu::backend_id::simavr_part(backend)
        .ok_or_else(|| anyhow::anyhow!("'{backend}' is not a 'simavr:<part>' backend"))?;
    let mut avr = AvrMcu::new(part, frequency_hz).map_err(|e| {
        anyhow::anyhow!(
            "{e}. The AVR co-sim runs whatever core simavr has, and will not \
             substitute a different one: an ATmega328P standing in for the part \
             the board fits would report a chip that is not on the board. The \
             parts with a built-in card are {}. Another core simavr has needs a \
             --models-dir card that sets `backend = \"simavr:<part>\"` explicitly \
             (and `default_clock_hz` for its clock).",
            hauksbee_models::ModelLibrary::builtin_shared()
                .simavr_parts()
                .join(", ")
        )
    })?;
    avr.register_all_port_hooks();
    Ok(Box::new(avr))
}

#[cfg(not(feature = "avr"))]
fn instantiate_avr(_backend: &str, _frequency_hz: u64) -> anyhow::Result<Box<dyn Mcu + Send>> {
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
    // What each renode backend actually loads: the normalised part prefixes
    // its platform stands for (a request starting with one is the modelled
    // core, possibly with a package/temperature suffix), the label to print,
    // and the family stem a same-family substitute shares.
    let (covers, label, stem): (&[&str], &str, &str) = match binding.backend.as_str() {
        // The stm32f4 / stm32f4_discovery backend always loads the F407 Discovery
        // core, regardless of which F4 variant the board specified.
        "renode:stm32f4" | "renode:stm32f4_discovery" => (&["STM32F407"], "STM32F407", "STM32F4"),
        "renode:stm32f103" => (&["STM32F103"], "STM32F103", "STM32F1"),
        // The composed G4 platform is the G474 die, which the G473/G483/G484
        // share; the G431/G441/G491 are smaller dies with fewer peripherals.
        "renode:stm32g4" => (
            &["STM32G473", "STM32G474", "STM32G483", "STM32G484"],
            "STM32G474",
            "STM32G4",
        ),
        // The F7 platform is the F746 map widened to the F76x/F77x RAM and
        // UART7/8; the F72x/F73x are a different, smaller die.
        "renode:stm32f7" => (
            &[
                "STM32F745",
                "STM32F746",
                "STM32F756",
                "STM32F765",
                "STM32F767",
                "STM32F769",
                "STM32F777",
                "STM32F779",
            ],
            "STM32F746/F767/F777 class",
            "STM32F7",
        ),
        // The L0 platform is Renode's STM32L071, standing for the L0x1 value
        // line; the L0x2/L0x3 add USB, LCD and AES it does not model.
        "renode:stm32l0" => (
            &[
                "STM32L011",
                "STM32L021",
                "STM32L031",
                "STM32L041",
                "STM32L051",
                "STM32L071",
                "STM32L081",
            ],
            "STM32L071 (L0x1 class)",
            "STM32L0",
        ),
        "renode:nrf52840" | "renode:nrf52" => (&["NRF52840"], "nRF52840", "NRF52"),
        "renode:sifive_fe310" | "renode:fe310" => (&["FE310"], "SiFive FE310", "FE3"),
        // simavr / qemu backends model the requested part directly; no
        // family-collapse substitution applies.
        _ => return None,
    };

    let norm = normalise_part(requested);
    // The board asked for a modelled core (possibly with a package/temp
    // suffix, e.g. STM32F407VGT6): not a substitution.
    if covers.iter().any(|c| norm.starts_with(c)) {
        return None;
    }
    // Only warn when the requested part looks like the SAME family but a
    // different member (e.g. STM32F411 vs STM32F407). A requested part that does
    // not share the modelled family's stem is a binding the router should not
    // have produced; do not invent a substitution narrative for it.
    if !norm.starts_with(stem) {
        return None;
    }
    Some(McuSubstitution {
        reference: binding.reference.clone(),
        backend: binding.backend.clone(),
        requested_part: requested.to_string(),
        modelled_core: label.to_string(),
    })
}

/// Uppercase + strip non-alphanumerics so "STM32F411RET6" and "stm32-f411"
/// compare equal up to a prefix.
fn normalise_part(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
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

/// An MCU pad whose drive comes from a peripheral output the co-sim does not
/// model as a level (see [`Scheduler::peripheral_pad_nets`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeripheralPad {
    /// The pad's own net.
    pub net: String,
    /// The MCU's reference.
    pub mcu: String,
    /// The pad's pin role, e.g. `gpio25`.
    pub pin: String,
    /// `None` when the backend saw a peripheral output routed onto the pad;
    /// otherwise why the backend could not observe peripheral routing for it
    /// (see `Mcu::peripheral_routing_unobserved`), so the firmware only *may*
    /// drive it.
    pub unobserved: Option<String>,
}

/// Whether a backend STRING names a core that can report pin drive direction,
/// decided from static data (no emulator is spawned).
///
/// The scaffold-time companion of [`Scheduler::drive_direction_observable`],
/// for callers (like `hauksbee-ci init`) that reason about a board before any
/// co-sim runs:
///   - `simavr:*`, true, the in-process core reads DDR;
///   - `renode:<part>`, true iff the part's `SoC` descriptor resolves and every
///     GPIO port carries a direction-register map (`dir = {...}`, verified
///     per-part; see `db/mcu/*.soc.toml`);
///   - anything else (QEMU, unknown futures), false, fail-safe.
#[must_use]
pub fn backend_reports_drive_direction(backend: &str) -> bool {
    if !backend_is_external(backend) {
        return true;
    }
    #[cfg(feature = "renode")]
    if let Some(BackendId::Renode(part)) = BackendId::parse(backend) {
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
    let gpio_maps = gpio_net_maps(mcus);

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

/// Each MCU's net-node -> (port, bit) GPIO map, the shape the 595 and 165
/// chain builders match a chain's control nets against.
fn gpio_net_maps(mcus: &[LiveMcu]) -> Vec<HashMap<i64, (char, u8)>> {
    mcus.iter()
        .map(|m| {
            m.binding
                .gpio_drivers
                .iter()
                .map(|(&pin, drv)| (i64::from(drv.net.0), pin))
                .collect()
        })
        .collect()
}

/// Whether every 595 chain whose parallel outputs feed a synchronous memory
/// has a control state the responder can prove before the first analogue solve.
/// Mutable OE can make the stored output byte electrically absent, and an
/// externally pulled control pin invalidates the chain controller's built-in
/// reset defaults. Chains not referenced by this memory are deliberately
/// ignored.
fn referenced_595_controls_are_proven(
    referenced: &std::collections::HashSet<usize>,
    controls: &[(Option<GpioPin>, Vec<GpioPin>)],
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
) -> (Vec<usize>, Vec<HashMap<GpioPin, NodeId>>) {
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
        .flat_map(|m| m.values().map(|n| i64::from(n.0)))
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
            seq_pins.contains(&role.as_str()) && driven_nets.contains(&i64::from(n.0))
        });
        if gpio_clocked {
            chips.push(i);
        }
    }
    (chips, pin_nets)
}

/// A responder's pin update in the MCU backend's drive vocabulary.
const fn pin_drive(update: crate::responders::InputDrive) -> hauksbee_mcu::PinDrive {
    let pin = PinId::new(update.pin.0, update.pin.1);
    match update.level {
        Some(high) => hauksbee_mcu::PinDrive::drive(pin, high),
        None => hauksbee_mcu::PinDrive::release(pin),
    }
}

/// Whether a pin role names a UART transmit line. The binder tags a pin whose
/// KiCad pin function is a UART/USART TX with a `_txd` suffix on its port-pin
/// role (`pd1_txd`, `p01_txd`, `pd3_txd1`).
fn is_uart_tx_role(role: &str) -> bool {
    role.to_ascii_lowercase().contains("_txd")
}

/// Whether a pin role names a UART transmit signal in any vendor spelling:
/// `pd1_txd`, `pa9_usart1_tx`, `uart0_tx`, `gpio16_txd0`, `d1_tx`.
fn is_uart_tx_net_role(role: &str) -> bool {
    role.to_ascii_lowercase().split('_').any(|word| {
        word.strip_prefix("tx").is_some_and(|rest| {
            rest.trim_start_matches('d')
                .chars()
                .all(|c| c.is_ascii_digit())
        })
    })
}

/// Whether a pin role carries one of the peripheral-signal `tokens` as a whole
/// `_`-separated word (`pc4_adc4_sda` has `sda`; `pd5_t1_oc0b` does not).
fn role_has_token(role: &str, tokens: &[&str]) -> bool {
    role.to_ascii_lowercase()
        .split('_')
        .any(|word| tokens.contains(&word))
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
        let mut sh = pin_sink.lock_or_recover();
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
            let mut b = bus.lock_or_recover();
            if asserted {
                b.cs_assert();
            } else {
                b.cs_deassert();
            }
        }
    }));
    let uart_sink = shared.clone();
    core.on_uart(Box::new(move |b: u8| {
        uart_sink.lock_or_recover().uart_out.push(b);
    }));
    // Tell a polling backend which ports the board actually wired, so it only
    // reads those output registers each chunk (no effect on push backends).
    let mut active_ports: Vec<char> = binding.gpio_drivers.keys().map(|(p, _)| *p).collect();
    active_ports.sort_unstable();
    active_ports.dedup();
    core.set_active_ports(&active_ports);
    let logic_high_v = logic_high_for_backend(&binding.backend);
    let node_index = |n: NodeId| usize::try_from(n.0).ok();
    let rails = crate::execution::RailWatch::new(
        RESET_ROLES
            .iter()
            .filter_map(|role| binding.role_nets.get(*role))
            .filter(|n| !n.is_ground())
            .filter_map(|&n| node_index(n))
            .collect(),
        direct_supply_nodes(&binding)
            .into_iter()
            .filter_map(node_index)
            .collect(),
    )
    .with_supply_floors(
        supply_floor_nodes(&binding)
            .into_iter()
            .filter_map(|(n, v)| Some((node_index(n)?, v)))
            .collect(),
    );
    LiveMcu {
        core,
        binding,
        shared,
        last_levels: HashMap::new(),
        configured_outputs: std::collections::HashSet::new(),
        logic_high_v,
        responder_input_pins: std::collections::HashSet::new(),
        digital_in_levels: HashMap::new(),
        uart_bytes: 0,
        drive_rows: HashMap::new(),
        drive_poll: 0,
        reset_gate: electrics::ResetGate::default(),
        i2c_events: Arc::new(AtomicU64::new(0)),
        spi_events: Arc::new(AtomicU64::new(0)),
        pin_toggles: std::collections::BTreeMap::new(),
        boot: None,
        rails,
    }
}

/// GPIO logic-high voltage by backend: STM32-class parts and the ESP32 family
/// are 3.3 V rails, the classic AVR parts are 5 V.
pub(crate) fn logic_high_for_backend(backend: &str) -> f64 {
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

/// Net names by node, indexed once (see `Scheduler::node_name_index`).
struct NodeNames(HashMap<u32, String>);

impl NodeNames {
    /// The name of the net on `node`, or `"node N"` for an unnamed one: what
    /// `Scheduler::net_name_of` answers.
    fn name(&self, node: u32) -> Cow<'_, str> {
        self.0.get(&node).map_or_else(
            || Cow::Owned(format!("node {node}")),
            |n| Cow::Borrowed(n.as_str()),
        )
    }
}

#[cfg(test)]
mod dead_end_resistor_tests {
    use super::*;

    fn resistor(a: u32, b: u32, ohms: f64) -> Device {
        Device::Resistor {
            name: format!("R{a}_{b}"),
            a: NodeId(a),
            b: NodeId(b),
            ohms,
            tc1: None,
        }
    }

    /// Evidence as `drive_evidence_for` builds it: every non-ground node a
    /// device touches maps to that device.
    fn evidence_of(devices: &[Device]) -> HashMap<u32, Vec<u32>> {
        let mut evidence: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, d) in (0u32..).zip(devices) {
            for n in d.nodes() {
                if n != NodeId::GROUND {
                    evidence.entry(n.0).or_default().push(i);
                }
            }
        }
        evidence
    }

    #[test]
    fn a_series_resistor_into_a_dead_end_is_not_drive_evidence() {
        // QSPI_SS (5) --1k-- BTN (6), the BOOTSEL switch open: nothing else on 6.
        let devices = [resistor(5, 6, 1_000.0)];
        let mut evidence = evidence_of(&devices);
        assert!(
            net_has_drive_evidence(&evidence, &devices, 5),
            "before pruning"
        );
        prune_dead_end_resistors(&devices, &HashSet::from([5]), &mut evidence);
        assert!(!net_has_drive_evidence(&evidence, &devices, 5));
        assert!(!net_has_drive_evidence(&evidence, &devices, 6));
    }

    #[test]
    fn a_resistor_to_ground_stays_drive_evidence() {
        // The same net with the switch closed: 1k then 0 ohm to ground is a real low.
        let devices = [resistor(5, 6, 1_000.0), resistor(6, 0, 1.0)];
        let mut evidence = evidence_of(&devices);
        prune_dead_end_resistors(&devices, &HashSet::from([5]), &mut evidence);
        assert!(net_has_drive_evidence(&evidence, &devices, 5));
    }

    #[test]
    fn a_pull_up_chain_to_a_rail_source_stays_drive_evidence() {
        let devices = [
            resistor(5, 7, 10_000.0),
            Device::Vsource {
                name: "V3V3".into(),
                p: NodeId(7),
                n: NodeId::GROUND,
                kind: SourceKind::Dc(3.3),
            },
        ];
        let mut evidence = evidence_of(&devices);
        prune_dead_end_resistors(&devices, &HashSet::from([5]), &mut evidence);
        assert!(net_has_drive_evidence(&evidence, &devices, 5));
    }

    #[test]
    fn a_resistor_between_two_mcu_pads_stays_drive_evidence() {
        // One pin can drive another through a series resistor.
        let devices = [resistor(5, 6, 330.0)];
        let mut evidence = evidence_of(&devices);
        prune_dead_end_resistors(&devices, &HashSet::from([5, 6]), &mut evidence);
        assert!(net_has_drive_evidence(&evidence, &devices, 5));
    }

    #[test]
    fn a_weak_resistor_never_counts() {
        let devices = [resistor(5, 6, 1e9)];
        let mut evidence = evidence_of(&devices);
        prune_dead_end_resistors(&devices, &HashSet::from([5]), &mut evidence);
        assert!(!net_has_drive_evidence(&evidence, &devices, 5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peripherals::controls::stamp_resistor;
    use crate::peripherals::test_support::FixedSpiSlave;
    use crate::responders::tests::one_bit_spec;
    use hauksbee_ir::SourceKind;

    /// A small valid AVR image for tests that exercise firmware-coupled pin
    /// replay. Firmware-less scheduler construction is deliberately passive;
    /// these tests need a live core, so they opt into an actual image instead
    /// of relying on the old implicit idle-core behaviour.
    #[cfg(feature = "avr")]
    fn avr_test_firmware() -> tempfile::NamedTempFile {
        use std::io::Write as _;

        let mut firmware = tempfile::Builder::new()
            .suffix(".hex")
            .tempfile()
            .expect("temporary AVR firmware");
        firmware
            .write_all(include_bytes!("../../hauksbee-ci/assets/firmware/demo.hex"))
            .expect("write AVR firmware");
        firmware
    }

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

    // Footprint, reference, value, x position; pads carry (number, y, net index).
    type FixturePart<'a> = (&'a str, &'a str, &'a str, u16, &'a [(u8, u8, usize)]);

    fn pcb_fixture(nets: &[&str], parts: &[FixturePart<'_>]) -> String {
        use std::fmt::Write as _;
        let mut pcb = String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n");
        for (id, name) in nets.iter().enumerate() {
            writeln!(pcb, "  (net {id} {name:?})").unwrap();
        }
        for (footprint, reference, value, x, pads) in parts {
            writeln!(pcb, "  (module {footprint} (layer F.Cu)\n    (at {x} 100)").unwrap();
            writeln!(
                pcb,
                "    (fp_text reference {reference} (at 0 0) (layer F.SilkS))"
            )
            .unwrap();
            writeln!(pcb, "    (fp_text value {value} (at 0 2) (layer F.Fab))").unwrap();
            for (number, y, net) in *pads {
                writeln!(
                    pcb,
                    "    (pad {number} thru_hole circle (at 0 {y}) (size 1 1) (net {net} {:?}))",
                    nets[*net]
                )
                .unwrap();
            }
            pcb.push_str("  )\n");
        }
        pcb.push_str(")\n");
        pcb
    }

    /// A passive +5V → R1(100Ω) → MID → R2(300Ω) → GND divider, no MCU.
    static DIVIDER_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "MID"],
            &[
                ("Resistor:R", "R1", "100", 110, &[(1, 0, 2), (2, 2, 3)]),
                ("Resistor:R", "R2", "300", 120, &[(1, 0, 3), (2, 2, 1)]),
            ],
        )
    });

    /// A live frame's notes name a supply-named rail nothing powers, the census
    /// hauksbee-ci banners, and say nothing of it once the rail is powered.
    #[test]
    fn a_live_frame_notes_an_unpowered_supply_rail() {
        use hauksbee_frontdoor_api::engine::Engine;
        const PADS: &[(u8, u8, usize)] = &[(1, 0, 2), (2, 2, 1)];
        let refs: Vec<String> = (1..=6).map(|i| format!("R{i}")).collect();
        for (rail, dead) in [("VDD_A", true), ("VDD_3V3", false)] {
            let parts: Vec<FixturePart<'_>> = (refs.iter().zip(100..))
                .map(|(r, x)| ("Resistor:R", r.as_str(), "10k", x, PADS))
                .collect();
            let text = pcb_fixture(&["", "GND", rail], &parts);
            let engine = crate::engine::HauksbeeEngine::from_board_file(&text, None, "");
            let (notes, said) = (engine.unwrap().step(1e-3).notes, format!("rail {rail}"));
            let named = notes.iter().any(|n| n.contains(&said));
            assert_eq!(named, dead, "{rail}: {notes:?}");
        }
    }

    /// A board with NO MCU module: two pulled nets (10 k to +5 V, 10 k to
    /// GND) plus a pulled-high net standing in for a responder-owned MISO.
    static PLAIN_INPUT_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "BTN_HI", "BTN_LO", "RESP"],
            &[
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 2), (2, 2, 3)]),
                ("Resistor:R2", "R2", "10k", 120, &[(1, 0, 4), (2, 2, 1)]),
                ("Resistor:R3", "R3", "10k", 130, &[(1, 0, 2), (2, 2, 5)]),
            ],
        )
    });

    /// A 74HC74 whose clock (pad 3 = `clk1`) sits on STROBE and data (pad 2 =
    /// `d1`) on DATA, plus a FREE net wired only to a pull-down.
    static PULSE_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "STROBE", "DATA", "FREE"],
            &[
                (
                    "Logic:74HC74",
                    "U5",
                    "74HC74",
                    100,
                    &[(2, 2, 4), (3, 3, 3), (7, 7, 1), (14, 14, 2)],
                ),
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 5), (2, 2, 1)]),
            ],
        )
    });

    /// A 74HC08 output (pad 3 = `y1`) on net SHARED that firmware may also
    /// drive as a GPIO output.
    static CONTENTION_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "SHARED", "INA", "INB"],
            &[
                (
                    "Logic:74HC08",
                    "U1",
                    "74HC08",
                    100,
                    &[(1, 1, 4), (2, 2, 5), (3, 3, 3), (7, 7, 1), (14, 14, 2)],
                ),
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 4), (2, 2, 1)]),
                ("Resistor:R2", "R2", "10k", 120, &[(1, 0, 5), (2, 2, 1)]),
            ],
        )
    });

    /// A 74HC125 whose `y1` shares BUS with a firmware pin but whose `oe_n_1`
    /// is tied HIGH (released).
    static TRISTATE_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "BUS", "INA"],
            &[
                (
                    "Logic:74HC125",
                    "U2",
                    "74HC125",
                    100,
                    &[(1, 1, 2), (2, 2, 4), (3, 3, 3), (7, 7, 1), (14, 14, 2)],
                ),
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 4), (2, 2, 1)]),
            ],
        )
    });

    #[path = "electrics.rs"]
    mod electrics_tests;

    // ── Shared fixtures ──────────────────────────────────────────────────────

    use hauksbee_mcu::traits::{
        I2cCb, InputResponderCb as EdgeResponder, InputResponderDirectionCb as DirectionResponder,
        PinChangeCb, SpiCb, UartCb,
    };

    /// One configurable trait-level core standing in for every backend shape
    /// the scheduler has to handle. Every field is a knob; the default is a
    /// faithful, silent, 16 MHz core that records nothing.
    #[derive(Default)]
    #[allow(clippy::struct_excessive_bools)] // independent test knobs, not a state machine
    struct MockCore {
        digital_ins: Arc<Mutex<Vec<(GpioPin, bool)>>>,
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
        reset_unsupported: bool,
        reset_fails: bool,
        i2c_cb: Arc<Mutex<Option<I2cCb>>>,
        spi_cb: Arc<Mutex<Option<SpiCb>>>,
        pin_cb: Arc<Mutex<Option<PinChangeCb>>>,
        i2c_addrs: Arc<Mutex<Vec<u8>>>,
        /// Models no bus controller, drops ADC channel 0 and carries a
        /// watchdog and timing limitation (the nRF52/ESP32 Renode/QEMU shape).
        bus_blind: bool,
        watchdog_resets: u64,
        /// Pins the core reports as routed to a peripheral output (a PWM, timer
        /// or RMT waveform through a pin matrix), latched.
        routed: Vec<(char, u8)>,
        /// Pins with no output stage (`pin_can_drive` false).
        input_only: Vec<(char, u8)>,
        /// Reports peripheral routing for every pin (the trait default is
        /// blind).
        routing_seen: bool,
        /// Implements the synchronous single-slot input responder hooks.
        legacy: bool,
        responder_installs: Arc<std::sync::atomic::AtomicUsize>,
        responder: Arc<Mutex<Option<EdgeResponder>>>,
        direction_responder: Arc<Mutex<Option<DirectionResponder>>>,
        /// Peripheral registers `read_register` answers from (drive-select
        /// fields, by address).
        registers: Arc<Mutex<HashMap<u32, u32>>>,
    }

    const WATCHDOG_LIMITATION: &str =
        "The nRF52840 watchdog arms in this co-simulator (it reads back as running, \
         with a correct 32768 Hz reload) but never fires: an unserviced watchdog will \
         NOT reset the core, so watchdog recovery is untested on this run.";
    const TIMING_LIMITATION: &str =
        "ESP32 virtual time is paced by the host wall clock in this co-simulator, \
         so simulated time is approximate and host-load dependent.";

    impl MockCore {
        fn handles(&self) -> Self {
            Self {
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
                registers: self.registers.clone(),
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
            self.micros.lock_or_recover().push(us);
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
                .lock_or_recover()
                .push(((pin.port, pin.bit), high));
        }
        fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}
        fn on_pin_change(&mut self, cb: PinChangeCb) {
            *self.pin_cb.lock_or_recover() = Some(cb);
        }
        fn on_input_responder(&mut self, responder: EdgeResponder) {
            if self.legacy {
                self.responder_installs
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.responder.lock_or_recover() = Some(responder);
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
                *self.direction_responder.lock_or_recover() = Some(responder);
            }
        }
        fn uart_write(&mut self, _bytes: &[u8]) {}
        fn on_uart(&mut self, _cb: UartCb) {}
        fn on_i2c(&mut self, cb: I2cCb) {
            *self.i2c_cb.lock_or_recover() = Some(cb);
        }
        fn on_spi(&mut self, cb: SpiCb) {
            *self.spi_cb.lock_or_recover() = Some(cb);
        }
        fn set_i2c_slave_addresses(&mut self, addresses: &[u8]) {
            *self.i2c_addrs.lock_or_recover() = addresses.to_vec();
        }
        fn read_register(&mut self, addr: u32) -> Option<u32> {
            self.registers.lock_or_recover().get(&addr).copied()
        }
        fn supports_reset(&self) -> bool {
            !self.reset_unsupported
        }
        fn reset(&mut self) -> anyhow::Result<()> {
            *self.was_reset.lock().unwrap() = true;
            self.cycles = 0;
            self.calls = 0;
            if std::mem::take(&mut self.reset_fails) {
                anyhow::bail!("injected reset failure");
            }
            Ok(())
        }
        fn drive_direction_observable(&self) -> bool {
            self.direction_observable
        }
        fn pins_routed_to_peripheral(&self) -> Vec<hauksbee_mcu::PinId> {
            self.routed
                .iter()
                .map(|&(port, bit)| hauksbee_mcu::PinId { port, bit })
                .collect()
        }
        fn peripheral_routing_unobserved(&self, _pin: hauksbee_mcu::PinId) -> Option<String> {
            (!self.routing_seen).then(|| "the mock reports no routing".to_string())
        }
        fn pin_can_drive(&self, pin: hauksbee_mcu::PinId) -> bool {
            !self.input_only.contains(&(pin.port, pin.bit))
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
        fn clock_host_paced(&self) -> bool {
            self.bus_blind
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
            clock: hauksbee_bind::mcu_clock::McuClock::default(),
            internal_clock: None,
            pad_roles: HashMap::new(),
            role_nets: HashMap::new(),
            gpio_drivers,
            adc_nets: HashMap::new(),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
            supply_stand_in: None,
            min_supply_v: None,
            supply_floors: HashMap::new(),
            electrical: hauksbee_bind::mcu_electrical::McuElectrical::default(),
        }
    }

    fn board_scheduler(text: &str) -> Scheduler {
        let board = hauksbee_extract::ExtractedBoard::from_auto(text).expect("board");
        let bound =
            hauksbee_bind::binder::bind_board(&board, &hauksbee_models::ModelLibrary::builtin());
        Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler")
    }

    /// Add one node per name to `circuit`, returning the nodes in the order
    /// the names were given and the name-to-node map a bound board carries.
    fn named_nodes<const N: usize>(
        circuit: &mut Circuit,
        names: [&str; N],
    ) -> ([NodeId; N], HashMap<String, NodeId>) {
        let mut by_name = HashMap::new();
        let nodes = names.map(|name| {
            let id = circuit.node(name);
            by_name.insert(name.to_string(), id);
            id
        });
        (nodes, by_name)
    }

    pub(super) fn bound_board(
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
            unsupported_mcus: Vec::new(),
            component_kinds: HashMap::new(),
            input_sources: HashMap::new(),
            supplies: Vec::new(),
            behavioral: Vec::new(),
            device_meta,
            dacs: Vec::new(),
            peripherals: Vec::new(),
            latching_switches: false,
            mcu_boot: HashMap::new(),
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

    /// A `PLAIN_INPUT_BOARD` scheduler with one mock MCU whose GPIO drivers sit
    /// on fresh per-pin nets. Returns the core's shared handles.
    fn sched_with_core(core: MockCore, pins: &[(char, u8)]) -> (Scheduler, MockCore) {
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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

    /// A `PLAIN_INPUT_BOARD` scheduler whose single live MCU is a bus-blind core
    /// with ADC channel 0 bound to `adc_net`.
    fn sched_with_bus_blind_core(adc_net: &str) -> Scheduler {
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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

    /// +5V -> U1 (3.3 V) -> R1 -> U2 (1.8 V) cascade powering mock MCU U9; U3 is a
    /// -5 V part returned to +3V; U4/U5 feed each other and nothing else.
    static REGULATOR_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let reg = |r, v, pads: &'static [_]| ("Package_TO_SOT_SMD:SOT-223", r, v, 100, pads);
        pcb_fixture(
            &[
                "", "GND", "+5V", "VOUT", "BIN", "LOW", "-5V", "+3V", "NEG", "A", "B",
            ],
            &[
                reg("U1", "AMS1117-3.3", &[(1, 1, 1), (2, 2, 3), (3, 3, 2)]),
                ("Resistor:R", "R1", "10", 110, &[(1, 0, 3), (2, 2, 4)]),
                reg("U2", "TLV1117-18", &[(1, 1, 1), (2, 2, 5), (3, 3, 4)]),
                reg("U3", "LM7905", &[(1, 1, 7), (2, 2, 6), (3, 3, 8)]),
                reg("U4", "L7805", &[(1, 1, 10), (2, 2, 1), (3, 3, 9)]),
                reg("U5", "AMS1117-3.3", &[(1, 1, 1), (2, 2, 10), (3, 3, 9)]),
            ],
        )
    });

    fn regulator_sched(volts: f64) -> (Scheduler, MockCore) {
        let mut sched = board_scheduler(&REGULATOR_BOARD);
        let gpio = sched.circuit.node("CS_B0");
        sched.net_nodes.insert("CS_B0".into(), gpio);
        let drivers = tristated_drivers(&mut sched, &[(('B', 0), gpio)]);
        let mut mcu = binding("U9", "simavr:test", drivers);
        mcu.role_nets.insert("vcc".into(), sched.net_nodes["LOW"]);
        let core = MockCore::default();
        let handles = core.handles();
        push_core(&mut sched, core, mcu);
        sched.relayout();
        assert!(sched.set_power_supply("+5V", PowerSupply::Ideal { volts }));
        (sched, handles)
    }

    #[test]
    fn regulation_table() {
        use hauksbee_ir::evidence::AssumptionKind::{NotExercised, ReducedFidelity};
        let sum = |t: &RegulationTaint| format!("{} <- {}", t.nets.join(","), t.refs.join(","));
        // U3 (negative, local return) is supported in every row; the U4/U5 cycle
        // never is, and the fixpoint still terminates.
        for (volts, starved, says) in [
            (5.0, false, "declare the supply that feeds B"),
            (4.4, true, "+5V is 4.400 V above its return, with"),
            (
                2.5,
                true,
                "needs (3.399 V upper output limit + 1.300 V declared dropout",
            ),
        ] {
            let (mut sched, core) = regulator_sched(volts);
            sched.establish_regulation();
            // Decided before any MCU advance, and never by seeding the real state.
            assert!(core.micros.lock().unwrap().is_empty() && sched.last_dc_seed.is_none());
            assert!(sched.node_volts.iter().all(|v| *v == 0.0), "{volts}");
            sched.step(1e-3);
            let taint = sched.regulation_taint();
            let tainted: Vec<_> = taint.iter().map(|t| t.refs[0].as_str()).collect();
            let want: &[_] = if starved {
                &["U1", "U2", "U4", "U5"]
            } else {
                &["U4", "U5"]
            };
            assert_eq!(tainted, want, "{volts}");
            assert!(taint.iter().all(|t| t.since_s == 0.0) && taint[0].reason.contains(says));
            // 10 uV: the part's 1 mA quiescent current now flows through the
            // 1 mOhm of the rail that holds NEG, about 1 uV.
            assert!(
                (sched.net_voltage("NEG").unwrap() + 2.0).abs() < 1e-5,
                "{volts}"
            );
            let kinds = sched.forced_voltage_assumptions().unwrap();
            let kinds: Vec<_> = kinds.iter().map(Assumption::kind).collect();
            let u12 = if starved {
                NotExercised
            } else {
                ReducedFidelity
            };
            assert_eq!(
                kinds,
                [u12, u12, ReducedFidelity, NotExercised, NotExercised]
            );
            if starved {
                // U1's taint crosses R1 and stops at U2's output; U2's reaches the MCU.
                assert_eq!(sum(&taint[0]), "BIN,VOUT <- U1");
                assert_eq!(sum(&taint[1]), "CS_B0,LOW <- U2,U9");
                assert!(taint[1]
                    .reason
                    .contains("BIN has no independent DC support"));
            } else {
                assert!((sched.net_voltage("VOUT").unwrap() - 3.3).abs() < 1e-6);
            }
        }
        // This dip bypasses `set_power_supply`, so only the monitor can catch it;
        // neither the recovery nor a reset restores trust, in U1 or downstream.
        let (mut sched, _core) = regulator_sched(5.0);
        sched.step(1e-3);
        let leg = sched.supplies.iter_mut().find(|s| s.net_name == "+5V");
        leg.unwrap()
            .reconfigure(&mut sched.circuit, PowerSupply::Ideal { volts: 2.5 });
        sched.step(1e-3);
        sched.set_power_supply("+5V", PowerSupply::Ideal { volts: 5.0 });
        sched.step(1e-3);
        sched.reset_run_state().unwrap();
        sched.step(1e-3);
        let taint = sched.regulation_taint();
        assert_eq!(taint.len(), 4, "{taint:?}");
        assert_eq!(
            (sum(&taint[2]), sum(&taint[3])),
            ("BIN,VOUT <- U1".into(), "CS_B0,LOW <- U2,U9".into())
        );
        assert!(taint[2].reason.contains(", first at t=0.00") && taint[2].since_s > 0.0);
        assert_eq!(sched.reg_supported, [false, false, true, false, false]);
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
        let mut sh = sched.mcus[0].shared.lock_or_recover();
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
        let mut bus = sched.i2c_buses()[0].lock_or_recover();
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
        assert_eq!(idle["powered"].to_bits(), 1.0_f64.to_bits());
        assert!((idle["supply_v"] - 3.3).abs() < 1e-6, "{idle:?}");
        assert!((idle["supply_current_a"] - 6e-6).abs() < 1e-12, "{idle:?}");

        let mut bus = sched.i2c_buses()[0].lock_or_recover();
        bus.dispatch(hauksbee_mcu::I2cEvent::Start {
            addr: 0x50,
            read: true,
        });
        assert_eq!(
            bus.dispatch(hauksbee_mcu::I2cEvent::Read { addr: 0x50 }),
            Some(0xff)
        );
        drop(bus);
        sched.step(DEFAULT_CHUNK_S);
        let read = sched.peripheral_states()["U1"].clone();
        assert!((read["supply_current_a"] - 1e-3).abs() < 1e-12, "{read:?}");

        // A brown rail disables both electrical draw and protocol response.
        let vcc = sched.net_nodes["+3V3"].0 as usize;
        sched.node_volts[vcc] = 1.0;
        sched.gate_model_peripherals_from_rails();
        let off = sched.peripheral_states()["U1"].clone();
        assert_eq!(off["powered"].to_bits(), 0);
        assert_eq!(off["supply_current_a"].to_bits(), 0);
        let mut bus = sched.i2c_buses()[0].lock_or_recover();
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
        drop(bus);
    }

    #[test]
    fn spi_deep_power_down_selects_the_datasheet_current_state() {
        let mut sched = board_scheduler(POWERED_FLASH_BOARD);
        assert_eq!(sched.spi_buses().len(), 1, "exact flash model attaches");
        let current = |sched: &Scheduler| sched.peripheral_states()["U1"]["supply_current_a"];
        let command = |sched: &Scheduler, byte: u8| {
            let mut bus = sched.spi_buses()[0].lock_or_recover();
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



    /// The G4/F7/L0 platforms stand for a named set of dies: a covered part
    /// (with any package suffix) is the modelled core, a same-family part off
    /// the set is a disclosed substitution, another family is neither.
    #[test]
    fn stm32_g4_f7_l0_substitution_follows_the_covered_dies() {
        let sub = |backend: &str, part: &str| {
            let mut b = binding("U1", backend, HashMap::new());
            b.requested_part = part.into();
            detect_substitution(&b).map(|s| s.modelled_core)
        };
        assert_eq!(sub("renode:stm32g4", "STM32G474RET6"), None);
        assert_eq!(sub("renode:stm32g4", "STM32G473CBT6"), None);
        assert_eq!(
            sub("renode:stm32g4", "STM32G431KB").as_deref(),
            Some("STM32G474")
        );
        assert_eq!(sub("renode:stm32f7", "STM32F777VIT6"), None);
        assert_eq!(sub("renode:stm32f7", "STM32F746NG"), None);
        assert_eq!(
            sub("renode:stm32f7", "STM32F722RE").as_deref(),
            Some("STM32F746/F767/F777 class")
        );
        assert_eq!(sub("renode:stm32l0", "STM32L011K4T6"), None);
        assert_eq!(
            sub("renode:stm32l0", "STM32L073RZ").as_deref(),
            Some("STM32L071 (L0x1 class)")
        );
        assert_eq!(sub("renode:stm32l0", "STM32F103C8"), None, "another family");
        assert_eq!(
            sub("renode:stm32f4", "STM32F411RET6").as_deref(),
            Some("STM32F407")
        );
    }

    /// An `ATmega328P` whose board clock verdict is `kind`, with a crystal of
    /// `crystal_hz` when one is fitted.
    fn avr_binding(
        kind: hauksbee_bind::mcu_clock::ClockKind,
        crystal_hz: Option<u64>,
    ) -> McuBinding {
        let mut b = binding("U1", "simavr:atmega328p", HashMap::new());
        b.requested_part = "ATmega328P-AU".into();
        b.clock = hauksbee_bind::mcu_clock::McuClock {
            kind,
            crystal_hz,
            ..Default::default()
        };
        b
    }

    fn fused_firmware(low: u8) -> tempfile::TempPath {
        let mut f = tempfile::Builder::new().suffix(".elf").tempfile().unwrap();
        std::io::Write::write_all(
            &mut f,
            &hauksbee_mcu::avr_fuses::fuse_only_elf(&[low, 0xDE, 0xFD]),
        )
        .unwrap();
        f.into_temp_path()
    }

    /// Fuses that contradict the board are reported, both ways round, and the
    /// core runs at the clock the fuses actually produce on this board.
    #[test]
    fn avr_fuses_inconsistent_with_the_board_are_reported() {
        use hauksbee_bind::mcu_clock::ClockKind;
        // Crystal selected (0xFF), nothing on XTAL1/XTAL2.
        let fw = fused_firmware(0xFF);
        let plan = avr_clock_plan(&avr_binding(ClockKind::Internal, None), Some(&fw));
        let f = plan.finding.expect("crystal fuses with no crystal fitted");
        assert!(f.message.contains("no crystal"), "{}", f.message);
        assert!(f.message.contains("0xFF"), "{}", f.message);

        // Internal 8 MHz RC with CKDIV8 (factory 0x62) on a 16 MHz crystal board:
        // reported, and the core runs at the fused 1 MHz, not the crystal's 16.
        let fw = fused_firmware(0x62);
        let plan = avr_clock_plan(
            &avr_binding(ClockKind::Crystal, Some(16_000_000)),
            Some(&fw),
        );
        assert!(plan.finding.unwrap().message.contains("crystal is unused"));
        assert_eq!(plan.frequency_hz, Some(1_000_000));
        assert_eq!(plan.basis, AvrClockBasis::Fuses);
    }

    /// The legal controls: consistent fuses produce no finding and the fused
    /// frequency; an undecided board clock is not second-guessed.
    #[test]
    fn avr_fuses_consistent_with_the_board_run_at_the_fused_clock() {
        use hauksbee_bind::mcu_clock::ClockKind;
        let fw = fused_firmware(0xFF);
        let plan = avr_clock_plan(&avr_binding(ClockKind::Crystal, Some(8_000_000)), Some(&fw));
        assert_eq!(plan.finding, None);
        assert_eq!(plan.frequency_hz, Some(8_000_000));
        assert_eq!(
            plan.guess, None,
            "fuses state the clock: nothing is guessed"
        );

        let fw = fused_firmware(0x7F); // crystal + CKDIV8
        let plan = avr_clock_plan(
            &avr_binding(ClockKind::Crystal, Some(16_000_000)),
            Some(&fw),
        );
        assert_eq!(plan.finding, None);
        assert_eq!(plan.frequency_hz, Some(2_000_000));

        let fw = fused_firmware(0xE2); // internal 8 MHz RC, no crystal
        let plan = avr_clock_plan(&avr_binding(ClockKind::Internal, None), Some(&fw));
        assert_eq!(plan.finding, None);
        assert_eq!(plan.frequency_hz, Some(8_000_000));

        // Crystal fuses on a board whose clock verdict is undecided and which
        // states no crystal frequency: not judged, and no frequency from the
        // fuses, so the card's default is used and SAID to be a guess.
        let fw = fused_firmware(0xFF);
        let plan = avr_clock_plan(&avr_binding(ClockKind::Unknown, None), Some(&fw));
        assert_eq!(plan.finding, None, "an unknown board clock is not judged");
        assert_eq!(plan.basis, AvrClockBasis::CardDefault);
        assert_eq!(plan.frequency_hz, Some(8_000_000));
        assert!(plan
            .guess
            .expect("a guessed clock is disclosed")
            .contains("fuses"),);
    }

    /// No fuses and no crystal: the card's `default_clock_hz` (DS40002061A
    /// section 9.2.1, PDF page 37: the `ATmega328P`'s internal RC is 8.0 MHz) is
    /// used, not the old 16 MHz, and the guess is stated with the number, the
    /// part and the CKDIV8 caveat.
    #[test]
    fn an_avr_with_no_crystal_and_no_fuses_runs_at_the_cards_internal_clock_and_says_so() {
        use hauksbee_bind::mcu_clock::ClockKind;
        let plan = avr_clock_plan(&avr_binding(ClockKind::Internal, None), None);
        assert_eq!(plan.finding, None);
        assert_eq!(plan.basis, AvrClockBasis::CardDefault);
        assert_eq!(plan.frequency_hz, Some(8_000_000));
        let guess = plan.guess.expect("the guess is disclosed");
        for needle in ["U1", "atmega328p", "guess", "8 MHz", "CKDIV8", "FUSES"] {
            assert!(guess.contains(needle), "{needle}: {guess}");
        }
        // A bound crystal whose value states no frequency does not speak.
        let plan = avr_clock_plan(&avr_binding(ClockKind::Crystal, None), None);
        assert_eq!(plan.basis, AvrClockBasis::CardDefault);
        assert!(plan
            .guess
            .unwrap()
            .contains("crystal value states no frequency"));
    }

    /// With no fuses, the board's crystal is the clock, whatever the card's
    /// default: a 12 MHz and a 20 MHz crystal board run at 12 and 20 MHz, and
    /// nothing is guessed.
    #[test]
    fn an_avr_with_a_bound_crystal_and_no_fuses_runs_at_the_crystal() {
        use hauksbee_bind::mcu_clock::ClockKind;
        for hz in [12_000_000, 16_000_000, 20_000_000] {
            let plan = avr_clock_plan(&avr_binding(ClockKind::Crystal, Some(hz)), None);
            assert_eq!(plan.frequency_hz, Some(hz));
            assert_eq!(plan.basis, AvrClockBasis::BoardCrystal);
            assert_eq!(plan.guess, None);
        }
    }

    /// A module's card (Arduino Nano: `ATmega328` "runs at a clock frequency of
    /// 16 MHz", A000005 section 3.2, PDF page 8) keeps its 16 MHz without a
    /// crystal on the bare chip's pins, and its sentence has no CKDIV8 caveat.
    #[test]
    fn an_arduino_module_runs_at_its_cards_16_mhz() {
        use hauksbee_bind::mcu_clock::ClockKind;
        let mut b = avr_binding(ClockKind::Unknown, None);
        b.requested_part = "Arduino_Nano".into();
        b.module = true;
        let plan = avr_clock_plan(&b, None);
        assert_eq!(plan.frequency_hz, Some(16_000_000));
        assert_eq!(plan.basis, AvrClockBasis::CardDefault);
        assert!(!plan.guess.unwrap().contains("CKDIV8"));
    }

    /// A part no card states a clock for refuses to build: no constant fills in.
    #[test]
    fn an_avr_core_no_card_clocks_is_not_built_with_an_invented_clock() {
        use hauksbee_bind::mcu_clock::ClockKind;
        let mut b = avr_binding(ClockKind::Internal, None);
        b.backend = "simavr:atmega4809".into();
        b.requested_part = "ATmega4809".into();
        let plan = avr_clock_plan(&b, None);
        assert_eq!(plan.frequency_hz, None);
        assert_eq!(plan.basis, AvrClockBasis::Unknown);
        let guess = plan.guess.expect("the refusal says why");
        assert!(guess.contains("default_clock_hz"), "{guess}");
        let err = instantiate_mcu(&b, None, &avr_clock_plan(&b, None))
            .err()
            .expect("no clock, no core")
            .to_string();
        assert!(err.contains("no core clock is known"), "{err}");
    }

    /// Non-AVR backends have no AVR clock plan.
    #[test]
    fn a_non_avr_backend_has_no_avr_clock_plan() {
        let b = binding("U1", "renode:stm32f4", HashMap::new());
        let plan = avr_clock_plan(&b, None);
        assert_eq!(plan.basis, AvrClockBasis::NotAvr);
        assert_eq!((plan.frequency_hz, plan.guess), (None, None));
    }

    /// Core cycles a board's AVR executes in 1 ms of SIMULATED time: the core is
    /// built exactly as a co-sim run builds it (plan, then `instantiate_mcu`) and
    /// the firmware is an `rjmp .-2` loop in Intel HEX. The count is a property
    /// of simulated time, not of how busy the machine is, so it needs no gate.
    #[cfg(feature = "avr")]
    fn cycles_in_one_simulated_ms(binding: &McuBinding) -> u64 {
        let mut f = tempfile::Builder::new().suffix(".hex").tempfile().unwrap();
        std::io::Write::write_all(&mut f, b":02000000FFCF30\n:00000001FF\n").unwrap();
        let hex = f.into_temp_path();
        let clock = avr_clock_plan(binding, Some(&hex));
        let mut core = instantiate_mcu(binding, Some(&hex), &clock).expect("build the AVR core");
        let start = core.state().cycles;
        core.run_micros(1000).expect("run 1 ms");
        core.state().cycles - start
    }

    /// The board's clock reaches the core: a bare 8 MHz internal-RC chip runs
    /// 8000 cycles per simulated millisecond (so a 1 ms firmware delay is 1 ms,
    /// and a 9600 baud UART divisor computed for 8 MHz holds), a 20 MHz crystal
    /// board runs 20000, and a Nano module keeps its 16000.
    #[cfg(feature = "avr")]
    #[test]
    fn the_planned_avr_clock_is_the_clock_the_core_runs_at() {
        use hauksbee_bind::mcu_clock::ClockKind;
        // `rjmp` is 2 cycles, so a chunk may overshoot by one instruction.
        let near = |got: u64, want: u64| (want..=want + 2).contains(&got);

        let internal = avr_binding(ClockKind::Internal, None);
        let got = cycles_in_one_simulated_ms(&internal);
        assert!(near(got, 8_000), "8 MHz internal RC: {got} cycles per ms");

        let crystal = avr_binding(ClockKind::Crystal, Some(20_000_000));
        let got = cycles_in_one_simulated_ms(&crystal);
        assert!(
            near(got, 20_000),
            "20 MHz crystal board: {got} cycles per ms"
        );

        let mut nano = avr_binding(ClockKind::Unknown, None);
        nano.requested_part = "Arduino_Nano".into();
        nano.module = true;
        let got = cycles_in_one_simulated_ms(&nano);
        assert!(near(got, 16_000), "Nano module: {got} cycles per ms");
    }

    #[test]
    fn public_mcu_binding_literal_remains_source_compatible() {
        let mut b = binding("U1", "renode:stm32f4", HashMap::new());
        b.requested_part = "STM32F411RET6".into();
        let substitution = detect_substitution(&b).expect("F411 uses the F407 stand-in");
        let public_event = substitution;
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
    /// the available cores named, never silently substituted by an `ATmega328P`.
    #[cfg(feature = "avr")]
    #[test]
    fn the_avr_backend_builds_the_part_the_board_asked_for_or_refuses() {
        // The parts under test are the parts the cards name, so a card with a
        // misspelt part fails here and a new card is covered without an edit.
        let parts = hauksbee_models::ModelLibrary::builtin_shared().simavr_parts();
        assert!(
            parts.len() >= 6,
            "the AVR cards name their parts: {parts:?}"
        );
        for part in &parts {
            assert!(
                instantiate_avr(&format!("simavr:{part}"), 8_000_000).is_ok(),
                "simavr knows {part}"
            );
        }
        let err = instantiate_avr("simavr:atmega4809", 8_000_000)
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
            supply: None,
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
        let mut sched = board_scheduler(&DIVIDER_BOARD);
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

    /// An LED's frame peak is the current its series resistor carries, not
    /// Shockley evaluated at the terminal voltage, which includes the card's
    /// `rs` drop and read a 25 mA indicator as hundreds of milliamps.
    #[test]
    fn an_led_peak_is_its_series_resistor_current() {
        let board = pcb_fixture(
            &["", "GND", "+5V", "ANODE"],
            &[
                ("Resistor:R", "R1", "120", 110, &[(1, 0, 2), (2, 2, 3)]),
                (
                    "LED_THT:LED_D5.0mm",
                    "D1",
                    "RED_LED",
                    120,
                    &[(1, 0, 1), (2, 2, 3)],
                ),
            ],
        );
        let mut sched = board_scheduler(&board);
        sched.step(1e-3);
        let peak = |r: &str| sched.frame_peak_current()[r];
        let (led, res) = (peak("D1"), peak("R1"));
        assert!(
            res > 0.015 && res < 0.03,
            "R1 carries an indicator's current: {res}"
        );
        assert!(
            (led - res).abs() < 0.02 * res,
            "D1 peak {led} A vs R1 {res} A"
        );
    }

    // ── AVR-bound electrical proofs (need the in-process simavr core) ────────

    /// The Arduino Nano module "A1" at x = 100 with the given pads.
    #[cfg(feature = "avr")]
    fn nano(pads: &'static [(u8, u8, usize)]) -> FixturePart<'static> {
        ("Module:Arduino_Nano", "A1", "Arduino_Nano", 100, pads)
    }

    /// A Nano module driving net CLK from A2 (PC2) into an RC integrator
    /// (10k into 100 nF, tau = 1 ms).
    #[cfg(feature = "avr")]
    static RC_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "CLK", "MID"],
            &[
                nano(&[(4, 4, 1), (27, 27, 2), (21, 21, 3)]),
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 3), (2, 2, 4)]),
                ("Capacitor:C", "C1", "100nF", 120, &[(1, 0, 4), (2, 2, 1)]),
            ],
        )
    });

    /// A Nano driving SER/SRCLK/RCLK of a real bound 74HC595 whose Q0/Q7 are
    /// pulled down.
    #[cfg(feature = "avr")]
    static CHAIN_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "SER", "SRCLK", "RCLK", "Q0", "Q7"],
            &[
                nano(&[
                    (4, 4, 1),
                    (27, 27, 2),
                    (14, 14, 3),
                    (16, 16, 4),
                    (15, 15, 5),
                ]),
                (
                    "Logic:SN74HC595",
                    "U2",
                    "74HC595",
                    120,
                    &[
                        (14, 0, 3),
                        (11, 1, 4),
                        (12, 2, 5),
                        (15, 3, 6),
                        (7, 4, 7),
                        (16, 5, 2),
                        (8, 6, 1),
                    ],
                ),
                ("Resistor:R", "R1", "10k", 130, &[(1, 0, 6), (2, 2, 1)]),
                ("Resistor:R2", "R2", "10k", 140, &[(1, 0, 7), (2, 2, 1)]),
            ],
        )
    });

    /// A Nano driving net BUS from A2 (PC2) into a plain 10k pull-down.
    #[cfg(feature = "avr")]
    static RELEASE_BOARD: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        pcb_fixture(
            &["", "GND", "+5V", "BUS"],
            &[
                nano(&[(4, 4, 1), (27, 27, 2), (21, 21, 3)]),
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 3), (2, 2, 1)]),
            ],
        )
    });

    /// Bind `board` and promote the given Nano pins to driven-low outputs,
    /// exactly as their first firmware edges would.
    ///
    /// The image is not optional. Firmware-less scheduler construction is
    /// deliberately passive and installs no MCU at all, so a `None` here would
    /// leave `sched.mcus` empty and every assertion below reading a bare board.
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
        let firmware = avr_test_firmware();
        Scheduler::new(bound, Some(firmware.path()), SolverOptions::default()).expect("scheduler")
    }

    /// A firmware-shaped shiftOut(MSBFIRST, 0xA6) bit-bang latches a REAL
    /// bound 74HC595 through its electrical nets: the latched byte is read
    /// back from the solved node voltages of the output nets.
    #[cfg(feature = "avr")]
    #[test]
    fn cosim_bitbang_595_latches_through_bound_nets() {
        // SER on D11/PB3, SRCLK on D13/PB5, RCLK on D12/PB4.
        let mut sched = nano_scheduler(&CHAIN_BOARD, &[('B', 3), ('B', 5), ('B', 4)]);

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

        let mut sched = nano_scheduler(&RC_BOARD, &[('C', 2)]);
        assert!(sched.solve_chunk(chunk), "control solve converges");
        let mid_dc = sched.net_voltage("MID").expect("MID");
        assert!(
            mid_dc.abs() < 0.05,
            "DC control must leave the RC at ~0 V, got {mid_dc:.3}"
        );

        let mut sched = nano_scheduler(&RC_BOARD, &[('C', 2)]);
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
        let mut sched = nano_scheduler(&RELEASE_BOARD, &[]);
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
                    ('B', u8::try_from(index).unwrap()),
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
            circuit.add(Device::vsource(
                "VSTATIC",
                vcc,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
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
                ro_strong: 50.0,
                supply: None,
            },
            roles,
            drivers: HashMap::new(),
            logic: Some(logic),
            supply: None,
            rails: hauksbee_bind::digital::RailLevels::default(),
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

        let mut slot = m.edge.lock_or_recover();
        let callback = slot.as_mut().expect("legacy singleton callback installed");
        m.direction
            .lock_or_recover()
            .as_mut()
            .expect("direction callback installed")(&[(PinId::new('B', 0), true, false)], 9);
        let _ = callback(PinId::new('B', 0), false, 10);
        assert_eq!(
            callback(PinId::new('B', 0), true, 11),
            vec![hauksbee_mcu::PinDrive::drive(PinId::new('B', 1), false)],
            "a sub-chunk pulse must synchronously write and return the new memory bit"
        );
        drop(slot);
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
        assert!(m.edge.lock_or_recover().is_none());
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
        // ONE_PIN_MEMORY with its data input moved onto its own "data" net.
        let spec = ONE_PIN_MEMORY
            .replace(r#"["gnd", "we_n"]"#, r#"["gnd", "data", "we_n"]"#)
            .replace(r#"data_in = ["gnd"]"#, r#"data_in = ["data"]"#);
        let mut m =
            scheduler_with_legacy_memory(&spec, &["gnd", "data", "we_n", "io0"], &["we_n", "io0"]);
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
                levels: m.sched.digital[0].levels.clone(),
                roles: HashMap::from([("qa".to_string(), we_node)]),
                drivers: HashMap::from([("qa".to_string(), output)]),
                logic: None,
                supply: None,
                rails: hauksbee_bind::digital::RailLevels::default(),
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
        let spec = ONE_PIN_MEMORY.replace(
            "outputs = [\"io0\"]",
            "outputs = [\"io0\", \"status\"]\ncomb = { status = \"we_n\" }",
        );
        let mut m = scheduler_with_legacy_memory(
            &spec,
            &["gnd", "we_n", "io0", "status"],
            &["we_n", "io0"],
        );
        m.sched.build_and_install_parallel_memories();
        assert!(m.sched.parallel_memory_chips.is_empty());
    }

    /// Specs with no usable firmware trigger: a write pin shorted to ground,
    /// an all-static responder, and a power-on-active read the responder
    /// cannot seed. None may be claimed.
    #[test]
    fn memories_without_a_reachable_trigger_are_not_claimed() {
        let gnd_write = r#"{ pin = "gnd", edge = "rising" }"#;
        let we_write = r#"{ pin = "we_n", edge = "rising" }"#;
        let grounded_write = one_bit_spec(
            gnd_write,
            "",
            r#"{ pin = "gnd", active = "high" }"#,
            r#""gnd""#,
        );
        let all_static = one_bit_spec(
            gnd_write,
            "",
            r#"{ pin = "gnd", active = "low" }"#,
            r#""gnd""#,
        );
        let power_on_read = one_bit_spec(
            we_write,
            "",
            r#"{ pin = "we_n", active = "low" }"#,
            r#""gnd", "we_n""#,
        );
        for (spec, roles, gpio) in [
            (grounded_write, &["gnd", "io0"][..], &["gnd", "io0"][..]),
            (all_static, &["gnd", "io0"], &["io0"]),
            (power_on_read, &["gnd", "we_n", "io0"], &["we_n", "io0"]),
        ] {
            let mut m = scheduler_with_legacy_memory(&spec, roles, gpio);
            m.sched.build_and_install_parallel_memories();
            assert!(m.sched.parallel_memory_chips.is_empty(), "{spec}");
        }
    }

    /// A real 5 V static node is still zero in the pre-solve snapshot and must
    /// not be responder-owned.
    #[test]
    fn static_high_node_is_not_claimed_from_the_unsolved_zero_snapshot() {
        let spec = ONE_PIN_MEMORY.replace("gnd", "vcc");
        let mut m = scheduler_with_legacy_memory(&spec, &["vcc", "we_n", "io0"], &["we_n", "io0"]);
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
            let we_n = m.sched.net_nodes["we_n"];
            stamp_resistor(&mut m.sched.circuit, format!("Rbias{index}"), a, we_n, ohms);
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
        let seeded = one_bit_spec(
            r#"{ pin = "we_n", edge = "rising" }"#,
            "",
            r#"{ pin = "we_n", active = "high" }, { pin = "gnd", active = "high" }"#,
            r#""gnd", "we_n""#,
        );
        assert!(pulled_memory(&seeded, &[(true, 10_000.0)])
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
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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

        let calls = digital_ins.lock_or_recover().clone();
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

    fn cs_frames(sched: &Scheduler, m: usize) -> usize {
        sched.mcus[m].shared.lock_or_recover().cs_frames.len()
    }

    /// A CS pin two MCUs both own frames on exactly one of them: the MCU that
    /// drives the CS net when that is known, else the first owner.
    #[test]
    fn cs_frame_installs_on_exactly_one_mcu_preferring_the_cs_net_driver() {
        let cs_pin = ('C', 2u8);
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
        let resp_node = sched.net_nodes["RESP"];
        for _ in 0..2 {
            let drivers = tristated_drivers(&mut sched, &[(cs_pin, resp_node)]);
            push_core(
                &mut sched,
                MockCore::default(),
                binding("U1", "simavr:test", drivers),
            );
        }
        let bus = Arc::new(Mutex::new(SpiBus::new(
            "U9",
            Box::new(FixedSpiSlave::new(0)),
        )));
        sched.register_cs_frame(&bus, Some(cs_pin), None);
        assert_eq!(cs_frames(&sched, 0) + cs_frames(&sched, 1), 1);

        // Same pin on DIFFERENT nets: MCU_0 on an unrelated net, MCU_1 on the
        // real CS net. The frame must land on MCU_1.
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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
        let bus = Arc::new(Mutex::new(SpiBus::new(
            "U9",
            Box::new(FixedSpiSlave::new(0)),
        )));
        sched.register_cs_frame(&bus, Some(cs_pin), Some(cs_net));
        assert_eq!(cs_frames(&sched, 0), 0);
        assert_eq!(cs_frames(&sched, 1), 1);
    }

    /// `pin_driving_node` must return the lowest (port, bit) on a shared net
    /// regardless of `HashMap` iteration order.
    #[test]
    fn pin_driving_node_is_deterministic_when_multiple_pins_share_a_net() {
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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
        sched.stats.get(net).map_or(0, |s| s.toggles)
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

    /// `net_range` keeps a sag that recovers, and its floor skips a power-on
    /// ramp (samples before `powered_from_s`) while its ceiling does not.
    #[test]
    fn net_range_keeps_a_recovered_sag_but_not_the_power_on_ramp() {
        let (mut sched, _h) = sched_with_core(MockCore::default(), &[]);
        count_toggles(&mut sched, "VDD", &[3.3, 2.9, 3.3]);
        assert_eq!(sched.net_range("VDD"), Some((2.9, 3.3)));
        let (mut sched, _h) = sched_with_core(MockCore::default(), &[]);
        sched.powered_from_s = 1e-3;
        count_toggles(&mut sched, "VDD", &[0.0, 1.6]);
        assert_eq!(sched.net_range("VDD"), None);
        sched.sim_time = 1e-3;
        count_toggles(&mut sched, "VDD", &[3.3, 3.2]);
        assert_eq!(sched.net_range("VDD"), Some((3.2, 3.3)));
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
        assert_eq!(sched.host_paced_mcus(), vec!["U1".to_string()]);
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
        assert!(sched.host_paced_mcus().is_empty());
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

        let addrs = h.i2c_addrs.lock_or_recover().clone();
        assert!(
            addrs.contains(&0x50) && addrs.contains(&0x48),
            "{addrs:#04x?}"
        );

        let mut slot = h.i2c_cb.lock_or_recover();
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

        assert_eq!(read_back, Some(0xAB));
        let b1 = bus1.lock_or_recover();
        assert_eq!(b1.slave::<Eeprom24c>(0x50).unwrap().contents()[0], 0xAB);
        drop(b1);
        assert!(bus2
            .lock_or_recover()
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
        let cs1 = ('C', 0u8);
        let cs2 = ('C', 1u8);
        let (mut sched, h) = sched_with_core(MockCore::default(), &[cs1, cs2]);
        let (slave1, slave2) = (FixedSpiSlave::new(0x5A), FixedSpiSlave::new(0xA5));
        let (got1, got2) = (slave1.got.clone(), slave2.got.clone());
        let bus = |slave: FixedSpiSlave| Arc::new(Mutex::new(SpiBus::new("U", Box::new(slave))));
        let resolved = |pin| {
            Some(ResolvedCs {
                pin,
                net: None,
                provenance: CsProvenance::SpecDeclared,
            })
        };
        sched.attach_spi_bus(bus(slave1), resolved(cs1));
        sched.attach_spi_bus(bus(slave2), resolved(cs2));

        let edge = |pin: (char, u8), high: bool| {
            let mut slot = h.pin_cb.lock_or_recover();
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
            let mut slot = h.spi_cb.lock_or_recover();
            (slot.as_mut().expect("on_spi handler installed"))(hauksbee_mcu::SpiEvent {
                mosi,
                deselect: false,
                cycle: 0,
            })
        };
        let bytes = |g: &Arc<Mutex<Vec<u8>>>| g.lock_or_recover().clone();

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
        let total = micros.lock_or_recover().iter().sum();
        total
    }

    /// `run_micros` takes integer microseconds: fractional and sub-microsecond
    /// chunks must carry their remainder so the firmware clock neither drifts
    /// behind (per-chunk rounding) nor races ahead (a min-1 clamp).
    #[test]
    fn fractional_and_sub_microsecond_chunks_track_true_elapsed_time() {
        let delivered = delivered_micros(1.3e-6, 10);
        assert!(delivered.abs_diff(13) <= 1, "{delivered} us vs 13.0 true");
        assert!(
            delivered >= 12,
            "per-chunk rounding undercounts: {delivered}"
        );

        let delivered = delivered_micros(0.5e-6, 20);
        assert!(delivered.abs_diff(10) <= 1, "{delivered} us vs 10.0 true");
        assert!(delivered <= 11, "min-1 clamp races ahead: {delivered}");
    }

    #[test]
    fn scheduler_error_budget_uses_measured_run_not_defaults() {
        let (mut sched, _micros) = sched_with_micros_core(false);
        sched.opts.reltol = 2.5e-4;
        sched.opts.vntol = 7.5e-7;
        sched.run_chunk(1e-4, &mut HashMap::new());

        let budget = sched.error_budget().expect("valid scheduler budget");
        assert_eq!(budget.tolerance().reltol().to_bits(), 2.5e-4_f64.to_bits());
        assert_eq!(budget.tolerance().vntol().to_bits(), 7.5e-7_f64.to_bits());
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

        sched.reset_run_state().unwrap();
        assert_eq!(sched.sim_time.to_bits(), 0);
        assert_eq!(sched.failed_chunk_count(), 0);
        assert!(sched.failed_windows().is_empty());
        assert!(sched.analog_valid());
        assert!(!sched.analog_abort_tripped());
    }

    #[test]
    fn grounded_reset_refuses_firmware_before_any_backend_starts() {
        let firmware = tempfile::NamedTempFile::new().unwrap();
        for role in [
            "reset",
            "reset2",
            "pc6_reset",
            "run",
            "gpio0",
            "reset_status",
            "en",
        ] {
            for grounded in [true, false] {
                let build = || {
                    let mut circuit = Circuit::new();
                    let node = if grounded {
                        NodeId::GROUND
                    } else {
                        circuit.node("RESET")
                    };
                    let mut bound =
                        bound_board("reset", circuit, HashMap::new(), Vec::new(), Vec::new());
                    // The first core must not even reach backend dispatch if a
                    // later binding makes the firmware run impossible.
                    bound
                        .mcus
                        .push(binding("U1", "invalid-sentinel", HashMap::new()));
                    let mut second = binding("U2", "invalid-sentinel", HashMap::new());
                    second.role_nets.insert(role.to_string(), node);
                    bound.mcus.push(second);
                    bound
                };
                assert!(Scheduler::new(build(), None, SolverOptions::default()).is_ok());
                let error =
                    Scheduler::new(build(), Some(firmware.path()), SolverOptions::default())
                        .err()
                        .expect("refusal")
                        .to_string();
                let held = grounded && matches!(role, "reset" | "reset2" | "pc6_reset" | "run");
                assert_eq!(
                    error.contains("wired directly to ground"),
                    held,
                    "{role}: {error}"
                );
                if held {
                    assert!(
                        error.contains("U2")
                            && error.contains(role)
                            && error.contains("firmware-less")
                    );
                } else {
                    assert!(error.contains("unknown MCU backend"), "{role}: {error}");
                }
            }
        }
    }

    #[test]
    fn pcb_numbered_reset_pins_reach_firmware_preflight() {
        let lib = hauksbee_models::ModelLibrary::builtin();
        let firmware = tempfile::NamedTempFile::new().unwrap();
        for (value, footprint, pad, reset_role) in [
            (
                "RP2040",
                "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm",
                "26",
                Some("run"),
            ),
            ("Pico", "Module:RaspberryPi_Pico", "30", Some("run")),
            (
                "RP2040",
                "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm",
                "1",
                None,
            ),
            ("Pico", "Module:RaspberryPi_Pico", "26", None),
        ] {
            // No pinfunction hints: the curated package/header map must carry
            // the electrical role. The return name is not a reset-net heuristic.
            let pcb = format!(
                r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
                (net 0 "") (net 1 "GND")
                (module "{footprint}" (layer F.Cu) (at 0 0)
                    (fp_text reference U1 (at 0 0) (layer F.SilkS))
                    (fp_text value "{value}" (at 0 2) (layer F.Fab))
                    (pad "{pad}" thru_hole circle (at 0 0) (size 1 1) (net 1 "GND"))))"#
            );
            let board = hauksbee_extract::ExtractedBoard::from_auto(&pcb).unwrap();
            let build = || hauksbee_bind::binder::bind_board(&board, &lib);
            let bound = build();
            assert_eq!(bound.mcus.len(), 1);
            let grounded_reset = bound.mcus[0]
                .role_nets
                .get("run")
                .is_some_and(|n| n.is_ground());
            assert_eq!(grounded_reset, reset_role.is_some(), "{value} pad {pad}");
            assert!(Scheduler::new(bound, None, SolverOptions::default()).is_ok());
            if reset_role.is_some() {
                let error =
                    Scheduler::new(build(), Some(firmware.path()), SolverOptions::default())
                        .err()
                        .expect("grounded reset refusal")
                        .to_string();
                assert!(
                    error.contains("U1") && error.contains("wired directly to ground"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one scripted reset scenario reads best as a single sequence
    fn reset_refusals_preserve_history_and_block_partial_cores() {
        for (unsupported, fails) in [(true, 2), (false, 1), (false, 2)] {
            let first = MockCore {
                reset_fails: fails == 1,
                ..Default::default()
            };
            let (mut sched, h1) = sched_with_core(first, &[]);
            let second = MockCore {
                reset_unsupported: unsupported,
                reset_fails: fails == 2,
                ..Default::default()
            };
            let h2 = second.handles();
            push_core(&mut sched, second, binding("U2", "test", HashMap::new()));
            sched.relayout();
            sched.step(0.001);
            sched.failed_windows.push((0.0, 0.001));
            sched.last_solve_error = Some("old analog failure".into());
            sched.mcus[0].last_levels.insert(('B', 5), true);
            sched.mcus[0]
                .shared
                .lock()
                .unwrap()
                .uart_out
                .extend_from_slice(b"held");
            sched.frame_peak_current.insert("R1".into(), 2.0);
            sched
                .faults_pending
                .push(hauksbee_bind::shorts::short_fault("A", "B", 0.001));
            let snapshot = |s: &Scheduler| {
                format!(
                    "{:?}",
                    (
                        s.sim_time,
                        &s.node_volts,
                        &s.failed_windows,
                        &s.last_solve_error,
                        &s.frame_v_extremes,
                        &s.stats,
                        &s.frame_peak_current,
                        s.failed_chunks,
                        &s.faults_pending,
                        &s.mcus[0].shared.lock().unwrap().uart_out
                    )
                )
            };
            let before = snapshot(&sched);
            let old_time = sched.sim_time;
            assert!(sched
                .reset_run_state()
                .unwrap_err()
                .contains(if fails == 1 { "U1" } else { "U2" }));
            assert_eq!(*h1.was_reset.lock().unwrap(), !unsupported);
            assert_eq!(*h2.was_reset.lock().unwrap(), !unsupported && fails == 2);
            assert_eq!(sched.mcus[0].core.state().pc == 0, !unsupported);
            assert!(sched.mcus[0].last_levels[&('B', 5)]);
            assert_eq!(sched.reset_failure().is_some(), !unsupported);
            if !unsupported {
                let calls = h1.micros.lock().unwrap().len();
                for _ in 0..2 {
                    assert_eq!(sched.step(0.1).sim_time.to_bits(), old_time.to_bits());
                }
                assert_eq!(h1.micros.lock().unwrap().len(), calls);
            }
            assert_eq!(before, snapshot(&sched));
            if !unsupported {
                use hauksbee_frontdoor_api::engine::Engine;
                let bound = bound_board(
                    "guard",
                    Circuit::new(),
                    HashMap::new(),
                    Vec::new(),
                    Vec::new(),
                );
                let mut engine =
                    crate::engine::HauksbeeEngine::from_bound(bound, None, "").unwrap();
                std::mem::swap(engine.scheduler_mut(), &mut sched);
                assert!(engine.run_failure().is_some());
                let stopped = engine.step(0.1);
                assert!(stopped.component_states.is_empty() && stopped.faults.is_empty());
                assert_eq!(before, snapshot(engine.scheduler()));
                std::mem::swap(engine.scheduler_mut(), &mut sched);
                let held_failure = sched.reset_failure.clone();
                push_core(
                    &mut sched,
                    MockCore {
                        reset_unsupported: true,
                        ..Default::default()
                    },
                    binding("U3", "unsupported", HashMap::new()),
                );
                assert!(sched.reset_run_state().is_err());
                assert_eq!(sched.reset_failure, held_failure);
                assert_eq!(before, snapshot(&sched));
                sched.mcus.pop();
                sched.responder_registries.pop();
                sched.reset_run_state().unwrap();
                assert!(sched.reset_failure().is_none());
                assert_eq!(sched.sim_time.to_bits(), 0);
                assert!(sched.failed_windows.is_empty());
                assert!(sched.step(0.001).sim_time > 0.0);
            }
        }
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

        sched.reset_run_state().unwrap();

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
        drop(sh);
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
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
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
        circuit.add(Device::vsource(
            "Vsupply_VCC",
            res,
            NodeId::GROUND,
            SourceKind::Dc(3.3),
        ));
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

    /// Firmware reaches what its pins control (a MOSFET load switch's output,
    /// a regulator's rail through its enable, a load on that rail) and not what
    /// feeds it: a pull-up to a held rail does not move the rail, and the MCU's
    /// own supply net, which its pin drivers draw from, is a load, not a path.
    #[test]
    fn firmware_influence_follows_control_paths_and_stops_at_held_rails() {
        let mut circuit = Circuit::new();
        let ([gpio, gate, load, vin, v3v3, vdd_mcu, avdd, en2, vsw, sense], net_nodes) =
            named_nodes(
                &mut circuit,
                [
                    "GPIO", "GATE", "LOAD", "VIN", "V3V3", "VDD_MCU", "AVDD", "EN2", "VSW", "SENSE",
                ],
            );
        let gnd = NodeId::GROUND;
        // VIN: an independent supply. V3V3: an always-on regulator from VIN
        // that feeds the MCU through a ferrite (VDD_MCU) and an RC-filtered
        // analogue rail (AVDD). VSW: a second regulator whose enable EN2 the
        // firmware drives, loaded by SENSE.
        circuit.add(Device::vsource("Vin", vin, gnd, SourceKind::Dc(5.0)));
        let reg_u2 = circuit.add(Device::vsource("Vreg_U2", v3v3, gnd, SourceKind::Dc(3.3)));
        let reg_u3 = circuit.add(Device::vsource("Vreg_U3", vsw, gnd, SourceKind::Dc(1.8)));
        for (name, a, b, ohms) in [
            ("FB1", v3v3, vdd_mcu, 0.1),
            ("R_AVDD", v3v3, avdd, 10.0),
            ("R_GATE", gpio, gate, 100.0),
            ("R_PD", gate, gnd, 100e3),
            ("R_PU", gpio, v3v3, 10e3),
            ("R_LOAD", vin, load, 50.0),
            ("R_SENSE", vsw, sense, 1e3),
            ("R_SENSE_PD", sense, gnd, 1e3),
        ] {
            circuit.add(Device::resistor(name, a, b, ohms));
        }
        circuit.add(Device::capacitor("C_AVDD", avdd, gnd, 1e-6));
        circuit.add(Device::Mosfet {
            name: "Q1".into(),
            d: load,
            g: gate,
            s: gnd,
            b: None,
            model: hauksbee_ir::MosfetModel::default(),
        });
        let bound = bound_board("influence", circuit, net_nodes, Vec::new(), Vec::new());
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        for (reference, source, enable) in [("U2", reg_u2, vec![]), ("U3", reg_u3, vec![en2])] {
            sched.regulators.push(SimpleRegulator {
                reference: reference.into(),
                source,
                input: vin,
                min_in_v: 0.0,
                max_in_v: f64::INFINITY,
                dropout_v: None,
                adj_return: None,
                helpers: Vec::new(),
                pass_level: None,
                enable,
                gate: None,
            });
            sched.reg_supported.push(true);
        }
        let mut drivers = HashMap::new();
        for (pin, node, tag) in [(('B', 0), gpio, "GPIO"), (('B', 1), en2, "EN2")] {
            let mut drv = hauksbee_bind::drivers::PinDriver::stamp(
                &mut sched.circuit,
                node,
                tag,
                tag,
                hauksbee_bind::drivers::DEFAULT_RO,
            );
            drv.route_supply(&mut sched.circuit, tag, vdd_mcu, gnd);
            drivers.insert(pin, drv);
        }
        let mut mcu = binding("U1", "qemu:test", drivers);
        mcu.role_nets.extend([
            ("pb0".to_string(), gpio),
            ("pb1".to_string(), en2),
            ("vdd".to_string(), vdd_mcu),
        ]);
        push_core(&mut sched, dir_core(true), mcu);
        // Nothing driven yet: nothing is downstream of firmware.
        assert!(sched.firmware_influenced_nets().is_empty());
        sched.mcus[0].last_levels.insert(('B', 0), true);
        sched.mcus[0].last_levels.insert(('B', 1), true);
        assert_eq!(sched.firmware_driven_nets(), ["EN2", "GPIO"]);
        let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                .collect()
        };
        assert_eq!(
            sched.firmware_influenced_nets(),
            pairs(&[
                ("GATE", "GPIO"),
                ("LOAD", "GPIO"),
                ("SENSE", "EN2"),
                ("VSW", "EN2"),
            ]),
            "V3V3, VDD_MCU, AVDD and VIN are held or fed, never controlled"
        );

        // Without the enable, U3's rail is as held as U2's.
        sched.regulators[1].enable.clear();
        assert_eq!(
            sched.firmware_influenced_nets(),
            pairs(&[("GATE", "GPIO"), ("LOAD", "GPIO")])
        );
    }

    /// A scheduler over `circuit` with one mock MCU whose pins sit on the nets
    /// named in `pins`, and the nets named in `driven` already written by the
    /// firmware.
    fn influence_sched(
        circuit: Circuit,
        net_nodes: HashMap<String, NodeId>,
        pins: &[((char, u8), &str)],
        driven: &[(char, u8)],
    ) -> Scheduler {
        let nodes: Vec<(_, NodeId)> = pins.iter().map(|(p, n)| (*p, net_nodes[*n])).collect();
        let bound = bound_board("influence", circuit, net_nodes, Vec::new(), Vec::new());
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let drivers = tristated_drivers(&mut sched, &nodes);
        let mut mcu = binding("U1", "qemu:test", drivers);
        for ((port, bit), node) in &nodes {
            mcu.role_nets
                .insert(format!("p{}{bit}", port.to_ascii_lowercase()), *node);
        }
        push_core(&mut sched, dir_core(true), mcu);
        for pin in driven {
            sched.mcus[0].last_levels.insert(*pin, true);
        }
        sched
    }

    fn named(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    }

    /// A DAC on an I2C bus sets its output net when the firmware writes it, so
    /// that net moves with the firmware's timing although no MCU pin drives it,
    /// and so does everything an op-amp buffer passes it to; a rail a real
    /// source holds stays out. Naming follows the bus net the firmware drove,
    /// then the bus's first signal net.
    #[test]
    fn a_dac_output_and_the_buffer_behind_it_follow_the_i2c_writes_that_set_it() {
        use crate::peripherals::register_map::tests::MINI_DAC_SPEC;
        let mut circuit = Circuit::new();
        let ([sda, scl, vout, buf, load, vref], net_nodes) =
            named_nodes(&mut circuit, ["SDA", "SCL", "VOUT", "BUF", "LOAD", "VREF"]);
        let gnd = NodeId::GROUND;
        circuit.add(Device::vsource("Vref", vref, gnd, SourceKind::Dc(2.5)));
        // A unity-gain buffer from the DAC output, loaded to ground and to the
        // held reference.
        circuit.add(Device::OpAmp {
            name: "U2".into(),
            out: buf,
            inp: vout,
            inn: buf,
            reference: None,
            gain: 1e5,
            pole_hz: None,
            slew: None,
            rail_lo: hauksbee_ir::Rail::fixed(0.0),
            rail_hi: hauksbee_ir::Rail::fixed(5.0),
            supply: hauksbee_ir::Supply::default(),
        });
        circuit.add(Device::resistor("R_LOAD", buf, load, 100.0));
        circuit.add(Device::resistor("R_PD", load, gnd, 10e3));
        circuit.add(Device::resistor("R_REF", load, vref, 100e3));
        let pins = [(('B', 0), "SDA"), (('B', 1), "SCL")];
        let mut sched = influence_sched(circuit, net_nodes, &pins, &[]);
        let driver =
            hauksbee_bind::drivers::PinDriver::stamp(&mut sched.circuit, vout, "VOUT", "dac", 1.0);
        let mut sensor = RegisterMapSensor::from_toml(MINI_DAC_SPEC).expect("spec");
        assert!(sensor.attach_output_driver_for_channel(0, driver));
        let mut bus = I2cBus::new("DAC").with_slave(Box::new(sensor));
        bus.add_signal_nodes([sda, scl]);
        sched.i2c_buses.push(Arc::new(Mutex::new(bus)));

        // The firmware has not touched any pin: the bus net still names it.
        assert!(sched.firmware_driven_nets().is_empty());
        let by = |net: &str| named(&[("BUF", net), ("LOAD", net), ("VOUT", net)]);
        assert_eq!(sched.firmware_influenced_nets(), by("SDA"));

        // A driven SCL is the bus net the firmware drove.
        sched.mcus[0].last_levels.insert(('B', 1), true);
        assert_eq!(sched.firmware_influenced_nets(), by("SCL"));

        // SDA driven as well: the first of the bus's nets the firmware drove.
        sched.mcus[0].last_levels.insert(('B', 0), true);
        assert_eq!(sched.firmware_influenced_nets(), by("SDA"));

        // No MCU runs, so nothing writes the bus.
        sched.mcus.clear();
        sched.responder_registries.clear();
        assert!(sched.firmware_influenced_nets().is_empty());
    }

    /// The binder hands the scheduler a DAC's bus pins and output nets, so a
    /// board with a real MCP4728 on it gets its VOUT net seeded from the nets
    /// the bus is wired to, with no part name known to the influence walk.
    #[test]
    fn a_bound_dac_seeds_its_vout_net_from_its_bus_nets() {
        let text = pcb_fixture(
            &["", "GND", "+3V3", "SCL", "SDA", "VOUT"],
            &[
                (
                    "Analog_DAC:MCP4728",
                    "U1",
                    "MCP4728",
                    100,
                    &[(1, 1, 2), (2, 2, 3), (3, 3, 4), (6, 6, 5), (10, 10, 1)],
                ),
                ("Resistor:R", "R1", "10k", 110, &[(1, 0, 5), (2, 2, 1)]),
            ],
        );
        let mut sched = board_scheduler(&text);
        let (sda, scl, vout) = (
            sched.net_nodes["SDA"],
            sched.net_nodes["SCL"],
            sched.net_nodes["VOUT"],
        );
        assert_eq!(sched.i2c_buses.len(), 1, "the DAC's bus attaches");
        let bus = sched.i2c_buses[0].lock().unwrap();
        assert_eq!(bus.signal_nodes(), [sda, scl]);
        assert_eq!(bus.driven_nodes(), [vout]);
        drop(bus);
        // No MCU, no firmware writes to the bus.
        assert!(sched.firmware_influenced_nets().is_empty());
        let drivers = tristated_drivers(&mut sched, &[(('B', 0), sda), (('B', 1), scl)]);
        let mut mcu = binding("U9", "qemu:test", drivers);
        mcu.role_nets
            .extend([("pb0".to_string(), sda), ("pb1".to_string(), scl)]);
        push_core(&mut sched, dir_core(true), mcu);
        sched.mcus[0].last_levels.insert(('B', 0), true);
        assert_eq!(sched.firmware_influenced_nets(), named(&[("VOUT", "SDA")]));
    }

    /// A 0 V current-sense ammeter in series with a feed is a wire to the
    /// influence walk (the taint walk still stops at it), while a rail a real
    /// source holds, reached through a pull-up beside the load, stays out.
    #[test]
    fn firmware_influence_passes_a_zero_volt_series_ammeter_but_not_a_held_rail() {
        let mut circuit = Circuit::new();
        let ([gpio, a, b, load, bus], net_nodes) =
            named_nodes(&mut circuit, ["GPIO", "A", "B", "LOAD", "BUS"]);
        let gnd = NodeId::GROUND;
        circuit.add(Device::vsource("Vbus", bus, gnd, SourceKind::Dc(5.0)));
        circuit.add(Device::resistor("R1", gpio, a, 100.0));
        circuit.add(Device::vsource("Vport_U9", a, b, SourceKind::Dc(0.0)));
        circuit.add(Device::resistor("R2", b, load, 100.0));
        circuit.add(Device::resistor("R3", load, bus, 10e3));
        let mut sched = influence_sched(circuit, net_nodes, &[(('B', 0), "GPIO")], &[('B', 0)]);
        let (_, walk) = sched.taint_walk();
        assert!(
            walk.stops().contains(&a) && walk.stops().contains(&b),
            "the regulation walk keeps stopping at the ammeter"
        );
        assert_eq!(
            sched.firmware_influenced_nets(),
            named(&[("A", "GPIO"), ("B", "GPIO"), ("LOAD", "GPIO")])
        );
        // The same board with the ammeter wired to ground is a held node.
        let ammeter = sched
            .circuit
            .devices
            .iter_mut()
            .find(|d| d.name() == "Vport_U9")
            .expect("the ammeter");
        let Device::Vsource { n, .. } = ammeter else {
            panic!("the ammeter is a voltage source");
        };
        *n = gnd;
        assert!(
            sched.firmware_influenced_nets().is_empty(),
            "a 0 V source to ground holds its node"
        );
    }

    /// A UART transmit pin is driven by the UART block, so a backend that never
    /// reports a GPIO write or direction for it leaves the line out of the
    /// driven set although the firmware sent bytes on it; an RC filter behind
    /// it then moves with the firmware's timing. A transmit pin on a core that
    /// sent nothing, and an unrelated net, stay out.
    #[test]
    fn a_uart_tx_line_the_firmware_sent_bytes_on_is_driven_and_seeds_the_walk() {
        let mut circuit = Circuit::new();
        let ([tx, filt, other, rail], net_nodes) =
            named_nodes(&mut circuit, ["UART_TX", "FILT", "OTHER", "RAIL"]);
        let gnd = NodeId::GROUND;
        circuit.add(Device::vsource("Vrail", rail, gnd, SourceKind::Dc(3.3)));
        circuit.add(Device::resistor("R_RC", tx, filt, 10e3));
        circuit.add(Device::capacitor("C_RC", filt, gnd, 1e-6));
        circuit.add(Device::resistor("R_PU", rail, other, 10e3));
        circuit.add(Device::resistor("R_PD", other, gnd, 10e3));
        let pins = [(('B', 1), "UART_TX")];
        let mut sched = influence_sched(circuit, net_nodes, &pins, &[]);
        // The binder's role for a pin whose function is a UART TX.
        let node = sched.net_nodes["UART_TX"];
        let roles = &mut sched.mcus[0].binding.role_nets;
        roles.remove("pb1");
        roles.insert("pb1_txd".to_string(), node);

        // Nothing sent, nothing written: the line is not driven.
        assert!(sched.firmware_driven_nets().is_empty());
        assert!(sched.firmware_influenced_nets().is_empty());

        // Bytes went out on a core with a transmit pin: the line is driven,
        // and the filter behind it follows; the divider and the rail do not.
        sched.mcus[0].uart_bytes = 12;
        assert_eq!(sched.firmware_driven_nets(), ["UART_TX"]);
        assert_eq!(
            sched.firmware_influenced_nets(),
            named(&[("FILT", "UART_TX")])
        );
        assert_eq!(
            sched.level_provenance("UART_TX"),
            LevelProvenance::FirmwareDriven
        );

        // A pin that is not a transmit line does not become driven by bytes.
        let roles = &mut sched.mcus[0].binding.role_nets;
        roles.remove("pb1_txd");
        roles.insert("pb1".to_string(), node);
        assert!(sched.firmware_driven_nets().is_empty());
    }

    /// Build a scheduler whose MCU has two lines (`LINE_A` with an RC filter
    /// behind it, `LINE_B` into a resistor) carrying the roles in `roles` (net,
    /// role), plus an unrelated divider, for the peripheral-activity tests.
    fn peripheral_sched(roles: &[(&str, &str)]) -> Scheduler {
        let mut circuit = Circuit::new();
        let ([a, b, filt, other, rail], net_nodes) =
            named_nodes(&mut circuit, ["LINE_A", "LINE_B", "FILT", "OTHER", "RAIL"]);
        let gnd = NodeId::GROUND;
        circuit.add(Device::vsource("Vrail", rail, gnd, SourceKind::Dc(3.3)));
        circuit.add(Device::resistor("R_RC", a, filt, 10e3));
        circuit.add(Device::capacitor("C_RC", filt, gnd, 1e-6));
        circuit.add(Device::resistor("R_B", b, gnd, 10e3));
        circuit.add(Device::resistor("R_PU", rail, other, 10e3));
        circuit.add(Device::resistor("R_PD", other, gnd, 10e3));
        let pins = [(('B', 1), "LINE_A"), (('B', 2), "LINE_B")];
        let mut sched = influence_sched(circuit, net_nodes, &pins, &[]);
        let (node_a, node_b) = (sched.net_nodes["LINE_A"], sched.net_nodes["LINE_B"]);
        let tagged = &mut sched.mcus[0].binding.role_nets;
        tagged.clear();
        for (net, role) in roles {
            let node = if *net == "LINE_A" { node_a } else { node_b };
            tagged.insert((*role).to_string(), node);
        }
        sched
    }

    /// The TWI block, not the GPIO register, drives SDA and SCL, so a backend
    /// that reports no pin write for them leaves the bus lines out of the
    /// driven set although the firmware talked on the bus; the RC behind SCL
    /// then moves with the firmware's timing. A core with no I2C events, and a
    /// pin whose role is not an I2C signal, stay out.
    #[test]
    fn an_i2c_line_the_firmware_talked_on_is_driven_and_seeds_the_walk() {
        let sched = peripheral_sched(&[("LINE_A", "pc5_adc5_scl"), ("LINE_B", "pc4_adc4_sda")]);
        assert!(sched.firmware_driven_nets().is_empty());
        assert!(sched.firmware_influenced_nets().is_empty());

        sched.mcus[0].i2c_events.store(4, Ordering::Relaxed);
        assert_eq!(sched.firmware_driven_nets(), ["LINE_A", "LINE_B"]);
        assert_eq!(
            sched.firmware_influenced_nets(),
            named(&[("FILT", "LINE_A")])
        );

        // SPI activity does not make an I2C-only pair driven, and a role that
        // merely contains the letters is not a signal word.
        let sched = peripheral_sched(&[("LINE_A", "pb1_sdaish"), ("LINE_B", "pd5_t1_oc0b")]);
        sched.mcus[0].i2c_events.store(4, Ordering::Relaxed);
        sched.mcus[0].spi_events.store(4, Ordering::Relaxed);
        assert!(sched.firmware_driven_nets().is_empty());
    }

    /// On a host-paced core whose backend cannot see peripheral routing, an
    /// output-capable pad with no drive evidence may still carry a PWM or
    /// timer waveform, so it and the RC filter behind it are listed under the
    /// pad. A pad that cannot drive (input-only), one wired to nothing else,
    /// and one the firmware already drove are left out, as is everything on a
    /// clock-exact core or a backend that observes routing. A pad the backend
    /// saw routed is listed as observed, and its filter follows it as an
    /// ordinary firmware-influenced net.
    #[test]
    fn unobserved_peripheral_pads_on_a_host_paced_core_seed_the_walk() {
        let mut circuit = Circuit::new();
        let ([a, filt, b, input_only, _lonely, drv, rail, other], net_nodes) = named_nodes(
            &mut circuit,
            [
                "LINE_A", "FILT", "LINE_B", "IN_ONLY", "LONELY", "DRV", "RAIL", "OTHER",
            ],
        );
        let gnd = NodeId::GROUND;
        circuit.add(Device::vsource("Vrail", rail, gnd, SourceKind::Dc(3.3)));
        circuit.add(Device::resistor("R_RC", a, filt, 10e3));
        circuit.add(Device::capacitor("C_RC", filt, gnd, 1e-6));
        circuit.add(Device::resistor("R_B", b, gnd, 10e3));
        circuit.add(Device::resistor("R_IN", input_only, gnd, 10e3));
        circuit.add(Device::resistor("R_DRV", drv, gnd, 10e3));
        circuit.add(Device::resistor("R_PU", rail, other, 10e3));
        circuit.add(Device::resistor("R_PD", other, gnd, 10e3));
        let pins = [
            (('B', 1), "LINE_A"),
            (('B', 2), "LINE_B"),
            (('B', 3), "IN_ONLY"),
            (('B', 4), "LONELY"),
            (('B', 5), "DRV"),
        ];
        let mut sched = influence_sched(circuit, net_nodes, &pins, &[('B', 5)]);
        let core = |bus_blind: bool, routing_seen: bool, routed: Vec<(char, u8)>| MockCore {
            bus_blind,
            routing_seen,
            routed,
            input_only: vec![('B', 3)],
            ..Default::default()
        };
        let pad = |net: &str, pin: &str, unobserved: bool| PeripheralPad {
            net: net.to_string(),
            mcu: "U1".to_string(),
            pin: pin.to_string(),
            unobserved: unobserved.then(|| "the mock reports no routing".to_string()),
        };
        let listed = |sched: &Scheduler| -> Vec<(String, String, bool)> {
            sched
                .peripheral_pad_nets()
                .into_iter()
                .map(|(n, p)| (n, p.net, p.unobserved.is_some()))
                .collect()
        };
        let row = |n: &str, of: &str, blind: bool| (n.to_string(), of.to_string(), blind);

        sched.mcus[0].core = Box::new(core(true, false, Vec::new()));
        assert_eq!(
            listed(&sched),
            [
                row("FILT", "LINE_A", true),
                row("LINE_A", "LINE_A", true),
                row("LINE_B", "LINE_B", true),
            ]
        );
        assert_eq!(sched.peripheral_pad_nets()[1].1, pad("LINE_A", "pb1", true));
        // Observation only: no driver is enabled and no net becomes driven.
        assert_eq!(sched.firmware_driven_nets(), ["DRV"]);

        // A clock-exact core keeps hard verdicts; a backend that sees routing
        // has nothing unobserved to report.
        sched.mcus[0].core = Box::new(core(false, false, Vec::new()));
        assert!(sched.peripheral_pad_nets().is_empty());
        sched.mcus[0].core = Box::new(core(true, true, Vec::new()));
        assert!(sched.peripheral_pad_nets().is_empty());

        // A pad seen routed is a driven, observed peripheral pad; its filter
        // follows it through the ordinary influence walk.
        sched.mcus[0].core = Box::new(core(true, true, vec![('B', 1)]));
        assert_eq!(listed(&sched), [row("LINE_A", "LINE_A", false)]);
        assert_eq!(
            sched.peripheral_pad_nets()[0].1,
            pad("LINE_A", "pb1", false)
        );
        assert!(sched
            .firmware_influenced_nets()
            .contains(&("FILT".to_string(), "LINE_A".to_string())));
    }

    /// A PWM, timer or RMT output reaches its pad through the pin matrix, not
    /// the GPIO output latch, so a backend can report no write and no direction
    /// for it although the firmware drove the pad. The core reporting the pad as
    /// routed to a peripheral makes the net driven, and the RC filter behind it
    /// follows; a pad it does not report stays out, and the routed list is
    /// observation only (no driver is enabled or level stamped from it).
    #[test]
    fn a_pad_routed_to_a_peripheral_output_is_driven_and_seeds_the_walk() {
        let mut sched = peripheral_sched(&[("LINE_A", "pb1"), ("LINE_B", "pb2")]);
        assert!(sched.firmware_driven_nets().is_empty());
        assert!(sched.firmware_influenced_nets().is_empty());

        let routed_core = |routed: Vec<(char, u8)>| MockCore {
            direction_observable: true,
            routed,
            ..Default::default()
        };
        sched.mcus[0].core = Box::new(routed_core(vec![('B', 1)]));
        assert_eq!(sched.firmware_driven_nets(), ["LINE_A"]);
        assert_eq!(
            sched.firmware_influenced_nets(),
            named(&[("FILT", "LINE_A")])
        );
        assert_eq!(
            sched.level_provenance("LINE_A"),
            LevelProvenance::FirmwareDriven
        );
        assert_eq!(
            sched.level_provenance("LINE_B"),
            LevelProvenance::Passive,
            "an unrouted pad on a direction-observable core is passive"
        );
        assert!(
            sched.mcus[0].last_levels.is_empty(),
            "routing is not a level"
        );

        // A pin with no net on this board adds nothing.
        sched.mcus[0].core = Box::new(routed_core(vec![('D', 7)]));
        assert!(sched.firmware_driven_nets().is_empty());
    }

    /// An SPI clock and data-out pad is driven by the SPI block: counted once
    /// the core delivered SPI events. Data-in (MISO) is the peripheral's, so it
    /// does not join, and I2C events alone leave the SPI pad out.
    #[test]
    fn an_spi_clock_and_data_out_line_the_firmware_clocked_is_driven() {
        let sched = peripheral_sched(&[("LINE_A", "pb5_sck"), ("LINE_B", "pb4_miso")]);
        assert!(sched.firmware_driven_nets().is_empty());
        sched.mcus[0].spi_events.store(2, Ordering::Relaxed);
        assert_eq!(sched.firmware_driven_nets(), ["LINE_A"]);
        assert_eq!(
            sched.firmware_influenced_nets(),
            named(&[("FILT", "LINE_A")])
        );
        sched.mcus[0].spi_events.store(0, Ordering::Relaxed);
        sched.mcus[0].i2c_events.store(2, Ordering::Relaxed);
        assert!(sched.firmware_driven_nets().is_empty());
    }

    /// A part whose pins carry no signal suffix (an ESP32 pad is just
    /// `gpioNN`) still has its bus lines counted: the hardware bus knows the
    /// nets its signals sit on. A soft bus on GPIO pins is not a hardware bus,
    /// so activity on the hardware one does not count its lines.
    #[test]
    fn a_hardware_bus_names_its_signal_nets_when_the_pins_carry_no_signal_role() {
        let mut sched = peripheral_sched(&[("LINE_A", "gpio21"), ("LINE_B", "gpio22")]);
        let (node_a, node_b) = (sched.net_nodes["LINE_A"], sched.net_nodes["LINE_B"]);
        let mut i2c = I2cBus::new("I2C0");
        i2c.add_signal_nodes([node_a]);
        let i2c = Arc::new(Mutex::new(i2c));
        sched.hw_i2c_buses.push(i2c.clone());
        sched.i2c_buses.push(i2c);
        let mut soft = I2cBus::new("SOFT");
        soft.add_signal_nodes([node_b]);
        sched.i2c_buses.push(Arc::new(Mutex::new(soft)));

        assert!(sched.firmware_driven_nets().is_empty());
        sched.mcus[0].i2c_events.store(1, Ordering::Relaxed);
        assert_eq!(sched.firmware_driven_nets(), ["LINE_A"]);
        assert_eq!(
            sched.firmware_influenced_nets(),
            named(&[("FILT", "LINE_A")])
        );

        let mut sched = peripheral_sched(&[("LINE_A", "gpio18"), ("LINE_B", "gpio19")]);
        let node_b = sched.net_nodes["LINE_B"];
        let mut spi = SpiBus::new("SPI", Box::new(crate::peripherals::Spi25Eeprom::new(256)));
        spi.add_signal_nodes([node_b]);
        sched.hw_spi_buses.push(Arc::new(Mutex::new(spi)));
        assert!(sched.firmware_driven_nets().is_empty());
        sched.mcus[0].spi_events.store(1, Ordering::Relaxed);
        assert_eq!(sched.firmware_driven_nets(), ["LINE_B"]);
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
            let q = stamp_resistor(&mut circuit, "Q1".into(), a, NodeId::GROUND, 1.0);
            let meta = DeviceMeta {
                derating: hauksbee_models::derating::Derating::default(),
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

    // ── Digital output levels follow the part's actual supply ────────────────

    /// A synthetic 3.3 V digital part whose card carries a scalar `voh` of 4.5
    /// (an HC card's 4.5 V row) beside a supply-dependent table must never put
    /// an output above its own 3.3 V rail, at ANY sample: not on the first
    /// chunk, before any solve has resolved the supply (which sits behind a
    /// series resistance, so no source holds it directly), and not after.
    #[test]
    fn a_digital_output_never_exceeds_its_supply_at_any_sample() {
        for with_table in [true, false] {
            let mut model = hauksbee_models::ModelEntry::default();
            for (key, v) in [("voh", 4.5), ("vol", 0.1), ("vih", 2.0), ("vil", 0.8)] {
                model.params.0.insert(key.into(), v.into());
            }
            if with_table {
                model.levels = std::collections::BTreeMap::from([(
                    "voh".to_string(),
                    vec![[2.0, 1.9], [4.5, 4.4], [6.0, 5.9]],
                )]);
            }
            let mut circuit = Circuit::new();
            let [src, vcc, a, y] = ["SRC", "VCC", "A", "Y"].map(|n| circuit.node(n));
            for (name, node) in [("V_SRC", src), ("V_A", a)] {
                circuit.add(Device::Vsource {
                    name: name.into(),
                    p: node,
                    n: NodeId::GROUND,
                    kind: SourceKind::Dc(3.3),
                });
            }
            // The supply reaches the part through a series resistance, and the
            // output is unconnected but for a light load.
            stamp_resistor(&mut circuit, "R_FERRITE".into(), src, vcc, 0.5);
            stamp_resistor(&mut circuit, "R_VCC".into(), vcc, NodeId::GROUND, 10_000.0);
            stamp_resistor(&mut circuit, "R_LOAD".into(), y, NodeId::GROUND, 100_000.0);
            let drivers = HashMap::from([(
                "y1".to_string(),
                hauksbee_bind::drivers::PinDriver::stamp(
                    &mut circuit,
                    y,
                    "Y",
                    "y1",
                    hauksbee_bind::drivers::DEFAULT_RO,
                ),
            )]);
            let roles = HashMap::from([
                ("a1".to_string(), a),
                ("y1".to_string(), y),
                ("vcc".to_string(), vcc),
            ]);
            let part =
                hauksbee_bind::digital::DigitalComponent::new("U1".into(), &model, roles, drivers)
                    .expect("passthrough compiles");
            let nets = HashMap::from([
                ("SRC".to_string(), src),
                ("VCC".to_string(), vcc),
                ("A".to_string(), a),
                ("Y".to_string(), y),
            ]);
            let bound = bound_board("levels", circuit, nets, vec![part], Vec::new());
            let mut sched =
                Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
            sched.chunk_s = 1.0e-4;
            let mut worst: f64 = 0.0;
            for chunk in 0..20 {
                sched.step(1.0e-4);
                let (v_vcc, v_y) = (
                    sched.net_voltage("VCC").unwrap(),
                    sched.net_voltage("Y").unwrap(),
                );
                worst = worst.max(v_y);
                assert!(
                    (-1e-9..=v_vcc + 1e-6).contains(&v_y) && v_y <= 3.3 + 1e-6,
                    "table={with_table} chunk {chunk}: Y = {v_y} V on a {v_vcc} V rail"
                );
            }
            // Powered, a high input is a high output: at the table's 3.3 V
            // level, or (no table) the rail itself, never the 4.5 V row.
            let want = if with_table { 3.2 } else { 3.3 };
            let y = sched.net_voltage("Y").unwrap();
            assert!(
                (y - want).abs() < 0.02,
                "table={with_table}: Y settled at {y} V"
            );
            assert!(worst <= 3.3 + 1e-6, "table={with_table}: peak {worst} V");
        }
    }

    // ── Sub-chunk pulses, timing policy and runtime contention ───────────────

    /// A 2 us GPIO pulse inside one 100 us chunk on the net clocking a
    /// tick-evaluated 74HC74 warns once per net per run, naming the net, the
    /// measured width and the part at risk; the pulse still counts two toggles.
    #[test]
    fn subchunk_pulse_on_a_tick_sequential_clock_net_warns_once() {
        let mut sched = pulse_scheduler(&PULSE_BOARD, &[(('B', 1), "STROBE")]);
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
        let mut sched = pulse_scheduler(&PULSE_BOARD, &[(('B', 1), "STROBE")]);
        let strobe = sched.net_nodes["STROBE"];
        stamp_resistor(
            &mut sched.circuit,
            "Rload".into(),
            strobe,
            NodeId::GROUND,
            10_000.0,
        );
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
        let mut sched = pulse_scheduler(&PULSE_BOARD, &[(('B', 1), "STROBE")]);
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

        let mut sched = pulse_scheduler(&PULSE_BOARD, &[(('B', 2), "FREE")]);
        preload_edges(&sched, ('B', 2), &[(100, true), (132, false)]);
        sched.step(DEFAULT_CHUNK_S);
        assert!(
            sched.short_pulses().is_empty(),
            "{:?}",
            sched.short_pulses()
        );
    }

    fn coarse_scheduler() -> Scheduler {
        let mut coarse = pulse_scheduler(&PULSE_BOARD, &[(('B', 1), "STROBE")]);
        coarse.mcus[0].core = Box::new(MockCore {
            coarse: true,
            ..Default::default()
        });
        coarse
    }

    #[test]
    fn timing_policy_uses_measured_backend_resolution_and_adapts_poll_chunks() {
        // No logic on this board, so only the push backend's resolution counts.
        let mut exact = pulse_scheduler(&PLAIN_INPUT_BOARD, &[(('B', 1), "BTN_HI")]);
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
        assert_eq!(exact.chunk_s.to_bits(), DEFAULT_CHUNK_S.to_bits());

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
    fn chunk_sampled_logic_reports_its_resolution_and_tightens_the_chunk() {
        let mut sched = pulse_scheduler(&CONTENTION_BOARD, &[(('B', 1), "SHARED")]);
        let logic = sched.timing_coverage().pop().expect("the 74HC08 row");
        assert_eq!(logic.mcu_ref, "U1");
        assert!(!logic.cycle_exact);
        assert!((logic.timestamp_precision_s - DEFAULT_CHUNK_S).abs() < 1e-15);
        sched
            .configure_timing(TimingRequirement {
                min_pulse_s: None,
                max_edge_error_s: Some(200e-9),
            })
            .expect("chunk-sampled logic tightens below the poll bridge quantum");
        assert!((sched.chunk_s - 200e-9).abs() < 1e-18);
        let logic = sched.timing_coverage().pop().unwrap();
        assert!((logic.minimum_guaranteed_pulse_s - 400e-9).abs() < 1e-18);
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
            coarse.chunk_s.to_bits(),
            DEFAULT_CHUNK_S.to_bits(),
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
        let mut sched = pulse_scheduler(&CONTENTION_BOARD, &[(('B', 1), "SHARED")]);
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
        let mut sched = pulse_scheduler(&TRISTATE_BOARD, &[(('B', 1), "BUS")]);
        sched.step(2.0 * DEFAULT_CHUNK_S);
        preload_edges(&sched, ('B', 1), &[(10, true)]);
        sched.step(3.0 * DEFAULT_CHUNK_S);
        assert!(
            sched.driver_contentions().is_empty(),
            "{:?}",
            sched.driver_contentions()
        );
    }

    /// A TXB0101 (1 VCCA, 2 GND, 3 A, 4 B, 5 OE, 6 VCCB) with VCCA on +1V8, OE
    /// on net index `oe`, VCCB on net index `vccb`, an optional resistor from B
    /// to net index `r.1`, and the mock MCU driving `mcu_net` at `high_v`.
    /// Nets: 1 GND, 2 +1V8, 3 +3V3, 4 A, 5 B, 6 `VB_OFF` (nothing powers it).
    fn txb_scheduler(
        oe: usize,
        vccb: usize,
        r: Option<(&str, usize)>,
        (mcu_net, high_v): (&str, f64),
    ) -> Scheduler {
        let txb: &[(u8, u8, usize)] = &[
            (1, 1, 2),
            (2, 2, 1),
            (3, 3, 4),
            (4, 4, 5),
            (5, 5, oe),
            (6, 6, vccb),
        ];
        let mut parts: Vec<FixturePart<'_>> =
            vec![("Package_TO_SOT_SMD:SOT-363", "U1", "TXB0101DCKR", 100, txb)];
        let pads = r.map(|(_, to)| [(1, 0, to), (2, 2, 5)]);
        if let (Some((value, _)), Some(pads)) = (r, pads.as_ref()) {
            parts.push(("Resistor:R", "R1", value, 110, pads));
        }
        let text = pcb_fixture(&["", "GND", "+1V8", "+3V3", "A", "B", "VB_OFF"], &parts);
        let mut sched = pulse_scheduler(&text, &[(('B', 1), mcu_net)]);
        sched.mcus[0].logic_high_v = high_v;
        sched
    }

    /// Drive the mock MCU pin `high`, settle, and read nets A and B.
    fn txb_drive(sched: &mut Scheduler, high: bool) -> (f64, f64) {
        sched.step(2.0 * DEFAULT_CHUNK_S);
        preload_edges(sched, ('B', 1), &[(10, high)]);
        sched.step(4.0 * DEFAULT_CHUNK_S);
        let v = |n| sched.net_voltage(n).expect(n);
        (v("A"), v("B"))
    }

    /// An auto-direction translator copies whichever port is driven to the
    /// other at that port's rail, and the MCU overdriving its weak DC driver
    /// is how it senses direction, not contention (TXB0101 SCES639F 6.3.1).
    #[test]
    fn a_txb_translates_either_way_and_is_overdriven_not_fought() {
        for (high, b) in [(true, 3.3), (false, 0.0)] {
            let mut sched = txb_scheduler(2, 3, None, ("A", 1.8));
            let (_, got) = txb_drive(&mut sched, high);
            assert!((got - b).abs() < 0.02, "A {high}: B at {got}");
            assert!(
                sched.driver_contentions().is_empty(),
                "{:?}",
                sched.driver_contentions()
            );
        }
        let mut sched = txb_scheduler(2, 3, None, ("B", 3.3));
        let (a, _) = txb_drive(&mut sched, true);
        assert!((a - 1.8).abs() < 0.02, "B high: A at {a}");
        assert!(sched.driver_contentions().is_empty());
    }

    /// The 4.5 kOhm cell against a pull or load: SCES639F 7.2.2 Eq. (2) puts B
    /// at 3.3 x 4.5k / (4.5k + 4.7k) under a 4.7 kOhm pull-up with A low, and
    /// Eq. (1) at 3.3 x 1k / (1k + 4.5k) into a 1 kOhm pull-down with A high.
    /// OE low or VCCB off leaves B to a 100 kOhm pull-down.
    #[test]
    fn a_txb_port_sags_against_a_strong_pull_and_releases_on_oe_or_isolation() {
        for (r, high, b) in [
            (("4.7k", 3), false, 3.3 * 4.5 / 9.2),
            (("1k", 1), true, 3.3 / 5.5),
            (("100k", 1), true, 3.3 * 100.0 / 104.5),
        ] {
            let (_, got) = txb_drive(&mut txb_scheduler(2, 3, Some(r), ("A", 1.8)), high);
            assert!(
                (got - b).abs() < 0.02,
                "{r:?}: B at {got}, Eq. gives {b:.3}"
            );
        }
        for (oe, vccb) in [(1, 3), (2, 6)] {
            let (_, got) = txb_drive(
                &mut txb_scheduler(oe, vccb, Some(("100k", 1)), ("A", 1.8)),
                true,
            );
            assert!(got < 0.02, "OE net {oe}, VCCB net {vccb}: B at {got}");
        }
    }

    /// A board-side MCU on `PLAIN_INPUT_BOARD` whose reset role sits on `reset`
    /// and whose `vdd` pad sits on `+5V`, with the given operating range.
    fn sched_with_rails(reset: &str, supply: Option<(f64, f64)>) -> (Scheduler, MockCore) {
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
        let mut b = binding("U1", "simavr:test", HashMap::new());
        b.role_nets.insert("reset".into(), sched.net_nodes[reset]);
        b.role_nets.insert("vdd".into(), sched.net_nodes["+5V"]);
        b.min_supply_v = supply.map(|(min, _)| min);
        b.max_supply_v = supply.map(|(_, max)| max);
        let core = MockCore::default();
        let handles = core.handles();
        push_core(&mut sched, core, b);
        (sched, handles)
    }

    fn fire_edge(h: &MockCore, port: char, bit: u8, high: bool) {
        let mut slot = h.pin_cb.lock_or_recover();
        (slot.as_mut().expect("on_pin_change installed"))(PinId { port, bit }, high, 0);
    }

    /// Legal control: firmware pin edges are counted per pin, the reset line is
    /// released and the supply sits in range, so the row is a firmware pass.
    #[test]
    fn execution_evidence_counts_firmware_edges_on_a_released_powered_core() {
        let (mut sched, h) = sched_with_rails("BTN_HI", Some((4.5, 5.5)));
        sched.step(1e-3);
        fire_edge(&h, 'B', 5, true);
        sched.step(1e-3);
        fire_edge(&h, 'B', 5, false);
        fire_edge(&h, 'C', 1, true);
        sched.step(1e-3);
        sched.refresh_execution_evidence();
        let rows = sched.execution_evidence();
        assert_eq!(rows.len(), 1);
        let e = &rows[0];
        assert!(e.executed, "{e:?}");
        assert_eq!(e.gpio_toggles.get("PB5"), Some(&2));
        assert_eq!(e.gpio_toggles.get("PC1"), Some(&1));
        assert_eq!(e.reset_held, crate::execution::Observed::No, "{e:?}");
        assert_eq!(e.supply_valid, crate::execution::Observed::Yes, "{e:?}");
        assert!(e.firmware_pass_eligible, "{:?}", e.blockers);
        assert!(!sched.firmware_quiet(0, false));
        assert!(sched.firmware_pass_blockers().is_empty());
    }

    /// Faulty input: a core that never reached its entry and drove nothing is
    /// quiet even when the analog side toggled nets, which never proves the
    /// firmware ran.
    #[test]
    fn analog_toggles_alone_are_not_execution_evidence() {
        let (mut sched, _h) = sched_with_rails("BTN_HI", Some((4.5, 5.5)));
        assert!(
            !sched.firmware_quiet(7, false),
            "no evidence yet: the old toggle test stands"
        );
        sched.step(1e-3);
        sched.refresh_execution_evidence();
        let e = &sched.execution_evidence()[0];
        assert!(!e.executed && !e.firmware_pass_eligible, "{e:?}");
        assert_eq!(e.reached_entry, crate::execution::Observed::Unknown);
        assert!(sched.firmware_quiet(7, false));
        assert!(
            !sched.firmware_quiet(0, true),
            "UART output is always activity"
        );
        assert!(crate::execution::execution_evidence_line(e).contains("NO EVIDENCE OF EXECUTION"));
    }

    /// Unsupported input: a supply pad on a net nothing drives (VDDA behind an
    /// unmodelled bead) and a bare reset pad are dropped from the watch, so
    /// they read unknown instead of "supply dead" and "held in reset".
    #[test]
    fn floating_reset_and_supply_pads_read_unknown_not_failed() {
        let mut sched = board_scheduler(&PLAIN_INPUT_BOARD);
        let floating = sched.circuit.node("VDDA_FLOAT");
        let bare_reset = sched.circuit.node("NRST_FLOAT");
        sched.node_volts.resize(sched.circuit.node_count(), 0.0);
        let mut b = binding("U1", "simavr:test", HashMap::new());
        b.role_nets.insert("vdda".into(), floating);
        b.role_nets.insert("reset".into(), bare_reset);
        b.min_supply_v = Some(1.8);
        b.max_supply_v = Some(4.0);
        push_core(&mut sched, MockCore::default(), b);
        sched.rebuild_digital_in_evidence();
        sched.drop_undriven_rail_nodes();
        sched.refresh_execution_evidence();
        let e = &sched.execution_evidence()[0];
        assert_eq!(e.reset_held, crate::execution::Observed::Unknown, "{e:?}");
        assert_eq!(e.supply_valid, crate::execution::Observed::Unknown, "{e:?}");
        assert!(
            e.blockers
                .iter()
                .all(|b| !b.contains("supply") && !b.contains("reset")),
            "{:?}",
            e.blockers
        );
    }

    /// Faulty input: the board holds reset low and feeds the pads above the
    /// part's range. The mock core still toggles a pin (an emulator runs
    /// regardless), so the row is executed but never a firmware pass.
    #[test]
    fn held_reset_and_overvoltage_block_a_firmware_pass() {
        let (mut sched, h) = sched_with_rails("BTN_LO", Some((1.8, 3.6)));
        sched.step(1e-3);
        fire_edge(&h, 'B', 5, true);
        sched.step(1e-3);
        sched.refresh_execution_evidence();
        let e = &sched.execution_evidence()[0];
        assert!(e.executed, "{e:?}");
        assert_eq!(e.reset_held, crate::execution::Observed::Yes, "{e:?}");
        assert_eq!(e.supply_valid, crate::execution::Observed::No, "{e:?}");
        assert!(!e.firmware_pass_eligible);
        assert_eq!(e.blockers.len(), 2, "{:?}", e.blockers);
        let blockers = sched.firmware_pass_blockers();
        assert_eq!(blockers.len(), 1);
        assert!(blockers[0].starts_with("U1: "), "{blockers:?}");
        assert!(!sched.firmware_quiet(0, false), "it did execute");
    }
}

#[cfg(test)]
#[path = "scheduler/tests/continuation.rs"]
mod continuation_tests;

/// `renode:<part>` instantiation consults the SoC-descriptor override dirs:
/// override beats builtin, an invalid override fails loudly, aliases fall back
/// to their canonical descriptors, and an unknown part names the dirs searched.
/// One test fn: it mutates `HAUKSBEE_MCU_DIR`.
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
