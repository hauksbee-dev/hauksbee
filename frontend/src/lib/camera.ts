// Camera / viewport transform for the board renderer.
// Board coordinates are in KiCad mm; screen coordinates are in CSS/canvas pixels.

export interface Camera {
  /** Pan offset in screen pixels */
  panX: number
  panY: number
  /** Scale: screen pixels per mm */
  scale: number
}

// Cap for the INITIAL zoom-to-fit only (px per mm). A tiny synthetic board
// (two footprints, no copper) would otherwise fit-fill the canvas and open on
// pads the size of dinner plates; 20 px/mm keeps a 1 mm pad at ~20 px, which
// reads as a component rather than a wall. Manual zoom can still go far past it.
const MAX_FIT_SCALE = 20

/** The zoom-to-fit scale (px per mm) for a board in a canvas, capped so small
 *  boards open at a human scale instead of filling the viewport. */
export function fitScaleFor(boardW: number, boardH: number, canvasW: number, canvasH: number): number {
  return Math.min(
    (canvasW * 0.9) / boardW,
    (canvasH * 0.9) / boardH,
    MAX_FIT_SCALE,
  )
}

export function makeCamera(
  boardW: number,
  boardH: number,
  boardCX: number,
  boardCY: number,
  canvasW: number,
  canvasH: number,
): Camera {
  const fitScale = fitScaleFor(boardW, boardH, canvasW, canvasH)
  // Center the board
  const panX = canvasW / 2 - boardCX * fitScale
  const panY = canvasH / 2 - boardCY * fitScale
  return { panX, panY, scale: fitScale }
}

/** Convert board mm → canvas px */
export function worldToScreen(cam: Camera, x: number, y: number): { sx: number; sy: number } {
  return { sx: x * cam.scale + cam.panX, sy: y * cam.scale + cam.panY }
}

/** Convert canvas px → board mm */
export function screenToWorld(cam: Camera, sx: number, sy: number): { x: number; y: number } {
  return { x: (sx - cam.panX) / cam.scale, y: (sy - cam.panY) / cam.scale }
}

export const MIN_SCALE = 0.05
export const MAX_SCALE = 3000

/** Multiplicative zoom anchored at a screen point: the board point under
 *  (focusSX, focusSY) stays under it. `factor` > 1 zooms in. */
export function zoomCamera(cam: Camera, factor: number, focusSX: number, focusSY: number): Camera {
  const newScale = Math.max(MIN_SCALE, Math.min(MAX_SCALE, cam.scale * factor))
  const panX = focusSX - (focusSX - cam.panX) * (newScale / cam.scale)
  const panY = focusSY - (focusSY - cam.panY) * (newScale / cam.scale)
  return { panX, panY, scale: newScale }
}

/** Turn a wheel event's delta into a multiplicative zoom factor.
 *  Exponential in the delta, so fast flicks zoom faster but each pixel of
 *  wheel travel is worth the same ratio (no first-tick jump, no stepping).
 *  Trackpad pinch arrives as ctrlKey+wheel with small pixel deltas and wants
 *  a stronger response than two-finger scroll. */
export function wheelZoomFactor(e: { deltaY: number; deltaMode: number; ctrlKey: boolean }): number {
  // Normalise to pixels: 0=pixel, 1=line (~16 px), 2=page (~160 px)
  const px = e.deltaY * (e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? 160 : 1)
  const sensitivity = e.ctrlKey ? 0.012 : 0.0022
  return Math.exp(-px * sensitivity)
}

export function panCamera(cam: Camera, dx: number, dy: number): Camera {
  return { ...cam, panX: cam.panX + dx, panY: cam.panY + dy }
}
