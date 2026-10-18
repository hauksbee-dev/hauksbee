// The guided model form and the TOML text are two views of ONE string. The
// form reads the text (smol-toml) and writes back by editing single lines, never
// by regenerating the file, so everything the form does not know about (pin
// maps, logic, comments, the author's own notes) survives a form edit and a
// TOML edit shows up in the form at once.

import { parse } from 'smol-toml'

export type Json = Record<string, unknown>

export type ParsedModel =
  | { ok: true; model: Json }
  | { ok: false; error: string }

/** The first `[[models]]` entry of the text, or why it cannot be read. */
export function readModel(text: string): ParsedModel {
  try {
    const doc = parse(text) as Json
    const models = doc.models
    const first = Array.isArray(models) ? models[0] : undefined
    if (!first || typeof first !== 'object') return { ok: false, error: 'there is no [[models]] entry yet' }
    return { ok: true, model: first as Json }
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message.split('\n')[0] : String(e) }
  }
}

const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
const headerRe = (header: string) => new RegExp(`^\\s*${escapeRe(header)}\\s*(#.*)?$`)
const commentedHeaderRe = (header: string) => new RegExp(`^\\s*#\\s*${escapeRe(header)}\\s*$`)
const anyHeader = /^\s*\[/

/** A TOML basic string. */
export const tomlString = (s: string) => JSON.stringify(s)

/** A number as a TOML float (an integer gets a `.0`, so a parameter the solver
 *  reads as a float never arrives as an integer), or null when it is not one. */
export function tomlNumber(raw: string): string | null {
  const s = raw.trim()
  if (!/^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(s)) return null
  const n = Number(s)
  if (!Number.isFinite(n)) return null
  const out = String(n)
  return /^-?\d+$/.test(out) ? `${out}.0` : out
}

/**
 * Set (or, with `literal === null`, remove) `key` in the table opened by
 * `header` (for example `[models.params]` or `[[models.source.references]]`).
 * The first matching table is the one edited. A table that is only present as a
 * commented-out line in the scaffold is switched on; one that is absent is
 * appended at the end of the file.
 */
export function setKey(text: string, header: string, key: string, literal: string | null): string {
  const lines = text.split('\n')
  const keyRe = new RegExp(`^\\s*${escapeRe(key)}\\s*=`)
  let at = lines.findIndex(l => headerRe(header).test(l))
  if (at < 0) {
    if (literal === null) return text
    const commented = lines.findIndex(l => commentedHeaderRe(header).test(l))
    if (commented >= 0) {
      lines[commented] = header
      at = commented
    } else {
      const trimmed = text.replace(/\n*$/, '')
      return `${trimmed}\n\n${header}\n${key} = ${literal}\n`
    }
  }
  let end = lines.length
  for (let i = at + 1; i < lines.length; i++) if (anyHeader.test(lines[i])) { end = i; break }
  for (let i = at + 1; i < end; i++) {
    if (!keyRe.test(lines[i])) continue
    if (literal === null) lines.splice(i, 1)
    else lines[i] = `${key} = ${literal}`
    return lines.join('\n')
  }
  if (literal === null) return text
  lines.splice(at + 1, 0, `${key} = ${literal}`)
  return lines.join('\n')
}

/** The keys that make a model "identity only": it binds by name and stays open. */
export const IDENTITY_KEYS = ['identity_only', 'warning', 'unlocked_by'] as const

export function isIdentityOnly(model: Json): boolean {
  const params = model.params
  return !!params && typeof params === 'object' && (params as Json).identity_only === true
}

/** The numeric parameters a model sets, in file order. */
export function numericParams(model: Json): [string, number][] {
  const params = model.params
  if (!params || typeof params !== 'object') return []
  return Object.entries(params as Json)
    .filter(([k, v]) => !(IDENTITY_KEYS as readonly string[]).includes(k) && (typeof v === 'number' || typeof v === 'bigint'))
    .map(([k, v]) => [k, Number(v)])
}

/** Parameter names the checker says are missing, with its alternative spelling
 *  when it accepts either (`rail_lo` or `swing_lo`). */
export function missingParams(error: string): { key: string; alt?: string }[] {
  const out: { key: string; alt?: string }[] = []
  for (const m of error.matchAll(/missing required param '([^']+)'(?: or '([^']+)')?/g)) out.push({ key: m[1], alt: m[2] })
  return out
}

/** A parameter in the words an engineer reads it by, and its unit. */
const PARAMS: Record<string, [label: string, unit: string]> = {
  vto: ['Threshold voltage (Vto)', 'V'],
  kp: ['Transconductance (Kp)', 'A/V²'],
  is: ['Saturation current (Is)', 'A'],
  bf: ['Forward current gain (Bf)', ''],
  nf: ['Forward emission coefficient (Nf)', ''],
  n: ['Ideality factor (n)', ''],
  rs: ['Series resistance (Rs)', 'Ω'],
  vout: ['Output voltage', 'V'],
  dropout_v: ['Dropout voltage', 'V'],
  iq_a: ['Quiescent current', 'A'],
  gain: ['Open-loop gain', ''],
  rail_lo: ['Output low, near the supply rail', 'V'],
  rail_hi: ['Output high, near the supply rail', 'V'],
  swing_lo: ['Output swing, low end', 'V'],
  swing_hi: ['Output swing, high end', 'V'],
  out_lo: ['Output low level', 'V'],
  out_hi: ['Output high level', 'V'],
  hysteresis: ['Hysteresis', 'V'],
  threshold_frac: ['Threshold, fraction of supply', ''],
  trigger_frac: ['Trigger, fraction of supply', ''],
  divider_ohms: ['Divider resistance', 'Ω'],
  rsat_ohms: ['Output saturation resistance', 'Ω'],
  ron: ['On resistance', 'Ω'],
  roff: ['Off resistance', 'Ω'],
}

export function paramInfo(key: string): { label: string; unit: string } {
  const known = PARAMS[key]
  if (known) return { label: known[0], unit: known[1] }
  const words = key.replace(/_/g, ' ')
  return { label: words.charAt(0).toUpperCase() + words.slice(1), unit: '' }
}

/** The names behind a list of missing parameters, as a sentence fragment. */
export function missingSentence(missing: { key: string; alt?: string }[]): string {
  return missing.map(m => paramInfo(m.key).label).join(', ')
}

/** The source reference the form edits: the first `[[models.source.references]]`. */
export interface SourceRef { url: string; title: string; locator: string }

export function readSource(model: Json): SourceRef {
  const source = model.source
  const refs = source && typeof source === 'object' ? (source as Json).references : undefined
  const first = Array.isArray(refs) ? refs[0] as Json | undefined : undefined
  const text = (v: unknown) => (typeof v === 'string' ? v : '')
  return { url: text(first?.url), title: text(first?.title), locator: text(first?.locator) }
}

const REF_HEADER = '[[models.source.references]]'

/** Write a complete source reference (all three of url, title and locator are
 *  required by the checker, so a half-filled one is not written). */
export function writeSource(text: string, ref: SourceRef): string {
  let out = text
  for (const key of ['url', 'title', 'locator'] as const) out = setKey(out, REF_HEADER, key, tomlString(ref[key].trim()))
  return out
}

/** True when the model describes any behaviour beyond naming the part. */
export function describesBehaviour(model: Json): boolean {
  if (isIdentityOnly(model)) return false
  if (numericParams(model).length > 0) return true
  return ['logic', 'behavioral', 'peripheral', 'current_program', 'pins'].some(k => {
    const v = model[k]
    return !!v && typeof v === 'object' && Object.keys(v as Json).length > 0
  })
}
