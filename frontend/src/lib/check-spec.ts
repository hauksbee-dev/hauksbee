import { parse as parseToml } from 'smol-toml'

// The checks spec, as a data model: the builder's rows, the TOML they compose
// into, and the round-trip back. The Checks view renders these; nothing here
// touches React, so the composer and the parser can be tested on their own.

export interface SupplyRow {
  net: string
  volts: string
}

/** A visual co-sim interaction. These are the three controls that can attach
 * safely to any real board net without pretending a bus model or connector
 * mapping exists. Rich bus devices remain model-owned or available in raw
 * mode until the browser can fill every required identity field honestly. */
export interface PeripheralRow {
  rowId: number
  id: string
  kind: 'stimulus' | 'pushbutton' | 'toggle'
  net: string
  to: string
  waveform: 'dc' | 'sine' | 'noise'
  offset: string
  amplitude: string
  freq_hz: string
  bounce_ms: string
  initial: string
  events: Array<{ t_ms: string; value: string }>
}

export interface SensorInputRow {
  rowId: number
  name: string
  value: string
}

/** A declarative register-map device. The spec bytes are kept inline in the
 * scenario so a downloaded check is self-contained and a local file path can
 * never go stale after upload. */
export interface SensorRow {
  rowId: number
  id: string
  componentRef: string
  modelId: string
  specName: string
  spec: string
  controller: string
  csNet: string
  inputs: SensorInputRow[]
}

/** One plain-language check. `kind` maps 1:1 onto the spec's [[assert]] kinds;
 *  fields are kept as strings so the inputs stay honest about what was typed. */
