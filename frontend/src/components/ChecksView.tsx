import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { CheckResult, QueuedLiveRegisterMap, QueuedRequest, Remedy, RunResponse, SensorCatalogEntry, WebReport } from '../types/report'
import type { ActionResultMsg } from '../types/protocol'
import { PlusIcon } from './Icons'
import { specStemFor, workflowExportAvailable, workflowPreviewYaml, workflowYaml } from '../lib/ci-workflow'
import { EmptyState } from '../motion'
import { buildCheckUpload, buildPortableCheckSpec } from '../lib/board-upload'
import type { SupplementalDesignFiles } from '../lib/board-upload'
import { checksStorageKey } from '../lib/checks-storage'
import { api, errorText, type LoadProgress } from '../lib/api'
import {
  assertToml, buildToml, componentCapabilityIssues, CONSTRAINT_KEYS, emptyCheck, emptyPeripheral,
  emptySensor, normalizeSavedCheck, overrideToml, peripheralIssues, peripheralToml, railPolarity, rowIssues,
  sensorIssues, sensorToml, scaffoldRows, supplyKvs, supplyToml, SUPPLY_KINDS, USB_PROFILES,
} from '../lib/check-spec'
import type { CheckRow, OverrideRow, PeripheralRow, RowIssue, SensorRow, SupplyRow } from '../lib/check-spec'
import {
  appendOp, checkOps, overrideOps, peripheralOps, readSpec, rootOps, sensorOps, supplyOps,
} from '../lib/spec-model'
import type { SpecModel } from '../lib/spec-model'
import { applyOps, readCst } from '../lib/toml-cst'
import type { PatchOp } from '../lib/toml-cst'
import { assertRowCount, coverageFor, runMarkdown, runView } from '../lib/report-view'
import { downloadText } from '../lib/report-export'
import { labelFor } from '../lib/labels'
import { useCatalog } from '../lib/check-catalog'
import { STATIC_CHECK_KIND, statusOf, type RunStatus } from '../lib/status-vocab'
import { ToBoard } from './checks/ChecksLinks'
import { BuilderSection, Field, NetField, RemoveButton } from './checks/pieces'
import { InteractionBuilder } from './checks/InteractionBuilder'
import { SensorBuilder } from './checks/SensorBuilder'
import { AssertionGroups, hasNoGroups } from './checks/AssertionGroups'
import { SpecPane, type SpecMode } from './checks/SpecPane'
import { AddCheckMenu } from './checks/AddCheckMenu'
import type { RemedyActions } from './checks/Remedies'
import type { GutterResult } from './editor/TomlEditor'
import { WritePart } from './WritePart'

/** The spec body that asks `/api/ci/scaffold` for the starter spec rather than
 *  a run; the server does not read it as TOML. */
const SCAFFOLD_SPEC = '# hauksbee:scaffold'

/** What a run the person cancelled says: nothing was judged, and how to go on. */
const RUN_CANCELLED =
  'The run was cancelled, so nothing was judged. Run the checks again when you are ready.'

const MODE_KEY = 'hauksbee.checks.mode'

// The Checks view: compose the body of a hauksbee-ci spec, run it through the
// REAL hauksbee-ci binary (`POST /api/check` shells the sibling install), and
// keep the artifact. The spec TEXT is the one source of truth: the builder's
// rows are read from it, every builder edit is a patch on the exact span it is
// about (lib/toml-cst.ts), and the TOML pane edits the same text. Comments, key
// order and anything the builder does not cover stay as written. The text
// auto-saves per board (localStorage) and restores when the same board is
// analyzed again.

/** What the shell's status chips show after a run. */
export interface ChecksSummary {
  passed: number
  failed: number
  invalid: number
  /** Reported without a bound: counted apart, because nothing judged them. */
  measured?: number
}

/** Shape of the autosaved localStorage payload (all fields best-effort: a
 *  corrupt or partial save must never break the view). Saves from before the
 *  text was the source of truth carry rows instead; they are composed once. */
interface SavedChecksState {
  specText?: string
  specName?: string
  duration?: string
  supplies?: SupplyRow[]
  peripherals?: PeripheralRow[]
  sensors?: SensorRow[]
  overrides?: OverrideRow[]
  noFit?: string[]
  checks?: CheckRow[]
  rawMode?: boolean
  rawText?: string
  includeFirmware?: boolean
}

/** Stable enough identity for a staged browser File during one session. A
 *  same-name replacement must stale a previous run even when the slot stays
 *  filled; the bytes themselves are authenticated by the upload path when the
 *  next run is sent. */
function fileIdentity(file: File | null): string | null {
  return file ? `${file.name}\u0000${file.size}\u0000${file.lastModified}` : null
}

function supplementalIdentity(files: SupplementalDesignFiles): string {
  return [files.bom, files.placement, files.variant, files.asbuilt, ...files.models]
    .map(fileIdentity)
    .join('\u0001')
}

/** The firmware decision is part of the run contract, not a builder-only
 *  setting. Keep it visible next to the builder, so a non-AVR build can always
 *  choose the static-only path before pressing Run. */
function FirmwareRunChoice({ firmware, include, avrAvailable, running, onChange }: {
  firmware: File
  include: boolean
  avrAvailable: boolean
  running: boolean
  onChange: (include: boolean) => void
}) {
  return (
    <section
      className="hb-card px-4 py-3.5 mb-4"
      data-testid="firmware-run-choice"
      style={{ borderColor: !avrAvailable && include ? 'var(--warn-border)' : 'var(--hairline)' }}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <label className="inline-flex items-center gap-2 text-[12px] cursor-pointer" style={{ color: 'var(--silk)' }}>
          <input
            data-testid="include-staged-firmware"
            type="checkbox"
            checked={include}
            disabled={running}
            onChange={event => onChange(event.currentTarget.checked)}
          />
          <span>{include ? 'Include staged firmware in this run' : 'Run checks without firmware'}</span>
        </label>
        <span className="text-[11px]" style={{ color: include ? 'var(--copper-hi)' : 'var(--silk-faint)' }}>
          {include ? 'firmware co-simulation requested' : 'static board checks only'}
        </span>
      </div>
      <p className="mt-1.5 text-[11px] leading-relaxed" style={{ color: !avrAvailable ? 'var(--warn-strong)' : 'var(--silk-dim)' }}>
        <span style={{ fontFamily: 'var(--font-mono)' }}>{firmware.name}</span> is staged. Supply, voltage, and button checks do not need an MCU backend. UART, blink,
        boot, and any combined firmware co-simulation do; this build reports that backend as {!avrAvailable ? 'unavailable' : 'available'}.
        {!avrAvailable && include && ' Turn the choice off to run the static checks without firmware.'}
      </p>
    </section>
  )
}

