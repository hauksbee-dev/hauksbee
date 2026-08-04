import { describe, expect, it } from 'bun:test'
import { parseKicadPcb } from '../src/lib/kicad-parser'

/** A KiCad 5 board outline: four straight edges joined by four 90-degree
 *  fillets, written the pre-6 way as (start = CENTRE, end = first endpoint,
 *  angle = degrees swept). No `mid` node anywhere, which is the whole point. */
const LEGACY_ROUNDED_RECT = `
(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (gr_line (start 11 10) (end 29 10) (layer Edge.Cuts) (width 0.15))
  (gr_line (start 30 11) (end 30 19) (layer Edge.Cuts) (width 0.15))
  (gr_line (start 29 20) (end 11 20) (layer Edge.Cuts) (width 0.15))
  (gr_line (start 10 19) (end 10 11) (layer Edge.Cuts) (width 0.15))
  (gr_arc (start 29 11) (end 29 10) (angle 90) (layer Edge.Cuts) (width 0.15))
  (gr_arc (start 29 19) (end 30 19) (angle 90) (layer Edge.Cuts) (width 0.15))
  (gr_arc (start 11 19) (end 11 20) (angle 90) (layer Edge.Cuts) (width 0.15))
  (gr_arc (start 11 11) (end 10 11) (angle 90) (layer Edge.Cuts) (width 0.15))
)
`

describe('legacy KiCad 5 gr_arc', () => {
  it('closes the outline instead of leaving a point at the origin', () => {
    const board = parseKicadPcb(LEGACY_ROUNDED_RECT)
    expect(board.gr_arcs.length).toBe(4)

    // The defect this pins: an absent `mid` used to default to (0, 0), so every
    // legacy arc dragged the outline to the origin, the Edge.Cuts loop never
    // closed, and the 3D view fell back to a bounding box. Any pre-6 board a
    // user dropped in rendered as a blank slab.
    for (const a of board.gr_arcs) {
      for (const p of [a.start, a.mid, a.end]) {
        expect(Math.hypot(p.x, p.y)).toBeGreaterThan(1)
      }
    }

    // Closure is the real property, and it holds without hardcoding a single
    // converted coordinate: every arc endpoint must coincide with a neighbouring
    // segment's endpoint, which is only true if the sweep direction is right.
    const lineEnds = board.gr_lines.flatMap((l) => [l.start, l.end])
    const nearest = (p: { x: number; y: number }, pool: { x: number; y: number }[]) =>
      Math.min(...pool.map((q) => Math.hypot(p.x - q.x, p.y - q.y)))
    for (const a of board.gr_arcs) {
      const others = board.gr_arcs.filter((o) => o !== a).flatMap((o) => [o.start, o.end])
      expect(nearest(a.start, [...lineEnds, ...others])).toBeLessThan(0.002)
      expect(nearest(a.end, [...lineEnds, ...others])).toBeLessThan(0.002)
    }

    // The midpoint sits on the arc, at the radius, not on the chord.
    for (const a of board.gr_arcs) {
      const cx = (a.start.x + a.end.x) / 2
      const cy = (a.start.y + a.end.y) / 2
      expect(Math.hypot(a.mid.x - cx, a.mid.y - cy)).toBeGreaterThan(0.05)
    }
  })

  it('leaves a modern three-point arc exactly as written', () => {
    const board = parseKicadPcb(`
(kicad_pcb (version 20221018)
  (gr_arc (start 1 2) (mid 3 4) (end 5 6) (stroke (width 0.1) (type solid)) (layer "Edge.Cuts"))
)
`)
    expect(board.gr_arcs[0]).toMatchObject({
      start: { x: 1, y: 2 },
      mid: { x: 3, y: 4 },
      end: { x: 5, y: 6 },
    })
  })
})
