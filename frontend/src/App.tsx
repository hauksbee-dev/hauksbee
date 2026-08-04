import { useCallback, useEffect, useRef, useState } from 'react'
import { Sidebar } from './components/Sidebar'
import { SessionRail } from './components/SessionSwitcher'
import type { AppView } from './components/Sidebar'
import { UploadView } from './components/UploadView'
import { BoardView } from './components/BoardView'
import { ChecksView, checksStorageKey } from './components/ChecksView'
import type { ChecksSummary } from './components/ChecksView'
import { DepsPanel } from './components/DepsPanel'
import SimView from './SimView'
import type { SimShellStatus } from './SimView'
import { useTheme } from './hooks/useTheme'
import { useBoardSession } from './hooks/useBoardSession'
import { useSessions } from './hooks/useSessions'
import type { SpecSnapshot } from './hooks/useSessions'
import type { SavedSession } from './lib/session-store'
import { BOARD_ACCEPT_ATTR } from './lib/board-formats'
import { BoardTargetIcon, PlayIcon } from './components/Icons'
import type { QueuedCheck, Startup, WebReport } from './types/report'

// One web experience (W6 §1) behind an app shell. The app asks the server how
// it was launched (`/api/startup`) and lands accordingly:
//   - `hauksbee serve`            -> { preloaded: false, live: true }: the
//     drop-a-board Board view; an uploaded board's report can launch a live
//     sim server-side (`/api/live/launch`).
//   - `hauksbee run <b> --serve`  -> { preloaded: true, report, live: true }:
//     the Board view opened on that board's report, with the live session
//     already running on /ws.
// The shell's left rail navigates between the four real surfaces: Board
// (upload + report), Checks (the spec builder), Live Sim, and Environment
// (co-sim backends and oracles on this machine). Views stay MOUNTED once
// opened (hidden, not unmounted) so the viewer camera, the checks results and
// the sim's fault log all survive navigation.

type Boot =
  | { kind: 'loading' }
  | {
      kind: 'ready'
      report: WebReport | null
      boardName: string | null
      canLaunchLive: boolean
      engineVersion: string | null
    }

export default function App() {
  const [boot, setBoot] = useState<Boot>({ kind: 'loading' })

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const res = await fetch('/api/startup')
        if (!res.ok) throw new Error(`startup ${res.status}`)
        const startup = await res.json() as Startup
        if (!alive) return
        if (startup.preloaded) {
          setBoot({
            kind: 'ready',
            report: startup.report,
            boardName: startup.board_name,
            canLaunchLive: startup.live === true,
            engineVersion: startup.version ?? null,
          })
        } else {
          setBoot({
            kind: 'ready',
            report: null,
            boardName: null,
            canLaunchLive: startup.live === true,
            engineVersion: startup.version ?? null,
          })
        }
      } catch {
        // No startup endpoint (a stale/odd deployment). Degrade to the
        // drop-a-board Board view, never the live-sim view, which would sit
        // "offline" with no way to load a board.
        if (alive) {
          setBoot({ kind: 'ready', report: null, boardName: null, canLaunchLive: false, engineVersion: null })
        }
      }
    })()
    return () => { alive = false }
  }, [])

  if (boot.kind === 'loading') {
    return (
      <div
        className="flex items-center justify-center h-screen text-sm"
        style={{ background: 'var(--canvas)', color: 'var(--silk-faint)' }}
      >
        hauksbee ...
      </div>
    )
  }

  return (
    <Shell
      preloadedReport={boot.report}
      preloadedBoardName={boot.boardName}
      canLaunchLive={boot.canLaunchLive}
      engineVersion={boot.engineVersion}
    />
  )
}

// Name the emulator a launch failure is missing, or null when the failure is
// something the Environment page cannot fix. Matched on the backends' own
// "not found" wording (hauksbee-mcu's `find_qemu` / `find_renode`), which is
// the text the launch error carries verbatim.
function missingEmulator(error: string): 'Espressif QEMU' | 'Renode' | null {
  if (!/not found/i.test(error)) return null
  if (/espressif qemu|qemu-system-/i.test(error)) return 'Espressif QEMU'
  if (/renode/i.test(error)) return 'Renode'
  return null
}

const VIEW_TITLES: Record<AppView, string> = {
  board: 'Board',
  checks: 'Checks',
  sim: 'Live Sim',
  env: 'Environment',
}

