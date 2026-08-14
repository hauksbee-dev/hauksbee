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

/** Firmware bytes are deliberately not copied into browser storage. A saved
 *  firmware-backed report therefore cannot honestly become a fresh run from
 *  the board bytes alone, even when the live server still retains that board. */
export function canReanalyzeSavedSession(
  session: Pick<SavedSession, 'firmwareName' | 'schematicName' | 'report'>,
): boolean {
  const basename = (path: string) => path.split(/[\\/]/).pop()?.toLowerCase() ?? ''
  const primaryName = basename(session.report?.file_name ?? '')
  const legacyCompanionSchematic = (session.report?.inventory ?? []).some(artifact =>
    (artifact.role === 'schematic' || /schematic/i.test(artifact.kind ?? ''))
      && basename(artifact.path) !== primaryName,
  )
  return session.firmwareName === null && !session.schematicName && !legacyCompanionSchematic
}

/** The authenticated primary-input digest that identifies the bytes behind a
 * saved report. Layouts, netlists, and primary schematics are all accepted at
 * intake; a filename is only a label because the live server may now have a
 * different revision open under the same name. */
export function expectedBoardSha256(
  report: WebReport | null,
  fileName: string,
): string | null {
  const primaryInputs = (report?.inventory ?? []).filter(artifact =>
    ['layout', 'netlist', 'schematic'].includes(artifact.role)
      && /^[0-9a-f]{64}$/i.test(artifact.sha256 ?? ''),
  )
  if (primaryInputs.length === 0) return null
  const basename = (path: string) => path.split(/[\\/]/).pop()?.toLowerCase() ?? ''
  const exact = primaryInputs.find(artifact => basename(artifact.path) === basename(fileName))
  if (exact) return exact.sha256!.toLowerCase()
  // Legacy reports sometimes kept one authenticated input under a normalized
  // path. One candidate is still unambiguous; two are not safe to guess between.
  return primaryInputs.length === 1 ? primaryInputs[0].sha256!.toLowerCase() : null
}

export async function boardBytesMatchExpected(
  bytes: Uint8Array,
  expectedSha256: string,
): Promise<boolean> {
  if (!/^[0-9a-f]{64}$/i.test(expectedSha256)) return false
  const owned = Uint8Array.from(bytes)
  const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', owned.buffer)))
    .map(byte => byte.toString(16).padStart(2, '0'))
    .join('')
  return digest === expectedSha256.toLowerCase()
}

/** The board file for `name`, IF this server still has it.
 *
 *  `/boards/{name}` serves the CURRENT live session's board file. The filename
 *  is not identity, so the bytes must also match the saved report's authenticated
 *  layout digest before this can become a real re-run rather than a restore. */
async function refetchServerBoard(fileName: string, expectedSha256: string): Promise<File | null> {
  if (!fileName || !/^[0-9a-f]{64}$/i.test(expectedSha256)) return null
  try {
    const res = await fetch(`/boards/${encodeURIComponent(fileName)}`)
    if (!res.ok) return null
    const bytes = new Uint8Array(await res.arrayBuffer())
    if (!(await boardBytesMatchExpected(bytes, expectedSha256))) return null
    const text = new TextDecoder().decode(bytes)
    // The route retains every text board format accepted at intake, not just
    // KiCad PCB. Reject the SPA fallback and unknown extensions, but allow a
    // live Eagle/IPC/Board-as-Code session to re-run from bytes the server
    // still has instead of degrading it to report-only.
    const compiledBoard = /^\s*\(kicad_pcb\b/i.test(text.slice(0, 128))
    const supportedTextBoard = /\.(kicad_pcb|kicad_sch|net|brd|pcbdoc|d356|board)$/i.test(fileName)
    const looksLikeHtml = /^\s*(?:<!doctype\s+html|<html\b)/i.test(text.slice(0, 128))
    if ((!supportedTextBoard && !compiledBoard) || !text.trim() || looksLikeHtml) return null
    // Board-as-Code (including zipped exports) is retained by the live server
    // as its compiled KiCad layout. Give those bytes the extension they now
    // have; feeding compiled text back under `.board` would try to compile the
    // KiCad S-expression as DSL a second time.
    const resumedName = compiledBoard && !/\.kicad_pcb$/i.test(fileName)
      ? `${fileName.replace(/\.[^.]+$/, '')}.compiled.kicad_pcb`
      : fileName
    return new File([text], resumedName, { type: 'text/plain' })
  } catch {
    return null
  }
}

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
        schematicName,
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
  }, [reportOk, report, sessionId, analyzedAt, firmwareName, schematicName, spec, checks, engineVersion])

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
    const expectedSha256 = expectedBoardSha256(saved.report, saved.board.fileName)
    const file = canReanalyzeSavedSession(saved) && expectedSha256
      ? await refetchServerBoard(saved.board.fileName, expectedSha256)
      : null
    if (file) {
      setCurrentSessionId(saved.id)
      setCurrentId(saved.id)
      opts.onReanalyze(file)
      return { kind: 'reanalyzed', boardName: saved.board.fileName }
    }
    if (!saved.report) {
      return {
        kind: 'unavailable',
        reason: saved.firmwareName || saved.schematicName
          ? `this run used ${[saved.firmwareName, saved.schematicName].filter(Boolean).join(' and ')}, whose bytes were not stored. Re-drop the board and companion files to run it again`
          : 'the report for that session was too large to keep, so there is nothing to show without the board file',
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
