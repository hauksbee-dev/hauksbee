/**
 * FaultPanel.tsx
 *
 * Displays SimFrame.faults (optional future protocol field) with red accent styling.
 * Uses optional chaining throughout so it's a no-op when the field is absent.
 * Shows a quiet empty state if no faults are present.
 */

import { useEffect, useRef, useState } from 'react'
import type { SimFrame } from '../types/protocol'

interface Fault {
  component: string
  fault_kind: string
  value: number
  limit: number
  t: number
}

interface FaultPanelProps {
  frame: SimFrame | null
  /** Callback to highlight a faulted component in 2D/3D */
  onFaultComponentSelect?: (ref: string | null) => void
  selectedFaultRef?: string | null
}

function useFaults(frame: SimFrame | null): Fault[] {
  // Defensive: SimFrame.faults is an optional future field
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  return (frame as any)?.faults ?? []
}

export function FaultPanel({ frame, onFaultComponentSelect, selectedFaultRef }: FaultPanelProps) {
  const faults = useFaults(frame)
  const seenRefs = useRef(new Set<string>())
  const [toasts, setToasts] = useState<{ ref: string; kind: string; id: number }[]>([])
  const toastId = useRef(0)

  // Toast on first occurrence
  useEffect(() => {
    for (const f of faults) {
      if (!seenRefs.current.has(f.component)) {
        seenRefs.current.add(f.component)
        const id = ++toastId.current
        setToasts(prev => [...prev, { ref: f.component, kind: f.fault_kind, id }])
        setTimeout(() => setToasts(prev => prev.filter(t => t.id !== id)), 4000)
      }
    }
  }, [faults])

  return (
    <>
      {/* Fault list panel */}
      <div
        className="flex flex-col gap-1"
        style={{
          background: '#0f172a',
          border: '1px solid #1e293b',
          borderRadius: 8,
          overflow: 'hidden',
        }}
      >
        <div className="px-3 pt-2.5 pb-1 flex items-center gap-2">
          <span
            className="text-[10px] font-bold tracking-wider"
            style={{ color: faults.length > 0 ? '#f87171' : '#64748b' }}
          >
            FAULTS
          </span>
          {faults.length > 0 && (
            <span
              className="text-[9px] font-bold px-1.5 py-0.5 rounded-full"
              style={{ background: '#7f1d1d', color: '#fca5a5' }}
            >
              {faults.length}
            </span>
          )}
          {selectedFaultRef && (
            <button
              onClick={() => onFaultComponentSelect?.(null)}
              className="ml-auto text-[10px] hover:opacity-70"
              style={{ color: '#475569' }}
            >
              clear
            </button>
          )}
        </div>

        {faults.length === 0 ? (
          <div className="px-3 pb-2.5 text-[10px]" style={{ color: '#334155' }}>
            No faults detected
          </div>
        ) : (
          <div className="overflow-y-auto" style={{ maxHeight: 200 }}>
            {faults.map(f => {
              const isSelected = f.component === selectedFaultRef
              return (
                <div
                  key={`${f.component}-${f.fault_kind}`}
                  onClick={() => onFaultComponentSelect?.(isSelected ? null : f.component)}
                  className="flex flex-col gap-0.5 px-3 py-2 cursor-pointer hover:opacity-90"
                  style={{
                    background: isSelected ? '#3b0a0a' : 'rgba(127,29,29,0.12)',
                    borderLeft: isSelected ? '2px solid #f87171' : '2px solid #7f1d1d',
                    marginBottom: 1,
                  }}
                >
                  <div className="flex items-center justify-between">
                    <span
                      className="text-[10px] font-mono font-bold"
                      style={{ color: '#fca5a5', fontFamily: "'JetBrains Mono', monospace" }}
                    >
                      {f.component}
                    </span>
                    <span
                      className="text-[9px] px-1.5 py-0.5 rounded"
                      style={{ background: '#7f1d1d', color: '#fca5a5' }}
                    >
                      {f.fault_kind}
                    </span>
                  </div>
                  <div className="text-[9px]" style={{ color: '#94a3b8' }}>
                    val: {f.value.toFixed(3)} / lim: {f.limit.toFixed(3)} @ {f.t.toFixed(4)}s
                  </div>
                </div>
              )
            })}
          </div>
        )}
      </div>

      {/* Toast notifications */}
      <div className="fixed bottom-8 right-4 flex flex-col gap-2 z-50 pointer-events-none">
        {toasts.map(t => (
          <div
            key={t.id}
            className="px-3 py-2 rounded-lg text-[11px] font-mono animate-pulse"
            style={{
              background: '#7f1d1d',
              color: '#fca5a5',
              border: '1px solid #f87171',
              boxShadow: '0 0 12px rgba(248,113,113,0.4)',
              fontFamily: "'JetBrains Mono', monospace",
            }}
          >
            FAULT: {t.ref} — {t.kind}
          </div>
        ))}
      </div>
    </>
  )
}
