import { useState, useCallback } from 'react'
import type { BoardInfoMsg, SimFrame, ClientMessage } from '../types/protocol'

interface InputSourcesPanelProps {
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  send: (msg: ClientMessage) => void
}

// Heuristic: nets that look like input sources
function isInputNet(name: string): boolean {
  return /^A\d+$/.test(name) || name.startsWith('INPUT') || name.startsWith('IN_')
}

export function InputSourcesPanel({ boardInfo, frame, send }: InputSourcesPanelProps) {
  const nets = boardInfo?.nets ?? []
  const inputNets = nets.filter(isInputNet)

  const [values, setValues] = useState<Record<string, number>>({})

  const handleChange = useCallback((source: string, value: number) => {
    setValues(prev => ({ ...prev, [source]: value }))
    send({ type: 'SetInput', source, value })
  }, [send])

  if (inputNets.length === 0) {
    return (
      <div className="px-3 py-4 text-[11px]" style={{ color: '#334155' }}>
        No input sources detected.<br />
        <span style={{ color: '#1e3a5f' }}>
          (Nets named A0..An appear here)
        </span>
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-2 px-3 py-2">
      <div className="text-[10px] font-bold tracking-wider mb-1" style={{ color: '#475569' }}>INPUT SOURCES</div>
      {inputNets.map(net => {
        const localVal = values[net] ?? 2.5
        const liveVolt = frame?.net_voltages[net]
        return (
          <div
            key={net}
            className="p-2 rounded-lg"
            style={{
              background: '#0f172a',
              border: '1px solid #1e293b',
            }}
          >
            <div className="flex items-center justify-between mb-2">
              <span
                className="text-[11px] font-mono font-bold"
                style={{ color: '#94a3b8', fontFamily: "'JetBrains Mono', monospace" }}
              >
                {net}
              </span>
              <div className="flex items-center gap-2">
                {liveVolt !== undefined && (
                  <span className="text-[10px]" style={{ color: '#475569' }}>
                    live: <span style={{ color: '#60a5fa' }}>{liveVolt.toFixed(3)}V</span>
                  </span>
                )}
                <span
                  className="text-[12px] font-mono font-bold"
                  style={{ color: '#3b82f6', fontFamily: "'JetBrains Mono', monospace" }}
                >
                  {localVal.toFixed(2)}V
                </span>
              </div>
            </div>
            <input
              type="range"
              min={0}
              max={5}
              step={0.01}
              value={localVal}
              onChange={e => handleChange(net, parseFloat(e.target.value))}
              className="w-full h-1.5 rounded cursor-pointer"
              style={{ accentColor: '#3b82f6' }}
            />
            <div className="flex justify-between text-[9px] mt-0.5" style={{ color: '#334155' }}>
              <span>0V</span><span>5V</span>
            </div>
          </div>
        )
      })}
    </div>
  )
}
