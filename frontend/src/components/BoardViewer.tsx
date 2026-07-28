import { useEffect, useRef, useState, useCallback, useMemo } from 'react'
import { parseKicadPcb, buildNetIndex } from '../lib/kicad-parser'
import type { ParsedBoard } from '../lib/kicad-parser'
import { makeCamera, fitScaleFor, zoomCamera, wheelZoomFactor, panCamera, screenToWorld, MIN_SCALE, MAX_SCALE } from '../lib/camera'
import type { Camera } from '../lib/camera'
import { renderStaticBoard, renderDynamicOverlay, LABEL_MIN_PX } from '../lib/board-renderer'
import type { OverlayData, RenderOptions } from '../lib/board-renderer'
import { getLayerStyle } from '../lib/layer-colors'
import type { SimFrame, BoardInfoMsg } from '../types/protocol'
import { Board3DViewer } from './Board3DViewer'
import { FitIcon, LayersIcon } from './Icons'

interface FootprintInfo {
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

interface BoardViewerProps {
  boardFile: string
  frame: SimFrame | null
  boardInfo?: BoardInfoMsg | null
  /** Externally chosen net to highlight (e.g., from probe click) */
  selectedNet?: string | null
  onFootprintClick?: (info: FootprintInfo) => void
  /** Click on bare copper: the nearest net (trace/pad hit-test), or null when
   *  nothing is within reach. Fires only for a true click (no drag). */
  onNetClick?: (net: string | null) => void
  /** Called when the file parsed but yielded NOTHING drawable (no footprints,
   *  segments or vias); the embedding view can fall back to a simpler map
   *  instead of showing an empty void. */
  onEmptyBoard?: () => void
  /** Faulted component references for pulse highlights */
  faultedRefs?: Set<string>
  /** Net names for the layers panel's highlight picker. When absent, the
   *  picker uses the nets parsed from the board file itself. */
  netOptions?: string[]
  /** Pan/zoom to a board location (mm) and drop a labeled marker there (the
   *  report's "show on board" affordance). A new `seq` re-triggers the move
   *  even for the same coordinates. */
  focusPoint?: { x: number; y: number; label?: string; seq: number } | null
  /** Fires when the 2D/3D segmented control switches, so the embedding view
   *  can adapt its own chrome (e.g. the caption under the canvas swaps to
   *  orbit instructions in 3D). */
  onViewModeChange?: (mode: '2d' | '3d') => void
}

const PARTICLE_COUNT = 4
const PARTICLE_SPEED = 0.3 // t units per second

// Map board name to GLB URL. Extends as more boards are exported.
const BOARD_GLB_MAP: Record<string, string> = {
  'demo': '/boards3d/demo.glb',
  'pic_programmer': '/boards3d/pic_programmer.glb',
  'stickhub': '/boards3d/stickhub.glb',
}

function resolveGlbUrl(boardFile: string, boardInfo?: BoardInfoMsg | null): string | null {
  // Check protocol-provided URL first (future field)
  if (boardInfo?.glb_url) return boardInfo.glb_url

  // Try matching by board name in the path
  for (const [key, url] of Object.entries(BOARD_GLB_MAP)) {
    if (boardFile.includes(key)) return url
  }
  return null
}

/** The Layers panel's rows, derived from what the parsed board actually
 *  contains: real copper/silk/fab layers only, never a fixed template. */
function layersPresent(board: ParsedBoard): string[] {
  const found = new Set<string>()
  for (const s of board.segments) found.add(s.layer)
  for (const a of board.arcs) found.add(a.layer)
  const grLayers = [
    ...board.gr_lines, ...board.gr_arcs, ...board.gr_circles,
    ...board.gr_rects, ...board.gr_polys,
  ]
  for (const g of grLayers) found.add(g.layer)
  for (const fp of board.footprints) {
    for (const l of fp.fp_lines) found.add(l.layer)
    for (const a of fp.fp_arcs) found.add(a.layer)
    for (const c of fp.fp_circles) found.add(c.layer)
    for (const r of fp.fp_rects) found.add(r.layer)
  }
  // Only layers the renderer would draw by default (palette-visible), in a
  // stable copper-first order.
  const order = [
    'F.Cu', 'In1.Cu', 'In2.Cu', 'In3.Cu', 'In4.Cu', 'B.Cu',
    'F.SilkS', 'F.Silkscreen', 'B.SilkS', 'B.Silkscreen',
    'F.Fab', 'B.Fab', 'Edge.Cuts', 'Dwgs.User', 'User.Drawings',
  ]
  return order.filter(l => found.has(l) && getLayerStyle(l).visible)
}

/** Friendly display names for KiCad layer ids. */
const LAYER_LABELS: Record<string, string> = {
  'F.Cu': 'Copper · front',
  'B.Cu': 'Copper · back',
  'In1.Cu': 'Copper · inner 1',
  'In2.Cu': 'Copper · inner 2',
  'In3.Cu': 'Copper · inner 3',
  'In4.Cu': 'Copper · inner 4',
  'F.SilkS': 'Silkscreen · front',
  'F.Silkscreen': 'Silkscreen · front',
  'B.SilkS': 'Silkscreen · back',
  'B.Silkscreen': 'Silkscreen · back',
  'F.Fab': 'Fab outline · front',
  'B.Fab': 'Fab outline · back',
  'Edge.Cuts': 'Board edge',
  'Dwgs.User': 'Drawings',
  'User.Drawings': 'Drawings',
}

/** One row in the layers panel: swatch, name, and a real switch. */
function LayerRow({ label, swatch, on, onToggle }: {
  label: string
  swatch?: string
  on: boolean
  onToggle: () => void
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      onClick={onToggle}
      className="hb-press flex items-center gap-2 w-full text-left cursor-pointer"
      style={{
        background: 'none', border: 'none', padding: '7px 10px',
        color: on ? 'var(--silk)' : 'var(--silk-faint)', fontSize: 12,
        borderRadius: 7,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 10, height: 10, borderRadius: 3, flexShrink: 0,
          background: swatch ?? 'var(--silk-faint)',
          opacity: on ? 1 : 0.25,
          outline: '1px solid var(--image-outline)',
          transition: 'opacity 0.15s',
        }}
      />
      <span className="flex-1 truncate">{label}</span>
      <span
        aria-hidden
        style={{
          width: 26, height: 15, borderRadius: 8, position: 'relative', flexShrink: 0,
          background: on ? 'var(--copper-tint-strong)' : 'var(--surface-3)',
          border: `1px solid ${on ? 'var(--copper-deep)' : 'var(--hairline)'}`,
          transition: 'background-color 0.15s, border-color 0.15s',
        }}
      >
        <span
          style={{
            position: 'absolute', top: 1.5, width: 10, height: 10, borderRadius: 5,
            left: on ? 13 : 2,
            background: on ? 'var(--copper)' : 'var(--silk-faint)',
            transition: 'left 0.15s cubic-bezier(0.2,0,0,1), background-color 0.15s',
          }}
        />
      </span>
    </button>
  )
}

export function BoardViewer({
  boardFile, frame, boardInfo, selectedNet, onFootprintClick, onNetClick,
  onEmptyBoard, faultedRefs, netOptions, focusPoint, onViewModeChange,
}: BoardViewerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const overlayRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)
  const zoomReadoutRef = useRef<HTMLSpanElement>(null)

