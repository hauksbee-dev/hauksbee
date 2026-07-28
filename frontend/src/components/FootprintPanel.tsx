import { useState } from 'react'
import { CloseIcon } from './Icons'

interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
  /** Net of the clicked pad, when one was in reach. */
  padNet?: string | null
}

interface FootprintPanelProps {
  info: FootprintInfo | null
  onClose: () => void
  /** The engine-bound model kind for this ref ("mcu", "bjt_npn", ...), or
   *  null/undefined when the part bound to nothing (open on the circuit). */
  boundKind?: string | null
  /** Queue a check into the report page's checks builder. Absent on surfaces
   *  without a builder (the standalone demo server). */
  onAssert?: (check: { kind: string; net?: string; ref?: string }) => void
}

export function FootprintPanel({ info, onClose, boundKind, onAssert }: FootprintPanelProps) {
  // Which assertion was just queued, for the "added" confirmation flash.
  const [queued, setQueued] = useState<string | null>(null)
  if (!info) return null

  const assert = (kind: string, check: { net?: string; ref?: string }) => {
    onAssert?.({ kind, ...check })
    setQueued(kind)
    setTimeout(() => setQueued(null), 1800)
  }

  return (
    <div
      className="flex flex-col gap-2 p-3 rounded-lg"
      style={{
        background: '#0f172a',
        border: '1px solid #1e293b',
        minWidth: 220,
        maxWidth: 300,
      }}
    >
      <div className="flex items-center justify-between">
        <span className="text-[10px] font-bold tracking-wider" style={{ color: '#64748b' }}>
          FOOTPRINT
        </span>
        <button
          onClick={onClose}
          className="text-[10px] hover:opacity-70"
          style={{ color: '#475569', display: 'inline-flex' }}
        >
          <CloseIcon size={12} />
        </button>
      </div>

      <div className="flex items-baseline gap-2">
        <span className="text-lg font-bold" style={{ color: '#e2e8f0' }}>{info.ref}</span>
        <span className="text-sm" style={{ color: '#94a3b8' }}>{info.value}</span>
      </div>

      {/* What the engine actually simulates for this part; honesty first. */}
      <div className="text-[10px]" data-testid="footprint-model" style={{ color: '#64748b' }}>
        {boundKind
          ? <>bound model: <span style={{ color: '#94a3b8', fontFamily: "'JetBrains Mono', monospace" }}>{boundKind}</span></>
          : 'no bound model (open on the live circuit)'}
      </div>

      <div className="text-[10px]" style={{ color: '#475569', wordBreak: 'break-all' }}>
        {info.lib_id}
      </div>

      <div className="flex gap-4 text-[10px]" style={{ color: '#64748b' }}>
        <span>x: <span style={{ color: '#94a3b8' }}>{info.x.toFixed(2)} mm</span></span>
        <span>y: <span style={{ color: '#94a3b8' }}>{info.y.toFixed(2)} mm</span></span>
      </div>

      {/* Component-shaped assertions, queued into the report's checks builder
          exactly like hand-built rows. */}
      {onAssert && (
        <div className="flex flex-col gap-1.5 mt-1">
          <button
            type="button"
            data-testid="sim-assert-current"
            onClick={() => assert('max_current', { ref: info.ref })}
            className="px-2.5 py-1.5 rounded text-[11px] text-left cursor-pointer"
            style={{ background: 'rgba(224,138,78,0.12)', border: '1px solid #7c4a1e', color: '#ffb072' }}
          >
            {queued === 'max_current' ? 'Added to the checks builder ✓' : '+ must stay under a current'}
          </button>
          <button
            type="button"
            data-testid="sim-assert-temp"
            onClick={() => assert('max_temp', { ref: info.ref })}
            className="px-2.5 py-1.5 rounded text-[11px] text-left cursor-pointer"
            style={{ background: 'rgba(224,138,78,0.12)', border: '1px solid #7c4a1e', color: '#ffb072' }}
          >
            {queued === 'max_temp' ? 'Added to the checks builder ✓' : '+ must stay cool'}
          </button>
          {info.padNet && (
            <button
              type="button"
              data-testid="sim-assert-padnet"
              onClick={() => assert('voltage', { net: info.padNet! })}
              className="px-2.5 py-1.5 rounded text-[11px] text-left cursor-pointer"
              style={{ background: 'rgba(224,138,78,0.12)', border: '1px solid #7c4a1e', color: '#ffb072' }}
            >
              {queued === 'voltage' ? 'Added to the checks builder ✓' : `+ net ${info.padNet} must sit at a voltage`}
            </button>
          )}
          <div className="text-[9px]" style={{ color: '#475569' }}>
            lands in the checks builder on the report page
          </div>
        </div>
      )}
    </div>
  )
}
