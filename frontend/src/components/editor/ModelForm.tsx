import { useState } from 'react'
import {
  IDENTITY_KEYS, type Json, isIdentityOnly, missingParams, numericParams, paramInfo, readModel, readSource,
  setKey, tomlNumber, tomlString, writeSource,
} from './toml-edit'
import type { SourceRef } from './toml-edit'

// The guided way into a part model: plain fields over the same TOML text the
// TOML view edits. A field edit rewrites one line of that text; a TOML edit shows
// up here at once. Nothing is held in the form that is not in the text, except
// what a person has typed and not yet finished (a minus sign, half a URL).

const LABEL = 'var(--silk-faint)'

/** A number box that keeps what is being typed, and writes the text only when it
 *  is a number (or empty, which removes the line). */
function NumberField({ id, label, unit, value, required, onCommit }: {
  id: string
  label: string
  unit: string
  value: number | undefined
  required: boolean
  onCommit: (literal: string | null) => void
}) {
  const [draft, setDraft] = useState<string | null>(null)
  const shown = draft ?? (value === undefined ? '' : String(value))
  const bad = draft !== null && draft.trim() !== '' && tomlNumber(draft) === null
  return (
    <label className="text-[12px] block min-w-0" style={{ color: LABEL }}>
      <span className="block mb-1">
        {label}{unit && <span>, {unit}</span>}
        {required && value === undefined && <span style={{ color: 'var(--warn-strong)' }}> · needed</span>}
      </span>
      <input
        data-testid={`param-${id}`}
        className="hb-input text-[12px] block w-full tnum"
        style={{ height: 30, fontFamily: 'var(--font-mono)' }}
        inputMode="decimal"
        value={shown}
        aria-invalid={bad || undefined}
        placeholder="from the datasheet"
        onChange={e => {
          const v = e.target.value
          setDraft(v)
          if (v.trim() === '') onCommit(null)
          else {
            const literal = tomlNumber(v)
            if (literal !== null) onCommit(literal)
          }
        }}
        onBlur={() => setDraft(null)}
      />
      {bad && <span className="block mt-0.5" style={{ color: 'var(--err)' }}>Not a number. Use 1.5, -2 or 3e-6.</span>}
    </label>
  )
}

