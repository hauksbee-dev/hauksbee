// Named sessions in localStorage: what the app remembers between visits.
//
// A session holds everything about one board that the browser CAN keep: the
// report, which firmware/schematic was staged beside it, the composed spec and
// the last run's counts. What it cannot keep is the uploaded FILE — a `File` is
// a handle to bytes granted for one visit — so every affordance built on this
// store says so rather than restoring a report that looks live and then fails
// on the first action needing the bytes.
//
// Layout: a light index (one row per session, enough to draw the switcher) plus
// one record per session holding the heavy part, so the switcher opens without
// parsing a 50-finding report per row. The store is also subscribable: every
// write bumps one in-memory snapshot, and `useSessions` reads that through
// `useSyncExternalStore`, so no component keeps its own copy of the rows.

import type { WebReport } from '../types/report'
import { api } from './api'

const INDEX_KEY = 'hauksbee.sessions.v1'
const CURRENT_KEY = 'hauksbee.sessions.current'
const RECORD_PREFIX = 'hauksbee.session.v1.'

/** How many sessions are kept. Past this the least recently updated is
 *  dropped: an unbounded list is a quota failure waiting for a big board. */
const MAX_SESSIONS = 12

/** One switcher row: enough to name a session and say when it was last
 *  touched, without loading its report. */
export interface SessionRow {
  id: string
  name: string
  createdAt: number
  updatedAt: number
  /** Client clock when the report was produced (null for a restored session
   *  whose report predates this page-load). */
  analyzedAt: number | null
  board: { fileName: string; boardName: string; numComponents: number; numNets: number }
  firmwareName: string | null
  /** Companion bytes are not copied into browser storage. Optional so
   *  sessions written by older app versions still load. */
  schematicName?: string | null
  /** False when the report could not be stored (see `SaveOutcome`). */
  hasReport: boolean
  /** How many assertions the saved spec carries, for the switcher's subtitle. */
  checkCount: number
}

/** The full record: a row plus everything that costs bytes. */
export interface SavedSession extends SessionRow {
  report: WebReport | null
  /** The key ChecksView autosaves the builder state under. Restoring a report
   *  with the same identity makes that restore happen by itself. */
  checksKey: string
  /** The exact spec text the Checks pane last composed, for the export. */
  spec: { toml: string; fileName: string } | null
  /** Last run's counts, when a run's result was still current. */
  checks: { passed: number; failed: number; invalid: number } | null
  engineVersion: string | null
}

/** What a save actually managed. Storage is a shared, finite, user-clearable
 *  resource; "saved" is a claim that has to be earned rather than assumed. */
export type SaveOutcome =
  | { kind: 'saved' }
  /** Stored, minus the component positions (the report's heaviest field, and
   *  the only one whose loss costs a fallback dot map rather than content). */
  | { kind: 'trimmed' }
  /** The report itself would not fit; the session is a name, a board and a
   *  spec. Resuming it needs the file again. */
  | { kind: 'metadata-only' }
  /** Storage is unavailable (private mode, blocked, full). Nothing was written. */
  | { kind: 'blocked'; reason: string }

// ── Storage ──────────────────────────────────────────────────────────────────

const isBrowser = typeof localStorage !== 'undefined'

/** Storage is a convenience, never a thing worth breaking the app over: every
 *  access is guarded, and a corrupt or unreadable value reads as absent. */
function readJson<T>(key: string): T | null {
  try {
    const raw = isBrowser ? localStorage.getItem(key) : null
    return raw ? (JSON.parse(raw) as T) : null
  } catch {
    return null
  }
}

function write(key: string, value: string | null): boolean {
  if (!isBrowser) return false
  try {
    if (value === null) localStorage.removeItem(key)
    else localStorage.setItem(key, value)
    return true
  } catch {
    return false
  }
}

const recordKey = (id: string) => `${RECORD_PREFIX}${id}`

// ── The snapshot ─────────────────────────────────────────────────────────────

export interface SessionsSnapshot {
  /** Every saved session, most recently updated first. */
  rows: SessionRow[]
  currentId: string | null
}

let snapshot: SessionsSnapshot | null = null
const listeners = new Set<() => void>()

function readSnapshot(): SessionsSnapshot {
  const rows = readJson<SessionRow[]>(INDEX_KEY)
  let currentId: string | null = null
  try { currentId = isBrowser ? localStorage.getItem(CURRENT_KEY) : null } catch { /* absent */ }
  return {
    rows: (Array.isArray(rows) ? rows : [])
      .filter(r => r && typeof r.id === 'string' && r.board && typeof r.board.fileName === 'string')
      .sort((a, b) => b.updatedAt - a.updatedAt),
    currentId,
  }
}

