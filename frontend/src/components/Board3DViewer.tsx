/**
 * Board3DViewer.tsx
 *
 * Lazy-loads three.js and renders a KiCad GLB file with orbit controls,
 * studio lighting, and live component state markers.
 *
 * Only mounted when the 3D tab is active; the three.js dynamic import
 * (board-3d-viewer.ts) is deferred until first render of this component.
 */

import { useEffect, useRef, useState, useCallback } from 'react'
import type { ParsedBoard } from '../lib/kicad-parser'
import type { SimFrame, BoardInfoMsg } from '../types/protocol'

interface Board3DViewerProps {
  glbUrl: string
  board: ParsedBoard | null
  frame: SimFrame | null
  boardInfo?: BoardInfoMsg | null
  faults?: { component: string; kind: string; value: number; limit: number; t: number }[]
}

export function Board3DViewer({ glbUrl, board, frame, boardInfo, faults }: Board3DViewerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const viewerRef = useRef<any>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  // Lazy-load the viewer class, then load the GLB
  useEffect(() => {
    if (!canvasRef.current) return
    let alive = true
    setLoading(true)
    setError(null)

    const canvas = canvasRef.current
    const container = containerRef.current

    // Ensure the canvas pixel dimensions are set before Three.js reads them.
    // In headless (Playwright) the ResizeObserver may not have fired yet; sync explicitly.
    if (container) {
      const { width, height } = container.getBoundingClientRect()
      if (width > 0 && height > 0) {
        canvas.width = Math.round(width)
        canvas.height = Math.round(height)
      }
    }

    import('../lib/board-3d-viewer').then(async ({ Board3DViewer: Viewer3D }) => {
      if (!alive) return
      const viewer = new Viewer3D(canvas)
      viewerRef.current = viewer
      try {
        await viewer.loadGLB(glbUrl)
        if (alive) setLoading(false)
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e))
      }
    }).catch(e => {
      if (alive) setError(e instanceof Error ? e.message : String(e))
    })

    return () => {
      alive = false
      viewerRef.current?.dispose()
      viewerRef.current = null
    }
  }, [glbUrl])

  // Resize observer
  useEffect(() => {
    const container = containerRef.current
    if (!container) return
    const ro = new ResizeObserver(entries => {
      const { width, height } = entries[0].contentRect
      canvasRef.current!.width = width
      canvasRef.current!.height = height
      viewerRef.current?.setSize(width, height)
    })
    ro.observe(container)
    return () => ro.disconnect()
  }, [])

  // Update markers whenever frame changes
  const updateMarkers = useCallback(() => {
    const viewer = viewerRef.current
    if (!viewer || !board || !frame) return
    viewer.updateMarkers(
      board,
      frame.component_states ?? {},
      boardInfo?.component_kinds ?? {},
      faults,
    )
  }, [board, frame, boardInfo, faults])

  useEffect(() => {
    updateMarkers()
  }, [updateMarkers])

  return (
    <div
      ref={containerRef}
      className="relative w-full h-full overflow-hidden"
      style={{ background: '#020617' }}
    >
      {/* Radial gradient backdrop, gives the board a grounded space instead of flat black */}
      <div
        className="absolute inset-0 pointer-events-none"
        style={{
          background: 'radial-gradient(ellipse 70% 60% at 50% 58%, #0d1a2e 0%, #07101f 45%, #020617 100%)',
          zIndex: 0,
        }}
      />
      <canvas
        ref={canvasRef}
        className="absolute inset-0 w-full h-full"
        style={{ display: 'block', zIndex: 1 }}
      />

      {loading && (
        <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div className="flex flex-col items-center gap-3">
            <div
              className="w-8 h-8 border-2 rounded-full animate-spin"
              style={{ borderColor: '#3b82f6', borderTopColor: 'transparent' }}
            />
            <span className="text-sm" style={{ color: '#64748b' }}>Loading 3D model...</span>
          </div>
        </div>
      )}

      {error && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div
            className="px-4 py-3 rounded-lg text-sm"
            style={{ background: '#1e293b', color: '#f87171', border: '1px solid #991b1b' }}
          >
            3D model error: {error}
          </div>
        </div>
      )}

      {!loading && !error && (
        <div
          className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded pointer-events-none"
          style={{ background: 'rgba(15,23,42,0.8)', color: '#475569' }}
        >
          drag=orbit · scroll=zoom · shift+drag=pan
        </div>
      )}
    </div>
  )
}
