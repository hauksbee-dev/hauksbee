// Regression tests for the board map's part hit target. Clicking a part's
// BODY, the plastic between an IC's pads or the silkscreen box around a
// two-pad passive, used to select nothing: the hit-test only knew the part
// origin and the pads. Run with: bun test  (from frontend/)

import { test, expect } from 'bun:test'
import { readFileSync } from 'node:fs'
import { parseKicadPcb, footprintHitBoxes, pickFootprintBox } from '../src/lib/kicad-parser'
import type { Footprint } from '../src/lib/kicad-parser'

const board = parseKicadPcb(readFileSync('public/samples/watchy.kicad_pcb', 'utf8'))
const boxes = footprintHitBoxes(board)

/** Is this board point inside any of the footprint's pads? */
function onPad(fp: Footprint, x: number, y: number): boolean {
  return fp.pads.some(p =>
    x >= p.at.x - p.size.w / 2 && x <= p.at.x + p.size.w / 2 &&
    y >= p.at.y - p.size.h / 2 && y <= p.at.y + p.size.h / 2)
}

test('every drawable footprint gets a hit box', () => {
  expect(boxes.length).toBeGreaterThan(50)
  expect(boxes.every(b => b.x2 >= b.x1 && b.y2 >= b.y1)).toBe(true)
})

test('boxes come smallest first, so a part inside a connector outline still wins', () => {
  const areas = boxes.map(b => b.area)
  expect([...areas].sort((a, b) => a - b)).toEqual(areas)
})

test('the centre of a multi-pad part resolves to that part, off its pads', () => {
  // Multi-pad parts whose geometric centre is bare body, not copper: exactly
  // the click that used to land on nothing.
  const bodyCentres = boxes.filter(b => {
    const cx = (b.x1 + b.x2) / 2, cy = (b.y1 + b.y2) / 2
    return b.fp.pads.length >= 2 && !onPad(b.fp, cx, cy)
  })
  expect(bodyCentres.length).toBeGreaterThan(20)

  for (const b of bodyCentres) {
    const cx = (b.x1 + b.x2) / 2, cy = (b.y1 + b.y2) / 2
    const hit = pickFootprintBox(boxes, cx, cy)
    expect(hit).not.toBeNull()
    // A smaller part may legitimately sit on top of this point (that is what
    // smallest-first is for); what must never happen is selecting nothing.
    if (hit!.ref !== b.fp.ref) {
      const other = boxes.find(o => o.fp === hit)!
      expect(other.area).toBeLessThanOrEqual(b.area)
    }
  }
})

test('a point off the board hits nothing', () => {
  expect(pickFootprintBox(boxes, -1e4, -1e4)).toBeNull()
})
