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
  /** Which side of the rail window this row bounds: 'dip' or 'spike'. A row
   *  is one-sided, so only that side's four fields are edited and emitted. */
  rail_polarity: string
  dip_below: string
  for_max_ms: string
  recover_to: string
  recover_within_ms: string
  spike_above: string
  spike_for_max_ms: string
  settle_to: string
  settle_within_ms: string
}

/** A row's value fields: everything but its identity and kind. */
export type CheckKey = Exclude<keyof CheckRow, 'id' | 'kind'>

/** One check with no row identity: what the in-place editor and the modal
 *  hand around before (or without) a builder row existing. */
export type ConstraintDraft = Omit<CheckRow, 'id'>

/** Rows saved before the polarity selector existed carry no mode. A populated
 *  over-voltage field is unambiguously a spike; otherwise keep the original
 *  dip behaviour. */
export function railPolarity(c: Pick<ConstraintDraft, 'rail_polarity' | 'spike_above'>): 'dip' | 'spike' {
  if (c.rail_polarity === 'spike' || c.rail_polarity === 'dip') return c.rail_polarity
  return (c.spike_above ?? '').trim() ? 'spike' : 'dip'
}
const isDip = (c: ConstraintDraft) => railPolarity(c) === 'dip'
const isSpike = (c: ConstraintDraft) => railPolarity(c) === 'spike'

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

const CHECK_KEYS: CheckKey[] = [
  'net', 'ref', 'min', 'max', 'after_ms', 'deadline_ms', 'contains', 'freq_hz', 'tolerance',
  'min_toggles', 'amps', 'celsius', 'dip_below', 'for_max_ms', 'recover_to', 'recover_within_ms',
  'spike_above', 'spike_for_max_ms', 'settle_to', 'settle_within_ms',
]

export function emptyCheck(id: number, kind: string, net = ''): CheckRow {
  const row = { id, kind, rail_polarity: 'dip' } as CheckRow
  for (const key of CHECK_KEYS) row[key] = key === 'net' ? net : ''
  return row
}

/** The same row without an identity, for the shared editor and the modal. */
export function emptyConstraint(kind: string, net = '', ref = ''): ConstraintDraft {
  const { id: _id, ...draft } = emptyCheck(0, kind, net)
  draft.ref = ref
  return draft
}

/** The value fields a queued check may carry, beyond its kind and subject:
 *  what the in-place editor settled before the row reached the builder. */
export const CONSTRAINT_KEYS = CHECK_KEYS.filter(key => key !== 'net' && key !== 'ref')

/** Every scalar field on a row, for repairing browser-saved state. */
const CHECK_STRING_FIELDS: CheckKey[] = ['rail_polarity', ...CHECK_KEYS]

/** Upgrade browser-saved rows from earlier releases before any validator or
 *  editor sees them. An unknown kind is dropped and a missing scalar becomes
 *  an empty input, rather than an exception or an invented constraint. */
export function normalizeSavedCheck(value: unknown, fallbackId: number): CheckRow | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null
  const saved = value as Partial<CheckRow>
  const kind = typeof saved.kind === 'string' ? saved.kind : ''
  if (!checkKind(kind)) return null
  const id = typeof saved.id === 'number' && Number.isFinite(saved.id) ? saved.id : fallbackId
  const row = { ...emptyCheck(id, kind), ...saved } as CheckRow
  for (const field of CHECK_STRING_FIELDS) {
    if (typeof row[field] !== 'string') row[field] = ''
  }
  row.rail_polarity = railPolarity(row)
  return row
}

/** One input on a check row, beyond the shared net/ref picker. */
export interface CheckField {
  key: CheckKey
  label: string
  /** What the value wants, in px; a narrow column caps it. */
  width?: number
  placeholder?: string
  /** A quoted TOML string rather than a number. */
  text?: boolean
  /** A fixed choice rather than a free input. */
  options?: readonly { value: string; label: string }[]
  /** Test id for the control, when the generated one is not the contract. */
  testId?: string
  /** What the control shows, when a stored row may not carry the field (an
   *  older save) and the displayed value has to be derived instead. */
  value?: (c: ConstraintDraft) => string
  /** A mode selector the editor owns: never a spec key, so it is neither
   *  emitted nor accepted on the way back in. */
  ui?: boolean
  /** Rendered, validated and emitted only while this holds. A row with two
   *  mutually exclusive shapes carries both sets and shows one. */
  active?: (c: ConstraintDraft) => boolean
  /** What else to blank when this field is set: a mode switch owns the fields
   *  the other mode filled, so a stale opposite side can never be emitted. */
  clears?: (value: string) => Partial<ConstraintDraft>
}