  const [board, setBoard] = useState<ParsedBoard | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [viewMode, setViewMode] = useState<'2d' | '3d'>('2d')
  const [layersOpen, setLayersOpen] = useState(false)

  // Tell the embedding view which mode the segmented control is in (effect,
  // not inline in the click handlers, so the initial mode is reported too).
  useEffect(() => {
    onViewModeChange?.(viewMode)
  }, [viewMode, onViewModeChange])

  // Layers panel state: per-layer overrides plus the pads/labels/activity
  // switches. Hidden layers also stop rendering activity on their copper.
  const [hiddenLayers, setHiddenLayers] = useState<Set<string>>(new Set())
  const [showPads, setShowPads] = useState(true)
  const [showLabels, setShowLabels] = useState(true)
  const [showActivity, setShowActivity] = useState(true)

  const renderOpts = useMemo<RenderOptions>(() => ({
    layerVisible: (layer: string) => !hiddenLayers.has(layer) && getLayerStyle(layer).visible,
    showPads,
    showLabels,
  }), [hiddenLayers, showPads, showLabels])

  // The camera lives in a ref, not React state: pan and zoom mutate it up to
  // 60 times a second and nothing in the DOM depends on it; the animation
  // loop reads it directly. Re-rendering React per camera tick was pure waste.
  const camRef = useRef<Camera>({ panX: 0, panY: 0, scale: 1 })
  // When the camera last moved (wheel/drag/pinch), for static-cache policy.
  const camMovedAt = useRef(0)
  const setCamera = useCallback((next: Camera) => {
    camRef.current = next
    camMovedAt.current = performance.now()
  }, [])
  // The fit scale for the current canvas size, so the zoom readout can say
  // "100%" at fit rather than an arbitrary internal scale.
  const fitScaleRef = useRef(1)

  // Whether the USER moved the camera this session (wheel, drag, pinch).
  // Auto-refit (on becoming visible again, on 3D-to-2D return, on resize) is
  // only allowed while this is false: a camera the user set is theirs, but a
  // camera nobody touched must never present a stale zoom pinned top-left.
  const userMovedCamera = useRef(false)

  // Static board cache: the full board drawn at a fixed camera, blitted with a
  // cheap transform every animation frame. Re-rendered when the camera settles
  // (or immediately when the render is cheap or the blit would degrade too far).
  const staticCache = useRef<{
    canvas: HTMLCanvasElement | null
    cam: Camera | null
    renderMs: number
  }>({ canvas: null, cam: null, renderMs: 0 })

  // Layer visibility changed: the cached static render is stale.
  useEffect(() => {
    staticCache.current.cam = null
  }, [renderOpts])

  // Smooth zoom: wheel ticks set a target scale and the animation loop glides
  // the camera toward it (anchored at the cursor), so discrete wheel notches
  // do not step visibly.
  const zoomTarget = useRef<{ scale: number; sx: number; sy: number } | null>(null)

  // Interaction state
  const dragging = useRef(false)
  const lastMouse = useRef({ x: 0, y: 0 })
  const animFrame = useRef<number>(0)
  const particlePhases = useRef<Map<string, number>>(new Map())
  const hoveredNet = useRef<string | null>(null)
  const probePos = useRef<{ boardX: number; boardY: number } | null>(null)
  const animTimeRef = useRef(0)

  const netIndex = useMemo(() => board ? buildNetIndex(board) : null, [board])
  const boardLayers = useMemo(() => board ? layersPresent(board) : [], [board])
  // Real (named) nets only: the KiCad net table's synthetic id-0 "" bucket is
  // not a net, and counting it disagreed with the report's own net count.
  const namedNetCount = useMemo(
    () => board ? [...board.nets.values()].filter(Boolean).length : 0,
    [board],
  )

  // Build net voltages map (throttled by frame reference -- only recompute when frame changes)
  const netVoltagesMap = useMemo(() => {
    if (!frame?.net_voltages) return undefined
    return new Map<string, number>(Object.entries(frame.net_voltages))
  }, [frame?.net_voltages])

  // GLB URL for 3D view
  const glbUrl = useMemo(() => resolveGlbUrl(boardFile, boardInfo), [boardFile, boardInfo])

