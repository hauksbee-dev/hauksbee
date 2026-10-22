// The logic-analyser half of the scope: digital lanes for pins, a decoded
// protocol track for buses, and the maths behind both. The geometry is pure
// (no canvas) so it is tested without one; the two painters take a context and
// are called from scope-draw's drawScope, so the report's run view and the live
// scope paint lanes, decoded boxes and event rules from one place.

import { cssToken, isLightTheme } from '../lib/theme-tokens'
import type { EdgeSample } from '../types/bench'

/** One pin as a two-level lane. */
export interface ScopeLane {
  /** Drawn at the right-hand end of the lane. */
  label: string
  /** A `--trace-N` token. */
  colorToken: string
  /** Level changes, oldest first. */
  edges: EdgeSample[]
  /** The level before the first edge. */
  initial: boolean
  /** Edges the engine counted; more than `edges.length` says the list is cut. */
  totalEdges?: number
  /** The focused pin: a tinted band and a brighter label. */
  focused?: boolean
}

/** One decoded bus transaction as a box on the protocol track. */
export interface DecodedItem {
  t: number
  /** The transaction in words. */
  summary: string
  /** A transaction nobody answered: drawn in the error tone. */
  err?: boolean
}

/** Height of one lane and of the decoded track, in CSS px. */
export const LANE_H = 22
export const DECODED_H = 22

/** The height a plot must add under its traces for these lanes and track. */
export function lanesBlockHeight(laneCount: number, decoded: boolean): number {
  return laneCount * LANE_H + (decoded ? DECODED_H + 4 : 0)
}

/** The level of a pin before its first edge: the opposite of where that edge
 *  went. With no edges at all, the level it held the whole time. */
export function initialLevel(edges: EdgeSample[], heldHigh: boolean): boolean {
  return edges.length > 0 ? !edges[0].high : heldHigh
}

/** The level a lane holds at time `t`. */
export function levelAt(lane: Pick<ScopeLane, 'edges' | 'initial'>, t: number): boolean {
  let level = lane.initial
  for (const e of lane.edges) {
    if (e.t_s > t) break
    level = e.high
  }
  return level
}

/** The lane's stepped waveform over [t0, t1] as pixel points: a level held, then
 *  a vertical edge, then the next level. `xAt` maps seconds to px; the path
 *  starts at the window's left edge at the level held there and ends at the right
 *  edge. Edges outside the window are not in the path. */
export function lanePath(
  lane: Pick<ScopeLane, 'edges' | 'initial'>,
  t0: number, t1: number,
  xAt: (t: number) => number, yHigh: number, yLow: number,
): [number, number][] {
  const y = (high: boolean) => (high ? yHigh : yLow)
  let level = levelAt(lane, t0)
  const out: [number, number][] = [[xAt(t0), y(level)]]
  for (const e of lane.edges) {
    if (e.t_s <= t0 || e.t_s > t1) continue
    const x = xAt(e.t_s)
    out.push([x, y(level)])
    level = e.high
    out.push([x, y(level)])
  }
  out.push([xAt(t1), y(level)])
  return out
}

/** The window a set of lanes spans when nothing else fixes it: first to last
 *  edge, widened so a single edge still has a frame. */
export function lanesExtent(lanes: Pick<ScopeLane, 'edges'>[]): [number, number] {
  let lo = Infinity
  let hi = -Infinity
  for (const l of lanes) for (const e of l.edges) { lo = Math.min(lo, e.t_s); hi = Math.max(hi, e.t_s) }
  if (!Number.isFinite(lo)) return [0, 1]
  return hi - lo < 1e-9 ? [lo, lo + 1] : [lo, hi]
}

/** Text cut to fit `maxW` px, with an ellipsis; empty when not even one letter fits. */
export function fitText(measure: (s: string) => number, text: string, maxW: number): string {
  if (maxW <= 0) return ''
  if (measure(text) <= maxW) return text
  const ell = '…'
  let n = text.length
  while (n > 0 && measure(text.slice(0, n) + ell) > maxW) n--
  return n > 0 ? text.slice(0, n) + ell : ''
}

/** A decoded box placed on the track. */
export interface DecodedBox {
  x0: number
  x1: number
  label: string
  err: boolean
  summary: string
}

/** Lay the transactions out as boxes: each starts at its time, is as wide as its
 *  text wants, and never runs into the next one; a box too narrow for any text is
 *  drawn blank (the summary is still its tooltip). */
export function decodedBoxes(
  items: DecodedItem[],
  t0: number, t1: number,
  xAt: (t: number) => number, xEnd: number,
  measure: (s: string) => number,
): DecodedBox[] {
  const sorted = items.filter(i => i.t >= t0 && i.t <= t1).sort((a, b) => a.t - b.t)
  const MIN_W = 4
  const PAD = 6
  return sorted.map((item, i) => {
    const x0 = xAt(item.t)
    const limit = (i + 1 < sorted.length ? xAt(sorted[i + 1].t) - 1 : xEnd)
    const wanted = x0 + measure(item.summary) + 2 * PAD
    const x1 = Math.max(Math.min(wanted, limit), Math.min(x0 + MIN_W, xEnd))
    return {
      x0, x1, err: !!item.err, summary: item.summary,
      label: fitText(measure, item.summary, x1 - x0 - 2 * PAD + 2),
    }
  })
}