/** An either/or requirement: at least one of `any` must be filled, else
 *  `message`. `when` limits the rule to rows where that field is filled. */
interface CheckNeed {
  any: CheckKey[]
  message: string
  when?: CheckKey
  /** Only checked while this holds (see `CheckField.active`). */
  active?: (c: ConstraintDraft) => boolean
}

/** One [[assert]] kind: plain words first, the TOML kind in small print, and
 *  the schema the composer, the round-trip parser, the preflight and the row
 *  form all read, so the four cannot drift. */
export interface CheckKind {
  kind: string
  label: string
  group: string
  hint: string
  /** What the check is about: a net or a part (ref). Required when present. */
  subject?: 'net' | 'ref'
  fields: CheckField[]
  needs: CheckNeed[]
  /** Refuse a round-trip to the builder for an [[assert]] this shape cannot
   *  hold without losing keys (the raw pane keeps it verbatim instead). */
  refuses?: (a: Record<string, unknown>) => boolean
  /** Settle the row's UI-only fields from what the spec actually carried. */
  hydrate?: (row: CheckRow) => void
}

const V = (key: CheckKey, label: string, width = 64): CheckField => ({ key, label, width })

export const CHECK_KINDS: CheckKind[] = [
  {
    kind: 'voltage', label: 'A net must sit at a voltage', group: 'Voltages',
    hint: 'min/max volts, optionally after a settle time', subject: 'net',
    fields: [V('min', 'min V'), V('max', 'max V'), V('after_ms', 'after ms')],
    needs: [{ any: ['min', 'max'], message: 'needs a min V and/or a max V' }],
  },
  {
    kind: 'rail_window', label: 'A rail excursion must stay within bounds', group: 'Voltages',
    hint: 'choose dip-below or spike-above, then set duration and recovery', subject: 'net',
    fields: [
      {
        key: 'rail_polarity', label: 'excursion', width: 110, ui: true,
        testId: 'rail-polarity', value: railPolarity,
        options: [{ value: 'dip', label: 'dip below' }, { value: 'spike', label: 'spike above' }],
        clears: value => value === 'spike'
          ? { dip_below: '', for_max_ms: '', recover_to: '', recover_within_ms: '' }
          : { spike_above: '', spike_for_max_ms: '', settle_to: '', settle_within_ms: '' },
      },
      { ...V('dip_below', 'dip below V'), active: isDip },
      { ...V('for_max_ms', 'for max ms'), active: isDip },
      { ...V('recover_to', 'recover to V'), active: isDip },
      { ...V('recover_within_ms', 'within ms'), active: isDip },
      { ...V('spike_above', 'spike above V', 78), active: isSpike },
      { ...V('spike_for_max_ms', 'for max ms'), active: isSpike },
      { ...V('settle_to', 'settle to V'), active: isSpike },
      { ...V('settle_within_ms', 'within ms'), active: isSpike },
    ],
    needs: [
      { any: ['dip_below'], message: 'dip below V is empty', active: isDip },
      { any: ['for_max_ms', 'recover_within_ms'], message: 'needs a for max ms or a recovery window (within ms)', when: 'dip_below', active: isDip },
      { any: ['recover_to'], message: 'recover to V is empty (needed with within ms)', when: 'recover_within_ms', active: isDip },
      { any: ['recover_within_ms'], message: 'within ms is empty (needed with recover to V)', when: 'recover_to', active: isDip },
      { any: ['spike_above'], message: 'spike above V is empty', active: isSpike },
      { any: ['spike_for_max_ms', 'settle_within_ms'], message: 'needs a for max ms or a settling window (within ms)', when: 'spike_above', active: isSpike },
      { any: ['settle_to'], message: 'settle to V is empty (needed with within ms)', when: 'settle_within_ms', active: isSpike },
      { any: ['settle_within_ms'], message: 'within ms is empty (needed with settle to V)', when: 'settle_to', active: isSpike },
    ],
    // A visual row has one polarity and therefore one set of keys. A spec
    // carrying both sides (or a spike duration with no threshold) stays in the
    // raw pane rather than losing fields on a builder round-trip.
    refuses: a => {
      const has = (keys: string[]) => keys.some(key => a[key] !== undefined)
      if (has(['dip_below', 'for_max_ms', 'recover_to', 'recover_within_ms'])
        && has(['spike_above', 'spike_for_max_ms', 'settle_to', 'settle_within_ms'])) return true
      return has(['spike_for_max_ms', 'settle_to', 'settle_within_ms']) && a.spike_above === undefined
    },
    // A parsed row starts on the default polarity, so the mode comes from the
    // key the spec actually carried rather than from that default.
    hydrate: row => { row.rail_polarity = row.spike_above.trim() ? 'spike' : 'dip' },
  },
  {
    kind: 'no_faults', label: 'Nothing over-stressed', group: 'Stress',
    hint: 'no component beyond its ratings at any point', fields: [], needs: [],
  },
  {
    kind: 'max_current', label: 'A part must stay under a current', group: 'Currents',
    hint: 'ceiling in amps for one component', subject: 'ref',
    fields: [V('amps', 'max A')],
    needs: [{ any: ['amps'], message: 'max A is empty' }],
  },
  {
    kind: 'max_temp', label: 'A part must stay cool', group: 'Temperatures',
    hint: 'junction temperature ceiling (or the part’s own rating)', subject: 'ref',
    // max °C may stay blank (falls back to the part's own rating).
    fields: [V('celsius', 'max °C (blank = part rating)', 70)],
    needs: [],
  },
  {
    kind: 'uart', label: 'The firmware must print', group: 'Firmware',
    hint: 'serial output contains a string',
    fields: [{ key: 'contains', label: 'must print', width: 220, placeholder: 'hello', text: true }],
    needs: [{ any: ['contains'], message: '"must print" is empty' }],
  },
  {
    kind: 'toggle', label: 'A net must blink', group: 'Activity',
    hint: 'toggle frequency or a minimum toggle count', subject: 'net',
    fields: [V('freq_hz', 'freq Hz'), V('tolerance', '±tol', 56), V('min_toggles', 'or min toggles')],
    needs: [{ any: ['freq_hz', 'min_toggles'], message: 'needs a freq Hz or a min toggles' }],
  },
  {
    kind: 'boot-coverage', label: 'Firmware must drive a net by a deadline', group: 'Firmware',
    hint: 'a gate/enable must be actively driven after reset', subject: 'net',
    fields: [V('min', 'reach V'), V('deadline_ms', 'within ms')],
    needs: [{ any: ['min'], message: 'reach V is empty' }, { any: ['deadline_ms'], message: 'within ms is empty' }],
  },
]

