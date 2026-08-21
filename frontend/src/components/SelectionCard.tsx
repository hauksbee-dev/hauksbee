import { useEffect, useState } from 'react'
import { CloseIcon } from './Icons'
import { displayNet } from '../lib/net-name'
import { netReadoutText, type NetReading } from '../lib/net-state'
import type { ModelCoverageComponent } from '../types/report'

// The unified selection card: what a click on the board surface reports, in
// the same card language on the report map and the live sim. A net shows its
// name (and live volts when a sim frame is in hand); a component shows refdes,
// value, its BOUND model (what the engine actually simulates; honesty first),
// and the nets on its pads. Both offer assertion starters through the same
// shared in-place editor; the selection card never decides to navigate or
// silently append a check itself.

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

export interface AssertOffer {
  kind: string
  label: string
  net?: string
  ref?: string
}

export function assertionOffers(
  net: string | null,
  component: SelectedComponent | null,
  assertionCapabilities: string[] = [],
): AssertOffer[] {
  if (net) {
    return [
      { kind: 'voltage', label: 'must sit at a voltage', net },
      { kind: 'rail_window', label: 'must stay inside a voltage window', net },
      { kind: 'toggle', label: 'must blink', net },
      { kind: 'boot-coverage', label: 'firmware must drive it by a deadline', net },
    ]
  }
  if (!component) return []
  const supported = new Set(assertionCapabilities)
  return [
    ...(supported.has('max_current')
      ? [{ kind: 'max_current', label: 'must stay under a current', ref: component.ref }]
      : []),
    ...(supported.has('max_temp')
      ? [{ kind: 'max_temp', label: 'must stay cool', ref: component.ref }]
      : []),
    ...(component.padNet
      ? [{ kind: 'voltage', label: `net ${displayNet(component.padNet)} must sit at a voltage`, net: component.padNet }]
      : []),
  ]
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
      {queued ? 'Constraint editor open ✓' : `+ ${offer.label}`}
    </button>
  )
}

