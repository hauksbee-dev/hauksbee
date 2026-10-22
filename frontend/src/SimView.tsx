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
import { Dock, readDockTab, type DockTab } from './components/sim/Dock'
import { seedFrom } from './components/sim/session-seed'
import { digitalNets } from './lib/bench-view'
import {
  provenanceFor, scopeCsv, serialTxt, faultsJson, downloadText, downloadCanvas, provenanceLines,
  type SerialLine,
} from './components/sim/sim-export'
import { ExportMenu, FaultCountChip, RunForButton, SupplyChip, useRunFor } from './components/sim/SessionTools'
import type { ScopeExport } from './components/ProbeScopePanel'
import { faultUnit } from './components/FaultPanel'
import { ResultLinkContext, type ResultLinkHandlers } from './components/report/ResultLinks'
import { vocabulary, type ResultEntity } from './lib/result-links'
import { McuChips } from './components/sim/McuChips'
import { SimStatusBar } from './components/sim/StatusBar'
import { LiveNetCard } from './components/sim/LiveNetCard'
import { deriveRunState, type RunState } from './components/sim/run-state'
import {
  BoardPlaceholder, ForeignSessionBanner, ServerErrorBanner, ShortsBanner,
} from './components/sim/Banners'
import {
  CpuIcon, SlidersIcon, PowerIcon, BoltIcon, TerminalIcon, LayersIcon,
} from './components/Icons'
import type { ActionResultMsg, ClientMessage, PowerSupplyWire } from './types/protocol'
import type { BoardRequest, ModelCoverageSnapshot, QueuedLiveRegisterMap, WebReport } from './types/report'
import { envelopesFromHistory, envelopeSource, readNet } from './lib/net-state'
import { coverageFor, modelsOnNet } from './lib/report-view'
import { peripheralPrefix } from './lib/check-spec'
import { Callout } from './components/ui'
import type { FootprintInfo } from './lib/board-geometry'

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
  /** ready (never played) / paused / running / offline, from the one run-state
   *  derivation, so the header chip cannot say "paused" over a sim that has not
   *  run yet while the transport says it is ready. */
  runState?: RunState
}

