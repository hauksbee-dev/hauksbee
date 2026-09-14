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
import { withBoardIdentity } from './fixture-report'

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
const liveBoards = new Map<string, string>()

/** A tiny hand-rolled SSE response: `event: <e>\ndata: <d>\n\n` per frame,
 *  spaced out so a client watching the stream sees real progress rather than
 *  everything landing in one chunk. Mirrors the shape `readSseStream` in
 *  lib/api.ts parses (the same framing the real deps-install and extraction
 *  endpoints use). */
function sseResponse(frames: { event: string; data: string }[]): Response {
  const encoder = new TextEncoder()
  const body = new ReadableStream<Uint8Array>({
    async start(controller) {
      for (const f of frames) {
        controller.enqueue(encoder.encode(`event: ${f.event}\ndata: ${f.data}\n\n`))
        await new Promise(resolve => setTimeout(resolve, 30))
      }
      controller.close()
    },
  })
  return new Response(body, { headers: { 'content-type': 'text/event-stream' } })
}

// The backend-settings state the settings page reads and writes, held in
// memory for the life of the fixture server so a PUT or a preset click shows
// up on the next GET, the way a real `extract.toml` would.
let settingsState: Record<string, unknown> | null = null
async function settings(): Promise<Record<string, unknown>> {
  if (!settingsState) settingsState = await file(join(FIXTURES, 'settings-extract.json')).json() as Record<string, unknown>
  return settingsState
}

/** Endpoints the lint's surfaces actually hit, in the shape the real server
 *  answers with. Anything else under /api returns 501 so a new fetch shows up
 *  as a loud gap rather than a silently empty panel. */
async function uploadedBoard(req: Request): Promise<{ name: string; text: string } | null> {
  const header = req.headers.get('X-Board-Filename')
  if (header) return { name: header, text: await req.text() }
  if (!req.headers.get('content-type')?.startsWith('multipart/form-data')) return null
  const board = (await req.formData()).get('board')
  return board instanceof File ? { name: board.name, text: await board.text() } : null
}

async function api(req: Request, url: URL): Promise<Response | null> {
  const method = req.method
  const p = url.pathname
  if (p === '/api/startup' && method === 'GET') return fixture('startup.json')
  if (p === '/api/live/status' && method === 'GET') return fixture('live-status.json')
  if ((p === '/api/analyze' || p === '/api/analyze-with-firmware') && method === 'POST') {
    // Two reports: the watchy sample (fully bound; no datasheet panel), and
    // the synthetic open-active-IC board the datasheet surfaces upload to get
    // a report that renders the "parts with no model" panel.
    const uploaded = await uploadedBoard(req)
    const name = uploaded?.name ?? null
    if (uploaded) liveBoards.set(uploaded.name, uploaded.text)
    const which = name?.includes('open_active_ic') ? 'analyze-openparts.json' : 'analyze-watchy.json'
    const captured = await file(join(FIXTURES, which)).json() as Record<string, unknown>
    const layoutSha256 = uploaded
      ? new Bun.CryptoHasher('sha256').update(uploaded.text).digest('hex')
      : null
    return json(withBoardIdentity(captured, name, layoutSha256))
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
  if (p === '/api/settings/extract' && method === 'GET') return json(await settings())
  if (p === '/api/settings/extract' && method === 'PUT') {
    const state = await settings()
    const body = await req.json() as Record<string, unknown>
    state.config = { ...(state.config as Record<string, unknown>), ...body }
    if (body.backend !== undefined) {
      (state.resolved as Record<string, unknown>).backend = body.backend
    }
    return json(state)
  }
  if (p.startsWith('/api/settings/extract/preset/') && method === 'POST') {
    const id = decodeURIComponent(p.slice('/api/settings/extract/preset/'.length))
    const state = await settings()
    const preset = (state.presets as { id: string; backend: string }[]).find(pr => pr.id === id)
    if (!preset) return json({ error: `no such preset: ${id}` }, 400)
    state.config = { ...(state.config as Record<string, unknown>), backend: preset.backend }
    return json(state)
  }
  if (p === '/api/settings/extract/test' && method === 'POST') {
    return sseResponse([
      { event: 'log', data: 'Contacting the configured backend ...' },
      { event: 'log', data: 'Sent a one-line test prompt.' },
      { event: 'done', data: 'OK' },
    ])
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
    const stubbed = await api(req, url)
    if (stubbed) return stubbed

    if (req.method === 'GET' && url.pathname.startsWith('/boards/')) {
      const name = decodeURIComponent(url.pathname.slice('/boards/'.length))
      const board = liveBoards.get(name)
      return board === undefined
        ? new Response('board is no longer live', { status: 404 })
        : new Response(board, { headers: { 'content-type': 'text/plain; charset=utf-8' } })
    }

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
