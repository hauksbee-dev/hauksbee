import { useCallback, useEffect, useRef, useState } from 'react'
import { Sidebar } from './components/Sidebar'
import type { AppView } from './components/Sidebar'
import { UploadView } from './components/UploadView'
import { BoardView } from './components/BoardView'
import { ChecksView, checksStorageKey } from './components/ChecksView'
import type { ChecksSummary } from './components/ChecksView'
import { DepsPanel } from './components/DepsPanel'
import SimView from './SimView'
import { useTheme } from './hooks/useTheme'
import { useBoardSession } from './hooks/useBoardSession'
import { PlayIcon } from './components/Icons'
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
  | { kind: 'ready'; report: WebReport | null; boardName: string | null; canLaunchLive: boolean }

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
          })
        } else {
          setBoot({ kind: 'ready', report: null, boardName: null, canLaunchLive: startup.live === true })
        }
      } catch {
        // No startup endpoint (a stale/odd deployment). Degrade to the
        // drop-a-board Board view, never the live-sim view, which would sit
        // "offline" with no way to load a board.
        if (alive) setBoot({ kind: 'ready', report: null, boardName: null, canLaunchLive: false })
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
    />
  )
}

const VIEW_TITLES: Record<AppView, string> = {
  board: 'Board',
  checks: 'Checks',
  sim: 'Live Sim',
  env: 'Environment',
}

function Shell({ preloadedReport, preloadedBoardName, canLaunchLive }: {
  preloadedReport: WebReport | null
  preloadedBoardName: string | null
  canLaunchLive: boolean
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
  const [simStatus, setSimStatus] = useState<{ running: boolean; faults: number }>({ running: false, faults: 0 })
  const [checksSummary, setChecksSummary] = useState<ChecksSummary | null>(null)

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

  const simEnabled = simMounted || (reportOk && session.liveMode !== 'none')

  const navigate = useCallback((v: AppView) => {
    if (v === 'env') setEnvVisited(true)
    if (v === 'sim' && !simMounted) {
      // No session yet: the nav entry launches one, exactly like the primary
      // action, so "Live Sim" never opens an offline shell.
      driveLive()
      return
    }
    setView(v)
  }, [driveLive, simMounted])

  const analyzeAnother = useCallback(() => {
    session.resetFlow()
    setChecksSummary(null)
    setView('board')
  }, [session])

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
        className="hb-press inline-flex items-center gap-1.5 px-2.5 rounded-full text-[11px] font-semibold tnum"
        style={{ ...tones[tone], height: 24, cursor: onClick ? 'pointer' : 'default' }}
      >
        {pulse && (
          <span className="run-dot" style={{ width: 6, height: 6, borderRadius: 3, background: 'currentColor', display: 'inline-block' }} />
        )}
        {label}
      </button>
    )
  }

  const chips: React.ReactNode[] = []
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
  if (simMounted) {
    if (simStatus.faults > 0) {
      chips.push(chip(`${simStatus.faults} fault${simStatus.faults === 1 ? '' : 's'}`, 'err', () => setView('sim'), 'chip-faults'))
    }
    chips.push(chip(
      simStatus.running ? 'sim running' : 'sim paused',
      simStatus.running ? 'ok' : 'quiet',
      () => setView('sim'),
      'chip-sim',
      simStatus.running,
    ))
  } else if (reportOk && session.liveMode === 'connected') {
    chips.push(chip('live session ready', 'ok', driveLive, 'chip-sim'))
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
      />

      <div className="flex-1 flex flex-col min-w-0">
        {/* Title row: where you are, what board this is, live status, and the
            primary action. */}
        <header
          className="flex items-center gap-3 px-5 shrink-0"
          style={{ height: 56, borderBottom: '1px solid var(--hairline)', background: 'var(--surface)' }}
        >
          <div className="min-w-0 flex items-baseline gap-2.5">
            <h1 className="text-[15px] font-semibold truncate" style={{ margin: 0, color: 'var(--silk)' }}>
              {VIEW_TITLES[view]}
            </h1>
            {session.boardLabel && (
              <span
                className="text-[12px] truncate"
                style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
              >
                {session.boardLabel}
              </span>
            )}
          </div>

          <div className="flex-1" />

          <div className="hidden sm:flex items-center gap-1.5 overflow-hidden">{chips}</div>

          {report && !session.busy && (
            <button
              type="button"
              data-testid="header-another-board"
              onClick={analyzeAnother}
              className="hb-btn hb-press px-3 text-[12px] whitespace-nowrap"
              style={{ height: 32 }}
            >
              Analyze another board
            </button>
          )}

          {reportOk && session.liveMode !== 'none' && view !== 'sim' && (
            <button
              type="button"
              data-testid="run-it"
              onClick={driveLive}
              disabled={session.launch.phase === 'launching'}
              className="hb-btn-primary hb-press inline-flex items-center gap-2 px-3.5 text-[12px] whitespace-nowrap"
              style={{ height: 32 }}
            >
              {session.launch.phase === 'launching'
                ? <><span className="slot-spin" style={{ borderTopColor: 'var(--on-copper)' }} /> Launching ...</>
                : simMounted
                  ? <><PlayIcon size={12} /> Open live sim</>
                  : <><PlayIcon size={12} /> Drive it live</>}
            </button>
          )}
        </header>

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
              />
            ) : (
              <UploadView session={session} />
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
              />
            </div>
          )}

          {simMounted && (
            <div style={{ display: view === 'sim' ? 'block' : 'none', height: '100%' }}>
              <SimView
                onQueueCheck={queueCheck}
                onStatus={setSimStatus}
              />
            </div>
          )}

          {envVisited && (
            <div
              className="overflow-y-auto"
              style={{ display: view === 'env' ? 'block' : 'none', height: '100%' }}
            >
              <div className="max-w-3xl mx-auto px-6 pb-16 view-enter">
                <DepsPanel />
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
        accept=".kicad_pcb,.kicad_sch,.brd,.PcbDoc,.d356,.zip,.txt,.board"
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
