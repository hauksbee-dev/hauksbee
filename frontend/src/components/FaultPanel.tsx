/**
 * FaultPanel.tsx
 *
 * The session's accumulated fault log for the sim rail's "Faults" card (the
 * card header owns the title and count badge). The server drains each fault
 * into exactly one SimFrame, so the OWNER of this panel (SimView) accumulates
 * them; a fault stays listed from the moment it was seen, with its sim
 * timestamp, until the user clears the log. First occurrences also raise a
 * toast so a fault at play speed is never a single-frame blink.
 */

import { useEffect, useRef, useState } from 'react'
import { BoltIcon } from './Icons'
import type { SimFault } from '../types/protocol'

interface FaultPanelProps {
  /** Accumulated fault log (SimView owns the accumulation). `restored`
   *  entries were replayed from the server backlog on a rejoin: history, not
   *  news, so they list without toasting. */
  faults: (SimFault & { restored?: boolean })[]
  /** Clears the accumulated log */
  onClear?: () => void
  /** Callback to highlight a faulted component in 2D/3D */
  onFaultComponentSelect?: (ref: string | null) => void
  selectedFaultRef?: string | null
}

export function FaultPanel({ faults, onClear, onFaultComponentSelect, selectedFaultRef }: FaultPanelProps) {
  const seenRefs = useRef(new Set<string>())
  const [toasts, setToasts] = useState<{ ref: string; kind: string; id: number }[]>([])
  const toastId = useRef(0)

  // Toast on first occurrence (never for backlog-restored history)
  useEffect(() => {
    for (const f of faults) {
      if (!seenRefs.current.has(f.component)) {
        seenRefs.current.add(f.component)
        if (f.restored) continue
        const id = ++toastId.current
        setToasts(prev => [...prev, { ref: f.component, kind: f.kind, id }])
        setTimeout(() => setToasts(prev => prev.filter(t => t.id !== id)), 4000)
      }
    }
  }, [faults])

  // What the list counts, said out loud: conditions vs parts differ whenever
  // one part trips several limits, and the shell chips count parts.
  const partCount = new Set(faults.map(f => f.component)).size

  return (
    <>
      {faults.length === 0 ? (
        <div className="px-3 py-2.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
          No faults detected in what is monitored
        </div>
      ) : (
        <div>
          <div className="px-3 pt-2 flex items-center justify-between gap-3">
            <span className="text-[10px] tnum" data-testid="fault-count-label" style={{ color: 'var(--silk-faint)' }}>
              {faults.length} condition{faults.length === 1 ? '' : 's'} on {partCount} part{partCount === 1 ? '' : 's'}
            </span>
            <div className="flex items-center gap-3">
            {selectedFaultRef && (
              <button
                onClick={() => onFaultComponentSelect?.(null)}
                className="hb-press text-[10px] cursor-pointer"
                style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
              >
                deselect
              </button>
            )}
            {onClear && (
              <button
                onClick={() => { seenRefs.current.clear(); onClear() }}
                className="hb-press text-[10px] cursor-pointer"
                style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
                title="Clear the fault log"
              >
                clear log
              </button>
            )}
            </div>
          </div>
          <div className="overflow-y-auto py-1.5 px-1.5 flex flex-col gap-1" style={{ maxHeight: 220 }}>
            {faults.map(f => {
              const isSelected = f.component === selectedFaultRef
              return (
                <div
                  key={`${f.component}-${f.kind}`}
                  onClick={() => onFaultComponentSelect?.(isSelected ? null : f.component)}
                  className="flex flex-col gap-0.5 px-2.5 py-2 cursor-pointer rounded-md"
                  style={{
                    background: 'var(--err-bg)',
                    border: '1px solid var(--err-border)',
                    borderLeft: isSelected ? '3px solid var(--err)' : '3px solid var(--err-border)',
                  }}
                >
                  <div className="flex items-center justify-between">
                    <div className="flex items-center gap-1.5">
                      <span style={{ color: 'var(--err)', display: 'inline-flex' }}><BoltIcon size={11} /></span>
                      <span
                        className="text-[11px] font-bold"
                        style={{ color: 'var(--err-strong)', fontFamily: 'var(--font-mono)' }}
                      >
                        {f.component}
                      </span>
                      {f.destroyed && (
                        <span className="text-[9px] font-bold uppercase" style={{ color: 'var(--err)' }}>
                          destroyed
                        </span>
                      )}
                    </div>
                    <span
                      className="text-[9px] px-1.5 py-0.5 rounded font-bold"
                      style={{
                        background: 'transparent',
                        color: 'var(--err)',
                        border: '1px solid var(--err-border)',
                      }}
                    >
                      {f.kind}
                    </span>
                  </div>
                  <div className="text-[10px] mt-0.5 tnum" style={{ color: 'var(--silk-dim)' }}>
                    <span style={{ color: 'var(--err)' }}>{f.value.toFixed(3)}</span>
                    <span style={{ color: 'var(--silk-faint)' }}> / lim: {f.limit.toFixed(3)} @ {f.t.toFixed(4)}s</span>
                  </div>
                </div>
              )
            })}
          </div>
        </div>
      )}

      {/* Monitoring-scope honesty: an empty list is only as reassuring as
          what the monitor actually watches, so the scope is stated
          persistently rather than implied. */}
      <div
        className="px-3 pb-2.5 pt-1 text-[10px] leading-relaxed"
        style={{ color: 'var(--silk-faint)', borderTop: '1px solid var(--rule)' }}
      >
        Watching datasheet limits on parts the model library rates: current
        (continuous and surge), voltage, power, reverse bias, junction
        temperature, per-pin drive current, and MCU/logic supply overvoltage.
        Parts with no known ratings are not checked.
      </div>

      {/* Toast notifications. Bottom-LEFT over the board canvas (clear of the
          sidebar and the shortcut hint), never bottom-right: there they
          stacked directly over the FAULTS rail, covering the very rows they
          announce. */}
      <div
        className="fixed flex flex-col gap-2 z-50 pointer-events-none"
        style={{ left: 224, bottom: 64 }}
      >
        {toasts.map(t => (
          <div
            key={t.id}
            className="px-3 py-2 rounded-lg text-[11px] fault-toast"
            style={{
              background: 'var(--surface)',
              color: 'var(--err-strong)',
              border: '1px solid var(--err-border)',
              boxShadow: 'var(--shadow-pop)',
              fontFamily: 'var(--font-mono)',
              display: 'flex',
              alignItems: 'center',
              gap: 8,
            }}
          >
            <span style={{ color: 'var(--err)', display: 'inline-flex' }}><BoltIcon size={14} /></span>
            <span>FAULT: <strong>{t.ref}</strong>, {t.kind}</span>
          </div>
        ))}
      </div>
    </>
  )
}
