// Analysis-report types, mirrors hauksbee-engine frontdoor.rs (WebReport et al)
// exactly. This is the JSON `/api/analyze` and `/api/analyze-with-firmware`
// return, and what `/api/startup` embeds under `report` when a board was
// preloaded via `hauksbee run <board> --serve`.

import type { BenchReport } from './bench'
export type * from './bench'

/** The C5.3 contract emitted by every invalid-for-analysis result. */
export interface RefusalContract {
  claim: string
  missing_prerequisite: string
  valid_partial_conclusions: string[]
  next_action: string
}

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

/** What an action does. A closed set: the app has one handler per id (the
 *  engine's `ActionId`), so a step the app cannot run is never sent as prose. */
export type ActionId =
  | 'model_parts'
  | 'extract_unmodelled'
  | 'add_bom'
  | 'choose_models_folder'
  | 'add_scenario'
  | 'add_supply'
  | 'add_ac_sweep'
  | 'add_firmware'
  | 'open_check'
  | 'show_on_board'
  | 'start_checks'
  | 'add_project_rules'
  | 'set_part_number'
  | 'mark_not_fitted'
  | 'install_backend'

/** The arguments a handler reads; every field is omitted when unset. */
export interface ActionArgs {
  refs?: string[]
  net?: string
  /** A check kind from the check catalogue. */
  check?: string
  /** A file name the action asks for (`.kicad_pro`). */
  file?: string
  /** A simulator backend id. */
  backend?: string
  volts?: number
  /** A board position, mm, layout space. */
  x?: number
  y?: number
}

/** One button the engine offers: the structured form of a next step the CLI
 *  prints as a command. `label` is the plain-English words for the button. */
export interface WebAction {
  id: ActionId
  label: string
  args?: ActionArgs
}

export interface WebSection {
  title: string
  verdict: string
  findings: WebFinding[]
  /** Actionable info-level notes; may be omitted when empty. */
  heads_up?: WebHeadsUp[]
  /** Buttons that move this section forward (give the open parts models, add
   *  the project rules file). Present on the section that states the problem in
   *  full; omitted when there is nothing to offer. */
  actions?: WebAction[]
}

export interface BindSummaryWeb {
  /** "M/N", active ICs that bound, over the total active ICs on the board. */
  critical_parts_bound: string
  /** Active ICs left open on the live circuit; may be omitted when empty. */
  active_path_unresolved?: string[]
  /** The same parts with their detail, so the report can offer to do something
   *  about them rather than only naming them. Omitted when empty. */
  open_parts?: WebOpenPart[]
  /** Parts whose board name matches several orderable variants of one family.
   *  Omitted when empty. */
  open_selections?: WebOpenSelection[]
}

/** A family the board does not pin to one variant. `same_pinout: true` binds
 *  the first non-C member as a flagged stand-in (CI sweeps every member);
 *  false leaves the part OPEN until a `[[override]] select` picks one. */
export interface WebOpenSelection {
  reference: string
  value: string
  family: string
  members: Array<{ id: string; selector: string; basis: string; verdict: 'S' | 'C' | 'U'; reason: string }>
  determined: string | null
  via: 'board' | 'spec' | 'sweep'
  same_pinout: boolean
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
  /** Set when the part is open because two parts share its designator: the fix
   *  is to rename one, so no model drafting is offered. */
  rename_fix?: string
}

