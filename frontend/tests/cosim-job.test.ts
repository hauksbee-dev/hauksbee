// The emulator co-sim job client: start, poll, progress words, failure text and
// abort, against a fake fetch and an injected sleep (no real waiting).

import { afterEach, describe, expect, test } from 'bun:test'
import { COSIM_POLL_MS, cosimJobStatus, cosimProgressLine, runCosimJob, startCosimJob } from '../src/lib/api'
import type { WebCosimSection } from '../src/types/report'

const realFetch = globalThis.fetch
afterEach(() => { globalThis.fetch = realFetch })

const section: WebCosimSection = { ran: true, seconds_simulated: 10, uart_output: 'hi', findings: [], analog_valid: true }
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

/** A fake server: a script of answers to the polls, in order. */
function fakeServer(polls: unknown[], opts: { startStatus?: number; startBody?: unknown } = {}) {
  const seen: { url: string; method: string; body: unknown }[] = []
  let i = 0
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    seen.push({ url, method: init?.method ?? 'GET', body: init?.body })
    if (url === '/api/cosim-run') return json(opts.startBody ?? { job_id: 'j1' }, opts.startStatus ?? 200)
    const next = polls[Math.min(i++, polls.length - 1)]
    return json(next)
  }) as unknown as typeof fetch
  return seen
}

const upload = () => { const f = new FormData(); f.append('board', new Blob(['x']), 'b.kicad_pcb'); return f }
const noWait = async () => {}

/** The message a promise rejects with (a bare `.rejects` trips the lint's await rule). */
async function rejection(p: Promise<unknown>): Promise<string> {
  try { await p } catch (e) { return (e instanceof Error ? e.message : String(e)) || 'rejected' }
  return ''
}

describe('progress words', () => {
  test('before any progress it only says the emulator is starting', () => {
    expect(cosimProgressLine()).toBe('Booting the emulator ...')
    expect(cosimProgressLine(0, 10)).toBe('Booting the emulator ...')
  })

  test('with progress it counts simulated seconds against the total', () => {
    expect(cosimProgressLine(3.21, 10)).toBe('Booting the emulator ... 3.2 / 10 s simulated')
    expect(cosimProgressLine(3.2)).toBe('Booting the emulator ... 3.2 s simulated')
    expect(cosimProgressLine(1, Number.NaN)).toBe('Booting the emulator ... 1.0 s simulated')
  })
})

describe('starting a job', () => {
  test('POSTs the same multipart body and returns the job id', async () => {
    const seen = fakeServer([])
    const form = upload()
    expect(await startCosimJob(form)).toBe('j1')
    expect(seen[0].url).toBe('/api/cosim-run')
    expect(seen[0].method).toBe('POST')
    expect(seen[0].body).toBe(form)
  })

  test('an answer with no job id is an error, not a silent hang', async () => {
    fakeServer([], { startBody: {} })
    expect(await rejection(startCosimJob(upload()))).toContain('did not name')
  })

  test('the server error text is shown, not just the status', async () => {
    fakeServer([], { startStatus: 400, startBody: { error: 'No firmware was uploaded' } })
    expect(await rejection(startCosimJob(upload()))).toContain('No firmware was uploaded')
  })

  test('a non-JSON failure page still gives readable text', async () => {
    globalThis.fetch = (async () => new Response('Bad gateway', { status: 502 })) as unknown as typeof fetch
    expect(await rejection(startCosimJob(upload()))).toContain('Bad gateway')
  })

  test('the job id is escaped when polling', async () => {
    const seen = fakeServer([{ state: 'running' }])
    await cosimJobStatus('a/b c')
    expect(seen[0].url).toBe('/api/cosim-run/a%2Fb%20c')
  })
})

describe('following a job', () => {
  test('polls through running to done and returns the section', async () => {
    const seen = fakeServer([
      { state: 'running' },
      { state: 'running', progress_s: 3.2, total_s: 10 },
      { state: 'done', section },
    ])
    const progress: [number | undefined, number | undefined][] = []
    const waits: number[] = []
    const got = await runCosimJob(upload(), {
      onProgress: (p, t) => progress.push([p, t]),
      sleep: async ms => { waits.push(ms) },
    })
    expect(got).toEqual(section)
    expect(progress).toEqual([[undefined, undefined], [undefined, undefined], [3.2, 10]])
    expect(waits).toEqual([COSIM_POLL_MS, COSIM_POLL_MS, COSIM_POLL_MS])
    expect(COSIM_POLL_MS).toBe(1500)
    expect(seen.filter(s => s.method === 'GET')).toHaveLength(3)
  })

  test('a failed job throws the server text; no text still says it failed', async () => {
    fakeServer([{ state: 'running', progress_s: 1, total_s: 10 }, { state: 'failed', error: 'renode exited with code 1' }])
    expect(await rejection(runCosimJob(upload(), { sleep: noWait }))).toContain('renode exited with code 1')
    fakeServer([{ state: 'failed' }])
    expect(await rejection(runCosimJob(upload(), { sleep: noWait }))).toContain('failed')
  })

  test('done without a section is an error', async () => {
    fakeServer([{ state: 'done' }])
    expect(await rejection(runCosimJob(upload(), { sleep: noWait }))).toContain('without a result')
  })

  test('a poll that errors stops the run with that error', async () => {
    let n = 0
    globalThis.fetch = (async (input: RequestInfo | URL) => {
      if (String(input) === '/api/cosim-run') return json({ job_id: 'j1' })
      n++
      return json({ error: 'no such job' }, 404)
    }) as unknown as typeof fetch
    expect(await rejection(runCosimJob(upload(), { sleep: noWait }))).toContain('no such job')
    expect(n).toBe(1)
  })

  test('aborting stops polling', async () => {
    const seen = fakeServer([{ state: 'running' }])
    const ctl = new AbortController()
    const p = runCosimJob(upload(), { signal: ctl.signal, sleep: async () => { ctl.abort() } })
    expect(await rejection(p)).toBeTruthy()
    expect(seen.filter(s => s.method === 'GET')).toHaveLength(0)
  })

  test('the real sleep is cut short by an abort', async () => {
    fakeServer([{ state: 'running' }])
    const ctl = new AbortController()
    const t0 = Date.now()
    const p = runCosimJob(upload(), { signal: ctl.signal })
    setTimeout(() => ctl.abort(), 20)
    expect(await rejection(p)).toBeTruthy()
    expect(Date.now() - t0).toBeLessThan(1000)
  })
})
