//! The co-simulation scheduler.
//!
//! Generalizes the Tarski-Emulator lockstep pattern. Each call to
//! [`Scheduler::step`] advances wall-clock `dt` in fixed sub-chunks (default
//! 100 µs). Per chunk:
//!
//! 1. **MCU**: run each emulated core for the chunk's cycles. GPIO output edges
//!    land in a shared queue via `on_pin_change`; UART output bytes are
//!    captured; the latest ADC voltages (from the *previous* chunk's solve) are
//!    injected continuously before the run.
//! 2. **Drivers**: apply captured GPIO edges to their Thevenin [`PinDriver`]s,
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
use crate::binder::{gpio_of_role, BoundBoard, McuBinding};
use crate::digital::DigitalComponent;
use crate::peripherals::{I2cBus, Mcp4728, PeripheralSet, SpiBus, TickCtx, TimelineEvent};
use crate::power_supply::{PowerSupply, SupplyLeg};
use crate::stress::{FaultEvent, StressMonitor};

/// Default co-sim chunk size (seconds).
pub const DEFAULT_CHUNK_S: f64 = 100e-6;

/// A strict headless run (`--strict`) or hauksbee-ci aborts (exit 3,
/// invalid-for-analysis) once the analog solve fails this many chunks in a row.
/// Three, not one: a single failed chunk is often a stiff step the warm-started
/// next chunk recovers from, so aborting on the first would be trigger-happy on a
/// board that self-heals within a chunk or two. Three back-to-back failures is a
/// solve that is genuinely stuck rather than a blip, and a run that reaches it is
/// reporting fiction (held stale voltages), so it must refuse rather than fake
/// (05 §3b, master doctrine §5).
pub const STRICT_CONSECUTIVE_FAILED_ABORT: u32 = 3;

/// Captured state shared between an MCU's C callbacks and the scheduler.
#[derive(Default)]
struct McuShared {
    /// Pin edges since last drain: (port, bit) -> latest level. Still used by
    /// ordinary GPIO drivers (which only care about the final level a chunk
    /// settles to) and diagnostics / frame state.
    pin_edges: HashMap<(char, u8), bool>,
    /// Ordered log of EVERY pin transition since the last drain, in the order
    /// the firmware produced them. Unlike `pin_edges` (latest-level map, which
    /// collapses a sub-µs `shiftOut` SCLK pulse train to its final level), this
    /// preserves each edge so the bit-banged 74HC595 chain can be clocked at
    /// edge granularity (FIX 1).
    pin_edge_log: Vec<(char, u8, bool)>,
    /// UART bytes the firmware emitted since last drain.
    uart_out: Vec<u8>,
}

/// One live MCU core plus its binding and shared capture state.
struct LiveMcu {
    core: Box<dyn Mcu + Send>,
    binding: McuBinding,
    shared: Arc<Mutex<McuShared>>,
    /// Last known GPIO output levels, for diagnostics / frame state.
    last_levels: HashMap<(char, u8), bool>,
    /// Logic-high output voltage for this MCU's GPIO drivers (rail-dependent:
    /// 5 V for classic AVR, 3.3 V for STM32-class parts).
    logic_high_v: f64,
}

