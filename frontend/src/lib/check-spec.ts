import { parse as parseToml } from 'smol-toml'
import type { ScaffoldSupply } from '../types/report'

// The checks spec, as a data model: the builder's rows, the TOML they compose
// into, and the round-trip back. The Checks view renders these; nothing here
// touches React, so the composer and the parser can be tested on their own.

export interface SupplyRow {
  net: string
  volts: string
  /** The source behind the rail; absent means an ideal one (rows saved before
   *  kinds existed). The scaffold seeds `bench` and `usb`, as `hauksbee-ci init`. */
  kind?: 'ideal' | 'bench' | 'usb'
  /** A usb source's negotiated profile, e.g. `5v1.5a`. */
  usb?: string
  /** A bench source's fold-back current, in A. */
  limit?: string
}

export const USB_PROFILES = ['5v0.5a', '5v1.5a', '5v3a']

/** The source kinds a checks spec can declare; `labelFor` has the words for each. */
export const SUPPLY_KINDS = ['ideal', 'bench', 'usb'] as const

/** Builder rows for the supplies `hauksbee-ci init` declares. */
export function scaffoldRows(supplies: ScaffoldSupply[]): SupplyRow[] {
  return supplies.map(s => ({
    net: s.net,
    volts: String(s.volts),
    kind: s.kind,
    ...(s.usb ? { usb: s.usb } : {}),
    ...(s.current_limit_a ? { limit: String(s.current_limit_a) } : {}),
  }))
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
  overrides: OverrideRow[]
  /** References left open as not fitted (`no_fit`). */
  noFit: string[]
}

/** An `[[override]]`: `select` settles an open variant selection for `ref`. */
export interface OverrideRow {
  ref: string
  select?: string
  value?: string
  reason?: string
}

/** Replace any override for the same ref (a re-pick never duplicates). */
export const upsertOverride = (rows: OverrideRow[], row: OverrideRow) =>
  [...rows.filter(r => r.ref !== row.ref), row]

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

export const CHECK_KEYS: CheckKey[] = [
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
  const known = checkKind(kind)
  if (!known) return null
  const id = typeof saved.id === 'number' && Number.isFinite(saved.id) ? saved.id : fallbackId
  const row = { ...emptyCheck(id, known.kind), ...saved, kind: known.kind } as CheckRow
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

/** One [[assert]] kind's FORM schema. Its words (title, group, hint) live in the
 *  served catalogue (`lib/check-catalog`), the one copy; this holds what only the
 *  builder needs: the schema the composer, the round-trip parser, the preflight and the row
 *  form all read, so the four cannot drift. */
export interface CheckKind {
  kind: string
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
    kind: 'voltage',
    subject: 'net',
    fields: [V('min', 'min V'), V('max', 'max V'), V('after_ms', 'after ms')],
    needs: [{ any: ['min', 'max'], message: 'needs a min V and/or a max V' }],
  },
  {
    kind: 'rail_window',
    subject: 'net',
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
    kind: 'no_faults',
    fields: [], needs: [],
  },
  {
    kind: 'max_current',
    subject: 'ref',
    fields: [V('amps', 'max A')],
    needs: [{ any: ['amps'], message: 'max A is empty' }],
  },
  {
    kind: 'max_temp',
    subject: 'ref',
    // max °C may stay blank (falls back to the part's own rating).
    fields: [V('celsius', 'max °C (blank = part rating)', 70)],
    needs: [],
  },
  {
    kind: 'uart',
    fields: [{ key: 'contains', label: 'must print', width: 220, placeholder: 'hello', text: true }],
    needs: [{ any: ['contains'], message: '"must print" is empty' }],
  },
  {
    kind: 'toggle',
    subject: 'net',
    fields: [V('freq_hz', 'freq Hz'), V('tolerance', '±tol', 56), V('min_toggles', 'or min toggles')],
    needs: [{ any: ['freq_hz', 'min_toggles'], message: 'needs a freq Hz or a min toggles' }],
  },
  {
    kind: 'boot_coverage',
    subject: 'net',
    fields: [V('min', 'reach V'), V('deadline_ms', 'within ms')],
    needs: [{ any: ['min'], message: 'reach V is empty' }, { any: ['deadline_ms'], message: 'within ms is empty' }],
  },
]

/** The builder's schema for a kind. The hyphenated spelling an older build wrote
 *  (`boot-coverage`) is the same kind as the engine's `boot_coverage`. */
export const checkKind = (kind: string): CheckKind | undefined =>
  CHECK_KINDS.find(k => k.kind === (kind === 'boot-coverage' ? 'boot_coverage' : kind))

/** The TOML keys an [[assert]] of this kind may carry: the composer writes
 *  exactly these, so the parser refuses anything else (it would survive the
 *  parse but vanish from the round-tripped spec). */
export function assertKeys(k: CheckKind): Set<string> {
  return new Set([
    'kind',
    ...(k.subject ? [k.subject] : []),
    ...k.fields.filter(f => !f.ui).map(f => f.key),
  ])
}

export function tomlString(v: string): string {
  return JSON.stringify(v)
}

/** The trimmed value when it is a TOML number literal, else null (a value
 *  TOML would reject, like `1.` or `05`, is never written). */
export function numOr(v: string): string | null {
  const t = v.trim()
  return /^[+-]?(0|[1-9](_?\d)*)(\.\d(_?\d)*)?([eE][+-]?\d(_?\d)*)?$/.test(t) ? t : null
}

/** One `key = literal` pair, in the order it is written. */
export type Kv = [key: string, literal: string]

export const lines = (kvs: readonly Kv[]) => kvs.map(([k, v]) => `${k} = ${v}\n`).join('')

