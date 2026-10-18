import { useEffect, useRef, useState } from 'react'
import type { SimFrame } from '../../types/protocol'
import { CloseIcon } from '../Icons'
import { displayNet } from '../../lib/net-name'
import { netReadoutText, readNet, type NetReading } from '../../lib/net-state'
import { drawScope, formatSimTime, timeTickLabeler, traceToken } from '../scope-draw'
import { formatClock } from './run-state'

// What a click on a net means in the LIVE view: the net's value right now, how
// it has moved, and a trace of it. The static "Check this" card (suggested
// assertions for a report) answers a different question, and on a live supply
// rail it offered nonsense ("+5V toggles at ... Hz"), so it is kept one
// disclosure away rather than being the first thing the click shows.

const CANVAS_H = 96
const TRACE_SLOT = 0

/** The retained frames' measurements of one net, oldest first, for the trace.
 *  Only measured readings are plotted: a trace is a claim that the level was
 *  observed, so an unobserved or unsupported net draws nothing. Stops where
 *  time goes backwards, because a restart makes the earlier frames a different
 *  run. */
export function netTrace(frames: SimFrame[], net: string): { x: number; y: number }[] {
  const out: { x: number; y: number }[] = []
  let next = Infinity
  for (let i = frames.length - 1; i >= 0; i--) {
    const f = frames[i]
    if (f.t > next) break
    const r = readNet(f, net)
    if (r.kind === 'measured') out.unshift({ x: f.t, y: r.volts })
    next = f.t
  }
  return out
}

/** One plain sentence about how the net is behaving. */
export function netBehaviour(reading: NetReading, span: number | null): string {
  if (reading.kind === 'unsupported') return 'No simulated value: it is held by a part this simulator cannot model yet.'
  if (reading.kind === 'unobserved') return 'This emulator cannot see how the firmware drives this pin, so there is no measured level.'
  if (reading.kind === 'absent') return 'Not in the simulation yet. Press Play.'
  const window = span !== null && span > 0 ? ` over the last ${formatSimTime(span)}` : ''
  return reading.moving
    ? `Moving: ${reading.min.toFixed(2)} V to ${reading.max.toFixed(2)} V${window}.`
    : `Steady${window}.`
}

export function LiveNetCard({
  net, reading, frame, frames, probed, onProbe, onUnprobe, onClose, checks,
}: {
  net: string
  reading: NetReading
  /** The frame on screen (the live one, or a retained one while reviewing). */
  frame: SimFrame | null
  /** The retained frames, oldest first. */
  frames: SimFrame[]
  /** Already on the scope. */
  probed: boolean
  onProbe: () => void
  onUnprobe: () => void
  onClose: () => void
  /** The static check card for this net, offered behind a disclosure. */
  checks?: React.ReactNode
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const boxRef = useRef<HTMLDivElement>(null)
  const [showChecks, setShowChecks] = useState(false)
  const points = netTrace(frames, net)
  const span = points.length > 1 ? points[points.length - 1].x - points[0].x : null

  // The trace repaints whenever the card re-renders (once per frame while the
  // sim runs); it is a 96 px canvas, so this is cheap.
  useEffect(() => {
    const canvas = canvasRef.current
    const box = boxRef.current
    if (!canvas || !box) return
    const dpr = window.devicePixelRatio || 1
    const w = Math.max(1, Math.round(box.clientWidth * dpr))
    const h = Math.round(CANVAS_H * dpr)
    if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h }
    const ctx = canvas.getContext('2d')
    if (!ctx) return
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    const lo = points.length ? points[0].x : 0
    const hi = points.length ? Math.max(points[points.length - 1].x, lo + 1e-3) : 1
    drawScope(ctx, w / dpr, h / dpr, {
      traces: [{ label: displayNet(net), colorToken: traceToken(TRACE_SLOT), points }],
      xLabel: '',
      yUnit: 'V',
      xTick: timeTickLabeler(lo, hi),
      xRange: points.length ? [lo, hi] : undefined,
      yFloor: [0, 3.3],
      hint: points.length === 0 ? 'No measured trace yet. Press Play.' : null,
    })
  })

  const measured = reading.kind === 'measured'
  // The level at this instant; whether the net is moving is the sentence below,
  // so a swinging net does not print a range where a value belongs.
  const value = reading.kind === 'measured' ? `${reading.volts.toFixed(3)} V` : netReadoutText(reading)

  return (
    <div
      data-testid="live-net-card"
      className="hb-card flex flex-col gap-2.5 px-3 pt-3 pb-3"
      style={{ width: '100%', boxShadow: 'var(--shadow-card)' }}
    >
      <div className="flex items-center justify-between">
        <span className="text-[10px] font-bold tracking-[0.14em]" style={{ color: 'var(--silk-faint)' }}>NET, LIVE</span>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close selection"
          className="hb-press cursor-pointer"
          style={{ color: 'var(--silk-faint)', display: 'inline-flex', background: 'none', border: 'none', padding: 6, margin: -6 }}
        >
          <CloseIcon size={13} />
        </button>
      </div>

      <div className="flex items-baseline justify-between gap-2 flex-wrap">
        <span className="text-[14px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>
          {displayNet(net)}
        </span>
        <span
          data-testid="live-net-value"
          className="tnum"
          title={frame ? `At t = ${formatClock(frame.t)}` : undefined}
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: measured ? 18 : 11,
            fontWeight: measured ? 700 : 400,
            fontStyle: measured ? undefined : 'italic',
            color: measured ? 'var(--copper-hi)' : 'var(--silk-faint)',
          }}
        >
          {value}
          {measured && frame && <span className="text-[10px] font-normal" style={{ color: 'var(--silk-faint)' }}> now</span>}
        </span>
      </div>

      <div className="text-[11px] leading-snug" data-testid="live-net-behaviour" style={{ color: 'var(--silk-dim)' }}>
        {netBehaviour(reading, span)}
      </div>

      <div
        ref={boxRef}
        data-testid="live-net-trace"
        style={{ border: '1px solid var(--scope-edge)', borderRadius: 6, overflow: 'hidden', background: 'var(--scope-face)' }}
      >
        <canvas ref={canvasRef} aria-label={`Recent trace of ${displayNet(net)}`} style={{ display: 'block', width: '100%', height: CANVAS_H }} />
      </div>

      <div className="flex items-center gap-2 flex-wrap">
        {probed ? (
          <button type="button" data-testid="live-net-unprobe" onClick={onUnprobe} className="hb-btn hb-press text-[11px] px-2.5 cursor-pointer" style={{ height: 28 }}>
            Remove from the scope
          </button>
        ) : (
          <button type="button" data-testid="live-net-probe" onClick={onProbe} className="hb-btn hb-press text-[11px] px-2.5 cursor-pointer" style={{ height: 28 }}>
            Probe this net on the scope
          </button>
        )}
        {probed && <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>Plotted on the scope below the board.</span>}
      </div>

      {checks && (
        <div className="flex flex-col gap-2" style={{ borderTop: '1px solid var(--hairline)', paddingTop: 8 }}>
          <button
            type="button"
            data-testid="live-net-checks-toggle"
            aria-expanded={showChecks}
            onClick={() => setShowChecks(s => !s)}
            className="hb-press flex items-center gap-1.5 text-[11px] cursor-pointer self-start"
            style={{ background: 'none', border: 'none', padding: 0, color: 'var(--silk-dim)' }}
          >
            <span aria-hidden="true" style={{ display: 'inline-block', width: 10 }}>{showChecks ? '▾' : '▸'}</span>
            Add a check on this net
          </button>
          {showChecks && checks}
        </div>
      )}
    </div>
  )
}
