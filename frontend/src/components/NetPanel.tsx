import type { SimFrame } from '../types/protocol'
import { displayNet } from '../lib/net-name'

// Live net-voltage list for the sim rail's "Net voltages" card. The card
// header is owned by the rail; this renders only the rows.

interface NetPanelProps {
  frame: SimFrame | null
  selectedNet: string | null
  onSelectNet: (net: string | null) => void
}

// Rows rendered at once. A 3,000-net board rebuilt thousands of DOM rows on
// every sim frame; the list is sorted by |V| so the cap keeps the rows that
// carry information and drops the tail of dead nets.
const MAX_ROWS = 150

export function NetPanel({ frame, selectedNet, onSelectNet }: NetPanelProps) {
  const voltages = frame?.net_voltages ?? {}
  const allEntries = Object.entries(voltages).sort((a, b) => Math.abs(b[1]) - Math.abs(a[1]))
  const entries = allEntries.slice(0, MAX_ROWS)
  // Never hide the selected net behind the cap
  if (selectedNet && voltages[selectedNet] !== undefined && !entries.some(([n]) => n === selectedNet)) {
    entries.push([selectedNet, voltages[selectedNet]])
  }

  if (entries.length === 0) {
    return (
      <div className="px-3 py-2.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
        No data, start the simulation
      </div>
    )
  }

  return (
    <div>
      {selectedNet && (
        <div className="px-3 pt-2 flex justify-end">
          <button
            onClick={() => onSelectNet(null)}
            className="hb-press text-[10px] cursor-pointer"
            style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
          >
            clear selection
          </button>
        </div>
      )}
      <div className="overflow-y-auto py-1" style={{ maxHeight: 280 }}>
        {entries.map(([name, voltage]) => {
          const isSelected = name === selectedNet
          const absV = Math.abs(voltage)
          const barWidth = Math.min(100, (absV / 5) * 100)
          const barColor = voltage > 4 ? 'var(--warn)'
            : voltage > 0.1 ? 'var(--ok)'
            : voltage < -0.1 ? 'var(--err)'
            : 'var(--silk-faint)'
          return (
            <div
              key={name}
              onClick={() => onSelectNet(isSelected ? null : name)}
              className="flex items-center gap-2 px-3 py-1.5 cursor-pointer"
              style={{
                background: isSelected ? 'var(--copper-tint)' : 'transparent',
                borderLeft: isSelected ? '2px solid var(--copper)' : '2px solid transparent',
              }}
            >
              <div className="flex-1 min-w-0">
                <div className="flex items-center justify-between mb-0.5">
                  <span className="text-[10px] truncate" style={{
                    color: isSelected ? 'var(--copper-hi)' : 'var(--silk-dim)',
                    fontFamily: 'var(--font-mono)',
                  }}>
                    {displayNet(name)}
                  </span>
                  <span className="text-[10px] ml-2 shrink-0 tnum" style={{
                    color: isSelected ? 'var(--copper)' : barColor,
                    fontFamily: 'var(--font-mono)',
                  }}>
                    {voltage >= 0 ? '+' : ''}{voltage.toFixed(3)}V
                  </span>
                </div>
                <div className="h-0.5 rounded-full overflow-hidden" style={{ background: 'var(--surface-3)' }}>
                  <div
                    className="h-full rounded-full"
                    style={{
                      width: `${barWidth}%`,
                      background: barColor,
                      transition: 'width 0.15s linear',
                    }}
                  />
                </div>
              </div>
            </div>
          )
        })}
        {allEntries.length > entries.length && (
          <div className="px-3 py-1.5 text-[9px]" style={{ color: 'var(--silk-faint)' }}>
            showing top {entries.length} of {allEntries.length} nets by |V|
          </div>
        )}
      </div>
    </div>
  )
}
