import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { CheckResult, QueuedRequest, RunResponse, SensorCatalogEntry, WebReport } from '../types/report'
import type { ActionResultMsg } from '../types/protocol'
import type { SelectedComponent } from './SelectionCard'
import { PlusIcon } from './Icons'
import { specStemFor, workflowExportAvailable, workflowYaml } from '../lib/ci-workflow'
import { EmptyState } from '../motion'
import { buildCheckUpload, buildPortableCheckSpec } from '../lib/board-upload'
import { sessionIdFor } from '../lib/session-store'
import { api, errorText } from '../lib/api'
import {
  assertToml, buildToml, CHECK_KINDS, emptyCheck, emptyPeripheral, emptySensor,
  peripheralIssues, peripheralToml, rowIssues, sensorIssues, sensorToml, supplyToml,
} from '../lib/check-spec'
import type {
  BuilderState, CheckRow, PeripheralRow, RowIssue, SensorRow, SupplyRow,
} from '../lib/check-spec'
import { BuilderSection, Field, RawModeSummary, RemoveButton } from './checks/pieces'
import { InteractionBuilder } from './checks/InteractionBuilder'
import { SensorBuilder } from './checks/SensorBuilder'
import { AssertionGroups, hasNoGroups } from './checks/AssertionGroups'
import { SpecPane } from './checks/SpecPane'

// The Checks view: compose the body of a hauksbee-ci spec with plain
// language, run it through the REAL hauksbee-ci binary (`POST /api/check`
// shells the sibling install), and keep the artifact. The spec TOML is a
// persistent synced pane: builder edits regenerate it live, and raw-edit mode
// takes over when the builder's vocabulary runs out (round-tripped back when
// possible). State auto-saves per board (localStorage) and auto-restores when
// the same board is analyzed again.
//
// The spec model itself (rows, the TOML composer, the round-trip parser and
// the per-row preflight) lives in lib/check-spec.ts; the builder's sections
// live in ./checks.

/** What the shell's status chips show after a run. */
export interface ChecksSummary {
  passed: number
  failed: number
  invalid: number
}

/** Shape of the autosaved localStorage payload (all fields best-effort: a
 *  corrupt or partial save must never break the view). */
interface SavedChecksState {
  specName?: string
  duration?: string
  supplies?: SupplyRow[]
  peripherals?: PeripheralRow[]
  sensors?: SensorRow[]
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
  return `hauksbee.checks.${sessionIdFor(report)}`
}

/** Drop one row's entry from a validation map, leaving the map alone when it
 *  has nothing to say about that row. */
function forget<T>(map: Map<number, T>, rowId: number): Map<number, T> {
  if (!map.has(rowId)) return map
  const next = new Map(map)
  next.delete(rowId)
  return next
}

