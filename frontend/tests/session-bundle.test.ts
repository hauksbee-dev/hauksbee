import { beforeAll, expect, test } from 'bun:test'
import type { WebReport } from '../src/types/report'

// Browser storage is faked BEFORE the store loads: the store decides at import
// time whether storage exists.
const data = new Map<string, string>()
;(globalThis as { localStorage?: Storage }).localStorage = {
  getItem: (k: string) => data.get(k) ?? null,
  setItem: (k: string, v: string) => { data.set(k, v) },
  removeItem: (k: string) => { data.delete(k) },
  clear: () => data.clear(),
  key: () => null,
  length: 0,
} as Storage

const store = await import('../src/lib/session-store')
const { parseLocation, snippetAt } = await import('../src/lib/parse-snippet')
const { boardVerdict } = await import('../src/lib/status-vocab')

function report(over: Partial<WebReport> = {}): WebReport {
  return {
    ok: true, board_name: 'b', file_name: 'b.kicad_pcb', num_components: 1, num_nets: 1,
    headline: '', serious: 0, total: 0, sections: [], components: [], ...over,
  }
}

async function saved(toml: string, specSha256: string | null) {
  const row = {
    id: 'b.kicad_pcb:1:1', name: 'My board', createdAt: 1, updatedAt: 1, analyzedAt: 1,
    board: { fileName: 'b.kicad_pcb', boardName: 'b', numComponents: 1, numNets: 1 },
    firmwareName: null, hasReport: true, checkCount: 1,
    verdict: { term: 'pass' as const, detail: '', tone: 'ok' as const },
    boardSha256: 'a'.repeat(64), specSha256,
  }
  return {
    ...row, report: report(), checksKey: 'checks-key', spec: { toml, fileName: 'b.toml' },
    checks: null, engineVersion: null,
  }
}

beforeAll(() => data.clear())

test('a session exports with its hashes and imports back as the same run', async () => {
  const toml = '[[assert]]\nkind = "x"\n'
  const sha = await store.sha256Hex(toml)
  expect(store.saveSession(await saved(toml, sha)).kind).toBe('saved')
  const bundle = store.sessionBundle('b.kicad_pcb:1:1')
  expect(bundle?.fileName).toBe('b.hauksbee-session.json')
  store.deleteSession('b.kicad_pcb:1:1')
  expect(store.loadSession('b.kicad_pcb:1:1')).toBeNull()

  const back = await store.importSessionBundle(bundle!.text)
  expect(back).toEqual({ ok: true, id: 'b.kicad_pcb:1:1', name: 'My board' })
  const row = store.listSessions()[0]
  expect(row.boardSha256).toBe('a'.repeat(64))
  expect(row.specSha256).toBe(sha)
  expect(row.verdict?.term).toBe('pass')
  expect(store.loadSession(row.id)?.checksKey).toBe('checks-key')
})

test('a bundle edited after export is refused, and so is anything else', async () => {
  const toml = '[[assert]]\nkind = "x"\n'
  store.saveSession(await saved(toml, await store.sha256Hex(toml)))
  const bundle = store.sessionBundle('b.kicad_pcb:1:1')!
  const edited = bundle.text.replace('kind = \\"x\\"', 'kind = \\"y\\"')
  expect(edited).not.toBe(bundle.text)
  const refused = await store.importSessionBundle(edited)
  expect(refused.ok).toBe(false)
  expect((await store.importSessionBundle('not json')).ok).toBe(false)
  expect((await store.importSessionBundle('{"format":"other"}')).ok).toBe(false)
})

test('a read error names a place and the file shows the text there', async () => {
  expect(parseLocation('parse error at line 2 (byte 14): unclosed list')).toEqual({ line: 2, byte: 14 })
  expect(parseLocation('no place named')).toBeNull()
  const file = new Blob(['(kicad_pcb\n  (bad thing\n  (ok)\n'])
  const snip = await snippetAt(file, { line: 2, byte: 14 })
  expect(snip?.lineNo).toBe(2)
  expect(snip?.text).toBe('  (bad thing')
  expect(snip?.caret).toBe(3)
})

test('a pass with findings but no conditions is a pass, not a conditional pass', () => {
  expect(boardVerdict(report({ ok: false }))).toBeNull()
  expect(boardVerdict(report({ serious: 2, total: 2, verdict: 'fail' }))?.term).toBe('fail')
  const qualified = boardVerdict(report({ total: 3, verdict: 'pass' }))
  expect(qualified?.term).toBe('pass')
  expect(qualified?.tone).toBe('warn')
  expect(boardVerdict(report({ total: 3, verdict: 'pass', verdict_conditions: ['rev B only'] }))?.term).toBe('conditional_pass')
})
