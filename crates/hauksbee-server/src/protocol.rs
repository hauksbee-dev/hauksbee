//! The wire protocol between the simulation engine and the web frontend: tagged
//! JSON messages in both directions, evolved from the Tarski emulator's protocol
//! with generalized board and solver controls. [`ServerMessage`] is the
//! engine→client stream (board info, sim frames, probe data, status, errors); the
//! client→engine [`ClientMessage`] carries the play / pause / step and control
//! commands.

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
    Error {
        message: String,
    },
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
    /// Configurable supply nets and their current supply config (Feature 1).
    /// Net name -> the supply currently driving it. Additive; older clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub power_supplies: HashMap<String, PowerSupplyConfig>,
    /// Attached peripherals the UI can wire controls to (id, kind). E.g.
    /// ("BTN1","pushbutton"), ("POT1","potentiometer"), ("U2","i2c_bus"),
    /// ("VCD","vcd_sink"). Additive; older clients ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peripherals: Vec<PeripheralInfo>,
}

/// One attached peripheral, for the UI's control panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeripheralInfo {
    /// Stable id used in `SetPeripheral { id, .. }`.
    pub id: String,
    /// Kind string ("pushbutton", "potentiometer", "i2c_bus", ...).
    pub kind: String,
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
    /// Faults raised since the last frame (Feature 2). Additive; older clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<FaultInfo>,
    /// Live supply readout per supply net (Feature 1): rail current and SoC.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub supply_states: HashMap<String, SupplyState>,
}

/// A fault raised by the stress monitor, for the UI's fault list / overlay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultInfo {
    /// Component reference designator (e.g. "D1").
    pub component: String,
    /// Fault kind: "overcurrent" | "surge_current" | "overpower" |
    /// "overvoltage" | "reverse_bias" | "pin_overcurrent".
    pub kind: String,
    /// The offending live value (A, V, or W depending on kind).
    pub value: f64,
    /// The rating it exceeded (same units as `value`).
    pub limit: f64,
    /// Simulation time (s) the fault was raised.
    pub t: f64,
    /// Whether the circuit was mutated (destructive mode) in response.
    #[serde(default)]
    pub destroyed: bool,
}

/// Live readout of a configurable supply (Feature 1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SupplyState {
    /// Supply kind: "ideal" | "bench" | "wall" | "usb" | "battery".
    pub kind: String,
    /// Last measured rail current delivered into the net (A).
    pub current_a: f64,
    /// Battery state-of-charge (0..1); 1.0 for non-depleting supplies.
    pub soc: f64,
}

/// Serde-friendly mirror of the engine's `PowerSupply` for the wire (Feature
/// 1). The engine maps this onto its internal behavioral supply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PowerSupplyConfig {
    /// Ideal constant-voltage rail.
    Ideal { volts: f64 },
    /// Bench PSU: constant voltage with constant-current foldback.
    Bench { volts: f64, current_limit_a: f64 },
    /// Wall adapter: nominal volts behind output resistance, with ripple.
    Wall {
        volts: f64,
        r_out_ohms: f64,
        ripple_vpp: f64,
        ripple_hz: f64,
    },
    /// USB source: 5 V with droop and a hard foldback at the spec limit.
    Usb { spec: UsbSpecConfig },
    /// Battery pack: cells in series, draining a capacity from an initial SoC.
    Battery {
        chemistry: ChemistryConfig,
        cells: u32,
        capacity_mah: f64,
        soc: f64,
        r_internal_ohms: f64,
    },
}

/// USB power profile (wire mirror).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbSpecConfig {
    /// 5 V, 0.5 A.
    V5_0_5a,
    /// 5 V, 1.5 A.
    V5_1_5a,
    /// 5 V, 3.0 A.
    V5_3a,
}

/// Battery chemistry (wire mirror).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChemistryConfig {
    LiIon,
    Alkaline,
    NiMh,
    LiFePo4,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub running: bool,
    pub sim_time: f64,
    pub options: SolverControls,
}

/// Every physics effect and solver knob the UI exposes. Mirrors
/// hauksbee-solve's options; kept stringly-light here so the server does not
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
    /// When true, faults mutate the bound circuit (devices open/short) and the
    /// sim keeps running so the consequence is visible. Default false: faults
    /// are reported continuously but the circuit is untouched. Additive;
    /// older clients omit it and get the safe default.
    #[serde(default)]
    pub destructive_faults: bool,
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
            destructive_faults: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    Play,
    Pause,
    Step {
        dt: f64,
    },
    Reset,
    SetSpeed {
        factor: f64,
    },
    SetControls(SolverControls),
    LoadBoard {
        path: String,
    },
    /// Bytes typed at the virtual serial console of an MCU.
    Serial {
        mcu: String,
        data: Vec<u8>,
    },
    /// Drive an alternative input source bound to a net (slider, signal
    /// generator, file). The engine decides what "value" means per source.
    SetInput {
        source: String,
        value: f64,
    },
    /// Configure the power supply driving a supply net (Feature 1).
    SetPowerSupply {
        net: String,
        supply: PowerSupplyConfig,
    },
    /// Live-control a peripheral by id (button press/release, pot/encoder
    /// position, sensor temperature, stimulus level). `value` is interpreted
    /// per peripheral kind. Additive; older clients never send it. The existing
    /// `SetInput` is also routed to peripherals as a fallback so a frontend
    /// slider wired to a peripheral id works without changes.
    SetPeripheral {
        id: String,
        value: f64,
    },
    AddProbe {
        net: String,
    },
    RemoveProbe {
        net: String,
    },
}
