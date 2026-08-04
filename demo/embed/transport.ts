// The cached transport: the one thing standing where a server would be.
//
// The widget runs the UNTOUCHED frontend, which talks to an engine over
// `fetch`. Here that fetch is answered from demo/sessions/cache/<board>.json,
// recorded by a real `hauksbee serve` on a real board (see
// demo/capture/record-embed-cache.ts). Rules of the house:
//
//   1. Every ENGINE answer is a recording. Nothing is invented, computed or
//      interpolated. A request with no recording behind it gets an honest
//      refusal, never a plausible-looking result and never a hang.
//   2. Session BOOKKEEPING (which board this page has loaded, whether a live
//      session is "running") is not an engine answer and is kept here, because
//      the app needs a coherent server to talk to. It is stated in demo/EMBED.md.
//   3. A check run assembled from per-rule recordings is flagged as assembled,
//      so the widget's own caption can say so.
//
// Anything that is not a hauksbee API path falls through to the real fetch.

import { canonicalizeSpec, assertKey, specKey } from '../shared/spec-key'
import { realFetch } from './cache'
import type { BoardCache, RecordedRun, RunResponse } from './cache'

/** What the app may ask for that this transport knows how to answer. */
const API_PREFIXES = ['/api/', '/boards/']

export interface TransportEvent {
  kind: 'hit' | 'assembled' | 'miss' | 'bookkeeping'
  path: string
  detail?: string
}

export interface TransportState {
  /** The board currently staged, keyed by its file name (what the app uploads). */
  board: BoardCache | null
  /** Set once the app "launches" the live session for the staged board. */
  liveBoard: string | null
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })

const HONEST_ANALYZE_REFUSAL =
  'This is the embedded Hauksbee demo: it replays runs the real engine already '
  + 'made on the sample boards, so it cannot analyze a board of yours. Install '
  + 'hauksbee (one line, runs entirely on your machine) and every surface here '
  + 'works on your own board.'

const honestCheckRefusal = (why: string) =>
  `${why} The embedded demo answers from recorded runs of the real engine, so it `
  + 'can only answer the specs that were recorded. Put the spec back the way it '
  + 'was, or install hauksbee and run this one for real: the spec file here is '
  + 'the file hauksbee-ci takes.'

/** Nothing is over-stressed about a run that only exists as recordings; the
 *  fields the panel reads out of a run are copied from the recordings that
 *  produced the rows, so a composed answer says exactly what its parts said. */
function assemble(rows: RecordedRun[]): RunResponse {
  const results = rows.flatMap(r => r.response.results ?? [])
  const merged: RunResponse = {
    ok: true,
    passed: results.every(x => x.invalid || x.passed),
    exit_code: results.some(x => !x.invalid && !x.passed) ? 1 : 0,
    analog_abort: rows.some(r => r.response.analog_abort === true),
    coverage: rows.map(r => r.response.coverage).find(c => typeof c === 'string' && c) ?? null,
    substitutions: [...new Set(rows.flatMap(r => r.response.substitutions ?? []))],
    coverage_warnings: [...new Set(rows.flatMap(r => r.response.coverage_warnings ?? []))],
    results,
  }
  return merged
}

export interface CachedTransport {
  state: TransportState
  /** Stage a board: from here on the transport answers about this board. */
  setBoard: (cache: BoardCache | null) => void
  /** Remove the shim and put the page's own fetch back. */
  uninstall: () => void
  /** Whether the last check answer was assembled from per-rule recordings. */
  lastRunAssembled: () => boolean
}

