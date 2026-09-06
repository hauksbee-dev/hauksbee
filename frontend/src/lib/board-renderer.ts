// Board renderer: draws a ParsedBoard onto HTML canvases through the Camera.
//
// Two passes so a large board stays interactive:
//   renderStaticBoard: everything that only depends on the camera (copper,
//     graphics, pads, vias, labels). Drawn into an offscreen canvas the viewer
//     blits every animation frame and re-renders only when the camera settles.
//   renderDynamicOverlay: everything that changes per frame (voltage tints,
//     net highlights, fault pulses, particles, probe tooltip, component
//     glows). Small working sets, so it can run at full frame rate.

import type { Camera } from './camera'
import { worldToScreen } from './camera'
import type { Graphics, Pad, ParsedBoard, Point, Segment, TrackArc, Via } from './kicad-parser'
import { allStrokes } from './kicad-parser'
import { boxesByRef, labelAnchor, LABEL_MIN_PX } from './board-geometry'
import type { FootprintBox } from './board-geometry'
import type { ImportMarker } from './report-view'
import { getLayerStyle, isCopperLayer, boardTheme } from './layer-colors'

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

function countPrimitives(board: ParsedBoard): number {
  let n = board.segments.length + board.arcs.length + board.vias.length + allStrokes(board.graphics).length
  for (const fp of board.footprints) n += fp.pads.length + allStrokes(fp.graphics).length
  return n
}

// ── Canvas helpers ───────────────────────────────────────────────────────────

type Ctx = CanvasRenderingContext2D

function ws(cam: Camera, p: Point): [number, number] {
  const { sx, sy } = worldToScreen(cam, p.x, p.y)
  return [sx, sy]
}

const lineWidth = (cam: Camera, mm: number) => Math.max(0.5, mm * cam.scale)

function setStroke(ctx: Ctx, color: string, glowColor: string | undefined, lw: number) {
  ctx.strokeStyle = color
  ctx.lineWidth = lw
  ctx.lineCap = 'round'
  ctx.lineJoin = 'round'
  if (glowColor && lw > 1.0) {
    ctx.shadowColor = glowColor
    ctx.shadowBlur = Math.min(lw * 2.5, 14)
  } else ctx.shadowBlur = 0
}

/** True when the screen-space segment cannot touch the canvas (with margin). */
function segOffscreen(ctx: Ctx, x1: number, y1: number, x2: number, y2: number): boolean {
  const m = 20, w = ctx.canvas.width, h = ctx.canvas.height
  return (x1 < -m && x2 < -m) || (x1 > w + m && x2 > w + m) || (y1 < -m && y2 < -m) || (y1 > h + m && y2 > h + m)
}

function offscreenCircle(ctx: Ctx, sx: number, sy: number, r: number): boolean {
  return sx + r < 0 || sx - r > ctx.canvas.width || sy + r < 0 || sy - r > ctx.canvas.height
}

/** Fill or stroke the current path. */
function finish(ctx: Ctx, color: string, fill: boolean | undefined) {
  if (fill) { ctx.fillStyle = color; ctx.fill() } else ctx.stroke()
}

/** KiCad arcs are start / mid (on the arc) / end: reconstruct the circle. */
function arcFromThreePoints(p1: Point, p2: Point, p3: Point): { cx: number; cy: number; r: number; startAngle: number; endAngle: number; ccw: boolean } | null {
  const D = 2 * (p1.x * (p2.y - p3.y) + p2.x * (p3.y - p1.y) + p3.x * (p1.y - p2.y))
  if (Math.abs(D) < 1e-10) return null
  const q1 = p1.x * p1.x + p1.y * p1.y, q2 = p2.x * p2.x + p2.y * p2.y, q3 = p3.x * p3.x + p3.y * p3.y
  const ux = (q1 * (p2.y - p3.y) + q2 * (p3.y - p1.y) + q3 * (p1.y - p2.y)) / D
  const uy = (q1 * (p3.x - p2.x) + q2 * (p1.x - p3.x) + q3 * (p2.x - p1.x)) / D
  const startAngle = Math.atan2(p1.y - uy, p1.x - ux)
  const endAngle = Math.atan2(p3.y - uy, p3.x - ux)
  const norm = (a: number) => ((a % (2 * Math.PI)) + 2 * Math.PI) % (2 * Math.PI)
  const sa = norm(startAngle), ma = norm(Math.atan2(p2.y - uy, p2.x - ux)), ea = norm(endAngle)
  // CCW if the midpoint sits between start and end in the CCW sense.
  const ccw = (sa <= ma && ma <= ea) || (ea < sa && (ma >= sa || ma <= ea))
  return { cx: ux, cy: uy, r: Math.hypot(p1.x - ux, p1.y - uy), startAngle, endAngle, ccw }
}

