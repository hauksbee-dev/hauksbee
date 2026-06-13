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
}

// ============================================================
// Server → Client
// ============================================================

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
  /** Future: list of power supply net names. Optional chaining required. */
  power_supplies?: string[]
  /** Future: URL to the pre-exported GLB for 3D view. Optional chaining required. */
  glb_url?: string
}

export interface SimFault {
  component: string
  fault_kind: string
  value: number
  limit: number
  /** Simulation time when fault was detected */
  t: number
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
  /** Future: per-component faults. Optional chaining required. */
  faults?: SimFault[]
  /** Future: power supply state of charge per net. Optional chaining required. */
  power_supply_soc?: Record<string, number>
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

export type ServerMessage = BoardInfoMsg | SimFrame | StatusMsg | ProbeDataMsg | ErrorMsg

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
  /** Future: configure a power supply on a net. Gate on BoardInfo.power_supplies presence. */
  | { type: 'SetPowerSupply'; net: string; supply: Record<string, unknown> }
