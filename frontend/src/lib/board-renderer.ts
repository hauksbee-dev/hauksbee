// Board renderer: draws a ParsedBoard onto HTML canvases.
// All draw calls go through the Camera transform.
//
// Rendering is split into two passes so a large board stays interactive:
//   renderStaticBoard: everything that only depends on the camera (copper,
//     graphics, pads, vias, labels). Drawn into an offscreen canvas that the
//     viewer blits every animation frame and re-renders only when the camera
//     settles (see BoardViewer's static cache).
//   renderDynamicOverlay: everything that changes per frame (voltage tints,
//     net highlights, fault pulses, particles, probe tooltip, component
//     glows). Small working sets, so it can run at full frame rate.

import type { Camera } from './camera'
import { worldToScreen } from './camera'
import type {
  ParsedBoard, Pad, Point, Segment,
} from './kicad-parser'
import { getLayerStyle, isCopperLayer, PAD_COLOR, PAD_GLOW, VIA_COLOR, VIA_DRILL_COLOR } from './layer-colors'

// ────────────────────── Layer ordering ───────────────────────────────

const LAYER_ORDER = [
  'B.Cu', 'In4.Cu', 'In3.Cu', 'In2.Cu', 'In1.Cu', 'F.Cu',
  'B.Fab', 'F.Fab',
  'B.SilkS', 'B.Silkscreen',
  'F.SilkS', 'F.Silkscreen',
  'Edge.Cuts',
  'Dwgs.User', 'User.Drawings',
]

// Above this many drawable primitives, canvas shadowBlur (the "glow") is
// disabled: each blurred stroke costs an offscreen composite, and on a
// 3,000-component board that alone pushes a full render into hundreds of ms.
const GLOW_PRIMITIVE_LIMIT = 1500

/** Total drawable primitives, used to decide whether glow effects are payable. */
export function countPrimitives(board: ParsedBoard): number {
  let n = board.segments.length + board.arcs.length + board.vias.length +
    board.gr_lines.length + board.gr_arcs.length + board.gr_circles.length +
    board.gr_rects.length + board.gr_polys.length
  for (const fp of board.footprints) {
    n += fp.pads.length + fp.fp_lines.length + fp.fp_arcs.length +
      fp.fp_circles.length + fp.fp_rects.length
  }
  return n
}

// ────────────────────── Canvas helpers ───────────────────────────────

function ws(cam: Camera, x: number, y: number): [number, number] {
  const { sx, sy } = worldToScreen(cam, x, y)
  return [sx, sy]
}

function lineWidth(cam: Camera, mm: number): number {
  return Math.max(0.5, mm * cam.scale)
}

function setStroke(ctx: CanvasRenderingContext2D, color: string, glowColor: string | undefined, lw: number) {
  ctx.strokeStyle = color
  ctx.lineWidth = lw
  ctx.lineCap = 'round'
  ctx.lineJoin = 'round'
  if (glowColor && lw > 1.0) {
    ctx.shadowColor = glowColor
    ctx.shadowBlur = Math.min(lw * 2.5, 14)
  } else {
    ctx.shadowBlur = 0
  }
}

/** True when the screen-space segment cannot touch the canvas (with margin). */
function segOffscreen(ctx: CanvasRenderingContext2D, x1: number, y1: number, x2: number, y2: number): boolean {
  const m = 20
  const w = ctx.canvas.width, h = ctx.canvas.height
  return (
    (x1 < -m && x2 < -m) || (x1 > w + m && x2 > w + m) ||
    (y1 < -m && y2 < -m) || (y1 > h + m && y2 > h + m)
  )
}

// ────────────────────── Three-point arc ──────────────────────────────
// KiCad arcs are defined by start, mid (on-arc), end.
// We reconstruct the circumscribed circle.

