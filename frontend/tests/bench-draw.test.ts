// The logic-analyser drawing maths: a pin's stepped waveform, the decoded
// track's boxes, the truncation note, and drawScope laying lanes under traces.
// Pure geometry is tested directly; drawScope runs against a recording context.

import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import {
  DECODED_H, LANE_H, decodedBoxes, fitText, initialLevel, lanePath, lanesBlockHeight, lanesExtent, levelAt, truncationNote,
} from '../src/components/bench-draw'
import type { ScopeLane } from '../src/components/bench-draw'
import { MARKER_BAND_H, SCOPE_PAD_T, drawScope, plotExtraHeight, plotTopPad, timeTickLabeler } from '../src/components/scope-draw'

const lane = (edges: [number, boolean][], initial = false): Pick<ScopeLane, 'edges' | 'initial'> => ({
  edges: edges.map(([t_s, high]) => ({ t_s, high })), initial,
})
const xAt = (t: number) => 100 + t * 100 // 100 px per second from x = 100

describe('lane levels', () => {
  test('the level before the first edge is the opposite of where it went', () => {
    expect(initialLevel([{ t_s: 1, high: true }], true)).toBe(false)
    expect(initialLevel([{ t_s: 1, high: false }], false)).toBe(true)
    expect(initialLevel([], true)).toBe(true)
    expect(initialLevel([], false)).toBe(false)
  })

  test('the level at a time is the last edge at or before it', () => {
    const l = lane([[1, true], [2, false]])
    expect(levelAt(l, 0.5)).toBe(false)
    expect(levelAt(l, 1)).toBe(true)
    expect(levelAt(l, 1.9)).toBe(true)
    expect(levelAt(l, 2)).toBe(false)
  })
})

describe('a lane as a stepped path', () => {
  const yHigh = 10
  const yLow = 30

  test('levels are held and edges are vertical', () => {
    const path = lanePath(lane([[1, true], [2, false]]), 0, 3, xAt, yHigh, yLow)
    expect(path).toEqual([
      [100, yLow],
      [200, yLow], [200, yHigh],
      [300, yHigh], [300, yLow],
      [400, yLow],
    ])
  })

  test('a lane with no edges is a flat line across the window', () => {
    expect(lanePath(lane([], true), 0, 2, xAt, yHigh, yLow)).toEqual([[100, yHigh], [300, yHigh]])
  })

  test('the window opens at the level held there and drops edges outside it', () => {
    const l = lane([[1, true], [2, false], [5, true]])
    const path = lanePath(l, 1.5, 3, xAt, yHigh, yLow)
    expect(path[0]).toEqual([xAt(1.5), yHigh])
    expect(path[path.length - 1]).toEqual([xAt(3), yLow])
    // Exactly the one edge at t = 2 is inside the window.
    expect(path.filter(([x]) => x === xAt(2))).toHaveLength(2)
    expect(path.some(([x]) => x === xAt(5))).toBe(false)
  })

  test('x never goes backwards', () => {
    const path = lanePath(lane([[0.1, true], [0.2, false], [0.3, true], [0.4, false]]), 0, 1, xAt, yHigh, yLow)
    for (let i = 1; i < path.length; i++) expect(path[i][0]).toBeGreaterThanOrEqual(path[i - 1][0])
  })

  test('the extent of lanes spans their edges, with a frame for a lone edge', () => {
    expect(lanesExtent([lane([[1, true], [4, false]]), lane([[2, true]])])).toEqual([1, 4])
    expect(lanesExtent([lane([[2, true]])])).toEqual([2, 3])
    expect(lanesExtent([])).toEqual([0, 1])
  })
})

