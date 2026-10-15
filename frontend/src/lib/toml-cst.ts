import { getStaticTOMLValue, parseTOML } from 'toml-eslint-parser'
import type { AST } from 'toml-eslint-parser'

// The spec text is the source of truth; this module reads it as a concrete
// syntax tree (toml-eslint-parser, MIT) and edits it with span patches. Every
// builder edit is one of the PatchOps below, applied to the exact range it is
// about, so comments, key order, spacing and number spellings anywhere else
// survive byte for byte. Nothing here touches React.

/** One `key = value` line, with the ranges an edit needs. */
export interface CstEntry {
  /** The dotted key as written (`a.b` for a dotted key). */
  key: string
  value: unknown
  /** The value literal as written (`5.00`, `"+5V"`). */
  raw: string
  valueRange: [number, number]
  /** The whole line(s) the entry occupies, newline included when there is one. */
  lineRange: [number, number]
}

/** One top-level table or array-of-tables element, with its sub-tables. */
export interface CstElement {
  /** The first key segment: `assert`, `supply`, `ac`. */
  name: string
  /** The element's position in its array (`[[assert]]` #2 is 1); null for a `[table]`. */
  index: number | null
  /** The header's dotted name as written (`peripheral.event` for a sub-table). */
  header: string
  /** Start of the header line. */
  headerStart: number
  /** Just past the header line's newline. */
  headerEnd: number
  /** The element's own range: comments directly above the header through its
   *  last content line (sub-tables included), newline included. */
  range: [number, number]
  /** Entries directly under the header (not those of a sub-table). */
  entries: CstEntry[]
  /** Sub-tables (`[[peripheral.event]]`, `[sensor.inputs]`) in order. */
  subtables: { header: string; entries: CstEntry[] }[]
}

export interface CstDoc {
  text: string
  /** Root keys before the first table. */
  root: CstEntry[]
  elements: CstElement[]
}

export interface CstError { message: string; offset: number; line: number; column: number }

export type CstRead = { ok: true; doc: CstDoc } | { ok: false; error: CstError }

/** A path to a table: [] is the root, ['assert', 2] the third `[[assert]]`,
 *  ['ac'] a plain `[ac]`. */
export type TablePath = readonly [] | readonly [string] | readonly [string, number]

/** A builder edit. Values are TOML literals as they should be written. */
export type PatchOp =
  | { op: 'setValue'; table: TablePath; key: string; value: string }
  | { op: 'insertKey'; table: TablePath; key: string; value: string }
  | { op: 'removeKey'; table: TablePath; key: string }
  | { op: 'appendTable'; name: string; body: string }
  | { op: 'removeTable'; table: TablePath }
  | { op: 'replaceBody'; table: TablePath; body: string }
  /** A raw splice: what every op's inverse is expressed as. */
  | { op: 'splice'; at: number; remove: number; insert: string }

const lineStart = (text: string, at: number) => text.lastIndexOf('\n', at - 1) + 1
const lineEnd = (text: string, at: number) => {
  const nl = text.indexOf('\n', at)
  return nl < 0 ? text.length : nl + 1
}
const lineText = (text: string, at: number) => text.slice(lineStart(text, at), lineEnd(text, at)).replace(/\r?\n$/, '')
const isBlankLine = (s: string) => s.trim() === ''
const isCommentLine = (s: string) => /^\s*#/.test(s)

const keyName = (key: AST.TOMLKey) =>
  key.keys.map(k => (k.type === 'TOMLBare' ? k.name : k.value)).join('.')

let cache: { text: string; read: CstRead } | null = null

/** Parse `text`; the last answer is memoised (the builder and the editor both
 *  read the same text on every keystroke). */
export function readCst(text: string): CstRead {
  if (cache && cache.text === text) return cache.read
  const read = parse(text)
  cache = { text, read }
  return read
}

