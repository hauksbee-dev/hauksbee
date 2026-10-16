// The one canvas painter behind every trace in the app — the live scope's
// rolling window and the deck panel's finished sweep — so the two cannot drift
// into different graticules, bloom recipes or axis labels. It knows nothing of
// time, frequency or volts: callers hand it points ALREADY in the x units they
// want on screen (seconds; log10(Hz) for Bode) plus the labels naming them,
// which is how one drawing serves a linear and a log sweep with no mode flag.

import { cssToken, isLightTheme } from '../lib/theme-tokens'

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
  /** Moments to mark with a labelled vertical rule (the live scope's faults). */
  markers?: { x: number; label: string }[]
}

/** CSS token for probe slot idx; used by the canvas beams AND the list dots. */
export function traceToken(idx: number): string {
  return `--trace-${(idx % 6) + 1}`
}

/** Roughly five round tick values across [lo, hi]. Both axes step through this,
 *  so graticule rows and x labels share one idea of a round number. */
export function ticksAcross(lo: number, hi: number): number[] {
  const range = Math.abs(hi - lo) || 1
  const pow = 10 ** Math.floor(Math.log10(range))
  const n = range / pow
  const step = pow * (n > 5 ? 2 : n > 2 ? 1 : 0.5)
  const out: number[] = []
  for (let v = Math.ceil(lo / step) * step; v <= hi + step * 1e-9; v += step) out.push(v)
  return out
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
    ctx.fillStyle = cssToken('--scope-hint')
    ctx.font = '11px monospace'
    ctx.textAlign = 'center'
    ctx.fillText(plot.hint, W / 2, H / 2)
    return
  }

  const PAD_R = 60
  const PAD_T = 8
  const PAD_B = plot.xTick ? 28 : 20
  const plotH = H - PAD_T - PAD_B

  const pts = plot.traces.flatMap(t => t.points)
  if (pts.length === 0) return
  const [xMin, xMax] = plot.xRange ?? [
    Math.min(...pts.map(p => p.x)),
    Math.max(...pts.map(p => p.x)),
  ]
  const xSpan = xMax - xMin || 1

  // Voltage (or dB, or degrees) range: autoscale, then 5% headroom.
  let yMin = plot.yFloor?.[0] ?? Infinity
  let yMax = plot.yFloor?.[1] ?? -Infinity
  for (const p of pts) {
    if (p.x < xMin || p.x > xMax) continue
    yMin = Math.min(yMin, p.y)
    yMax = Math.max(yMax, p.y)
  }
  if (!Number.isFinite(yMin) || !Number.isFinite(yMax)) return
  const yRange = yMax - yMin || 1
  yMin -= yRange * 0.05
  yMax += yRange * 0.05

  // The labels are measured BEFORE the left pad is fixed; see leftPadFor.
  const yVals = ticksAcross(yMin, yMax)
  const yStep = yVals.length > 1 ? yVals[1] - yVals[0] : yRange
  const yTicks = yVals.map(v => `${tickText(v, yStep)}${plot.yUnit}`)
  ctx.font = '9px JetBrains Mono, monospace'
  const PAD_L = leftPadFor(ctx, yTicks, W)
  const plotW = W - PAD_L - PAD_R

  const xAt = (x: number) => PAD_L + ((x - xMin) / xSpan) * plotW
  const yAt = (y: number) => PAD_T + plotH - ((y - yMin) / (yMax - yMin)) * plotH

  // Graticule (phosphor green in dark, deep green ink in light)
  ctx.strokeStyle = cssToken('--scope-grid')
  ctx.fillStyle = cssToken('--scope-tick')
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
  // Intermediate x ticks, so a value can be read off the curve.
  const xTick = plot.xTick
  if (xTick) {
    ctx.textAlign = 'center'
    let lastRight = -Infinity
    for (const gx of ticksAcross(xMin, xMax)) {
      const px = xAt(gx)
      ctx.beginPath()
      ctx.moveTo(px, PAD_T)
      ctx.lineTo(px, PAD_T + plotH)
      ctx.stroke()
      // On a phone the decades are 40px apart and the labels ran together into
      // one illegible word, so a label is drawn only when it has room.
      const half = ctx.measureText(xTick(gx)).width / 2
      if (px - half < lastRight + 6) continue
      ctx.fillText(xTick(gx), px, H - PAD_B + 10)
      lastRight = px + half
    }
  }
  ctx.setLineDash([])

  ctx.fillStyle = cssToken('--scope-axis')
  ctx.textAlign = 'center'
  ctx.fillText(plot.xLabel, PAD_L + plotW / 2, H - 4)

  // Event rules under the traces: where in time something happened.
  const markColor = cssToken('--err')
  ctx.font = 'bold 9px JetBrains Mono, monospace'
  ctx.textAlign = 'left'
  for (const m of plot.markers ?? []) {
    if (m.x < xMin || m.x > xMax) continue
    const px = xAt(m.x)
    ctx.strokeStyle = markColor
    ctx.fillStyle = markColor
    ctx.lineWidth = 1
    ctx.setLineDash([3, 3])
    ctx.beginPath()
    ctx.moveTo(px, PAD_T)
    ctx.lineTo(px, PAD_T + plotH)
    ctx.stroke()
    ctx.setLineDash([])
    ctx.fillText(m.label, Math.min(px + 3, PAD_L + plotW - ctx.measureText(m.label).width), PAD_T + 9)
  }

  // Traces: multi-pass phosphor bloom, at a fraction of the strength in light
  // mode or the beam reads as smudge on paper.
  for (const trace of plot.traces) {
    const color = cssToken(trace.colorToken)
    const path = trace.points.filter(p => p.x >= xMin && p.x <= xMax).map(p => ({ x: xAt(p.x), y: yAt(p.y) }))
    if (path.length < 2) continue

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
