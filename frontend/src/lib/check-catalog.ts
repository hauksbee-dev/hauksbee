import { createContext, useContext, useEffect, useState } from 'react'
import { getJson, isAbort, postForm } from './api'
import { buildCheckUpload, type SupplementalDesignFiles } from './board-upload'
import { displayNet } from './net-name'
import { tomlString } from './check-spec'
import type { ActionId, BoardRequest, ComponentRatings } from '../types/report'
import { CATALOG_BLOCK } from './web-actions'

// The assertion catalogue as the click-to-assert UI reads it (`GET /api/ci/catalog`,
// version 2): the selection card's offers, the in-place editor's fields and
// pre-filled values, the "Add a check" menu and the measurement probe all work
// from this one table, so none of them keeps a vocabulary of its own. Everything
// here is pure except the fetch, the cache and the host context.

export interface DefaultFrom {
  source: 'measured' | 'measured_peak' | 'rating' | 'card_logic_levels' | 'first_uart_line' | 'constant' | 'bench'
  pct?: number
  factor?: number
  key?: string
  derate?: number
  value?: number
  fallback?: DefaultFrom
}

export interface CatalogField {
  key: string
  label: string
  unit: string | null
  type: 'number' | 'integer' | 'string' | 'boolean'
  required: boolean
  help: string
  if_absent: string | null
  default_from: DefaultFrom | null
}

/** What the engine says to do about a missing prerequisite: one id from its
 *  closed action set and the words for the button. Null when the app has no step
 *  for it (a captured trace); absent on an older server. */
export interface PrerequisiteAction { id: ActionId; label: string }

export interface Prerequisite { need: string; label: string; fix: string; action?: PrerequisiteAction | null }

export interface CatalogKind {
  kind: string
  title: string
  summary: string
  example: string
  fails_because: string
  targets: string[]
  offer: Record<string, string>
  fields: CatalogField[]
  prerequisites: Prerequisite[]
}

interface CatalogAnswer { ok?: boolean; version?: number; kinds?: CatalogKind[] }

let cached: Promise<CatalogKind[]> | null = null

/** The catalogue, fetched once per page load. A failure is not remembered, so the
 *  next mount asks again; a v1 server (no `fields`) is the same as no catalogue. */
export function loadCatalog(): Promise<CatalogKind[]> {
  cached ??= getJson<CatalogAnswer>('/api/ci/catalog')
    .then(answer => {
      const kinds = answer.ok && answer.version === 2 ? answer.kinds ?? [] : []
      if (kinds.length === 0) throw new Error('no catalogue')
      return kinds
    })
    .catch((error: unknown) => {
      cached = null
      throw error
    })
  return cached
}

/** The catalogue once it has arrived; null while loading or when the server has none. */
export function useCatalog(): CatalogKind[] | null {
  const [kinds, setKinds] = useState<CatalogKind[] | null>(null)
  useEffect(() => {
    let live = true
    loadCatalog().then(k => { if (live) setKinds(k) }).catch(() => { if (live) setKinds(null) })
    return () => { live = false }
  }, [])
  return kinds
}

/** The groups of the "Add a check" menu, in order. */
export const KIND_GROUPS: ReadonlyArray<readonly [string, readonly string[]]> = [
  ['Voltages', ['voltage', 'rail_window', 'logic_level', 'protection_trip']],
  ['Timing', ['toggle', 'pulse_width', 'fires', 'increases', 'decreases']],
  ['Currents and heat', ['max_current', 'max_temp', 'no_faults']],
  ['Firmware', ['uart', 'uart_baud', 'reaches', 'resets', 'bus', 'boot_coverage', 'peripheral']],
  ['Small-signal', ['phase_margin', 'ac_gain']],
  ['Coverage', ['model_coverage', 'hwtrace']],
]

export const groupOf = (kind: string): string =>
  KIND_GROUPS.find(([, kinds]) => kinds.includes(kind))?.[0] ?? 'Coverage'

// ── Nets ────────────────────────────────────────────────────────────────