  // ── Load board ──
  useEffect(() => {
    // Changing boardFile does not implicitly cancel the previous fetch: a slow
    // earlier load could otherwise setBoard (or report an empty board) AFTER
    // the newer file already rendered. Abort on cleanup and drop late resolves.
    let cancelled = false
    const ctrl = new AbortController()
    setLoading(true)
    setError(null)
    setBoard(null)
    fetch(boardFile, { signal: ctrl.signal })
      .then(r => {
        if (!r.ok) throw new Error(`HTTP ${r.status}: ${r.url}`)
        return r.text()
      })
      .then(text => {
        if (cancelled) return
        const parsed = parseKicadPcb(text)
        // Test hook, sibling of __hbCam below: lets browser-driven checks
        // compute exact label/pad geometry without React devtools.
        ;(window as unknown as { __hbBoard?: ParsedBoard }).__hbBoard = parsed
        setBoard(parsed)
        setLoading(false)
        if (
          parsed.footprints.length === 0 &&
          parsed.segments.length === 0 &&
          parsed.vias.length === 0
        ) {
          onEmptyBoard?.()
        }
      })
      .catch((e: Error) => {
        if (cancelled || e.name === 'AbortError') return
        setError(e.message)
        setLoading(false)
      })
    return () => {
      cancelled = true
      ctrl.abort()
    }
  }, [boardFile])

  // ── Fit camera when board loads or canvas resizes ──
  const fitToView = useCallback(() => {
    if (!board || !canvasRef.current) return
    const { width: cw, height: ch } = canvasRef.current.getBoundingClientRect()
    const b = board.bounds
    setCamera(makeCamera(b.width, b.height, b.cx, b.cy, cw || 800, ch || 600))
    fitScaleRef.current = fitScaleFor(b.width, b.height, cw || 800, ch || 600)
    zoomTarget.current = null
    staticCache.current.cam = null
  }, [board, setCamera])

  useEffect(() => { fitToView() }, [fitToView])

  // Returning from 3D to 2D: the 2D camera is whatever it was when the user
  // left, which after a mount-while-hidden or a long 3D session reads as a
  // stale zoom pinned in a corner. Refit unless the user set the camera.
  useEffect(() => {
    if (viewMode === '2d' && !userMovedCamera.current) fitToView()
  }, [viewMode, fitToView])

  // ── Canvas resize observer ──
  useEffect(() => {
    const container = containerRef.current
    if (!container) return
    const ro = new ResizeObserver(() => {
      const canvas = canvasRef.current
      const overlay = overlayRef.current
      if (!canvas || !overlay) return
      const { width, height } = container.getBoundingClientRect()
      canvas.width = width
      canvas.height = height
      overlay.width = width
      overlay.height = height
      staticCache.current.cam = null
      // Hidden (display:none) views resize to 0x0; there is nothing to fit
      // until the view is shown again, at which point this observer re-fires
      // with the real size and the refit below runs.
      if (width < 2 || height < 2) return
      if (board) {
        const b = board.bounds
        const c = camRef.current
        const fit = fitScaleFor(b.width, b.height, width, height)
        fitScaleRef.current = fit
        // A camera the user never touched always refits (a mount-while-hidden
        // camera was fitted for a default 800x600 guess and shows the board
        // pinned top-left otherwise). A user-set camera refits only when it
        // was already near fit, so an intentional zoom survives a resize.
        const refitThreshold = 0.15
        const relativeDiff = Math.abs(c.scale - fit) / fit
        if (!userMovedCamera.current || relativeDiff < refitThreshold) {
          setCamera(makeCamera(b.width, b.height, b.cx, b.cy, width, height))
          zoomTarget.current = null
        }
      }
    })
    ro.observe(container)
    return () => ro.disconnect()
  }, [board, setCamera])

  // ── Find nearest net to a board coordinate ──
  // `reachPx` is the screen-pixel pick radius: hover keeps the tight default
  // so the readout tracks exactly what is under the cursor; a CLICK passes a
  // coarser radius (clicking is a blunter gesture, and on an unrouted board
  // the only copper is pads).
  const findNearestNet = useCallback((bx: number, by: number, reachPx = 3, includePads = true): string | null => {
    if (!board) return null
    let best: string | null = null
    let bestDist = Infinity
    const threshold = reachPx / camRef.current.scale

    for (const s of board.segments) {
      if (!s.netName) continue
      const dx = s.end.x - s.start.x
      const dy = s.end.y - s.start.y
      const len2 = dx * dx + dy * dy
      if (len2 === 0) continue
      const t = Math.max(0, Math.min(1, ((bx - s.start.x) * dx + (by - s.start.y) * dy) / len2))
      const projX = s.start.x + t * dx
      const projY = s.start.y + t * dy
      const dist = Math.sqrt((bx - projX) ** 2 + (by - projY) ** 2)
      if (dist < threshold && dist < bestDist) {
        bestDist = dist
        best = s.netName
      }
    }

    if (includePads) {
      for (const fp of board.footprints) {
        for (const pad of fp.pads) {
          if (!pad.netName) continue
          const d = Math.sqrt((bx - pad.at.x) ** 2 + (by - pad.at.y) ** 2)
          const padR = Math.max(pad.size.w, pad.size.h) / 2
          if (d < padR + threshold && d < bestDist) {
            bestDist = d
            best = pad.netName
          }
        }
      }
    }

    return best
  }, [board])

  // One FootprintInfo shape for every selection path (body/pad hit, label
  // hit), so they cannot drift apart.
  const describeFootprint = useCallback((fp: ParsedBoard['footprints'][number]): FootprintInfo => {
    // Distinct pad nets in pad order, for the selection card's net list.
    const nets: string[] = []
    for (const pad of fp.pads) {
      if (pad.netName && !nets.includes(pad.netName)) nets.push(pad.netName)
    }
    return { ref: fp.ref, value: fp.value, lib_id: fp.lib_id, x: fp.at.x, y: fp.at.y, padNets: nets }
  }, [])

