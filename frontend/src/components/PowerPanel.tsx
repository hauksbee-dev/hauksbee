/**
 * PowerPanel.tsx
 *
 * Power-rail configuration for the sim rail's "Power rails" card. Gated on
 * BoardInfo.power_supplies being present (a net -> supply-config MAP on the
 * wire; omitted when the board has none). Per rail: the live voltage and
 * delivered current from the frame, the supply type, and its parameters.
 *
 * Sends SetPowerSupply messages: { type: 'SetPowerSupply', net, supply } where
 * `supply` is the server's tagged wire shape (see lib/supply-wire.ts).
 */

import { useState } from 'react'
import type { BoardInfoMsg, ClientMessage, SimFrame } from '../types/protocol'
import { toWireSupply, supplyNetNames, appliedVolts, fromWireSupply, SUPPLY_DEFAULTS } from '../lib/supply-wire'
import type { SupplyConfig, SupplyType } from '../lib/supply-wire'
import { displayNet } from '../lib/net-name'

interface PowerPanelProps {
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  send: (msg: ClientMessage) => void
}

export function PowerPanel({ boardInfo, frame, send }: PowerPanelProps) {
  // power_supplies is a net -> config map (omitted when the board has none);
  // extract the net names. Treating it as an array crashed the whole view.
  const supplies = boardInfo?.power_supplies
  const supplyNets = supplyNetNames(supplies)
  // Only rails the user has edited live here; an untouched rail reads what the
  // server says it runs, so VDD_3V3 is not shown (or first edited) at 5 V.
  const [edited, setEdited] = useState<Record<string, SupplyConfig>>({})
  const configFor = (net: string, from = edited) => from[net] ?? fromWireSupply(supplies?.[net])

  function updateConfig(net: string, patch: Partial<SupplyConfig>) {
    setEdited(prev => {
      const next = { ...prev, [net]: { ...configFor(net, prev), ...patch } }
      // The server deserializes a tagged enum, and the raw UI config fails
      // serde there, leaving the panel silently doing nothing. Map to the wire
      // shape.
      send({ type: 'SetPowerSupply', net, supply: toWireSupply(next[net]) })
      return next
    })
  }

  // Live rail readout comes from the frame: the rail's solved voltage and the
  // supply's delivered current / state of charge.
  const supplyStates = frame?.supply_states

  return (
    <div className="overflow-y-auto" style={{ maxHeight: 340 }}>
      {(supplyNets ?? []).map((net, i) => {
        const cfg = configFor(net)
        // What the rail will actually run at: USB and battery set their own
        // voltage, so the box shows that rather than a setpoint nothing reads.
        const applied = appliedVolts(cfg)
        const state = supplyStates?.[net]
        const liveVolts = frame?.net_voltages?.[net]

        return (
          <div
            key={net}
            className="px-3 py-2.5 flex flex-col gap-2"
            style={{ borderTop: i > 0 ? '1px solid var(--rule)' : 'none' }}
          >
            <div className="flex items-baseline justify-between gap-2">
              <span
                className="text-[12px] font-bold truncate"
                style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
              >
                {displayNet(net)}
              </span>
              <span className="text-[11px] shrink-0 tnum" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                {liveVolts !== undefined && <span style={{ color: 'var(--copper)' }}>{liveVolts.toFixed(3)} V</span>}
                {state && <span> · {state.current_a.toFixed(3)} A</span>}
              </span>
            </div>
            {state && state.soc < 1 && (
              <div className="flex items-center gap-2">
                <div className="h-1 rounded-full overflow-hidden flex-1" style={{ background: 'var(--surface-3)' }}>
                  <div
                    className="h-full rounded-full"
                    style={{
                      width: `${(state.soc * 100).toFixed(1)}%`,
                      background: state.soc > 0.3 ? 'var(--ok)' : 'var(--err)',
                      transition: 'width 0.3s linear',
                    }}
                  />
                </div>
                <span className="text-[9px] tnum" style={{ color: 'var(--silk-faint)' }}>
                  SoC {(state.soc * 100).toFixed(1)}%
                </span>
              </div>
            )}

            {/* Supply type selector */}
            <div className="flex gap-1 flex-wrap">
              {(Object.keys(SUPPLY_DEFAULTS) as SupplyType[]).map(t => (
                <button
                  key={t}
                  onClick={() => updateConfig(net, { ...SUPPLY_DEFAULTS[t], volts: cfg.volts })}
                  className="hb-press text-[10px] px-2 py-0.5 rounded-md cursor-pointer"
                  style={{
                    background: cfg.type === t ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
                    color: cfg.type === t ? 'var(--copper-hi)' : 'var(--silk-faint)',
                    border: cfg.type === t ? '1px solid var(--copper-deep)' : '1px solid var(--hairline)',
                  }}
                >
                  {t}
                </button>
              ))}
            </div>

            {/* Fields. Keyed on the supply type so switching type starts the
                boxes fresh (a "capped at" note from the old type must not
                outlive it). */}
            <div className="grid grid-cols-2 gap-x-3 gap-y-1" key={cfg.type}>
              <SupplyField
                label="Voltage (V)"
                value={applied.volts}
                step={0.1}
                min={0}
                max={48}
                unit="V"
                fixedBy={applied.fixedBy}
                onChange={v => updateConfig(net, { volts: v })}
              />
              <SupplyField
                label="I limit (A)"
                value={cfg.currentLimit}
                step={0.1}
                min={0}
                max={20}
                unit="A"
                onChange={v => updateConfig(net, { currentLimit: v })}
              />
              {cfg.type !== 'Ideal' && (
                <SupplyField
                  label="Ripple (Vpp)"
                  value={cfg.ripple}
                  step={0.01}
                  min={0}
                  max={1}
                  unit="Vpp"
                  onChange={v => updateConfig(net, { ripple: v })}
                />
              )}
              {cfg.type === 'Battery' && (
                <SupplyField
                  label="Capacity (Ah)"
                  value={cfg.capacity}
                  step={0.1}
                  min={0}
                  max={100}
                  unit="Ah"
                  onChange={v => updateConfig(net, { capacity: v })}
                />
              )}
            </div>
          </div>
        )
      })}
    </div>
  )
}

