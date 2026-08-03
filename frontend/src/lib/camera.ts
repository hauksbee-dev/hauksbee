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

/** How far past zoom-to-fit the map is allowed to go, as a multiple of the fit
 *  scale. The readout is expressed against fit ("100%" IS fit), so 16x reads as
 *  1600%.
 *
 *  This is a relative cap rather than an absolute px-per-mm one because the
 *  absolute cap (MAX_SCALE, 3000 px/mm) is meaningless as a limit: on a 40 mm
 *  board it is reached at roughly 20000%, which is a viewport showing about
 *  four microns of copper. There is nothing on a PCB at that scale; the
 *  renderer draws one flat colour, the pan gesture moves the board a screen
 *  width per pixel of mouse travel, and the only way back is Fit. 16x is
 *  already past the finest thing on a board (a 4 mil trace is ~10 px at fit on
 *  a 100 mm board, so ~160 px here). Fit itself is untouched, and MIN_SCALE
 *  still allows zooming out past fit. */
export const MAX_ZOOM_RATIO = 16

/** The largest scale the user may zoom to, for a given zoom-to-fit scale. */
export function maxScaleFor(fitScale: number): number {
  if (!Number.isFinite(fitScale) || fitScale <= 0) return MAX_SCALE
  return Math.min(MAX_SCALE, fitScale * MAX_ZOOM_RATIO)
}

/** Multiplicative zoom anchored at a screen point: the board point under
 *  (focusSX, focusSY) stays under it. `factor` > 1 zooms in.
 *
 *  `maxScale` defaults to the absolute ceiling; callers that know the board's
 *  fit scale pass `maxScaleFor(fit)` so the cap is the one a reader can see in
 *  the zoom readout. */
export function zoomCamera(
  cam: Camera,
  factor: number,
  focusSX: number,
  focusSY: number,
  maxScale: number = MAX_SCALE,
): Camera {
  const newScale = Math.max(MIN_SCALE, Math.min(maxScale, cam.scale * factor))
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
  // A pinch is ctrlKey + a stream of small pixel deltas. A MOUSE wheel held
  // with ctrl (the report map's zoom gesture) sets the same flag but arrives
  // in ~120 px notches, where the pinch sensitivity made every notch a 4x
  // jump; that gets the ordinary wheel response.
  const pinch = e.ctrlKey && e.deltaMode === 0 && Math.abs(px) < 40
  const sensitivity = pinch ? 0.012 : 0.0022
  return Math.exp(-px * sensitivity)
}

export function panCamera(cam: Camera, dx: number, dy: number): Camera {
  return { ...cam, panX: cam.panX + dx, panY: cam.panY + dy }
}