export const checkKind = (kind: string): CheckKind | undefined => CHECK_KINDS.find(k => k.kind === kind)

/** The display groups, in a stable order (only groups with rows render). */
export const GROUP_ORDER = ['Voltages', 'Currents', 'Temperatures', 'Stress', 'Firmware', 'Activity']

/** The TOML keys an [[assert]] of this kind may carry: the composer writes
 *  exactly these, so the parser refuses anything else (it would survive the
 *  parse but vanish from the round-tripped spec). */
function assertKeys(k: CheckKind): Set<string> {
  return new Set([
    'kind',
    ...(k.subject ? [k.subject] : []),
    ...k.fields.filter(f => !f.ui).map(f => f.key),
  ])
}

export function tomlString(v: string): string {
  return JSON.stringify(v)
}

/** The trimmed value when it is a finite number, else null. */
function numOr(v: string): string | null {
  const t = v.trim()
  if (t === '') return null
  return Number.isFinite(Number(t)) ? t : null
}

/** One [[supply]] block, or nothing for a row with no net. */
export function supplyToml(s: SupplyRow): string {
  if (!s.net.trim()) return ''
  return `\n[[supply]]\nnet = ${tomlString(s.net.trim())}\nkind = "ideal"\nvolts = ${numOr(s.volts) ?? '5.0'}\n`
}

export function peripheralToml(p: PeripheralRow): string {
  let out = `\n[[peripheral]]\nid = ${tomlString(p.id.trim())}\ntype = ${tomlString(p.kind)}\nnet = ${tomlString(p.net.trim())}\n`
  if (p.kind === 'stimulus') {
    out += `waveform = ${tomlString(p.waveform)}\noffset = ${numOr(p.offset) ?? '0'}\n`
    if (p.waveform !== 'dc') out += `amplitude = ${numOr(p.amplitude) ?? '1'}\nfreq_hz = ${numOr(p.freq_hz) ?? '1000'}\n`
  } else {
    if (p.to.trim()) out += `to = ${tomlString(p.to.trim())}\n`
    if (p.kind === 'pushbutton' && numOr(p.bounce_ms)) out += `bounce_ms = ${numOr(p.bounce_ms)}\n`
    if (numOr(p.initial)) out += `initial = ${numOr(p.initial)}\n`
  }
  for (const event of p.events) out += `[[peripheral.event]]\nt_ms = ${numOr(event.t_ms) ?? '0'}\nvalue = ${numOr(event.value) ?? '0'}\n`
  return out
}

