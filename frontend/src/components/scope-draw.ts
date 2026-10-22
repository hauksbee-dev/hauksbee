// The one canvas painter behind every trace in the app — the live scope's
// rolling window and the deck panel's finished sweep — so the two cannot drift
// into different graticules, bloom recipes or axis labels. It knows nothing of
// time, frequency or volts: callers hand it points ALREADY in the x units they
// want on screen (seconds; log10(Hz) for Bode) plus the labels naming them,
// which is how one drawing serves a linear and a log sweep with no mode flag.

import { cssToken, isLightTheme } from '../lib/theme-tokens'
import { LANE_H, lanesBlockHeight, lanesExtent, paintDecoded, paintLanes } from './bench-draw'
import type { DecodedItem, ScopeLane } from './bench-draw'

export interface ScopeTrace {
  /** Drawn at the right-hand end of the trace. */
  label: string
  /** A `--trace-N` token; the list dots below the canvas read the same one. */
  colorToken: string
  points: { x: number; y: number }[]
}

export interface ScopePlot {
  traces: ScopeTrace[]
  /** One line under the axis: "← 3s window →", "frequency 10 Hz → 1 MHz (log)". */
  xLabel: string
  /** Suffix on every y tick: 'V', 'dB', '°', ''. */
  yUnit: string
  /** Labels an intermediate x tick in the caller's own x units (the AC panel is
   *  handed log10(Hz) and prints the frequency). Without it only the end values
   *  can be read off the curve. */
  xTick?: (v: number) => string
  /** Pin the x axis instead of fitting it to the points (the live window). */
  xRange?: [number, number]
  /** A y range the autoscale may only grow, never shrink (the scope's 0–5 V). */
  yFloor?: [number, number]
  /** Drawn centred in place of the plot; for "no probes attached" etc. */
  hint?: string | null
  /** Moments to mark with a labelled vertical rule (the live scope's faults, a
   *  run's resets). `tone` picks the rule's colour: `err` (the default), `warn`,
   *  or `note` for a moment that is information rather than trouble. */
  markers?: { x: number; label: string; tone?: 'err' | 'warn' | 'note' }[]
  /** Pins as digital lanes under the traces (bench-draw.ts). */
  lanes?: ScopeLane[]
  /** Decoded bus transactions on a protocol track under the lanes. */
  decoded?: DecodedItem[]
}

/** The extra canvas height a plot's lanes and decoded track need under its
 *  traces; a caller adds it to the height it gives the analogue part. */
export function plotExtraHeight(plot: Pick<ScopePlot, 'lanes' | 'decoded'>): number {
  return lanesBlockHeight(plot.lanes?.length ?? 0, !!plot.decoded && plot.decoded.length > 0)
}

/** Space above the plot, in CSS px (a click on a lane needs to know where lanes start). */
export const SCOPE_PAD_T = 8

/** Height of the label band a lanes-only plot keeps above its lanes for event
 *  names (two staggered rows), so a label never sits on a waveform. */
export const MARKER_BAND_H = 22

/** The top pad for a plot, in CSS px: where its first lane or trace starts. */
export function plotTopPad(plot: Pick<ScopePlot, 'traces' | 'markers'>): number {
  const lanesOnly = !plot.traces.some(t => t.points.length > 0)
  return SCOPE_PAD_T + (lanesOnly && (plot.markers?.length ?? 0) > 0 ? MARKER_BAND_H : 0)
}

/** CSS token for probe slot idx; used by the canvas beams AND the list dots. */
export function traceToken(idx: number): string {
  return `--trace-${(idx % 6) + 1}`
}

/** The gap between ticks that gives roughly five round values across `range`. */
export function tickStep(range: number): number {
  const r = Math.abs(range) || 1
  const pow = 10 ** Math.floor(Math.log10(r))
  const n = r / pow
  return pow * (n > 5 ? 2 : n > 2 ? 1 : 0.5)
}

/** Roughly five round tick values across [lo, hi]. Both axes step through this,
 *  so graticule rows and x labels share one idea of a round number. */
export function ticksAcross(lo: number, hi: number): number[] {
  const step = tickStep(hi - lo)
  const out: number[] = []
  for (let v = Math.ceil(lo / step) * step; v <= hi + step * 1e-9; v += step) out.push(v)
  return out
}

/** The y axis: a range that CONTAINS every plotted value, with headroom, ending
 *  on round tick values. */
export interface YAxis {
  lo: number
  hi: number
  step: number
  ticks: number[]
}

/** Plan the y axis. The old axis padded the data by 5% and then stopped at the
 *  last tick below it, so a 5.000 V probe sat on the top edge of a 0-4 V axis
 *  with its trace clipped away. Here the top and bottom are rounded OUT to the
 *  next tick, so the data can never be outside the frame.
 *
 *  `floor` is a range the axis may only grow: the scope's usual 0-3.3 V, so a
 *  quiet rail does not zoom into noise. When the floor starts at zero and the
 *  data never goes negative the baseline stays on zero rather than floating
 *  below it. */
