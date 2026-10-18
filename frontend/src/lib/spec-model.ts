import {
  assertKeys, assertKvs, CHECK_KEYS, checkKind, emptyCheck, emptyPeripheral, emptySensor, eventsToml, inputsToml,
  lines, numOr, overrideKvs, peripheralKvs, sensorKvs, SUPPLY_KINDS, supplyKvs, tomlString,
} from './check-spec'
import type {
  BuilderState, CheckRow, ConstraintDraft, Kv, OverrideRow, PeripheralRow, SensorRow, SupplyRow,
} from './check-spec'
import { readCst } from './toml-cst'
import type { CstEntry, PatchOp, TablePath } from './toml-cst'

// The spec text read back into builder rows, and builder edits written as
// patches on the text. Kept apart from check-spec (the composer the board view
// also uses) so the TOML parser loads with the Checks view, not the first paint.

// ── Reading the text back ─────────────────────────────────────────────────
// Fields the builder edits on each table. A table carrying anything else (an
// assertion scenario, a ripple spec, an unknown kind...) is shown as "kept as
// written": the builder leaves its bytes alone and the TOML pane edits it.
const SUPPLY_FIELDS = new Set(['net', 'kind', 'volts', 'usb', 'current_limit_a'])
const PERIPHERAL_FIELDS = new Set([
  'id', 'type', 'net', 'to', 'waveform', 'offset', 'amplitude', 'freq_hz',
  'bounce_ms', 'initial', 'event',
])
const PERIPHERAL_EVENT_FIELDS = new Set(['t_ms', 'value'])
const SENSOR_FIELDS = new Set(['id', 'spec', 'controller', 'cs_net', 'inputs'])
const OVERRIDE_FIELDS = new Set(['ref', 'select', 'value', 'reason'])
/** Root keys the builder edits, and the path keys it leaves to the server. */
const ROOT_EDITED = new Set(['name', 'duration_ms', 'no_fit'])
const ROOT_SILENT = new Set(['board', 'firmware', 'mcu'])
const ARRAYS = new Set(['supply', 'peripheral', 'sensor', 'assert', 'override'])

/** Something in the spec the builder shows but does not edit. */
export interface KeptBlock {
  /** What it is, as written: `[[assert]] ac_gain`, `[ac]`, `schematic`. */
  label: string
  /** Where it starts in the text, to jump the TOML pane there. */
  offset: number
}

/** The builder's view of the spec text. Row ids are positions in the text:
 *  check `id` is the [[assert]] index plus one, a peripheral's or sensor's
 *  `rowId` its array index plus one, and supplies and overrides carry their
 *  index, so an edit always finds its own table. */
export interface SpecModel {
  name: string
  duration: string
  noFit: string[]
  supplies: { row: SupplyRow; index: number }[]
  peripherals: PeripheralRow[]
  sensors: SensorRow[]
  checks: CheckRow[]
  overrides: { row: OverrideRow; index: number }[]
  kept: KeptBlock[]
}

type Table = Record<string, unknown>

const tableOf = (entries: CstEntry[]): Table | null => {
  const out: Table = {}
  for (const e of entries) {
    if (e.key.includes('.') || e.key in out) return null
    out[e.key] = e.value
  }
  return out
}

function supplyRow(s: Table): SupplyRow | null {
  if (Object.keys(s).some(k => !SUPPLY_FIELDS.has(k))) return null
  if (s.kind && !(SUPPLY_KINDS as readonly string[]).includes(String(s.kind))) return null
  return {
    net: String(s.net ?? ''),
    volts: String(s.volts ?? ''),
    ...(s.kind && s.kind !== 'ideal' ? { kind: s.kind as SupplyRow['kind'] } : {}),
    ...(s.usb !== undefined ? { usb: String(s.usb) } : {}),
    ...(s.current_limit_a !== undefined ? { limit: String(s.current_limit_a) } : {}),
  }
}

function peripheralRow(p: Table, rowId: number): PeripheralRow | null {
  if (Object.keys(p).some(k => !PERIPHERAL_FIELDS.has(k))) return null
  const kind = String(p.type ?? '')
  if (!['stimulus', 'pushbutton', 'toggle'].includes(kind)) return null
  const events = (p.event as Table[] | undefined) ?? []
  if (events.some(event => !event || Object.keys(event).some(k => !PERIPHERAL_EVENT_FIELDS.has(k)))) return null
  const row = emptyPeripheral(rowId, kind as PeripheralRow['kind'], String(p.net ?? ''))
  row.id = String(p.id ?? row.id)
  row.to = String(p.to ?? row.to)
  row.waveform = String(p.waveform ?? row.waveform) as PeripheralRow['waveform']
  if (!['dc', 'sine', 'noise'].includes(row.waveform)) return null
  row.offset = String(p.offset ?? row.offset)
  row.amplitude = String(p.amplitude ?? row.amplitude)
  row.freq_hz = String(p.freq_hz ?? row.freq_hz)
  row.bounce_ms = String(p.bounce_ms ?? row.bounce_ms)
  row.initial = String(p.initial ?? row.initial)
  row.events = events.map(event => ({ t_ms: String(event.t_ms ?? ''), value: String(event.value ?? '') }))
  return row
}

