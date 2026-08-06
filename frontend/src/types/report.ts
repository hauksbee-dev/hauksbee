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
  /** Board location (mm, layout space, same space as WebComponent x/y) when
   *  the finding points at one physical spot (a DRC short, the tightest
   *  clearance gap). Drives the "show on board" affordance. */
  x?: number
  y?: number
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
  /** The same parts with their detail, so the report can offer to do something
   *  about them rather than only naming them. Omitted when empty. */
  open_parts?: WebOpenPart[]
}

/** One part on the open-active-path list. `bound: false` means it has no model
 *  at all, which is the case a datasheet extraction can fix; `bound: true` means
 *  it bound and is open on the live circuit, which is a wiring matter. */
export interface WebOpenPart {
  reference: string
  value: string
  reason: string
  consequence: string
  active_ic: boolean
  bound: boolean
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

/** One immutable run assumption from hauksbee-ir. The sentence fields are the
 * canonical wording shared by CLI, JSON and web; clients display them rather
 * than paraphrasing the limitation. */
export interface EvidenceAssumption {
  id: string
  kind: string
  source: string
  scope: { type: string; value?: unknown }
  statement: string
  because: string
  consequence: string
  replacement: string
  expires?: string
}

/** The evidence attached to one reported assertion. Provenance fields are kept
 * structurally open here because their tagged IR variants remain additive; the
 * web needs the stable assertion/status/assumption contract to render trust. */
export interface EvidenceMap {
  assertion: string
  artifacts?: number[]
  models?: unknown[]
  parameters?: unknown[]
  assumptions?: string[]
  error_budget?: unknown
  coverage?: string
  status: 'clean' | 'qualified' | 'undermined'
}

export interface ArtifactProvenance {
  path: string
  kind: string
  role: string
  sha256?: string
  contributed?: Array<{ what: string; detail: string }>
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
  /** Canonical assumption registry and per-assertion maps from the engine. */
  inventory?: ArtifactProvenance[]
  assumptions?: EvidenceAssumption[]
  evidence?: EvidenceMap[]
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
  | { preloaded: false; live?: boolean; version?: string }
  | { preloaded: true; board_name: string; report: WebReport | null; live?: boolean; version?: string }

/** What `GET /api/live/status` returns: whether (and for which board) a live
 *  session is running server-side. The server holds ONE session globally, so
 *  this is the truth the client binds its live affordances to. */
export interface LiveStatus {
  active: boolean
  board_name?: string
}

/** What `POST /api/live/launch` returns. */
export interface LiveLaunchResponse {
  ok: boolean
  error?: string
  board_name?: string
  /** True when an existing live session was replaced by this launch. */
  replaced?: boolean
}

/** What `GET /api/models/extract/ready` returns: whether a datasheet extraction
 *  can run on this machine, and everything the consent step must say. The notice
 *  and the kind list come from the engine so the browser cannot drift from what
 *  `hauksbee models extract` shows and accepts. */
export interface ExtractReady {
  ready: boolean
  /** "codex" | "api" | "mock" */
  backend: string
  /** Why it cannot run (only when ready is false). */
  reason?: string | null
  /** The one command that fixes it (only when ready is false). */
  fix?: string | null
  /** The exact notice the user must read before anything is sent. */
  consent_notice: string
  /** "datasheet-extracted" */
  provenance: string
  kinds: { id: string; label: string }[]
  /** The model that runs when the user does not pick one, and how hard it
   *  thinks. Named on the page rather than described as "the default", since it
   *  is what is about to read their datasheet and bill their account. */
  default_model: string
  default_effort: string
  cost: string
}

/** One value in a drafted model, with the datasheet citation beside it. */
export interface CardValue {
  section: string
  key: string
  value: string
  source: string
  assumed: boolean
}

/** A drafted model, returned by `POST /api/models/extract` for review. Nothing
 *  has been written when this arrives; `POST /api/models/save` is what keeps it. */
export interface ModelCard {
  ok: boolean
  reference: string
  part: string
  kind: string
  provenance: string
  model_id: string
  description: string
  file_name: string
  toml: string
  values: CardValue[]
  assumptions: string[]
}

/** What `POST /api/models/save` returns. */
export interface ModelSaveResult {
  ok: boolean
  error?: string
  path?: string
  note?: string
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