/** A straight stroke between two board points, culled when it cannot touch
 *  the canvas. */
function drawLine(ctx: Ctx, cam: Camera, start: Point, end: Point, color: string, glow: string | undefined, width: number) {
  const [x1, y1] = ws(cam, start), [x2, y2] = ws(cam, end)
  if (segOffscreen(ctx, x1, y1, x2, y2)) return
  ctx.beginPath()
  setStroke(ctx, color, glow, lineWidth(cam, width))
  ctx.moveTo(x1, y1)
  ctx.lineTo(x2, y2)
  ctx.stroke()
}

function drawArc(ctx: Ctx, cam: Camera, a: { start: Point; mid: Point; end: Point; width: number }, color: string, glow: string | undefined) {
  const arc = arcFromThreePoints(a.start, a.mid, a.end)
  if (!arc) { drawLine(ctx, cam, a.start, a.end, color, glow, a.width); return }
  const [cx, cy] = ws(cam, { x: arc.cx, y: arc.cy })
  ctx.beginPath()
  setStroke(ctx, color, glow, lineWidth(cam, a.width))
  ctx.arc(cx, cy, arc.r * cam.scale, arc.startAngle, arc.endAngle, arc.ccw)
  ctx.stroke()
}

/** A copper track: a segment or an arc, whichever it is. */
function drawTrack(ctx: Ctx, cam: Camera, t: Segment | TrackArc, color: string, glow: string | undefined) {
  if ('mid' in t) drawArc(ctx, cam, t, color, glow)
  else drawLine(ctx, cam, t.start, t.end, color, glow, t.width)
}

/** Every stroke of a graphics set that sits on `layer`. */
function drawGraphics(ctx: Ctx, cam: Camera, g: Graphics, layer: string, color: string, glow: string | undefined) {
  for (const l of g.lines) if (l.layer === layer) drawLine(ctx, cam, l.start, l.end, color, glow, l.width)
  for (const a of g.arcs) if (a.layer === layer) drawArc(ctx, cam, a, color, glow)
  for (const c of g.circles) {
    if (c.layer !== layer) continue
    const [sx, sy] = ws(cam, c.center)
    const r = Math.hypot(c.end.x - c.center.x, c.end.y - c.center.y) * cam.scale
    if (r < 0.5) continue
    setStroke(ctx, color, glow, lineWidth(cam, c.width))
    ctx.beginPath()
    ctx.arc(sx, sy, r, 0, Math.PI * 2)
    finish(ctx, color, c.fill)
  }
  for (const r of g.rects) {
    if (r.layer !== layer) continue
    const [x1, y1] = ws(cam, r.start), [x2, y2] = ws(cam, r.end)
    setStroke(ctx, color, glow, lineWidth(cam, r.width))
    ctx.beginPath()
    ctx.rect(Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1))
    finish(ctx, color, r.fill)
  }
  for (const p of g.polys) {
    if (p.layer !== layer || p.pts.length < 2) continue
    ctx.beginPath()
    p.pts.forEach((pt, i) => { const [x, y] = ws(cam, pt); if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y) })
    ctx.closePath()
    setStroke(ctx, color, glow, lineWidth(cam, p.width))
    finish(ctx, color, p.fill)
  }
}

/** A via: annulus in `ring`, then its drill. */
function drawVia(ctx: Ctx, cam: Camera, via: Via, ring: string, glow?: string) {
  const [sx, sy] = ws(cam, via.at)
  const r = (via.size / 2) * cam.scale
  if (r < 0.5 || offscreenCircle(ctx, sx, sy, r)) return
  ctx.beginPath()
  ctx.fillStyle = ring
  if (glow) { ctx.shadowColor = glow; ctx.shadowBlur = 6 }
  ctx.arc(sx, sy, r, 0, Math.PI * 2)
  ctx.fill()
  ctx.shadowBlur = 0
  ctx.beginPath()
  ctx.fillStyle = boardTheme().viaDrill
  ctx.arc(sx, sy, Math.max((via.drill / 2) * cam.scale, 0.5), 0, Math.PI * 2)
  ctx.fill()
}

