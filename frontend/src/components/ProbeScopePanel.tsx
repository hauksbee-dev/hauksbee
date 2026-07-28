import { useEffect, useRef, useState } from 'react'
import type { BoardInfoMsg, SimFrame, ClientMessage } from '../types/protocol'
import { CloseIcon, PlusIcon, EyeIcon, EyeOffIcon } from './Icons'
import { displayNet } from '../lib/net-name'

// The scope card: a phosphor-style rolling trace per probed net, with a
// per-trace visibility toggle (the probe keeps buffering while hidden, so
// showing it again has history) and the net picker to attach more probes.
// The scope face is an instrument surface: dark in both themes.

const PROBE_COLORS = ['#3b82f6', '#f59e0b', '#10b981', '#ef4444', '#8b5cf6', '#ec4899']
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
  send: (msg: ClientMessage) => void
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
      const v = frame.net_voltages[net]
      if (v === undefined) continue
      if (!buffers.current.has(net)) buffers.current.set(net, [])
      const buf = buffers.current.get(net)!
      buf.push({ t: frame.t, v })
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
      const W = canvas.width
      const H = canvas.height

      ctx.clearRect(0, 0, W, H)
      // CRT-like phosphor background: very dark blue-green
      ctx.fillStyle = '#00080f'
      ctx.fillRect(0, 0, W, H)
      // Subtle scanline effect (every 3px a slightly lighter strip)
      ctx.fillStyle = 'rgba(0,255,100,0.012)'
      for (let sy = 0; sy < H; sy += 3) {
        ctx.fillRect(0, sy, W, 1)
      }

      const visible = probes.filter(p => !hiddenRef.current.has(p))
      if (probes.length === 0) {
        ctx.fillStyle = 'rgba(0,200,80,0.25)'
        ctx.font = '11px monospace'
        ctx.textAlign = 'center'
        ctx.fillText('Attach a probe from the net list below', W / 2, H / 2)
        return
      }
      if (visible.length === 0) {
        ctx.fillStyle = 'rgba(0,200,80,0.25)'
        ctx.font = '11px monospace'
        ctx.textAlign = 'center'
        ctx.fillText('All traces hidden', W / 2, H / 2)
        return
      }

      const PAD_L = 38
      const PAD_R = 60
      const PAD_T = 8
      const PAD_B = 20
      const plotW = W - PAD_L - PAD_R
      const plotH = H - PAD_T - PAD_B

      // Determine time range (visible traces only)
      let tMax = 0
      for (const net of visible) {
        const buf = buffers.current.get(net)
        if (buf && buf.length > 0) tMax = Math.max(tMax, buf[buf.length - 1].t)
      }
      const tMin = tMax - WINDOW_SECS

      // Determine voltage range (autoscale with nice grid)
      let vMin = 0, vMax = 5
      for (const net of visible) {
        for (const s of buffers.current.get(net) ?? []) {
          if (s.t >= tMin) {
            vMin = Math.min(vMin, s.v)
            vMax = Math.max(vMax, s.v)
          }
        }
      }
      // Add padding
      const vRange = vMax - vMin || 1
      vMin -= vRange * 0.05
      vMax += vRange * 0.05

      // Nice grid lines at round voltages
      const gridStep = vRange > 4 ? 1 : vRange > 2 ? 0.5 : 0.2
      const gridStart = Math.ceil(vMin / gridStep) * gridStep

      // Grid, phosphor green grid
      ctx.strokeStyle = 'rgba(0,200,80,0.15)'
      ctx.lineWidth = 0.5
      ctx.setLineDash([2, 4])
      for (let gv = gridStart; gv <= vMax; gv += gridStep) {
        const gy = PAD_T + plotH - ((gv - vMin) / (vMax - vMin)) * plotH
        ctx.beginPath()
        ctx.moveTo(PAD_L, gy)
        ctx.lineTo(PAD_L + plotW, gy)
        ctx.stroke()

        // Voltage label, dim phosphor green
        ctx.fillStyle = 'rgba(0,200,80,0.45)'
        ctx.font = '9px JetBrains Mono, monospace'
        ctx.textAlign = 'right'
        ctx.fillText(`${gv.toFixed(gv % 1 !== 0 ? 1 : 0)}V`, PAD_L - 4, gy + 3)
      }
      ctx.setLineDash([])

      // Time axis label
      ctx.fillStyle = 'rgba(0,200,80,0.35)'
      ctx.font = '9px JetBrains Mono, monospace'
      ctx.textAlign = 'center'
      ctx.fillText('← 3s window →', PAD_L + plotW / 2, H - 4)

      // Traces, phosphor glow with multi-pass bloom
      probes.forEach((net, idx) => {
        if (hiddenRef.current.has(net)) return
        const buf = buffers.current.get(net) ?? []
        const color = PROBE_COLORS[idx % PROBE_COLORS.length]

        if (buf.length < 2) return

        // Build path points once
        const pts: { x: number; y: number }[] = []
        for (const s of buf) {
          if (s.t < tMin) continue
          pts.push({
            x: PAD_L + ((s.t - tMin) / WINDOW_SECS) * plotW,
            y: PAD_T + plotH - ((s.v - vMin) / (vMax - vMin)) * plotH,
          })
        }
        if (pts.length < 2) return

        const drawPath = () => {
          ctx.beginPath()
          ctx.moveTo(pts[0].x, pts[0].y)
          for (let i = 1; i < pts.length; i++) ctx.lineTo(pts[i].x, pts[i].y)
          ctx.stroke()
        }

        // Pass 1: wide soft glow bloom
        ctx.strokeStyle = color + '33'
        ctx.lineWidth = 6
        ctx.shadowColor = color
        ctx.shadowBlur = 12
        drawPath()

        // Pass 2: medium halo
        ctx.strokeStyle = color + '66'
        ctx.lineWidth = 3
        ctx.shadowColor = color
        ctx.shadowBlur = 8
        drawPath()

        // Pass 3: crisp bright core (phosphor "beam")
        ctx.strokeStyle = color
        ctx.lineWidth = 1.2
        ctx.shadowColor = color
        ctx.shadowBlur = 4
        drawPath()
        ctx.shadowBlur = 0

        // Legend label on right
        const lastSample = buf[buf.length - 1]
        const labelY = lastSample
          ? PAD_T + plotH - ((lastSample.v - vMin) / (vMax - vMin)) * plotH
          : PAD_T + (idx + 1) * 14
        ctx.fillStyle = color
        ctx.font = 'bold 9px JetBrains Mono, monospace'
        ctx.textAlign = 'left'
        ctx.shadowColor = color
        ctx.shadowBlur = 4
        ctx.fillText(displayNet(net), PAD_L + plotW + 4, Math.max(PAD_T + 10, Math.min(H - PAD_B - 2, labelY + 3)))
        ctx.shadowBlur = 0
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
      {/* Oscilloscope face, an instrument surface (dark in both themes) */}
      <div
        className="relative"
        style={{
          border: '1px solid rgba(0,180,70,0.3)',
          borderRadius: 6,
          overflow: 'hidden',
          background: '#00080f',
          boxShadow: '0 0 12px rgba(0,180,70,0.12), inset 0 0 20px rgba(0,0,0,0.6)',
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
            const color = PROBE_COLORS[idx % PROBE_COLORS.length]
            const isHidden = hidden.has(net)
            const live = frame?.net_voltages[net]
            return (
              <div
                key={net}
                className="flex items-center gap-2 px-2 py-1 rounded-md"
                style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', opacity: isHidden ? 0.6 : 1 }}
              >
                <div
                  className="w-2 h-2 rounded-full shrink-0"
                  style={{ background: color, boxShadow: isHidden ? 'none' : `0 0 4px ${color}80` }}
                />
                <span
                  className="text-[11px] flex-1 truncate"
                  style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
                >
                  {displayNet(net)}
                </span>
                {live !== undefined && !isHidden && (
                  <span className="text-[10px] tnum shrink-0" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                    {live.toFixed(3)}V
                  </span>
                )}
                <button
                  type="button"
                  onClick={() => toggleHidden(net)}
                  className="hb-press cursor-pointer"
                  aria-label={isHidden ? `Show the ${net} trace` : `Hide the ${net} trace`}
                  title={isHidden ? 'Show this trace' : 'Hide this trace (keeps recording)'}
                  style={{ background: 'none', border: 'none', color: isHidden ? 'var(--silk-faint)' : 'var(--silk-dim)', display: 'inline-flex', padding: 4 }}
                >
                  {isHidden ? <EyeOffIcon size={12} /> : <EyeIcon size={12} />}
                </button>
                <button
                  type="button"
                  onClick={() => onRemoveProbe(net)}
                  className="hb-press cursor-pointer"
                  aria-label={`Detach the probe from ${net}`}
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
              className="hb-press flex items-center gap-2 px-2 py-1 rounded-md text-left cursor-pointer"
              style={{ background: 'transparent', border: '1px solid transparent' }}
              onMouseEnter={e => { (e.currentTarget as HTMLElement).style.background = 'var(--copper-tint)' }}
              onMouseLeave={e => { (e.currentTarget as HTMLElement).style.background = 'transparent' }}
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
          {nets.length === 0 && (
            <div className="text-[11px] px-2 py-1" style={{ color: 'var(--silk-faint)' }}>No nets loaded</div>
          )}
          {nets.length > 0 && filtered.length === 0 && (
            <div className="text-[11px] px-2 py-1" style={{ color: 'var(--silk-faint)' }}>No nets match</div>
          )}
        </div>
      </div>
    </div>
  )
}
