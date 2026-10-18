import { createContext, useContext, useEffect, useState } from 'react'
import { getJson, isAbort, postForm } from './api'
import { buildCheckUpload, type SupplementalDesignFiles } from './board-upload'
import { displayNet } from './net-name'
import { tomlString } from './check-spec'
import type { BoardRequest, ComponentRatings } from '../types/report'

// The assertion catalogue as the click-to-assert UI reads it (`GET /api/ci/catalog`,
// version 2): the selection card's offers, the in-place editor's fields and
// pre-filled values, the "Add a check" menu and the measurement probe all work
// from this one table, so none of them keeps a vocabulary of its own. Everything
// here is pure except the fetch, the cache and the host context.

export interface DefaultFrom {
  source: 'measured' | 'measured_peak' | 'rating' | 'card_logic_levels' | 'first_uart_line' | 'constant'
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

export interface Prerequisite { need: string; label: string; fix: string }

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
  ['Firmware', ['uart', 'boot_coverage', 'peripheral']],
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
    default: return false
  }
}

/** Which catalogue example holds the minimal block that satisfies a prerequisite. */
const NEED_SOURCE: Record<string, string> = {
  ac: 'ac_gain', scenario: 'rail_window', search: 'increases', supply: 'protection_trip', peripheral: 'peripheral',
}

export interface Fix { label: string; toml?: string; pickFirmware?: boolean }

/** The one-click fix for an unmet prerequisite: a minimal block lifted from the
 *  catalogue's own example, or the firmware picker. A captured trace has no fix. */
export function fixFor(need: Prerequisite, kinds: readonly CatalogKind[]): Fix | null {
  if (need.need === 'firmware') return { label: 'Add firmware', pickFirmware: true }
  const source = kinds.find(k => k.kind === NEED_SOURCE[need.need])
  if (!source) return null
  const blocks = source.example.trim().split(/\n\s*\n/).filter(b => !b.trim().startsWith('[[assert]]'))
  return blocks.length ? { label: `Add ${need.label}`, toml: `${blocks.join('\n\n')}\n` } : null
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
  template.replace('{net}', net ? displayNet(net) : 'this net').replace('{ref}', ref ?? 'this part').replace(/\.\.\./g, '…')

/** Part offers say what is measured, not just the part's name. They are
 *  ACTIONS ("Set a temperature limit for U3"): the engine's own wording ends
 *  in a literal "... C" placeholder, which read as a sentence cut short. The
 *  number is the user's to give (a rating is never guessed), and the check
 *  editor that opens asks for it. */
const PART_WORDS: Record<string, string> = {
  max_current: 'Set a current limit for {ref}',
  max_temp: 'Set a temperature limit for {ref}',
}

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
    const words = PART_WORDS[entry.kind] ?? entry.offer.part ?? entry.title
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
export const requestFor = (entry: CatalogKind, draft: Draft): BoardRequest =>
  ({ type: 'assert', kind: entry.kind, toml: assertBlock(entry, draft) })

export interface Reading {
  value: number | null
  min: number | null
  max: number | null
}

export interface DefaultSources {
  reading: Reading | null
  /** The rail the net is, from the supply list or its name. */
  nominal: number | null
  ratings: ComponentRatings | undefined
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
      const how = src.reading?.value != null ? 'the simulated value' : 'the rail\'s nominal voltage'
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
    default:
      // Logic levels are read from the cards by the engine when the field is empty;
      // the first printed line has no source outside a running firmware.
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
    default: return null
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
