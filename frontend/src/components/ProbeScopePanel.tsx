import { useEffect, useImperativeHandle, useRef, useState } from 'react'
import type { Ref } from 'react'
import type { BoardInfoMsg, SimFault, SimFrame } from '../types/protocol'
import { CloseIcon, PlusIcon, EyeIcon, EyeOffIcon } from './Icons'
import { displayNet } from '../lib/net-name'
import { netReadoutText, readNet } from '../lib/net-state'
import { drawScope, traceToken } from './scope-draw'
import type { ScopePlot } from './scope-draw'
import { cssToken } from '../lib/theme-tokens'
import { useDismissable } from '../hooks/useDismissable'

// The live scope, under the board as part of the one working surface: a trace
// per probed net, a dashed rule at the sim time of every fault, the probes the
// session started with (and why), and a picker for more. The painting lives in
// scope-draw.ts, shared with the deck panel. All colours are --scope-* /
// --trace-* tokens; a probe's dot and its beam read the same --trace-N.

/** The live window: the newest few seconds, or the whole run if shorter. */
const WINDOW_SECS = 3.0
/** Retained per probe for export; the CLI's waveform writer caps at the same. */
const MAX_SAMPLES = 20_000

interface Sample {
  t: number
  v: number
}

/** What the export menu asks the scope for. */
export interface ScopeExport {
  series: () => { net: string; points: Sample[] }[]
  /** The whole retained run drawn on a fresh canvas under `header` lines. */
  png: (header: string[]) => HTMLCanvasElement
}

const timeTick = (span: number) => (t: number) =>
  span < 0.01 ? `${(t * 1e6).toFixed(0)}µs` : span < 2 ? `${(t * 1000).toFixed(span < 0.1 ? 1 : 0)}ms` : `${t.toFixed(1)}s`

interface ProbeScopePanelProps {
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  probes: string[]
  /** Why a probe was attached by the session seed ("a check names it"). */
  probeReasons?: Record<string, string>
  /** Logged faults: each is a rule on the trace at its sim time. */
  faults?: SimFault[]
  onAddProbe: (net: string) => void
  onRemoveProbe: (net: string) => void
  exportRef?: Ref<ScopeExport>
  height?: number
}

