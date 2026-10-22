import { useState, useEffect, useRef, useCallback } from 'react'
import type { SerialLine } from './sim/sim-export'
import { AssertThis } from './checks/CheckThis'
import type { Observation } from '../lib/check-catalog'

// The serial console for the live dock: one terminal per MCU (tabs when
// several), stamped in SIM time (what a reviewer correlates with the scope),
// with what the analysis's static co-sim printed one click above the live
// output, and a send line wired to the MCU's UART. The transcript is owned by
// SimView, so the export writes exactly what is shown here. A printed line can be
// picked (click it) and turned into a UART check: "Assert this" holds the
// firmware to that line, "Assert what I expected" leaves the text to the user.

interface SerialConsoleProps {
  mcus: [string, string][]
  lines: SerialLine[]
  /** What the analysis's static co-sim printed before this session. */
  analysis?: { seconds: number; uart: string } | null
  onSend: (mcu: string, text: string) => void
  onClear: (mcu: string) => void
  /** Replay sources cannot take UART input. */
  readOnly?: boolean
}

function McuTerminal({ mcu, lines, analysis, onSend, onClear, readOnly, scoped }: {
  /** Several MCUs share this board: a check made here names this one. */
  scoped?: boolean
  mcu: [string, string]
  lines: SerialLine[]
  analysis?: { seconds: number; uart: string } | null
  onSend: (mcu: string, text: string) => void
  onClear: (mcu: string) => void
  readOnly?: boolean
}) {
  const [mcuRef, mcuBackend] = mcu
  const [input, setInput] = useState('')
  const [showAnalysis, setShowAnalysis] = useState(false)
  const scrollRef = useRef<HTMLDivElement>(null)
  const autoScroll = useRef(true)
  const mine = lines.filter(l => l.mcu === mcuRef)
  // The line "Assert this" holds the firmware to: the one clicked, else the
  // first the firmware printed (the catalogue's own starting point).
  const [picked, setPicked] = useState<number | null>(null)
  const printed = mine.map((l, i) => ({ l, i })).filter(({ l }) => l.dir === 'rx' && l.text.trim() !== '')
  const chosen = printed.find(({ i }) => i === picked) ?? printed[0]
  const observation = (): Observation => ({
    kind: 'uart', line: chosen?.l.text ?? '', mcu: scoped ? mcuRef : undefined,
    transcript: printed.map(({ l }) => l.text).join('\n'),
  })

  useEffect(() => {
    if (autoScroll.current && scrollRef.current) scrollRef.current.scrollTop = scrollRef.current.scrollHeight
  }, [mine.length])

  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    autoScroll.current = el.scrollHeight - el.scrollTop - el.clientHeight < 20
  }, [])

  const handleSubmit = useCallback((e: React.FormEvent) => {
    e.preventDefault()
    if (!input) return
    onSend(mcuRef, input)
    setInput('')
  }, [input, mcuRef, onSend])

  const analysisLines = analysis?.uart ? analysis.uart.replace(/\n$/, '').split('\n').length : 0

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center justify-between px-3 py-1.5 shrink-0" style={{ borderBottom: '1px solid var(--rule)' }}>
        <div className="flex items-center gap-2">
          <span className="text-[11px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{mcuRef}</span>
          {/* The chip and how it is emulated live in the dock header; here only
              what this pane is. */}
          <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }} title={`Emulation backend: ${mcuBackend}`}>serial port</span>
        </div>
        <button onClick={() => onClear(mcuRef)} className="hb-press text-[9px] cursor-pointer"
          style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}>
          clear
        </button>
      </div>

      <div
        ref={scrollRef}
        onScroll={handleScroll}
        data-testid="serial-scrollback"
        className="flex-1 overflow-y-auto p-2 text-[11px] leading-relaxed"
        style={{ background: 'var(--instrument)', fontFamily: 'var(--font-mono)' }}
      >
        {analysisLines > 0 && (
          <div className="mb-1.5" data-testid="serial-analysis">
            <button type="button" onClick={() => setShowAnalysis(v => !v)} aria-expanded={showAnalysis}
              className="hb-press text-[10px] cursor-pointer underline"
              style={{ background: 'none', border: 'none', padding: 0, color: 'var(--term-info)' }}>
              {showAnalysis ? 'hide' : 'show'} what the analysis printed ({analysisLines} line{analysisLines === 1 ? '' : 's'} in {analysis!.seconds} s of static co-sim)
            </button>
            {showAnalysis && (
              <pre className="mt-1 pl-2" style={{ color: 'var(--term-ts)', whiteSpace: 'pre-wrap', wordBreak: 'break-all', borderLeft: '2px solid var(--hairline)', margin: 0 }}>
                {analysis!.uart}
              </pre>
            )}
          </div>
        )}
        {mine.length === 0 && <div style={{ color: 'var(--term-ts)' }}>No serial output yet. It appears here as the firmware prints.</div>}
        {mine.map((entry, i) => (
          <div
            key={i} className="flex gap-2 mb-0.5"
            data-testid="serial-line" data-picked={chosen?.i === i ? 'true' : undefined}
            {...(entry.dir === 'rx' && entry.text.trim() !== '' ? {
              role: 'button', tabIndex: 0, title: 'Pick this line to assert it',
              onClick: () => setPicked(i),
              onKeyDown: (e: React.KeyboardEvent) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); setPicked(i) } },
            } : {})}
            style={chosen?.i === i ? { background: 'var(--copper-tint)', cursor: 'pointer' } : entry.dir === 'rx' ? { cursor: 'pointer' } : undefined}
          >
            <span className="tnum" style={{ color: 'var(--term-ts)', flexShrink: 0 }}>{(entry.t * 1000).toFixed(1)}ms</span>
            <span style={{ color: entry.dir === 'rx' ? 'var(--term-rx)' : 'var(--term-tx)', whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>
              {entry.dir === 'tx' && <span style={{ color: 'var(--term-prompt)' }}>{'> '}</span>}
              {entry.text}
            </span>
          </div>
        ))}
      </div>

      <div className="px-2 py-1.5 shrink-0 overflow-y-auto empty:hidden" style={{ borderTop: '1px solid var(--rule)', maxHeight: '60%' }}>
        <AssertThis observation={observation} compact />
      </div>

      {readOnly ? (
        <div className="px-2 py-1.5 text-[10px] shrink-0"
          style={{ borderTop: '1px solid var(--rule)', color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
          recorded output; sending needs a live engine
        </div>
      ) : (
        <form onSubmit={handleSubmit} className="flex items-center gap-0 shrink-0" style={{ borderTop: '1px solid var(--rule)' }}>
          <span className="px-2 text-[11px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>›</span>
          <input
            value={input}
            onChange={e => setInput(e.target.value)}
            placeholder="Type a line and press Enter to send it to the firmware"
            aria-label={`Send to ${mcuRef}'s UART`}
            className="flex-1 bg-transparent text-[11px] py-1.5 pr-2 outline-none placeholder:opacity-40"
            style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)', border: 'none' }}
            spellCheck={false}
            autoComplete="off"
          />
          <button type="submit" className="hb-press px-2.5 py-1 text-[11px] mr-1.5 rounded-md cursor-pointer font-semibold"
            style={{ color: 'var(--copper-hi)', background: 'var(--copper-tint)', border: '1px solid var(--copper-deep)' }}>
            send
          </button>
        </form>
      )}
    </div>
  )
}

