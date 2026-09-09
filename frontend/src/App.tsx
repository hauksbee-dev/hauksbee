import { useCallback, useEffect, useRef, useState } from 'react'
import { Sidebar } from './components/Sidebar'
import { SessionRail } from './components/SessionSwitcher'
import type { AppView } from './components/Sidebar'
import { UploadView } from './components/UploadView'
import { BoardView } from './components/BoardView'
import { ChecksView, checksStorageKey } from './components/ChecksView'
import type { ChecksSummary } from './components/ChecksView'
import { DepsPanel } from './components/DepsPanel'
import { ShellChips } from './components/ShellChips'
import { LaunchErrorBanner, ReplaceLiveConfirm, ResumeErrorBanner } from './components/ShellBanners'
import SimView from './SimView'
import type { SimShellStatus } from './SimView'
import { useTheme } from './hooks/useTheme'
import { useBoardSession } from './hooks/useBoardSession'
import { useSessions } from './hooks/useSessions'
import type { SpecSnapshot } from './hooks/useSessions'
import type { SavedSession } from './lib/session-store'
import { BOARD_ACCEPT_ATTR } from './lib/board-formats'
import { BoardTargetIcon, PlayIcon } from './components/Icons'
import { api } from './lib/api'
import type { BoardRequest, QueuedLiveRegisterMap, QueuedRequest, WebReport } from './types/report'
import type { ActionResultMsg } from './types/protocol'

// One web experience behind an app shell. The app asks the server how it was
// launched (`/api/startup`) and lands accordingly:
//   - `hauksbee serve`           -> the drop-a-board Board view; an uploaded
//     board's report can launch a live sim server-side (`/api/live/launch`).
//   - `hauksbee run <b> --serve` -> the Board view opened on that board's
//     report, with the live session already running on /ws.
// The shell's left rail navigates between the four real surfaces: Board
// (upload + report), Checks (the spec builder), Live Sim, and Environment
// (co-sim backends and oracles on this machine). Views stay MOUNTED once
// opened (hidden, not unmounted) so the viewer camera, the checks results and
// the sim's fault log all survive navigation.

interface Boot {
  report: WebReport | null
  boardName: string | null
  canLaunchLive: boolean
  avrAvailable: boolean
  engineVersion: string | null
}

/** What the shell falls back to with no startup endpoint (a stale or odd
 *  deployment): the drop-a-board Board view, never the live-sim view, which
 *  would sit "offline" with no way to load a board. */
const DEGRADED: Boot = {
  report: null, boardName: null, canLaunchLive: false, avrAvailable: false, engineVersion: null,
}

export default function App() {
  const [boot, setBoot] = useState<Boot | null>(null)

  useEffect(() => {
    let alive = true
    void api.startup()
      .then(startup => ({
        report: startup.preloaded ? startup.report : null,
        boardName: startup.preloaded ? startup.board_name : null,
        canLaunchLive: startup.live === true,
        avrAvailable: startup.avr !== false,
        engineVersion: startup.version ?? null,
      }))
      .catch(() => DEGRADED)
      .then(next => { if (alive) setBoot(next) })
    return () => { alive = false }
  }, [])

  if (!boot) {
    return (
      <div
        className="flex items-center justify-center h-screen text-sm"
        style={{ background: 'var(--canvas)', color: 'var(--silk-faint)' }}
      >
        hauksbee ...
      </div>
    )
  }

  return <Shell boot={boot} />
}

const VIEW_TITLES: Record<AppView, string> = {
  board: 'Board',
  checks: 'Checks',
  sim: 'Live Sim',
  env: 'Environment',
}

/** A queue of things a board surface asked the checks builder for, consumed by
 *  `seq` so nothing applies twice. */
function useQueue<T extends { seq: number }>(nextSeq: () => number) {
  const [items, setItems] = useState<T[]>([])
  const push = useCallback((item: Omit<T, 'seq'>) => {
    const seq = nextSeq()
    setItems(previous => [...previous, { ...item, seq } as T])
    return seq
  }, [nextSeq])
  const consume = useCallback((upToSeq: number) => {
    setItems(previous => previous.filter(item => item.seq > upToSeq))
  }, [])
  const clear = useCallback(() => setItems([]), [])
  return { items, push, consume, clear }
}