export function ProbeScopePanel({
  boardInfo, frame, probes, probeReasons = {}, faults = [], onAddProbe, onRemoveProbe, exportRef, height = 170,
}: ProbeScopePanelProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const boxRef = useRef<HTMLDivElement>(null)
  const buffers = useRef<Map<string, Sample[]>>(new Map())
  const rafRef = useRef<number>(0)
  // Hidden traces: still probed and buffered, just not drawn.
  const [hidden, setHidden] = useState<Set<string>>(new Set())
  const hiddenRef = useRef(hidden)
  hiddenRef.current = hidden
  const faultsRef = useRef(faults)
  faultsRef.current = faults

  // Accumulate samples
  useEffect(() => {
    if (!frame) return
    for (const net of probes) {
      // A trace is a claim that this level was measured, so a net the backend
      // did not simulate or cannot observe is not buffered at all.
      const r = readNet(frame, net)
      if (r.kind !== 'measured') continue
      if (!buffers.current.has(net)) buffers.current.set(net, [])
      const buf = buffers.current.get(net)!
      // A restart begins time again at 0: the old run is a different story.
      if (buf.length > 0 && frame.t < buf[buf.length - 1].t) buf.length = 0
      if (buf.length > 0 && buf[buf.length - 1].t === frame.t) continue
      buf.push({ t: frame.t, v: r.volts })
      if (buf.length > MAX_SAMPLES) buf.splice(0, buf.length - MAX_SAMPLES)
    }
    for (const k of buffers.current.keys()) {
      if (!probes.includes(k)) buffers.current.delete(k)
    }
  }, [frame, probes])

  const plotFor = (visible: string[], live: boolean): ScopePlot => {
    let tMax = -Infinity
    let tFirst = Infinity
    for (const net of visible) {
      const buf = buffers.current.get(net)
      if (buf && buf.length > 0) {
        tMax = Math.max(tMax, buf[buf.length - 1].t)
        tFirst = Math.min(tFirst, buf[0].t)
      }
    }
    const has = Number.isFinite(tMax)
    const lo = live ? Math.max(tFirst, tMax - WINDOW_SECS) : tFirst
    const span = has ? Math.max(tMax - lo, 1e-6) : WINDOW_SECS
    return {
      traces: visible.map(net => ({
        label: displayNet(net),
        colorToken: traceToken(probes.indexOf(net)),
        points: (buffers.current.get(net) ?? []).map(s => ({ x: s.t, y: s.v })),
      })),
      xLabel: live && has && tMax - tFirst > WINDOW_SECS ? `newest ${WINDOW_SECS} s of sim time` : 'sim time',
      yUnit: 'V',
      xTick: timeTick(span),
      xRange: has ? [lo, lo + span] : undefined,
      yFloor: [0, 3.3],
      markers: faultsRef.current.map(f => ({ x: f.t, label: f.component })),
      hint: probes.length === 0
        ? 'Probe a net to plot its voltage'
        : visible.length === 0 ? 'All traces hidden'
          : !has ? 'Waiting for the first sample: press play or run the checks duration' : null,
    }
  }
  const plotRef = useRef(plotFor)
  plotRef.current = plotFor

  // Render loop; the canvas tracks its box at device resolution.
  useEffect(() => {
    function draw() {
      rafRef.current = requestAnimationFrame(draw)
      const canvas = canvasRef.current
      const box = boxRef.current
      if (!canvas || !box) return
      const dpr = window.devicePixelRatio || 1
      const w = Math.max(1, Math.round(box.clientWidth * dpr))
      const h = Math.max(1, Math.round(height * dpr))
      if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h }
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      const visible = probes.filter(p => !hiddenRef.current.has(p))
      drawScope(ctx, w / dpr, h / dpr, plotRef.current(visible, true))
    }
    rafRef.current = requestAnimationFrame(draw)
    return () => cancelAnimationFrame(rafRef.current)
  }, [probes, height])

  useImperativeHandle(exportRef, () => ({
    series: () => probes.map(net => ({ net, points: [...(buffers.current.get(net) ?? [])] })),
    png: (header: string[]) => {
      const W = 1400
      const lineH = 16
      const headH = header.length * lineH + 16
      const plotH = 520
      const c = document.createElement('canvas')
      c.width = W
      c.height = headH + plotH
      const ctx = c.getContext('2d')!
      ctx.fillStyle = cssToken('--surface')
      ctx.fillRect(0, 0, W, headH)
      ctx.fillStyle = cssToken('--silk')
      ctx.font = '12px JetBrains Mono, monospace'
      ctx.textAlign = 'left'
      header.forEach((l, i) => ctx.fillText(l, 12, 18 + i * lineH))
      ctx.translate(0, headH)
      drawScope(ctx, W, plotH, plotRef.current(probes, false))
      return c
    },
  }), [probes])

  const toggleHidden = (net: string) => {
    setHidden(prev => {
      const next = new Set(prev)
      if (next.has(net)) next.delete(net)
      else next.add(net)
      return next
    })
  }

  return (
    <div className="flex flex-col sm:flex-row gap-2 p-2" data-testid="sim-scope">
      <div
        ref={boxRef}
        className="relative flex-1 min-w-0"
        style={{
          border: '1px solid var(--scope-edge)',
          borderRadius: 6,
          overflow: 'hidden',
          background: 'var(--scope-face)',
          boxShadow: 'var(--scope-glow)',
        }}
      >
        <canvas ref={canvasRef} aria-label="Scope traces of the probed nets" style={{ display: 'block', width: '100%', height }} />
      </div>

      <div className="flex flex-col gap-1 sm:w-[230px] shrink-0 min-w-0">
        <div className="flex flex-col gap-0.5 overflow-y-auto" style={{ maxHeight: height - 32 }}>
          {probes.map((net, idx) => {
            const color = `var(${traceToken(idx)})`
            const isHidden = hidden.has(net)
            const live = frame ? readNet(frame, net) : null
            const why = probeReasons[net]
            return (
              <div
                key={net}
                data-testid="probe-row"
                className="flex items-center gap-1.5 px-2 py-1 rounded-md"
                title={why ? `Attached because ${why}` : undefined}
                style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', opacity: isHidden ? 0.6 : 1 }}
              >
                <div className="w-2 h-2 rounded-full shrink-0" style={{ background: color }} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-baseline gap-1.5 min-w-0">
                    <span className="text-[11px] truncate" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
                      {displayNet(net)}
                    </span>
                    {live && live.kind !== 'absent' && !isHidden && (
                      <span className="text-[10px] tnum truncate ml-auto" data-testid="probe-readout" title={netReadoutText(live)}
                        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)', fontStyle: live.kind === 'measured' ? undefined : 'italic' }}>
                        {netReadoutText(live)}
                      </span>
                    )}
                  </div>
                  {why && (
                    <div className="text-[9px] truncate" data-testid="probe-why" style={{ color: 'var(--silk-faint)' }}>{why}</div>
                  )}
                </div>
                <button
                  type="button"
                  onClick={() => toggleHidden(net)}
                  className="hb-press cursor-pointer shrink-0"
                  aria-label={isHidden ? `Show the ${displayNet(net)} trace` : `Hide the ${displayNet(net)} trace`}
                  title={isHidden ? 'Show this trace' : 'Hide this trace (keeps recording)'}
                  style={{ background: 'none', border: 'none', color: isHidden ? 'var(--silk-faint)' : 'var(--silk-dim)', display: 'inline-flex', padding: 3 }}
                >
                  {isHidden ? <EyeOffIcon size={12} /> : <EyeIcon size={12} />}
                </button>
                <button
                  type="button"
                  onClick={() => onRemoveProbe(net)}
                  className="hb-press cursor-pointer shrink-0"
                  aria-label={`Detach the probe from ${displayNet(net)}`}
                  title="Detach this probe"
                  style={{ background: 'none', border: 'none', color: 'var(--silk-faint)', display: 'inline-flex', padding: 3 }}
                >
                  <CloseIcon size={11} />
                </button>
              </div>
            )
          })}
        </div>
        <NetPicker
          testId="probe-add"
          label="Probe a net"
          nets={(boardInfo?.nets ?? []).filter(n => !probes.includes(n))}
          onPick={onAddProbe}
        />
      </div>
    </div>
  )
}