export interface CheckRow {
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

export interface BuilderState {
  name: string
  duration: string
  supplies: SupplyRow[]
  peripherals: PeripheralRow[]
  sensors: SensorRow[]
  checks: CheckRow[]
}

export function emptySensor(rowId: number, id = '', componentRef = '', modelId = ''): SensorRow {
  return {
    rowId,
    id: id || `SENSOR${rowId}`,
    componentRef,
    modelId,
    specName: '',
    spec: '',
    controller: '',
    csNet: '',
    inputs: [],
  }
}

export const peripheralPrefix = (kind: PeripheralRow['kind']) =>
  kind === 'stimulus' ? 'STIM' : kind === 'pushbutton' ? 'BTN' : 'SW'

export function emptyPeripheral(rowId: number, kind: PeripheralRow['kind'], net = ''): PeripheralRow {
  return {
    rowId,
    id: `${peripheralPrefix(kind)}${rowId}`,
    kind,
    net,
    to: 'GND',
    waveform: 'dc',
    offset: kind === 'stimulus' ? '0' : '',
    amplitude: '1',
    freq_hz: '1000',
    bounce_ms: kind === 'pushbutton' ? '5' : '',
    initial: '0',
    events: [],
  }
}

export const emptyCheck = (id: number, kind: string, net = ''): CheckRow => ({
  id, kind, net,
  ref: '', min: '', max: '', after_ms: '', deadline_ms: '', contains: '',
  freq_hz: '', tolerance: '', min_toggles: '', amps: '', celsius: '',
  dip_below: '', for_max_ms: '', recover_to: '', recover_within_ms: '',
})

/** The check-kind vocabulary: plain words first, the TOML kind in small print. */
export const CHECK_KINDS: { kind: string; label: string; group: string; hint: string }[] = [
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
export const GROUP_ORDER = ['Voltages', 'Currents', 'Temperatures', 'Stress', 'Firmware', 'Activity']

// Kinds that carry a net / a component ref. Shared by the TOML composer, the
// raw parser's round-trip check, and the per-kind field pickers so the three
// cannot drift apart.
export const NET_KINDS = ['voltage', 'toggle', 'boot-coverage', 'rail_window']
export const REF_KINDS = ['max_current', 'max_temp']

export function tomlString(v: string): string {
  return JSON.stringify(v)
}

/** The trimmed value when it is a finite number, else null. */
function numOr(v: string): string | null {
  const t = v.trim()
  if (t === '') return null
  return Number.isFinite(Number(t)) ? t : null
}

/** Compose the spec BODY (no board/firmware keys; the server injects those
 *  from the uploaded files). */
export function buildToml(
  name: string,
  duration: string,
  supplies: SupplyRow[],
  peripherals: PeripheralRow[],
  checks: CheckRow[],
  sensors: SensorRow[] = [],
): string {
  let out = `name = ${tomlString(name)}\n`
  const dur = numOr(duration)
  if (dur) out += `duration_ms = ${dur}\n`
  for (const s of supplies) {
    if (!s.net.trim()) continue
    out += `\n[[supply]]\nnet = ${tomlString(s.net.trim())}\nkind = "ideal"\nvolts = ${numOr(s.volts) ?? '5.0'}\n`
  }
  for (const p of peripherals) {
    out += `\n[[peripheral]]\nid = ${tomlString(p.id.trim())}\ntype = ${tomlString(p.kind)}\nnet = ${tomlString(p.net.trim())}\n`
    if (p.kind === 'stimulus') {
      out += `waveform = ${tomlString(p.waveform)}\n`
      out += `offset = ${numOr(p.offset) ?? '0'}\n`
      if (p.waveform !== 'dc') out += `amplitude = ${numOr(p.amplitude) ?? '1'}\n`
      if (p.waveform === 'sine' || p.waveform === 'noise') out += `freq_hz = ${numOr(p.freq_hz) ?? '1000'}\n`
    } else {
      if (p.to.trim()) out += `to = ${tomlString(p.to.trim())}\n`
      if (p.kind === 'pushbutton' && numOr(p.bounce_ms)) out += `bounce_ms = ${numOr(p.bounce_ms)}\n`
      if (numOr(p.initial)) out += `initial = ${numOr(p.initial)}\n`
    }
    for (const event of p.events) {
      out += `[[peripheral.event]]\nt_ms = ${numOr(event.t_ms) ?? '0'}\nvalue = ${numOr(event.value) ?? '0'}\n`
    }
  }
  for (const sensor of sensors) {
    out += `\n[[sensor]]\nid = ${tomlString(sensor.id.trim())}\n`
    // JSON string escaping is valid TOML basic-string escaping and avoids the
    // delimiter collision of a pasted spec containing triple quotes.
    out += `spec = ${tomlString(sensor.spec)}\n`
    if (sensor.controller.trim()) out += `controller = ${tomlString(sensor.controller.trim())}\n`
    if (sensor.csNet.trim()) out += `cs_net = ${tomlString(sensor.csNet.trim())}\n`
    const inputs = sensor.inputs.filter(input => input.name.trim() && numOr(input.value))
    if (inputs.length > 0) {
      out += `[sensor.inputs]\n`
      for (const input of inputs) out += `${tomlString(input.name.trim())} = ${numOr(input.value)}\n`
    }
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
const PERIPHERAL_FIELDS = new Set([
  'id', 'type', 'net', 'to', 'waveform', 'offset', 'amplitude', 'freq_hz',
  'bounce_ms', 'initial', 'event',
])
const PERIPHERAL_EVENT_FIELDS = new Set(['t_ms', 'value'])
const SENSOR_FIELDS = new Set(['id', 'spec', 'controller', 'cs_net', 'inputs'])
const ASSERT_FIELDS = new Set([
  'kind', 'net', 'ref', 'min', 'max', 'after_ms', 'deadline_ms', 'contains',
  'freq_hz', 'tolerance', 'min_toggles', 'amps', 'celsius', 'dip_below',
  'for_max_ms', 'recover_to', 'recover_within_ms',
])

/** Best-effort: load a raw TOML back into builder rows. Returns null when the
 *  spec uses vocabulary the builder doesn't cover (an unknown top-level key,
 *  OR any nested supply/assert field the builder would not write back out);
 *  the caller then stays in raw mode, or warns before discarding. */
export function tomlToBuilder(raw: string): BuilderState | null {
  let doc: Record<string, unknown>
  try {
    doc = parseToml(raw) as Record<string, unknown>
  } catch {
    return null
  }
  const KNOWN = new Set(['name', 'duration_ms', 'supply', 'peripheral', 'sensor', 'assert', 'board', 'firmware', 'mcu'])
  if (Object.keys(doc).some(k => !KNOWN.has(k))) return null
  const supplies: SupplyRow[] = []
  for (const s of (doc.supply as Record<string, unknown>[] | undefined) ?? []) {
    if (Object.keys(s).some(k => !SUPPLY_FIELDS.has(k))) return null
    if (s.kind && s.kind !== 'ideal') return null
    supplies.push({ net: String(s.net ?? ''), volts: String(s.volts ?? '') })
  }
  const peripherals: PeripheralRow[] = []
  let peripheralId = 1
  for (const p of (doc.peripheral as Record<string, unknown>[] | undefined) ?? []) {
    if (Object.keys(p).some(k => !PERIPHERAL_FIELDS.has(k))) return null
    const kind = String(p.type ?? '')
    if (!['stimulus', 'pushbutton', 'toggle'].includes(kind)) return null
    const events = (p.event as Record<string, unknown>[] | undefined) ?? []
    if (events.some(event => Object.keys(event).some(k => !PERIPHERAL_EVENT_FIELDS.has(k)))) return null
    const row = emptyPeripheral(peripheralId++, kind as PeripheralRow['kind'], String(p.net ?? ''))
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
    peripherals.push(row)
  }
  const sensors: SensorRow[] = []
  let sensorId = 1
  for (const sensor of (doc.sensor as Record<string, unknown>[] | undefined) ?? []) {
    if (Object.keys(sensor).some(key => !SENSOR_FIELDS.has(key))) return null
    if (typeof sensor.spec !== 'string') return null
    const inputsObject = sensor.inputs as Record<string, unknown> | undefined
    if (inputsObject && (Array.isArray(inputsObject) || typeof inputsObject !== 'object')) return null
    const row = emptySensor(sensorId++, String(sensor.id ?? ''))
    row.spec = sensor.spec
    row.controller = String(sensor.controller ?? '')
    row.csNet = String(sensor.cs_net ?? '')
    row.inputs = Object.entries(inputsObject ?? {}).map(([name, value], index) => ({
      rowId: index + 1,
      name,
      value: String(value),
    }))
    sensors.push(row)
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
    const grab = (key: keyof CheckRow) => {
      const v = a[key]
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
    peripherals,
    sensors,
    checks,
  }
}

/** One builder-row validation problem: which UI field, said in the UI's own
 *  words (never TOML key names; the raw pane owns that vocabulary). */
export interface RowIssue {
  /** CheckRow field whose input gets the highlight. */
  field: keyof CheckRow
  message: string
}

/** Builder-mode preflight, mirroring hauksbee-ci's per-assertion requirements
 *  but speaking in the builder's field labels. Only fields actually missing
 *  are named, so "ref present, amps empty" says just "max A is empty". */
export function rowIssues(c: CheckRow): RowIssue[] {
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

/** Everything wrong with one register-map row, in the builder's words. */
export function sensorIssues(sensor: SensorRow): string[] {
  const issues: string[] = []
  if (!sensor.id.trim()) issues.push('Give the register-map device a stable id.')
  if (!sensor.spec.trim()) {
    issues.push('Choose or paste a declarative sensor spec. Hauksbee will not guess a bus protocol.')
  } else {
    try { parseToml(sensor.spec) } catch { issues.push('The pasted sensor spec is not valid TOML.') }
  }
  for (const input of sensor.inputs) {
    if (!input.name.trim()) issues.push('Every input override needs a name.')
    if (!numOr(input.value)) issues.push(`Input ${input.name || '(unnamed)'} needs a finite numeric value.`)
  }
  return issues
}

/** Everything wrong with one interaction row. */
export function peripheralIssues(peripheral: PeripheralRow): string[] {
  const issues: string[] = []
  if (!peripheral.id.trim()) issues.push('Give the interaction a stable id.')
  if (!peripheral.net.trim()) issues.push('Choose the board net this interaction attaches to.')
  if (peripheral.kind === 'stimulus' && peripheral.waveform !== 'dc') {
    if (!numOr(peripheral.amplitude)) issues.push('Enter a numeric waveform amplitude.')
    if (!numOr(peripheral.freq_hz)) issues.push('Enter a numeric waveform frequency.')
  }
  return issues
}
