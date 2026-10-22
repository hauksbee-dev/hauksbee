// The report, derived ONCE from the engine's JSON into what the surfaces show.
//
// `types/report.ts` mirrors the Rust JsonReport; nothing here re-interprets
// it. What this module owns is every decision that used to be repeated per
// surface: which findings collapse into one card, which top-level note the
// bind table already says, which timing refusal the refusal contract already
// carries, what the verdict's colour is, and what "has evidence" means. The
// on-screen report (components/report/*), the standalone HTML export
// (lib/report-export.ts), the shell chips and the selection cards all read the
// same `ReportView`, so they cannot disagree.

import { labelFor } from './labels'
import { benchView } from './bench-view'
import type { BenchView } from './bench-view'
import type { BenchReport } from '../types/bench'
import type {
  CheckResult, ErrorBudget, ErrorBudgetWindow, EvidenceAssumption, EvidenceMap, FixFirst, JsonNote, ModelCoverageComponent,
  ModelCoverageSnapshot, ModelGroup, ModelOnPath, RefusalContract, RunResponse, WebCosimSection, WebFallbackWindow, WebFinding,
  WebAction, WebGpioNet, WebHeadsUp, WebImportObject, WebReport, WebSection, WebTimingCoverage,
} from '../types/report'
import {
  NOT_ROUTED_MESSAGE_PREFIX, STATIC_CHECK_FAILS_LEAD, STATIC_CHECK_KIND, STATIC_CHECK_NOT_JUDGED_LEAD,
} from './status-vocab'

// ── Findings ─────────────────────────────────────────────────────────────────

/** A run of findings that share level + why + fix: the DRC clearance case
 *  where 128 warnings differ only in which net-pair/location. Each item keeps
 *  its own `what` AND its own board location (if any). */
export interface FindingGroup {
  level: string
  why: string
  fix: string
  items: { what: string; x?: number; y?: number }[]
}

/** Collapse same-shaped findings so the shared explanation is shown ONCE.
 *  Order-independent; nothing is hidden, every `what` is still listed. */
export function groupFindings(findings: WebFinding[]): FindingGroup[] {
  const groups: FindingGroup[] = []
  for (const f of findings) {
    const item = { what: f.what, x: f.x, y: f.y }
    const g = groups.find(x => x.level === f.level && x.why === f.why && x.fix === f.fix)
    if (g) g.items.push(item)
    else groups.push({ level: f.level, why: f.why, fix: f.fix, items: [item] })
  }
  return groups
}

export interface SectionView {
  title: string
  verdict: string
  groups: FindingGroup[]
  headsUp: WebHeadsUp[]
  /** The engine's buttons for this section (see `lib/web-actions`). */
  actions: WebAction[]
}

const sectionView = (s: WebSection): SectionView =>
  ({ title: s.title, verdict: s.verdict, groups: groupFindings(s.findings), headsUp: s.heads_up ?? [], actions: s.actions ?? [] })

// ── Refusal contract (C5.3) ──────────────────────────────────────────────────

/** Lossless display rows; keeping this pure makes field loss testable. */
export function refusalLines(refusal: RefusalContract): [string, string][] {
  return [
    ['Refused claim', refusal.claim],
    ['Missing prerequisite', refusal.missing_prerequisite],
    ['Still valid', refusal.valid_partial_conclusions.join('; ')],
    ['Next action', refusal.next_action],
  ]
}

// ── Evidence ─────────────────────────────────────────────────────────────────

export interface EvidenceSummary {
  clean: number
  qualified: number
  undermined: number
  caveated: number
}

/** Count the engine's derived statuses without reinterpreting them. */
export function summarizeEvidence(maps: readonly EvidenceMap[] = []): EvidenceSummary {
  const summary: EvidenceSummary = { clean: 0, qualified: 0, undermined: 0, caveated: 0 }
  for (const map of maps) {
    summary[map.status] += 1
    if (map.status !== 'clean') summary.caveated += 1
  }
  return summary
}

/** Human projection of the canonical source record. Unknown remains the word
 *  unknown; the browser never substitutes a guessed percentage or range. */
export function describeModelSource(model: ModelOnPath): string {
  const accuracy = model.source.uncertainty.some(item => item.status === 'unknown')
    ? 'uncertainty unknown'
    : model.source.uncertainty.some(item =>
      item.status === 'interval' && (item.kind === 'typical-range' || item.kind === 'estimated-range'))
      ? 'non-guaranteed typical/estimated range'
      : 'validated two-sided bound'
  return `${model.reference} ${model.model_id}: ${model.source.tier} · ${model.source.validation} · ${accuracy}`
}

/** One line for a group of parts that share a model: the first few parts, then
 *  a count of the rest, so a rail with thousands of parts stays one line. */
export function describeModelGroup(group: ModelGroup): string {
  const shown = 6
  const head = group.references.slice(0, shown).join(', ')
  const rest = group.references.length - shown
  const parts = rest > 0 ? `${head} and ${rest} more` : head
  const row = { reference: parts, model_id: group.model_id, source: group.source, confidence: group.confidence }
  return `${describeModelSource(row)} (${plural(group.references.length, 'part')})`
}

/** The model lines of an evidence map, whichever shape it arrived in. */
export function modelProvenanceLines(map: EvidenceMap | undefined): string[] {
  if (!map) return []
  return [...(map.models ?? []).map(describeModelSource), ...(map.model_groups ?? []).map(describeModelGroup)]
}

/** Resolve a map's assumption ids through the run's registry. Missing ids are
 *  ignored rather than rephrased: the server owns the evidence vocabulary. */
