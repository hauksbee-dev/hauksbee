import { useEffect, useRef, useState } from 'react'
import type { BoardInfoMsg, SimFrame } from '../types/protocol'
import { CloseIcon, PlusIcon, EyeIcon, EyeOffIcon } from './Icons'
import { displayNet } from '../lib/net-name'
import { netReadoutText, readNet } from '../lib/net-state'
import { drawScope, traceToken } from './scope-draw'

// The scope card: a rolling trace per probed net, with a per-trace visibility
// toggle (the probe keeps buffering while hidden, so showing it again has
// history) and the net picker to attach more probes. The face is phosphor CRT
// in dark and a light graticule in light mode; all colours come from the
// --scope-* / --trace-* tokens in index.css. The trace beams and the probe
// dots in the list below share the SAME --trace-N token, which is how a
// reader pairs trace to net; retune the token, never one end of the pair.
//
// The painting itself lives in scope-draw.ts, shared with the deck panel. The
// 3-second window below is this caller's alone: it is what a LIVE scope shows,
// and a finished sweep has its own axis.

const WINDOW_SECS = 3.0
const MAX_SAMPLES = 600

interface Sample {
  t: number
  v: number
}

interface ProbeScopePanelProps {
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  probes: string[]
  onAddProbe: (net: string) => void
  onRemoveProbe: (net: string) => void
}

