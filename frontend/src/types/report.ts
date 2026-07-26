// Analysis-report types, mirrors hauksbee-engine frontdoor.rs (WebReport et al)
// exactly. This is the JSON `/api/analyze` and `/api/analyze-with-firmware`
// return, and what `/api/startup` embeds under `report` when a board was
// preloaded via `hauksbee run <board> --serve`.

export interface WebFinding {
  /** "serious" | "warning" | "note" */
  level: string
  what: string
  why: string
  fix: string
}

/** An actionable heads-up note, glossed with the same what/why/what-to-do shape
 *  a finding gets. `why` / `fix` are omitted when the note is self-contained. */
export interface WebHeadsUp {
  what: string
  why?: string
  fix?: string
}

export interface WebSection {
  title: string
  verdict: string
  findings: WebFinding[]
  /** Actionable info-level notes; may be omitted when empty. */
  heads_up?: WebHeadsUp[]
}

export interface BindSummaryWeb {
  /** "M/N", active ICs that bound, over the total active ICs on the board. */
  critical_parts_bound: string
  /** Active ICs left open on the live circuit; may be omitted when empty. */
  active_path_unresolved?: string[]
}

export interface WebGpioNet {
  name: string
  volts: number
  driven: boolean
}

export interface WebFailedWindow {
  start_s: number
  end_s: number
}

export interface WebSpiFraming {
  bus: string
  /** "exact" | "backend" | "heuristic" */
  mode: string
}

export interface WebCosimSection {
  ran: boolean
  seconds_simulated: number
  uart_output: string
  findings: WebFinding[]
  gpio_nets?: WebGpioNet[]
  analog_valid: boolean
  failed_windows?: WebFailedWindow[]
  spi_framing?: WebSpiFraming[]
}

export interface WebComponent {
  reference: string
  value: string
  x: number
  y: number
  rot: number
}

export interface JsonNote {
  kind: string
  message: string
}

export interface WebReport {
  ok: boolean
  error?: string | null
  board_name: string
  file_name: string
  num_components: number
  num_nets: number
  headline: string
  serious: number
  total: number
  sections: WebSection[]
  components: WebComponent[]
  bind?: BindSummaryWeb | null
  notes?: JsonNote[]
  /** Every net name, sorted; the checks builder's pickers. */
  nets?: string[]
  /** Binder-detected supplies (rail net → nominal volts) for the checks
   *  builder's prefill. */
  supplies?: WebSupply[]
  cosim?: WebCosimSection | null
}

export interface WebSupply {
  net: string
  volts: number
}

/** What `/api/startup` returns: how the server was launched. */
export type Startup =
  | { preloaded: false }
  | { preloaded: true; board_name: string; report: WebReport | null }
