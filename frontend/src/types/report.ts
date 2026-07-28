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
  /** reference -> bound model kind ("mcu", "bjt_npn", ...); what a component
   *  click on the board map reports as the part's bound model. */
  component_kinds?: Record<string, string>
  /** Binder-detected supplies (rail net → nominal volts) for the checks
   *  builder's prefill. */
  supplies?: WebSupply[]
  cosim?: WebCosimSection | null
}

export interface WebSupply {
  net: string
  volts: number
}

/** What `/api/startup` returns: how the server was launched. `live` is true
 *  when the server can launch a live sim for an uploaded board
 *  (`POST /api/live/launch`); absent on older/non-live deployments, where the
 *  report falls back to the CLI hint. */
export type Startup =
  | { preloaded: false; live?: boolean }
  | { preloaded: true; board_name: string; report: WebReport | null; live?: boolean }

/** What `POST /api/live/launch` returns. */
export interface LiveLaunchResponse {
  ok: boolean
  error?: string
  board_name?: string
  /** True when an existing live session was replaced by this launch. */
  replaced?: boolean
}

/** One check queued from a board surface (a net or component click on the
 *  report map or the live sim) for the checks builder to append verbatim.
 *  `seq` orders and de-duplicates consumption across remounts. */
export interface QueuedCheck {
  seq: number
  kind: string
  net?: string
  ref?: string
}
