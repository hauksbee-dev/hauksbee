import { useEffect, useMemo, useRef, useState } from 'react'
import { CatalogFields } from '../ConstraintEditor'
import {
  badNumbers, countAsserts, looksLikeSupply, measureAs, measureOn, missingRequired, observed as observe,
  offerFromObservation, offersForNet, offersForPart, prefill, questionBlock, requestFor, specFacts, useCatalog,
  useCheckHost, wouldFail,
  type AssertMode, type CatalogKind, type CheckHost, type Draft, type MeasureResult, type Observation,
  type ObservedEditor, type Offer, type Reading,
} from '../../lib/check-catalog'
import type { BoardRequest } from '../../types/report'

// Click-to-assert, in place: the checks that apply to what is selected, each one
// expandable into the exact assertion with sensible values already filled in and
// the quantity it judges read off the board. Adding queues one `[[assert]]` into the
// Checks view's single spec; nothing here keeps a vocabulary of its own, every kind,
// field and default comes from the assertion catalogue.

/** Checks added from the board in this page session, for the toast. */
let addedThisSession = 0

const UNITS: Record<string, string> = {
  voltage: 'V', rail_window: 'V', boot_coverage: 'V', toggle: 'Hz', max_current: 'A',
  pulse_width: 'ms', phase_margin: '°', ac_gain: 'dB',
}

/** Checks that read as nonsense on a supply rail: a rail does not "toggle at
 *  … Hz", have a pulse width, get driven to a level by firmware at reset, or
 *  have a loop gain. Dropped from the suggestions for a rail; the Checks view
 *  can still author any of them. */
const NOT_FOR_RAILS = new Set(['toggle', 'pulse_width', 'boot_coverage', 'phase_margin', 'ac_gain'])

const offerKey = (o: Offer) => `${o.kind}|${o.net ?? ''}|${o.ref ?? ''}`

const fmt = (x: number) => String(Number(x.toPrecision(4)))

/** What the board says today about the quantity a draft judges. */
type Preview =
  | { state: 'none' }
  | { state: 'pending' }
  | { state: 'error'; message: string }
  | { state: 'ready'; reading: Reading; source: 'live' | 'simulated' | 'observed'; unit: string; origin?: string; note?: string }

function usePreview(host: CheckHost | null, offer: Offer, draft: Draft, live: number | null, skip = false): Preview {
  // An editor opened from an observation already holds the reading it judges.
  const as = skip ? null : measureAs(offer.kind)
  const isVoltage = as === 'voltage'
  const useLive = isVoltage && live !== null && (offer.net ?? '') === draft.net
  const question = as ? questionBlock(as, draft) : null
  const subjectKnown = !!(as === 'max_current' ? draft.ref : draft.net)
  const [result, setResult] = useState<{ q: string; r: MeasureResult } | null>(null)
  const hostRef = useRef(host)
  hostRef.current = host
  const ready = !!host?.board
  useEffect(() => {
    if (!question || !subjectKnown || useLive || !ready) return
    let alive = true
    const asked = measureOn(hostRef.current, question)
    asked?.then(r => { if (alive) setResult({ q: question, r }) }).catch(() => {
      if (alive) setResult({ q: question, r: { ok: false, error: 'the measurement request failed' } })
    })
    return () => { alive = false }
  }, [question, subjectKnown, useLive, ready])

  if (!as || !subjectKnown) return { state: 'none' }
  if (useLive) return { state: 'ready', reading: { value: live, min: live, max: live }, source: 'live', unit: 'V' }
  if (!ready) return { state: 'none' }
  if (!result || result.q !== question) return { state: 'pending' }
  const r = result.r
  if (!r.ok) return { state: 'error', message: r.error }
  return {
    state: 'ready', source: 'simulated', unit: r.unit ?? UNITS[as] ?? '',
    reading: { value: r.value, min: r.min, max: r.max },
  }
}