/** The rows and the current id, as one stable object until the next write. */
export function getSessions(): SessionsSnapshot {
  return (snapshot ??= readSnapshot())
}

export function subscribeSessions(listener: () => void): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

function changed() {
  snapshot = null
  for (const l of listeners) l()
}

/** The switcher's rows, most recently updated first. */
export const listSessions = (): SessionRow[] => getSessions().rows

const writeIndex = (rows: SessionRow[]) => write(INDEX_KEY, JSON.stringify(rows))

export function loadSession(id: string): SavedSession | null {
  const rec = readJson<SavedSession>(recordKey(id))
  return rec && rec.id === id ? rec : null
}

export const currentSessionId = (): string | null => getSessions().currentId

export function setCurrentSessionId(id: string | null) {
  write(CURRENT_KEY, id)
  changed()
}

// ── Identity ─────────────────────────────────────────────────────────────────

const SHA256 = /^[0-9a-f]{64}$/i
const basename = (path: string) => path.split(/[\\/]/).pop()?.toLowerCase() ?? ''

/** A stable id for the exact analyzed inputs, so re-analyzing the same bytes
 *  updates that session while a different revision cannot overwrite it merely
 *  because it kept the same filename and counts. Every authenticated input
 *  hash (board, project, schematic, firmware) is included so two runs whose
 *  evidence differs never share checks. Old reports without inventory keep the
 *  legacy fingerprint and remain resumable. */
export function sessionIdFor(report: WebReport): string {
  const hashes = [...new Set((report.inventory ?? [])
    .map(artifact => artifact.sha256?.toLowerCase())
    .filter((hash): hash is string => !!hash && SHA256.test(hash)))].sort()
  return hashes.length > 0
    ? `${report.file_name}:${hashes.join('.')}`
    : `${report.file_name}:${report.num_components}:${report.num_nets}`
}

/** A first name for a session: the board, as a person would say it. */
export const defaultSessionName = (report: WebReport): string => report.board_name || report.file_name || 'untitled board'

/** True while a session still carries the name it was given automatically
 *  (the board's), so surfaces that show both names do not print one string
 *  twice. Once renamed, the name is information and every surface shows it. */
export const hasDefaultName = (row: SessionRow): boolean => row.name === row.board.boardName || row.name === row.board.fileName

/** How many `[[assert]]` blocks a spec text carries, counted from the text so
 *  it cannot disagree with what is stored. */
export const countAsserts = (toml: string | null | undefined): number => (toml?.match(/^\s*\[\[assert\]\]/gm) ?? []).length

// ── Writing ──────────────────────────────────────────────────────────────────

/** Write a session, degrading rather than failing. Returns what was actually
 *  kept so the UI can say it; a silent partial save is the failure mode this
 *  whole store exists to avoid. */
export function saveSession(session: SavedSession): SaveOutcome {
  if (!isBrowser) return { kind: 'blocked', reason: 'this browser has no local storage' }
  const key = recordKey(session.id)
  const attempts: [SavedSession, SaveOutcome][] = [
    [session, { kind: 'saved' }],
    // The component list feeds one thing, the fallback dot map: first to go.
    [session.report ? { ...session, report: { ...session.report, components: [] } } : session, { kind: 'trimmed' }],
    [{ ...session, report: null, hasReport: false }, { kind: 'metadata-only' }],
  ]
  let written: [SavedSession, SaveOutcome] | null = null
  for (const attempt of attempts) {
    if (write(key, JSON.stringify(attempt[0]))) { written = attempt; break }
    // Out of room: drop the oldest other session, then let the next (smaller)
    // attempt try again.
    pruneOldest(session.id)
  }
  if (!written) return { kind: 'blocked', reason: 'local storage refused the write (full, or blocked for this site)' }

  const [rec, outcome] = written
  const row: SessionRow = {
    id: rec.id, name: rec.name, createdAt: rec.createdAt, updatedAt: rec.updatedAt, analyzedAt: rec.analyzedAt,
    board: rec.board, firmwareName: rec.firmwareName, schematicName: rec.schematicName ?? null,
    hasReport: rec.report !== null, checkCount: rec.checkCount,
  }
  const rows = [row, ...listSessions().filter(r => r.id !== row.id)]
  for (const dropped of rows.slice(MAX_SESSIONS)) write(recordKey(dropped.id), null)
  const ok = writeIndex(rows.slice(0, MAX_SESSIONS))
  changed()
  return ok ? outcome : { kind: 'blocked', reason: 'local storage refused the session index' }
}

