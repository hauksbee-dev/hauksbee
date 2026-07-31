import { useCallback, useEffect, useRef, useState } from 'react'
import type { CardValue, ExtractReady, ModelCard, ModelSaveResult, WebOpenPart } from '../types/report'
import { CheckIcon, WarningIcon } from './Icons'
import { readSseStream } from '../lib/sse'

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

function Chip({ text, tone }: { text: string; tone: 'copper' | 'warn' | 'ok' }) {
  const tones = {
    copper: { bg: 'var(--copper-tint)', border: 'var(--copper-deep)', fg: 'var(--copper-hi)' },
    warn: { bg: 'var(--warn-bg)', border: 'var(--warn-border)', fg: 'var(--warn-strong)' },
    ok: { bg: 'var(--ok-bg)', border: 'var(--ok-border)', fg: 'var(--ok)' },
  }[tone]
  return (
    <span
      className="rounded px-1.5 py-0.5 text-[10px] font-bold tracking-widest uppercase"
      style={{ background: tones.bg, border: `1px solid ${tones.border}`, color: tones.fg, whiteSpace: 'nowrap' }}
    >
      {text}
    </span>
  )
}

function CopyText({ text, label = 'Copy' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false)
  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(text)
    } catch {
      const ta = document.createElement('textarea')
      ta.value = text
      ta.style.position = 'fixed'
      ta.style.opacity = '0'
      document.body.appendChild(ta)
      ta.select()
      try { document.execCommand('copy') } catch { /* nothing more to try */ }
      document.body.removeChild(ta)
    }
    setCopied(true)
    setTimeout(() => setCopied(false), 1500)
  }, [text])
  return (
    <button
      type="button"
      onClick={copy}
      className="hb-press ml-2 rounded px-2 py-0.5 text-[11px] font-semibold cursor-pointer"
      style={{
        background: copied ? 'var(--ok-bg)' : 'var(--copper-tint)',
        border: `1px solid ${copied ? 'var(--ok-border)' : 'var(--copper-deep)'}`,
        color: copied ? 'var(--ok)' : 'var(--copper-hi)',
        whiteSpace: 'nowrap',
      }}
    >
      {copied ? <span className="inline-flex items-center gap-1"><CheckIcon size={11} /> Copied</span> : label}
    </button>
  )
}

/** Blocks where a missing citation is worth pointing out. A pin map or a match
 *  regex has no datasheet page to cite; a simulation parameter or an
 *  absolute-maximum rating always does, and one that arrived without a source is
 *  the first thing a reviewer should be suspicious of. */
const CITED_SECTIONS = ['params', 'ratings']

/** The values table, grouped by the block each value came from. The citation
 *  column is the point: a number with no source cannot be checked, and checking
 *  is the whole job being asked of the reviewer. */
function ValueTable({ values }: { values: CardValue[] }) {
  const sections: { name: string; rows: CardValue[] }[] = []
  for (const v of values) {
    const existing = sections.find(s => s.name === v.section)
    if (existing) existing.rows.push(v)
    else sections.push({ name: v.section, rows: [v] })
  }
  return (
    <div className="mt-3 rounded-lg overflow-hidden" style={{ border: '1px solid var(--hairline)' }}>
      {sections.map(s => (
        <div key={s.name}>
          <div
            className="px-3 py-1 text-[10px] font-bold tracking-widest uppercase"
            style={{ background: 'var(--code-bg)', color: 'var(--silk-faint)', borderTop: '1px solid var(--hairline)' }}
          >
            {s.name}
          </div>
          {s.rows.map((v, i) => (
            <div
              key={`${v.key}-${i}`}
              className="px-3 py-1.5 flex items-baseline gap-3 flex-wrap text-[12px]"
              style={{ borderTop: '1px solid var(--rule)', background: v.assumed ? 'var(--warn-bg)' : 'var(--surface)' }}
            >
              <span style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)', minWidth: '9rem' }}>{v.key}</span>
              <span className="tnum" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{v.value}</span>
              {v.assumed && <Chip text="assumed" tone="warn" />}
              <span
                className="flex-1 min-w-0"
                style={{ color: v.source ? 'var(--silk-faint)' : 'var(--warn-strong)' }}
              >
                {v.source || (CITED_SECTIONS.includes(v.section) ? 'no source cited' : '')}
              </span>
            </div>
          ))}
        </div>
      ))}
    </div>
  )
}