function arcFromThreePoints(p1: Point, p2: Point, p3: Point): { cx: number; cy: number; r: number; startAngle: number; endAngle: number; ccw: boolean } | null {
  const ax = p1.x, ay = p1.y
  const bx = p2.x, by = p2.y
  const cx2 = p3.x, cy2 = p3.y

  const D = 2 * (ax * (by - cy2) + bx * (cy2 - ay) + cx2 * (ay - by))
  if (Math.abs(D) < 1e-10) return null

  const ux = ((ax * ax + ay * ay) * (by - cy2) + (bx * bx + by * by) * (cy2 - ay) + (cx2 * cx2 + cy2 * cy2) * (ay - by)) / D
  const uy = ((ax * ax + ay * ay) * (cx2 - bx) + (bx * bx + by * by) * (ax - cx2) + (cx2 * cx2 + cy2 * cy2) * (bx - ax)) / D

  const r = Math.sqrt((ax - ux) ** 2 + (ay - uy) ** 2)
  const startAngle = Math.atan2(ay - uy, ax - ux)
  const endAngle = Math.atan2(cy2 - uy, cx2 - ux)

  // Determine direction using the midpoint
  const midAngle = Math.atan2(by - uy, bx - ux)
  // Normalise angles to [0, 2π]
  function norm(a: number) { return ((a % (2 * Math.PI)) + 2 * Math.PI) % (2 * Math.PI) }
  const sa = norm(startAngle), ma = norm(midAngle), ea = norm(endAngle)
  // CCW if ma is between sa and ea in the CCW sense
  const ccw = (sa <= ma && ma <= ea) || (ea < sa && (ma >= sa || ma <= ea))

  return { cx: ux, cy: uy, r, startAngle, endAngle, ccw }
}

function drawArc(ctx: CanvasRenderingContext2D, cam: Camera, start: Point, mid: Point, end: Point, color: string, glow: string | undefined, width: number) {
  const arc = arcFromThreePoints(start, mid, end)
  if (!arc) {
    // Degenerate: draw a line
    const [sx, sy] = ws(cam, start.x, start.y)
    const [ex, ey] = ws(cam, end.x, end.y)
    ctx.beginPath()
    setStroke(ctx, color, glow, lineWidth(cam, width))
    ctx.moveTo(sx, sy)
    ctx.lineTo(ex, ey)
    ctx.stroke()
    return
  }
  const [cxs, cys] = ws(cam, arc.cx, arc.cy)
  const rs = arc.r * cam.scale
  ctx.beginPath()
  setStroke(ctx, color, glow, lineWidth(cam, width))
  ctx.arc(cxs, cys, rs, arc.startAngle, arc.endAngle, arc.ccw)
  ctx.stroke()
}

// ────────────────────── Pad drawing ──────────────────────────────────

function drawPad(ctx: CanvasRenderingContext2D, cam: Camera, pad: Pad, color: string, glowColor: string | undefined, alpha = 1) {
  const [sx, sy] = ws(cam, pad.at.x, pad.at.y)
  const w = pad.size.w * cam.scale
  const h = pad.size.h * cam.scale
  const minDim = Math.min(w, h)
  if (minDim < 0.5) return
  // Cull: cheap screen-space reject before any canvas state changes
  const reach = Math.max(w, h)
  if (sx + reach < 0 || sx - reach > ctx.canvas.width || sy + reach < 0 || sy - reach > ctx.canvas.height) return

  ctx.save()
  ctx.globalAlpha = alpha
  ctx.fillStyle = color
  if (glowColor) {
    ctx.shadowColor = glowColor
    ctx.shadowBlur = Math.min(w * 0.6, 8)
  }
  ctx.translate(sx, sy)
  ctx.rotate((pad.angle * Math.PI) / 180)

  ctx.beginPath()
  switch (pad.shape) {
    case 'circle': {
      const r = w / 2
      ctx.arc(0, 0, r, 0, Math.PI * 2)
      break
    }
    case 'rect':
      ctx.rect(-w / 2, -h / 2, w, h)
      break
    case 'oval':
    case 'roundrect': {
      const r = Math.min(w, h) / 2
      ctx.roundRect(-w / 2, -h / 2, w, h, r)
      break
    }
    default:
      ctx.rect(-w / 2, -h / 2, w, h)
  }

  ctx.fill()

  // Draw drill hole for through-hole pads
  if (pad.type === 'thru_hole' || pad.type === 'np_thru_hole') {
    const drill = pad.drill
    if (drill) {
      ctx.shadowBlur = 0
      ctx.fillStyle = VIA_DRILL_COLOR
      ctx.beginPath()
      if (drill.oval && drill.dx && drill.dy) {
        const dw = drill.dx * cam.scale
        const dh = drill.dy * cam.scale
        ctx.ellipse(0, 0, dw / 2, dh / 2, 0, 0, Math.PI * 2)
      } else {
        const dr = (drill.diameter / 2) * cam.scale
        ctx.arc(0, 0, Math.max(dr, 0.5), 0, Math.PI * 2)
      }
      ctx.fill()
    }
  }

  ctx.restore()
}