/** One supply parameter box.
 *
 *  The box and the value the sim is running on must never disagree. `min`/`max`
 *  on a number input are advisory (the browser lets you type straight past
 *  them), so a typed 60 in a 0..48 box would sit there in red while the panel
 *  passed 60 down, and a supply type that sets its own voltage (USB, battery)
 *  would leave the box reading whatever was last typed while the rail ran at
 *  5 V. So:
 *
 *  - out-of-range input is clamped to the bound, applied clamped, and the box
 *    snaps to what was applied on commit (blur / Enter), saying "capped at N";
 *  - a value the supply TYPE fixes is shown read-only with the reason, since
 *    there is nothing to type there.
 *
 *  While the box has focus the draft is left alone: clamping mid-keystroke
 *  fights the typist ("1" on the way to "12" is not a request for 1). */
function SupplyField({ label, value, step, min, max, unit, fixedBy, onChange }: {
  label: string
  value: number
  step: number
  min: number
  max: number
  unit: string
  /** Set when the supply type owns this value: renders read-only with the note. */
  fixedBy?: string
  onChange: (v: number) => void
}) {
  const [draft, setDraft] = useState<string | null>(null)
  const [capped, setCapped] = useState<number | null>(null)

  const clamp = (v: number) => Math.min(max, Math.max(min, v))
  const shown = fixedBy ? String(value) : (draft ?? String(value))

  function commit() {
    const parsed = parseFloat(draft ?? '')
    const applied = Number.isFinite(parsed) ? clamp(parsed) : value
    setCapped(Number.isFinite(parsed) && applied !== parsed ? applied : null)
    setDraft(null)
    if (applied !== value) onChange(applied)
  }

  const note = fixedBy ?? (capped !== null ? `capped at ${capped} ${unit}` : null)

  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[9px]" style={{ color: 'var(--silk-faint)' }}>{label}</span>
      <input
        type="number"
        value={shown}
        step={step}
        min={min}
        max={max}
        readOnly={!!fixedBy}
        aria-describedby={note ? `${label}-note` : undefined}
        title={fixedBy}
        onChange={e => {
          if (fixedBy) return
          setDraft(e.target.value)
          const parsed = parseFloat(e.target.value)
          if (!Number.isFinite(parsed)) return
          const applied = clamp(parsed)
          setCapped(applied !== parsed ? applied : null)
          onChange(applied)
        }}
        onBlur={commit}
        onKeyDown={e => { if (e.key === 'Enter') { commit(); (e.target as HTMLInputElement).blur() } }}
        className="hb-input tnum text-[11px] w-full"
        style={{
          fontFamily: 'var(--font-mono)', padding: '2px 6px',
          ...(fixedBy ? { color: 'var(--silk-faint)', cursor: 'default' } : {}),
        }}
      />
      {note && (
        <span
          id={`${label}-note`}
          className="text-[9px] leading-tight"
          style={{ color: fixedBy ? 'var(--silk-faint)' : 'var(--warn-strong)' }}
        >
          {note}
        </span>
      )}
    </div>
  )
}