/** A footprint's drawn extent as a padded screen rectangle. */
function boxRect(cam: Camera, box: FootprintBox, pad = 3): [number, number, number, number] {
  const [x1, y1] = ws(cam, { x: box.x1, y: box.y1 }), [x2, y2] = ws(cam, { x: box.x2, y: box.y2 })
  return [Math.min(x1, x2) - pad, Math.min(y1, y2) - pad, Math.abs(x2 - x1) + pad * 2, Math.abs(y2 - y1) + pad * 2]
}

function drawPad(ctx: Ctx, cam: Camera, pad: Pad, color: string, glowColor: string | undefined) {
  const [sx, sy] = ws(cam, pad.at)
  const w = pad.size.w * cam.scale, h = pad.size.h * cam.scale
  if (Math.min(w, h) < 0.5 || offscreenCircle(ctx, sx, sy, Math.max(w, h))) return
  ctx.save()
  ctx.fillStyle = color
  if (glowColor) { ctx.shadowColor = glowColor; ctx.shadowBlur = Math.min(w * 0.6, 8) }
  ctx.translate(sx, sy)
  ctx.rotate((pad.angle * Math.PI) / 180)
  ctx.beginPath()
  switch (pad.shape) {
    case 'circle': ctx.arc(0, 0, w / 2, 0, Math.PI * 2); break
    case 'oval': case 'roundrect': ctx.roundRect(-w / 2, -h / 2, w, h, Math.min(w, h) / 2); break
    default: ctx.rect(-w / 2, -h / 2, w, h)
  }
  ctx.fill()
  const drill = pad.drill
  if (drill && (pad.type === 'thru_hole' || pad.type === 'np_thru_hole')) {
    ctx.shadowBlur = 0
    ctx.fillStyle = boardTheme().viaDrill
    ctx.beginPath()
    if (drill.oval && drill.dx && drill.dy) ctx.ellipse(0, 0, (drill.dx * cam.scale) / 2, (drill.dy * cam.scale) / 2, 0, 0, Math.PI * 2)
    else ctx.arc(0, 0, Math.max((drill.diameter / 2) * cam.scale, 0.5), 0, Math.PI * 2)
    ctx.fill()
  }
  ctx.restore()
}

// ── Static pass ──────────────────────────────────────────────────────────────

/** View options the Layers panel controls. Everything defaults to the layer
 *  palette's own visibility, so callers without a panel change nothing. */
export interface RenderOptions {
  /** Per-layer visibility override; unlisted layers use the palette default. */
  layerVisible?: (layer: string) => boolean
  /** Pads (and their drills). Default true. */
  showPads?: boolean
  /** Reference labels. Default true. */
  showLabels?: boolean
}

const layerOn = (layer: string, opts?: RenderOptions) => (opts?.layerVisible ? opts.layerVisible(layer) : getLayerStyle(layer).visible)

/** Draw everything that depends only on the camera: board graphics, copper in
 *  its base layer colours, pads, vias, and reference labels. */
