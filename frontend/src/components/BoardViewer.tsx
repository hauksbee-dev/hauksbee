import { useEffect, useRef, useState, useCallback, useMemo } from 'react'
import { parseKicadPcb } from '../lib/kicad-parser'
import type { ParsedBoard } from '../lib/kicad-parser'
import { segmentsByNet } from '../lib/board-geometry'
import type { FootprintInfo } from '../lib/board-geometry'
import { makeCamera, fitScaleFor, zoomCamera, wheelZoomFactor, maxScaleFor, MIN_SCALE } from '../lib/camera'
import type { Camera } from '../lib/camera'
import { renderStaticBoard, renderDynamicOverlay } from '../lib/board-renderer'
import type { OverlayData, RenderOptions } from '../lib/board-renderer'
import { getLayerStyle, boardTheme } from '../lib/layer-colors'
import { onThemeChange } from '../lib/theme-tokens'
import type { SimFrame, BoardInfoMsg } from '../types/protocol'
import { FitIcon, ExpandIcon, CollapseIcon } from './Icons'
import { displayNet } from '../lib/net-name'
import { fetchFile, isAbort } from '../lib/api'
import { LayersControl, useLayerControls } from './board/LayersPanel'
import { useBoardPointer } from '../hooks/useBoardPointer'

/** Pixels from the top of the viewer to clear the floating toolbar. The toolbar
 *  sits at top-3 (12px) and its controls are 28px tall inside a 1px border;
 *  anything anchored below this cannot collide with it. Exported so the views
 *  that float a selection card over the viewer stay in step with the toolbar. */
export const TOOLBAR_CLEARANCE = 52

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
  /** Located objects from the import-coverage panel. Missing/unplaced objects
   *  are intentionally absent because the reader supplied no coordinate. */
  importMarkers?: Array<{ x: number; y: number; status: 'recovered' | 'partial'; nets: string[] }>
  /** Expanded-to-viewport state, owned by the embedding view (it also holds
   *  the floating selection card, so the two must expand together). When a
   *  toggle is supplied the toolbar grows a fullscreen control and Escape
   *  collapses; without one the control is absent. */
  fullscreen?: boolean
  onToggleFullscreen?: () => void
  /** How the wheel behaves over the canvas.
   *
   *  `'always'` (default) suits a full-height surface that owns the viewport:
   *  the wheel is the zoom, there is nothing behind it to scroll.
   *
   *  `'capture-on-focus'` suits a map embedded in a scrolling document (the
   *  report): a plain wheel scrolls the PAGE, and the canvas only takes the
   *  wheel once the reader has clicked it, or while ctrl/cmd is held. Without
   *  that, skimming past the map zooms the board down to 1% and leaves a blank
   *  panel. */
  wheelMode?: 'always' | 'capture-on-focus'
  /** The engine's PART count for this board, when the embedding view has a
   *  report to hand. The footer chip counts FOOTPRINTS, which is a larger
   *  number on nearly every real board: test points, mounting holes, fiducials
   *  and logos are all footprints and none of them is a part. Shown as
   *  "86 footprints (82 parts)" so the two numbers on screen explain each other
   *  instead of contradicting each other. */
  partCount?: number
}

const PARTICLE_COUNT = 4
const PARTICLE_SPEED = 0.3 // t units per second

/** Below this the reported per-net current is solver noise, not flow (1 µA).
 *  The flow animation is a claim that charge is moving; it may only be made
 *  about a net whose current the frame actually MEASURED above this floor. */
const FLOW_CURRENT_FLOOR_A = 1e-6