export function installCachedTransport(opts: {
  version: string
  onEvent?: (e: TransportEvent) => void
}): CachedTransport {
  const state: TransportState = { board: null, liveBoard: null }
  let assembled = false
  const emit = (e: TransportEvent) => opts.onEvent?.(e)

  const patched = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string'
      ? input
      : input instanceof URL ? input.toString() : input.url
    const method = (init?.method ?? (input instanceof Request ? input.method : 'GET')).toUpperCase()
    let path: string
    try {
      path = new URL(url, location.href).pathname
    } catch {
      return realFetch(input as RequestInfo, init)
    }
    if (!API_PREFIXES.some(p => path.startsWith(p))) {
      return realFetch(input as RequestInfo, init)
    }

    const board = state.board

    // ── How the app was launched. Never "preloaded": the widget hands the
    // board to the app through the app's own file input, so the app takes the
    // same code path a dropped board takes and ends up holding the real bytes
    // (which the checks panel needs). `live: true` is bookkeeping: the live
    // surface here is a recorded session, and the widget's caption says so.
    if (path === '/api/startup' && method === 'GET') {
      emit({ kind: 'bookkeeping', path })
      return json({ preloaded: false, live: true, version: opts.version })
    }

    if (path === '/api/live/status' && method === 'GET') {
      emit({ kind: 'bookkeeping', path })
      return json({ active: state.liveBoard !== null, board_name: state.liveBoard })
    }

    if (path === '/api/live/launch' && method === 'POST') {
      if (!board) return json({ ok: false, error: HONEST_ANALYZE_REFUSAL }, 400)
      state.liveBoard = board.board_name
      emit({ kind: 'bookkeeping', path, detail: board.board_name })
      return json({ ok: true, board_name: board.board_name })
    }

    // ── The report. Answered only for the board this widget staged; a board of
    // the visitor's own gets the honest refusal, because there is no engine
    // here that could look at it.
    if (path === '/api/analyze' && method === 'POST') {
      const name = init?.headers
        ? new Headers(init.headers).get('X-Board-Filename')
        : null
      if (!board || (name && name !== board.board_name)) {
        emit({ kind: 'miss', path, detail: name ?? 'unknown board' })
        return new Response(HONEST_ANALYZE_REFUSAL, { status: 501 })
      }
      emit({ kind: 'hit', path, detail: board.board_name })
      return json(board.analyze)
    }

    if (path === '/api/analyze-with-firmware' && method === 'POST') {
      if (!board || !board.analyze_with_firmware) {
        emit({ kind: 'miss', path })
        return new Response(HONEST_ANALYZE_REFUSAL, { status: 501 })
      }
      emit({ kind: 'hit', path, detail: board.board_name })
      return json(board.analyze_with_firmware)
    }

    // ── A checks run.
    if (path === '/api/check' && method === 'POST') {
      if (!board) return json({ ok: false, error: HONEST_ANALYZE_REFUSAL })
      const body = init?.body
      if (!(body instanceof FormData)) {
        return json({ ok: false, error: honestCheckRefusal('That run did not carry a spec.') })
      }
      const specBlob = body.get('spec')
      const firmware = body.get('firmware')
      const firmwareName = firmware instanceof File ? firmware.name : null
      const toml = specBlob instanceof Blob ? await specBlob.text() : ''
      const answer = answerCheck(board, toml, firmwareName)
      assembled = answer.assembled
      emit({
        kind: answer.assembled ? 'assembled' : answer.response.ok ? 'hit' : 'miss',
        path,
        detail: answer.detail,
      })
      return json(answer.response)
    }

    // ── The board layout text, when something asks for it by URL rather than
    // holding the File (the report map uses an object URL of the upload).
    if (path.startsWith('/boards/')) {
      emit({ kind: 'miss', path })
      return new Response('not served by the embedded demo', { status: 404 })
    }

    // ── Everything else the app can ask a real engine for: the environment
    // page's backend probes, the datasheet extractor, part-model saves. None of
    // it is recorded, and the widget hides those surfaces; a request that still
    // arrives is answered honestly rather than left hanging.
    emit({ kind: 'miss', path })
    return json({
      ok: false,
      error: 'Not available in the embedded demo: this surface needs the engine '
        + 'running locally. Install hauksbee to use it.',
    }, 501)
  }) as (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>

  // Cast: the page's fetch carries vendor extras (`preconnect`) that a plain
  // function does not, and they are not ours to implement. Everything the app
  // uses is the call signature.
  const patchedFetch = patched as unknown as typeof fetch
  const before = globalThis.fetch
  globalThis.fetch = patchedFetch

  return {
    state,
    setBoard: cache => {
      state.board = cache
      state.liveBoard = null
    },
    uninstall: () => { globalThis.fetch = before },
    lastRunAssembled: () => assembled,
  }
}

/** Resolve one checks run against the cache. Exported for the tests. */
export function answerCheck(
  board: BoardCache,
  toml: string,
  firmwareName: string | null,
): { response: RunResponse; assembled: boolean; detail: string } {
  const canon = canonicalizeSpec(toml)
  const wantKey = specKey(canon, firmwareName)

  // 1. The spec as a whole was recorded: hand back exactly what came off that
  //    run of the engine.
  const exact = board.checks.find(r => r.key === wantKey)
  if (exact) {
    return { response: exact.response, assembled: false, detail: exact.id }
  }

  // 2. Every ASSERTION in it was recorded on its own, in this run context (same
  //    duration, same supplies, same firmware). The engine is deterministic for
  //    a fixed board/firmware/supplies/duration, so each row is the verdict this
  //    spec's row would have; the widget's caption still says it was assembled.
  if (canon.asserts.length > 0) {
    const parts: RecordedRun[] = []
    for (const a of canon.asserts) {
      const key = assertKey(canon, a, firmwareName)
      const hit = board.checks.find(r => r.assert_keys.length === 1 && r.assert_keys[0] === key)
      if (!hit) {
        return {
          response: {
            ok: false,
            error: honestCheckRefusal(
              `No recorded run covers “${describeAssert(a.kind, a.fields)}” on this board`
              + `${firmwareName ? ' with the firmware staged' : ' with no firmware staged'}.`,
            ),
          },
          assembled: false,
          detail: `miss:${a.kind}`,
        }
      }
      parts.push(hit)
    }
    return { response: assemble(parts), assembled: true, detail: `assembled:${parts.length}` }
  }

  return {
    response: {
      ok: false,
      error: honestCheckRefusal('That spec has no assertions in it.'),
    },
    assembled: false,
    detail: 'empty',
  }
}

function describeAssert(kind: string, fields: Record<string, string | number>): string {
  const bits = Object.entries(fields).map(([k, v]) => `${k}=${v}`)
  return bits.length > 0 ? `${kind} (${bits.join(', ')})` : kind
}