export function ProbeScopePanel({ boardInfo, frame, probes, onAddProbe, onRemoveProbe }: ProbeScopePanelProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const buffers = useRef<Map<string, Sample[]>>(new Map())
  const rafRef = useRef<number>(0)
  // Hidden traces: still probed and buffered, just not drawn.
  const [hidden, setHidden] = useState<Set<string>>(new Set())
  const hiddenRef = useRef(hidden)
  hiddenRef.current = hidden
  const [filter, setFilter] = useState('')

  const nets = boardInfo?.nets ?? []

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
      buf.push({ t: frame.t, v: r.volts })
      if (buf.length > MAX_SAMPLES) buf.splice(0, buf.length - MAX_SAMPLES)
    }
    // Remove stale buffers
    for (const k of buffers.current.keys()) {
      if (!probes.includes(k)) buffers.current.delete(k)
    }
  }, [frame, probes])

  // Render loop
  useEffect(() => {
    function draw() {
      rafRef.current = requestAnimationFrame(draw)
      const canvas = canvasRef.current
      if (!canvas) return
      const ctx = canvas.getContext('2d')
      if (!ctx) return

      // The window ends at the newest sample any VISIBLE trace has.
      const visible = probes.filter(p => !hiddenRef.current.has(p))
      let tMax = 0
      for (const net of visible) {
        const buf = buffers.current.get(net)
        if (buf && buf.length > 0) tMax = Math.max(tMax, buf[buf.length - 1].t)
      }
      drawScope(ctx, canvas.width, canvas.height, {
        traces: visible.map(net => ({
          label: displayNet(net),
          colorToken: traceToken(probes.indexOf(net)),
          points: (buffers.current.get(net) ?? []).map(s => ({ x: s.t, y: s.v })),
        })),
        xLabel: `← ${WINDOW_SECS}s window →`,
        yUnit: 'V',
        xTick: (t: number) => `${t.toFixed(1)}s`,
        xRange: [tMax - WINDOW_SECS, tMax],
        yFloor: [0, 5],
        hint: probes.length === 0
          ? 'Pick a net below to plot its voltage'
          : visible.length === 0 ? 'All traces hidden' : null,
      })
    }

    rafRef.current = requestAnimationFrame(draw)
    return () => cancelAnimationFrame(rafRef.current)
  }, [probes])

  const toggleHidden = (net: string) => {
    setHidden(prev => {
      const next = new Set(prev)
      if (next.has(net)) next.delete(net)
      else next.add(net)
      return next
    })
  }

  const unprobed = nets.filter(n => !probes.includes(n))
  const filtered = filter
    ? unprobed.filter(n => n.toLowerCase().includes(filter.toLowerCase()))
    : unprobed

  return (
    <div className="flex flex-col gap-2 px-2.5 py-2.5">
      {/* Oscilloscope face: theme-tuned via the --scope-* tokens */}
      <div
        className="relative"
        style={{
          border: '1px solid var(--scope-edge)',
          borderRadius: 6,
          overflow: 'hidden',
          background: 'var(--scope-face)',
          boxShadow: 'var(--scope-glow)',
        }}
      >
        <canvas
          ref={canvasRef}
          width={272}
          height={160}
          style={{ display: 'block', width: '100%', height: 160 }}
        />
      </div>

      {/* Attached traces: colour, visibility eye, detach */}
      {probes.length > 0 && (
        <div className="flex flex-col gap-0.5">
          {probes.map((net, idx) => {
            // The dot and the beam read the same token; see traceToken above.
            const color = `var(${traceToken(idx)})`
            const isHidden = hidden.has(net)
            const live = frame ? readNet(frame, net) : null
            return (
              <div
                key={net}
                className="flex items-center gap-2 px-2 py-1 rounded-md"
                style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', opacity: isHidden ? 0.6 : 1 }}
              >
                <div
                  className="w-2 h-2 rounded-full shrink-0"
                  style={{ background: color, boxShadow: isHidden ? 'none' : `0 0 4px color-mix(in srgb, ${color} 50.2%, transparent)` }}
                />
                <span
                  className="text-[11px] shrink-0 truncate max-w-[50%]"
                  style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
                >
                  {displayNet(net)}
                </span>
                {live && live.kind !== 'absent' && !isHidden && (
                  // A status reading is far longer than a voltage: it gives up its width first,
                  // so the net name and the detach button never leave the row.
                  <span className="text-[10px] tnum flex-1 min-w-0 truncate text-right" data-testid="probe-readout" title={netReadoutText(live)}
                    style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)', fontStyle: live.kind === 'measured' ? undefined : 'italic' }}>
                    {netReadoutText(live)}
                  </span>
                )}
                <button
                  type="button"
                  onClick={() => toggleHidden(net)}
                  className="hb-press cursor-pointer ml-auto shrink-0"
                  aria-label={isHidden ? `Show the ${displayNet(net)} trace` : `Hide the ${displayNet(net)} trace`}
                  title={isHidden ? 'Show this trace' : 'Hide this trace (keeps recording)'}
                  style={{ background: 'none', border: 'none', color: isHidden ? 'var(--silk-faint)' : 'var(--silk-dim)', display: 'inline-flex', padding: 4 }}
                >
                  {isHidden ? <EyeOffIcon size={12} /> : <EyeIcon size={12} />}
                </button>
                <button
                  type="button"
                  onClick={() => onRemoveProbe(net)}
                  className="hb-press cursor-pointer shrink-0"
                  aria-label={`Detach the probe from ${displayNet(net)}`}
                  title="Detach this probe"
                  style={{ background: 'none', border: 'none', color: 'var(--silk-faint)', display: 'inline-flex', padding: 4 }}
                >
                  <CloseIcon size={11} />
                </button>
              </div>
            )
          })}
        </div>
      )}

      {/* Net picker */}
      <div>
        <div className="text-[10px] font-bold tracking-wider mb-1.5" style={{ color: 'var(--silk-faint)' }}>
          ATTACH A PROBE
        </div>
        {nets.length > 8 && (
          <input
            className="hb-input w-full mb-1.5"
            placeholder="filter nets"
            value={filter}
            onChange={e => setFilter(e.target.value)}
          />
        )}
        <div className="flex flex-col gap-0.5 max-h-40 overflow-y-auto">
          {filtered.map(net => (
            <button
              key={net}
              onClick={() => onAddProbe(net)}
              className="hb-press flex items-center gap-2 px-2 py-1 rounded-md text-left cursor-pointer border border-transparent hover:bg-[var(--copper-tint)]"
            >
              <span
                className="text-[11px] flex-1 truncate"
                style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}
              >
                {displayNet(net)}
              </span>
              <span style={{ color: 'var(--silk-faint)', display: 'inline-flex' }}>
                <PlusIcon size={10} />
              </span>
            </button>
          ))}
          {filtered.length === 0 && (
            <div className="text-[11px] px-2 py-1" style={{ color: 'var(--silk-faint)' }}>
              {nets.length === 0 ? 'No nets loaded' : 'No nets match'}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
