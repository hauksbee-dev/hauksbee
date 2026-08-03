#!/usr/bin/env bun
// The server the visual lint runs against in CI: `frontend/dist` on disk plus
// the handful of `/api/...` responses the surfaces need, replayed from
// fixtures/ (captured from a real `hauksbee serve`, see capture-fixtures.ts).
//
// Why a fixture server and not the real engine: the lint is about layout, and
// building the Rust workspace to get one report back would make a
// three-minute job a twenty-minute one. The fixtures ARE real engine output,
// so the DOM under test is the DOM a user gets. Run the lint against a real
// serve with HB_LINT_BASE=http://127.0.0.1:3001 when you want the round trip.
//
// Usage: bun run tests/visual-lint/fixture-server.ts [port]

import { file } from 'bun'
import { existsSync } from 'node:fs'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const DIST = join(here, '../../dist')
const FIXTURES = join(here, 'fixtures')

if (!existsSync(join(DIST, 'index.html'))) {
  console.error(`no build at ${DIST}: run \`bun run build\` in frontend/ first`)
  process.exit(1)
}

const fixture = (name: string) => new Response(file(join(FIXTURES, name)), {
  headers: { 'content-type': 'application/json' },
})
const json = (body: unknown, status = 200) =>
  Response.json(body, { status })

/** Endpoints the lint's surfaces actually hit, in the shape the real server
 *  answers with. Anything else under /api returns 501 so a new fetch shows up
 *  as a loud gap rather than a silently empty panel. */
function api(url: URL, method: string): Response | null {
  const p = url.pathname
  if (p === '/api/startup' && method === 'GET') return fixture('startup.json')
  if (p === '/api/live/status' && method === 'GET') return fixture('live-status.json')
  if ((p === '/api/analyze' || p === '/api/analyze-with-firmware') && method === 'POST') {
    // One report for any upload: the lint always feeds it the watchy sample.
    return fixture('analyze-watchy.json')
  }
  if (p === '/api/check' && method === 'POST') return fixture('check-run.json')
  if (p === '/api/deps' && method === 'GET') return fixture('deps.json')
  if (p === '/api/models/extract/ready' && method === 'GET') return fixture('extract-ready.json')
  if (p === '/api/models/check' && method === 'POST') return fixture('models-check.json')
  if (p === '/api/models/save' && method === 'POST') {
    return json({ ok: true, path: '/home/runner/.hauksbee/models/lint.toml' })
  }
  if (p === '/api/models/extract' && method === 'POST') {
    // Extraction talks to an LLM backend. The lint renders the form, never a run.
    return json({ ok: false, error: 'extraction is not available on the fixture server' })
  }
  if (p === '/api/live/launch' && method === 'POST') {
    return json({ ok: false, error: 'the fixture server does not run live sessions' })
  }
  if (p.startsWith('/api/')) return json({ ok: false, error: `no fixture for ${method} ${p}` }, 501)
  return null
}

const port = Number(process.argv[2] ?? process.env.LINT_PORT ?? 3479)

const server = Bun.serve({
  port,
  idleTimeout: 60,
  async fetch(req) {
    const url = new URL(req.url)
    const stubbed = api(url, req.method)
    if (stubbed) return stubbed

    // Static dist/, with index.html for anything that is not a real file (the
    // app is a single bundle with no server-side routes).
    const path = url.pathname === '/' ? '/index.html' : decodeURIComponent(url.pathname)
    if (path.includes('..')) return new Response('no', { status: 400 })
    const asset = file(join(DIST, path))
    if (await asset.exists()) return new Response(asset)
    return new Response(file(join(DIST, 'index.html')), {
      headers: { 'content-type': 'text/html' },
    })
  },
})

console.log(`[visual-lint fixture server] http://127.0.0.1:${server.port}`)