export function assumptionsForEvidence(map: EvidenceMap, registry: readonly EvidenceAssumption[] = []): EvidenceAssumption[] {
  const byId = new Map(registry.map(assumption => [assumption.id, assumption]))
  return (map.assumptions ?? []).flatMap(id => {
    const assumption = byId.get(id)
    return assumption ? [assumption] : []
  })
}

export function repeatedAssumptions(results: readonly (CheckResult | null)[]): Set<string>[] {
  const seen = new Set<string>()
  return results.map(result => new Set((result?.evidence?.assumptions ?? []).filter(id => {
    if (seen.has(id)) return true
    seen.add(id)
    return false
  })))
}

const ms = (seconds: number): string => (seconds * 1e3).toFixed(3)
const span = (w: ErrorBudgetWindow): string => `${ms(w.start_s)}–${ms(w.end_s)} ms`

/** Human-readable qualification of machine-readable numerical evidence.
 *  Settings are labelled as settings; only a producer-measured residual is
 *  called measured. Missing fields remain visibly unmeasured. */
export function summarizeErrorBudget(budget: ErrorBudget): string[] {
  const { tolerance: t } = budget
  const rows = [
    `Tolerance: rel ${t.reltol} · V ${t.vntol} V · I ${t.abstol} A · Q ${t.chgtol} C`,
    budget.residual
      ? `Residual: ${budget.residual.max_abs} A at ${budget.residual.at}`
      : 'Residual: unmeasured by this solver path',
  ]
  const methods = [...new Set((budget.methods ?? []).map(entry => entry.method))]
  if (methods.length > 0) rows.push(`Methods: ${methods.map(labelFor).join(', ')}`)
  if (budget.failed_windows?.length) rows.push(`Invalid result spans: ${budget.failed_windows.map(span).join(', ')}`)
  if (budget.event_time_error_s !== undefined) {
    rows.push(`Event timing error: ≤${ms(budget.event_time_error_s)} ms (chunk quantization)`)
  }
  if (budget.model_uncertainty?.length) rows.push(`Model intervals: ${budget.model_uncertainty.length} attached`)
  return rows
}

// ── Co-sim ───────────────────────────────────────────────────────────────────

function duration(value: number): string {
  return value < 1e-3 ? `${(value * 1e6).toFixed(3)} us` : `${(value * 1e3).toFixed(3)} ms`
}

export function timingCoverageLine(row: WebTimingCoverage): string {
  const stamps = row.cycle_exact ? 'cycle-exact stamps' : 'poll-boundary stamps'
  return `${row.mcu_ref} (${row.backend}): ${stamps}; edge uncertainty <= ${duration(row.timestamp_precision_s)}; pulses >= ${duration(row.minimum_guaranteed_pulse_s)} guaranteed; ${duration(row.chunk_s)} solver chunk.`
}

export function fallbackWindowLine(window: WebFallbackWindow): string {
  const estimate = window.error_estimate_v == null
    ? 'no measured error estimate'
    : `${window.error_estimate_v.toFixed(3)} V measured chunk-end error estimate`
  const jump = window.state_jump_v == null
    ? ''
    : `; carried state discarded, ${window.state_jump_v.toFixed(3)} V jump at the window start`
  return `${ms(window.start_s)}-${ms(window.end_s)} ms: ${labelFor(window.method)}; ${estimate}${jump}; ${window.fidelity_note}.`
}

/** The timing refusals the refusal contract does not already carry: the
 *  diagnosis it names is rendered once, by the contract. */
export function uncoveredTimingRefusals(refusals: string[] | undefined, refusal: RefusalContract | null | undefined): string[] {
  return (refusals ?? []).filter(line => line !== refusal?.missing_prerequisite)
}

export interface CosimView {
  ran: boolean
  /** "Ran the firmware for 0.100s on the board's microcontroller." */
  ranLine: string
  analogValid: boolean
  /** Why it did not run (only when `ran` is false). */
  notRanLines: string[]
  groups: FindingGroup[]
  timingLines: string[]
  timingRefusals: string[]
  fallbackLines: string[]
  budgetLines: string[]
  uart: string
  gpio: WebGpioNet[]
  /** The bench strip's chips and headlines; null when the run carries none. */
  bench: BenchView | null
  /** The raw observations behind `bench`, for the run view's lanes. */
  benchReport: BenchReport | null
  /** The MCU needs an emulator the synchronous analysis never runs. */
  externalBackend: boolean
}

function cosimView(c: WebCosimSection, refusal: RefusalContract | null | undefined): CosimView {
  const findings = c.findings ?? []
  return {
    ran: c.ran,
    ranLine: `Ran the firmware for ${(c.seconds_simulated || 0).toFixed(3)}s on the board's microcontroller.`,
    analogValid: c.analog_valid,
    notRanLines: (findings.length > 0 ? findings : [{ what: 'Co-sim not available for this board.', why: '' }])
      .map(f => `${f.what} ${f.why}`.trim()),
    groups: groupFindings(findings),
    timingLines: (c.timing_coverage ?? []).map(timingCoverageLine),
    timingRefusals: uncoveredTimingRefusals(c.timing_refusals, refusal),
    fallbackLines: (c.fallback_windows ?? []).map(fallbackWindowLine),
    budgetLines: c.error_budget ? summarizeErrorBudget(c.error_budget) : [],
    uart: c.uart_output,
    gpio: c.gpio_nets ?? [],
    bench: c.bench ? benchView(c.bench) : null,
    benchReport: c.bench ?? null,
    externalBackend: !!c.external_backend,
  }
}