const baseName = (net: string) => net.replace(/^.*\//, '')

/** Ground and other reference nets: a voltage window on one is meaningless. */
export function isReferenceNet(net: string): boolean {
  return /^[+-]?(?:[ADPS]?GND[A-Z0-9_]*|VSS[A-Z0-9_]*|0V|EARTH|COM)$/i.test(baseName(net))
}

/** A net no check is worth asking about: a ground, or a pin the layout left unconnected. */
export const isSkippedNet = (net: string): boolean => isReferenceNet(net) || /^unconnected-/i.test(net)

/** A rail by its name: `+3V3`, `5V`, `VCC`, `VBUS`, `VDDA`... */
export function looksLikeSupply(net: string): boolean {
  const base = baseName(net)
  return /^[+-]?\d+V\d*$/i.test(base) || /^V(?:CC|DD|BUS|IN|BAT|BATT|REF|SYS|MCU|IO|USB|A)/i.test(base)
}

/** Volts a rail's name states (`+3V3` is 3.3, `5V` is 5), else null. */
export function nominalFromName(net: string): number | null {
  const hit = /^([+-]?)(\d+)V(\d*)$/i.exec(baseName(net))
  if (!hit) return null
  const volts = Number(`${hit[2]}.${hit[3] || '0'}`)
  return hit[1] === '-' ? -volts : volts
}

/** A part's nets for voltage offers: rails first, then likely outputs, then the
 *  rest; reference nets never. */
export function rankNets(nets: readonly string[], supplyNets: ReadonlySet<string> = new Set()): string[] {
  const rank = (net: string) => {
    if (supplyNets.has(net) || looksLikeSupply(net)) return 0
    return /OUT|TX|MOSI|SCK|SCL|PWM|LED|DRV|GATE|^D\d+$/i.test(baseName(net)) ? 1 : 2
  }
  return [...new Set(nets)].filter(n => !isSkippedNet(n))
    .sort((a, b) => rank(a) - rank(b) || a.localeCompare(b))
}

// ── The spec, as far as offers need to read it ──────────────────────────

export const countAsserts = (toml: string): number => (toml.match(/^\s*\[\[assert\]\]/gm) ?? []).length

/** The runnable body: the portable spec's board/firmware header is the server's to inject. */
export function specBody(toml: string): string {
  const lines = toml.split('\n')
  const firstTable = lines.findIndex(l => l.trimStart().startsWith('['))
  const end = firstTable < 0 ? lines.length : firstTable
  return lines.filter((l, i) => i >= end
    || !/^\s*(?:board|firmware|schematic|bom|placement|variant|asbuilt|models_dir)\s*=/.test(l)).join('\n')
}

/** The spec with its `[[assert]]` tables removed: what a measurement's cache key is about. */
function withoutAsserts(toml: string): string {
  const out: string[] = []
  let inAssert = false
  for (const line of toml.split('\n')) {
    const t = line.trim()
    if (t.startsWith('[')) inAssert = t === '[[assert]]'
    if (!inAssert) out.push(line)
  }
  return out.join('\n')
}

export function scenarioIds(toml: string): string[] {
  const ids: string[] = []
  const blocks = toml.split(/^\s*\[\[scenario\]\]\s*$/m).slice(1)
  for (const block of blocks) {
    const hit = /^\s*id\s*=\s*"([^"]+)"/m.exec(block.split(/^\s*\[/m)[0])
    if (hit) ids.push(hit[1])
  }
  return ids
}

/** The nets a `[[search.sweep]]` varies, in order. */
export function sweepNets(toml: string): string[] {
  return toml.split(/^\s*\[\[search\.sweep\]\]\s*$/m).slice(1)
    .map(block => /^\s*net\s*=\s*"([^"]+)"/m.exec(block.split(/^\s*\[/m)[0])?.[1])
    .filter((n): n is string => !!n)
}

export interface SpecFacts { toml: string; hasFirmware: boolean }

/** Whether the spec already carries what a kind needs (a prerequisite's wire token). */
export function needMet(need: string, spec: SpecFacts): boolean {
  switch (need) {
    case 'firmware': return spec.hasFirmware
    case 'ac': return /^\s*\[ac\]/m.test(spec.toml)
    case 'scenario': return /^\s*\[\[scenario\]\]/m.test(spec.toml)
    case 'search': return /^\s*\[search\]/m.test(spec.toml)
    case 'supply': return /^\s*kind\s*=\s*"battery"/m.test(spec.toml)
    case 'peripheral': return /^\s*\[\[(?:peripheral|sensor)\]\]/m.test(spec.toml)
    // The bench record (what the co-sim saw on the buses, resets and UART clock)
    // is a run product, not something a spec can carry: the engine says "needs
    // bench data" and these kinds wait for it.
    case 'bench': return false
    default: return false
  }
}

