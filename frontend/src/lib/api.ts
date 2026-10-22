// The one client for the local engine: every HTTP endpoint, the SSE reader
// the two long-running POSTs share, and the /ws socket. Components and hooks
// call these by name so the URL, the method and the answer's shape live here
// once, and a stale server answers the same way everywhere.

import type {
  DepInfo, DepsEnvironment, ExtractBackendsConfig, ExtractReady, ExtractSettings, LiveLaunchResponse, LiveStatus,
  ModelSaveResult, RunResponse, ScaffoldResponse, SensorCatalogEntry, Startup, WebCosimSection, WebReport,
} from '../types/report'
import type { ClientMessage, ServerMessage } from '../types/protocol'
import { buildBoardUpload, type SupplementalDesignFiles } from './board-upload'
import { restoreRepeats } from './intern'

/** What to show a user for a thrown value of unknown type. */
export const errorText = (e: unknown): string => (e instanceof Error ? e.message : String(e))

/** Fetch aborts (a newer run superseded this one) are expected, not errors. */
export const isAbort = (e: unknown): boolean => e instanceof Error && e.name === 'AbortError'

const statusLine = (res: Response) => `${res.status} ${res.statusText}`

/** GET a JSON endpoint, refusing anything but a 2xx. */
export async function getJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) throw new Error(`the server answered ${statusLine(res)}`)
  return await res.json() as T
}

/** Send a JSON body and read the JSON answer, whatever the status: these
 *  endpoints report their own refusals in the body, so a caller that needs to
 *  tell success from a 400 reads the status or the body's `error` itself (see
 *  `extractSettingsSave`). */
async function sendJson<T>(method: string, url: string, body: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
  return await res.json() as T
}

export const postJson = <T>(url: string, body: unknown): Promise<T> => sendJson<T>('POST', url, body)

/** Read a body ONCE as text, then parse defensively: a stale build, a proxy or
 *  a body-limit refusal answers in plaintext, and `res.json()` on that throws
 *  a SyntaxError that hides the real message. */
export async function readJson<T>(res: Response): Promise<T> {
  const text = await res.text()
  try {
    return JSON.parse(text) as T
  } catch {
    throw new Error(text.trim().slice(0, 400) || statusLine(res) || 'the server returned an empty or non-JSON response')
  }
}

/** POST a multipart body and parse the answer with `readJson`. */
export async function postForm<T>(url: string, body: FormData, signal?: AbortSignal): Promise<T> {
  return readJson<T>(await fetch(url, { method: 'POST', body, signal }))
}

/** Fetch a static URL (a bundled sample, a server-retained board) as a File
 *  named after its last path segment. */
export async function fetchFile(url: string, signal?: AbortSignal, fallbackName = 'file'): Promise<File> {
  const res = await fetch(url, { signal })
  if (!res.ok) throw new Error(`could not fetch ${url}: ${res.status}`)
  return new File([await res.blob()], url.split('/').pop() || fallbackName)
}

/** The layout text the board viewer draws for a served board file. A native
 *  KiCad layout is its own text. A schematic is retained as source (so a saved
 *  session can resume from the bytes it was checked on), so the server also
 *  keeps the extracted layout at `<url>/layout`. Anything else comes back as
 *  fetched, and the parser reports that it has no layout. */
