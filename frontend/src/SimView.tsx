import { useState, useCallback, useEffect, useRef, useMemo } from 'react'
import { useSimulation } from './hooks/useSimulation'
import { useSimSession } from './hooks/useSimSession'
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
import { RailCard, useRailCards } from './components/sim/RailCard'
import { McuChips } from './components/sim/McuChips'
import { SimStatusBar } from './components/sim/StatusBar'
import {
  BoardPlaceholder, ForeignSessionBanner, ServerErrorBanner, ShortsBanner,
} from './components/sim/Banners'
import {
  CpuIcon, SlidersIcon, PowerIcon, ProbeIcon, BoltIcon, TerminalIcon, LayersIcon,
} from './components/Icons'
import type { ActionResultMsg } from './types/protocol'
import type { ModelCoverageSnapshot, QueuedLiveRegisterMap } from './types/report'
import { envelopesFromHistory, envelopeSource, readNet } from './lib/net-state'
import { Callout } from './components/ui'

interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
  padNet?: string | null
  padNets?: string[]
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
// right rail of instrument cards (MCU, inputs, power, scope, nets, faults,
// serial, solver). Mounting it opens the sim WebSocket (via useSimulation), so
// the shell only mounts it once a live board is actually being served, and
// keeps it mounted (hidden) so the session's fault log and scope survive
// navigation. The accumulated session state lives in hooks/useSimSession.
export default function SimView({
  onQueueCheck, onQueuePeripheral, onQueueSensor, onQueueSupply,
  pendingLiveRegisterMaps = [], onLiveRegisterMapsConsumed, onLiveActionResult,
  onStatus, expectedBoard, sessionMatchesCurrent, onRelaunch, modelCoverage,
}: {
  /** Queue a check into the checks builder from a click on the live board.
   *  Absent on the standalone demo server. */
  onQueueCheck?: (check: { kind: string; net?: string; ref?: string }) => void
  /** Queue a real scenario interaction (not an assertion) for a clicked net. */
  onQueuePeripheral?: (peripheral: { id?: string; kind: 'stimulus' | 'pushbutton' | 'toggle'; net?: string; ref?: string }) => void
  onQueueSensor?: (sensor: { id: string; ref?: string; modelId?: string | null }) => void
  onQueueSupply?: (supply: { net: string; volts?: number }) => void
  pendingLiveRegisterMaps?: QueuedLiveRegisterMap[]
  onLiveRegisterMapsConsumed?: (upToSeq: number) => void
  /** Engine-confirmed receipts, forwarded to the scenario row that originated
   * the request. This is never an optimistic "sent" acknowledgement. */
  onLiveActionResult?: (result: ActionResultMsg) => void
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
  /** Coverage retained from the analysis that launched this session. The live
   *  wire can run standalone, so this is optional; when present the component
   *  card keeps the same model-honesty detail while the board is moving. */
  modelCoverage?: ModelCoverageSnapshot | null
} = {}) {
  const {
    connected, boardInfo, frame: liveFrame, status, send, replay, backlog,
    serverError, actionResults = [],
  } = useSimulation()
  const session = useSimSession({ liveFrame, boardInfo, backlog, send })
  const {
    frame, frames, history, faultLog, activeFaultRefs, faultCount, clearFaults,
    probes, addProbe, removeProbe, uartActive, everHadSession,
    send: sendWrapped, selectedFaultRef, setSelectedFaultRef, sessionEpoch,
  } = session

  const cards = useRailCards()
  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedFp, setSelectedFp] = useState<FootprintInfo | null>(null)
  // A 316 px instrument rail consumes the entire work surface on a phone.
  // Start it collapsed there so Drive it live still lands on the board; the
  // labelled 18 px toggle keeps every instrument one tap away.
  const [railOpen, setRailOpen] = useState(
    () => typeof window === 'undefined' || window.innerWidth >= 640,
  )
  // Expand-to-viewport for the live board. Per-view, not persisted.
  const [boardFullscreen, setBoardFullscreen] = useState(false)
  const liveInteractionSeq = useRef(0)
  const reportedActionResults = useRef(0)

  // The selection belonged to the session that just went away.
  useEffect(() => {
    setSelectedNet(null)
    setSelectedFp(null)
  }, [sessionEpoch])

  const queueAndAttachPeripheral = useCallback((peripheral: {
    kind: 'stimulus' | 'pushbutton' | 'toggle'; net?: string; ref?: string
  }) => {
    if (!peripheral.net) return
    liveInteractionSeq.current += 1
    const stem = peripheral.net.replace(/[^A-Za-z0-9]+/g, '_').replace(/^_+|_+$/g, '').slice(0, 28) || 'NET'
    const prefix = peripheral.kind === 'stimulus' ? 'STIM' : peripheral.kind === 'pushbutton' ? 'BTN' : 'SW'
    const id = `${prefix}_${stem}_${liveInteractionSeq.current}`
    // Keep the replayable experiment and the immediate circuit mutation tied
    // to one stable id. The server refuses any unsupported/unknown net.
    onQueuePeripheral?.({ ...peripheral, id })
    send({
      type: 'AttachPeripheral', id, kind: peripheral.kind, net: peripheral.net,
      to: peripheral.kind === 'stimulus' ? undefined : 'GND',
      offset: peripheral.kind === 'stimulus' ? 0 : undefined,
      bounce_ms: peripheral.kind === 'pushbutton' ? 5 : undefined,
      initial: 0,
    })
  }, [onQueuePeripheral, send])

  const queueAndSetSupply = useCallback((supply: { net: string; volts?: number }) => {
    const volts = supply.volts ?? 3.3
    onQueueSupply?.({ ...supply, volts })
    send({ type: 'SetPowerSupply', net: supply.net, supply: { kind: 'ideal', volts } })
  }, [onQueueSupply, send])

  // The scenario builder owns register-map authoring and validation. Once the
  // user explicitly presses "attach live", consume those exact bytes here on
  // the one WebSocket already owned by the live view. The row stays in the
  // scenario builder, so immediate exploration and deterministic replay never
  // drift into two separately-authored devices.
  useEffect(() => {
    if (!connected || pendingLiveRegisterMaps.length === 0) return
    for (const request of pendingLiveRegisterMaps) {
      send({
        type: 'AttachRegisterMap',
        id: request.id,
        request_id: request.seq,
        spec_toml: request.spec_toml,
        inputs: request.inputs,
        controller: request.controller,
        cs_net: request.cs_net,
      })
    }
    onLiveRegisterMapsConsumed?.(pendingLiveRegisterMaps[pendingLiveRegisterMaps.length - 1].seq)
  }, [connected, onLiveRegisterMapsConsumed, pendingLiveRegisterMaps, send])

  useEffect(() => {
    if (!onLiveActionResult) return
    // A reconnect starts a fresh bounded receipt list.
    if (actionResults.length < reportedActionResults.current) reportedActionResults.current = 0
    for (const result of actionResults.slice(reportedActionResults.current)) onLiveActionResult(result)
    reportedActionResults.current = actionResults.length
  }, [actionResults, onLiveActionResult])

  // Per-net excursion over the retained frames, for the surfaces that name a
  // net. Recomputed per frame over a short window (see net-state.ts): the
  // engine's own intra-chunk extremes would be better and are preferred
  // automatically the moment the wire carries them.
  // Keyed on `liveFrame` alone: `frames` is a ref array whose identity never
  // changes, so it would neither trigger nor prevent a recompute.
  const netEnvelopes = useMemo(() => envelopesFromHistory(frames), [liveFrame, frames])
  const netEnvelopeSource = envelopeSource(liveFrame, netEnvelopes)

  const running = status?.running ?? false
  const sessionBoard = boardInfo?.name ?? null

  // Report status up to the shell (chips + nav badges); only on change.
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
    // The next focused element is not known until after the event settles.
    const onFocusOut = () =>
      requestAnimationFrame(() => setInputFocused(isFormField(document.activeElement)))
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

  // Board URL comes from BoardInfo (sent on connect). No demo fallback: in the
  // shell, SimView mounts only for a launched session.
  const boardUrl = boardInfo?.board_url ?? null
  const mcus = boardInfo?.mcus ?? []
  const hasSupplies = !!boardInfo?.power_supplies && Object.keys(boardInfo.power_supplies).length > 0
  const shorts = boardInfo?.shorts ?? null

  // Faulted refs drive the part glow, and the glow means "over its rating NOW".
  // A part that recovered keeps its row in the log but stops glowing.
  const faultedRefs = activeFaultRefs.size === 0 ? undefined : activeFaultRefs

  // The FAULTS card badge counts what the card lists: fault CONDITIONS
  // (component + kind entries). The header chips / status bar / nav badge count
  // faulted PARTS, and say so; the two numbers legitimately differ.
  const faultBadge = faultLog.length > 0 ? (
    <span
      className="text-[10px] font-bold px-1.5 rounded-full tnum"
      title={`${faultLog.length} fault condition${faultLog.length === 1 ? '' : 's'} logged`}
      style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)', minWidth: 17, textAlign: 'center' }}
    >
      {faultLog.length}
    </span>
  ) : undefined

  // The session was live and the socket has since dropped. Distinct from "not
  // connected yet": the first is a loss to report, the second is a wait. The
  // whole surface (canvas, banner, rail) reads this so it stops presenting the
  // empty cards of a dead session as though they described a real board.
  const sessionLost = everHadSession && !connected

  return (
    <div className="flex flex-col h-full overflow-hidden" style={{ background: 'var(--canvas)' }}>
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

      {sessionMatchesCurrent === false && sessionBoard && (
        <ForeignSessionBanner
          sessionBoard={sessionBoard}
          expectedBoard={expectedBoard}
          onRelaunch={onRelaunch}
        />
      )}
      {serverError && <ServerErrorBanner message={serverError} />}
      {shorts && <ShortsBanner shorts={shorts} />}

      <div className="flex flex-1 overflow-hidden">
        {/* Board canvas (dominant center) */}
        <div
          className="flex-1 relative min-w-0 overflow-hidden"
          style={boardFullscreen
            ? { position: 'fixed', inset: 0, zIndex: 60, background: 'var(--instrument)' }
            : undefined}
        >
          {boardUrl === null ? (
            <BoardPlaceholder sessionLost={sessionLost} expectedBoard={expectedBoard} onRelaunch={onRelaunch} />
          ) : (
            <BoardViewer
              boardFile={boardUrl}
              frame={frame}
              boardInfo={boardInfo}
              selectedNet={selectedNet}
              onFootprintClick={info => { setSelectedFp(info); setSelectedNet(null) }}
              // A trace click replaces a part selection and vice versa: one
              // floating panel at a time.
              onNetClick={net => { setSelectedNet(net); if (net) setSelectedFp(null) }}
              faultedRefs={faultedRefs}
              fullscreen={boardFullscreen}
              onToggleFullscreen={() => setBoardFullscreen(v => !v)}
            />
          )}

          {/* Floating selection card: same language as the report map. */}
          {(selectedFp || selectedNet) && (
            <div className="absolute left-3 z-10 flex items-end pointer-events-none" style={{ top: TOOLBAR_CLEARANCE, bottom: 36 }}>
              <div className="pointer-events-auto" style={{ maxHeight: '100%', display: 'flex' }}>
                <SelectionCard
                  net={selectedFp ? null : selectedNet}
                  // What the net actually is, not just a number: driven, moving,
                  // or unobservable on this backend (see lib/net-state.ts).
                  reading={selectedNet && !selectedFp ? readNet(frame, selectedNet, netEnvelopes) : undefined}
                  component={selectedFp}
                  boundKind={selectedFp ? boardInfo?.component_kinds?.[selectedFp.ref] ?? null : null}
                  modelCoverage={selectedFp
                    ? modelCoverage?.components.find(c => c.reference === selectedFp.ref) ?? null
                    : null}
                  netModels={selectedNet && !selectedFp
                    ? (modelCoverage?.components ?? []).filter(c => c.pins.some(pin => pin.net === selectedNet))
                    : []}
                  onQueueCheck={onQueueCheck}
                  onQueuePeripheral={queueAndAttachPeripheral}
                  onQueueSensor={onQueueSensor}
                  onQueueSupply={queueAndSetSupply}
                  peripheralMode="live-and-scenario"
                  onAddProbe={addProbe}
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
              : 'Space=play/pause · N=step · scroll=zoom · drag=pan · hover=probe'}
          </div>
        </div>

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
            {/* Without this the rail's cards ("No nets loaded", an empty scope)
                read as findings ABOUT the board rather than as the aftermath of
                a lost connection. */}
            {sessionLost && (
              <Callout
                tone="err"
                testId="rail-session-lost"
                title="Session ended"
                className="px-3 py-2.5 text-[12px] shrink-0"
                style={{ color: 'var(--silk)' }}
              >
                These cards are empty because the live link dropped, not because
                the board has nothing on it.
              </Callout>
            )}

            {mcus.length > 0 && (
              <RailCard id="mcu" title="MCU" icon={<CpuIcon size={13} />} cards={cards}>
                <McuChips mcus={mcus} frame={frame} uartActive={uartActive} />
              </RailCard>
            )}

            {/* Input-shaped panels only against a live engine: in a replay the
                knobs would silently do nothing, so instead of disabled controls
                the rail says plainly what a recording is. */}
            {replay ? (
              <RailCard id="recorded" title="Recorded run" icon={<PowerIcon size={13} />} cards={cards}>
                <div className="px-3 py-2.5 text-[11px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
                  Inputs, power rails and solver options were set when this
                  session was captured from the real engine; a recording
                  cannot take new ones. Install hauksbee to turn the knobs on
                  your own boards.
                </div>
              </RailCard>
            ) : (
              <>
                <RailCard id="inputs" title="Inputs" icon={<SlidersIcon size={13} />} cards={cards}>
                  <InputSourcesPanel boardInfo={boardInfo} frame={frame} send={send} />
                </RailCard>

                {hasSupplies && (
                  <RailCard id="power" title="Power rails" icon={<PowerIcon size={13} />} cards={cards}>
                    <PowerPanel boardInfo={boardInfo} frame={frame} send={send} />
                  </RailCard>
                )}
              </>
            )}

            <RailCard id="scope" title="Scope" icon={<ProbeIcon size={13} />} cards={cards}>
              <ProbeScopePanel
                boardInfo={boardInfo}
                frame={frame}
                probes={probes}
                onAddProbe={addProbe}
                onRemoveProbe={removeProbe}
                send={send}
              />
            </RailCard>

            <RailCard id="nets" title="Net voltages" icon={<LayersIcon size={13} />} defaultOpen={false} cards={cards}>
              <NetPanel
                frame={frame}
                selectedNet={selectedNet}
                onSelectNet={setSelectedNet}
                envelopes={netEnvelopes}
                envelopeSource={netEnvelopeSource}
              />
            </RailCard>

            <RailCard id="faults" title="Faults" icon={<BoltIcon size={13} />} badge={faultBadge} cards={cards}>
              <FaultPanel
                faults={faultLog}
                activeRefs={activeFaultRefs}
                onClear={clearFaults}
                onFaultComponentSelect={setSelectedFaultRef}
                selectedFaultRef={selectedFaultRef}
              />
            </RailCard>

            <RailCard id="serial" title="Serial console" icon={<TerminalIcon size={13} />} cards={cards}>
              <div style={{ height: 300 }}>
                <SerialConsole mcus={mcus} frames={frames} send={send} readOnly={!!replay} />
              </div>
            </RailCard>

            {!replay && (
              <RailCard id="solver" title="Solver" icon={<SlidersIcon size={13} />} defaultOpen={false} cards={cards}>
                <SolverControlsPanel controls={status?.options ?? null} send={send} />
              </RailCard>
            )}
          </div>
        )}
      </div>

      <SimStatusBar
        running={running}
        sessionLost={sessionLost}
        simTime={status?.sim_time ?? 0}
        frame={frame}
        replay={!!replay}
        probes={probes}
        faultCount={faultCount}
        boardInfo={boardInfo}
      />
    </div>
  )
}