describe('the decoded track', () => {
  // 6 px per character, so a 12-char summary is 72 px.
  const measure = (s: string) => s.length * 6

  test('text is cut with an ellipsis to fit, or dropped when nothing fits', () => {
    expect(fitText(measure, 'hello', 100)).toBe('hello')
    expect(fitText(measure, 'hello world', 36)).toBe('hello…'.slice(0, 5) + '…')
    expect(fitText(measure, 'hello', 3)).toBe('')
    expect(fitText(measure, 'hello', 0)).toBe('')
  })

  test('a box is as wide as its text wants and stops before the next one', () => {
    const boxes = decodedBoxes(
      [{ t: 0, summary: 'ab' }, { t: 0.2, summary: 'a much longer summary here' }, { t: 5, summary: 'late' }],
      0, 10, xAt, 1100, measure,
    )
    expect(boxes).toHaveLength(3)
    // First: 12 px of text + 12 px of padding = 24 px, well before the next at 20 px? No: capped by it.
    expect(boxes[0].x0).toBe(100)
    expect(boxes[0].x1).toBeLessThanOrEqual(boxes[1].x0 - 1)
    // Second has room until the third (t = 5): its whole text fits.
    expect(boxes[1].label).toBe('a much longer summary here')
    expect(boxes[1].x1).toBeLessThan(boxes[2].x0)
    // Last may run to the right edge but not past it.
    expect(boxes[2].x1).toBeLessThanOrEqual(1100)
  })

  test('boxes never overlap and are time ordered', () => {
    const items = Array.from({ length: 30 }, (_, i) => ({ t: i * 0.05, summary: `txn number ${i}` }))
    const boxes = decodedBoxes(items.reverse(), 0, 2, xAt, 300, measure)
    for (let i = 1; i < boxes.length; i++) {
      expect(boxes[i].x0).toBeGreaterThanOrEqual(boxes[i - 1].x0)
      expect(boxes[i - 1].x1).toBeLessThanOrEqual(boxes[i].x0)
    }
  })

  test('a box too narrow for text is blank but still there', () => {
    const boxes = decodedBoxes([{ t: 0, summary: 'a long one' }, { t: 0.01, summary: 'x' }], 0, 1, xAt, 300, measure)
    expect(boxes[0].label).toBe('')
    expect(boxes[0].x1).toBeGreaterThanOrEqual(boxes[0].x0)
  })

  test('only transactions inside the window are laid out; NACK carries through', () => {
    const boxes = decodedBoxes(
      [{ t: -1, summary: 'before' }, { t: 1, summary: 'in', err: true }, { t: 9, summary: 'after' }],
      0, 5, xAt, 700, measure,
    )
    expect(boxes.map(b => b.summary)).toEqual(['in'])
    expect(boxes[0].err).toBe(true)
  })
})

describe('what a lane says about itself', () => {
  test('a cut edge list is named on the lane; a whole one is not', () => {
    expect(truncationNote({ edges: new Array(400).fill({ t_s: 0, high: true }), totalEdges: 5230 })).toBe('first 400 of 5230 edges')
    expect(truncationNote({ edges: [{ t_s: 0, high: true }], totalEdges: 1 })).toBeNull()
    expect(truncationNote({ edges: [] })).toBeNull()
  })

  test('the block height is a row per lane and one track', () => {
    expect(lanesBlockHeight(0, false)).toBe(0)
    expect(lanesBlockHeight(3, false)).toBe(3 * LANE_H)
    expect(lanesBlockHeight(3, true)).toBe(3 * LANE_H + DECODED_H + 4)
    expect(plotExtraHeight({ lanes: [], decoded: [] })).toBe(0)
    expect(plotExtraHeight({ lanes: [{ label: 'a', colorToken: '--trace-1', edges: [], initial: false }], decoded: [{ t: 0, summary: 's' }] }))
      .toBe(LANE_H + DECODED_H + 4)
  })
})

describe('the label band', () => {
  const pts = [{ label: 'V', colorToken: '--trace-1', points: [{ x: 0, y: 1 }] }]
  test('a lanes-only plot with events keeps a band above the lanes for their names', () => {
    expect(plotTopPad({ traces: [], markers: [{ x: 1, label: 'a' }] })).toBe(SCOPE_PAD_T + MARKER_BAND_H)
  })
  test('no events, or analogue traces under the labels, keep the plain pad', () => {
    expect(plotTopPad({ traces: [], markers: [] })).toBe(SCOPE_PAD_T)
    expect(plotTopPad({ traces: [], markers: undefined })).toBe(SCOPE_PAD_T)
    expect(plotTopPad({ traces: pts, markers: [{ x: 1, label: 'a' }] })).toBe(SCOPE_PAD_T)
  })
})

// A recording 2D context: every call is kept, nothing is drawn.
function recorder() {
  const calls: { fn: string; args: unknown[] }[] = []
  const state: Record<string, unknown> = {}
  const ctx = new Proxy({}, {
    get: (_t, prop: string) => {
      if (prop === 'measureText') return (s: string) => ({ width: s.length * 5 })
      if (prop in state) return state[prop]
      return (...args: unknown[]) => { calls.push({ fn: prop, args }) }
    },
    set: (_t, prop: string, v) => { state[prop] = v; return true },
  }) as unknown as CanvasRenderingContext2D
  return { ctx, calls }
}