export function renderStaticBoard(ctx: Ctx, board: ParsedBoard, cam: Camera, opts?: RenderOptions) {
  const W = ctx.canvas.width, H = ctx.canvas.height
  const theme = boardTheme()
  ctx.clearRect(0, 0, W, H)
  ctx.fillStyle = theme.bg
  ctx.fillRect(0, 0, W, H)
  // Subtle radial vignette so the board area stands out against the flat background.
  const grad = ctx.createRadialGradient(W / 2, H / 2, 0, W / 2, H / 2, Math.max(W, H) * 0.65)
  grad.addColorStop(0, theme.vignette0)
  grad.addColorStop(1, theme.vignette1)
  ctx.fillStyle = grad
  ctx.fillRect(0, 0, W, H)

  const glowOk = countPrimitives(board) <= GLOW_PRIMITIVE_LIMIT

  for (const layer of LAYER_ORDER) {
    if (!layerOn(layer, opts)) continue
    const { color, glow: layerGlow } = getLayerStyle(layer)
    const glow = glowOk ? layerGlow : undefined
    drawGraphics(ctx, cam, board.graphics, layer, color, glow)
    for (const fp of board.footprints) drawGraphics(ctx, cam, fp.graphics, layer, color, glow)
    // Copper in its base colour; live voltage tints are painted over by the
    // dynamic pass.
    if (isCopperLayer(layer)) {
      for (const t of [...board.segments, ...board.arcs]) if (t.layer === layer) drawTrack(ctx, cam, t, color, glow)
    }
  }

  ctx.shadowBlur = 0
  for (const v of board.vias) drawVia(ctx, cam, v, theme.via)

  if (opts?.showPads !== false) {
    const padGlow = glowOk ? theme.padGlow : undefined
    for (const fp of board.footprints) for (const pad of fp.pads) drawPad(ctx, cam, pad, theme.pad, padGlow)
  }

  // Reference labels, Google-Maps style: a label appears only once its
  // footprint is large enough ON SCREEN to hang a readable name on, and fades
  // in as it grows. Zoomed out the board is clean copper (no 3,443-label
  // smear), zoomed in every part is named.
  if (opts?.showLabels === false) return
  ctx.save()
  ctx.textAlign = 'center'
  ctx.textBaseline = 'bottom'
  ctx.shadowColor = theme.labelShadow
  ctx.shadowBlur = 3
  for (const fp of board.footprints) {
    const anchor = labelAnchor(fp, cam)
    if (!anchor) continue
    const { cx, topY, fontPx, extentPx } = anchor
    if (cx < -60 || cx > W + 60 || topY < -20 || topY > H + 20) continue
    ctx.globalAlpha = Math.min(1, (extentPx - LABEL_MIN_PX) / 18) * 0.9
    ctx.font = `${fontPx}px ui-monospace, monospace`
    ctx.fillStyle = theme.label
    ctx.fillText(fp.ref, cx, topY - 3)
  }
  ctx.globalAlpha = 1
  ctx.restore()
}

// ── Dynamic pass ─────────────────────────────────────────────────────────────

/** Apply the same net selection to import markers as to the copper beneath
 *  them. With no selected net the coverage overlay shows every located object;
 *  with one selected it shows only objects whose recovered pins name that net. */
export function visibleImportMarkers(markers: ImportMarker[], highlightNets: Set<string>): ImportMarker[] {
  if (highlightNets.size === 0) return markers
  return markers.filter(point => point.nets.some(net => highlightNets.has(net)))
}

export interface OverlayData {
  /** Pulsing glow on these nets */
  highlightNets: Set<string>
  /** A "show on board" marker (board mm): pulsing ring + optional label. */
  marker?: { x: number; y: number; label?: string } | null
  /** Import completeness markers: only objects with real source coordinates. */
  importMarkers?: ImportMarker[]
  /** Dim the non-highlighted board when a highlight is active */
  dimOthers?: boolean
  /** Signal flow particles: netName → positions (t∈[0,1]) along each segment */
  particles: Map<string, number[]>
  probe?: { x: number; y: number; label: string; value: string }
  netVoltages?: Map<string, number>
  componentStates?: Record<string, Record<string, number>>
  componentKinds?: Record<string, string>
  /** References of faulted components for pulsing red highlight */
  faultedRefs?: Set<string>
  /** The footprint under the cursor: the same highlight ink as a hovered net,
   *  over the same extent the click hit-test uses. Steady, not pulsing. */
  hoverRefs?: Set<string>
  /** Animation time in seconds (for pulsing effects) */
  animTime?: number
  /** The activity overlay (voltage tints, flow particles, component glows).
   *  Default on; the Layers panel can switch it off to read bare copper. */
  showActivity?: boolean
  /** Layer/pad visibility, shared with the static pass so the overlay never
   *  paints activity on copper the user hid. */
  renderOpts?: RenderOptions
}

function heatColor(t: number): string {
  // t: 0=blue, 0.5=yellow, 1=red
  const r = Math.round(Math.min(255, t * 2 * 255))
  const g = Math.round(Math.min(255, (1 - Math.abs(t - 0.5) * 2) * 200))
  const b = Math.round(Math.max(0, (1 - t * 2) * 255))
  return `rgb(${r},${g},${b})`
}

/** Copper tinted by net voltage: 0 V is the base layer colour, positive
 *  blends toward the theme's rail-amber, negative toward its blue. */
