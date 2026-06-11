import type { SimFrame } from '../types/protocol'
import type { ClientMessage } from '../types/protocol'

interface ControlBarProps {
  connected: boolean
  frame: SimFrame | null
  onPlay: () => void
  onPause: () => void
  boardName: string
  boards: { id: string; label: string }[]
  selectedBoard: string
  onBoardChange: (id: string) => void
  send: (msg: ClientMessage) => void
}

export function ControlBar({
  connected, frame, onPlay, onPause, boardName,
  boards, selectedBoard, onBoardChange,
}: ControlBarProps) {
  const running = frame?.running ?? false
  const time = frame ? `${frame.time_ms.toFixed(1)} ms` : '--'
  const speed = frame ? `${frame.speed.toFixed(1)}x` : '--'

  return (
    <div
      className="flex items-center gap-3 px-3 py-2 shrink-0"
      style={{
        background: '#0f172a',
        borderBottom: '1px solid #1e293b',
        height: 44,
      }}
    >
      {/* Logo */}
      <div className="font-bold tracking-widest text-sm select-none" style={{ color: '#3b82f6', letterSpacing: '0.15em' }}>
        GALVANI
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: '#1e293b' }} />

      {/* Board picker */}
      <select
        value={selectedBoard}
        onChange={e => onBoardChange(e.target.value)}
        className="text-[11px] px-2 py-1 rounded outline-none"
        style={{
          background: '#1e293b',
          color: '#cbd5e1',
          border: '1px solid #334155',
          cursor: 'pointer',
        }}
      >
        {boards.map(b => (
          <option key={b.id} value={b.id}>{b.label}</option>
        ))}
      </select>

      <div className="w-px h-5 shrink-0" style={{ background: '#1e293b' }} />

      {/* Play / Pause */}
      <button
        onClick={running ? onPause : onPlay}
        className="flex items-center gap-1.5 px-3 py-1 rounded text-[11px] font-bold transition-opacity hover:opacity-80"
        style={{
          background: running ? '#1d4ed8' : '#065f46',
          color: '#fff',
        }}
      >
        {running ? '⏸ Pause' : '▶ Play'}
      </button>

      {/* Sim stats */}
      <div className="flex items-center gap-3 text-[10px]" style={{ color: '#64748b' }}>
        <span>t = <span style={{ color: '#94a3b8' }}>{time}</span></span>
        <span>speed = <span style={{ color: '#94a3b8' }}>{speed}</span></span>
        {frame && <span>step <span style={{ color: '#94a3b8' }}>{frame.timestep}</span></span>}
      </div>

      <div className="flex-1" />

      {/* Connection status */}
      <div className="flex items-center gap-2 text-[10px]">
        <div
          className="w-2 h-2 rounded-full"
          style={{
            background: connected ? '#22c55e' : '#ef4444',
            boxShadow: connected ? '0 0 6px #22c55e' : 'none',
          }}
        />
        <span style={{ color: connected ? '#86efac' : '#fca5a5' }}>
          {connected ? 'connected' : 'offline'}
        </span>
        <span style={{ color: '#334155' }}>· mock-server :3002</span>
      </div>

      {/* Board name dim */}
      <span className="text-[10px]" style={{ color: '#334155' }}>{boardName}</span>
    </div>
  )
}
