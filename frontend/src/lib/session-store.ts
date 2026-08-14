// Named sessions in localStorage: what the app remembers between visits.
//
// A session is everything about one board that the browser CAN keep: the
// report the engine returned, which firmware was staged beside it, the spec the
// Checks builder composed, and the last run's counts. What it cannot keep is
// the uploaded FILE. A `File` is a handle to bytes the page was granted for one
// visit; nothing in web storage holds it, and re-obtaining it needs the user to
// point at it again. Every affordance built on this store says that out loud
// rather than restoring a report that looks live and then failing on the first
// action that needs the bytes.
//
// Layout: a light index (one row per session, enough to draw the switcher) and
// one record per session holding the heavy part. The switcher opens without
// parsing a 50-finding report per row, and one oversized report cannot make the
// whole list unreadable.
//
// The composed spec is NOT duplicated here: ChecksView already autosaves it
// under its own per-board key (`checksStorageKey`), and a session record carries
// that key. Two copies of the builder state would drift the moment one was
// written and the other was not; the session keeps a snapshot of the spec TEXT
// only, which is what the export needs and is never read back into the builder.

import type { WebReport } from '../types/report'

const INDEX_KEY = 'hauksbee.sessions.v1'
const CURRENT_KEY = 'hauksbee.sessions.current'
const RECORD_PREFIX = 'hauksbee.session.v1.'

/** How many sessions are kept. Past this the least recently updated is
 *  dropped: an unbounded list is a quota failure waiting for a big board, and
 *  a switcher nobody can read. */
export const MAX_SESSIONS = 12

/** The board a session is about, as the report described it. */
export interface SessionBoard {
  /** `report.file_name`: what was dropped. */
  fileName: string
  /** `report.board_name`: what the engine called it. */
  boardName: string
  numComponents: number
  numNets: number
}

/** One switcher row: enough to name a session and say when it was last touched,
 *  without loading its report. */
export interface SessionRow {
  id: string
  name: string
  createdAt: number
  updatedAt: number
  /** Client clock when the report was produced (null for a restored session
   *  whose report predates this page-load). */
  analyzedAt: number | null
  board: SessionBoard
  firmwareName: string | null
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
  /** The engine version that produced the report. */
  engineVersion: string | null
}

/** What a save actually managed. Storage is a shared, finite, user-clearable
 *  resource; "saved" is a claim that has to be earned rather than assumed. */
export type SaveOutcome =
  /** Everything went in. */
  | { kind: 'saved' }
  /** Stored, minus the component positions (the report's heaviest field, and
   *  the only one whose loss costs a fallback dot map rather than content). */
  | { kind: 'trimmed' }
  /** The report itself would not fit; the session is a name, a board and a
   *  spec. Resuming it needs the file again. */
  | { kind: 'metadata-only' }
  /** Storage is unavailable (private mode, blocked, full of someone else's
   *  data). Nothing was written and nothing will be. */
  | { kind: 'blocked'; reason: string }

const isBrowser = typeof localStorage !== 'undefined'

function readJson<T>(key: string): T | null {
  if (!isBrowser) return null
  try {
    const raw = localStorage.getItem(key)
    return raw ? (JSON.parse(raw) as T) : null
  } catch {
    // Corrupt or unreadable: treat as absent. A saved session is a
    // convenience, never a thing worth breaking the app over.
    return null
  }
}

/** The switcher's rows, most recently updated first. */
export function listSessions(): SessionRow[] {
  const rows = readJson<SessionRow[]>(INDEX_KEY)
  if (!Array.isArray(rows)) return []
  return rows
    .filter(r => r && typeof r.id === 'string' && r.board && typeof r.board.fileName === 'string')
    .sort((a, b) => b.updatedAt - a.updatedAt)
}

function writeIndex(rows: SessionRow[]): boolean {
  if (!isBrowser) return false
  try {
    localStorage.setItem(INDEX_KEY, JSON.stringify(rows))
    return true
  } catch {
    return false
  }
}

const recordKey = (id: string) => `${RECORD_PREFIX}${id}`

export function loadSession(id: string): SavedSession | null {
  const rec = readJson<SavedSession>(recordKey(id))
  if (!rec || rec.id !== id) return null
  return rec
}

export function currentSessionId(): string | null {
  if (!isBrowser) return null
  try {
    return localStorage.getItem(CURRENT_KEY)
  } catch {
    return null
  }
}

export function setCurrentSessionId(id: string | null) {
  if (!isBrowser) return
  try {
    if (id === null) localStorage.removeItem(CURRENT_KEY)
    else localStorage.setItem(CURRENT_KEY, id)
  } catch { /* nothing to do: the indicator just will not persist */ }
}

/** A stable id for the exact analyzed inputs, so re-analyzing the same bytes
 *  updates that session while a different revision cannot overwrite it merely
 *  because it kept the same filename and component/net counts. Include every
 *  authenticated input hash (board, project, schematic, firmware) so two runs
 *  whose evidence differs never share checks. Old reports without inventory
 *  retain the legacy fingerprint and remain resumable. */