/** One drafted model, for review. Editable, because a reviewer who spots a wrong
 *  number should be able to correct it before accepting rather than throwing the
 *  whole draft away; the server re-validates whatever arrives. */
function ReviewCard({
  card,
  onAccept,
  onReject,
  saving,
  saveError,
}: {
  card: ModelCard
  onAccept: (toml: string) => void
  onReject: () => void
  saving: boolean
  saveError: string | null
}) {
  const [toml, setToml] = useState(card.toml)
  const edited = toml !== card.toml
  return (
    <div data-testid="extract-review" className="mt-3">
      <div className="flex items-center gap-2 flex-wrap">
        <span className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
          Draft model for {card.reference || card.part}
        </span>
        <span className="text-[12px]" style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}>
          {card.model_id || card.part} · {card.kind}
        </span>
        <Chip text={card.provenance} tone="copper" />
      </div>
      {card.description && (
        <div className="text-[12px] mt-1" style={{ color: 'var(--silk-dim)' }}>{card.description}</div>
      )}

      <div
        className="mt-2.5 rounded-lg px-3 py-2 text-[12px] leading-relaxed"
        style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
      >
        Nothing has been saved. This is a draft an LLM wrote from the datasheet, not a
        measurement. Read it, then accept or reject it.
      </div>

      {card.assumptions.length > 0 && (
        <div
          data-testid="extract-assumptions"
          className="mt-2.5 rounded-lg px-3 py-2.5 text-[12px]"
          style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
        >
          <span className="text-[10px] font-bold tracking-widest uppercase block mb-1" style={{ color: 'var(--warn-strong)' }}>
            {card.assumptions.length} {card.assumptions.length === 1 ? 'value was' : 'values were'} not stated in the datasheet
          </span>
          <ul className="list-disc pl-4">
            {card.assumptions.map((a, i) => (
              <li key={i} style={{ color: 'var(--silk-dim)' }}>{a}</li>
            ))}
          </ul>
        </div>
      )}

      <ValueTable values={card.values} />

      <div className="mt-3">
        <div className="flex items-center justify-between flex-wrap gap-2">
          <span className="text-[11px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
            saves as {card.file_name}
          </span>
          <span className="flex items-center">
            {edited && (
              <span className="text-[11px] mr-1" style={{ color: 'var(--warn-strong)' }}>
                edited; the table above is the original extraction
              </span>
            )}
            <CopyText text={toml} label="Copy TOML" />
          </span>
        </div>
        <textarea
          data-testid="extract-toml"
          value={toml}
          onChange={e => setToml(e.target.value)}
          spellCheck={false}
          className="mt-1 w-full rounded-lg px-3 py-2 text-[11px]"
          style={{
            minHeight: 220,
            background: 'var(--instrument)',
            border: '1px solid var(--hairline)',
            color: 'var(--instrument-text)',
            fontFamily: 'var(--font-mono)',
            resize: 'vertical',
          }}
        />
      </div>

      {saveError && (
        <div
          data-testid="extract-save-error"
          aria-live="polite"
          className="mt-2 rounded-lg px-3 py-2 text-[12px] whitespace-pre-wrap"
          style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
        >
          {saveError}
        </div>
      )}

      <div className="mt-3 flex items-center gap-2 flex-wrap">
        <button
          type="button"
          data-testid="extract-accept"
          disabled={saving}
          onClick={() => onAccept(toml)}
          className="hb-btn-primary hb-press px-3.5 text-[12px]"
          style={{ height: 30 }}
        >
          {saving ? 'Saving ...' : 'Accept and save to my models'}
        </button>
        <button
          type="button"
          data-testid="extract-reject"
          disabled={saving}
          onClick={onReject}
          className="hb-btn hb-press px-3 text-[12px]"
          style={{ height: 30 }}
        >
          Reject
        </button>
        <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
          saving writes one TOML into ~/.hauksbee/models and nothing else
        </span>
      </div>
    </div>
  )
}