/** Prerequisites the engine sends no action for, though the app can still add the
 *  block (the catalogue kind whose example holds it). The others are read from
 *  the prerequisite's `action`. */
const NO_ACTION_SOURCE: Record<string, string> = { search: 'increases', peripheral: 'peripheral' }

export interface Fix { label: string; toml?: string; pickFirmware?: boolean }

/** The one-click fix for an unmet prerequisite, from the action the engine named:
 *  the firmware picker, or a minimal block lifted from the catalogue's own
 *  example. A captured trace has no fix. */
export function fixFor(need: Prerequisite, kinds: readonly CatalogKind[]): Fix | null {
  const action = need.action
  if (action?.id === 'add_firmware') return { label: action.label, pickFirmware: true }
  const kind = action ? CATALOG_BLOCK[action.id] : NO_ACTION_SOURCE[need.need]
  const source = kind ? kinds.find(k => k.kind === kind) : undefined
  if (!source) return null
  const blocks = source.example.trim().split(/\n\s*\n/).filter(b => !b.trim().startsWith('[[assert]]'))
  return blocks.length ? { label: action?.label ?? `Add ${need.label}`, toml: `${blocks.join('\n\n')}\n` } : null
}

// ── Offers ──────────────────────────────────────────────────────────────

export interface Blocked { reason: string; need?: Prerequisite; fix?: Fix | null }

export interface Offer {
  kind: string
  entry: CatalogKind
  text: string
  net?: string
  ref?: string
  /** Pin context for the label, when the pick came from a pin. */
  pin?: string
  blocked?: Blocked
}

const speak = (template: string, net?: string, ref?: string) =>
  template.replace('{net}', net ? displayNet(net) : 'this net').replace('{ref}', ref ?? 'this part')

export function blockedBy(entry: CatalogKind, spec: SpecFacts, kinds: readonly CatalogKind[]): Blocked | undefined {
  const need = entry.prerequisites.find(p => !needMet(p.need, spec))
  if (!need) return undefined
  return { reason: need.fix, need, fix: fixFor(need, kinds) }
}

export interface OfferContext {
  kinds: readonly CatalogKind[]
  spec: SpecFacts
  supplyNets: ReadonlySet<string>
  /** Assertion kinds the bound circuit can evaluate for this part. */
  capabilities: readonly string[]
  boundKind: string | null
}

const sorted = (offers: Offer[]) => [...offers.filter(o => !o.blocked), ...offers.filter(o => o.blocked)]

/** Every check that applies to a net, blocked ones last with the reason. */
export function offersForNet(net: string, ctx: OfferContext, pin?: string): Offer[] {
  if (isSkippedNet(net)) return []
  return sorted(ctx.kinds.filter(e => e.targets.includes('net'))
    // A protection cutoff belongs to a supply; a captured trace is picked from the menu.
    .filter(e => e.kind !== 'hwtrace' && (e.kind !== 'protection_trip' || ctx.supplyNets.has(net) || looksLikeSupply(net)))
    .map(entry => ({
      kind: entry.kind, entry, net, pin,
      text: speak(entry.offer.net ?? entry.title, net),
      blocked: blockedBy(entry, ctx.spec, ctx.kinds),
    })))
}

const COMPARATORS = new Set(['comparator', 'opamp', 'timer'])

/** Every check that applies to a part, then voltage checks on its best nets. */
export function offersForPart(
  ref: string, nets: readonly string[], ctx: OfferContext, netLimit = 3, padNet?: string,
): Offer[] {
  const partKinds = ctx.kinds.filter(e => e.targets.includes('part')).filter(e =>
    !['fires', 'increases', 'decreases'].includes(e.kind) || (ctx.boundKind !== null && COMPARATORS.has(ctx.boundKind)))
  const own: Offer[] = partKinds.map(entry => {
    const words = entry.offer.part ?? entry.title
    const unmeasurable = (entry.kind === 'max_current' || entry.kind === 'max_temp') && !ctx.capabilities.includes(entry.kind)
    return {
      kind: entry.kind, entry, ref, text: speak(words, undefined, ref),
      blocked: unmeasurable
        ? {
          reason: entry.kind === 'max_current'
            ? `${ref} has no measurable through-current; choose a resistor or diode in that path`
            : `${ref} has no thermal model, so its junction temperature cannot be evaluated`,
        }
        : blockedBy(entry, ctx.spec, ctx.kinds),
    }
  })
  const voltage = ctx.kinds.find(e => e.kind === 'voltage')
  // A pad the user pointed at comes first and says which pin it is.
  const picked = padNet && !isSkippedNet(padNet) ? padNet : undefined
  const ranked = rankNets(nets, ctx.supplyNets).filter(n => n !== picked)
  const onNets: Offer[] = voltage
    ? [...(picked ? [picked] : []), ...ranked.slice(0, netLimit - (picked ? 1 : 0))].map(net => ({
      kind: 'voltage', entry: voltage, net, pin: net === picked ? net : undefined,
      text: net === picked
        ? `Pad of ${ref} on ${displayNet(net)}: ${speak(voltage.offer.net ?? voltage.title, net)}`
        : speak(voltage.offer.net ?? voltage.title, net),
    }))
    : []
  return [...sorted(own), ...onNets]
}

