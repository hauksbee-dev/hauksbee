// Geometry queries over a ParsedBoard: what is at a board point, where a
// part's label sits, which copper belongs to a net. Pure functions over the
// parser's primitives, so the pointer hook (hover, click, tap) and the
// renderer (the hover/fault extents, the label pass) resolve against exactly
// the same shapes and cannot disagree about what the cursor is on.

import type { Camera } from './camera'
import { worldToScreen } from './camera'
import type { Footprint, ParsedBoard, Point, Segment } from './kicad-parser'

/** What a click on a part reports back. */
export interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
  /** Net of the pad nearest the click, when one was in reach. On an unrouted
   *  board pads are the ONLY copper, so a part click must still surface its
   *  net or net checks become unreachable from the map. */
  padNet?: string | null
  /** All distinct nets on the part's pads, for the selection card. */
  padNets?: string[]
}

/** One FootprintInfo shape for every selection path (body/pad hit, label
 *  hit), so they cannot drift apart. Distinct pad nets in pad order. */
export function describeFootprint(fp: Footprint): FootprintInfo {
  const nets: string[] = []
  for (const pad of fp.pads) if (pad.netName && !nets.includes(pad.netName)) nets.push(pad.netName)
  return { ref: fp.ref, value: fp.value, lib_id: fp.lib_id, x: fp.at.x, y: fp.at.y, padNets: nets }
}

// ── Part extents ─────────────────────────────────────────────────────────────

/** One footprint's drawn extent in board mm. */
export interface FootprintBox {
  fp: Footprint
  x1: number
  y1: number
  x2: number
  y2: number
  area: number
}

/** Each footprint's drawn extent: the union of its pads and its body outline
 *  (silkscreen / fab lines, rects, circles). This is the shape a reader sees
 *  as "the part", so it is the shape a click on the part should hit; pad-only
 *  hit-testing left the plastic between an IC's pads, and the silkscreen box
 *  around a two-pad passive, selecting nothing.
 *
 *  Returned smallest-area-first so [`pickFootprintBox`] resolves a small part
 *  sitting inside a big connector's outline to the small part. */
export function footprintHitBoxes(board: ParsedBoard): FootprintBox[] {
  const boxes: FootprintBox[] = []
  for (const fp of board.footprints) {
    let x1 = Infinity, y1 = Infinity, x2 = -Infinity, y2 = -Infinity
    const add = (p: Point) => {
      if (p.x < x1) x1 = p.x
      if (p.x > x2) x2 = p.x
      if (p.y < y1) y1 = p.y
      if (p.y > y2) y2 = p.y
    }
    for (const pad of fp.pads) {
      add({ x: pad.at.x - pad.size.w / 2, y: pad.at.y - pad.size.h / 2 })
      add({ x: pad.at.x + pad.size.w / 2, y: pad.at.y + pad.size.h / 2 })
    }
    const g = fp.graphics
    for (const l of [...g.lines, ...g.rects]) { add(l.start); add(l.end) }
    for (const c of g.circles) {
      const r = Math.hypot(c.end.x - c.center.x, c.end.y - c.center.y)
      add({ x: c.center.x - r, y: c.center.y - r })
      add({ x: c.center.x + r, y: c.center.y + r })
    }
    if (!Number.isFinite(x1)) continue
    boxes.push({ fp, x1, y1, x2, y2, area: (x2 - x1) * (y2 - y1) })
  }
  return boxes.sort((a, b) => a.area - b.area)
}

/** The smallest footprint whose extent contains this board point, or null. */
export function pickFootprintBox(boxes: FootprintBox[], bx: number, by: number): Footprint | null {
  return boxes.find(b => bx >= b.x1 && bx <= b.x2 && by >= b.y1 && by <= b.y2)?.fp ?? null
}

// The overlay paints each hovered/faulted part's drawn extent, which is the
// same geometry the click hit-test uses. Cached per board because the overlay
// pass runs every animation frame and the boxes never change for a board.
const boxCache = new WeakMap<ParsedBoard, Map<string, FootprintBox>>()
export function boxesByRef(board: ParsedBoard): Map<string, FootprintBox> {
  let byRef = boxCache.get(board)
  if (!byRef) {
    byRef = new Map(footprintHitBoxes(board).map(b => [b.fp.ref, b]))
    boxCache.set(board, byRef)
  }
  return byRef
}

// ── Labels ───────────────────────────────────────────────────────────────────

/** Minimum ON-SCREEN footprint extent (px) before its reference label is
 *  drawn. */
export const LABEL_MIN_PX = 26

/** Where a footprint's reference label sits on screen, and how big, or null
 *  when the part is too small at this zoom to carry one. The renderer paints
 *  here and the label hit-test reads the same anchor. */