export function SelectionCard({
  net, liveVolts, reading, component, boundKind, modelCoverage, netModels = [], assertionCapabilities = [],
  onQueueCheck, onQueuePeripheral, onQueueSensor, onQueueSupply, peripheralMode = 'scenario', onAddProbe, onAuthorModel, onClose, onPickNet,
}: {
  /** Selected net, when the click landed on copper. */
  net: string | null
  /** Live voltage for the net, when a sim frame carries it. */
  /** Raw volts, for the report map which has no live frame semantics. */
  liveVolts?: number
  /** The live sim's fuller answer: driven, moving, or unobservable. Preferred
   *  over `liveVolts` when present, because "0.000 V" alone cannot say which
   *  of those three a net is (see lib/net-state.ts). */
  reading?: NetReading
  /** Selected component, when the click landed on a part. */
  component: SelectedComponent | null
  /** Engine-bound model kind for the component ("mcu", "bjt_npn", ...). */
  boundKind?: string | null
  /** Winning model plus its declared executable scope. Unlike `boundKind`,
   *  this distinguishes an identity card from a complete behavioural model. */
  modelCoverage?: ModelCoverageComponent | null
  /** Exact component assertion kinds emitted by the bound circuit. Missing
   *  data fails closed: unsupported current/temperature checks are not shown. */
  assertionCapabilities?: string[]
  /** Modelled devices touching a selected trace/net. */
  netModels?: ModelCoverageComponent[]
  onQueueCheck?: (check: { kind: string; net?: string; ref?: string }) => void
  /** Add a physical interaction to the visual co-sim builder. This changes the
   *  experiment and is intentionally presented separately from assertions. */
  onQueuePeripheral?: (peripheral: { id?: string; kind: 'stimulus' | 'pushbutton' | 'toggle'; net?: string; ref?: string }) => void
  /** Start a validated register-map bus-device scenario for this component.
   * The follow-on builder requires explicit spec bytes; clicking never guesses
   * a datasheet protocol from a part name. */
  onQueueSensor?: (sensor: { id: string; ref?: string; modelId?: string | null }) => void
  onQueueSupply?: (supply: { net: string; volts?: number }) => void
  /** Say whether the action only prepares a replayable scenario or also
   *  mutates the connected live solver immediately. */
  peripheralMode?: 'scenario' | 'live-and-scenario'
  /** Add the selected copper to the live scope immediately. Only supplied by
   *  the live view; the report view has no running wire to probe. */
  onAddProbe?: (net: string) => void
  /** Open the deterministic local model editor for the selected component. */
  onAuthorModel?: (component: ModelCoverageComponent) => void
  onClose: () => void
  /** Jump the selection to one of the part's pad nets. */
  onPickNet?: (net: string) => void
}) {
  if (!net && !component) return null

  const offers = assertionOffers(net, component, assertionCapabilities)
  const needsRegisterMapWork = modelCoverage?.missing?.some(item => /i2c|spi|register|bus/i.test(item)) ?? false
  const modelOwnsRegisterMap = modelCoverage?.implements?.some(item => /register[_-]map/i.test(item)) ?? false

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
            {reading
              ? reading.kind !== 'absent' && (
                <span
                  className="text-[12px] tnum shrink-0"
                  data-testid="selection-reading"
                  title={reading.kind === 'unobserved'
                    ? 'This backend cannot see whether the MCU drives this pin; the level is the passive network idling, not a measurement.'
                    : reading.moving
                      ? 'The net moved over the observed span; this is its excursion, not one sampled instant.'
                      : undefined}
                  style={{
                    color: reading.kind === 'unobserved' ? 'var(--silk-faint)' : 'var(--copper)',
                    fontFamily: 'var(--font-mono)',
                    fontStyle: reading.kind === 'unobserved' ? 'italic' : undefined,
                  }}
                >
                  {netReadoutText(reading)}
                </span>
              )
              : liveVolts !== undefined && (
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
          {modelCoverage && (
            <div
              data-testid="selection-model-coverage"
              className="rounded-md px-2.5 py-2 text-[10px] leading-relaxed"
              style={{
                border: `1px solid ${modelCoverage.actionable_behavior_gap ? 'var(--warn-border)' : 'var(--ok-border)'}`,
                background: modelCoverage.actionable_behavior_gap ? 'var(--warn-bg)' : 'var(--ok-bg)',
                color: 'var(--silk-dim)',
              }}
            >
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                <b style={{ color: modelCoverage.actionable_behavior_gap ? 'var(--warn-strong)' : 'var(--ok)' }}>
                  {modelCoverage.stage.replaceAll('_', ' ')}
                </b>
                {modelCoverage.model_id && (
                  <code style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{modelCoverage.model_id}</code>
                )}
                <span>{modelCoverage.source.validation}</span>
              </div>
              {(modelCoverage.implements?.length ?? 0) > 0 && (
                <div className="mt-1">
                  <b>Runs:</b> {modelCoverage.implements!.join(', ')}
                </div>
              )}
              {(modelCoverage.missing?.length ?? 0) > 0 && (
                <div className="mt-1" data-testid="selection-model-missing">
                  <b>Still missing:</b> {modelCoverage.missing!.join(', ')}
                </div>
              )}
              {(modelCoverage.references?.length ?? 0) > 0 && (() => {
                const reference = modelCoverage.references![0]
                const safeUrl = /^https?:\/\//i.test(reference.url) ? reference.url : null
                return (
                  <div className="mt-1">
                    Source: {safeUrl ? (
                      <a href={safeUrl} target="_blank" rel="noreferrer" style={{ color: 'var(--copper-hi)' }}>
                        {reference.title}
                      </a>
                    ) : reference.title}
                    {reference.sha256 ? ' · hash pinned' : ' · hash not pinned'}
                  </div>
                )
              })()}
            </div>
          )}
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

      {net && netModels.length > 0 && (
        <div
          data-testid="selection-net-models"
          className="rounded-md px-2.5 py-2 text-[10px] leading-relaxed"
          style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)', color: 'var(--silk-dim)' }}
        >
          <b style={{ color: 'var(--silk)' }}>Devices on this trace</b>
          {netModels.map(model => (
            <div key={model.reference} className="mt-1">
              <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--copper-hi)' }}>{model.reference}</span>{' '}
              · {model.stage.replaceAll('_', ' ')}
              {(model.missing?.length ?? 0) > 0 ? ` · missing ${model.missing!.join(', ')}` : ''}
            </div>
          ))}
        </div>
      )}

      {net && onAddProbe && (
        <button
          type="button"
          data-testid="selection-add-probe"
          onClick={() => onAddProbe(net)}
          className="hb-btn-primary hb-press px-2.5 py-1.5 text-[12px] text-left"
        >
          + Watch this trace live
        </button>
      )}

      {net && onQueuePeripheral && (
        <div className="flex flex-col gap-1.5">
          <button
            type="button"
            data-testid="selection-add-stimulus"
            onClick={() => onQueuePeripheral({ kind: 'stimulus', net })}
            className="hb-chip hb-press px-2.5 py-1.5 text-[12px] text-left"
          >
            {peripheralMode === 'live-and-scenario'
              ? '+ Drive this trace now and save the interaction'
              : '+ Drive this trace in a co-sim scenario'}
          </button>
          <button
            type="button"
            data-testid="selection-add-button"
            onClick={() => onQueuePeripheral({ kind: 'pushbutton', net })}
            className="hb-chip hb-press px-2.5 py-1.5 text-[12px] text-left"
          >
            {peripheralMode === 'live-and-scenario'
              ? '+ Attach a pushbutton now and save it'
              : '+ Attach a pushbutton to this trace'}
          </button>
        </div>
      )}

      {net && onQueueSupply && (
        <button
          type="button"
          data-testid="selection-add-supply"
          onClick={() => onQueueSupply({ net, volts: 3.3 })}
          className="hb-chip hb-press px-2.5 py-1.5 text-[12px] text-left"
        >
          {peripheralMode === 'live-and-scenario'
            ? '+ Power this trace now at 3.3 V and save the supply'
            : '+ Use this trace as a 3.3 V scenario supply'}
        </button>
      )}

      {component && modelCoverage?.actionable_behavior_gap && onAuthorModel && (
        <button
          type="button"
          data-testid="selection-author-model"
          onClick={() => onAuthorModel(modelCoverage)}
          className="hb-btn-primary hb-press px-2.5 py-1.5 text-[12px] text-left"
        >
          Extend {component.ref}'s model
        </button>
      )}

      {component && onQueueSensor && needsRegisterMapWork && !modelOwnsRegisterMap && (
        <button
          type="button"
          data-testid="selection-add-sensor"
          onClick={() => onQueueSensor({ id: component.ref, ref: component.ref, modelId: modelCoverage?.model_id })}
          className="hb-chip hb-press px-2.5 py-1.5 text-[12px] text-left"
        >
          + Open its register-map behavior builder
        </button>
      )}

      {component && needsRegisterMapWork && modelOwnsRegisterMap && (
        <div
          data-testid="selection-register-map-owned"
          className="rounded-md px-2.5 py-2 text-[10px] leading-relaxed"
          style={{ color: 'var(--silk-dim)', border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}
        >
          This part already auto-attaches model-owned register behavior. Extend
          its model to add the missing registers; adding a second device at the
          same address would not represent this board.
        </div>
      )}

      {onQueueCheck && offers.length > 0 && (
        <div className="flex flex-col gap-1.5 mt-0.5">
          {offers.map(o => <OfferButton key={o.kind + (o.net ?? '')} offer={o} onQueue={onQueueCheck} />)}
          <div className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>
            opens the shared constraint editor
          </div>
        </div>
      )}
      </div>
    </div>
  )
}
