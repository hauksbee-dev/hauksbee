import { useEffect, useRef, useState, useCallback, useMemo } from 'react'
import { parseKicadPcb, buildNetIndex, footprintHitBoxes, pickFootprintBox } from '../lib/kicad-parser'
import type { ParsedBoard } from '../lib/kicad-parser'
import { makeCamera, fitScaleFor, zoomCamera, wheelZoomFactor, panCamera, screenToWorld, maxScaleFor, MIN_SCALE } from '../lib/camera'
import type { Camera } from '../lib/camera'
import { renderStaticBoard, renderDynamicOverlay, LABEL_MIN_PX } from '../lib/board-renderer'
import type { OverlayData, RenderOptions } from '../lib/board-renderer'
import { getLayerStyle, boardTheme } from '../lib/layer-colors'
import { onThemeChange } from '../lib/theme-tokens'
import type { SimFrame, BoardInfoMsg } from '../types/protocol'
import { Board3DViewer } from './Board3DViewer'
import { FitIcon, LayersIcon, ExpandIcon, CollapseIcon } from './Icons'
import { displayNet } from '../lib/net-name'

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
  /** Fires when the 2D/3D segmented control switches, so the embedding view
   *  can adapt its own chrome (e.g. the caption under the canvas swaps to
   *  orbit instructions in 3D). */
  onViewModeChange?: (mode: '2d' | '3d') => void
  /** How the wheel behaves over the 2D canvas.
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
  const [viewMode, setViewMode] = useState<'2d' | '3d'>('2d')
  const [layersOpen, setLayersOpen] = useState(false)
  const layersWrap = useRef<HTMLDivElement>(null)
  const layersTrigger = useRef<HTMLButtonElement>(null)
  // `capture-on-focus` only: has the reader claimed the map by clicking it?
  // Until then the wheel belongs to the page. Cleared by a click anywhere else,
  // so the map gives the page back the moment attention moves on.
  const [zoomFocused, setZoomFocused] = useState(false)
  const [hovering, setHovering] = useState(false)

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

  // Theme flipped: the cached static render was painted in the other
  // palette, so it must be redrawn (the rAF loop picks the drop up on its
  // next tick).
  useEffect(() => onThemeChange(() => { staticCache.current.cam = null }), [])

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
  /** The footprint under the cursor, resolved with the click's hit-test. */
  const hoveredRef = useRef<string | null>(null)
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

  // Computed once per board: a 3,000-part flagship must not rebuild the part
  // extents on every click.
  const footprintBoxes = useMemo(() => (board ? footprintHitBoxes(board) : []), [board])

  // ── Find footprint at board coordinate ──
  // Pads and the part origin first (the precise targets), then the part BODY:
  // clicking the plastic between an IC's pads, or the silkscreen box around a
  // two-pad passive, is a click on that part and must select it.
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
    if (best) return best

    const body = pickFootprintBox(footprintBoxes, bx, by)
    return body ? describeFootprint(body) : null
  }, [board, footprintBoxes, describeFootprint])

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

      // Which net, if any, has earned the flow animation.
      //
      // Deliberately not `selectedNet ?? Object.keys(frame.net_voltages)[0]`:
      // whatever net comes first in the frame's map would get animated
      // charge flowing down it, forever, on any board that reported voltages at
      // all. A watchy with no firmware and every net sitting at 0.000 V still
      // showed a net visibly running. That is the viewer inventing a
      // measurement.
      //
      // The animation is now a statement about `net_currents`, and only about
      // `net_currents`: a net flows when the frame MEASURED current through it
      // above the noise floor. No current map (no co-sim, a backend that does
      // not report currents) means no flow, rather than a guess. A net the
      // backend cannot observe never qualifies, however its passive level
      // reads. A selected net does not get flow for being selected; it gets the
      // highlight, which is a statement about the cursor, not about physics.
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
  }, [board, netIndex, viewMode, setCamera, importMarkers])

  // ── Wheel zoom ──
  // Attached natively (non-passive): React registers wheel listeners as
  // passive at the root, so preventDefault in an onWheel prop cannot stop the
  // browser's own pinch-zoom of the page.
  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || viewMode === '3d') return
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
  }, [viewMode, wheelMode, zoomFocused])

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

  // A "click" is a press that never travelled: dragging.current is armed on
  // EVERY mousedown (it also drives pan), so it cannot distinguish click from
  // drag; track actual movement instead.
  const movedSinceDown = useRef(false)

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button === 0) {
      // Touching the map is what claims the wheel from the page.
      setZoomFocused(true)
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
      // Hover resolves the cursor the SAME way the click does (see `selectAt`
      // below): a bare trace first, then the footprint, then its label, then
      // the nearest pad net. Testing only traces and pads leaves parts unlit,
      // so the board reads as though only copper were live, and worse,
      // hovering a part's body highlights nothing while clicking it
      // selected the part. Both questions now get the same answer.
      const traceNet = findNearestNet(x, y, 5, false)
      if (traceNet) {
        hoveredNet.current = traceNet
        hoveredRef.current = null
      } else {
        const fp = findFootprintAt(x, y) ?? findLabelAt(sx, sy)
        hoveredRef.current = fp?.ref ?? null
        // The pad's net still feeds the probe tooltip, so reading a voltage off
        // a part's pin keeps working exactly as before.
        hoveredNet.current = findNearestNet(x, y, 8)
      }
    }
  }, [findNearestNet, findFootprintAt, findLabelAt, setCamera])

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
    hoveredRef.current = null
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
        setCamera(zoomCamera(camRef.current, factor, cx - rect.left, cy - rect.top, maxScaleFor(fitScaleRef.current)))
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

  // Same dismissal as the export menu and the session switcher: an outside
  // click or Escape. The panel covers a third of the map, so re-finding the one
  // button that closes it is not an acceptable way out. Escape hands focus back
  // to the trigger, because a keyboard reader who dismissed the panel has to
  // land somewhere, and the control they just used is the only sane place.
  useEffect(() => {
    if (!layersOpen) return
    const onDown = (e: MouseEvent) => {
      if (!layersWrap.current?.contains(e.target as Node)) setLayersOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // The expanded view listens for Escape on `window`, which bubbles LAST,
      // after `document`. Stopping propagation here is what makes the panel the
      // innermost dismissible surface: one Escape closes the panel and leaves
      // the map expanded, a second collapses it. A guard on the other effect
      // cannot do this, because within a single keydown React has not yet
      // processed the state change and both listeners are still attached.
      e.stopPropagation()
      setLayersOpen(false)
      layersTrigger.current?.focus()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [layersOpen])

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

      {/* The wheel currently belongs to the page: say so, and say how to take
          it, rather than letting the reader discover it by scrolling and
          watching the board vanish. */}
      {viewMode === '2d' && wheelMode === 'capture-on-focus' && hovering && !zoomFocused && (
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
      {/* Wraps. Held to one row, `justify-between` pushed the Layers button and
          the expand control clean off a 320px viewport, where nothing could
          scroll to them: the map's own controls were unreachable on a phone.
          The groups keep their order and Layers stays right-aligned on whatever
          line it lands on. */}
      <div className="absolute top-3 left-3 right-3 z-20 flex flex-wrap items-start justify-between gap-2 pointer-events-none">
        <div className="flex flex-wrap items-center gap-2 pointer-events-auto">
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
              data-testid="view-2d"
              onClick={() => setViewMode('2d')}
              className="hb-press"
              style={toolbarBtn(viewMode === '2d')}
            >
              2D
            </button>
            <button
              type="button"
              data-testid="view-3d"
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
          )}

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

        {viewMode === '2d' && board && (
          <div ref={layersWrap} className="flex flex-col items-end gap-2 ml-auto pointer-events-auto">
            <button
              type="button"
              ref={layersTrigger}
              data-testid="layers-toggle"
              onClick={() => setLayersOpen(o => !o)}
              aria-expanded={layersOpen}
              aria-haspopup="dialog"
              title={layersOpen ? 'Close the layer controls (Esc)' : 'Show and hide board layers'}
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
                <LayerRow label="Pads" swatch={boardTheme().pad} on={showPads} onToggle={() => setShowPads(v => !v)} />
                <LayerRow label="Reference labels" on={showLabels} onToggle={() => setShowLabels(v => !v)} />
                <LayerRow
                  label="Activity overlay"
                  swatch={boardTheme().activity}
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

      {board && !loading && viewMode === '2d' && (
        <div className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded tnum"
          style={{ background: 'var(--overlay-chip-bg)', color: 'var(--overlay-chip-text)', pointerEvents: 'none', fontFamily: 'var(--font-mono)' }}>
          {/* "fp" was a footgun as well as an abbreviation: the report banner
              says "82 parts" and this chip said "86 fp", two numbers about the
              same board with no way to tell whether one of them was wrong.
              Neither was: the extra two are a test point and a mounting hole,
              which are footprints and are not parts. Say the word, and name the
              part count next to it when the embedding view has one. */}
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