// ── Verdict ──────────────────────────────────────────────────────────────────

export type ReportVerdictTone = 'ok' | 'warning' | 'error'

/** One verdict contract for the browser card, the shell chip and the export. */
export function reportVerdictTone(report: WebReport): ReportVerdictTone {
  // The engine's verdict leads: a gating finding fails the run even when none
  // of its findings is graded serious, and an inconclusive run is never green.
  if (report.verdict === 'fail') return 'error'
  if (report.serious > 0 || (report.cosim?.findings ?? []).some(f => f.level === 'serious')) return 'error'
  if (report.verdict === 'invalid') return 'warning'
  const cosimQualified = !!(
    report.refusal
    || report.cosim?.findings?.length
    || report.cosim?.timing_refusals?.length
    || report.cosim?.fallback_windows?.length
    || report.cosim?.analog_valid === false
  )
  if (
    report.total > 0
    || bindOpen(report)
    || (report.sections ?? []).some(section => section.heads_up?.length)
    || summarizeEvidence(report.evidence).caveated > 0
    || cosimQualified
  ) return 'warning'
  return 'ok'
}

const PALETTE: Record<ReportVerdictTone, { border: string; background: string }> = {
  error: { border: 'var(--err-border)', background: 'var(--err-bg)' },
  warning: { border: 'var(--warn-border)', background: 'var(--warn-bg)' },
  ok: { border: 'var(--ok-border)', background: 'var(--ok-bg)' },
}

export const reportVerdictPalette = (report: WebReport) => PALETTE[reportVerdictTone(report)]

export function reportVerdictHeadline(report: WebReport): string {
  if (report.refusal && report.serious === 0 && report.headline.includes('Looks healthy')) {
    return 'Analysis invalid for the requested firmware co-simulation. Static board findings remain valid.'
  }
  return report.headline
}

// ── The whole view ───────────────────────────────────────────────────────────

const bindOpen = (r: WebReport) => !!r.bind?.active_path_unresolved?.length

const BRIEF_MAX = 140

/** The first sentence of `text`, capped at a word boundary. A sentence ends at
 *  `. ` / `! ` / `? ` before a capital or bracket, so "4.5.2" and "e.g. x"
 *  stay whole. */
