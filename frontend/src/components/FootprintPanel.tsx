interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
}

interface FootprintPanelProps {
  info: FootprintInfo | null
  onClose: () => void
}

export function FootprintPanel({ info, onClose }: FootprintPanelProps) {
  if (!info) return null

  return (
    <div
      className="flex flex-col gap-2 p-3 rounded-lg"
      style={{
        background: '#0f172a',
        border: '1px solid #1e293b',
        minWidth: 220,
      }}
    >
      <div className="flex items-center justify-between">
        <span className="text-[10px] font-bold tracking-wider" style={{ color: '#64748b' }}>
          FOOTPRINT
        </span>
        <button
          onClick={onClose}
          className="text-[10px] hover:opacity-70"
          style={{ color: '#475569' }}
        >
          ✕
        </button>
      </div>

      <div className="flex items-baseline gap-2">
        <span className="text-lg font-bold" style={{ color: '#e2e8f0' }}>{info.ref}</span>
        <span className="text-sm" style={{ color: '#94a3b8' }}>{info.value}</span>
      </div>

      <div className="text-[10px]" style={{ color: '#475569', wordBreak: 'break-all' }}>
        {info.lib_id}
      </div>

      <div className="flex gap-4 text-[10px]" style={{ color: '#64748b' }}>
        <span>x: <span style={{ color: '#94a3b8' }}>{info.x.toFixed(2)} mm</span></span>
        <span>y: <span style={{ color: '#94a3b8' }}>{info.y.toFixed(2)} mm</span></span>
      </div>
    </div>
  )
}