/// The scheduler driving one bound board.
pub struct Scheduler {
    pub circuit: Circuit,
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
    /// MCP4728 DAC VOUT drivers: each entry maps an I2C slave (by address, on
    /// `dac_bus`) to its four VOUT-channel [`PinDriver`]s. Every chunk the
    /// scheduler reads the slave's computed VOUT per channel and pushes it onto
    /// the matching driver before the analog solve (the `Hc595Chain::apply`
    /// cadence), so firmware DAC writes become real analog output voltages.
    dac_vouts: Vec<DacVoutDrive>,
    /// MCU-bit-banged 74HC165 read chains, resolved at GPIO-output edge
    /// granularity inside the owning MCU's run loop via its synchronous input
    /// responder (the read-direction analogue of `chains`). Wrapped in
    /// Arc<Mutex<>> because the responder closure owns a clone. Each shares the
    /// `input_volts` snapshot the scheduler refreshes from the last solve so the
    /// 165 captures the latest spike-latch states on its PL load.
    hc165_chains: Vec<Arc<Mutex<crate::digital::Hc165Chain>>>,
    /// Latest solved node voltages, shared with the 165 read chains so their
    /// PL-load sampling (which fires inside the MCU run, before this chunk's
    /// solve) sees the previous chunk's settled latch voltages.
    input_volts: Arc<Mutex<Vec<f64>>>,
    /// Forced node-voltage overrides applied to `node_volts` AFTER each chunk's
    /// analog solve. Used by the firmware-driven Tarski inference to drive the 10
    /// output SPIKE nets from the EXACT feedforward decomposition (the monolith
    /// does not converge) — the genuine per-column spikes the decomposition
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

impl McuSubstitution {
    /// A one-line warning sentence suitable for stderr or a JSON note.
    pub fn message(&self) -> String {
        format!(
            "co-sim: {} requested {} but it is modelled as an {} core; \
             firmware behaviour is emulated on the substitute and may differ on \
             the real part (e.g. peripheral set, flash/RAM size, clock tree).",
            self.reference, self.requested_part, self.modelled_core
        )
    }
}


/// One MCP4728's analog VOUT drive: the bus its slave lives on, the slave's
/// 7-bit address, and the four channel drivers (None where the VOUT net is not
/// connected on the board).
struct DacVoutDrive {
    bus: Arc<Mutex<I2cBus>>,
    address: u8,
    drivers: [Option<crate::drivers::PinDriver>; 4],

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
            let external =
                binding.backend.starts_with("renode:") || binding.backend.starts_with("qemu:");
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
        // is left to the old once-per-chunk digital tick (so nothing regresses).
        let (chains, chain_mcu, chain_chips) = build_595_chains(&digital, &live);

        let n_nodes = circuit.node_count();
        let layout = Layout::new(&circuit);
        let n_branch = layout.size.saturating_sub(layout.n_nodes);
        let mut sched = Scheduler {
            circuit,
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
            stress: StressMonitor::new(device_meta),
            faults_pending: Vec::new(),
            chunk_s: DEFAULT_CHUNK_S,
            opts,
            sim_time: 0.0,
            stats: HashMap::new(),
            peripherals: PeripheralSet::new(),
            i2c_buses: Vec::new(),
            spi_buses: Vec::new(),
            spi_controller_map: HashMap::new(),
            substitutions,
            dac_vouts: Vec::new(),
            hc165_chains: Vec::new(),
            input_volts: Arc::new(Mutex::new(vec![0.0; n_nodes])),
            forced_node_volts: HashMap::new(),
            failed_chunks: 0,
            failed_windows: Vec::new(),
            consecutive_failed_chunks: 0,
            max_consecutive_failed_chunks: 0,
        };

        // Build the edge-driven 74HC165 read chains and install each as its
        // owning MCU's synchronous input responder, so a firmware readback
        // (bit-banged SCLK + digitalRead(MISO)) resolves at edge granularity.
        sched.build_and_install_165_chains();

        // Wire up the board's MCP4728 quad DACs: build one I2C slave per binding
        // at its assigned address, attach them on a shared bus (so firmware TWI
        // writes reach them through `on_i2c`), and record each slave's VOUT
        // drivers so the chunk loop pushes computed VOUT onto the analog nets.
        if !dacs.is_empty() {
            sched.attach_mcp4728_dacs(dacs);
        }

        Ok(sched)
    }

