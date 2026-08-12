import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { parse as parseToml } from 'smol-toml'
import type { ArtifactProvenance, EvidenceAssumption, EvidenceMap, QueuedCheck, WebReport } from '../types/report'
import type { SelectedComponent } from './SelectionCard'
import { PlusIcon } from './Icons'
import { specStemFor, workflowYaml } from '../lib/ci-workflow'
import { downloadText } from '../lib/report-export'
import { ArriveOnce, EmptyState, StaggerItem, ValueSettle, VerdictBadge, ARRIVE, LEAVE } from '../motion'
import { refusalLines, type RefusalContract } from '../lib/refusal-contract'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { assumptionsForEvidence, describeModelSource } from '../lib/evidence'
import { summarizeErrorBudget } from '../lib/error-budget'
import { buildCheckUpload, buildPortableCheckSpec } from '../lib/board-upload'

// The Checks view: compose the body of a hauksbee-ci spec with plain
// language, run it through the REAL hauksbee-ci binary (`POST /api/check`
// shells the sibling install), and keep the artifact. Assertions are grouped
// by kind with counts; each row is a readable inline condition whose latest
// PASS/FAIL rides on the row after a run. The spec TOML is a persistent
// synced pane: builder edits regenerate it live, and raw-edit mode takes over
// when the builder's vocabulary runs out (round-tripped back when possible).
// State auto-saves per board (localStorage) and auto-restores when the same
// board is analyzed again.

interface SupplyRow {
  net: string
  volts: string
}

// One plain-language check. `kind` maps 1:1 onto the spec's [[assert]] kinds;
// fields are kept as strings so the inputs stay honest about what was typed.
interface CheckRow {
  id: number
  kind: string
  net: string
  ref: string
  min: string
  max: string
  after_ms: string
  deadline_ms: string
  contains: string
  freq_hz: string
  tolerance: string
  min_toggles: string
  amps: string
  celsius: string
  dip_below: string
  for_max_ms: string
  recover_to: string
  recover_within_ms: string
}

const emptyCheck = (id: number, kind: string, net = ''): CheckRow => ({
  id, kind, net,
  ref: '', min: '', max: '', after_ms: '', deadline_ms: '', contains: '',
  freq_hz: '', tolerance: '', min_toggles: '', amps: '', celsius: '',
  dip_below: '', for_max_ms: '', recover_to: '', recover_within_ms: '',
})

// The check-kind vocabulary: plain words first, the TOML kind in small print.
const CHECK_KINDS: { kind: string; label: string; group: string; hint: string }[] = [
  { kind: 'voltage', label: 'A net must sit at a voltage', group: 'Voltages', hint: 'min/max volts, optionally after a settle time' },
  { kind: 'rail_window', label: 'A rail may only dip briefly', group: 'Voltages', hint: 'bound brownout depth, duration and recovery' },
  { kind: 'no_faults', label: 'Nothing over-stressed', group: 'Stress', hint: 'no component beyond its ratings at any point' },
  { kind: 'max_current', label: 'A part must stay under a current', group: 'Currents', hint: 'ceiling in amps for one component' },
  { kind: 'max_temp', label: 'A part must stay cool', group: 'Temperatures', hint: 'junction temperature ceiling (or the part’s own rating)' },
  { kind: 'uart', label: 'The firmware must print', group: 'Firmware', hint: 'serial output contains a string' },
  { kind: 'toggle', label: 'A net must blink', group: 'Activity', hint: 'toggle frequency or a minimum toggle count' },
  { kind: 'boot-coverage', label: 'Firmware must drive a net by a deadline', group: 'Firmware', hint: 'a gate/enable must be actively driven after reset' },
]

/** The display groups, in a stable order (only groups with rows render). */
const GROUP_ORDER = ['Voltages', 'Currents', 'Temperatures', 'Stress', 'Firmware', 'Activity']

// Kinds that carry a net / a component ref. Shared by the TOML composer, the
// raw parser's round-trip check, and the per-kind field pickers so the three
// cannot drift apart.
const NET_KINDS = ['voltage', 'toggle', 'boot-coverage', 'rail_window']
const REF_KINDS = ['max_current', 'max_temp']

// The workflow YAML and the spec's file stem are composed in lib/ci-workflow.ts,
// which the Export menu shares: the action reference in it is pinned to this
// build's release tag (lib/version.ts, derived from package.json), so there is
// exactly one string in the app that has to be right.

interface CheckResult {
  label: string
  kind: string
  passed: boolean
  invalid: boolean
  detail: string
  evidence?: EvidenceMap
}

interface RunResponse {
  ok: boolean
  error?: string
  passed?: boolean
  exit_code?: number
  analog_abort?: boolean
  refusal?: RefusalContract
  coverage?: string | null
  substitutions?: string[]
  coverage_warnings?: string[]
  inventory?: ArtifactProvenance[]
  assumptions?: EvidenceAssumption[]
  evidence?: EvidenceMap[]
  timing_coverage?: Array<{
    mcu_ref: string
    backend: string
    cycle_exact: boolean
    timestamp_precision_s: number
    minimum_guaranteed_pulse_s: number
    chunk_s: number
  }>
  timing_refusals?: string[]
  results?: CheckResult[]
}

/** What the shell's status chips show after a run. */
export interface ChecksSummary {
  passed: number
  failed: number
  invalid: number
}

function tomlString(v: string): string {
  return JSON.stringify(v)
}

function numOr(v: string): string | null {
  const t = v.trim()
  if (t === '') return null
  return Number.isFinite(Number(t)) ? t : null
}

/** Compose the spec BODY (no board/firmware keys; the server injects those
 *  from the uploaded files). */
function buildToml(name: string, duration: string, supplies: SupplyRow[], checks: CheckRow[]): string {
  let out = `name = ${tomlString(name)}\n`
  const dur = numOr(duration)
  if (dur) out += `duration_ms = ${dur}\n`
  for (const s of supplies) {
    if (!s.net.trim()) continue
    out += `\n[[supply]]\nnet = ${tomlString(s.net.trim())}\nkind = "ideal"\nvolts = ${numOr(s.volts) ?? '5.0'}\n`
  }
  for (const c of checks) {
    out += `\n[[assert]]\nkind = ${tomlString(c.kind)}\n`
    const put = (key: string, v: string, quote = false) => {
      const t = v.trim()
      if (!t) return
      out += quote ? `${key} = ${tomlString(t)}\n` : (numOr(v) ? `${key} = ${numOr(v)}\n` : '')
    }
    if (NET_KINDS.includes(c.kind)) put('net', c.net, true)
    if (REF_KINDS.includes(c.kind)) put('ref', c.ref, true)
    if (c.kind === 'uart') put('contains', c.contains, true)
    put('min', c.min)
    put('max', c.max)
    put('after_ms', c.after_ms)
    put('deadline_ms', c.deadline_ms)
    put('freq_hz', c.freq_hz)
    put('tolerance', c.tolerance)
    put('min_toggles', c.min_toggles)
    put('amps', c.amps)
    put('celsius', c.celsius)
    put('dip_below', c.dip_below)
    put('for_max_ms', c.for_max_ms)
    put('recover_to', c.recover_to)
    put('recover_within_ms', c.recover_within_ms)
  }
  return out
}

