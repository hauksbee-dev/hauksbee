import { useState, useEffect, useRef, useCallback } from 'react'
import type { ServerMessage, SimFrame, BoardInfoMsg, StatusMsg, ProbeDataMsg, ClientMessage } from '../types/protocol'

const WS_URL = `ws://${window.location.hostname}:3001/ws`

export interface SimulationState {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  status: StatusMsg | null
  probeData: ProbeDataMsg[]
  send: (msg: ClientMessage) => void
}

export function useSimulation(): SimulationState {
  const [connected, setConnected] = useState(false)
  const [boardInfo, setBoardInfo] = useState<BoardInfoMsg | null>(null)
  const [frame, setFrame] = useState<SimFrame | null>(null)
  const [status, setStatus] = useState<StatusMsg | null>(null)
  const [probeData, setProbeData] = useState<ProbeDataMsg[]>([])
  const wsRef = useRef<WebSocket | null>(null)

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
          case 'SimFrame': setFrame(msg); break
          case 'Status': setStatus(msg); break
          case 'ProbeData':
            setProbeData(prev => {
              const filtered = prev.filter(p => p.net !== msg.net)
              return [...filtered, msg]
            })
            break
          case 'Error':
            console.warn('[galvani] server error:', msg.message)
            break
        }
      }

      ws.onclose = () => {
        if (!alive) return
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

  return { connected, boardInfo, frame, status, probeData, send }
}
