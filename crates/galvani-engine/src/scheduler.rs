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

use galvani_ir::{Circuit, Device, NodeId};
use galvani_mcu::{AvrMcu, Mcu, PinId};
use galvani_solve::{SolverOptions, Transient};

use crate::binder::{BoundBoard, McuBinding};
use crate::digital::DigitalComponent;

/// Default co-sim chunk size (seconds).
pub const DEFAULT_CHUNK_S: f64 = 100e-6;

/// Captured state shared between an MCU's C callbacks and the scheduler.
#[derive(Default)]
struct McuShared {
    /// Pin edges since last drain: (port, bit) -> latest level.
    pin_edges: HashMap<(char, u8), bool>,
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
}

/// The scheduler driving one bound board.
pub struct Scheduler {
    pub circuit: Circuit,
    pub net_nodes: HashMap<String, NodeId>,
    pub digital: Vec<DigitalComponent>,
    mcus: Vec<LiveMcu>,
    /// Latest solved node voltages, indexed by `NodeId.0`.
    pub node_volts: Vec<f64>,
    pub chunk_s: f64,
    pub opts: SolverOptions,
    pub sim_time: f64,
    /// Per-net toggle counters and min/max, for headless stats.
    pub stats: HashMap<String, NetStat>,
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
            ..
        } = bound;

        let mut live = Vec::new();
        for binding in mcus {
            let core = instantiate_mcu(&binding, firmware)?;
            live.push(core_with_hooks(core, binding));
        }

        let n_nodes = circuit.node_count();
        Ok(Scheduler {
            circuit,
            net_nodes,
            digital,
            mcus: live,
            node_volts: vec![0.0; n_nodes],
            chunk_s: DEFAULT_CHUNK_S,
            opts,
            sim_time: 0.0,
            stats: HashMap::new(),
        })
    }

    /// Number of live MCU cores.
    pub fn mcu_count(&self) -> usize {
        self.mcus.len()
    }

    /// Reference strings of the live MCUs (for serial routing).
    pub fn mcu_refs(&self) -> Vec<String> {
        self.mcus.iter().map(|m| m.binding.reference.clone()).collect()
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

        // 1. MCU: inject latest ADC voltages, run the chunk, drain captures.
        for m in &mut self.mcus {
            for (&ch, &node) in &m.binding.adc_nets {
                let v = self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0);
                m.core.set_analog_in(ch, v.max(0.0));
            }
            let _ = m.core.run_micros(micros);

            let (edges, bytes) = {
                let mut sh = m.shared.lock().unwrap();
                (
                    std::mem::take(&mut sh.pin_edges),
                    std::mem::take(&mut sh.uart_out),
                )
            };
            if !bytes.is_empty() {
                uart.entry(m.binding.reference.clone())
                    .or_default()
                    .extend(bytes);
            }
            // 2. Apply GPIO edges to drivers. An edge means the firmware has
            // configured the pin as a driven output, so enable the (initially
            // tri-stated) Thevenin leg before setting its level.
            for ((port, bit), level) in edges {
                m.last_levels.insert((port, bit), level);
                if let Some(drv) = m.binding.gpio_drivers.get_mut(&(port, bit)) {
                    drv.set_enabled(&mut self.circuit, true);
                    let v = if level { 5.0 } else { 0.0 };
                    drv.set_volts(&mut self.circuit, v);
                }
            }
        }

        // 5(prev). Digital components drive their outputs from current state,
        // sampling the previous chunk's solved node voltages.
        {
            let volts = self.node_volts.clone();
            let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
            for d in &mut self.digital {
                d.tick(&mut self.circuit, &node_v);
            }
        }

        // 3. Analog: solve a transient over the chunk; read final voltages.
        self.solve_chunk(chunk);

        // 4. Update running stats and advance time.
        self.sim_time += chunk;
        self.update_stats();
    }

    fn solve_chunk(&mut self, chunk: f64) {
        // Keep temperature in sync with the circuit's global temp.
        self.circuit.temp_c = self.opts.temperature_c;
        let t = Transient::new(self.opts);
        let n_nodes = self.circuit.node_count();
        let mut final_x: Vec<f64> = Vec::new();
        // Run a short transient; capture the last accepted step's unknowns.
        let res = t.run_streaming(&self.circuit, chunk, |s| {
            final_x.clear();
            final_x.extend_from_slice(s.x);
        });
        match res {
            Ok(()) => {
                self.node_volts.resize(n_nodes, 0.0);
                self.node_volts[0] = 0.0;
                for node in 1..n_nodes {
                    self.node_volts[node] = final_x.get(node - 1).copied().unwrap_or(0.0);
                }
            }
            Err(_) => {
                // Solver failure: hold previous voltages rather than crash the
                // whole co-sim (the chunk is short; next chunk may recover).
                self.node_volts.resize(n_nodes, 0.0);
            }
        }
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
                    *kind = galvani_ir::SourceKind::Dc(value);
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

    /// Digital component register states, for component-state frames.
    pub fn digital_states(&self) -> HashMap<String, HashMap<String, f64>> {
        self.digital
            .iter()
            .map(|d| (d.reference.clone(), d.state_summary()))
            .collect()
    }
}

/// What one `step` produced (beyond the in-place voltage/stat updates).
pub struct StepResult {
    pub sim_time: f64,
    pub uart: HashMap<String, Vec<u8>>,
}

/// Instantiate an MCU core for a binding and load firmware if given.
fn instantiate_mcu(
    binding: &McuBinding,
    firmware: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn Mcu + Send>> {
    let backend = binding.backend.as_str();
    let mut core: Box<dyn Mcu + Send> = if backend.contains("atmega328p") {
        let mut avr = AvrMcu::atmega328p_16mhz()?;
        avr.register_port_hooks(&['B', 'C', 'D']);
        Box::new(avr)
    } else {
        // Unknown backend: fall back to atmega328p so the co-sim still runs.
        let mut avr = AvrMcu::new("atmega328p", 16_000_000)?;
        avr.register_port_hooks(&['B', 'C', 'D']);
        Box::new(avr)
    };
    if let Some(fw) = firmware {
        core.load_firmware(fw)?;
    }
    Ok(core)
}

/// Wire `on_pin_change` / `on_uart` hooks into a shared capture buffer.
fn core_with_hooks(mut core: Box<dyn Mcu + Send>, binding: McuBinding) -> LiveMcu {
    let shared = Arc::new(Mutex::new(McuShared::default()));
    let pin_sink = shared.clone();
    core.on_pin_change(Box::new(move |pin: PinId, high: bool| {
        pin_sink
            .lock()
            .unwrap()
            .pin_edges
            .insert((pin.port, pin.bit), high);
    }));
    let uart_sink = shared.clone();
    core.on_uart(Box::new(move |b: u8| {
        uart_sink.lock().unwrap().uart_out.push(b);
    }));
    LiveMcu {
        core,
        binding,
        shared,
        last_levels: HashMap::new(),
    }
}