  // ── Find footprint at board coordinate ──
  const findFootprintAt = useCallback((bx: number, by: number): FootprintInfo | null => {
    if (!board) return null
    let best: FootprintInfo | null = null
    let bestDist = Infinity
    const threshold = 5 / camRef.current.scale

    for (const fp of board.footprints) {
      const d = Math.sqrt((bx - fp.at.x) ** 2 + (by - fp.at.y) ** 2)
      if (d < threshold && d < bestDist) {
        bestDist = d
        best = describeFootprint(fp)
      }
      for (const pad of fp.pads) {
        const pd = Math.sqrt((bx - pad.at.x) ** 2 + (by - pad.at.y) ** 2)
        const padR = Math.max(pad.size.w, pad.size.h) / 2 + 0.5
        if (pd < padR && pd < bestDist) {
          bestDist = pd
          best = describeFootprint(fp)
        }
      }
    }
    return best
  }, [board, describeFootprint])

  // ── Find the footprint whose reference LABEL is under screen coords ──
  // The label is part of the part's visual identity, so clicking it selects
  // the part exactly like clicking its body/pads. Mirrors the renderer's
  // label placement rule (pad-bbox top center, size-gated) so the hit area is
  // where the text actually is.
  const findLabelAt = useCallback((sx: number, sy: number): FootprintInfo | null => {
    if (!board || !showLabels) return null
    const cam = camRef.current
    for (const fp of board.footprints) {
      if (fp.pads.length === 0) continue
      let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity
      for (const pad of fp.pads) {
        minX = Math.min(minX, pad.at.x - pad.size.w / 2)
        maxX = Math.max(maxX, pad.at.x + pad.size.w / 2)
        minY = Math.min(minY, pad.at.y - pad.size.h / 2)
        maxY = Math.max(maxY, pad.at.y + pad.size.h / 2)
      }
      const extentPx = Math.max(maxX - minX, maxY - minY) * cam.scale
      if (extentPx < LABEL_MIN_PX) continue
      const sx1 = minX * cam.scale + cam.panX
      const sx2 = maxX * cam.scale + cam.panX
      const sy1 = minY * cam.scale + cam.panY
      const sy2 = maxY * cam.scale + cam.panY
      const cx = (sx1 + sx2) / 2
      const topY = Math.min(sy1, sy2)
      const fontPx = Math.min(13, Math.max(9, extentPx * 0.18))
      // Monospace glyphs are ~0.6em wide; a small floor keeps 1-char refs
      // clickable.
      const w = Math.max(18, fp.ref.length * fontPx * 0.62)
      const yBottom = topY - 3
      const yTop = yBottom - fontPx - 2
      if (sx >= cx - w / 2 && sx <= cx + w / 2 && sy >= yTop && sy <= yBottom) {
        return describeFootprint(fp)
      }
    }
    return null
  }, [board, describeFootprint, showLabels])

  // ── Animation loop (2D only) ──
  // Per-frame data reaches the loop through refs, NOT effect deps: putting
  // `frame` in the deps tore down and restarted the rAF loop 30 times a
  // second on a live sim.
  const netVoltagesRef = useRef(netVoltagesMap)
  netVoltagesRef.current = netVoltagesMap
  const faultedRefsRef = useRef(faultedRefs)
  faultedRefsRef.current = faultedRefs
  const frameRef = useRef(frame)
  frameRef.current = frame
  const boardInfoRef = useRef(boardInfo)
  boardInfoRef.current = boardInfo
  const selectedNetRef = useRef(selectedNet)
  selectedNetRef.current = selectedNet
  const renderOptsRef = useRef(renderOpts)
  renderOptsRef.current = renderOpts
  const showActivityRef = useRef(showActivity)
  showActivityRef.current = showActivity
  const markerRef = useRef<{ x: number; y: number; label?: string } | null>(null)

  // "Show on board": jump the camera to the finding's spot at a readable
  // close-up and keep a pulsing marker there. Counts as a user move (the
  // focused framing must not be clobbered by an auto-refit).
  useEffect(() => {
    if (!focusPoint || !board || !canvasRef.current) {
      markerRef.current = focusPoint ?? null
      return
    }
    markerRef.current = { x: focusPoint.x, y: focusPoint.y, label: focusPoint.label }
    const canvas = canvasRef.current
    const { width: cw, height: ch } = canvas.getBoundingClientRect()
    if (cw < 2 || ch < 2) return
    const scale = Math.min(MAX_SCALE, Math.max(fitScaleRef.current * 5, camRef.current.scale))
    userMovedCamera.current = true
    zoomTarget.current = null
    setCamera({
      scale,
      panX: cw / 2 - focusPoint.x * scale,
      panY: ch / 2 - focusPoint.y * scale,
    })
    // Re-trigger on seq even for identical coordinates.
  }, [focusPoint, focusPoint?.seq, board, setCamera])