export function sessionIdFor(report: WebReport): string {
  const hashes = [...new Set((report.inventory ?? [])
    .map(artifact => artifact.sha256?.toLowerCase())
    .filter((hash): hash is string => !!hash && /^[0-9a-f]{64}$/.test(hash)))]
    .sort()
  if (hashes.length > 0) return `${report.file_name}:${hashes.join('.')}`
  return `${report.file_name}:${report.num_components}:${report.num_nets}`
}

/** A first name for a session: the board, as a person would say it. Renamed
 *  from the switcher afterwards. */
export function defaultSessionName(report: WebReport): string {
  return report.board_name || report.file_name || 'untitled board'
}

function tryWrite(key: string, value: string): boolean {
  if (!isBrowser) return false
  try {
    localStorage.setItem(key, value)
    return true
  } catch {
    return false
  }
}

/** Write a session, degrading rather than failing. Returns what was actually
 *  kept so the UI can say it; a silent partial save is the failure mode this
 *  whole store exists to avoid. */
export function saveSession(session: SavedSession): SaveOutcome {
  if (!isBrowser) return { kind: 'blocked', reason: 'this browser has no local storage' }

  const key = recordKey(session.id)
  const attempts: { rec: SavedSession; outcome: SaveOutcome }[] = [
    { rec: session, outcome: { kind: 'saved' } },
    // The component list is 82 rows on a smartwatch and tens of thousands on a
    // backplane, and it feeds one thing: the fallback dot map. It is the first
    // thing to give up.
    {
      rec: session.report
        ? { ...session, report: { ...session.report, components: [] } }
        : session,
      outcome: { kind: 'trimmed' },
    },
    { rec: { ...session, report: null, hasReport: false }, outcome: { kind: 'metadata-only' } },
  ]

  let written: { rec: SavedSession; outcome: SaveOutcome } | null = null
  for (const attempt of attempts) {
    if (tryWrite(key, JSON.stringify(attempt.rec))) {
      written = attempt
      break
    }
    // Out of room: make some by dropping the oldest other session, then let the
    // next (smaller) attempt try again.
    pruneOldest(session.id)
  }
  if (!written) {
    return { kind: 'blocked', reason: 'local storage refused the write (full, or blocked for this site)' }
  }

  const row: SessionRow = {
    id: written.rec.id,
    name: written.rec.name,
    createdAt: written.rec.createdAt,
    updatedAt: written.rec.updatedAt,
    analyzedAt: written.rec.analyzedAt,
    board: written.rec.board,
    firmwareName: written.rec.firmwareName,
    hasReport: written.rec.report !== null,
    checkCount: written.rec.checkCount,
  }
  const rows = listSessions().filter(r => r.id !== row.id)
  rows.unshift(row)
  // Trim to the cap, removing the records of anything that falls off.
  const kept = rows.slice(0, MAX_SESSIONS)
  for (const dropped of rows.slice(MAX_SESSIONS)) removeRecord(dropped.id)
  if (!writeIndex(kept)) {
    return { kind: 'blocked', reason: 'local storage refused the session index' }
  }
  return written.outcome
}

/** Drop the least recently updated session that is not `keepId`. */
function pruneOldest(keepId: string) {
  const rows = listSessions().filter(r => r.id !== keepId)
  const victim = rows[rows.length - 1]
  if (!victim) return
  removeRecord(victim.id)
  writeIndex(listSessions().filter(r => r.id !== victim.id))
}

function removeRecord(id: string) {
  if (!isBrowser) return
  try {
    localStorage.removeItem(recordKey(id))
  } catch { /* already gone, or storage is unreachable */ }
}

export function renameSession(id: string, name: string): boolean {
  const trimmed = name.trim()
  if (!trimmed) return false
  const rec = loadSession(id)
  if (rec) tryWrite(recordKey(id), JSON.stringify({ ...rec, name: trimmed }))
  return writeIndex(listSessions().map(r => (r.id === id ? { ...r, name: trimmed } : r)))
}

/** Forget a session: its record, its row, and the Checks builder state saved
 *  under its board's key. Leaving the latter behind would silently repopulate
 *  the builder the next time that board was analyzed, which is a deleted
 *  session coming back. */
export function deleteSession(id: string) {
  const rec = loadSession(id)
  removeRecord(id)
  writeIndex(listSessions().filter(r => r.id !== id))
  if (rec?.checksKey && isBrowser) {
    try {
      localStorage.removeItem(rec.checksKey)
    } catch { /* nothing more to do */ }
  }
  if (currentSessionId() === id) setCurrentSessionId(null)
}

/** True while a session still carries the name it was given automatically (the
 *  board's). Every surface that shows both a session name and a board name uses
 *  this to avoid printing the same string twice: three rows reading
 *  `watchy.kicad_pcb` is not an identity, it is noise. Once renamed, the name is
 *  information and every surface shows it. */
export function hasDefaultName(row: SessionRow): boolean {
  return row.name === row.board.boardName || row.name === row.board.fileName
}

/** How many `[[assert]]` blocks a spec text carries. Used for the switcher
 *  subtitle, counted from the text so it cannot disagree with what is stored. */
export function countAsserts(toml: string | null | undefined): number {
  if (!toml) return 0
  return (toml.match(/^\s*\[\[assert\]\]/gm) ?? []).length
}
