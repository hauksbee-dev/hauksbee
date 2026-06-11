// WebSocket protocol types -- mirrors galvani-server protocol shapes.
// Server uses serde tag = "type" so every message has a `type` discriminant.

// ============================================================
// Server → Client
// ============================================================

export interface BoardInfoMsg {
  type: 'BoardInfo'
  num_nets: number
  num_components: number
  board_file: string
}

export interface SimFrame {
  type: 'SimFrame'
  time_ms: number
  timestep: number
  running: boolean
  speed: number
  /** net name → voltage in V */
  net_voltages: Record<string, number>
  /** component ref → state */
  component_states: Record<string, ComponentVizState>
  /** Signal particles: net name → list of t∈[0,1] along the net's segments */
  signal_particles: Record<string, number[]>
}

export type ComponentVizState =
  | { kind: 'ShiftRegister'; bits: boolean[]; value: number }
  | { kind: 'Dac'; voltage: number; channel: number }
  | { kind: 'Comparator'; output_high: boolean }
  | { kind: 'Latch'; q: boolean }
  | { kind: 'Switch'; closed: boolean }
  | { kind: 'Generic'; label: string }

export type ServerMessage = BoardInfoMsg | SimFrame

// ============================================================
// Client → Server
// ============================================================

export type ClientMessage =
  | { type: 'Play' }
  | { type: 'Pause' }
  | { type: 'Step' }
  | { type: 'SetSpeed'; speed: number }
  | { type: 'AddProbe'; net_name: string }
  | { type: 'RemoveProbe'; net_name: string }
  | { type: 'GetBoardInfo' }