// ────────────────────── Static pass ──────────────────────────────────

/**
 * Draw everything that depends only on the camera: board graphics, copper in
 * its base layer colours, pads, vias, and reference labels. No per-frame
 * state (voltages, highlights, faults) touches this pass, so the result can
 * be cached and blitted.
 */
export function renderStaticBoard(
  ctx: CanvasRenderingContext2D,
  board: ParsedBoard,
  cam: Camera,
) {
  ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height)

  // Background
  ctx.fillStyle = '#020617'
  ctx.fillRect(0, 0, ctx.canvas.width, ctx.canvas.height)

  // Subtle radial vignette so the board area stands out against the flat background
  {
    const W = ctx.canvas.width
    const H = ctx.canvas.height
    const grad = ctx.createRadialGradient(W / 2, H / 2, 0, W / 2, H / 2, Math.max(W, H) * 0.65)
    grad.addColorStop(0, 'rgba(10,18,40,0)')
    grad.addColorStop(1, 'rgba(0,0,0,0.55)')
    ctx.fillStyle = grad
    ctx.fillRect(0, 0, W, H)
  }

  const glowOk = countPrimitives(board) <= GLOW_PRIMITIVE_LIMIT

  // ── Board graphics, grouped by layer ──
  for (const layer of LAYER_ORDER) {
    const style = getLayerStyle(layer)
    if (!style.visible) continue

    const isCopper = isCopperLayer(layer)
    const color = style.color
    const glow = glowOk ? style.glow : undefined

    // gr_lines on this layer
    for (const l of board.gr_lines) {
      if (l.layer !== layer) continue
      const [x1, y1] = ws(cam, l.start.x, l.start.y)
      const [x2, y2] = ws(cam, l.end.x, l.end.y)
      if (segOffscreen(ctx, x1, y1, x2, y2)) continue
      ctx.beginPath()
      setStroke(ctx, color, glow, lineWidth(cam, l.width))
      ctx.moveTo(x1, y1)
      ctx.lineTo(x2, y2)
      ctx.stroke()
    }

    // gr_arcs
    for (const a of board.gr_arcs) {
      if (a.layer !== layer) continue
      drawArc(ctx, cam, a.start, a.mid, a.end, color, glow, a.width)
    }

    // gr_circles
    for (const c of board.gr_circles) {
      if (c.layer !== layer) continue
      const [csx, csy] = ws(cam, c.center.x, c.center.y)
      const dx = c.end.x - c.center.x
      const dy = c.end.y - c.center.y
      const r = Math.sqrt(dx * dx + dy * dy) * cam.scale
      if (r < 0.5) continue
      setStroke(ctx, color, glow, lineWidth(cam, c.width))
      ctx.beginPath()
      ctx.arc(csx, csy, r, 0, Math.PI * 2)
      if (c.fill) { ctx.fillStyle = color; ctx.fill() }
      else ctx.stroke()
    }

    // gr_rects
    for (const r of board.gr_rects) {
      if (r.layer !== layer) continue
      const [x1, y1] = ws(cam, r.start.x, r.start.y)
      const [x2, y2] = ws(cam, r.end.x, r.end.y)
      setStroke(ctx, color, glow, lineWidth(cam, r.width))
      ctx.beginPath()
      ctx.rect(Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1))
      if (r.fill) { ctx.fillStyle = color; ctx.fill() }
      else ctx.stroke()
    }

    // gr_polys
    for (const p of board.gr_polys) {
      if (p.layer !== layer || p.pts.length < 2) continue
      ctx.beginPath()
      const [fx, fy] = ws(cam, p.pts[0].x, p.pts[0].y)
      ctx.moveTo(fx, fy)
      for (let i = 1; i < p.pts.length; i++) {
        const [px, py] = ws(cam, p.pts[i].x, p.pts[i].y)
        ctx.lineTo(px, py)
      }
      ctx.closePath()
      setStroke(ctx, color, glow, lineWidth(cam, p.width))
      if (p.fill) { ctx.fillStyle = color; ctx.fill() }
      else ctx.stroke()
    }

    // Footprint graphics on this layer
    for (const fp of board.footprints) {
      for (const l of fp.fp_lines) {
        if (l.layer !== layer) continue
        const [x1, y1] = ws(cam, l.start.x, l.start.y)
        const [x2, y2] = ws(cam, l.end.x, l.end.y)
        if (segOffscreen(ctx, x1, y1, x2, y2)) continue
        ctx.beginPath()
        setStroke(ctx, color, glow, lineWidth(cam, l.width))
        ctx.moveTo(x1, y1)
        ctx.lineTo(x2, y2)
        ctx.stroke()
      }
      for (const a of fp.fp_arcs) {
        if (a.layer !== layer) continue
        drawArc(ctx, cam, a.start, a.mid, a.end, color, glow, a.width)
      }
      for (const c of fp.fp_circles) {
        if (c.layer !== layer) continue
        const [csx, csy] = ws(cam, c.center.x, c.center.y)
        const dx = c.end.x - c.center.x
        const dy = c.end.y - c.center.y
        const r = Math.sqrt(dx * dx + dy * dy) * cam.scale
        if (r < 0.5) continue
        setStroke(ctx, color, glow, lineWidth(cam, c.width))
        ctx.beginPath()
        ctx.arc(csx, csy, r, 0, Math.PI * 2)
        ctx.stroke()
      }
      for (const r of fp.fp_rects) {
        if (r.layer !== layer) continue
        const [x1, y1] = ws(cam, r.start.x, r.start.y)
        const [x2, y2] = ws(cam, r.end.x, r.end.y)
        setStroke(ctx, color, glow, lineWidth(cam, r.width))
        ctx.beginPath()
        ctx.rect(Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1))
        ctx.stroke()
      }
    }

    // Tracks / segments on this copper layer, base colour. Live voltage tints
    // are painted over these by the dynamic pass.
    if (isCopper) {
      for (const s of board.segments) {
        if (s.layer !== layer) continue
        const [x1, y1] = ws(cam, s.start.x, s.start.y)
        const [x2, y2] = ws(cam, s.end.x, s.end.y)
        if (segOffscreen(ctx, x1, y1, x2, y2)) continue
        ctx.beginPath()
        setStroke(ctx, color, glow, lineWidth(cam, s.width))
        ctx.moveTo(x1, y1)
        ctx.lineTo(x2, y2)
        ctx.stroke()
      }

      for (const a of board.arcs) {
        if (a.layer !== layer) continue
        drawArc(ctx, cam, a.start, a.mid, a.end, color, glow, a.width)
      }
    }
  }

  // ── Vias ──
  ctx.shadowBlur = 0
  for (const v of board.vias) {
    const [sx, sy] = ws(cam, v.at.x, v.at.y)
    const r = (v.size / 2) * cam.scale
    if (r < 0.5) continue
    if (sx + r < 0 || sx - r > ctx.canvas.width || sy + r < 0 || sy - r > ctx.canvas.height) continue
    const dr = (v.drill / 2) * cam.scale
    ctx.beginPath()
    ctx.fillStyle = VIA_COLOR
    ctx.arc(sx, sy, r, 0, Math.PI * 2)
    ctx.fill()
    ctx.beginPath()
    ctx.fillStyle = VIA_DRILL_COLOR
    ctx.arc(sx, sy, Math.max(dr, 0.5), 0, Math.PI * 2)
    ctx.fill()
  }

  // ── Pads ──
  const padGlow = glowOk ? PAD_GLOW : undefined
  for (const fp of board.footprints) {
    for (const pad of fp.pads) {
      drawPad(ctx, cam, pad, PAD_COLOR, padGlow)
    }
  }

  // ── Reference labels, Google-Maps style ──
  // A label appears only once its footprint is large enough ON SCREEN to hang
  // a readable name on, and fades in as it grows: zoomed out the board is
  // clean copper (no 3,443-label smear), zoomed in every part is named. The
  // screen-size rule is self-limiting, so no explicit density cap is needed.
  const LABEL_MIN_PX = 26
  ctx.save()
  ctx.textAlign = 'center'
  ctx.textBaseline = 'bottom'
  ctx.shadowColor = 'rgba(0,0,0,0.9)'
  ctx.shadowBlur = 3
  const cw = ctx.canvas.width
  const chh = ctx.canvas.height
  for (const fp of board.footprints) {
    if (fp.pads.length === 0) continue
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity
    for (const pad of fp.pads) {
      minX = Math.min(minX, pad.at.x - pad.size.w / 2)
      maxX = Math.max(maxX, pad.at.x + pad.size.w / 2)
      minY = Math.min(minY, pad.at.y - pad.size.h / 2)
      maxY = Math.max(maxY, pad.at.y + pad.size.h / 2)
    }
    const extentPx = Math.max(maxX - minX, maxY - minY) * cam.scale
    if (extentPx < LABEL_MIN_PX) continue
    const [sx1, sy1] = ws(cam, minX, minY)
    const [sx2, sy2] = ws(cam, maxX, maxY)
    const cx = (sx1 + sx2) / 2
    const topY = Math.min(sy1, sy2)
    if (cx < -60 || cx > cw + 60 || topY < -20 || topY > chh + 20) continue
    const fade = Math.min(1, (extentPx - LABEL_MIN_PX) / 18)
    const fontPx = Math.min(13, Math.max(9, extentPx * 0.18))
    ctx.globalAlpha = fade * 0.9
    ctx.font = `${fontPx}px ui-monospace, monospace`
    ctx.fillStyle = '#cdd6e4'
    ctx.fillText(fp.ref, cx, topY - 3)
  }
  ctx.globalAlpha = 1
  ctx.restore()
}

