import { expect, test } from 'bun:test'
import { deckPlots, engineering, isSpiceDeck, runDeck } from '../src/lib/deck-sim'
import { leftPadFor, ticksAcross } from '../src/components/scope-draw'
import { seedServedFormats } from './served-formats'

seedServedFormats()

const NET_KICAD = '(export (version "E")\n  (design ...'
const NET_SPICE = '* divider\nV1 in 0 5\nR1 in out 1k\n.tran 1u 1m\n.end\n'

test('.net is routed by content, not by name', () => {
  expect(isSpiceDeck('board.net', NET_KICAD)).toBe(false)
  expect(isSpiceDeck('divider.net', NET_SPICE)).toBe(true)
  // The two unambiguous deck extensions never need the sniff.
  expect(isSpiceDeck('rc_ac_bode.cir', '')).toBe(true)
  expect(isSpiceDeck('rc.SP', '')).toBe(true)
  expect(isSpiceDeck('watchy.kicad_pcb', NET_SPICE)).toBe(false)
})

test('a transient plots against its own time column', () => {
  const [plot] = deckPlots({
    ok: true, analysis: 'tran',
    columns: ['time_s', 'V(out)'],
    rows: [[0, 0], [1e-3, 2.5], [2e-3, 5]],
  })
  expect(plot.traces).toHaveLength(1)
  expect(plot.traces[0].label).toBe('V(out)')
  expect(plot.traces[0].points).toEqual([{ x: 0, y: 0 }, { x: 1e-3, y: 2.5 }, { x: 2e-3, y: 5 }])
  expect(plot.xLabel).toBe('time 0 s → 2 ms')
  // The axis is readable between its ends, in the column's own unit.
  expect(plot.yUnit).toBe(' V')
  expect(plot.xTick?.(1e-3)).toBe('1 ms')
})

test('a dc sweep names the swept source on its axis', () => {
  const [plot] = deckPlots({ ok: true, analysis: 'dc', columns: ['Vin', 'V(out)'], rows: [[0, 0], [3, 6]] })
  expect(plot.xLabel).toBe('Vin 0 → 3')
  expect(plot.traces[0].points[1]).toEqual({ x: 3, y: 6 })
})

test('ac magnitude becomes dB on a log-frequency axis, phase gets its own panel', () => {
  const plots = deckPlots({
    ok: true, analysis: 'ac',
    columns: ['frequency', 'V(out)', 'V(out):phase_deg'],
    rows: [[10, 1, 0], [1000, 0.7071, -45], [1e6, 1e-3, -90]],
  })
  expect(plots).toHaveLength(2)
  const [mag, phase] = plots
  expect(mag.yUnit).toBe(' dB')
  expect(mag.xLabel).toBe('frequency 10 Hz → 1 MHz (log)')
  // x is handed to the painter as log10(Hz); a tick prints the frequency.
  expect(mag.xTick?.(4)).toBe('10 kHz')
  // x is log10(f): a decade sweep on a linear axis is one spike at the edge.
  expect(mag.traces[0].points.map(p => p.x)).toEqual([1, 3, 6])
  expect(mag.traces[0].points[0].y).toBeCloseTo(0, 9)
  expect(mag.traces[0].points[1].y).toBeCloseTo(-3.0103, 3)
  expect(mag.traces[0].points[2].y).toBeCloseTo(-60, 9)
  // The phase column is already degrees; it is passed through, not derived.
  expect(phase.yUnit).toBe('°')
  expect(phase.traces[0].points.map(p => p.y)).toEqual([0, -45, -90])
})

test('a plot carries the unit its own columns name, and none when they differ', () => {
  const unit = (cols: string[]) => deckPlots({ ok: true, analysis: 'tran', columns: cols, rows: [[0, 0, 1], [1, 2, 3]] })[0].yUnit
  expect(unit(['time_s', 'V(out)', 'V(in)'])).toBe(' V')
  expect(unit(['time_s', 'I(V1)', 'I(R2)'])).toBe(' A')
  // Volts and amps on one axis share no unit, and a wrong one is worse than none.
  expect(unit(['time_s', 'V(out)', 'I(V1)'])).toBe('')
})

test('the painted axes are readable: no clipped minus, and ticks between the ends', () => {
  // The bug: a fixed 38px left pad right-aligned "-160 dB" (42px in the scope's
  // 9px mono) off the canvas, so a 160 dB ATTENUATION read as 160 dB of gain.
  const ctx = { measureText: (t: string) => ({ width: t.length * 6 }) } as unknown as CanvasRenderingContext2D
  expect(leftPadFor(ctx, ['0 dB', '-160 dB'], 400)).toBeGreaterThanOrEqual(42)
  // A decade sweep drawn as log10(Hz) gets labelled decades in between.
  expect(ticksAcross(1, 9).length).toBeGreaterThan(2)
  expect(ticksAcross(0, 2e-3).length).toBeGreaterThan(2)
})

test('an operating point has nothing to plot', () => {
  expect(deckPlots({ ok: true, analysis: 'op', columns: ['V(out)'], rows: [[2.5]] })).toEqual([])
})

test('engineering units read the way an axis is read', () => {
  expect(engineering(1500, 'Hz')).toBe('1.5 kHz')
  expect(engineering(2.2e-4, 's')).toBe('220 µs')
  expect(engineering(0, 's')).toBe('0 s')
})

test('a refusal is surfaced verbatim, JSON body or not', async () => {
  const real = globalThis.fetch
  try {
    globalThis.fetch = (async () => new Response(
      JSON.stringify({ ok: false, error: 'the deck has no `.tran` card' }),
      { status: 400 },
    )) as unknown as typeof fetch
    const json = await runDeck(new File(['*deck'], 'd.cir'), new AbortController().signal)
    expect(json).toEqual({ ok: false, error: 'the deck has no `.tran` card' })

    // A proxy or a body-limit refusal answers in plaintext; JSON.parse on that
    // would throw a SyntaxError that hides the real message.
    globalThis.fetch = (async () => new Response('body limit exceeded', { status: 413 })) as unknown as typeof fetch
    const text = await runDeck(new File(['*deck'], 'd.cir'), new AbortController().signal)
    expect(text).toEqual({ ok: false, error: 'body limit exceeded' })
  } finally {
    globalThis.fetch = real
  }
})
