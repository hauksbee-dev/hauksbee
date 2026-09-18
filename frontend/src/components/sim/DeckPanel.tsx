import { useEffect, useRef, useState } from 'react'
import { Callout } from '../ui'
import { drawScope } from '../scope-draw'
import type { ScopePlot } from '../scope-draw'
import { deckPlots, runDeck, type DeckSimResponse } from '../../lib/deck-sim'
import { downloadText } from '../../lib/report-export'
import { errorText, isAbort } from '../../lib/api'

// A dropped SPICE deck, run by the engine and shown as one page: the analysis
// the DECK asked for (no selector — the `.tran`/`.ac`/`.dc` card decides, and
// offering a choice the engine would ignore would be a lie), the trace, the
// solver's notes, the first and last rows, and the CLI's CSV on a button.

/** Enough digits to be checkable, not so many that a row wraps on a phone. */
const cell = (v: number) => (v === 0 ? '0' : Math.abs(v) >= 1e4 || Math.abs(v) < 1e-3 ? v.toExponential(4) : v.toPrecision(6))

function Plot({ plot }: { plot: ScopePlot }) {
  const ref = useRef<HTMLCanvasElement>(null)
  useEffect(() => {
    const canvas = ref.current
    if (!canvas) return
    // Paint at the canvas's REAL on-screen width: a fixed backing store
    // stretched to a phone squashes the axis text into unreadable ink.
    const paint = () => {
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      canvas.width = Math.max(240, Math.round(canvas.clientWidth))
      canvas.height = 200
      drawScope(ctx, canvas.width, canvas.height, plot)
    }
    paint()
    const observer = new ResizeObserver(paint)
    observer.observe(canvas)
    return () => observer.disconnect()
  }, [plot])
  return (
    <div style={{ border: '1px solid var(--scope-edge)', borderRadius: 6, overflow: 'hidden', background: 'var(--scope-face)' }}>
      <canvas ref={ref} height={200} role="img" aria-label={`${plot.xLabel}, y axis in ${plot.yUnit.trim() || 'unitless values'}`} style={{ display: 'block', width: '100%', height: 200 }} />
      {/* The y quantity in words: the AC plot is dB, the table below it is the
          linear magnitude the CLI printed, and nothing else said so. */}
      <div className="px-2 py-1 text-[10px]" data-testid="plot-y-caption" style={{ color: 'var(--silk-faint)', borderTop: '1px solid var(--scope-edge)' }}>
        y axis: {plot.yUnit.trim() || 'unitless'}{plot.yUnit === ' dB' && ' — 20·log₁₀ of the magnitude in the table below'}
      </div>
    </div>
  )
}

export function DeckPanel({ file }: { file: File }) {
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<DeckSimResponse | null>(null)
  const [error, setError] = useState<string | null>(null)
  const abort = useRef<AbortController | null>(null)

  // A newer run supersedes the one in flight, and leaving the panel cancels it.
  useEffect(() => () => abort.current?.abort(), [])

  const run = () => {
    abort.current?.abort()
    const controller = new AbortController()
    abort.current = controller
    setBusy(true)
    setError(null)
    setResult(null)
    void runDeck(file, controller.signal)
      .then(res => { if (res.ok) setResult(res); else setError(res.error ?? 'the engine refused the deck without saying why') })
      .catch((e: unknown) => { if (!isAbort(e)) setError(errorText(e)) })
      .finally(() => { if (!controller.signal.aborted) setBusy(false) })
  }

  const columns = result?.columns ?? []
  const rows = result?.rows ?? []
  // First and last few rows: the ends are what an engineer checks by eye, the
  // whole table is the CSV's job, and `null` is the elision marker (without it
  // rows 4 and 5 read as consecutive).
  const shown: (number[] | null)[] = rows.length > 8 ? [...rows.slice(0, 4), null, ...rows.slice(-4)] : rows

  return (
    <div className="max-w-3xl mx-auto px-6 py-6 flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-3">
        <span className="text-[13px] truncate" style={{ fontFamily: 'var(--font-mono)', color: 'var(--silk)' }}>{file.name}</span>
        {result?.analysis && (
          <span className="hb-chip text-[11px]" data-testid="deck-analysis">{result.analysis.toUpperCase()} — the deck&rsquo;s own card</span>
        )}
        <span className="flex-1" />
        <button type="button" className="hb-btn-primary hb-press px-3 text-[12px]" style={{ height: 32 }} onClick={run} disabled={busy} data-testid="deck-run">
          {busy ? 'Running…' : 'Run this deck'}
        </button>
        {result?.csv && (
          <button type="button" className="hb-btn hb-press px-3 text-[12px]" style={{ height: 32 }} data-testid="deck-export"
            onClick={() => downloadText(`${file.name.replace(/\.[^.]+$/, '')}.csv`, result.csv ?? '', 'text/csv')}>
            Export CSV
          </button>
        )}
      </div>

      {error && <Callout tone="err" title="The deck did not run" testId="deck-error" live>{error}</Callout>}

      {result && (
        <div className="flex flex-col gap-4" data-testid="deck-result" aria-live="polite">
          {(result.notes ?? []).length > 0 && (
            <Callout tone="note" title="Solver notes" testId="deck-notes" accent>
              <ul className="list-disc pl-4">{(result.notes ?? []).map(n => <li key={n}>{n}</li>)}</ul>
            </Callout>
          )}
          {deckPlots(result).map(plot => <Plot key={`${plot.yUnit}${plot.xLabel}`} plot={plot} />)}
          <div className="overflow-x-auto">
            <table className="text-[11px] tnum" style={{ fontFamily: 'var(--font-mono)', color: 'var(--silk-dim)', borderCollapse: 'collapse' }}>
              <thead>
                {/* The bytes are the CLI's and are never restyled; the header
                    names the quantity, since the plot above is in dB. */}
                <tr>{columns.map((c, j) => <th key={c} className="px-2 py-1 text-right" style={{ color: 'var(--silk)', borderBottom: '1px solid var(--hairline)' }}>
                  {c}{result?.analysis === 'ac' && j > 0 && !/:phase_deg$/.test(c) && <span style={{ color: 'var(--silk-faint)' }}> (magnitude, V/V)</span>}
                </th>)}</tr>
              </thead>
              <tbody>
                {shown.map((r, i) => (
                  <tr key={i}>{columns.map((c, j) => (
                    <td key={c} className="px-2 py-0.5 text-right">{r === null ? '⋮' : cell(r[j])}</td>
                  ))}</tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            {rows.length} row(s) × {columns.length} column(s); the export is the bytes <code className="hb-inline">hauksbee sim {file.name}</code> prints.
          </div>
        </div>
      )}
    </div>
  )
}
