import { useState, useCallback } from 'react'
import type { BoardInfoMsg, StatusMsg, ClientMessage } from '../types/protocol'

interface TransportBarProps {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  status: StatusMsg | null
  send: (msg: ClientMessage) => void
}

export function TransportBar({ connected, boardInfo, status, send }: TransportBarProps) {
  const [speedInput, setSpeedInput] = useState(1.0)

  const running = status?.running ?? false
  const simTime = status?.sim_time ?? 0
  const boardName = boardInfo?.name ?? '--'

  const handlePlayPause = useCallback(() => {
    send({ type: running ? 'Pause' : 'Play' })
  }, [running, send])

  const handleStep = useCallback(() => {
    send({ type: 'Step', dt: 0.001 })
  }, [send])

  const handleReset = useCallback(() => {
    send({ type: 'Reset' })
  }, [send])

  const handleSpeedChange = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const v = parseFloat(e.target.value)
    setSpeedInput(v)
    send({ type: 'SetSpeed', factor: v })
  }, [send])

  const formatTime = (t: number) => {
    if (t < 0.001) return `${(t * 1e6).toFixed(1)} µs`
    if (t < 1) return `${(t * 1000).toFixed(2)} ms`
    return `${t.toFixed(3)} s`
  }

  return (
    <div
      className="flex items-center gap-3 px-4 py-0 shrink-0 select-none"
      style={{
        background: '#0a0f1e',
        borderBottom: '1px solid #1e293b',
        height: 44,
      }}
    >
      {/* Logo — electric-blue wordmark with glow */}
      <div
        className="font-bold tracking-widest select-none"
        style={{
          fontSize: 13,
          color: '#60a5fa',
          letterSpacing: '0.25em',
          fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
          textShadow: '0 0 8px rgba(96,165,250,0.8), 0 0 20px rgba(59,130,246,0.4)',
        }}
      >
        GALVANI
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: '#1e293b' }} />

      {/* Board name */}
      <span
        className="text-xs"
        style={{ color: '#64748b', fontFamily: "'JetBrains Mono', monospace" }}
      >
        {boardName}
      </span>

      <div className="w-px h-5 shrink-0" style={{ background: '#1e293b' }} />

      {/* Transport controls */}
      <div className="flex items-center gap-1.5">
        {/* Play/Pause */}
        <button
          onClick={handlePlayPause}
          title={running ? 'Pause (Space)' : 'Play (Space)'}
          className="flex items-center justify-center w-8 h-7 rounded text-xs font-bold transition-all"
          style={{
            background: running ? 'rgba(59,130,246,0.2)' : 'rgba(34,197,94,0.15)',
            border: running ? '1px solid rgba(59,130,246,0.4)' : '1px solid rgba(34,197,94,0.3)',
            color: running ? '#60a5fa' : '#4ade80',
            boxShadow: running ? '0 0 8px rgba(59,130,246,0.2)' : '0 0 8px rgba(34,197,94,0.15)',
          }}
        >
          {running ? '⏸' : '▶'}
        </button>

        {/* Step */}
        <button
          onClick={handleStep}
          title="Step 1ms (N)"
          className="flex items-center justify-center w-8 h-7 rounded text-xs font-bold transition-all hover:opacity-80"
          style={{
            background: 'rgba(148,163,184,0.08)',
            border: '1px solid #1e293b',
            color: '#64748b',
          }}
        >
          ⏭
        </button>

        {/* Reset */}
        <button
          onClick={handleReset}
          title="Reset"
          className="flex items-center justify-center w-8 h-7 rounded text-xs font-bold transition-all hover:opacity-80"
          style={{
            background: 'rgba(148,163,184,0.08)',
            border: '1px solid #1e293b',
            color: '#64748b',
          }}
        >
          ↺
        </button>
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: '#1e293b' }} />

      {/* Speed */}
      <div className="flex items-center gap-2">
        <span className="text-[10px]" style={{ color: '#475569' }}>SPEED</span>
        <input
          type="range"
          min={0.1}
          max={10}
          step={0.1}
          value={speedInput}
          onChange={handleSpeedChange}
          className="w-20 h-1 rounded appearance-none cursor-pointer"
          style={{ accentColor: '#3b82f6' }}
        />
        <span
          className="text-[11px] font-mono w-8 text-right"
          style={{ color: '#94a3b8', fontFamily: "'JetBrains Mono', monospace" }}
        >
          {speedInput.toFixed(1)}x
        </span>
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: '#1e293b' }} />

      {/* Sim time */}
      <div className="flex items-center gap-2 text-[11px]" style={{ fontFamily: "'JetBrains Mono', monospace" }}>
        <span style={{ color: '#475569' }}>t =</span>
        <span style={{ color: '#94a3b8' }}>{formatTime(simTime)}</span>
      </div>

      <div className="flex-1" />

      {/* Connection status */}
      <div className="flex items-center gap-2 text-[10px]">
        <div
          className="w-2 h-2 rounded-full shrink-0"
          style={{
            background: connected ? '#22c55e' : '#ef4444',
            boxShadow: connected ? '0 0 6px #22c55e80' : 'none',
          }}
        />
        <span style={{ color: connected ? '#86efac' : '#fca5a5' }}>
          {connected ? 'connected' : 'offline'}
        </span>
        <span style={{ color: '#334155' }}>· :3001</span>
      </div>
    </div>
  )
}
