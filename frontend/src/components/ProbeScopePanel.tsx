import { useEffect, useRef } from 'react'
import type { BoardInfoMsg, SimFrame, ClientMessage } from '../types/protocol'

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

      if (probes.length === 0) {
        ctx.fillStyle = 'rgba(0,200,80,0.25)'
        ctx.font = '11px monospace'
        ctx.textAlign = 'center'
        ctx.fillText('Select nets below to probe', W / 2, H / 2)
        return
      }

      const PAD_L = 38
      const PAD_R = 60
      const PAD_T = 8
      const PAD_B = 20
      const plotW = W - PAD_L - PAD_R
      const plotH = H - PAD_T - PAD_B

      // Determine time range
      let tMax = 0
      for (const buf of buffers.current.values()) {
        if (buf.length > 0) tMax = Math.max(tMax, buf[buf.length - 1].t)
      }
      const tMin = tMax - WINDOW_SECS

      // Determine voltage range (autoscale with nice grid)
      let vMin = 0, vMax = 5
      for (const buf of buffers.current.values()) {
        for (const s of buf) {
          if (s.t >= tMin) {
            vMin = Math.min(vMin, s.v)
            vMax = Math.max(vMax, s.v)
          }
        }
      }
      // Add 10% padding
      const vRange = vMax - vMin || 1
      vMin -= vRange * 0.05
      vMax += vRange * 0.05

      // Nice grid lines at round voltages
      const gridStep = vRange > 4 ? 1 : vRange > 2 ? 0.5 : 0.2
      const gridStart = Math.ceil(vMin / gridStep) * gridStep

      // Grid — phosphor green grid
      ctx.strokeStyle = 'rgba(0,200,80,0.15)'
      ctx.lineWidth = 0.5
      ctx.setLineDash([2, 4])
      for (let gv = gridStart; gv <= vMax; gv += gridStep) {
        const gy = PAD_T + plotH - ((gv - vMin) / (vMax - vMin)) * plotH
        ctx.beginPath()
        ctx.moveTo(PAD_L, gy)
        ctx.lineTo(PAD_L + plotW, gy)
        ctx.stroke()

        // Voltage label — dim phosphor green
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

      // Traces — phosphor glow with multi-pass bloom
      probes.forEach((net, idx) => {
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
        ctx.fillText(net, PAD_L + plotW + 4, Math.max(PAD_T + 10, Math.min(H - PAD_B - 2, labelY + 3)))
        ctx.shadowBlur = 0
      })
    }

    rafRef.current = requestAnimationFrame(draw)
    return () => cancelAnimationFrame(rafRef.current)
  }, [probes])

  return (
    <div className="flex flex-col gap-2">
      {/* Oscilloscope canvas — CRT-style container */}
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
          width={260}
          height={160}
          style={{ display: 'block', width: '100%', height: 160 }}
        />
      </div>

      {/* Net picker */}
      <div>
        <div className="text-[10px] font-bold tracking-wider mb-1.5 px-1" style={{ color: '#475569' }}>NETS</div>
        <div className="flex flex-col gap-0.5 max-h-48 overflow-y-auto">
          {nets.map((net) => {
            const active = probes.includes(net)
            const color = active ? PROBE_COLORS[probes.indexOf(net) % PROBE_COLORS.length] : undefined
            return (
              <button
                key={net}
                onClick={() => active ? onRemoveProbe(net) : onAddProbe(net)}
                className="flex items-center gap-2 px-2 py-1 rounded text-left transition-all hover:opacity-80"
                style={{
                  background: active ? 'rgba(59,130,246,0.08)' : 'transparent',
                  border: active ? `1px solid ${color}30` : '1px solid transparent',
                }}
              >
                <div
                  className="w-2 h-2 rounded-full shrink-0"
                  style={{
                    background: active ? color : '#1e293b',
                    boxShadow: active ? `0 0 4px ${color}80` : 'none',
                  }}
                />
                <span
                  className="text-[11px] font-mono flex-1"
                  style={{
                    color: active ? '#e2e8f0' : '#64748b',
                    fontFamily: "'JetBrains Mono', monospace",
                  }}
                >
                  {net}
                </span>
                <span className="text-[9px]" style={{ color: active ? color : '#334155' }}>
                  {active ? '✕' : '+'}
                </span>
              </button>
            )
          })}
          {nets.length === 0 && (
            <div className="text-[11px] px-2 py-1" style={{ color: '#334155' }}>No nets loaded</div>
          )}
        </div>
      </div>
    </div>
  )
}
