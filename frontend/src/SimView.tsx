import { useState, useCallback, useEffect, useRef, useMemo } from 'react'
import { useSimulation } from './hooks/useSimulation'
import { BoardViewer } from './components/BoardViewer'
import { TransportBar } from './components/TransportBar'
import { FootprintPanel } from './components/FootprintPanel'
import { NetPanel } from './components/NetPanel'
import { SerialConsole } from './components/SerialConsole'
import { SolverControlsPanel } from './components/SolverControlsPanel'
import { InputSourcesPanel } from './components/InputSourcesPanel'
import { ProbeScopePanel } from './components/ProbeScopePanel'
import { FaultPanel } from './components/FaultPanel'
import { PowerPanel } from './components/PowerPanel'
import type { ClientMessage, SimFrame, SimFault } from './types/protocol'

interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
  /** Net of the clicked pad, when one was in reach. */
  padNet?: string | null
}

// A clicked trace's net, with one-click assertion offers that land in the
// report page's checks builder as ordinary prefilled rows.
function NetAssertChip({ net, liveVolts, onQueueCheck, onClose }: {
  net: string
  liveVolts: number | undefined
  onQueueCheck: (check: { kind: string; net?: string; ref?: string }) => void
  onClose: () => void
}) {
  const [queued, setQueued] = useState<string | null>(null)
  const offer = (kind: string, label: string) => (
    <button
      type="button"
      data-testid={`sim-assert-${kind}`}
      onClick={() => {
        onQueueCheck({ kind, net })
        setQueued(kind)
        setTimeout(() => setQueued(null), 1800)
      }}
      className="px-2.5 py-1.5 rounded text-[11px] text-left cursor-pointer"
      style={{ background: 'rgba(224,138,78,0.12)', border: '1px solid #7c4a1e', color: '#ffb072' }}
    >
      {queued === kind ? 'Added to the checks builder ✓' : `+ ${label}`}
    </button>
  )
  return (
    <div
      data-testid="sim-net-chip"
      className="flex flex-col gap-1.5 p-3 rounded-lg"
      style={{ background: '#0f172a', border: '1px solid #1e293b', minWidth: 220, maxWidth: 300 }}
    >
      <div className="flex items-center justify-between">
        <span className="text-[10px] font-bold tracking-wider" style={{ color: '#64748b' }}>NET</span>
        <button onClick={onClose} className="text-[10px] hover:opacity-70" style={{ color: '#475569' }}>×</button>
      </div>
      <div className="flex items-baseline gap-2">
        <span className="text-sm font-bold" style={{ color: '#e2e8f0', fontFamily: "'JetBrains Mono', monospace" }}>{net}</span>
        {liveVolts !== undefined && (
          <span className="text-[11px]" style={{ color: '#60a5fa', fontFamily: "'JetBrains Mono', monospace" }}>
            {liveVolts.toFixed(3)} V
          </span>
        )}
      </div>
      {offer('voltage', 'must sit at a voltage')}
      {offer('toggle', 'must blink')}
      {offer('boot-coverage', 'firmware must drive it')}
      <div className="text-[9px]" style={{ color: '#475569' }}>
        lands in the checks builder on the report page
      </div>
    </div>
  )
}

type SidebarTab = 'nets' | 'serial' | 'controls' | 'inputs' | 'probes' | 'faults' | 'power'

const TAB_LABELS: { id: SidebarTab; label: string }[] = [
  { id: 'nets', label: 'Nets' },
  { id: 'probes', label: 'Scope' },
  { id: 'serial', label: 'Serial' },
  { id: 'inputs', label: 'Inputs' },
  { id: 'faults', label: 'Faults' },
  { id: 'power', label: 'Power' },
  { id: 'controls', label: 'Solver' },
]

