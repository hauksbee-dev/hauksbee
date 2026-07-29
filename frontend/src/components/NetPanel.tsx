import type { SimFrame } from '../types/protocol'
import { displayNet } from '../lib/net-name'
import { readNet, type Envelopes, type EnvelopeSource } from '../lib/net-state'

// Live net list for the sim rail's "Net voltages" card. The card header is
// owned by the rail; this renders only the rows.
//
// A row says three things now, because a bare instant said too little: the
// sampled level, how far the net MOVED over the observed span, and whether the
// backend could observe it at all. A bit-banged bus whose strobe lands between
// samples used to read as a flat rail and sort to the bottom next to the dead
// nets; it now shows its excursion and sorts by it.

interface NetPanelProps {
  frame: SimFrame | null
  selectedNet: string | null
  onSelectNet: (net: string | null) => void
  /** Fallback min/max across retained frames, used when the wire carries no
   *  intra-chunk extremes. See lib/net-state.ts. */
  envelopes?: Envelopes
  /** Where those extremes came from, so the footnote can be honest. */
  envelopeSource?: EnvelopeSource
}

// Rows rendered at once. A 3,000-net board rebuilt thousands of DOM rows on
// every sim frame; the cap keeps the rows that carry information.
const MAX_ROWS = 150

const FULL_SCALE_V = 5

export function NetPanel({
  frame, selectedNet, onSelectNet, envelopes, envelopeSource = 'none',
}: NetPanelProps) {
  const voltages = frame?.net_voltages ?? {}
  const names = Object.keys(voltages)

  // Read every net once, then rank by what a reader is actually hunting for:
  // a net that MOVED beats a net that merely sits high, because movement is
  // the thing that was invisible. Unobserved nets sink below both: they carry
  // no measurement to rank on.
  const rows = names.map(name => ({ name, r: readNet(frame, name, envelopes) }))
  rows.sort((a, b) => {
    const rank = (x: typeof a) => {
      if (x.r.kind === 'unobserved') return -1
      if (x.r.kind !== 'measured') return -2
      const swing = x.r.max - x.r.min
      // Any real excursion outranks any static level.
      return x.r.moving ? 1000 + swing : Math.abs(x.r.volts)
    }
    return rank(b) - rank(a)
  })

  const entries = rows.slice(0, MAX_ROWS)
  if (selectedNet && voltages[selectedNet] !== undefined && !entries.some(e => e.name === selectedNet)) {
    entries.push({ name: selectedNet, r: readNet(frame, selectedNet, envelopes) })
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
        {entries.map(({ name, r }) => {
          const isSelected = name === selectedNet
          const unobserved = r.kind === 'unobserved'
          const volts = r.kind === 'measured' ? r.volts : 0
          const barColor = unobserved ? 'var(--silk-faint)'
            : volts > 4 ? 'var(--warn)'
            : volts > 0.1 ? 'var(--ok)'
            : volts < -0.1 ? 'var(--err)'
            : 'var(--silk-faint)'

          // The track carries the excursion as a band and the sampled instant
          // as a tick inside it. On a static net the two coincide and it reads
          // exactly like the old single bar.
          const pct = (v: number) => Math.max(0, Math.min(100, (Math.abs(v) / FULL_SCALE_V) * 100))
          const lo = r.kind === 'measured' ? pct(r.min) : 0
          const hi = r.kind === 'measured' ? pct(r.max) : 0
          const bandLeft = Math.min(lo, hi)
          const bandWidth = Math.max(Math.abs(hi - lo), 0)
          const moving = r.kind === 'measured' && r.moving

          return (
            <div
              key={name}
              onClick={() => onSelectNet(isSelected ? null : name)}
              data-testid={moving ? 'net-row-moving' : undefined}
              className="flex items-center gap-2 px-3 py-1.5 cursor-pointer"
              style={{
                background: isSelected ? 'var(--copper-tint)' : 'transparent',
                borderLeft: isSelected ? '2px solid var(--copper)' : '2px solid transparent',
                opacity: unobserved ? 0.72 : 1,
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
                  <span
                    className="text-[10px] ml-2 shrink-0 tnum"
                    title={unobserved
                      ? 'This backend cannot see whether the MCU is driving this pin. The level shown elsewhere is the passive network idling, not a measurement.'
                      : moving
                        ? 'This net moved over the observed span; the range is its excursion, not one sampled instant.'
                        : undefined}
                    style={{
                      color: unobserved ? 'var(--silk-faint)'
                        : isSelected ? 'var(--copper)'
                        : moving ? 'var(--copper-hi)' : barColor,
                      fontFamily: 'var(--font-mono)',
                      fontStyle: unobserved ? 'italic' : undefined,
                    }}
                  >
                    {unobserved
                      ? 'not observed'
                      : moving
                        ? `${r.min.toFixed(2)}–${r.max.toFixed(2)}V`
                        : `${volts >= 0 ? '+' : ''}${volts.toFixed(3)}V`}
                  </span>
                </div>
                <div
                  className="h-0.5 rounded-full overflow-hidden relative"
                  style={{
                    background: 'var(--surface-3)',
                    // An unobserved net gets a dashed, hollow track: there is
                    // no measurement to fill it with.
                    ...(unobserved ? { border: '1px dashed var(--rule)', height: 3 } : null),
                  }}
                >
                  {!unobserved && (
                    <>
                      {/* the excursion band */}
                      {moving && (
                        <div
                          className="h-full absolute rounded-full"
                          style={{
                            left: `${bandLeft}%`,
                            width: `${bandWidth}%`,
                            background: 'var(--copper)',
                            opacity: 0.55,
                          }}
                        />
                      )}
                      {/* the sampled instant */}
                      <div
                        className="h-full absolute rounded-full"
                        style={{
                          left: 0,
                          width: moving ? '2px' : `${pct(volts)}%`,
                          marginLeft: moving ? `${pct(volts)}%` : undefined,
                          background: moving ? 'var(--copper-hi)' : barColor,
                          transition: moving ? undefined : 'width 0.15s linear',
                        }}
                      />
                    </>
                  )}
                </div>
              </div>
            </div>
          )
        })}
        {rows.length > entries.length && (
          <div className="px-3 py-1.5 text-[9px]" style={{ color: 'var(--silk-faint)' }}>
            showing top {entries.length} of {rows.length} nets, moving nets first
          </div>
        )}
        {envelopeSource === 'frames' && (
          // Never let a range imply more resolution than it has: this one is
          // built from the frames that reached the browser, so a pulse shorter
          // than a chunk is still not in it.
          <div className="px-3 py-1.5 text-[9px] leading-snug" style={{ color: 'var(--silk-faint)' }}>
            ranges span the frames received; a pulse shorter than one sim chunk
            is not captured
          </div>
        )}
      </div>
    </div>
  )
}