export function DatasheetExtract({ openParts }: { openParts: WebOpenPart[] }) {
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
  const logRef = useRef<HTMLPreElement>(null)

  useEffect(() => {
    if (draftable.length === 0) return
    let cancelled = false
    void (async () => {
      try {
        const res = await fetch('/api/models/extract/ready')
        if (!res.ok) {
          if (!cancelled) {
            setReady({
              phase: 'unavailable',
              reason: `the server answered ${res.status} ${res.statusText}`,
            })
          }
          return
        }
        const info = (await res.json()) as ExtractReady
        if (!cancelled) setReady({ phase: 'ready', info })
      } catch (e) {
        // An older server, or one started without the tool hooks: say so rather
        // than offering a button that cannot work.
        if (!cancelled) {
          setReady({
            phase: 'unavailable',
            reason: e instanceof Error ? e.message : String(e),
          })
        }
      }
    })()
    return () => { cancelled = true }
  }, [draftable.length])

  useEffect(() => {
    const el = logRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [flow])

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
      const res = await fetch('/api/models/extract', { method: 'POST', body: form })
      if (!res.ok || !res.body) {
        setFlow({ step: 'failed', message: `the server refused the extraction (${res.status} ${res.statusText})`, log })
        return
      }
      let settled = false
      await readSseStream(res.body, ({ event, data }) => {
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
      setFlow({ step: 'failed', message: `the extraction request failed: ${e instanceof Error ? e.message : String(e)}`, log })
    }
  }, [active, file, part, kind])

  const accept = useCallback(async (toml: string) => {
    if (flow.step !== 'review') return
    setSaving(true)
    setSaveError(null)
    try {
      const res = await fetch('/api/models/save', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ part: flow.card.part, kind: flow.card.kind, toml }),
      })
      const result = (await res.json()) as ModelSaveResult
      if (result.ok) setFlow({ step: 'saved', card: flow.card, result })
      else setSaveError(result.error ?? 'the server refused the save without saying why')
    } catch (e) {
      setSaveError(`the save request failed: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setSaving(false)
    }
  }, [flow])

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
                className="hb-btn hb-press px-3 text-[12px]"
                style={{ height: 28, flexShrink: 0 }}
              >
                Draft a model from a datasheet
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
                className="hb-btn hb-press px-2.5 text-[11px]"
                style={{ height: 24 }}
              >
                Close
              </button>
            </div>

            {/* Step 1: can it run, and does the user consent. Both before any
                file picker exists: an extraction that dies on "codex is not
                signed in" after the datasheet was chosen has already asked for
                permission it did not need. */}
            {flow.step === 'consent' && (
              <div className="mt-2">
                {ready.phase === 'loading' && (
                  <div className="text-[12px] flex items-center gap-2" role="status" aria-live="polite" style={{ color: 'var(--silk-dim)' }}>
                    <span className="slot-spin" /> Checking whether an extraction can run on this machine ...
                  </div>
                )}
                {blocked && info && (
                  <div
                    data-testid="extract-blocked"
                    className="rounded-lg px-3 py-2.5 text-[12px]"
                    style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--silk)' }}
                  >
                    <span className="text-[10px] font-bold tracking-widest uppercase block mb-1" style={{ color: 'var(--err-strong)' }}>
                      Extraction cannot run yet
                    </span>
                    <div style={{ color: 'var(--silk-dim)' }}>{info.reason}</div>
                    {info.fix && (
                      <div className="mt-1.5 flex items-center flex-wrap" style={{ color: 'var(--silk-faint)' }}>
                        <span className="mr-1">Terminal:</span>
                        <code
                          className="px-1.5 py-0.5 rounded"
                          style={{ background: 'var(--code-bg)', color: 'var(--silk-dim)', border: '1px solid var(--hairline)' }}
                        >
                          {info.fix}
                        </code>
                        <CopyText text={info.fix} />
                      </div>
                    )}
                    <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>{info.cost}</div>
                  </div>
                )}
                {info && info.ready && (
                  <>
                    <div
                      data-testid="extract-consent"
                      className="rounded-lg px-3 py-2.5 text-[12px] leading-relaxed"
                      style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
                    >
                      <span className="inline-flex items-center gap-1.5 text-[10px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--warn-strong)' }}>
                        <WarningIcon size={12} /> Leaves your machine
                      </span>
                      <div style={{ color: 'var(--silk-dim)' }}>{info.consent_notice}</div>
                      <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>{info.cost}</div>
                    </div>
                    <div className="mt-2.5 flex items-center gap-2 flex-wrap">
                      <button
                        type="button"
                        data-testid="extract-consent-accept"
                        onClick={() => setFlow({ step: 'attach' })}
                        className="hb-btn-primary hb-press px-3.5 text-[12px]"
                        style={{ height: 30 }}
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
                  <label className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">Part number</span>
                    <input
                      data-testid="extract-part"
                      className="hb-input text-[12px]"
                      style={{ height: 30, width: '12rem', fontFamily: 'var(--font-mono)' }}
                      value={part}
                      onChange={e => setPart(e.target.value)}
                      placeholder="e.g. TP4054"
                    />
                  </label>
                  <label className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">
                      What kind of part is it{' '}
                      <span style={{ color: 'var(--silk-faint)' }}>(optional)</span>
                    </span>
                    <select
                      data-testid="extract-kind"
                      className="hb-input text-[12px]"
                      style={{ height: 30, width: '17rem' }}
                      value={kind}
                      onChange={e => setKind(e.target.value)}
                    >
                      {/* The default. The datasheet says what the part is on
                          its first page and the model is about to read it, so
                          making someone classify their part first is a barrier
                          at exactly the wrong moment. The picker stays for
                          anyone who knows better than the model. */}
                      <option value="">work it out from the datasheet</option>
                      {info.kinds.map(k => (
                        <option key={k.id} value={k.id}>{k.id} · {k.label}</option>
                      ))}
                    </select>
                  </label>
                  <label className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">
                      Model to read it with{' '}
                      <span style={{ color: 'var(--silk-faint)' }}>(optional)</span>
                    </span>
                    <input
                      data-testid="extract-model"
                      className="hb-input text-[12px]"
                      style={{ height: 30, width: '17rem' }}
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
                  <label className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                    <span className="block mb-1">Datasheet PDF</span>
                    <input
                      data-testid="extract-file"
                      type="file"
                      accept="application/pdf,.pdf"
                      onChange={e => setFile(e.target.files?.[0] ?? null)}
                      className="text-[12px]"
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
                    className="hb-btn-primary hb-press px-3.5 text-[12px]"
                    style={{ height: 30 }}
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
                {flow.log.length > 0 && (
                  <pre
                    ref={logRef}
                    data-testid="extract-log"
                    className="rounded-lg px-3 py-2 text-[11px] overflow-x-auto overflow-y-auto whitespace-pre-wrap"
                    style={{
                      maxHeight: 160,
                      background: 'var(--instrument)',
                      border: '1px solid var(--hairline)',
                      color: 'var(--silk-dim)',
                      fontFamily: 'var(--font-mono)',
                    }}
                  >
                    {flow.log.join('\n')}
                  </pre>
                )}
                <div className="mt-1.5 text-[12px] flex items-center gap-2" role="status" aria-live="polite" style={{ color: 'var(--copper-hi)' }}>
                  <span className="slot-spin" /> Drafting the model; this usually takes one to three minutes.
                </div>
              </div>
            )}

            {flow.step === 'review' && (
              <ReviewCard
                card={flow.card}
                saving={saving}
                saveError={saveError}
                onAccept={toml => void accept(toml)}
                onReject={close}
              />
            )}

            {flow.step === 'saved' && (
              <div
                data-testid="extract-saved"
                aria-live="polite"
                className="mt-2.5 rounded-lg px-3 py-2.5 text-[12px]"
                style={{ background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--silk)' }}
              >
                <span className="inline-flex items-center gap-1.5 text-[10px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--ok)' }}>
                  <CheckIcon size={12} /> Saved
                </span>
                <div style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}>{flow.result.path}</div>
                {flow.result.note && (
                  <div className="mt-1" style={{ color: 'var(--silk-dim)' }}>{flow.result.note}</div>
                )}
                <div className="mt-1" style={{ color: 'var(--silk-faint)' }}>
                  Re-analyse the board to bind it.
                </div>
              </div>
            )}

            {flow.step === 'failed' && (
              <div className="mt-2.5">
                {flow.log.length > 0 && (
                  <pre
                    className="rounded-lg px-3 py-2 text-[11px] overflow-x-auto overflow-y-auto whitespace-pre-wrap"
                    style={{
                      maxHeight: 140,
                      background: 'var(--instrument)',
                      border: '1px solid var(--hairline)',
                      color: 'var(--silk-dim)',
                      fontFamily: 'var(--font-mono)',
                    }}
                  >
                    {flow.log.join('\n')}
                  </pre>
                )}
                <div
                  data-testid="extract-failed"
                  aria-live="polite"
                  className="mt-1.5 rounded-lg px-3 py-2 text-[12px] whitespace-pre-wrap"
                  style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
                >
                  {flow.message}
                </div>
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