// ── Values: required fields, defaults, TOML ─────────────────────────────

export type Draft = Record<string, string>

/** Fields that name what the check is about, and are shown first. */
const SUBJECT_KEYS = new Set(['net', 'ref', 'supply_net', 'along', 'id'])

/** The fields shown up front (required, or pre-fillable); the rest sit under "More options". */
export function splitFields(entry: CatalogKind): { main: CatalogField[]; more: CatalogField[] } {
  const usable = entry.fields.filter(f => f.key !== 'name')
  return {
    main: usable.filter(f => f.required || f.default_from !== null || SUBJECT_KEYS.has(f.key)),
    more: usable.filter(f => !(f.required || f.default_from !== null || SUBJECT_KEYS.has(f.key))),
  }
}

/** Names that can stand in for a required field ("min ... (or give max)"). */
function alternatives(field: CatalogField): string[] {
  const note = /\((or [^)]*)\)/.exec(field.help)?.[1] ?? ''
  return [...note.matchAll(/(?:\bor|\/)\s*(?:give\s+)?([a-z][a-z_]*)/g)].map(m => m[1])
}

const blank = (value: string | undefined) => (value ?? '').trim() === ''

/** Required fields that are empty and have no filled alternative. */
export function missingRequired(entry: CatalogKind, draft: Draft): CatalogField[] {
  return entry.fields.filter(f => f.required && blank(draft[f.key])
    && !alternatives(f).some(alt => !blank(draft[alt])))
}

/** Numeric fields holding something that is not a finite number. */
export function badNumbers(entry: CatalogKind, draft: Draft): CatalogField[] {
  return entry.fields.filter(f => (f.type === 'number' || f.type === 'integer')
    && !blank(draft[f.key]) && !Number.isFinite(Number(draft[f.key])))
}

/** Periods a toggle check needs in its run to time an edge-to-edge rate. */
export const TOGGLE_PERIODS = 5

/** The run length in ms a check needs when the spec's own is shorter, or null
 *  when the kind and values name none: a blink rate needs its periods in the run
 *  (the engine refuses to time one from fewer), so adding "5 Hz" to a 200 ms
 *  spec would fail on a window the check itself never asked for. */
export function runWindowMs(kind: string, draft: Draft): number | null {
  if (kind !== 'toggle') return null
  const hz = Number(draft.freq_hz)
  return Number.isFinite(hz) && hz > 0 ? Math.ceil((TOGGLE_PERIODS * 1000) / hz / 100) * 100 : null
}

/** The `[[assert]]` table for a draft: filled fields only, in the catalogue's order. */
export function assertBlock(entry: CatalogKind, draft: Draft): string {
  let out = `[[assert]]\nkind = ${tomlString(entry.kind)}\n`
  for (const f of entry.fields) {
    const v = (draft[f.key] ?? '').trim()
    if (v === '') continue
    if (f.type === 'string') out += `${f.key} = ${tomlString(v)}\n`
    else if (f.type === 'boolean') { if (v === 'true' || v === 'false') out += `${f.key} = ${v}\n` }
    else if (Number.isFinite(Number(v))) out += `${f.key} = ${Number(v)}\n`
  }
  return out
}

/** The queued request for a draft: the exact table, nothing guessed. */
export function requestFor(entry: CatalogKind, draft: Draft, minRunMs: number | null = null): BoardRequest {
  const run = Math.max(runWindowMs(entry.kind, draft) ?? 0, minRunMs ?? 0)
  return { type: 'assert', kind: entry.kind, toml: assertBlock(entry, draft), ...(run > 0 ? { run_ms: String(run) } : {}) }
}

