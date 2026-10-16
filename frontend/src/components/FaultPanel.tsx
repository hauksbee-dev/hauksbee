/**
 * FaultPanel.tsx
 *
 * The session's fault log as a table for the live dock's Faults tab: part
 * (a chip that lights the part on the map), what tripped, the value against
 * the part's rating as a bar, and the sim time. The server drains each fault
 * into exactly one SimFrame, so the OWNER (SimView, via useSimSession)
 * accumulates them; a row stays from the moment it was seen until the log is
 * cleared. The same faults glow on the map and are ruled on the scope at their
 * sim time, so nothing here pops up over the work.
 */

import { BoltIcon } from './Icons'
import type { SimFault } from '../types/protocol'
import { EntityChip } from './report/ResultLinks'

/** The unit each fault kind's value and limit are measured in. */
const FAULT_UNIT: Record<string, string> = {
  overcurrent: 'A',
  surge_current: 'A',
  pin_overcurrent: 'A',
  short: 'A',
  overvoltage: 'V',
  gate_overvoltage: 'V',
  reverse_bias: 'V',
  overpower: 'W',
  overtemperature: '°C',
}

export const faultUnit = (kind: string): string => FAULT_UNIT[kind] ?? ''

/** A sim time a person reads: "412.0 ms", "1.250 s". */
export function simTimeText(t: number): string {
  if (t < 0.001) return `${(t * 1e6).toFixed(1)} µs`
  if (t < 1) return `${(t * 1000).toFixed(1)} ms`
  return `${t.toFixed(3)} s`
}

interface FaultPanelProps {
  /** Accumulated fault log. `restored` entries were replayed from the server
   *  backlog on a rejoin. */
  faults: (SimFault & { restored?: boolean })[]
  /** Components whose condition is STILL over the limit in the current frame;
   *  a ref absent here is shown held. */
  activeRefs?: Set<string>
  /** Why part of the circuit was not simulated (`SimFrame.unsupported_reasons`). */
  unsupportedReasons?: string[]
  /** What the stress verdicts rest on (`SimFrame.notes`). */
  notes?: string[]
  onClear?: () => void
  onFaultComponentSelect?: (ref: string | null) => void
  selectedFaultRef?: string | null
}

const cell: React.CSSProperties = { padding: '5px 6px', borderBottom: '1px solid var(--rule)', verticalAlign: 'middle' }