export function ModelForm({ text, onChange, error, onShowToml }: {
  text: string
  /** The whole file after the form's one-line edit. */
  onChange: (text: string) => void
  /** The checker's last complaint about this text, when it had one. */
  error: string | null
  onShowToml: () => void
}) {
  const parsed = readModel(text)
  const model: Json = parsed.ok ? parsed.model : {}
  const source = readSource(model)
  const [ref, setRef] = useState<SourceRef>(source)

  if (!parsed.ok) {
    return (
      <div data-testid="model-form-unreadable" className="rounded-lg px-3 py-2.5 text-[12px]" style={{ border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}>
        The form cannot read the file as it stands ({parsed.error}).{' '}
        <button type="button" className="hb-press underline cursor-pointer" style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)' }} onClick={onShowToml}>
          Fix it in the TOML view
        </button>
      </div>
    )
  }

  const identity = isIdentityOnly(model)
  const have = new Map(numericParams(model))
  const needed = error ? missingParams(error) : []
  // Fields: what the checker still wants first, then the numbers already set.
  const fields: { key: string; required: boolean }[] = [
    ...needed.map(m => ({ key: m.key, required: true })),
    ...[...have.keys()].filter(k => !needed.some(m => m.key === k)).map(k => ({ key: k, required: false })),
  ]
  const description = typeof model.description === 'string' ? model.description : ''
  const params = (model.params && typeof model.params === 'object' ? model.params : {}) as Json
  const otherParams = Object.keys(params).filter(k => !(IDENTITY_KEYS as readonly string[]).includes(k) && !have.has(k)).length

  const setParam = (key: string, literal: string | null) => onChange(setKey(text, '[models.params]', key, literal))
  const setIdentity = (on: boolean) => {
    let next = text
    if (on) {
      next = setKey(next, '[models.params]', 'unlocked_by', tomlString('a behavioural model written from the datasheet'))
      next = setKey(next, '[models.params]', 'warning', tomlString('behaviour not simulated yet'))
      next = setKey(next, '[models.params]', 'identity_only', 'true')
    } else {
      for (const key of IDENTITY_KEYS) next = setKey(next, '[models.params]', key, null)
    }
    onChange(next)
  }
  const setSource = (patch: Partial<SourceRef>) => {
    const next = { ...ref, ...patch }
    setRef(next)
    if (next.url.trim() && next.title.trim() && next.locator.trim()) onChange(writeSource(text, next))
  }
  const partial = [ref.url, ref.title, ref.locator].some(v => v.trim()) && ![ref.url, ref.title, ref.locator].every(v => v.trim())

  return (
    <div data-testid="model-form" className="grid gap-3">
      <label className="text-[12px] block min-w-0" style={{ color: LABEL }}>
        <span className="block mb-1">What this entry says about the part</span>
        <input
          data-testid="model-description"
          className="hb-input text-[12px] block w-full"
          style={{ height: 30 }}
          value={description}
          onChange={e => onChange(setKey(text, '[[models]]', 'description', tomlString(e.target.value)))}
        />
      </label>

      <div>
        <div className="text-[12px] mb-1.5" style={{ color: 'var(--silk-dim)' }}>
          Values, each one read from a datasheet table or measured
        </div>
        {fields.length > 0 ? (
          <div className="grid gap-2.5 sm:grid-cols-2">
            {fields.map(f => {
              const info = paramInfo(f.key)
              return (
                <NumberField
                  key={f.key}
                  id={f.key}
                  label={info.label}
                  unit={info.unit}
                  value={have.get(f.key)}
                  required={f.required}
                  onCommit={literal => setParam(f.key, literal)}
                />
              )
            })}
          </div>
        ) : (
          <div className="text-[12px]" style={{ color: 'var(--silk-faint)' }} data-testid="model-no-values">
            {identity
              ? 'No values yet: the part stays open in the simulation.'
              : 'This kind of part needs no values here. Describe its behaviour in the TOML view.'}
          </div>
        )}
        {otherParams > 0 && (
          <div className="text-[12px] mt-1.5" style={{ color: 'var(--silk-faint)' }}>
            {otherParams} more {otherParams === 1 ? 'setting is' : 'settings are'} in the TOML view.
          </div>
        )}
      </div>

      <label className="text-[12px] flex items-start gap-2 cursor-pointer" style={{ color: 'var(--silk-dim)' }}>
        <input type="checkbox" data-testid="model-identity-only" className="mt-0.5" checked={identity} onChange={e => setIdentity(e.target.checked)} />
        <span>
          I do not have the values yet: keep this part open for now.
          <span className="block" style={{ color: 'var(--silk-faint)' }}>
            Reports will say its behaviour is not simulated, and what would unlock it.
          </span>
        </span>
      </label>

      <fieldset className="min-w-0" style={{ border: 'none', padding: 0, margin: 0 }}>
        <legend className="text-[12px] mb-1.5" style={{ color: 'var(--silk-dim)', padding: 0 }}>Where the values come from</legend>
        <div className="grid gap-2.5 sm:grid-cols-3">
          {([
            ['title', 'Datasheet title and revision', 'e.g. BS250 datasheet, rev 3'],
            ['locator', 'Table or page', 'e.g. Table 2, page 4'],
            ['url', 'Link (https)', 'https://'],
          ] as const).map(([key, label, placeholder]) => (
            <label key={key} className="text-[12px] block min-w-0" style={{ color: LABEL }}>
              <span className="block mb-1">{label}</span>
              <input
                data-testid={`source-${key}`}
                className="hb-input text-[12px] block w-full"
                style={{ height: 30 }}
                value={ref[key]}
                placeholder={placeholder}
                onChange={e => setSource({ [key]: e.target.value })}
              />
            </label>
          ))}
        </div>
        {partial && (
          <div className="text-[12px] mt-1" style={{ color: 'var(--silk-faint)' }}>
            Fill in all three to record the source in the file.
          </div>
        )}
      </fieldset>
    </div>
  )
}
