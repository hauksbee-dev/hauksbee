import { pathAt } from './toml-cst'

// Completion and hover for the spec editor, from three server sources: the
// spec's JSON Schema (`/api/ci/schema`, keys and enums, with the catalogue's
// words on every assertion property), the assertion catalogue
// (`/api/ci/catalog`, kinds and their one-liners) and the glossary the schema
// carries. The words are the CLI's; nothing here writes its own copy. Plain
// data in and out, so CodeMirror stays in the editor component.

/** The slice of a JSON Schema this reads. */
export interface JsonSchema {
  type?: string | string[]
  description?: string
  properties?: Record<string, JsonSchema>
  items?: JsonSchema | JsonSchema[]
  enum?: unknown[]
  const?: unknown
  anyOf?: JsonSchema[]
  oneOf?: JsonSchema[]
  allOf?: JsonSchema[]
  $ref?: string
  definitions?: Record<string, JsonSchema>
  'x-hauksbee-doc'?: { by_kind?: Record<string, KindDoc> }
  'x-hauksbee-glossary'?: GlossaryEntry[]
}

interface KindDoc { what?: string; unit?: string; required?: boolean; title?: string; summary?: string }
interface GlossaryEntry { term?: string; label?: string; short?: string; what_to_do?: string }

/** The slice of a catalogue v2 entry this reads. */
export interface AssistKind {
  kind: string
  title: string
  summary: string
  fields?: { key: string; help?: string; unit?: string | null; required?: boolean }[]
}

export interface AssistData {
  schema: JsonSchema | null
  kinds: readonly AssistKind[]
  nets: readonly string[]
  refs: readonly string[]
}

export interface Suggestion {
  label: string
  /** What replaces the typed word. */
  apply: string
  detail?: string
  info?: string
  type: 'property' | 'enum' | 'variable' | 'keyword'
}

export interface Hover { from: number; to: number; title: string; body: string }

function deref(root: JsonSchema, s: JsonSchema | undefined, depth = 0): JsonSchema | undefined {
  if (!s || depth > 8) return s
  if (s.$ref?.startsWith('#/definitions/')) return deref(root, root.definitions?.[s.$ref.slice(14)], depth + 1)
  const alt = s.anyOf ?? s.oneOf ?? s.allOf
  if (alt && !s.properties && !s.enum) {
    const pick = alt.map(x => deref(root, x, depth + 1)).find(x => x && x.type !== 'null')
    return pick ?? s
  }
  return s
}

/** The schema of the table a header path names (`['assert']`, `['peripheral', 'event']`). */
function tableSchema(root: JsonSchema, table: readonly string[]): JsonSchema | undefined {
  let s: JsonSchema | undefined = root
  for (const seg of table) {
    s = deref(root, s)?.properties?.[seg]
    s = deref(root, s)
    if (s && (s.items || s.type === 'array')) s = deref(root, Array.isArray(s.items) ? s.items[0] : s.items)
  }
  return deref(root, s)
}

/** A property's allowed literal values, when the schema lists them. */
function enumValues(root: JsonSchema, prop: JsonSchema | undefined): unknown[] {
  const s = deref(root, prop)
  if (!s) return []
  if (s.enum) return s.enum
  const alt = prop?.anyOf ?? prop?.oneOf ?? s.anyOf ?? s.oneOf ?? []
  return alt.flatMap(x => {
    const d = deref(root, x)
    return d?.const !== undefined ? [d.const] : d?.enum ?? []
  })
}

function propDoc(data: AssistData, table: readonly string[], key: string, kind: string | null): { title: string; body: string } | null {
  const root = data.schema
  const fromKind = data.kinds.find(k => k.kind === kind)?.fields?.find(f => f.key === key)
  const prop = root ? deref(root, tableSchema(root, table)?.properties?.[key]) : undefined
  const raw = root ? tableSchema(root, table)?.properties?.[key] : undefined
  const byKind = (raw?.['x-hauksbee-doc'] ?? prop?.['x-hauksbee-doc'])?.by_kind?.[kind ?? '']
  const text = byKind?.what ?? fromKind?.help ?? raw?.description ?? prop?.description
  const unit = byKind?.unit ?? fromKind?.unit
  const glossary = glossaryFor(data, key)
  if (!text && !glossary) return null
  const body = [text, glossary].filter(Boolean).join('\n\n')
  return { title: unit ? `${key} (${unit})` : key, body }
}

