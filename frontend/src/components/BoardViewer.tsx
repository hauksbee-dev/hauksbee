import { useEffect, useRef, useState, useCallback, useMemo } from 'react'
import { parseKicadPcb, buildNetIndex } from '../lib/kicad-parser'
import type { ParsedBoard } from '../lib/kicad-parser'
import { makeCamera, zoomCamera, panCamera, screenToWorld } from '../lib/camera'
import type { Camera } from '../lib/camera'
import { renderBoard, renderOverlay } from '../lib/board-renderer'
import type { OverlayData } from '../lib/board-renderer'
import type { SimFrame } from '../types/protocol'

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
  /** Externally chosen net to highlight (e.g., from probe click) */
  selectedNet?: string | null
  onFootprintClick?: (info: FootprintInfo) => void
}

const PARTICLE_COUNT = 4
const PARTICLE_SPEED = 0.3 // t units per second

export function BoardViewer({ boardFile, frame, selectedNet, onFootprintClick }: BoardViewerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const overlayRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)

  const [board, setBoard] = useState<ParsedBoard | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [cam, setCam] = useState<Camera>({ panX: 0, panY: 0, scale: 1 })

  // Interaction state
  const dragging = useRef(false)
  const lastMouse = useRef({ x: 0, y: 0 })
  const animFrame = useRef<number>(0)
  const particlePhases = useRef<Map<string, number>>(new Map())
  const hoveredNet = useRef<string | null>(null)
  const probePos = useRef<{ boardX: number; boardY: number } | null>(null)

  const netIndex = useMemo(() => board ? buildNetIndex(board) : null, [board])

  // ── Load board ──
  useEffect(() => {
    setLoading(true)
    setError(null)
    setBoard(null)
    fetch(boardFile)
      .then(r => {
        if (!r.ok) throw new Error(`HTTP ${r.status}: ${r.url}`)
        return r.text()
      })
      .then(text => {
        const parsed = parseKicadPcb(text)
        setBoard(parsed)
        setLoading(false)
      })
      .catch((e: Error) => {
        setError(e.message)
        setLoading(false)
      })
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
          // Refit only if scale is the initial fit
          const fitScale = Math.min((width * 0.9) / b.width, (height * 0.9) / b.height)
          // If scale is close to a fit scale, refit
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
  const findNearestNet = useCallback((bx: number, by: number): string | null => {
    if (!board) return null
    let best: string | null = null
    let bestDist = Infinity
    const threshold = 3 / cam.scale // 3 screen px

    for (const s of board.segments) {
      if (!s.netName) continue
      // Distance from point to segment
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

    // Also check pads
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
      // Also check if within any pad
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

  // ── Animation loop ──
  const camRef = useRef(cam)
  camRef.current = cam

  useEffect(() => {
    if (!board) return

    let lastT = performance.now()

    function tick(now: number) {
      const dt = (now - lastT) / 1000
      lastT = now

      const canvas = canvasRef.current
      const overlay = overlayRef.current
      if (!canvas || !overlay) { animFrame.current = requestAnimationFrame(tick); return }

      const ctx = canvas.getContext('2d')!
      const octx = overlay.getContext('2d')!
      const currentCam = camRef.current

      // Collect nets to highlight
      const hlNets = new Set<string>()
      if (selectedNet) hlNets.add(selectedNet)
      if (hoveredNet.current) hlNets.add(hoveredNet.current)

      // Advance particles
      const flowNet = selectedNet ?? (frame?.net_voltages ? Object.keys(frame.net_voltages)[0] : null)
      if (flowNet && netIndex?.has(flowNet)) {
        const segs = netIndex.get(flowNet)!.segments
        if (segs.length > 0) {
          // Cycle particles per-segment
          for (let i = 0; i < PARTICLE_COUNT; i++) {
            const key = `${flowNet}:${i}`
            const prev = particlePhases.current.get(key) ?? (i / PARTICLE_COUNT)
            const next = (prev + PARTICLE_SPEED * dt) % 1
            particlePhases.current.set(key, next)
          }
        }
      }

      // Build overlay data
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

      // Probe tooltip
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
      }

      if (!board) { animFrame.current = requestAnimationFrame(tick); return }
      renderBoard(ctx, board, currentCam, {
        highlightNets: hlNets,
        dimOthers: hlNets.size > 0,
      })
      renderOverlay(octx, board, currentCam, overlayData)

      animFrame.current = requestAnimationFrame(tick)
    }

    animFrame.current = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(animFrame.current)
  }, [board, selectedNet, netIndex, frame])

  // ── Mouse / wheel handlers ──
  const handleWheel = useCallback((e: React.WheelEvent) => {
    e.preventDefault()
    const rect = canvasRef.current!.getBoundingClientRect()
    const sx = e.clientX - rect.left
    const sy = e.clientY - rect.top
    setCam(c => zoomCamera(c, -e.deltaY, sx, sy))
  }, [])

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button === 0) {
      dragging.current = true
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
      lastMouse.current = { x: e.clientX, y: e.clientY }
      setCam(c => panCamera(c, dx, dy))
    } else {
      // Update hover net and probe position
      const { x, y } = screenToWorld(camRef.current, sx, sy)
      probePos.current = { boardX: x, boardY: y }
      hoveredNet.current = findNearestNet(x, y)
    }
  }, [findNearestNet])

  const handleMouseUp = useCallback((e: React.MouseEvent) => {
    if (e.button === 0 && !dragging.current) {
      // click
    }
    if (e.button === 0) {
      const wasDragging = dragging.current
      dragging.current = false
      if (!wasDragging) {
        // Single click: check footprint
        const rect = canvasRef.current!.getBoundingClientRect()
        const sx = e.clientX - rect.left
        const sy = e.clientY - rect.top
        const { x, y } = screenToWorld(camRef.current, sx, sy)
        const fp = findFootprintAt(x, y)
        if (fp && onFootprintClick) onFootprintClick(fp)
      }
    }
  }, [findFootprintAt, onFootprintClick])

  const handleMouseLeave = useCallback(() => {
    dragging.current = false
    hoveredNet.current = null
    probePos.current = null
  }, [])

  // Touch support
  const lastTouchDist = useRef<number>(0)
  const handleTouchStart = useCallback((e: React.TouchEvent) => {
    if (e.touches.length === 1) {
      dragging.current = true
      lastMouse.current = { x: e.touches[0].clientX, y: e.touches[0].clientY }
    } else if (e.touches.length === 2) {
      dragging.current = false
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

  const handleTouchEnd = useCallback(() => { dragging.current = false }, [])

  return (
    <div
      ref={containerRef}
      className="relative w-full h-full overflow-hidden"
      style={{ background: '#020617', cursor: dragging.current ? 'grabbing' : 'crosshair' }}
    >
      <canvas
        ref={canvasRef}
        className="absolute inset-0"
        style={{ display: 'block' }}
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
        style={{ display: 'block' }}
      />

      {loading && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div className="flex flex-col items-center gap-3">
            <div className="w-8 h-8 border-2 border-t-transparent rounded-full animate-spin"
              style={{ borderColor: '#3b82f6', borderTopColor: 'transparent' }} />
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

      {board && !loading && (
        <div className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded"
          style={{ background: 'rgba(15,23,42,0.8)', color: '#475569', pointerEvents: 'none' }}>
          {board.footprints.length} fp · {board.segments.length} segs · {board.nets.size} nets
        </div>
      )}
    </div>
  )
}