function sensorRow(sensor: Table, rowId: number): SensorRow | null {
  if (Object.keys(sensor).some(key => !SENSOR_FIELDS.has(key))) return null
  if (typeof sensor.spec !== 'string') return null
  const inputsObject = sensor.inputs as Table | undefined
  if (inputsObject && (Array.isArray(inputsObject) || typeof inputsObject !== 'object')) return null
  const row = emptySensor(rowId, String(sensor.id ?? ''))
  row.spec = sensor.spec
  row.controller = String(sensor.controller ?? '')
  row.csNet = String(sensor.cs_net ?? '')
  row.inputs = Object.entries(inputsObject ?? {}).map(([name, value], index) => ({
    rowId: index + 1,
    name,
    value: String(value),
  }))
  return row
}

function checkRow(a: Table, id: number): CheckRow | null {
  const k = checkKind(String(a.kind ?? ''))
  if (!k) return null
  const allowed = assertKeys(k)
  if (Object.keys(a).some(key => !allowed.has(key))) return null
  if (k.refuses?.(a)) return null
  if (Object.values(a).some(v => typeof v === 'object')) return null
  const row = emptyCheck(id, k.kind)
  for (const key of CHECK_KEYS) if (a[key] !== undefined) row[key] = String(a[key])
  k.hydrate?.(row)
  return row
}

function overrideRow(o: Table): OverrideRow | null {
  if (Object.keys(o).some(k => !OVERRIDE_FIELDS.has(k))) return null
  if (Object.values(o).some(v => typeof v !== 'string')) return null
  return o as unknown as OverrideRow
}

/** Read the spec text into builder rows plus what the builder keeps as
 *  written; null when the text does not parse. */
export function readSpec(text: string): SpecModel | null {
  const read = readCst(text)
  if (!read.ok) return null
  const { doc } = read
  const model: SpecModel = {
    name: 'board checks', duration: '100', noFit: [],
    supplies: [], peripherals: [], sensors: [], checks: [], overrides: [], kept: [],
  }
  for (const e of doc.root) {
    const keep = () => model.kept.push({ label: e.key, offset: e.lineRange[0] })
    if (ROOT_SILENT.has(e.key)) continue
    if (!ROOT_EDITED.has(e.key)) { keep(); continue }
    if (e.key === 'name') model.name = String(e.value ?? '')
    else if (e.key === 'duration_ms') model.duration = String(e.value ?? '')
    else if (Array.isArray(e.value) && e.value.every(r => typeof r === 'string')) model.noFit = e.value as string[]
    else keep()
  }
  for (const el of doc.elements) {
    const own = tableOf(el.entries)
    let table: Table | null = own
    if (table && el.subtables.length) {
      for (const sub of el.subtables) {
        const body = tableOf(sub.entries)
        if (!body) { table = null; break }
        if (sub.header === 'peripheral.event') {
          const list = (table.event as Table[] | undefined) ?? []
          if (!Array.isArray(list)) { table = null; break }
          table.event = [...list, body]
        } else if (sub.header === 'sensor.inputs' && table.inputs === undefined) {
          table.inputs = body
        } else { table = null; break }
      }
    }
    const index = el.index ?? -1
    let ok = false
    if (table && el.index !== null && ARRAYS.has(el.name)) {
      if (el.name === 'supply') {
        const row = supplyRow(table)
        if (row) { model.supplies.push({ row, index }); ok = true }
      } else if (el.name === 'peripheral') {
        const row = peripheralRow(table, index + 1)
        if (row) { model.peripherals.push(row); ok = true }
      } else if (el.name === 'sensor') {
        const row = sensorRow(table, index + 1)
        if (row) { model.sensors.push(row); ok = true }
      } else if (el.name === 'assert') {
        const row = checkRow(table, index + 1)
        if (row) { model.checks.push(row); ok = true }
      } else {
        const row = overrideRow(table)
        if (row) { model.overrides.push({ row, index }); ok = true }
      }
    }
    if (!ok) {
      const kind = el.name === 'assert' && typeof own?.kind === 'string' ? ` ${own.kind}` : ''
      const label = el.index === null ? `[${el.header}]` : `[[${el.header}]]${kind}`
      model.kept.push({ label, offset: el.headerStart })
    }
  }
  return model
}

/** Best-effort, all-or-nothing: the builder rows for a spec the builder covers
 *  completely, or null when it does not parse or keeps anything as written. */
