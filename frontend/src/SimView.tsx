import { useState, useCallback, useEffect, useRef, useMemo } from 'react'
import { useSimulation } from './hooks/useSimulation'
import { BoardViewer, TOOLBAR_CLEARANCE } from './components/BoardViewer'
import { TransportBar } from './components/TransportBar'
import { SelectionCard } from './components/SelectionCard'
import { NetPanel } from './components/NetPanel'
import { SerialConsole } from './components/SerialConsole'
import { SolverControlsPanel } from './components/SolverControlsPanel'
import { InputSourcesPanel } from './components/InputSourcesPanel'
import { ProbeScopePanel } from './components/ProbeScopePanel'
import { FaultPanel } from './components/FaultPanel'
import { PowerPanel } from './components/PowerPanel'
import {
  ChevronDownIcon, ChevronRightIcon, CpuIcon, SlidersIcon, PowerIcon,
  ProbeIcon, BoltIcon, TerminalIcon, LayersIcon,
} from './components/Icons'
import type { ClientMessage, SimFrame, SimFault } from './types/protocol'
import { envelopesFromHistory, envelopeSource, readNet } from './lib/net-state'

interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
  padNet?: string | null
  padNets?: string[]
}

// ── Right-rail card: the concept's card language. Collapsible, with the open
// state persisted so the rail comes back the way it was left. ──

const CARD_STATE_KEY = 'hauksbee.simrail'

function loadCardState(): Record<string, boolean> {
  try {
    return JSON.parse(localStorage.getItem(CARD_STATE_KEY) ?? '{}') as Record<string, boolean>
  } catch {
    return {}
  }
}

function RailCard({ id, title, icon, badge, defaultOpen = true, children, cardState, onToggle }: {
  id: string
  title: string
  icon: React.ReactNode
  badge?: React.ReactNode
  defaultOpen?: boolean
  children: React.ReactNode
  cardState: Record<string, boolean>
  onToggle: (id: string, open: boolean) => void
}) {
  const open = cardState[id] ?? defaultOpen
  return (
    <section
      // shrink-0: the rail is a flex column; without it the cards compress to
      // share the viewport height and every card body gets clipped.
      className="hb-card overflow-hidden shrink-0"
      // scrollSnapAlign start, against the rail's scroll-padding: the rail
      // settles with this card's header as the first thing on screen, never
      // part-way down its body and never with the card above it clipped.
      style={{ borderRadius: 10, scrollSnapAlign: 'start' }}
      data-testid={`rail-${id}`}
    >
      <button
        type="button"
        onClick={() => onToggle(id, !open)}
        aria-expanded={open}
        className="hb-press flex items-center gap-2 w-full px-3 cursor-pointer"
        style={{
          height: 36, background: 'none', border: 'none',
          borderBottom: open ? '1px solid var(--rule)' : 'none',
          color: 'var(--silk-dim)', textAlign: 'left',
        }}
      >
        <span style={{ display: 'inline-flex', color: 'var(--copper)' }}>{icon}</span>
        <span className="text-[11px] font-bold tracking-[0.1em] uppercase flex-1" style={{ color: 'var(--silk-dim)' }}>
          {title}
        </span>
        {badge}
        <span style={{ display: 'inline-flex', color: 'var(--silk-faint)' }}>
          {open ? <ChevronDownIcon size={13} /> : <ChevronRightIcon size={13} />}
        </span>
      </button>
      {open && <div>{children}</div>}
    </section>
  )
}

/** MCU stat chips: what the co-sim is actually doing per MCU (backend, run
 *  state, recent UART traffic). Only real signals from the wire; no invented
 *  frequency counters. */
function McuChips({ mcus, frame, uartActive }: {
  mcus: [string, string][]
  frame: SimFrame | null
  uartActive: Set<string>
}) {
  return (
    <div className="px-3 py-2.5 flex flex-col gap-2">
      {mcus.map(([ref, backend]) => {
        const running = (frame?.component_states?.[ref]?.['running'] ?? 0) > 0
        const uart = uartActive.has(ref)
        return (
          <div key={ref} className="flex items-center gap-2 flex-wrap">
            <span className="text-[12px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
              {ref}
            </span>
            <span
              className="text-[10px] px-1.5 py-0.5 rounded"
              style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}
              title="Emulation backend"
            >
              {backend}
            </span>
            <span
              className="text-[10px] px-1.5 py-0.5 rounded inline-flex items-center gap-1"
              style={{
                background: running ? 'var(--ok-bg)' : 'var(--surface-2)',
                border: running ? '1px solid var(--ok-border)' : '1px solid var(--hairline)',
                color: running ? 'var(--ok)' : 'var(--silk-faint)',
              }}
            >
              {running && <span className="run-dot" style={{ width: 5, height: 5, borderRadius: 3, background: 'currentColor' }} />}
              {running ? 'running' : 'halted'}
            </span>
            <span
              className="text-[10px] px-1.5 py-0.5 rounded"
              style={{
                background: uart ? 'var(--copper-tint)' : 'var(--surface-2)',
                border: uart ? '1px solid var(--copper-deep)' : '1px solid var(--hairline)',
                color: uart ? 'var(--copper-hi)' : 'var(--silk-faint)',
              }}
              title="UART traffic in the last two seconds"
            >
              uart {uart ? '●' : '○'}
            </span>
          </div>
        )
      })}
    </div>
  )
}

