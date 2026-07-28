import { useState, useCallback } from 'react'
import type { BoardInfoMsg, StatusMsg, ClientMessage } from '../types/protocol'
import type { ReplayTransport } from '../hooks/useSimulation'
import { PlayIcon, PauseIcon, StepIcon, ResetIcon } from './Icons'

// The live sim's transport row: play/pause, step, restart, speed, elapsed sim
// time and the realtime factor, with the connection state at the far end. The
// shell's sidebar owns navigation and the wordmark, so this row is purely the
// instrument's controls. With a replay source the row gains what only a
// finite recording can offer: a scrub slider over the whole timeline.

interface TransportBarProps {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  status: StatusMsg | null
  realtimeFactor: number | null
  send: (msg: ClientMessage) => void
  /** Present when the source is a recorded replay: enables the scrubber and
   *  relabels the connection dot (a loaded file is not a live link). */
  replay?: ReplayTransport
}

export function TransportBar({ connected, boardInfo, status, realtimeFactor, send, replay }: TransportBarProps) {
  const [speedInput, setSpeedInput] = useState(1.0)

  const running = status?.running ?? false
  const simTime = status?.sim_time ?? 0

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

  const quietBtn: React.CSSProperties = {
    background: 'var(--surface-2)',
    border: '1px solid var(--hairline)',
    color: 'var(--silk-dim)',
    cursor: 'pointer',
  }

  return (
    <div
      className="flex items-center gap-3 px-4 shrink-0 select-none"
      style={{
        background: 'var(--surface)',
        borderBottom: '1px solid var(--hairline)',
        height: 46,
      }}
    >
      {/* Transport controls */}
      <div className="flex items-center gap-1.5">
        <button
          onClick={handlePlayPause}
          title={running ? 'Pause (Space)' : 'Play (Space)'}
          aria-label={running ? 'Pause' : 'Play'}
          className="hb-press flex items-center justify-center rounded-lg"
          style={{
            width: 34, height: 30, cursor: 'pointer',
            // running → live green; ready-to-play → copper (the action accent)
            background: running ? 'var(--ok-bg)' : 'var(--copper-tint-strong)',
            border: running ? '1px solid var(--ok-border)' : '1px solid var(--copper-deep)',
            color: running ? 'var(--ok)' : 'var(--copper-hi)',
          }}
        >
          {/* Play triangle sits optically left of center; nudge it right. */}
          {running ? <PauseIcon size={13} /> : <PlayIcon size={13} style={{ marginLeft: 1 }} />}
        </button>

        <button
          onClick={handleStep}
          title="Step 1 ms (N)"
          aria-label="Step one millisecond"
          className="hb-press flex items-center justify-center rounded-lg"
          style={{ ...quietBtn, width: 34, height: 30 }}
        >
          <StepIcon size={13} />
        </button>

        <button
          onClick={handleReset}
          title="Restart the simulation"
          aria-label="Restart the simulation"
          className="hb-press flex items-center justify-center rounded-lg"
          style={{ ...quietBtn, width: 34, height: 30 }}
        >
          <ResetIcon size={13} />
        </button>
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />

      {/* Scrub, replay only: live sim time cannot be dragged backwards. */}
      {replay && replay.duration > 0 && (
        <>
          <div className="flex items-center gap-2 flex-1 min-w-0" style={{ maxWidth: 320 }}>
            <input
              type="range"
              min={0}
              max={replay.duration}
              step={replay.duration / 500}
              value={replay.position}
              onChange={e => replay.seek(parseFloat(e.target.value))}
              aria-label="Scrub the recording"
              data-testid="replay-scrub"
              className="w-full h-1 rounded cursor-pointer"
              style={{ accentColor: 'var(--copper)' }}
            />
            <span
              className="text-[10px] tnum shrink-0"
              style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
            >
              / {replay.duration.toFixed(1)}s
            </span>
          </div>
          <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />
        </>
      )}

      {/* Speed */}
      <div className="flex items-center gap-2">
        <span className="text-[10px] uppercase tracking-wider" style={{ color: 'var(--silk-faint)' }}>speed</span>
        <input
          type="range"
          min={0.1}
          max={10}
          step={0.1}
          value={speedInput}
          onChange={handleSpeedChange}
          aria-label="Simulation speed factor"
          className="w-20 h-1 rounded cursor-pointer"
          style={{ accentColor: 'var(--copper)' }}
        />
        <span
          className="text-[11px] w-9 text-right tnum"
          style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}
        >
          {speedInput.toFixed(1)}x
        </span>
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />

      {/* Elapsed sim time, primary readout */}
      <div className="flex items-baseline gap-1.5" style={{ fontFamily: 'var(--font-mono)' }}>
        <span className="text-[10px] uppercase tracking-wider" style={{ color: 'var(--silk-faint)' }}>t</span>
        <span className="text-[13px] font-semibold tnum" style={{ color: 'var(--silk)' }}>{formatTime(simTime)}</span>
        {realtimeFactor !== null && (
          <span className="text-[11px] tnum" style={{ color: 'var(--silk-faint)' }}>
            · {realtimeFactor.toFixed(2)}x realtime
          </span>
        )}
      </div>

      <div className="flex-1" />

      {/* Board identity + connection */}
      {boardInfo && (
        <span className="text-[11px] truncate" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
          {boardInfo.name}
        </span>
      )}
      <div className="flex items-center gap-1.5 text-[11px]">
        <div
          className={connected ? 'run-dot' : undefined}
          style={{
            width: 8, height: 8, borderRadius: 4,
            background: connected ? 'var(--ok)' : 'var(--err)',
          }}
        />
        <span style={{ color: connected ? 'var(--ok)' : 'var(--err)' }}>
          {connected ? (replay ? 'replay' : 'connected') : 'offline'}
        </span>
      </div>
    </div>
  )
}