export function planYAxis(dataLo: number, dataHi: number, floor?: [number, number]): YAxis | null {
  let lo = Math.min(dataLo, floor?.[0] ?? Infinity)
  let hi = Math.max(dataHi, floor?.[1] ?? -Infinity)
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) return null
  if (hi - lo < 1e-12) {
    // A flat line (or a lone sample): give it a frame to sit in.
    const m = Math.max(Math.abs(hi), 1)
    lo -= m * 0.5
    hi += m * 0.5
  }
  const range = hi - lo
  const keepZero = floor?.[0] === 0 && lo >= 0
  hi += range * 0.08
  if (!keepZero) lo -= range * 0.08
  const step = tickStep(hi - lo)
  // Round OUT to the tick grid. The 1e-9 guards a value already on a tick from
  // being pushed a whole step away by floating-point noise.
  const snap = (v: number) => Number(v.toFixed(10))
  const sLo = snap(Math.floor(lo / step + 1e-9) * step)
  const sHi = snap(Math.ceil(hi / step - 1e-9) * step)
  const ticks: number[] = []
  for (let v = sLo; v <= sHi + step * 1e-9; v += step) ticks.push(snap(v))
  return { lo: sLo, hi: sHi, step, ticks }
}

/** A duration in sim time with the unit a person would say it in: "2.51 s",
 *  "12.5 ms", "840 µs". Three significant figures, no trailing zeros. */
export function formatSimTime(t: number): string {
  const a = Math.abs(t)
  const trim = (v: number) => Number(v.toPrecision(3)).toString()
  if (a === 0) return '0 s'
  if (a < 1e-3) return `${trim(t * 1e6)} µs`
  if (a < 1) return `${trim(t * 1e3)} ms`
  return `${trim(t)} s`
}

/** x tick labels for a window [lo, hi] of sim time: ONE unit for the whole axis
 *  (chosen from the window's largest value) and just enough decimals that
 *  neighbouring ticks never print the same text. A window 1 ms wide at t = 2.5 s
 *  reads "2.5004 s", "2.5006 s"; the old labels printed the raw microsecond
 *  count, "2509611µs", at both ends. */
export function timeTickLabeler(lo: number, hi: number): (t: number) => string {
  const top = Math.max(Math.abs(lo), Math.abs(hi))
  const [scale, unit] = top >= 1 ? [1, 's'] : top >= 1e-3 ? [1e3, 'ms'] : [1e6, 'µs']
  const step = tickStep(hi - lo) * scale
  const decimals = Math.max(0, Math.min(6, Math.ceil(-Math.log10(step) - 1e-9)))
  return t => `${(t * scale).toFixed(decimals)} ${unit}`
}

/** The samples of a trace that fall inside the x window, as a drawable shape:
 *  a polyline of two or more, or a single held level. A lone sample is a real
 *  measurement (a probe attached to a paused sim has exactly one), and drawing
 *  nothing for it made a correct 5 V reading look like an empty scope. */
export function traceShape(
  points: { x: number; y: number }[], xMin: number, xMax: number,
): { kind: 'none' } | { kind: 'point'; point: { x: number; y: number } } | { kind: 'line'; points: { x: number; y: number }[] } {
  const inside = points.filter(p => p.x >= xMin && p.x <= xMax)
  if (inside.length === 0) return { kind: 'none' }
  if (inside.length === 1) return { kind: 'point', point: inside[0] }
  return { kind: 'line', points: inside }
}

/** Left pad wide enough for the widest y label. Fixed at 38px it clipped the
 *  leading minus off "-100 dB", which then read as a gain of +100 dB. */
export function leftPadFor(ctx: CanvasRenderingContext2D, labels: string[], W: number): number {
  return Math.min(W / 3, Math.max(34, ...labels.map(t => ctx.measureText(t).width + 8)))
}

/** A tick label with only as many decimals as the gap between ticks needs. */
function tickText(v: number, step: number): string {
  const places = Math.max(0, Math.min(6, -Math.floor(Math.log10(Math.abs(step) || 1))))
  return v.toFixed(places)
}