/** A button that opens a filterable net list; the one net picker the live
 *  view uses (probes and inputs). */
export function NetPicker({ label, nets, onPick, testId, align = 'right', drop = 'up' }: {
  label: string
  nets: string[]
  onPick: (net: string) => void
  testId: string
  align?: 'left' | 'right'
  drop?: 'up' | 'down'
}) {
  const [open, setOpen] = useState(false)
  const [filter, setFilter] = useState('')
  const wrap = useRef<HTMLDivElement>(null)
  useDismissable(open, wrap, setOpen)
  const shown = filter ? nets.filter(n => displayNet(n).toLowerCase().includes(filter.toLowerCase())) : nets
  return (
    <div ref={wrap} className="relative">
      <button
        type="button"
        data-testid={testId}
        onClick={() => setOpen(o => !o)}
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={nets.length === 0}
        className="hb-btn hb-press inline-flex items-center gap-1.5 px-2 text-[11px] w-full justify-center"
        style={{ height: 26, opacity: nets.length === 0 ? 0.5 : 1 }}
      >
        <PlusIcon size={10} /> {label}
      </button>
      {open && (
        <div
          className="hb-card absolute z-30 p-1.5 flex flex-col gap-1"
          style={{ [drop === 'up' ? 'bottom' : 'top']: 'calc(100% + 4px)', [align]: 0, width: 240, maxWidth: '80vw', boxShadow: 'var(--shadow-pop)' }}
        >
          <input
            autoFocus
            className="hb-input w-full"
            placeholder="filter nets"
            aria-label="Filter nets"
            value={filter}
            onChange={e => setFilter(e.target.value)}
          />
          <div role="listbox" className="flex flex-col max-h-56 overflow-y-auto">
            {shown.map(net => (
              <button
                key={net}
                type="button"
                role="option"
                aria-selected={false}
                data-testid={`${testId}-option`}
                onClick={() => { onPick(net); setOpen(false); setFilter('') }}
                className="hb-press text-left px-2 py-1 rounded-md text-[11px] cursor-pointer border border-transparent hover:bg-[var(--copper-tint)]"
                style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)', background: 'none' }}
              >
                {displayNet(net)}
              </button>
            ))}
            {shown.length === 0 && (
              <div className="text-[11px] px-2 py-1" style={{ color: 'var(--silk-faint)' }}>No nets match</div>
            )}
          </div>
        </div>
      )}
    </div>
  )
}
