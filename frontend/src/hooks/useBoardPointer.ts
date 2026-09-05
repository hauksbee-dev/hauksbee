import { useCallback, useRef } from 'react'
import { maxScaleFor, panCamera, screenToWorld, zoomCamera } from '../lib/camera'
import type { Camera } from '../lib/camera'
import type { BoardHitTest, FootprintInfo } from './useBoardHitTest'

// Pointer and touch on the board canvas: pan, pinch, and the one hit-test that
// resolves a click or a tap. Mouse and touch share `selectAt`, so the two
// selection paths cannot drift apart.

export interface BoardPointerHandlers {
  onMouseDown: (e: React.MouseEvent) => void
  onMouseMove: (e: React.MouseEvent) => void
  onMouseUp: (e: React.MouseEvent) => void
  onMouseLeave: () => void
  onTouchStart: (e: React.TouchEvent) => void
  onTouchMove: (e: React.TouchEvent) => void
  onTouchEnd: (e: React.TouchEvent) => void
  /** True while a drag is in flight, for the cursor. */
  dragging: React.RefObject<boolean>
}

export function useBoardPointer({
  canvasRef, camRef, fitScaleRef, userMovedCamera, setCamera, hit,
  hoveredNet, hoveredRef, probePos, onClaimWheel, onNetClick, onFootprintClick,
}: {
  canvasRef: React.RefObject<HTMLCanvasElement | null>
  camRef: React.RefObject<Camera>
  fitScaleRef: React.RefObject<number>
  userMovedCamera: React.RefObject<boolean>
  setCamera: (next: Camera) => void
  hit: BoardHitTest
  /** Read by the render loop, so hover state lives in refs, not state. */
  hoveredNet: React.RefObject<string | null>
  hoveredRef: React.RefObject<string | null>
  probePos: React.RefObject<{ boardX: number; boardY: number } | null>
  /** Touching the map is what claims the wheel from the page. */
  onClaimWheel: () => void
  onNetClick?: (net: string | null) => void
  onFootprintClick?: (info: FootprintInfo) => void
}): BoardPointerHandlers {
  const dragging = useRef(false)
  const lastPoint = useRef({ x: 0, y: 0 })
  // A "click" is a press that never travelled: `dragging` is armed on EVERY
  // mousedown (it also drives pan), so it cannot tell a click from a drag.
  const movedSinceDown = useRef(false)
  const lastTouchDist = useRef(0)
  // Where a single touch began, so touchend can tell a tap (stayed put) from a
  // pan (travelled). Null once a second finger lands: a pinch is never a tap.
  const touchStart = useRef<{ x: number; y: number } | null>(null)

  const canvasPoint = useCallback((clientX: number, clientY: number) => {
    const rect = canvasRef.current!.getBoundingClientRect()
    return { sx: clientX - rect.left, sy: clientY - rect.top }
  }, [canvasRef])

  const pan = useCallback((clientX: number, clientY: number) => {
    const dx = clientX - lastPoint.current.x
    const dy = clientY - lastPoint.current.y
    const travelled = Math.abs(dx) + Math.abs(dy) > 2
    if (travelled) {
      movedSinceDown.current = true
      userMovedCamera.current = true
    }
    lastPoint.current = { x: clientX, y: clientY }
    setCamera(panCamera(camRef.current, dx, dy))
  }, [camRef, setCamera, userMovedCamera])

  /**
   * Resolve a tap/click that did not travel, and fire the consumer callbacks.
   *
   * Layered and EXCLUSIVE: firing footprint and net together collapses every
   * part click into a net selection, which makes "click a part, see its bound
   * model" unreachable.
   *   1. a routed TRACE within tight reach wins: the click was on bare copper;
   *   2. otherwise a footprint hit wins, carrying the nearest pad's net along
   *      (on an unrouted board pads are the only copper, so a part click must
   *      still surface its net);
   *   3. otherwise the nearest pad net within coarse reach, or null to clear.
   */
  const selectAt = useCallback((sx: number, sy: number) => {
    const { x, y } = screenToWorld(camRef.current, sx, sy)
    const traceNet = hit.nearestNet(x, y, 5, false)
    if (traceNet) {
      onNetClick?.(traceNet)
      return
    }
    const fp = hit.footprintAt(x, y)
    if (fp && onFootprintClick) {
      onFootprintClick({ ...fp, padNet: hit.nearestNet(x, y, 8) })
      return
    }
    // The reference LABEL is part of the part's visual identity, so clicking it
    // selects the part; the test is in SCREEN space because the label's size is
    // screen-fixed, not board-fixed.
    const labelFp = hit.labelAt(sx, sy)
    if (labelFp && onFootprintClick) {
      onFootprintClick({ ...labelFp, padNet: hit.nearestNet(labelFp.x, labelFp.y, 12) })
      return
    }
    onNetClick?.(hit.nearestNet(x, y, 8))
  }, [camRef, hit, onFootprintClick, onNetClick])

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button !== 0) return
    onClaimWheel()
    dragging.current = true
    movedSinceDown.current = false
    lastPoint.current = { x: e.clientX, y: e.clientY }
  }, [onClaimWheel])

  const onMouseMove = useCallback((e: React.MouseEvent) => {
    if (dragging.current) {
      pan(e.clientX, e.clientY)
      return
    }
    const { sx, sy } = canvasPoint(e.clientX, e.clientY)
    const { x, y } = screenToWorld(camRef.current, sx, sy)
    probePos.current = { boardX: x, boardY: y }
    // Hover resolves the cursor the SAME way `selectAt` does. Testing only
    // traces and pads would leave parts unlit, so the board would read as
    // though only the copper were live.
    const traceNet = hit.nearestNet(x, y, 5, false)
    if (traceNet) {
      hoveredNet.current = traceNet
      hoveredRef.current = null
      return
    }
    hoveredRef.current = (hit.footprintAt(x, y) ?? hit.labelAt(sx, sy))?.ref ?? null
    // The pad's net still feeds the probe tooltip, so reading a voltage off a
    // part's pin keeps working.
    hoveredNet.current = hit.nearestNet(x, y, 8)
  }, [camRef, canvasPoint, hit, hoveredNet, hoveredRef, pan, probePos])

  const onMouseUp = useCallback((e: React.MouseEvent) => {
    if (e.button !== 0) return
    dragging.current = false
    if (movedSinceDown.current) return
    const { sx, sy } = canvasPoint(e.clientX, e.clientY)
    selectAt(sx, sy)
  }, [canvasPoint, selectAt])

  const onMouseLeave = useCallback(() => {
    dragging.current = false
    hoveredNet.current = null
    hoveredRef.current = null
    probePos.current = null
  }, [hoveredNet, hoveredRef, probePos])

  const fingerSpread = (e: React.TouchEvent) =>
    Math.hypot(e.touches[0].clientX - e.touches[1].clientX, e.touches[0].clientY - e.touches[1].clientY)

  const onTouchStart = useCallback((e: React.TouchEvent) => {
    if (e.touches.length === 1) {
      dragging.current = true
      lastPoint.current = { x: e.touches[0].clientX, y: e.touches[0].clientY }
      touchStart.current = { ...lastPoint.current }
    } else if (e.touches.length === 2) {
      dragging.current = false
      touchStart.current = null
      lastTouchDist.current = fingerSpread(e)
    }
  }, [])

  const onTouchMove = useCallback((e: React.TouchEvent) => {
    e.preventDefault()
    if (e.touches.length === 1 && dragging.current) {
      pan(e.touches[0].clientX, e.touches[0].clientY)
      return
    }
    if (e.touches.length !== 2) return
    const dist = fingerSpread(e)
    // True pinch semantics: the zoom factor IS the ratio of finger spreads, so
    // the board tracks the fingers exactly (no tuning constant involved).
    if (lastTouchDist.current > 0) {
      userMovedCamera.current = true
      const cx = (e.touches[0].clientX + e.touches[1].clientX) / 2
      const cy = (e.touches[0].clientY + e.touches[1].clientY) / 2
      const { sx, sy } = canvasPoint(cx, cy)
      setCamera(zoomCamera(
        camRef.current, dist / lastTouchDist.current, sx, sy, maxScaleFor(fitScaleRef.current),
      ))
    }
    lastTouchDist.current = dist
  }, [camRef, canvasPoint, fitScaleRef, pan, setCamera, userMovedCamera])

  const onTouchEnd = useCallback((e: React.TouchEvent) => {
    dragging.current = false
    const start = touchStart.current
    touchStart.current = null
    // Tap-to-select: a single touch that lifted without travelling (< 10px)
    // runs the same hit-test the mouse-up click does.
    if (!start || e.touches.length !== 0 || e.changedTouches.length !== 1) return
    const t = e.changedTouches[0]
    if (Math.hypot(t.clientX - start.x, t.clientY - start.y) > 10) return
    const { sx, sy } = canvasPoint(t.clientX, t.clientY)
    selectAt(sx, sy)
  }, [canvasPoint, selectAt])

  return {
    onMouseDown, onMouseMove, onMouseUp, onMouseLeave,
    onTouchStart, onTouchMove, onTouchEnd, dragging,
  }
}
