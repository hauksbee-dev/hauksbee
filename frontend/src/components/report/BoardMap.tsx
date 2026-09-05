import { useEffect, useRef } from 'react'
import type { WebComponent, WebImportDiagnostics } from '../../types/report'
import { cssToken, onThemeChange } from '../../lib/theme-tokens'

/** Simple 2D footprint dot map, drawn from the report's component positions
 *  (board mm), for formats the client-side renderer cannot draw. Sits on the
 *  instrument surface and follows the theme via the --map-* tokens. */
export function BoardMap({ components, importDiagnostics, showImportOverlay, selectedNet }: {
  components: WebComponent[]
  importDiagnostics?: WebImportDiagnostics
  showImportOverlay?: boolean
  selectedNet?: string | null
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const cv = canvasRef.current
    const ctx = cv?.getContext('2d')
    if (!cv || !ctx) return
    const draw = () => {
      const W = cv.width, H = cv.height, pad = 28
      let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity
      for (const c of components) {
        minX = Math.min(minX, c.x); minY = Math.min(minY, c.y)
        maxX = Math.max(maxX, c.x); maxY = Math.max(maxY, c.y)
      }
      const scale = Math.min(
        (W - 2 * pad) / Math.max(1e-6, maxX - minX),
        (H - 2 * pad) / Math.max(1e-6, maxY - minY),
      )
      ctx.clearRect(0, 0, W, H)
      // Labels only while they can be read: past a few hundred parts every
      // reference overlaps its neighbours and the map collapses into a grey
      // smear. Dense boards get clean position dots; the real geometry lives in
      // the BoardViewer path.
      const drawLabels = components.length <= 300
      const dotR = components.length > 1000 ? 1.5 : 3
      const importStatus = new Map((importDiagnostics?.objects ?? []).map(o => [o.id, o.status]))
      const selectedObjects = new Set((importDiagnostics?.objects ?? [])
        .filter(o => selectedNet && o.nets?.includes(selectedNet))
        .map(o => o.id))
      for (const c of components) {
        const x = pad + (c.x - minX) * scale
        const y = pad + (c.y - minY) * scale
        const status = showImportOverlay ? importStatus.get(c.reference) : undefined
        ctx.fillStyle = status === 'recovered' ? '#22c55e' : status === 'partial' ? '#f59e0b' : cssToken('--map-dot')
        ctx.beginPath(); ctx.arc(x, y, status ? dotR + 2 : dotR, 0, Math.PI * 2); ctx.fill()
        if (selectedObjects.has(c.reference)) {
          ctx.strokeStyle = cssToken('--copper-hi')
          ctx.lineWidth = 2
          ctx.beginPath(); ctx.arc(x, y, dotR + 6, 0, Math.PI * 2); ctx.stroke()
        }
        if (drawLabels) {
          ctx.fillStyle = cssToken('--map-label'); ctx.font = '10px sans-serif'
          ctx.fillText(c.reference, x + 5, y + 3)
        }
      }
      if (!drawLabels) {
        ctx.fillStyle = cssToken('--map-note'); ctx.font = '11px sans-serif'
        ctx.fillText(`${components.length} parts (labels hidden at this density)`, pad, H - 10)
      }
    }
    draw()
    // Canvas pixels do not restyle themselves when the theme flips; redraw.
    return onThemeChange(draw)
  }, [components, importDiagnostics, selectedNet, showImportOverlay])

  return (
    <canvas
      ref={canvasRef}
      width={760}
      height={460}
      className="w-full rounded-lg block"
      style={{ background: 'var(--instrument)', border: '1px solid var(--instrument-edge)' }}
    />
  )
}
