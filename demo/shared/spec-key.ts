// The check-spec cache key, shared by the recorder (demo/capture) and the
// embed's cached transport (demo/embed). Both sides MUST agree byte for byte
// or a recorded run would look un-recorded, so the canonicalisation lives in
// exactly one file.
//
// A key is a canonical string, not a hash: when a run misses the cache the
// key itself is the debugging output, and a browser has no synchronous hash.
//
// What the key ignores, deliberately:
//   - the spec's `name` (the builder types it; it changes nothing about the run)
//   - comments, blank lines, key order inside a block, number spelling
//     (3.0 / 3 / 3.00 are one number)
//   - the order of [[supply]] blocks
// What it does NOT ignore: the assertions, their fields, the duration, the
// supplies, and whether a firmware was attached. Those change the verdict.

export interface CanonAssert {
  /** Canonical assertion kind (`boot-coverage` folded to `boot_coverage`). */
  kind: string
  /** Every other field, sorted, values normalised. */
  fields: Record<string, string | number>
}

export interface CanonSpec {
  duration_ms: number | null
  /** Top-level keys other than name/duration_ms (mcu, frame_ms, ...). */
  top: Record<string, string | number>
  supplies: Record<string, string | number>[]
  asserts: CanonAssert[]
}

/** Strip `#` comments, respecting double-quoted strings. */
function stripComment(line: string): string {
  let out = ''
  let inStr = false
  for (let i = 0; i < line.length; i++) {
    const c = line[i]
    if (c === '"' && line[i - 1] !== '\\') inStr = !inStr
    if (c === '#' && !inStr) break
    out += c
  }
  return out
}

function normValue(raw: string): string | number {
  const t = raw.trim()
  if (t.startsWith('"') && t.endsWith('"') && t.length >= 2) {
    return t.slice(1, -1)
  }
  const n = Number(t)
  if (t !== '' && Number.isFinite(n)) return n
  return t
}

const sortObject = (o: Record<string, string | number>): Record<string, string | number> => {
  const out: Record<string, string | number> = {}
  for (const k of Object.keys(o).sort()) out[k] = o[k]
  return out
}

/** Parse the flat subset of TOML the checks builder emits (and a hand-written
 *  spec of the same shape): top-level scalars plus `[[supply]]` / `[[assert]]`
 *  array-of-table blocks. Anything else (inline tables, dotted keys) lands in
 *  the block it appears in verbatim, which is enough to make it part of the
 *  key even when this parser does not understand it. */
export function canonicalizeSpec(toml: string): CanonSpec {
  const top: Record<string, string | number> = {}
  const supplies: Record<string, string | number>[] = []
  const asserts: CanonAssert[] = []
  let cur: Record<string, string | number> = top

  for (const rawLine of toml.split('\n')) {
    const line = stripComment(rawLine).trim()
    if (!line) continue
    if (line === '[[supply]]') {
      cur = {}
      supplies.push(cur)
      continue
    }
    if (line === '[[assert]]') {
      cur = {}
      asserts.push({ kind: '', fields: cur })
      continue
    }
    const eq = line.indexOf('=')
    if (eq < 0) continue
    const key = line.slice(0, eq).trim().replace(/^"|"$/g, '')
    cur[key] = normValue(line.slice(eq + 1))
  }

  const canonAsserts: CanonAssert[] = asserts.map(a => {
    const fields = { ...a.fields }
    const kindRaw = String(fields.kind ?? '')
    delete fields.kind
    return { kind: kindRaw.replace(/-/g, '_'), fields: sortObject(fields) }
  })

  const duration = typeof top.duration_ms === 'number' ? top.duration_ms : null
  const topRest = { ...top }
  delete topRest.name
  delete topRest.duration_ms

  return {
    duration_ms: duration,
    top: sortObject(topRest),
    supplies: supplies
      .map(sortObject)
      .sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
    asserts: canonAsserts,
  }
}

/** The key for one assertion in isolation: what a per-rule recording answers.
 *  The run context (duration, supplies, firmware) is part of it, because the
 *  same assertion on the same board can pass at 1000 ms and fail at 50 ms. */
export function assertKey(spec: CanonSpec, a: CanonAssert, firmware: string | null): string {
  return JSON.stringify({
    d: spec.duration_ms,
    t: spec.top,
    s: spec.supplies,
    f: firmware ?? null,
    a: { kind: a.kind, fields: a.fields },
  })
}

/** The key for a whole spec: what a verbatim recording of one run answers. */
export function specKey(spec: CanonSpec, firmware: string | null): string {
  return JSON.stringify({
    d: spec.duration_ms,
    t: spec.top,
    s: spec.supplies,
    f: firmware ?? null,
    a: spec.asserts.map(a => ({ kind: a.kind, fields: a.fields })),
  })
}

/** Convenience for callers holding raw spec text. */
export const keysForSpec = (toml: string, firmware: string | null) => {
  const spec = canonicalizeSpec(toml)
  return {
    spec,
    key: specKey(spec, firmware),
    assertKeys: spec.asserts.map(a => assertKey(spec, a, firmware)),
  }
}
