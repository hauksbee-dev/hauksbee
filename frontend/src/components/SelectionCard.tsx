import { useEffect, useState } from 'react'
import { CloseIcon } from './Icons'
import { displayNet } from '../lib/net-name'

// The unified selection card: what a click on the board surface reports, in
// the same card language on the report map and the live sim. A net shows its
// name (and live volts when a sim frame is in hand); a component shows refdes,
// value, its BOUND model (what the engine actually simulates; honesty first),
// and the nets on its pads. Both offer one-click assertions that land in the
// checks builder as ordinary prefilled rows.

/** A component picked on the board map (the viewer's footprint hit-test). */
export interface SelectedComponent {
  ref: string
  value: string
  lib_id: string
  /** Net of the pad nearest the click, when one was in reach. */
  padNet?: string | null
  /** All distinct nets on this part's pads (the viewer knows the copper). */
  padNets?: string[]
}

interface AssertOffer {
  kind: string
  label: string
  net?: string
  ref?: string
}

function OfferButton({ offer, onQueue }: {
  offer: AssertOffer
  onQueue: (check: { kind: string; net?: string; ref?: string }) => void
}) {
  const [queued, setQueued] = useState(false)
  // The confirmation flash resets on its own; clear the timer if the card
  // unmounts first.
  useEffect(() => {
    if (!queued) return
    const t = setTimeout(() => setQueued(false), 1800)
    return () => clearTimeout(t)
  }, [queued])
  return (
    <button
      type="button"
      data-testid={`assert-${offer.kind}`}
      onClick={() => {
        onQueue({ kind: offer.kind, net: offer.net, ref: offer.ref })
        setQueued(true)
      }}
      className="hb-chip hb-press px-2.5 py-1.5 text-[12px] text-left"
    >
      {queued ? 'Added to the checks builder ✓' : `+ ${offer.label}`}
    </button>
  )
}

export function SelectionCard({
  net, liveVolts, component, boundKind, onQueueCheck, onClose, onPickNet,
}: {
  /** Selected net, when the click landed on copper. */
  net: string | null
  /** Live voltage for the net, when a sim frame carries it. */
  liveVolts?: number
  /** Selected component, when the click landed on a part. */
  component: SelectedComponent | null
  /** Engine-bound model kind for the component ("mcu", "bjt_npn", ...). */
  boundKind?: string | null
  onQueueCheck?: (check: { kind: string; net?: string; ref?: string }) => void
  onClose: () => void
  /** Jump the selection to one of the part's pad nets. */
  onPickNet?: (net: string) => void
}) {
  if (!net && !component) return null

  const offers: AssertOffer[] = net
    ? [
        { kind: 'voltage', label: 'must sit at a voltage', net },
        { kind: 'toggle', label: 'must blink', net },
        { kind: 'boot-coverage', label: 'firmware must drive it by a deadline', net },
      ]
    : component
      ? [
          { kind: 'max_current', label: 'must stay under a current', ref: component.ref },
          { kind: 'max_temp', label: 'must stay cool', ref: component.ref },
          ...(component.padNet
            ? [{ kind: 'voltage', label: `net ${displayNet(component.padNet)} must sit at a voltage`, net: component.padNet }]
            : []),
        ]
      : []

  return (
    // maxHeight 100% + an internal scroll: the card is anchored inside a strip
    // that starts BELOW the viewer toolbar (see BoardView / SimView), so it can
    // never grow up under the 2D/3D and Fit controls and hide its own title.
    // A part with fifty nets scrolls; the identity row and the close button
    // stay pinned to the top of the card, always reachable.
    <div
      data-testid="selection-card"
      className="hb-card flex flex-col"
      style={{ minWidth: 230, maxWidth: 320, maxHeight: '100%', boxShadow: 'var(--shadow-pop)' }}
    >
      <div className="shrink-0 flex flex-col gap-2 px-3 pt-3 pb-2">
        <div className="flex items-center justify-between">
          <span className="text-[10px] font-bold tracking-[0.14em]" style={{ color: 'var(--silk-faint)' }}>
            {net ? 'NET' : 'COMPONENT'}
          </span>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close selection"
            className="hb-press cursor-pointer"
            style={{
              color: 'var(--silk-faint)', display: 'inline-flex', background: 'none',
              border: 'none', padding: 6, margin: -6,
            }}
          >
            <CloseIcon size={13} />
          </button>
        </div>

        {net ? (
          <div className="flex items-baseline gap-2">
            <span className="text-[14px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>
              {displayNet(net)}
            </span>
            {liveVolts !== undefined && (
              <span className="text-[12px] tnum shrink-0" style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}>
                {liveVolts.toFixed(3)} V
              </span>
            )}
          </div>
        ) : component ? (
          <div className="flex items-baseline gap-2">
            <span className="text-[17px] font-bold" style={{ color: 'var(--silk)' }}>{component.ref}</span>
            {component.value && <span className="text-[13px]" style={{ color: 'var(--silk-dim)' }}>{component.value}</span>}
          </div>
        ) : null}
      </div>

      <div
        data-testid="selection-card-body"
        className="flex flex-col gap-2 overflow-y-auto px-3 pb-3"
        style={{ minHeight: 0 }}
      >
      {component ? (
        <>
          <div className="text-[11px]" data-testid="selection-model" style={{ color: 'var(--silk-dim)' }}>
            {boundKind
              ? <>bound model: <span style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}>{boundKind}</span></>
              : 'no bound model (this part is open on the live circuit)'}
          </div>
          {component.lib_id && (
            <div className="text-[10px]" style={{ color: 'var(--silk-faint)', wordBreak: 'break-all' }}>
              {component.lib_id}
            </div>
          )}
          {component.padNets && component.padNets.length > 0 && (
            // Every net, not the first eight: the card scrolls, so "+44 more"
            // with no way to reach them was a dead end.
            <div className="text-[11px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
              on {component.padNets.length} {component.padNets.length === 1 ? 'net' : 'nets'}:{' '}
              {component.padNets.map((n, i) => (
                <span key={n}>
                  {i > 0 && ', '}
                  {onPickNet ? (
                    <button
                      type="button"
                      onClick={() => onPickNet(n)}
                      className="hb-press cursor-pointer"
                      style={{
                        background: 'none', border: 'none', padding: 0,
                        color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)', fontSize: 11,
                        textDecoration: 'underline', textDecorationColor: 'var(--rule)',
                        textAlign: 'left', wordBreak: 'break-all',
                      }}
                    >
                      {displayNet(n)}
                    </button>
                  ) : (
                    <span style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{displayNet(n)}</span>
                  )}
                </span>
              ))}
            </div>
          )}
        </>
      ) : null}

      {onQueueCheck && offers.length > 0 && (
        <div className="flex flex-col gap-1.5 mt-0.5">
          {offers.map(o => <OfferButton key={o.kind + (o.net ?? '')} offer={o} onQueue={onQueueCheck} />)}
          <div className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>
            lands in the checks builder
          </div>
        </div>
      )}
      </div>
    </div>
  )
}
