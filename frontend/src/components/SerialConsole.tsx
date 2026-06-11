import { useState, useEffect, useRef, useCallback } from 'react'
import type { SimFrame, ClientMessage } from '../types/protocol'

interface SerialEntry {
  kind: 'rx' | 'tx' | 'info'
  text: string
  ts: string
}

interface SerialConsoleProps {
  mcus: [string, string][]
  frames: SimFrame[]
  send: (msg: ClientMessage) => void
}

function fmtTs(d: Date) {
  const h = d.getHours().toString().padStart(2, '0')
  const m = d.getMinutes().toString().padStart(2, '0')
  const s = d.getSeconds().toString().padStart(2, '0')
  const ms = d.getMilliseconds().toString().padStart(3, '0')
  return `${h}:${m}:${s}.${ms}`
}

function useSerialLog(mcu: string, frames: SimFrame[]) {
  const [log, setLog] = useState<SerialEntry[]>([])
  const seenFramesRef = useRef(0)

  useEffect(() => {
    const newFrames = frames.slice(seenFramesRef.current)
    seenFramesRef.current = frames.length

    const newEntries: SerialEntry[] = []
    for (const f of newFrames) {
      const bytes = f.uart[mcu]
      if (bytes && bytes.length > 0) {
        const text = new TextDecoder().decode(new Uint8Array(bytes))
        newEntries.push({ kind: 'rx', text, ts: fmtTs(new Date()) })
      }
    }

    if (newEntries.length > 0) {
      setLog(prev => [...prev.slice(-500), ...newEntries])
    }
  }, [frames, mcu])

  const addTx = useCallback((text: string) => {
    setLog(prev => [...prev.slice(-500), { kind: 'tx', text, ts: fmtTs(new Date()) }])
  }, [])

  const clear = useCallback(() => setLog([]), [])

  return { log, addTx, clear }
}

function McuTerminal({ mcu, frames, send }: { mcu: [string, string]; frames: SimFrame[]; send: (msg: ClientMessage) => void }) {
  const [mcuRef, mcuName] = mcu
  const { log, addTx, clear } = useSerialLog(mcuRef, frames)
  const [input, setInput] = useState('')
  const scrollRef = useRef<HTMLDivElement>(null)
  const autoScroll = useRef(true)

  // Autoscroll
  useEffect(() => {
    if (autoScroll.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [log])

  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 20
    autoScroll.current = atBottom
  }, [])

  const handleSubmit = useCallback((e: React.FormEvent) => {
    e.preventDefault()
    if (!input) return
    const text = input
    const data = Array.from(new TextEncoder().encode(text + '\n'))
    send({ type: 'Serial', mcu: mcuRef, data })
    addTx(text)
    setInput('')
  }, [input, mcuRef, send, addTx])

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div
        className="flex items-center justify-between px-3 py-1.5 shrink-0"
        style={{ borderBottom: '1px solid #1e293b' }}
      >
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 rounded-full" style={{ background: '#22c55e', boxShadow: '0 0 4px #22c55e80' }} />
          <span className="text-[11px] font-mono font-bold" style={{ color: '#94a3b8', fontFamily: "'JetBrains Mono', monospace" }}>
            {mcuRef}
          </span>
          <span className="text-[9px]" style={{ color: '#334155' }}>{mcuName}</span>
        </div>
        <button
          onClick={clear}
          className="text-[9px] hover:opacity-70"
          style={{ color: '#334155' }}
        >
          clear
        </button>
      </div>

      {/* Scrollback */}
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex-1 overflow-y-auto p-2 font-mono text-[11px] leading-relaxed"
        style={{
          background: '#010b1a',
          fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
        }}
      >
        {log.length === 0 && (
          <div style={{ color: '#1e293b' }}>-- no output --</div>
        )}
        {log.map((entry, i) => (
          <div key={i} className="flex gap-2 mb-0.5">
            <span style={{ color: '#1e3a5f', flexShrink: 0 }}>{entry.ts}</span>
            <span
              style={{
                color: entry.kind === 'rx' ? '#4ade80'
                  : entry.kind === 'tx' ? '#64748b'
                  : '#475569',
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-all',
              }}
            >
              {entry.kind === 'tx' && <span style={{ color: '#334155' }}>{'> '}</span>}
              {entry.text}
            </span>
          </div>
        ))}
      </div>

      {/* Input */}
      <form
        onSubmit={handleSubmit}
        className="flex items-center gap-0 shrink-0"
        style={{ borderTop: '1px solid #1e293b' }}
      >
        <span className="px-2 text-[11px] font-mono" style={{ color: '#334155' }}>›</span>
        <input
          value={input}
          onChange={e => setInput(e.target.value)}
          placeholder="type here, Enter to send..."
          className="flex-1 bg-transparent text-[11px] font-mono py-1.5 pr-2 outline-none placeholder:opacity-30"
          style={{
            color: '#94a3b8',
            fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
          }}
          spellCheck={false}
          autoComplete="off"
        />
        <button
          type="submit"
          className="px-2 py-1.5 text-[10px] mr-1 rounded transition-opacity hover:opacity-80"
          style={{ color: '#3b82f6', background: 'transparent' }}
        >
          send
        </button>
      </form>
    </div>
  )
}

export function SerialConsole({ mcus, frames, send }: SerialConsoleProps) {
  const [activeMcu, setActiveMcu] = useState(0)

  if (mcus.length === 0) {
    return (
      <div className="flex items-center justify-center h-24 text-[11px]" style={{ color: '#334155' }}>
        No MCUs in board info
      </div>
    )
  }

  return (
    <div className="flex flex-col h-full">
      {/* MCU tab switcher (if multiple) */}
      {mcus.length > 1 && (
        <div className="flex shrink-0" style={{ borderBottom: '1px solid #1e293b' }}>
          {mcus.map(([ref], i) => (
            <button
              key={ref}
              onClick={() => setActiveMcu(i)}
              className="px-3 py-1.5 text-[10px] font-mono transition-colors"
              style={{
                color: activeMcu === i ? '#94a3b8' : '#334155',
                borderBottom: activeMcu === i ? '2px solid #3b82f6' : '2px solid transparent',
                background: 'transparent',
                fontFamily: "'JetBrains Mono', monospace",
              }}
            >
              {ref}
            </button>
          ))}
        </div>
      )}

      <div className="flex-1 min-h-0">
        <McuTerminal
          key={mcus[activeMcu]?.[0]}
          mcu={mcus[activeMcu] ?? mcus[0]}
          frames={frames}
          send={send}
        />
      </div>
    </div>
  )
}
