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

import type {
  CheckResult, ErrorBudget, ErrorBudgetWindow, EvidenceAssumption, EvidenceMap, FixFirst, JsonNote, ModelCoverageComponent,
  ModelCoverageSnapshot, ModelOnPath, RefusalContract, RunResponse, WebCosimSection, WebFallbackWindow, WebFinding,
  WebGpioNet, WebHeadsUp, WebImportObject, WebReport, WebSection, WebTimingCoverage,
} from '../types/report'

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
}

const sectionView = (s: WebSection): SectionView =>
  ({ title: s.title, verdict: s.verdict, groups: groupFindings(s.findings), headsUp: s.heads_up ?? [] })

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
  if (methods.length > 0) rows.push(`Methods: ${methods.join(', ')}`)
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
  return `${ms(window.start_s)}-${ms(window.end_s)} ms: ${window.method}; ${estimate}${jump}; ${window.fidelity_note}.`
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
  /** Open parts, from the evidence list: the one count every surface quotes. */
  openParts: string[]
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

/** The designator an open-part assumption (`open-part:C2`) is about. */
export const openPartRef = (a: EvidenceAssumption): string | null =>
  a.kind === 'open_part' ? a.id.replace(/^open-part:/, '') : null

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
  const reason = notice && r.verdict_reason?.startsWith('the board must be routed') ? null : (r.verdict_reason ?? null)
  return {
    tone: reportVerdictTone(r),
    palette: reportVerdictPalette(r),
    headline: headlineWithoutConditions(reportVerdictHeadline(r), conditions),
    conditions,
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
    openParts: assumptions.flatMap(a => openPartRef(a) ?? []),
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
export const stageWords = (stage: string) => stage.replaceAll('_', ' ')

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
 *  own cause, a conditional one lists its conditions, and an unrouted one is
 *  explained by its headline and fix-first: none of those needs a panel. */
export const explainReport = (r: WebReport): VerdictExplanation | null =>
  r.verdict === 'invalid' && !r.fix_first?.notice ? explainVerdict('invalid', r.verdict_reason, r.refusal) : null

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

/** The run's one-word verdict, from the per-assertion results when there are
 *  any and from the runner's own exit otherwise. */
export function overallVerdict(result: RunResponse): Overall {
  const results = result.results ?? []
  if (results.length === 0) return result.passed ? 'passed' : 'failed'
  if (results.some(x => !x.invalid && !x.passed)) return 'failed'
  if (results.some(x => x.invalid)) return 'invalid'
  // Nothing judged is not a pass: the CLI headlines such a run MEASURED.
  return results.every(x => x.measured) ? 'measured' : 'passed'
}
