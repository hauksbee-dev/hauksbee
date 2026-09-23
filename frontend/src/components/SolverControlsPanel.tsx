import { useState, useEffect, useCallback } from 'react'
import type { SolverControls, ClientMessage } from '../types/protocol'
import { Switch } from './ui'

// Solver controls for the sim rail's "Solver" card: ambient temperature,
// model fidelity toggles, integration method, timestep and granularity.

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
        <div className="text-[11px]" style={{ color: 'var(--silk)' }}>{label}</div>
        {description && <div className="text-[9px]" style={{ color: 'var(--silk-faint)' }}>{description}</div>}
      </div>
      <Switch on={value} onToggle={() => onChange(!value)} label={label} />
    </div>
  )
}

/** A labelled range with its current value and the meaning of each end. */
function Slider({ label, aria, readout, min, max, step, ends, value, onChange }: {
  label: string
  aria: string
  readout: string
  min: number
  max: number
  step: number
  ends: [string, string]
  value: number
  onChange: (v: number) => void
}) {
  return (
    <div className="py-1">
      <div className="flex items-center justify-between mb-1">
        <span className="text-[11px]" style={{ color: 'var(--silk)' }}>{label}</span>
        <span className="text-[11px] tnum" style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}>
          {readout}
        </span>
      </div>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={e => onChange(parseFloat(e.target.value))}
        aria-label={aria}
        className="w-full h-1 rounded cursor-pointer"
        style={{ accentColor: 'var(--copper)' }}
      />
      <div className="flex justify-between text-[9px] mt-0.5" style={{ color: 'var(--silk-faint)' }}>
        <span>{ends[0]}</span><span>{ends[1]}</span>
      </div>
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
    <div className="flex flex-col gap-1 px-3 py-2.5">
      <Slider
        label="Temperature" aria="Ambient temperature in Celsius" readout={`${local.temperature_c.toFixed(0)}°C`}
        min={-40} max={125} step={1} ends={['-40°C', '125°C']}
        value={local.temperature_c} onChange={v => update({ temperature_c: v })}
      />

      <div className="w-full h-px my-1" style={{ background: 'var(--rule)' }} />

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

      <div className="w-full h-px my-1" style={{ background: 'var(--rule)' }} />

      {/* Integration method */}
      <div className="py-1">
        <div className="text-[11px] mb-1" style={{ color: 'var(--silk)' }}>Integration</div>
        <div className="flex gap-1">
          {(['trap', 'gear2'] as const).map(m => (
            <button
              key={m}
              onClick={() => update({ integration: m })}
              className="hb-press flex-1 py-1 rounded-md text-[10px] cursor-pointer"
              style={{
                background: local.integration === m ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
                border: local.integration === m ? '1px solid var(--copper-deep)' : '1px solid var(--hairline)',
                color: local.integration === m ? 'var(--copper-hi)' : 'var(--silk-faint)',
                fontFamily: 'var(--font-mono)',
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
          <span className="text-[11px]" style={{ color: 'var(--silk)' }}>Fixed dt</span>
          <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>0 = adaptive</span>
        </div>
        <input
          type="number"
          min={0}
          max={0.01}
          step={0.0001}
          value={local.fixed_dt}
          onChange={e => update({ fixed_dt: parseFloat(e.target.value) || 0 })}
          aria-label="Fixed timestep in seconds, zero for adaptive"
          className="hb-input tnum w-full text-[11px]"
          style={{ fontFamily: 'var(--font-mono)' }}
        />
      </div>

      <Slider
        label="Granularity" aria="Solver granularity" readout={local.granularity.toFixed(2)}
        min={0} max={1} step={0.01} ends={['fast', 'accurate']}
        value={local.granularity} onChange={v => update({ granularity: v })}
      />
    </div>
  )
}
