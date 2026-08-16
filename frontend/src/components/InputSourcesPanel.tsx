import { useState, useCallback } from 'react'
import type { BoardInfoMsg, SimFrame, ClientMessage } from '../types/protocol'

// Virtual peripherals for the sim rail's "Inputs" card. Scalar inputs are
// listed only when BoardInfo explicitly exposes a SetInput id/range; a net name
// is never a source id. Attached peripherals remain live controls by kind.

interface InputSourcesPanelProps {
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  send: (msg: ClientMessage) => void
}

/** A peripheral that reads as a pushbutton. */
function isButtonKind(kind: string): boolean {
  return /button|switch|btn/i.test(kind)
}

export function InputSourcesPanel({ boardInfo, send }: InputSourcesPanelProps) {
  const inputSources = boardInfo?.input_sources ?? []
  const peripherals = boardInfo?.peripherals ?? []

  const [values, setValues] = useState<Record<string, number>>({})
  const [periphState, setPeriphState] = useState<Record<string, number>>({})

  const handleChange = useCallback((source: string, value: number) => {
    setValues(prev => ({ ...prev, [source]: value }))
    send({ type: 'SetInput', source, value })
  }, [send])

  const setPeripheral = useCallback((id: string, value: number) => {
    setPeriphState(prev => ({ ...prev, [id]: value }))
    send({ type: 'SetPeripheral', id, value })
  }, [send])

  if (inputSources.length === 0 && peripherals.length === 0) {
    return (
      <div className="px-3 py-2.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
        No input sources detected.<br />
        <span style={{ opacity: 0.7 }}>(Add a typed stimulus/control to the co-sim spec; arbitrary net names are never treated as sources.)</span>
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-2 px-3 py-2.5">
      {/* Attached peripherals: buttons press-and-hold; anything else gets a
          0..1 slider (the server interprets the value per kind). */}
      {peripherals.map(p => {
        const val = periphState[p.id] ?? 0
        return isButtonKind(p.kind) ? (
          <div key={p.id} className="flex items-center justify-between gap-2 rounded-lg px-2.5 py-2"
            style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}>
            <div className="min-w-0">
              <div className="text-[11px] font-bold truncate" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
                {p.id}
              </div>
              <div className="text-[9px]" style={{ color: val > 0 ? 'var(--ok)' : 'var(--silk-faint)' }}>
                {val > 0 ? 'pressed' : 'released'}
              </div>
            </div>
            <button
              type="button"
              className="hb-press rounded-full cursor-pointer"
              aria-pressed={val > 0}
              onMouseDown={() => setPeripheral(p.id, 1)}
              onMouseUp={() => setPeripheral(p.id, 0)}
              onMouseLeave={() => { if (val > 0) setPeripheral(p.id, 0) }}
              onTouchStart={e => { e.preventDefault(); setPeripheral(p.id, 1) }}
              onTouchEnd={() => setPeripheral(p.id, 0)}
              title="Press and hold"
              style={{
                width: 34, height: 34,
                background: val > 0 ? 'var(--copper)' : 'var(--surface-3)',
                border: `2px solid ${val > 0 ? 'var(--copper-hi)' : 'var(--hairline)'}`,
                boxShadow: val > 0 ? '0 0 10px var(--copper-glow)' : 'none',
              }}
            />
          </div>
        ) : (
          <div key={p.id} className="rounded-lg px-2.5 py-2"
            style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}>
            <div className="flex items-center justify-between mb-1.5">
              <span className="text-[11px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
                {p.id}
              </span>
              <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>{p.kind}</span>
            </div>
            <input
              type="range"
              min={0}
              max={p.kind === 'stimulus' ? 5 : 1}
              step={p.kind === 'stimulus' ? 0.01 : 0.01}
              value={val}
              onChange={e => setPeripheral(p.id, parseFloat(e.target.value))}
              className="w-full h-1.5 rounded cursor-pointer"
              style={{ accentColor: 'var(--copper)' }}
            />
            {p.kind === 'stimulus' && (
              <div className="text-right text-[10px] mt-1 tnum" style={{ color: 'var(--copper)' }}>
                {val.toFixed(2)} V
              </div>
            )}
          </div>
        )
      })}

      {inputSources.map(source => {
        const localVal = values[source.id] ?? source.initial
        return (
          <div
            key={source.id}
            className="rounded-lg px-2.5 py-2"
            style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}
          >
            <div className="flex items-center justify-between mb-1.5">
              <span
                className="text-[11px] font-bold"
                style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
              >
                {source.id}
              </span>
              <div className="flex items-center gap-2 tnum">
                <span
                  className="text-[12px] font-bold"
                  style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}
                >
                  {localVal.toFixed(2)} {source.unit}
                </span>
              </div>
            </div>
            <input
              type="range"
              min={source.min}
              max={source.max}
              step={(source.max - source.min) / 500}
              value={localVal}
              onChange={e => handleChange(source.id, parseFloat(e.target.value))}
              className="w-full h-1.5 rounded cursor-pointer"
              style={{ accentColor: 'var(--copper)' }}
            />
            <div className="flex justify-between text-[9px] mt-0.5" style={{ color: 'var(--silk-faint)' }}>
              <span>{source.min} {source.unit}</span><span>{source.max} {source.unit}</span>
            </div>
          </div>
        )
      })}
    </div>
  )
}