function glossaryFor(data: AssistData, word: string): string | null {
  const w = word.toLowerCase()
  const hit = (data.schema?.['x-hauksbee-glossary'] ?? [])
    .find(g => g.term?.toLowerCase() === w || g.label?.toLowerCase() === w)
  return hit ? [hit.short, hit.what_to_do].filter(Boolean).join(' ') : null
}

const NET_KEYS = new Set(['net', 'to', 'cs_net', 'net_a', 'net_b', 'output', 'input'])
const REF_KEYS = new Set(['ref', 'controller'])

/** What can go at `offset`; null when nothing useful does. */
export function suggest(text: string, offset: number, data: AssistData): { from: number; options: Suggestion[] } | null {
  const at = pathAt(text, offset)
  const root = data.schema
  // Inside an open quote the value goes in as typed, closing the quote when
  // nothing closes it yet; otherwise it goes in as a quoted string.
  const close = at.quoted && !/^["']/.test(text.slice(offset)) ? text[at.from - 1] : ''
  const quote = (v: string) => (at.quoted ? v + close : JSON.stringify(v))
  if (at.inValue) {
    let options: Suggestion[] = []
    if (at.key === 'kind' && at.table[0] === 'assert') {
      options = data.kinds.map(k => ({ label: k.kind, apply: quote(k.kind), detail: k.title, info: k.summary, type: 'keyword' }))
    } else if (NET_KEYS.has(at.key)) {
      options = data.nets.map(n => ({ label: n, apply: quote(n), type: 'variable' }))
    } else if (REF_KEYS.has(at.key)) {
      options = data.refs.map(r => ({ label: r, apply: quote(r), type: 'variable' }))
    } else if (root) {
      const prop = tableSchema(root, at.table)?.properties?.[at.key]
      options = enumValues(root, prop).filter(v => typeof v === 'string' || typeof v === 'boolean').map(v => ({
        label: String(v), apply: typeof v === 'string' ? quote(v) : String(v), type: 'enum',
      }))
    }
    return options.length ? { from: at.from, options } : null
  }
  if (/[=]/.test(text.slice(text.lastIndexOf('\n', offset - 1) + 1, offset))) return null
  const props = root ? tableSchema(root, at.table)?.properties ?? {} : {}
  const fields = data.kinds.find(k => k.kind === at.kind)?.fields
  // Inside an [[assert]] whose kind is known, offer that kind's keys first.
  const keys = at.table[0] === 'assert' && fields
    ? ['kind', ...fields.map(f => f.key)]
    : Object.keys(props)
  if (keys.length === 0) return null
  return {
    from: at.from,
    options: keys.map(key => {
      const doc = propDoc(data, at.table, key, at.kind)
      const required = fields?.find(f => f.key === key)?.required
      return { label: key, apply: `${key} = `, detail: required ? 'required' : undefined, info: doc?.body, type: 'property' }
    }),
  }
}

/** The help for the key, or the assertion kind, under `offset`. */
export function hoverAt(text: string, offset: number, data: AssistData): Hover | null {
  const ls = text.lastIndexOf('\n', offset - 1) + 1
  const le = text.indexOf('\n', offset)
  const line = text.slice(ls, le < 0 ? text.length : le)
  const m = /^(\s*)([A-Za-z0-9_.-]+)(\s*=\s*)(.*)$/.exec(line)
  if (!m) return null
  const keyFrom = ls + m[1].length
  const keyTo = keyFrom + m[2].length
  const at = pathAt(text, ls)
  if (offset >= keyFrom && offset <= keyTo) {
    const doc = propDoc(data, at.table, m[2], at.kind)
    return doc ? { from: keyFrom, to: keyTo, ...doc } : null
  }
  const valueFrom = keyTo + m[3].length
  if (m[2] === 'kind' && offset >= valueFrom) {
    const name = /^["']?([^"'\s#]*)/.exec(m[4])?.[1] ?? ''
    const k = data.kinds.find(x => x.kind === name)
    if (!k) return null
    return { from: valueFrom, to: valueFrom + m[4].replace(/\s*#.*$/, '').length, title: k.title, body: k.summary }
  }
  return null
}
