// WebSocket protocol types -- mirrors hauksbee-server protocol.rs exactly.
// Server uses serde tag = "type" so every message has a `type` discriminant.

// ============================================================
// Shared structures
// ============================================================

export interface SolverControls {
  temperature_c: number   // -40..125°C
  parasitics: boolean
  junction_caps: boolean
  tolerances: boolean
  integration: 'trap' | 'gear2'
  fixed_dt: number        // seconds; 0 = adaptive
  granularity: number     // 0..1
  /** Faults mutate the circuit (destructive mode). Additive: server defaults false. */
  destructive_faults?: boolean
}

// ============================================================
// Power supplies (wire mirror of protocol.rs PowerSupplyConfig)
// ============================================================

/** USB power profile. serde snake_case of V5_0_5a / V5_1_5a / V5_3a. */
export type UsbSpecWire = 'v5_0_5a' | 'v5_1_5a' | 'v5_3a'

/** Battery chemistry. serde snake_case of LiIon / Alkaline / NiMh / LiFePo4. */
export type ChemistryWire = 'li_ion' | 'alkaline' | 'ni_mh' | 'li_fe_po4'

/** Tagged supply config exactly as the server (de)serializes it
 *  (`#[serde(tag = "kind", rename_all = "snake_case")]`). */
export type PowerSupplyWire =
  | { kind: 'ideal'; volts: number }
  | { kind: 'bench'; volts: number; current_limit_a: number }
  | { kind: 'wall'; volts: number; r_out_ohms: number; ripple_vpp: number; ripple_hz: number }
  | { kind: 'usb'; spec: UsbSpecWire }
  | {
      kind: 'battery'
      chemistry: ChemistryWire
      cells: number
      capacity_mah: number
      soc: number
      r_internal_ohms: number
    }

// ============================================================
// Server → Client
// ============================================================

/** One attached peripheral (id, kind) for the UI's control panel. */
export interface PeripheralInfo {
  id: string
  kind: string
}

export interface BoardInfoMsg {
  type: 'BoardInfo'
  name: string
  board_url: string
  num_components: number
  num_nets: number
  nets: string[]
  /** reference -> model kind ("bjt_npn", "mcu", ...) */
  component_kinds: Record<string, string>
  /** [(ref, backend_name), ...] -- serialized as nested arrays */
  mcus: [string, string][]
  /** Configurable supply nets: net name -> the supply currently driving it.
   *  A MAP on the wire (not a list of names); omitted when the board has none. */
  power_supplies?: Record<string, PowerSupplyWire>
  /** Attached peripherals; omitted when empty. */
  peripherals?: PeripheralInfo[]
  /** Future: URL to the pre-exported GLB for 3D view. Optional chaining required. */
  glb_url?: string
  /** Copper-short honesty: what happened to the DRC's detected shorts on the
   *  live engine. Absent when no shorts were detected. */
  shorts?: ShortsDisclosure
}

/** Live-sim disclosure of what happened to the DRC's detected copper shorts. */
export interface ShortsDisclosure {
  /** Copper shorts the geometric DRC detected on this board. */
  detected: number
  /** How many were bridged into the live circuit before streaming. */
  bridged: number
  /** Why nothing was bridged despite detected > 0 (e.g. unvalidated layout
   *  version); absent when the shorts were applied. */
  unapplied_reason?: string
}

export interface SimFault {
  component: string
  /** Fault kind ("overcurrent", "overpower", ...). Wire field is `kind`. */
  kind: string
  value: number
  limit: number
  /** Simulation time when fault was detected */
  t: number
  /** Whether the circuit was mutated (destructive mode) in response. */
  destroyed?: boolean
}

/** Live readout of a configurable supply (SimFrame.supply_states values). */
export interface SupplyStateWire {
  /** "ideal" | "bench" | "wall" | "usb" | "battery" */
  kind: string
  /** Last measured rail current delivered into the net (A). */
  current_a: number
  /** Battery state-of-charge (0..1); 1.0 for non-depleting supplies. */
  soc: number
}

export interface SimFrame {
  type: 'SimFrame'
  /** Simulation time in seconds */
  t: number
  realtime_factor: number
  /** net name → voltage in V */
  net_voltages: Record<string, number>
  /** component ref → state map (keys: "dissipation_mw", "running", "conducting", ...) */
  component_states: Record<string, Record<string, number>>
  /** UART bytes since last frame, per MCU reference */
  uart: Record<string, number[]>
  /** Per-net current magnitude (A), optional */
  net_currents?: Record<string, number>
  /** Faults raised since the last frame; omitted when empty. */
  faults?: SimFault[]
  /** Live supply readout per supply net; omitted when empty. */
  supply_states?: Record<string, SupplyStateWire>
}

export interface StatusMsg {
  type: 'Status'
  running: boolean
  sim_time: number
  options: SolverControls
}

export interface ProbeDataMsg {
  type: 'ProbeData'
  net: string
  time: number[]
  volts: number[]
}

export interface ErrorMsg {
  type: 'Error'
  message: string
}

/** Server-held session history replayed right after BoardInfo on subscribe:
 *  the accumulated fault log and the active probe set, so a mid-session
 *  reload rejoins with the story intact. */
export interface BacklogMsg {
  type: 'Backlog'
  /** First occurrence per (component, kind), in firing order; omitted when empty. */
  faults?: SimFault[]
  /** Nets with an active probe; omitted when empty. */
  probes?: string[]
}

export type ServerMessage = BoardInfoMsg | SimFrame | StatusMsg | ProbeDataMsg | ErrorMsg | BacklogMsg

// ============================================================
// Client → Server
// ============================================================

export type ClientMessage =
  | { type: 'Play' }
  | { type: 'Pause' }
  | { type: 'Step'; dt: number }
  | { type: 'Reset' }
  | { type: 'SetSpeed'; factor: number }
  | ({ type: 'SetControls' } & SolverControls)
  | { type: 'LoadBoard'; path: string }
  | { type: 'Serial'; mcu: string; data: number[] }
  | { type: 'SetInput'; source: string; value: number }
  | { type: 'AddProbe'; net: string }
  | { type: 'RemoveProbe'; net: string }
  /** Configure a power supply on a net. Gate on BoardInfo.power_supplies presence. */
  | { type: 'SetPowerSupply'; net: string; supply: PowerSupplyWire }
  /** Live-control a peripheral by id (value interpreted per peripheral kind). */
  | { type: 'SetPeripheral'; id: string; value: number }
