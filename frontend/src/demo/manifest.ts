import type { ServerMessage } from '../types/protocol'
import type { Startup } from '../types/report'

// Types for demo/sessions/manifest.json and the .jsonl session files, exactly
// as frontend/capture/record-demo-sessions.ts writes them.

export interface ManifestAction {
  at_sim: number
  label: string
}

export interface ManifestSession {
  id: string
  board: string
  scenario: string
  title: string
  description: string
  board_name: string
  firmware: string | null
  mcus: [string, string][]
  engine_commit: string
  engine_version: string
  captured_at: string
  duration_s: number
  frames: number
  bytes: number
  /** 1 = every frame the wire carried; N > 1 = cadence thinned to 1-in-N to
   *  fit the size budget (disclosed in the session info). */
  thinned_keep_every: number
  /** Site-relative URLs to the session's assets. */
  session: string
  report: string
  board_file: string
  actions: ManifestAction[]
}

export interface DemoManifest {
  generated_at: string
  engine_commit: string
  engine_version: string
  sessions: ManifestSession[]
}

/** One recorded server message and its position on the sim-time axis. */
export interface SessionLine {
  at: number
  m: ServerMessage
}

export interface LoadedSession {
  entry: ManifestSession
  lines: SessionLine[]
  /** Timeline length: the last recorded message's sim time. */
  duration: number
  /** Where the board layout text lives on THIS site (the recorded
   *  BoardInfo.board_url points at the capture server's /boards route). */
  boardFileUrl: string
  startup: Startup | null
}

/** Parse a .jsonl session body. Lines carrying `sent` are the recorder's own
 *  scripted actions (kept in the file for transparency); replay consumes only
 *  the server messages. */
export function parseSession(entry: ManifestSession, text: string, startup: Startup | null): LoadedSession {
  const lines: SessionLine[] = []
  for (const raw of text.split('\n')) {
    if (!raw.trim()) continue
    let parsed: { type?: string; at?: number; m?: ServerMessage }
    try { parsed = JSON.parse(raw) } catch { continue }
    if (parsed.type === 'meta' || !parsed.m || typeof parsed.at !== 'number') continue
    lines.push({ at: parsed.at, m: parsed.m })
  }
  const duration = lines.length > 0 ? lines[lines.length - 1].at : 0
  return { entry, lines, duration, boardFileUrl: `/${entry.board_file}`, startup }
}
