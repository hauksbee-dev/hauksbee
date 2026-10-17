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
const SCH = '(kicad_sch (version 20231120) (generator "eeschema"))\n'

test('a native KiCad layout is drawn as served, with no second request', async () => {
  const asked = serve({ '/boards/b.kicad_pcb': KICAD })
  expect(await fetchLayoutText('/boards/b.kicad_pcb')).toBe(KICAD)
  expect(asked).toEqual(['/boards/b.kicad_pcb'])
})

test('a retained schematic source draws the extracted layout the server kept', async () => {
  const asked = serve({ '/boards/b.kicad_sch': SCH, '/boards/b.kicad_sch/layout': KICAD })
  expect(await fetchLayoutText('/boards/b.kicad_sch')).toBe(KICAD)
  expect(asked).toEqual(['/boards/b.kicad_sch', '/boards/b.kicad_sch/layout'])
})

test('with no extracted layout the source comes back, so the parser says it has none', async () => {
  serve({ '/boards/b.kicad_sch': SCH, '/boards/b.kicad_sch/layout': '{"available":false}' })
  expect(await fetchLayoutText('/boards/b.kicad_sch')).toBe(SCH)
  serve({ '/boards/b.kicad_sch': SCH })
  expect(await fetchLayoutText('/boards/b.kicad_sch')).toBe(SCH)
})