// Fields the builder round-trips on a [[supply]] / an [[assert]]. Anything
// outside these sets (an assertion scenario, a ripple spec, a sensor...) would
// be silently destroyed on the way back to the builder, so its presence makes
// tomlToBuilder refuse the conversion.
const SUPPLY_FIELDS = new Set(['net', 'kind', 'volts'])
const ASSERT_FIELDS = new Set([
  'kind', 'net', 'ref', 'min', 'max', 'after_ms', 'deadline_ms', 'contains',
  'freq_hz', 'tolerance', 'min_toggles', 'amps', 'celsius', 'dip_below',
  'for_max_ms', 'recover_to', 'recover_within_ms',
])

/** Best-effort: load a raw TOML back into builder rows. Returns null when the
 *  spec uses vocabulary the builder doesn't cover (an unknown top-level key,
 *  OR any nested supply/assert field the builder would not write back out);
 *  the caller then stays in raw mode, or warns before discarding. */
function tomlToBuilder(raw: string): { name: string; duration: string; supplies: SupplyRow[]; checks: CheckRow[] } | null {
  let doc: Record<string, unknown>
  try {
    doc = parseToml(raw) as Record<string, unknown>
  } catch {
    return null
  }
  const KNOWN = new Set(['name', 'duration_ms', 'supply', 'assert', 'board', 'firmware', 'mcu'])
  if (Object.keys(doc).some(k => !KNOWN.has(k))) return null
  const supplies: SupplyRow[] = []
  for (const s of (doc.supply as Record<string, unknown>[] | undefined) ?? []) {
    if (Object.keys(s).some(k => !SUPPLY_FIELDS.has(k))) return null
    if (s.kind && s.kind !== 'ideal') return null
    supplies.push({ net: String(s.net ?? ''), volts: String(s.volts ?? '') })
  }
  const checks: CheckRow[] = []
  let id = 1
  for (const a of (doc.assert as Record<string, unknown>[] | undefined) ?? []) {
    const kind = String(a.kind ?? '')
    if (!CHECK_KINDS.some(k => k.kind === kind)) return null
    // Refuse any field the composer would not re-emit for this kind: it would
    // survive the parse but vanish from the round-tripped spec.
    for (const key of Object.keys(a)) {
      if (!ASSERT_FIELDS.has(key)) return null
      if (key === 'net' && !NET_KINDS.includes(kind)) return null
      if (key === 'ref' && !REF_KINDS.includes(kind)) return null
      if (key === 'contains' && kind !== 'uart') return null
    }
    const row = emptyCheck(id++, kind)
    const grab = (key: keyof CheckRow, tomlKey?: string) => {
      const v = a[tomlKey ?? key]
      if (v !== undefined) (row[key] as string) = String(v)
    }
    grab('net'); grab('ref'); grab('min'); grab('max'); grab('after_ms'); grab('deadline_ms')
    grab('contains'); grab('freq_hz'); grab('tolerance'); grab('min_toggles'); grab('amps')
    grab('celsius'); grab('dip_below'); grab('for_max_ms'); grab('recover_to'); grab('recover_within_ms')
    checks.push(row)
  }
  return {
    name: String(doc.name ?? 'board checks'),
    duration: String(doc.duration_ms ?? '100'),
    supplies,
    checks,
  }
}

/** The left column while raw-edit mode owns the spec: a live, read-only
 *  summary of what the raw TOML currently says (name, run length, supplies,
 *  the checks grouped by family), so the column is not a blank void. Falls
 *  back to a best-effort description when the spec uses vocabulary the
 *  builder cannot parse. */
function RawModeSummary({ rawText }: { rawText: string }) {
  const parsed = useMemo(() => tomlToBuilder(rawText), [rawText])
  return (
    <div className="hb-card px-4 py-4 text-[13px] leading-relaxed" style={{ color: 'var(--silk-dim)' }} data-testid="raw-summary">
      <div className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: 'var(--silk-faint)' }}>
        What the raw spec says
      </div>
      {parsed ? (
        <>
          <div>
            <b style={{ color: 'var(--silk)', fontWeight: 600 }}>{parsed.name}</b>
            {' '}· run length {parsed.duration} ms
          </div>
          {parsed.supplies.length > 0 && (
            <div className="mt-2">
              <span style={{ color: 'var(--silk)' }}>Power supplies:</span>{' '}
              {parsed.supplies.map(s => `${s.net} at ${s.volts} V`).join(', ')}
            </div>
          )}
          <ul className="mt-2 pl-4" style={{ listStyleType: 'disc' }}>
            {parsed.checks.map((c, i) => {
              const meta = CHECK_KINDS.find(k => k.kind === c.kind)
              const subject = c.net ? ` on ${c.net}` : c.ref ? ` for ${c.ref}` : ''
              return (
                <li key={i} className="my-0.5">
                  {meta?.label ?? c.kind}{subject}
                  <code className="text-[10px] ml-1.5" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                    {c.kind}
                  </code>
                </li>
              )
            })}
            {parsed.checks.length === 0 && <li>no assertions yet</li>}
          </ul>
        </>
      ) : (
        <div>
          The spec uses vocabulary beyond the visual builder (tolerances, scenarios,
          overrides, sensors ...), so it cannot be summarized here.
        </div>
      )}
      <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-faint)' }}>
        The raw TOML on the right is the source of truth now. The visual builder comes
        back when the spec fits its vocabulary (the “back to the builder” switch on the
        pane).
      </div>
    </div>
  )
}

function Field({ label, value, onChange, width = 90, placeholder, invalid }: {
  label: string
  value: string
  onChange: (v: string) => void
  width?: number
  placeholder?: string
  /** Highlight as the offending input of a validation message. */
  invalid?: boolean
}) {
  return (
    <label className="inline-flex items-center gap-1.5 text-[12px] min-w-0 max-w-full" style={{ color: invalid ? 'var(--err)' : 'var(--silk-faint)' }}>
      {label}
      <input
        className="hb-input tnum min-w-0"
        aria-invalid={invalid || undefined}
        // `width` is what the value wants; on a narrow column it is a ceiling and
        // the field takes what is left rather than pushing the row off the card.
        style={{ width, maxWidth: '100%', ...(invalid ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : {}) }}
        value={value}
        placeholder={placeholder}
        onChange={e => onChange(e.target.value)}
      />
    </label>
  )
}

/** One builder-row validation problem: which UI field, said in the UI's own
 *  words (never TOML key names; the raw pane owns that vocabulary). */
interface RowIssue {
  /** CheckRow field whose input gets the highlight. */
  field: keyof CheckRow
  message: string
}

/** Builder-mode preflight, mirroring hauksbee-ci's per-assertion requirements
 *  but speaking in the builder's field labels. Only fields actually missing
 *  are named, so "ref present, amps empty" says just "max A is empty". */