/** The spec bytes are kept inline (JSON string escaping is valid TOML
 *  basic-string escaping) so a downloaded check is self-contained. */
export function sensorToml(sensor: SensorRow): string {
  let out = `\n[[sensor]]\nid = ${tomlString(sensor.id.trim())}\nspec = ${tomlString(sensor.spec)}\n`
  if (sensor.controller.trim()) out += `controller = ${tomlString(sensor.controller.trim())}\n`
  if (sensor.csNet.trim()) out += `cs_net = ${tomlString(sensor.csNet.trim())}\n`
  const inputs = sensor.inputs.filter(input => input.name.trim() && numOr(input.value))
  if (inputs.length > 0) {
    out += `[sensor.inputs]\n`
    for (const input of inputs) out += `${tomlString(input.name.trim())} = ${numOr(input.value)}\n`
  }
  return out
}

/** One [[assert]] block: the kind's subject, then its schema fields in order,
 *  each only when filled (strings quoted, numbers only when finite). */
export function assertToml(c: CheckRow): string {
  const k = checkKind(c.kind)
  let out = `\n[[assert]]\nkind = ${tomlString(c.kind)}\n`
  if (!k) return out
  if (k.subject && c[k.subject].trim()) out += `${k.subject} = ${tomlString(c[k.subject].trim())}\n`
  for (const f of k.fields) {
    if (f.ui || (f.active && !f.active(c))) continue
    const t = c[f.key].trim()
    if (!t) continue
    if (f.text) out += `${f.key} = ${tomlString(t)}\n`
    else if (numOr(t)) out += `${f.key} = ${numOr(t)}\n`
  }
  return out
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
  const dur = numOr(duration)
  return `name = ${tomlString(name)}\n${dur ? `duration_ms = ${dur}\n` : ''}`
    + supplies.map(supplyToml).join('')
    + peripherals.map(peripheralToml).join('')
    + sensors.map(sensorToml).join('')
    + checks.map(assertToml).join('')
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
    const k = checkKind(String(a.kind ?? ''))
    if (!k) return null
    const allowed = assertKeys(k)
    if (Object.keys(a).some(key => !allowed.has(key))) return null
    if (k.refuses?.(a)) return null
    const row = emptyCheck(id++, k.kind)
    for (const key of CHECK_KEYS) if (a[key] !== undefined) row[key] = String(a[key])
    k.hydrate?.(row)
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

/** One builder-row validation problem: which UI fields could satisfy it,
 *  said in the UI's own words (never TOML key names; the raw pane owns that
 *  vocabulary). */
export interface RowIssue {
  /** Every input whose value would settle the requirement; all get the highlight. */
  fields: CheckKey[]
  message: string
}

/** Builder-mode preflight, mirroring hauksbee-ci's per-assertion requirements
 *  but speaking in the builder's field labels. Only fields actually missing
 *  are named, so "ref present, amps empty" says just "max A is empty". */
export function rowIssues(c: ConstraintDraft): RowIssue[] {
  const k = checkKind(c.kind)
  if (!k) return []
  // A row saved by an older frontend may not carry a field introduced later.
  // Treat a missing one exactly like an empty input, so the preflight stays
  // fail-closed instead of throwing on a partial draft.
  const blank = (key: CheckKey) => (c[key] ?? '').trim() === ''
  const issues: RowIssue[] = []
  if (k.subject && blank(k.subject)) {
    issues.push({ fields: [k.subject], message: k.subject === 'net' ? 'net is empty' : 'part (ref) is empty' })
  }
  for (const need of k.needs) {
    if (need.active && !need.active(c)) continue
    if (need.when && blank(need.when)) continue
    if (need.any.every(blank)) issues.push({ fields: need.any, message: need.message })
  }
  return issues
}

/** What the bound circuit can actually measure for a part. A current or
 *  temperature check on a part with no such model is refused here rather than
 *  by the engine after a run. */
export function componentCapabilityIssues(
  c: ConstraintDraft,
  componentAssertions: Record<string, string[]> | undefined,
): RowIssue[] {
  const ref = (c.ref ?? '').trim()
  if (!['max_current', 'max_temp'].includes(c.kind) || !ref) return []
  if ((componentAssertions?.[ref] ?? []).includes(c.kind)) return []
  return [{
    fields: ['ref'],
    message: c.kind === 'max_current'
      ? `${ref} has no measurable through-current; choose a resistor or diode in that path`
      : `${ref} has no thermal model, so its junction temperature cannot be evaluated`,
  }]
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
