import { expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { api, parseLoadProgress, readAnalysisStream } from '../src/lib/api'
import { etaText, LoadBar, loadPercent } from '../src/components/LoadBar'

const encoder = new TextEncoder()
function streamOf(...chunks: string[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(c) {
      for (const chunk of chunks) c.enqueue(encoder.encode(chunk))
      c.close()
    },
  })
}
const frame = (event: string, data: string) => `event: ${event}\ndata: ${data}\n\n`

test('a progress frame is read into the stage words and a clamped fraction', () => {
  const p = parseLoadProgress('{"stage":"bind","label":"Matching parts to models","fraction":0.42,"elapsed_ms":1200}')
  expect(p).toEqual({ stage: 'bind', label: 'Matching parts to models', fraction: 0.42, elapsedMs: 1200 })
  expect(parseLoadProgress('{"label":"x","fraction":7}')?.fraction).toBe(1)
  expect(parseLoadProgress('not json')).toBeNull()
  expect(parseLoadProgress('{"label":"x"}')).toBeNull()
})

test('the stream hands progress to the callback and the report back as done', async () => {
  const seen: number[] = []
  const out = await readAnalysisStream(
    streamOf(
      frame('progress', '{"stage":"read","label":"Reading the board file","fraction":0.1,"elapsed_ms":5}'),
      frame('progress', '{"stage":"bind","label":"Matching parts to models","fraction":0.5,"elapsed_ms":50}'),
      frame('done', '{"verdict":"ok"}'),
    ),
    p => seen.push(p.fraction),
  )
  expect(seen).toEqual([0.1, 0.5])
  expect(out.done).toBe('{"verdict":"ok"}')
})

test('a stream that ends with no done frame says so, so the caller can ask again', async () => {
  const out = await readAnalysisStream(
    streamOf(frame('progress', '{"label":"x","fraction":0.3}')),
    () => {},
  )
  expect(out.done).toBeUndefined()
  expect(out.error).toBeUndefined()
})

test('an error frame carries the engine message', async () => {
  const out = await readAnalysisStream(streamOf(frame('error', 'the analysis task panicked')), () => {})
  expect(out.error).toBe('the analysis task panicked')
})

test('the percentage never reads 100 before the load is done', () => {
  expect(loadPercent(0)).toBe(0)
  expect(loadPercent(0.996)).toBe(99)
  expect(loadPercent(1)).toBe(100)
  expect(loadPercent(-3)).toBe(0)
})

const reading = { stage: 'read', label: 'Reading the board file', fraction: 0.4, elapsedMs: 900 }

test('the bar is a labelled progressbar once a report has arrived', () => {
  const html = renderToStaticMarkup(<LoadBar delayMs={0} progress={reading} />)
  expect(html).toContain('role="progressbar"')
  expect(html).toContain('aria-valuenow="40"')
  expect(html).toContain('aria-valuetext="40 percent, Reading the board file"')
  expect(html).toContain('Reading the board file')
  expect(html).toContain('40%')
})

test('there is no bar without a report, or before the short-load delay has passed', () => {
  expect(renderToStaticMarkup(<LoadBar delayMs={0} progress={null} />)).not.toContain('progressbar')
  // A fresh load has waited nothing yet, so a quick one never draws the bar.
  expect(renderToStaticMarkup(<LoadBar progress={reading} />)).not.toContain('progressbar')
})

test('a progress frame carries the engine estimate of the time left when it has one', () => {
  expect(parseLoadProgress('{"label":"Simulating","fraction":0.25,"elapsed_ms":10000,"eta_ms":30000}')?.etaMs).toBe(30000)
  expect(parseLoadProgress('{"label":"Simulating","fraction":0.01,"elapsed_ms":100,"eta_ms":null}')?.etaMs).toBeUndefined()
})

test('the time left is said in rounded words and held back until there is an estimate', () => {
  expect(etaText(undefined)).toBeNull()
  expect(etaText(2000)).toBe('almost done')
  expect(etaText(42_000)).toBe('about 40 s left')
  expect(etaText(5 * 60_000)).toBe('about 5 min left')
  expect(etaText(3 * 3_600_000)).toBe('about 3 h left')
})

test('the bar names what it is for and shows the estimate beside the percentage', () => {
  const html = renderToStaticMarkup(
    <LoadBar delayMs={0} ariaLabel="Running the checks" progress={{ ...reading, label: 'Simulating', etaMs: 120_000 }} />,
  )
  expect(html).toContain('aria-label="Running the checks"')
  expect(html).toContain('40%, about 2 min left')
})

function sseResponse(...frames: string[]): Response {
  return new Response(streamOf(...frames), { headers: { 'content-type': 'text/event-stream' } })
}
const realFetch = globalThis.fetch

test('a streamed run reports progress and returns the document in the done frame', async () => {
  const asked: { accept: string | null } = { accept: null }
  globalThis.fetch = (async (_url: unknown, init?: RequestInit) => {
    asked.accept = new Headers(init?.headers).get('accept')
    return sseResponse(
      frame('progress', '{"label":"Simulating","fraction":0.5,"elapsed_ms":4000,"eta_ms":4000}'),
      frame('done', '{"ok":true,"results":[]}'),
    )
  }) as unknown as typeof fetch
  try {
    const seen: string[] = []
    const out = await api.check(new FormData(), undefined, p => seen.push(p.label))
    expect(asked.accept).toBe('text/event-stream')
    expect(seen).toEqual(['Simulating'])
    expect(out.ok).toBe(true)
  } finally {
    globalThis.fetch = realFetch
  }
})

test('a server that does not stream is read as plain JSON, as before', async () => {
  globalThis.fetch = (async () => new Response('{"ok":false,"error":"nope"}', {
    headers: { 'content-type': 'application/json' },
  })) as unknown as typeof fetch
  try {
    const out = await api.check(new FormData(), undefined, () => {})
    expect(out).toEqual({ ok: false, error: 'nope' })
  } finally {
    globalThis.fetch = realFetch
  }
})

test('a stream that dies without an answer is an error, not a second run', async () => {
  let calls = 0
  globalThis.fetch = (async () => {
    calls += 1
    return sseResponse(frame('progress', '{"label":"Simulating","fraction":0.2,"elapsed_ms":900}'))
  }) as unknown as typeof fetch
  try {
    await expect(api.check(new FormData(), undefined, () => {})).rejects.toThrow('dropped')
    expect(calls).toBe(1)
  } finally {
    globalThis.fetch = realFetch
  }
})

test('an engine error frame is the message', async () => {
  globalThis.fetch = (async () => sseResponse(frame('error', 'the check task panicked; see the server log'))) as unknown as typeof fetch
  try {
    await expect(api.scaffold(new FormData(), undefined, () => {})).rejects.toThrow('panicked')
  } finally {
    globalThis.fetch = realFetch
  }
})