function voltageTintColor(baseColor: string, voltage: number, maxV = 5): string {
  const t = Math.max(-1, Math.min(1, voltage / maxV))
  if (t === 0) return baseColor
  const theme = boardTheme()
  const target = t > 0 ? theme.voltWarm : theme.voltCool
  const strength = Math.abs(t) * 0.72
  const mix = (base: number, to: number) => Math.round(base + (to - base) * strength).toString(16).padStart(2, '0')
  return `#${mix(parseInt(baseColor.slice(1, 3), 16), target.r)}${mix(parseInt(baseColor.slice(3, 5), 16), target.g)}${mix(parseInt(baseColor.slice(5, 7), 16), target.b)}`
}

/** A rounded, glowing label box, for the probe tooltip and the marker. */
function labelBox(ctx: Ctx, x: number, y: number, w: number, h: number, bg: string, border: string, radius: number) {
  ctx.fillStyle = bg
  ctx.strokeStyle = border
  ctx.beginPath()
  ctx.roundRect(x, y, w, h, radius)
  ctx.fill()
  ctx.stroke()
}

export function renderDynamicOverlay(ctx: Ctx, board: ParsedBoard, cam: Camera, overlay: OverlayData) {
  const W = ctx.canvas.width, H = ctx.canvas.height
  ctx.clearRect(0, 0, W, H)
  const theme = boardTheme()
  const { highlightNets, dimOthers, netVoltages, faultedRefs, animTime = 0, renderOpts } = overlay
  const activityOn = overlay.showActivity !== false
  const hasHighlight = highlightNets.size > 0
  const glowOk = board.segments.length + board.arcs.length <= GLOW_PRIMITIVE_LIMIT
  const tracks = [...board.segments, ...board.arcs]

  // Dim veil when a net is highlighted: the static pass cannot dim
  // per-primitive (it is cached), so a translucent veil stands in and the
  // highlighted copper is drawn bright on top. Moderate alpha on purpose: the
  // highlight needs contrast, not a blackout.
  if (hasHighlight && dimOthers) {
    ctx.fillStyle = theme.dimVeil
    ctx.fillRect(0, 0, W, H)
  }

  // Voltage-tinted copper over the static base.
  if (activityOn && netVoltages && netVoltages.size > 0 && !hasHighlight) {
    for (const layer of LAYER_ORDER) {
      if (!isCopperLayer(layer) || !layerOn(layer, renderOpts)) continue
      const base = getLayerStyle(layer).color
      for (const t of tracks) {
        if (t.layer !== layer || !t.netName) continue
        const v = netVoltages.get(t.netName)
        if (v === undefined || Math.abs(v) < 0.05) continue
        drawTrack(ctx, cam, t, voltageTintColor(base, v), glowOk ? (v > 0 ? theme.voltWarmGlow : theme.voltCoolGlow) : undefined)
      }
    }
    ctx.shadowBlur = 0
  }

  // Highlighted nets: copper, vias, pads drawn bright.
  if (hasHighlight) {
    for (const t of tracks) if (t.netName && highlightNets.has(t.netName)) drawTrack(ctx, cam, t, theme.highlight, theme.highlightGlow)
    ctx.shadowBlur = 0
    for (const v of board.vias) if (v.netName && highlightNets.has(v.netName)) drawVia(ctx, cam, v, theme.highlightVia, theme.highlightGlow)
    for (const fp of board.footprints) {
      for (const pad of fp.pads) if (pad.netName && highlightNets.has(pad.netName)) drawPad(ctx, cam, pad, theme.highlightPad, theme.highlightPadGlow)
    }
  }

  // Hovered footprint: the part answers the cursor, over the same extent the
  // click hit-test resolves against. Drawn before the fault pass so a faulted
  // part under the cursor still reads as faulted.
  const hoverRefs = overlay.hoverRefs
  if (hoverRefs && hoverRefs.size > 0) {
    const boxes = boxesByRef(board)
    for (const fp of board.footprints) {
      if (!hoverRefs.has(fp.ref)) continue
      const box = boxes.get(fp.ref)
      if (box) {
        ctx.strokeStyle = theme.highlight
        ctx.lineWidth = 1.5
        ctx.shadowColor = theme.highlightGlow
        ctx.shadowBlur = 8
        ctx.strokeRect(...boxRect(cam, box))
        ctx.shadowBlur = 0
      }
      for (const p of fp.pads) drawPad(ctx, cam, p, theme.highlightPad, theme.highlightPadGlow)
    }
  }

  // Faulted footprints: the part itself turns red. Painting its own extent
  // (body plus pads) is the message with no indirection; the pulse rides
  // alpha only, so the shape stays stable.
  if (faultedRefs && faultedRefs.size > 0) {
    const faultPulse = 0.35 + 0.65 * Math.abs(Math.sin(animTime * Math.PI * 4))
    const boxes = boxesByRef(board)
    for (const fp of board.footprints) {
      if (!faultedRefs.has(fp.ref)) continue
      const box = boxes.get(fp.ref)
      if (box) {
        const [bx, by, bw, bh] = boxRect(cam, box)
        ctx.fillStyle = `rgba(${theme.faultFillRGB},${0.18 + faultPulse * 0.2})`
        ctx.fillRect(bx, by, bw, bh)
        ctx.strokeStyle = `rgba(${theme.faultStrokeRGB},${0.55 + faultPulse * 0.45})`
        ctx.lineWidth = 1.5
        ctx.shadowColor = theme.faultShadow
        ctx.shadowBlur = 10 * faultPulse
        ctx.strokeRect(bx, by, bw, bh)
        ctx.shadowBlur = 0
      }
      const padColor = theme.faultPad(faultPulse)
      for (const p of fp.pads) drawPad(ctx, cam, p, padColor, theme.faultPadGlow)
    }
  }

  // Component state glows. Radial gradients are not free: cull offscreen
  // parts and cap the total so a 3,000-part board dissipating everywhere does
  // not pay thousands per frame.
  if (activityOn && overlay.componentStates && overlay.componentKinds) {
    let drawn = 0
    for (const fp of board.footprints) {
      if (drawn >= 400) break
      const states = overlay.componentStates[fp.ref]
      const kind = overlay.componentKinds[fp.ref]
      if (!states && !kind) continue
      const [fpX, fpY] = ws(cam, fp.at)
      const radius = Math.max(20, 12 * cam.scale)
      if (offscreenCircle(ctx, fpX, fpY, radius)) continue
      const running = states?.['running'] ?? 0
      const dissipation = states?.['dissipation_mw'] ?? 0
      let stops: [number, string][] | null = null
      if (kind === 'mcu' && running > 0) stops = [[0, theme.mcuGlow0], [0.6, theme.mcuGlow1], [1, theme.mcuGlow2]]
      else if (dissipation > 0) {
        const t = Math.min(1, dissipation / 500)
        stops = [[0, heatColor(t).replace('rgb', 'rgba').replace(')', `,${0.25 * t + 0.05})`)], [1, 'rgba(0,0,0,0)']]
      }
      if (!stops) continue
      const grad = ctx.createRadialGradient(fpX, fpY, 0, fpX, fpY, radius)
      for (const [at, color] of stops) grad.addColorStop(at, color)
      ctx.fillStyle = grad
      ctx.beginPath()
      ctx.arc(fpX, fpY, radius, 0, Math.PI * 2)
      ctx.fill()
      drawn++
    }
  }

  // Net highlight glow pulses.
  for (const netName of overlay.highlightNets) {
    for (const s of board.segments) {
      if (s.netName !== netName) continue
      const [x1, y1] = ws(cam, s.start), [x2, y2] = ws(cam, s.end)
      if (segOffscreen(ctx, x1, y1, x2, y2)) continue
      ctx.beginPath()
      ctx.strokeStyle = theme.netGlowStroke
      ctx.lineWidth = Math.max(2, s.width * cam.scale * 3)
      ctx.lineCap = 'round'
      ctx.shadowColor = theme.netGlowShadow
      ctx.shadowBlur = 12
      ctx.moveTo(x1, y1)
      ctx.lineTo(x2, y2)
      ctx.stroke()
    }
    ctx.shadowBlur = 0
  }

  // Signal flow particles.
  ctx.shadowBlur = 0
  if (activityOn) {
    for (const [netName, positions] of overlay.particles) {
      for (const s of board.segments) {
        if (s.netName !== netName) continue
        for (const t of positions) {
          const [sx, sy] = ws(cam, { x: s.start.x + (s.end.x - s.start.x) * t, y: s.start.y + (s.end.y - s.start.y) * t })
          const r = Math.max(2, s.width * cam.scale * 0.6)
          ctx.beginPath()
          ctx.fillStyle = theme.particle
          ctx.shadowColor = theme.particleGlow
          ctx.shadowBlur = r * 2
          ctx.arc(sx, sy, r, 0, Math.PI * 2)
          ctx.fill()
          ctx.shadowBlur = 0
        }
      }
    }
  }

  // Probe tooltip.
  if (overlay.probe) {
    const { label, value } = overlay.probe
    const [sx, sy] = ws(cam, overlay.probe)
    const padding = 8, fontSize = 12
    ctx.font = `bold ${fontSize}px 'JetBrains Mono', monospace`
    const boxW = Math.max(ctx.measureText(label).width, ctx.measureText(value).width) + padding * 2
    const boxH = fontSize * 2 + padding * 2 + 4
    const bx = sx + 12, by = sy - boxH - 8
    ctx.lineWidth = 1.5
    ctx.shadowColor = theme.probeBorder
    ctx.shadowBlur = 8
    labelBox(ctx, bx, by, boxW, boxH, theme.probeBg, theme.probeBorder, 6)
    ctx.shadowBlur = 0
    ctx.fillStyle = theme.probeLabel
    ctx.fillText(label, bx + padding, by + padding + fontSize)
    ctx.fillStyle = theme.probeValue
    ctx.fillText(value, bx + padding, by + padding + fontSize * 2 + 4)
    ctx.beginPath()
    ctx.arc(sx, sy, 4, 0, Math.PI * 2)
    ctx.fill()
  }

  // Import completeness: located recovered/partial objects only.
  if (overlay.importMarkers?.length) {
    ctx.save()
    for (const point of visibleImportMarkers(overlay.importMarkers, overlay.highlightNets)) {
      const [sx, sy] = ws(cam, point)
      if (offscreenCircle(ctx, sx, sy, 10)) continue
      const recovered = point.status === 'recovered'
      const color = recovered ? '#22c55e' : '#f59e0b'
      ctx.beginPath()
      ctx.arc(sx, sy, recovered ? 5 : 7, 0, Math.PI * 2)
      ctx.fillStyle = `${color}66`
      ctx.strokeStyle = color
      ctx.lineWidth = recovered ? 1.5 : 2
      ctx.fill()
      ctx.stroke()
    }
    ctx.restore()
  }

  // "Show on board" marker: pulsing ring + crosshair at a finding's spot.
  if (overlay.marker) {
    const { label } = overlay.marker
    const [sx, sy] = ws(cam, overlay.marker)
    const pulse = 0.5 + 0.5 * Math.sin(animTime * 4)
    const ringR = 14 + pulse * 6
    ctx.save()
    ctx.strokeStyle = theme.markerStroke
    ctx.lineWidth = 2
    ctx.shadowColor = theme.markerShadow
    ctx.shadowBlur = 10
    ctx.globalAlpha = 0.9 - pulse * 0.4
    ctx.beginPath()
    ctx.arc(sx, sy, ringR, 0, Math.PI * 2)
    ctx.stroke()
    ctx.globalAlpha = 1
    ctx.shadowBlur = 0
    ctx.beginPath()
    ctx.moveTo(sx - 8, sy); ctx.lineTo(sx + 8, sy)
    ctx.moveTo(sx, sy - 8); ctx.lineTo(sx, sy + 8)
    ctx.lineWidth = 1.5
    ctx.stroke()
    if (label) {
      const fontSize = 11
      ctx.font = `bold ${fontSize}px 'JetBrains Mono', monospace`
      // Clamp the label to a readable width; the finding card has the rest.
      const shown = label.length > 60 ? `${label.slice(0, 57)}...` : label
      const w = ctx.measureText(shown).width + 14
      const bx = Math.min(Math.max(6, sx - w / 2), W - w - 6)
      const by = sy + ringR + 8
      ctx.lineWidth = 1
      labelBox(ctx, bx, by, w, fontSize + 12, theme.markerLabelBg, theme.markerLabelBorder, 5)
      ctx.fillStyle = theme.markerLabelText
      ctx.fillText(shown, bx + 7, by + fontSize + 4)
    }
    ctx.restore()
  }
}