  useEffect(() => {
    if (!board || viewMode === '3d') return

    let lastT = performance.now()
    let lastReadout = 0

    // Draw the cached static board onto the main canvas, transformed from the
    // camera it was rendered at to the current camera. Between static
    // re-renders (pan in flight, zoom gliding) this is one drawImage.
    function blitStatic(ctx: CanvasRenderingContext2D, cam: Camera) {
      const st = staticCache.current
      if (!st.canvas || !st.cam) return
      if (st.canvas.width === 0 || st.canvas.height === 0) return
      ctx.fillStyle = '#020617'
      ctx.fillRect(0, 0, ctx.canvas.width, ctx.canvas.height)
      const k = cam.scale / st.cam.scale
      ctx.setTransform(k, 0, 0, k, cam.panX - k * st.cam.panX, cam.panY - k * st.cam.panY)
      ctx.drawImage(st.canvas, 0, 0)
      ctx.setTransform(1, 0, 0, 1, 0, 0)
    }

    function ensureStatic(canvas: HTMLCanvasElement, now: number) {
      if (!board) return
      const st = staticCache.current
      const cam = camRef.current
      const same = st.cam && st.canvas &&
        st.cam.panX === cam.panX && st.cam.panY === cam.panY && st.cam.scale === cam.scale &&
        st.canvas.width === canvas.width && st.canvas.height === canvas.height
      if (same) return

      // Decide whether to pay for a fresh static render this frame:
      //  - nothing cached yet: always
      //  - the last render was cheap: every frame (small boards stay crisp)
      //  - the blit has degraded past 2x in either direction: now
      //  - the camera has been still for 120 ms: now (gesture settled)
      const ratio = st.cam ? cam.scale / st.cam.scale : 1
      const cheap = st.renderMs < 25
      const degraded = ratio > 2.0 || ratio < 0.5
      const settled = now - camMovedAt.current > 120
      if (st.canvas && st.cam && !cheap && !degraded && !settled) return

      let off = st.canvas
      if (!off || off.width !== canvas.width || off.height !== canvas.height) {
        off = document.createElement('canvas')
        off.width = canvas.width
        off.height = canvas.height
      }
      const offCtx = off.getContext('2d')!
      const t0 = performance.now()
      renderStaticBoard(offCtx, board, cam, renderOptsRef.current)
      st.renderMs = performance.now() - t0
      st.canvas = off
      st.cam = { ...cam }
    }

    function tick(now: number) {
      const dt = (now - lastT) / 1000
      lastT = now
      animTimeRef.current += dt

      const canvas = canvasRef.current
      const overlay = overlayRef.current
      if (!canvas || !overlay || !board) { animFrame.current = requestAnimationFrame(tick); return }
      // A hidden view (display:none) resizes the canvases to 0x0; drawing
      // into or blitting from a 0-sized canvas throws, and an exception in a
      // React-owned effect tears the whole tree down. Idle until visible.
      if (canvas.width === 0 || canvas.height === 0) {
        animFrame.current = requestAnimationFrame(tick)
        return
      }

      const ctx = canvas.getContext('2d')!
      const octx = overlay.getContext('2d')!

      // Glide toward the wheel zoom target (anchored at the cursor).
      const zt = zoomTarget.current
      if (zt) {
        const cur = camRef.current
        const logRatio = Math.log(zt.scale / cur.scale)
        if (Math.abs(logRatio) < 0.005) {
          setCamera(zoomCamera(cur, zt.scale / cur.scale, zt.sx, zt.sy))
          zoomTarget.current = null
        } else {
          const step = Math.exp(logRatio * Math.min(1, dt * 16))
          setCamera(zoomCamera(cur, step, zt.sx, zt.sy))
        }
      }

      const currentCam = camRef.current
      // Test hook: lets browser-driven checks read the live camera (and lets a
      // human debug zoom maths from the console) without React devtools.
      ;(window as unknown as { __hbCam?: Camera }).__hbCam = currentCam

      // Zoom readout: imperative DOM write, throttled; React state per camera
      // tick would re-render the whole viewer at gesture rate for a label.
      if (now - lastReadout > 120 && zoomReadoutRef.current) {
        lastReadout = now
        const pct = Math.round((currentCam.scale / (fitScaleRef.current || 1)) * 100)
        zoomReadoutRef.current.textContent = `${pct}%`
      }
      const frame = frameRef.current
      const selectedNet = selectedNetRef.current

      const hlNets = new Set<string>()
      if (selectedNet) hlNets.add(selectedNet)
      if (hoveredNet.current) hlNets.add(hoveredNet.current)

      // Advance particles
      const flowNet = selectedNet ?? (frame?.net_voltages ? Object.keys(frame.net_voltages)[0] : null)
      if (flowNet && netIndex?.has(flowNet)) {
        const segs = netIndex.get(flowNet)!.segments
        if (segs.length > 0) {
          for (let i = 0; i < PARTICLE_COUNT; i++) {
            const key = `${flowNet}:${i}`
            const prev = particlePhases.current.get(key) ?? (i / PARTICLE_COUNT)
            const next = (prev + PARTICLE_SPEED * dt) % 1
            particlePhases.current.set(key, next)
          }
        }
      }

      const particles = new Map<string, number[]>()
      if (flowNet) {
        const ts: number[] = []
        for (let i = 0; i < PARTICLE_COUNT; i++) {
          const key = `${flowNet}:${i}`
          const t = particlePhases.current.get(key) ?? (i / PARTICLE_COUNT)
          ts.push(t)
        }
        particles.set(flowNet, ts)
      }

      const probeNet = hoveredNet.current
      const probeData = probeNet && probePos.current && frame?.net_voltages[probeNet] !== undefined
        ? {
            x: probePos.current.boardX,
            y: probePos.current.boardY,
            label: probeNet,
            value: `${(frame.net_voltages[probeNet]!).toFixed(3)} V`,
          }
        : undefined

      const overlayData: OverlayData = {
        highlightNets: hlNets,
        dimOthers: hlNets.size > 0,
        particles,
        probe: probeData,
        netVoltages: netVoltagesRef.current,
        componentStates: frame?.component_states,
        componentKinds: boardInfoRef.current?.component_kinds,
        faultedRefs: faultedRefsRef.current,
        animTime: animTimeRef.current,
        showActivity: showActivityRef.current,
        renderOpts: renderOptsRef.current,
        marker: markerRef.current,
      }

      ensureStatic(canvas, now)
      blitStatic(ctx, currentCam)
      renderDynamicOverlay(octx, board, currentCam, overlayData)

      animFrame.current = requestAnimationFrame(tick)
    }

    animFrame.current = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(animFrame.current)
  }, [board, netIndex, viewMode, setCamera])

