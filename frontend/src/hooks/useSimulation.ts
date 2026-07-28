import { useState, useEffect, useRef, useCallback, useContext } from 'react'
import type { ServerMessage, SimFrame, BoardInfoMsg, StatusMsg, ProbeDataMsg, BacklogMsg, ClientMessage } from '../types/protocol'
import { SimSourceContext } from '../demo/simSource'

// Connect to the same origin that served the page, so the viewer works on any
// `hauksbee run --port <PORT>` (and over https). In `vite dev` the dev server
// proxies `/ws` to the backend (see vite.config.ts), so window.location.host is
// correct there too. Override with VITE_WS_URL only for unusual split setups.
const WS_URL =
  import.meta.env.VITE_WS_URL ??
  `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}/ws`

export interface SimulationState {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  status: StatusMsg | null
  probeData: ProbeDataMsg[]
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
export function useLiveSimulation(): SimulationState {
  const [connected, setConnected] = useState(false)
  const [boardInfo, setBoardInfo] = useState<BoardInfoMsg | null>(null)
  const [frame, setFrame] = useState<SimFrame | null>(null)
  const [status, setStatus] = useState<StatusMsg | null>(null)
  const [probeData, setProbeData] = useState<ProbeDataMsg[]>([])
  const [backlog, setBacklog] = useState<BacklogMsg | null>(null)
  const wsRef = useRef<WebSocket | null>(null)

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

  const send = useCallback((msg: ClientMessage) => {
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg))
    }
  }, [])

  useEffect(() => {
    let alive = true
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null

    function connect() {
      if (!alive) return

      const ws = new WebSocket(WS_URL)
      wsRef.current = ws

      ws.onopen = () => {
        if (!alive) { ws.close(); return }
        setConnected(true)
        // Server sends BoardInfo automatically on connect -- no GetBoardInfo needed
      }

      ws.onmessage = (ev: MessageEvent) => {
        if (!alive) return
        let msg: ServerMessage
        try { msg = JSON.parse(ev.data as string) as ServerMessage }
        catch { return }
        switch (msg.type) {
          case 'BoardInfo': setBoardInfo(msg); break
          // Always a fresh object (even when empty) so the consumer's
          // seed-from-backlog effect re-fires per (re)connect.
          case 'Backlog': setBacklog({ ...msg }); break
          case 'SimFrame': pendingFrame.current = msg; scheduleFlush(); break
          case 'Status': pendingStatus.current = msg; scheduleFlush(); break
          case 'ProbeData':
            setProbeData(prev => {
              const filtered = prev.filter(p => p.net !== msg.net)
              return [...filtered, msg]
            })
            break
          case 'Error':
            console.warn('[hauksbee] server error:', msg.message)
            break
        }
      }

      ws.onclose = () => {
        if (!alive) return
        // Drop anything queued for the rAF flush so a pending frame cannot
        // resurrect state after the disconnect clears it.
        pendingFrame.current = null
        pendingStatus.current = null
        setConnected(false)
        setBoardInfo(null)
        setFrame(null)
        setStatus(null)
        reconnectTimer = setTimeout(connect, 2000)
      }

      ws.onerror = () => {
        ws.close()
      }
    }

    connect()

    return () => {
      alive = false
      if (reconnectTimer) clearTimeout(reconnectTimer)
      wsRef.current?.close()
    }
  }, [])

  return { connected, boardInfo, frame, status, probeData, backlog, send }
}
