import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { BacklogMsg, BoardInfoMsg, ClientMessage, SimFault, SimFrame } from '../types/protocol'
import type { HistoryReview } from '../components/TransportBar'

// The live session's accumulated state: the retained frames the transport can
// walk backwards through, the fault log, and the per-MCU UART activity light.
// All three outlive any single frame, and all three belong to ONE session: a
// replacement clears them together.

/** How many complete frames the transport can walk back through. */
const HISTORY_LEN = 120

interface SimSession {
  /** The frame every display reads: a retained one while reviewing, else live. */
  frame: SimFrame | null
  /** The retained frames, for surfaces that read a window (the serial console). */
  frames: SimFrame[]
  history: HistoryReview
  faultLog: (SimFault & { restored?: boolean })[]
  /** Logged parts that are STILL over a limit right now. */
  activeFaultRefs: Set<string>
  /** Distinct faulted PARTS. */
  faultCount: number
  clearFaults: () => void
  probes: string[]
  addProbe: (net: string) => void
  removeProbe: (net: string) => void
  /** MCUs that saw UART traffic in the last two seconds. */
  uartActive: Set<string>
  /** True once a session has existed, so a dropped socket reads as a loss
   *  rather than a first connect that has not landed yet. */
  everHadSession: boolean
  /** `send`, plus the local resets a Reset implies. */
  send: (msg: ClientMessage) => void
  selectedFaultRef: string | null
  setSelectedFaultRef: (ref: string | null) => void
  /** Bumped when the socket brings back a DIFFERENT session, so a caller can
   *  drop its own per-session state (a floating selection) at the same moment. */
  sessionEpoch: number
}

