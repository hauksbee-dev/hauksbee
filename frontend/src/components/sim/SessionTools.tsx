import { useEffect, useRef, useState } from 'react'
import type { ClientMessage, PowerSupplyWire, SimFrame } from '../../types/protocol'
import { useDismissable } from '../../hooks/useDismissable'
import { BoltIcon, PowerIcon } from '../Icons'
import { sameSupply, supplyLabel } from './session-seed'
import { simTimeText } from '../FaultPanel'

// The transport row's session tools: how this session relates to the checks
// (supply, duration), the fault count, and the one export menu. Each is a
// compact chip; the detail lives one click away in the dock.

const chip = (tone: 'ok' | 'warn' | 'err' | 'plain'): React.CSSProperties => ({
  height: 26,
  color: tone === 'plain' ? 'var(--silk-dim)' : `var(--${tone === 'warn' ? 'warn-strong' : tone})`,
  background: tone === 'plain' ? 'var(--surface-2)' : `var(--${tone}-bg)`,
  border: `1px solid ${tone === 'plain' ? 'var(--hairline)' : `var(--${tone}-border)`}`,
})

/** Whether the live rails run what the checks ran, and the one action that
 *  makes them. Absent without a checks supply. */
export function SupplyChip({ running, checks, onUseChecks, onOpen }: {
  running: Record<string, PowerSupplyWire>
  checks: { net: string; wire: PowerSupplyWire }[]
  onUseChecks: () => void
  onOpen: () => void
}) {
  if (checks.length === 0) return null
  const differing = checks.filter(c => !sameSupply(running[c.net], c.wire))
  const same = differing.length === 0
  const first = checks[0]
  return (
    <span className="inline-flex items-center gap-1 min-w-0 max-w-full">
      <button
        type="button"
        data-testid="sim-supply-chip"
        onClick={onOpen}
        title={same
          ? 'Every rail the checks power runs the same supply here. Open the power rails.'
          : differing.map(c => `${c.net}: live ${supplyLabel(running[c.net])}, checks ${supplyLabel(c.wire)}`).join('\n')}
        className="hb-press inline-flex items-center gap-1.5 px-2 rounded-md text-[11px] min-w-0 cursor-pointer"
        style={chip(same ? 'ok' : 'warn')}
      >
        <PowerIcon size={11} />
        <span className="truncate">
          {same
            ? `same supply as checks (${checks.length === 1 ? `${first.net} ${supplyLabel(first.wire)}` : `${checks.length} rails`})`
            : `${differing.length} rail${differing.length === 1 ? '' : 's'} differ from checks`}
        </span>
      </button>
      {!same && (
        <button type="button" data-testid="sim-use-checks-supply" onClick={onUseChecks}
          className="hb-press px-2 rounded-md text-[11px] font-semibold cursor-pointer shrink-0"
          style={{ height: 26, color: 'var(--copper-hi)', background: 'var(--copper-tint-strong)', border: '1px solid var(--copper-deep)' }}>
          Use checks supply
        </button>
      )}
    </span>
  )
}

/** Runs the sim for exactly `seconds` of sim time in small steps (one frame
 *  each, so the scope gets a real trace), then stops. Pressing it again while
 *  it runs cancels. */
export function useRunFor(send: (msg: ClientMessage) => void, liveFrame: SimFrame | null) {
  const job = useRef<{ left: number; dt: number; lastT: number } | null>(null)
  const [busy, setBusy] = useState(false)
  useEffect(() => {
    const j = job.current
    if (!j || !liveFrame || liveFrame.t === j.lastT) return
    j.lastT = liveFrame.t
    if (j.left <= 0) {
      job.current = null
      setBusy(false)
      return
    }
    j.left -= 1
    send({ type: 'Step', dt: j.dt })
  }, [liveFrame, send])
  const start = (seconds: number) => {
    if (job.current) {
      job.current = null
      setBusy(false)
      return
    }
    // ~100 frames over the run, each at most the server's 1 s step ceiling.
    const steps = Math.max(1, Math.min(400, Math.ceil(Math.max(100, seconds))))
    const dt = seconds / steps
    send({ type: 'Pause' })
    job.current = { left: steps - 1, dt, lastT: liveFrame?.t ?? -1 }
    setBusy(true)
    send({ type: 'Step', dt })
  }
  return { busy, start }
}

export function RunForButton({ seconds, busy, onRun }: { seconds: number; busy: boolean; onRun: () => void }) {
  return (
    <button type="button" data-testid="sim-run-checks-duration" onClick={onRun}
      title={busy ? 'Stop the run' : `Step the sim through ${simTimeText(seconds)} of sim time, the duration the checks simulate, then pause`}
      className="hb-press px-2 rounded-md text-[11px] cursor-pointer shrink-0"
      style={busy ? chip('warn') : chip('plain')}>
      {busy ? 'Stop run' : `Run ${simTimeText(seconds)} (checks duration)`}
    </button>
  )
}

/** The fault count: opens the fault table. Quiet when nothing tripped. */
export function FaultCountChip({ parts, conditions, onOpen }: { parts: number; conditions: number; onOpen: () => void }) {
  return (
    <button type="button" data-testid="sim-fault-count" onClick={onOpen}
      title={conditions === 0 ? 'No part has gone over a rating. Open the fault table.' : `${conditions} condition${conditions === 1 ? '' : 's'} logged. Open the fault table.`}
      className="hb-press inline-flex items-center gap-1 px-2 rounded-md text-[11px] tnum cursor-pointer shrink-0"
      style={chip(parts > 0 ? 'err' : 'plain')}>
      <BoltIcon size={11} />
      {parts === 0 ? 'no faults' : `${parts} part${parts === 1 ? '' : 's'} faulted`}
    </button>
  )
}

export interface ExportItem {
  id: string
  label: string
  run: () => void
  disabled?: boolean
}

/** Every export the session offers, in one menu. */
export function ExportMenu({ items }: { items: ExportItem[] }) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLSpanElement>(null)
  useDismissable(open, wrap, setOpen)
  return (
    <span ref={wrap} className="relative shrink-0">
      <button type="button" data-testid="sim-export" aria-expanded={open} onClick={() => setOpen(o => !o)}
        className="hb-press px-2 rounded-md text-[11px] cursor-pointer" style={chip('plain')}>
        Export ▾
      </button>
      {open && (
        <div role="menu" className="absolute right-0 z-30 mt-1 flex flex-col py-1 rounded-md"
          style={{ minWidth: 190, background: 'var(--surface)', border: '1px solid var(--hairline)', boxShadow: 'var(--shadow-pop)' }}>
          {items.map(it => (
            <button key={it.id} type="button" role="menuitem" data-testid={`sim-export-${it.id}`} disabled={it.disabled}
              onClick={() => { it.run(); setOpen(false) }}
              className="hb-press text-left px-3 py-1.5 text-[11px]"
              style={{ background: 'none', border: 'none', color: it.disabled ? 'var(--silk-faint)' : 'var(--silk)', cursor: it.disabled ? 'not-allowed' : 'pointer' }}>
              {it.label}
            </button>
          ))}
        </div>
      )}
    </span>
  )
}