export interface WebGpioNet {
  name: string
  volts: number
  driven: boolean
  /** Level changes over the run; absent on engines that predate it. */
  toggles?: number
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

export interface WebTimingCoverage {
  mcu_ref: string
  backend: string
  cycle_exact: boolean
  timestamp_precision_s: number
  minimum_guaranteed_pulse_s: number
  chunk_s: number
}

export interface WebFallbackWindow {
  start_s: number
  end_s: number
  method: string
  fidelity_note: string
  error_estimate_v?: number
  state_jump_v?: number
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
  timing_coverage?: WebTimingCoverage[]
  timing_refusals?: string[]
  fallback_windows?: WebFallbackWindow[]
  error_budget?: ErrorBudget
  /** What the run looked like from the bench: pins, serial, buses, events. */
  bench?: BenchReport | null
  /** True when the board's MCU needs an emulator (Renode, QEMU) that the
   *  synchronous analysis never runs: the app offers to run it as a job. */
  external_backend?: boolean
}

export interface WebComponent {
  reference: string
  value: string
  x: number
  y: number
  rot: number
}

export interface WebImportObject {
  id: string
  status: 'recovered' | 'partial'
  confidence: 'high' | 'medium' | 'low'
  x?: number
  y?: number
  explanation: string
  nets?: string[]
}

export interface WebImportIssue {
  kind: 'split_net' | 'missing' | string
  title: string
  explanation: string
  suggested_fix: string
  net?: string
}

export interface WebImportDiagnostics {
  format: string
  recovered: number
  partial: number
  unplaced: number
  missing_or_refused: number
  objects: WebImportObject[]
  issues?: WebImportIssue[]
}

/** Which refusal this was (engine `ImportFailureKind`, snake_case). */
export type ImportFailureKind =
  | 'truncated'
  | 'syntax_error'
  | 'wrong_content'
  | 'unsupported_extension'
  | 'empty_file'
  | 'unsupported_format'
  | 'invalid_content'
  | 'fab_package'
  | 'schematic_sheets'
  | 'unreadable'

/** One board format hauksbee reads, served as data beside a refusal. */
export interface WebSupportedFormat {
  vendor: string
  exts: string[]
}

export interface WebImportFailure {
  stage: string
  excerpt?: string
  suggested_fix: string
  // Everything below is sent by current engines only; an older server sends
  // just the three fields above, and `UploadFailure` falls back to phrase
  // matching on the error text for it.
  kind?: ImportFailureKind
  /** The file as named on upload. */
  file?: string
  /** Lower case with the dot; absent when the file has none. */
  extension?: string
  /** Where the parser stopped, when it said. */
  line?: number
  byte?: number
  /** `wrong_content`: what the contents are, when recognisable ("a Git LFS pointer"). */
  detected_format?: string
  /** `wrong_content`: what the extension promised ("a KiCad board"). */
  expected_format?: string
  /** The engine's plain wording: heading, one-sentence cause, ordered steps. */
  title?: string
  cause?: string
  steps?: string[]
  /** The formats hauksbee reads, so a refusal can list them without prose. */
  supported_formats?: WebSupportedFormat[]
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
  /** The glossary term the sentence is an instance of (wire name). */
  term?: string
  /** What closes the gap, as data (from hauksbee-ir `Remedy`). */
  remedy?: Remedy
}

/** `Remedy` on the wire: tagged by `type`. `other` means "follow the
 *  assumption's `replacement` sentence". */
export type Remedy =
  | { type: 'add_supply'; net: string; volts_hint?: number }
  | { type: 'add_model'; reference: string }
  | { type: 'set_part_number'; reference: string }
  | { type: 'mark_dnp'; reference: string }
  | { type: 'rename_duplicate'; reference: string; conflict: string }
  | { type: 'add_firmware' }
  | { type: 'add_bom' }
  | { type: 'install_backend'; id: string }
  | { type: 'provide_project_rules'; file: string }
  | { type: 'other' }

export interface ErrorBudgetWindow {
  start_s: number
  end_s: number
}

export interface ErrorBudget {
  tolerance: {
    reltol: number
    vntol: number
    abstol: number
    chgtol: number
  }
  methods?: Array<{
    window: ErrorBudgetWindow
    method: 'trapezoidal' | 'gear2' | 'backward-euler' | 'reduced-step' | 'cold-start-backward-euler' | 'subdivided-backward-euler'
  }>
  residual?: { max_abs: number; at: string }
  failed_windows?: ErrorBudgetWindow[]
  event_time_error_s?: number
  /** Model intervals are additive and producer-specific; the UI currently
   * reports their count rather than interpreting unlike physical quantities. */
  model_uncertainty?: Array<Record<string, unknown>>
}

/** The evidence attached to one reported assertion. Provenance fields are kept
 * structurally open here because their tagged IR variants remain additive; the
 * web needs the stable assertion/status/assumption contract to render trust. */
export interface EvidenceMap {
  assertion: string
  artifacts?: number[]
  models?: ModelOnPath[]
  /** The same models grouped by shared model, source and confidence; the
   *  engine sends these instead of `models` when a map lists more than 256. */
  model_groups?: ModelGroup[]
  parameters?: unknown[]
  /** The same parameters grouped by shared value and origin, sent instead of
   *  `parameters` for a long list. */
  parameter_groups?: Array<Record<string, unknown>>
  assumptions?: string[]
  error_budget?: ErrorBudget
  coverage?: string
  status: 'clean' | 'qualified' | 'undermined'
}

export type ModelUncertainty =
  | { status: 'interval'; parameter: string; low: number; high: number; unit: string; kind: 'specification-limits' | 'empirical-error' | 'typical-range' | 'estimated-range'; basis: string }
  | { status: 'unknown'; parameter: string; reason: string }

export interface ModelSource {
  tier: string
  layer: string
  origin: string
  validation: string
  uncertainty: ModelUncertainty[]
}

export interface ModelOnPath {
  reference: string
  model_id: string
  source: ModelSource
  confidence: string
}

/** Parts that share one model, source and confidence. `subjects` runs parallel
 *  to `references` and is absent when every identity is its reference. */
export interface ModelGroup {
  model_id: string
  layer: string
  confidence: string
  source: ModelSource
  references: string[]
  subjects?: string[]
}

export interface ArtifactProvenance {
  path: string
  kind: string
  role: string
  sha256?: string
  contributed?: Array<{ what: string; detail: string }>
}

export type ModelCoverageStage =
  | 'unresolved'
  | 'identity_only'
  | 'executable_scope_unspecified'
  | 'executable_partial'
  | 'executable_declared'
  | 'ignored'

export interface ModelCoverageReference {
  url: string
  title: string
  locator: string
  sha256?: string
}

export interface ModelCoveragePin {
  number: string
  function: string
  kind: string
  net?: string | null
  position_mm?: [number, number] | null
}

/** One exact extracted component and what its winning model can actually do.
 *  `stage` is intentionally richer than bound/open: identity-only and partial
 *  cards must never be presented as complete simulation. */
export interface ModelCoverageComponent {
  reference: string
  value: string
  lib_id: string
  footprint: string
  stage: ModelCoverageStage
  actionable_behavior_gap: boolean
  /** Duplicate designator: the sentence that fixes it; a model cannot. */
  rename_fix?: string
  model_id?: string | null
  model_kind?: string | null
  layer: string
  origin: string
  source: Omit<ModelSource, 'uncertainty'> & { uncertainty?: unknown[] }
  references?: ModelCoverageReference[]
  properties?: Array<{ name: string; value: string }>
  static_contracts?: string[]
  implements?: string[]
  missing?: string[]
  pins: ModelCoveragePin[]
}

export interface ModelCoverageSnapshot {
  schema_version: number
  summary: {
    active_connected: number
    identified: number
    executable_available: number
    unresolved: number
    identity_only: number
    executable_scope_unspecified: number
    executable_partial: number
    executable_declared: number
    ignored: number
    actionable_behavior_gaps: number
    authoring_targets: number
    requirements_total: number
    requirements_met: number
    requirements_unmet: number
  }
  components: ModelCoverageComponent[]
  authoring_targets: ModelCoverageComponent[]
}

/** What to fix first, from the engine's one ordering (the CLI prints the same).
 *  `notice` is set on a board with placed pads and no copper: its copper
 *  findings are held back until it is routed. */
export interface FixFirst {
  notice?: string
  items: Array<WebFinding & { similar: number }>
  /** Serious causes beyond the ones listed. */
  more: number
  /** On a pre-layout board, how many copper findings were held back. */
  copper_held?: number
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
  fix_first?: FixFirst
  sections: WebSection[]
  components: WebComponent[]
  import_diagnostics?: WebImportDiagnostics | null
  import_failure?: WebImportFailure | null
  bind?: BindSummaryWeb | null
  /** Exact model identity and executable-behaviour scope, shared with the CLI
   *  coverage command. Present even when every part technically bound. */
  model_coverage?: ModelCoverageSnapshot | null
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
  /** Component-scoped assertion kinds the bound circuit can evaluate. */
  component_assertions?: Record<string, string[]>
  /** Ratings the bound models state, by reference; a key is absent when no
   *  model states it. Never a datasheet citation: the page is not recorded. */
  component_ratings?: Record<string, ComponentRatings>
  /** Binder-detected supplies (rail net → nominal volts) for the checks
   *  builder's prefill. */
  supplies?: WebSupply[]
  cosim?: WebCosimSection | null
  /** Why a requested conclusion could not be made, when co-sim was invalid. */
  refusal?: RefusalContract | null
  /** Why the verdict is not a pass, and what to do next: one sentence, the
   *  same text the CLI, `--json` and the CI report carry. */
  verdict_reason?: string
  /** `verdict_reason` without its terminal-command "next:" clause: show this
   *  sentence and render `next_actions` as buttons instead. */
  verdict_why?: string
  /** The steps that would move the verdict forward, as buttons. Omitted when
   *  nothing is open. */
  next_actions?: WebAction[]
  /** The assumptions a pass rests on beyond the checks, one sentence each
   *  (e.g. "Q1 passes across assumed corners (...); put the MPN on Q1 to
   *  remove this assumption."): the same list `--json` and the CLI's
   *  "VERDICT: clean, conditionally: ..." carry. Absent when there are none or
   *  the verdict is not a pass. */
  verdict_conditions?: string[]
  /** The machine verdict, the same one `run --check --json` prints for this
   *  board: a fail or an invalid run never reads as a pass in the browser.
   *  Absent on reports from engines that predate it. */
  verdict?: 'pass' | 'fail' | 'invalid'
  /** On a pass with items the order triage lists to inspect, the verdict's tag
   *  for it ("inspect before ordering (3 items)"): the words the CLI's closing
   *  `VERDICT:` line uses, so the browser never calls that pass clean. Absent
   *  otherwise. */
  verdict_inspect?: string
  /** True when the verdict is PARTLY JUDGED: `invalid`, never a pass. Some key
   *  parts have no usable model, so the checks they could change were not
   *  judged; `judgement_scope` says which. The same flag `--json` prints. */
  partly_judged?: boolean
  /** What a partly judged run judged and what it did not, in the engine's
   *  words (`--json` carries the same object). */
  judgement_scope?: JudgementScope
  /** Every term the page can explain, in the engine's words. */
  glossary?: GlossaryRow[]
  /** The `run --check --json` document this verdict was derived from, as the
   *  CLI prints it (same serializer). Absent from older engines and from
   *  sessions saved before it existed; the export then rebuilds the verdict
   *  fields from this report. */
  check_report?: CheckReportJson | null
  /** Set instead of `check_report` when the document was too large to send
   *  (its size in bytes); the export then rebuilds the verdict fields and says
   *  to run the reproduce command for the full document. */
  check_report_omitted_bytes?: number | null
}

/** One finding of the `run --check --json` document. */
export interface CheckReportFinding {
  check: string
  kind: string
  /** "serious" | "warning" | "info" */
  severity: string
  nets?: string[]
  location_mm?: [number, number] | null
  layer?: string | null
  refs?: string[]
  actionable?: boolean
  message: string
  plain?: string
  fix?: string | null
}

/** The `run --check --json` document (`hauksbee-run-report.schema.json`):
 *  the fields the browser reads, the rest passed through untouched. */
export interface CheckReportJson {
  ok: boolean
  verdict: 'pass' | 'fail' | 'invalid'
  partly_judged?: boolean
  verdict_reason?: string
  serious_count: number
  actionable_count: number
  schema_version?: number
  board?: string
  findings?: CheckReportFinding[]
  verdict_conditions?: string[]
  judgement_scope?: JudgementScope
  [field: string]: unknown
}

/** The key parts with no usable model, and the nets whose checks they could
 *  change (those were not judged). */
export interface JudgementScope {
  open_parts: string[]
  /** Named nets on the board; each has a wiring/value claim and an electrical claim. */
  nets: number
  /** Nets whose wiring/value checks were not judged. */
  wiring_dependent: string[]
  /** Nets whose electrical checks (levels, ratings, rails) were not judged. */
  electrical_dependent: string[]
}

export interface WebSupply {
  net: string
  volts: number
}

/** What `/api/startup` returns: how the server was launched. `live` is true
 *  when the server can launch a live sim for an uploaded board
 *  (`POST /api/live/launch`); absent on older/non-live deployments, where the
 *  report falls back to the CLI hint. */
export type Startup = StartupCommon & (
  | { preloaded: false }
  | { preloaded: true; board_name: string; report: WebReport | null }
)

/** The fields both startup shapes carry. `board_formats` is the engine's one
 *  accepted-formats table (see lib/board-formats). */
interface StartupCommon {
  live?: boolean
  avr?: boolean
  version?: string
  board_formats?: import('../lib/board-formats').ServedBoardFormats
}

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
/** Another installed, signed-in backend that could run this extraction. */
export type ExtractAlternative = Pick<ExtractReady,
  'backend' | 'backend_label' | 'sends_data_to' | 'cost' | 'default_model' | 'default_effort'> & { id: string }

export interface ExtractReady {
  /** Other backends that could run it right now (only when blocked). */
  alternatives?: ExtractAlternative[]
  ready: boolean
  /** "codex" | "claude-code" | "agy" | "api" | "mock" */
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
  /** The active backend's human label ("Claude Code"), when the server reports
   *  one; falls back to `backend` itself when absent (an older server). */
  backend_label?: string
  /** One sentence naming where the active backend was chosen from ("from
   *  ~/.config/hauksbee/extract.toml", "overridden by $VAR for this server
   *  process", ...). */
  backend_source?: string
  /** Who the datasheet's text actually goes to, for the active backend. */
  sends_data_to?: string
  /** Where the backend config file lives (whether or not it exists yet). */
  config_path?: string
}

/** Where a resolved setting's effective value came from. */
export type ExtractBackendSource =
  | { kind: 'config' }
  | { kind: 'env'; name: string }
  | { kind: 'auto' }
  | { kind: 'flag' }

export interface ExtractAgentConfig {
  command?: string
  model?: string
  effort?: string
  permission_mode?: string
  profile?: string
  extra_args?: string[]
  /** Models tried in order when the previous tier fails. Absent = the backend's
   *  built-in ladder; `[]` = never escalate. */
  escalate?: string[]
}

export type ExtractCodexBackendConfig = ExtractAgentConfig
export type ExtractClaudeCodeBackendConfig = ExtractAgentConfig
export type ExtractAgyBackendConfig = ExtractAgentConfig

export interface ExtractApiBackendConfig {
  base_url?: string
  model?: string
  api_key_env?: string
}

/** What is actually saved to `extract.toml`: every field optional/absent when
 *  the user never set it. */
export interface ExtractBackendsConfig {
  backend?: string
  retries?: number
  timeout_secs?: number
  codex?: ExtractCodexBackendConfig
  'claude-code'?: ExtractClaudeCodeBackendConfig
  agy?: ExtractAgyBackendConfig
  api?: ExtractApiBackendConfig
}

/** The same shape as `ExtractBackendsConfig`'s per-backend blocks, but with
 *  every field resolved to the value that will actually be used. */
export interface ExtractResolvedAgent {
  command?: string
  model: string
  effort: string
  permission_mode?: string
  profile?: string | null
  extra_args: string[]
  /** The effective ladder after the first model (always an array). Absent only
   *  from an older server. */
  escalate?: string[]
}

export type ExtractResolvedCodex = ExtractResolvedAgent
export type ExtractResolvedClaudeCode = ExtractResolvedAgent
export type ExtractResolvedAgy = ExtractResolvedAgent

export interface ExtractResolvedApi {
  base_url: string
  model: string
  api_key_env: string
}

export interface ExtractResolved {
  backend: string
  backend_source: ExtractBackendSource
  retries: number
  timeout_secs: number
  codex: ExtractResolvedCodex
  claude_code: ExtractResolvedClaudeCode
  agy: ExtractResolvedAgy
  api: ExtractResolvedApi
  /** Environment variables that are currently overriding a saved value, for
   *  this server process (e.g. `HAUKSBEE_CLAUDE_MODEL`). */
  env_overrides: string[]
}

export interface ExtractBackendAvailability {
  id: string
  label: string
  backend: string
  available: boolean
  detail: string
  install: string
  sends_data_to: string
}

export interface ExtractPreset {
  id: string
  label: string
  /** The terminal's wording (it may name a command). */
  summary: string
  /** The app's wording: plain sentences, no terminal command. */
  web_summary?: string
  backend: string
  model: string
  effort: string
  api_base: string
  api_key_env: string
  recommended: boolean
  /** The cheaper way to use the same tool as the recommended preset (at most one
   *  per tool). Absent from an older server. */
  cheaper?: boolean
  /** The models tried after `model` fails, in order. Absent from an older server. */
  escalate?: string[]
  /** A caution the server attaches to this preset, shown as written. */
  badge?: string
}

/** One picker entry; the note is the server's single wording for CLI and web. */
export interface ModelChoice {
  name: string
  note: string
  /** Tokens of context each stage may use; non-zero means the model runs the
   *  staged harness. 0 or absent = unbounded. */
  context_budget?: number
  /** Short display name for ladders ("Haiku 5.5"), chosen by the server. */
  label?: string
  /** A caution the server attaches to this tier, shown as written (for
   *  example "below Luna baseline"). */
  badge?: string
  /** True for the model the server would run when none is chosen. */
  default?: boolean
}

export interface ExtractOptions {
  efforts: Record<string, string[]>
  permission_modes: Record<string, string[]>
  models: Record<string, ModelChoice[]>
  /** Per backend, the name prefix of a model that is "listed" (a name that does
   *  not start with it gets no "not in the list" hint). Absent from an older server. */
  model_hint_prefix?: Record<string, string>
}

export interface ExtractSettingsKey {
  key: string
  help: string
  default: string
}

/** What `GET /api/settings/extract` returns, and what `PUT`, and each preset
 *  POST, echo back after applying the change. */
export interface ExtractSettings {
  path: string
  exists: boolean
  /** Present only when the file exists but could not be parsed. */
  load_error?: string
  config: ExtractBackendsConfig
  resolved: ExtractResolved
  summary: string
  backends: ExtractBackendAvailability[]
  presets: ExtractPreset[]
  /** The id of the preset a fresh install is steered to, chosen by the engine
   *  (the same rule as `hauksbee models backend setup`). Absent from an older
   *  server. */
  default_preset?: string
  options: ExtractOptions
  keys: ExtractSettingsKey[]
  consent_notice: string
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
  /** Why a saved model left coverage where it was, when it did. */
  coverage_note?: string | null
}

/** The assertion values the board-side constraint modal can edit before a
 *  check is queued. Every one is the raw string the field held, so an empty
 *  box stays absent rather than becoming a guessed number. */
export interface CheckConstraints {
  min?: string
  max?: string
  after_ms?: string
  deadline_ms?: string
  contains?: string
  freq_hz?: string
  tolerance?: string
  min_toggles?: string
  amps?: string
  celsius?: string
  /** Which side of the window the row bounds: 'dip' or 'spike'. */
  rail_polarity?: string
  dip_below?: string
  for_max_ms?: string
  recover_to?: string
  recover_within_ms?: string
  spike_above?: string
  spike_for_max_ms?: string
  settle_to?: string
  settle_within_ms?: string
}

/** The limits a part's bound model states (amps, watts, volts). */
export interface ComponentRatings {
  max_current_a?: number
  max_pin_current_a?: number
  max_power_w?: number
  max_voltage_v?: number
  vih?: number
  vil?: number
}

/** What a board surface (a net or component click on the report map or the
 *  live sim) asks the checks builder for. Each stays typed rather than
 *  collapsing into "a row": a stimulus or a supply changes the experiment,
 *  while a check judges it, and a register-map device needs a human-authored
 *  spec before it can run. Nothing here is guessed from a part name. */
export type BoardRequest =
  /** `run_ms`: the spec's run length, raised when the check needs a longer window. */
  | ({ type: 'check'; kind: string; net?: string; ref?: string; run_ms?: string } & CheckConstraints)
  | { type: 'peripheral'; id?: string; kind: 'stimulus' | 'pushbutton' | 'toggle'; net?: string; ref?: string }
  | { type: 'sensor'; id: string; ref?: string; modelId?: string | null }
  /** An ideal scenario supply: the checks runner rebuilds the circuit with it
   *  on the next run; it is not a live post-solve voltage force. */
  | { type: 'supply'; net: string; volts?: number }
  /** Settle an open variant selection with `[[override]] select`. */
  | { type: 'select'; ref: string; select: string }
  /** One exact `[[assert]]` table from the in-place editor, queued as written. */
  | { type: 'assert'; kind: string; toml: string
    /** The run length (ms) the check needs, raised never lowered, like a `check` request's. */
    run_ms?: string }

/** A request in the queue; `seq` orders and de-duplicates consumption across
 *  remounts. */
export type QueuedRequest = BoardRequest & { seq: number }

/** Exact register-map bytes queued for immediate attachment to the current
 * live session. This is separate from QueuedSensor: the latter opens an
 * authoring row, while this request is only created after that row validates. */
export interface QueuedLiveRegisterMap {
  seq: number
  id: string
  spec_toml: string
  inputs: Record<string, number>
  controller?: string
  cs_net?: string
}

/** One optional co-sim backend or oracle, from `GET /api/deps` (the engine's
 *  own discovery, the same resolvers a real run uses). */
export interface DepInfo {
  id: string
  name: string
  present: boolean
  path: string | null
  version: string | null
  unlocks: string
  /** What is missing without it, in plain words (engine text). Absent on an older server. */
  without?: string
  installable: boolean
  cost: string
  manual: string
  detail: string | null
  /** Present only on a dependency that sends the user's data off this machine;
   *  rendered as its own line rather than folded into `unlocks`. */
  sends_data_offhost?: string | null
  /** Datasheet-extraction CLIs only: from the CLI's own read-only status command.
   *  `not_checked` = the CLI has no status command. */
  sign_in?: 'signed_in' | 'not_signed_in' | 'not_checked' | null
}

/** The machine fingerprint beside the dependency rows in `GET /api/deps`: what
 *  a sign-off record needs to name the tools a result came from. */
export interface DepsEnvironment {
  engine_version: string
  exe_sha256: string | null
  os: string
  arch: string
  doctor?: unknown
}

/** One bundled register-map behaviour, from `GET /api/sensor-specs`. */
export interface SensorCatalogEntry {
  id: string
  name: string
  bus: 'i2c' | 'spi'
  scope: string
  spec_toml: string
}

/** One assertion's verdict from a hauksbee-ci run. */
export interface CheckResult {
  label: string
  kind: string
  passed: boolean
  invalid: boolean
  /** The check carried no bound, so it reports a value and judges nothing. */
  measured?: boolean
  detail: string
  /** What to do about a failed result, when the engine says (the board's own
   *  static check always does). */
  why?: string
  /** Every part and net a long `label` stands for ("... and 44 more"); absent
   *  when the label names them all. */
  all_subjects?: string[]
  evidence?: EvidenceMap
}

/** One supply `hauksbee-ci init` would declare for a board. */
export interface ScaffoldSupply {
  net: string
  kind: 'usb' | 'bench'
  volts: number
  usb?: string
  current_limit_a?: number
  /** `rail` (the board names it) or `regulator_input` (nothing else powers it). */
  origin: 'rail' | 'regulator_input'
}

/** What `POST /api/ci/scaffold` answers with. */
export interface ScaffoldResponse {
  ok: boolean
  error?: string
  supplies?: ScaffoldSupply[]
}

/** What `POST /api/check` answers with. Errors arrive in the body, so a
 *  non-2xx status is still parsed rather than replaced with a status line. */
export interface RunResponse {
  ok: boolean
  error?: string
  passed?: boolean
  exit_code?: number
  analog_abort?: boolean
  refusal?: RefusalContract
  /** The one-sentence reason the run is not green, shared with the CLI. */
  verdict_reason?: string
  coverage?: string | null
  substitutions?: string[]
  coverage_warnings?: string[]
  /** What a pass rests on beyond its assertions, one sentence each: open
   *  variant or corner sets the member sweep could not cross in full. Omitted
   *  when every combination ran. */
  verdict_conditions?: string[]
  inventory?: ArtifactProvenance[]
  assumptions?: EvidenceAssumption[]
  evidence?: EvidenceMap[]
  timing_coverage?: WebTimingCoverage[]
  timing_refusals?: string[]
  results?: CheckResult[]
  /** What the verdict and each assumption's terms mean, from hauksbee-ir. */
  glossary?: GlossaryRow[]
  /** The run's JUnit, `--json` and manifest files, served at
   *  `/api/ci/export/{export_id}/{results.xml|report.json|manifest.json}`. */
  export_id?: string
}

/** One term's explanation, from `hauksbee_ir::glossary`: the words every
 *  surface (CLI, JSON, export, this app) shows for it. */
export interface GlossaryRow {
  term: string
  label: string
  short: string
  what_to_do: string
}

/** Plan usage as percent of a subscription's limits, for one moment. */
export interface PlanWindows {
  week: number | null
  /** The 5-hour window. */
  session: number | null
}

/** How the server says a run is billed. Dollars are meaningful only for
 *  "metered" (per-token API billing); "plan" is a subscription login (Claude
 *  plan, ChatGPT plan); "unknown" means the server could not tell. The page
 *  works none of this out itself. */
export type ActivityBilling = 'metered' | 'plan' | 'unknown'


/** One live event from a running extraction, delivered on the SSE stream as an
 *  `activity: <json>` line. `t` is seconds since the run started. */
export type ActivityEvent =
  | {
      t: number; kind: 'tier'; model: string; tier: number; tiers: number; harness: string; budget: number
      /** Display name and caution for this tier, when the server sends them. */
      label?: string; badge?: string
    }
  | { t: number; kind: 'stage'; stage: string; attempt: number; attempts: number }
  | { t: number; kind: 'read'; path: string; page?: number }
  | { t: number; kind: 'tool'; name: string; detail?: string }
  | {
      t: number; kind: 'usage'; model: string; context: number; input: number; output: number
      cache_read: number; cache_write: number
      /** Null unless `billing` is "metered". */
      cost_usd: number | null
      total_tokens: number
      total_cost_usd: number | null
      billing?: ActivityBilling | string
    }
  /** Sent once at the end of a subscription run: plan usage before and after. */
  | {
      t: number; kind: 'plan'; tool?: string; available: boolean
      before?: PlanWindows; after?: PlanWindows
    }
  | { t: number; kind: 'validator'; ok: boolean; error?: string }
  | { t: number; kind: 'escalate'; from: string; to: string; reason: string }
  | { t: number; kind: 'final'; model: string; tier: number; tiers: number; escalations: string[]; confidence: string }
  | { t: number; kind: 'note'; text: string }