export async function fetchLayoutText(url: string, signal?: AbortSignal): Promise<string> {
  const text = await (await fetchFile(url, signal)).text()
  if (/^\s*\(kicad_pcb\b/.test(text)) return text
  const res = await fetch(`${url}/layout`, { signal })
  if (!res.ok) return text
  const layout = await res.text()
  return /^\s*\(kicad_pcb\b/.test(layout) ? layout : text
}

// ── Server-Sent Events ───────────────────────────────────────────────────────
// Installs and datasheet extraction run for minutes, and a request that blocks
// silently until the end is indistinguishable from a hang. `EventSource` cannot
// POST, so the body is read by hand; this is the one copy of the frame
// splitting (a frame split across chunks, a `data:` with no space, a
// multi-line payload) because two copies drift in different ways.

export interface SseFrame {
  event: string
  data: string
}

/** Read an SSE body to the end, calling `onFrame` per frame. It does NOT
 *  decide what a missing terminal event means: "the connection dropped" and
 *  "the install failed" need different words, so the caller notices. */
export async function readSseStream(body: ReadableStream<Uint8Array>, onFrame: (frame: SseFrame) => void): Promise<void> {
  const reader = body.getReader()
  const dec = new TextDecoder()
  let buf = ''
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buf += dec.decode(value, { stream: true })
    let idx: number
    while ((idx = buf.indexOf('\n\n')) >= 0) {
      const raw = buf.slice(0, idx)
      buf = buf.slice(idx + 2)
      // The servers emit an unlabelled progress line as the `log` event.
      let event = 'log'
      const data: string[] = []
      for (const line of raw.split('\n')) {
        if (line.startsWith('event: ')) event = line.slice(7).trim()
        else if (line.startsWith('data: ')) data.push(line.slice(6))
        else if (line === 'data:') data.push('')
      }
      onFrame({ event, data: data.join('\n') })
    }
  }
}

/** POST and stream the SSE answer. Throws when the server refuses the request
 *  outright; returns once the stream closes. */
export async function postSse(url: string, body: FormData | undefined, onFrame: (frame: SseFrame) => void): Promise<void> {
  const res = await fetch(url, { method: 'POST', body })
  if (!res.ok || !res.body) throw new Error(`the server refused it (${statusLine(res)})`)
  await readSseStream(res.body, onFrame)
}

// ── Board-load progress ──────────────────────────────────────────────────────
// Loading a large board takes tens of seconds. A page that asks for the
// analysis as a stream (`Accept: text/event-stream`) gets `progress` frames while
// it runs and then one `done` frame carrying the same JSON document the plain
// endpoint answers. A server or a test double that does not stream answers the
// plain JSON, and the page reads that exactly as before.

/** Where a board load is: the stage in user words and the overall fraction. */
export interface LoadProgress {
  stage: string
  /** The words for the current stage, from the engine's one stage table. */
  label: string
  /** Overall progress, 0..1. The engine never reports a smaller value than before. */
  fraction: number
  elapsedMs: number
  /** Milliseconds left at the pace so far, once there is enough pace to read. */
  etaMs?: number
}

/** One `progress` frame's data, or null when it is not a progress report. */
export function parseLoadProgress(data: string): LoadProgress | null {
  try {
    const v = JSON.parse(data) as Partial<Record<'stage' | 'label' | 'fraction' | 'elapsed_ms' | 'eta_ms', unknown>>
    if (typeof v.label !== 'string' || typeof v.fraction !== 'number' || !Number.isFinite(v.fraction)) return null
    return {
      stage: typeof v.stage === 'string' ? v.stage : '',
      label: v.label,
      fraction: Math.min(1, Math.max(0, v.fraction)),
      elapsedMs: typeof v.elapsed_ms === 'number' ? v.elapsed_ms : 0,
      etaMs: typeof v.eta_ms === 'number' && Number.isFinite(v.eta_ms) ? v.eta_ms : undefined,
    }
  } catch {
    return null
  }
}

/** Read a streamed analysis: progress frames go to `onProgress`, and the answer
 *  is the `done` frame's data. `done` is undefined when the stream ended without
 *  one (a dropped connection), which the caller answers by asking again without
 *  streaming; `error` is the engine's own message when it reported one. */
export async function readAnalysisStream(
  body: ReadableStream<Uint8Array>,
  onProgress: (p: LoadProgress) => void,
): Promise<{ done?: string; error?: string }> {
  const out: { done?: string; error?: string } = {}
  await readSseStream(body, frame => {
    if (frame.event === 'progress') {
      const p = parseLoadProgress(frame.data)
      if (p) onProgress(p)
    } else if (frame.event === 'done') out.done = frame.data
    else if (frame.event === 'error') out.error = frame.data
  })
  return out
}

/** POST a multipart body and read the answer as a stream when the server gives
 *  one: progress frames go to `onProgress`, and the `done` frame is the same
 *  JSON document the plain endpoint answers. A server that does not stream
 *  answers plain JSON, which is read as before; `signal` aborting closes the
 *  request, which is how the server learns the page no longer wants the answer.
 *  A stream that ends with no answer throws, because asking again would start
 *  the whole run over. */
export async function postFormStreamed<T>(
  url: string,
  body: FormData,
  signal?: AbortSignal,
  onProgress?: (p: LoadProgress) => void,
): Promise<T> {
  const res = await fetch(url, {
    method: 'POST',
    body,
    signal,
    headers: onProgress ? { Accept: 'text/event-stream' } : {},
  })
  if (onProgress && res.body && (res.headers.get('content-type') ?? '').includes('text/event-stream')) {
    const streamed = await readAnalysisStream(res.body, onProgress)
    if (streamed.done !== undefined) return JSON.parse(streamed.done) as T
    throw new Error(streamed.error ?? 'the connection to the server dropped before the run finished')
  }
  return readJson<T>(res)
}

/** Read a settings answer, preferring the server's own refusal message over
 *  the bare status line: both settings writes report a 400 in the body. */
const settingsOrRefusal = async (sent: Promise<Response>): Promise<ExtractSettings> => {
  const res = await sent
  const json = await readJson<ExtractSettings & { error?: string }>(res)
  if (!res.ok) throw new Error(json.error ?? `the server answered ${statusLine(res)}`)
  return json
}

// ── The endpoints ────────────────────────────────────────────────────────────

export const api = {
  startup: async (): Promise<Startup> => {
    const startup = await getJson<Startup>('/api/startup')
    // A preloaded large report travels with its repeated values shared.
    return startup.preloaded && startup.report ? { ...startup, report: restoreRepeats(startup.report) } : startup
  },
  liveStatus: () => getJson<LiveStatus>('/api/live/status'),
  /** Launching replaces the one live session and the server closes every
   *  socket attached to the old one. The page's `/ws` is held, and reattached
   *  to the new session the moment the launch answers, so a command typed in
   *  between goes to the new session rather than to a dying one. */
  liveLaunch: async (upload: FormData) => {
    sessionReplacing = true
    try {
      const launched = await postForm<LiveLaunchResponse>('/api/live/launch', upload)
      settleLiveSockets(launched.ok)
      return launched
    } catch (e) {
      settleLiveSockets(false)
      throw e
    }
  },
  /** A bare board goes as its own bytes: a Blob body streams off disk, where
   *  `arrayBuffer()` would pull a 300 MB upload into the heap first. The
   *  companion form is the multipart route. The status is attached to a
   *  refusal so the caller can name it. */
  analyze: async (
    board: File,
    firmware: File | null,
    schematic: File | null,
    signal: AbortSignal,
    supplemental?: SupplementalDesignFiles,
    onProgress?: (p: LoadProgress) => void,
  ): Promise<WebReport> => {
    const hasSupplemental = !!supplemental && (
      !!supplemental.bom || !!supplemental.placement || !!supplemental.variant
      || !!supplemental.asbuilt || supplemental.models.length > 0
    )
    const send = (stream: boolean) => {
      const accept: Record<string, string> = stream ? { Accept: 'text/event-stream' } : {}
      return firmware || schematic || hasSupplemental
        ? fetch('/api/analyze-with-firmware', {
            method: 'POST',
            headers: accept,
            body: buildBoardUpload(board, firmware, schematic, supplemental),
            signal,
          })
        : fetch('/api/analyze', {
            method: 'POST',
            headers: { 'X-Board-Filename': board.name, 'Content-Type': 'application/octet-stream', ...accept },
            body: board,
            signal,
          })
    }
    const refuse = async (res: Response): Promise<never> => {
      const err = new Error((await res.text()).trim().slice(0, 400) || statusLine(res)) as Error & { status?: number }
      err.status = res.status
      throw err
    }
    let res = await send(!!onProgress)
    if (!res.ok) return refuse(res)
    if (onProgress && res.body && (res.headers.get('content-type') ?? '').includes('text/event-stream')) {
      const streamed = await readAnalysisStream(res.body, onProgress)
      if (streamed.done !== undefined) return restoreRepeats(JSON.parse(streamed.done) as WebReport)
      if (streamed.error !== undefined) throw new Error(streamed.error)
      // The stream ended with no answer (a dropped connection, a proxy that cut
      // it): the report still has to arrive, so ask again the plain way.
      res = await send(false)
      if (!res.ok) return refuse(res)
    }
    return restoreRepeats(await readJson<WebReport>(res))
  },
  /** hauksbee-ci reports its refusals in the body, so a non-2xx is parsed too. */
  check: (upload: FormData, signal?: AbortSignal, onProgress?: (p: LoadProgress) => void) =>
    postFormStreamed<RunResponse>('/api/check', upload, signal, onProgress),
  /** The starter spec's supplies for an uploaded board: the answer of
   *  `hauksbee-ci init`, so the builder starts from what the CLI would write. */
  scaffold: (upload: FormData, signal?: AbortSignal, onProgress?: (p: LoadProgress) => void) =>
    postFormStreamed<ScaffoldResponse>('/api/ci/scaffold', upload, signal, onProgress),
  sensorSpecs: (signal: AbortSignal) => getJson<{ entries?: SensorCatalogEntry[] }>('/api/sensor-specs', { signal }),
  deps: () => getJson<{ deps?: DepInfo[]; environment?: DepsEnvironment }>('/api/deps'),
  depsInstall: (id: string, onFrame: (frame: SseFrame) => void) =>
    postSse(`/api/deps/install/${encodeURIComponent(id)}`, undefined, onFrame),
  extractReady: () => getJson<ExtractReady>('/api/models/extract/ready'),
  extract: (form: FormData, onFrame: (frame: SseFrame) => void) => postSse('/api/models/extract', form, onFrame),
  extractSettings: () => getJson<ExtractSettings>('/api/settings/extract'),
  /** PUT the backend config. Unlike `postJson`, this throws on a non-2xx (a
   *  400 with `{"error": "..."}`) so the settings page can show the server's
   *  own refusal message next to Save rather than silently swallowing it. */
  extractSettingsSave: (config: ExtractBackendsConfig): Promise<ExtractSettings> =>
    settingsOrRefusal(fetch('/api/settings/extract', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(config),
    })),
  extractSettingsPreset: (id: string): Promise<ExtractSettings> =>
    settingsOrRefusal(fetch(`/api/settings/extract/preset/${encodeURIComponent(id)}`, { method: 'POST' })),
  /** Runs one tiny model call against the configured backend, streamed the
   *  same way an install or an extraction is. */
  extractSettingsTest: (onFrame: (frame: SseFrame) => void) =>
    postSse('/api/settings/extract/test', undefined, onFrame),
  modelsDraft: (body: unknown) => postJson<{ ok?: boolean; toml?: string; error?: string }>('/api/models/draft', body),
  modelsCheck: (body: { toml: string; format: string }) =>
    postJson<{ ok?: boolean; summary?: string; error?: string }>('/api/models/check', body),
  modelsSave: (body: { part: string; kind: string; toml: string }) => postJson<ModelSaveResult>('/api/models/save', body),
  /** The CURRENT live session's board file, as bytes; null when the server
   *  does not have one by that name. */
  boardBytes: async (fileName: string): Promise<Uint8Array | null> => {
    const res = await fetch(`/boards/${encodeURIComponent(fileName)}`)
    return res.ok ? new Uint8Array(await res.arrayBuffer()) : null
  },
}

