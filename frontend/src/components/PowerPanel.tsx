/**
 * PowerPanel.tsx
 *
 * Power supply configuration panel. Gated on BoardInfo.power_supplies being present
 * (a net -> supply-config MAP on the wire; omitted when the board has none).
 * Shows a quiet empty state when absent.
 *
 * Sends SetPowerSupply messages: { type: 'SetPowerSupply', net, supply } where
 * `supply` is the server's tagged wire shape (see lib/supply-wire.ts).
 */

import { useState, useEffect } from 'react'
import type { BoardInfoMsg, SimFrame } from '../types/protocol'
import type { ClientMessage } from '../types/protocol'
import { toWireSupply, supplyNetNames } from '../lib/supply-wire'
import type { SupplyConfig, SupplyType } from '../lib/supply-wire'

const SUPPLY_DEFAULTS: Record<SupplyType, SupplyConfig> = {
  Ideal:   { type: 'Ideal',   volts: 5.0, currentLimit: 10,   ripple: 0,    capacity: 0 },
  Bench:   { type: 'Bench',   volts: 5.0, currentLimit: 3.0,  ripple: 0.02, capacity: 0 },
  Wall:    { type: 'Wall',    volts: 5.0, currentLimit: 2.0,  ripple: 0.1,  capacity: 0 },
  USB:     { type: 'USB',     volts: 5.0, currentLimit: 0.5,  ripple: 0.05, capacity: 0 },
  Battery: { type: 'Battery', volts: 3.7, currentLimit: 2.0,  ripple: 0,    capacity: 2.0 },
}

interface PowerPanelProps {
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  send: (msg: ClientMessage) => void
}

function usePowerSupplies(boardInfo: BoardInfoMsg | null): string[] | null {
  // power_supplies is a net -> config map (omitted when the board has none);
  // extract the net names. Treating it as an array crashed the whole view.
  return supplyNetNames(boardInfo?.power_supplies)
}

export function PowerPanel({ boardInfo, frame, send }: PowerPanelProps) {
  const supplyNets = usePowerSupplies(boardInfo)
  const [configs, setConfigs] = useState<Record<string, SupplyConfig>>({})

  // Initialise configs when nets appear
  useEffect(() => {
    if (!supplyNets) return
    setConfigs(prev => {
      const next = { ...prev }
      for (const net of supplyNets) {
        if (!next[net]) next[net] = { ...SUPPLY_DEFAULTS.Ideal }
      }
      return next
    })
  }, [supplyNets])

  if (!supplyNets) {
    return (
      <div
        className="flex flex-col gap-1"
        style={{ background: '#0f172a', border: '1px solid #1e293b', borderRadius: 8 }}
      >
        <div className="px-3 pt-2.5 pb-2.5">
          <span className="text-[10px] font-bold tracking-wider" style={{ color: '#64748b' }}>
            POWER SUPPLIES
          </span>
          <div className="mt-1 text-[10px]" style={{ color: '#334155' }}>
            Not available for this board
          </div>
        </div>
      </div>
    )
  }

  function updateConfig(net: string, patch: Partial<SupplyConfig>) {
    setConfigs(prev => {
      const next = { ...prev, [net]: { ...(prev[net] ?? SUPPLY_DEFAULTS.Ideal), ...patch } }
      // The server deserializes a tagged enum; the raw UI config used to fail
      // serde and the panel silently did nothing. Map to the wire shape.
      send({ type: 'SetPowerSupply', net, supply: toWireSupply(next[net]) })
      return next
    })
  }

  // Live SoC comes from SimFrame.supply_states (net -> {kind, current_a, soc}).
  const supplyStates = frame?.supply_states

  return (
    <div
      className="flex flex-col gap-1"
      style={{ background: '#0f172a', border: '1px solid #1e293b', borderRadius: 8, overflow: 'hidden' }}
    >
      <div className="px-3 pt-2.5 pb-1">
        <span className="text-[10px] font-bold tracking-wider" style={{ color: '#64748b' }}>
          POWER SUPPLIES
        </span>
      </div>

      <div className="overflow-y-auto" style={{ maxHeight: 300 }}>
        {supplyNets.map(net => {
          const cfg = configs[net] ?? SUPPLY_DEFAULTS.Ideal
          const soc = supplyStates?.[net]?.soc

          return (
            <div
              key={net}
              className="px-3 py-2 flex flex-col gap-2"
              style={{ borderTop: '1px solid #1e293b' }}
            >
              <div className="flex items-center justify-between">
                <span
                  className="text-[10px] font-mono font-bold"
                  style={{ color: '#93c5fd', fontFamily: "'JetBrains Mono', monospace" }}
                >
                  {net}
                </span>
                {soc !== undefined && (
                  <span className="text-[9px]" style={{ color: '#64748b' }}>
                    SoC: {(soc * 100).toFixed(1)}%
                  </span>
                )}
              </div>

              {/* Supply type selector */}
              <div className="flex gap-1 flex-wrap">
                {(Object.keys(SUPPLY_DEFAULTS) as SupplyType[]).map(t => (
                  <button
                    key={t}
                    onClick={() => updateConfig(net, { ...SUPPLY_DEFAULTS[t] })}
                    className="text-[9px] px-2 py-0.5 rounded transition-all"
                    style={{
                      background: cfg.type === t ? '#1e40af' : '#1e293b',
                      color: cfg.type === t ? '#93c5fd' : '#475569',
                      border: cfg.type === t ? '1px solid #3b82f6' : '1px solid #334155',
                    }}
                  >
                    {t}
                  </button>
                ))}
              </div>

              {/* Fields */}
              <div className="grid grid-cols-2 gap-x-3 gap-y-1">
                <SupplyField
                  label="Voltage (V)"
                  value={cfg.volts}
                  step={0.1}
                  min={0}
                  max={48}
                  onChange={v => updateConfig(net, { volts: v })}
                />
                <SupplyField
                  label="I limit (A)"
                  value={cfg.currentLimit}
                  step={0.1}
                  min={0}
                  max={20}
                  onChange={v => updateConfig(net, { currentLimit: v })}
                />
                {cfg.type !== 'Ideal' && (
                  <SupplyField
                    label="Ripple (Vpp)"
                    value={cfg.ripple}
                    step={0.01}
                    min={0}
                    max={1}
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
                    onChange={v => updateConfig(net, { capacity: v })}
                  />
                )}
              </div>
            </div>
          )
        })}
      </div>
    </div>
  )
}

function SupplyField({ label, value, step, min, max, onChange }: {
  label: string
  value: number
  step: number
  min: number
  max: number
  onChange: (v: number) => void
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[9px]" style={{ color: '#475569' }}>{label}</span>
      <input
        type="number"
        value={value}
        step={step}
        min={min}
        max={max}
        onChange={e => onChange(parseFloat(e.target.value) || 0)}
        className="text-[10px] px-1.5 py-0.5 rounded font-mono w-full"
        style={{
          background: '#0a0f1e',
          border: '1px solid #334155',
          color: '#94a3b8',
          fontFamily: "'JetBrains Mono', monospace",
          outline: 'none',
        }}
      />
    </div>
  )
}