/** Where a lane's "cut short" note goes and what it says; null for a lane that
 *  holds every edge the engine counted. */
export function truncationNote(lane: Pick<ScopeLane, 'edges' | 'totalEdges'>): string | null {
  const total = lane.totalEdges ?? lane.edges.length
  return total > lane.edges.length ? `first ${lane.edges.length} of ${total} edges` : null
}

// ── Painters ─────────────────────────────────────────────────────────────

/** The plot rectangle the painters work in, in CSS px. */
export interface LaneFrame {
  left: number
  right: number
  top: number
  t0: number
  t1: number
  xAt: (t: number) => number
}

/** Paint the lanes from `frame.top` down; returns the y just below them. */
export function paintLanes(ctx: CanvasRenderingContext2D, frame: LaneFrame, lanes: ScopeLane[]): number {
  const light = isLightTheme()
  let top = frame.top
  for (const lane of lanes) {
    const color = cssToken(lane.colorToken)
    const yHigh = top + 5
    const yLow = top + LANE_H - 5
    if (lane.focused) {
      ctx.fillStyle = color + (light ? '1f' : '26')
      ctx.fillRect(frame.left, top + 1, frame.right - frame.left, LANE_H - 2)
    }
    // The low rail: where the lane rests.
    ctx.strokeStyle = cssToken('--scope-grid')
    ctx.lineWidth = 0.5
    ctx.setLineDash([2, 4])
    ctx.beginPath()
    ctx.moveTo(frame.left, yLow + 0.5)
    ctx.lineTo(frame.right, yLow + 0.5)
    ctx.stroke()
    ctx.setLineDash([])

    const path = lanePath(lane, frame.t0, frame.t1, frame.xAt, yHigh, yLow)
    ctx.strokeStyle = color
    ctx.lineWidth = lane.focused ? 2 : 1.4
    ctx.lineJoin = 'miter'
    ctx.shadowColor = color
    ctx.shadowBlur = light ? 0 : lane.focused ? 6 : 3
    ctx.beginPath()
    path.forEach(([x, y], i) => (i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y)))
    ctx.stroke()
    ctx.shadowBlur = 0

    // The pin's name, in the right-hand gutter, level with its lane.
    ctx.font = `${lane.focused ? 'bold ' : ''}9px JetBrains Mono, monospace`
    ctx.textAlign = 'left'
    ctx.fillStyle = lane.focused ? cssToken('--silk') : color
    ctx.fillText(lane.label, frame.right + 4, top + LANE_H / 2 + 3)

    const note = truncationNote(lane)
    if (note) {
      // Past the last recorded edge the lane is not data: a pill over its right
      // end says so, on the lane, where the reader is looking.
      ctx.font = 'italic 9px JetBrains Mono, monospace'
      const w = ctx.measureText(note).width + 10
      ctx.fillStyle = cssToken('--scope-face')
      ctx.fillRect(frame.right - w, top + 2, w, LANE_H - 4)
      ctx.strokeStyle = cssToken('--warn')
      ctx.lineWidth = 1
      ctx.strokeRect(frame.right - w + 0.5, top + 2.5, w - 1, LANE_H - 5)
      ctx.fillStyle = cssToken('--warn')
      ctx.textAlign = 'right'
      ctx.fillText(note, frame.right - 5, top + LANE_H / 2 + 3)
    }
    top += LANE_H
  }
  return top
}

/** Paint the decoded track at `frame.top`: a box per transaction, the error tone
 *  for one nobody answered. */
export function paintDecoded(ctx: CanvasRenderingContext2D, frame: LaneFrame, items: DecodedItem[]): void {
  const light = isLightTheme()
  ctx.font = '9px JetBrains Mono, monospace'
  const boxes = decodedBoxes(items, frame.t0, frame.t1, frame.xAt, frame.right, s => ctx.measureText(s).width)
  const y = frame.top + 3
  const h = DECODED_H - 6
  for (const b of boxes) {
    const tone = cssToken(b.err ? '--err' : '--silk-dim')
    ctx.fillStyle = tone + (b.err ? (light ? '26' : '33') : (light ? '1a' : '22'))
    ctx.strokeStyle = tone
    ctx.lineWidth = b.err ? 1.4 : 1
    const w = Math.max(1, b.x1 - b.x0)
    ctx.beginPath()
    ctx.roundRect(b.x0, y, w, h, 3)
    ctx.fill()
    ctx.stroke()
    if (b.label) {
      ctx.fillStyle = cssToken(b.err ? '--err-strong' : '--silk')
      ctx.textAlign = 'left'
      ctx.fillText(b.label, b.x0 + 6, y + h / 2 + 3)
    }
  }
}
