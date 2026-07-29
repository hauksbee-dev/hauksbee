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
import { isLightTheme, onThemeChange } from '../lib/theme-tokens'

interface Board3DViewerProps {
  /** Pre-exported GLB, when one exists for this board. Null falls back to a
   *  model generated from the parsed layout (substrate + pads + bodies). */
  glbUrl: string | null
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
      try {
        // Inside the try: the constructor itself throws where WebGL is
        // unavailable, and that failure must land in the SAME error state
        // (not an unhandled rejection over a spinner that never resolves).
        const viewer = new Viewer3D(canvas)
        viewerRef.current = viewer
        if (glbUrl) {
          await viewer.loadGLB(glbUrl)
        } else if (board) {
          // No exported GLB: build the model from the parsed layout itself,
          // so 3D is available for ANY board the 2D view can draw.
          viewer.buildFromParsedBoard(board)
        } else {
          throw new Error('the board has not loaded yet')
        }
        if (alive) setLoading(false)
      } catch (e) {
        if (alive) {
          setError(e instanceof Error ? e.message : String(e))
          // The error state replaces the loading state; leaving the spinner
          // up painted "Loading..." under the error forever.
          setLoading(false)
        }
      }
    }).catch(e => {
      if (alive) {
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      }
    })

    return () => {
      alive = false
      viewerRef.current?.dispose()
      viewerRef.current = null
    }
    // `board` is loaded once per file and stays referentially stable, so this
    // effect re-runs only when the source (GLB vs parsed board) changes.
  }, [glbUrl, board])

  // Theme toggles while the 3D view is open: retune the scene (fog, exposure)
  // in place rather than tearing down and re-loading the model.
  useEffect(() => onThemeChange(() => viewerRef.current?.setTheme(isLightTheme())), [])

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
      style={{ background: 'var(--scene-bg)' }}
    >
      {/* Radial gradient backdrop, gives the board a grounded space instead of a flat fill */}
      <div
        className="absolute inset-0 pointer-events-none"
        style={{
          background: 'radial-gradient(ellipse 70% 60% at 50% 58%, var(--scene-glow-hi) 0%, var(--scene-glow-mid) 45%, var(--scene-bg) 100%)',
          zIndex: 0,
        }}
      />
      <canvas
        ref={canvasRef}
        className="absolute inset-0 w-full h-full"
        style={{ display: 'block', zIndex: 1 }}
      />

      {loading && !error && (
        <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div className="flex flex-col items-center gap-3">
            <div
              className="w-8 h-8 border-2 rounded-full animate-spin"
              style={{ borderColor: 'var(--trace-1)', borderTopColor: 'transparent' }}
            />
            <span className="text-sm" style={{ color: 'var(--overlay-chip-text)' }}>Loading 3D model...</span>
          </div>
        </div>
      )}

      {error && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div
            className="px-4 py-3 rounded-lg text-sm"
            style={{ background: 'var(--overlay-err-bg)', color: 'var(--err)', border: '1px solid var(--overlay-err-border)', maxWidth: '80%' }}
          >
            <div>3D view unavailable: {error}</div>
            <div className="mt-1 text-xs" style={{ color: 'var(--note)' }}>
              The 2D view still works; switch back with the 2D button above.
            </div>
          </div>
        </div>
      )}

      {!loading && !error && (
        <div
          className="absolute bottom-2 right-2 text-[10px] px-2 py-1 rounded pointer-events-none"
          style={{ background: 'var(--overlay-chip-bg)', color: 'var(--map-note)' }}
        >
          drag=orbit · scroll=zoom · shift+drag=pan
        </div>
      )}
    </div>
  )
}