export function ChecksView({
  report,
  boardFile,
  firmwareFile,
  schematicFile,
  selectedNet,
  selectedComponent,
  pending,
  onPendingConsumed,
  liveRegisterMapAvailable = false,
  onAttachRegisterMapLive,
  liveActionResult,
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
  /** What a board surface (report map or live sim) queued: appended here as
   *  ordinary prefilled rows so the spec TOML is exactly what a hand-built
   *  row produces. */
  pending: QueuedRequest[]
  /** Every pending request up to (and including) seq has been applied. */
  onPendingConsumed: (upToSeq: number) => void
  /** True only when the analyzed board owns the current live session. */
  liveRegisterMapAvailable?: boolean
  /** Explicit user action after local row validation; never automatic on
   * paste/select and never invokes a network or LLM model-extraction path. */
  onAttachRegisterMapLive?: (request: {
    id: string
    spec_toml: string
    inputs: Record<string, number>
    controller?: string
    cs_net?: string
  }) => number
  /** Latest engine receipt. It is correlated by request_id before being shown,
   * so editing/re-attaching a reused device id cannot surface stale success. */
  liveActionResult?: ActionResultMsg | null
  /** Latest run's pass/fail counts for the shell's status chips (null when no
   *  current run result exists). */
  onSummary: (s: ChecksSummary | null) => void
  /** The spec text as it currently stands, for the shell's Export menu and the
   *  saved session. Reported rather than recomposed there: the exported spec must
   *  be the same bytes this pane shows and the Download button writes. */
  onSpec: (s: { toml: string; fileName: string } | null) => void
}) {
  const storageKey = checksStorageKey(report)
  // Restore this board's saved session in the state initializers. Because the
  // view remounts per board (keyed on storageKey by the shell) that is
  // race-free: the autosave effect cannot fire before the restore has
  // happened, and a board with no saved state starts from the report's own
  // defaults instead of inheriting the previous board's rows.
  const saved = useMemo<SavedChecksState | null>(() => {
    try {
      const raw = localStorage.getItem(storageKey)
      return raw ? (JSON.parse(raw) as SavedChecksState) : null
    } catch {
      return null
    }
  }, [storageKey])
  const savedArray = <T,>(rows: T[] | undefined): T[] => (Array.isArray(rows) ? rows : [])
  const initialChecks = savedArray(saved?.checks).length
    ? savedArray(saved?.checks)
    : [emptyCheck(1, 'no_faults')]

  const [specName, setSpecName] = useState(saved?.specName || `${report.board_name || report.file_name} checks`)
  const [duration, setDuration] = useState(saved?.duration || '100')
  const [supplies, setSupplies] = useState<SupplyRow[]>(
    () => (savedArray(saved?.supplies).length
      ? savedArray(saved?.supplies)
      : (report.supplies ?? []).map(s => ({ net: s.net, volts: String(s.volts) }))),
  )
  const [peripherals, setPeripherals] = useState<PeripheralRow[]>(() => savedArray(saved?.peripherals))
  const [sensors, setSensors] = useState<SensorRow[]>(() => savedArray(saved?.sensors))
  const [checks, setChecks] = useState<CheckRow[]>(initialChecks)
  const [rawMode, setRawMode] = useState(!!(saved?.rawMode && typeof saved.rawText === 'string'))
  const [rawText, setRawText] = useState(typeof saved?.rawText === 'string' ? saved.rawText : '')
  const [addOpen, setAddOpen] = useState(false)
  const [running, setRunning] = useState(false)
  // The last run's response, plus the exact spec text it ran, so results can
  // be flagged stale the moment the spec diverges.
  const [run, setRun] = useState<{ response: RunResponse; toml: string } | null>(null)
  // Builder-mode preflight results, per row. Set when a run is attempted with
  // holes; a row's entry clears the moment that row is edited so the highlight
  // never nags about fixed input.
  const [validation, setValidation] = useState<Map<number, RowIssue[]>>(new Map())
  const [peripheralValidation, setPeripheralValidation] = useState<Map<number, string[]>>(new Map())
  const [sensorValidation, setSensorValidation] = useState<Map<number, string[]>>(new Map())
  const [sensorLiveRequests, setSensorLiveRequests] = useState<Map<number, number>>(new Map())
  const [sensorCatalog, setSensorCatalog] = useState<SensorCatalogEntry[]>([])
  const [sensorCatalogError, setSensorCatalogError] = useState<string | null>(null)

  const nextId = useRef(initialChecks.reduce((m, c) => Math.max(m, c.id), 0) + 1)
  const nextPeripheralId = useRef(
    savedArray(saved?.peripherals).reduce((m, p) => Math.max(m, p.rowId ?? 0), 0) + 1,
  )
  const nextSensorId = useRef(
    savedArray(saved?.sensors).reduce((m, s) => Math.max(m, s.rowId ?? 0), 0) + 1,
  )
  const nextSensorInputId = useRef(
    savedArray(saved?.sensors).flatMap(s => s.inputs ?? [])
      .reduce((m, input) => Math.max(m, input.rowId ?? 0), 0) + 1,
  )

  useEffect(() => {
    const abort = new AbortController()
    void api.sensorSpecs(abort.signal)
      .then(value => {
        if (!Array.isArray(value.entries)) throw new Error('sensor catalog has no entries')
        setSensorCatalog(value.entries)
        setSensorCatalogError(null)
      })
      .catch(error => {
        if (abort.signal.aborted) return
        setSensorCatalogError(errorText(error))
      })
    return () => abort.abort()
  }, [])

  const builtToml = useMemo(
    () => buildToml(specName, duration, supplies, peripherals, checks, sensors),
    [specName, duration, supplies, peripherals, checks, sensors],
  )
  const effectiveToml = rawMode ? rawText : builtToml
  const stale = run !== null && run.toml !== effectiveToml

  // Auto-save (the "things auto load in future" contract).
  useEffect(() => {
    try {
      localStorage.setItem(storageKey, JSON.stringify({ specName, duration, supplies, peripherals, sensors, checks, rawMode, rawText }))
    } catch { /* storage full/blocked: the session still works */ }
  }, [storageKey, specName, duration, supplies, peripherals, sensors, checks, rawMode, rawText])

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

  const updateCheck = (id: number, patch: Partial<CheckRow>) => {
    setChecks(cs => cs.map(c => (c.id === id ? { ...c, ...patch } : c)))
    setValidation(prev => forget(prev, id))
  }

  const addPeripheral = (kind: PeripheralRow['kind'], net = '') => {
    setPeripherals(rows => [...rows, emptyPeripheral(nextPeripheralId.current++, kind, net)])
  }

  const updatePeripheral = (rowId: number, patch: Partial<PeripheralRow>) => {
    setPeripherals(rows => rows.map(row => row.rowId === rowId ? { ...row, ...patch } : row))
    setPeripheralValidation(prev => forget(prev, rowId))
  }

  const addSensor = (id = '', componentRef = '', modelId = '') => {
    setSensors(rows => [...rows, emptySensor(nextSensorId.current++, id, componentRef, modelId)])
  }

  const updateSensor = (rowId: number, patch: Partial<SensorRow>) => {
    setSensors(rows => rows.map(row => row.rowId === rowId ? { ...row, ...patch } : row))
    // Any earlier engine receipt describes the bytes before this edit.
    setSensorLiveRequests(prev => forget(prev, rowId))
    setSensorValidation(prev => forget(prev, rowId))
  }

  const attachSensorLive = (sensor: SensorRow) => {
    const issues = sensorIssues(sensor)
    if (issues.length > 0) {
      setSensorValidation(previous => new Map(previous).set(sensor.rowId, issues))
      return
    }
    const inputs: Record<string, number> = {}
    for (const input of sensor.inputs) inputs[input.name.trim()] = Number(input.value)
    const requestId = onAttachRegisterMapLive?.({
      id: sensor.id.trim(),
      spec_toml: sensor.spec,
      inputs,
      controller: sensor.controller.trim() || undefined,
      cs_net: sensor.csNet.trim() || undefined,
    })
    if (requestId !== undefined) {
      setSensorLiveRequests(previous => new Map(previous).set(sensor.rowId, requestId))
    }
  }

  // Consume what a board surface queued (a net/component click on the report
  // map or in the live sim). Each request becomes the SAME row the builder
  // would make by hand; builder mode appends the row, raw mode appends that
  // row's TOML, so nothing queued is ever silently dropped and the two modes
  // cannot compose it differently. A register-map device is never guessed
  // from the part name: the row opens named, and the user supplies the exact
  // spec bytes before Run becomes available (raw mode keeps the same
  // incomplete row as an explicit TODO).
  useEffect(() => {
    if (pending.length === 0) return
    const checkRows: CheckRow[] = [], peripheralRows: PeripheralRow[] = []
    const sensorRows: SensorRow[] = [], supplyRows: SupplyRow[] = []
    let raw = ''
    for (const r of pending) {
      if (r.type === 'check') {
        const row = emptyCheck(nextId.current++, r.kind, r.net ?? '')
        row.ref = r.ref ?? ''
        checkRows.push(row)
        raw += assertToml(row)
      } else if (r.type === 'peripheral') {
        const row = emptyPeripheral(nextPeripheralId.current++, r.kind, r.net ?? '')
        if (r.id) row.id = r.id
        peripheralRows.push(row)
        raw += peripheralToml(row)
      } else if (r.type === 'sensor') {
        const row = emptySensor(nextSensorId.current++, r.id, r.ref ?? '', r.modelId ?? '')
        sensorRows.push(row)
        raw += `\n# TODO: paste a validated register-map spec for ${r.ref ?? r.id}${sensorToml(row)}`
      } else {
        const row = { net: r.net, volts: String(r.volts ?? 3.3) }
        supplyRows.push(row)
        raw += supplyToml(row)
      }
    }
    if (rawMode) setRawText(previous => previous + raw)
    else {
      if (checkRows.length) setChecks(cs => [...cs, ...checkRows])
      if (peripheralRows.length) setPeripherals(rows => [...rows, ...peripheralRows])
      if (sensorRows.length) setSensors(rows => [...rows, ...sensorRows])
      if (supplyRows.length) setSupplies(rows => [...rows, ...supplyRows])
    }
    onPendingConsumed(pending[pending.length - 1].seq)
  }, [pending, rawMode, onPendingConsumed])

  const runChecks = useCallback(async () => {
    if (!boardFile) return
    // Preflight the builder rows in the builder's own vocabulary before
    // anything is POSTed: the server's TOML-keyed errors ("needs `ref` and
    // `amps`") belong to raw mode, and they also over-name fields that are
    // actually present.
    if (!rawMode) {
      const collect = <T,>(rows: T[], key: (row: T) => number, issuesOf: (row: T) => string[]) => {
        const problems = new Map<number, string[]>()
        for (const row of rows) {
          const issues = issuesOf(row)
          if (issues.length > 0) problems.set(key(row), issues)
        }
        return problems
      }
      const peripheralProblems = collect(peripherals, p => p.rowId, peripheralIssues)
      const sensorProblems = collect(sensors, s => s.rowId, sensorIssues)
      const problems = new Map<number, RowIssue[]>()
      for (const c of checks) {
        const issues = rowIssues(c)
        if (issues.length > 0) problems.set(c.id, issues)
      }
      setPeripheralValidation(peripheralProblems)
      setSensorValidation(sensorProblems)
      setValidation(problems)
      if (problems.size > 0 || peripheralProblems.size > 0 || sensorProblems.size > 0) return
    }
    setRunning(true)
    const tomlAtRun = effectiveToml
    const fail = (error: string) => setRun({ response: { ok: false, error }, toml: tomlAtRun })
    try {
      const response = await api.check(buildCheckUpload(boardFile, firmwareFile, schematicFile, tomlAtRun))
      setRun({ response, toml: tomlAtRun })
    } catch (e) {
      fail(errorText(e))
    } finally {
      setRunning(false)
    }
  }, [boardFile, firmwareFile, schematicFile, effectiveToml, rawMode, checks, peripherals, sensors])

  const specStem = specStemFor(report.file_name)
  const specFileName = `${specStem}.toml`
  // The runnable spec: the composed body with the board/firmware paths the
  // recommended repo layout puts them at. The builder composes a fragment (it
  // never writes board/firmware lines), but a raw spec may be a full file that
  // already names its paths; a key is prepended only when the spec text does not
  // already carry it, so nothing is doubled.
  //
  // Memoized because it is THE spec: the pane renders it, the Download button
  // writes it, the Export menu offers it and the saved session stores it. Four
  // readers of a per-render function are four chances to disagree.
  const specText = useMemo(
    () => buildPortableCheckSpec(report.file_name, firmwareFile, schematicFile, effectiveToml),
    [effectiveToml, firmwareFile, schematicFile, report.file_name],
  )

  // Hand the spec up to the shell, so the Export menu and the saved session
  // carry the same bytes this pane shows rather than a second composition of
  // them. This pane owns the lifetime of that value, INCLUDING clearing it on
  // the way out: a restored session remounts this pane in the same commit the
  // shell resets its run state, and only the child knows which order that
  // leaves the spec in.
  useEffect(() => {
    onSpec({ toml: specText, fileName: specFileName })
  }, [specText, specFileName, onSpec])
  useEffect(() => () => onSpec(null), [onSpec])

  /** Return from raw-edit mode. A parsed spec replaces the builder rows
   *  wholesale (an intentionally emptied list clears them too); null means the
   *  user chose to discard raw text the builder cannot represent. */
  const applyRawSpec = (parsed: BuilderState | null) => {
    if (parsed) {
      setSpecName(parsed.name)
      setDuration(parsed.duration)
      setSupplies(parsed.supplies)
      setPeripherals(parsed.peripherals)
      setSensors(parsed.sensors)
      setChecks(parsed.checks)
      nextId.current = parsed.checks.reduce((m, c) => Math.max(m, c.id), 0) + 1
      nextPeripheralId.current = parsed.peripherals.reduce((m, p) => Math.max(m, p.rowId), 0) + 1
      nextSensorId.current = parsed.sensors.reduce((m, s) => Math.max(m, s.rowId), 0) + 1
      nextSensorInputId.current = parsed.sensors.flatMap(s => s.inputs)
        .reduce((m, input) => Math.max(m, input.rowId), 0) + 1
    }
    setRawMode(false)
  }

  const workflowYml = workflowExportAvailable ? workflowYaml(specStem) : null

  const result = run?.response ?? null
  const results = result?.ok ? result.results ?? [] : []

  // Row -> result: results come back in [[assert]] order, which is the
  // `checks` array order (buildToml writes them in sequence). Grouping in the
  // display reorders only what is shown.
  const resultForRow = (id: number): CheckResult | null => {
    if (rawMode || results.length === 0) return null
    const i = checks.findIndex(c => c.id === id)
    return i >= 0 && i < results.length ? results[i] : null
  }

  /** The copper offer chips above the builder, from the board selection. */
  const quickAdd = (testid: string, label: React.ReactNode, onClick: () => void) => (
    <button
      key={testid}
      type="button"
      data-testid={testid}
      onClick={onClick}
      className="hb-chip hb-press px-3 py-1.5 text-[12px]"
    >
      {label}
    </button>
  )

  return (
    <div className="h-full overflow-y-auto view-enter" data-testid="checks-panel">
      <div className="max-w-6xl mx-auto px-6 pt-5 pb-16">
        <div className="text-[13px] leading-relaxed mb-4" style={{ color: 'var(--silk-dim)', maxWidth: '46rem' }}>
          Pick what must hold (a rail voltage, a blink, a print, nothing over-stressed), run it
          here, then take the spec file with you; the same file <code className="hb-inline">hauksbee-ci</code>{' '}
          runs in a pipeline. Click a trace on the Board view to start a check on that net.
        </div>

        {/* The one net picker every field in this view completes from. */}
        <datalist id="net-options">
          {(report.nets ?? []).map(n => <option key={n} value={n} />)}
        </datalist>

        {/* Two columns where both fit, one stacked column below 1024px. The
            track sizes and the stacking live on `.checks-grid` in index.css,
            next to the rest of this surface's box discipline. */}
        <div className="checks-grid gap-5">
          {/* ── Left: the builder ── */}
          <div className="min-w-0 checks-flush">
            {selectedNet && !rawMode && (
              <div className="mb-3 flex flex-wrap gap-2">
                {quickAdd('quick-add-net', <>+ Check a voltage on “{selectedNet}”</>, () => addCheck('voltage', selectedNet))}
                {quickAdd('quick-add-net-stimulus', <>+ Drive “{selectedNet}” with a waveform</>, () => addPeripheral('stimulus', selectedNet))}
                {quickAdd('quick-add-net-button', <>+ Put a button on “{selectedNet}”</>, () => addPeripheral('pushbutton', selectedNet))}
                {quickAdd('quick-add-net-supply', <>+ Power “{selectedNet}” from a supply</>,
                  () => setSupplies(rows => [...rows, { net: selectedNet, volts: '3.3' }]))}
              </div>
            )}
            {selectedComponent && !rawMode && (
              <div className="mb-3 flex flex-wrap gap-2">
                {quickAdd('quick-add-ref-current',
                  <>+ “{selectedComponent.ref}” must stay under a current (clicked on the map)</>,
                  () => addCheck('max_current', '', selectedComponent.ref))}
                {quickAdd('quick-add-ref-temp', <>+ “{selectedComponent.ref}” must stay cool</>,
                  () => addCheck('max_temp', '', selectedComponent.ref))}
                {quickAdd('quick-add-ref-sensor',
                  <>+ Attach register-map behavior to “{selectedComponent.ref}”</>,
                  () => addSensor(selectedComponent.ref, selectedComponent.ref))}
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

                {/* Power. The net picker gives up width first (it is the one
                    field with slack), then the row wraps; `remove` is last in
                    reading order but never last in line, so it stays inside
                    the card at every width. */}
                <BuilderSection
                  title="Power supplies"
                  actions={<span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>detected from the board, adjust if wrong</span>}
                >
                  {supplies.map((s, i) => {
                    const patch = (next: Partial<SupplyRow>) =>
                      setSupplies(ss => ss.map((x, j) => (j === i ? { ...x, ...next } : x)))
                    return (
                      <div key={i} className="flex flex-wrap items-center gap-x-2 gap-y-1.5 mb-1.5">
                        <input
                          className="hb-input min-w-0 flex-1"
                          style={{ maxWidth: 180 }}
                          list="net-options"
                          value={s.net}
                          placeholder="net (e.g. +5V)"
                          onChange={e => patch({ net: e.target.value })}
                        />
                        <Field label="volts" value={s.volts} width={70} onChange={volts => patch({ volts })} />
                        <RemoveButton
                          className="text-[12px] shrink-0"
                          onClick={() => setSupplies(ss => ss.filter((_, j) => j !== i))}
                        />
                      </div>
                    )
                  })}
                  <button
                    type="button"
                    className="hb-press text-[12px] cursor-pointer inline-flex items-center gap-1"
                    style={{ color: 'var(--silk-dim)', background: 'none', border: 'none' }}
                    onClick={() => setSupplies(ss => [...ss, { net: '', volts: '5.0' }])}
                  >
                    <PlusIcon size={12} /> add a supply
                  </button>
                </BuilderSection>

                <InteractionBuilder
                  peripherals={peripherals}
                  validation={peripheralValidation}
                  onAdd={kind => addPeripheral(kind)}
                  onUpdate={updatePeripheral}
                  onRemove={rowId => setPeripherals(rows => rows.filter(row => row.rowId !== rowId))}
                />

                <SensorBuilder
                  sensors={sensors}
                  validation={sensorValidation}
                  catalog={sensorCatalog}
                  catalogError={sensorCatalogError}
                  liveRequests={sensorLiveRequests}
                  liveActionResult={liveActionResult}
                  liveAttachAvailable={liveRegisterMapAvailable && !!onAttachRegisterMapLive}
                  nextInputId={() => nextSensorInputId.current++}
                  onAdd={() => addSensor()}
                  onUpdate={updateSensor}
                  onRemove={rowId => setSensors(rows => rows.filter(row => row.rowId !== rowId))}
                  onAttachLive={attachSensorLive}
                />

                <AssertionGroups
                  checks={checks}
                  validation={validation}
                  stale={stale}
                  resultForRow={resultForRow}
                  assumptions={run?.response.assumptions ?? []}
                  inventory={run?.response.inventory ?? []}
                  onAdd={addCheck}
                  onUpdate={updateCheck}
                  onRemove={id => setChecks(cs => cs.filter(x => x.id !== id))}
                />

                {/* Every check removed. A blank area is a question the
                    interface asked and then refused to answer: say what is
                    missing, what happens if it stays missing (the run is
                    refused, not silently empty), and the one check worth
                    starting from. */}
                {hasNoGroups(checks) && (
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
                  <button
                    type="button"
                    data-testid="add-check"
                    className="hb-btn hb-press px-3 py-1.5 text-[13px] inline-flex items-center gap-1.5"
                    onClick={() => setAddOpen(o => !o)}
                  >
                    <PlusIcon size={13} /> Add a check
                  </button>
                  {addOpen && (
                    <div className="hb-card view-enter absolute z-10 mt-1 overflow-hidden" style={{ width: 400, maxWidth: '100%', boxShadow: 'var(--shadow-pop)' }}>
                      {CHECK_KINDS.map(k => (
                        <button
                          key={k.kind}
                          type="button"
                          onClick={() => addCheck(k.kind)}
                          className="hb-press block w-full text-left px-3 py-2 cursor-pointer"
                          style={{ background: 'none', border: 'none' }}
                          onMouseEnter={e => { e.currentTarget.style.background = 'var(--copper-tint)' }}
                          onMouseLeave={e => { e.currentTarget.style.background = 'none' }}
                        >
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

          <SpecPane
            specText={specText}
            specStem={specStem}
            specFileName={specFileName}
            builtToml={builtToml}
            rawMode={rawMode}
            rawText={rawText}
            setRawText={setRawText}
            onEnterRawMode={() => setRawMode(true)}
            onLeaveRawMode={applyRawSpec}
            stale={stale}
            running={running}
            canRun={!!boardFile}
            onRun={() => void runChecks()}
            validationCount={validation.size}
            result={result}
            runKey={run?.toml}
            workflowYml={workflowYml}
          />
        </div>
      </div>
    </div>
  )
}