export function useSimSession({ liveFrame, boardInfo, backlog, send }: {
  liveFrame: SimFrame | null
  boardInfo: BoardInfoMsg | null
  backlog?: BacklogMsg | null
  send: (msg: ClientMessage) => void
}): SimSession {
  // Each retained frame is a COMPLETE state (net voltages, component states,
  // faults, supplies, UART), which is what lets the transport walk backwards
  // through them: the engine cannot un-step a simulation, but the client can
  // re-show one it was already sent.
  const frameHistory = useRef<SimFrame[]>([])
  const [reviewIndex, setReviewIndex] = useState<number | null>(null)
  const [faultLog, setFaultLog] = useState<(SimFault & { restored?: boolean })[]>([])
  const [selectedFaultRef, setSelectedFaultRef] = useState<string | null>(null)
  const [probes, setProbes] = useState<string[]>([])
  const [uartActive, setUartActive] = useState<Set<string>>(new Set())
  const [everHadSession, setEverHadSession] = useState(false)
  const [sessionEpoch, setSessionEpoch] = useState(0)

  useEffect(() => {
    if (liveFrame) frameHistory.current = [...frameHistory.current.slice(-(HISTORY_LEN - 1)), liveFrame]
  }, [liveFrame])

  // Persistent fault log. The server drains each fault into exactly ONE frame,
  // so at play speed a fault is visible for a single frame; anything that reads
  // the current frame alone misses it. Accumulate every fault the session has
  // seen (first occurrence per component+kind keeps its timestamp) until the
  // user clears the log or resets the sim.
  useEffect(() => {
    const faults = liveFrame?.faults
    if (!faults || faults.length === 0) return
    setFaultLog(prev => {
      let next: SimFault[] | null = null
      for (const f of faults) {
        const seen = (list: SimFault[]) => list.some(e => e.component === f.component && e.kind === f.kind)
        if (!seen(prev) && !(next && seen(next))) {
          next = next ?? [...prev]
          next.push(f)
        }
      }
      return next ?? prev
    })
  }, [liveFrame])

  // Recent UART activity per MCU (drives the uart chip). A ref-tracked map of
  // last-seen wall times, surfaced as a Set via state on a slow tick.
  const uartLastSeen = useRef<Map<string, number>>(new Map())
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
      setUartActive(prev => (prev.size === next.size && [...prev].every(x => next.has(x)) ? prev : next))
    }, 500)
    return () => clearInterval(t)
  }, [])

  // A session REPLACEMENT reconnects the socket: boardInfo drops to null, then
  // the new session's BoardInfo arrives. The previous session's fault log,
  // frame history, selection and probes are that session's story, not this
  // one's, and carrying them over makes a replaced sim show the old board's
  // faults.
  const hadInfo = useRef(false)
  const infoWasNull = useRef(true)
  useEffect(() => {
    if (!boardInfo) {
      infoWasNull.current = true
      return
    }
    setEverHadSession(true)
    if (infoWasNull.current && hadInfo.current) {
      setFaultLog([])
      setSelectedFaultRef(null)
      setProbes([])
      frameHistory.current = []
      // A review pointing into the retained frames would show the OLD board.
      setReviewIndex(null)
      setSessionEpoch(n => n + 1)
    }
    hadInfo.current = true
    infoWasNull.current = false
  }, [boardInfo])

  // Rejoin restore: the server replays its session history (fault backlog +
  // active probe set) right after BoardInfo on every subscribe, so a
  // mid-session reload shows everything that already fired instead of a clean
  // log over a sim that kept running. Declared AFTER the replacement reset
  // above so a replaced session's backlog seeds the already-cleared log.
  useEffect(() => {
    if (!backlog) return
    // The backlog is AUTHORITATIVE per delivery, not additive. The server
    // records every fault before broadcasting it, so replacing loses nothing a
    // merge would have kept, while a merge cannot express the one thing an
    // EMPTY backlog means: a Reset cleared the fault story, and a tab that did
    // not send that Reset must drop its pre-reset faults too.
    const heldFaults = backlog.faults ?? []
    setFaultLog(prev => (prev.length === 0 && heldFaults.length === 0
      ? prev
      : heldFaults.map(f => ({ ...f, restored: true }))))
    const heldProbes = backlog.probes ?? []
    setProbes(prev => (prev.length === heldProbes.length && heldProbes.every(p => prev.includes(p))
      ? prev
      : [...heldProbes]))
  }, [backlog])

  // EVERYTHING that displays sim state reads this, so pointing it at a retained
  // frame puts the whole instrument (board, scope, nets, faults) at that
  // instant at once. The accumulating effects above deliberately keep reading
  // `liveFrame`: re-playing an old frame through them would log its faults and
  // UART bytes a second time.
  const frame = reviewIndex !== null ? (frameHistory.current[reviewIndex] ?? liveFrame) : liveFrame

  const stepBack = useCallback(() => {
    // Reviewing a past frame while the sim runs on would be unreadable, and the
    // buffer would slide under the cursor. Stepping back stops the clock.
    send({ type: 'Pause' })
    setReviewIndex(prev => {
      const len = frameHistory.current.length
      if (len === 0) return prev
      return Math.max(0, (prev ?? len - 1) - 1)
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

  const resume = useCallback(() => setReviewIndex(null), [])

  const history = useMemo<HistoryReview>(() => ({
    retained: frameHistory.current.length,
    index: reviewIndex,
    t: reviewIndex !== null ? (frameHistory.current[reviewIndex]?.t ?? null) : null,
    canStepBack: frameHistory.current.length > 1 && (reviewIndex === null || reviewIndex > 0),
    stepBack,
    stepForward,
    resume,
    // `liveFrame` is in the deps on purpose though it is not read here: the
    // counts above come out of a REF, so without it the retained total would
    // stay frozen at whatever it was when the review last changed.
  }), [reviewIndex, stepBack, stepForward, resume, liveFrame])

  const clearFaults = useCallback(() => {
    setFaultLog([])
    setSelectedFaultRef(null)
  }, [])

  // Which logged parts are STILL over a limit right now. The stress monitor
  // latches (a fault fires once and does not re-fire), so the log alone cannot
  // tell "happening now" from "happened at t=0.4s". The live signal is the
  // per-component stress fraction on every frame, saturated at 1.0 by the
  // engine, so 1.0 means "at or past its rating right now". A part the frame
  // does not report on is left as it was rather than declared recovered, and a
  // destroyed part never recovers.
  const activeFaultRefs = useMemo(() => {
    const states = frame?.component_states
    const active = new Set<string>()
    for (const f of faultLog) {
      const stress = states?.[f.component]?.stress
      if (f.destroyed || stress === undefined || stress >= 1) active.add(f.component)
    }
    return active
  }, [faultLog, frame])

  const addProbe = useCallback((net: string) => {
    setProbes(prev => (prev.includes(net) ? prev : [...prev, net]))
    send({ type: 'AddProbe', net })
  }, [send])

  const removeProbe = useCallback((net: string) => {
    setProbes(prev => prev.filter(p => p !== net))
    send({ type: 'RemoveProbe', net })
  }, [send])

  // Reset also starts the fault story over: a log of faults from the previous
  // run reads as live faults on the fresh one.
  const sendWrapped = useCallback((msg: ClientMessage) => {
    if (msg.type === 'Reset') clearFaults()
    send(msg)
  }, [clearFaults, send])

  return {
    frame,
    frames: frameHistory.current,
    history,
    faultLog,
    activeFaultRefs,
    faultCount: new Set(faultLog.map(f => f.component)).size,
    clearFaults,
    probes,
    addProbe,
    removeProbe,
    uartActive,
    everHadSession,
    send: sendWrapped,
    selectedFaultRef,
    setSelectedFaultRef,
    sessionEpoch,
  }
}