export { checksStorageKey }

/** Drop one row's entry from a validation map, leaving the map alone when it
 *  has nothing to say about that row. */
function forget<T>(map: Map<number, T>, rowId: number): Map<number, T> {
  if (!map.has(rowId)) return map
  const next = new Map(map)
  next.delete(rowId)
  return next
}

const EMPTY_MODEL: SpecModel = {
  name: '', duration: '', noFit: [], supplies: [], peripherals: [], sensors: [], checks: [], overrides: [], kept: [],
}

/** A row as typed but not (yet) as the text reads it back: a number half
 *  typed, `5.0` that reads back as `5`, a mode switch with nothing filled. The
 *  draft shows until the text under it changes. */
interface Draft { base: string; row: unknown }

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b)

/** A fresh id like `STIM3` that the spec does not use yet. */
function freshId(prefix: string, used: readonly string[], from: number): string {
  let n = Math.max(1, from)
  while (used.includes(`${prefix}${n}`)) n++
  return `${prefix}${n}`
}

export function ChecksView({
  report,
  boardFile,
  firmwareFile,
  schematicFile,
  supplementalFiles,
  avrAvailable = true,
  pending,
  onPendingConsumed,
  liveRegisterMapAvailable = false,
  onAttachRegisterMapLive,
  liveActionResult,
  onSummary,
  onSpec,
  onModelSaved,
}: {
  report: WebReport
  boardFile: File | null
  firmwareFile: File | null
  schematicFile: File | null
  /** The manufacturing and as-built inputs staged with the board. */
  supplementalFiles: SupplementalDesignFiles
  /** Whether this build has the MCU backend needed for firmware co-simulation. */
  avrAvailable?: boolean
  /** What a board surface (report map or live sim) queued: appended to the
   *  spec text exactly as a hand-built row writes it. */
  pending: QueuedRequest[]
  /** Every pending request up to (and including) seq has been applied. */
  onPendingConsumed: (upToSeq: number) => void
  /** True only when the analyzed board owns the current live session. */
  liveRegisterMapAvailable?: boolean
  /** Explicit user action after local row validation; never automatic on
   * paste/select and never invokes a network or LLM model-extraction path. */
  onAttachRegisterMapLive?: (request: Omit<QueuedLiveRegisterMap, 'seq'>) => number
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
  /** A model written from a "write a model" remedy was saved: the shell
   *  re-analyzes the board so the next run sees it. */
  onModelSaved?: () => void
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
  const savedText = typeof saved?.specText === 'string'
    ? saved.specText
    : saved?.rawMode && typeof saved.rawText === 'string' ? saved.rawText : null
  // What the first render seeded, so the scaffold below replaces it only if
  // nobody has edited the spec while it was on its way.
  const seed = useRef<{ name: string; duration: string; checks: CheckRow[]; text: string } | null>(null)
  const [specText, setSpecTextState] = useState<string>(() => {
    if (savedText !== null) return savedText
    const savedArray = <T,>(rows: T[] | undefined): T[] => (Array.isArray(rows) ? rows : [])
    // A save from before the text was the source of truth: compose it once.
    // Rows from earlier releases are repaired (unknown kinds dropped) first.
    const checks = savedArray(saved?.checks)
      .map((check, index) => normalizeSavedCheck(check, index + 1))
      .filter((check): check is CheckRow => check !== null)
    const supplies = savedArray(saved?.supplies).length
      ? savedArray(saved?.supplies)
      : (report.supplies ?? []).map(s => ({ net: s.net, volts: String(s.volts) }))
    const name = saved?.specName || `${report.board_name || report.file_name} checks`
    const duration = saved?.duration || '100'
    const startChecks = checks.length ? checks : [emptyCheck(1, 'no_faults')]
    const text = buildToml(
      name, duration, supplies, savedArray(saved?.peripherals), startChecks,
      savedArray(saved?.sensors), savedArray(saved?.overrides), savedArray(saved?.noFit),
    )
    seed.current = { name, duration, checks: startChecks, text }
    return text
  })
  // The latest text, for edits made between renders (a builder event computes
  // its patch against this, never against a stale closure).
  const textRef = useRef(specText)
  const setSpecText = useCallback((text: string) => {
    textRef.current = text
    setSpecTextState(text)
  }, [])

  // A board with no saved spec starts from `hauksbee-ci init`'s own supply list
  // (rails, plus the input of any regulator nothing else powers). Run waits for
  // it, so the first run is the one the CLI would make.
  const [scaffolding, setScaffolding] = useState(
    () => !!boardFile && savedText === null && !(Array.isArray(saved?.supplies) && saved.supplies.length > 0),
  )
  // Remedy state: the model editor opened for a part, and the Run button's pulse.
  const [modelFor, setModelFor] = useState<{ reference: string; signal: number } | null>(null)
  const [pulseRun, setPulseRun] = useState(false)
  // Firmware is staged on the board session, but including it in a check run
  // is an explicit choice. Preserve the old default on AVR-capable builds;
  // permissive/non-AVR builds start on the static-only path.
  const [includeFirmware, setIncludeFirmware] = useState(
    () => !!firmwareFile && avrAvailable && (saved?.includeFirmware ?? true),
  )
  const previousFirmware = useRef<File | null>(firmwareFile)
  const [addOpen, setAddOpen] = useState(false)
  // One working surface at a time: the builder or the spec TOML. The choice is
  // remembered across visits (a per-browser convenience, never required).
  const [mode, setModeState] = useState<SpecMode>(() => {
    try { return localStorage.getItem(MODE_KEY) === 'toml' ? 'toml' : 'builder' } catch { return 'builder' }
  })
  const setMode = (next: SpecMode) => {
    setModeState(next)
    try { localStorage.setItem(MODE_KEY, next) } catch { /* private window: the choice just is not remembered */ }
  }
  const [select, setSelect] = useState<{ start: number; end: number; seq: number } | null>(null)
  const [running, setRunning] = useState(false)
  // How far the engine says the work in hand (the preparation, then the run) has
  // got, and the handles that stop it. The engine's report is the only source:
  // nothing here is a guess at a duration.
  const [progress, setProgress] = useState<LoadProgress | null>(null)
  const runAbort = useRef<AbortController | null>(null)
  const scaffoldAbort = useRef<AbortController | null>(null)
  const mounted = useRef(true)
  // Leaving the view ends a run nobody is waiting for: the server stops its work
  // when the request closes.
  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
      runAbort.current?.abort()
    }
  }, [])
  // Cancel ends the run in hand; before the run it skips the preparation, and
  // the first run then starts from the supplies the board's own report found.
  const cancelWork = useCallback(() => {
    if (runAbort.current) {
      runAbort.current.abort()
      return
    }
    scaffoldAbort.current?.abort()
    setScaffolding(false)
    setProgress(null)
  }, [])
  // The last run's response, plus the exact spec text it ran, so results can
  // be flagged stale the moment the spec diverges.
  const [run, setRun] = useState<{
    response: RunResponse
    toml: string
    firmwareIdentity: string | null
    boardIdentity: string | null
    schematicIdentity: string | null
    supplementalIdentity: string
  } | null>(null)
  // Builder preflight results, per row. Set when a run is attempted with
  // holes; a row's entry clears the moment that row is edited so the highlight
  // never nags about fixed input.
  const [validation, setValidation] = useState<Map<number, RowIssue[]>>(new Map())
  const [peripheralValidation, setPeripheralValidation] = useState<Map<number, string[]>>(new Map())
  const [sensorValidation, setSensorValidation] = useState<Map<number, string[]>>(new Map())
  const [sensorLiveRequests, setSensorLiveRequests] = useState<Map<number, number>>(new Map())
  const [sensorCatalog, setSensorCatalog] = useState<SensorCatalogEntry[]>([])
  const [sensorCatalogError, setSensorCatalogError] = useState<string | null>(null)
  const [drafts, setDrafts] = useState<Map<string, Draft>>(new Map())
  // Sensor input rows are a UI list (an unnamed input is not in the text yet).
  const nextSensorInputId = useRef(1000)
  const nextGenerated = useRef(1)

  // The builder's view of the text. While the text does not parse (someone is
  // mid-edit in the TOML pane) the builder shows the last spec that did, paused.
  const parsed = useMemo(() => readSpec(specText), [specText])
  const lastGood = useRef<SpecModel>(parsed ?? EMPTY_MODEL)
  if (parsed) lastGood.current = parsed
  const model = parsed ?? lastGood.current
  const parseError = useMemo(() => {
    if (parsed) return null
    const read = readCst(specText)
    return read.ok ? null : read.error
  }, [parsed, specText])

  const withDraft = <T,>(key: string, derived: T): T => {
    const d = drafts.get(key)
    return d && d.base === JSON.stringify(derived) ? (d.row as T) : derived
  }
  const checks = model.checks.map(c => withDraft(`assert:${c.id}`, c))
  const supplies = model.supplies.map(s => ({ ...s, row: withDraft(`supply:${s.index}`, s.row) }))
  const peripherals = model.peripherals.map(p => withDraft(`peripheral:${p.rowId}`, p))
  const sensors = model.sensors.map(s => withDraft(`sensor:${s.rowId}`, s))
  const root = withDraft('root', { name: model.name, duration: model.duration, noFit: model.noFit })

  /** Apply ops built from the current text's model. */
  const edit = useCallback((ops: (m: SpecModel) => PatchOp[]) => {
    const text = textRef.current
    const m = readSpec(text)
    if (!m) return
    const next = applyOps(text, ops(m))
    if (next !== text) setSpecText(next)
  }, [setSpecText])

  /** One builder row edited: patch the text, and keep what was typed showing
   *  when the text reads it back differently. */
  function updateRow<T>(
    key: string, find: (m: SpecModel) => T | undefined, opsFor: (derived: T, next: T) => PatchOp[], patch: Partial<T>,
  ) {
    const text = textRef.current
    const m = readSpec(text)
    const derived = m ? find(m) : undefined
    if (!derived) return
    const next = { ...withDraft(key, derived), ...patch } as T
    const after = applyOps(text, opsFor(derived, next))
    if (after !== text) setSpecText(after)
    const reread = readSpec(after)
    const back = reread ? find(reread) : undefined
    setDrafts(prev => {
      const out = new Map(prev)
      if (back !== undefined && !same(back, next)) out.set(key, { base: JSON.stringify(back), row: next })
      else out.delete(key)
      return out
    })
  }

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

  // Mount only: the view remounts per board, and the scaffold is that board's.
  useEffect(() => {
    if (!scaffolding || !boardFile) return
    const abort = new AbortController()
    scaffoldAbort.current = abort
    void api.scaffold(
      buildCheckUpload(boardFile, null, schematicFile, SCAFFOLD_SPEC, supplementalFiles),
      abort.signal,
      p => { if (!abort.signal.aborted) setProgress(p) },
    )
      .then(answer => {
        const s = seed.current
        if (!answer.ok || !answer.supplies?.length || !s) return
        if (textRef.current !== s.text) return // edited meanwhile: the edit wins
        setSpecText(buildToml(s.name, s.duration, scaffoldRows(answer.supplies), [], s.checks))
      })
      .catch(() => undefined) // the report's own rails stay: a scaffold is a convenience
      .finally(() => {
        if (abort.signal.aborted) return
        setScaffolding(false)
        setProgress(null)
      })
    return () => abort.abort()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const firmwareForRun = includeFirmware ? firmwareFile : null
  // A result is only about the spec AND the exact files it ran against.
  const stale = run !== null && (
    run.toml !== specText
    || run.firmwareIdentity !== fileIdentity(firmwareForRun)
    || run.boardIdentity !== fileIdentity(boardFile)
    || run.schematicIdentity !== fileIdentity(schematicFile)
    || run.supplementalIdentity !== supplementalIdentity(supplementalFiles)
  )

  // A newly staged image gets the build's safe default. Removing it always
  // turns the choice off, so an old firmware-backed result cannot look current.
  useEffect(() => {
    if (previousFirmware.current !== firmwareFile) {
      previousFirmware.current = firmwareFile
      setIncludeFirmware(!!firmwareFile && avrAvailable)
    } else if (!firmwareFile) {
      setIncludeFirmware(false)
    }
  }, [firmwareFile, avrAvailable])

  // Auto-save (the "things auto load in future" contract): the text is the spec.
  useEffect(() => {
    try {
      localStorage.setItem(storageKey, JSON.stringify({ specText, includeFirmware }))
    } catch { /* storage full/blocked: the session still works */ }
  }, [storageKey, specText, includeFirmware])

  // Report the run summary to the shell's chips; a stale or failed run
  // reports nothing rather than a number that no longer matches the spec.
  useEffect(() => {
    // What the page judges: the spec's rows and the board findings that fail
    // the circuit. The not-routed state is layout readiness, which the board's
    // own verdict already carries, so it is not counted a second time here.
    const results = run?.response.ok ? runView(run.response, run.toml).judged : []
    if (!run || stale || !run.response.ok || results.length === 0) {
      onSummary(null)
      return
    }
    onSummary({
      passed: results.filter(x => !x.invalid && !x.measured && x.passed).length,
      measured: results.filter(x => !x.invalid && x.measured).length,
      failed: results.filter(x => !x.invalid && !x.passed).length,
      invalid: results.filter(x => x.invalid).length,
    })
  }, [run, stale, onSummary])

  const addCheck = (kind: string, net = '', ref = '') => {
    const row = emptyCheck(0, kind, net)
    row.ref = ref
    edit(() => [appendOp(assertToml(row))])
    setAddOpen(false)
  }

  const updateCheck = (id: number, patch: Partial<CheckRow>) => {
    updateRow<CheckRow>(`assert:${id}`, m => m.checks.find(c => c.id === id), checkOps, patch)
    setValidation(prev => forget(prev, id))
  }

  const updateSupply = (index: number, patch: Partial<SupplyRow>) =>
    updateRow<SupplyRow>(`supply:${index}`, m => m.supplies.find(s => s.index === index)?.row,
      (derived, next) => supplyOps(index, derived, next), patch)

  const updateRoot = (patch: Partial<typeof root>) =>
    updateRow<typeof root>('root', m => ({ name: m.name, duration: m.duration, noFit: m.noFit }), rootOps, patch)

  const appendSupply = (row: SupplyRow) =>
    edit(() => [{ op: 'appendTable', name: 'supply', body: supplyKvs(row).map(([k, v]) => `${k} = ${v}\n`).join('') }])

  const newPeripheral = (kind: PeripheralRow['kind'], net: string, m: SpecModel) => {
    const row = emptyPeripheral(m.peripherals.length + 1, kind, net)
    row.id = freshId(row.id.replace(/\d+$/, ''), m.peripherals.map(p => p.id), nextGenerated.current++)
    return row
  }
  const addPeripheral = (kind: PeripheralRow['kind'], net = '') =>
    edit(m => [appendOp(peripheralToml(newPeripheral(kind, net, m)))])

  const updatePeripheral = (rowId: number, patch: Partial<PeripheralRow>) => {
    updateRow<PeripheralRow>(`peripheral:${rowId}`, m => m.peripherals.find(p => p.rowId === rowId), peripheralOps, patch)
    setPeripheralValidation(prev => forget(prev, rowId))
  }

  const newSensor = (m: SpecModel, id = '', componentRef = '', modelId = '') => {
    const row = emptySensor(m.sensors.length + 1, id, componentRef, modelId)
    if (!id) row.id = freshId('SENSOR', m.sensors.map(s => s.id), m.sensors.length + 1)
    return row
  }
  const addSensor = (id = '', componentRef = '', modelId = '') =>
    edit(m => [appendOp(sensorToml(newSensor(m, id, componentRef, modelId)))])

  const updateSensor = (rowId: number, patch: Partial<SensorRow>) => {
    updateRow<SensorRow>(`sensor:${rowId}`, m => m.sensors.find(s => s.rowId === rowId), sensorOps, patch)
    // Any earlier engine receipt describes the bytes before this edit.
    setSensorLiveRequests(prev => forget(prev, rowId))
    setSensorValidation(prev => forget(prev, rowId))
  }

  /** Remove a table; rows after it shift up, so per-row notes start over. */
  const removeTable = (name: string, index: number) => {
    edit(() => [{ op: 'removeTable', table: [name, index] }])
    setDrafts(new Map())
    setValidation(new Map())
    setPeripheralValidation(new Map())
    setSensorValidation(new Map())
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
  // map or in the live sim). Each request becomes the SAME table a hand-built
  // row writes, appended to the text, so nothing queued is ever silently
  // dropped. A register-map device is never guessed from the part name: the
  // table opens named, and the user supplies the exact spec bytes before Run
  // becomes available.
  useEffect(() => {
    if (pending.length === 0) return
    edit(m => {
      const ops: PatchOp[] = []
      let duration = m.duration
      const peripheralsSoFar = [...m.peripherals]
      const overrides = [...m.overrides]
      let sensorsSoFar = m.sensors.length
      for (const r of pending) {
        if (r.type === 'check') {
          const row = emptyCheck(0, r.kind, r.net ?? '')
          row.ref = r.ref ?? ''
          // The board card edits the exact assertion before queueing it, so
          // every value it settled is carried into the row rather than reset.
          for (const key of CONSTRAINT_KEYS) {
            const value = r[key]
            if (value !== undefined) row[key] = value
          }
          row.rail_polarity = railPolarity(row)
          // A check that needs a longer window (a blink rate) raises the run length, never lowers it.
          if (r.run_ms && Number(r.run_ms) > Number(duration)) duration = r.run_ms
          ops.push(appendOp(assertToml(row)))
        } else if (r.type === 'peripheral') {
          const row = newPeripheral(r.kind, r.net ?? '', { ...m, peripherals: peripheralsSoFar })
          if (r.id) row.id = r.id
          peripheralsSoFar.push(row)
          ops.push(appendOp(peripheralToml(row)))
        } else if (r.type === 'sensor') {
          sensorsSoFar += 1
          const row = emptySensor(sensorsSoFar, r.id, r.ref ?? '', r.modelId ?? '')
          ops.push({ op: 'splice', at: Number.MAX_SAFE_INTEGER, remove: 0, insert: `\n# TODO: paste a validated register-map spec for ${r.ref ?? r.id}` })
          ops.push(appendOp(sensorToml(row)))
        } else if (r.type === 'assert') {
          // The board card's exact table (and any prerequisite blocks with it).
          // The document is the one source, so appending it is all it takes:
          // the builder re-reads the rows from the text.
          ops.push(appendOp(r.toml))
          // A blink rate cannot be timed in a run shorter than its periods.
          if (r.run_ms && Number(r.run_ms) > Number(duration)) duration = r.run_ms
        } else if (r.type === 'select') {
          // The last pick for a ref wins: an existing override is edited in place.
          const row: OverrideRow = { ref: r.ref, select: r.select, reason: 'picked in the app' }
          const hit = overrides.find(o => o.row.ref === r.ref)
          if (hit) {
            ops.push(...overrideOps(hit.index, hit.row, row))
            hit.row = row
          } else {
            ops.push(appendOp(overrideToml(row)))
            overrides.push({ row, index: overrides.length })
          }
        } else {
          ops.push(appendOp(supplyToml({ net: r.net, volts: String(r.volts ?? 3.3) })))
        }
      }
      if (duration !== m.duration) ops.unshift(...rootOps(m, { ...m, duration }))
      return ops
    })
    onPendingConsumed(pending[pending.length - 1].seq)
  }, [pending, onPendingConsumed, edit])

  const runChecks = useCallback(async () => {
    if (!boardFile) return
    // Preflight the builder rows in the builder's own vocabulary before
    // anything is POSTed: the server's TOML-keyed errors ("needs `ref` and
    // `amps`") belong to the TOML pane's lint, and they also over-name fields
    // that are actually present. While the text does not parse, the server's
    // answer is the only honest one, so the preflight stands aside.
    if (parsed) {
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
        const issues = [...rowIssues(c), ...componentCapabilityIssues(c, report.component_assertions)]
        if (issues.length > 0) problems.set(c.id, issues)
      }
      setPeripheralValidation(peripheralProblems)
      setSensorValidation(sensorProblems)
      setValidation(problems)
      if (problems.size > 0 || peripheralProblems.size > 0 || sensorProblems.size > 0) {
        // A local preflight result supersedes a stale server error. Put the
        // explanation on the row it judges, then move focus to the first field
        // that needs attention, so nobody has to translate a TOML key from a
        // banner on the other side of the page.
        setRun(null)
        onSummary(null)
        requestAnimationFrame(() => {
          const firstInvalid = document.querySelector<HTMLElement>('.checks-flush [aria-invalid="true"]')
          firstInvalid?.scrollIntoView({ behavior: 'smooth', block: 'center' })
          firstInvalid?.focus({ preventScroll: true })
        })
        return
      }
    }
    setRunning(true)
    setProgress(null)
    const abort = new AbortController()
    runAbort.current = abort
    const tomlAtRun = specText
    const firmwareAtRun = firmwareForRun
    const ran = {
      toml: tomlAtRun,
      firmwareIdentity: fileIdentity(firmwareAtRun),
      boardIdentity: fileIdentity(boardFile),
      schematicIdentity: fileIdentity(schematicFile),
      supplementalIdentity: supplementalIdentity(supplementalFiles),
    }
    try {
      const response = await api.check(
        buildCheckUpload(boardFile, firmwareAtRun, schematicFile, tomlAtRun, supplementalFiles),
        abort.signal,
        p => { if (!abort.signal.aborted) setProgress(p) },
      )
      if (!mounted.current) return
      setRun({ response, ...ran })
    } catch (e) {
      if (!mounted.current) return
      // A stop the person asked for is not a fault in the board: say what it
      // means for the verdict and what to do next.
      setRun({
        response: { ok: false, error: abort.signal.aborted ? RUN_CANCELLED : errorText(e) },
        ...ran,
      })
    } finally {
      if (runAbort.current === abort) runAbort.current = null
      if (mounted.current) {
        setRunning(false)
        setProgress(null)
      }
    }
  }, [
    boardFile, firmwareForRun, schematicFile, supplementalFiles, specText, parsed,
    checks, peripherals, sensors, onSummary, report.component_assertions,
  ])

  const specStem = specStemFor(report.file_name)
  const specFileName = `${specStem}.toml`
  // The runnable spec: the text with the board/firmware paths the recommended
  // repo layout puts them at. A key is prepended only when the text does not
  // already carry it, so nothing is doubled.
  //
  // Memoized because it is THE spec: the pane renders it, the Download button
  // writes it, the Export menu offers it and the saved session stores it. Four
  // readers of a per-render function are four chances to disagree.
  const portableText = useMemo(
    () => buildPortableCheckSpec(
      report.file_name, firmwareForRun, schematicFile, specText, includeFirmware, supplementalFiles,
    ),
    [specText, firmwareForRun, schematicFile, supplementalFiles, report.file_name, includeFirmware],
  )
  // The lines the download adds above the text, shown above the editor.
  const pathLines = useMemo(() => {
    const at = portableText.lastIndexOf(specText)
    return at > 0 && at + specText.length === portableText.length ? portableText.slice(0, at) : ''
  }, [portableText, specText])

  // Hand the spec up to the shell, so the Export menu and the saved session
  // carry the same bytes this pane shows rather than a second composition of
  // them. This pane owns the lifetime of that value, INCLUDING clearing it on
  // the way out: a restored session remounts this pane in the same commit the
  // shell resets its run state, and only the child knows which order that
  // leaves the spec in.
  useEffect(() => {
    onSpec({ toml: portableText, fileName: specFileName })
  }, [portableText, specFileName, onSpec])
  useEffect(() => () => onSpec(null), [onSpec])

  const workflowYml = workflowExportAvailable ? workflowYaml(specStem) : workflowPreviewYaml(specStem)

  const result = run?.response ?? null
  const results = useMemo(() => (result?.ok ? result.results ?? [] : []), [result])

  // The verdict the band above the rows states (it offers the modelling action
  // when that verdict is "not judged", so the rows do not repeat it).
  const bandVerdict = run?.response.ok ? runView(run.response, run.toml).verdict : null
  // Row -> result: results come back in [[assert]] order, and a row's id is
  // its [[assert]] position, so a kept-as-written assertion between two rows
  // never shifts the mapping. Grouping in the display reorders only what is shown.
  const resultForRow = (id: number): CheckResult | null => results[id - 1] ?? null
  // The results after the last row: shown in their own card, so every count in
  // the strip and the top bar has something to point at.
  const catalog = useCatalog()
  const downloadMarkdown = () => {
    if (!result?.ok || !run) return
    const md = runMarkdown({
      result, specToml: run.toml, specFileName, boardLabel: report.file_name ?? specStem,
      title: r => r.kind === STATIC_CHECK_KIND ? "The board's own check" : catalog?.find(k => k.kind === r.kind)?.title ?? r.label,
    })
    downloadText(`${specStem}-checks.md`, md, 'text/markdown;charset=utf-8')
  }

  const gutter = useMemo<(GutterResult | null)[]>(() => results.map((r): GutterResult => ({
    status: r.invalid ? 'invalid' : r.measured ? 'measured' : r.passed ? 'pass' : 'fail',
    label: `${r.invalid ? 'INVALID' : r.measured ? 'MEASURED' : r.passed ? 'PASS' : 'FAIL'}: ${r.label}`,
    stale,
  })), [results, stale])

  /** A verdict's place in the active surface: the gutter dot's click, or the run
   *  strip's count. In the TOML it selects that assertion's header line; in the
   *  builder it brings the row into view. */
  const jumpToResult = (assertIndex: number) => {
    // A result no spec row owns (the board's own check) has its card, not a row.
    if (assertIndex >= assertRowCount(run?.toml ?? '')) {
      const card = document.querySelector<HTMLElement>('[data-testid="board-gate"], [data-testid="run-summary"]')
      card?.scrollIntoView({ behavior: 'smooth', block: 'start' })
      card?.animate?.([{ boxShadow: 'inset 0 0 0 2px var(--copper)' }, { boxShadow: 'inset 0 0 0 2px transparent' }], { duration: 1200 })
      return
    }
    if (mode === 'toml') {
      const at = Array.from(specText.matchAll(/^\[\[assert\]\]/gm))[assertIndex]?.index
      if (at !== undefined) showInText(at)
      return
    }
    const row = document.querySelector<HTMLElement>(`.check-row[data-row-id="${assertIndex + 1}"]`)
    if (!row) return
    // 'start': a row with long evidence is taller than the viewport, and
    // centring it would scroll its heading (the part that names it) away.
    row.scrollIntoView({ behavior: 'smooth', block: 'start' })
    row.animate?.([{ boxShadow: 'inset 0 0 0 2px var(--copper)' }, { boxShadow: 'inset 0 0 0 2px transparent' }], { duration: 1200 })
  }
  const jumpToStatus = (status: RunStatus) => {
    const first = results.findIndex(r => statusOf(r) === status)
    if (first >= 0) jumpToResult(first)
  }

  // One click per remedy. Each edits the text the way a hand edit would, so
  // the spec pane shows exactly what the click did, and then points at the Run
  // button: the verdict is only ever re-judged by a run.
  useEffect(() => {
    if (!pulseRun) return
    const timer = window.setTimeout(() => setPulseRun(false), 5000)
    return () => window.clearTimeout(timer)
  }, [pulseRun])

  const remedyActions = useMemo<RemedyActions>(() => {
    const flagRerun = () => setPulseRun(true)
    const stored = (net: string) => parsed
      ? parsed.supplies.some(s => s.row.net.trim() === net)
      : specText.includes(`net = ${JSON.stringify(net)}`)
    const left = (reference: string) => !!parsed?.noFit.includes(reference)
    return {
      addSupply: (net, hint) => {
        if (!stored(net)) {
          edit(m => (m.supplies.some(s => s.row.net.trim() === net) ? [] : [appendOp(supplyToml({
            net, volts: hint && hint > 0 ? String(Math.ceil(hint)) : '9', kind: 'bench', limit: '2.0',
          }))]))
        }
        flagRerun()
        // The new row is not in the DOM until React commits it.
        window.setTimeout(() => {
          const row = Array.from(document.querySelectorAll<HTMLElement>('[data-testid="supply-row"]'))
            .find(el => el.dataset.net === net)
          const volts = row?.querySelector<HTMLInputElement>('[data-testid="supply-volts"]')
          volts?.scrollIntoView({ behavior: 'smooth', block: 'center' })
          volts?.focus({ preventScroll: true })
          volts?.select()
        }, 60)
      },
      markNotFitted: reference => {
        edit(m => (m.noFit.includes(reference) ? [] : rootOps(m, { ...m, noFit: [...m.noFit, reference] })))
        flagRerun()
      },
      openModel: reference => setModelFor(current => ({ reference, signal: (current?.signal ?? 0) + 1 })),
      applied: (remedy: Remedy) => {
        // Only meaningful while the spec differs from the run that asked for it.
        if (!stale) return false
        if (remedy.type === 'add_supply') return stored(remedy.net)
        if (remedy.type === 'mark_dnp') return left(remedy.reference)
        return false
      },
    }
  }, [parsed, specText, stale, edit])

  // The editor a "write a model" remedy opens, in place under the verdict.
  // `suggested` is memoized: WritePart re-prepares its draft whenever it changes.
  const modelPart = useMemo(() => {
    if (!modelFor) return undefined
    // The coverage row when the part has one, so a part with an executable
    // model opens on Extend; otherwise its board identity.
    const row = coverageFor(report.model_coverage, modelFor.reference)
    if (row) return row
    const part = report.components?.find(c => c.reference === modelFor.reference)
    return { reference: modelFor.reference, value: part?.value ?? '' }
  }, [modelFor, report.components, report.model_coverage])
  const modelEditor = modelFor && modelPart ? (
    <div className="mt-3" data-testid="remedy-model-editor">
      <WritePart
        onSaved={onModelSaved}
        suggested={modelPart}
        openSignal={`${modelFor.reference}:${modelFor.signal}`}
        boardLabel={report.file_name}
      />
    </div>
  ) : null

  const nets = report.nets ?? []
  const refs = useMemo(() => (report.components ?? []).map(c => c.reference), [report.components])
  const keptAsserts = model.kept.filter(k => k.label.startsWith('[[assert]]')).length

  /** Select a kept-as-written block's first line in the TOML pane. */
  const showInText = (offset: number) => {
    setMode('toml')
    const end = specText.indexOf('\n', offset)
    setSelect(prev => ({ start: offset, end: end < 0 ? specText.length : end, seq: (prev?.seq ?? 0) + 1 }))
  }

  // The builder surface. While the text does not parse, it shows the last spec
  // that did, read-only: an edit here would have nothing to patch.
  const builder = (
    <div className="min-w-0 checks-flush">
      {parseError && (
        <div
          data-testid="builder-paused"
          role="status"
          className="rounded-lg px-3 py-2.5 mb-4 text-[13px] leading-relaxed"
          style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
        >
          The TOML has a syntax error on line {parseError.line}: {parseError.message}. The builder
          is paused on the last version that parsed, and picks up again as soon as the text does.
        </div>
      )}

      {/* While the text does not parse, the builder shows the last spec that
          did, read-only: an edit here would have nothing to patch. */}
      <fieldset disabled={!parsed} className="min-w-0" style={{ border: 0, padding: 0, margin: 0, opacity: parsed ? 1 : 0.6 }}>
        <AssertionGroups
          checks={checks}
          validation={validation}
          stale={stale}
          resultForRow={resultForRow}
          bandOffersModelling={bandVerdict === 'invalid'}
          assumptions={run?.response.assumptions ?? []}
          inventory={run?.response.inventory ?? []}
          onAdd={addCheck}
          onUpdate={updateCheck}
          onRemove={id => removeTable('assert', id - 1)}
          footer={<>
            {/* Every check removed. A blank area is a question the
                interface asked and then refused to answer: say what is
                missing, what happens if it stays missing (the run is
                refused, not silently empty), and the one check worth
                starting from. */}
            {hasNoGroups(checks) && keptAsserts === 0 && (
              <div className="mb-4">
                <EmptyState
                  testId="checks-empty"
                  title="No checks in this spec yet"
                  body={<>
                    A spec with no assertions has nothing to pass or fail, so the run is
                    refused rather than reported as green. Add one below, or{' '}
                    <ToBoard>click a trace on the Board view</ToBoard> to start from a real net.
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

            {/* The one way to add a check here: every kind the engine knows,
            grouped; the board's check-this dock is the other door. */}
        <AddCheckMenu open={addOpen} onOpenChange={setAddOpen} />
          </>}
        />


        {/* Power. The net picker gives up width first (it is the one
            field with slack), then the row wraps; `remove` is last in
            reading order but never last in line, so it stays inside
            the card at every width. */}
        <BuilderSection
          testId="test-setup"
          title="Test setup"
          caption="What the board is powered from and what is done to it while the checks run. The supplies were read off the board; adjust them if they are wrong."
        >
          <h3 className="text-[12px] font-semibold mb-1.5" style={{ margin: 0, color: 'var(--silk-dim)' }}>Power</h3>
          {supplies.map(({ row: s, index }) => {
            const patch = (next: Partial<SupplyRow>) => updateSupply(index, next)
            return (
              <div
                key={index}
                data-testid="supply-row"
                data-net={s.net.trim()}
                className="flex flex-wrap items-center gap-x-2 gap-y-1.5 mb-1.5"
              >
                <NetField
                  value={s.net}
                  nets={nets}
                  ariaLabel="supply net"
                  onChange={net => patch({ net })}
                />
                <select
                  className="hb-input"
                  aria-label={`source behind ${s.net || 'this supply'}`}
                  value={s.kind ?? 'ideal'}
                  onChange={e => patch({ kind: e.target.value as SupplyRow['kind'] })}
                >
                  {SUPPLY_KINDS.map(kind => <option key={kind} value={kind}>{labelFor(kind)}</option>)}
                </select>
                {s.kind === 'usb' ? (
                  <select
                    className="hb-input"
                    aria-label="USB profile"
                    value={s.usb ?? USB_PROFILES[1]}
                    onChange={e => patch({ usb: e.target.value })}
                  >
                    {USB_PROFILES.map(p => <option key={p} value={p}>{labelFor(p)}</option>)}
                  </select>
                ) : (
                  <Field label="volts" suffix="V" testId="supply-volts" value={s.volts} width={70} onChange={volts => patch({ volts })} />
                )}
                {s.kind === 'bench' && (
                  <Field label="limit" suffix="A" value={s.limit ?? ''} width={60} onChange={limit => patch({ limit })} />
                )}
                <RemoveButton
                  className="text-[12px] shrink-0"
                  onClick={() => removeTable('supply', index)}
                />
              </div>
            )
          })}
          <button
            type="button"
            className="hb-press text-[12px] cursor-pointer inline-flex items-center gap-1"
            style={{ color: 'var(--silk-dim)', background: 'none', border: 'none' }}
            onClick={() => appendSupply({ net: '', volts: '5.0' })}
          >
            <PlusIcon size={12} /> add a supply
          </button>

          {/* Stimuli and bus devices: rows when there are any, otherwise one
              quiet line of ways to add them. */}
          {peripherals.length === 0 && sensors.length === 0 ? (
            <div className="mt-3 pt-3 flex flex-wrap items-center gap-x-2 gap-y-1.5 text-[12px]" data-testid="setup-add-more" style={{ borderTop: '1px solid var(--hairline)', color: 'var(--silk-faint)' }}>
              <span>Drive it while it runs:</span>
              {(['stimulus', 'pushbutton', 'toggle'] as const).map(kind => (
                <button key={kind} type="button" className="hb-chip hb-press px-2 py-0.5 text-[11px]" onClick={() => addPeripheral(kind)}>
                  + {kind === 'stimulus' ? 'waveform' : kind === 'pushbutton' ? 'button' : 'switch'}
                </button>
              ))}
              <button type="button" className="hb-chip hb-press px-2 py-0.5 text-[11px]" onClick={() => addSensor()}>+ bus device</button>
              <span>or <ToBoard>click a net or part on the board</ToBoard>.</span>
            </div>
          ) : null}
          <div className="mt-2 text-[12px]" data-testid="setup-firmware" style={{ color: 'var(--silk-faint)' }}>
            {firmwareFile
              ? <>Firmware: <span style={{ fontFamily: 'var(--font-mono)' }}>{firmwareFile.name}</span> (choose above whether this run includes it).</>
              : <>No firmware loaded, so UART, blink and boot checks have nothing to run. <ToBoard>Add it on the Board view</ToBoard>.</>}
          </div>
        </BuilderSection>

        {(peripherals.length > 0 || sensors.length > 0) && <>
        <InteractionBuilder
          peripherals={peripherals}
          validation={peripheralValidation}
          onAdd={kind => addPeripheral(kind)}
          onUpdate={updatePeripheral}
          onRemove={rowId => removeTable('peripheral', rowId - 1)}
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
          onRemove={rowId => removeTable('sensor', rowId - 1)}
          onAttachLive={attachSensorLive}
        />

        </>}

        {/* What the builder shows but leaves to the TOML pane: tables and
            keys outside its vocabulary, kept byte for byte. */}
        {model.kept.length > 0 && (
          <section className="hb-card px-4 py-3.5 mb-4" data-testid="kept-as-written">
            <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ margin: 0, color: 'var(--silk-faint)' }}>
              Kept as written
            </h2>
            <p className="text-[12px] mt-1 mb-2 leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
              These run as written. The builder does not edit them and never changes them;
              click one to edit it in the TOML.
            </p>
            <div className="flex flex-wrap gap-1.5">
              {model.kept.map(k => (
                <button
                  key={`${k.offset}:${k.label}`}
                  type="button"
                  data-testid="kept-chip"
                  onClick={() => showInText(k.offset)}
                  className="hb-chip hb-press px-2.5 py-1 text-[12px]"
                  style={{ fontFamily: 'var(--font-mono)' }}
                >
                  {k.label}
                </button>
              ))}
            </div>
          </section>
        )}

        {/* Rarely changed: behind one disclosure, each with what its default means. */}
        <details className="mb-4 text-[12px]" data-testid="checks-advanced" style={{ color: 'var(--silk-faint)' }}>
          <summary className="cursor-pointer">Advanced: spec name and simulated time</summary>
          <div className="hb-card mt-2 px-4 py-3 flex flex-wrap items-center gap-x-5 gap-y-2">
            <label className="inline-flex items-center gap-2 text-[12px] min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
              Spec name
              <input
                className="hb-input min-w-0 flex-1"
                style={{ maxWidth: 220 }}
                value={root.name}
                onChange={e => updateRoot({ name: e.target.value })}
              />
            </label>
            <span title="How long each run simulates the board after power-up. 100 ms covers rails settling and a typical boot; a check that needs longer (a slow blink, a late UART line) raises it when added.">
              <Field label="Simulated time" suffix="ms" value={root.duration} width={70} onChange={duration => updateRoot({ duration })} />
            </span>
            <span className="text-[11px]">Default 100 ms: rails settle and a typical boot finishes. Checks that need longer raise it.</span>
          </div>
        </details>

      </fieldset>
    </div>
  )

  const firmwareChoice = firmwareFile ? (
    <FirmwareRunChoice
      firmware={firmwareFile}
      include={includeFirmware}
      avrAvailable={avrAvailable}
      running={running}
      onChange={setIncludeFirmware}
    />
  ) : null

  return (
    <div className="h-full overflow-y-auto view-enter" data-testid="checks-panel">
      <div className="max-w-5xl mx-auto px-6 pt-3 pb-16">
        {/* The one net picker every field in this view completes from. */}
        <datalist id="net-options">
          {nets.map(n => <option key={n} value={n} />)}
        </datalist>

        <SpecPane
          mode={mode}
          onModeChange={setMode}
          builder={builder}
          firmwareChoice={firmwareChoice}
          onJump={jumpToStatus}
          specText={portableText}
          specStem={specStem}
          specFileName={specFileName}
          bodyText={specText}
          onBodyChange={setSpecText}
          pathLines={pathLines}
          select={select}
          nets={nets}
          refs={refs}
          gutter={gutter}
          onGutterClick={jumpToResult}
          showAllResults={keptAsserts > 0 || !parsed}
          stale={stale}
          running={running}
          canRun={!!boardFile}
          preparing={scaffolding}
          progress={progress}
          onCancel={cancelWork}
          pulseRun={pulseRun}
          remedyActions={remedyActions}
          remedyEditor={modelEditor}
          onRun={() => void runChecks()}
          validationCount={validation.size}
          result={result}
          runKey={run?.toml}
          workflowYml={workflowYml}
          workflowPinned={workflowExportAvailable}
          reportSerious={report.serious}
          preLayoutNotice={report.fix_first?.notice ?? null}
          onMarkdown={downloadMarkdown}
        />
      </div>
    </div>
  )
}