/** What the sim view reports up to the shell: run state, fault count, and,
 *  crucially, the SESSION's identity (the board /ws says it is streaming), so
 *  the shell's header and chips bind to the session rather than to the
 *  locally analyzed board. */
export interface SimShellStatus {
  running: boolean
  faults: number
  /** Board name from the session's BoardInfo frame; null while disconnected. */
  sessionBoard: string | null
  connected: boolean
}

// The live-sim view: transport row, the board as the hero surface, and the
// right rail reorganised into cards (MCU, inputs, power, scope, nets, faults,
// serial, solver). Mounting it opens the sim WebSocket (via useSimulation),
// so the shell only mounts it once a live board is actually being served, and
// keeps it mounted (hidden) so the session's fault log and scope survive
// navigation.
export default function SimView({ onQueueCheck, onStatus, expectedBoard, sessionMatchesCurrent, onRelaunch }: {
  /** Queue a check into the checks builder from a click on the live board.
   *  Absent on the standalone demo server. */
  onQueueCheck?: (check: { kind: string; net?: string; ref?: string }) => void
  /** Report running state + fault count + session identity up to the shell. */
  onStatus?: (s: SimShellStatus) => void
  /** The board currently analyzed in this tab (for the wrong-board banner). */
  expectedBoard?: string | null
  /** True when THIS page launched (or preloaded) the session for the current
   *  board; false means the session on /ws is foreign (another board, a stale
   *  tab, a pre-reload launch) and the view must say so. */
  sessionMatchesCurrent?: boolean
  /** Replace the running session with the analyzed board (label says so). */
  onRelaunch?: () => void
} = {}) {
  const { connected, boardInfo, frame: liveFrame, status, send, replay, backlog, serverError } = useSimulation()

  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedFp, setSelectedFp] = useState<FootprintInfo | null>(null)
  const [railOpen, setRailOpen] = useState(true)
  // Expand-to-viewport for the live board. Per-view, not persisted.
  const [boardFullscreen, setBoardFullscreen] = useState(false)
  // 2D/3D mode of the viewer's segmented control, so the hint chip describes
  // the interactions that actually exist in that mode.
  const [viewerMode, setViewerMode] = useState<'2d' | '3d'>('2d')
  const [probes, setProbes] = useState<string[]>([])
  const [selectedFaultRef, setSelectedFaultRef] = useState<string | null>(null)
  const frameHistory = useRef<SimFrame[]>([])
  const [cardState, setCardState] = useState<Record<string, boolean>>(loadCardState)

  const toggleCard = useCallback((id: string, open: boolean) => {
    setCardState(prev => {
      const next = { ...prev, [id]: open }
      try { localStorage.setItem(CARD_STATE_KEY, JSON.stringify(next)) } catch { /* non-fatal */ }
      return next
    })
  }, [])

  // Retained frames. Each is a COMPLETE state (net voltages, component states,
  // faults, supplies, UART), which is what lets the transport walk backwards
  // through them: the engine cannot un-step a simulation, but the client can
  // re-show one it was already sent. Feeds the serial console too.
  const HISTORY_LEN = 120
  useEffect(() => {
    if (liveFrame) {
      frameHistory.current = [...frameHistory.current.slice(-(HISTORY_LEN - 1)), liveFrame]
    }
  }, [liveFrame])

  // Where the review is looking, or null while the screen shows the live frame.
  const [reviewIndex, setReviewIndex] = useState<number | null>(null)

  // EVERYTHING below that displays sim state reads `frame`, so pointing it at a
  // retained frame puts the whole instrument (board, scope, nets, faults) at
  // that instant at once. The accumulating effects above deliberately keep
  // reading `liveFrame`: re-playing an old frame through them would log its
  // faults and UART bytes a second time.
  const frame = reviewIndex !== null
    ? (frameHistory.current[reviewIndex] ?? liveFrame)
    : liveFrame

  // Per-net excursion over the retained frames, for the surfaces that name a
  // net. Recomputed per frame over a short window (see net-state.ts): the
  // engine's own intra-chunk extremes would be better and are preferred
  // automatically the moment the wire carries them.
  const netEnvelopes = useMemo(
    () => envelopesFromHistory(frameHistory.current),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [liveFrame],
  )
  const netEnvelopeSource = envelopeSource(liveFrame, netEnvelopes)

  const stepBack = useCallback(() => {
    // Reviewing a past frame while the sim runs on would be unreadable, and the
    // buffer would slide under the cursor. Stepping back stops the clock.
    send({ type: 'Pause' })
    setReviewIndex(prev => {
      const len = frameHistory.current.length
      if (len === 0) return prev
      const from = prev ?? len - 1
      return Math.max(0, from - 1)
    })
  }, [send])

  const stepForward = useCallback(() => {
    setReviewIndex(prev => {
      if (prev === null) return null
      // Past the newest retained frame the review is over: hand the screen back
      // to the live feed rather than sitting on a stale "latest".
      return prev + 1 >= frameHistory.current.length - 1 ? null : prev + 1
    })
  }, [])

  const resumeLive = useCallback(() => setReviewIndex(null), [])

  const history = useMemo(() => ({
    retained: frameHistory.current.length,
    index: reviewIndex,
    t: reviewIndex !== null ? (frameHistory.current[reviewIndex]?.t ?? null) : null,
    canStepBack: frameHistory.current.length > 1
      && (reviewIndex === null || reviewIndex > 0),
    stepBack,
    stepForward,
    resume: resumeLive,
    // `liveFrame` is in the deps on purpose though it is not read here: the
    // counts above come out of a REF, so without it the retained total would
    // stay frozen at whatever it was when the review last changed.
  }), [reviewIndex, stepBack, stepForward, resumeLive, liveFrame])

  // Recent UART activity per MCU (drives the uart chip). A ref-tracked map of
  // last-seen wall times, surfaced as a Set via state on a slow tick.
  const uartLastSeen = useRef<Map<string, number>>(new Map())
  const [uartActive, setUartActive] = useState<Set<string>>(new Set())
  useEffect(() => {
    if (!liveFrame) return
    const now = Date.now()
    for (const [mcu, bytes] of Object.entries(liveFrame.uart)) {
      if (bytes.length > 0) uartLastSeen.current.set(mcu, now)
    }
  }, [liveFrame])
  useEffect(() => {
    const t = setInterval(() => {
      const now = Date.now()
      const next = new Set<string>()
      for (const [mcu, at] of uartLastSeen.current) {
        if (now - at < 2000) next.add(mcu)
      }
      setUartActive(prev => {
        if (prev.size === next.size && [...prev].every(x => next.has(x))) return prev
        return next
      })
    }, 500)
    return () => clearInterval(t)
  }, [])

  // Persistent fault log. The server drains each fault into exactly ONE frame,
  // so at play speed a fault is visible for a single frame; anything that reads
  // the current frame alone misses it. Accumulate every fault the session has
  // seen (first occurrence per component+kind keeps its timestamp) until the
  // user clears the log or resets the sim. `restored` marks entries replayed
  // from the server's backlog on a rejoin: they are history, not news, so
  // they must not re-toast.
  const [faultLog, setFaultLog] = useState<(SimFault & { restored?: boolean })[]>([])
  useEffect(() => {
    const faults = liveFrame?.faults
    if (!faults || faults.length === 0) return
    setFaultLog(prev => {
      let next: SimFault[] | null = null
      for (const f of faults) {
        if (!prev.some(e => e.component === f.component && e.kind === f.kind) &&
            !(next?.some(e => e.component === f.component && e.kind === f.kind))) {
          next = next ?? [...prev]
          next.push(f)
        }
      }
      return next ?? prev
    })
  }, [liveFrame])

  const clearFaults = useCallback(() => {
    setFaultLog([])
    setSelectedFaultRef(null)
  }, [])

  // A session REPLACEMENT reconnects the socket: boardInfo drops to null, then
  // the new session's BoardInfo arrives. The previous session's fault log,
  // frame history, selection and probes are that session's story, not this
  // one's, and carrying them over makes a replaced sim show the old board's
  // faults.
  const hadInfo = useRef(false)
  const infoWasNull = useRef(true)
  // Mirrors `hadInfo` as STATE, because the disconnect surface has to re-render
  // on it. A dropped socket clears boardInfo, so without this the view fell
  // back to its first-connect "Waiting for the live session ..." spinner and
  // sat there forever: a loading state for a session that had already ended.
  const [everHadSession, setEverHadSession] = useState(false)
  useEffect(() => {
    if (boardInfo) {
      setEverHadSession(true)
      if (infoWasNull.current && hadInfo.current) {
        setFaultLog([])
        setSelectedFaultRef(null)
        setSelectedNet(null)
        setSelectedFp(null)
        setProbes([])
        frameHistory.current = []
        // The retained frames belonged to the session that just went away;
        // a review pointing into them would show the OLD board.
        setReviewIndex(null)
      }
      hadInfo.current = true
      infoWasNull.current = false
    } else {
      infoWasNull.current = true
    }
  }, [boardInfo])

  // Rejoin restore: the server replays its session history (fault backlog +
  // active probe set) right after BoardInfo on every subscribe, so a
  // mid-session reload shows everything that already fired instead of a clean
  // log over a sim that kept running. Declared AFTER the session-replacement
  // reset above so a replaced session's backlog seeds the already-cleared log.
  useEffect(() => {
    if (!backlog) return
    // The backlog is AUTHORITATIVE per delivery, not additive. It arrives on
    // every (re)connect and as the server's Reset acknowledgement; the server
    // records every fault before broadcasting it, so replacing loses nothing
    // a merge would have kept, while a merge cannot process the one thing an
    // EMPTY backlog means: Reset cleared the fault story, and a tab that did
    // not send the Reset must drop its pre-reset faults too.
    const heldFaults = backlog.faults ?? []
    setFaultLog(prev => {
      if (prev.length === 0 && heldFaults.length === 0) return prev
      return heldFaults.map(f => ({ ...f, restored: true }))
    })
    const heldProbes = backlog.probes ?? []
    setProbes(prev => {
      if (prev.length === heldProbes.length && heldProbes.every(p => prev.includes(p))) {
        return prev
      }
      return [...heldProbes]
    })
  }, [backlog])

  // Reset also starts the fault story over: a log of faults from the previous
  // run reads as live faults on the fresh one.
  const sendWrapped = useCallback((msg: ClientMessage) => {
    if (msg.type === 'Reset') {
      setFaultLog([])
      setSelectedFaultRef(null)
    }
    send(msg)
  }, [send])

  // Which logged parts are STILL over a limit right now. The stress monitor
  // latches (a fault fires once and does not re-fire), so the log alone cannot
  // tell "happening now" from "happened at t=0.4s". The live signal is the
  // per-component stress fraction already on every frame. The engine saturates
  // it at 1.0 (stress.rs `worst_stress.min(1.0)`), so 1.0 means "at or past its
  // rating right now" and anything below means the condition has cleared. A
  // part the frame does not report on is left as it was rather than declared
  // recovered, and a destroyed part never recovers.
  const activeFaultRefs = useMemo(() => {
    const states = frame?.component_states
    const active = new Set<string>()
    for (const f of faultLog) {
      const stress = states?.[f.component]?.stress
      if (f.destroyed || stress === undefined || stress >= 1) active.add(f.component)
    }
    return active
  }, [faultLog, frame])

  // Faulted refs drive the 2D/3D part glow, and the glow means "over its rating
  // NOW". A part that recovered keeps its row in the log but stops glowing.
  const faultedRefs = useMemo(
    () => (activeFaultRefs.size === 0 ? undefined : activeFaultRefs),
    [activeFaultRefs],
  )

  const faultCount = new Set(faultLog.map(f => f.component)).size
  const running = status?.running ?? false

  // Report status up to the shell (chips + nav badges); only on change.
  const sessionBoard = boardInfo?.name ?? null
  const lastReported = useRef<SimShellStatus | null>(null)
  useEffect(() => {
    if (!onStatus) return
    const cur: SimShellStatus = { running, faults: faultCount, sessionBoard, connected }
    const prev = lastReported.current
    if (!prev || prev.running !== cur.running || prev.faults !== cur.faults
      || prev.sessionBoard !== cur.sessionBoard || prev.connected !== cur.connected) {
      lastReported.current = cur
      onStatus(cur)
    }
  }, [running, faultCount, sessionBoard, connected, onStatus])

  // Keyboard shortcuts. While an input has focus they are suppressed (typing
  // must win); inputFocused drives the hint bar affordance below.
  const [inputFocused, setInputFocused] = useState(false)
  useEffect(() => {
    const isFormField = (t: EventTarget | null) =>
      t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement
    const onFocusIn = (e: FocusEvent) => { if (isFormField(e.target)) setInputFocused(true) }
    const onFocusOut = () => {
      // The next focused element is not known until after the event settles.
      requestAnimationFrame(() => setInputFocused(isFormField(document.activeElement)))
    }
    window.addEventListener('focusin', onFocusIn)
    window.addEventListener('focusout', onFocusOut)
    return () => {
      window.removeEventListener('focusin', onFocusIn)
      window.removeEventListener('focusout', onFocusOut)
    }
  }, [])

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return
      if (e.key === ' ') {
        e.preventDefault()
        send({ type: (status?.running ?? false) ? 'Pause' : 'Play' })
      } else if (e.key === 'n' || e.key === 'N') {
        e.preventDefault()
        send({ type: 'Step', dt: 0.001 })
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [status, send])

  const handleFootprintClick = useCallback((info: FootprintInfo) => {
    setSelectedFp(info)
    setSelectedNet(null)
  }, [])

  const handleAddProbe = useCallback((net: string) => {
    setProbes(prev => prev.includes(net) ? prev : [...prev, net])
    send({ type: 'AddProbe', net } satisfies ClientMessage)
  }, [send])

  const handleRemoveProbe = useCallback((net: string) => {
    setProbes(prev => prev.filter(p => p !== net))
    send({ type: 'RemoveProbe', net } satisfies ClientMessage)
  }, [send])

  // Board URL comes from BoardInfo (sent on connect). No demo fallback: in
  // the shell, SimView mounts only for a launched session, and the fallback
  // fetch just 404ed while BoardInfo was in flight.
  const boardUrl = boardInfo?.board_url ?? null
  const mcus = boardInfo?.mcus ?? []
  const hasSupplies = !!boardInfo?.power_supplies && Object.keys(boardInfo.power_supplies).length > 0

  // The FAULTS card badge counts what the card lists: fault CONDITIONS
  // (component + kind entries). The header chips / status bar / nav badge
  // count faulted PARTS, and say so; the two numbers legitimately differ
  // (one part can trip several limits).
  const faultBadge = faultLog.length > 0 ? (
    <span
      className="text-[10px] font-bold px-1.5 rounded-full tnum"
      title={`${faultLog.length} fault condition${faultLog.length === 1 ? '' : 's'} logged`}
      style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)', minWidth: 17, textAlign: 'center' }}
    >
      {faultLog.length}
    </span>
  ) : undefined

  // Copper-short honesty for the live surface, mirroring the report co-sim's
  // "ran WITH the shorts bridged" note (or the version-warning refusal).
  const shorts = boardInfo?.shorts ?? null

  // The session was live and the socket has since dropped. Distinct from "not
  // connected yet": the first is a loss to report, the second is a wait. The
  // whole surface (canvas, banner, rail) reads this so it stops presenting the
  // empty cards of a dead session as though they described a real board.
  const sessionLost = everHadSession && !connected

  return (
    <div className="flex flex-col h-full overflow-hidden" style={{ background: 'var(--canvas)' }}>
      {/* Transport row */}
      <TransportBar
        connected={connected}
        boardInfo={boardInfo}
        status={status}
        // In a replay the capture-time throughput would read as the playback
        // rate; the speed control already owns that number. The rates come off
        // the LIVE frame: a retained frame's rates describe the moment it was
        // computed, and the bar is reporting how the loop is doing now.
        realtimeFactor={replay ? null : liveFrame?.realtime_factor ?? null}
        requestedFactor={replay ? null : liveFrame?.requested_factor ?? null}
        rateLimited={replay ? false : liveFrame?.rate_limited ?? false}
        send={sendWrapped}
        replay={replay}
        // A recording has a real timeline to scrub; the retained-frame walk is
        // for the live sim, which has none.
        history={replay ? undefined : history}
      />

      {/* Session-identity banner: this view is bound to what /ws streams. When
          that session was not launched by this page for the analyzed board (a
          different board, a stale tab, a launch from before a reload), say so
          explicitly and offer the relaunch, instead of letting the canvas
          silently show one board under a header that claims another. */}
      {sessionMatchesCurrent === false && sessionBoard && (
        <div
          data-testid="sim-foreign-session"
          className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5 text-[12px] shrink-0"
          style={{ background: 'var(--warn-bg)', borderBottom: '1px solid var(--warn-border)', color: 'var(--silk)' }}
        >
          <span>
            This live session is running{' '}
            <b style={{ fontFamily: 'var(--font-mono)', fontWeight: 600 }}>{sessionBoard}</b>
            {expectedBoard && expectedBoard !== sessionBoard ? (
              <>
                , not the board you analyzed (
                <b style={{ fontFamily: 'var(--font-mono)', fontWeight: 600 }}>{expectedBoard}</b>).
              </>
            ) : expectedBoard ? (
              <>, launched before this page load; it may not match the file you just analyzed.</>
            ) : (
              <>, launched before this page load.</>
            )}
          </span>
          {onRelaunch && expectedBoard && (
            <button
              type="button"
              data-testid="sim-relaunch-current"
              onClick={onRelaunch}
              className="hb-btn hb-press px-2.5 text-[11px]"
              style={{ height: 26 }}
            >
              Relaunch with {expectedBoard}
            </button>
          )}
        </div>
      )}

      {/* Terminal-failure banner: the server stopped this session (a dead
          analog solve, an engine crash) and said why. The reason must be ON
          the surface: a stop message that only reaches the devtools console
          leaves the user watching a frozen clock with no explanation. */}
      {serverError && (
        <div
          data-testid="sim-server-error"
          className="px-4 py-2.5 text-[12px] shrink-0"
          style={{
            background: 'var(--warn-bg)',
            borderBottom: '1px solid var(--warn-border)',
            color: 'var(--silk)',
          }}
        >
          {serverError}
        </div>
      )}

      {/* Copper-short disclosure: this session's engine either bridged the
          DRC's detected shorts (so the rails reflect the board as built,
          matching the report's co-sim block) or refused to (unvalidated
          layout version), and either way the surface must say so instead of
          letting idealised or sagged rails pass without explanation. */}
      {shorts && (
        <div
          data-testid="sim-shorts-note"
          className="px-4 py-2 text-[12px] shrink-0"
          style={{
            background: 'var(--surface)',
            borderBottom: '1px solid var(--hairline)',
            borderLeft: '4px solid var(--copper)',
            color: 'var(--silk)',
          }}
        >
          {shorts.bridged > 0 ? (
            <>
              {shorts.bridged} copper short{shorts.bridged === 1 ? '' : 's'} bridged into the live
              circuit; voltages reflect the board as built, not an idealised un-shorted version
              (the report's co-sim ran with the same bridge).
            </>
          ) : (
            <>
              {shorts.detected} copper short{shorts.detected === 1 ? '' : 's'} detected but NOT
              bridged ({shorts.unapplied_reason ?? 'could not be applied'}); the live voltages
              show the un-shorted board.
            </>
          )}
        </div>
      )}

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Board canvas (dominant center) */}
        <div
          className="flex-1 relative min-w-0 overflow-hidden"
          style={boardFullscreen
            ? { position: 'fixed', inset: 0, zIndex: 60, background: 'var(--instrument)' }
            : undefined}
        >
          {boardUrl === null ? (
            <div
              className="absolute inset-0 flex items-center justify-center px-6"
              style={{ background: 'var(--instrument)' }}
              data-testid={sessionLost ? 'sim-session-lost' : 'sim-waiting'}
            >
              {sessionLost ? (
                // The session ENDED; a spinner here claimed it was still
                // arriving. Name the loss, say what the client is doing about
                // it, and point at the one thing that actually recovers.
                <div className="text-center" style={{ maxWidth: '28rem' }}>
                  <div className="text-[15px] font-semibold" style={{ color: 'var(--err-strong)' }}>
                    The live session ended.
                  </div>
                  <div className="mt-2 text-[13px] leading-relaxed" style={{ color: 'var(--overlay-chip-text)' }}>
                    The connection to <code className="hb-inline">/ws</code> dropped, so the board,
                    the rails and the scope below have nothing to read. Everything this page
                    already recorded is kept; nothing new arrives until a session is back.
                  </div>
                  <div className="mt-2 text-[12px] flex items-center justify-center gap-2" style={{ color: 'var(--overlay-hint-dim)' }}>
                    <span className="slot-spin" /> Retrying every 2 seconds.
                  </div>
                  {onRelaunch && expectedBoard && (
                    <button
                      type="button"
                      data-testid="sim-relaunch-after-loss"
                      onClick={onRelaunch}
                      className="hb-btn-primary hb-press px-3.5 text-[12px] mt-3"
                      style={{ height: 30 }}
                    >
                      Launch {expectedBoard} again
                    </button>
                  )}
                  <div className="mt-2.5 text-[12px]" style={{ color: 'var(--overlay-hint-dim)' }}>
                    If the server itself is gone, restart it with{' '}
                    <code className="hb-inline">hauksbee serve</code> and reload this page.
                  </div>
                </div>
              ) : (
                <div className="flex items-center gap-2 text-sm" style={{ color: 'var(--overlay-chip-text)' }}>
                  <span className="slot-spin" /> Waiting for the live session ...
                </div>
              )}
            </div>
          ) : (
          <BoardViewer
            boardFile={boardUrl}
            frame={frame}
            boardInfo={boardInfo}
            selectedNet={selectedNet}
            onFootprintClick={handleFootprintClick}
            onNetClick={net => {
              setSelectedNet(net)
              // A trace click replaces a part selection (and vice versa in
              // handleFootprintClick): one floating panel at a time.
              if (net) setSelectedFp(null)
            }}
            faultedRefs={faultedRefs}
            onViewModeChange={setViewerMode}
            fullscreen={boardFullscreen}
            onToggleFullscreen={() => setBoardFullscreen(v => !v)}
          />
          )}

          {/* Floating selection card: same language as the report map. */}
          {(selectedFp || selectedNet) && (
            <div
              className="absolute left-3 z-10 flex items-end pointer-events-none"
              style={{ top: TOOLBAR_CLEARANCE, bottom: 36 }}
            >
              <div className="pointer-events-auto" style={{ maxHeight: '100%', display: 'flex' }}>
              <SelectionCard
                net={selectedFp ? null : selectedNet}
                // What the net actually is, not just a number: driven, moving,
                // or unobservable on this backend (see lib/net-state.ts).
                reading={selectedNet && !selectedFp
                  ? readNet(frame, selectedNet, netEnvelopes)
                  : undefined}
                component={selectedFp}
                boundKind={selectedFp ? boardInfo?.component_kinds?.[selectedFp.ref] ?? null : null}
                onQueueCheck={onQueueCheck}
                onClose={() => { setSelectedFp(null); setSelectedNet(null) }}
                onPickNet={net => { setSelectedFp(null); setSelectedNet(net) }}
              />
              </div>
            </div>
          )}

          {/* Board overlay hints. While an input has focus the keyboard
              shortcuts are suppressed, so the hint dims and says why. */}
          <div
            className="absolute bottom-2 left-2 text-[10px] px-2 py-1 rounded pointer-events-none"
            style={{
              background: 'var(--overlay-hint-bg)',
              color: inputFocused ? 'var(--overlay-hint-dim)' : 'var(--overlay-chip-text)',
              border: '1px solid var(--overlay-hint-border)',
              opacity: inputFocused ? 0.65 : 1,
              transition: 'opacity 0.2s ease, color 0.2s ease',
            }}
          >
            {inputFocused
              ? 'shortcuts paused, click away from the input to use Space / N'
              : viewerMode === '3d'
                ? 'Space=play/pause · N=step · drag=orbit · scroll=zoom · shift+drag=pan'
                : 'Space=play/pause · N=step · scroll=zoom · drag=pan · hover=probe'}
          </div>
        </div>

        {/* Right rail toggle */}
        <button
          onClick={() => setRailOpen(o => !o)}
          className="hb-press flex items-center justify-center shrink-0"
          style={{
            width: 18,
            background: 'var(--surface)',
            borderLeft: '1px solid var(--hairline)',
            color: 'var(--silk-faint)',
            fontSize: 10,
            cursor: 'pointer',
          }}
          aria-label={railOpen ? 'Collapse the control rail' : 'Expand the control rail'}
          title={railOpen ? 'Collapse the control rail' : 'Expand the control rail'}
        >
          {railOpen ? '›' : '‹'}
        </button>

        {/* Right rail: the instrument cards */}
        {railOpen && (
          <div
            className="flex flex-col gap-2.5 shrink-0 overflow-y-auto p-2.5"
            style={{
              width: 316,
              borderLeft: '1px solid var(--hairline)',
              background: 'var(--canvas)',
              // Snap the scroll to card starts, and hold the rail's own padding
              // clear of the top edge, so a card never comes to rest clipped
              // mid-input: the first thing at the top is always a card header.
              scrollSnapType: 'y proximity',
              scrollPaddingTop: 10,
            }}
          >
            {/* Without this the rail's cards ("No nets loaded", "No MCUs in
                board info", an empty scope) read as findings ABOUT the board
                rather than as the aftermath of a lost connection. */}
            {sessionLost && (
              <div
                data-testid="rail-session-lost"
                className="rounded-lg px-3 py-2.5 text-[12px] leading-relaxed shrink-0"
                style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--silk)' }}
              >
                <span className="text-[10px] font-bold tracking-widest uppercase block mb-1" style={{ color: 'var(--err-strong)' }}>
                  Session ended
                </span>
                These cards are empty because the live link dropped, not because
                the board has nothing on it.
              </div>
            )}

            {mcus.length > 0 && (
              <RailCard id="mcu" title="MCU" icon={<CpuIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
                <McuChips mcus={mcus} frame={frame} uartActive={uartActive} />
              </RailCard>
            )}

            {/* Input-shaped panels only against a live engine: in a replay
                the knobs would silently do nothing, so instead of disabled
                controls the rail says plainly what a recording is. */}
            {replay ? (
              <RailCard id="recorded" title="Recorded run" icon={<PowerIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
                <div className="px-3 py-2.5 text-[11px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
                  Inputs, power rails and solver options were set when this
                  session was captured from the real engine; a recording
                  cannot take new ones. Install hauksbee to turn the knobs on
                  your own boards.
                </div>
              </RailCard>
            ) : (
              <>
                <RailCard id="inputs" title="Inputs" icon={<SlidersIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
                  <InputSourcesPanel boardInfo={boardInfo} frame={frame} send={send} />
                </RailCard>

                {hasSupplies && (
                  <RailCard id="power" title="Power rails" icon={<PowerIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
                    <PowerPanel boardInfo={boardInfo} frame={frame} send={send} />
                  </RailCard>
                )}
              </>
            )}

            <RailCard id="scope" title="Scope" icon={<ProbeIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
              <ProbeScopePanel
                boardInfo={boardInfo}
                frame={frame}
                probes={probes}
                onAddProbe={handleAddProbe}
                onRemoveProbe={handleRemoveProbe}
                send={send}
              />
            </RailCard>

            <RailCard id="nets" title="Net voltages" icon={<LayersIcon size={13} />} defaultOpen={false} cardState={cardState} onToggle={toggleCard}>
              <NetPanel
                frame={frame}
                selectedNet={selectedNet}
                onSelectNet={setSelectedNet}
                envelopes={netEnvelopes}
                envelopeSource={netEnvelopeSource}
              />
            </RailCard>

            <RailCard id="faults" title="Faults" icon={<BoltIcon size={13} />} badge={faultBadge} cardState={cardState} onToggle={toggleCard}>
              <FaultPanel
                faults={faultLog}
                activeRefs={activeFaultRefs}
                onClear={clearFaults}
                onFaultComponentSelect={setSelectedFaultRef}
                selectedFaultRef={selectedFaultRef}
              />
            </RailCard>

            <RailCard id="serial" title="Serial console" icon={<TerminalIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
              <div style={{ height: 300 }}>
                <SerialConsole mcus={mcus} frames={frameHistory.current} send={send} readOnly={!!replay} />
              </div>
            </RailCard>

            {!replay && (
              <RailCard id="solver" title="Solver" icon={<SlidersIcon size={13} />} defaultOpen={false} cardState={cardState} onToggle={toggleCard}>
                <SolverControlsPanel controls={status?.options ?? null} send={send} />
              </RailCard>
            )}
          </div>
        )}
      </div>

      {/* Bottom status bar */}
      <div
        className="flex items-center gap-3 px-3 shrink-0 text-[10px] overflow-hidden tnum"
        style={{
          minHeight: 26,
          height: 26,
          background: 'var(--surface)',
          borderTop: '1px solid var(--hairline)',
          fontFamily: 'var(--font-mono)',
          flexWrap: 'nowrap',
          color: 'var(--silk-faint)',
        }}
      >
        {/* Running indicator */}
        {/* "paused" is a claim about a sim that is still there. A dropped
            session is not paused, and saying so sent readers looking for a
            play button that could not help them. */}
        <div className="flex items-center gap-1.5">
          <div
            className={running ? 'run-dot' : undefined}
            style={{
              width: 6, height: 6, borderRadius: 3,
              background: sessionLost ? 'var(--err)' : running ? 'var(--ok)' : 'var(--silk-faint)',
            }}
          />
          <span style={{ color: sessionLost ? 'var(--err)' : running ? 'var(--ok)' : 'var(--silk-faint)' }}>
            {sessionLost ? 'session ended' : running ? 'running' : 'paused'}
          </span>
        </div>

        <span style={{ color: 'var(--hairline)' }}>|</span>

        <span>
          sim: <span style={{ color: 'var(--silk-dim)' }}>{(status?.sim_time ?? 0).toFixed(4)}s</span>
        </span>

        {frame && (
          <>
            {/* Capture-time throughput would masquerade as playback rate in a
                replay; the transport's speed control owns that number there. */}
            {!replay && (
              <>
                <span style={{ color: 'var(--hairline)' }}>|</span>
                <span>
                  rt: <span style={{ color: 'var(--silk-dim)' }}>{frame.realtime_factor.toFixed(2)}x</span>
                </span>
              </>
            )}
            <span style={{ color: 'var(--hairline)' }}>|</span>
            <span>
              nets: <span style={{ color: 'var(--silk-dim)' }}>{Object.keys(frame.net_voltages).length}</span>
            </span>
          </>
        )}

        {probes.length > 0 && (
          <>
            <span style={{ color: 'var(--hairline)' }}>|</span>
            <span className="overflow-hidden" style={{ maxWidth: 180, whiteSpace: 'nowrap', textOverflow: 'ellipsis' }}>
              probes: <span style={{ color: 'var(--copper)' }}>{probes.join(', ')}</span>
            </span>
          </>
        )}

        {faultCount > 0 && (
          <>
            <span style={{ color: 'var(--hairline)' }}>|</span>
            <span style={{ color: 'var(--err)' }}>
              FAULTS: {faultCount} part{faultCount === 1 ? '' : 's'}
            </span>
          </>
        )}

        <div className="flex-1" />

        {boardInfo && (
          <span>
            {boardInfo.num_components} comp · {boardInfo.num_nets} nets
          </span>
        )}
      </div>
    </div>
  )
}
