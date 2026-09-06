import { useCallback, useEffect, useState } from 'react'
import type { ExtractReady, ModelCard, ModelSaveResult, WebOpenPart } from '../types/report'
import { CheckIcon, WarningIcon } from './Icons'
import { api, errorText } from '../lib/api'
import { BusyLine, Callout, LogWell, TerminalCommand } from './ui'
import { ReviewCard } from './extract/ReviewCard'

// Drafting a device model from a datasheet, offered where the user learns they
// need one: the report's list of parts that could not be bound.
//
// This holds the same consent contract `hauksbee models extract` holds, in the
// same order, and for the same reason: the extraction sends the datasheet's text
// off this machine and the user cannot unsend it.
//
//   1. Whether it can run at all is settled first, from
//      GET /api/models/extract/ready. If codex is missing or not signed in, the
//      blocker and its fix are shown here, and no file picker is ever reached.
//   2. The consent notice (served verbatim by the engine, so this page cannot
//      soften the CLI's wording) needs an explicit click.
//   3. Only then does the datasheet get attached.
//   4. Progress streams from POST /api/models/extract as Server-Sent Events, the
//      same framing the dependency installs use.
//   5. The result is a DRAFT, shown for review with every value the model
//      admitted it assumed called out. Nothing is written until Accept, which is
//      the only thing that calls POST /api/models/save.

/** Where the flow is, for the one part being worked on. */
type Flow =
  | { step: 'consent' }
  | { step: 'attach' }
  | { step: 'running'; log: string[] }
  | { step: 'review'; card: ModelCard }
  | { step: 'saved'; card: ModelCard; result: ModelSaveResult }
  | { step: 'failed'; message: string; log: string[] }

type Ready =
  | { phase: 'loading' }
  | { phase: 'ready'; info: ExtractReady }
  | { phase: 'unavailable'; reason: string }

