import { useState, useEffect, useCallback } from 'react'
import type { SolverControls, ClientMessage } from '../types/protocol'
import { Switch } from './ui'
import { labelFor } from '../lib/labels'

// The "Conditions" tab of the live dock: what a user can reasonably change about
// the circuit's surroundings (temperature, whether parts sit exactly on their
// nominal values) up front, and the numerical-solver internals behind an
// "Advanced" disclosure with a plain sentence for each, because "trap", "gear2"
// and "granularity" mean nothing until they are explained.

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
    <div className="flex items-center justify-between gap-3 py-1">
      <div className="min-w-0">
        <div className="text-[11px]" style={{ color: 'var(--silk)' }}>{label}</div>
        {description && <div className="text-[10px] leading-snug" style={{ color: 'var(--silk-faint)' }}>{description}</div>}
      </div>
      <Switch on={value} onToggle={() => onChange(!value)} label={label} />
    </div>
  )
}

/** A labelled range with its current value and the meaning of each end. */
function Slider({ label, aria, readout, min, max, step, ends, value, onChange, description }: {
  label: string
  aria: string
  readout: string
  min: number
  max: number
  step: number
  ends: [string, string]
  value: number
  onChange: (v: number) => void
  description?: string
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
      <div className="flex justify-between text-[10px] mt-0.5" style={{ color: 'var(--silk-faint)' }}>
        <span>{ends[0]}</span><span>{ends[1]}</span>
      </div>
      {description && <div className="text-[10px] leading-snug mt-0.5" style={{ color: 'var(--silk-faint)' }}>{description}</div>}
    </div>
  )
}

const METHODS: { id: SolverControls['integration']; label: string; blurb: string }[] = [
  { id: 'trap', label: labelFor('trap'), blurb: 'The default: accurate for almost every board.' },
  { id: 'gear2', label: labelFor('gear2'), blurb: 'Damps numerical ringing. Try it if a switching circuit rings in the sim but not on the bench.' },
]