export interface Reading {
  value: number | null
  min: number | null
  max: number | null
  /** The text a text-judging kind (`uart`) is read against, when it is that kind. */
  text?: string | null
}

export interface DefaultSources {
  reading: Reading | null
  /** The rail the net is, from the supply list or its name. */
  nominal: number | null
  ratings: ComponentRatings | undefined
  /** The first line the firmware printed, when a run has been seen. */
  firstUartLine?: string | null
  /** Values read off the bench record, by the catalogue's `bench` key. */
  bench?: Readonly<Record<string, string | number>>
  /** Where `reading` came from, in words ("the simulated value" when absent). */
  origin?: string
}

const round3 = (x: number) => Number(x.toFixed(3))

/** A pre-filled value and the words that say where it came from, or null when the
 *  source has nothing yet (the field then stays empty rather than guessing). */
export function resolveDefault(
  from: DefaultFrom | null | undefined, src: DefaultSources,
): { value: string; note: string } | null {
  if (!from) return null
  const base = src.reading?.value ?? src.nominal
  switch (from.source) {
    case 'measured': {
      if (base === null || base === undefined) return null
      const pct = from.pct ?? 0
      const moved = pct === 0 ? base : base + (pct < 0 ? -1 : 1) * Math.abs(base) * Math.abs(pct) / 100
      const how = src.reading?.value != null ? src.origin ?? 'the simulated value' : 'the rail\'s nominal voltage'
      return { value: String(round3(moved)), note: pct === 0 ? `from ${how}` : `${pct > 0 ? '+' : '-'}${Math.abs(pct)} % of ${how}` }
    }
    case 'measured_peak': {
      const peak = src.reading?.value
      return peak == null ? null : { value: String(round3(peak * (from.factor ?? 1))), note: `${from.factor ?? 1} × the measured peak` }
    }
    case 'rating': {
      const key = from.key as keyof ComponentRatings
      const rated = src.ratings?.[key] ?? (key === 'max_current_a' ? src.ratings?.max_pin_current_a : undefined)
      if (typeof rated === 'number') {
        const derate = from.derate ?? 1
        return { value: String(round3(rated * derate)), note: `${Math.round(derate * 100)} % of the ${formatAmps(rated)} rating` }
      }
      return resolveDefault(from.fallback, src)
    }
    case 'constant':
      return from.value === undefined ? null : { value: String(from.value), note: 'a usual starting value' }
    case 'first_uart_line': {
      const line = src.firstUartLine?.trim()
      return line ? { value: line, note: 'the first line the firmware printed' } : null
    }
    case 'bench': {
      const seen = from.key === undefined ? undefined : src.bench?.[from.key]
      return seen === undefined ? null : { value: String(seen), note: 'from the bench record of this run' }
    }
    default:
      // Logic levels are read from the cards by the engine when the field is empty.
      return null
  }
}

function formatAmps(a: number): string {
  return a < 1 ? `${Number((a * 1000).toPrecision(3))} mA` : `${Number(a.toPrecision(3))} A`
}

/** Starting values where the catalogue names no source: honest round numbers, said so. */
const GAP_DEFAULTS: Record<string, string> = {
  'boot_coverage.deadline_ms': '20',
  'phase_margin.min': '45',
  'ac_gain.min': '-3',
  'protection_trip.expect_trip': 'true',
  'model_coverage.min_critical': '1',
}

const VOLT_KINDS = new Set(['voltage', 'rail_window', 'boot_coverage'])

export interface Prefill { values: Draft; notes: Record<string, string> }

/** What a freshly opened editor holds: the subject, then every field a source can fill. */
export function prefill(
  entry: CatalogKind,
  subject: { net?: string; ref?: string },
  src: DefaultSources,
  spec: SpecFacts,
): Prefill {
  const values: Draft = {}
  const notes: Record<string, string> = {}
  // A rail's name states a voltage, which says nothing about a width or a frequency.
  const sources = VOLT_KINDS.has(entry.kind) ? src : { ...src, nominal: null }
  for (const f of entry.fields) {
    if (f.key === 'net') values.net = subject.net ?? ''
    else if (f.key === 'ref') values.ref = subject.ref ?? ''
    else if (f.key === 'supply_net') values.supply_net = subject.net ?? ''
    else if (f.key === 'scenario') values.scenario = scenarioIds(spec.toml)[0] ?? ''
    else if (f.key === 'along') values.along = sweepNets(spec.toml)[0] ?? ''
    else {
      const filled = resolveDefault(f.default_from, sources)
        ?? (GAP_DEFAULTS[`${entry.kind}.${f.key}`] !== undefined
          ? { value: GAP_DEFAULTS[`${entry.kind}.${f.key}`], note: 'a starting value; tighten it to your design' }
          : null)
      values[f.key] = filled?.value ?? ''
      if (filled) notes[f.key] = filled.note
    }
  }
  return { values, notes }
}

