import { useEffect, useState } from 'react'
import { CheckIcon } from '../Icons'

/** The server's no-history estimate, used until it sends one of its own. */
export const DEFAULT_ESTIMATE_SECS = 180

/** How far along a run of `t` seconds probably is. It eases to 90% at the
 *  estimate, then keeps creeping towards (never reaching) 100%, three times
 *  slower: a bar that stalls reads as a hang, and a full bar before the server
 *  answers is a lie. */
export function progressAt(t: number, estimate: number): number {
  if (t <= estimate) return 1 - Math.exp(-t * Math.LN10 / estimate)
  return 0.9 + 0.1 * (1 - Math.exp(-(t - estimate) / (3 * estimate)))
}

/** What the estimate honestly supports: whole minutes, then an admission. */
export function remainingLabel(t: number, estimate: number): string {
  const left = estimate - t
  if (left <= 0) return 'taking longer than usual'
  return left < 60 ? 'under a minute left (estimate)' : `about ${Math.round(left / 60)} min left (estimate)`
}

const clock = (t: number) => {
  const s = Math.floor(t)
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s`
}

/** A datasheet extraction in flight: the phase the server reports, the time
 *  spent, and an estimated bar. `finished` freezes it full, for the review. */
export function ExtractProgress({ phase, estimate, started, finished }: {
  phase: string
  estimate: number
  started: number
  finished?: number
}) {
  const [live, setLive] = useState({ t: 0, p: 0 })
  useEffect(() => {
    if (finished !== undefined) return
    const id = setInterval(() => setLive(prev => {
      const t = (Date.now() - started) / 1000
      // Monotone even when a late server estimate would pull the curve back.
      return { t, p: Math.max(prev.p, progressAt(t, estimate)) }
    }), 1000)
    return () => clearInterval(id)
  }, [started, estimate, finished])

  const done = finished !== undefined
  const t = done ? (finished - started) / 1000 : live.t
  const label = remainingLabel(t, estimate)
  return (
    <div
      data-testid="extract-progress"
      className="mt-2.5 rounded-lg px-3 py-2.5"
      style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
    >
      <div className="flex items-center gap-2 text-[12px]">
        {done ? <CheckIcon size={12} /> : <span className="slot-spin shrink-0" aria-hidden />}
        {/* Phase and the coarse estimate only: the per-second clock beside it
            would have a screen reader talking without pause. */}
        <span role="status" aria-live="polite" className="flex-1 min-w-0" style={{ color: 'var(--silk)' }}>
          {done ? 'Drafted' : `${phase.charAt(0).toUpperCase()}${phase.slice(1)} · ${label}`}
        </span>
        <span aria-hidden className="tnum shrink-0 text-[11px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
          {clock(t)}
        </span>
      </div>
      <div
        role="progressbar"
        aria-label="Drafting the model (estimated)"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round((done ? 1 : live.p) * 100)}
        aria-valuetext={done ? 'done' : label}
        data-state={done ? 'done' : t > estimate ? 'over' : 'on'}
        className="extract-bar mt-2"
      >
        <span style={{ width: `${(done ? 1 : live.p) * 100}%` }} />
      </div>
    </div>
  )
}
