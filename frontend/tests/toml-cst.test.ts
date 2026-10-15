// The spec text is the one source of truth: every builder edit is a patch on
// the span it is about, and its inverse puts the bytes back exactly. These
// properties run over generated specs with comments, odd key orders and blank
// lines in random places, and over the shipped example specs.

import { describe, expect, test } from 'bun:test'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { applyOp, assertHeaderOffsets, pathAt, readCst } from '../src/lib/toml-cst'
import type { PatchOp, TablePath } from '../src/lib/toml-cst'
import {
  appendOp, assertToml, checkOps, emptyCheck, readSpec, rootOps, supplyOps, tomlToBuilder,
} from '../src/lib/check-spec'
import { hoverAt, suggest } from '../src/lib/toml-schema-assist'

/** A small deterministic PRNG (mulberry32), so a failure reproduces. */
function rng(seed: number) {
  let a = seed >>> 0
  const next = () => {
    a = (a + 0x6d2b79f5) >>> 0
    let t = a
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
  const int = (n: number) => Math.floor(next() * n)
  const pick = <T,>(xs: readonly T[]): T => xs[int(xs.length)]
  const shuffle = <T,>(xs: T[]): T[] => {
    const out = [...xs]
    for (let i = out.length - 1; i > 0; i--) {
      const j = int(i + 1)
      ;[out[i], out[j]] = [out[j], out[i]]
    }
    return out
  }
  return { next, int, pick, shuffle, chance: (p: number) => next() < p }
}
type Rng = ReturnType<typeof rng>

const NETS = ['+5V', '+3V3', 'VIN', 'GND', 'LED', '/VBUS', 'Net-(U1-PA0)']
const comment = (r: Rng) => r.pick(['# my note', '#tight', '# ünïcode – note', '# TODO: check the datasheet', '##'])

/** Key lines in a random order, each maybe with a trailing comment. */
function body(r: Rng, kvs: [string, string][]): string {
  return r.shuffle(kvs).map(([k, v]) => {
    const pre = r.chance(0.15) ? `${comment(r)}\n` : ''
    const post = r.chance(0.2) ? `  ${comment(r)}` : ''
    return `${pre}${k} = ${v}${post}\n`
  }).join('')
}

const num = (r: Rng) => r.pick(['0', '1', '3.3', '5.0', '-0.5', '1e3', '12_000', '0.25'])

function table(r: Rng): [string, string] {
  const which = r.int(4)
  if (which === 0) {
    return ['supply', body(r, [['net', JSON.stringify(r.pick(NETS))], ['volts', num(r)], ...(r.chance(0.5) ? [['kind', '"ideal"'] as [string, string]] : [])])]
  }
  if (which === 1) {
    const kv: [string, string][] = [['kind', '"voltage"'], ['net', JSON.stringify(r.pick(NETS))]]
    if (r.chance(0.7)) kv.push(['min', num(r)])
    if (r.chance(0.7)) kv.push(['max', num(r)])
    if (r.chance(0.3)) kv.push(['after_ms', num(r)])
    return ['assert', body(r, kv)]
  }
  if (which === 2) return ['assert', body(r, [['kind', '"no_faults"']])]
  // Something outside the builder's vocabulary: kept as written.
  return ['assert', body(r, [['kind', '"ac_gain"'], ['net', '"OUT"'], ['min_db', num(r)]])]
}

function genSpec(r: Rng): string {
  const blank = () => (r.chance(0.6) ? '\n' : r.chance(0.5) ? '\n\n' : '')
  let text = r.chance(0.3) ? `${comment(r)}\n` : ''
  const root: [string, string][] = [['name', JSON.stringify(`spec ${r.int(100)}`)]]
  if (r.chance(0.7)) root.push(['duration_ms', String(1 + r.int(500))])
  if (r.chance(0.3)) root.push(['no_fit', '["R1", "C2"]'])
  text += body(r, root)
  const n = 1 + r.int(6)
  for (let i = 0; i < n; i++) {
    const [name, b] = table(r)
    text += blank()
    if (r.chance(0.3)) text += `${comment(r)}\n`
    text += `[[${name}]]\n${b}`
  }
  if (r.chance(0.2)) text = text.replace(/\n$/, '') // no final newline
  if (r.chance(0.1)) text = text.replace(/\n/g, '\r\n')
  return text
}

/** Every table path in a parsed spec, root included. */
function tables(text: string): TablePath[] {
  const read = readCst(text)
  if (!read.ok) return []
  return [[] as TablePath, ...read.doc.elements.filter(e => e.index !== null).map(e => [e.name, e.index!] as TablePath)]
}

function randomOp(r: Rng, text: string): PatchOp {
  const paths = tables(text)
  const t = r.pick(paths)
  const read = readCst(text)
  const entries = read.ok
    ? (t.length === 0 ? read.doc.root : read.doc.elements.find(e => e.name === t[0] && e.index === t[1])?.entries ?? [])
    : []
  const key = entries.length && r.chance(0.7) ? r.pick(entries).key : r.pick(['max', 'min', 'volts', 'note_key'])
  switch (r.int(6)) {
    case 0: return { op: 'setValue', table: t, key, value: num(r) }
    case 1: return { op: 'insertKey', table: t, key: `k${r.int(1000)}`, value: JSON.stringify('x y') }
    case 2: return { op: 'removeKey', table: t, key }
    case 3: return appendOp(assertToml({ ...emptyCheck(0, 'voltage', r.pick(NETS)), max: num(r) }))
    case 4: return t.length ? { op: 'removeTable', table: t } : { op: 'appendTable', name: 'supply', body: 'net = "VIN"\nvolts = 9\n' }
    default: return t.length ? { op: 'replaceBody', table: t, body: 'kind = "no_faults"\n' } : { op: 'setValue', table: t, key: 'name', value: '"renamed"' }
  }
}

/** The one region two strings differ in: [start, endInA, endInB]. */
function diffSpan(a: string, b: string): [number, number, number] {
  let s = 0
  while (s < a.length && s < b.length && a[s] === b[s]) s++
  let ea = a.length
  let eb = b.length
  while (ea > s && eb > s && a[ea - 1] === b[eb - 1]) { ea--; eb-- }
  return [s, ea, eb]
}

describe('patch ops on generated specs', () => {
  const specs = Array.from({ length: 200 }, (_, i) => genSpec(rng(1000 + i)))

  test('every generated spec parses and reads into the builder', () => {
    for (const spec of specs) {
      expect(readCst(spec).ok).toBe(true)
      expect(readSpec(spec)).not.toBeNull()
    }
  })

  test('random ops then their inverses give back the exact bytes', () => {
    specs.forEach((spec, i) => {
      const r = rng(77 + i)
      let text = spec
      const undo: PatchOp[] = []
      for (let step = 0; step < 8; step++) {
        const done = applyOp(text, randomOp(r, text))
        if (!done) continue
        expect(readCst(done.text).ok).toBe(true)
        undo.push(done.inverse)
        text = done.text
      }
      for (const inverse of undo.reverse()) text = applyOp(text, inverse)!.text
      expect(text).toBe(spec)
    })
  })

  test('a setValue changes exactly the value span and nothing else', () => {
    specs.forEach((spec, i) => {
      const r = rng(5000 + i)
      const read = readCst(spec)
      if (!read.ok) throw new Error('unparsed')
      const el = r.pick(read.doc.elements)
      const entry = r.pick(el.entries)
      const value = entry.raw === '42.5' ? '43' : '42.5'
      const done = applyOp(spec, { op: 'setValue', table: [el.name, el.index!], key: entry.key, value })!
      const [s, ea, eb] = diffSpan(spec, done.text)
      expect(s).toBeGreaterThanOrEqual(entry.valueRange[0])
      expect(ea).toBeLessThanOrEqual(entry.valueRange[1])
      expect(done.text.slice(0, entry.valueRange[0]) + done.text.slice(entry.valueRange[0] + value.length))
        .toBe(spec.slice(0, entry.valueRange[0]) + spec.slice(entry.valueRange[1]))
      expect(eb - s).toBeLessThanOrEqual(value.length)
    })
  })

  test('a builder edit to one assertion leaves every comment and every other table alone', () => {
    specs.forEach((spec, i) => {
      const model = readSpec(spec)!
      const row = model.checks.find(c => c.kind === 'voltage')
      if (!row) return
      const next = { ...row, max: String(10 + i) }
      const ops = checkOps(row, next)
      let text = spec
      const undo: PatchOp[] = []
      for (const op of ops) {
        const done = applyOp(text, op)!
        undo.push(done.inverse)
        text = done.text
      }
      const after = readSpec(text)!
      expect(after.checks.find(c => c.id === row.id)!.max).toBe(String(10 + i))
      // Every comment line survives, in order.
      const comments = (s: string) => s.split(/\r?\n/).filter(l => l.includes('#'))
      expect(comments(text).join('\n').replace(/= [^ ]+/g, '')).toBe(comments(spec).join('\n').replace(/= [^ ]+/g, ''))
      for (const inverse of undo.reverse()) text = applyOp(text, inverse)!.text
      expect(text).toBe(spec)
    })
  })

  test('re-reading and writing back an unchanged row is no edit at all', () => {
    for (const spec of specs) {
      const m = readSpec(spec)!
      for (const c of m.checks) expect(checkOps(c, c)).toEqual([])
      for (const s of m.supplies) expect(supplyOps(s.index, s.row, s.row)).toEqual([])
      expect(rootOps(m, m)).toEqual([])
    }
  })
})

describe('the comment above an assertion', () => {
  test('survives changing that assertion\'s max in the builder', () => {
    const spec = 'name = "n"\n\n[[assert]]\nkind = "no_faults"\n\n# my note\n[[assert]]\nkind = "voltage"\nnet = "+5V"\nmax = 5.5 # upper\n'
    const row = readSpec(spec)!.checks[1]
    const text = checkOps(row, { ...row, max: '5.25' }).reduce((t, op) => applyOp(t, op)!.text, spec)
    expect(text).toBe(spec.replace('5.5 #', '5.25 #'))
  })
})

// The shipped example specs (Tarski-derived ones are skipped by name).
const repo = join(import.meta.dir, '..', '..')
const exampleFiles = [
  ...['examples/ci-specs', 'crates/hauksbee-ci/examples'].flatMap(dir => {
    try {
      return readdirSync(join(repo, dir)).filter(f => f.endsWith('.toml')).map(f => join(dir, f))
    } catch { return [] }
  }),
].filter(f => !/tarski/i.test(f))

describe('example specs', () => {
  test('there are examples to read', () => {
    expect(exampleFiles.length).toBeGreaterThan(5)
  })

  for (const file of exampleFiles) {
    test(`${file} loads into the builder and round-trips byte for byte`, () => {
      const spec = readFileSync(join(repo, file), 'utf8')
      const model = readSpec(spec)
      expect(model).not.toBeNull()
      // Writing back what was read changes nothing.
      for (const c of model!.checks) expect(checkOps(c, c)).toEqual([])
      for (const s of model!.supplies) expect(supplyOps(s.index, s.row, s.row)).toEqual([])
      // An edit and its inverse give back the exact file.
      const done = applyOp(spec, { op: 'setValue', table: [], key: 'name', value: '"edited"' })!
      expect(readSpec(done.text)!.name).toBe('edited')
      expect(applyOp(done.text, done.inverse)!.text).toBe(spec)
      for (const c of model!.checks) {
        const ops = checkOps(c, { ...c, max: '1.5', min: '0.5' })
        let text = spec
        const undo: PatchOp[] = []
        for (const op of ops) {
          const step = applyOp(text, op)!
          undo.push(step.inverse)
          text = step.text
        }
        for (const inverse of undo.reverse()) text = applyOp(text, inverse)!.text
        expect(text).toBe(spec)
      }
    })
  }
})

describe('kept as written', () => {
  test('a table outside the builder is kept, and tomlToBuilder declines the whole spec', () => {
    const spec = 'name = "n"\n[ac]\nfreq = 1\n[[assert]]\nkind = "ac_gain"\nnet = "OUT"\n'
    const m = readSpec(spec)!
    expect(m.kept.map(k => k.label)).toEqual(['[ac]', '[[assert]] ac_gain'])
    expect(spec.slice(m.kept[0].offset).startsWith('[ac]')).toBe(true)
    expect(tomlToBuilder(spec)).toBeNull()
  })

  test('an unparsable spec reads as null and reports where', () => {
    const read = readCst('name = "n"\n[[assert]\n')
    expect(read.ok).toBe(false)
    if (!read.ok) expect(read.error.line).toBe(2)
    expect(readSpec('name = "n"\n[[assert]\n')).toBeNull()
  })
})

describe('cursor context, completion and hover', () => {
  const data = {
    schema: {
      properties: {
        assert: {
          type: 'array',
          items: {
            properties: {
              kind: { type: 'string' },
              rail_window: { type: 'object', description: 'A rail excursion must stay within bounds.' },
            },
          },
        },
      },
    },
    kinds: [
      { kind: 'boot_coverage', title: 'Firmware must drive a net by a deadline', summary: 'Boot coverage.' },
      { kind: 'voltage', title: 'A net must sit at a voltage', summary: 'Voltage.', fields: [{ key: 'net', help: 'The net.' }, { key: 'max', help: 'Upper bound.', unit: 'V' }] },
    ],
    nets: ['/VBUS', '/VIN', 'GND'],
    refs: ['U1'],
  }

  test('pathAt finds the table, its index and the key', () => {
    const text = '[[assert]]\nkind = "no_faults"\n[[assert]]\nkind = "voltage"\nnet = "/V'
    const at = pathAt(text, text.length)
    expect(at.table).toEqual(['assert'])
    expect(at.index).toBe(1)
    expect(at.key).toBe('net')
    expect(at.inValue).toBe(true)
    expect(at.quoted).toBe(true)
    expect(at.kind).toBe('voltage')
  })

  test('net values complete from the board, closing the quote', () => {
    const text = '[[assert]]\nkind = "voltage"\nnet = "/V'
    const hit = suggest(text, text.length, data)!
    expect(hit.options.map(o => o.label)).toEqual(['/VBUS', '/VIN', 'GND'])
    expect(hit.options[0].apply).toBe('/VBUS"')
    expect(text.slice(hit.from)).toBe('/V')
  })

  test('kind values complete from the catalogue', () => {
    const text = '[[assert]]\nkind = "bo'
    const hit = suggest(text, text.length, data)!
    expect(hit.options.map(o => o.label)).toContain('boot_coverage')
  })

  test('hover names a key in the schema\'s words, and a kind in the catalogue\'s', () => {
    const text = '[[assert]]\nrail_window = {}\nkind = "voltage"\n'
    const key = hoverAt(text, text.indexOf('rail_window') + 2, data)!
    expect(key.body).toContain('rail excursion')
    const kind = hoverAt(text, text.indexOf('"voltage"') + 2, data)!
    expect(kind.title).toBe('A net must sit at a voltage')
  })

  test('assert headers are found even while the text does not parse', () => {
    const text = '[[assert]]\nkind = "x\n  [[assert]]\n'
    expect(assertHeaderOffsets(text)).toEqual([0, text.indexOf('  [[assert]]')])
  })
})