export function tomlToBuilder(raw: string): BuilderState | null {
  const model = readSpec(raw)
  if (!model || model.kept.length > 0) return null
  return {
    name: model.name,
    duration: model.duration,
    supplies: model.supplies.map(s => s.row),
    peripherals: model.peripherals,
    sensors: model.sensors,
    checks: model.checks,
    overrides: model.overrides.map(o => o.row),
    noFit: model.noFit,
  }
}

// ── Builder edits as patch ops ────────────────────────────────────────────

/** The ops that turn `prev`'s keys into `next`'s, touching only keys whose
 *  literal changed. Keys in `hold` (a number still being typed) keep their
 *  current text. */
export function kvDiffOps(table: TablePath, prev: readonly Kv[], next: readonly Kv[], hold: ReadonlySet<string> = new Set()): PatchOp[] {
  const before = new Map(prev)
  const after = new Map(next)
  const ops: PatchOp[] = []
  for (const [key, value] of after) {
    if (hold.has(key)) continue
    if (!before.has(key)) ops.push({ op: 'insertKey', table, key, value })
    else if (before.get(key) !== value) ops.push({ op: 'setValue', table, key, value })
  }
  for (const key of before.keys()) {
    if (!after.has(key) && !hold.has(key)) ops.push({ op: 'removeKey', table, key })
  }
  return ops
}

/** A typed value that is not (yet) a TOML number. */
const unfinished = (v: string | undefined) => !!v && v.trim() !== '' && numOr(v) === null

export function checkOps(prev: CheckRow, next: ConstraintDraft): PatchOp[] {
  const k = checkKind(next.kind)
  const hold = new Set((k?.fields ?? []).filter(f => !f.text && !f.ui && unfinished(next[f.key])).map(f => f.key as string))
  return kvDiffOps(['assert', prev.id - 1], assertKvs(prev), assertKvs(next), hold)
}

export function supplyOps(index: number, prev: SupplyRow, next: SupplyRow): PatchOp[] {
  const hold = new Set<string>()
  if (unfinished(next.volts)) hold.add('volts')
  if (unfinished(next.limit)) hold.add('current_limit_a')
  return kvDiffOps(['supply', index], supplyKvs(prev), supplyKvs(next), hold)
}

export function peripheralOps(prev: PeripheralRow, next: PeripheralRow): PatchOp[] {
  const table: TablePath = ['peripheral', prev.rowId - 1]
  const numbers = [next.offset, next.amplitude, next.freq_hz, next.bounce_ms, next.initial,
    ...next.events.flatMap(e => [e.t_ms, e.value])]
  if (numbers.some(unfinished)) return []
  if (JSON.stringify(prev.events) !== JSON.stringify(next.events)) {
    return [{ op: 'replaceBody', table, body: lines(peripheralKvs(next)) + eventsToml(next) }]
  }
  return kvDiffOps(table, peripheralKvs(prev), peripheralKvs(next))
}

export function sensorOps(prev: SensorRow, next: SensorRow): PatchOp[] {
  const table: TablePath = ['sensor', prev.rowId - 1]
  if (next.inputs.some(input => unfinished(input.value))) return []
  if (inputsToml(prev) !== inputsToml(next)) {
    return [{ op: 'replaceBody', table, body: lines(sensorKvs(next)) + inputsToml(next) }]
  }
  return kvDiffOps(table, sensorKvs(prev), sensorKvs(next))
}

export function overrideOps(index: number, prev: OverrideRow, next: OverrideRow): PatchOp[] {
  return kvDiffOps(['override', index], overrideKvs(prev), overrideKvs(next))
}

/** The root keys: name, run length, the not-fitted list. */
export function rootOps(
  prev: Pick<SpecModel, 'name' | 'duration' | 'noFit'>,
  next: Pick<SpecModel, 'name' | 'duration' | 'noFit'>,
): PatchOp[] {
  const kvs = (m: typeof prev): Kv[] => {
    const out: Kv[] = [['name', tomlString(m.name)]]
    const dur = numOr(m.duration)
    if (dur) out.push(['duration_ms', dur])
    if (m.noFit.length) out.push(['no_fit', `[${m.noFit.map(tomlString).join(', ')}]`])
    return out
  }
  return kvDiffOps([], kvs(prev), kvs(next), new Set(unfinished(next.duration) ? ['duration_ms'] : []))
}

/** A new table at the end of the spec, from the same composer a fresh spec uses. */
export function appendOp(block: string): PatchOp {
  const m = /^\n?\[\[([^\]]+)\]\]\n/.exec(block)
  if (!m) return { op: 'splice', at: Number.MAX_SAFE_INTEGER, remove: 0, insert: block }
  return { op: 'appendTable', name: m[1], body: block.slice(m[0].length) }
}
