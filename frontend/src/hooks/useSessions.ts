import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { WebReport } from '../types/report'
import {
  countAsserts, defaultSessionName, deleteSession, listSessions, loadSession,
  renameSession, saveSession, sessionIdFor, setCurrentSessionId, currentSessionId,
} from '../lib/session-store'
import type { SaveOutcome, SavedSession, SessionRow } from '../lib/session-store'

// The saved-session layer above the store: when to write, what the indicator
// says, and what "resume" is actually able to do.
//
// The save is automatic and debounced. There is no Save button, because the
// thing being saved is not a document the user authored: it is where they were.
// A button for that only creates a way to lose it.

/** The composed spec, as the Checks pane last had it. */
export interface SpecSnapshot {
  toml: string
  fileName: string
}

export interface SessionsState {
  /** Every saved session, most recently touched first. */
  rows: SessionRow[]
  /** The row for the board on screen, when it has been saved. */
  current: SessionRow | null
  /** What the last write managed. `null` before anything was written. */
  outcome: SaveOutcome | null
  /** The session offered on the landing page, when no board is loaded and one
   *  was saved. */
  resumable: SessionRow | null
  rename: (id: string, name: string) => void
  remove: (id: string) => void
  /** Load a saved session. Re-runs the real board when this server still has
   *  the bytes, otherwise restores the stored report and says so. */
  resume: (id: string) => Promise<ResumeResult>
  /** Forget the offer without deleting anything (the "start fresh" path). */
  dismissResume: () => void
}

export type ResumeResult =
  /** The server still had the board file, so this is a fresh, real run: every
   *  action (re-run checks, drive it live) works. */
  | { kind: 'reanalyzed'; boardName: string }
  /** The stored report is back; the file is not. Checks and live need it again. */
  | { kind: 'report-only'; session: SavedSession }
  /** Nothing to restore: the record is gone, or its report never fitted. */
  | { kind: 'unavailable'; reason: string }

/** The board file for `name`, IF this server still has it.
 *
 *  `/boards/{name}` serves the CURRENT live session's own board file and
 *  nothing else, so a hit here means the server genuinely still holds the bytes
 *  this session was about. That is the one case where a resume can be a real
 *  re-run rather than a restore, and it is worth taking: the alternative is
 *  telling someone to go and find a file the machine already has open. */
async function refetchServerBoard(fileName: string): Promise<File | null> {
  if (!fileName) return null
  try {
    const res = await fetch(`/boards/${encodeURIComponent(fileName)}`)
    if (!res.ok) return null
    const text = await res.text()
    // The route is KiCad layout text. Anything else (an index.html from the SPA
    // fallback, most likely) is not a board and must not be fed to the analyzer.
    if (!/^\s*\(kicad_pcb/.test(text.slice(0, 64))) return null
    return new File([text], fileName, { type: 'text/plain' })
  } catch {
    return null
  }
}

export function useSessions(opts: {
  report: WebReport | null
  firmwareName: string | null
  analyzedAt: number | null
  engineVersion: string | null
  spec: SpecSnapshot | null
  checks: { passed: number; failed: number; invalid: number } | null
  /** Hand a re-fetched board file back to the session so it runs for real. */
  onReanalyze: (file: File) => void
  /** Install a stored report with no file behind it. */
  onRestoreReport: (session: SavedSession) => void
}): SessionsState {
  const { report, firmwareName, analyzedAt, engineVersion, spec, checks } = opts

  const [rows, setRows] = useState<SessionRow[]>(() => listSessions())
  const [outcome, setOutcome] = useState<SaveOutcome | null>(null)
  const [resumeDismissed, setResumeDismissed] = useState(false)
  const [currentId, setCurrentId] = useState<string | null>(() => currentSessionId())

  const reportOk = report?.ok === true
  const sessionId = reportOk && report ? sessionIdFor(report) : null

  // The autosave. Debounced because its inputs include the spec text, which
  // changes on every keystroke in the raw TOML pane: writing (and re-writing the
  // index) per character would serialize a whole report each time.
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(() => {
    if (!reportOk || !report || !sessionId) return
    if (timer.current) clearTimeout(timer.current)
    timer.current = setTimeout(() => {
      const existing = loadSession(sessionId)
      const now = Date.now()
      const record: SavedSession = {
        id: sessionId,
        // A renamed session keeps its name across re-analysis: the name is the
        // user's, and re-running the board must not overwrite it.
        name: existing?.name ?? defaultSessionName(report),
        createdAt: existing?.createdAt ?? now,
        updatedAt: now,
        analyzedAt,
        board: {
          fileName: report.file_name,
          boardName: report.board_name,
          numComponents: report.num_components,
          numNets: report.num_nets,
        },
        firmwareName,
        hasReport: true,
        checkCount: countAsserts(spec?.toml),
        report,
        checksKey: `hauksbee.checks.${sessionId}`,
        spec,
        checks,
        engineVersion,
      }
      setOutcome(saveSession(record))
      setCurrentSessionId(sessionId)
      setCurrentId(sessionId)
      setRows(listSessions())
    }, 450)
    return () => {
      if (timer.current) clearTimeout(timer.current)
    }
  }, [reportOk, report, sessionId, analyzedAt, firmwareName, spec, checks, engineVersion])

  const rename = useCallback((id: string, name: string) => {
    renameSession(id, name)
    setRows(listSessions())
  }, [])

  const remove = useCallback((id: string) => {
    deleteSession(id)
    setRows(listSessions())
    if (currentSessionId() === null) setCurrentId(null)
  }, [])

  const resume = useCallback(async (id: string): Promise<ResumeResult> => {
    const saved = loadSession(id)
    if (!saved) return { kind: 'unavailable', reason: 'that session is no longer in this browser' }
    setResumeDismissed(true)
    const file = await refetchServerBoard(saved.board.fileName)
    if (file) {
      setCurrentSessionId(saved.id)
      setCurrentId(saved.id)
      opts.onReanalyze(file)
      return { kind: 'reanalyzed', boardName: saved.board.fileName }
    }
    if (!saved.report) {
      return {
        kind: 'unavailable',
        reason: 'the report for that session was too large to keep, so there is nothing to show '
          + 'without the board file',
      }
    }
    setCurrentSessionId(saved.id)
    setCurrentId(saved.id)
    opts.onRestoreReport(saved)
    return { kind: 'report-only', session: saved }
  }, [opts])

  const current = useMemo(
    () => rows.find(r => r.id === (sessionId ?? currentId)) ?? null,
    [rows, sessionId, currentId],
  )

  // The landing offer: the most recent session, only while nothing is loaded and
  // only if it has a report to come back to.
  const resumable = useMemo(() => {
    if (reportOk || resumeDismissed) return null
    return rows.find(r => r.hasReport) ?? null
  }, [reportOk, resumeDismissed, rows])

  return {
    rows,
    current,
    outcome,
    resumable,
    rename,
    remove,
    resume,
    dismissResume: () => setResumeDismissed(true),
  }
}