export function SerialConsole({ mcus, lines, analysis, onSend, onClear, readOnly }: SerialConsoleProps) {
  const [activeMcu, setActiveMcu] = useState(0)

  if (mcus.length === 0) {
    return (
      <div className="flex items-center justify-center h-24 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
        No MCU on this board, so no serial port
      </div>
    )
  }

  return (
    <div className="flex flex-col h-full">
      {mcus.length > 1 && (
        <div className="flex shrink-0" style={{ borderBottom: '1px solid var(--rule)' }}>
          {mcus.map(([ref], i) => (
            <button key={ref} onClick={() => setActiveMcu(i)} className="hb-press px-3 py-1.5 text-[10px] cursor-pointer"
              style={{
                color: activeMcu === i ? 'var(--copper-hi)' : 'var(--silk-faint)',
                borderBottom: activeMcu === i ? '2px solid var(--copper)' : '2px solid transparent',
                background: 'transparent', fontFamily: 'var(--font-mono)',
              }}>
              {ref}
            </button>
          ))}
        </div>
      )}
      <div className="flex-1 min-h-0">
        <McuTerminal
          key={mcus[activeMcu]?.[0]}
          mcu={mcus[activeMcu] ?? mcus[0]}
          lines={lines}
          // The analysis's UART is the board's console; it is shown on the
          // first MCU, which is the one the static co-sim ran.
          analysis={activeMcu === 0 ? analysis : null}
          onSend={onSend}
          onClear={onClear}
          readOnly={readOnly}
          scoped={mcus.length > 1}
        />
      </div>
    </div>
  )
}