function parse(text: string): CstRead {
  let ast: AST.TOMLProgram
  try {
    ast = parseTOML(text)
  } catch (e) {
    const err = e as { message?: string; index?: number; lineNumber?: number; column?: number }
    let offset = typeof err.index === 'number' ? Math.min(err.index, text.length) : text.length
    // An error found at the end of the input belongs to the last line written,
    // not to the empty line after the final newline.
    if (offset >= text.length) while (offset > 0 && /\s/.test(text[offset - 1])) offset--
    const before = text.slice(0, offset)
    return {
      ok: false,
      error: {
        message: err.message ?? 'not valid TOML',
        offset,
        line: before.split('\n').length,
        column: offset - (before.lastIndexOf('\n') + 1) + 1,
      },
    }
  }
  const entry = (kv: AST.TOMLKeyValue): CstEntry => {
    let value: unknown
    try { value = getStaticTOMLValue(kv.value) } catch { value = undefined }
    const start = lineStart(text, kv.range[0])
    return {
      key: keyName(kv.key),
      value,
      raw: text.slice(kv.value.range[0], kv.value.range[1]),
      valueRange: [kv.value.range[0], kv.value.range[1]],
      lineRange: [start, lineEnd(text, kv.range[1])],
    }
  }
  const root: CstEntry[] = []
  const elements: CstElement[] = []
  const counts = new Map<string, number>()
  for (const node of ast.body[0].body) {
    if (node.type === 'TOMLKeyValue') {
      root.push(entry(node))
      continue
    }
    const header = keyName(node.key)
    const first = String(node.resolvedKey[0])
    const parent = elements[elements.length - 1]
    if (node.key.keys.length > 1 && parent && parent.name === first && (
      parent.index === null || node.resolvedKey[1] === parent.index
    )) {
      parent.subtables.push({ header, entries: node.body.map(entry) })
      parent.range[1] = node.range[1]
      continue
    }
    const index = node.kind === 'array' && node.key.keys.length === 1 ? counts.get(first) ?? 0 : null
    if (index !== null) counts.set(first, index + 1)
    const headerStart = lineStart(text, node.range[0])
    elements.push({
      name: node.key.keys.length === 1 ? first : header,
      index,
      header,
      headerStart,
      headerEnd: lineEnd(text, node.key.range[1]),
      range: [headerStart, node.range[1]],
      entries: node.body.map(entry),
      subtables: [],
    })
  }
  // Settle each element's range: up over the comment lines directly above its
  // header (no blank line between), and down to the end of its last line.
  for (const el of elements) {
    let start = el.headerStart
    while (start > 0) {
      const prev = lineStart(text, start - 1)
      const line = text.slice(prev, start).replace(/\r?\n$/, '')
      if (!isCommentLine(line)) break
      start = prev
    }
    el.range = [start, lineEnd(text, Math.max(el.range[1] - 1, el.headerStart))]
  }
  // A comment line between a table's last entry and the next header belongs to
  // that next header, which already claimed it; nothing to do for the earlier one.
  return { ok: true, doc: { text, root, elements } }
}

/** The element a table path names. */
export function findElement(doc: CstDoc, table: TablePath): CstElement | undefined {
  if (table.length === 0) return undefined
  const name = table[0]
  const index = table.length === 2 ? table[1] : undefined
  return doc.elements.find(el => el.name === name && (index === undefined ? el.index === null : el.index === index))
}

function entriesOf(doc: CstDoc, table: TablePath): CstEntry[] | undefined {
  if (table.length === 0) return doc.root
  return findElement(doc, table)?.entries
}

/** Where a new key goes in a table: after its last own entry, else right after
 *  the header (the root: after its last key, else at the very top). */
function insertionPoint(doc: CstDoc, table: TablePath): number | undefined {
  const entries = entriesOf(doc, table)
  if (!entries) return undefined
  if (entries.length) return entries[entries.length - 1].lineRange[1]
  if (table.length === 0) return 0
  return findElement(doc, table)!.headerEnd
}

/** The edit an op makes, as one splice; undefined when it does not apply. */
export function opSplice(text: string, op: PatchOp): { at: number; remove: number; insert: string } | undefined {
  if (op.op === 'splice') return { at: op.at, remove: op.remove, insert: op.insert }
  if (op.op === 'appendTable') {
    const sep = text === '' ? '' : text.endsWith('\n') ? '\n' : '\n\n'
    const body = op.body === '' || op.body.endsWith('\n') ? op.body : `${op.body}\n`
    return { at: text.length, remove: 0, insert: `${sep}[[${op.name}]]\n${body}` }
  }
  const read = readCst(text)
  if (!read.ok) return undefined
  const doc = read.doc
  switch (op.op) {
    case 'setValue': {
      const hit = entriesOf(doc, op.table)?.find(e => e.key === op.key)
      if (!hit) return opSplice(text, { ...op, op: 'insertKey' })
      return { at: hit.valueRange[0], remove: hit.valueRange[1] - hit.valueRange[0], insert: op.value }
    }
    case 'insertKey': {
      if (entriesOf(doc, op.table)?.some(e => e.key === op.key)) return undefined
      const at = insertionPoint(doc, op.table)
      if (at === undefined) return undefined
      const line = `${op.key} = ${op.value}`
      // At the end of a file with no final newline, the newline goes first.
      if (at === text.length && text !== '' && !text.endsWith('\n')) return { at, remove: 0, insert: `\n${line}` }
      return { at, remove: 0, insert: `${line}\n` }
    }
    case 'removeKey': {
      const hit = entriesOf(doc, op.table)?.find(e => e.key === op.key)
      if (!hit) return undefined
      let start = hit.lineRange[0]
      const end = hit.lineRange[1]
      // The last line with no final newline: take the line break before it.
      if (end === text.length && !text.endsWith('\n') && start > 0) start -= text.endsWith('\r\n', start) ? 2 : 1
      return { at: start, remove: end - start, insert: '' }
    }
    case 'removeTable': {
      const el = findElement(doc, op.table)
      if (!el) return undefined
      let [start, end] = el.range
      // Take one blank separator line with it, so the gap does not double up.
      if (start > 0 && isBlankLine(lineText(text, start - 1))) start = lineStart(text, start - 1)
      else if (end < text.length && isBlankLine(lineText(text, end))) end = lineEnd(text, end)
      return { at: start, remove: end - start, insert: '' }
    }
    case 'replaceBody': {
      const el = findElement(doc, op.table)
      if (!el) return undefined
      const body = op.body === '' || op.body.endsWith('\n') ? op.body : `${op.body}\n`
      return { at: el.headerEnd, remove: el.range[1] - el.headerEnd, insert: body }
    }
  }
}