/** The quantity a kind's preview reads: itself when the probe measures it, else
 *  the voltage of its net (a boot or window default is "a fraction of the level"). */
const MEASURABLE = new Set(['voltage', 'toggle', 'max_current', 'phase_margin', 'ac_gain', 'pulse_width'])
export function measureAs(kind: string): string | null {
  if (MEASURABLE.has(kind)) return kind
  return ['rail_window', 'boot_coverage'].includes(kind) ? 'voltage' : null
}

/** Would the check, as filled in, already fail on the reading? Null when the reading
 *  cannot say (not a number, or a kind that judges a condition). */
export function wouldFail(kind: string, draft: Draft, reading: Reading): boolean | null {
  const num = (key: string) => (blank(draft[key]) || !Number.isFinite(Number(draft[key])) ? null : Number(draft[key]))
  const lo = num('min')
  const hi = num('max')
  switch (kind) {
    case 'voltage': case 'rail_window': case 'boot_coverage': {
      const low = reading.min ?? reading.value
      const high = reading.max ?? reading.value
      if (low === null || high === null || (lo === null && hi === null)) return null
      if (kind === 'boot_coverage') return lo !== null && high < lo
      return (lo !== null && low < lo) || (hi !== null && high > hi)
    }
    case 'pulse_width': case 'phase_margin': case 'ac_gain': {
      const v = reading.value
      if (v === null || (lo === null && hi === null)) return null
      return (lo !== null && v < lo) || (hi !== null && v > hi)
    }
    case 'max_current': {
      const amps = num('amps')
      return reading.value === null || amps === null ? null : reading.value > amps
    }
    case 'toggle': {
      const f = num('freq_hz')
      const tol = num('tolerance') ?? 0.25
      return reading.value === null || f === null || f === 0 ? null : Math.abs(reading.value - f) / f > tol
    }
    case 'uart': {
      // The same substring test the engine runs on the UART text.
      const wanted = draft.contains ?? ''
      return blank(wanted) || reading.text == null ? null : !reading.text.includes(wanted)
    }
    default: return null
  }
}

// ── Observations: from something already on screen to a check ───────────
//
// A scope trace or a printed UART line is a measurement the user is already
// looking at. "Assert this" turns it into the check that says "it keeps doing
// that"; "Assert what I expected" opens the same check with the numbers left
// for the user, and the observation shown beside them so a wrong expectation
// reads "would FAIL today" before it is added. The kinds, fields and defaults
// are the catalogue's; this section only reads the observation.

export interface TracePoint { t: number; v: number }

/** What the app was looking at when the user asked: a trace, or one UART line. */
export type Observation =
  | { kind: 'trace'; net: string; points: readonly TracePoint[] }
  | { kind: 'uart'; mcu?: string; line: string; transcript: string }

/** A level swing under this is a rail with ripple, not a signal that toggles. */
const TOGGLE_SWING_V = 0.5

export interface TraceFacts {
  lo: number
  hi: number
  last: number
  /** Level changes counted with hysteresis (30 % to 70 % of the swing). */
  edges: number
  rising: number
  /** The rate, when at least two full periods were seen; null otherwise. */
  hz: number | null
}

/** The facts a trace states: its range, whether it toggles and how fast. Null
 *  for a trace with fewer than two samples. Mirrors the engine's edge count: a
 *  rate needs two full periods, fewer edges are only counted. */
export function traceFacts(points: readonly TracePoint[]): TraceFacts | null {
  if (points.length < 2) return null
  let lo = Infinity
  let hi = -Infinity
  for (const { v } of points) { lo = Math.min(lo, v); hi = Math.max(hi, v) }
  const last = points[points.length - 1].v
  const swing = hi - lo
  if (!Number.isFinite(swing)) return null
  if (swing < TOGGLE_SWING_V) return { lo, hi, last, edges: 0, rising: 0, hz: null }
  const high = lo + 0.7 * swing
  const low = lo + 0.3 * swing
  let state: 'high' | 'low' | null = null
  let edges = 0
  const risings: number[] = []
  for (const { t, v } of points) {
    if (v >= high && state !== 'high') {
      if (state !== null) { edges += 1; risings.push(t) }
      state = 'high'
    } else if (v <= low && state !== 'low') {
      if (state !== null) edges += 1
      state = 'low'
    }
  }
  const span = risings.length >= 3 ? risings[risings.length - 1] - risings[0] : 0
  return { lo, hi, last, edges, rising: risings.length, hz: span > 0 ? (risings.length - 1) / span : null }
}