// ────────────────────── Dynamic pass ─────────────────────────────────
// Renders per-frame data on the overlay canvas above the (blitted) static
// board: voltage tints, highlights, faults, particles, probe tooltip.

export interface OverlayData {
  /** Pulsing glow on these nets */
  highlightNets: Set<string>
  /** Dim the non-highlighted board when a highlight is active */
  dimOthers?: boolean
  /** Signal flow particles: netName → list of positions (t∈[0,1]) along each segment */
  particles: Map<string, number[]>
  /** Probe tooltip */
  probe?: { x: number; y: number; label: string; value: string }
  /** Active net voltages for colour tinting */
  netVoltages?: Map<string, number>
  /** Component state glows: ref -> state map */
  componentStates?: Record<string, Record<string, number>>
  /** Component kinds: ref -> kind string */
  componentKinds?: Record<string, string>
  /** References of faulted components for pulsing red highlight */
  faultedRefs?: Set<string>
  /** Animation time in seconds (for pulsing effects) */
  animTime?: number
}

function heatColor(t: number): string {
  // t: 0=blue, 0.5=yellow, 1=red
  const r = Math.round(Math.min(255, t * 2 * 255))
  const g = Math.round(Math.min(255, (1 - Math.abs(t - 0.5) * 2) * 200))
  const b = Math.round(Math.max(0, (1 - t * 2) * 255))
  return `rgb(${r},${g},${b})`
}

