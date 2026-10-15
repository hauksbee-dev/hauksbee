import { useEffect, useMemo, useRef, useState } from 'react'
import { CatalogFields } from '../ConstraintEditor'
import {
  badNumbers, countAsserts, measureAs, measureOn, missingRequired, offersForNet, offersForPart,
  prefill, questionBlock, requestFor, specFacts, useCatalog, useCheckHost, wouldFail,
  type CatalogKind, type CheckHost, type Draft, type MeasureResult, type Offer, type Reading,
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

const offerKey = (o: Offer) => `${o.kind}|${o.net ?? ''}|${o.ref ?? ''}`

const fmt = (x: number) => String(Number(x.toPrecision(4)))

/** What the board says today about the quantity a draft judges. */
type Preview =
  | { state: 'none' }
  | { state: 'pending' }
  | { state: 'error'; message: string }
  | { state: 'ready'; reading: Reading; source: 'live' | 'simulated'; unit: string }

function usePreview(host: CheckHost | null, offer: Offer, draft: Draft, live: number | null): Preview {
  const as = measureAs(offer.kind)
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
          <span>{preview.source === 'live' ? 'Right now' : 'In the simulation'}: </span>
          {preview.reading.value !== null
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
export function OfferEditor({ offer, host, live, onAdd, onCollapse, defaultNets }: {
  offer: Offer
  host: CheckHost | null
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
  const preview = usePreview(host, offer, {
    net: edited.net ?? offer.net ?? '', ref: edited.ref ?? offer.ref ?? '',
    after_ms: edited.after_ms ?? '', level: edited.level ?? '',
  }, live)
  const reading = preview.state === 'ready' ? preview.reading : null
  const hasReading = reading !== null
  const [rValue, rMin, rMax] = [reading?.value ?? null, reading?.min ?? null, reading?.max ?? null]
  const { toml: specToml, hasFirmware } = specFacts(host)
  const auto = useMemo(
    () => prefill(entry, subject, {
      reading: !hasReading ? null : { value: rValue, min: rMin, max: rMax }, nominal, ratings,
    }, { toml: specToml, hasFirmware }),
    // `reading` is rebuilt every render; its three numbers are what matter.
    [entry, subject, hasReading, rValue, rMin, rMax, nominal, ratings, specToml, hasFirmware],
  )
  const values: Draft = { ...auto.values, ...edited }
  const notes = Object.fromEntries(Object.entries(auto.notes).filter(([k]) => !(k in edited)))
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
        <span data-testid="blocked-reason">{blocked.reason}</span>
        {blocked.fix && (
          <button type="button" data-testid="needs_fix" className="hb-chip hb-press px-2 py-0.5 text-[11px]" onClick={onFix}>
            {blocked.fix.label}
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
export function CheckThis({ net, component, boundKind, capabilities, live, padNet, onQueue }: {
  net: string | null
  component: { ref: string; padNets?: string[] } | null
  boundKind?: string | null
  capabilities: readonly string[]
  /** Volts of `net` in the current frame. */
  live: number | null
  padNet?: string | null
  onQueue?: (request: BoardRequest) => void
}) {
  const host = useCheckHost()
  const kinds = useCatalog()
  const [open, setOpen] = useState<string | null>(null)
  const [added, setAdded] = useState(0)
  const buttons = useRef(new Map<string, HTMLButtonElement | null>())
  const adder = useAdder(host, onQueue)
  const offers = useMemo(() => {
    if (!kinds) return []
    const ctx = {
      kinds, spec: { toml: host?.specToml ?? '', hasFirmware: !!host?.firmware && /^\s*firmware\s*=/m.test(host?.specToml ?? '') },
      supplyNets: new Set((host?.supplies ?? []).map(s => s.net)), capabilities, boundKind: boundKind ?? null,
    }
    if (net) return offersForNet(net, ctx)
    if (component) return offersForPart(component.ref, component.padNets ?? [], ctx, 3, padNet ?? undefined)
    return []
  }, [kinds, host?.specToml, host?.firmware, host?.supplies, capabilities, boundKind, net, component, padNet])

  // A new selection starts with every offer collapsed.
  const subject = `${net ?? ''}|${component?.ref ?? ''}`
  useEffect(() => { setOpen(null) }, [subject])

  if (!kinds || offers.length === 0) return null
  const fix = (offer: Offer) => {
    const f = offer.blocked?.fix
    if (!f) return
    if (f.pickFirmware) document.querySelector<HTMLInputElement>('#firmware-file')?.click()
    else if (f.toml) adder({ type: 'assert', kind: offer.blocked!.need!.need, toml: f.toml }, false)
  }

  return (
    <div data-testid="check-this" tabIndex={-1} className="flex flex-col gap-1.5 outline-none">
      <div className="text-[10px] font-bold tracking-[0.14em]" style={{ color: 'var(--silk-faint)' }}>CHECK THIS</div>
      {offers.map(offer => {
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
              className="hb-chip hb-press px-2.5 py-1.5 text-[12px] text-left"
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
      })}
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
