import { useState } from 'react'
import type { CardValue, ModelCard } from '../../types/report'
import { Callout, CopyButton } from '../ui'

/** Blocks where a missing citation is worth pointing out. A pin map or a match
 *  regex has no datasheet page to cite; a simulation parameter or an
 *  absolute-maximum rating always does, and one that arrived without a source is
 *  the first thing a reviewer should be suspicious of. */
const CITED_SECTIONS = ['params', 'ratings']

export function Chip({ text, tone }: { text: string; tone: 'copper' | 'warn' | 'ok' }) {
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
              <span className="flex-1 min-w-0" style={{ color: v.source ? 'var(--silk-faint)' : 'var(--warn-strong)' }}>
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
export function ReviewCard({ card, onAccept, onReject, saving, saveError }: {
  card: ModelCard
  onAccept: (toml: string, saveAs: string) => void
  onReject: () => void
  saving: boolean
  saveError: string | null
}) {
  const [toml, setToml] = useState(card.toml)
  const [saveAs, setSaveAs] = useState(card.part)
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
        <Callout
          tone="warn"
          testId="extract-assumptions"
          className="mt-2.5 py-2.5 text-[12px]"
          title={`${card.assumptions.length} ${card.assumptions.length === 1 ? 'value was' : 'values were'} not stated in the datasheet`}
        >
          <ul className="list-disc pl-4">
            {card.assumptions.map((a, i) => <li key={i} style={{ color: 'var(--silk-dim)' }}>{a}</li>)}
          </ul>
        </Callout>
      )}

      <ValueTable values={card.values} />

      <div className="mt-3">
        <div className="flex items-end justify-between flex-wrap gap-2">
          <label className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            <span className="block mb-1">Save name</span>
            <input
              data-testid="extract-save-as"
              value={saveAs}
              onChange={event => setSaveAs(event.target.value)}
              className="hb-input text-[11px]"
              style={{ height: 30, minWidth: 190, fontFamily: 'var(--font-mono)' }}
              aria-label="Model save name"
            />
          </label>
          <span className="flex items-center">
            {toml !== card.toml && (
              <span className="text-[11px] mr-1" style={{ color: 'var(--warn-strong)' }}>
                edited; the table above is the original extraction
              </span>
            )}
            <CopyButton text={toml} label="Copy TOML" />
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
        <Callout tone="err" testId="extract-save-error" live className="mt-2 py-2 text-[12px] whitespace-pre-wrap">
          {saveError}
        </Callout>
      )}

      <div className="mt-3 flex items-center gap-2 flex-wrap">
        <button
          type="button"
          data-testid="extract-accept"
          disabled={saving || saveAs.trim().length === 0}
          onClick={() => onAccept(toml, saveAs.trim())}
          className="hb-btn-primary hb-press px-3.5 py-1 text-[12px] max-w-full"
          style={{ minHeight: 30 }}
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
