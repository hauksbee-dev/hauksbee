import { CloseIcon } from './Icons'
import { displayNet } from '../lib/net-name'
import { explainFor, labelFor } from '../lib/labels'
import { netReadoutText, type NetReading } from '../lib/net-state'
import type { ModelCoverageComponent, BoardRequest, WebOpenPart, WebOpenSelection } from '../types/report'
import { Callout } from './ui'
import { CheckThis } from './checks/CheckThis'

// The unified selection card: what a click on the board surface reports, in
// the same card language on the report map and the live sim. A net shows its
// name (and live volts when a sim frame is in hand); a component shows refdes,
// value, its BOUND model (what the engine actually simulates; honesty first),
// and the nets on its pads. Both offer the checks that apply (CheckThis, from
// the assertion catalogue) and edit them in place; the card never decides to
// navigate or silently append a check itself.

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

/** Pick the orderable variant a part really is: each option names the member,
 *  its orderable codes and the datasheet page that says so. A pick becomes an
 *  `[[override]] select` in the checks spec. */
export function VariantPicker({ selection: s, onQueue, refs = [s.reference] }: {
  selection: WebOpenSelection
  onQueue: (request: BoardRequest) => void
  /** Every part this one pick settles (a group of identical open parts). */
  refs?: string[]
}) {
  return (
    <select
      className="hb-input text-[12px]"
      style={{ maxWidth: '100%' }}
      data-testid={refs.length > 1 ? 'variant-picker-group' : `variant-picker-${s.reference}`}
      aria-label={`${refs.join(', ')} variant`}
      value=""
      onChange={e => refs.forEach(ref => onQueue({ type: 'select', ref, select: e.target.value }))}
    >
      <option value="" disabled>Pick the fitted {memberName(s.family)} variant{refs.length > 1 ? ` for all ${refs.length}` : ''}…</option>
      {s.members.map(m => (
        <option key={m.id} value={m.id} disabled={m.verdict === 'C'}>
          {memberName(m.id)}: {m.selector || 'no orderable codes'} · {m.basis}{m.verdict === 'C' ? ` (ruled out: ${m.reason})` : ''}
        </option>
      ))}
    </select>
  )
}

/** A variant as a reader names it: a part number stays as written, an internal
 *  family id (`led_generic_corner_high_vf`) goes through the label table. */
const memberName = (id: string) => (id.includes('_') ? labelFor(id) : id)

/** Stand-in wording, shared by the banner and the card. */
const selectionState = (s: WebOpenSelection) => s.same_pinout
  ? `simulated as ${memberName(s.members.find(m => m.verdict !== 'C')?.id ?? '?')}; CI checks every variant`
  : 'selection required: left open until you pick one'

/** Parts that share a family, stand-in state and candidate list are one
 *  question, asked once. */
function groupSelections(selections: WebOpenSelection[]): WebOpenSelection[][] {
  const groups = new Map<string, WebOpenSelection[]>()
  for (const s of selections) {
    const key = `${s.family}|${s.same_pinout}|${s.members.map(m => m.id).join(',')}`
    groups.set(key, [...(groups.get(key) ?? []), s])
  }
  return [...groups.values()]
}

export function OpenSelectionsBanner({ selections, onQueue }: {
  selections: WebOpenSelection[]
  onQueue?: (request: BoardRequest) => void
}) {
  if (selections.length === 0) return null
  const row = (s: WebOpenSelection) => (
    <div key={s.reference} className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1">
      <b>{s.reference}</b> <span style={{ fontFamily: 'var(--font-mono)' }}>{s.value}</span>
      <span style={{ color: 'var(--silk-dim)' }}>{selectionState(s)}</span>
      {onQueue && <VariantPicker selection={s} onQueue={onQueue} />}
    </div>
  )
  return (
    <Callout tone="warn" testId="open-selections" className="mt-3" title="Variant not pinned by the board">
      {groupSelections(selections).map(group => {
        if (group.length === 1) return row(group[0])
        const [first] = group
        const refs = group.map(s => s.reference)
        return (
          <div key={refs.join()} data-testid="open-selection-group" className="mt-1">
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
              <b>{group.length} {first.value} parts</b> ({refs.join(', ')})
              <span style={{ color: 'var(--silk-dim)' }}>{selectionState(first)}</span>
              {onQueue && <VariantPicker selection={first} onQueue={onQueue} refs={refs} />}
            </div>
            <details className="mt-1 text-[12px]">
              <summary className="cursor-pointer" style={{ color: 'var(--silk-dim)' }}>Pick one by one</summary>
              {group.map(row)}
            </details>
          </div>
        )
      })}
    </Callout>
  )
}