/** The keys one [[supply]] carries (net only when named). */
export function supplyKvs(s: SupplyRow): Kv[] {
  const kind = s.kind ?? 'ideal'
  const out: Kv[] = []
  if (s.net.trim()) out.push(['net', tomlString(s.net.trim())])
  out.push(['kind', `"${kind}"`])
  if (kind === 'usb') return [...out, ['usb', tomlString(s.usb || USB_PROFILES[1])]]
  out.push(['volts', numOr(s.volts) ?? '5.0'])
  const limit = kind === 'bench' ? numOr(s.limit ?? '') : null
  if (limit) out.push(['current_limit_a', limit])
  return out
}

/** One [[supply]] block, or nothing for a row with no net. */
export function supplyToml(s: SupplyRow): string {
  return s.net.trim() ? `\n[[supply]]\n${lines(supplyKvs(s))}` : ''
}

export function peripheralKvs(p: PeripheralRow): Kv[] {
  const out: Kv[] = [['id', tomlString(p.id.trim())], ['type', tomlString(p.kind)], ['net', tomlString(p.net.trim())]]
  if (p.kind === 'stimulus') {
    out.push(['waveform', tomlString(p.waveform)], ['offset', numOr(p.offset) ?? '0'])
    if (p.waveform !== 'dc') out.push(['amplitude', numOr(p.amplitude) ?? '1'], ['freq_hz', numOr(p.freq_hz) ?? '1000'])
  } else {
    if (p.to.trim()) out.push(['to', tomlString(p.to.trim())])
    if (p.kind === 'pushbutton' && numOr(p.bounce_ms)) out.push(['bounce_ms', numOr(p.bounce_ms)!])
    if (numOr(p.initial)) out.push(['initial', numOr(p.initial)!])
  }
  return out
}

export const eventsToml = (p: PeripheralRow) => p.events
  .map(event => `[[peripheral.event]]\nt_ms = ${numOr(event.t_ms) ?? '0'}\nvalue = ${numOr(event.value) ?? '0'}\n`).join('')

export function peripheralToml(p: PeripheralRow): string {
  return `\n[[peripheral]]\n${lines(peripheralKvs(p))}${eventsToml(p)}`
}

export function sensorKvs(sensor: SensorRow): Kv[] {
  const out: Kv[] = [['id', tomlString(sensor.id.trim())], ['spec', tomlString(sensor.spec)]]
  if (sensor.controller.trim()) out.push(['controller', tomlString(sensor.controller.trim())])
  if (sensor.csNet.trim()) out.push(['cs_net', tomlString(sensor.csNet.trim())])
  return out
}

export const inputsToml = (sensor: SensorRow) => {
  const inputs = sensor.inputs.filter(input => input.name.trim() && numOr(input.value))
  if (inputs.length === 0) return ''
  return `[sensor.inputs]\n${inputs.map(input => `${tomlString(input.name.trim())} = ${numOr(input.value)}\n`).join('')}`
}

/** The spec bytes are kept inline (JSON string escaping is valid TOML
 *  basic-string escaping) so a downloaded check is self-contained. */
export function sensorToml(sensor: SensorRow): string {
  return `\n[[sensor]]\n${lines(sensorKvs(sensor))}${inputsToml(sensor)}`
}

export function overrideKvs(o: OverrideRow): Kv[] {
  const out: Kv[] = [['ref', tomlString(o.ref)]]
  for (const key of ['select', 'value', 'reason'] as const) {
    if (o[key] !== undefined) out.push([key, tomlString(o[key])])
  }
  return out
}

export function overrideToml(o: OverrideRow): string {
  return `\n[[override]]\n${lines(overrideKvs(o))}`
}

/** The keys one [[assert]] carries: its kind, its subject, then its schema
 *  fields in order, each only when filled (strings quoted, numbers only when
 *  they are TOML numbers). */
export function assertKvs(c: ConstraintDraft): Kv[] {
  const k = checkKind(c.kind)
  const out: Kv[] = [['kind', tomlString(c.kind)]]
  if (!k) return out
  if (k.subject && c[k.subject].trim()) out.push([k.subject, tomlString(c[k.subject].trim())])
  for (const f of k.fields) {
    if (f.ui || (f.active && !f.active(c))) continue
    const t = c[f.key].trim()
    if (!t) continue
    if (f.text) out.push([f.key, tomlString(t)])
    else if (numOr(t)) out.push([f.key, numOr(t)!])
  }
  return out
}

/** One [[assert]] block. */
export function assertToml(c: ConstraintDraft): string {
  return `\n[[assert]]\n${lines(assertKvs(c))}`
}

/** Compose a whole spec BODY from rows (no board/firmware keys; the server
 *  injects those from the uploaded files). Used only to start a spec (a new
 *  board, a save from before the text was the source of truth); every edit
 *  after that is a patch on the text. */
export function buildToml(
  name: string,
  duration: string,
  supplies: SupplyRow[],
  peripherals: PeripheralRow[],
  checks: readonly (ConstraintDraft | CheckRow)[],
  sensors: SensorRow[] = [],
  overrides: OverrideRow[] = [],
  noFit: string[] = [],
): string {
  const dur = numOr(duration)
  return `name = ${tomlString(name)}\n${dur ? `duration_ms = ${dur}\n` : ''}`
    + (noFit.length ? `no_fit = [${noFit.map(tomlString).join(', ')}]\n` : '')
    + supplies.map(supplyToml).join('')
    + peripherals.map(peripheralToml).join('')
    + sensors.map(sensorToml).join('')
    + checks.map(assertToml).join('')
    + overrides.map(overrideToml).join('')
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
