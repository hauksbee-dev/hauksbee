// The assertion catalogue the server serves (`GET /api/ci/catalog`, the table
// `hauksbee-ci explain` prints), and the one way an example gets into raw TOML.

export interface CatalogField { name: string; what: string; default: string }

export interface CatalogEntry {
  kind: string
  title: string
  summary: string
  required: CatalogField[]
  optional: CatalogField[]
  /** What else the spec must declare: `firmware`, `[ac]`, `[[scenario]]`... */
  needs: string[]
  /** Valid TOML: the `[[assert]]` plus any block it depends on. */
  example: string
  fails_because: string
}

export interface CatalogResponse { ok: boolean; error?: string; kinds?: CatalogEntry[] }

/** A selection in the raw text: the value the reader should type over first. */
export interface RawSelection { start: number; end: number }

/** Append `example` to `base`, leaving out any block `base` already has (a second
 *  `[ac]` would be a TOML error, a second identical `[[scenario]]` is noise), and
 *  select the first required value of the assertion so typing replaces it. */
export function appendExample(
  base: string, example: string, fill: readonly string[],
): { text: string; select: RawSelection | null } {
  const blocks = example.trim().split(/\n\s*\n/)
  const lines = new Set(base.split('\n').map(l => l.trim()))
  const kept = blocks.filter(block => {
    const header = block.trim().split('\n')[0].trim()
    if (header === '[[assert]]') return true
    if (header.startsWith('[[')) return !base.includes(block.trim())
    return !(header.startsWith('[') && lines.has(header))
  })
  if (kept.length === 0) return { text: base, select: null }
  const gap = base === '' ? '' : base.endsWith('\n\n') ? '' : base.endsWith('\n') ? '\n' : '\n\n'
  const added = `${kept.join('\n\n')}\n`
  const text = `${base}${gap}${added}`
  const origin = base.length + gap.length
  const assertAt = origin + Math.max(0, added.lastIndexOf('[[assert]]'))
  for (const name of fill) {
    const hit = new RegExp(`^${name} = (.*)$`, 'm').exec(text.slice(assertAt))
    if (hit) {
      const start = assertAt + hit.index + `${name} = `.length
      return { text, select: { start, end: start + hit[1].length } }
    }
  }
  return { text, select: null }
}