/** Drop the least recently updated session that is not `keepId`. */
function pruneOldest(keepId: string) {
  const rows = listSessions().filter(r => r.id !== keepId)
  const victim = rows[rows.length - 1]
  if (!victim) return
  write(recordKey(victim.id), null)
  writeIndex(listSessions().filter(r => r.id !== victim.id))
  snapshot = null
}

export function renameSession(id: string, name: string) {
  const trimmed = name.trim()
  if (!trimmed) return
  const rec = loadSession(id)
  if (rec) write(recordKey(id), JSON.stringify({ ...rec, name: trimmed }))
  writeIndex(listSessions().map(r => (r.id === id ? { ...r, name: trimmed } : r)))
  changed()
}

/** Forget a session: its record, its row, and the Checks builder state saved
 *  under its board's key (leaving that would repopulate the builder the next
 *  time the board was analyzed, which is a deleted session coming back). */
export function deleteSession(id: string) {
  const rec = loadSession(id)
  write(recordKey(id), null)
  writeIndex(listSessions().filter(r => r.id !== id))
  if (rec?.checksKey) write(rec.checksKey, null)
  if (currentSessionId() === id) write(CURRENT_KEY, null)
  changed()
}

// ── Resuming ─────────────────────────────────────────────────────────────────

/** Firmware and schematic bytes are deliberately not copied into browser
 *  storage, so a saved report that used them cannot honestly become a fresh
 *  run from the board bytes alone, even when the live server still has them. */
export function canReanalyzeSavedSession(session: Pick<SavedSession, 'firmwareName' | 'schematicName' | 'report'>): boolean {
  const primaryName = basename(session.report?.file_name ?? '')
  const legacyCompanionSchematic = (session.report?.inventory ?? []).some(artifact =>
    (artifact.role === 'schematic' || /schematic/i.test(artifact.kind ?? '')) && basename(artifact.path) !== primaryName)
  return session.firmwareName === null && !session.schematicName && !legacyCompanionSchematic
}

/** The authenticated primary-input digest behind a saved report. A filename
 *  is only a label: the live server may have a different revision open under
 *  the same name. Legacy reports sometimes kept one authenticated input under
 *  a normalized path; one candidate is unambiguous, two are not safe to guess
 *  between. */
export function expectedBoardSha256(report: WebReport | null, fileName: string): string | null {
  const primary = (report?.inventory ?? []).filter(artifact =>
    ['layout', 'netlist', 'schematic'].includes(artifact.role) && SHA256.test(artifact.sha256 ?? ''))
  const exact = primary.find(artifact => basename(artifact.path) === basename(fileName)) ?? (primary.length === 1 ? primary[0] : null)
  return exact?.sha256?.toLowerCase() ?? null
}

export async function boardBytesMatchExpected(bytes: Uint8Array, expectedSha256: string): Promise<boolean> {
  if (!SHA256.test(expectedSha256)) return false
  const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', Uint8Array.from(bytes).buffer)))
    .map(byte => byte.toString(16).padStart(2, '0')).join('')
  return digest === expectedSha256.toLowerCase()
}

/** The board file for a saved session, IF this server still has it and the
 *  bytes match the saved report's authenticated digest. The route retains
 *  every text board format accepted at intake; the SPA fallback and unknown
 *  extensions are rejected. Board-as-Code is retained as its compiled KiCad
 *  layout, and gets that extension back so it is not compiled twice. */
export async function refetchServerBoard(saved: SavedSession): Promise<File | null> {
  const fileName = saved.board.fileName
  const expected = canReanalyzeSavedSession(saved) ? expectedBoardSha256(saved.report, fileName) : null
  if (!fileName || !expected) return null
  try {
    const bytes = await api.boardBytes(fileName)
    if (!bytes || !(await boardBytesMatchExpected(bytes, expected))) return null
    const text = new TextDecoder().decode(bytes)
    const head = text.slice(0, 128)
    const compiledBoard = /^\s*\(kicad_pcb\b/i.test(head)
    const supportedTextBoard = /\.(kicad_pcb|kicad_sch|net|brd|pcbdoc|d356|board)$/i.test(fileName)
    if ((!supportedTextBoard && !compiledBoard) || !text.trim() || /^\s*(?:<!doctype\s+html|<html\b)/i.test(head)) return null
    const resumedName = compiledBoard && !/\.kicad_pcb$/i.test(fileName)
      ? `${fileName.replace(/\.[^.]+$/, '')}.compiled.kicad_pcb`
      : fileName
    return new File([text], resumedName, { type: 'text/plain' })
  } catch {
    return null
  }
}