function rowIssues(c: CheckRow): RowIssue[] {
  const blank = (v: string) => v.trim() === ''
  const issues: RowIssue[] = []
  const needNet = () => { if (blank(c.net)) issues.push({ field: 'net', message: 'net is empty' }) }
  switch (c.kind) {
    case 'voltage':
      needNet()
      if (blank(c.min) && blank(c.max)) {
        issues.push({ field: 'min', message: 'needs a min V and/or a max V' })
      }
      break
    case 'uart':
      if (blank(c.contains)) issues.push({ field: 'contains', message: '"must print" is empty' })
      break
    case 'toggle':
      needNet()
      if (blank(c.freq_hz) && blank(c.min_toggles)) {
        issues.push({ field: 'freq_hz', message: 'needs a freq Hz or a min toggles' })
      }
      break
    case 'boot-coverage':
      needNet()
      if (blank(c.min)) issues.push({ field: 'min', message: 'reach V is empty' })
      if (blank(c.deadline_ms)) issues.push({ field: 'deadline_ms', message: 'within ms is empty' })
      break
    case 'max_current':
      if (blank(c.ref)) issues.push({ field: 'ref', message: 'part (ref) is empty' })
      if (blank(c.amps)) issues.push({ field: 'amps', message: 'max A is empty' })
      break
    case 'max_temp':
      // max °C may stay blank (falls back to the part's own rating).
      if (blank(c.ref)) issues.push({ field: 'ref', message: 'part (ref) is empty' })
      break
    case 'rail_window':
      needNet()
      if (blank(c.dip_below)) {
        issues.push({ field: 'dip_below', message: 'dip below V is empty' })
      } else if (blank(c.for_max_ms) && blank(c.recover_within_ms)) {
        issues.push({ field: 'for_max_ms', message: 'needs a for max ms or a recovery window (within ms)' })
      }
      if (!blank(c.recover_within_ms) && blank(c.recover_to)) {
        issues.push({ field: 'recover_to', message: 'recover to V is empty (needed with within ms)' })
      }
      break
    default:
      break
  }
  return issues
}

/** Inline PASS / FAIL / INVALID chip riding on a check row after a run. */
function ResultChip({ result, stale }: { result: CheckResult; stale: boolean }) {
  const tone = result.invalid
    ? { color: 'var(--warn)', bg: 'var(--warn-bg)', border: 'var(--warn-border)', label: 'INVALID' }
    : result.passed
      ? { color: 'var(--ok)', bg: 'var(--ok-bg)', border: 'var(--ok-border)', label: 'PASS' }
      : { color: 'var(--err)', bg: 'var(--err-bg)', border: 'var(--err-border)', label: 'FAIL' }
  // The word swaps (PASS becoming FAIL on a re-run) inside a fixed grid cell,
  // so the row never reflows on a verdict change. The colour is not animated:
  // it is applied by the wrapper's style, which means the chip is the right
  // colour on its first painted frame.
  return (
    <VerdictBadge
      data-testid="row-result"
      label={tone.label}
      title={stale ? `${result.detail} (from the last run; the spec has changed since)` : result.detail}
      className="px-2 rounded-full text-[10px] font-bold tracking-wide"
      style={{
        background: tone.bg, border: `1px solid ${tone.border}`, color: tone.color,
        height: 20, opacity: stale ? 0.45 : 1, flexShrink: 0,
      }}
    />
  )
}

/** Canonical assertion evidence from the CI runner. Wording is rendered
 * verbatim from the shared registry so web, CLI, JSON and CI artifacts cannot
 * drift into different explanations of the same limitation. */
function AssertionEvidence({
  result,
  assumptions,
  inventory,
  stale,
}: {
  result: CheckResult
  assumptions: readonly EvidenceAssumption[]
  inventory: readonly ArtifactProvenance[]
  stale: boolean
}) {
  const map = result.evidence
  if (!map) return null
  const linked = assumptionsForEvidence(map, assumptions)
  const artifacts = (map.artifacts ?? []).flatMap(index => inventory[index] ? [inventory[index]] : [])
  if (map.status === 'clean' && linked.length === 0 && !map.error_budget) return null
  return (
    <div
      data-testid="assertion-evidence"
      className="mt-2 rounded-md px-2.5 py-2 text-[11px]"
      style={{
        background: 'var(--surface-2)',
        border: '1px solid var(--hairline)',
        color: 'var(--silk-dim)',
        opacity: stale ? 0.55 : 1,
      }}
    >
      <div className="font-semibold uppercase tracking-wide" style={{ color: map.status === 'undermined' ? 'var(--warn)' : 'var(--silk-faint)' }}>
        Evidence {map.status}{map.error_budget ? ' · numeric qualification' : ''}
      </div>
      {map.error_budget && (
        <div className="mt-1.5" data-testid="error-budget-summary">
          {summarizeErrorBudget(map.error_budget).map(row => <div key={row}>{row}</div>)}
        </div>
      )}
      {linked.map(assumption => (
        <div key={assumption.id} className="mt-1.5">
          <code style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}>{assumption.id}</code>
          <div style={{ color: 'var(--silk)' }}>{assumption.statement}</div>
          <div>{assumption.because} {assumption.consequence}</div>
          <div>To remove it: {assumption.replacement}</div>
        </div>
      ))}
      {artifacts.length > 0 && (
        <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>
          Inputs: {artifacts.map(artifact => artifact.path.split('/').pop() ?? artifact.path).join(', ')}
        </div>
      )}
      {(map.models ?? []).length > 0 && (
        <div className="mt-1.5" data-testid="model-provenance" style={{ color: 'var(--silk-faint)' }}>
          Models: {(map.models ?? []).map(describeModelSource).join('; ')}
        </div>
      )}
    </div>
  )
}

/** Shape of the autosaved localStorage payload (all fields best-effort: a
 *  corrupt or partial save must never break the view). */
interface SavedChecksState {
  specName?: string
  duration?: string
  supplies?: SupplyRow[]
  checks?: CheckRow[]
  rawMode?: boolean
  rawText?: string
}

/** Storage key for a board's saved checks. The file name alone collides for
 *  common names (every project has a board.kicad_pcb), so a cheap fingerprint
 *  from the report disambiguates. The shell also uses this as the view's React
 *  key so it remounts per board: the mount-time restore is then authoritative
 *  and one board's state can never leak into another's. */
export function checksStorageKey(report: WebReport): string {
  return `hauksbee.checks.${report.file_name}:${report.num_components}:${report.num_nets}`
}