// The live-sim view (scope, board viewers, transport). This is the whole app
// pre-W6 §1; it is now what the landing report expands into on "drive it
// live". Mounting it opens the sim WebSocket (via useSimulation), so the
// landing only mounts it once a live board is actually being served.
export default function SimView({ onExit, onQueueCheck }: {
  /** Back to the report page (the session keeps running server-side). */
  onExit?: () => void
  /** Queue a check into the report page's checks builder from a click on the
   *  live board (net or component). Absent on the standalone demo server. */
  onQueueCheck?: (check: { kind: string; net?: string; ref?: string }) => void
} = {}) {
  const { connected, boardInfo, frame, status, send } = useSimulation()

  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedFp, setSelectedFp] = useState<FootprintInfo | null>(null)
  const [sidebarTab, setSidebarTab] = useState<SidebarTab>('nets')
  const [sidebarOpen, setSidebarOpen] = useState(true)
  const [probes, setProbes] = useState<string[]>([])
  const [selectedFaultRef, setSelectedFaultRef] = useState<string | null>(null)
  const frameHistory = useRef<SimFrame[]>([])

  // Accumulate frame history for serial console
  useEffect(() => {
    if (frame) {
      frameHistory.current = [...frameHistory.current.slice(-119), frame]
    }
  }, [frame])

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

  // Keyboard shortcuts. While a sidebar input has focus they are suppressed
  // (typing must win); inputFocused drives the hint bar affordance below.
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
        const running = status?.running ?? false
        send({ type: running ? 'Pause' : 'Play' })
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
  }, [])

  const handleAddProbe = useCallback((net: string) => {
    setProbes(prev => prev.includes(net) ? prev : [...prev, net])
    send({ type: 'AddProbe', net } satisfies ClientMessage)
  }, [send])

  const handleRemoveProbe = useCallback((net: string) => {
    setProbes(prev => prev.filter(p => p !== net))
    send({ type: 'RemoveProbe', net } satisfies ClientMessage)
  }, [send])

  // Board URL from boardInfo, fallback to demo
  const boardUrl = boardInfo?.board_url ?? '/boards/demo.kicad_pcb'

  const mcus = boardInfo?.mcus ?? []

  const faultCount = faultedRefs?.size ?? 0

  return (
    <div
      className="flex flex-col h-screen overflow-hidden"
      style={{ background: '#020617', fontFamily: 'system-ui, sans-serif' }}
    >
      {/* Top transport bar */}
      <TransportBar
        connected={connected}
        boardInfo={boardInfo}
        status={status}
        send={sendWrapped}
        onExit={onExit}
      />

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Board canvas (dominant center) */}
        <div className="flex-1 relative min-w-0 overflow-hidden">
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

          {/* Floating footprint panel: the part's identity, its BOUND model
              (what the engine actually simulates), and component-shaped
              assertion offers that land in the report's checks builder. */}
          {selectedFp && (
            <div className="absolute top-3 left-3 z-10">
              <FootprintPanel
                info={selectedFp}
                onClose={() => setSelectedFp(null)}
                boundKind={boardInfo?.component_kinds?.[selectedFp.ref] ?? null}
                onAssert={onQueueCheck}
              />
            </div>
          )}

          {/* Floating net chip: a clicked trace's net with its assertion
              offers, mirrored from the report map so both live surfaces speak
              the same "click copper, get a check" language. */}
          {selectedNet && !selectedFp && onQueueCheck && (
            <div className="absolute top-3 left-3 z-10">
              <NetAssertChip
                net={selectedNet}
                liveVolts={frame?.net_voltages[selectedNet]}
                onQueueCheck={onQueueCheck}
                onClose={() => setSelectedNet(null)}
              />
            </div>
          )}

          {/* Board overlay hints. While a sidebar input has focus the keyboard
              shortcuts are suppressed, so the hint dims and says why. */}
          <div
            className="absolute bottom-2 left-2 text-[9px] px-2 py-1 rounded pointer-events-none"
            style={{
              background: 'rgba(10,15,30,0.8)',
              color: inputFocused ? '#1e293b' : '#334155',
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

        {/* Right sidebar toggle */}
        <button
          onClick={() => setSidebarOpen(o => !o)}
          className="flex items-center justify-center w-5 shrink-0 transition-all hover:opacity-80"
          style={{
            background: '#0a0f1e',
            borderLeft: '1px solid #1e293b',
            color: '#334155',
            fontSize: 10,
            cursor: 'pointer',
          }}
          title={sidebarOpen ? 'Collapse sidebar' : 'Expand sidebar'}
        >
          {sidebarOpen ? '›' : '‹'}
        </button>

        {/* Right sidebar */}
        {sidebarOpen && (
          <div
            className="flex flex-col shrink-0 overflow-hidden"
            style={{
              width: 280,
              borderLeft: '1px solid #1e293b',
              background: '#0a0f1e',
              // Inner top border accent gives the panel a grounded top edge
              boxShadow: 'inset 0 1px 0 rgba(224,138,78,0.12)',
            }}
          >
            {/* Tab bar */}
            <div
              className="flex shrink-0 overflow-x-auto"
              style={{ borderBottom: '1px solid #1e293b' }}
            >
              {TAB_LABELS.map(({ id, label }) => (
                <button
                  key={id}
                  onClick={() => setSidebarTab(id)}
                  className="px-3 py-2 text-[10px] font-bold tracking-wider whitespace-nowrap transition-all relative"
                  style={{
                    color: sidebarTab === id ? '#ffb072' : '#475569',
                    borderBottom: sidebarTab === id ? '2px solid #e08a4e' : '2px solid transparent',
                    background: sidebarTab === id ? 'rgba(224,138,78,0.08)' : 'transparent',
                    boxShadow: sidebarTab === id ? 'inset 0 -1px 0 rgba(224,138,78,0.4), 0 0 6px rgba(224,138,78,0.08)' : 'none',
                    textShadow: sidebarTab === id ? '0 0 8px rgba(224,138,78,0.4)' : 'none',
                  }}
                >
                  {label}
                  {id === 'faults' && faultCount > 0 && (
                    <span
                      className="absolute -top-0.5 -right-0.5 text-[8px] px-1 rounded-full font-bold"
                      style={{ background: '#7f1d1d', color: '#fca5a5', minWidth: 14 }}
                    >
                      {faultCount}
                    </span>
                  )}
                </button>
              ))}
            </div>

            {/* Simulation stats, always-visible compact card below the tab bar */}
            <div
              className="shrink-0 px-3 py-2"
              style={{ borderBottom: '1px solid #1e293b', background: 'rgba(15,23,42,0.6)' }}
            >
              <div className="flex items-center justify-between mb-1">
                <span className="text-[9px] font-bold tracking-widest uppercase" style={{ color: '#334155' }}>Simulation</span>
                <span
                  className="text-[9px] px-1.5 py-0.5 rounded"
                  style={{
                    background: status?.running ? 'rgba(34,197,94,0.12)' : 'rgba(71,85,105,0.2)',
                    color: status?.running ? '#4ade80' : '#475569',
                    border: status?.running ? '1px solid rgba(34,197,94,0.25)' : '1px solid #1e293b',
                  }}
                >
                  {status?.running ? 'running' : 'paused'}
                </span>
              </div>
              <div className="grid grid-cols-2 gap-x-3 gap-y-0.5">
                <div className="flex justify-between">
                  <span className="text-[10px]" style={{ color: '#475569' }}>t</span>
                  <span className="text-[10px] font-mono" style={{ color: '#94a3b8' }}>{(status?.sim_time ?? 0).toFixed(4)}s</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-[10px]" style={{ color: '#475569' }}>rt</span>
                  <span className="text-[10px] font-mono" style={{ color: '#94a3b8' }}>{frame ? `${frame.realtime_factor.toFixed(2)}x` : '-'}</span>
                </div>
                {boardInfo && (
                  <>
                    <div className="flex justify-between">
                      <span className="text-[10px]" style={{ color: '#475569' }}>comp</span>
                      <span className="text-[10px] font-mono" style={{ color: '#64748b' }}>{boardInfo.num_components}</span>
                    </div>
                    <div className="flex justify-between">
                      <span className="text-[10px]" style={{ color: '#475569' }}>nets</span>
                      <span className="text-[10px] font-mono" style={{ color: '#64748b' }}>{boardInfo.num_nets}</span>
                    </div>
                  </>
                )}
                {status?.options?.integration && (
                  <div className="col-span-2 flex justify-between">
                    <span className="text-[10px]" style={{ color: '#475569' }}>solver</span>
                    <span className="text-[10px] font-mono" style={{ color: '#64748b' }}>{status.options.integration}</span>
                  </div>
                )}
              </div>
            </div>

            {/* Tab content */}
            <div className="flex-1 overflow-y-auto">
              {sidebarTab === 'nets' && (
                <div className="p-2">
                  <NetPanel
                    frame={frame}
                    selectedNet={selectedNet}
                    onSelectNet={setSelectedNet}
                  />
                </div>
              )}

              {sidebarTab === 'probes' && (
                <div className="p-2">
                  <ProbeScopePanel
                    boardInfo={boardInfo}
                    frame={frame}
                    probes={probes}
                    onAddProbe={handleAddProbe}
                    onRemoveProbe={handleRemoveProbe}
                    send={send}
                  />
                </div>
              )}

              {sidebarTab === 'serial' && (
                <div className="h-full" style={{ minHeight: 300 }}>
                  <SerialConsole
                    mcus={mcus}
                    frames={frameHistory.current}
                    send={send}
                  />
                </div>
              )}

              {sidebarTab === 'inputs' && (
                <InputSourcesPanel
                  boardInfo={boardInfo}
                  frame={frame}
                  send={send}
                />
              )}

              {sidebarTab === 'faults' && (
                <div className="p-2">
                  <FaultPanel
                    faults={faultLog}
                    onClear={clearFaults}
                    onFaultComponentSelect={setSelectedFaultRef}
                    selectedFaultRef={selectedFaultRef}
                  />
                </div>
              )}

              {sidebarTab === 'power' && (
                <div className="p-2">
                  <PowerPanel
                    boardInfo={boardInfo}
                    frame={frame}
                    send={send}
                  />
                </div>
              )}

              {sidebarTab === 'controls' && (
                <SolverControlsPanel
                  controls={status?.options ?? null}
                  send={send}
                />
              )}

              {/* Empty-state hint when the active tab has no data yet */}
              {(sidebarTab === 'faults' && faultCount === 0) && (
                <div className="px-4 py-6 flex flex-col items-center gap-2">
                  <div className="text-[10px] font-mono text-center" style={{ color: '#1e293b' }}>no faults detected</div>
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      {/* Bottom status bar */}
      <div
        className="flex items-center gap-3 px-3 shrink-0 text-[10px] overflow-hidden"
        style={{
          minHeight: 26,
          height: 26,
          background: '#050d1a',
          borderTop: '1px solid #1e293b',
          fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
          flexWrap: 'nowrap',
        }}
      >
        {/* Running indicator */}
        <div className="flex items-center gap-1.5">
          <div
            className="w-1.5 h-1.5 rounded-full"
            style={{
              background: status?.running ? '#22c55e' : '#475569',
              boxShadow: status?.running ? '0 0 4px #22c55e80' : 'none',
              animation: status?.running ? 'pulse 1s ease-in-out infinite' : 'none',
            }}
          />
          <span style={{ color: status?.running ? '#4ade80' : '#475569' }}>
            {status?.running ? 'running' : 'paused'}
          </span>
        </div>

        <span style={{ color: '#1e293b' }}>|</span>

        {/* Sim time */}
        <span style={{ color: '#334155' }}>
          sim: <span style={{ color: '#64748b' }}>{(status?.sim_time ?? 0).toFixed(4)}s</span>
        </span>

        {/* Realtime factor */}
        {frame && (
          <>
            <span style={{ color: '#1e293b' }}>|</span>
            <span style={{ color: '#334155' }}>
              rt: <span style={{ color: '#64748b' }}>{frame.realtime_factor.toFixed(2)}x</span>
            </span>
          </>
        )}

        {/* Net voltages count */}
        {frame && (
          <>
            <span style={{ color: '#1e293b' }}>|</span>
            <span style={{ color: '#334155' }}>
              nets: <span style={{ color: '#64748b' }}>{Object.keys(frame.net_voltages).length}</span>
            </span>
          </>
        )}

        {/* Probes */}
        {probes.length > 0 && (
          <>
            <span style={{ color: '#1e293b' }}>|</span>
            <span className="overflow-hidden" style={{ color: '#334155', maxWidth: 160, whiteSpace: 'nowrap', textOverflow: 'ellipsis' }}>
              probes: <span style={{ color: '#e08a4e' }}>{probes.join(', ')}</span>
            </span>
          </>
        )}

        {/* Fault indicator */}
        {faultCount > 0 && (
          <>
            <span style={{ color: '#1e293b' }}>|</span>
            <span style={{ color: '#f87171' }}>
              FAULTS: {faultCount}
            </span>
          </>
        )}

        <div className="flex-1" />

        {/* Board info */}
        {boardInfo && (
          <span style={{ color: '#334155' }}>
            {boardInfo.num_components} comp · {boardInfo.num_nets} nets
          </span>
        )}
      </div>
    </div>
  )
}
