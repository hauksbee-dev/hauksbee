import { useState, useEffect, useCallback } from 'react'
import type { SolverControls, ClientMessage } from '../types/protocol'

const DEFAULT_CONTROLS: SolverControls = {
  temperature_c: 27,
  parasitics: false,
  junction_caps: true,
  tolerances: false,
  integration: 'trap',
  fixed_dt: 0,
  granularity: 1.0,
}

interface SolverControlsPanelProps {
  controls: SolverControls | null
  send: (msg: ClientMessage) => void
}

function Toggle({ label, value, onChange, description }: {
  label: string
  value: boolean
  onChange: (v: boolean) => void
  description?: string
}) {
  return (
    <div className="flex items-center justify-between py-1">
      <div>
        <div className="text-[11px]" style={{ color: '#94a3b8' }}>{label}</div>
        {description && <div className="text-[9px]" style={{ color: '#334155' }}>{description}</div>}
      </div>
      <button
        onClick={() => onChange(!value)}
        className="relative w-9 h-5 rounded-full transition-all"
        style={{
          background: value ? 'rgba(59,130,246,0.3)' : '#1e293b',
          border: value ? '1px solid rgba(59,130,246,0.5)' : '1px solid #334155',
          boxShadow: value ? '0 0 8px rgba(59,130,246,0.25)' : 'none',
        }}
      >
        <div
          className="absolute top-0.5 w-4 h-4 rounded-full transition-all"
          style={{
            background: value ? '#3b82f6' : '#475569',
            left: value ? 'calc(100% - 18px)' : '2px',
            boxShadow: value ? '0 0 6px rgba(59,130,246,0.6)' : 'none',
          }}
        />
      </button>
    </div>
  )
}

export function SolverControlsPanel({ controls, send }: SolverControlsPanelProps) {
  const [local, setLocal] = useState<SolverControls>(controls ?? DEFAULT_CONTROLS)

  // Sync from server
  useEffect(() => {
    if (controls) setLocal(controls)
  }, [controls])

  const update = useCallback((patch: Partial<SolverControls>) => {
    setLocal(prev => {
      const next = { ...prev, ...patch }
      send({ type: 'SetControls', ...next })
      return next
    })
  }, [send])

  return (
    <div className="flex flex-col gap-1 px-3 py-2">
      <div className="text-[10px] font-bold tracking-wider mb-1" style={{ color: '#475569' }}>SOLVER CONTROLS</div>

      {/* Temperature */}
      <div className="py-1">
        <div className="flex items-center justify-between mb-1">
          <span className="text-[11px]" style={{ color: '#94a3b8' }}>Temperature</span>
          <span
            className="text-[11px] font-mono"
            style={{ color: '#60a5fa', fontFamily: "'JetBrains Mono', monospace" }}
          >
            {local.temperature_c.toFixed(0)}°C
          </span>
        </div>
        <input
          type="range"
          min={-40}
          max={125}
          step={1}
          value={local.temperature_c}
          onChange={e => update({ temperature_c: parseFloat(e.target.value) })}
          className="w-full h-1 rounded cursor-pointer"
          style={{ accentColor: '#3b82f6' }}
        />
        <div className="flex justify-between text-[9px] mt-0.5" style={{ color: '#334155' }}>
          <span>-40°C</span><span>125°C</span>
        </div>
      </div>

      <div className="w-full h-px my-1" style={{ background: '#1e293b' }} />

      {/* Toggles */}
      <Toggle
        label="Parasitics"
        description="RC parasitics on traces"
        value={local.parasitics}
        onChange={v => update({ parasitics: v })}
      />
      <Toggle
        label="Junction Caps"
        description="BJT/diode junction capacitances"
        value={local.junction_caps}
        onChange={v => update({ junction_caps: v })}
      />
      <Toggle
        label="Tolerances"
        description="Component value spread"
        value={local.tolerances}
        onChange={v => update({ tolerances: v })}
      />

      <div className="w-full h-px my-1" style={{ background: '#1e293b' }} />

      {/* Integration method */}
      <div className="py-1">
        <div className="text-[11px] mb-1" style={{ color: '#94a3b8' }}>Integration</div>
        <div className="flex gap-1">
          {(['trap', 'gear2'] as const).map(m => (
            <button
              key={m}
              onClick={() => update({ integration: m })}
              className="flex-1 py-1 rounded text-[10px] font-mono transition-all"
              style={{
                background: local.integration === m ? 'rgba(59,130,246,0.2)' : '#0f172a',
                border: local.integration === m ? '1px solid rgba(59,130,246,0.4)' : '1px solid #1e293b',
                color: local.integration === m ? '#60a5fa' : '#475569',
                fontFamily: "'JetBrains Mono', monospace",
              }}
            >
              {m}
            </button>
          ))}
        </div>
      </div>

      {/* Fixed dt */}
      <div className="py-1">
        <div className="flex items-center justify-between mb-1">
          <span className="text-[11px]" style={{ color: '#94a3b8' }}>Fixed dt</span>
          <span className="text-[10px]" style={{ color: '#475569' }}>0 = adaptive</span>
        </div>
        <input
          type="number"
          min={0}
          max={0.01}
          step={0.0001}
          value={local.fixed_dt}
          onChange={e => update({ fixed_dt: parseFloat(e.target.value) || 0 })}
          className="w-full px-2 py-1 rounded text-[11px] font-mono outline-none"
          style={{
            background: '#0f172a',
            border: '1px solid #1e293b',
            color: '#94a3b8',
            fontFamily: "'JetBrains Mono', monospace",
          }}
        />
      </div>

      {/* Granularity */}
      <div className="py-1">
        <div className="flex items-center justify-between mb-1">
          <span className="text-[11px]" style={{ color: '#94a3b8' }}>Granularity</span>
          <span
            className="text-[11px] font-mono"
            style={{ color: '#60a5fa', fontFamily: "'JetBrains Mono', monospace" }}
          >
            {local.granularity.toFixed(2)}
          </span>
        </div>
        <input
          type="range"
          min={0}
          max={1}
          step={0.01}
          value={local.granularity}
          onChange={e => update({ granularity: parseFloat(e.target.value) })}
          className="w-full h-1 rounded cursor-pointer"
          style={{ accentColor: '#3b82f6' }}
        />
        <div className="flex justify-between text-[9px] mt-0.5" style={{ color: '#334155' }}>
          <span>fast</span><span>accurate</span>
        </div>
      </div>
    </div>
  )
}