// ── The emulator co-sim job ──────────────────────────────────────────────────
// A board whose MCU needs Renode or QEMU is never run by the synchronous
// analysis (booting an emulator takes seconds, not milliseconds). The page
// starts it as a job with the same multipart body the analysis used, and polls.

/** What the server says about a co-sim job. */
export interface CosimJobStatus {
  state: 'running' | 'done' | 'failed'
  /** Simulated seconds done so far, and the run's total. */
  progress_s?: number
  total_s?: number
  /** The finished co-sim section, when `state` is `done`. */
  section?: WebCosimSection
  error?: string
}

/** How often a running job is asked how far it is. */
export const COSIM_POLL_MS = 1500

/** The words for a job's progress: `Booting the emulator ... 3.2 / 10 s simulated`. */
export function cosimProgressLine(progressS?: number, totalS?: number): string {
  if (progressS === undefined || !Number.isFinite(progressS) || progressS <= 0) return 'Booting the emulator ...'
  const done = progressS.toFixed(1)
  return totalS !== undefined && Number.isFinite(totalS) && totalS > 0
    ? `Booting the emulator ... ${done} / ${Number(totalS.toPrecision(3))} s simulated`
    : `Booting the emulator ... ${done} s simulated`
}

/** Read a job answer, preferring the server's own words over the bare status. */
async function jobAnswer<T>(res: Response): Promise<T> {
  const text = await res.text()
  let body: (T & { error?: string }) | null = null
  try { body = JSON.parse(text) as T & { error?: string } } catch { /* not JSON */ }
  if (!res.ok) throw new Error(body?.error ?? (text.trim().slice(0, 400) || statusLine(res)))
  if (body === null) throw new Error(text.trim().slice(0, 400) || 'the server returned an empty or non-JSON response')
  return body
}