/** Apply one op. Returns the new text and the op that undoes it exactly, or
 *  null when the op does not apply (the path is gone, the text does not parse). */
export function applyOp(text: string, op: PatchOp): { text: string; inverse: PatchOp } | null {
  const s = opSplice(text, op)
  if (!s) return null
  const removed = text.slice(s.at, s.at + s.remove)
  return {
    text: text.slice(0, s.at) + s.insert + text.slice(s.at + s.remove),
    inverse: { op: 'splice', at: s.at, remove: s.insert.length, insert: removed },
  }
}

/** Apply ops in order (each against the text the previous one left); an op
 *  that no longer applies is skipped. */
export function applyOps(text: string, ops: readonly PatchOp[]): string {
  let out = text
  for (const op of ops) out = applyOp(out, op)?.text ?? out
  return out
}

/** A string as a TOML basic string (JSON escaping is valid TOML escaping for
 *  the characters it produces). */
export const tomlStringLiteral = (v: string) => JSON.stringify(v)

/** A TOML number literal exactly as typed, or null when TOML would reject it
 *  (`1.`, `.5`, `-`, `1e`): only a valid literal is ever written. */
export function tomlNumberLiteral(v: string): string | null {
  const t = v.trim()
  return /^[+-]?(0|[1-9](_?\d)*)(\.\d(_?\d)*)?([eE][+-]?\d(_?\d)*)?$/.test(t) ? t : null
}

/** The lexical context at `offset`, for completion and hover. Lexical on
 *  purpose: while someone types, the text usually does not parse. */
export interface CursorPath {
  /** The table header the cursor is under: [] root, ['assert'], ['peripheral', 'event']. */
  table: string[]
  /** Which `[[name]]` element (0-based), when the table is an array element. */
  index: number | null
  /** The key on this line, when the cursor is right of `=`; else the partial key. */
  key: string
  /** True right of `=`. */
  inValue: boolean
  /** Start of the word being completed. */
  from: number
  /** The text typed so far for the word being completed. */
  typed: string
  /** True when the value being typed opened a quote. */
  quoted: boolean
  /** The `kind = "..."` of the table, when it has one. */
  kind: string | null
}

const HEADER = /^\s*\[\[?\s*([^\]]+?)\s*\]\]?/

export function pathAt(text: string, offset: number): CursorPath {
  const ls = lineStart(text, offset)
  const before = text.slice(ls, offset)
  let table: string[] = []
  let index: number | null = null
  let headerAt = -1
  // Walk up to the nearest header, counting earlier elements of the same array.
  let pos = ls
  while (pos > 0) {
    const prev = lineStart(text, pos - 1)
    const m = HEADER.exec(text.slice(prev, pos))
    if (m) {
      table = m[1].split('.').map(s => s.trim().replace(/^"|"$/g, ''))
      headerAt = prev
      break
    }
    pos = prev
  }
  if (headerAt >= 0 && /^\s*\[\[/.test(text.slice(headerAt, headerAt + 64))) {
    const name = table.join('.')
    const re = new RegExp(`^\\s*\\[\\[\\s*${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*\\]\\]`, 'gm')
    index = (text.slice(0, headerAt).match(re) ?? []).length
  }
  // The table's kind, from its own lines.
  let kind: string | null = null
  if (headerAt >= 0) {
    const rest = text.slice(lineEnd(text, headerAt))
    const next = rest.search(/^\s*\[/m)
    const body = next < 0 ? rest : rest.slice(0, next)
    kind = /^\s*kind\s*=\s*["']([^"'\n]*)["']/m.exec(body)?.[1] ?? null
  }
  const kv = /^\s*([A-Za-z0-9_.-]+)\s*=\s*(.*)$/.exec(before)
  if (kv) {
    const value = kv[2]
    const quote = /^["']([^"']*)$/.exec(value)
    const word = quote ? quote[1] : /[^\s,[\]{}"']*$/.exec(value)?.[0] ?? ''
    return {
      table, index, key: kv[1], inValue: true, from: offset - word.length, typed: word, quoted: !!quote, kind,
    }
  }
  const word = /[A-Za-z0-9_-]*$/.exec(before)?.[0] ?? ''
  return { table, index, key: word, inValue: false, from: offset - word.length, typed: word, quoted: false, kind }
}

/** Every `[[assert]]` header's line start, in order (lexical, so a gutter can
 *  still mark results while the text does not parse). */
export function assertHeaderOffsets(text: string): number[] {
  const out: number[] = []
  const re = /^[ \t]*\[\[[ \t]*assert[ \t]*\]\]/gm
  for (let m = re.exec(text); m; m = re.exec(text)) out.push(m.index)
  return out
}