function PreviewStrip({ preview, verdict, kind }: { preview: Preview; verdict: boolean | null; kind: string }) {
  if (preview.state === 'none') return null
  return (
    <div
      data-testid="preview-strip" aria-live="polite"
      className="rounded-md px-2.5 py-1.5 text-[11px]"
      style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)', color: 'var(--silk-dim)' }}
    >
      {preview.state === 'pending' && <span data-testid="preview-pending">measuring…</span>}
      {preview.state === 'error' && (
        <span data-testid="preview-error">Could not read it from the board: {preview.message}</span>
      )}
      {preview.state === 'ready' && (
        <>
          <span>{preview.origin ?? (preview.source === 'live' ? 'Right now' : 'In the simulation')}: </span>
          {preview.note !== undefined
            ? <span data-testid="preview-note">{preview.note}</span>
            : preview.reading.value !== null
            ? (
              <>
                <b className="tnum" style={{ color: 'var(--silk)' }}>
                  <span data-testid="preview-value">{fmt(preview.reading.value)}</span>
                  {preview.unit && <span data-testid="preview-unit"> {preview.unit}</span>}
                </b>
                {preview.reading.min !== null && preview.reading.max !== null
                  && fmt(preview.reading.max - preview.reading.min) !== '0'
                  && <span className="tnum"> (ranges {fmt(preview.reading.min)} to {fmt(preview.reading.max)})</span>}
              </>
            )
            : <span>no value in this run</span>}
          {verdict === true && (
            <b data-testid="preview-would-fail" className="ml-2" style={{ color: 'var(--err-strong)' }}>
              would FAIL today
            </b>
          )}
          {verdict === false && <span className="ml-2" data-testid="preview-would-pass">would pass today</span>}
          {kind === 'rail_window' && <div className="mt-0.5" style={{ color: 'var(--silk-faint)' }}>The scenario runs the whole window; this is the rail at rest.</div>}
        </>
      )}
    </div>
  )
}

