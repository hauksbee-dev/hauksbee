import { useEffect, useState } from 'react'
import type { LoadProgress } from '../lib/api'

/** A load shorter than this shows no bar: the spinner tile carries it. */
export const LOAD_BAR_DELAY_MS = 300

/** Whole percent for the readout. 99 is held until the load is truly done, so a
 *  rounding of 0.996 never reads as finished. */
export function loadPercent(fraction: number): number {
  const pct = Math.round(Math.min(1, Math.max(0, fraction)) * 100)
  return fraction < 1 ? Math.min(pct, 99) : 100
}

/**
 * The determinate bar for a board load: a percentage and the stage in the
 * engine's own words ("Matching parts to models"). It stays out of the way
 * until the load has lasted `delayMs` and a real progress report has arrived;
 * against a server that does not stream there is never a report, so nothing is
 * drawn here and the caller's spinner stands alone, as it always did.
 *
 * The space is reserved from the first render, so the card does not change
 * height when the bar appears.
 */
export function LoadBar({
  progress,
  delayMs = LOAD_BAR_DELAY_MS,
}: {
  progress: LoadProgress | null
  delayMs?: number
}) {
  const [waited, setWaited] = useState(delayMs <= 0)
  useEffect(() => {
    if (waited) return
    const id = window.setTimeout(() => setWaited(true), delayMs)
    return () => window.clearTimeout(id)
  }, [waited, delayMs])

  const shown = waited && progress !== null
  const pct = progress ? loadPercent(progress.fraction) : 0
  return (
    <div
      data-testid="load-bar"
      className="load-bar mx-auto mt-4"
      style={{ minHeight: 34, maxWidth: 360 }}
    >
      {shown && progress && (
        <>
          <div
            role="progressbar"
            aria-label="Loading the board"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={pct}
            aria-valuetext={`${pct} percent, ${progress.label}`}
            className="load-bar-track"
          >
            <span style={{ width: `${pct}%` }} />
          </div>
          <div className="load-bar-text" aria-live="polite">
            <span data-testid="load-bar-label">{progress.label}</span>
            <span data-testid="load-bar-percent" style={{ fontFamily: 'var(--font-mono)' }}>
              {pct}%
            </span>
          </div>
        </>
      )}
    </div>
  )
}
