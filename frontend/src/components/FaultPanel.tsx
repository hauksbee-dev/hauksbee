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
          background: faults.length > 0 ? 'rgba(30,0,0,0.95)' : '#0f172a',
          border: faults.length > 0 ? '1px solid #7f1d1d' : '1px solid #1e293b',
          borderRadius: 8,
          overflow: 'hidden',
          boxShadow: faults.length > 0 ? '0 0 16px rgba(248,71,71,0.15), inset 0 0 20px rgba(127,29,29,0.08)' : 'none',
          transition: 'all 0.3s ease',
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
              className="text-[9px] font-bold px-1.5 py-0.5 rounded-full animate-pulse"
              style={{
                background: '#7f1d1d',
                color: '#fca5a5',
                boxShadow: '0 0 6px rgba(248,113,113,0.6)',
              }}
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
                    background: isSelected ? '#4a0a0a' : 'rgba(127,29,29,0.2)',
                    borderLeft: isSelected ? '3px solid #f87171' : '3px solid #dc2626',
                    marginBottom: 1,
                    boxShadow: isSelected ? 'inset 0 0 12px rgba(248,71,71,0.1)' : 'none',
                  }}
                >
                  <div className="flex items-center justify-between">
                    <div className="flex items-center gap-1.5">
                      <span style={{ color: '#f87171', fontSize: 10 }}>⚡</span>
                      <span
                        className="text-[10px] font-mono font-bold"
                        style={{ color: '#fca5a5', fontFamily: "'JetBrains Mono', monospace" }}
                      >
                        {f.component}
                      </span>
                    </div>
                    <span
                      className="text-[9px] px-1.5 py-0.5 rounded font-bold"
                      style={{
                        background: '#991b1b',
                        color: '#fca5a5',
                        border: '1px solid #dc2626',
                      }}
                    >
                      {f.fault_kind}
                    </span>
                  </div>
                  <div className="text-[9px] mt-0.5" style={{ color: '#94a3b8' }}>
                    <span style={{ color: '#f87171' }}>{f.value.toFixed(3)}</span>
                    <span style={{ color: '#64748b' }}> / lim: {f.limit.toFixed(3)} @ {f.t.toFixed(4)}s</span>
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
            className="px-3 py-2 rounded-lg text-[11px] font-mono fault-toast"
            style={{
              background: 'rgba(127,29,29,0.95)',
              color: '#fca5a5',
              border: '1px solid #f87171',
              boxShadow: '0 0 18px rgba(248,113,113,0.5), 0 4px 20px rgba(0,0,0,0.5)',
              fontFamily: "'JetBrains Mono', monospace",
              backdropFilter: 'blur(8px)',
              display: 'flex',
              alignItems: 'center',
              gap: 8,
            }}
          >
            <span style={{ color: '#f87171', fontSize: 14 }}>⚡</span>
            <span>FAULT: <strong style={{ color: '#fca5a5' }}>{t.ref}</strong> — {t.kind}</span>
          </div>
        ))}
      </div>
    </>
  )
}
