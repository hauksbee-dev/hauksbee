import { useState, useEffect, useRef, useCallback } from 'react'
import type { SimFrame, ClientMessage } from '../types/protocol'

// The serial console card: one terminal per MCU (tabs when several), a dark
// instrument scrollback in both themes, and a send line wired to the MCU's
// UART.

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
        style={{ borderBottom: '1px solid var(--rule)' }}
      >
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 rounded-full" style={{ background: 'var(--ok)' }} />
          <span className="text-[11px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
            {mcuRef}
          </span>
          <span className="text-[9px]" style={{ color: 'var(--silk-faint)' }}>{mcuName}</span>
        </div>
        <button
          onClick={clear}
          className="hb-press text-[9px] cursor-pointer"
          style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
        >
          clear
        </button>
      </div>

      {/* Scrollback: an instrument surface, dark in both themes. */}
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex-1 overflow-y-auto p-2 text-[11px] leading-relaxed"
        style={{
          background: 'var(--instrument)',
          fontFamily: 'var(--font-mono)',
        }}
      >
        {log.length === 0 && (
          <div style={{ color: '#31435c' }}>-- no output --</div>
        )}
        {log.map((entry, i) => (
          <div key={i} className="flex gap-2 mb-0.5">
            <span className="tnum" style={{ color: '#31435c', flexShrink: 0 }}>{entry.ts}</span>
            <span
              style={{
                color: entry.kind === 'rx' ? '#4ade80'
                  : entry.kind === 'tx' ? '#8ba0bb'
                  : '#5b6c84',
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-all',
              }}
            >
              {entry.kind === 'tx' && <span style={{ color: '#44546e' }}>{'> '}</span>}
              {entry.text}
            </span>
          </div>
        ))}
      </div>

      {/* Input */}
      <form
        onSubmit={handleSubmit}
        className="flex items-center gap-0 shrink-0"
        style={{ borderTop: '1px solid var(--rule)' }}
      >
        <span className="px-2 text-[11px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>›</span>
        <input
          value={input}
          onChange={e => setInput(e.target.value)}
          placeholder="type here, Enter to send..."
          className="flex-1 bg-transparent text-[11px] py-1.5 pr-2 outline-none placeholder:opacity-40"
          style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)', border: 'none' }}
          spellCheck={false}
          autoComplete="off"
        />
        <button
          type="submit"
          className="hb-press px-2.5 py-1 text-[11px] mr-1.5 rounded-md cursor-pointer font-semibold"
          style={{ color: 'var(--copper-hi)', background: 'var(--copper-tint)', border: '1px solid var(--copper-deep)' }}
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
      <div className="flex items-center justify-center h-24 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
        No MCUs in board info
      </div>
    )
  }

  return (
    <div className="flex flex-col h-full">
      {/* MCU tab switcher (if multiple) */}
      {mcus.length > 1 && (
        <div className="flex shrink-0" style={{ borderBottom: '1px solid var(--rule)' }}>
          {mcus.map(([ref], i) => (
            <button
              key={ref}
              onClick={() => setActiveMcu(i)}
              className="hb-press px-3 py-1.5 text-[10px] cursor-pointer"
              style={{
                color: activeMcu === i ? 'var(--copper-hi)' : 'var(--silk-faint)',
                borderBottom: activeMcu === i ? '2px solid var(--copper)' : '2px solid transparent',
                background: 'transparent',
                border: 'none',
                fontFamily: 'var(--font-mono)',
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