export function labelAnchor(fp: Footprint, cam: Camera): { cx: number; topY: number; fontPx: number; extentPx: number } | null {
  if (fp.pads.length === 0) return null
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity
  for (const pad of fp.pads) {
    minX = Math.min(minX, pad.at.x - pad.size.w / 2)
    maxX = Math.max(maxX, pad.at.x + pad.size.w / 2)
    minY = Math.min(minY, pad.at.y - pad.size.h / 2)
    maxY = Math.max(maxY, pad.at.y + pad.size.h / 2)
  }
  const extentPx = Math.max(maxX - minX, maxY - minY) * cam.scale
  if (extentPx < LABEL_MIN_PX) return null
  const a = worldToScreen(cam, minX, minY), b = worldToScreen(cam, maxX, maxY)
  return {
    cx: (a.sx + b.sx) / 2,
    topY: Math.min(a.sy, b.sy),
    fontPx: Math.min(13, Math.max(9, extentPx * 0.18)),
    extentPx,
  }
}

/** The footprint whose reference LABEL is under SCREEN coords. The label is
 *  part of the part's visual identity, so clicking it selects the part. */
export function labelAt(board: ParsedBoard, cam: Camera, sx: number, sy: number): Footprint | null {
  for (const fp of board.footprints) {
    const anchor = labelAnchor(fp, cam)
    if (!anchor) continue
    // Monospace glyphs are ~0.6em wide; a small floor keeps 1-char refs clickable.
    const w = Math.max(18, fp.ref.length * anchor.fontPx * 0.62)
    const yBottom = anchor.topY - 3
    if (sx >= anchor.cx - w / 2 && sx <= anchor.cx + w / 2 && sy >= yBottom - anchor.fontPx - 2 && sy <= yBottom) return fp
  }
  return null
}

// ── Nets ─────────────────────────────────────────────────────────────────────

/** Distance from a point to a segment. */
function distToSegment(bx: number, by: number, s: Segment): number {
  const dx = s.end.x - s.start.x, dy = s.end.y - s.start.y
  const len2 = dx * dx + dy * dy
  if (len2 === 0) return Infinity
  const t = Math.max(0, Math.min(1, ((bx - s.start.x) * dx + (by - s.start.y) * dy) / len2))
  return Math.hypot(bx - (s.start.x + t * dx), by - (s.start.y + t * dy))
}

/** Nearest net to a board coordinate within `reach` mm, or null. Pads count
 *  when `includePads`: on an unrouted board they are the only copper. */
export function nearestNet(board: ParsedBoard, bx: number, by: number, reach: number, includePads = true): string | null {
  let best: string | null = null
  let bestDist = Infinity
  for (const s of board.segments) {
    if (!s.netName) continue
    const dist = distToSegment(bx, by, s)
    if (dist < reach && dist < bestDist) { bestDist = dist; best = s.netName }
  }
  if (includePads) {
    for (const fp of board.footprints) {
      for (const pad of fp.pads) {
        if (!pad.netName) continue
        const d = Math.hypot(bx - pad.at.x, by - pad.at.y)
        if (d < Math.max(pad.size.w, pad.size.h) / 2 + reach && d < bestDist) { bestDist = d; best = pad.netName }
      }
    }
  }
  return best
}

/** The footprint at a board coordinate: pads and the part origin first (the
 *  precise targets, within `reach` mm), then the part BODY, because clicking
 *  the plastic between an IC's pads, or the silkscreen box around a two-pad
 *  passive, is a click on that part. */
export function footprintAt(board: ParsedBoard, boxes: FootprintBox[], bx: number, by: number, reach: number): Footprint | null {
  let best: Footprint | null = null
  let bestDist = Infinity
  for (const fp of board.footprints) {
    const d = Math.hypot(bx - fp.at.x, by - fp.at.y)
    if (d < reach && d < bestDist) { bestDist = d; best = fp }
    for (const pad of fp.pads) {
      const pd = Math.hypot(bx - pad.at.x, by - pad.at.y)
      if (pd < Math.max(pad.size.w, pad.size.h) / 2 + 0.5 && pd < bestDist) { bestDist = pd; best = fp }
    }
  }
  return best ?? pickFootprintBox(boxes, bx, by)
}

/** netName → its routed segments, for the flow animation. */
export function segmentsByNet(board: ParsedBoard): Map<string, Segment[]> {
  const idx = new Map<string, Segment[]>()
  for (const s of board.segments) {
    if (!s.netName) continue
    const list = idx.get(s.netName)
    if (list) list.push(s)
    else idx.set(s.netName, [s])
  }
  return idx
}