function firstSentence(text: string): string {
  const end = text.search(/[.!?](?=\s+[A-Z(])/)
  const sentence = (end < 0 ? text : text.slice(0, end)).trim().replace(/\.$/, '')
  if (sentence.length <= BRIEF_MAX) return sentence
  return `${sentence.slice(0, BRIEF_MAX).replace(/\s+\S*$/, '')}…`
}

/** The engine's reason sentence ("<what>; next: <fix>.") cut to a headline for
 *  the verdict card: the first sentence of each half. The first finding's
 *  full text (pad strings, net chains) stays in the findings list and in the
 *  full reason, which the card keeps as its tooltip. */
export function briefReason(reason: string): string {
  const at = reason.lastIndexOf('; next: ')
  if (at < 0) return firstSentence(reason)
  const what = firstSentence(reason.slice(0, at))
  const next = firstSentence(reason.slice(at + '; next: '.length))
  const closed = (line: string) => (line.endsWith('…') ? line : `${line}.`)
  return closed(next ? `${what}; next: ${next}` : what)
}

export const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

export interface ImportMarker {
  x: number
  y: number
  status: WebImportObject['status']
  nets: string[]
}

/** A partly judged run's scope, for the verdict card: which parts have no
 *  usable model and what that left unjudged. */
export interface PartlyJudgedView {
  /** The key parts with no usable model. */
  openParts: string[]
  /** Nets whose wiring/value checks were not judged. */
  wiring: string[]
  /** Nets whose electrical checks (levels, ratings, rails) were not judged. */
  electrical: string[]
  /** Net checks judged, of `total` (each net has a wiring and an electrical claim). */
  judged: number
  total: number
}

/** The parts with no usable model: the one measure of the modelling gap. Every
 *  surface that counts or names them (the verdict card, the open-parts finding,
 *  the board overlay's "no model" tags, the export) reads this, so the numbers
 *  never disagree. Key parts are the ones the verdict depends on (the judged
 *  scope's open parts and the active ICs left open on the live circuit); the
 *  others are every further part the evidence lists as open. */
export interface UnmodelledParts {
  key: string[]
  other: string[]
  /** `key` then `other`. */
  all: string[]
}

export function unmodelledParts(r: WebReport): UnmodelledParts {
  const key = [...new Set([...(r.judgement_scope?.open_parts ?? []), ...(r.bind?.active_path_unresolved ?? [])])]
  const keys = new Set(key)
  const listed = (r.assumptions ?? []).flatMap(a => openPartRef(a) ?? [])
  const other = [...new Set(listed)].filter(ref => !keys.has(ref))
  return { key, other, all: [...key, ...other] }
}

/** Names in a sentence: "U1", "Q1 and U4", "Q1, U4 and U5", the first few then a count. */
function namesInSentence(names: string[]): string {
  const SHOWN = 6
  if (names.length > SHOWN) return `${names.slice(0, SHOWN).join(', ')} and ${names.length - SHOWN} more`
  return names.length <= 1 ? names.join('') : `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`
}

/** The words after the key parts' names in [`unmodelledSentence`]: what a
 *  surface that shows the names as chips prints after them. */
export function unmodelledTail(u: UnmodelledParts): string {
  const { key, other } = u
  if (key.length === 0) {
    return other.length === 0 ? '' : `${plural(other.length, 'part')} ${other.length === 1 ? 'has' : 'have'} no usable model`
  }
  const keyed = `(${key.length === 1 ? 'key part' : 'key parts'})`
  if (other.length === 0) return `${keyed} ${key.length === 1 ? 'has' : 'have'} no usable model`
  return `${keyed} and ${plural(other.length, 'other part')} have no usable model`
}

/** The measure in one sentence, the one wording every surface uses:
 *  "U1 (key part) and 6 other parts have no usable model". Empty when none. */
export function unmodelledSentence(u: UnmodelledParts): string {
  return [namesInSentence(u.key), unmodelledTail(u)].filter(Boolean).join(' ')
}

/** Where the board report states the measure, once: in the verdict card when
 *  the verdict rests on it (a partly judged run, or a pass whose engine
 *  condition names the parts), else as a finding beside the board. */
export function unmodelledHome(r: WebReport, u: UnmodelledParts): 'verdict' | 'findings' | null {
  if (u.all.length === 0) return null
  if (r.partly_judged) return 'verdict'
  if (u.key.length === 0 && r.verdict === 'pass') return 'verdict'
  return 'findings'
}

/** What a partly judged run left unjudged, each net named once under the kind
 *  of check that was not judged on it. The electrical set holds the wiring set
 *  (a part that cannot be modelled changes the electrical claim on every net it
 *  can reach, and the wiring claim on the nets it touches), so a flat pair of
 *  lists names the touched nets twice. */
export type UnjudgedKind = 'both' | 'electrical' | 'wiring'
export interface UnjudgedGroup {
  kind: UnjudgedKind
  /** What was not judged on these nets, in words. */
  label: string
  nets: string[]
}

const UNJUDGED_LABELS: Record<UnjudgedKind, string> = {
  both: 'wiring, value and electrical checks',
  electrical: 'electrical checks (levels, ratings, rails)',
  wiring: 'wiring and value checks',
}

export function unjudgedGroups(scope: Pick<PartlyJudgedView, 'wiring' | 'electrical'>): UnjudgedGroup[] {
  const wiring = new Set(scope.wiring)
  const electrical = new Set(scope.electrical)
  const pick = (names: string[], keep: (n: string) => boolean) => [...new Set(names)].filter(keep)
  const groups: UnjudgedGroup[] = [
    { kind: 'both', label: UNJUDGED_LABELS.both, nets: pick(scope.wiring, n => electrical.has(n)) },
    { kind: 'electrical', label: UNJUDGED_LABELS.electrical, nets: pick(scope.electrical, n => !wiring.has(n)) },
    { kind: 'wiring', label: UNJUDGED_LABELS.wiring, nets: pick(scope.wiring, n => !electrical.has(n)) },
  ]
  return groups.filter(g => g.nets.length > 0)
}

export interface ReportView {
  tone: ReportVerdictTone
  palette: { border: string; background: string }
  headline: string
  /** The engine's one sentence: why this is not a pass, and the next action. */
  reason: string | null
  /** [`briefReason`] of `reason`, for the verdict card. */
  reasonBrief: string | null
  /** The assumptions a pass rests on, one sentence each; empty for an
   *  unconditional report and for a verdict that is not a pass. */
  conditions: string[]
  /** Set when the run is PARTLY JUDGED (never a pass): what was and was not
   *  judged. Null for every other verdict. */
  partlyJudged: PartlyJudgedView | null
  /** `board_name`, else the file name. */
  title: string
  /** "3 parts, 4 nets" */
  sizeLine: string
  /** Active ICs that could not be bound or are left open on the live circuit. */
  unresolved: string[]
  bindOpen: boolean
  /** Top-level notes, minus the bind-role note when the bind table already
   *  says it in full (the JSON carries both for CLI parity). */
  notes: JsonNote[]
  /** The causes to fix first; null when there are none. */
  fixFirst: FixFirst | null
  /** A board with no copper yet: the engine's notice, and the copper section
   *  the report folds under it (it stays in `sections` for the export). */
  preLayout: { notice: string; copper: SectionView | null } | null
  /** The parts with no usable model ([`unmodelledParts`]): the one measure
   *  every surface counts and names. */
  unmodelled: UnmodelledParts
  refusalRows: [string, string][] | null
  sections: SectionView[]
  cosim: CosimView | null
  evidence: {
    has: boolean
    summary: EvidenceSummary
    caveated: EvidenceMap[]
    maps: EvidenceMap[]
    assumptions: EvidenceAssumption[]
    inventory: NonNullable<WebReport['inventory']>
  }
  /** Imported objects with a real source coordinate; unplaced ones stay in
   *  the list rather than being plotted at a guess. */
  importMarkers: ImportMarker[]
}

/** The engine's conditional headline carries the same sentences as
 *  `verdict_conditions` ("OK to order, conditionally: ...; this rests on:
 *  <joined>"). The
 *  card lists them as items, so the headline keeps only its lead-in rather than
 *  saying everything twice. Any other headline is left alone. */
function headlineWithoutConditions(headline: string, conditions: string[]): string {
  const joined = conditions.join(' ')
  if (!joined || !headline.endsWith(joined)) return headline
  return headline.slice(0, headline.length - joined.length).trimEnd()
}

/** The part an assumption is about, from its typed remedy's `reference`, else
 *  its typed scope. The id's namespace (`open-part:C2`) is a key, never parsed. */
export function assumptionPart(a: EvidenceAssumption): string | undefined {
  const r = a.remedy
  let raw: string | undefined
  if (r && 'reference' in r) raw = r.reference
  else if (a.scope.type === 'subjects' && Array.isArray(a.scope.value)) {
    raw = (a.scope.value as { kind?: string; id?: string }[]).find(s => s.kind === 'part' || s.kind === 'pin')?.id
  }
  return raw?.split('@')[0].split('/')[0] || undefined
}

/** The designator an open-part assumption is about. */
export const openPartRef = (a: EvidenceAssumption): string | null =>
  a.kind === 'open_part' ? assumptionPart(a) ?? null : null

/** The judged scope of a PARTLY JUDGED run: the same numbers the engine's
 *  "... has no usable model, so the checks it could change are not judged" sentence states (`2 * nets` claims, less the dependent ones). */
function partlyJudgedView(r: WebReport): PartlyJudgedView | null {
  if (!r.partly_judged) return null
  const scope = r.judgement_scope
  const total = 2 * (scope?.nets ?? 0)
  const wiring = scope?.wiring_dependent ?? []
  const electrical = scope?.electrical_dependent ?? []
  return {
    openParts: scope?.open_parts ?? [],
    wiring,
    electrical,
    judged: Math.max(0, total - wiring.length - electrical.length),
    total,
  }
}

export function reportView(r: WebReport): ReportView {
  const unresolved = r.bind?.active_path_unresolved ?? []
  const open = unresolved.length > 0
  const maps = r.evidence ?? []
  const assumptions = r.assumptions ?? []
  const inventory = r.inventory ?? []
  const conditions = (r.verdict_conditions ?? []).filter(c => c.trim() !== '')
  const sections = (r.sections ?? []).map(sectionView)
  const notice = r.fix_first?.notice
  const copper = (notice && sections.find(s => s.title.startsWith('Copper spacing'))) || null
  // An unrouted board's headline already says so and its fix-first item says
  // what to do, so the routing-only reason would be a third telling.
  const why = r.verdict_why ?? r.verdict_reason
  const reason = notice && why?.startsWith('the board must be routed') ? null : (why ?? null)
  return {
    tone: reportVerdictTone(r),
    palette: reportVerdictPalette(r),
    headline: headlineWithoutConditions(reportVerdictHeadline(r), conditions),
    conditions,
    partlyJudged: partlyJudgedView(r),
    reason,
    reasonBrief: reason ? briefReason(reason) : null,
    title: r.board_name || r.file_name,
    sizeLine: `${plural(r.num_components, 'part')}, ${plural(r.num_nets, 'net')}`,
    unresolved,
    bindOpen: open,
    notes: (r.notes ?? []).filter(n => !(open && n.kind === 'bind_role')),
    refusalRows: r.refusal ? refusalLines(r.refusal) : null,
    sections,
    fixFirst: r.fix_first?.items.length ? r.fix_first : null,
    preLayout: notice ? { notice, copper } : null,
    unmodelled: unmodelledParts(r),
    cosim: r.cosim ? cosimView(r.cosim, r.refusal) : null,
    evidence: {
      has: maps.length > 0 || assumptions.length > 0 || inventory.length > 0,
      summary: summarizeEvidence(maps),
      caveated: maps.filter(map => map.status !== 'clean'),
      maps,
      assumptions,
      inventory,
    },
    importMarkers: (r.import_diagnostics?.objects ?? []).flatMap(o =>
      o.x !== undefined && o.y !== undefined ? [{ x: o.x, y: o.y, status: o.status, nets: o.nets ?? [] }] : []),
  }
}

// ── The verdict card's one sentence ─────────────────────────────────────────

/** The engine headline's own verdict words, each said once already by the
 *  card's verdict word: a lead-in ("OK to order, conditionally: ..."), a
 *  trailing restatement ("... OK to order, conditionally") and pointers at
 *  rows the card shows anyway ("see the reason below", "this rests on:"). */
const VERDICT_LEADS = /^(ok to order(,\s*conditionally)?|partly judged,\s*not a pass|inconclusive,\s*not a pass|not routed yet|not conclusive|clean(,\s*conditionally)?)\s*[:.]\s*/i
const VERDICT_TAILS = [
  /\s*ok to order(,\s*conditionally)?\.?$/i,
  /[;,]?\s*see the reason below\.?$/i,
  /[;,]?\s*this rests on:?$/i,
]

/** The headline as the one sentence under the verdict word: what the run
 *  found, without the verdict said a second time in other words. Empty when
 *  the headline is nothing but the verdict. */
export function verdictSentence(headline: string): string {
  let text = headline.trim().replace(VERDICT_LEADS, '')
  for (let changed = true; changed;) {
    changed = false
    for (const tail of VERDICT_TAILS) {
      const next = text.replace(tail, '').trim()
      if (next !== text) { text = next; changed = true }
    }
  }
  if (!text) return ''
  text = text[0].toUpperCase() + text.slice(1)
  return /[.!?…]$/.test(text) ? text : `${text}.`
}

// ── The findings list beside the board ──────────────────────────────────────

/** One severity scale on the page: the engine's serious/warning/note, with a
 *  section's heads-up and a top-level note both a Note. */
export type FindingLevel = 'serious' | 'warning' | 'note'
export const LEVEL_WORD: Record<FindingLevel, string> = { serious: 'Serious', warning: 'Warning', note: 'Note' }
const LEVEL_ORDER: Record<FindingLevel, number> = { serious: 0, warning: 1, note: 2 }

export interface FindingEntry {
  key: string
  level: FindingLevel
  /** The section it came from, without its parenthetical ("Connectivity & wiring"). */
  section: string
  /** One line: the finding's first sentence. */
  title: string
  /** The rest of the finding's own text, when it has more than one sentence. */
  detail: string | null
  /** Why it matters, when the engine gave a reason. */
  why: string | null
  /** A "why" that is really measured data ("J2: CC1 Rd=none"), shown as data. */
  data: string | null
  /** What to do, once. */
  fix: string | null
  /** Every place it was found (more than one for a group of similar findings). */
  items: { what: string; x?: number; y?: number }[]
  /** The text the parts and nets it names are read from. */
  text: string
  /** The finding asks for a stated intent the Checks view can record. */
  wantsCheck: boolean
}

/** A section's name without its parenthetical qualifier. */
export const sectionName = (title: string) => title.replace(/\s*\(.*\)$/, '')

/** "Remedy: ..." inside the finding's own text, when the fix says it again. */
const withoutRemedy = (what: string, fix: string | null) => {
  const at = what.search(/\s*Remedy:\s/)
  return fix && at > 0 ? what.slice(0, at).trim() : what
}

/** A terminal pointer inside a note, for a step the app does itself. */
const CLI_SCAFFOLD = /[;,]?\s*`hauksbee-ci init` scaffolds one to confirm\.?/

/** Split a finding's text into a one-line title and the rest. */
function titleAndDetail(what: string): { title: string; detail: string | null } {
  const end = what.search(/[.!?](?=\s+[A-Z(])/)
  if (end < 0) return { title: what.trim(), detail: null }
  return { title: what.slice(0, end + 1).trim(), detail: what.slice(end + 1).trim() || null }
}

/** A "why" with no sentence in it, only fields and values. */
const isData = (why: string) => !/[.!?]$/.test(why.trim()) && /[=:]/.test(why) && why.length < 120

function entry(key: string, level: FindingLevel, section: string, what: string, why: string, fix: string, items: FindingEntry['items']): FindingEntry {
  const cleanFix = fix.trim() || null
  const wantsCheck = CLI_SCAFFOLD.test(what)
  const own = withoutRemedy(what.replace(CLI_SCAFFOLD, '.').replace(/\.\.$/, '.'), cleanFix)
  const { title, detail } = titleAndDetail(own)
  const reason = why.trim()
  const data = reason && isData(reason) ? reason : null
  return {
    key, level, section, title, detail,
    why: reason && !data ? reason : null,
    data,
    // A fix the title already says verbatim is not said twice.
    fix: cleanFix && !own.includes(cleanFix.replace(/\.$/, '')) ? cleanFix : null,
    items,
    text: [what, ...items.map(i => i.what)].join(' '),
    wantsCheck,
  }
}

/** Every finding, heads-up and top-level note on the page, worst first, as
 *  the cards beside the board. The copper findings an unrouted board holds
 *  back are not here (the pre-layout line counts them). */
export function findingEntries(v: ReportView): FindingEntry[] {
  const out: FindingEntry[] = []
  v.sections.filter(s => s !== v.preLayout?.copper).forEach((s, si) => {
    const section = sectionName(s.title)
    s.groups.forEach((g, gi) => {
      const level: FindingLevel = g.level === 'serious' || g.level === 'warning' ? g.level : 'note'
      out.push(entry(`s${si}g${gi}`, level, section, g.items[0].what, g.why, g.fix, g.items))
    })
    s.headsUp.forEach((h, hi) => {
      out.push(entry(`s${si}h${hi}`, 'note', section, h.what, h.why ?? '', h.fix ?? '', [{ what: h.what }]))
    })
  })
  v.notes.forEach((n, ni) => {
    out.push(entry(`n${ni}`, 'note', 'Note', n.message, '', '', [{ what: n.message }]))
  })
  return out
    .map((e, i) => ({ e, i }))
    .sort((a, b) => LEVEL_ORDER[a.e.level] - LEVEL_ORDER[b.e.level] || a.i - b.i)
    .map(({ e }) => e)
}

/** The sections with nothing to show as a card, by what they said: checked
 *  and clear, or not able to judge (an INCONCLUSIVE section, whose parts with
 *  no model the page offers to model). A section that both found things and
 *  could not judge everything is in `inconclusive` too. */
export interface SectionStatus {
  clear: { title: string; verdict: string }[]
  inconclusive: { title: string; verdict: string }[]
}

export function sectionStatus(v: ReportView): SectionStatus {
  const clear: SectionStatus['clear'] = []
  const inconclusive: SectionStatus['inconclusive'] = []
  for (const s of v.sections) {
    if (s === v.preLayout?.copper) continue
    if (/^INCONCLUSIVE\b/i.test(s.verdict)) inconclusive.push({ title: sectionName(s.title), verdict: s.verdict })
    else if (s.groups.length === 0 && s.headsUp.length === 0) clear.push({ title: s.title, verdict: s.verdict })
  }
  return { clear, inconclusive }
}

/** The engine reason's first half, without a terminal command the page offers
 *  as a button ("...; next: draft their models with `hauksbee models ...`"). */
export function reasonForCard(reason: string): string {
  const { what, next } = splitReason(reason)
  if (next && /`/.test(next)) return briefReason(what)
  return briefReason(reason)
}

// ── Model coverage lookups (the selection cards on both boards) ─────────────

/** The coverage row for one part, or null. */
export function coverageFor(snapshot: ModelCoverageSnapshot | null | undefined, ref: string | null | undefined): ModelCoverageComponent | null {
  return ref ? snapshot?.components.find(c => c.reference === ref) ?? null : null
}

/** Every part with a pin on `net`. */
export function modelsOnNet(snapshot: ModelCoverageSnapshot | null | undefined, net: string | null | undefined): ModelCoverageComponent[] {
  return net ? (snapshot?.components ?? []).filter(c => c.pins.some(pin => pin.net === net)) : []
}

/** The distinct nets on a part's pins, in pin order. */
export const padNetsOf = (component: ModelCoverageComponent): string[] =>
  [...new Set(component.pins.flatMap(pin => pin.net ? [pin.net] : []))]

/** A coverage stage as words. */
/** A coverage stage in the shared label table's words (labels.ts). */
export const stageWords = (stage: string) => labelFor(stage)

const STAGE_TERMS: Record<string, string> = {
  unresolved: 'open_part', identity_only: 'identity_only', executable_partial: 'executable_partial',
  executable_scope_unspecified: 'scope_unspecified',
}
/** The glossary term a coverage stage is an instance of, if it has one. */
export const stageTerm = (stage: string): string | undefined => STAGE_TERMS[stage]

/** The glossary term an evidence status is an instance of; a clean one needs no explaining. */
export const evidenceStatusTerm = (status: string): string | undefined =>
  status === 'qualified' ? 'conditional_pass' : status === 'undermined' ? 'invalid' : undefined

/** The engine's reason ("<what>; next: <fix>.") as its two halves. */
export function splitReason(reason: string | null | undefined): { what: string; next: string | null } {
  const text = (reason ?? '').trim().replace(/\.$/, '')
  const at = text.lastIndexOf('; next: ')
  return at < 0 ? { what: text, next: null } : { what: text.slice(0, at), next: text.slice(at + 8) || null }
}

/** Under "Explain this verdict": a glossary term for what it means, why this
 *  verdict, what to do next, and the refusal contract as evidence. */
export interface VerdictExplanation {
  term: string
  why: string | null
  doThis: string | null
  evidence: [string, string][]
}

const sentence = (t: string) => (/[.!?…]$/.test(t) ? t : `${t}.`)

/** Explain a refused ('invalid') or failed ('fail') verdict from the engine's
 *  own sentences. */
function explainVerdict(term: 'invalid' | 'fail', reason: string | undefined, refusal: RefusalContract | null | undefined): VerdictExplanation {
  const { what, next } = splitReason(reason)
  const cause = term === 'invalid' ? (refusal?.missing_prerequisite ?? what).replace(/\.$/, '') : what
  // After "because", a sentence's first word is lower case unless it is a designator ("Q1").
  const because = cause.replace(/^([A-Z])(?=[a-z]|\s)/, c => c.toLowerCase())
  return {
    term,
    why: !cause ? null : term === 'invalid' ? `Not judged because ${because}.` : sentence(cause),
    doThis: refusal?.next_action ?? (next ? sentence(next) : null),
    evidence: refusal ? refusalLines(refusal) : [],
  }
}

/** The board's verdict, explained when it is refused. A failing board states its
 *  own cause, a conditional one lists its conditions, a partly judged one names
 *  its judged scope, and an unrouted one is explained by its headline and
 *  fix-first: none of those needs a panel. */
export const explainReport = (r: WebReport): VerdictExplanation | null =>
  r.verdict === 'invalid' && !r.partly_judged && !r.fix_first?.notice ? explainVerdict('invalid', r.verdict_why ?? r.verdict_reason, r.refusal) : null

/** The same for a checks run (a clean or measured-only run has nothing to explain). */
export function explainRun(result: RunResponse, overall: Overall, conditional: boolean): VerdictExplanation | null {
  if (overall === 'invalid' || overall === 'failed') {
    return explainVerdict(overall === 'invalid' ? 'invalid' : 'fail', result.verdict_reason, result.refusal)
  }
  return overall === 'passed' && conditional
    ? { term: 'conditional_pass', why: 'It rests on the condition listed below.', doThis: null, evidence: [] }
    : null
}

/** Every glossary term a report shows (the export's appendix). */
export function reportTerms(r: WebReport, checks?: { failed: number; invalid: number } | null): string[] {
  const terms = new Set<string>()
  for (const a of r.assumptions ?? []) if (a.term) terms.add(a.term)
  for (const c of r.model_coverage?.components ?? []) if (stageTerm(c.stage)) terms.add(stageTerm(c.stage)!)
  if (r.verdict === 'invalid' || summarizeEvidence(r.evidence).undermined > 0 || (checks?.invalid ?? 0) > 0) terms.add('invalid')
  if ((checks?.failed ?? 0) > 0) terms.add('fail')
  return [...terms]
}

export type Overall = 'passed' | 'measured' | 'invalid' | 'failed'

/** How many `[[assert]]` rows a spec text holds: the results the run returns
 *  come back in that order, one per row, and anything after them was added by
 *  the engine (the board's own static check). */
export const assertRowCount = (specToml: string): number => (specToml.match(/^\[\[assert\]\]/gm) ?? []).length

/** The results no spec row owns. The strip and the top bar count every result,
 *  so each of these needs a place of its own on the Checks page. */
export const resultsWithoutRows = <T,>(results: readonly T[], specToml: string): T[] =>
  results.slice(assertRowCount(specToml))

/** The run's one-word verdict, from the per-assertion results when there are
 *  any and from the runner's own exit otherwise. */
export function overallVerdict(result: RunResponse): Overall {
  return verdictOf(result.results ?? [], !!result.passed)
}

/** The one-word verdict of a set of results (`passed` when there are none and
 *  the runner itself passed). */
export function verdictOf(results: readonly CheckResult[], runnerPassed: boolean): Overall {
  if (results.length === 0) return runnerPassed ? 'passed' : 'failed'
  if (results.some(x => !x.invalid && !x.passed)) return 'failed'
  if (results.some(x => x.invalid)) return 'invalid'
  // Nothing judged is not a pass: the CLI headlines such a run MEASURED.
  return results.every(x => x.measured) ? 'measured' : 'passed'
}

// ── A checks run, read for the Checks page ──────────────────────────────────

/** A row the board's own static check added to a run, read back into its parts:
 *  the engine's plain sentence without the lead every such row repeats. */
export interface StaticFinding {
  /** The finding in plain words ("the board is not routed yet, so ..."). */
  plain: string
  severity: string
  /** Every part and net the finding names. */
  subjects: string[]
  /** What to do, from the finding's own fix. */
  fix: string | null
  /** The as-built variant: the rework could not be applied, so not judged. */
  notJudged: boolean
}

/** Read a static-check row; null for any other result. */
export function readStaticFinding(r: CheckResult): StaticFinding | null {
  if (r.kind !== STATIC_CHECK_KIND) return null
  let rest = r.detail
  const notJudged = rest.startsWith(STATIC_CHECK_NOT_JUDGED_LEAD)
  if (rest.startsWith(STATIC_CHECK_FAILS_LEAD)) rest = rest.slice(STATIC_CHECK_FAILS_LEAD.length)
  else if (notJudged) rest = rest.slice(STATIC_CHECK_NOT_JUDGED_LEAD.length).replace(/^\s*\([^)]*\)/, '')
  rest = rest.replace(/^:\s*/, '')
  const sev = /^\[([a-z]+)\]\s*/.exec(rest)
  const plain = sev ? rest.slice(sev[0].length) : rest
  // The label names the first few subjects ("on A, B and 43 more"); the full
  // list rides in `all_subjects` whenever the label shortened it.
  const on = / on (.+?)(?: #\d+)?$/.exec(r.label)
  const subjects = r.all_subjects?.length
    ? r.all_subjects
    : on ? on[1].split(', ').filter(s => !/^\S+ and \d+ more$/.test(s)) : []
  return { plain, severity: sev?.[1] ?? '', subjects, fix: r.why ?? null, notJudged }
}

/** The engine's own pre-layout state: no copper on a board with placed pads.
 *  Layout readiness, not a defect in the circuit the checks judge. */
export const isLayoutReadiness = (r: CheckResult): boolean =>
  readStaticFinding(r)?.plain.startsWith(NOT_ROUTED_MESSAGE_PREFIX) ?? false

/** A run split the way the Checks page shows it. */
export interface RunView {
  /** The results of the spec's own `[[assert]]` rows, in row order. */
  rows: CheckResult[]
  /** Findings of the board's own check that fail the run (a short, a pin
   *  conflict): judged, failing, shown once beside the verdict. */
  boardFindings: CheckResult[]
  /** The not-routed state, when the run carried it: layout readiness, shown as
   *  a note. It still fails a CI run of the same spec, and the note says so. */
  layout: CheckResult | null
  /** Everything the page judges: the rows plus the board findings. */
  judged: CheckResult[]
  verdict: Overall
}

export function runView(result: RunResponse, specToml: string): RunView {
  const results = result.ok ? result.results ?? [] : []
  const count = assertRowCount(specToml)
  const rows = results.slice(0, count)
  const added = results.slice(count)
  const layout = added.find(isLayoutReadiness) ?? null
  const boardFindings = added.filter(r => r !== layout)
  const judged = [...rows, ...boardFindings]
  return { rows, boardFindings, layout, judged, verdict: verdictOf(judged, !!result.passed) }
}

/** The run as Markdown: the verdict, every check with its status, and the
 *  command that reproduces it. For a ticket, a review or a lab notebook. */
export function runMarkdown({ result, specToml, specFileName, boardLabel, title }: {
  result: RunResponse
  specToml: string
  specFileName: string
  boardLabel: string
  /** A check's display title, from the catalogue. */
  title: (r: CheckResult) => string
}): string {
  const v = runView(result, specToml)
  const word = { passed: 'PASS', measured: 'MEASURED', invalid: 'INVALID', failed: 'FAIL' }[v.verdict]
  const lines = [
    `# Checks: ${boardLabel}`,
    '',
    `**Verdict: ${word}** (${v.judged.length} ${v.judged.length === 1 ? 'check' : 'checks'})`,
    '',
    '| Status | Check | Detail |',
    '| --- | --- | --- |',
    ...v.judged.map(r => {
      const status = r.invalid ? 'INVALID' : r.measured ? 'MEASURED' : r.passed ? 'PASS' : 'FAIL'
      const detail = (readStaticFinding(r)?.plain ?? r.detail).replace(/\|/g, '\\|').replace(/\s+/g, ' ')
      return `| ${status} | ${title(r).replace(/\|/g, '\\|')} | ${detail} |`
    }),
  ]
  if (v.layout) {
    const f = readStaticFinding(v.layout)
    lines.push('', `**Layout:** ${f?.plain ?? v.layout.detail}. A CI run of this spec fails until the board is routed.`)
  }
  const conditions = (result.verdict_conditions ?? []).filter(c => c.trim())
  if (conditions.length) lines.push('', '**Rests on:**', ...conditions.map(c => `- ${c}`))
  lines.push('', `Reproduce: \`hauksbee-ci run ci/${specFileName}\``, '')
  return lines.join('\n')
}