/** Start the job; the answer is its id. */
export async function startCosimJob(upload: FormData, signal?: AbortSignal): Promise<string> {
  const res = await fetch('/api/cosim-run', { method: 'POST', body: upload, signal })
  const { job_id } = await jobAnswer<{ job_id?: string }>(res)
  if (!job_id) throw new Error('the server did not name the co-sim job it started')
  return job_id
}

/** Ask how a job is doing. */
export async function cosimJobStatus(jobId: string, signal?: AbortSignal): Promise<CosimJobStatus> {
  return jobAnswer<CosimJobStatus>(await fetch(`/api/cosim-run/${encodeURIComponent(jobId)}`, { signal }))
}

/** Start a job and follow it to the end: progress goes to `onProgress`, the
 *  finished section is the answer, and a failed job throws its own error text.
 *  `sleep` is injectable so a test does not wait out the polling interval. */
export async function runCosimJob(
  upload: FormData,
  opts: {
    signal?: AbortSignal
    onProgress?: (progressS: number | undefined, totalS: number | undefined) => void
    intervalMs?: number
    sleep?: (ms: number, signal?: AbortSignal) => Promise<void>
  } = {},
): Promise<WebCosimSection> {
  const { signal, onProgress, intervalMs = COSIM_POLL_MS, sleep = abortableSleep } = opts
  const id = await startCosimJob(upload, signal)
  onProgress?.(undefined, undefined)
  for (;;) {
    await sleep(intervalMs, signal)
    if (signal?.aborted) throw new DOMException('aborted', 'AbortError')
    const status = await cosimJobStatus(id, signal)
    if (status.state === 'done') {
      if (!status.section) throw new Error('the co-sim job finished without a result')
      return status.section
    }
    if (status.state === 'failed') throw new Error(status.error || 'the co-sim job failed')
    onProgress?.(status.progress_s, status.total_s)
  }
}

function abortableSleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new DOMException('aborted', 'AbortError')); return }
    const timer = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve() }, ms)
    const onAbort = () => { clearTimeout(timer); reject(new DOMException('aborted', 'AbortError')) }
    signal?.addEventListener('abort', onAbort, { once: true })
  })
}

// ── The live socket ──────────────────────────────────────────────────────────

// Same origin as the page, so the viewer works on any `hauksbee run --port`
// (and over https); `vite dev` proxies `/ws` to the backend. VITE_WS_URL is
// only for unusual split setups.
const wsUrl = () => import.meta.env.VITE_WS_URL
  ?? `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}/ws`

export interface LiveSocket {
  send: (msg: ClientMessage) => void
  close: () => void
}

/** How long a command waits for the socket to (re)open before it is dropped:
 *  past that the user has moved on and replaying it would surprise them. A
 *  launch of a big board can take most of this. */
const QUEUED_COMMAND_TTL_MS = 30_000
const QUEUED_COMMAND_MAX = 32

/** A launch is replacing the live session (see `api.liveLaunch`). While true no
 *  socket sends: whatever is open is attached to the session being torn down. */
let sessionReplacing = false
const liveSockets = new Set<(reattach: boolean) => void>()

/** The launch answered: every socket either reattaches to the new session
 *  (`true`) or, when the launch failed and the old session stands, carries on. */
