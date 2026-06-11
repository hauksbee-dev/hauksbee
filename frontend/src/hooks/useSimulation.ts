import { useState, useEffect, useRef, useCallback } from 'react'
import type { ServerMessage, SimFrame, BoardInfoMsg, ClientMessage } from '../types/protocol'

export interface SimulationState {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  send: (msg: ClientMessage) => void
}

export function useSimulation(wsUrl: string): SimulationState {
  const [connected, setConnected] = useState(false)
  const [boardInfo, setBoardInfo] = useState<BoardInfoMsg | null>(null)
  const [frame, setFrame] = useState<SimFrame | null>(null)
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

      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      ws.onopen = () => {
        if (!alive) { ws.close(); return }
        setConnected(true)
        ws.send(JSON.stringify({ type: 'GetBoardInfo' } satisfies ClientMessage))
      }

      ws.onmessage = (ev: MessageEvent) => {
        if (!alive) return
        let msg: ServerMessage
        try { msg = JSON.parse(ev.data as string) as ServerMessage }
        catch { return }
        switch (msg.type) {
          case 'BoardInfo': setBoardInfo(msg); break
          case 'SimFrame': setFrame(msg); break
        }
      }

      ws.onclose = () => {
        if (!alive) return
        setConnected(false)
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
  }, [wsUrl])

  return { connected, boardInfo, frame, send }
}