export function FaultPanel({ faults, activeRefs, unsupportedReasons, notes, onClear, onFaultComponentSelect, selectedFaultRef }: FaultPanelProps) {
  const partCount = new Set(faults.map(f => f.component)).size
  const caveats = [...(unsupportedReasons ?? []).map(r => `Not simulated, so not monitored: ${r}`), ...(notes ?? []).map(n => `Qualified: ${n}`)]

  return (
    <div className="flex flex-col">
      {faults.length === 0 ? (
        <div className="px-3 py-2.5 text-[11px]" style={{ color: 'var(--silk-dim)' }}>
          No faults detected in what is monitored.
        </div>
      ) : (
        <>
          <div className="px-3 pt-2 pb-1 flex items-center justify-between gap-3">
            <span className="text-[10px] tnum" data-testid="fault-count-label" style={{ color: 'var(--silk-dim)' }}>
              {faults.length} condition{faults.length === 1 ? '' : 's'} on {partCount} part{partCount === 1 ? '' : 's'}
            </span>
            <div className="flex items-center gap-3">
              {selectedFaultRef && (
                <button onClick={() => onFaultComponentSelect?.(null)} className="hb-press text-[10px] cursor-pointer"
                  style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}>
                  deselect
                </button>
              )}
              {onClear && (
                <button onClick={onClear} className="hb-press text-[10px] cursor-pointer" title="Clear the fault log"
                  style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}>
                  clear log
                </button>
              )}
            </div>
          </div>
          <table className="w-full text-[11px] tnum" data-testid="fault-table" style={{ borderCollapse: 'collapse' }}>
            <thead>
              <tr className="text-[9px] uppercase tracking-wider" style={{ color: 'var(--silk-faint)', textAlign: 'left' }}>
                <th style={cell}>Part</th>
                <th style={cell}>Value vs rating</th>
                <th style={{ ...cell, textAlign: 'right' }}>Sim time</th>
              </tr>
            </thead>
            <tbody>
              {faults.map(f => {
                const isSelected = f.component === selectedFaultRef
                // A destroyed part never recovers; anything else is "held"
                // once its condition has cleared.
                const active = !activeRefs || f.destroyed || activeRefs.has(f.component)
                const unit = faultUnit(f.kind)
                const ratio = f.limit > 0 ? Math.abs(f.value) / f.limit : 1
                return (
                  <tr
                    key={`${f.component}-${f.kind}`}
                    data-testid={active ? 'fault-row-active' : 'fault-row-held'}
                    onClick={() => onFaultComponentSelect?.(isSelected ? null : f.component)}
                    className="cursor-pointer"
                    style={{
                      background: isSelected ? 'var(--err-bg)' : undefined,
                      boxShadow: isSelected ? 'inset 3px 0 0 var(--err)' : undefined,
                    }}
                  >
                    <td style={cell}>
                      <span className="inline-flex items-center gap-1" onClick={e => e.stopPropagation()}>
                        <span style={{ color: active ? 'var(--err)' : 'var(--silk-faint)', display: 'inline-flex' }}><BoltIcon size={10} /></span>
                        <EntityChip entity={{ kind: 'part', ref: f.component }} />
                      </span>
                      <div className="text-[9px] mt-0.5" style={{ color: active ? 'var(--err)' : 'var(--silk-faint)' }}>
                        {f.kind.replace(/_/g, ' ')}
                        {f.destroyed ? ' · destroyed' : !active ? ' · held' : ''}
                      </div>
                    </td>
                    <td style={cell}>
                      <div className="flex items-baseline gap-1" style={{ fontFamily: 'var(--font-mono)' }}>
                        <span style={{ color: active ? 'var(--err-strong)' : 'var(--silk-dim)' }} title={f.far_past}>
                          {f.far_past ? 'far past' : <>{f.value.toPrecision(3)}{unit && ` ${unit}`}</>}
                        </span>
                        <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>/ {f.limit.toPrecision(3)}{unit && ` ${unit}`}</span>
                      </div>
                      {/* The rating sits at the bar's midpoint; the fill is the value. */}
                      <div className="relative h-1 rounded-full mt-1" style={{ background: 'var(--surface-3)', width: 96 }}
                        title={`${(ratio * 100).toFixed(0)}% of the rating`}>
                        <div className="absolute inset-y-0 left-0 rounded-full"
                          style={{ width: `${Math.min(ratio / 2, 1) * 100}%`, background: active ? 'var(--err)' : 'var(--silk-faint)' }} />
                        <div className="absolute" style={{ left: '50%', top: -2, bottom: -2, width: 1, background: 'var(--silk-dim)' }} />
                      </div>
                    </td>
                    <td style={{ ...cell, textAlign: 'right', fontFamily: 'var(--font-mono)', color: 'var(--silk-dim)' }}>
                      {simTimeText(f.t)}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </>
      )}

      {/* What the monitor watches, and what it could not: one click away,
          because an empty table is only as reassuring as its scope. */}
      <details className="px-3 py-2 text-[10px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
        <summary className="cursor-pointer" data-testid="fault-scope"
          style={{ color: caveats.length > 0 ? 'var(--warn)' : 'var(--silk-faint)' }}>
          What is monitored{caveats.length > 0 ? ` (${caveats.length} caveat${caveats.length === 1 ? '' : 's'})` : ''}
        </summary>
        <p className="mt-1">
          Datasheet limits on parts the model library rates: current (continuous
          and surge), voltage, power, reverse bias, junction temperature, per-pin
          drive current, and MCU/logic supply overvoltage. Parts with no known
          ratings are not checked.
        </p>
        {caveats.map(c => (
          <p key={c} className="mt-1" data-testid="fault-caveat" style={{ color: 'var(--warn)' }}>{c}</p>
        ))}
      </details>
    </div>
  )
}
