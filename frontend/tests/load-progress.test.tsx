import { expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { parseLoadProgress, readAnalysisStream } from '../src/lib/api'
import { LoadBar, loadPercent } from '../src/components/LoadBar'

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