describe('drawScope with lanes', () => {
  // Other test files share this process, so what is stubbed is put back.
  const g = globalThis as unknown as Record<string, unknown>
  const saved = { document: g.document, getComputedStyle: g.getComputedStyle }
  beforeAll(() => {
    g.document = { documentElement: { dataset: {} } }
    g.getComputedStyle = () => ({ getPropertyValue: () => '#336699' })
  })
  afterAll(() => {
    if (saved.document === undefined) delete g.document; else g.document = saved.document
    if (saved.getComputedStyle === undefined) delete g.getComputedStyle; else g.getComputedStyle = saved.getComputedStyle
  })

  const base = {
    traces: [], xLabel: 'simulated time', yUnit: '', xTick: timeTickLabeler(0, 2), xRange: [0, 2] as [number, number],
  }
  const oneLane: ScopeLane = { label: 'PC13', colorToken: '--trace-1', edges: [{ t_s: 0.5, high: true }, { t_s: 1, high: false }], initial: false }

  test('a plot of lanes alone is drawn (no volts axis needed)', () => {
    const { ctx, calls } = recorder()
    drawScope(ctx, 600, 100, { ...base, lanes: [oneLane] })
    expect(calls.some(c => c.fn === 'stroke')).toBe(true)
    // The pin's name is drawn in the gutter.
    expect(calls.some(c => c.fn === 'fillText' && c.args[0] === 'PC13')).toBe(true)
  })

  test('an empty plot stays blank, as before', () => {
    const { ctx, calls } = recorder()
    drawScope(ctx, 600, 100, base)
    expect(calls.some(c => c.fn === 'fillText')).toBe(false)
  })

  test('decoded boxes and event rules are painted over lanes', () => {
    const { ctx, calls } = recorder()
    drawScope(ctx, 600, 140, {
      ...base,
      lanes: [oneLane],
      decoded: [{ t: 0.2, summary: 'LM75 0x48 ACK' }, { t: 1.2, summary: 'EEPROM NACK', err: true }],
      markers: [{ x: 1.5, label: 'Reset', tone: 'warn' }],
    })
    expect(calls.filter(c => c.fn === 'roundRect')).toHaveLength(2)
    expect(calls.some(c => c.fn === 'fillText' && c.args[0] === 'Reset')).toBe(true)
  })

  test('a truncated lane says so on the lane', () => {
    const { ctx, calls } = recorder()
    drawScope(ctx, 600, 100, { ...base, lanes: [{ ...oneLane, totalEdges: 900 }] })
    expect(calls.some(c => c.fn === 'fillText' && c.args[0] === 'first 2 of 900 edges')).toBe(true)
  })

  test('lanes under analogue traces keep the traces their own height', () => {
    const { ctx, calls } = recorder()
    drawScope(ctx, 600, 150 + LANE_H, {
      ...base,
      traces: [{ label: 'V', colorToken: '--trace-2', points: [{ x: 0, y: 0 }, { x: 2, y: 3.3 }] }],
      yUnit: 'V',
      lanes: [oneLane],
    })
    expect(calls.some(c => c.fn === 'fillText' && c.args[0] === 'V')).toBe(true)
    expect(calls.some(c => c.fn === 'fillText' && c.args[0] === 'PC13')).toBe(true)
  })

  test('colliding event labels step down a row; the third is dropped but its rule stays', () => {
    const { ctx, calls } = recorder()
    drawScope(ctx, 600, 100, {
      ...base,
      lanes: [oneLane],
      markers: [
        { x: 1.0, label: 'Alpha' }, { x: 1.01, label: 'Beta' }, { x: 1.02, label: 'Gamma' },
      ],
    })
    const labelYs = calls.filter(c => c.fn === 'fillText' && ['Alpha', 'Beta', 'Gamma'].includes(c.args[0] as string)).map(c => c.args[2])
    expect(labelYs).toHaveLength(2)
    expect(labelYs[1]).not.toBe(labelYs[0])
    expect(calls.filter(c => c.fn === 'lineTo').length).toBeGreaterThanOrEqual(3)
  })
})
