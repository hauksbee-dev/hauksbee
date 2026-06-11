//! The wire protocol between engine and frontend: tagged JSON, evolved from
//! the Tarski emulator's protocol with generalized board/solver controls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Sent once on connect and whenever a new board is loaded.
    BoardInfo(BoardInfo),
    /// Live frame at the streaming rate while running.
    SimFrame(SimFrame),
    /// Time series for an explicit probe.
    ProbeData {
        net: String,
        time: Vec<f64>,
        volts: Vec<f64>,
    },
    Status(Status),
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardInfo {
    pub name: String,
    /// Relative URL the frontend fetches the original board file from for
    /// rendering (it has its own KiCad parser).
    pub board_url: String,
    pub num_components: usize,
    pub num_nets: usize,
    pub nets: Vec<String>,
    /// reference -> resolved model kind ("bjt_npn", "mcu", ...). Drives
    /// component-state coloring.
    pub component_kinds: HashMap<String, String>,
    /// MCUs available for interaction (reference, backend name).
    pub mcus: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimFrame {
    /// Simulation time in seconds.
    pub t: f64,
    /// Wall-clock speedup factor actually achieved.
    pub realtime_factor: f64,
    pub net_voltages: HashMap<String, f64>,
    /// reference -> small state map ("dissipation_mw", "conducting", ...).
    pub component_states: HashMap<String, HashMap<String, f64>>,
    /// UART bytes since last frame, per MCU reference.
    pub uart: HashMap<String, Vec<u8>>,
    /// Per-net current magnitude (A) for flow animation, when enabled.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub net_currents: HashMap<String, f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub running: bool,
    pub sim_time: f64,
    pub options: SolverControls,
}

/// Every physics effect and solver knob the UI exposes. Mirrors
/// galvani-solve's options; kept stringly-light here so the server does not
/// depend on solver internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolverControls {
    pub temperature_c: f64,
    pub parasitics: bool,
    pub junction_caps: bool,
    pub tolerances: bool,
    /// "trap" | "gear2"
    pub integration: String,
    /// Fixed timestep in seconds, or 0 for adaptive.
    pub fixed_dt: f64,
    /// 0.0..=1.0 granularity dial: 1.0 full physics, lower trades accuracy
    /// for speed (larger tolerances, coarser event resolution).
    pub granularity: f64,
}

impl Default for SolverControls {
    fn default() -> Self {
        SolverControls {
            temperature_c: 27.0,
            parasitics: false,
            junction_caps: true,
            tolerances: false,
            integration: "trap".into(),
            fixed_dt: 0.0,
            granularity: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    Play,
    Pause,
    Step { dt: f64 },
    Reset,
    SetSpeed { factor: f64 },
    SetControls(SolverControls),
    LoadBoard { path: String },
    /// Bytes typed at the virtual serial console of an MCU.
    Serial { mcu: String, data: Vec<u8> },
    /// Drive an alternative input source bound to a net (slider, signal
    /// generator, file). The engine decides what "value" means per source.
    SetInput { source: String, value: f64 },
    AddProbe { net: String },
    RemoveProbe { net: String },
}