/**
 * Compute a tinted copper colour from a net voltage.
 * 0 V     = base layer colour (pass-through)
 * +5 V    = warm bright (#ffb347 blended with base)
 * negative= cool blue (#60a0ff blended with base)
 * Smooth lerp; only applied to nets present in netVoltages.
 */
function voltageTintColor(baseColor: string, voltage: number, maxV = 5): string {
  const t = Math.max(-1, Math.min(1, voltage / maxV))  // -1..1

  // Parse base color (assumes #rrggbb format)
  const br = parseInt(baseColor.slice(1, 3), 16)
  const bg = parseInt(baseColor.slice(3, 5), 16)
  const bb = parseInt(baseColor.slice(5, 7), 16)

  let tr: number, tg: number, tb: number, strength: number
  if (t > 0) {
    // Warm: brighter amber-orange for high voltage (3.3V / 5V rails)
    tr = 0xff; tg = 0xc0; tb = 0x40
    strength = t * 0.72
  } else if (t < 0) {
    // Cool blue: negative voltage
    tr = 0x60; tg = 0xa0; tb = 0xff
    strength = (-t) * 0.72
  } else {
    return baseColor
  }

  const r = Math.round(br + (tr - br) * strength)
  const g = Math.round(bg + (tg - bg) * strength)
  const b = Math.round(bb + (tb - bb) * strength)
  return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`
}

/** Stroke one copper track (segment) with optional glow. */
function strokeSeg(ctx: CanvasRenderingContext2D, cam: Camera, s: Segment, color: string, glow: string | undefined) {
  const [x1, y1] = ws(cam, s.start.x, s.start.y)
  const [x2, y2] = ws(cam, s.end.x, s.end.y)
  if (segOffscreen(ctx, x1, y1, x2, y2)) return
  ctx.beginPath()
  setStroke(ctx, color, glow, lineWidth(cam, s.width))
  ctx.moveTo(x1, y1)
  ctx.lineTo(x2, y2)
  ctx.stroke()
}

export function renderDynamicOverlay(
  ctx: CanvasRenderingContext2D,
  board: ParsedBoard,
  cam: Camera,
  overlay: OverlayData,
) {
  ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height)

  const { highlightNets, dimOthers, netVoltages, faultedRefs, animTime = 0 } = overlay
  const hasHighlight = highlightNets.size > 0
  // Glow is affordable here when the ACTIVE set is small; count as we draw.
  const glowOk = board.segments.length + board.arcs.length <= GLOW_PRIMITIVE_LIMIT

  // ── Dim veil when a net is highlighted ──
  // The static pass cannot dim per-primitive (it is cached), so a translucent
  // veil over the whole blit stands in for dimming everything else, and the
  // highlighted copper is drawn bright on top. The alpha is deliberately
  // moderate: at 0.72 the rest of the board effectively vanished and all
  // orientation context was lost; the highlight only needs contrast, not a
  // blackout.
  if (hasHighlight && dimOthers) {
    ctx.fillStyle = 'rgba(2,6,23,0.45)'
    ctx.fillRect(0, 0, ctx.canvas.width, ctx.canvas.height)
  }

  // ── Voltage-tinted copper over the static base ──
  if (netVoltages && netVoltages.size > 0 && !hasHighlight) {
    for (const layer of LAYER_ORDER) {
      if (!isCopperLayer(layer)) continue
      const style = getLayerStyle(layer)
      if (!style.visible) continue
      for (const s of board.segments) {
        if (s.layer !== layer || !s.netName) continue
        const v = netVoltages.get(s.netName)
        if (v === undefined || Math.abs(v) < 0.05) continue
        const glow = glowOk ? (v > 0 ? '#ffb347cc' : '#60a0ffcc') : undefined
        strokeSeg(ctx, cam, s, voltageTintColor(style.color, v), glow)
      }
      for (const a of board.arcs) {
        if (a.layer !== layer || !a.netName) continue
        const v = netVoltages.get(a.netName)
        if (v === undefined || Math.abs(v) < 0.05) continue
        const glow = glowOk ? (v > 0 ? '#ffb347cc' : '#60a0ffcc') : undefined
        drawArc(ctx, cam, a.start, a.mid, a.end, voltageTintColor(style.color, v), glow, a.width)
      }
    }
    ctx.shadowBlur = 0
  }

  // ── Highlighted nets: copper, arcs, vias, pads drawn bright ──
  if (hasHighlight) {
    for (const s of board.segments) {
      if (!s.netName || !highlightNets.has(s.netName)) continue
      strokeSeg(ctx, cam, s, '#ffffff', '#80c0ff')
    }
    for (const a of board.arcs) {
      if (!a.netName || !highlightNets.has(a.netName)) continue
      drawArc(ctx, cam, a.start, a.mid, a.end, '#ffffff', '#80c0ff', a.width)
    }
    ctx.shadowBlur = 0
    for (const v of board.vias) {
      if (!v.netName || !highlightNets.has(v.netName)) continue
      const [sx, sy] = ws(cam, v.at.x, v.at.y)
      const r = (v.size / 2) * cam.scale
      if (r < 0.5) continue
      ctx.beginPath()
      ctx.fillStyle = '#ffffffcc'
      ctx.shadowColor = '#80c0ff'
      ctx.shadowBlur = 6
      ctx.arc(sx, sy, r, 0, Math.PI * 2)
      ctx.fill()
      ctx.shadowBlur = 0
      ctx.beginPath()
      ctx.fillStyle = VIA_DRILL_COLOR
      ctx.arc(sx, sy, Math.max((v.drill / 2) * cam.scale, 0.5), 0, Math.PI * 2)
      ctx.fill()
    }
    for (const fp of board.footprints) {
      for (const pad of fp.pads) {
        if (!pad.netName || !highlightNets.has(pad.netName)) continue
        drawPad(ctx, cam, pad, '#ffdd44', '#ffe080')
      }
    }
  }

  // ── Faulted footprints: pulsing red pads + ring overlays ──
  if (faultedRefs && faultedRefs.size > 0) {
    const faultPulse = 0.35 + 0.65 * Math.abs(Math.sin(animTime * Math.PI * 4))
    for (const fp of board.footprints) {
      if (!faultedRefs.has(fp.ref)) continue
      const padColor = `rgba(248,${Math.round(50 + faultPulse * 50)},50,1)`
      for (const pad of fp.pads) {
        drawPad(ctx, cam, pad, padColor, '#ff2222')
      }
      const [fpX, fpY] = ws(cam, fp.at.x, fp.at.y)
      const ringR = Math.max(8, 6 * cam.scale)
      // Double ring: outer softer, inner crisp
      ctx.beginPath()
      ctx.arc(fpX, fpY, ringR * (1.4 + faultPulse * 0.5), 0, Math.PI * 2)
      ctx.strokeStyle = `rgba(248,71,71,${0.15 + faultPulse * 0.25})`
      ctx.lineWidth = 3
      ctx.shadowColor = '#ff2222'
      ctx.shadowBlur = 14 * faultPulse
      ctx.stroke()

      ctx.beginPath()
      ctx.arc(fpX, fpY, ringR * (1 + faultPulse * 0.2), 0, Math.PI * 2)
      ctx.strokeStyle = `rgba(248,71,71,${0.5 + faultPulse * 0.5})`
      ctx.lineWidth = 1.5
      ctx.shadowColor = '#ff3333'
      ctx.shadowBlur = 8 * faultPulse
      ctx.stroke()
      ctx.shadowBlur = 0
    }
  }

  // ── Component state glows ──
  if (overlay.componentStates && overlay.componentKinds) {
    // The gradient fills are pretty but not free; a 3,000-component board with
    // every part dissipating would otherwise pay thousands of radial gradients
    // per frame. Cull offscreen parts and cap the total.
    let drawn = 0
    const GLOW_CAP = 400
    for (const fp of board.footprints) {
      if (drawn >= GLOW_CAP) break
      const states = overlay.componentStates[fp.ref]
      const kind = overlay.componentKinds[fp.ref]
      if (!states && !kind) continue

      const [fpX, fpY] = ws(cam, fp.at.x, fp.at.y)
      const radius = Math.max(20, 12 * cam.scale)
      if (fpX + radius < 0 || fpX - radius > ctx.canvas.width || fpY + radius < 0 || fpY - radius > ctx.canvas.height) continue

      const running = states?.['running'] ?? 0
      const dissipation = states?.['dissipation_mw'] ?? 0

      if (kind === 'mcu' && running > 0) {
        // Faint cyan glow for running MCU
        const grad = ctx.createRadialGradient(fpX, fpY, 0, fpX, fpY, radius)
        grad.addColorStop(0, 'rgba(34,211,238,0.18)')
        grad.addColorStop(0.6, 'rgba(34,211,238,0.06)')
        grad.addColorStop(1, 'rgba(34,211,238,0)')
        ctx.fillStyle = grad
        ctx.beginPath()
        ctx.arc(fpX, fpY, radius, 0, Math.PI * 2)
        ctx.fill()
        drawn++
      } else if (dissipation > 0) {
        // Heat color glow for dissipation
        const t = Math.min(1, dissipation / 500)
        const color = heatColor(t)
        const grad = ctx.createRadialGradient(fpX, fpY, 0, fpX, fpY, radius)
        grad.addColorStop(0, color.replace('rgb', 'rgba').replace(')', `,${0.25 * t + 0.05})`))
        grad.addColorStop(1, 'rgba(0,0,0,0)')
        ctx.fillStyle = grad
        ctx.beginPath()
        ctx.arc(fpX, fpY, radius, 0, Math.PI * 2)
        ctx.fill()
        drawn++
      }
    }
  }

  // ── Net highlight glow pulses ──
  for (const netName of overlay.highlightNets) {
    for (const s of board.segments) {
      if (s.netName !== netName) continue
      const [x1, y1] = ws(cam, s.start.x, s.start.y)
      const [x2, y2] = ws(cam, s.end.x, s.end.y)
      if (segOffscreen(ctx, x1, y1, x2, y2)) continue
      ctx.beginPath()
      ctx.strokeStyle = 'rgba(100,200,255,0.35)'
      ctx.lineWidth = Math.max(2, s.width * cam.scale * 3)
      ctx.lineCap = 'round'
      ctx.shadowColor = '#40a0ff'
      ctx.shadowBlur = 12
      ctx.moveTo(x1, y1)
      ctx.lineTo(x2, y2)
      ctx.stroke()
    }
    ctx.shadowBlur = 0
  }

  // ── Signal flow particles ──
  ctx.shadowBlur = 0
  for (const [netName, positions] of overlay.particles) {
    for (const s of board.segments) {
      if (s.netName !== netName) continue
      for (const t of positions) {
        const px = s.start.x + (s.end.x - s.start.x) * t
        const py = s.start.y + (s.end.y - s.start.y) * t
        const [sx, sy] = ws(cam, px, py)
        const r = Math.max(2, s.width * cam.scale * 0.6)
        ctx.beginPath()
        ctx.fillStyle = '#60ff80'
        ctx.shadowColor = '#00ff40'
        ctx.shadowBlur = r * 2
        ctx.arc(sx, sy, r, 0, Math.PI * 2)
        ctx.fill()
        ctx.shadowBlur = 0
      }
    }
  }

  // ── Probe tooltip ──
  if (overlay.probe) {
    const { x, y, label, value } = overlay.probe
    const [sx, sy] = ws(cam, x, y)
    const padding = 8
    const fontSize = 12
    ctx.font = `bold ${fontSize}px 'JetBrains Mono', monospace`
    const labelW = ctx.measureText(label).width
    const valueW = ctx.measureText(value).width
    const boxW = Math.max(labelW, valueW) + padding * 2
    const boxH = fontSize * 2 + padding * 2 + 4

    const bx = sx + 12
    const by = sy - boxH - 8

    ctx.fillStyle = 'rgba(15, 23, 42, 0.92)'
    ctx.strokeStyle = '#3b82f6'
    ctx.lineWidth = 1.5
    ctx.shadowColor = '#3b82f6'
    ctx.shadowBlur = 8
    ctx.beginPath()
    ctx.roundRect(bx, by, boxW, boxH, 6)
    ctx.fill()
    ctx.stroke()
    ctx.shadowBlur = 0

    ctx.fillStyle = '#94a3b8'
    ctx.fillText(label, bx + padding, by + padding + fontSize)
    ctx.fillStyle = '#60a5fa'
    ctx.fillText(value, bx + padding, by + padding + fontSize * 2 + 4)

    // Crosshair dot
    ctx.beginPath()
    ctx.arc(sx, sy, 4, 0, Math.PI * 2)
    ctx.fillStyle = '#60a5fa'
    ctx.fill()
  }
}