    /// Build and attach the MCP4728 DAC slaves discovered by the binder. One
    /// shared [`I2cBus`] holds all of them (addressed 0x60/0x61/0x62); the bus
    /// is registered as every MCU's `on_i2c` handler. Each DAC's VOUT-channel
    /// drivers are kept in `dac_vouts` so [`Scheduler::push_dac_vouts`] can
    /// drive the analog nets each chunk.
    fn attach_mcp4728_dacs(&mut self, dacs: Vec<crate::binder::DacBinding>) {
        let mut bus = I2cBus::new("MCP4728_BUS");
        for d in &dacs {
            let mut slave = Mcp4728::with_config(d.address, d.vref, d.gain);
            // Carry the datasheet ROUT through so state() / diagnostics agree
            // with the stamped driver resistance.
            slave.rout = 1.0;
            bus.add_slave(Box::new(slave));
        }
        let bus = Arc::new(Mutex::new(bus));
        self.attach_i2c_bus(bus.clone());
        for d in dacs {
            self.dac_vouts.push(DacVoutDrive {
                bus: bus.clone(),
                address: d.address,
                drivers: d.vout_drivers,
            });
        }
        // Seed the analog nets with the DACs' power-on VOUT (code 0 -> ~0 V).
        self.push_dac_vouts();
    }

