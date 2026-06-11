// Board renderer: draws a ParsedBoard onto an HTML canvas.
// All draw calls go through the Camera transform.

import type { Camera } from './camera'
import { worldToScreen } from './camera'
import type {
  ParsedBoard, Pad, Point,
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
  if (glowColor && lw > 1.5) {
    ctx.shadowColor = glowColor
    ctx.shadowBlur = Math.min(lw * 1.5, 8)
  } else {
    ctx.shadowBlur = 0
  }
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

function drawPad(ctx: CanvasRenderingContext2D, cam: Camera, pad: Pad, color: string, glowColor: string, alpha = 1) {
  const [sx, sy] = ws(cam, pad.at.x, pad.at.y)
  const w = pad.size.w * cam.scale
  const h = pad.size.h * cam.scale
  const minDim = Math.min(w, h)
  if (minDim < 0.5) return

  ctx.save()
  ctx.globalAlpha = alpha
  ctx.fillStyle = color
  ctx.shadowColor = glowColor
  ctx.shadowBlur = Math.min(w * 0.4, 5)
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

// ────────────────────── Main render function ─────────────────────────

export interface RenderOptions {
  highlightNets?: Set<string>
  dimOthers?: boolean
  /** net name → voltage, used to tint copper traces */
  netVoltages?: Map<string, number>
  /** References of faulted components for pulsing red highlight */
  faultedRefs?: Set<string>
  /** Animation time in seconds (for pulsing effects) */
  animTime?: number
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
    // Warm: #ffb347
    tr = 0xff; tg = 0xb3; tb = 0x47
    strength = t * 0.55
  } else if (t < 0) {
    // Cool blue: #60a0ff
    tr = 0x60; tg = 0xa0; tb = 0xff
    strength = (-t) * 0.55
  } else {
    return baseColor
  }

  const r = Math.round(br + (tr - br) * strength)
  const g = Math.round(bg + (tg - bg) * strength)
  const b = Math.round(bb + (tb - bb) * strength)
  return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`
}

export function renderBoard(
  ctx: CanvasRenderingContext2D,
  board: ParsedBoard,
  cam: Camera,
  opts: RenderOptions = {},
) {
  ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height)

  // Background
  ctx.fillStyle = '#020617'
  ctx.fillRect(0, 0, ctx.canvas.width, ctx.canvas.height)

  const { highlightNets, dimOthers, netVoltages, faultedRefs, animTime = 0 } = opts
  const hasHighlight = highlightNets && highlightNets.size > 0
  const hasVoltages = netVoltages && netVoltages.size > 0

  // Pulsing factor for fault highlight (0..1, 1Hz)
  const faultPulse = faultedRefs && faultedRefs.size > 0
    ? 0.4 + 0.6 * Math.abs(Math.sin(animTime * Math.PI * 2))
    : 0

  // ── Board graphics, grouped by layer ──
  const layersToRender = [...LAYER_ORDER]

  for (const layer of layersToRender) {
    const style = getLayerStyle(layer)
    if (!style.visible) continue

    const isCopper = isCopperLayer(layer)
    const color = style.color
    const glow = style.glow

    // gr_lines on this layer
    for (const l of board.gr_lines) {
      if (l.layer !== layer) continue
      const lw = lineWidth(cam, l.width)
      ctx.beginPath()
      setStroke(ctx, color, glow, lw)
      const [x1, y1] = ws(cam, l.start.x, l.start.y)
      const [x2, y2] = ws(cam, l.end.x, l.end.y)
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
        const lw = lineWidth(cam, l.width)
        ctx.beginPath()
        setStroke(ctx, color, glow, lw)
        const [x1, y1] = ws(cam, l.start.x, l.start.y)
        const [x2, y2] = ws(cam, l.end.x, l.end.y)
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

    // Tracks / segments on this copper layer
    if (isCopper) {
      for (const s of board.segments) {
        if (s.layer !== layer) continue
        const isHighlighted = hasHighlight && s.netName != null && highlightNets!.has(s.netName)
        const alpha = hasHighlight && dimOthers && !isHighlighted ? 0.15 : 1
        if (alpha < 1) ctx.globalAlpha = alpha

        let trackColor = color
        let trackGlow = glow
        if (isHighlighted) {
          trackColor = '#ffffff'
          trackGlow = '#80c0ff'
        } else if (hasVoltages && s.netName && netVoltages!.has(s.netName)) {
          const v = netVoltages!.get(s.netName)!
          trackColor = voltageTintColor(color, v)
          // Bloom-like glow on active traces
          if (Math.abs(v) > 0.05) {
            trackGlow = v > 0 ? '#ffb34788' : '#60a0ff88'
          }
        }

        const lw = lineWidth(cam, s.width)
        ctx.beginPath()
        setStroke(ctx, trackColor, trackGlow, lw)
        const [x1, y1] = ws(cam, s.start.x, s.start.y)
        const [x2, y2] = ws(cam, s.end.x, s.end.y)
        ctx.moveTo(x1, y1)
        ctx.lineTo(x2, y2)
        ctx.stroke()
        ctx.globalAlpha = 1
      }

      for (const a of board.arcs) {
        if (a.layer !== layer) continue
        const isHighlighted = hasHighlight && a.netName != null && highlightNets!.has(a.netName)
        const alpha = hasHighlight && dimOthers && !isHighlighted ? 0.15 : 1
        if (alpha < 1) ctx.globalAlpha = alpha

        let trackColor = color
        let trackGlow = glow
        if (isHighlighted) {
          trackColor = '#ffffff'
          trackGlow = '#80c0ff'
        } else if (hasVoltages && a.netName && netVoltages!.has(a.netName)) {
          const v = netVoltages!.get(a.netName)!
          trackColor = voltageTintColor(color, v)
          if (Math.abs(v) > 0.05) trackGlow = v > 0 ? '#ffb34788' : '#60a0ff88'
        }

        drawArc(ctx, cam, a.start, a.mid, a.end, trackColor, trackGlow, a.width)
        ctx.globalAlpha = 1
      }
    }
  }

  // ── Vias ──
  ctx.shadowBlur = 0
  for (const v of board.vias) {
    const isHighlighted = hasHighlight && v.netName != null && highlightNets!.has(v.netName)
    const alpha = hasHighlight && dimOthers && !isHighlighted ? 0.2 : 1
    const [sx, sy] = ws(cam, v.at.x, v.at.y)
    const r = (v.size / 2) * cam.scale
    const dr = (v.drill / 2) * cam.scale
    if (r < 0.5) continue
    ctx.globalAlpha = alpha
    ctx.beginPath()
    ctx.fillStyle = isHighlighted ? '#ffffffcc' : VIA_COLOR
    if (isHighlighted) { ctx.shadowColor = '#80c0ff'; ctx.shadowBlur = 6 }
    ctx.arc(sx, sy, r, 0, Math.PI * 2)
    ctx.fill()
    ctx.shadowBlur = 0
    ctx.beginPath()
    ctx.fillStyle = VIA_DRILL_COLOR
    ctx.arc(sx, sy, Math.max(dr, 0.5), 0, Math.PI * 2)
    ctx.fill()
    ctx.globalAlpha = 1
  }

  // ── Pads ──
  for (const fp of board.footprints) {
    const isFaulted = faultedRefs?.has(fp.ref) ?? false
    for (const pad of fp.pads) {
      const isHighlighted = hasHighlight && pad.netName != null && highlightNets!.has(pad.netName)
      const alpha = hasHighlight && dimOthers && !isHighlighted && !isFaulted ? 0.15 : 1
      let padColor = PAD_COLOR
      let padGlow = PAD_GLOW
      if (isFaulted) {
        padColor = `rgba(248,${Math.round(50 + faultPulse * 50)},50,1)`
        padGlow = '#ff2222'
      } else if (isHighlighted) {
        padColor = '#ffdd44'
        padGlow = '#ffe080'
      }
      drawPad(ctx, cam, pad, padColor, padGlow, alpha)
    }
  }

  // ── Faulted footprint ring overlays ──
  if (faultedRefs && faultedRefs.size > 0) {
    for (const fp of board.footprints) {
      if (!faultedRefs.has(fp.ref)) continue
      const [fpX, fpY] = ws(cam, fp.at.x, fp.at.y)
      const ringR = Math.max(8, 6 * cam.scale)
      ctx.beginPath()
      ctx.arc(fpX, fpY, ringR * (1 + faultPulse * 0.3), 0, Math.PI * 2)
      ctx.strokeStyle = `rgba(248,71,71,${0.3 + faultPulse * 0.5})`
      ctx.lineWidth = 2
      ctx.shadowColor = '#ff2222'
      ctx.shadowBlur = 10 * faultPulse
      ctx.stroke()
      ctx.shadowBlur = 0
    }
  }
}

// ────────────────────── Overlay renderer ─────────────────────────────
// Renders dynamic data on a second (overlay) canvas on top of the board canvas.

export interface OverlayData {
  /** Pulsing glow on these nets */
  highlightNets: Set<string>
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
}

function heatColor(t: number): string {
  // t: 0=blue, 0.5=yellow, 1=red
  const r = Math.round(Math.min(255, t * 2 * 255))
  const g = Math.round(Math.min(255, (1 - Math.abs(t - 0.5) * 2) * 200))
  const b = Math.round(Math.max(0, (1 - t * 2) * 255))
  return `rgb(${r},${g},${b})`
}

export function renderOverlay(
  ctx: CanvasRenderingContext2D,
  board: ParsedBoard,
  cam: Camera,
  overlay: OverlayData,
) {
  ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height)

  // ── Component state glows ──
  if (overlay.componentStates && overlay.componentKinds) {
    for (const fp of board.footprints) {
      const states = overlay.componentStates[fp.ref]
      const kind = overlay.componentKinds[fp.ref]
      if (!states && !kind) continue

      const [fpX, fpY] = ws(cam, fp.at.x, fp.at.y)
      const radius = Math.max(20, 12 * cam.scale)

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
      }
    }
  }

  // ── Net highlight glow pulses (driven from outside with animation) ──
  for (const netName of overlay.highlightNets) {
    for (const s of board.segments) {
      if (s.netName !== netName) continue
      const [x1, y1] = ws(cam, s.start.x, s.start.y)
      const [x2, y2] = ws(cam, s.end.x, s.end.y)
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