/** One expanded offer: fields, the preview and the add buttons. */
export function OfferEditor({ offer, host, live, onAdd, onCollapse, defaultNets, observed }: {
  offer: Offer
  host: CheckHost | null
  /** Opened from something already on screen (a trace, a printed line): the reading
   *  it judges is that observation, not a fresh measurement. */
  observed?: ObservedEditor
  /** Volts of the offer's net in a live frame, when there is one. */
  live: number | null
  onAdd: (request: BoardRequest, run: boolean) => void
  onCollapse: () => void
  defaultNets?: readonly string[]
}) {
  const entry: CatalogKind = offer.entry
  const [edited, setEdited] = useState<Draft>({})
  const [attempted, setAttempted] = useState(false)
  const subject = useMemo(() => ({ net: offer.net, ref: offer.ref }), [offer.net, offer.ref])
  const ratings = offer.ref ? host?.ratings[offer.ref] : undefined
  const nominal = useMemo(() => {
    if (!offer.net) return null
    return host?.supplies.find(s => s.net === offer.net)?.volts ?? null
  }, [host?.supplies, offer.net])

  // The first pass fills from what the name and the spec state; the measurement
  // then refines any field the user has not touched.
  const measured = usePreview(host, offer, {
    net: edited.net ?? offer.net ?? '', ref: edited.ref ?? offer.ref ?? '',
    after_ms: edited.after_ms ?? '', level: edited.level ?? '',
  }, live, observed !== undefined)
  const preview: Preview = observed
    ? { state: 'ready', source: 'observed', reading: observed.reading, unit: observed.unit, origin: observed.origin, note: observed.note }
    : measured
  const reading = preview.state === 'ready' ? preview.reading : null
  // What pre-fills the measured fields: the observation when asked to assert it,
  // nothing when the user is saying what they expected.
  const fillFrom = observed ? (observed.fillReading ? observed.reading : null) : reading
  const hasReading = fillFrom !== null
  const [rValue, rMin, rMax] = [fillFrom?.value ?? null, fillFrom?.min ?? null, fillFrom?.max ?? null]
  const seed = observed?.seed
  const { toml: specToml, hasFirmware } = specFacts(host)
  const auto = useMemo(
    () => prefill(entry, subject, {
      reading: !hasReading ? null : { value: rValue, min: rMin, max: rMax }, nominal, ratings,
      origin: observed ? observed.origin.replace('In this', 'this') : undefined,
    }, { toml: specToml, hasFirmware }),
    // `reading` is rebuilt every render; its three numbers are what matter.
    [entry, subject, hasReading, rValue, rMin, rMax, nominal, ratings, specToml, hasFirmware, observed],
  )
  // A seeded count replaces the rate it stands in for ("freq_hz or min_toggles, not both").
  const seeded = seed?.values ?? {}
  const base: Draft = { ...auto.values, ...seeded }
  if (seeded.min_toggles) base.freq_hz = ''
  const values: Draft = { ...base, ...edited }
  const notes = Object.fromEntries(
    Object.entries({ ...auto.notes, ...(seed?.notes ?? {}) }).filter(([k]) => !(k in edited)),
  )
  const verdict = reading ? wouldFail(offer.kind, values, reading) : null

  const missing = missingRequired(entry, values)
  const bad = badNumbers(entry, values)
  const problems = [...missing.map(f => `${f.label} is needed`), ...bad.map(f => `${f.label} must be a number`)]
  const invalid = attempted ? [...missing, ...bad].map(f => f.key) : []
  const root = useRef<HTMLDivElement>(null)
  useEffect(() => { root.current?.querySelector<HTMLElement>('input, select')?.focus() }, [])

  const add = (run: boolean) => {
    setAttempted(true)
    if (problems.length > 0) return
    onAdd(requestFor(entry, values), run)
  }
  return (
    <div
      ref={root} data-testid="check-editor" data-kind={entry.kind}
      className="flex flex-col gap-2 rounded-md px-2.5 py-2.5"
      style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}
      onKeyDown={event => {
        if (event.key === 'Escape') { event.stopPropagation(); onCollapse() }
        else if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) { event.preventDefault(); add(false) }
      }}
    >
      <div className="text-[11px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
        {entry.summary}
      </div>
      <CatalogFields
        entry={entry} draft={values} notes={notes} invalid={invalid} nets={defaultNets}
        onChange={(key, value) => setEdited(previous => ({ ...previous, [key]: value }))}
      />
      <PreviewStrip preview={preview} verdict={verdict} kind={entry.kind} />
      {attempted && problems.length > 0 && (
        <div data-testid="editor-problems" role="alert" className="text-[11px]" style={{ color: 'var(--err-strong)' }}>
          {problems.join(' · ')}
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <button type="button" data-testid="editor-add" className="hb-btn-primary hb-press px-3 py-1.5 text-[12px]" onClick={() => add(false)}>
          Add check
        </button>
        <button type="button" data-testid="editor-add-run" className="hb-btn hb-press px-3 py-1.5 text-[12px]" onClick={() => add(true)}>
          Add and run
        </button>
        <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }}>Ctrl+Enter adds, Esc closes</span>
      </div>
    </div>
  )
}

/** Queue a request, and for "Add and run", press Run once the spec has the new check. */
export function useAdder(host: CheckHost | null, fallback?: (request: BoardRequest) => void) {
  const hostRef = useRef(host)
  hostRef.current = host
  return (request: BoardRequest, run: boolean, kind?: string) => {
    const before = countAsserts(hostRef.current?.specToml ?? '')
    if (host) host.queue(request)
    else fallback?.(request)
    if (!run) return
    host?.openChecks()
    const stop = Date.now() + 8000
    const timer = window.setInterval(() => {
      const spec = hostRef.current?.specToml ?? ''
      const button = document.querySelector<HTMLButtonElement>('[data-testid="run-checks"]')
      const landed = countAsserts(spec) > before && (!kind || spec.includes(`kind = "${kind}"`))
      if (landed && button && !button.disabled) { window.clearInterval(timer); button.click() }
      else if (Date.now() > stop) window.clearInterval(timer)
    }, 150)
  }
}

/** Carry out the one-click fix of a blocked offer: the firmware picker, or the
 *  block the missing prerequisite names. */