/** A short list of plain labels (capability ids through the label table), the
 *  first few inline with their one-line explanation as a tooltip and the rest one
 *  click away. Never prints a raw id. */
function PlainList({ ids, testId, limit = 3 }: { ids: readonly string[]; testId?: string; limit?: number }) {
  const shown = ids.slice(0, limit)
  const rest = ids.slice(limit)
  const item = (id: string) => (
    <span key={id} title={explainFor(id)} style={explainFor(id) ? { textDecoration: 'underline dotted', textDecorationColor: 'var(--rule)', cursor: 'help' } : undefined}>
      {labelFor(id)}
    </span>
  )
  return (
    <span data-testid={testId}>
      {shown.map((id, i) => <span key={id}>{i > 0 && ', '}{item(id)}</span>)}
      {rest.length > 0 && (
        <details className="inline">
          <summary className="inline cursor-pointer" style={{ color: 'var(--silk-faint)' }}> and {rest.length} more</summary>
          <span> {rest.map((id, i) => <span key={id}>{i > 0 && ', '}{item(id)}</span>)}</span>
        </details>
      )}
    </span>
  )
}

/** Coverage stages whose label alone does not tell a reader what the part's
 *  results are worth ("Scope not stated"): the card prints the label table's
 *  explanation under the headline. */
const PLAIN_STAGES: ReadonlySet<string> = new Set(['executable_scope_unspecified', 'identity_only'])

/** "gap" or "gaps": the count of things a model still does not cover. */
const gaps = (n: number) => `${n} ${n === 1 ? 'gap' : 'gaps'}`

/** Scroll the report to where it names `name` (a part or a net), and flash it.
 *  The report's findings, conditions and coverage rows carry the same result
 *  links as everything else (`data-testid="result-link"`), so the card needs no
 *  copy of the findings: it points at them. */
