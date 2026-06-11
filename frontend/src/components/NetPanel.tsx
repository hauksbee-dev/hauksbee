import type { SimFrame } from '../types/protocol'

interface NetPanelProps {
  frame: SimFrame | null
  selectedNet: string | null
  onSelectNet: (net: string | null) => void
}

export function NetPanel({ frame, selectedNet, onSelectNet }: NetPanelProps) {
  const voltages = frame?.net_voltages ?? {}
  const entries = Object.entries(voltages).sort((a, b) => Math.abs(b[1]) - Math.abs(a[1]))

  return (
    <div
      className="flex flex-col gap-1"
      style={{
        background: '#0f172a',
        border: '1px solid #1e293b',
        borderRadius: 8,
        overflow: 'hidden',
      }}
    >
      <div className="px-3 pt-2.5 pb-1 flex items-center justify-between">
        <span className="text-[10px] font-bold tracking-wider" style={{ color: '#64748b' }}>
          NET VOLTAGES
        </span>
        {selectedNet && (
          <button
            onClick={() => onSelectNet(null)}
            className="text-[10px] hover:opacity-70"
            style={{ color: '#475569' }}
          >
            clear
          </button>
        )}
      </div>

      {entries.length === 0 ? (
        <div className="px-3 pb-2.5 text-[10px]" style={{ color: '#334155' }}>
          No data — start simulation
        </div>
      ) : (
        <div className="overflow-y-auto" style={{ maxHeight: 280 }}>
          {entries.map(([name, v]) => {
            const isSelected = name === selectedNet
            const voltage = v
            const absV = Math.abs(voltage)
            const barWidth = Math.min(100, (absV / 5) * 100)
            return (
              <div
                key={name}
                onClick={() => onSelectNet(isSelected ? null : name)}
                className="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:opacity-90"
                style={{
                  background: isSelected ? '#1e3a5f' : 'transparent',
                  borderLeft: isSelected ? '2px solid #3b82f6' : '2px solid transparent',
                }}
              >
                <div className="flex-1 min-w-0">
                  <div className="flex items-center justify-between mb-0.5">
                    <span className="text-[10px] truncate font-mono" style={{
                      color: isSelected ? '#93c5fd' : '#94a3b8',
                      fontFamily: "'JetBrains Mono', monospace",
                    }}>
                      {name}
                    </span>
                    <span className="text-[10px] font-mono ml-2 shrink-0" style={{
                      color: isSelected ? '#60a5fa'
                        : voltage > 4 ? '#f59e0b'
                        : voltage > 0.1 ? '#4ade80'
                        : voltage < -0.1 ? '#f87171'
                        : '#64748b',
                      fontFamily: "'JetBrains Mono', monospace",
                    }}>
                      {voltage >= 0 ? '+' : ''}{voltage.toFixed(3)}V
                    </span>
                  </div>
                  <div className="h-0.5 rounded-full overflow-hidden" style={{ background: '#0f172a' }}>
                    <div
                      className="h-full rounded-full transition-all duration-150"
                      style={{
                        width: `${barWidth}%`,
                        background: voltage > 4 ? '#f59e0b'
                          : voltage > 0.1 ? '#22c55e'
                          : voltage < -0.1 ? '#ef4444'
                          : '#475569',
                        boxShadow: absV > 0.5
                          ? `0 0 4px ${voltage > 0 ? '#22c55e80' : '#ef444480'}`
                          : 'none',
                      }}
                    />
                  </div>
                </div>
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
