// The one client for the local engine: every HTTP endpoint, the SSE reader
// the two long-running POSTs share, and the /ws socket. Components and hooks
// call these by name so the URL, the method and the answer's shape live here
// once, and a stale server answers the same way everywhere.

import type {
  DepInfo, ExtractReady, LiveLaunchResponse, LiveStatus, ModelSaveResult,
  RunResponse, SensorCatalogEntry, Startup, WebReport,
} from '../types/report'
import type { ClientMessage, ServerMessage } from '../types/protocol'
import { buildBoardUpload } from './board-upload'

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

/** POST a JSON body and read the JSON answer, whatever the status: these
 *  endpoints report their own refusals in the body. */
export async function postJson<T>(url: string, body: unknown): Promise<T> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
  return await res.json() as T
}

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
async function postSse(url: string, body: FormData | undefined, onFrame: (frame: SseFrame) => void): Promise<void> {
  const res = await fetch(url, { method: 'POST', body })
  if (!res.ok || !res.body) throw new Error(`the server refused it (${statusLine(res)})`)
  await readSseStream(res.body, onFrame)
}

// ── The endpoints ────────────────────────────────────────────────────────────

export const api = {
  startup: () => getJson<Startup>('/api/startup'),
  liveStatus: () => getJson<LiveStatus>('/api/live/status'),
  liveLaunch: (upload: FormData) => postForm<LiveLaunchResponse>('/api/live/launch', upload),
  /** A bare board goes as its own bytes: a Blob body streams off disk, where
   *  `arrayBuffer()` would pull a 300 MB upload into the heap first. The
   *  companion form is the multipart route. The status is attached to a
   *  refusal so the caller can name it. */
  analyze: async (board: File, firmware: File | null, schematic: File | null, signal: AbortSignal): Promise<WebReport> => {
    const res = firmware || schematic
      ? await fetch('/api/analyze-with-firmware', { method: 'POST', body: buildBoardUpload(board, firmware, schematic), signal })
      : await fetch('/api/analyze', {
          method: 'POST',
          headers: { 'X-Board-Filename': board.name, 'Content-Type': 'application/octet-stream' },
          body: board,
          signal,
        })
    if (!res.ok) {
      const err = new Error((await res.text()).trim().slice(0, 400) || statusLine(res)) as Error & { status?: number }
      err.status = res.status
      throw err
    }
    return readJson<WebReport>(res)
  },
  /** hauksbee-ci reports its refusals in the body, so a non-2xx is parsed too. */
  check: (upload: FormData) => postForm<RunResponse>('/api/check', upload),
  sensorSpecs: (signal: AbortSignal) => getJson<{ entries?: SensorCatalogEntry[] }>('/api/sensor-specs', { signal }),
  deps: () => getJson<{ deps?: DepInfo[] }>('/api/deps'),
  depsInstall: (id: string, onFrame: (frame: SseFrame) => void) =>
    postSse(`/api/deps/install/${encodeURIComponent(id)}`, undefined, onFrame),
  extractReady: () => getJson<ExtractReady>('/api/models/extract/ready'),
  extract: (form: FormData, onFrame: (frame: SseFrame) => void) => postSse('/api/models/extract', form, onFrame),
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

/** Open /ws and keep it open: a dropped socket reconnects after two seconds
 *  until `close()`. Unparseable frames are dropped. */
export function openLiveSocket(handlers: {
  onOpen: () => void
  onMessage: (msg: ServerMessage) => void
  onClose: () => void
}): LiveSocket {
  let alive = true
  let ws: WebSocket | null = null
  let timer: ReturnType<typeof setTimeout> | null = null
  const connect = () => {
    if (!alive) return
    ws = new WebSocket(wsUrl())
    ws.onopen = () => { if (alive) handlers.onOpen(); else ws?.close() }
    ws.onmessage = (ev: MessageEvent) => {
      if (!alive) return
      try { handlers.onMessage(JSON.parse(ev.data as string) as ServerMessage) } catch { /* not a frame */ }
    }
    ws.onclose = () => {
      if (!alive) return
      handlers.onClose()
      timer = setTimeout(connect, 2000)
    }
    ws.onerror = () => ws?.close()
  }
  connect()
  return {
    send: msg => { if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg)) },
    close: () => {
      alive = false
      if (timer) clearTimeout(timer)
      ws?.close()
    },
  }
}