function mentionsOf(name: string): HTMLElement[] {
  if (typeof document === 'undefined') return []
  const report = document.querySelector('[data-testid="report"]')
  if (!report) return []
  const escaped = typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(name) : name.replace(/"/g, '\\"')
  return [...report.querySelectorAll<HTMLElement>(`[data-testid="result-link"][data-name="${escaped}"]`)]
    .filter(el => !el.closest('[data-testid="selection-card"]'))
}

function showMention(name: string) {
  const el = mentionsOf(name)[0]
  if (!el) return
  el.scrollIntoView({ block: 'center', behavior: 'smooth' })
  const before = el.style.outline
  el.style.outline = '2px solid var(--copper-hi)'
  window.setTimeout(() => { el.style.outline = before }, 1600)
}

export function SelectionCard({
  net, liveVolts, reading, component, boundKind, modelCoverage, netModels = [], assertionCapabilities = [],
  onQueue, openOffer = null, peripheralMode = 'scenario', onAddProbe, onAuthorModel, onClose, onPickNet, openSelection, docked = false,
  liveSim, embedded = false,
}: {
  /** Shown INSIDE another card (the live net card's "Add a check" disclosure):
   *  no frame, no title row, no net name or reading and no probe button, all of
   *  which the host card already has. What is left is what only this card adds:
   *  the devices on the net, the checks and the drive controls. */
  embedded?: boolean
  /** In a dock (a rail or a block under the map) rather than floating over it:
   *  fills the dock's width and may grow taller. */
  docked?: boolean
  /** The selected part's open variant selection, when the board leaves one. */
  openSelection?: WebOpenSelection
  /** Force the live-simulation card (a net shows its live value and trace, not
   *  the static check card). Left out, it is inferred from what the live view
   *  already passes: a probe handler, a reading, or live-and-scenario mode. */
  liveSim?: boolean

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
  /** Hand a check, an interaction, a supply or a register-map device to the
   *  checks builder (see `BoardRequest`). Absent where there is no builder
   *  (the standalone demo), which hides every such offer. */
  onQueue?: (request: BoardRequest) => void
  /** A check kind to open in the CheckThis dock (the result panel's "Add as check"). */
  openOffer?: { kind: string; seq: number } | null
  /** Say whether the action only prepares a replayable scenario or also
   *  mutates the connected live solver immediately. */
  peripheralMode?: 'scenario' | 'live-and-scenario'
  /** Add the selected copper to the live scope immediately. Only supplied by
   *  the live view; the report view has no running wire to probe. */
  onAddProbe?: (net: string) => void
  /** Open the deterministic local model editor for the selected component. */
  onAuthorModel?: (component: ModelCoverageComponent | WebOpenPart) => void
  onClose: () => void
  /** Jump the selection to one of the part's pad nets. */
  onPickNet?: (net: string) => void
}) {
  if (!net && !component) return null

  const live = liveSim ?? (!!onAddProbe || reading !== undefined || peripheralMode === 'live-and-scenario')
  const needsRegisterMapWork = modelCoverage?.missing?.some(item => /i2c|spi|register|bus/i.test(item)) ?? false
  const modelOwnsRegisterMap = modelCoverage?.implements?.some(item => /register[_-]map/i.test(item)) ?? false
  const subject = net ?? component?.ref ?? ''
  const hasMention = mentionsOf(subject).length > 0
  // The one primary action of a part card is the model, when it has a gap and
  // the model flow is reachable; otherwise the first check is.
  const noModel = !!component && (modelCoverage ? modelCoverage.stage === 'unresolved' : !boundKind)
  // A part with no model at all always offers one, even when the report carries
  // no coverage row for it (then the editor is opened from the part itself).
  const modelPrimary = !!(component && !modelCoverage?.rename_fix && onAuthorModel
    && (modelCoverage?.actionable_behavior_gap || noModel))
  const measuredNow = reading?.kind === 'measured'
  const readingNote = reading?.kind === 'unsupported'
    ? 'This net is held by a regulator whose input headroom was never established; the number is that ideal source’s setpoint, not a simulated value.'
    : reading?.kind === 'unobserved'
      ? 'This backend cannot see whether the MCU drives this pin; the level is the passive network idling, not a measurement.'
      : reading?.kind === 'measured' && reading.moving
        ? 'The net moved over the observed span; this is its excursion, not one sampled instant.'
        : undefined

  const findingsButton = hasMention && (
    <button
      type="button"
      data-testid="selection-related-findings"
      onClick={() => showMention(subject)}
      className="hb-btn hb-press px-2.5 py-1.5 text-[12px] text-left"
    >
      Show findings that name {displayNet(subject)}
    </button>
  )

  return (
    // maxHeight 100% + an internal scroll: the card is anchored inside a strip
    // that starts BELOW the viewer toolbar (see BoardView / SimView), so it can
    // never grow up under the Fit control and hide its own title.
    // A part with fifty nets scrolls; the identity row and the close button
    // stay pinned to the top of the card, always reachable.
    <div
      data-testid="selection-card"
      data-mode={net && live ? 'live-net' : net ? 'net' : 'part'}
      className={embedded ? 'flex flex-col' : 'hb-card flex flex-col'}
      style={embedded ? { width: '100%' } : docked
        // Same height as the map beside it, so the dock never leaves a hole
        // under the map or runs past it.
        ? { width: '100%', maxHeight: 'clamp(420px, 52vh, 620px)', boxShadow: 'var(--shadow-card)' }
        : { minWidth: 230, maxWidth: 320, maxHeight: '100%', boxShadow: 'var(--shadow-pop)' }}
    >
      {!embedded && (
      <div className="shrink-0 flex flex-col gap-2 px-3 pt-3 pb-2">
        <div className="flex items-center justify-between">
          <span className="text-[10px] font-bold tracking-[0.14em]" style={{ color: 'var(--silk-faint)' }}>
            {net ? (live ? 'NET · LIVE' : 'NET') : 'PART'}
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
          <div className="flex flex-col gap-0.5">
            <div className="flex items-baseline gap-2">
              <span className="text-[14px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>
                {displayNet(net)}
              </span>
              {reading
                ? reading.kind !== 'absent' && (
                  <span
                    className={live ? 'text-[15px] tnum shrink-0 font-semibold' : 'text-[12px] tnum shrink-0'}
                    data-testid="selection-reading"
                    title={readingNote}
                    style={{
                      color: measuredNow ? 'var(--copper)' : 'var(--silk-faint)',
                      fontFamily: 'var(--font-mono)',
                      fontStyle: measuredNow ? undefined : 'italic',
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
            {live && reading && (
              <span className="text-[11px]" data-testid="selection-reading-note" style={{ color: 'var(--silk-dim)' }}>
                {reading.kind === 'measured'
                  ? reading.moving ? 'Swinging between these levels while the sim runs.' : 'Right now, from the running simulation.'
                  : readingNote ?? 'No value for this net in the current frame.'}
              </span>
            )}
          </div>
        ) : component ? (
          <div className="flex flex-col gap-0.5">
            <div className="flex items-baseline gap-2">
              <span className="text-[17px] font-bold" style={{ color: 'var(--silk)' }}>{component.ref}</span>
              {component.value && <span className="text-[13px]" style={{ color: 'var(--silk-dim)' }}>{component.value}</span>}
            </div>
            {boundKind && (
              <span className="text-[11px]" data-testid="selection-kind" style={{ color: 'var(--silk-dim)' }}>
                {labelFor(boundKind)}
              </span>
            )}
          </div>
        ) : null}
      </div>
      )}

      <div
        data-testid="selection-card-body"
        className={embedded ? 'flex flex-col gap-2.5' : 'flex flex-col gap-2.5 overflow-y-auto px-3 pb-3'}
        style={{ minHeight: 0 }}
      >
      {component ? (
        <>
          <div
            data-testid="selection-model-coverage"
            className="rounded-md px-2.5 py-2 text-[11px] leading-relaxed"
            style={{
              border: `1px solid ${noModel || modelCoverage?.actionable_behavior_gap ? 'var(--warn-border)' : 'var(--ok-border)'}`,
              background: noModel || modelCoverage?.actionable_behavior_gap ? 'var(--warn-bg)' : 'var(--ok-bg)',
              color: 'var(--silk-dim)',
            }}
          >
            {modelCoverage ? (
              <>
                <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5" data-testid="selection-model">
                  <b
                    title={explainFor(modelCoverage.stage)}
                    style={{ color: noModel || modelCoverage.actionable_behavior_gap ? 'var(--warn-strong)' : 'var(--ok)' }}
                  >
                    {labelFor(modelCoverage.stage)}
                  </b>
                  {(modelCoverage.missing?.length ?? 0) > 0 && <span>· {gaps(modelCoverage.missing!.length)}</span>}
                </div>
                {/* A stage whose name is jargon says what it means for the
                    reader, in the label table's own sentence, not only in a
                    hover. */}
                {!noModel && PLAIN_STAGES.has(modelCoverage.stage) && explainFor(modelCoverage.stage) && (
                  <div className="mt-0.5" data-testid="selection-model-meaning">{explainFor(modelCoverage.stage)}</div>
                )}
                {!noModel && (
                  <div className="mt-0.5" data-testid="selection-model-source">
                    {[labelFor(modelCoverage.source.tier), labelFor(modelCoverage.source.validation)].filter(Boolean).join(' · ')}
                  </div>
                )}
                {noModel && (
                  <div className="mt-0.5">{explainFor('open_part') ?? 'Left out of the simulated circuit.'}</div>
                )}
                {(modelCoverage.implements?.length ?? 0) > 0 && (
                  <div className="mt-1">
                    <b>Covers:</b> <PlainList ids={modelCoverage.implements!} />
                  </div>
                )}
                {(modelCoverage.missing?.length ?? 0) > 0 && (
                  <div className="mt-1">
                    <b>Not yet covered:</b> <PlainList ids={modelCoverage.missing!} testId="selection-model-missing" />
                  </div>
                )}
              </>
            ) : (
              <span data-testid="selection-model">
                {boundKind
                  ? <><b style={{ color: 'var(--ok)' }}>Modelled</b>{' · '}{labelFor(boundKind)}</>
                  : <><b style={{ color: 'var(--warn-strong)' }}>No model</b>{' · left out of the simulated circuit'}</>}
              </span>
            )}
          </div>

          {openSelection && (
            <div className="flex flex-col gap-1 text-[11px]" data-testid="selection-variant" style={{ color: 'var(--warn-strong)' }}>
              {selectionState(openSelection)}
              {onQueue && <VariantPicker selection={openSelection} onQueue={onQueue} />}
            </div>
          )}

          {modelPrimary && (
            <button
              type="button"
              data-testid="selection-author-model"
              onClick={() => onAuthorModel!(modelCoverage ?? {
                reference: component.ref, value: component.value, reason: '', consequence: '', active_ic: false, bound: false,
              })}
              className="hb-btn-primary hb-press px-2.5 py-1.5 text-[12px] text-left"
            >
              {noModel ? `Give ${component.ref} a model` : `Extend ${component.ref}'s model`}
            </button>
          )}

          {onQueue && needsRegisterMapWork && !modelOwnsRegisterMap && (
            <button
              type="button"
              data-testid="selection-add-sensor"
              onClick={() => onQueue({ type: 'sensor', id: component.ref, ref: component.ref, modelId: modelCoverage?.model_id })}
              className="hb-btn hb-press px-2.5 py-1.5 text-[12px] text-left"
            >
              Simulate its registers with a behavior builder
            </button>
          )}

          {needsRegisterMapWork && modelOwnsRegisterMap && (
            <div
              data-testid="selection-register-map-owned"
              className="rounded-md px-2.5 py-2 text-[11px] leading-relaxed"
              style={{ color: 'var(--silk-dim)', border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}
            >
              This part already auto-attaches model-owned register behavior. Extend
              its model to add the missing registers; adding a second device at the
              same address would not represent this board.
            </div>
          )}
        </>
      ) : null}

      {net && netModels.length > 0 && (
        <div
          data-testid="selection-net-models"
          className="rounded-md px-2.5 py-2 text-[11px] leading-relaxed"
          style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)', color: 'var(--silk-dim)' }}
        >
          <b style={{ color: 'var(--silk)' }}>
            {netModels.length === 1 ? '1 device on this net' : `${netModels.length} devices on this net`}
          </b>
          {netModels.slice(0, live ? 3 : 6).map(model => (
            <div key={model.reference} className="mt-0.5">
              <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--silk)' }}>{model.reference}</span>{' '}
              · {labelFor(model.stage)}
              {(model.missing?.length ?? 0) > 0 && <> · {gaps(model.missing!.length)}</>}
            </div>
          ))}
          {netModels.length > (live ? 3 : 6) && (
            <div className="mt-0.5" style={{ color: 'var(--silk-faint)' }}>and {netModels.length - (live ? 3 : 6)} more</div>
          )}
        </div>
      )}

      {/* A live net's one primary action is the probe; a static net's is its first check. */}
      {net && onAddProbe && !embedded && (
        <button
          type="button"
          data-testid="selection-add-probe"
          onClick={() => onAddProbe(net)}
          className="hb-btn-primary hb-press px-2.5 py-1.5 text-[12px] text-left"
        >
          Show {displayNet(net)} on the scope
        </button>
      )}

      {onQueue && (
        <CheckThis
          net={net} component={component} boundKind={boundKind} capabilities={assertionCapabilities}
          live={reading ? (reading.kind === 'measured' ? reading.volts : null) : liveVolts ?? null}
          padNet={component?.padNet} onQueue={onQueue} openOffer={openOffer}
          heading={net ? 'Check this net' : 'Check this part'}
          max={3} hideBlocked={live} primaryFirst={!live && !modelPrimary && !(net && onAddProbe)}
        />
      )}

      {net && onQueue && (
        <details data-testid="selection-drive" className="flex flex-col gap-1.5">
          <summary className="cursor-pointer text-[12px]" style={{ color: 'var(--silk-dim)' }}>
            {live ? 'Drive it now' : 'Drive it'}
          </summary>
          <div className="mt-1.5 flex flex-col gap-1.5">
            <button
              type="button"
              data-testid="selection-add-stimulus"
              onClick={() => onQueue({ type: 'peripheral', kind: 'stimulus', net })}
              className="hb-btn hb-press px-2.5 py-1.5 text-[12px] text-left"
            >
              {live ? 'Apply a signal now and save it as a test' : 'Apply a signal to this net in a test'}
            </button>
            <button
              type="button"
              data-testid="selection-add-button"
              onClick={() => onQueue({ type: 'peripheral', kind: 'pushbutton', net })}
              className="hb-btn hb-press px-2.5 py-1.5 text-[12px] text-left"
            >
              {live ? 'Attach a pushbutton now and save it' : 'Attach a pushbutton to this net'}
            </button>
            <button
              type="button"
              data-testid="selection-add-supply"
              onClick={() => onQueue({ type: 'supply', net, volts: 3.3 })}
              className="hb-btn hb-press px-2.5 py-1.5 text-[12px] text-left"
            >
              {live ? 'Power this net at 3.3 V now and save the supply' : 'Power this net at 3.3 V in a test'}
            </button>
          </div>
        </details>
      )}

      {findingsButton}

      {component && (component.padNets?.length ?? 0) > 0 && (
        // Every net, not the first eight: the card scrolls, so "+44 more"
        // with no way to reach them was a dead end.
        <div className="text-[11px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
          on {component.padNets!.length} {component.padNets!.length === 1 ? 'net' : 'nets'}:{' '}
          {component.padNets!.map((n, i) => (
            <span key={n}>
              {i > 0 && ', '}
              {onPickNet ? (
                <button
                  type="button"
                  onClick={() => {
                    onPickNet(n)
                    // The card is rebuilt for the net: keep the keyboard in it.
                    window.setTimeout(() => document.querySelector<HTMLElement>('[data-testid="check-this"]')?.focus(), 60)
                  }}
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

      {component && (component.lib_id || modelCoverage?.model_id || (modelCoverage?.references?.length ?? 0) > 0) && (
        <details data-testid="selection-technical" className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
          <summary className="cursor-pointer">Technical details</summary>
          <dl className="mt-1 flex flex-col gap-0.5" style={{ wordBreak: 'break-all' }}>
            {modelCoverage?.model_id && <div>Model: <code style={{ fontFamily: 'var(--font-mono)' }}>{modelCoverage.model_id}</code></div>}
            {modelCoverage?.footprint && <div>Footprint: <code style={{ fontFamily: 'var(--font-mono)' }}>{modelCoverage.footprint}</code></div>}
            {component.lib_id && <div>Library part: <code style={{ fontFamily: 'var(--font-mono)' }}>{component.lib_id}</code></div>}
            {modelCoverage?.layer && <div>Model from: {labelFor(modelCoverage.layer)}</div>}
            {(modelCoverage?.references?.length ?? 0) > 0 && (() => {
              const reference = modelCoverage!.references![0]
              const safeUrl = /^https?:\/\//i.test(reference.url) ? reference.url : null
              return (
                <div>
                  Source: {safeUrl ? (
                    <a href={safeUrl} target="_blank" rel="noreferrer" style={{ color: 'var(--copper-hi)' }}>
                      {reference.title}
                    </a>
                  ) : reference.title}
                  {reference.sha256 ? ' · hash pinned' : ' · hash not pinned'}
                </div>
              )
            })()}
          </dl>
        </details>
      )}

      </div>
    </div>
  )
}