function Shell({ preloadedReport, preloadedBoardName, canLaunchLive, engineVersion }: {
  preloadedReport: WebReport | null
  preloadedBoardName: string | null
  canLaunchLive: boolean
  engineVersion: string | null
}) {
  const { theme, toggleTheme } = useTheme()
  const session = useBoardSession({
    preloadedReport,
    preloadedBoardName,
    sessionPreloaded: preloadedReport !== null,
    canLaunchLive,
  })

  const [view, setView] = useState<AppView>('board')
  // The sim view stays mounted once launched: unmounting would close the
  // WebSocket and drop the fault log / scope buffers.
  const [simMounted, setSimMounted] = useState(false)
  // Environment mounts on first visit (its /api/deps fetch is on mount).
  const [envVisited, setEnvVisited] = useState(false)
  // `sessionBoard` is the board the /ws session says IT is running (from its
  // BoardInfo frame): the sim surface's identity is bound to this, never to
  // the locally analyzed board.
  const [simStatus, setSimStatus] = useState<SimShellStatus>({
    running: false, faults: 0, sessionBoard: null, connected: false,
  })
  const [checksSummary, setChecksSummary] = useState<ChecksSummary | null>(null)
  // The spec as the Checks pane currently has it, lifted here because two things
  // outside that pane need the same bytes: the Export menu and the saved session.
  // The pane sets it and clears it on unmount; nothing here clears it, so a
  // remount and a run-state reset in the same commit cannot race.
  const [spec, setSpec] = useState<SpecSnapshot | null>(null)
  // A resume that could not happen, said where the user asked for it.
  const [resumeError, setResumeError] = useState<string | null>(null)

  // Checks queued from a board surface (net/component clicks on the report
  // map or the live sim) for the checks builder. The builder consumes by
  // `seq` so nothing applies twice.
  const [queuedChecks, setQueuedChecks] = useState<QueuedCheck[]>([])
  const seqRef = useRef(0)
  const queueCheck = useCallback((check: { kind: string; net?: string; ref?: string }) => {
    seqRef.current += 1
    setQueuedChecks(prev => [...prev, { ...check, seq: seqRef.current }])
  }, [])
  const consumeChecks = useCallback((upToSeq: number) => {
    setQueuedChecks(prev => prev.filter(c => c.seq > upToSeq))
  }, [])

  const { report } = session
  const reportOk = report?.ok === true

  const driveLive = useCallback(() => {
    session.launchLive(() => {
      setSimMounted(true)
      setView('sim')
    })
  }, [session])

  // Attach to the session ALREADY running server-side (a stale tab, a reload,
  // a session for another board) without launching anything: the sim view
  // binds its identity to what /ws streams and says which board that is.
  const openLiveSession = useCallback(() => {
    session.refreshLiveStatus()
    setSimMounted(true)
    setView('sim')
  }, [session])

  // Relaunch the session with the CURRENT board, replacing whatever runs now.
  // Used where the affordance's label already names the replacement.
  const relaunchWithCurrent = useCallback(() => {
    session.forceLaunch(() => {
      setSimMounted(true)
      setView('sim')
    })
  }, [session])

  const serverLiveActive = session.serverLive?.active === true
  const simEnabled = simMounted || (reportOk && session.liveMode !== 'none') || serverLiveActive

  const navigate = useCallback((v: AppView) => {
    if (v === 'env') setEnvVisited(true)
    if (v === 'sim' && !simMounted) {
      if (reportOk && session.liveMode !== 'none') {
        // The nav entry launches (or asks to replace), exactly like the
        // primary action, so "Live Sim" never opens an offline shell.
        driveLive()
        return
      }
      if (serverLiveActive) {
        // No launchable board here, but the server IS running a session:
        // open it (the sim view names which board it belongs to) instead of
        // being a dead click.
        openLiveSession()
        return
      }
      return
    }
    setView(v)
  }, [driveLive, openLiveSession, reportOk, serverLiveActive, session.liveMode, simMounted])

  const analyzeAnother = useCallback(() => {
    session.resetFlow()
    setChecksSummary(null)
    setView('board')
  }, [session])

  // Saved sessions. The board session above owns the run; this owns the memory
  // of it, and the two only meet at the two callbacks below: one hands a
  // re-fetched board file back for a REAL run, the other installs a stored
  // report with nothing behind it (and says so, everywhere it shows).
  const sessions = useSessions({
    report,
    firmwareName: session.firmwareFile?.name ?? session.restoredFrom?.firmwareName ?? null,
    analyzedAt: session.analyzedAt,
    engineVersion,
    spec,
    checks: checksSummary,
    onReanalyze: session.handleBoard,
    onRestoreReport: (saved: SavedSession) => {
      if (!saved.report) return
      session.restoreReport({
        report: saved.report,
        analyzedAt: saved.analyzedAt,
        boardName: saved.board.fileName,
        firmwareName: saved.firmwareName,
        sessionName: saved.name,
      })
    },
  })

  const resumeSession = useCallback((id: string) => {
    setResumeError(null)
    void sessions.resume(id).then(result => {
      if (result.kind === 'unavailable') {
        setResumeError(result.reason)
        return
      }
      setView('board')
    })
  }, [sessions])

  // A fresh report belongs to a (possibly) different board: last run's checks
  // summary would lie next to it.
  const reportIdentity = report ? checksStorageKey(report) : null
  const prevIdentity = useRef(reportIdentity)
  useEffect(() => {
    if (prevIdentity.current !== reportIdentity) {
      prevIdentity.current = reportIdentity
      setChecksSummary(null)
    }
  }, [reportIdentity])

  // The shell's share of the one reset path (see `clearRunState` in
  // useBoardSession): a check queued from a click on the OLD board is about a
  // net the new board may not even have, and the summary chips describe a run
  // that no longer exists. Both go the moment a new run starts, not when its
  // report happens to land.
  const runEpoch = session.runEpoch
  useEffect(() => {
    setQueuedChecks([])
    setChecksSummary(null)
  }, [runEpoch])

  const chip = (
    label: string,
    tone: 'ok' | 'err' | 'warn' | 'quiet',
    onClick?: () => void,
    testid?: string,
    pulse = false,
  ) => {
    const tones: Record<string, React.CSSProperties> = {
      ok: { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' },
      err: { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)' },
      warn: { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--warn)' },
      quiet: { background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' },
    }
    return (
      <button
        key={label}
        type="button"
        data-testid={testid}
        onClick={onClick}
        disabled={!onClick}
        // A chip is a pill with one line in it. Left shrinkable, "checks 2 passed"
        // wrapped to two lines inside a 24px pill and spilled out the bottom of it,
        // so the chip keeps its own width and the row clips whole chips instead.
        className="hb-press inline-flex items-center gap-1.5 px-2.5 rounded-full text-[11px] font-semibold tnum whitespace-nowrap shrink-0"
        style={{ ...tones[tone], height: 24, cursor: onClick ? 'pointer' : 'default' }}
      >
        {pulse && (
          <span className="run-dot" style={{ width: 6, height: 6, borderRadius: 3, background: 'currentColor', display: 'inline-block' }} />
        )}
        {label}
      </button>
    )
  }

  // The live session's identity, as the session itself reports it (BoardInfo
  // over /ws when the sim view is connected, /api/live/status otherwise).
  // Empty-string guard: an unnamed engine identity must fall through to the
  // launch file name from /api/live/status, never read as "a" session name.
  const sessionBoard = (simStatus.sessionBoard?.trim() || null)
    ?? (session.serverLive?.boardName?.trim() || null)
  // "Matches" means THIS page launched (or preloaded) the session for the
  // board currently analyzed; a same-named session from a previous page-load
  // is deliberately foreign (its content may differ).
  const sessionMatchesCurrent = session.liveMode === 'connected'
  // Fault chips count faulted PARTS (the card in the sim rail counts its own
  // fault conditions and labels itself); every surface must say what it counts.
  const faultChipLabel = `${simStatus.faults} part${simStatus.faults === 1 ? '' : 's'} faulted`
  // A session that was live and lost its socket is not "paused": the chip said
  // so anyway, which read as a sim sitting there waiting for a play button.
  // `simMounted` is what proves there WAS a session to lose (the view only
  // mounts on a real launch), so a not-yet-connected shell never reads as one.
  const simOffline = simMounted && !simStatus.connected

  // The primary action's label, in one place: the header shows it as words where
  // there is room and as the play glyph alone on a phone, and both spellings have
  // to say the same thing (the icon-only form carries it as its accessible name
  // and its tooltip). A label the header cut in half ("Drive it liv") was the
  // narrow-width defect this replaces.
  const liveLabel = session.launch.phase === 'launching'
    ? 'Launching ...'
    : simMounted ? 'Open live sim' : 'Drive it live'

  const chips: React.ReactNode[] = []
  if (view === 'sim') {
    // On the Live Sim view the header describes THE SESSION's board only: the
    // analyzed board's findings/checks chips belong to the Board/Checks
    // surfaces, and "another session is live: X" while viewing exactly that
    // session mislabeled the very surface the user was on.
    if (simOffline) {
      chips.push(chip('sim offline', 'err', undefined, 'chip-sim'))
    } else if (sessionMatchesCurrent) {
      if (simStatus.faults > 0) {
        chips.push(chip(faultChipLabel, 'err', () => setView('sim'), 'chip-faults'))
      }
      chips.push(chip(
        simStatus.running ? 'sim running' : 'sim paused',
        simStatus.running ? 'ok' : 'quiet',
        undefined,
        'chip-sim',
        simStatus.running,
      ))
    } else if (sessionBoard) {
      chips.push(chip(`live: ${sessionBoard}`, 'quiet', undefined, 'chip-sim', simStatus.running))
    }
  } else {
    if (reportOk && report) {
      if (report.serious > 0) {
        chips.push(chip(`${report.serious} serious`, 'err', () => setView('board'), 'chip-findings'))
      } else if (report.total > 0) {
        chips.push(chip(`${report.total} findings`, 'warn', () => setView('board'), 'chip-findings'))
      } else {
        chips.push(chip('analysis clean', 'ok', () => setView('board'), 'chip-findings'))
      }
    }
    if (checksSummary) {
      const { passed, failed, invalid } = checksSummary
      if (failed > 0) chips.push(chip(`checks ${failed} failed`, 'err', () => setView('checks'), 'chip-checks'))
      else if (invalid > 0) chips.push(chip(`checks ${invalid} invalid`, 'warn', () => setView('checks'), 'chip-checks'))
      else chips.push(chip(`checks ${passed} passed`, 'ok', () => setView('checks'), 'chip-checks'))
    }
    if (simOffline) {
      chips.push(chip('sim offline', 'err', () => setView('sim'), 'chip-sim'))
    } else if (simMounted && sessionMatchesCurrent) {
      if (simStatus.faults > 0) {
        chips.push(chip(faultChipLabel, 'err', () => setView('sim'), 'chip-faults'))
      }
      chips.push(chip(
        simStatus.running ? 'sim running' : 'sim paused',
        simStatus.running ? 'ok' : 'quiet',
        () => setView('sim'),
        'chip-sim',
        simStatus.running,
      ))
    } else if ((simMounted || serverLiveActive) && sessionBoard) {
      // A session is live for some OTHER board (or a pre-reload launch of
      // this one): never show "sim running" as if it were this board's. The
      // chip names the session's board and opens it.
      chips.push(chip(
        `another session is live: ${sessionBoard}`,
        'warn',
        simMounted ? () => setView('sim') : openLiveSession,
        'chip-sim',
      ))
    } else if (reportOk && sessionMatchesCurrent) {
      chips.push(chip('live session ready', 'ok', driveLive, 'chip-sim'))
    }
  }

  return (
    <div className="flex h-screen overflow-hidden" style={{ background: 'var(--canvas)', color: 'var(--silk)' }}>
      <Sidebar
        nav={{
          view,
          setView: navigate,
          checksEnabled: reportOk,
          simEnabled,
          simRunning: simStatus.running,
          faultCount: simStatus.faults,
        }}
        report={report}
        boardLabel={session.boardLabel}
        analyzedAt={session.analyzedAt}
        theme={theme}
        onToggleTheme={toggleTheme}
        sessions={<SessionRail state={sessions} onResume={resumeSession} />}
      />

      <div className="flex-1 flex flex-col min-w-0">
        {/* Title row: where you are, what board this is, live status, and the
            primary action. */}
        <header
          className="flex items-center gap-3 px-5 shrink-0"
          style={{ height: 56, borderBottom: '1px solid var(--hairline)', background: 'var(--surface)' }}
        >
          <div className="min-w-0 flex items-baseline gap-2.5">
            {/* The view's name answers "where am I" and is four words at most, so
                it keeps its width and the board name (which has a tooltip and a
                natural ellipsis point) gives ground first. Ellipsed the other way
                round, a 320px header said "B..." for Board. */}
            <h1 className="text-[15px] font-semibold shrink-0" style={{ margin: 0, color: 'var(--silk)' }}>
              {VIEW_TITLES[view]}
            </h1>
            {/* On the sim view the header names the SESSION's board (what /ws
                actually streams), never the locally analyzed one: the two can
                differ, and the canvas/nets/footer follow the session. */}
            {/* Environment is about this machine's backends and oracles, not about
                a board, so it does not carry one. That also stops the longest view
                title from squeezing the name down to two characters on a phone. */}
            {view !== 'env' && (view === 'sim' ? (sessionBoard ?? session.boardLabel) : session.boardLabel) && (
              <span
                className="text-[12px] truncate"
                // On a phone header this is the only place the board's name
                // appears (the rail's identity card is icon-collapsed), and it
                // gets ellipsed there: the full name stays reachable.
                title={(view === 'sim' ? (sessionBoard ?? session.boardLabel) : session.boardLabel) ?? undefined}
                style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
              >
                {view === 'sim' ? (sessionBoard ?? session.boardLabel) : session.boardLabel}
              </span>
            )}
          </div>

          <div className="flex-1" />

          {/* Status chips need about 190px of their own. They appear from `md`,
              where the header has it to spare once the secondary action is a glyph. */}
          <div className="hidden md:flex items-center gap-1.5 overflow-hidden">{chips}</div>

          {/* Both header actions keep their glyph at every width and give up their
              words as the header narrows: the secondary one first (from `lg`), the
              primary CTA last (from `sm`). Icon-only, never a label the header cut
              in half: the words ride along as the accessible name and the tooltip. */}
          {report && !session.busy && (
            <button
              type="button"
              data-testid="header-another-board"
              onClick={analyzeAnother}
              title="Analyze another board"
              aria-label="Analyze another board"
              className="hb-btn hb-press inline-flex items-center justify-center gap-2 px-2.5 lg:px-3 text-[12px] whitespace-nowrap shrink-0"
              style={{ height: 32 }}
            >
              <BoardTargetIcon size={14} />
              <span className="hidden lg:inline">Analyze another board</span>
            </button>
          )}

          {reportOk && session.liveMode !== 'none' && view !== 'sim' && (
            <button
              type="button"
              data-testid="run-it"
              onClick={driveLive}
              disabled={session.launch.phase === 'launching'}
              title={liveLabel}
              aria-label={liveLabel}
              className="hb-btn-primary hb-press inline-flex items-center justify-center gap-2 px-2.5 sm:px-3.5 text-[12px] whitespace-nowrap shrink-0"
              style={{ height: 32 }}
            >
              {session.launch.phase === 'launching'
                ? <span className="slot-spin" style={{ borderTopColor: 'var(--on-copper)' }} />
                : <PlayIcon size={12} />}
              <span className="hidden sm:inline">{liveLabel}</span>
            </button>
          )}
        </header>

        {/* Replace-the-running-session confirmation, in-app (a native
            window.confirm is auto-dismissed by automation and unstylable).
            Covers ALL foreign-session cases: another board, a stale tab, a
            launch from before a reload. */}
        {session.launch.phase === 'confirm' && (
          <div
            data-testid="live-replace-confirm"
            className="mx-5 mt-3 rounded-lg px-4 py-3 text-[13px]"
            style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase block mb-1" style={{ color: 'var(--warn-strong)' }}>
              A live session is already running
            </span>
            The server is running a live session for{' '}
            <span style={{ fontFamily: 'var(--font-mono)' }}>{session.launch.activeBoard}</span>
            {session.launch.activeBoard === session.launch.targetBoard
              ? ' (launched before this page, so it may not match what you just analyzed)'
              : ''}
            . One session runs at a time.
            <div className="mt-2.5 flex flex-wrap gap-2">
              <button
                type="button"
                data-testid="confirm-replace-live"
                onClick={() => {
                  session.confirmReplace()
                  setSimMounted(true)
                  setView('sim')
                }}
                className="hb-btn-primary hb-press px-3 text-[12px]"
                style={{ height: 30 }}
              >
                Replace it with {session.launch.targetBoard}
              </button>
              <button
                type="button"
                data-testid="open-running-live"
                onClick={() => {
                  session.cancelLaunch()
                  openLiveSession()
                }}
                className="hb-btn hb-press px-3 text-[12px]"
                style={{ height: 30 }}
              >
                Open the running session
              </button>
              <button
                type="button"
                data-testid="cancel-replace-live"
                onClick={session.cancelLaunch}
                className="hb-btn hb-press px-3 text-[12px]"
                style={{ height: 30 }}
              >
                Cancel
              </button>
            </div>
          </div>
        )}

        {/* Launch failures surface here, verbatim, wherever you are. */}
        {session.launch.phase === 'error' && session.launch.error && (
          <div
            data-testid="live-launch-error"
            className="mx-5 mt-3 rounded-lg px-4 py-2.5 text-[13px]"
            style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase block mb-0.5" style={{ color: 'var(--err)' }}>
              Live launch failed
            </span>
            {session.launch.error}
            {/* A missing emulator is the one launch failure this app can fix
                by itself: the Environment page installs Renode and Espressif
                QEMU with one click. Sending the user to a release page they
                have to find, unpack and PATH themselves (which the backend
                error text alone used to do) is the terminal-forcing moment
                the cold-install audit caught. */}
            {missingEmulator(session.launch.error) && (
              <div className="mt-2.5">
                <button
                  type="button"
                  data-testid="launch-error-open-env"
                  onClick={() => navigate('env')}
                  className="hb-btn-primary hb-press px-3 text-[12px]"
                  style={{ height: 30 }}
                >
                  Install {missingEmulator(session.launch.error)} on the Environment page
                </button>
              </div>
            )}
          </div>
        )}

        {/* A session that could not be reopened. Same place, and the same
            plainness, as a launch failure: it says what is missing rather than
            leaving a click that did nothing. */}
        {resumeError && (
          <div
            data-testid="resume-error"
            className="mx-5 mt-3 rounded-lg px-4 py-2.5 text-[13px]"
            style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase block mb-0.5" style={{ color: 'var(--warn-strong)' }}>
              Could not reopen that session
            </span>
            {resumeError}. Drop the board again and everything you composed against it is
            still here.
            <div className="mt-2">
              <button
                type="button"
                data-testid="resume-error-dismiss"
                onClick={() => setResumeError(null)}
                className="hb-btn hb-press px-3 text-[12px]"
                style={{ height: 28 }}
              >
                Got it
              </button>
            </div>
          </div>
        )}

        {/* Views. Mounted once, hidden on navigation. */}
        <main className="flex-1 min-h-0 relative">
          <div style={{ display: view === 'board' ? 'block' : 'none', height: '100%' }}>
            {report ? (
              <BoardView
                session={session}
                onQueueCheck={queueCheck}
                onDriveLive={driveLive}
                simMounted={simMounted}
                engineVersion={engineVersion}
                spec={spec}
                checks={checksSummary}
                sessionName={sessions.current?.name ?? null}
              />
            ) : (
              <UploadView
                session={session}
                onOpenLive={openLiveSession}
                sessions={sessions}
                onResume={resumeSession}
              />
            )}
          </div>

          {reportOk && report && (
            <div style={{ display: view === 'checks' ? 'block' : 'none', height: '100%' }}>
              <ChecksView
                // Remount per board (the key doubles as the localStorage key):
                // the panel's mount-time restore is then authoritative, and one
                // board's builder state can never leak into another board's.
                key={checksStorageKey(report)}
                report={report}
                boardFile={session.boardFile}
                firmwareFile={session.firmwareFile}
                selectedNet={session.selectedNet}
                selectedComponent={session.selectedComponent}
                pendingChecks={queuedChecks}
                onPendingConsumed={consumeChecks}
                onSummary={setChecksSummary}
                onSpec={setSpec}
              />
            </div>
          )}

          {simMounted && (
            <div style={{ display: view === 'sim' ? 'block' : 'none', height: '100%' }}>
              <SimView
                onQueueCheck={queueCheck}
                onStatus={setSimStatus}
                expectedBoard={session.boardLabel}
                sessionMatchesCurrent={sessionMatchesCurrent}
                onRelaunch={reportOk && session.liveMode !== 'none' ? relaunchWithCurrent : undefined}
              />
            </div>
          )}

          {envVisited && (
            <div
              className="overflow-y-auto"
              style={{ display: view === 'env' ? 'block' : 'none', height: '100%' }}
            >
              <div className="max-w-3xl mx-auto px-6 pb-16 view-enter">
                <DepsPanel engineVersion={engineVersion} />
              </div>
            </div>
          )}
        </main>
      </div>

      {/* Hidden file inputs, shared by the drop card and every firmware jack,
          so the report view can keep offering both slots. */}
      <input
        id="board-file"
        type="file"
        // From lib/board-formats, the one accepted-formats list, so the
        // picker cannot offer a different set from the one the copy names.
        accept={BOARD_ACCEPT_ATTR}
        className="hidden"
        onChange={e => { const f = e.target.files?.[0]; if (f) session.handleBoard(f); e.target.value = '' }}
      />
      <input
        id="firmware-file"
        type="file"
        accept=".elf,.hex,.zip"
        className="hidden"
        onChange={e => { const f = e.target.files?.[0]; if (f) session.handleFirmware(f); e.target.value = '' }}
      />
    </div>
  )
}