function Shell({ boot }: { boot: Boot }) {
  const { report: preloadedReport, boardName: preloadedBoardName, canLaunchLive, avrAvailable, engineVersion } = boot
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
  const [liveActionResult, setLiveActionResult] = useState<ActionResultMsg | null>(null)

  const seqRef = useRef(0)
  const nextSeq = useCallback(() => ++seqRef.current, [])
  const requests = useQueue<QueuedRequest>(nextSeq)
  const queuedLiveRegisterMaps = useQueue<QueuedLiveRegisterMap>(nextSeq)

  const pushRequest = requests.push
  const queueRequest = useCallback((request: BoardRequest) => {
    pushRequest(request)
    // A register map cannot be guessed from the clicked part. Unlike a simple
    // live switch or supply, this action always has a required human-authored
    // next step, so take the user directly to the exact-byte builder.
    if (request.type === 'sensor') setView('checks')
  }, [pushRequest])

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
      // The nav entry launches (or asks to replace), exactly like the primary
      // action, so "Live Sim" never opens an offline shell. With no launchable
      // board but a session running server-side, open that instead of being a
      // dead click.
      if (reportOk && session.liveMode !== 'none') driveLive()
      else if (serverLiveActive) openLiveSession()
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
    schematicName: session.schematicFile?.name ?? session.restoredFrom?.schematicName ?? null,
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
        schematicName: saved.schematicName ?? null,
        sessionName: saved.name,
      })
    },
  })

  const resumeSession = useCallback((id: string) => {
    setResumeError(null)
    void sessions.resume(id).then(result => {
      if (result.kind === 'unavailable') setResumeError(result.reason)
      else setView('board')
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
  const clearRequests = requests.clear
  const clearRegisterMaps = queuedLiveRegisterMaps.clear
  useEffect(() => {
    clearRequests()
    clearRegisterMaps()
    setLiveActionResult(null)
    setChecksSummary(null)
  }, [runEpoch, clearRequests, clearRegisterMaps])

  // The live session's identity, as the session itself reports it (BoardInfo
  // over /ws when the sim view is connected, /api/live/status otherwise).
  // Empty-string guard: an unnamed engine identity must fall through to the
  // launch file name from /api/live/status, never read as "a" session name.
  const sessionBoard = (simStatus.sessionBoard?.trim() || null)
    ?? (session.serverLive?.boardName?.trim() || null)
  const sessionMatchesCurrent = session.liveMode === 'connected'
  const simOffline = simMounted && !simStatus.connected

  // The primary action's label, in one place: the header shows it as words
  // where there is room and as the play glyph alone on a phone, and both
  // spellings have to say the same thing (the icon-only form carries it as its
  // accessible name).
  const liveLabel = session.launch.phase === 'launching'
    ? 'Launching ...'
    : simMounted ? 'Open live sim' : 'Drive it live'

  // On the sim view the header names the SESSION's board (what /ws actually
  // streams), never the locally analyzed one: the two can differ, and the
  // canvas/nets/footer follow the session. Environment is about this machine's
  // backends and oracles, not about a board, so it carries none.
  const headerBoard = view === 'env'
    ? null
    : view === 'sim' ? (sessionBoard ?? session.boardLabel) : session.boardLabel

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
            {/* The view's name answers "where am I" and is four words at most,
                so it keeps its width and the board name (which has a tooltip
                and a natural ellipsis point) gives ground first. */}
            <h1 className="text-[15px] font-semibold shrink-0" style={{ margin: 0, color: 'var(--silk)' }}>
              {VIEW_TITLES[view]}
            </h1>
            {headerBoard && (
              <span
                className="text-[12px] truncate"
                // On a phone header this is the only place the board's name
                // appears (the rail's identity card is icon-collapsed), and it
                // gets ellipsed there: the full name stays reachable.
                title={headerBoard}
                style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
              >
                {headerBoard}
              </span>
            )}
          </div>

          <div className="flex-1" />

          {/* Status chips need about 190px of their own. They appear from `md`,
              where the header has it to spare once the secondary action is a
              glyph. A long board name can still leave too little: the row then
              scrolls horizontally in its own container, so every chip stays
              reachable and none is served cut in half. */}
          <div className="hidden md:flex items-center gap-1.5 overflow-x-auto min-w-0">
            <ShellChips
              view={view}
              report={report}
              checks={checksSummary}
              sim={simStatus}
              simMounted={simMounted}
              simOffline={simOffline}
              sessionBoard={sessionBoard}
              sessionMatchesCurrent={sessionMatchesCurrent}
              serverLiveActive={serverLiveActive}
              goTo={setView}
              onOpenLiveSession={openLiveSession}
              onDriveLive={driveLive}
            />
          </div>

          {/* Both header actions keep their glyph at every width and give up
              their words as the header narrows: the secondary one first (from
              `lg`), the primary CTA last (from `sm`). Icon-only, never a label
              the header cut in half: the words ride along as the accessible
              name, and only as the name — a `title` carrying the same string
              would make the button announce itself twice. */}
          {report && !session.busy && (
            <button
              type="button"
              data-testid="header-another-board"
              onClick={analyzeAnother}
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

        {session.launch.phase === 'confirm' && (
          <ReplaceLiveConfirm
            launch={session.launch}
            onReplace={() => {
              session.confirmReplace()
              setSimMounted(true)
              setView('sim')
            }}
            onOpenRunning={() => { session.cancelLaunch(); openLiveSession() }}
            onCancel={session.cancelLaunch}
          />
        )}
        {session.launch.phase === 'error' && session.launch.error && (
          <LaunchErrorBanner error={session.launch.error} onOpenEnv={() => navigate('env')} />
        )}
        {resumeError && (
          <ResumeErrorBanner reason={resumeError} onDismiss={() => setResumeError(null)} />
        )}

        {/* Views. Mounted once, hidden on navigation. */}
        <main className="flex-1 min-h-0 relative">
          <div style={{ display: view === 'board' ? 'block' : 'none', height: '100%' }}>
            {report ? (
              <BoardView
                session={session}
                onQueue={queueRequest}
                onOpenChecks={() => setView('checks')}
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
                avrAvailable={avrAvailable}
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
                schematicFile={session.schematicFile}
                supplementalFiles={session.supplementalFiles}
                avrAvailable={avrAvailable}
                selectedNet={session.selectedNet}
                selectedComponent={session.selectedComponent}
                pending={requests.items}
                onPendingConsumed={requests.consume}
                liveRegisterMapAvailable={simMounted && sessionMatchesCurrent}
                onAttachRegisterMapLive={queuedLiveRegisterMaps.push}
                liveActionResult={liveActionResult}
                onSummary={setChecksSummary}
                onSpec={setSpec}
              />
            </div>
          )}

          {simMounted && (
            <div style={{ display: view === 'sim' ? 'block' : 'none', height: '100%' }}>
              <SimView
                onQueue={queueRequest}
                onOpenChecks={() => setView('checks')}
                pendingLiveRegisterMaps={queuedLiveRegisterMaps.items}
                onLiveRegisterMapsConsumed={queuedLiveRegisterMaps.consume}
                onLiveActionResult={setLiveActionResult}
                onStatus={setSimStatus}
                expectedBoard={session.boardLabel}
                sessionMatchesCurrent={sessionMatchesCurrent}
                modelCoverage={report?.model_coverage ?? null}
                componentAssertions={report?.component_assertions ?? null}
                onRelaunch={reportOk && session.liveMode !== 'none' ? relaunchWithCurrent : undefined}
              />
            </div>
          )}

          {envVisited && (
            <div className="overflow-y-auto" style={{ display: view === 'env' ? 'block' : 'none', height: '100%' }}>
              <div className="max-w-3xl mx-auto px-6 pb-16 view-enter">
                <DepsPanel engineVersion={engineVersion} />
              </div>
            </div>
          )}
        </main>
      </div>

      {/* Hidden file inputs, shared by the drop card and every firmware jack,
          so the report view can keep offering both slots. The board picker's
          accepted list comes from lib/board-formats, the one such list, so it
          cannot offer a different set from the one the copy names. */}
      <input
        id="board-file"
        type="file"
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
