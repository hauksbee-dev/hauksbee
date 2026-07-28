import { useState, useCallback, useEffect, useRef, useMemo } from 'react'
import { useSimulation } from './hooks/useSimulation'
import { BoardViewer } from './components/BoardViewer'
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
      style={{ borderRadius: 10 }}
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

// The live-sim view: transport row, the board as the hero surface, and the
// right rail reorganised into cards (MCU, inputs, power, scope, nets, faults,
// serial, solver). Mounting it opens the sim WebSocket (via useSimulation),
// so the shell only mounts it once a live board is actually being served, and
// keeps it mounted (hidden) so the session's fault log and scope survive
// navigation.
export default function SimView({ onQueueCheck, onStatus }: {
  /** Queue a check into the checks builder from a click on the live board.
   *  Absent on the standalone demo server. */
  onQueueCheck?: (check: { kind: string; net?: string; ref?: string }) => void
  /** Report running state + fault count up to the shell's status chips. */
  onStatus?: (s: { running: boolean; faults: number }) => void
} = {}) {
  const { connected, boardInfo, frame, status, send } = useSimulation()

  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedFp, setSelectedFp] = useState<FootprintInfo | null>(null)
  const [railOpen, setRailOpen] = useState(true)
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

  // Accumulate frame history for serial console
  useEffect(() => {
    if (frame) {
      frameHistory.current = [...frameHistory.current.slice(-119), frame]
    }
  }, [frame])

  // Recent UART activity per MCU (drives the uart chip). A ref-tracked map of
  // last-seen wall times, surfaced as a Set via state on a slow tick.
  const uartLastSeen = useRef<Map<string, number>>(new Map())
  const [uartActive, setUartActive] = useState<Set<string>>(new Set())
  useEffect(() => {
    if (!frame) return
    const now = Date.now()
    for (const [mcu, bytes] of Object.entries(frame.uart)) {
      if (bytes.length > 0) uartLastSeen.current.set(mcu, now)
    }
  }, [frame])
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
  // user clears the log or resets the sim.
  const [faultLog, setFaultLog] = useState<SimFault[]>([])
  useEffect(() => {
    const faults = frame?.faults
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
  }, [frame])

  const clearFaults = useCallback(() => {
    setFaultLog([])
    setSelectedFaultRef(null)
  }, [])

  // Reset also starts the fault story over: a log of faults from the previous
  // run reads as live faults on the fresh one.
  const sendWrapped = useCallback((msg: ClientMessage) => {
    if (msg.type === 'Reset') {
      setFaultLog([])
      setSelectedFaultRef(null)
    }
    send(msg)
  }, [send])

  // Faulted refs (for 2D/3D highlights), from the accumulated log so the
  // highlight persists as long as the fault is listed.
  const faultedRefs = useMemo(() => {
    if (faultLog.length === 0) return undefined
    return new Set(faultLog.map(f => f.component))
  }, [faultLog])

  const faultCount = faultedRefs?.size ?? 0
  const running = status?.running ?? false

  // Report status up to the shell (chips + nav badges); only on change.
  const lastReported = useRef<{ running: boolean; faults: number } | null>(null)
  useEffect(() => {
    if (!onStatus) return
    const cur = { running, faults: faultCount }
    const prev = lastReported.current
    if (!prev || prev.running !== cur.running || prev.faults !== cur.faults) {
      lastReported.current = cur
      onStatus(cur)
    }
  }, [running, faultCount, onStatus])

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

  const faultBadge = faultCount > 0 ? (
    <span
      className="text-[10px] font-bold px-1.5 rounded-full tnum"
      style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)', minWidth: 17, textAlign: 'center' }}
    >
      {faultCount}
    </span>
  ) : undefined

  return (
    <div className="flex flex-col h-full overflow-hidden" style={{ background: 'var(--canvas)' }}>
      {/* Transport row */}
      <TransportBar
        connected={connected}
        boardInfo={boardInfo}
        status={status}
        realtimeFactor={frame?.realtime_factor ?? null}
        send={sendWrapped}
      />

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Board canvas (dominant center) */}
        <div className="flex-1 relative min-w-0 overflow-hidden">
          {boardUrl === null ? (
            <div
              className="absolute inset-0 flex items-center justify-center"
              style={{ background: 'var(--instrument)' }}
            >
              <div className="flex items-center gap-2 text-sm" style={{ color: '#64748b' }}>
                <span className="slot-spin" /> Waiting for the live session ...
              </div>
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
          />
          )}

          {/* Floating selection card: same language as the report map. */}
          {(selectedFp || selectedNet) && (
            <div className="absolute bottom-9 left-3 z-10">
              <SelectionCard
                net={selectedFp ? null : selectedNet}
                liveVolts={selectedNet && !selectedFp ? frame?.net_voltages[selectedNet] : undefined}
                component={selectedFp}
                boundKind={selectedFp ? boardInfo?.component_kinds?.[selectedFp.ref] ?? null : null}
                onQueueCheck={onQueueCheck}
                onClose={() => { setSelectedFp(null); setSelectedNet(null) }}
                onPickNet={net => { setSelectedFp(null); setSelectedNet(net) }}
              />
            </div>
          )}

          {/* Board overlay hints. While an input has focus the keyboard
              shortcuts are suppressed, so the hint dims and says why. */}
          <div
            className="absolute bottom-2 left-2 text-[10px] px-2 py-1 rounded pointer-events-none"
            style={{
              background: 'rgba(10,15,30,0.8)',
              color: inputFocused ? '#334155' : '#64748b',
              border: '1px solid #1e293b',
              opacity: inputFocused ? 0.65 : 1,
              transition: 'opacity 0.2s ease, color 0.2s ease',
            }}
          >
            {inputFocused
              ? 'shortcuts paused, click away from the input to use Space / N'
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
            }}
          >
            {mcus.length > 0 && (
              <RailCard id="mcu" title="MCU" icon={<CpuIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
                <McuChips mcus={mcus} frame={frame} uartActive={uartActive} />
              </RailCard>
            )}

            <RailCard id="inputs" title="Inputs" icon={<SlidersIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
              <InputSourcesPanel boardInfo={boardInfo} frame={frame} send={send} />
            </RailCard>

            {hasSupplies && (
              <RailCard id="power" title="Power rails" icon={<PowerIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
                <PowerPanel boardInfo={boardInfo} frame={frame} send={send} />
              </RailCard>
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
              <NetPanel frame={frame} selectedNet={selectedNet} onSelectNet={setSelectedNet} />
            </RailCard>

            <RailCard id="faults" title="Faults" icon={<BoltIcon size={13} />} badge={faultBadge} cardState={cardState} onToggle={toggleCard}>
              <FaultPanel
                faults={faultLog}
                onClear={clearFaults}
                onFaultComponentSelect={setSelectedFaultRef}
                selectedFaultRef={selectedFaultRef}
              />
            </RailCard>

            <RailCard id="serial" title="Serial console" icon={<TerminalIcon size={13} />} cardState={cardState} onToggle={toggleCard}>
              <div style={{ height: 300 }}>
                <SerialConsole mcus={mcus} frames={frameHistory.current} send={send} />
              </div>
            </RailCard>

            <RailCard id="solver" title="Solver" icon={<SlidersIcon size={13} />} defaultOpen={false} cardState={cardState} onToggle={toggleCard}>
              <SolverControlsPanel controls={status?.options ?? null} send={send} />
            </RailCard>
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
        <div className="flex items-center gap-1.5">
          <div
            className={running ? 'run-dot' : undefined}
            style={{
              width: 6, height: 6, borderRadius: 3,
              background: running ? 'var(--ok)' : 'var(--silk-faint)',
            }}
          />
          <span style={{ color: running ? 'var(--ok)' : 'var(--silk-faint)' }}>
            {running ? 'running' : 'paused'}
          </span>
        </div>

        <span style={{ color: 'var(--hairline)' }}>|</span>

        <span>
          sim: <span style={{ color: 'var(--silk-dim)' }}>{(status?.sim_time ?? 0).toFixed(4)}s</span>
        </span>

        {frame && (
          <>
            <span style={{ color: 'var(--hairline)' }}>|</span>
            <span>
              rt: <span style={{ color: 'var(--silk-dim)' }}>{frame.realtime_factor.toFixed(2)}x</span>
            </span>
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
              FAULTS: {faultCount}
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