export function ChecksView({
  report,
  boardFile,
  firmwareFile,
  schematicFile,
  selectedNet,
  selectedComponent,
  pendingChecks,
  onPendingConsumed,
  onSummary,
  onSpec,
}: {
  report: WebReport
  boardFile: File | null
  firmwareFile: File | null
  schematicFile: File | null
  /** Net last clicked on the board render, offered as a one-click check. */
  selectedNet: string | null
  /** Component last clicked on the board render, offered as ref checks. */
  selectedComponent: SelectedComponent | null
  /** Checks queued from a board surface (report map or live sim), appended
   *  here as ordinary prefilled rows so the spec TOML is exactly what a
   *  hand-built check produces. */
  pendingChecks: QueuedCheck[]
  /** Every pending check up to (and including) seq has been applied. */
  onPendingConsumed: (upToSeq: number) => void
  /** Latest run's pass/fail counts for the shell's status chips (null when no
   *  current run result exists). */
  onSummary: (s: ChecksSummary | null) => void
  /** The spec text as it currently stands, for the shell's Export menu and the
   *  saved session. Reported rather than recomposed there: the exported spec must
   *  be the same bytes this pane shows and the Download button writes. */
  onSpec: (s: { toml: string; fileName: string } | null) => void
}) {
  const reduced = useReducedMotion()
  const storageKey = checksStorageKey(report)
  // ── Restore a saved session for this board (auto-load). Because the view
  // remounts per board (keyed on storageKey by the shell), restoring in the
  // state initializers is race-free: the autosave effect cannot fire before
  // the restore has happened, and a board with no saved state starts from the
  // report's own defaults instead of inheriting the previous board's rows. ──
  const saved = useMemo<SavedChecksState | null>(() => {
    try {
      const raw = localStorage.getItem(storageKey)
      return raw ? (JSON.parse(raw) as SavedChecksState) : null
    } catch {
      return null
    }
  }, [storageKey])
  const savedSupplies = saved?.supplies
  const savedChecks = saved?.checks
  const initialChecks = Array.isArray(savedChecks) && savedChecks.length
    ? savedChecks
    : [emptyCheck(1, 'no_faults')]
  const [specName, setSpecName] = useState(saved?.specName || `${report.board_name || report.file_name} checks`)
  const [duration, setDuration] = useState(saved?.duration || '100')
  const [supplies, setSupplies] = useState<SupplyRow[]>(
    () => (Array.isArray(savedSupplies) && savedSupplies.length
      ? savedSupplies
      : (report.supplies ?? []).map(s => ({ net: s.net, volts: String(s.volts) }))),
  )
  const [checks, setChecks] = useState<CheckRow[]>(initialChecks)
  const [rawMode, setRawMode] = useState(!!(saved?.rawMode && typeof saved.rawText === 'string'))
  const [rawText, setRawText] = useState(typeof saved?.rawText === 'string' ? saved.rawText : '')
  const [addOpen, setAddOpen] = useState(false)
  const [running, setRunning] = useState(false)
  // The last run's response, plus the exact spec text it ran, so results can
  // be flagged stale the moment the spec diverges.
  const [run, setRun] = useState<{ response: RunResponse; toml: string } | null>(null)
  const [ciOpen, setCiOpen] = useState(false)
  const nextId = useRef(initialChecks.reduce((m, c) => Math.max(m, c.id), 0) + 1)

  const builtToml = useMemo(
    () => buildToml(specName, duration, supplies, checks),
    [specName, duration, supplies, checks],
  )
  const effectiveToml = rawMode ? rawText : builtToml
  const stale = run !== null && run.toml !== effectiveToml

  // ── Auto-save (the "things auto load in future" contract). ──
  useEffect(() => {
    try {
      localStorage.setItem(storageKey, JSON.stringify({ specName, duration, supplies, checks, rawMode, rawText }))
    } catch { /* storage full/blocked: the session still works */ }
  }, [storageKey, specName, duration, supplies, checks, rawMode, rawText])

  // Report the run summary to the shell's chips; a stale or failed run
  // reports nothing rather than a number that no longer matches the spec.
  useEffect(() => {
    const results = run?.response.ok ? run.response.results ?? [] : []
    if (!run || stale || !run.response.ok || results.length === 0) {
      onSummary(null)
      return
    }
    onSummary({
      passed: results.filter(x => !x.invalid && x.passed).length,
      failed: results.filter(x => !x.invalid && !x.passed).length,
      invalid: results.filter(x => x.invalid).length,
    })
  }, [run, stale, onSummary])

  const addCheck = (kind: string, net = '', ref = '') => {
    setChecks(cs => {
      const row = emptyCheck(nextId.current++, kind, net)
      row.ref = ref
      return [...cs, row]
    })
    setAddOpen(false)
  }

  // Consume checks queued from a board surface (a net/component click on the
  // report map or in the live sim). Builder mode appends them as ordinary
  // rows; raw mode appends the equivalent [[assert]] text so nothing queued
  // is ever silently dropped.
  useEffect(() => {
    if (pendingChecks.length === 0) return
    const upTo = pendingChecks[pendingChecks.length - 1].seq
    if (rawMode) {
      setRawText(prev => prev + pendingChecks.map(c => {
        let s = `\n[[assert]]\nkind = ${tomlString(c.kind)}\n`
        if (c.net) s += `net = ${tomlString(c.net)}\n`
        if (c.ref) s += `ref = ${tomlString(c.ref)}\n`
        return s
      }).join(''))
    } else {
      setChecks(cs => [
        ...cs,
        ...pendingChecks.map(c => {
          const row = emptyCheck(nextId.current++, c.kind, c.net ?? '')
          if (c.ref) row.ref = c.ref
          return row
        }),
      ])
    }
    onPendingConsumed(upTo)
  }, [pendingChecks, rawMode, onPendingConsumed])

  // Builder-mode preflight results: row id -> its missing-field issues.
  // Set when a run is attempted with holes; a row's entry clears the moment
  // that row is edited so the highlight never nags about fixed input.
  const [validation, setValidation] = useState<Map<number, RowIssue[]>>(new Map())

  const update = (id: number, patch: Partial<CheckRow>) => {
    setChecks(cs => cs.map(c => (c.id === id ? { ...c, ...patch } : c)))
    setValidation(prev => {
      if (!prev.has(id)) return prev
      const next = new Map(prev)
      next.delete(id)
      return next
    })
  }

  const runChecks = useCallback(async () => {
    if (!boardFile) return
    // Preflight the builder rows in the builder's own vocabulary before
    // anything is POSTed: the server's TOML-keyed errors ("needs `ref` and
    // `amps`") belong to raw mode, and they also over-named fields that were
    // actually present.
    if (!rawMode) {
      const problems = new Map<number, RowIssue[]>()
      for (const c of checks) {
        const issues = rowIssues(c)
        if (issues.length > 0) problems.set(c.id, issues)
      }
      setValidation(problems)
      if (problems.size > 0) return
    }
    setRunning(true)
    const tomlAtRun = effectiveToml
    try {
      const fd = buildCheckUpload(boardFile, firmwareFile, schematicFile, tomlAtRun)
      const res = await fetch('/api/check', { method: 'POST', body: fd })
      const text = await res.text()
      try {
        setRun({ response: JSON.parse(text) as RunResponse, toml: tomlAtRun })
      } catch {
        setRun({
          response: { ok: false, error: text.trim().slice(0, 400) || `${res.status} ${res.statusText}` },
          toml: tomlAtRun,
        })
      }
    } catch (e) {
      setRun({
        response: { ok: false, error: e instanceof Error ? e.message : String(e) },
        toml: tomlAtRun,
      })
    } finally {
      setRunning(false)
    }
  }, [boardFile, firmwareFile, schematicFile, effectiveToml, rawMode, checks])

  const specStem = specStemFor(report.file_name)
  // The runnable spec: the composed body with the board/firmware paths the
  // recommended repo layout puts them at. The builder composes a fragment (it
  // never writes board/firmware lines), but a raw spec may be a full file that
  // already names its paths; a key is prepended only when the spec text does not
  // already carry it, so nothing is doubled.
  //
  // Memoized because it is now THE spec: the pane renders it, the Download
  // button writes it, the Export menu offers it and the saved session stores it.
  // Four readers of a function recomputed per render is four chances for them to
  // disagree about what the file is.
  const specText = useMemo(() => {
    return buildPortableCheckSpec(
      report.file_name,
      firmwareFile,
      schematicFile,
      effectiveToml,
    )
  }, [effectiveToml, firmwareFile, schematicFile, report.file_name])

  const specFileName = `${specStem}.toml`

  // Hand the spec up to the shell, so the Export menu and the saved session
  // carry the same bytes this pane shows rather than a second composition of
  // them.
  //
  // This pane owns the lifetime of that value, including clearing it on the way
  // out. The shell used to clear it alongside the rest of its run-derived state,
  // which lost a race on the one path where both happen in the same commit: a
  // restored session remounts this pane (its key is the board's identity) and the
  // child's effect reports the spec BEFORE the parent's effect clears it, so the
  // Export menu offered a report with no spec in it, on exactly the surface the
  // feature exists for.
  useEffect(() => {
    onSpec({ toml: specText, fileName: specFileName })
  }, [specText, specFileName, onSpec])
  useEffect(() => () => onSpec(null), [onSpec])

  const download = (name: string, contents: string) =>
    downloadText(name, contents, 'text/plain;charset=utf-8')

  const workflowYml = workflowYaml(specStem)

  const netOptions = report.nets ?? []
  const result = run?.response ?? null
  const results = result?.ok ? result.results ?? [] : []

  // Row -> result: results come back in [[assert]] order, which is the
  // `checks` array order (buildToml writes them in sequence). Grouping below
  // reorders the DISPLAY only, so each row carries its array index along.
  const rowIndex = new Map<number, number>()
  checks.forEach((c, i) => rowIndex.set(c.id, i))
  const resultForRow = (id: number): CheckResult | null => {
    if (rawMode || results.length === 0) return null
    const i = rowIndex.get(id)
    if (i === undefined || i >= results.length) return null
    return results[i]
  }

  // Group rows by display group, in stable order.
  const grouped = GROUP_ORDER
    .map(group => ({
      group,
      kinds: CHECK_KINDS.filter(k => k.group === group),
      rows: checks.filter(c => CHECK_KINDS.find(k => k.kind === c.kind)?.group === group),
    }))
    .filter(g => g.rows.length > 0)

  const overall = result?.ok
    ? results.length > 0
      ? results.some(x => !x.invalid && !x.passed)
        ? 'failed'
        : results.some(x => x.invalid) ? 'invalid' : 'passed'
      : (result.passed ? 'passed' : 'failed')
    : null

  return (
    <div className="h-full overflow-y-auto view-enter" data-testid="checks-panel">
      <div className="max-w-6xl mx-auto px-6 pt-5 pb-16">
        <div className="text-[13px] leading-relaxed mb-4" style={{ color: 'var(--silk-dim)', maxWidth: '46rem' }}>
          Pick what must hold (a rail voltage, a blink, a print, nothing over-stressed), run it
          here, then take the spec file with you; the same file <code className="hb-inline">hauksbee-ci</code>{' '}
          runs in a pipeline. Click a trace on the Board view to start a check on that net.
        </div>

        {/* datalist for every net picker */}
        <datalist id="net-options">
          {netOptions.map(n => <option key={n} value={n} />)}
        </datalist>

        {/* Two columns where both fit, one stacked column below 1024px. The
            track sizes and the stacking live on `.checks-grid` in index.css,
            next to the rest of this surface's box discipline. */}
        <div className="checks-grid gap-5">
          {/* ── Left: the builder ── */}
          <div className="min-w-0 checks-flush">
            {/* Quick adds from the board-surface selection */}
            {selectedNet && !rawMode && (
              <button
                type="button"
                data-testid="quick-add-net"
                onClick={() => addCheck('voltage', selectedNet)}
                className="hb-chip hb-press mb-3 px-3 py-1.5 text-[12px]"
              >
                + Check a voltage on “{selectedNet}” (clicked on the map)
              </button>
            )}
            {selectedComponent && !rawMode && (
              <div className="mb-3 flex flex-wrap gap-2">
                <button
                  type="button"
                  data-testid="quick-add-ref-current"
                  onClick={() => addCheck('max_current', '', selectedComponent.ref)}
                  className="hb-chip hb-press px-3 py-1.5 text-[12px]"
                >
                  + “{selectedComponent.ref}” must stay under a current (clicked on the map)
                </button>
                <button
                  type="button"
                  data-testid="quick-add-ref-temp"
                  onClick={() => addCheck('max_temp', '', selectedComponent.ref)}
                  className="hb-chip hb-press px-3 py-1.5 text-[12px]"
                >
                  + “{selectedComponent.ref}” must stay cool
                </button>
              </div>
            )}

            {rawMode ? (
              <RawModeSummary rawText={rawText} />
            ) : (
              <>
                {/* Spec identity */}
                <div className="hb-card px-4 py-3 mb-4 flex flex-wrap items-center gap-x-5 gap-y-2">
                  <label className="inline-flex items-center gap-2 text-[12px] min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
                    spec name
                    <input
                      className="hb-input min-w-0 flex-1"
                      style={{ maxWidth: 220 }}
                      value={specName}
                      onChange={e => setSpecName(e.target.value)}
                    />
                  </label>
                  <Field label="run length (ms)" value={duration} width={70} onChange={setDuration} />
                  <span className="text-[12px]" style={{ color: 'var(--silk-faint)' }}>
                    {firmwareFile
                      ? <>firmware: <span style={{ color: 'var(--ok)', fontFamily: 'var(--font-mono)' }}>{firmwareFile.name}</span> (co-simulated)</>
                      : 'no firmware loaded: add it on the Board view to check UART/blink/boot'}
                  </span>
                </div>

                {/* Power */}
                <section className="hb-card px-4 py-3.5 mb-4">
                  <div className="flex items-center justify-between mb-2">
                    <h2 className="text-[11px] font-bold tracking-widest uppercase" style={{ margin: 0, color: 'var(--silk-faint)' }}>
                      Power supplies
                    </h2>
                    <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                      detected from the board, adjust if wrong
                    </span>
                  </div>
                  {/* net, volts, remove. The net picker gives up width first (it
                      is the one field with slack), then the row wraps. `remove` is
                      last in reading order but never last in line: it holds its
                      size and stays inside the card at every width. */}
                  {supplies.map((s, i) => (
                    <div key={i} className="flex flex-wrap items-center gap-x-2 gap-y-1.5 mb-1.5">
                      <input
                        className="hb-input min-w-0 flex-1"
                        style={{ maxWidth: 180 }}
                        list="net-options"
                        value={s.net}
                        placeholder="net (e.g. +5V)"
                        onChange={e => setSupplies(ss => ss.map((x, j) => (j === i ? { ...x, net: e.target.value } : x)))}
                      />
                      <Field label="volts" value={s.volts} width={70}
                        onChange={v => setSupplies(ss => ss.map((x, j) => (j === i ? { ...x, volts: v } : x)))} />
                      <button type="button" className="hb-press text-[12px] cursor-pointer shrink-0" style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
                        onClick={() => setSupplies(ss => ss.filter((_, j) => j !== i))}>remove</button>
                    </div>
                  ))}
                  <button type="button" className="hb-press text-[12px] cursor-pointer inline-flex items-center gap-1" style={{ color: 'var(--silk-dim)', background: 'none', border: 'none' }}
                    onClick={() => setSupplies(ss => [...ss, { net: '', volts: '5.0' }])}>
                    <PlusIcon size={12} /> add a supply
                  </button>
                </section>

                {/* Assertions, grouped by kind. The groups stagger in once on
                    mount (a restored board's saved spec arriving), which is the
                    only time this list is new to the reader. */}
                {grouped.map(({ group, kinds, rows }, gi) => (
                  <StaggerItem key={group} index={gi}>
                  <section className="hb-card px-4 py-3.5 mb-4" data-testid={`check-group-${group}`}>
                    <div className="flex items-center justify-between mb-2">
                      <h2 className="text-[11px] font-bold tracking-widest uppercase inline-flex items-center gap-2" style={{ margin: 0, color: 'var(--silk-faint)' }}>
                        {group}
                        <span
                          className="tnum text-[10px] px-1.5 rounded-full"
                          style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
                        >
                          {rows.length}
                        </span>
                      </h2>
                      {/* Add another of this group's first kind (the common case);
                          the global menu below covers everything. */}
                      <button
                        type="button"
                        onClick={() => addCheck(kinds[0].kind)}
                        className="hb-press text-[11px] cursor-pointer inline-flex items-center gap-1"
                        style={{ color: 'var(--copper)', background: 'none', border: 'none' }}
                      >
                        <PlusIcon size={11} /> add
                      </button>
                    </div>

                    {/* Rows appear and disappear as the reader composes the
                        spec. Without presence, a removed row vanishes between
                        frames and the rows below jump up into the gap, which
                        reads as "something else also changed". The exit is
                        deliberately faster than the entry (see ../motion
                        tokens): a row on its way out must not hold its slot
                        open while the reader waits to see the result. */}
                    <AnimatePresence initial={false}>
                    {rows.map(c => {
                      const meta = CHECK_KINDS.find(k => k.kind === c.kind)
                      const rowResult = resultForRow(c.id)
                      const issues = validation.get(c.id) ?? []
                      // Combined either/or requirements highlight every input
                      // that could satisfy them (min OR max, freq OR toggles).
                      const bad = (field: keyof CheckRow) => issues.some(i => i.field === field)
                      return (
                        <motion.div
                          key={c.id}
                          layout={reduced ? false : 'position'}
                          initial={reduced ? false : { opacity: 0, y: -4 }}
                          animate={{ opacity: 1, y: 0 }}
                          exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, transition: LEAVE }}
                          transition={reduced ? { duration: 0 } : ARRIVE}
                          className="check-row py-2.5"
                          style={{ overflow: 'hidden' }}
                        >
                          <div className="flex items-center justify-between gap-2">
                            <div className="text-[13px] min-w-0 flex flex-wrap items-center gap-x-2" style={{ color: 'var(--silk)' }}>
                              {/* The plain-language label is what the row IS, so it
                                  wraps rather than truncating. On a phone column the
                                  kind chip and the verdict left it about 24px, which
                                  showed "A…" beside a perfectly legible `voltage`:
                                  the row's whole point, ellipsed away. */}
                              <span>{meta?.label ?? c.kind}</span>
                              <code className="text-[10px] shrink-0" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>{c.kind}</code>
                              {rowResult && <ResultChip result={rowResult} stale={stale} />}
                            </div>
                            <button type="button" className="hb-press text-[12px] cursor-pointer shrink-0" style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
                              onClick={() => setChecks(cs => cs.filter(x => x.id !== c.id))}>remove</button>
                          </div>
                          <div className="mt-2 flex flex-wrap gap-x-4 gap-y-2">
                            {NET_KINDS.includes(c.kind) && (
                              <label className="inline-flex items-center gap-1.5 text-[12px] min-w-0 max-w-full" style={{ color: bad('net') ? 'var(--err)' : 'var(--silk-faint)' }}>
                                net
                                <input className="hb-input min-w-0 flex-1" aria-invalid={bad('net') || undefined}
                                  style={{ maxWidth: 170, ...(bad('net') ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : {}) }}
                                  list="net-options" value={c.net}
                                  onChange={e => update(c.id, { net: e.target.value })} />
                              </label>
                            )}
                            {REF_KINDS.includes(c.kind) && (
                              <Field label="part (ref)" value={c.ref} width={90} placeholder="U1" invalid={bad('ref')}
                                onChange={v => update(c.id, { ref: v })} />
                            )}
                            {c.kind === 'voltage' && (
                              <>
                                <Field label="min V" value={c.min} width={64} invalid={bad('min')} onChange={v => update(c.id, { min: v })} />
                                <Field label="max V" value={c.max} width={64} invalid={bad('min')} onChange={v => update(c.id, { max: v })} />
                                <Field label="after ms" value={c.after_ms} width={64} onChange={v => update(c.id, { after_ms: v })} />
                              </>
                            )}
                            {c.kind === 'uart' && (
                              <Field label="must print" value={c.contains} width={220} placeholder="hello" invalid={bad('contains')}
                                onChange={v => update(c.id, { contains: v })} />
                            )}
                            {c.kind === 'toggle' && (
                              <>
                                <Field label="freq Hz" value={c.freq_hz} width={64} invalid={bad('freq_hz')} onChange={v => update(c.id, { freq_hz: v })} />
                                <Field label="±tol" value={c.tolerance} width={56} onChange={v => update(c.id, { tolerance: v })} />
                                <Field label="or min toggles" value={c.min_toggles} width={64} invalid={bad('freq_hz')} onChange={v => update(c.id, { min_toggles: v })} />
                              </>
                            )}
                            {c.kind === 'boot-coverage' && (
                              <>
                                <Field label="reach V" value={c.min} width={64} invalid={bad('min')} onChange={v => update(c.id, { min: v })} />
                                <Field label="within ms" value={c.deadline_ms} width={64} invalid={bad('deadline_ms')} onChange={v => update(c.id, { deadline_ms: v })} />
                              </>
                            )}
                            {c.kind === 'max_current' && (
                              <Field label="max A" value={c.amps} width={64} invalid={bad('amps')} onChange={v => update(c.id, { amps: v })} />
                            )}
                            {c.kind === 'max_temp' && (
                              <Field label="max °C (blank = part rating)" value={c.celsius} width={70}
                                onChange={v => update(c.id, { celsius: v })} />
                            )}
                            {c.kind === 'rail_window' && (
                              <>
                                <Field label="dip below V" value={c.dip_below} width={64} invalid={bad('dip_below')} onChange={v => update(c.id, { dip_below: v })} />
                                <Field label="for max ms" value={c.for_max_ms} width={64} invalid={bad('for_max_ms')} onChange={v => update(c.id, { for_max_ms: v })} />
                                <Field label="recover to V" value={c.recover_to} width={64} invalid={bad('recover_to')} onChange={v => update(c.id, { recover_to: v })} />
                                <Field label="within ms" value={c.recover_within_ms} width={64} invalid={bad('for_max_ms')} onChange={v => update(c.id, { recover_within_ms: v })} />
                              </>
                            )}
                          </div>
                          {/* The missing-values verdict, on the row it judges,
                              in the builder's own field names. */}
                          {issues.length > 0 && (
                            <div data-testid="row-validation" className="mt-1.5 text-[11px]" style={{ color: 'var(--err)' }}>
                              Not run: {issues.map(i => i.message).join(' · ')}
                            </div>
                          )}
                          {/* The run's measured value, on the row it judged:
                              a bare PASS chip hid the number the run actually
                              produced (it lived only in a hover tooltip). */}
                          {rowResult && rowResult.detail && (
                            <div
                              data-testid="row-result-detail"
                              className="mt-1.5 text-[11px] tnum"
                              style={{
                                color: 'var(--silk-faint)',
                                fontFamily: 'var(--font-mono)',
                                opacity: stale ? 0.55 : 1,
                              }}
                            >
                              {/* The measured number rolls to its new value on
                                  a re-run: up from below when it rose, down
                                  from above when it fell. That direction is the
                                  only thing the animation carries, and it is
                                  the thing the reader would otherwise have to
                                  remember. No colour: a measurement going up is
                                  not good news, it is a measurement. */}
                              <ValueSettle value={rowResult.detail} />
                              {stale ? ' (from the last run; the spec has changed since)' : ''}
                            </div>
                          )}
                          {rowResult && (
                            <AssertionEvidence
                              result={rowResult}
                              assumptions={run?.response.assumptions ?? []}
                              inventory={run?.response.inventory ?? []}
                              stale={stale}
                            />
                          )}
                        </motion.div>
                      )
                    })}
                    </AnimatePresence>
                  </section>
                  </StaggerItem>
                ))}

                {/* Every check removed. The area above is now blank, and a blank
                    area is a question the interface asked and then refused to
                    answer: this says what is missing, what happens if it stays
                    missing (the run is refused, not silently empty), and the one
                    check worth starting from. */}
                {grouped.length === 0 && (
                  <div className="mb-4">
                    <EmptyState
                      testId="checks-empty"
                      title="No checks in this spec yet"
                      body={<>
                        A spec with no assertions has nothing to pass or fail, so the run is
                        refused rather than reported as green. Add one below, or click a trace
                        on the Board view to start from a real net.
                      </>}
                      action={
                        <button
                          type="button"
                          data-testid="checks-empty-add"
                          onClick={() => addCheck('no_faults')}
                          className="hb-btn-primary hb-press px-3.5 py-2 text-[13px] inline-flex items-center gap-1.5"
                        >
                          <PlusIcon size={13} /> Start with “nothing over-stressed”
                        </button>
                      }
                    />
                  </div>
                )}

                {/* Global add menu: the whole vocabulary */}
                <div className="relative">
                  <button type="button" data-testid="add-check" className="hb-btn hb-press px-3 py-1.5 text-[13px] inline-flex items-center gap-1.5"
                    onClick={() => setAddOpen(o => !o)}>
                    <PlusIcon size={13} /> Add a check
                  </button>
                  {addOpen && (
                    <div className="hb-card view-enter absolute z-10 mt-1 overflow-hidden" style={{ width: 400, maxWidth: '100%', boxShadow: 'var(--shadow-pop)' }}>
                      {CHECK_KINDS.map(k => (
                        <button key={k.kind} type="button" onClick={() => addCheck(k.kind)}
                          className="hb-press block w-full text-left px-3 py-2 cursor-pointer"
                          style={{ background: 'none', border: 'none' }}
                          onMouseEnter={e => { (e.currentTarget as HTMLElement).style.background = 'var(--copper-tint)' }}
                          onMouseLeave={e => { (e.currentTarget as HTMLElement).style.background = 'none' }}>
                          <div className="text-[13px]" style={{ color: 'var(--silk)' }}>{k.label}</div>
                          <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{k.hint}</div>
                        </button>
                      ))}
                    </div>
                  )}
                </div>
              </>
            )}
          </div>

          {/* ── Right: the spec TOML, source of truth, always in view ──
              Pinned beside the builder in two columns; below the stacking width
              it sits after the builder and scrolls with it (`.checks-spec`). */}
          <div className="min-w-0">
            <div className="sticky top-0 checks-spec">
              <div className="hb-card overflow-hidden">
                <div
                  className="flex items-center justify-between px-3 py-2"
                  style={{ borderBottom: '1px solid var(--hairline)' }}
                >
                  <div className="text-[11px] font-bold tracking-widest uppercase inline-flex items-center gap-2" style={{ color: 'var(--silk-faint)' }}>
                    <span
                      style={{
                        width: 7, height: 7, borderRadius: 4, display: 'inline-block',
                        background: stale ? 'var(--warn)' : 'var(--ok)',
                      }}
                      title={stale
                        ? 'The spec changed since the last run'
                        : 'These are the assertions that run, and this is the file the download writes'}
                    />
                    spec.toml
                  </div>
                  <button type="button" data-testid="raw-toggle" className="hb-press text-[11px] cursor-pointer"
                    style={{ color: 'var(--copper)', background: 'none', border: 'none' }}
                    onClick={() => {
                      if (!rawMode) {
                        setRawText(builtToml)
                        setRawMode(true)
                      } else {
                        const parsed = tomlToBuilder(rawText)
                        if (parsed) {
                          // Raw is the source of truth on a successful parse: an
                          // intentionally emptied list clears the builder rows too.
                          setSpecName(parsed.name)
                          setDuration(parsed.duration)
                          setSupplies(parsed.supplies)
                          setChecks(parsed.checks)
                          nextId.current = parsed.checks.reduce((m, c) => Math.max(m, c.id), 0) + 1
                          setRawMode(false)
                        } else if (window.confirm(
                          'This TOML uses features the visual builder does not cover '
                          + '(tolerances, scenarios, overrides, sensors...). Going back to the '
                          + 'builder will DISCARD the raw text. Continue?')) {
                          setRawMode(false)
                        }
                      }
                    }}>
                    {rawMode ? '← back to the builder' : 'edit raw →'}
                  </button>
                </div>
                {rawMode ? (
                  <textarea
                    data-testid="raw-toml"
                    value={rawText}
                    onChange={e => setRawText(e.target.value)}
                    spellCheck={false}
                    className="w-full block p-3"
                    style={{
                      background: 'var(--code-bg)', border: 'none', outline: 'none', resize: 'vertical',
                      color: 'var(--silk)', minHeight: 260, maxHeight: '46vh',
                      fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: 1.5,
                    }}
                  />
                ) : (
                  // What this pane shows IS the file the Download button
                  // writes, byte for byte, including the `board` (and
                  // `firmware`) path lines. It used to render the builder's
                  // fragment instead, so the preview was missing the two lines
                  // that make the spec runnable and the download was a
                  // different file from the one that had been reviewed.
                  <pre
                    data-testid="spec-preview"
                    className="p-3 m-0 overflow-auto text-[12px] leading-relaxed"
                    style={{
                      background: 'var(--code-bg)', color: 'var(--silk-dim)',
                      fontFamily: 'var(--font-mono)', maxHeight: '46vh',
                    }}
                  >
                    {specText}
                  </pre>
                )}
              </div>
              {!rawMode && (
                <div className="mt-1.5 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                  This is the file the Download button writes. Running from here uses the
                  design inputs already uploaded in this session, so the path lines are for
                  the checked-in copy.
                </div>
              )}

              {/* Actions */}
              <div className="mt-3 flex flex-wrap items-center gap-2">
                <button type="button" data-testid="run-checks" disabled={!boardFile || running}
                  onClick={() => void runChecks()}
                  className="hb-btn-primary hb-press px-4 py-2 text-[13px] inline-flex items-center gap-2">
                  {running && <span className="slot-spin" style={{ borderTopColor: 'var(--on-copper)' }} />}
                  {running ? 'Running…' : 'Run these checks now'}
                </button>
                <button type="button" className="hb-btn hb-press px-3 py-2 text-[13px]"
                  onClick={() => download(specFileName, specText)}>
                  Download {specStem}.toml
                </button>
                <button type="button" className="hb-btn hb-press px-3 py-2 text-[13px]"
                  onClick={() => setCiOpen(o => !o)}>
                  {ciOpen ? 'Hide GitHub CI setup' : 'Set up GitHub CI'}
                </button>
              </div>
              {!boardFile && (
                <div className="mt-1.5 text-[12px]" style={{ color: 'var(--silk-faint)' }}>
                  (running from here needs the board uploaded in this session)
                </div>
              )}
              {!rawMode && validation.size > 0 && (
                <div
                  data-testid="builder-validation"
                  className="mt-3 rounded-lg px-3 py-2.5 text-[13px]"
                  style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
                >
                  Nothing was run: {validation.size === 1 ? 'a check is' : `${validation.size} checks are`} missing
                  values. The highlighted fields above say which.
                </div>
              )}

              {/* Overall result. It fades up once when the run lands; it does
                  NOT re-animate when the spec is edited afterwards, because the
                  panel then goes stale rather than becoming new. */}
              {result && (
                <ArriveOnce key={run?.toml} className="mt-3">
                <div data-testid="check-results" aria-live="polite">
                  {result.ok === false ? (
                    // The server's vocabulary errors list the accepted values
                    // as one unbroken pipe-separated run ("voltage|uart|..."),
                    // which has no space to break at: without `anywhere` it
                    // ran straight off the card's right edge. `anywhere` (not
                    // `break-word`) is what lets a single long token wrap.
                    <div className="rounded-lg px-3 py-2.5 text-[13px]"
                      style={{
                        background: 'var(--err-bg)', border: '1px solid var(--err-border)',
                        color: 'var(--err-strong)', overflowWrap: 'anywhere',
                      }}>
                      {result.error}
                    </div>
                  ) : (
                    <>
                      <div className="rounded-lg px-3 py-2.5 text-[14px] font-semibold"
                        style={overall === 'passed'
                          ? { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' }
                          : overall === 'invalid'
                            ? { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--warn-strong)' }
                            : { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}>
                        {overall === 'passed' ? 'All checks passed.' : overall === 'invalid' ? 'Some checks could not be judged.' : 'Checks failed.'}
                        {result.analog_abort && ' (analog solve aborted, results not trustworthy)'}
                        {stale && (
                          <div className="text-[11px] font-normal mt-0.5" style={{ color: 'var(--silk-dim)' }}>
                            from a previous run; the spec has changed since
                          </div>
                        )}
                      </div>
                      {overall === 'invalid' && result.refusal && (
                        <div className="mt-2 rounded-lg px-3 py-2.5 text-[12px]"
                          data-testid="refusal-contract"
                          style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)' }}>
                          {refusalLines(result.refusal).map(([label, value]) => (
                            <div key={label} className="mt-1 first:mt-0" style={{ color: 'var(--silk)' }}>
                              <strong style={{ color: 'var(--warn-strong)' }}>{label}:</strong> {value}
                            </div>
                          ))}
                        </div>
                      )}
                      {/* Raw mode has no rows to annotate; list results here. */}
                      {rawMode && results.map((x, i) => (
                        <div key={i} className="mt-1.5 rounded-lg px-3 py-2 text-[13px]"
                          style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}>
                          <div className="flex flex-wrap gap-2">
                            <span style={{ color: x.invalid ? 'var(--warn)' : x.passed ? 'var(--ok)' : 'var(--err)', fontWeight: 700 }}>
                              {x.invalid ? 'INVALID' : x.passed ? 'PASS' : 'FAIL'}
                            </span>
                            <span style={{ color: 'var(--silk)' }}>{x.label}</span>
                            <span style={{ color: 'var(--silk-faint)' }}>{x.detail}</span>
                          </div>
                          <AssertionEvidence
                            result={x}
                            assumptions={result.assumptions ?? []}
                            inventory={result.inventory ?? []}
                            stale={stale}
                          />
                        </div>
                      ))}
                      {(result.substitutions ?? []).map((s, i) => (
                        <div key={`s${i}`} className="mt-1.5 text-[12px]" style={{ color: 'var(--warn)' }}>substitute core: {s}</div>
                      ))}
                      {(result.coverage_warnings ?? []).map((s, i) => (
                        <div key={`c${i}`} className="mt-1 text-[12px]" style={{ color: 'var(--warn)' }}>coverage: {s}</div>
                      ))}
                      {(result.timing_coverage ?? []).map((t, i) => (
                        <div key={`t${i}`} className="mt-1 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                          timing {t.mcu_ref} ({t.backend}): edge ±{(t.timestamp_precision_s * 1e6).toFixed(3)} µs;
                          {' '}pulses ≥ {(t.minimum_guaranteed_pulse_s * 1e6).toFixed(3)} µs guaranteed;
                          {' '}{t.cycle_exact ? 'cycle-exact' : 'poll-boundary'}
                        </div>
                      ))}
                      {(result.timing_refusals ?? []).map((s, i) => (
                        <div key={`tr${i}`} className="mt-1 text-[12px]" style={{ color: 'var(--err)' }}>
                          timing invalid: {s}
                        </div>
                      ))}
                    </>
                  )}
                </div>
                </ArriveOnce>
              )}

              {/* GitHub CI setup: the two files and where they go. */}
              {ciOpen && (
                <div className="hb-card view-enter mt-3 px-3 py-3">
                  <div className="text-[13px] mb-2" style={{ color: 'var(--silk-dim)' }}>
                    Two files make this run on every push. Recommended layout: board in{' '}
                    <code className="hb-inline">hardware/</code>, firmware in <code className="hb-inline">firmware/</code>,
                    this spec in <code className="hb-inline">ci/</code>.
                  </div>
                  <div className="text-[12px] mb-1" style={{ color: 'var(--silk-dim)' }}>
                    1. <code className="hb-inline break-all">ci/{specStem}.toml</code>; the Download button above produces it
                    (paths already relative to that layout).
                  </div>
                  <div className="text-[12px] mb-1.5" style={{ color: 'var(--silk-dim)' }}>
                    {/* break-all: the workflow path outgrows the sticky card at
                        320px wide; a wrapped path beats one clipped mid-word. */}
                    2. <code className="hb-inline break-all">.github/workflows/hauksbee-ci.yml</code>:
                  </div>
                  {/* pre-wrap: the long `paths:` lines soft-wrap at spaces
                      instead of clipping at the card's right edge (the copied
                      text keeps its real newlines either way). */}
                  <pre className="hb-code p-3 overflow-x-auto whitespace-pre-wrap text-[11px] leading-relaxed">
                    {workflowYml}
                  </pre>
                  <button type="button" className="hb-btn hb-press mt-2 px-3 py-1.5 text-[12px]"
                    onClick={() => download('hauksbee-ci.yml', workflowYml)}>
                    Download hauksbee-ci.yml
                  </button>
                </div>
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