// The live-sim view: transport row, the board and its scope as the one
// working surface, and a tabbed dock for the secondary instruments (inputs,
// power, serial, faults, nets, solver), one open at a time. It continues the
// analysis on screen: the checks' supplies and the nets the checks and
// findings name are applied when a session starts (sim/session-seed.ts). Mounting it opens the sim WebSocket (via useSimulation), so
// the shell only mounts it once a live board is actually being served, and
// keeps it mounted (hidden) so the session's fault log and scope survive
// navigation. The accumulated session state lives in hooks/useSimSession.
export default function SimView({
  onQueue,
  pendingLiveRegisterMaps = [], onLiveRegisterMapsConsumed, onLiveActionResult,
  onStatus, expectedBoard, sessionMatchesCurrent, onRelaunch, modelCoverage,
  componentAssertions, report, specToml, engineVersion = null,
}: {
  /** Hand a click on the live board to the checks builder. Absent on the
   *  standalone demo server. */
  onQueue?: (request: BoardRequest) => void
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
  /** Exact assertion support retained from the analysis that launched this
   *  session. Missing data fails closed in the selection card. */
  componentAssertions?: Record<string, string[]> | null
  /** The analysis this session continues (seed, co-sim overlay, provenance). */
  report?: WebReport | null
  /** The checks spec on screen: its supplies and duration seed the session. */
  specToml?: string | null
  engineVersion?: string | null
} = {}) {
  const {
    connected, boardInfo, frame: liveFrame, status: rawStatus, send, replay, backlog,
    serverError, actionResults = [],
  } = useSimulation()
  // One run state, from the same stream as the clock: while sim time advances
  // the sim IS running, whatever a lagging `running` flag last said, so the
  // transport, the dock and the header chip never say "paused" while T moves.
  const status = useAdvancingStatus(rawStatus)
  const session = useSimSession({ liveFrame, boardInfo, backlog, send })
  const {
    frame, frames, history, faultLog, activeFaultRefs, faultCount, clearFaults,
    probes, addProbe, removeProbe, uartActive, everHadSession,
    send: sendWrapped, selectedFaultRef, setSelectedFaultRef, sessionEpoch,
  } = session

  const seed = useMemo(() => seedFrom(report, specToml), [report, specToml])
  const checksSupplies = useMemo(
    () => Object.fromEntries(seed.supplies.map(sp => [sp.net, sp.wire])) as Record<string, PowerSupplyWire>,
    [seed],
  )
  const probeReasons = useMemo(() => Object.fromEntries(seed.probes.map(p => [p.net, p.why])), [seed])
  // What the analysis run saw on the bench: the nets MCU pins drive (drawn as
  // lanes on the scope), and the bus decode and events laid over the same time.
  const bench = report?.cosim?.ran ? report.cosim.bench ?? null : null
  const benchDigitalNets = useMemo(() => digitalNets(bench), [bench])
  // What each rail runs: the launch map, then every change this page sent.
  const [supplyEdits, setSupplyEdits] = useState<Record<string, PowerSupplyWire>>({})
  const [supplyGen, setSupplyGen] = useState(0)
  const runningSupplies = useMemo(
    () => ({ ...(boardInfo?.power_supplies ?? {}), ...supplyEdits }),
    [boardInfo, supplyEdits],
  )
  const [dockTab, setDockTab] = useState(() => readDockTab('serial'))
  const [serial, setSerial] = useState<SerialLine[]>([])
  const decoders = useRef(new Map<string, TextDecoder>())
  const scopeRef = useRef<ScopeExport>(null)
  const runFor = useRunFor(sendWrapped, liveFrame)
  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedFp, setSelectedFp] = useState<FootprintInfo | null>(null)
  // A 316 px instrument rail consumes the entire work surface on a phone.
  // Start it collapsed there so Drive it live still lands on the board; the
  // labelled 18 px toggle keeps every instrument one tap away.
  const [narrow] = useState(() => typeof window !== 'undefined' && window.innerWidth < 640)
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
    setSupplyEdits({})
    setSerial([])
    decoders.current = new Map()
  }, [sessionEpoch])

  const applyChecksSupplies = useCallback(() => {
    const nets = boardInfo?.power_supplies ?? {}
    const applied: Record<string, PowerSupplyWire> = {}
    for (const sp of seed.supplies) {
      if (!(sp.net in nets)) continue
      send({ type: 'SetPowerSupply', net: sp.net, supply: sp.wire })
      applied[sp.net] = sp.wire
    }
    setSupplyEdits(prev => ({ ...prev, ...applied }))
    setSupplyGen(g => g + 1)
  }, [boardInfo, seed, send])

  // Seed a fresh session from the analysis, once: the checks' supplies, and
  // probes on the nets the checks and findings name. Waits for the rejoin
  // backlog (authoritative for probes) and leaves a session that already has
  // probes or faults, or that belongs to another board, as it is.
  const seededEpoch = useRef(-1)
  useEffect(() => {
    if (!boardInfo || !backlog || replay || sessionMatchesCurrent === false) return
    if (seededEpoch.current === sessionEpoch) return
    seededEpoch.current = sessionEpoch
    if ((backlog.probes ?? []).length > 0 || (backlog.faults ?? []).length > 0) return
    applyChecksSupplies()
    for (const p of seed.probes) if (boardInfo.nets.includes(p.net)) addProbe(p.net)
  }, [boardInfo, backlog, replay, sessionMatchesCurrent, sessionEpoch, applyChecksSupplies, seed, addProbe])

  // The serial transcript, stamped in sim time: kept here so the console and
  // its export are the same lines.
  useEffect(() => {
    if (!liveFrame?.uart) return
    const { uart, t } = liveFrame
    // Decoded once, outside the updater: the decoders carry split UTF-8
    // sequences across frames, so they must see each chunk exactly once.
    const chunks = Object.entries(uart).filter(([, bytes]) => bytes.length > 0).map(([mcu, bytes]) => {
      let dec = decoders.current.get(mcu)
      if (!dec) decoders.current.set(mcu, (dec = new TextDecoder()))
      return [mcu, dec.decode(new Uint8Array(bytes), { stream: true })] as const
    })
    if (chunks.length === 0) return
    setSerial(prev => {
      const next = [...prev]
      for (const [mcu, text] of chunks) {
        let lastIdx = -1
        for (let i = next.length - 1; i >= 0; i--) if (next[i].mcu === mcu && next[i].dir === 'rx') { lastIdx = i; break }
        const parts = text.split(/(?<=\n)/)
        for (const part of parts) {
          const last = lastIdx >= 0 ? next[lastIdx] : null
          if (last && !last.text.endsWith('\n')) {
            next[lastIdx] = { ...last, text: last.text + part }
          } else {
            next.push({ t, mcu, dir: 'rx', text: part })
            lastIdx = next.length - 1
          }
        }
      }
      return next.length > 5000 ? next.slice(-5000) : next
    })
  }, [liveFrame])

  const sendSerial = useCallback((mcu: string, text: string) => {
    const data = Array.from(new TextEncoder().encode(`${text}\n`))
    send({ type: 'Serial', mcu, data })
    setSerial(prev => [...prev, { t: status?.sim_time ?? 0, mcu, dir: 'tx', text }])
  }, [send, status])

  // A live click both changes the running circuit NOW and saves the same
  // change into the replayable scenario, under one stable id, so immediate
  // exploration and deterministic replay never drift into two experiments.
  // The server refuses any unsupported/unknown net.
  const queueAndApply = useCallback((request: BoardRequest) => {
    // A check is made in place in the card (the exact assertion, edited there),
    // so what arrives here is already complete: it is queued as written.
    if (request.type === 'peripheral') {
      if (!request.net) return
      liveInteractionSeq.current += 1
      const stem = request.net.replace(/[^A-Za-z0-9]+/g, '_').replace(/^_+|_+$/g, '').slice(0, 28) || 'NET'
      const id = `${peripheralPrefix(request.kind)}_${stem}_${liveInteractionSeq.current}`
      onQueue?.({ ...request, id })
      send({
        type: 'AttachPeripheral', id, kind: request.kind, net: request.net,
        to: request.kind === 'stimulus' ? undefined : 'GND',
        offset: request.kind === 'stimulus' ? 0 : undefined,
        bounce_ms: request.kind === 'pushbutton' ? 5 : undefined,
        initial: 0,
      })
    } else if (request.type === 'supply') {
      const volts = request.volts ?? 3.3
      onQueue?.({ ...request, volts })
      send({ type: 'SetPowerSupply', net: request.net, supply: { kind: 'ideal', volts } })
    } else onQueue?.(request)
  }, [onQueue, send])

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
  // The session replaces `frames` on every retained sample.
  const netEnvelopes = useMemo(() => envelopesFromHistory(frames), [frames])
  const netEnvelopeSource = envelopeSource(liveFrame, netEnvelopes)

  // ONE run state for the whole view. `status.running` is already derived from
  // the clock's own stream (useAdvancingStatus, below): "Run 100 ms" and the
  // step button advance the clock with the loop off, and a status read from the
  // flag alone said "paused" over a clock that was counting up. This only adds
  // the words (ready / paused / offline); the play button, the header chip (via
  // onStatus), the MCU chips and the keyboard all read it.
  const simTime = status?.sim_time ?? 0
  const runState = deriveRunState({ connected, running: status?.running ?? false, simTime })
  const running = runState === 'running'
  const sessionBoard = boardInfo?.name ?? null

  // Pause and Restart end a "Run N ms" in progress, or its next step would
  // carry on after the user said stop; Play hands the clock back to the loop.
  const transportSend = useCallback((msg: ClientMessage) => {
    if (msg.type === 'Pause' || msg.type === 'Reset' || msg.type === 'Play') runFor.cancel()
    sendWrapped(msg)
  }, [runFor, sendWrapped])

  // Report status up to the shell (chips + nav badges); only on change.
  const lastReported = useRef<SimShellStatus | null>(null)
  useEffect(() => {
    if (!onStatus) return
    const cur: SimShellStatus = { running, faults: faultCount, sessionBoard, connected, runState }
    const prev = lastReported.current
    if (!prev || prev.running !== cur.running || prev.faults !== cur.faults
      || prev.sessionBoard !== cur.sessionBoard || prev.connected !== cur.connected
      || prev.runState !== cur.runState) {
      lastReported.current = cur
      onStatus(cur)
    }
  }, [running, faultCount, sessionBoard, connected, runState, onStatus])

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
        transportSend({ type: running ? 'Pause' : 'Play' })
      } else if (e.key === 'n' || e.key === 'N') {
        e.preventDefault()
        send({ type: 'Step', dt: 0.001 })
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [running, send, transportSend])

  // Board URL comes from BoardInfo (sent on connect). No demo fallback: in the
  // shell, SimView mounts only for a launched session.
  const boardUrl = boardInfo?.board_url ?? null
  const mcus = boardInfo?.mcus ?? []
  const hasSupplies = !!boardInfo?.power_supplies && Object.keys(boardInfo.power_supplies).length > 0
  const shorts = boardInfo?.shorts ?? null

  // Faulted refs drive the part glow, and the glow means "over its rating NOW".
  // A part that recovered keeps its row in the log but stops glowing.
  const faultedRefs = activeFaultRefs.size === 0 ? undefined : activeFaultRefs

  // The Faults tab badge counts what the table lists: fault CONDITIONS
  // (component + kind entries). The transport chip / status bar / nav badge
  // count faulted PARTS, and say so; the two numbers legitimately differ.
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
  // connected yet": the first is a loss to report, the second is a wait.
  const sessionLost = everHadSession && !connected

  // Chips anywhere in the view point at (hover) and frame (click) their part
  // or net on the live board, as on the result panel.
  const [pointed, setPointed] = useState<ResultEntity | null>(null)
  const [frameTarget, setFrameTarget] = useState<{ refs: string[]; nets: string[]; seq: number } | null>(null)
  const frameSeq = useRef(0)
  const goTo = useCallback((entity: ResultEntity) => {
    frameSeq.current += 1
    if (entity.kind === 'part') {
      setSelectedNet(null)
      setFrameTarget({ refs: [entity.ref], nets: [], seq: frameSeq.current })
    } else {
      setSelectedFp(null)
      setSelectedNet(entity.net)
      setFrameTarget({ refs: [], nets: [entity.net], seq: frameSeq.current })
    }
  }, [])
  const links = useMemo<ResultLinkHandlers>(() => ({
    vocab: report ? vocabulary(report) : null,
    onPoint: setPointed,
    onGo: goTo,
  }), [report, goTo])
  const pointedRefs = useMemo(() => {
    const refs = pointed?.kind === 'part' ? [pointed.ref] : []
    return selectedFaultRef && !refs.includes(selectedFaultRef) ? [...refs, selectedFaultRef] : refs
  }, [pointed, selectedFaultRef])
  const pointedNets = useMemo(() => (pointed?.kind === 'net' ? [pointed.net] : []), [pointed])
  const selectFault = useCallback((ref: string | null) => {
    setSelectedFaultRef(ref)
    if (ref) goTo({ kind: 'part', ref })
  }, [setSelectedFaultRef, goTo])

  const openTab = useCallback((id: string) => {
    setDockTab(id)
    setRailOpen(true)
  }, [])

  // Every export opens with the same provenance (sim time, supplies, firmware
  // and board hashes, engine and model versions).
  const provenance = () => provenanceFor(report, sessionBoard, engineVersion, status?.sim_time ?? 0, runningSupplies)
  const stem = (sessionBoard || report?.file_name || 'board').replace(/\.[^.]+$/, '').replace(/[^A-Za-z0-9_-]+/g, '_')
  const exportItems = [
    {
      id: 'scope-csv', label: 'Scope traces (.csv)', disabled: probes.length === 0,
      run: () => {
        const sr = scopeRef.current?.series()
        if (sr) downloadText(`${stem}-scope.csv`, scopeCsv(provenance(), sr), 'text/csv')
      },
    },
    {
      id: 'scope-png', label: 'Scope picture (.png)', disabled: probes.length === 0,
      run: () => {
        const c = scopeRef.current?.png(provenanceLines(provenance()))
        if (c) downloadCanvas(c, `${stem}-scope.png`)
      },
    },
    {
      id: 'serial', label: 'Serial output (.txt)', disabled: mcus.length === 0,
      run: () => downloadText(`${stem}-serial.txt`, serialTxt(provenance(), serial, seed.cosim), 'text/plain'),
    },
    {
      id: 'faults', label: 'Fault log (.json)',
      run: () => downloadText(`${stem}-faults.json`, faultsJson(provenance(), faultLog, faultUnit), 'application/json'),
    },
  ]

  const tools = boardInfo && (
    <>
      {!replay && hasSupplies && (
        <SupplyChip running={runningSupplies} checks={seed.supplies.filter(sp => sp.net in (boardInfo.power_supplies ?? {}))}
          onUseChecks={applyChecksSupplies} onOpen={() => openTab('power')} />
      )}
      {!replay && seed.durationS !== null && (
        <RunForButton seconds={seed.durationS} busy={runFor.busy} onRun={() => runFor.start(seed.durationS!)} />
      )}
      <FaultCountChip parts={faultCount} conditions={faultLog.length} onOpen={() => openTab('faults')} />
      <ExportMenu items={exportItems} />
    </>
  )

  const selection = !!(selectedFp || selectedNet)
  // A phone's lower area scrolls; a new selection's card is at its top, so a
  // scroll position left over from the last card must not hide it.
  const lowerRef = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (selection) lowerRef.current?.scrollTo({ top: 0 })
  }, [selection, selectedFp, selectedNet])
  // On a wide screen the card lives in the dock, so a selection opens it. A
  // phone keeps the board in view instead and shows the card as a sheet.
  useEffect(() => {
    if (selection && !narrow && !boardFullscreen) setRailOpen(true)
  }, [selection, narrow, boardFullscreen])
  const staticCardFor = (embedded: boolean) => (
    <SelectionCard
      net={selectedFp ? null : selectedNet}
      docked={!boardFullscreen}
      embedded={embedded}
      // What the net actually is, not just a number: driven, moving, or
      // unobservable on this backend (see lib/net-state.ts).
      reading={selectedNet && !selectedFp ? readNet(frame, selectedNet, netEnvelopes) : undefined}
      component={selectedFp}
      boundKind={selectedFp ? boardInfo?.component_kinds?.[selectedFp.ref] ?? null : null}
      assertionCapabilities={selectedFp ? componentAssertions?.[selectedFp.ref] ?? [] : []}
      modelCoverage={coverageFor(modelCoverage, selectedFp?.ref)}
      netModels={selectedFp ? [] : modelsOnNet(modelCoverage, selectedNet)}
      onQueue={onQueue && queueAndApply}
      peripheralMode="live-and-scenario"
      onAddProbe={addProbe}
      onClose={() => { setSelectedFp(null); setSelectedNet(null) }}
      onPickNet={net => { setSelectedFp(null); setSelectedNet(net) }}
    />
  )
  const staticCard = staticCardFor(false)
  // In the LIVE view a click on a net asks "what is it doing right now?", so the
  // answer is its value, how it moves and a trace. The static check card (the
  // report's suggested assertions, which read as nonsense on a supply rail) is
  // one disclosure away on that card instead of replacing it. A part click keeps
  // the component card, whose model-coverage detail is the same live or not.
  const card = selectedNet && !selectedFp ? (
    <LiveNetCard
      net={selectedNet}
      reading={readNet(frame, selectedNet, netEnvelopes)}
      frame={frame}
      frames={frames}
      probed={probes.includes(selectedNet)}
      onProbe={() => addProbe(selectedNet)}
      onUnprobe={() => removeProbe(selectedNet)}
      onClose={() => { setSelectedFp(null); setSelectedNet(null) }}
      checks={onQueue ? staticCardFor(true) : undefined}
    />
  ) : staticCard

  const tabs: DockTab[] = [
    ...(replay ? [{
      id: 'recorded', label: 'Recorded', icon: <PowerIcon size={12} />,
      body: (
        <div className="px-3 py-2.5 text-[11px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
          Inputs, power rails and solver options were set when this session was
          captured from the real engine; a recording cannot take new ones.
          Install hauksbee to turn the knobs on your own boards.
        </div>
      ),
    }] : [
      {
        id: 'inputs', label: 'Inputs', icon: <SlidersIcon size={12} />,
        body: (
          <InputSourcesPanel boardInfo={boardInfo} send={send}
            onAttach={(kind, net) => queueAndApply({ type: 'peripheral', kind, net })} />
        ),
      },
      ...(hasSupplies ? [{
        id: 'power', label: 'Power', icon: <PowerIcon size={12} />,
        body: (
          <PowerPanel key={supplyGen} boardInfo={boardInfo} frame={frame} send={send} supplies={runningSupplies}
            onSupply={(net, wire) => setSupplyEdits(prev => ({ ...prev, [net]: wire }))}
            checksSupplies={checksSupplies} />
        ),
      }] : []),
    ]),
    ...(mcus.length > 0 ? [{
      id: 'serial', label: 'Serial', icon: <TerminalIcon size={12} />,
      body: (
        <div style={{ height: '100%', minHeight: 260 }}>
          <SerialConsole mcus={mcus} lines={serial} analysis={seed.cosim} onSend={sendSerial}
            onClear={mcu => setSerial(prev => prev.filter(l => l.mcu !== mcu))} readOnly={!!replay} />
        </div>
      ),
    }] : []),
    {
      id: 'faults', label: 'Faults', icon: <BoltIcon size={12} />, badge: faultBadge,
      body: (
        <FaultPanel faults={faultLog} activeRefs={activeFaultRefs} unsupportedReasons={frame?.unsupported_reasons}
          notes={frame?.notes} onClear={clearFaults} onFaultComponentSelect={selectFault} selectedFaultRef={selectedFaultRef} />
      ),
    },
    {
      id: 'nets', label: 'Nets', icon: <LayersIcon size={12} />,
      body: (
        <NetPanel frame={frame} selectedNet={selectedNet} onSelectNet={net => { setSelectedNet(net); if (net) setSelectedFp(null) }}
          envelopes={netEnvelopes} envelopeSource={netEnvelopeSource} />
      ),
    },
    ...(!replay ? [{
      id: 'solver', label: 'Conditions', icon: <CpuIcon size={12} />,
      body: <SolverControlsPanel controls={status?.options ?? null} send={send} />,
    }] : []),
  ]

  return (
    <ResultLinkContext.Provider value={links}>
      <div className="flex flex-col h-full overflow-hidden" style={{ background: 'var(--canvas)' }}>
        <TransportBar
          connected={connected}
          status={status}
          runState={runState}
          // In a replay the capture-time throughput would read as the playback
          // rate; the speed control already owns that number. The rates come
          // off the LIVE frame: a retained frame's rates describe the moment it
          // was computed, and the bar is reporting how the loop is doing now.
          realtimeFactor={replay ? null : liveFrame?.realtime_factor ?? null}
          requestedFactor={replay ? null : liveFrame?.requested_factor ?? null}
          rateLimited={replay ? false : liveFrame?.rate_limited ?? false}
          send={transportSend}
          replay={replay}
          // A recording has a real timeline to scrub; the retained-frame walk
          // is for the live sim, which has none.
          history={replay ? undefined : history}
          tools={tools}
          toolsAlert={faultCount > 0 ? `${faultCount} faulted` : null}
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
          {/* The working surface: the board, and its scope under it. On a
              phone the open dock takes the screen instead of squeezing both. */}
          <div className={`flex-1 min-w-0 flex flex-col overflow-hidden${railOpen ? ' max-sm:hidden' : ''}`}>
            <div
              className="flex-1 relative min-w-0 min-h-0 max-sm:min-h-[200px] overflow-hidden"
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
                  pointedRefs={pointedRefs}
                  pointedNets={pointedNets}
                  frameTarget={frameTarget}
                  fullscreen={boardFullscreen}
                  onToggleFullscreen={() => setBoardFullscreen(v => !v)}
                />
              )}

              {/* Full-screen board: no room for a dock, so the card floats. */}
              {boardFullscreen && selection && (
                <div className="selection-dock absolute left-3 z-10 flex items-end pointer-events-none" style={{ top: TOOLBAR_CLEARANCE, bottom: 36 }}>
                  <div className="pointer-events-auto" style={{ maxHeight: '100%', display: 'flex' }}>{card}</div>
                </div>
              )}

              {/* The server starts every session paused, so the first thing a
                  user sees is a still board. Say what to do about it, once,
                  in words; it is not a second button (Play is the one). */}
              {runState === 'ready' && !replay && boardUrl !== null && !selection && (
                <div
                  data-testid="sim-press-play"
                  className="absolute left-1/2 text-[12px] px-3 py-1.5 rounded-full pointer-events-none text-center"
                  style={{
                    top: TOOLBAR_CLEARANCE,
                    transform: 'translateX(-50%)',
                    maxWidth: 'calc(100% - 1.5rem)',
                    background: 'var(--overlay-hint-bg)',
                    color: 'var(--overlay-chip-text)',
                    border: '1px solid var(--overlay-hint-border)',
                  }}
                >
                  The simulation is waiting. Press Play to start it.
                </div>
              )}
            </div>

            {/* Phone: everything under the board is ONE scroll area, in reading
                order: the selected card, then the scope, then the dock opener.
                Each used to have its own cap (the card 45 %, the scope fixed),
                so a card with its checks open scrolled inside a sliver of the
                screen and its last button sat clipped at the scope's edge.
                On a wide screen the wrapper vanishes (`contents`) and the
                children lay out exactly as before. */}
            <div ref={lowerRef} data-testid="sim-lower" className="max-sm:shrink-0 max-sm:flex max-sm:flex-col max-sm:min-h-0 max-sm:max-h-[58%] max-sm:overflow-y-auto sm:contents">
            {narrow && !railOpen && !boardFullscreen && selection && (
              <div data-testid="selection-dock" className="shrink-0 p-2"
                style={{ borderTop: '1px solid var(--hairline)', background: 'var(--canvas)' }}>
                {card}
              </div>
            )}

            {boardUrl !== null && (
              <div className="shrink-0" data-testid="sim-scope" style={{ borderTop: '1px solid var(--hairline)', background: 'var(--surface)' }}>
                <ProbeScopePanel
                  boardInfo={boardInfo}
                  frame={frame}
                  retained={frames}
                  probes={probes}
                  probeReasons={probeReasons}
                  digitalNets={benchDigitalNets}
                  bus={bench?.bus_log}
                  events={bench?.events}
                  faults={faultLog}
                  onAddProbe={addProbe}
                  onRemoveProbe={removeProbe}
                  exportRef={scopeRef}
                  height={narrow ? 96 : 150}
                />
              </div>
            )}

            {/* Phone: the 18 px edge strip below is unreadable and easy to
                miss, so the dock opens from a labelled control that names
                what is inside it. */}
            {!boardFullscreen && (
              <button
                type="button"
                data-testid="sim-dock-open"
                onClick={() => setRailOpen(true)}
                className="hb-btn hb-press sm:hidden shrink-0 flex items-center justify-between gap-2 px-3 text-[12px] cursor-pointer"
                style={{ minHeight: 40, borderRadius: 0, borderLeft: 'none', borderRight: 'none' }}
              >
                <span className="font-semibold">Instruments</span>
                <span className="truncate text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                  {tabs.map(t => t.label).join(' · ')}
                </span>
              </button>
            )}
            </div>
          </div>

          <button
            onClick={() => setRailOpen(o => !o)}
            className="hb-press max-sm:hidden flex items-center justify-center shrink-0"
            style={{
              width: 18,
              background: 'var(--surface)',
              borderLeft: '1px solid var(--hairline)',
              color: 'var(--silk-faint)',
              fontSize: 10,
              cursor: 'pointer',
            }}
            aria-label={railOpen ? 'Collapse the instrument dock' : 'Expand the instrument dock'}
            title={railOpen ? 'Collapse the instrument dock' : 'Expand the instrument dock'}
          >
            {railOpen ? '›' : '‹'}
          </button>

          {railOpen && (
            <div className="shrink-0 flex flex-col min-h-0 w-[340px] max-sm:w-auto max-sm:flex-1 max-sm:min-w-0"
              style={{ borderLeft: '1px solid var(--hairline)', background: 'var(--canvas)' }}>
              <button
                type="button"
                data-testid="sim-dock-close"
                onClick={() => { setRailOpen(false); setSelectedFp(null); setSelectedNet(null) }}
                className="hb-btn hb-press sm:hidden shrink-0 text-[12px] px-3 cursor-pointer text-left"
                style={{ minHeight: 40, borderRadius: 0, borderLeft: 'none', borderRight: 'none', borderTop: 'none' }}
              >
                ← Back to the board
              </button>
              {/* Without this the dock's panels ("No nets loaded") read as
                  findings ABOUT the board rather than as a lost connection. */}
              {sessionLost && (
                <Callout
                  tone="err"
                  testId="rail-session-lost"
                  title="Session ended"
                  className="m-2 px-3 py-2.5 text-[12px] shrink-0"
                  style={{ color: 'var(--silk)' }}
                >
                  These panels are empty because the live link dropped, not
                  because the board has nothing on it.
                </Callout>
              )}
              {/* A selection is what the dock is about until it is closed: the
                  card (assert on it, probe it, drive it) takes the panel. */}
              {!boardFullscreen && selection ? (
                <div data-testid="selection-dock" className="flex-1 min-h-0 overflow-y-auto p-2">{card}</div>
              ) : (
                <Dock
                  tabs={tabs}
                  active={dockTab}
                  onActivate={setDockTab}
                  header={mcus.length > 0 ? <McuChips mcus={mcus} frame={frame} uartActive={uartActive} running={running} /> : undefined}
                />
              )}
            </div>
          )}
        </div>

        <SimStatusBar sessionLost={sessionLost} inputFocused={inputFocused} />
      </div>
    </ResultLinkContext.Provider>
  )
}

/** The session status with `running` true while `sim_time` is advancing (a
 *  rise within the last 1.5 s), as well as whenever the engine says so. */
function useAdvancingStatus<S extends { running: boolean; sim_time: number }>(status: S | null): S | null {
  const simTime = status?.sim_time ?? 0
  const last = useRef(simTime)
  const [advancing, setAdvancing] = useState(false)
  useEffect(() => {
    const rose = simTime > last.current
    last.current = simTime
    if (!rose) return
    setAdvancing(true)
    const timer = window.setTimeout(() => setAdvancing(false), 1500)
    return () => window.clearTimeout(timer)
  }, [simTime])
  // The engine saying it stopped (running true -> false) ends it at once.
  const wasRunning = useRef(status?.running ?? false)
  useEffect(() => {
    const now = status?.running ?? false
    if (wasRunning.current && !now) setAdvancing(false)
    wasRunning.current = now
  }, [status?.running])
  return useMemo(
    () => (status && advancing && !status.running ? { ...status, running: true } : status),
    [status, advancing],
  )
}
