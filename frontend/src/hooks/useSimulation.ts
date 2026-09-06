import { useState, useEffect, useRef, useCallback, useContext } from 'react'
import type { SimFrame, BoardInfoMsg, StatusMsg, ProbeDataMsg, BacklogMsg, ClientMessage, ActionResultMsg } from '../types/protocol'
import { SimSourceContext } from '../demo/simSource'
import { openLiveSocket } from '../lib/api'
import type { LiveSocket } from '../lib/api'

export interface SimulationState {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  status: StatusMsg | null
  probeData: ProbeDataMsg[]
  /** The server's last Error for this session (a dead analog solve, an engine
   *  crash, a replaced session), for a visible banner; null while healthy.
   *  Sticky until a Reset is sent or the session recovers (a new SimFrame
   *  after a plain informational error). Optional so replay/demo sources
   *  without an error channel need not provide it. */
  serverError?: string | null
  /** Correlated receipts for explicit live mutations. Bounded to the current
   * connection and ordered exactly as the engine accepted/refused them. */
  actionResults?: ActionResultMsg[]
  /** The session's server-held history (fault log, probe set), replayed once
   *  per (re)connect so a reload rejoins with everything that already fired.
   *  A fresh object per connect; absent on sources without one (the demo). */
  backlog?: BacklogMsg | null
  send: (msg: ClientMessage) => void
  /** Present only when the source is a recorded-session replay (the demo):
   *  the replay's own transport, for the scrub UI and read-only affordances. */
  replay?: ReplayTransport
}

/** What a replay source exposes beyond the live wire: a finite timeline. */
export interface ReplayTransport {
  /** Recording length on the sim-time axis, seconds. */
  duration: number
  /** Current playhead position, seconds. */
  position: number
  /** Jump the playhead (state is rebuilt from the recording up to `t`). */
  seek: (t: number) => void
}

/** The SimSource seam: live WebSocket by default; the demo build provides a
 *  replay hook via SimSourceContext. The resolved hook's identity is constant
 *  per mount (see simSource.ts), so the hook call order is stable. */
export function useSimulation(): SimulationState {
  const override = useContext(SimSourceContext)
  const impl = override ?? useLiveSimulation
  return impl()
}

/** Today's path, unchanged: the live session on /ws. */
function useLiveSimulation(): SimulationState {
  const [connected, setConnected] = useState(false)
  const [boardInfo, setBoardInfo] = useState<BoardInfoMsg | null>(null)
  const [frame, setFrame] = useState<SimFrame | null>(null)
  const [status, setStatus] = useState<StatusMsg | null>(null)
  const [probeData, setProbeData] = useState<ProbeDataMsg[]>([])
  const [backlog, setBacklog] = useState<BacklogMsg | null>(null)
  const [serverError, setServerError] = useState<string | null>(null)
  const [actionResults, setActionResults] = useState<ActionResultMsg[]>([])
  const wsRef = useRef<LiveSocket | null>(null)

  // Coalesce high-rate messages to one React commit per animation frame. At
  // play speed the server emits SimFrame + Status 30x/s; pushing each into
  // state re-rendered the whole app twice per server tick, and when the tab
  // fell behind the queue only grew. Latest-wins per rAF is the honest rate.
  const pendingFrame = useRef<SimFrame | null>(null)
  const pendingStatus = useRef<StatusMsg | null>(null)
  const flushScheduled = useRef(false)
  const scheduleFlush = useCallback(() => {
    if (flushScheduled.current) return
    flushScheduled.current = true
    requestAnimationFrame(() => {
      flushScheduled.current = false
      if (pendingFrame.current) { setFrame(pendingFrame.current); pendingFrame.current = null }
      if (pendingStatus.current) { setStatus(pendingStatus.current); pendingStatus.current = null }
    })
  }, [])

  // Deliberately NO optimistic clear on Reset: the server broadcasts its
  // refreshed backlog when it actually processes one, and that is what lifts
  // the banner. Clearing on send would hide the only explanation of a session
  // whose loop is dead and can never process the Reset.
  const send = useCallback((msg: ClientMessage) => wsRef.current?.send(msg), [])

  useEffect(() => {
    const ws = openLiveSocket({
      // The server sends BoardInfo on connect; no GetBoardInfo needed.
      onOpen: () => setConnected(true),
      onMessage: msg => {
        switch (msg.type) {
          case 'BoardInfo': setBoardInfo(msg); break
          // Always a fresh object (even when empty) so the consumer's
          // seed-from-backlog effect re-fires per (re)connect.
          case 'Backlog':
            setBacklog({ ...msg })
            // The replayed backlog is authoritative for this (re)connect: a
            // terminal failure it carries is shown, and its ABSENCE clears a
            // stale banner from a previous session (a healthy replacement
            // must not wear its predecessor's death notice).
            setServerError(msg.fatal ?? null)
            break
          case 'SimFrame':
            pendingFrame.current = msg
            scheduleFlush()
            // A frame means the session is stepping again: any earlier stop
            // reason no longer describes the present. (A dead solve emits its
            // Error AFTER its last frame and then stops framing, so the
            // banner set by that Error is never cleared by this.)
            setServerError(prev => (prev === null ? prev : null))
            break
          case 'Status': pendingStatus.current = msg; scheduleFlush(); break
          case 'ProbeData':
            setProbeData(prev => {
              const filtered = prev.filter(p => p.net !== msg.net)
              return [...filtered, msg]
            })
            break
          case 'Error':
            console.warn('[hauksbee] server error:', msg.message)
            // Surface it: a session-stopping failure the user only ever sees
            // in the devtools console is not an honest abort.
            setServerError(msg.message)
            break
          case 'ActionResult':
            setActionResults(previous => [...previous.slice(-49), msg])
            break
        }
      },
      onClose: () => {
        // Drop anything queued for the rAF flush so a pending frame cannot
        // resurrect state after the disconnect clears it.
        pendingFrame.current = null
        pendingStatus.current = null
        setConnected(false)
        setBoardInfo(null)
        setFrame(null)
        setStatus(null)
        setActionResults([])
      },
    })
    wsRef.current = ws
    return () => ws.close()
  }, [scheduleFlush])

  return { connected, boardInfo, frame, status, probeData, backlog, serverError, actionResults, send }
}