function applyFix(offer: Offer, adder: ReturnType<typeof useAdder>) {
  const f = offer.blocked?.fix
  if (!f) return
  if (f.pickFirmware) document.querySelector<HTMLInputElement>('#firmware-file')?.click()
  else if (f.toml) adder({ type: 'assert', kind: offer.blocked!.need!.need, toml: f.toml }, false)
}

function BlockedOffer({ offer, onFix }: { offer: Offer; onFix: () => void }) {
  const blocked = offer.blocked!
  return (
    <div
      data-testid={`offer-${offer.kind}`} data-net={offer.net} data-blocked="true" aria-disabled="true"
      className="rounded-md px-2.5 py-1.5 text-[12px]"
      style={{ border: '1px dashed var(--hairline)', color: 'var(--silk-faint)' }}
    >
      <div>{offer.text}</div>
      <div className="mt-0.5 flex flex-wrap items-center gap-2 text-[11px]">
        <span data-testid="blocked-reason">{blocked.reason.replace(/`/g, '')}</span>
        {blocked.fix && (
          <button type="button" data-testid="needs_fix" className="hb-btn hb-press px-2 py-0.5 text-[11px]" onClick={onFix}>
            {/* The catalogue's label can be TOML syntax ("Add [ac]"); a reader gets a plain verb. */}
            {/[[\]=]/.test(blocked.fix.label) ? 'Set it up' : blocked.fix.label}
          </button>
        )}
      </div>
    </div>
  )
}

/**
 * The checks for the current selection. Renders nothing until the catalogue has
 * loaded (a server without one has no offers to make).
 */
export function CheckThis({
  net, component, boundKind, capabilities, live, padNet, onQueue, openOffer,
  heading = 'Check this', max, hideBlocked = false, primaryFirst = false,
}: {
  net: string | null
  component: { ref: string; padNets?: string[] } | null
  boundKind?: string | null
  capabilities: readonly string[]
  /** Volts of `net` in the current frame. */
  live: number | null
  padNet?: string | null
  onQueue?: (request: BoardRequest) => void
  /** Open the offer of this kind once (a new `seq` asks again); how the result panel's "Add as check" lands here. */
  openOffer?: { kind: string; seq: number } | null
  /** The section's title. */
  heading?: string
  /** Suggestions shown up front; the rest sit under "More checks". Absent: all. */
  max?: number
  /** Leave out checks that cannot run yet (a live view has no use for "load
   *  firmware first" tiles). */
  hideBlocked?: boolean
  /** Give the first available suggestion the one primary-button look. */
  primaryFirst?: boolean
}) {
  const host = useCheckHost()
  const kinds = useCatalog()
  const [open, setOpen] = useState<string | null>(null)
  const [added, setAdded] = useState(0)
  const [more, setMore] = useState(false)
  const buttons = useRef(new Map<string, HTMLButtonElement | null>())
  const adder = useAdder(host, onQueue)
  const offers = useMemo(() => {
    if (!kinds) return []
    const ctx = {
      kinds, spec: { toml: host?.specToml ?? '', hasFirmware: !!host?.firmware && /^\s*firmware\s*=/m.test(host?.specToml ?? '') },
      supplyNets: new Set((host?.supplies ?? []).map(s => s.net)), capabilities, boundKind: boundKind ?? null,
    }
    if (net) {
      const rail = ctx.supplyNets.has(net) || looksLikeSupply(net)
      return offersForNet(net, ctx).filter(o => !(rail && NOT_FOR_RAILS.has(o.kind)))
    }
    if (component) return offersForPart(component.ref, component.padNets ?? [], ctx, 3, padNet ?? undefined)
    return []
  }, [kinds, host?.specToml, host?.firmware, host?.supplies, capabilities, boundKind, net, component, padNet])

  // A new selection starts with every offer collapsed.
  const subject = `${net ?? ''}|${component?.ref ?? ''}`
  useEffect(() => { setOpen(null) }, [subject])
  const openedSeq = useRef(0)
  useEffect(() => {
    if (!openOffer || openOffer.seq === openedSeq.current || offers.length === 0) return
    const hit = offers.find(o => o.kind === openOffer.kind && !o.blocked)
    openedSeq.current = openOffer.seq
    if (hit) setOpen(offerKey(hit))
  }, [openOffer, offers])

  const available = hideBlocked ? offers.filter(o => !o.blocked) : offers
  const front = max === undefined ? available : available.filter(o => !o.blocked).slice(0, max)
  // Two suggestions behind a "More checks (2)" fold, with nothing above it, is
  // a disclosure that hides everything the card has to say: show them.
  const showAll = front.length === 0 && available.length > 0 && available.length <= 2
  const rest = showAll ? [] : available.filter(o => !front.includes(o))
  const restHoldsOpen = rest.some(o => offerKey(o) === open)
  if (!kinds || available.length === 0) return null
  const fix = (offer: Offer) => applyFix(offer, adder)

  const row = (offer: Offer, primary: boolean) => {
    const key = offerKey(offer)
    if (offer.blocked) return <BlockedOffer key={key} offer={offer} onFix={() => fix(offer)} />
    const isOpen = open === key
    return (
      <div key={key} className="flex flex-col gap-1.5">
        <button
          type="button"
          ref={el => { buttons.current.set(key, el) }}
          data-testid={`offer-${offer.kind}`} data-net={offer.net} data-ref={offer.ref}
          aria-expanded={isOpen}
          className={`${primary ? 'hb-btn-primary' : 'hb-btn'} hb-press px-2.5 py-1.5 text-[12px] text-left`}
          onClick={() => setOpen(isOpen ? null : key)}
        >
          {isOpen ? '▾ ' : '+ '}{offer.text}
        </button>
        {isOpen && (
          <OfferEditor
            offer={offer} host={host} defaultNets={host?.nets}
            live={offer.net === net ? live : null}
            onCollapse={() => { setOpen(null); requestAnimationFrame(() => buttons.current.get(key)?.focus()) }}
            onAdd={(request, run) => {
              adder(request, run, offer.kind)
              addedThisSession += 1
              setAdded(addedThisSession)
              setOpen(null)
              requestAnimationFrame(() => buttons.current.get(key)?.focus())
            }}
          />
        )}
      </div>
    )
  }

  return (
    <div data-testid="check-this" tabIndex={-1} className="flex flex-col gap-1.5 outline-none">
      <div className="text-[10px] font-bold tracking-[0.14em] uppercase" style={{ color: 'var(--silk-faint)' }}>{heading}</div>
      {(showAll ? available : front).map((offer, i) => row(offer, primaryFirst && i === 0 && !offer.blocked))}
      {rest.length > 0 && (
        <details
          data-testid="check-this-more" open={more || restHoldsOpen}
          onToggle={event => setMore(event.currentTarget.open)}
          className="flex flex-col gap-1.5"
        >
          <summary className="cursor-pointer text-[11px]" style={{ color: 'var(--silk-dim)' }}>
            More checks ({rest.length})
          </summary>
          <div className="mt-1.5 flex flex-col gap-1.5">{rest.map(offer => row(offer, false))}</div>
        </details>
      )}
      {added > 0 && (
        <div data-testid="added-toast" role="status" className="text-[11px]" style={{ color: 'var(--ok)' }}>
          Added to Checks ({added}){' '}
          {host && (
            <button type="button" data-testid="added-toast-open" className="underline cursor-pointer" style={{ color: 'var(--copper-hi)', background: 'none', border: 'none', padding: 0 }}
              onClick={() => host.openChecks()}>
              Open Checks
            </button>
          )}
        </div>
      )}
    </div>
  )
}

/**
 * "Assert this" / "Assert what I expected" for something the user is already
 * looking at: a scope trace or a printed UART line.
 *
 * "Assert this" opens the check that says the board keeps doing what it just
 * did, with the observed number or line filled in and the catalogue's own
 * tolerance. "Assert what I expected" opens the same check with the numbers left
 * for the user, and the observation beside them, so a wrong expectation reads
 * "would FAIL today" before it is added. Adding queues one `[[assert]]` into the
 * Checks view's single spec, like every other entry point.
 *
 * `observation` may be a function: it is read when a button is pressed, so a
 * scope can hand over its live buffer without re-rendering on every sample.
 * Renders nothing where there is no Checks view to add to (the standalone demo).
 */
export function AssertThis({ observation, compact = false, testIdSuffix = '' }: {
  observation: Observation | (() => Observation | null)
  /** One small line of two links, for a row in a list. */
  compact?: boolean
  /** Keeps test ids unique where several are on screen at once. */
  testIdSuffix?: string
}) {
  const host = useCheckHost()
  const kinds = useCatalog()
  const adder = useAdder(host)
  const [asked, setAsked] = useState<{ obs: Observation; mode: AssertMode } | null>(null)
  const [added, setAdded] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const spec = specFacts(host)
  const shown = useMemo(() => {
    if (!asked || !kinds) return null
    const offer = offerFromObservation(asked.obs, asked.mode, kinds, spec)
    const seen = observe(asked.obs, asked.mode)
    return offer && seen ? { offer, seen } : null
    // `spec` is rebuilt each render; its two inputs are what matter.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [asked, kinds, spec.toml, spec.hasFirmware])
  if (!host || !kinds) return null

  const ask = (mode: AssertMode) => {
    const obs = typeof observation === 'function' ? observation() : observation
    setAdded(false)
    setAsked(obs ? { obs, mode } : null)
  }
  const close = () => setAsked(null)
  const link = (mode: AssertMode, label: string) => (
    <button
      type="button" data-testid={`${mode === 'as_seen' ? 'assert-this' : 'assert-expected'}${testIdSuffix}`}
      aria-expanded={asked?.mode === mode}
      className="hb-press cursor-pointer underline text-[10px]"
      style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)' }}
      onClick={() => (asked?.mode === mode ? close() : ask(mode))}
    >
      {label}
    </button>
  )
  return (
    <div ref={root} data-testid={`assert-this-box${testIdSuffix}`} className={compact ? 'flex flex-col gap-1' : 'flex flex-col gap-1.5'}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5">
        {link('as_seen', 'Assert this')}
        {link('expected', 'Assert what I expected')}
      </div>
      {asked && shown === null && (
        <div data-testid="assert-this-empty" role="status" className="text-[11px]" style={{ color: 'var(--silk-dim)' }}>
          {asked.obs.kind === 'trace'
            ? 'There is not enough of this trace yet to say what it does. Let it run a little and try again.'
            : 'The firmware has not printed anything yet, so there is nothing to assert as seen. "Assert what I expected" lets you write the text yourself.'}
        </div>
      )}
      {shown && (
        <div className="flex flex-col gap-1.5">
          <div data-testid="assert-seen" className="text-[11px]" style={{ color: 'var(--silk-dim)' }}>
            {asked?.mode === 'expected' ? 'Seen: ' : 'Seen and asserted: '}{shown.seen.seen}
          </div>
          {shown.offer.blocked
            ? <BlockedOffer offer={shown.offer} onFix={() => applyFix(shown.offer, adder)} />
            : (
              <OfferEditor
                key={`${asked?.mode}|${shown.offer.kind}|${shown.offer.net ?? ''}`}
                offer={shown.offer} host={host} defaultNets={host.nets} live={null}
                observed={shown.seen}
                onCollapse={close}
                onAdd={(request, run) => {
                  adder(request, run, shown.offer.kind)
                  addedThisSession += 1
                  setAdded(true)
                  close()
                }}
              />
            )}
        </div>
      )}
      {added && !asked && (
        <div data-testid="assert-added" role="status" className="text-[11px]" style={{ color: 'var(--ok)' }}>
          Added to Checks.{' '}
          <button type="button" data-testid="assert-added-open" className="underline cursor-pointer"
            style={{ color: 'var(--copper-hi)', background: 'none', border: 'none', padding: 0 }}
            onClick={() => host.openChecks()}>
            Open Checks
          </button>
        </div>
      )}
    </div>
  )
}