function settleLiveSockets(reattach: boolean): void {
  sessionReplacing = false
  for (const settle of liveSockets) settle(reattach)
}

/** Open /ws and keep it open: a dropped socket reconnects after two seconds
 *  until `close()`. Unparseable frames are dropped. A command sent while the
 *  socket is connecting, between reconnects, or while a launch is replacing the
 *  session is held and sent, in order, once the right socket is open, so the
 *  first Play after a launch is not lost to that gap. */
export function openLiveSocket(handlers: {
  onOpen: () => void
  onMessage: (msg: ServerMessage) => void
  onClose: () => void
}): LiveSocket {
  let alive = true
  let ws: WebSocket | null = null
  let timer: ReturnType<typeof setTimeout> | null = null
  let queued: { msg: ClientMessage; at: number }[] = []
  const isOpen = () => ws?.readyState === WebSocket.OPEN
  const flush = () => {
    const now = Date.now()
    const due = queued.filter(q => now - q.at <= QUEUED_COMMAND_TTL_MS)
    queued = []
    for (const q of due) ws?.send(JSON.stringify(q.msg))
  }
  const connect = () => {
    if (!alive) return
    const socket = new WebSocket(wsUrl())
    ws = socket
    socket.onopen = () => {
      if (!alive) { socket.close(); return }
      if (!sessionReplacing) flush()
      handlers.onOpen()
    }
    socket.onmessage = (ev: MessageEvent) => {
      if (!alive) return
      try { handlers.onMessage(JSON.parse(ev.data as string) as ServerMessage) } catch { /* not a frame */ }
    }
    socket.onclose = () => {
      if (!alive) return
      handlers.onClose()
      timer = setTimeout(connect, 2000)
    }
    socket.onerror = () => socket.close()
  }
  const settle = (reattach: boolean) => {
    if (!alive) return
    if (!reattach) { if (isOpen()) flush(); return }
    // Drop the socket that belongs to the replaced session without waiting for
    // the server's close (or the two second timer) and attach to the new one.
    if (timer) { clearTimeout(timer); timer = null }
    if (ws) {
      ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null
      ws.close()
      handlers.onClose()
    }
    connect()
  }
  liveSockets.add(settle)
  connect()
  return {
    send: msg => {
      if (!sessionReplacing && isOpen()) { ws?.send(JSON.stringify(msg)); return }
      queued = [...queued, { msg, at: Date.now() }].slice(-QUEUED_COMMAND_MAX)
    },
    close: () => {
      alive = false
      queued = []
      liveSockets.delete(settle)
      if (timer) clearTimeout(timer)
      ws?.close()
    },
  }
}
