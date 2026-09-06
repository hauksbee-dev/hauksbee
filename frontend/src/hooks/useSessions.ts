import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from 'react'
import type { WebReport } from '../types/report'
import {
  countAsserts, defaultSessionName, deleteSession, getSessions, loadSession, refetchServerBoard,
  renameSession, saveSession, sessionIdFor, setCurrentSessionId, subscribeSessions,
} from '../lib/session-store'
import type { SaveOutcome, SavedSession, SessionRow } from '../lib/session-store'

// The saved-session layer above the store: when to write, what the indicator
// says, and what "resume" is actually able to do. The save is automatic and
// debounced. There is no Save button, because the thing being saved is not a
// document the user authored: it is where they were.

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

type ResumeResult =
  /** The server still had the board file, so this is a fresh, real run. */
  | { kind: 'reanalyzed'; boardName: string }
  /** The stored report is back; the file is not. Checks and live need it again. */
  | { kind: 'report-only'; session: SavedSession }
  /** Nothing to restore: the record is gone, or its report never fitted. */
  | { kind: 'unavailable'; reason: string }

export function useSessions(opts: {
  report: WebReport | null
  firmwareName: string | null
  schematicName: string | null
  analyzedAt: number | null
  engineVersion: string | null
  spec: SpecSnapshot | null
  checks: { passed: number; failed: number; invalid: number } | null
  /** Hand a re-fetched board file back to the session so it runs for real. */
  onReanalyze: (file: File) => void
  /** Install a stored report with no file behind it. */
  onRestoreReport: (session: SavedSession) => void
}): SessionsState {
  const { report, firmwareName, schematicName, analyzedAt, engineVersion, spec, checks } = opts
  const { rows, currentId } = useSyncExternalStore(subscribeSessions, getSessions, getSessions)
  const [outcome, setOutcome] = useState<SaveOutcome | null>(null)
  const [resumeDismissed, setResumeDismissed] = useState(false)

  const reportOk = report?.ok === true
  const sessionId = reportOk && report ? sessionIdFor(report) : null

  // The autosave. Debounced because its inputs include the spec text, which
  // changes on every keystroke in the raw TOML pane: writing (and re-writing
  // the index) per character would serialize a whole report each time.
  useEffect(() => {
    if (!reportOk || !report || !sessionId) return
    const timer = setTimeout(() => {
      const existing = loadSession(sessionId)
      const now = Date.now()
      setOutcome(saveSession({
        id: sessionId,
        // A renamed session keeps its name across re-analysis: the name is the
        // user's, and re-running the board must not overwrite it.
        name: existing?.name ?? defaultSessionName(report),
        createdAt: existing?.createdAt ?? now,
        updatedAt: now,
        analyzedAt,
        board: {
          fileName: report.file_name, boardName: report.board_name,
          numComponents: report.num_components, numNets: report.num_nets,
        },
        firmwareName,
        schematicName,
        hasReport: true,
        checkCount: countAsserts(spec?.toml),
        report,
        checksKey: `hauksbee.checks.${sessionId}`,
        spec,
        checks,
        engineVersion,
      }))
      setCurrentSessionId(sessionId)
    }, 450)
    return () => clearTimeout(timer)
  }, [reportOk, report, sessionId, analyzedAt, firmwareName, schematicName, spec, checks, engineVersion])

  const resume = useCallback(async (id: string): Promise<ResumeResult> => {
    const saved = loadSession(id)
    if (!saved) return { kind: 'unavailable', reason: 'that session is no longer in this browser' }
    setResumeDismissed(true)
    const file = await refetchServerBoard(saved)
    if (file) {
      setCurrentSessionId(saved.id)
      opts.onReanalyze(file)
      return { kind: 'reanalyzed', boardName: saved.board.fileName }
    }
    if (!saved.report) {
      const companions = [saved.firmwareName, saved.schematicName].filter(Boolean).join(' and ')
      return {
        kind: 'unavailable',
        reason: companions
          ? `this run used ${companions}, whose bytes were not stored. Re-drop the board and companion files to run it again`
          : 'the report for that session was too large to keep, so there is nothing to show without the board file',
      }
    }
    setCurrentSessionId(saved.id)
    opts.onRestoreReport(saved)
    return { kind: 'report-only', session: saved }
  }, [opts])

  const current = useMemo(() => rows.find(r => r.id === (sessionId ?? currentId)) ?? null, [rows, sessionId, currentId])

  // The landing offer: the most recent session, only while nothing is loaded
  // and only if it has a report to come back to.
  const resumable = reportOk || resumeDismissed ? null : rows.find(r => r.hasReport) ?? null

  return {
    rows,
    current,
    outcome,
    resumable,
    rename: renameSession,
    remove: deleteSession,
    resume,
    dismissResume: () => setResumeDismissed(true),
  }
}
