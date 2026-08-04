// The recorded interaction cache, as demo/capture/record-embed-cache.ts wrote
// it, plus the loader that pulls one board's worth of it (lazily, per board).

import type { CheckRowSeed, SupplyRow } from '../shared/checks-spec'

export interface CheckResultRow {
  label: string
  kind: string
  passed: boolean
  invalid: boolean
  detail: string
}

export interface RunResponse {
  ok: boolean
  error?: string
  passed?: boolean
  exit_code?: number
  analog_abort?: boolean
  coverage?: string | null
  substitutions?: string[]
  coverage_warnings?: string[]
  results?: CheckResultRow[]
  [k: string]: unknown
}

export interface RecordedRun {
  id: string
  label: string
  note: string
  key: string
  assert_keys: string[]
  kind: 'rule' | 'preset'
  rule_id?: string
  rows: CheckRowSeed[]
  toml: string
  firmware: string | null
  verdict: 'passed' | 'failed' | 'invalid' | 'error'
  wall_ms: number
  response: RunResponse
}

export interface BoardCache {
  id: string
  title: string
  tagline: string
  session_id: string
  engine_commit: string
  engine_version: string
  recorded_at: string
  board_file: string
  board_name: string
  firmware_file: string | null
  firmware_name: string | null
  firmware_source: string | null
  board_source: string
  duration_ms: string
  spec_name: string
  supplies: SupplyRow[]
  feature_nets: string[]
  feature_parts: string[]
  nets: string[]
  parts: { ref: string; value: string }[]
  default_preset: string
  analyze: Record<string, unknown> & { file_name: string; num_components: number; num_nets: number }
  analyze_with_firmware: (Record<string, unknown> & { file_name: string }) | null
  live_status: Record<string, unknown>
  checks: RecordedRun[]
}

export interface CacheIndexEntry {
  id: string
  title: string
  tagline: string
  cache: string
  session_id: string
  board_file: string
  firmware_file: string | null
  recorded_runs: number
  verdicts: { passed: number; failed: number; invalid: number; error: number }
}

export interface CacheIndex {
  generated_at: string
  engine_commit: string
  engine_version: string
  boards: CacheIndexEntry[]
}

/** Everything one board needs, in the browser: the recorded responses, the
 *  board bytes as a File (the app's own upload path takes a File), the
 *  firmware where one is staged, and the recorded sim session. */
export interface LoadedBoard {
  cache: BoardCache
  boardFile: File
  firmwareFile: File | null
  /** Raw .jsonl text of the recorded sim session (parsed by the replay hook). */
  sessionText: string
  /** The session's manifest entry. */
  sessionEntry: unknown
}

/** Where the widget's assets live, e.g. "/hauksbee-embed/". Always ends in a
 *  slash. Sessions and the cache hang off it. */
export function normalizeAssetBase(base: string): string {
  if (!base) return './'
  return base.endsWith('/') ? base : `${base}/`
}

async function fetchText(url: string): Promise<string> {
  const res = await realFetch(url)
  if (!res.ok) throw new Error(`${url} answered ${res.status}`)
  return res.text()
}

/** The page's fetch as it was before the transport shim replaced it. Captured
 *  at module load so the loader's own requests can never be intercepted by the
 *  shim (which would be a loop, and would also hide them from a network
 *  assertion in the tests). */
export const realFetch: typeof fetch = globalThis.fetch.bind(globalThis)

export async function loadIndex(assetBase: string): Promise<CacheIndex> {
  const base = normalizeAssetBase(assetBase)
  return JSON.parse(await fetchText(`${base}sessions/cache/index.json`)) as CacheIndex
}

export interface SessionManifest {
  sessions: { id: string; session: string; report: string; board_file: string }[]
  [k: string]: unknown
}

export async function loadManifest(assetBase: string): Promise<SessionManifest> {
  const base = normalizeAssetBase(assetBase)
  return JSON.parse(await fetchText(`${base}sessions/manifest.json`)) as SessionManifest
}

/** Load one board: the cache, the board bytes, the firmware, the session. Every
 *  request is a static asset under the widget's asset base; there is no engine
 *  and no API on the other end of any of them. */
export async function loadBoard(
  assetBase: string,
  entry: CacheIndexEntry,
  manifest: SessionManifest,
): Promise<LoadedBoard> {
  const base = normalizeAssetBase(assetBase)
  const cache = JSON.parse(await fetchText(`${base}${entry.cache}`)) as BoardCache
  const sessionEntry = manifest.sessions.find(s => s.id === cache.session_id)
  if (!sessionEntry) throw new Error(`no recorded session "${cache.session_id}" in the manifest`)

  const [boardText, sessionText, firmwareBuf] = await Promise.all([
    fetchText(`${base}${cache.board_file}`),
    fetchText(`${base}${sessionEntry.session}`),
    cache.firmware_file
      ? realFetch(`${base}${cache.firmware_file}`).then(r => {
          if (!r.ok) throw new Error(`${cache.firmware_file} answered ${r.status}`)
          return r.arrayBuffer()
        })
      : Promise.resolve(null),
  ])

  return {
    cache,
    boardFile: new File([boardText], cache.board_name, { type: 'text/plain' }),
    firmwareFile: firmwareBuf && cache.firmware_name
      ? new File([firmwareBuf], cache.firmware_name, { type: 'application/octet-stream' })
      : null,
    sessionText,
    sessionEntry,
  }
}