  // ── Wheel zoom ──
  // Attached natively (non-passive): React registers wheel listeners as
  // passive at the root, so preventDefault in an onWheel prop cannot stop the
  // browser's own pinch-zoom of the page.
  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || viewMode === '3d') return
    const onWheel = (e: WheelEvent) => {
      e.preventDefault()
      const rect = canvas.getBoundingClientRect()
      const sx = e.clientX - rect.left
      const sy = e.clientY - rect.top
      const factor = wheelZoomFactor(e)
      const base = zoomTarget.current?.scale ?? camRef.current.scale
      const target = Math.max(MIN_SCALE, Math.min(MAX_SCALE, base * factor))
      zoomTarget.current = { scale: target, sx, sy }
      camMovedAt.current = performance.now()
      userMovedCamera.current = true
    }
    canvas.addEventListener('wheel', onWheel, { passive: false })
    return () => canvas.removeEventListener('wheel', onWheel)
  }, [viewMode])

  // A "click" is a press that never travelled: dragging.current is armed on
  // EVERY mousedown (it also drives pan), so it cannot distinguish click from
  // drag; track actual movement instead.
  const movedSinceDown = useRef(false)

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button === 0) {
      dragging.current = true
      movedSinceDown.current = false
      lastMouse.current = { x: e.clientX, y: e.clientY }
    }
  }, [])

  const handleMouseMove = useCallback((e: React.MouseEvent) => {
    const rect = canvasRef.current!.getBoundingClientRect()
    const sx = e.clientX - rect.left
    const sy = e.clientY - rect.top

    if (dragging.current) {
      const dx = e.clientX - lastMouse.current.x
      const dy = e.clientY - lastMouse.current.y
      if (Math.abs(dx) + Math.abs(dy) > 2) {
        movedSinceDown.current = true
        userMovedCamera.current = true
      }
      lastMouse.current = { x: e.clientX, y: e.clientY }
      setCamera(panCamera(camRef.current, dx, dy))
    } else {
      const { x, y } = screenToWorld(camRef.current, sx, sy)
      probePos.current = { boardX: x, boardY: y }
      hoveredNet.current = findNearestNet(x, y)
    }
  }, [findNearestNet, setCamera])

  // Shared hit-test for a tap/click that did not travel: resolves the footprint
  // and nearest net under canvas-relative screen coords (sx, sy) and fires the
  // consumer callbacks. Used by BOTH the mouse-up click and the touch tap so the
  // two selection paths cannot drift apart.
  //
  // Layered, exclusive resolution (firing footprint AND net together made
  // every part click collapse into a net selection, so the "click a part, see
  // its bound model" flow was unreachable):
  //   1. a routed TRACE within tight reach wins: the click was on bare copper;
  //   2. otherwise a footprint hit wins, carrying the nearest pad's net along
  //      (on an unrouted board pads are the only copper, so the part click
  //      must still surface its net);
  //   3. otherwise the nearest pad net within coarse reach (clicking is a
  //      blunt gesture), or null to clear the selection.
  const selectAt = useCallback((sx: number, sy: number) => {
    const { x, y } = screenToWorld(camRef.current, sx, sy)
    const traceNet = findNearestNet(x, y, 5, false)
    if (traceNet) {
      if (onNetClick) onNetClick(traceNet)
      return
    }
    const fp = findFootprintAt(x, y)
    if (fp && onFootprintClick) {
      onFootprintClick({ ...fp, padNet: findNearestNet(x, y, 8) })
      return
    }
    // The reference LABEL is part of the part's visual identity: clicking it
    // selects the part like clicking its body/pads (label test in SCREEN
    // space; the label's size is screen-fixed, not board-fixed).
    const labelFp = findLabelAt(sx, sy)
    if (labelFp && onFootprintClick) {
      onFootprintClick({ ...labelFp, padNet: findNearestNet(labelFp.x, labelFp.y, 12) })
      return
    }
    if (onNetClick) onNetClick(findNearestNet(x, y, 8))
  }, [findFootprintAt, findLabelAt, onFootprintClick, onNetClick, findNearestNet])

  const handleMouseUp = useCallback((e: React.MouseEvent) => {
    if (e.button === 0) {
      dragging.current = false
      if (!movedSinceDown.current) {
        const rect = canvasRef.current!.getBoundingClientRect()
        selectAt(e.clientX - rect.left, e.clientY - rect.top)
      }
    }
  }, [selectAt])

  const handleMouseLeave = useCallback(() => {
    dragging.current = false
    hoveredNet.current = null
    probePos.current = null
  }, [])

  // Touch support
  const lastTouchDist = useRef<number>(0)
  // Where a single touch began, so touchend can tell a tap (stayed put) from a
  // pan (travelled). Null once a second finger lands: a pinch is never a tap.
  const touchStartPos = useRef<{ x: number; y: number } | null>(null)
  const handleTouchStart = useCallback((e: React.TouchEvent) => {
    if (e.touches.length === 1) {
      dragging.current = true
      lastMouse.current = { x: e.touches[0].clientX, y: e.touches[0].clientY }
      touchStartPos.current = { x: e.touches[0].clientX, y: e.touches[0].clientY }
    } else if (e.touches.length === 2) {
      dragging.current = false
      touchStartPos.current = null
      const dx = e.touches[0].clientX - e.touches[1].clientX
      const dy = e.touches[0].clientY - e.touches[1].clientY
      lastTouchDist.current = Math.sqrt(dx * dx + dy * dy)
    }
  }, [])

  const handleTouchMove = useCallback((e: React.TouchEvent) => {
    e.preventDefault()
    if (e.touches.length === 1 && dragging.current) {
      const dx = e.touches[0].clientX - lastMouse.current.x
      const dy = e.touches[0].clientY - lastMouse.current.y
      if (Math.abs(dx) + Math.abs(dy) > 2) userMovedCamera.current = true
      lastMouse.current = { x: e.touches[0].clientX, y: e.touches[0].clientY }
      setCamera(panCamera(camRef.current, dx, dy))
    } else if (e.touches.length === 2) {
      const dx = e.touches[0].clientX - e.touches[1].clientX
      const dy = e.touches[0].clientY - e.touches[1].clientY
      const dist = Math.sqrt(dx * dx + dy * dy)
      // True pinch semantics: the zoom factor IS the ratio of finger spreads,
      // so the board tracks the fingers exactly (no tuning constant involved).
      if (lastTouchDist.current > 0) {
        userMovedCamera.current = true
        const factor = dist / lastTouchDist.current
        const cx = (e.touches[0].clientX + e.touches[1].clientX) / 2
        const cy = (e.touches[0].clientY + e.touches[1].clientY) / 2
        const rect = canvasRef.current!.getBoundingClientRect()
        setCamera(zoomCamera(camRef.current, factor, cx - rect.left, cy - rect.top))
      }
      lastTouchDist.current = dist
    }
  }, [setCamera])

  const handleTouchEnd = useCallback((e: React.TouchEvent) => {
    dragging.current = false
    const start = touchStartPos.current
    touchStartPos.current = null
    // Tap-to-select: a single touch that lifted without travelling (< 10px) runs
    // the same hit-test the mouse-up click does.
    if (start && e.touches.length === 0 && e.changedTouches.length === 1) {
      const t = e.changedTouches[0]
      const moved = Math.hypot(t.clientX - start.x, t.clientY - start.y)
      if (moved <= 10) {
        const rect = canvasRef.current!.getBoundingClientRect()
        selectAt(t.clientX - rect.left, t.clientY - rect.top)
      }
    }
  }, [selectAt])

  // Nets for the highlight picker: prefer the live protocol's list, fall back
  // to what the parser found on the copper.
  const pickerNets = useMemo(() => {
    if (netOptions && netOptions.length > 0) return netOptions
    if (boardInfo?.nets && boardInfo.nets.length > 0) return boardInfo.nets
    if (!board) return []
    return [...board.nets.values()].filter(Boolean).sort()
  }, [netOptions, boardInfo, board])

  const toolbarBtn = (active: boolean): React.CSSProperties => ({
    background: active ? 'var(--copper-tint-strong)' : 'transparent',
    color: active ? 'var(--copper-hi)' : 'var(--silk-faint)',
    border: 'none',
    padding: '5px 11px',
    fontSize: 11,
    fontWeight: 700,
    letterSpacing: '0.06em',
    cursor: 'pointer',
    minHeight: 28,
    display: 'inline-flex',
    alignItems: 'center',
    gap: 5,
  })

  return (
    <div
      ref={containerRef}
      className="relative w-full h-full overflow-hidden"
      style={{ background: 'var(--instrument)', cursor: viewMode === '2d' ? (dragging.current ? 'grabbing' : 'crosshair') : 'default' }}
    >
      {/* 2D canvas layer */}
      <canvas
        ref={canvasRef}
        role="img"
        aria-label="Board map: scroll to zoom, drag to pan, click a trace to select its net. Keyboard users can pick a net in the checks panel."
        className="absolute inset-0"
        style={{ display: viewMode === '2d' ? 'block' : 'none' }}
        onMouseDown={handleMouseDown}
        onMouseMove={handleMouseMove}
        onMouseUp={handleMouseUp}
        onMouseLeave={handleMouseLeave}
        onTouchStart={handleTouchStart}
        onTouchMove={handleTouchMove}
        onTouchEnd={handleTouchEnd}
      />
      <canvas
        ref={overlayRef}
        className="absolute inset-0 pointer-events-none"
        style={{ display: viewMode === '2d' ? 'block' : 'none' }}
      />

      {/* Visually-hidden guidance for keyboard / screen-reader users: the canvas
          has no keyboard net-selection path, so point them to the net pickers in
          the checks panel (which are ordinary focusable inputs). */}
      <p className="sr-only">
        This is an interactive board map. Selecting a net by pointer needs a mouse
        or touch. Keyboard users can pick a net using the net fields in the checks
        view.
      </p>

      {/* 3D view -- only mounted when 3D tab is active. Boards without a
          pre-exported GLB get a model GENERATED from the parsed layout
          (extruded substrate, instanced pads/vias/bodies), so 3D works for
          any board that renders in 2D, the 3,443-part flagship included. */}
      {viewMode === '3d' && (
        <div className="absolute inset-0">
          {(glbUrl || board) ? (
            <Board3DViewer
              glbUrl={glbUrl}
              board={board}
              frame={frame}
              boardInfo={boardInfo}
              faults={frame?.faults}
            />
          ) : (
            <div className="absolute inset-0 flex items-center justify-center">
              <div className="hb-card px-4 py-3 text-sm" style={{ color: 'var(--silk-dim)' }}>
                No 3D model available for this board
              </div>
            </div>
          )}
        </div>
      )}

      {/* ── Viewer toolbar ── */}
      <div className="absolute top-3 left-3 right-3 z-20 flex items-start justify-between pointer-events-none">
        <div className="flex items-center gap-2 pointer-events-auto">
          {/* 2D / 3D segmented control */}
          <div
            className="flex rounded-lg overflow-hidden"
            style={{
              border: '1px solid var(--hairline)',
              background: 'color-mix(in srgb, var(--surface) 88%, transparent)',
              backdropFilter: 'blur(6px)',
              boxShadow: 'var(--shadow-card)',
            }}
          >
            <button
              type="button"
              onClick={() => setViewMode('2d')}
              className="hb-press"
              style={toolbarBtn(viewMode === '2d')}
            >
              2D
            </button>
            <button
              type="button"
              disabled={!glbUrl && !board}
              onClick={() => { if (glbUrl || board) setViewMode('3d') }}
              title={!glbUrl && !board ? 'The board has not loaded yet' : undefined}
              className="hb-press"
              style={{
                ...toolbarBtn(viewMode === '3d'),
                borderLeft: '1px solid var(--hairline)',
                cursor: (glbUrl || board) ? 'pointer' : 'not-allowed',
                opacity: (glbUrl || board) ? 1 : 0.4,
              }}
            >
              3D
            </button>
          </div>

          {viewMode === '2d' && (
            <div
              className="flex items-center rounded-lg overflow-hidden"
              style={{
                border: '1px solid var(--hairline)',
                background: 'color-mix(in srgb, var(--surface) 88%, transparent)',
                backdropFilter: 'blur(6px)',
                boxShadow: 'var(--shadow-card)',
              }}
            >
              <button
                type="button"
                onClick={() => {
                  // An explicit Fit is a return to the automatic framing, so
                  // auto-refit may take over again from here.
                  userMovedCamera.current = false
                  fitToView()
                }}
                title="Fit the board to the view"
                aria-label="Fit the board to the view"
                className="hb-press"
                style={toolbarBtn(false)}
              >
                <FitIcon size={13} /> Fit
              </button>
              <span
                ref={zoomReadoutRef}
                className="tnum"
                aria-label="Zoom level relative to fit"
                style={{
                  color: 'var(--silk-faint)', fontSize: 11, fontFamily: 'var(--font-mono)',
                  padding: '5px 10px', borderLeft: '1px solid var(--hairline)', minWidth: 52,
                  textAlign: 'right', display: 'inline-block',
                }}
              >
                100%
              </span>
            </div>
          )}
        </div>

        {viewMode === '2d' && board && (
          <div className="flex flex-col items-end gap-2 pointer-events-auto">
            <button
              type="button"
              onClick={() => setLayersOpen(o => !o)}
              aria-expanded={layersOpen}
              className="hb-press rounded-lg"
              style={{
                ...toolbarBtn(layersOpen),
                border: '1px solid var(--hairline)',
                background: layersOpen
                  ? 'var(--copper-tint-strong)'
                  : 'color-mix(in srgb, var(--surface) 88%, transparent)',
                backdropFilter: 'blur(6px)',
                boxShadow: 'var(--shadow-card)',
              }}
            >
              <LayersIcon size={13} /> Layers
            </button>

            {/* ── Layers panel: only what this board actually has ── */}
            {layersOpen && (
              <div
                data-testid="layers-panel"
                className="hb-card view-enter overflow-y-auto"
                style={{ width: 228, maxHeight: 'calc(100% - 8px)', padding: 6, boxShadow: 'var(--shadow-pop)' }}
              >
                {boardLayers.map(layer => (
                  <LayerRow
                    key={layer}
                    label={LAYER_LABELS[layer] ?? layer}
                    swatch={getLayerStyle(layer).color}
                    on={!hiddenLayers.has(layer)}
                    onToggle={() => setHiddenLayers(prev => {
                      const next = new Set(prev)
                      if (next.has(layer)) next.delete(layer)
                      else next.add(layer)
                      return next
                    })}
                  />
                ))}
                <div style={{ height: 1, background: 'var(--rule)', margin: '5px 8px' }} />
                <LayerRow label="Pads" swatch="#c8a040" on={showPads} onToggle={() => setShowPads(v => !v)} />
                <LayerRow label="Reference labels" on={showLabels} onToggle={() => setShowLabels(v => !v)} />
                <LayerRow
                  label="Activity overlay"
                  swatch="#ffb347"
                  on={showActivity}
                  onToggle={() => setShowActivity(v => !v)}
                />
                {pickerNets.length > 0 && onNetClick && (
                  <div style={{ padding: '7px 8px 5px' }}>
                    <label className="block text-[10px] font-bold tracking-[0.1em] mb-1" style={{ color: 'var(--silk-faint)' }}>
                      HIGHLIGHT A NET
                    </label>
                    <input
                      className="hb-input w-full"
                      list="viewer-net-options"
                      placeholder="type a net name"
                      value={selectedNet ?? ''}
                      onChange={e => {
                        const v = e.target.value
                        if (v === '') onNetClick(null)
                        else if (pickerNets.includes(v)) onNetClick(v)
                      }}
                    />
                    <datalist id="viewer-net-options">
                      {pickerNets.map(n => <option key={n} value={n} />)}
                    </datalist>
                  </div>
                )}
              </div>
            )}
          </div>
        )}
      </div>

      {loading && (
        <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div className="flex flex-col items-center gap-3">
            <div className="w-8 h-8 border-2 border-t-transparent rounded-full animate-spin"
              style={{ borderColor: 'var(--copper)', borderTopColor: 'transparent' }} />
            <span className="text-sm" style={{ color: '#64748b' }}>Parsing board...</span>
          </div>
        </div>
      )}

      {error && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div className="px-4 py-3 rounded-lg text-sm" style={{ background: '#1e293b', color: '#f87171', border: '1px solid #991b1b' }}>
            {error}
          </div>
        </div>
      )}

      {board && !loading && viewMode === '2d' && (
        <div className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded tnum"
          style={{ background: 'rgba(15,23,42,0.8)', color: '#64748b', pointerEvents: 'none', fontFamily: 'var(--font-mono)' }}>
          {board.footprints.length} fp · {board.segments.length} segs
          {/* Net count: NAMED nets only. The KiCad net table always carries a
              synthetic id-0 "no net" bucket; counting it here made this chip
              disagree with the report banner (which counts real nets) by
              exactly one on every board. When a live frame is present the
              solver's solved-net count is a different (also true) number, so
              it is labelled distinctly rather than shown as a bare conflict. */}
          {frame && Object.keys(frame.net_voltages).length > 0
            ? ` · ${Object.keys(frame.net_voltages).length} nets solved`
            : namedNetCount > 0 ? ` · ${namedNetCount} nets` : null}
        </div>
      )}
    </div>
  )
}

// Re-export FootprintInfo for consumers
export type { FootprintInfo }