    /// Build the edge-driven 74HC165 read chains (one per physical chain whose
    /// PL / CLK / QH→MISO pins bind to an MCU's GPIO) and install each as that
    /// MCU's synchronous input responder. The responder fires on every GPIO
    /// output edge during the MCU's run: it forwards PL / SCLK edges to the
    /// chain, which samples the spike-latch inputs on a PL load and presents the
    /// next QH bit on MISO, returning the (MISO pin, level) to drive immediately.
    /// This closes the readback inside the firmware's own bit-bang loop.
    fn build_and_install_165_chains(&mut self) {
        use crate::digital::{order_165_chains, Hc165Chain, LogicLevels};

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
                let pl_n = chain.pl_n;
                let clk = chain.clk;
                let chain = Arc::new(Mutex::new(chain));
                let chain_cb = chain.clone();
                let volts = self.input_volts.clone();
                // The responder: only PL/SCLK edges matter; ignore everything
                // else cheaply. Read the shared voltage snapshot for the
                // PL-load sampling of the latch inputs.
                self.mcus[mi].core.on_input_responder(Box::new(
                    move |pin: PinId, high: bool| -> Vec<(PinId, bool)> {
                        let pin = (pin.port, pin.bit);
                        if pin != pl_n && pin != clk {
                            return Vec::new();
                        }
                        let v = volts.lock().unwrap_or_else(|e| e.into_inner());
                        let node_v = |n: hauksbee_ir::NodeId| {
                            v.get(n.0 as usize).copied().unwrap_or(0.0)
                        };
                        let mut ch = chain_cb.lock().unwrap_or_else(|e| e.into_inner());
                        match ch.on_edge(pin, high, &node_v, &levels) {
                            Some(((port, bit), level)) => {
                                vec![(PinId { port, bit }, level)]
                            }
                            None => Vec::new(),
                        }
                    },
                ));
                self.hc165_chains.push(chain);
                break;
            }
        }
    }

    /// Read each MCP4728 slave's current per-channel VOUT and push it onto that
    /// channel's [`PinDriver`]. Called each chunk after the MCU runs (so a just-
    /// completed I2C DAC write is reflected) and before the analog solve, the
    /// same cadence the 74HC595 chains use.
    fn push_dac_vouts(&mut self) {
        for dv in &self.dac_vouts {
            // Snapshot the four VOUT voltages under the bus lock, then release it
            // before mutating the circuit (drivers borrow `self.circuit`).
            let vouts: [f64; 4] = {
                let bus = dv.bus.lock().unwrap_or_else(|e| e.into_inner());
                match bus.slave::<Mcp4728>(dv.address) {
                    Some(s) => [s.vout(0), s.vout(1), s.vout(2), s.vout(3)],
                    None => continue,
                }
            };
            for (ch, drv) in dv.drivers.iter().enumerate() {
                if let Some(drv) = drv {
                    drv.set_volts(&mut self.circuit, vouts[ch]);
                }
            }
        }
    }

    /// Chip-substitution events detected at build time (Track B). Empty when
    /// every instantiated MCU was modelled by its exact requested part.
    pub fn substitutions(&self) -> &[McuSubstitution] {
        &self.substitutions
    }

    /// Whether any MCU produced at least one GPIO output edge — i.e. the firmware
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
        let addresses = bus.lock().unwrap_or_else(|e| e.into_inner()).addresses();
        for m in &mut self.mcus {
            m.core.set_i2c_slave_addresses(&addresses);
            let b = bus.clone();
            m.core.on_i2c(Box::new(move |ev| {
                b.lock().unwrap_or_else(|e| e.into_inner()).dispatch(ev)
            }));
        }
        self.i2c_buses.push(bus);
    }

    /// Attach a SPI bus and register it as every live MCU's `on_spi` handler.
    pub fn attach_spi_bus(&mut self, bus: Arc<Mutex<SpiBus>>) {
        for m in &mut self.mcus {
            let b = bus.clone();
            m.core.on_spi(Box::new(move |ev| {
                let mut guard = b.lock().unwrap_or_else(|e| e.into_inner());
                if ev.deselect {
                    // Chip-select deasserted: reset the slave state machine.
                    guard.slave_deselect();
                    0xFF
                } else {
                    guard.transfer(ev.mosi)
                }
            }));
        }
        self.spi_buses.push(bus);
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
    pub fn attach_spi_bus_on(&mut self, controller: &str, bus: Arc<Mutex<SpiBus>>) {
        for m in &mut self.mcus {
            let b = bus.clone();
            let ctrl = controller.to_string();
            m.core.on_spi_controller(&ctrl, Box::new(move |ev| {
                let mut guard = b.lock().unwrap_or_else(|e| e.into_inner());
                if ev.deselect {
                    guard.slave_deselect();
                    0xFF
                } else {
                    guard.transfer(ev.mosi)
                }
            }));
        }
        self.spi_controller_map.insert(controller.to_string(), bus.clone());
        self.spi_buses.push(bus);
    }

    /// Look up the SPI bus attached to a specific named controller.
    ///
    /// Returns `None` if no bus was attached to that controller via
    /// [`attach_spi_bus_on`]. Buses attached via the controller-agnostic
    /// [`attach_spi_bus`] are not findable by name (they carry no controller
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
        self.mcus.iter().any(|m| {
            m.binding.backend.starts_with("renode:") || m.binding.backend.starts_with("qemu:")
        })
    }

    /// Advance the co-sim by `dt` seconds in fixed chunks.
    pub fn step(&mut self, dt: f64) -> StepResult {
        let mut uart: HashMap<String, Vec<u8>> = HashMap::new();
        let mut chunks = (dt / self.chunk_s).round() as u64;
        if chunks == 0 {
            chunks = 1;
        }
        let chunk = dt / chunks as f64;

        for _ in 0..chunks {
            self.run_chunk(chunk, &mut uart);
        }

        StepResult {
            sim_time: self.sim_time,
            uart,
        }
    }

    fn run_chunk(&mut self, chunk: f64, uart: &mut HashMap<String, Vec<u8>>) {
        let micros = (chunk * 1e6).round().max(1.0) as u64;

        // Refresh the snapshot the edge-driven 74HC165 read chains sample on a
        // PL load (it fires inside the MCU run below, so it must reflect the
        // PREVIOUS chunk's settled spike-latch voltages). Cheap clone; only
        // taken when a 165 read chain is present.
        if !self.hc165_chains.is_empty() {
            let mut snap = self.input_volts.lock().unwrap_or_else(|e| e.into_inner());
            snap.clear();
            snap.extend_from_slice(&self.node_volts);
        }

        // 1. MCU: inject latest ADC voltages, run the chunk, drain captures.
        for mi in 0..self.mcus.len() {
            let m = &mut self.mcus[mi];
            for (&ch, &node) in &m.binding.adc_nets {
                // Skip a pin the firmware has promoted to a GPIO output. An
                // analog-capable pin binds BOTH an ADC channel and a tri-stated
                // GPIO driver (dynamic promotion, 05-cosim-fidelity §4.1); once
                // that driver is enabled the pin is being DRIVEN, not read, so
                // injecting an ADC voltage for it is contradictory (a phantom
                // analog reading on a pin the firmware owns as an output).
                // Promotion is detected as an enabled driver on this pin's own
                // net; a pin never driven keeps its driver disabled and is
                // injected normally.
                let promoted = m
                    .binding
                    .gpio_drivers
                    .values()
                    .any(|d| d.net == node && d.enabled);
                if promoted {
                    continue;
                }
                let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
                m.core.set_analog_in(ch, v.max(0.0));
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
            let _ = m.core.run_micros(micros);

            let (edges, edge_log, bytes) = {
                let mut sh = m.shared.lock().unwrap_or_else(|e| e.into_inner());
                (
                    std::mem::take(&mut sh.pin_edges),
                    std::mem::take(&mut sh.pin_edge_log),
                    std::mem::take(&mut sh.uart_out),
                )
            };
            if !bytes.is_empty() {
                uart.entry(m.binding.reference.clone())
                    .or_default()
                    .extend(bytes);
            }
            // 1b. Edge-driven 74HC595 chains: replay THIS MCU's ordered
            // transition log so each bit-banged SRCLK/RCLK pulse clocks the
            // chains it owns (FIX 1). Only chains whose owning MCU is `mi` are
            // replayed, so a different MCU's identically-named pin cannot inject
            // spurious clocks. The latched outputs are pushed onto the analog
            // nets after the digital tick (so they are not clobbered by it).
            if !edge_log.is_empty() {
                for (ci, chain) in self.chains.iter_mut().enumerate() {
                    if self.chain_mcu[ci] == mi {
                        chain.replay(&edge_log);
                    }
                }
            }
            // 2. Apply GPIO edges to drivers. An edge means the firmware has
            // configured the pin as a driven output, so enable the (initially
            // tri-stated) Thevenin leg before setting its level. This is also
            // the promotion path for a dual-bound analog-capable pin (dynamic
            // promotion, 05-cosim-fidelity §4.1): the first firmware drive of
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
            // 2b. Early promotion from the configured pin direction. A pin the
            // firmware set as an OUTPUT but has so far only held at its reset
            // level (DDR write, no PORT toggle — e.g. an active-low enable held
            // low from boot) emits no pin-change edge, so the loop above never
            // enables its driver and the net would float. Enable such drivers
            // from `pins_configured_output`, at the pin's last known level
            // (default low, the AVR reset PORT state). Backends that cannot
            // report direction return an empty set, making this a no-op there;
            // the edge-driven enable above remains the primary path.
            let configured = m.core.pins_configured_output();
            for pin in configured {
                if let Some(drv) = m.binding.gpio_drivers.get_mut(&(pin.port, pin.bit)) {
                    if !drv.enabled {
                        drv.set_enabled(&mut self.circuit, true);
                        let level = m
                            .last_levels
                            .get(&(pin.port, pin.bit))
                            .copied()
                            .unwrap_or(false);
                        let v = if level { m.logic_high_v } else { 0.0 };
                        drv.set_volts(&mut self.circuit, v);
                    }
                }
            }
        }

        // 5(prev). Digital components drive their outputs from current state,
        // sampling the previous chunk's solved node voltages. Chips owned by an
        // edge-driven 595 chain are SKIPPED here: they are clocked by the edge
        // path above and have their latched outputs applied below, so ticking
        // them once-per-chunk too would double-drive them with a stale,
        // pulse-collapsed sample.
        {
            let volts = self.node_volts.clone();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for (i, d) in self.digital.iter_mut().enumerate() {
                if self.chain_chips.contains(&i) {
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

        // 5c(prev). Push each MCP4728's computed VOUT onto its channel drivers,
        // so a DAC write the MCU just issued over TWI (handled in step 1 via
        // `on_i2c`) appears as a real analog output voltage in this chunk's
        // solve. Same cadence as the 595 chain apply above.
        if !self.dac_vouts.is_empty() {
            self.push_dac_vouts();
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
        let chunk_converged = self.solve_chunk(chunk);

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

        // 3c. SPI bus chunk-boundary deselect (chunk-cadence CS heuristic).
        //
        // This resets each SPI slave's command state machine so the next
        // transaction starts fresh. It is a HEURISTIC standing in for a real
        // chip-select edge, inherited from the AVR backend: simavr's SPI IRQ
        // surfaces only byte transfers, never CS, so the chunk boundary is used
        // as a frame boundary. The Renode backend has the same gap for software-
        // NSS firmware (SSM=1, SSI=1 in STM32 SPI CR1): `STM32SPI` only fires
        // `FinishTransmission` on a hardware NSS toggle, so soft-NSS firmware
        // never produces a real CS edge and the chunk boundary is the only
        // reliable reset point. Backends that DO surface CS (hardware NSS ->
        // `FinishTransmission`) reset earlier and more precisely via the
        // `deselect` SpiEvent in `attach_spi_bus`; this call is then idempotent.
        //
        // LIMITATIONS (where the heuristic is wrong):
        //   * Two CS-framed transactions inside one chunk are NOT separated: the
        //     second transaction's bytes are appended to the first slave state,
        //     because no reset happens between them.
        //   * A single transaction that SPANS a chunk boundary is reset mid-way:
        //     the slave is deselected with bytes still pending, corrupting the
        //     reply. The debug guard below flags exactly this case.
        // Both are avoided in practice by keeping the chunk period larger than a
        // full transaction's wall-time and firmware that issues at most one
        // transaction per chunk. A correct fix needs a real per-edge CS path
        // from the backend (hardware NSS or a software-NSS GPIO watch).
        //
        // Runs UNCONDITIONALLY on a failed chunk (05 §3b), for the same reason as
        // the 3b post_solve deselects: this is a DIGITAL frame-boundary reset of
        // the SPI slave command state machine, not an analog sample. The byte
        // transfers it frames already happened in this chunk's `run_micros`, so
        // whether the analog march converged is irrelevant. Skipping it on a
        // failed chunk would leave the slave mid-command and desync the next
        // transaction, so we keep it and instead surface the failed span via
        // `failed_windows` / `analog_valid:false` for any downstream consumer.
        for bus in &self.spi_buses {
            let mut guard = bus.lock().unwrap_or_else(|e| e.into_inner());
            // Debug-only: a slave still mid-transaction at the chunk boundary
            // means a transfer spanned the boundary — the heuristic cannot frame
            // it. Warn loudly in debug builds rather than silently truncating.
            // (Not an assert/panic: spanning is a known limitation, not a bug to
            // abort on, and some firmware cadences may legitimately hit it.)
            #[cfg(debug_assertions)]
            if guard.slave_mid_transaction() {
                eprintln!(
                    "WARN: SPI bus '{}' deselected mid-transaction at chunk boundary \
                     (transfer spans the {:.3} ms chunk); reply may be truncated. \
                     Increase chunk_s or use a backend that surfaces chip-select.",
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
        let res = t.run_streaming_seeded(
            &self.circuit,
            chunk,
            self.last_dc_seed.as_deref(),
            |s| {
                final_x.clear();
                final_x.extend_from_slice(s.x);
            },
        );
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
                        self.branch_x[b] = final_x.get(self.layout.n_nodes + b).copied().unwrap_or(0.0);
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
        // and the consecutive streak drives the strict/CI abort (05 §3b).
        if converged {
            self.consecutive_failed_chunks = 0;
        } else {
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
                let v = if t >= t_start && t < t_end { high_volts } else { low_volts };
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

    /// False once any chunk's analog solve failed this run: the co-sim held stale
    /// voltages over at least one window, so it cannot be reported as a faithful
    /// analog result. Drives `analog_valid` in coverage and the co-sim JSON.
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

    fn update_stats(&mut self) {
        for (name, &node) in &self.net_nodes {
            let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
            let st = self.stats.entry(name.clone()).or_default();
            st.min_v = st.min_v.min(v);
            st.max_v = st.max_v.max(v);
            // Logic level with 2.5 V midpoint and 0.5 V hysteresis.
            let logic = if v > 3.0 {
                Some(true)
            } else if v < 2.0 {
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
    pub fn net_voltage(&self, net: &str) -> Option<f64> {
        let node = self.net_nodes.get(net)?;
        self.node_volts.get(node.0 as usize).copied()
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
    /// and may further narrow to nets with no static bias resistor — the case
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
                let Some((port, bit)) = gpio_of_role(role, m.binding.module) else {
                    continue;
                };
                // The firmware's most recent (and, for a held line, final) drive.
                if m.last_levels.get(&(port, bit)) != Some(&true) {
                    continue;
                }
                let Some(name) = name_of(node) else { continue };
                let Some(st) = self.stats.get(name) else { continue };
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

    /// MCU GPIO nets the firmware drove to a *defined* level during the run —
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
                let Some((port, bit)) = gpio_of_role(role, m.binding.module) else {
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
                let Some((port, bit)) = gpio_of_role(role, m.binding.module) else {
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

    /// Per edge-driven 74HC595 chain, the MCU GPIO `(port, bit)` bound to its
    /// SRCLK, RCLK, optional SRCLR_n, optional OE_n, and head SER, plus the chip
    /// count. Empty when no chain is clocked by the MCU (the old once-per-chunk
    /// path is used). Exposed for diagnostics and co-sim tests of the chain
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
/// For the QEMU backend the firmware path is the merged flash image, which QEMU
/// boots from at spawn; there is no separate load step (the trait's
/// `load_firmware` is a no-op for QEMU).
fn instantiate_mcu(
    binding: &McuBinding,
    firmware: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    let backend = binding.backend.as_str();

    let mut core: Box<dyn Mcu + Send> = if let Some(part) = backend.strip_prefix("renode:") {
        instantiate_renode(part)?
    } else if let Some(part) = backend.strip_prefix("qemu:") {
        instantiate_qemu(part, firmware)?
    } else {
        instantiate_avr(backend)?
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
    use hauksbee_mcu::{QemuBackend, QemuConfig};
    let config = match part {
        "esp32" => QemuConfig::esp32(),
        "esp32s3" => QemuConfig::esp32s3(),
        "esp32c3" => QemuConfig::esp32c3(),
        other => anyhow::bail!("unknown qemu backend part '{other}'"),
    };
    let flash = firmware.ok_or_else(|| {
        anyhow::anyhow!(
            "the qemu:{part} backend needs a merged flash image as the firmware \
             path (build it with esp-idf + esptool merge_bin)"
        )
    })?;
    Ok(Box::new(QemuBackend::new(config, flash)?))
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
    use hauksbee_mcu::{RenodeBackend, RenodeConfig};
    let config = match part {
        "stm32f103" => RenodeConfig::stm32f103(),
        "stm32f4_discovery" | "stm32f4" => RenodeConfig::stm32f4_discovery(),
        "nrf52840" | "nrf52" => RenodeConfig::nrf52840(),
        "sifive_fe310" | "fe310" => RenodeConfig::sifive_fe310(),
        other => anyhow::bail!("unknown renode backend part '{other}'"),
    };
    Ok(Box::new(RenodeBackend::new(config)?))
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

/// Wire `on_pin_change` / `on_uart` hooks into a shared capture buffer.
fn core_with_hooks(mut core: Box<dyn Mcu + Send>, binding: McuBinding) -> LiveMcu {
    let shared = Arc::new(Mutex::new(McuShared::default()));
    let pin_sink = shared.clone();
    // These callbacks fire from inside the MCU core's run loop, which for the
    // simavr backend is across an `extern "C"` FFI boundary where an unwind is
    // UB. So never panic on a poisoned lock: recover the guard and keep going
    // (the captured data is a simple accumulation buffer).
    core.on_pin_change(Box::new(move |pin: PinId, high: bool| {
        let mut sh = pin_sink.lock().unwrap_or_else(|e| e.into_inner());
        sh.pin_edges.insert((pin.port, pin.bit), high);
        sh.pin_edge_log.push((pin.port, pin.bit, high));
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
        logic_high_v,
    }
}

/// GPIO logic-high voltage by backend: STM32-class parts and the ESP32 family
/// are 3.3 V rails, the classic AVR parts are 5 V.
fn logic_high_for_backend(backend: &str) -> f64 {
    if backend.starts_with("renode:") || backend.starts_with("qemu:") {
        3.3
    } else {
        5.0
    }
}