export function SolverControlsPanel({ controls, send }: SolverControlsPanelProps) {
  const [local, setLocal] = useState<SolverControls>(controls ?? DEFAULT_CONTROLS)
  const [advanced, setAdvanced] = useState(false)

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

  const method = METHODS.find(m => m.id === local.integration) ?? METHODS[0]
  const changed = local.parasitics !== DEFAULT_CONTROLS.parasitics
    || local.junction_caps !== DEFAULT_CONTROLS.junction_caps
    || local.integration !== DEFAULT_CONTROLS.integration
    || local.fixed_dt !== DEFAULT_CONTROLS.fixed_dt
    || local.granularity !== DEFAULT_CONTROLS.granularity

  return (
    <div className="flex flex-col gap-1 px-3 py-2.5" data-testid="conditions-panel">
      <Slider
        label="Temperature" aria="Ambient temperature in Celsius" readout={`${local.temperature_c.toFixed(0)}°C`}
        min={-40} max={125} step={1} ends={['-40°C', '125°C']}
        value={local.temperature_c} onChange={v => update({ temperature_c: v })}
        description="The ambient temperature the sim runs at. Temperature-dependent models, such as diodes and transistors, follow it."
      />

      <div className="w-full h-px my-1" style={{ background: 'var(--rule)' }} />

      <Toggle
        label="Part tolerances"
        description="Let each part's value vary within its rated tolerance, as real parts do, instead of sitting exactly on nominal."
        value={local.tolerances}
        onChange={v => update({ tolerances: v })}
      />

      <div className="w-full h-px my-1" style={{ background: 'var(--rule)' }} />

      <button
        type="button"
        data-testid="solver-advanced-toggle"
        aria-expanded={advanced}
        onClick={() => setAdvanced(a => !a)}
        className="hb-press flex items-center gap-1.5 text-[11px] cursor-pointer self-start"
        style={{ background: 'none', border: 'none', padding: '2px 0', color: 'var(--silk-dim)' }}
      >
        <span aria-hidden="true" style={{ display: 'inline-block', width: 10 }}>{advanced ? '▾' : '▸'}</span>
        Advanced solver settings
        {changed && !advanced && <span className="text-[10px]" style={{ color: 'var(--warn-strong)' }}>(changed)</span>}
      </button>

      {advanced && (
        <div className="flex flex-col gap-1 pl-3" data-testid="solver-advanced" style={{ borderLeft: '2px solid var(--hairline)' }}>
          <div className="text-[10px] leading-snug" style={{ color: 'var(--silk-faint)' }}>
            These tune the numerical engine, not the circuit. The defaults suit almost every board; change them to
            chase a speed or accuracy problem.
          </div>

          <Toggle
            label="Trace parasitics"
            description="Add the small resistance and capacitance of the copper traces. Slower, and only matters for very fast edges."
            value={local.parasitics}
            onChange={v => update({ parasitics: v })}
          />
          <Toggle
            label="Junction capacitance"
            description="Include the internal capacitance of transistors and diodes. Needed for realistic switching edges; turning it off is faster but idealised."
            value={local.junction_caps}
            onChange={v => update({ junction_caps: v })}
          />

          <div className="py-1">
            <div className="text-[11px] mb-1" style={{ color: 'var(--silk)' }}>Integration method</div>
            <div className="flex gap-1" role="group" aria-label="Integration method">
              {METHODS.map(m => (
                <button
                  key={m.id}
                  type="button"
                  aria-pressed={local.integration === m.id}
                  onClick={() => update({ integration: m.id })}
                  className="hb-press flex-1 py-1 rounded-md text-[11px] cursor-pointer"
                  style={{
                    background: local.integration === m.id ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
                    border: local.integration === m.id ? '1px solid var(--copper-deep)' : '1px solid var(--hairline)',
                    color: local.integration === m.id ? 'var(--copper-hi)' : 'var(--silk-faint)',
                  }}
                >
                  {m.label}
                </button>
              ))}
            </div>
            <div className="text-[10px] leading-snug mt-1" style={{ color: 'var(--silk-faint)' }}>{method.blurb}</div>
          </div>

          <div className="py-1">
            <div className="flex items-center justify-between mb-1">
              <label htmlFor="solver-fixed-dt" className="text-[11px]" style={{ color: 'var(--silk)' }}>Fixed time step (s)</label>
              <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>{local.fixed_dt === 0 ? 'solver chooses' : 'fixed'}</span>
            </div>
            <input
              id="solver-fixed-dt"
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
            <div className="text-[10px] leading-snug mt-0.5" style={{ color: 'var(--silk-faint)' }}>
              Leave at 0 and the solver picks the step as it goes, small around fast edges and large when nothing
              moves. A fixed step is only for comparing runs exactly.
            </div>
          </div>

          <Slider
            label="Accuracy vs speed" aria="Solver granularity: fast to accurate" readout={local.granularity.toFixed(2)}
            min={0} max={1} step={0.01} ends={['faster', 'more accurate']}
            value={local.granularity} onChange={v => update({ granularity: v })}
            description="How finely each step is subdivided. More accurate is slower."
          />

          {changed && (
            <button
              type="button"
              data-testid="solver-restore-defaults"
              onClick={() => update({
                parasitics: DEFAULT_CONTROLS.parasitics,
                junction_caps: DEFAULT_CONTROLS.junction_caps,
                integration: DEFAULT_CONTROLS.integration,
                fixed_dt: DEFAULT_CONTROLS.fixed_dt,
                granularity: DEFAULT_CONTROLS.granularity,
              })}
              className="hb-btn hb-press self-start text-[11px] px-2 cursor-pointer"
              style={{ height: 26 }}
            >
              Restore the defaults
            </button>
          )}
        </div>
      )}
    </div>
  )
}