export function BoardViewer({
  boardFile, frame, boardInfo, selectedNet, onFootprintClick, onNetClick,
  onEmptyBoard, faultedRefs, netOptions, focusPoint,
  importMarkers,
  fullscreen = false, onToggleFullscreen,
  wheelMode = 'always',
  partCount,
}: BoardViewerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const overlayRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)
  const zoomReadoutRef = useRef<HTMLSpanElement>(null)

  const [board, setBoard] = useState<ParsedBoard | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  // `capture-on-focus` only: has the reader claimed the map by clicking it?
  // Until then the wheel belongs to the page. Cleared by a click anywhere else,
  // so the map gives the page back the moment attention moves on.
  const [zoomFocused, setZoomFocused] = useState(false)
  const [hovering, setHovering] = useState(false)

  // Per-layer overrides plus the pads/labels/activity switches. Hidden layers
  // also stop rendering activity on their copper.
  const layers = useLayerControls()
  const { hiddenLayers, showPads, showLabels, showActivity } = layers

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
  // Auto-refit (on becoming visible again, on resize) is
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

  // Theme flipped: the cached static render was painted in the other
  // palette, so it must be redrawn (the rAF loop picks the drop up on its
  // next tick).
  useEffect(() => onThemeChange(() => { staticCache.current.cam = null }), [])

  // Smooth zoom: wheel ticks set a target scale and the animation loop glides
  // the camera toward it (anchored at the cursor), so discrete wheel notches
  // do not step visibly.
  const zoomTarget = useRef<{ scale: number; sx: number; sy: number } | null>(null)

  const animFrame = useRef<number>(0)
  const particlePhases = useRef<Map<string, number>>(new Map())
  const hoveredNet = useRef<string | null>(null)
  /** The footprint under the cursor, resolved with the click's hit-test. */
  const hoveredRef = useRef<string | null>(null)
  const probePos = useRef<{ boardX: number; boardY: number } | null>(null)
  const animTimeRef = useRef(0)

  const netIndex = useMemo(() => board ? segmentsByNet(board) : null, [board])
  // Real (named) nets only: the KiCad net table's synthetic id-0 "" bucket is
  // not a net, and counting it disagreed with the report's own net count.
  const namedNetCount = useMemo(
    () => board ? [...board.nets.values()].filter(Boolean).length : 0,
    [board],
  )

  // Nets whose drive this backend cannot see. The frame still carries a number
  // for them, but that number is the passive network's static level, not a
  // measurement of what the MCU is doing. Anything that would read as "we
  // measured this" has to leave them out.
  const unobservedNets = useMemo(
    () => new Set(frame?.unobserved_drive_nets ?? []),
    [frame?.unobserved_drive_nets],
  )

  // Build net voltages map (throttled by frame reference -- only recompute when
  // frame changes). Unobserved nets are dropped rather than passed through as a
  // measured value: the voltage tint says "this net is sitting here", which is
  // a claim the backend has not earned for them. They render as bare copper,
  // and the probe tooltip below names them as not observed.
  const netVoltagesMap = useMemo(() => {
    if (!frame?.net_voltages) return undefined
    const m = new Map<string, number>()
    for (const [net, v] of Object.entries(frame.net_voltages)) {
      if (!unobservedNets.has(net)) m.set(net, v)
    }
    return m
  }, [frame?.net_voltages, unobservedNets])

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
    fetchFile(boardFile, ctrl.signal)
      .then(f => f.text())
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
        if (cancelled || isAbort(e)) return
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

  // ── Animation loop ──
  // Per-frame data reaches the loop through refs, NOT effect deps: putting
  // `frame` in the deps tore down and restarted the rAF loop 30 times a
  // second on a live sim.
  const netVoltagesRef = useRef(netVoltagesMap)
  netVoltagesRef.current = netVoltagesMap
  const unobservedNetsRef = useRef(unobservedNets)
  unobservedNetsRef.current = unobservedNets
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
    const scale = Math.min(maxScaleFor(fitScaleRef.current), Math.max(fitScaleRef.current * 5, camRef.current.scale))
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
    if (!board) return

    let lastT = performance.now()
    let lastReadout = 0

    // Draw the cached static board onto the main canvas, transformed from the
    // camera it was rendered at to the current camera. Between static
    // re-renders (pan in flight, zoom gliding) this is one drawImage.
    function blitStatic(ctx: CanvasRenderingContext2D, cam: Camera) {
      const st = staticCache.current
      if (!st.canvas || !st.cam) return
      if (st.canvas.width === 0 || st.canvas.height === 0) return
      ctx.fillStyle = boardTheme().bg
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
          setCamera(zoomCamera(cur, zt.scale / cur.scale, zt.sx, zt.sy, maxScaleFor(fitScaleRef.current)))
          zoomTarget.current = null
        } else {
          const step = Math.exp(logRatio * Math.min(1, dt * 16))
          setCamera(zoomCamera(cur, step, zt.sx, zt.sy, maxScaleFor(fitScaleRef.current)))
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

      // Which net, if any, has earned the flow animation. It is a statement
      // about `net_currents` and nothing else: a net flows when the frame
      // MEASURED current through it above the noise floor. No current map (no
      // co-sim, a backend that does not report currents) means no flow rather
      // than a guess, and a net the backend cannot observe never qualifies
      // however its passive level reads. Selection does not earn flow; it earns
      // the highlight, which is a claim about the cursor, not about physics.
      const currents = frame?.net_currents
      const flowNet = (() => {
        if (!currents) return null
        let best: string | null = null
        let bestMag = FLOW_CURRENT_FLOOR_A
        for (const [net, a] of Object.entries(currents)) {
          if (unobservedNetsRef.current.has(net)) continue
          const mag = Math.abs(a)
          if (mag > bestMag) { bestMag = mag; best = net }
        }
        // Prefer the net under inspection when it is genuinely carrying, so
        // clicking a live net does not move the animation somewhere else.
        if (selectedNet
          && !unobservedNetsRef.current.has(selectedNet)
          && Math.abs(currents[selectedNet] ?? 0) > FLOW_CURRENT_FLOOR_A) {
          return selectedNet
        }
        return best
      })()
      if (flowNet && netIndex?.has(flowNet)) {
        const segs = netIndex.get(flowNet)!
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

      // The probe reads out what was measured. On a net whose drive the backend
      // cannot see, the frame's number is the passive level, so the tooltip
      // says so instead of printing a confident "0.000 V".
      const probeNet = hoveredNet.current
      const probeData = probeNet && probePos.current && frame?.net_voltages[probeNet] !== undefined
        ? {
            x: probePos.current.boardX,
            y: probePos.current.boardY,
            label: displayNet(probeNet),
            value: unobservedNetsRef.current.has(probeNet)
              ? 'not observed'
              : `${(frame.net_voltages[probeNet]!).toFixed(3)} V`,
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
        hoverRefs: hoveredRef.current ? new Set([hoveredRef.current]) : undefined,
        animTime: animTimeRef.current,
        showActivity: showActivityRef.current,
        renderOpts: renderOptsRef.current,
        marker: markerRef.current,
        importMarkers,
      }

      ensureStatic(canvas, now)
      blitStatic(ctx, currentCam)
      renderDynamicOverlay(octx, board, currentCam, overlayData)

      animFrame.current = requestAnimationFrame(tick)
    }

    animFrame.current = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(animFrame.current)
  }, [board, netIndex, setCamera, importMarkers])

  // ── Wheel zoom ──
  // Attached natively (non-passive): React registers wheel listeners as
  // passive at the root, so preventDefault in an onWheel prop cannot stop the
  // browser's own pinch-zoom of the page.
  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const onWheel = (e: WheelEvent) => {
      // Embedded in a scrolling document, the map only takes the wheel with
      // intent: ctrl/cmd held (the universal "zoom this, not the page"), or
      // after the reader clicked it. Otherwise let the event through so the
      // page scrolls past, instead of silently zooming the board to nothing.
      // A trackpad pinch also arrives as ctrlKey+wheel, which is exactly the
      // gesture that should zoom here.
      if (wheelMode === 'capture-on-focus' && !zoomFocused && !e.ctrlKey && !e.metaKey) return
      e.preventDefault()
      const rect = canvas.getBoundingClientRect()
      const sx = e.clientX - rect.left
      const sy = e.clientY - rect.top
      const factor = wheelZoomFactor(e)
      const base = zoomTarget.current?.scale ?? camRef.current.scale
      // Clamped against the FIT scale, not the absolute px-per-mm ceiling:
      // see MAX_ZOOM_RATIO in lib/camera.ts for why 3000 px/mm was not a
      // limit at all on a small board. Fit is unaffected, and zooming out
      // still goes to MIN_SCALE.
      const target = Math.max(MIN_SCALE, Math.min(maxScaleFor(fitScaleRef.current), base * factor))
      zoomTarget.current = { scale: target, sx, sy }
      camMovedAt.current = performance.now()
      userMovedCamera.current = true
    }
    canvas.addEventListener('wheel', onWheel, { passive: false })
    return () => canvas.removeEventListener('wheel', onWheel)
  }, [wheelMode, zoomFocused])

  // Give the wheel back to the page as soon as the reader clicks away. Capture
  // phase, so it fires even when the click lands on something that stops
  // propagation.
  useEffect(() => {
    if (wheelMode !== 'capture-on-focus' || !zoomFocused) return
    const onDocDown = (e: PointerEvent) => {
      if (!containerRef.current?.contains(e.target as Node)) setZoomFocused(false)
    }
    document.addEventListener('pointerdown', onDocDown, true)
    return () => document.removeEventListener('pointerdown', onDocDown, true)
  }, [wheelMode, zoomFocused])

  const pointer = useBoardPointer({
    board,
    showLabels,
    canvasRef,
    camRef,
    fitScaleRef,
    userMovedCamera,
    setCamera,
    hoveredNet,
    hoveredRef,
    probePos,
    onClaimWheel: () => setZoomFocused(true),
    onNetClick,
    onFootprintClick,
  })

  // Nets for the highlight picker: prefer the live protocol's list, fall back
  // to what the parser found on the copper.
  const pickerNets = useMemo(() => {
    if (netOptions && netOptions.length > 0) return netOptions
    if (boardInfo?.nets && boardInfo.nets.length > 0) return boardInfo.nets
    if (!board) return []
    return [...board.nets.values()].filter(Boolean).sort()
  }, [netOptions, boardInfo, board])

  // Escape leaves the expanded view, the way every other fullscreen surface
  // behaves. Only bound while expanded, so it never eats an Escape elsewhere.
  // Bound on `window` deliberately: it is the last node an Escape reaches, so
  // any dismissible surface inside the view gets first refusal on the key.
  useEffect(() => {
    if (!fullscreen || !onToggleFullscreen) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') { e.preventDefault(); onToggleFullscreen() }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [fullscreen, onToggleFullscreen])

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
      onMouseEnter={() => setHovering(true)}
      onMouseLeave={() => setHovering(false)}
      style={{ background: 'var(--instrument)', cursor: pointer.dragging.current ? 'grabbing' : 'crosshair' }}
    >
      {/* Board canvas layer */}
      <canvas
        ref={canvasRef}
        role="img"
        aria-label="Board map: scroll to zoom, drag to pan, click a trace to select its net. Keyboard users can pick a net in the checks panel."
        className="absolute inset-0"
        onMouseDown={pointer.onMouseDown}
        onMouseMove={pointer.onMouseMove}
        onMouseUp={pointer.onMouseUp}
        onMouseLeave={pointer.onMouseLeave}
        onTouchStart={pointer.onTouchStart}
        onTouchMove={pointer.onTouchMove}
        onTouchEnd={pointer.onTouchEnd}
      />
      <canvas
        ref={overlayRef}
        className="absolute inset-0 pointer-events-none"
      />

      {/* The wheel currently belongs to the page: say so, and say how to take
          it, rather than letting the reader discover it by scrolling and
          watching the board vanish. */}
      {wheelMode === 'capture-on-focus' && hovering && !zoomFocused && (
        <div
          data-testid="zoom-hint"
          className="absolute bottom-3 left-1/2 z-20 px-2.5 py-1 rounded-md text-[11px] pointer-events-none"
          style={{
            transform: 'translateX(-50%)',
            background: 'color-mix(in srgb, var(--surface) 88%, transparent)',
            border: '1px solid var(--hairline)',
            backdropFilter: 'blur(6px)',
            color: 'var(--silk-dim)',
          }}
        >
          Click to enable zoom · or hold ctrl and scroll
        </div>
      )}

      {/* Visually-hidden guidance for keyboard / screen-reader users: the canvas
          has no keyboard net-selection path, so point them to the net pickers in
          the checks panel (which are ordinary focusable inputs). */}
      <p className="sr-only">
        This is an interactive board map. Selecting a net by pointer needs a mouse
        or touch. Keyboard users can pick a net using the net fields in the checks
        view.
      </p>

      {/* ── Viewer toolbar ── */}
      {/* Wraps. Held to one row, `justify-between` pushed the Layers button and
          the expand control clean off a 320px viewport, where nothing could
          scroll to them: the map's own controls were unreachable on a phone.
          The groups keep their order and Layers stays right-aligned on whatever
          line it lands on. */}
      <div className="absolute top-3 left-3 right-3 z-20 flex flex-wrap items-start justify-between gap-2 pointer-events-none">
        <div className="flex flex-wrap items-center gap-2 pointer-events-auto">
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
              data-testid="zoom-readout"
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

          {onToggleFullscreen && (
            <button
              type="button"
              data-testid="viewer-fullscreen"
              onClick={onToggleFullscreen}
              aria-pressed={fullscreen}
              title={fullscreen ? 'Collapse the board view (Esc)' : 'Expand the board view'}
              aria-label={fullscreen ? 'Collapse the board view' : 'Expand the board view'}
              className="hb-press rounded-lg"
              style={{
                ...toolbarBtn(fullscreen),
                border: '1px solid var(--hairline)',
                background: fullscreen
                  ? 'var(--copper-tint-strong)'
                  : 'color-mix(in srgb, var(--surface) 88%, transparent)',
                backdropFilter: 'blur(6px)',
                boxShadow: 'var(--shadow-card)',
              }}
            >
              {fullscreen ? <CollapseIcon size={13} /> : <ExpandIcon size={13} />}
            </button>
          )}
        </div>

        {board && (
          <LayersControl
            board={board}
            controls={layers}
            pickerNets={pickerNets}
            selectedNet={selectedNet}
            onNetClick={onNetClick}
            buttonStyle={toolbarBtn}
          />
        )}
      </div>

      {loading && (
        <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div className="flex flex-col items-center gap-3">
            <div className="w-8 h-8 border-2 border-t-transparent rounded-full animate-spin"
              style={{ borderColor: 'var(--copper)', borderTopColor: 'transparent' }} />
            <span className="text-sm" style={{ color: 'var(--overlay-chip-text)' }}>Parsing board...</span>
          </div>
        </div>
      )}

      {error && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div className="px-4 py-3 rounded-lg text-sm" style={{ background: 'var(--overlay-err-bg)', color: 'var(--err)', border: '1px solid var(--overlay-err-border)' }}>
            {error}
          </div>
        </div>
      )}

      {board && !loading && (
        <div className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded tnum"
          style={{ background: 'var(--overlay-chip-bg)', color: 'var(--overlay-chip-text)', pointerEvents: 'none', fontFamily: 'var(--font-mono)' }}>
          {/* Footprints, said in full, with the part count beside it when the
              embedding view has one: a test point and a mounting hole are
              footprints and are not parts, so the two numbers differ on nearly
              every real board and each has to explain the other. */}
          {board.footprints.length} footprint{board.footprints.length === 1 ? '' : 's'}
          {partCount !== undefined && partCount !== board.footprints.length && (
            <span title="Footprints include test points, mounting holes, fiducials and logos; parts are the components the analysis reasons about.">
              {' '}({partCount} part{partCount === 1 ? '' : 's'})
            </span>
          )}
          {' · '}{board.segments.length} segs
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