export function DatasheetExtract({
  openParts,
  onSaved,
}: {
  openParts: WebOpenPart[]
  onSaved?: () => void
}) {
  // Only a part with no model at all can be helped by drafting one. A part that
  // bound and is open on the live circuit has a model already; offering an
  // extraction there would send someone's datasheet to solve a wiring problem.
  const draftable = openParts.filter(p => !p.bound)

  const [ready, setReady] = useState<Ready>({ phase: 'loading' })
  const [active, setActive] = useState<WebOpenPart | null>(null)
  const [flow, setFlow] = useState<Flow>({ step: 'consent' })
  const [part, setPart] = useState('')
  const [kind, setKind] = useState('')
  // Empty means "use the default", which the server names in `default_model`.
  const [model, setModel] = useState('')
  const [file, setFile] = useState<File | null>(null)
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)

  useEffect(() => {
    if (draftable.length === 0) return
    let cancelled = false
    void api.extractReady()
      .then(info => { if (!cancelled) setReady({ phase: 'ready', info }) })
      // An older server, or one started without the tool hooks: say so rather
      // than offering a button that cannot work.
      .catch((e: unknown) => { if (!cancelled) setReady({ phase: 'unavailable', reason: errorText(e) }) })
    return () => { cancelled = true }
  }, [draftable.length])

  const begin = useCallback((p: WebOpenPart) => {
    setActive(p)
    setFlow({ step: 'consent' })
    // The board's value field is usually the manufacturer part number, which is
    // exactly what the extraction needs, but it is also where "TBD" and "DNP"
    // live, so it is a prefill and not an answer.
    setPart(p.value.trim())
    setKind('')
    setFile(null)
    setSaveError(null)
  }, [])

  const close = useCallback(() => {
    setActive(null)
    setFlow({ step: 'consent' })
    setFile(null)
    setSaveError(null)
  }, [])

  const run = useCallback(async () => {
    if (!active || !file) return
    const log: string[] = []
    setFlow({ step: 'running', log: [] })
    const append = (line: string) => {
      log.push(line)
      setFlow({ step: 'running', log: [...log] })
    }
    const form = new FormData()
    form.append('datasheet', file, file.name)
    form.append('part', part)
    form.append('kind', kind)
    form.append('model', model)
    form.append('reference', active.reference)
    try {
      let settled = false
      await api.extract(form, ({ event, data }) => {
        if (event === 'log') append(data)
        else if (event === 'card') {
          try {
            settled = true
            setFlow({ step: 'review', card: JSON.parse(data) as ModelCard })
          } catch {
            setFlow({ step: 'failed', message: 'the server sent a model card this page could not read', log })
          }
        } else if (event === 'error') {
          settled = true
          setFlow({ step: 'failed', message: data, log })
        }
      })
      if (!settled) {
        setFlow({
          step: 'failed',
          message: 'the connection closed before the extraction reported a result. It may still be running on the server.',
          log,
        })
      }
    } catch (e) {
      setFlow({ step: 'failed', message: `the extraction request failed: ${errorText(e)}`, log })
    }
  }, [active, file, part, kind, model])

  const accept = useCallback(async (toml: string, saveAs: string) => {
    if (flow.step !== 'review') return
    setSaving(true)
    setSaveError(null)
    try {
      const result = await api.modelsSave({ part: saveAs, kind: flow.card.kind, toml })
      if (result.ok) {
        setFlow({ step: 'saved', card: flow.card, result })
        onSaved?.()
      } else setSaveError(result.error ?? 'the server refused the save without saying why')
    } catch (e) {
      setSaveError(`the save request failed: ${errorText(e)}`)
    } finally {
      setSaving(false)
    }
  }, [flow, onSaved])

  if (draftable.length === 0) return null

  const info = ready.phase === 'ready' ? ready.info : null
  const blocked = info !== null && !info.ready

  return (
    <section className="mt-4" data-testid="datasheet-extract">
      <div
        className="rounded-xl px-4 py-3"
        style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
      >
        <div className="text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
          Parts with no model
        </div>
        <div className="text-[12px] mt-1 leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
          These parts default to open, so anything analog on their nets is guesswork. A model is one
          TOML file: write it by hand, or draft one from the part's datasheet and check it.
          {info && <> {info.cost}</>}
        </div>

        <div className="mt-2.5 rounded-lg overflow-hidden" style={{ border: '1px solid var(--hairline)' }}>
          {draftable.map((p, i) => (
            <div
              key={p.reference}
              data-testid={`open-part-${p.reference}`}
              className="px-3 py-2.5 flex items-start gap-3 flex-wrap"
              style={{ borderTop: i > 0 ? '1px solid var(--hairline)' : 'none' }}
            >
              <div className="flex-1 min-w-0">
                <div className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
                  <span style={{ fontFamily: 'var(--font-mono)' }}>{p.reference}</span>
                  {p.value && (
                    <span className="ml-2 text-[12px] font-normal" style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}>
                      {p.value}
                    </span>
                  )}
                </div>
                <div className="text-[11px] mt-0.5 leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                  {p.reason}
                </div>
              </div>
              <button
                type="button"
                data-testid={`extract-start-${p.reference}`}
                disabled={active !== null}
                onClick={() => begin(p)}
                /* Named for its part. One row per unnamed part means one button
                   per part, and "Draft a model from a datasheet" repeated down
                   the list is a set of controls that cannot be told apart. */
                aria-label={`Draft a model for ${p.reference} from a datasheet`}
                className="hb-btn hb-press px-3 py-1 text-[12px] shrink-0 max-w-full whitespace-nowrap w-full sm:w-auto"
                style={{ minHeight: 28 }}
              >
                {/* The row already wraps, so on a phone this sits on its own line
                    across the card. Even then 320px has no room for the long
                    spelling, and the short one says the same thing. */}
                <span className="hidden min-[400px]:inline">Draft a model from a datasheet</span>
                <span className="min-[400px]:hidden">Draft from a datasheet</span>
              </button>
            </div>
          ))}
        </div>

        {ready.phase === 'unavailable' && (
          <div
            data-testid="extract-unavailable"
            className="mt-2.5 rounded-lg px-3 py-2 text-[12px]"
            style={{ border: '1px solid var(--hairline)', color: 'var(--silk-faint)' }}
          >
            This server does not offer datasheet extraction ({ready.reason}). From a terminal:{' '}
            <code className="hb-inline">hauksbee models extract --pdf &lt;file&gt; --part &lt;mpn&gt; --kind &lt;kind&gt;</code>
          </div>
        )}

        {active && (
          <div
            className="mt-3 rounded-lg px-3.5 py-3"
            style={{ border: '1px solid var(--copper-deep)', background: 'var(--copper-tint)' }}
          >
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <span className="text-[12px] font-semibold" style={{ color: 'var(--silk)' }}>
                {active.reference}
                {active.value ? ` · ${active.value}` : ''}
              </span>
              <button
                type="button"
                data-testid="extract-close"
                onClick={close}
                disabled={flow.step === 'running'}
                className="hb-btn hb-press px-2.5 text-[11px]"
                style={{ height: 24 }}
              >
                {flow.step === 'running' ? 'Drafting ...' : 'Close'}
              </button>
            </div>

            {/* Step 1: can it run, and does the user consent. Both before any
                file picker exists: an extraction that dies on "codex is not
                signed in" after the datasheet was chosen has already asked for
                permission it did not need. */}
            {flow.step === 'consent' && (
              <div className="mt-2">
                {ready.phase === 'loading' && (
                  <BusyLine color="var(--silk-dim)">
                    Checking whether an extraction can run on this machine ...
                  </BusyLine>
                )}
                {blocked && info && (
                  <Callout
                    tone="err"
                    testId="extract-blocked"
                    className="py-2.5 text-[12px]"
                    style={{ color: 'var(--silk)' }}
                    title="Extraction cannot run yet"
                  >
                    <div style={{ color: 'var(--silk-dim)' }}>{info.reason}</div>
                    {info.fix && (
                      <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>
                        <TerminalCommand command={info.fix} />
                      </div>
                    )}
                    <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>{info.cost}</div>
                  </Callout>
                )}
                {info && info.ready && (
                  <>
                    <Callout
                      tone="warn"
                      testId="extract-consent"
                      className="py-2.5 text-[12px]"
                      title={<span className="inline-flex items-center gap-1.5"><WarningIcon size={12} /> Leaves your machine</span>}
                    >
                      <div style={{ color: 'var(--silk-dim)' }}>{info.consent_notice}</div>
                      <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>{info.cost}</div>
                    </Callout>
                    <div className="mt-2.5 flex items-center gap-2 flex-wrap">
                      <button
                        type="button"
                        data-testid="extract-consent-accept"
                        onClick={() => setFlow({ step: 'attach' })}
                        className="hb-btn-primary hb-press px-3.5 py-1 text-[12px] max-w-full"
                        style={{ minHeight: 30 }}
                      >
                        I understand, let me attach the datasheet
                      </button>
                      <button
                        type="button"
                        onClick={close}
                        className="hb-btn hb-press px-3 text-[12px]"
                        style={{ height: 30 }}
                      >
                        Not now
                      </button>
                    </div>
                  </>
                )}
              </div>
            )}

            {/* Step 2: the datasheet, the part number, and the kind. */}
            {flow.step === 'attach' && info && (
              <div className="mt-2.5">
                <div className="flex flex-wrap gap-3 items-end">
                  <label className="text-[11px] block min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">Part number</span>
                    <input
                      data-testid="extract-part"
                      className="hb-input text-[12px] block w-full"
                      style={{ height: 30, maxWidth: '12rem', fontFamily: 'var(--font-mono)' }}
                      value={part}
                      onChange={e => setPart(e.target.value)}
                      placeholder="e.g. TP4054"
                    />
                  </label>
                  <label className="text-[11px] block min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">
                      What kind of part is it{' '}
                      <span style={{ color: 'var(--silk-faint)' }}>(optional)</span>
                    </span>
                    <select
                      data-testid="extract-kind"
                      className="hb-input text-[12px] block w-full"
                      style={{ height: 30, maxWidth: '17rem' }}
                      value={kind}
                      onChange={e => setKind(e.target.value)}
                    >
                      {/* The default. The datasheet says what the part is on
                          its first page and the model is about to read it, so
                          making someone classify their part first is a barrier
                          at exactly the wrong moment. The picker stays for
                          anyone who knows better than the model. */}
                      <option value="">from the datasheet</option>
                      {/* The kind's own name only. What a kind COVERS is shown
                          below, where it can wrap: in the option text it makes
                          the widest option wider than this card is on a phone,
                          and the browser cuts it rather than wrapping. */}
                      {info.kinds.map(k => (
                        <option key={k.id} value={k.id}>{k.id}</option>
                      ))}
                    </select>
                    <span className="block mt-1" style={{ color: 'var(--silk-faint)' }}>
                      {kind
                        ? (info.kinds.find(k => k.id === kind)?.label ?? '')
                        : 'The datasheet names the part on its first page and the model reads it there. Pick a kind only if you know better.'}
                    </span>
                  </label>
                  <label className="text-[11px] block min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">
                      Model to read it with{' '}
                      <span style={{ color: 'var(--silk-faint)' }}>(optional)</span>
                    </span>
                    <input
                      data-testid="extract-model"
                      className="hb-input text-[12px] block w-full"
                      style={{ height: 30, maxWidth: '17rem' }}
                      value={model}
                      onChange={e => setModel(e.target.value)}
                      placeholder={`${info.default_model} (${info.default_effort} effort)`}
                    />
                    {/* Reading a datasheet is not a cheap task. The values are
                        easy; the pin map is where a weaker model fails, because
                        package drawings are rotated, mirrored and often
                        unnumbered, and a wrong pin map still binds cleanly. So
                        the default is the strong tier, and this box is for
                        someone who has a reason to differ. */}
                    <span className="block mt-1" style={{ color: 'var(--silk-faint)' }}>
                      Leave blank for {info.default_model}. A weaker model reads pin
                      numbering wrong, and a wrong pin map simulates a different circuit.
                    </span>
                  </label>
                  <label className="text-[11px] block min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">Datasheet PDF</span>
                    <input
                      data-testid="extract-file"
                      type="file"
                      accept="application/pdf,.pdf"
                      onChange={e => setFile(e.target.files?.[0] ?? null)}
                      className="text-[12px] block w-full max-w-full"
                      style={{ color: 'var(--silk-dim)' }}
                    />
                  </label>
                </div>
                <div className="mt-2.5 flex items-center gap-2 flex-wrap">
                  <button
                    type="button"
                    data-testid="extract-run"
                    disabled={!file || part.trim().length === 0}
                    onClick={() => void run()}
                    className="hb-btn-primary hb-press px-3.5 py-1 text-[12px] max-w-full"
                    style={{ minHeight: 30 }}
                  >
                    Send the datasheet and draft the model
                  </button>
                  <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                    the draft comes back here for you to accept or reject
                  </span>
                </div>
              </div>
            )}

            {/* Step 3: progress. Codex is silent for minutes, so the stream
                heartbeats rather than pretending to know how far along it is. */}
            {flow.step === 'running' && (
              <div className="mt-2.5">
                <LogWell lines={flow.log} testId="extract-log" maxHeight={160} />
                <BusyLine className="mt-1.5">
                  Drafting the model; this usually takes one to three minutes.
                </BusyLine>
                <div className="mt-1 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                  Keep this panel open. Closing the page cannot cancel work already sent to the model service.
                </div>
              </div>
            )}

            {flow.step === 'review' && (
              <ReviewCard
                card={flow.card}
                saving={saving}
                saveError={saveError}
                onAccept={(toml, saveAs) => void accept(toml, saveAs)}
                onReject={close}
              />
            )}

            {flow.step === 'saved' && (
              <Callout
                tone="ok"
                testId="extract-saved"
                live
                className="mt-2.5 py-2.5 text-[12px]"
                title={<span className="inline-flex items-center gap-1.5"><CheckIcon size={12} /> Saved</span>}
              >
                <div style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}>{flow.result.path}</div>
                {flow.result.note && (
                  <div className="mt-1" style={{ color: 'var(--silk-dim)' }}>{flow.result.note}</div>
                )}
                <div className="mt-1" style={{ color: 'var(--silk-faint)' }}>
                  Re-analysing this board now so the new model can bind.
                </div>
              </Callout>
            )}

            {flow.step === 'failed' && (
              <div className="mt-2.5">
                <LogWell lines={flow.log} maxHeight={140} />
                <Callout tone="err" testId="extract-failed" live className="mt-1.5 py-2 text-[12px] whitespace-pre-wrap">
                  {flow.message}
                </Callout>
                <div className="mt-2">
                  <button
                    type="button"
                    onClick={() => setFlow({ step: 'attach' })}
                    className="hb-btn hb-press px-3 text-[12px]"
                    style={{ height: 28 }}
                  >
                    Try again
                  </button>
                </div>
              </div>
            )}
          </div>
        )}
      </div>
    </section>
  )
}
