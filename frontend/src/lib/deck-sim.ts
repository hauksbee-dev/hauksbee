// A SPICE deck dropped on the app: how to recognise one, how to ask the engine
// to run it, and how to turn its answer into traces. The CSV is NOT built here:
// `POST /api/sim` returns the bytes `hauksbee sim` itself prints and the export
// downloads exactly those, so "the export matches the CLI" is the server's
// property, not a second serializer's.

import type { ScopePlot } from '../components/scope-draw'
import { traceToken } from '../components/scope-draw'

/** `POST /api/sim`'s answer. `columns`/`rows` are the CSV's own header and rows;
 *  column 0 is the x axis (`time_s`, the swept source, or `frequency`). */
export interface DeckSimResponse {
  ok: boolean
  analysis?: 'op' | 'tran' | 'dc' | 'ac'
  columns?: string[]
  rows?: number[][]
  notes?: string[]
  csv?: string
  error?: string
}

/** Extensions that MIGHT be a deck; `.net` is also KiCad, so see `isSpiceDeck`. */
export const DECK_EXTS = ['.cir', '.sp', '.net']

/** Whether these bytes are a SPICE deck for `/api/sim` rather than a board.
 *  `.cir`/`.sp` belong to nothing else here; `.net` is claimed by KiCad too, so
 *  it is sniffed the way session-store.ts sniffs board-as-code (a KiCad netlist
 *  opens an s-expression, a deck carries at least one dot-card). */
export function isSpiceDeck(fileName: string, head: string): boolean {
  if (/\.(cir|sp)$/i.test(fileName)) return true
  if (!/\.net$/i.test(fileName)) return false
  if (/^\s*\(/.test(head)) return false
  return /^\s*\.(tran|ac|dc|op|print|end|model|options|temp|subckt)\b/im.test(head)
}

/** Run a deck. The bytes go up as themselves (no multipart, no re-encoding) so
 *  the server parses the file the user has on disk. */
export async function runDeck(file: File, signal: AbortSignal): Promise<DeckSimResponse> {
  const res = await fetch('/api/sim', {
    method: 'POST',
    headers: { 'X-Board-Filename': file.name, 'Content-Type': 'application/octet-stream' },
    body: file,
    signal,
  })
  const text = await res.text()
  try {
    return JSON.parse(text) as DeckSimResponse
  } catch {
    return { ok: false, error: text.trim().slice(0, 400) || `the server answered ${res.status} ${res.statusText}` }
  }
}

/** A number as an axis is read: 1.5 kHz, 220 µs. */
export function engineering(v: number, unit: string): string {
  const a = Math.abs(v)
  if (a === 0) return `0 ${unit}`.trim()
  const prefixes: [number, string][] = [
    [1e9, 'G'], [1e6, 'M'], [1e3, 'k'], [1, ''], [1e-3, 'm'], [1e-6, 'µ'], [1e-9, 'n'], [1e-12, 'p'],
  ]
  const [scale, prefix] = prefixes.find(([s]) => a >= s) ?? [1e-12, 'p']
  return `${Number((v / scale).toPrecision(3))} ${prefix}${unit}`.trim()
}

/** The unit a set of columns names for itself: `V(out)` is volts, `I(V1)` amps.
 *  Columns that disagree get no unit: a wrong quantity is worse than a bare one. */
export function columnUnit(names: string[]): string {
  const units = new Set(names.map(n => (/^v\s*\(/i.test(n) ? ' V' : /^i\s*\(/i.test(n) ? ' A' : '')))
  return units.size === 1 ? [...units][0] : ''
}

/**
 * The plots a response asks for: one for a transient or a sweep, two for AC
 * (magnitude and phase are different units and cannot share a y axis). AC x is
 * log10(frequency) — a decade sweep on a linear axis is one spike at the right
 * edge — and AC magnitude is LINEAR in the response (`v.norm()` in run_ac), so
 * dB = 20*log10(mag) is computed here. Phase is already degrees and passes
 * through. Nothing else is derived: a column not in the answer is not on screen.
 */
export function deckPlots(res: DeckSimResponse): ScopePlot[] {
  const columns = res.columns ?? []
  const rows = res.rows ?? []
  if (columns.length < 2 || rows.length < 2) return []
  const xs = rows.map(r => r[0])
  const xUnit = res.analysis === 'tran' ? 's' : res.analysis === 'ac' ? 'Hz' : ''
  const range = `${engineering(xs[0], xUnit)} → ${engineering(xs[xs.length - 1], xUnit)}`

  const column = (j: number, map: (v: number) => number, x: (v: number) => number) => ({
    // The `:phase_deg` suffix is the y axis's job, and a 16-character legend
    // does not fit beside a trace.
    label: columns[j].replace(/:phase_deg$/, ''),
    colorToken: traceToken(j - 1),
    points: rows.map((r, i) => ({ x: x(xs[i]), y: map(r[j]) })),
  })

  if (res.analysis === 'ac') {
    const log10 = (f: number) => Math.log10(Math.max(f, 1e-300))
    const db = (m: number) => 20 * Math.log10(Math.max(Math.abs(m), 1e-300))
    const phaseAt = (j: number) => /:phase_deg$/.test(columns[j])
    const xLabel = `frequency ${range} (log)`
    // x is log10(Hz) on screen, so a tick prints the frequency it stands for.
    const xTick = (x: number) => engineering(10 ** x, 'Hz')
    const pick = (phase: boolean) => columns.map((_, j) => j).filter(j => j > 0 && phaseAt(j) === phase)
    return [
      { traces: pick(false).map(j => column(j, db, log10)), xLabel, yUnit: ' dB', xTick },
      { traces: pick(true).map(j => column(j, v => v, log10)), xLabel, yUnit: '°', xTick },
    ].filter(p => p.traces.length > 0)
  }

  return [{
    traces: columns.map((_, j) => j).filter(j => j > 0).map(j => column(j, v => v, x => x)),
    xLabel: `${res.analysis === 'tran' ? 'time' : columns[0]} ${range}`,
    // The y unit is the columns' own: V(out) is volts, I(V1) is amps.
    yUnit: columnUnit(columns.slice(1)),
    xTick: (x: number) => engineering(x, xUnit),
  }]
}
