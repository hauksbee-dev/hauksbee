import { afterEach, expect, test } from 'bun:test'
import { fetchLayoutText } from '../src/lib/api'

const realFetch = globalThis.fetch
afterEach(() => {
  globalThis.fetch = realFetch
})

/** Serve `routes` (url -> body or status) and record which urls were asked for. */
function serve(routes: Record<string, string | number>): string[] {
  const asked: string[] = []
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input)
    asked.push(url)
    const hit = routes[url]
    if (typeof hit === 'string') return new Response(hit)
    return new Response('', { status: typeof hit === 'number' ? hit : 404 })
  }) as typeof fetch
  return asked
}

const KICAD = '(kicad_pcb (version 20240108) (net 0 ""))'
const DSL = 'board "rails" {\n  part R1 ...\n}\n'

test('a native KiCad layout is drawn as served, with no second request', async () => {
  const asked = serve({ '/boards/b.kicad_pcb': KICAD })
  expect(await fetchLayoutText('/boards/b.kicad_pcb')).toBe(KICAD)
  expect(asked).toEqual(['/boards/b.kicad_pcb'])
})

test('a retained .board source draws the compiled layout the server kept', async () => {
  const asked = serve({ '/boards/b.board': DSL, '/boards/b.board/layout': KICAD })
  expect(await fetchLayoutText('/boards/b.board')).toBe(KICAD)
  expect(asked).toEqual(['/boards/b.board', '/boards/b.board/layout'])
})

test('with no compiled layout the source comes back, so the parser says it has none', async () => {
  serve({ '/boards/b.board': DSL, '/boards/b.board/layout': '{"available":false}' })
  expect(await fetchLayoutText('/boards/b.board')).toBe(DSL)
  serve({ '/boards/b.board': DSL })
  expect(await fetchLayoutText('/boards/b.board')).toBe(DSL)
})
