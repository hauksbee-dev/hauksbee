// Camera / viewport transform for the board renderer.
// Board coordinates are in KiCad mm; screen coordinates are in CSS/canvas pixels.

export interface Camera {
  /** Pan offset in screen pixels */
  panX: number
  panY: number
  /** Scale: screen pixels per mm */
  scale: number
}

export function makeCamera(
  boardW: number,
  boardH: number,
  boardCX: number,
  boardCY: number,
  canvasW: number,
  canvasH: number,
): Camera {
  // Fit the board in the canvas with 10% padding
  const fitScale = Math.min(
    (canvasW * 0.9) / boardW,
    (canvasH * 0.9) / boardH,
  )
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

export function zoomCamera(cam: Camera, delta: number, focusSX: number, focusSY: number): Camera {
  const factor = delta > 0 ? 1.12 : 1 / 1.12
  const newScale = Math.max(0.5, Math.min(2000, cam.scale * factor))
  // Zoom toward the cursor
  const panX = focusSX - (focusSX - cam.panX) * (newScale / cam.scale)
  const panY = focusSY - (focusSY - cam.panY) * (newScale / cam.scale)
  return { panX, panY, scale: newScale }
}

export function panCamera(cam: Camera, dx: number, dy: number): Camera {
  return { ...cam, panX: cam.panX + dx, panY: cam.panY + dy }
}