/** Paint `plot` over `ctx`'s canvas, from a rAF loop or an effect; owns no state. */
export function drawScope(ctx: CanvasRenderingContext2D, W: number, H: number, plot: ScopePlot): void {
  ctx.clearRect(0, 0, W, H)
  const light = isLightTheme()
  // The face: phosphor-dark or graticule-light, then scanlines (a CRT artifact,
  // so the light token is fully transparent).
  ctx.fillStyle = cssToken('--scope-face')
  ctx.fillRect(0, 0, W, H)
  if (!light) {
    ctx.fillStyle = cssToken('--scope-scan')
    for (let sy = 0; sy < H; sy += 3) ctx.fillRect(0, sy, W, 1)
  }

  if (plot.hint) {
    ctx.fillStyle = cssToken('--silk-dim')
    ctx.font = '11px monospace'
    ctx.textAlign = 'center'
    ctx.fillText(plot.hint, W / 2, H / 2)
    return
  }

  const lanes = plot.lanes ?? []
  const decoded = plot.decoded ?? []
  const laneBlock = plotExtraHeight(plot)
  const PAD_T = plotTopPad(plot)
  const PAD_B = plot.xTick ? 28 : 20

  const pts = plot.traces.flatMap(t => t.points)
  const analog = pts.length > 0
  // A scope with nothing to draw stays blank; one with only lanes (the run
  // view) has no volts axis at all, and its lanes start at the top.
  if (!analog && laneBlock === 0) return
  const plotH = analog ? Math.max(24, H - PAD_T - PAD_B - laneBlock) : 0
  const lanesTop = PAD_T + plotH
  const bottom = lanesTop + laneBlock
  const [xMin, xMax] = plot.xRange ?? (analog
    ? [Math.min(...pts.map(p => p.x)), Math.max(...pts.map(p => p.x))]
    : lanesExtent(lanes))
  const xSpan = xMax - xMin || 1

  // Voltage (or dB, or degrees) range: autoscaled to CONTAIN every plotted
  // value, with headroom, ending on round ticks (see planYAxis).
  let axis: YAxis | null = null
  if (analog) {
    let dataLo = Infinity
    let dataHi = -Infinity
    for (const p of pts) {
      if (p.x < xMin || p.x > xMax) continue
      dataLo = Math.min(dataLo, p.y)
      dataHi = Math.max(dataHi, p.y)
    }
    axis = planYAxis(dataLo, dataHi, plot.yFloor)
    if (!axis) return
  }
  const yMin = axis?.lo ?? 0
  const yMax = axis?.hi ?? 1

  // The labels are measured BEFORE the left pad is fixed; see leftPadFor.
  const yVals = axis?.ticks ?? []
  const yTicks = axis ? yVals.map(v => `${tickText(v, axis.step)}${plot.yUnit}`) : []
  ctx.font = '9px JetBrains Mono, monospace'
  const PAD_L = analog ? leftPadFor(ctx, yTicks, W) : 14
  // The right gutter carries each trace's and each lane's name.
  ctx.font = 'bold 9px JetBrains Mono, monospace'
  const PAD_R = Math.min(W / 3, Math.max(60, ...lanes.map(l => ctx.measureText(l.label).width + 10)))
  ctx.font = '9px JetBrains Mono, monospace'
  const plotW = W - PAD_L - PAD_R

  const xAt = (x: number) => PAD_L + ((x - xMin) / xSpan) * plotW
  const yAt = (y: number) => PAD_T + plotH - ((y - yMin) / (yMax - yMin)) * plotH

  // Graticule (phosphor green in dark, deep green ink in light). The numbers
  // on it are NOT green: dim phosphor on a near-black face was unreadable at
  // 9 px, so the labels use the app's own dim text colour.
  ctx.strokeStyle = cssToken('--scope-grid')
  ctx.fillStyle = cssToken('--silk-dim')
  ctx.lineWidth = 0.5
  ctx.setLineDash([2, 4])
  ctx.textAlign = 'right'
  yVals.forEach((gv, i) => {
    ctx.beginPath()
    ctx.moveTo(PAD_L, yAt(gv))
    ctx.lineTo(PAD_L + plotW, yAt(gv))
    ctx.stroke()
    ctx.fillText(yTicks[i], PAD_L - 4, yAt(gv) + 3)
  })
  // Intermediate x ticks, so a value can be read off the curve. They run down
  // through the lanes too, so a lane's edge reads against the same time axis.
  const xTick = plot.xTick
  if (xTick) {
    ctx.textAlign = 'center'
    let lastRight = -Infinity
    for (const gx of ticksAcross(xMin, xMax)) {
      const px = xAt(gx)
      ctx.beginPath()
      ctx.moveTo(px, PAD_T)
      ctx.lineTo(px, bottom)
      ctx.stroke()
      // On a phone the decades are 40px apart and the labels ran together into
      // one illegible word, so a label is drawn only when it has room.
      const half = ctx.measureText(xTick(gx)).width / 2
      // Keep the label inside the plot's own columns: centred on a tick at the
      // left edge it ran under the y labels ("0V" over "2.0 s").
      const cx = Math.min(Math.max(px, PAD_L + half), W - half - 2)
      if (cx - half < lastRight + 6) continue
      ctx.fillText(xTick(gx), cx, H - PAD_B + 10)
      lastRight = cx + half
    }
  }
  ctx.setLineDash([])

  ctx.fillStyle = cssToken('--silk-dim')
  ctx.textAlign = 'center'
  ctx.fillText(plot.xLabel, PAD_L + plotW / 2, H - 4)

  // Digital lanes, then the decoded track, under the analogue traces.
  const frame = { left: PAD_L, right: PAD_L + plotW, top: lanesTop, t0: xMin, t1: xMax, xAt }
  if (lanes.length > 0) paintLanes(ctx, frame, lanes)
  if (decoded.length > 0) paintDecoded(ctx, { ...frame, top: lanesTop + lanes.length * LANE_H + 4 }, decoded)

  // Event rules over everything: where in time something happened. Labels that
  // would land on the previous one step down a row; a third row is not drawn
  // (the rule still is).
  ctx.font = 'bold 9px JetBrains Mono, monospace'
  ctx.textAlign = 'left'
  const rowRight = [-Infinity, -Infinity]
  for (const m of plot.markers ?? []) {
    if (m.x < xMin || m.x > xMax) continue
    const px = xAt(m.x)
    const color = cssToken(m.tone === 'note' ? '--silk-dim' : m.tone === 'warn' ? '--warn' : '--err')
    ctx.strokeStyle = color
    ctx.fillStyle = color
    ctx.lineWidth = 1
    ctx.setLineDash([3, 3])
    ctx.beginPath()
    ctx.moveTo(px, SCOPE_PAD_T)
    ctx.lineTo(px, bottom)
    ctx.stroke()
    ctx.setLineDash([])
    const w = ctx.measureText(m.label).width
    const x = Math.min(px + 3, PAD_L + plotW - w)
    const row = rowRight.findIndex(r => x > r + 4)
    if (row < 0) continue
    rowRight[row] = x + w
    ctx.fillText(m.label, x, SCOPE_PAD_T + 9 + row * 10)
  }

  // Traces: multi-pass phosphor bloom, at a fraction of the strength in light
  // mode or the beam reads as smudge on paper.
  for (const trace of plot.traces) {
    const color = cssToken(trace.colorToken)
    const shape = traceShape(trace.points, xMin, xMax)
    if (shape.kind === 'none') continue
    // A lone sample: a dot where it was measured and a dashed rule carrying the
    // level across, so a correct reading is visible rather than an empty scope.
    const path = shape.kind === 'line'
      ? shape.points.map(p => ({ x: xAt(p.x), y: yAt(p.y) }))
      : [{ x: xAt(shape.point.x), y: yAt(shape.point.y) }]
    if (shape.kind === 'point') {
      const dot = path[0]
      ctx.strokeStyle = color + (light ? '80' : '99')
      ctx.lineWidth = 1.2
      ctx.setLineDash([4, 4])
      ctx.beginPath()
      ctx.moveTo(dot.x, dot.y)
      ctx.lineTo(PAD_L + plotW, dot.y)
      ctx.stroke()
      ctx.setLineDash([])
      ctx.fillStyle = color
      ctx.shadowColor = color
      ctx.shadowBlur = light ? 0 : 8
      ctx.beginPath()
      ctx.arc(dot.x, dot.y, 3.5, 0, Math.PI * 2)
      ctx.fill()
      ctx.shadowBlur = 0
    }

    const drawPath = () => {
      ctx.beginPath()
      ctx.moveTo(path[0].x, path[0].y)
      for (let i = 1; i < path.length; i++) ctx.lineTo(path[i].x, path[i].y)
      ctx.stroke()
    }
    // Pass 1: wide soft glow bloom. Pass 2: medium halo. Pass 3: crisp core.
    for (const [alpha, lightAlpha, width, lightWidth, blur, lightBlur] of [
      ['33', '1a', 6, 4.5, 12, 5],
      ['66', '40', 3, 2.4, 8, 3],
      ['', '', 1.2, 1.4, 4, 0],
    ] as const) {
      ctx.strokeStyle = color + (light ? lightAlpha : alpha)
      ctx.lineWidth = light ? lightWidth : width
      ctx.shadowColor = color
      ctx.shadowBlur = light ? lightBlur : blur
      drawPath()
    }
    ctx.shadowBlur = 0

    // Legend label on the right, level with where the trace ends.
    ctx.fillStyle = color
    ctx.font = 'bold 9px JetBrains Mono, monospace'
    ctx.textAlign = 'left'
    ctx.shadowColor = color
    ctx.shadowBlur = light ? 0 : 4
    ctx.fillText(trace.label, PAD_L + plotW + 4, Math.max(PAD_T + 10, Math.min(H - PAD_B - 2, path[path.length - 1].y + 3)))
    ctx.shadowBlur = 0
  }
}