export type AssertMode = 'as_seen' | 'expected'

/** What an editor opened from an observation holds and shows. */
export interface ObservedEditor {
  /** Words for the preview strip: "In this trace" / "In this output". */
  origin: string
  reading: Reading
  unit: string
  /** Shown in place of a number, for an observation that is not one. */
  note?: string
  /** Fields set from the observation (the as-seen mode), with where each came from. */
  seed: Prefill
  /** Whether the reading also pre-fills the catalogue's measured fields. */
  fillReading: boolean
  /** One plain sentence of what was seen. */
  seen: string
}

const ROUND4 = (x: number) => Number(x.toPrecision(4))

/** The catalogue kind an observation is asserted with. */
export function observationKind(obs: Observation): string {
  if (obs.kind === 'uart') return 'uart'
  const facts = traceFacts(obs.points)
  return facts && facts.edges >= 1 ? 'toggle' : 'voltage'
}

/** The editor contents for an observation; null when it holds nothing to assert
 *  (a trace with under two samples, a blank UART line). */
export function observed(obs: Observation, mode: AssertMode): ObservedEditor | null {
  const fillReading = mode === 'as_seen'
  if (obs.kind === 'uart') {
    const line = obs.line.trim()
    // Nothing printed yet is still something to expect: the user types the text.
    if (line === '' && fillReading) return null
    const seed: Prefill = { values: {}, notes: {} }
    if (fillReading) {
      seed.values.contains = line
      seed.notes.contains = /\d{3,}/.test(line)
        ? 'the line you picked; it holds a number that may change from run to run, so keep the fixed part'
        : 'the line you picked'
    }
    if (obs.mcu) seed.values.mcu = obs.mcu
    const lines = obs.transcript.split('\n').filter(l => l.trim() !== '').length
    return {
      origin: 'In this output', unit: '', fillReading, seed,
      reading: { value: null, min: null, max: null, text: obs.transcript },
      note: `${lines} line${lines === 1 ? '' : 's'} printed so far`,
      seen: line === '' ? 'Nothing printed yet' : `${obs.mcu ?? 'The firmware'} printed "${line}"`,
    }
  }
  const facts = traceFacts(obs.points)
  if (!facts) return null
  const name = displayNet(obs.net)
  const seed: Prefill = { values: {}, notes: {} }
  if (facts.edges >= 1) {
    if (facts.hz !== null) {
      return {
        origin: 'In this trace', unit: 'Hz', fillReading, seed,
        reading: { value: ROUND4(facts.hz), min: null, max: null },
        seen: `${name} toggles at ${ROUND4(facts.hz)} Hz`,
      }
    }
    // Too short to time a rate: count the edges, with the slack the engine's own
    // scaffold uses, and say so.
    if (fillReading) {
      seed.values.min_toggles = String(Math.max(1, Math.floor(facts.edges * 0.8)))
      seed.notes.min_toggles = `${facts.edges} edge${facts.edges === 1 ? '' : 's'} seen, too few to time a rate`
    }
    return {
      origin: 'In this trace', unit: 'Hz', fillReading: false, seed,
      reading: { value: null, min: null, max: null },
      note: `${facts.edges} edge${facts.edges === 1 ? '' : 's'}, too few to time a rate`,
      seen: `${name} changed level ${facts.edges} time${facts.edges === 1 ? '' : 's'}`,
    }
  }
  return {
    origin: 'In this trace', unit: 'V', fillReading, seed,
    reading: { value: ROUND4(facts.last), min: ROUND4(facts.lo), max: ROUND4(facts.hi) },
    seen: `${name} sits at ${ROUND4(facts.last)} V`,
  }
}

/** The offer an observation makes: the catalogue's entry for its kind, with the
 *  reason when the spec cannot run it yet (a UART line needs firmware). */
