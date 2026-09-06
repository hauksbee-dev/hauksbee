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
  ErrorBudget, ErrorBudgetWindow, EvidenceAssumption, EvidenceMap, JsonNote, ModelCoverageComponent,
  ModelCoverageSnapshot, ModelOnPath, RefusalContract, WebCosimSection, WebFallbackWindow, WebFinding,
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
  return `${ms(window.start_s)}-${ms(window.end_s)} ms: ${window.method}; ${estimate}; ${window.fidelity_note}.`
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
  if (report.serious > 0 || (report.cosim?.findings ?? []).some(f => f.level === 'serious')) return 'error'
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

export function reportView(r: WebReport): ReportView {
  const unresolved = r.bind?.active_path_unresolved ?? []
  const open = unresolved.length > 0
  const maps = r.evidence ?? []
  const assumptions = r.assumptions ?? []
  const inventory = r.inventory ?? []
  return {
    tone: reportVerdictTone(r),
    palette: reportVerdictPalette(r),
    headline: reportVerdictHeadline(r),
    title: r.board_name || r.file_name,
    sizeLine: `${plural(r.num_components, 'part')}, ${plural(r.num_nets, 'net')}`,
    unresolved,
    bindOpen: open,
    notes: (r.notes ?? []).filter(n => !(open && n.kind === 'bind_role')),
    refusalRows: r.refusal ? refusalLines(r.refusal) : null,
    sections: (r.sections ?? []).map(sectionView),
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
