//! The wire protocol between a simulation engine and a web frontend: tagged
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
    /// Correlated receipt for an explicit live-session mutation. This is
    /// deliberately separate from `Error`: a refused attachment is a local
    /// action result, not evidence that the simulation itself died.
    ActionResult {
        action: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        ok: bool,
        message: String,
    },
    /// Sent once right after `BoardInfo` on every subscribe: the session's
    /// server-held history (accumulated faults, the active probe set), so a
    /// client that reloads mid-session rejoins with the story intact instead
    /// of a blank log over a sim that kept running. Additive; older clients
    /// ignore it.
    Backlog(SessionBacklog),
}

/// The server-held per-session history replayed to every new subscriber.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionBacklog {
    /// Every distinct (component, kind) fault the session has raised, first
    /// occurrence each, in the order they fired. Cleared on `Reset`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<FaultInfo>,
    /// Nets with an active probe (`AddProbe` without a matching `RemoveProbe`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<String>,
    /// The session's terminal failure, when it has one: the analog solve died
    /// irrecoverably or the engine panicked mid-step. Broadcast as an `Error`
    /// when it happens AND replayed here, so a client that connects (or
    /// reloads) afterwards still learns why the sim is stopped instead of
    /// staring at a frozen clock. Cleared on `Reset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fatal: Option<String>,
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
    /// Explicit engine-owned scalar inputs. Never inferred from a net name:
    /// sending `SetInput` with an arbitrary net does not drive that net, it only
    /// updates a source device whose id matches. Additive for older clients.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_sources: Vec<InputSourceInfo>,
    /// Copper-short honesty for the live sim: whether the DRC's detected
    /// shorts were bridged into this engine before it started streaming.
    /// Absent when the board has no detected shorts. The UI must disclose
    /// this the same way the report's co-sim block does, or the live rails
    /// read as an idealised un-shorted board. Additive; older clients ignore
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shorts: Option<ShortsDisclosure>,
}

/// Live-sim disclosure of what happened to the DRC's detected copper shorts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortsDisclosure {
    /// Copper shorts the geometric DRC detected on this board.
    pub detected: usize,
    /// How many of them were bridged into the live circuit before streaming.
    pub bridged: usize,
    /// Why nothing was bridged despite `detected > 0` (e.g. an unvalidated
    /// layout version makes the shorts potentially phantom). None when the
    /// shorts were applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unapplied_reason: Option<String>,
}

/// One attached peripheral, for the UI's control panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeripheralInfo {
    /// Stable id used in `SetPeripheral { id, .. }`.
    pub id: String,
    /// Kind string ("pushbutton", "potentiometer", "i2c_bus", ...).
    pub kind: String,
}

/// One input the running engine explicitly exposes to `SetInput`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSourceInfo {
    /// Stable source id accepted by `SetInput.source`.
    pub id: String,
    /// Human-facing quantity, currently `voltage` or `current`.
    pub kind: String,
    /// Display/control range; an engine contract, not a UI guess.
    pub min: f64,
    pub max: f64,
    pub initial: f64,
    pub unit: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimFrame {
    /// Simulation time in seconds.
    pub t: f64,
    /// Wall-clock speedup factor actually ACHIEVED: sim seconds advanced per
    /// wall second, measured over a rolling window where the sim loop steps.
    /// Never the requested multiplier; a sim that cannot keep up reports the
    /// smaller number it really delivered.
    pub realtime_factor: f64,
    /// The speed multiplier the user REQUESTED (`SetSpeed`). Distinct from
    /// `realtime_factor` on purpose: the UI must be able to show "requested
    /// 1.00x, achieving 0.31x" instead of conflating the two. Additive; older
    /// clients ignore it.
    #[serde(default)]
    pub requested_factor: f64,
    /// True while the sim loop is pacing BELOW the requested factor because
    /// the measured sustainable rate is lower (the honest cap): the requested
    /// rate is not achievable on this board/backend right now. Additive.
    #[serde(default)]
    pub rate_limited: bool,
    /// Nets connected to an MCU pin whose drive this backend has NOT observed:
    /// the pin's driver is still tri-stated and the backend cannot report drive
    /// direction (e.g. the ESP32 QEMU mailbox carries levels only, and models
    /// no GPSPI/I2C controller), so the shown voltage is the passive network's
    /// static level, not a measurement of MCU activity. Empty on backends with
    /// authoritative direction reporting (simavr, dir-mapped Renode), where an
    /// undriven pin's level IS a real measurement. Additive; older clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unobserved_drive_nets: Vec<String>,
    pub net_voltages: HashMap<String, f64>,
    /// Per-net `(min_v, max_v)` over the frame's whole chunk, not just the
    /// instant `net_voltages` sampled.
    ///
    /// A chunk is much longer than a bit-banged strobe, so a net that swung
    /// rail to rail a thousand times inside one chunk still lands on whatever
    /// level it happened to hold at the sample point, and a working board
    /// reads as a flat dead one. That is what made an EEPROM programmer look
    /// broken to someone who knew it was fine. The envelope is what tells a
    /// client that the flat number hides motion.
    ///
    /// The scheduler already tracked this for `hauksbee-ci`. Additive, and
    /// omitted when empty, so older clients ignore it.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub net_v_extremes: HashMap<String, (f64, f64)>,
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
    /// The session's current requested speed multiplier, so a client that
    /// (re)connects mid-session learns the real setting instead of assuming
    /// its local default. Additive; older clients ignore it.
    #[serde(default)]
    pub requested_factor: f64,
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
    /// Drive an input source explicitly listed in `BoardInfo.input_sources`.
    /// A board net name alone is never a source id.
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
    /// Attach a real control/stimulus to the running circuit. The browser also
    /// retains the equivalent `[[peripheral]]` spec for deterministic replay.
    AttachPeripheral(LivePeripheralSpec),
    /// Attach a validated declarative I2C/SPI register-map device to the
    /// running co-simulation. The exact spec bytes are also retained by the
    /// browser's scenario builder for deterministic replay.
    AttachRegisterMap(LiveRegisterMapSpec),
    AddProbe {
        net: String,
    },
    RemoveProbe {
        net: String,
    },
}

/// Safe, deliberately small live-attachment vocabulary. Bus devices require a
/// model/spec with identity and framing, so they are not smuggled through this
/// net-only control path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivePeripheralSpec {
    pub id: String,
    /// `stimulus` | `pushbutton` | `toggle`.
    pub kind: String,
    pub net: String,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub offset: Option<f64>,
    #[serde(default)]
    pub bounce_ms: Option<f64>,
    #[serde(default)]
    pub initial: Option<f64>,
}

/// Explicit live register-map attachment. No part name or selected component
/// is used to guess bus behavior: the browser sends exact validated spec bytes
/// and any physical input values the user chose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveRegisterMapSpec {
    pub id: String,
    /// Browser-generated correlation id. It has no simulation meaning and is
    /// echoed only in `ActionResult`, so repeated edits of the same device id
    /// cannot display a stale earlier receipt.
    #[serde(default)]
    pub request_id: Option<u64>,
    pub spec_toml: String,
    #[serde(default)]
    pub inputs: HashMap<String, f64>,
    #[serde(default)]
    pub controller: Option<String>,
    /// Required for SPI when exact chip-select framing is desired. If omitted,
    /// the scheduler's explicit controller/chunk framing limitations remain
    /// visible in the ordinary co-sim report.
    #[serde(default)]
    pub cs_net: Option<String>,
}
