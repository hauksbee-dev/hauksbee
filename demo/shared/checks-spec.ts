// A faithful mirror of the checks builder's spec composer
// (frontend/src/components/ChecksView.tsx `buildToml`) and of the shape it
// autosaves to localStorage.
//
// Two callers need it and neither may import from frontend/src's private
// internals: the recorder composes the spec text it POSTs to a real engine, and
// the embed seeds the panel's saved state so the surface opens on the rows that
// were actually recorded. Because the cache key is canonicalised
// (demo/shared/spec-key.ts), a cosmetic drift between this composer and the
// panel's is harmless; a SEMANTIC drift (a field written here and not there)
// would cost a cache hit, which is why every field is mirrored explicitly.

export interface SupplyRow {
  net: string
  volts: string
}

/** Every field of the builder's CheckRow except its ephemeral `id`. */
export interface CheckRowSeed {
  kind: string
  net?: string
  ref?: string
  min?: string
  max?: string
  after_ms?: string
  deadline_ms?: string
  contains?: string
  freq_hz?: string
  tolerance?: string
  min_toggles?: string
  amps?: string
  celsius?: string
  dip_below?: string
  for_max_ms?: string
  recover_to?: string
  recover_within_ms?: string
}

const NET_KINDS = ['voltage', 'toggle', 'boot-coverage', 'rail_window']
const REF_KINDS = ['max_current', 'max_temp']

/** The builder's field order, which is also the order buildToml writes them. */
export const ROW_FIELDS = [
  'min', 'max', 'after_ms', 'deadline_ms', 'freq_hz', 'tolerance',
  'min_toggles', 'amps', 'celsius', 'dip_below', 'for_max_ms',
  'recover_to', 'recover_within_ms',
] as const

const tomlString = (v: string) => JSON.stringify(v)

const numOr = (v: string): string | null => {
  const t = v.trim()
  if (t === '') return null
  return Number.isFinite(Number(t)) ? t : null
}

export function buildToml(
  name: string,
  duration: string,
  supplies: SupplyRow[],
  checks: CheckRowSeed[],
): string {
  let out = `name = ${tomlString(name)}\n`
  const dur = numOr(duration)
  if (dur) out += `duration_ms = ${dur}\n`
  for (const s of supplies) {
    if (!s.net.trim()) continue
    out += `\n[[supply]]\nnet = ${tomlString(s.net.trim())}\nkind = "ideal"\nvolts = ${numOr(s.volts) ?? '5.0'}\n`
  }
  for (const c of checks) {
    out += `\n[[assert]]\nkind = ${tomlString(c.kind)}\n`
    const put = (key: string, v: string | undefined, quote = false) => {
      const t = (v ?? '').trim()
      if (!t) return
      out += quote ? `${key} = ${tomlString(t)}\n` : (numOr(t) ? `${key} = ${numOr(t)}\n` : '')
    }
    if (NET_KINDS.includes(c.kind)) put('net', c.net, true)
    if (REF_KINDS.includes(c.kind)) put('ref', c.ref, true)
    if (c.kind === 'uart') put('contains', c.contains, true)
    for (const f of ROW_FIELDS) put(f, c[f])
  }
  return out
}

/** The exact payload the panel autosaves (and restores) per board. */
export interface SavedChecksState {
  specName: string
  duration: string
  supplies: SupplyRow[]
  checks: (CheckRowSeed & { id: number })[]
  rawMode: boolean
  rawText: string
}

/** Fill a seed row out to every field the panel expects, so a restored row is
 *  indistinguishable from one the builder created. */
export function fullRow(seed: CheckRowSeed, id: number): CheckRowSeed & { id: number } {
  return {
    id,
    kind: seed.kind,
    net: seed.net ?? '',
    ref: seed.ref ?? '',
    min: seed.min ?? '',
    max: seed.max ?? '',
    after_ms: seed.after_ms ?? '',
    deadline_ms: seed.deadline_ms ?? '',
    contains: seed.contains ?? '',
    freq_hz: seed.freq_hz ?? '',
    tolerance: seed.tolerance ?? '',
    min_toggles: seed.min_toggles ?? '',
    amps: seed.amps ?? '',
    celsius: seed.celsius ?? '',
    dip_below: seed.dip_below ?? '',
    for_max_ms: seed.for_max_ms ?? '',
    recover_to: seed.recover_to ?? '',
    recover_within_ms: seed.recover_within_ms ?? '',
  }
}

/** The panel's own storage key (mirrors ChecksView.checksStorageKey). */
export const checksStorageKey = (r: { file_name: string; num_components: number; num_nets: number }) =>
  `hauksbee.checks.${r.file_name}:${r.num_components}:${r.num_nets}`

/** The panel's default spec name for a board. */
export const defaultSpecName = (r: { board_name?: string; file_name: string }) =>
  `${r.board_name || r.file_name} checks`
