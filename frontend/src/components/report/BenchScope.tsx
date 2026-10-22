import { useContext, useEffect, useMemo, useRef } from 'react'
import type { BenchReport } from '../../types/bench'
import { benchLanes, benchSummaryLine, decodedItems, eventMarkers } from '../../lib/bench-view'
import { LANE_H } from '../bench-draw'
import { drawScope, plotExtraHeight, plotTopPad, timeTickLabeler } from '../scope-draw'
import type { ScopePlot } from '../scope-draw'
import { onThemeChange } from '../../lib/theme-tokens'
import { ResultLinkContext } from './ResultLinks'
import { displayNet } from '../../lib/net-name'

// The run view under the bench strip: the logic analyser. One lane per pin the
// firmware drove, a decoded track for the buses, and a dashed rule for every
// event, all on one axis of simulated seconds. The focused pin (the chip last
// clicked) is the highlighted lane. The painting is scope-draw's, shared with the
// live scope, so the two cannot disagree about what a lane looks like.

const AXIS_H = 28

export function BenchScope({ bench, focusNet, onFocus }: {
  bench: BenchReport
  focusNet: string | null
  onFocus: (net: string) => void
}) {
  const { onGo } = useContext(ResultLinkContext)
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const boxRef = useRef<HTMLDivElement>(null)

  const { lanes, rows, folded } = useMemo(() => benchLanes(bench, focusNet), [bench, focusNet])
  const decoded = useMemo(() => decodedItems(bench.bus_log), [bench])
  const markers = useMemo(() => eventMarkers(bench.events), [bench])
  const plot = useMemo<ScopePlot>(() => ({
    traces: [],
    xLabel: 'simulated time',
    yUnit: '',
    xTick: timeTickLabeler(0, bench.duration_s),
    xRange: [0, Math.max(bench.duration_s, 1e-6)],
    markers,
    lanes,
    decoded,
  }), [bench, lanes, decoded, markers])
  const topPad = plotTopPad(plot)
  const height = topPad + plotExtraHeight(plot) + AXIS_H
  const plotRef = useRef(plot)
  plotRef.current = plot

  useEffect(() => {
    const canvas = canvasRef.current
    const box = boxRef.current
    if (!canvas || !box) return
    const paint = () => {
      const dpr = window.devicePixelRatio || 1
      const w = Math.max(1, Math.round(box.clientWidth * dpr))
      const h = Math.max(1, Math.round(height * dpr))
      if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h }
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      drawScope(ctx, w / dpr, h / dpr, plotRef.current)
    }
    paint()
    const ro = new ResizeObserver(paint)
    ro.observe(box)
    const off = onThemeChange(paint)
    return () => { ro.disconnect(); off() }
  }, [height, plot])

  if (lanes.length === 0 && decoded.length === 0) {
    return (
      <div className="text-xs mb-3" data-testid="bench-scope-empty" style={{ color: 'var(--silk-dim)' }}>
        No pin changed level and no bus carried a transaction during the run, so there is no waveform to draw.
      </div>
    )
  }

  // A click on a lane's band focuses that pin (the same focus a chip sets).
  const pick = (ev: React.MouseEvent<HTMLCanvasElement>) => {
    const rect = ev.currentTarget.getBoundingClientRect()
    const i = Math.floor((ev.clientY - rect.top - topPad) / LANE_H)
    const net = rows[i]?.net
    if (net) onFocus(net)
  }
  const focusLane = lanes.find(l => l.focused)
  const focusRow = focusNet ? bench.mcus.flatMap(m => m.pins).find(p => p.net === focusNet) : undefined

  return (
    <div className="mb-3" data-testid="bench-scope">
      <div
        ref={boxRef}
        style={{ border: '1px solid var(--scope-edge)', borderRadius: 6, overflow: 'hidden', background: 'var(--scope-face)', boxShadow: 'var(--scope-glow)' }}
      >
        <canvas
          ref={canvasRef}
          role="img"
          aria-label={benchSummaryLine(bench, lanes.length)}
          data-testid="bench-scope-canvas"
          data-lanes={lanes.map(l => l.label).join(',')}
          data-focused={focusLane?.label ?? ''}
          data-decoded={decoded.length}
          data-events={markers.length}
          onClick={pick}
          style={{ display: 'block', width: '100%', height, cursor: 'pointer' }}
        />
      </div>
      <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 mt-1 text-[11px]" style={{ color: 'var(--silk-dim)' }}>
        {focusNet && (
          <span data-testid="bench-focus">
            Focused: <b style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{focusRow?.pin ?? displayNet(focusNet)}</b>
            {' on '}<span style={{ fontFamily: 'var(--font-mono)' }}>{displayNet(focusNet)}</span>
            {onGo && (
              <>
                {' · '}
                <button
                  type="button"
                  data-testid="bench-show-board"
                  onClick={() => onGo({ kind: 'net', net: focusNet })}
                  className="hb-press cursor-pointer"
                  style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)', textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)', fontSize: 11 }}
                >
                  Show it on the board
                </button>
              </>
            )}
          </span>
        )}
        {folded > 0 && <span data-testid="bench-folded">{folded} more active pin{folded === 1 ? '' : 's'} not drawn</span>}
        {bench.truncated && (
          <span data-testid="bench-recorder-cut" style={{ color: 'var(--warn-strong)' }}>
            The recorder hit its memory cap: counts are exact, the waveform and log are cut short.
          </span>
        )}
      </div>
    </div>
  )
}
