import { useContext } from 'react'
import type { BenchView, McuStripView, PinChipView, UartChipView } from '../../lib/bench-view'
import { ResultLinkContext } from './ResultLinks'

// The run as a bench engineer sees it: per MCU, one headline (running, parked at
// Error_Handler, restarted twice) and one chip per pin or serial port saying what
// it did. Trouble is drawn as an issue inside the strip, not in a list somewhere
// else. A chip with a net is a result link: it lights that net on the board and
// is the focus of the run view under the strip. A chip without one says why it
// does nothing.

const TONE = {
  ok: { fg: 'var(--ok)', border: 'var(--ok-border)', bg: 'var(--ok-bg)', strong: 'var(--ok)' },
  warn: { fg: 'var(--warn)', border: 'var(--warn-border)', bg: 'var(--warn-bg)', strong: 'var(--warn-strong)' },
  err: { fg: 'var(--err)', border: 'var(--err-border)', bg: 'var(--err-bg)', strong: 'var(--err-strong)' },
} as const

/** A chip button: pin or port name, then what it did. */
function Chip({ name, net, focused, muted, title, onFocus, children }: {
  name: string
  net: string | null
  focused: boolean
  muted?: boolean
  title: string
  onFocus: (net: string) => void
  children: React.ReactNode
}) {
  const { onPoint, onGo } = useContext(ResultLinkContext)
  const linked = net !== null
  const body = (
    <>
      <span style={{ color: 'var(--silk)', fontWeight: 700 }}>{name}</span>
      <span style={{ color: muted ? 'var(--silk-faint)' : 'var(--silk-dim)' }}>{children}</span>
    </>
  )
  const base: React.CSSProperties = {
    fontFamily: 'var(--font-mono)',
    fontSize: 12,
    lineHeight: 1.45,
    padding: '2px 8px',
    borderRadius: 6,
    display: 'inline-flex',
    alignItems: 'baseline',
    gap: 8,
    background: focused ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
    border: `1px solid ${focused ? 'var(--copper)' : 'var(--hairline)'}`,
    opacity: muted ? 0.8 : 1,
  }
  if (!linked) {
    return (
      <span data-testid="bench-chip" data-pin={name} data-linked="false" title={title} style={{ ...base, cursor: 'default' }}>
        {body}
      </span>
    )
  }
  return (
    <button
      type="button"
      data-testid="bench-chip"
      data-pin={name}
      data-net={net}
      data-linked="true"
      aria-pressed={focused}
      title={title}
      onMouseEnter={() => onPoint?.({ kind: 'net', net })}
      onMouseLeave={() => onPoint?.(null)}
      onFocus={() => onPoint?.({ kind: 'net', net })}
      onBlur={() => onPoint?.(null)}
      onClick={() => {
        onFocus(net)
        // The run view is what the reader came to look at, so the page stays put.
        onGo?.({ kind: 'net', net }, { scroll: false })
      }}
      className="hb-press"
      style={{ ...base, color: 'inherit', cursor: 'pointer' }}
    >
      {body}
    </button>
  )
}

function PinChip({ chip, focusNet, onFocus }: { chip: PinChipView; focusNet: string | null; onFocus: (net: string) => void }) {
  return (
    <Chip name={chip.pin} net={chip.net} focused={chip.net !== null && chip.net === focusNet} muted={chip.muted} title={chip.title} onFocus={onFocus}>
      {chip.summary}
    </Chip>
  )
}

function UartRow({ chip, focusNet, onFocus }: { chip: UartChipView; focusNet: string | null; onFocus: (net: string) => void }) {
  return (
    <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1" data-testid="bench-uart">
      <Chip name={chip.port} net={chip.net} focused={chip.net !== null && chip.net === focusNet} title={chip.title} onFocus={onFocus}>
        {chip.baud ? `${chip.baud}, ` : ''}{chip.bytes}
      </Chip>
      {chip.firstLine !== null && (
        <code
          className="truncate min-w-0 max-w-full"
          data-testid="bench-uart-line"
          title={chip.firstLine}
          style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--instrument-text)', background: 'var(--instrument)', border: '1px solid var(--instrument-edge)', borderRadius: 4, padding: '1px 6px' }}
        >
          {chip.firstLine}
        </code>
      )}
      {chip.garbled && (
        <span className="text-[11px]" style={{ color: 'var(--warn-strong)' }}>{chip.garbled}</span>
      )}
    </div>
  )
}

function McuCard({ mcu, focusNet, onFocus }: { mcu: McuStripView; focusNet: string | null; onFocus: (net: string) => void }) {
  const tone = TONE[mcu.tone]
  return (
    <div
      className="rounded-lg px-4 py-3 mb-2"
      data-testid="bench-mcu"
      data-tone={mcu.tone}
      style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
    >
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <span className="text-sm font-bold" style={{ fontFamily: 'var(--font-mono)', color: 'var(--silk)' }}>{mcu.ref}</span>
        <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{mcu.backend}</span>
        <span className="text-[11px] ml-auto" data-testid="bench-facts" style={{ color: 'var(--silk-dim)' }}>
          {mcu.facts.join(' · ')}
        </span>
      </div>

      {mcu.issue ? (
        <div
          className="rounded-md px-3 py-2 mt-2"
          data-testid="bench-issue"
          role="status"
          style={{ border: `1px solid ${tone.border}`, borderLeft: `4px solid ${tone.fg}`, background: tone.bg }}
        >
          <div className="flex flex-wrap items-baseline gap-x-2">
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tone.strong }}>{mcu.coreLabel}</span>
            <span className="text-sm font-semibold" data-testid="bench-headline" style={{ color: 'var(--silk)' }}>{mcu.headline}</span>
          </div>
          {mcu.detail.length > 0 && (
            <div className="text-[11px] mt-0.5" style={{ fontFamily: 'var(--font-mono)', color: 'var(--silk-dim)' }}>
              {mcu.detail.join(' · ')}
            </div>
          )}
        </div>
      ) : (
        <div className="flex items-center gap-2 mt-2">
          <span aria-hidden="true" className="inline-block rounded-full" style={{ width: 8, height: 8, background: tone.fg }} />
          <span className="text-sm font-semibold" data-testid="bench-headline" style={{ color: 'var(--silk)' }}>{mcu.headline}</span>
        </div>
      )}

      {mcu.pins.length > 0 && (
        <div className="flex flex-wrap gap-1.5 mt-2.5" data-testid="bench-pins">
          {mcu.pins.map(c => <PinChip key={c.key} chip={c} focusNet={focusNet} onFocus={onFocus} />)}
        </div>
      )}
      {mcu.uarts.length > 0 && (
        <div className="flex flex-col gap-1.5 mt-2">
          {mcu.uarts.map(c => <UartRow key={c.key} chip={c} focusNet={focusNet} onFocus={onFocus} />)}
        </div>
      )}
    </div>
  )
}

export function BenchStrip({ view, focusNet, onFocus }: {
  view: BenchView
  focusNet: string | null
  onFocus: (net: string) => void
}) {
  return (
    <div data-testid="bench-strip" className="mb-3">
      {view.mcus.map(m => <McuCard key={m.ref} mcu={m} focusNet={focusNet} onFocus={onFocus} />)}
    </div>
  )
}