export function offerFromObservation(
  obs: Observation, mode: AssertMode, kinds: readonly CatalogKind[], spec: SpecFacts,
): Offer | null {
  const entry = kinds.find(k => k.kind === observationKind(obs))
  if (!entry) return null
  const net = obs.kind === 'trace' ? obs.net : undefined
  const words = entry.offer[obs.kind === 'trace' ? 'trace' : 'uart'] ?? entry.title
  return {
    kind: entry.kind, entry, net,
    text: mode === 'expected' ? 'Assert what I expected' : speak(words, net),
    blocked: blockedBy(entry, spec, kinds),
  }
}

// ── The measurement probe ───────────────────────────────────────────────

export type MeasureResult =
  | { ok: true; kind: string; value: number | null; min: number | null; max: number | null; unit: string | null
    window_ms: number | null; t_ms: number; detail: string; cached: boolean }
  | { ok: false; error: string }

const fileKey = (f: File | null) => (f ? `${f.name}:${f.size}:${f.lastModified}` : '-')

export interface MeasureInputs {
  board: File
  firmware: File | null
  schematic: File | null
  supplemental: SupplementalDesignFiles
  specToml: string
}

const probes = new Map<string, Promise<MeasureResult>>()

/** Read one quantity from the board. The block carries the question only (kind,
 *  subject, window), never the bounds, so editing a limit does not ask again. */
export function measure(block: string, inputs: MeasureInputs, signal?: AbortSignal): Promise<MeasureResult> {
  const body = specBody(inputs.specToml)
  const sup = inputs.supplemental
  const key = [
    fileKey(inputs.board), fileKey(inputs.firmware), fileKey(inputs.schematic),
    [sup.bom, sup.placement, sup.variant, sup.asbuilt, ...sup.models].map(fileKey).join(','),
    withoutAsserts(body), block,
  ].join('|')
  const hit = probes.get(key)
  if (hit) return hit
  const spec = `${body.trimEnd()}\n\n${block}`
  const form = buildCheckUpload(inputs.board, inputs.firmware, inputs.schematic, spec, inputs.supplemental)
  const asked = postForm<MeasureResult>(`/api/ci/measure?assertion=${countAsserts(body)}`, form, signal)
    .catch((e: unknown): MeasureResult => {
      if (isAbort(e)) throw e
      return { ok: false, error: e instanceof Error ? e.message : String(e) }
    })
  probes.set(key, asked)
  // An abort or a transport failure must not poison the cache.
  void asked.then(r => { if (!r.ok) probes.delete(key) }, () => probes.delete(key))
  return asked
}

/** The question for a measurement: the kind's subject and window, no limits. */
export function questionBlock(kind: string, draft: Draft): string {
  const asked = ['net', 'ref', 'after_ms', 'level']
  let out = `[[assert]]\nkind = ${tomlString(kind)}\n`
  for (const key of asked) {
    const v = (draft[key] ?? '').trim()
    if (v === '') continue
    if (key === 'after_ms') { if (Number.isFinite(Number(v))) out += `after_ms = ${Number(v)}\n` }
    else out += `${key} = ${tomlString(v)}\n`
  }
  return out
}

// ── The host: what the board surfaces need from the shell ───────────────

export interface CheckHost {
  /** Hand a request to the Checks view. */
  queue: (request: BoardRequest) => void
  openChecks: () => void
  board: File | null
  firmware: File | null
  schematic: File | null
  supplemental: SupplementalDesignFiles
  /** The spec as the Checks view has it (portable form), once that view exists. */
  specToml: string | null
  nets: string[]
  supplies: Array<{ net: string; volts: number }>
  ratings: Record<string, ComponentRatings>
}

/** Provided once by the shell; absent in the standalone demo and in unit tests. */
export const CheckHostContext = createContext<CheckHost | null>(null)
export const useCheckHost = (): CheckHost | null => useContext(CheckHostContext)

export function specFacts(host: CheckHost | null): SpecFacts {
  const toml = host?.specToml ?? ''
  return { toml, hasFirmware: !!host?.firmware && /^\s*firmware\s*=/m.test(toml) }
}

/** Run the probe, if there is a board to run it on. */
export function measureOn(host: CheckHost | null, block: string, signal?: AbortSignal): Promise<MeasureResult> | null {
  if (!host?.board) return null
  const firmware = specFacts(host).hasFirmware ? host.firmware : null
  return measure(block, {
    board: host.board, firmware, schematic: host.schematic, supplemental: host.supplemental,
    specToml: host.specToml ?? 'name = "preview"\nduration_ms = 100\n',
  }, signal)
}
