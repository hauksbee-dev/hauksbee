import { useState, useCallback } from 'react'
import type { BoardInfoMsg, ClientMessage } from '../types/protocol'
import { NetPicker } from './ProbeScopePanel'

/** The peripheral kinds a live attach offers, in the words the panel shows. */
export type AttachKind = 'pushbutton' | 'toggle' | 'stimulus'
const ATTACH_KINDS: { kind: AttachKind; label: string; hint: string }[] = [
  { kind: 'pushbutton', label: 'Button', hint: 'a momentary switch to GND, press and hold' },
  { kind: 'toggle', label: 'Switch', hint: 'a latching switch to GND' },
  { kind: 'stimulus', label: 'Voltage', hint: 'a driven voltage you set with a slider' },
]

// Virtual peripherals for the sim rail's "Inputs" card. Scalar inputs are
// listed only when BoardInfo explicitly exposes a SetInput id/range; a net name
// is never a source id. Attached peripherals remain live controls by kind.

interface InputSourcesPanelProps {
  boardInfo: BoardInfoMsg | null
  send: (msg: ClientMessage) => void
  /** Attach a peripheral on a net (live, and saved into the checks scenario).
   *  Absent where nothing can be attached (the standalone demo). */
  onAttach?: (kind: AttachKind, net: string) => void
}

/** A peripheral that reads as a pushbutton. */
function isButtonKind(kind: string): boolean {
  return /button|switch|btn/i.test(kind)
}

export function InputSourcesPanel({ boardInfo, send, onAttach }: InputSourcesPanelProps) {
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

  const attach = onAttach && <AttachInput nets={boardInfo?.nets ?? []} onAttach={onAttach} />

  if (inputSources.length === 0 && peripherals.length === 0) {
    return (
      <div className="flex flex-col gap-2 px-3 py-2.5">
        <div className="text-[11px]" style={{ color: 'var(--silk-dim)' }}>
          No buttons or signals are attached yet.
        </div>
        {attach}
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-2 px-3 py-2.5">
      {attach}
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
          <SliderCard key={p.id} id={p.id} min={0} max={p.kind === 'stimulus' ? 5 : 1} step={0.01} value={val}
            onChange={v => setPeripheral(p.id, v)}
            note={<span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>{p.kind}</span>}>
            {p.kind === 'stimulus' && (
              <div className="text-right text-[10px] mt-1 tnum" style={{ color: 'var(--copper)' }}>
                {val.toFixed(2)} V
              </div>
            )}
          </SliderCard>
        )
      })}

      {inputSources.map(source => {
        const localVal = values[source.id] ?? source.initial
        return (
          <SliderCard key={source.id} id={source.id} min={source.min} max={source.max}
            step={(source.max - source.min) / 500} value={localVal} onChange={v => handleChange(source.id, v)}
            note={
              <span className="text-[12px] font-bold tnum" style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}>
                {localVal.toFixed(2)} {source.unit}
              </span>
            }>
            <div className="flex justify-between text-[9px] mt-0.5" style={{ color: 'var(--silk-faint)' }}>
              <span>{source.min} {source.unit}</span><span>{source.max} {source.unit}</span>
            </div>
          </SliderCard>
        )
      })}
    </div>
  )
}

/** Pick what to attach, then the net: the attach happens on the pick. */
function AttachInput({ nets, onAttach }: { nets: string[]; onAttach: (kind: AttachKind, net: string) => void }) {
  const [kind, setKind] = useState<AttachKind>('pushbutton')
  return (
    <div className="flex flex-col gap-1.5" data-testid="attach-input">
      <div className="flex gap-1" role="radiogroup" aria-label="What to attach">
        {ATTACH_KINDS.map(k => (
          <button
            key={k.kind}
            type="button"
            role="radio"
            aria-checked={kind === k.kind}
            title={k.hint}
            onClick={() => setKind(k.kind)}
            className="hb-press text-[10px] px-2 py-0.5 rounded-md cursor-pointer"
            style={{
              background: kind === k.kind ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
              color: kind === k.kind ? 'var(--copper-hi)' : 'var(--silk-faint)',
              border: `1px solid ${kind === k.kind ? 'var(--copper-deep)' : 'var(--hairline)'}`,
            }}
          >
            {k.label}
          </button>
        ))}
      </div>
      <NetPicker testId="attach-input-net" label="Attach input" nets={nets} drop="down" align="left"
        onPick={net => onAttach(kind, net)} />
    </div>
  )
}

/** One slider card, shared by attached peripherals and engine-declared inputs. */
function SliderCard({ id, note, min, max, step, value, onChange, children }: {
  id: string; note: React.ReactNode; min: number; max: number; step: number
  value: number; onChange: (v: number) => void; children?: React.ReactNode
}) {
  return (
    <div className="rounded-lg px-2.5 py-2" style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}>
      <div className="flex items-center justify-between mb-1.5">
        <span className="text-[11px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
          {id}
        </span>
        {note}
      </div>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={e => onChange(parseFloat(e.target.value))}
        className="w-full h-1.5 rounded cursor-pointer"
        style={{ accentColor: 'var(--copper)' }}
      />
      {children}
    </div>
  )
}
