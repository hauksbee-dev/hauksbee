import { useEffect, useRef, useState, useCallback, useMemo } from 'react'
import { parseKicadPcb, buildNetIndex } from '../lib/kicad-parser'
import type { ParsedBoard } from '../lib/kicad-parser'
import { makeCamera, zoomCamera, panCamera, screenToWorld } from '../lib/camera'
import type { Camera } from '../lib/camera'
import { renderBoard, renderOverlay } from '../lib/board-renderer'
import type { OverlayData } from '../lib/board-renderer'
import type { SimFrame, BoardInfoMsg } from '../types/protocol'
import { Board3DViewer } from './Board3DViewer'

interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
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

export function BoardViewer({ boardFile, frame, boardInfo, selectedNet, onFootprintClick, onNetClick, onEmptyBoard, faultedRefs }: BoardViewerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const overlayRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)

  const [board, setBoard] = useState<ParsedBoard | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [cam, setCam] = useState<Camera>({ panX: 0, panY: 0, scale: 1 })
  const [viewMode, setViewMode] = useState<'2d' | '3d'>('2d')

  // Interaction state
  const dragging = useRef(false)
  const lastMouse = useRef({ x: 0, y: 0 })
  const animFrame = useRef<number>(0)
  const particlePhases = useRef<Map<string, number>>(new Map())
  const hoveredNet = useRef<string | null>(null)
  const probePos = useRef<{ boardX: number; boardY: number } | null>(null)
  const animTimeRef = useRef(0)
  const lastTickTime = useRef(performance.now())

  const netIndex = useMemo(() => board ? buildNetIndex(board) : null, [board])

  // Precompute per-net segment lists (avoids recomputing in the render loop)
  // This is already done by buildNetIndex; netIndex.get(net).segments is the precomputed list.

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
  useEffect(() => {
    if (!board || !canvasRef.current) return
    const { width: cw, height: ch } = canvasRef.current.getBoundingClientRect()
    const b = board.bounds
    setCam(makeCamera(b.width, b.height, b.cx, b.cy, cw || 800, ch || 600))
  }, [board])

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
      if (board) {
        const b = board.bounds
        setCam(c => {
          const fitScale = Math.min((width * 0.9) / b.width, (height * 0.9) / b.height)
          const refitThreshold = 0.15
          const relativeDiff = Math.abs(c.scale - fitScale) / fitScale
          if (relativeDiff < refitThreshold) {
            return makeCamera(b.width, b.height, b.cx, b.cy, width, height)
          }
          return c
        })
      }
    })
    ro.observe(container)
    return () => ro.disconnect()
  }, [board])

  // ── Find nearest net to a board coordinate ──
  // `reachPx` is the screen-pixel pick radius: hover keeps the tight default
  // so the readout tracks exactly what is under the cursor; a CLICK passes a
  // coarser radius (clicking is a blunter gesture, and on an unrouted board
  // the only copper is pads).
  const findNearestNet = useCallback((bx: number, by: number, reachPx = 3): string | null => {
    if (!board) return null
    let best: string | null = null
    let bestDist = Infinity
    const threshold = reachPx / cam.scale

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

    return best
  }, [board, cam.scale])

  // ── Find footprint at board coordinate ──
  const findFootprintAt = useCallback((bx: number, by: number): FootprintInfo | null => {
    if (!board) return null
    let best: FootprintInfo | null = null
    let bestDist = Infinity
    const threshold = 5 / cam.scale

    for (const fp of board.footprints) {
      const d = Math.sqrt((bx - fp.at.x) ** 2 + (by - fp.at.y) ** 2)
      if (d < threshold && d < bestDist) {
        bestDist = d
        best = { ref: fp.ref, value: fp.value, lib_id: fp.lib_id, x: fp.at.x, y: fp.at.y }
      }
      for (const pad of fp.pads) {
        const pd = Math.sqrt((bx - pad.at.x) ** 2 + (by - pad.at.y) ** 2)
        const padR = Math.max(pad.size.w, pad.size.h) / 2 + 0.5
        if (pd < padR && pd < bestDist) {
          bestDist = pd
          best = { ref: fp.ref, value: fp.value, lib_id: fp.lib_id, x: fp.at.x, y: fp.at.y }
        }
      }
    }
    return best
  }, [board, cam.scale])

  // ── Animation loop (2D only) ──
  const camRef = useRef(cam)
  camRef.current = cam
  const netVoltagesRef = useRef(netVoltagesMap)
  netVoltagesRef.current = netVoltagesMap
  const faultedRefsRef = useRef(faultedRefs)
  faultedRefsRef.current = faultedRefs

  useEffect(() => {
    if (!board || viewMode === '3d') return

    let lastT = performance.now()

    function tick(now: number) {
      const dt = (now - lastT) / 1000
      lastT = now
      animTimeRef.current += dt
      lastTickTime.current = now

      const canvas = canvasRef.current
      const overlay = overlayRef.current
      if (!canvas || !overlay) { animFrame.current = requestAnimationFrame(tick); return }

      const ctx = canvas.getContext('2d')!
      const octx = overlay.getContext('2d')!
      const currentCam = camRef.current

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
        particles,
        probe: probeData,
        componentStates: frame?.component_states,
        componentKinds: boardInfo?.component_kinds,
      }

      if (!board) { animFrame.current = requestAnimationFrame(tick); return }
      renderBoard(ctx, board, currentCam, {
        highlightNets: hlNets,
        dimOthers: hlNets.size > 0,
        netVoltages: netVoltagesRef.current,
        faultedRefs: faultedRefsRef.current,
        animTime: animTimeRef.current,
      })
      renderOverlay(octx, board, currentCam, overlayData)

      animFrame.current = requestAnimationFrame(tick)
    }

    animFrame.current = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(animFrame.current)
  }, [board, selectedNet, netIndex, frame, boardInfo, viewMode])

  // ── Mouse / wheel handlers ──
  const handleWheel = useCallback((e: React.WheelEvent) => {
    e.preventDefault()
    const rect = canvasRef.current!.getBoundingClientRect()
    const sx = e.clientX - rect.left
    const sy = e.clientY - rect.top
    setCam(c => zoomCamera(c, -e.deltaY, sx, sy))
  }, [])

  // A "click" is a press that never travelled: dragging.current is armed on
  // EVERY mousedown (it also drives pan), so it cannot distinguish click from
  // drag; the old `!wasDragging` guard was always false and footprint clicks
  // never fired. Track actual movement instead.
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
      if (Math.abs(dx) + Math.abs(dy) > 2) movedSinceDown.current = true
      lastMouse.current = { x: e.clientX, y: e.clientY }
      setCam(c => panCamera(c, dx, dy))
    } else {
      const { x, y } = screenToWorld(camRef.current, sx, sy)
      probePos.current = { boardX: x, boardY: y }
      hoveredNet.current = findNearestNet(x, y)
    }
  }, [findNearestNet])

  // Shared hit-test for a tap/click that did not travel: resolves the footprint
  // and nearest net under canvas-relative screen coords (sx, sy) and fires the
  // consumer callbacks. Used by BOTH the mouse-up click and the touch tap so the
  // two selection paths cannot drift apart.
  const selectAt = useCallback((sx: number, sy: number) => {
    const { x, y } = screenToWorld(camRef.current, sx, sy)
    const fp = findFootprintAt(x, y)
    if (fp && onFootprintClick) onFootprintClick(fp)
    // Bare-copper click: the nearest trace/pad's net (for "set a check on
    // this net" flows). Fired alongside the footprint hit so a consumer
    // can use either.
    if (onNetClick) onNetClick(findNearestNet(x, y, 8))
  }, [findFootprintAt, onFootprintClick, onNetClick, findNearestNet])

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
      lastMouse.current = { x: e.touches[0].clientX, y: e.touches[0].clientY }
      setCam(c => panCamera(c, dx, dy))
    } else if (e.touches.length === 2) {
      const dx = e.touches[0].clientX - e.touches[1].clientX
      const dy = e.touches[0].clientY - e.touches[1].clientY
      const dist = Math.sqrt(dx * dx + dy * dy)
      const delta = dist - lastTouchDist.current
      lastTouchDist.current = dist
      const cx = (e.touches[0].clientX + e.touches[1].clientX) / 2
      const cy = (e.touches[0].clientY + e.touches[1].clientY) / 2
      const rect = canvasRef.current!.getBoundingClientRect()
      setCam(c => zoomCamera(c, delta, cx - rect.left, cy - rect.top))
    }
  }, [])

  const handleTouchEnd = useCallback((e: React.TouchEvent) => {
    dragging.current = false
    const start = touchStartPos.current
    touchStartPos.current = null
    // Tap-to-select: a single touch that lifted without travelling (< 10px) runs
    // the same hit-test the mouse-up click does. Touch previously had no way to
    // select a net or footprint (only mouse onMouseUp fired it).
    if (start && e.touches.length === 0 && e.changedTouches.length === 1) {
      const t = e.changedTouches[0]
      const moved = Math.hypot(t.clientX - start.x, t.clientY - start.y)
      if (moved <= 10) {
        const rect = canvasRef.current!.getBoundingClientRect()
        selectAt(t.clientX - rect.left, t.clientY - rect.top)
      }
    }
  }, [selectAt])

  return (
    <div
      ref={containerRef}
      className="relative w-full h-full overflow-hidden"
      style={{ background: '#020617', cursor: viewMode === '2d' ? (dragging.current ? 'grabbing' : 'crosshair') : 'default' }}
    >
      {/* 2D canvas layer */}
      <canvas
        ref={canvasRef}
        role="img"
        aria-label="Board map: scroll to zoom, drag to pan, click a trace to select its net. Keyboard users can pick a net in the checks panel below."
        className="absolute inset-0"
        style={{ display: viewMode === '2d' ? 'block' : 'none' }}
        onWheel={handleWheel}
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
          the checks panel below (which are ordinary focusable inputs). */}
      <p className="sr-only">
        This is an interactive board map. Selecting a net by pointer needs a mouse
        or touch. Keyboard users can pick a net using the net fields in the checks
        panel below this viewer.
      </p>

      {/* 3D view -- only mounted when 3D tab is active */}
      {viewMode === '3d' && (
        <div className="absolute inset-0">
          {glbUrl ? (
            <Board3DViewer
              glbUrl={glbUrl}
              board={board}
              frame={frame}
              boardInfo={boardInfo}
              faults={frame?.faults}
            />
          ) : (
            <div className="absolute inset-0 flex items-center justify-center">
              <div
                className="px-4 py-3 rounded-lg text-sm"
                style={{ background: '#1e293b', color: '#94a3b8', border: '1px solid #334155' }}
              >
                No 3D model available for this board
              </div>
            </div>
          )}
        </div>
      )}

      {/* 2D/3D toggle */}
      <div
        className="absolute top-3 right-3 z-20 flex rounded overflow-hidden"
        style={{ border: '1px solid #1e293b', boxShadow: '0 2px 8px rgba(0,0,0,0.4)' }}
      >
        <button
          onClick={() => setViewMode('2d')}
          className="px-3 py-1 text-[10px] font-bold tracking-wider transition-all"
          style={{
            background: viewMode === '2d' ? 'rgba(224,138,78,0.14)' : '#0a0f1e',
            color: viewMode === '2d' ? '#ffb072' : '#334155',
            borderRight: '1px solid #1e293b',
          }}
        >
          2D
        </button>
        <button
          disabled={!glbUrl}
          onClick={() => {
            if (!glbUrl) return
            setViewMode('3d')
          }}
          title={!glbUrl ? 'No 3D model available for this board' : undefined}
          className="px-3 py-1 text-[10px] font-bold tracking-wider transition-all"
          style={{
            background: viewMode === '3d' ? 'rgba(224,138,78,0.14)' : '#0a0f1e',
            color: viewMode === '3d' ? '#ffb072' : glbUrl ? '#334155' : '#1e293b',
            cursor: glbUrl ? 'pointer' : 'not-allowed',
          }}
        >
          3D
        </button>
      </div>

      {loading && (
        <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div className="flex flex-col items-center gap-3">
            <div className="w-8 h-8 border-2 border-t-transparent rounded-full animate-spin"
              style={{ borderColor: '#e08a4e', borderTopColor: 'transparent' }} />
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
        <div className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded"
          style={{ background: 'rgba(15,23,42,0.8)', color: '#475569', pointerEvents: 'none' }}>
          {board.footprints.length} fp · {board.segments.length} segs
          {/* Show live net count from simulation frame (consistent with status bar) when available */}
          {frame && Object.keys(frame.net_voltages).length > 0
            ? ` · ${Object.keys(frame.net_voltages).length} nets`
            : board.nets.size > 0 ? ` · ${board.nets.size} nets` : null}
        </div>
      )}
    </div>
  )
}

// Re-export FootprintInfo for App.tsx usage
export type { FootprintInfo }
