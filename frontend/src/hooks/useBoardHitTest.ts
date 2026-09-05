import { useCallback, useMemo } from 'react'
import { footprintHitBoxes, pickFootprintBox } from '../lib/kicad-parser'
import type { ParsedBoard } from '../lib/kicad-parser'
import type { Camera } from '../lib/camera'
import { labelAnchor } from '../lib/board-renderer'

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

export interface BoardHitTest {
  /** Nearest net to a board coordinate, or null when nothing is within reach.
   *  `reachPx` is the screen-pixel pick radius: hover keeps the tight default
   *  so the readout tracks exactly what is under the cursor; a CLICK passes a
   *  coarser radius, because clicking is a blunter gesture. */
  nearestNet: (bx: number, by: number, reachPx?: number, includePads?: boolean) => string | null
  /** The footprint at a board coordinate: pads and the part origin first (the
   *  precise targets), then the part BODY, because clicking the plastic between
   *  an IC's pads, or the silkscreen box around a two-pad passive, is a click on
   *  that part. */
  footprintAt: (bx: number, by: number) => FootprintInfo | null
  /** The footprint whose reference LABEL is under SCREEN coords. The label is
   *  part of the part's visual identity, so clicking it selects the part;
   *  mirrors the renderer's placement rule so the hit area is where the text
   *  actually is. */
  labelAt: (sx: number, sy: number) => FootprintInfo | null
}

export function useBoardHitTest(
  board: ParsedBoard | null,
  camera: React.RefObject<Camera>,
  showLabels: boolean,
): BoardHitTest {
  const nearestNet = useCallback((bx: number, by: number, reachPx = 3, includePads = true) => {
    if (!board) return null
    let best: string | null = null
    let bestDist = Infinity
    const threshold = reachPx / camera.current.scale

    for (const s of board.segments) {
      if (!s.netName) continue
      const dx = s.end.x - s.start.x
      const dy = s.end.y - s.start.y
      const len2 = dx * dx + dy * dy
      if (len2 === 0) continue
      const t = Math.max(0, Math.min(1, ((bx - s.start.x) * dx + (by - s.start.y) * dy) / len2))
      const dist = Math.hypot(bx - (s.start.x + t * dx), by - (s.start.y + t * dy))
      if (dist < threshold && dist < bestDist) {
        bestDist = dist
        best = s.netName
      }
    }

    if (includePads) {
      for (const fp of board.footprints) {
        for (const pad of fp.pads) {
          if (!pad.netName) continue
          const d = Math.hypot(bx - pad.at.x, by - pad.at.y)
          if (d < Math.max(pad.size.w, pad.size.h) / 2 + threshold && d < bestDist) {
            bestDist = d
            best = pad.netName
          }
        }
      }
    }
    return best
  }, [board, camera])

  // One FootprintInfo shape for every selection path (body/pad hit, label hit),
  // so they cannot drift apart.
  const describe = useCallback((fp: ParsedBoard['footprints'][number]): FootprintInfo => {
    // Distinct pad nets in pad order, for the selection card's net list.
    const nets: string[] = []
    for (const pad of fp.pads) {
      if (pad.netName && !nets.includes(pad.netName)) nets.push(pad.netName)
    }
    return { ref: fp.ref, value: fp.value, lib_id: fp.lib_id, x: fp.at.x, y: fp.at.y, padNets: nets }
  }, [])

  // Computed once per board: a 3,000-part flagship must not rebuild the part
  // extents on every click.
  const boxes = useMemo(() => (board ? footprintHitBoxes(board) : []), [board])

  const footprintAt = useCallback((bx: number, by: number): FootprintInfo | null => {
    if (!board) return null
    let best: FootprintInfo | null = null
    let bestDist = Infinity
    const threshold = 5 / camera.current.scale

    for (const fp of board.footprints) {
      const d = Math.hypot(bx - fp.at.x, by - fp.at.y)
      if (d < threshold && d < bestDist) {
        bestDist = d
        best = describe(fp)
      }
      for (const pad of fp.pads) {
        const pd = Math.hypot(bx - pad.at.x, by - pad.at.y)
        if (pd < Math.max(pad.size.w, pad.size.h) / 2 + 0.5 && pd < bestDist) {
          bestDist = pd
          best = describe(fp)
        }
      }
    }
    if (best) return best

    const body = pickFootprintBox(boxes, bx, by)
    return body ? describe(body) : null
  }, [board, boxes, describe, camera])

  const labelAt = useCallback((sx: number, sy: number): FootprintInfo | null => {
    if (!board || !showLabels) return null
    for (const fp of board.footprints) {
      const anchor = labelAnchor(fp, camera.current)
      if (!anchor) continue
      // Monospace glyphs are ~0.6em wide; a small floor keeps 1-char refs
      // clickable.
      const w = Math.max(18, fp.ref.length * anchor.fontPx * 0.62)
      const yBottom = anchor.topY - 3
      if (sx >= anchor.cx - w / 2 && sx <= anchor.cx + w / 2
        && sy >= yBottom - anchor.fontPx - 2 && sy <= yBottom) {
        return describe(fp)
      }
    }
    return null
  }, [board, describe, showLabels, camera])

  return { nearestNet, footprintAt, labelAt }
}
