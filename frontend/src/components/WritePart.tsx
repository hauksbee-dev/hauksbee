import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { WebOpenPart } from '../types/report'
import { api, errorText } from '../lib/api'
import { tildeHome } from '../lib/display-path'
import { DatasheetExtract } from './DatasheetExtract'

// Model this part: one surface with three ways in, as tabs. Extend copies the
// part's existing executable model to edit; Draft from datasheet runs the
// extraction for this one part; Write it opens hauksbee's native TOML on a
// scaffold that already validates (identity only: the part binds by name and
// reports say what it does not simulate yet).
//
// The validation comes from POST /api/models/check, which runs the SAME checks
// the save path runs, never a friendlier client-side copy: an editor that
// accepts what the save refuses teaches an author their model is fine and then
// loses their work at the last step. Its complaints land on the TOML line they
// are about, as a margin mark, with a one-click fix where one is honest.

type Format = 'toml' | 'spice'
export type ModelTab = 'extend' | 'draft' | 'write'

/** The tab the next opening starts on, asked for by the open-parts sheet just
 *  before it opens the editor for a part. */
let requestedTab: ModelTab | null = null
export const requestModelTab = (tab: ModelTab) => { requestedTab = tab }

/** A deck that loads. The title line is not decoration: the first line of a
 *  SPICE deck is always a comment, so a deck without one silently loses its
 *  first real card. */
const SPICE_STARTER = `* my divider
.subckt divider in out
R1 in out 1k
R2 out 0 1k
.ends
V1 vin 0 5
X1 vin mid divider
`

/** A model that validates as written: the part binds by its value, and the
 *  report says plainly that its behaviour is not simulated yet. */
export function identityScaffold(part?: { reference: string; value: string }): string {
  const value = (part?.value.trim() || 'MY_PART').replace(/["\\']/g, '')
  const id = `${(part?.reference ?? 'part').toLowerCase()}_${value.toLowerCase()}`.replace(/[^a-z0-9]+/g, '_').replace(/^_|_$/g, '')
  const re = value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  return `[[models]]
id = "${id}"
kind = "digital"
description = "${value}: identified on the board, behaviour not modelled yet"

# Which parts on a board this entry claims: a regex over the value field.
[models.match]
value_re = '^${re}$'

# Identity only: the part binds by name and stays open in the simulation.
# Pick a kind above to replace this with a model that simulates.
[models.params]
identity_only = true
warning = "${value} is identified but its behaviour is not simulated"
unlocked_by = "a behavioural model written from the ${value} datasheet"
`
}

interface ModelAuthoringSuggestion {
  reference: string
  value: string
  lib_id?: string
  footprint?: string
  model_kind?: string | null
  model_id?: string | null
  stage?: string
  missing?: string[]
  properties?: Array<{ name: string; value: string }>
}

function draftRequest(suggested: WebOpenPart | ModelAuthoringSuggestion, boardLabel?: string, kind = '') {
  return {
    board_label: boardLabel ?? 'uploaded board',
    reference: suggested.reference,
    value: suggested.value,
    lib_id: 'lib_id' in suggested ? suggested.lib_id ?? '' : '',
    footprint: 'footprint' in suggested ? suggested.footprint ?? '' : '',
    properties: 'properties' in suggested
      ? (suggested.properties ?? []).map(property => [property.name, property.value])
      : [],
    model_id: 'model_id' in suggested ? suggested.model_id ?? '' : '',
    kind,
  }
}

/** The kinds a hand-written model can be, in the words someone with a datasheet
 *  in hand would use. A reference letter does not say (`Q` is a BJT or a MOSFET),
 *  so the author chooses. */
const KIND_CHOICES: [id: string, what: string][] = [
  ['passive', 'resistor, capacitor, inductor'], ['diode', 'diode, Schottky, Zener, LED'],
  ['bjt_npn', 'NPN transistor'], ['bjt_pnp', 'PNP transistor'],
  ['nmos', 'N-channel MOSFET'], ['pmos', 'P-channel MOSFET'],
  ['vreg', 'regulator, LDO, charger'], ['opamp', 'op-amp'], ['comparator', 'comparator'],
  ['timer', '555-class timer'], ['analog_switch', 'analogue or load switch'],
  ['digital', 'logic or digital part'], ['dac', 'DAC'], ['adc', 'ADC'],
  ['shift_register', 'shift register'], ['mcu', 'microcontroller'], ['connector', 'connector'],
]

type CheckState =
  | { phase: 'idle' }
  | { phase: 'checking'; body: string; format: Format }
  | { phase: 'ok'; summary: string; body: string; format: Format }
  | { phase: 'bad'; error: string; body: string; format: Format }

export function isModelCheckCurrent(check: CheckState, body: string, format: Format): boolean {
  return check.phase !== 'idle' && check.body === body && check.format === format
}

/** One validator complaint, placed on the line it is about. */
export interface TomlIssue {
  /** 1-based line. */
  line: number
  message: string
  fix?: { label: string; apply: (text: string) => string }
}

const lineOf = (lines: string[], test: RegExp) => {
  const i = lines.findIndex(l => test.test(l))
  return i < 0 ? null : i + 1
}

const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

/** Put `line` under the first `[models.params]`, adding the table if absent. */
function underParams(text: string, line: string): string {
  const lines = text.split('\n')
  const at = lines.findIndex(l => /^\s*\[models\.params\]/.test(l))
  if (at < 0) return `${text.replace(/\n*$/, '\n')}\n[models.params]\n${line}\n`
  lines.splice(at + 1, 0, line)
  return lines.join('\n')
}

const IDENTITY_FIX = {
  label: 'Keep it identity only for now',
  apply: (text: string) => ['identity_only = true', 'warning = "behaviour not simulated yet"', 'unlocked_by = "a behavioural model from the datasheet"']
    .filter(l => !new RegExp(`^\\s*${l.split(' ')[0]}\\s*=`, 'm').test(text))
    .reduceRight((t, l) => underParams(t, l), text),
}

/** The validator's answer, split into its complaints and placed on lines. */
export function tomlIssues(text: string, error: string): TomlIssue[] {
  const lines = text.split('\n')
  const top = lineOf(lines, /^\s*\[\[models\]\]/) ?? 1
  const header = (name: string) => lineOf(lines, new RegExp(`^\\s*\\[+models\\.${escapeRe(name)}\\]`))
  const body = error.replace(/^entry '[^']*'(?::\s*|\s+(?=has no match))/, '')
  return body.split(/;\s+/).filter(m => m.trim() !== '').map(message => {
    const at = /\bline (\d+)/.exec(message)
    if (at) return { line: Math.min(Number(at[1]), lines.length), message }
    const misplaced = /`(\w+)` is set directly in \[models\.(\w+)\]/.exec(message)
    if (misplaced) {
      const key = misplaced[1]
      const line = lineOf(lines, new RegExp(`^\\s*${key}\\s*=`)) ?? top
      return {
        line, message,
        fix: {
          label: `Move ${key} into [models.params]`,
          apply: (t: string) => {
            const ls = t.split('\n')
            const [moved] = ls.splice(line - 1, 1)
            return underParams(ls.join('\n'), moved.trim())
          },
        },
      }
    }
    const param = /\bparams?\.(\w+)|\bparam '([^']+)'/.exec(message)
    if (param) {
      const key = param[1] ?? param[2].split("'")[0]
      const line = lineOf(lines, new RegExp(`^\\s*${escapeRe(key)}\\s*=`)) ?? header('params') ?? top
      if (/identity-only model requires/.test(message)) {
        const value = key === 'warning' ? 'behaviour not simulated yet' : 'a behavioural model from the datasheet'
        return { line, message, fix: { label: `Add ${key}`, apply: (t: string) => underParams(t, `${key} = "${value}"`) } }
      }
      return { line, message, fix: /missing required param/.test(message) ? IDENTITY_FIX : undefined }
    }
    const role = /'([^']+)' is not a role in \[models\.pins\]/.exec(message)
    if (role) return { line: lineOf(lines, new RegExp(`["']${escapeRe(role[1])}["']`)) ?? header('pins') ?? top, message }
    if (/has no match rules/.test(message)) {
      return {
        line: top, message,
        fix: { label: 'Add a match rule', apply: (t: string) => `${t.replace(/\n*$/, '\n')}\n[models.match]\nvalue_re = '^MY_PART$'\n` },
      }
    }
    if (/no executable behavior/.test(message)) return { line: lineOf(lines, /^\s*kind\s*=/) ?? top, message, fix: IDENTITY_FIX }
    const table = /\[models\.(\w+)\]/.exec(message)
    return { line: (table && header(table[1])) ?? top, message }
  })
}

const LINE_H = 18
const PAD = 10

/** The TOML box with a margin: line numbers, a mark on each line the
 *  validator names, the line tinted, and the issue under the caret (or the
 *  first) spelled out under the box with its fix. */
function MarkedEditor({ value, onChange, issues, tone, disabled, busy }: {
  value: string
  onChange: (text: string, fixed?: boolean) => void
  issues: TomlIssue[]
  tone: 'warn' | 'err'
  disabled: boolean
  busy: boolean
}) {
  const [scroll, setScroll] = useState(0)
  const [caretLine, setCaretLine] = useState(1)
  const area = useRef<HTMLTextAreaElement>(null)
  const lines = value.split('\n')
  const byLine = new Map<number, TomlIssue[]>()
  for (const issue of issues) byLine.set(issue.line, [...(byLine.get(issue.line) ?? []), issue])
  const shown = byLine.get(caretLine)?.[0] ?? issues[0]
  const caret = (el: HTMLTextAreaElement) => setCaretLine(el.value.slice(0, el.selectionStart).split('\n').length)
  const goTo = (line: number) => {
    const el = area.current
    if (!el) return
    const offset = lines.slice(0, line - 1).reduce((n, l) => n + l.length + 1, 0)
    el.focus()
    el.setSelectionRange(offset, offset + (lines[line - 1]?.length ?? 0))
    el.scrollTop = Math.max(0, (line - 3) * LINE_H)
    setCaretLine(line)
  }
  const color = tone === 'err' ? 'var(--err)' : 'var(--warn)'
  const tint = tone === 'err' ? 'var(--err-bg)' : 'var(--warn-bg)'
  return (
    <>
      <div className="relative flex rounded-lg overflow-hidden" style={{ border: '1px solid var(--hairline)', background: 'var(--input-bg)' }}>
        <div aria-hidden="true" className="shrink-0 overflow-hidden select-none text-right tnum" style={{ width: 34, background: 'var(--surface-2)', borderRight: '1px solid var(--hairline)' }}>
          <div style={{ transform: `translateY(${-scroll}px)`, paddingTop: PAD, paddingBottom: PAD }}>
            {lines.map((_, i) => {
              const marked = byLine.get(i + 1)
              return (
                <div
                  key={i}
                  data-testid={marked ? 'toml-gutter-mark' : undefined}
                  title={marked?.map(m => m.message).join('\n')}
                  onMouseDown={marked ? e => { e.preventDefault(); goTo(i + 1) } : undefined}
                  className="pr-1.5 text-[10px]"
                  style={{ height: LINE_H, lineHeight: `${LINE_H}px`, color: marked ? color : 'var(--silk-faint)', cursor: marked ? 'pointer' : 'default', fontWeight: marked ? 700 : 400 }}
                >
                  {marked ? '●' : i + 1}
                </div>
              )
            })}
          </div>
        </div>
        <div className="relative flex-1 min-w-0">
          <div aria-hidden="true" className="absolute inset-0 overflow-hidden pointer-events-none">
            <div style={{ transform: `translateY(${-scroll}px)`, paddingTop: PAD }}>
              {lines.map((_, i) => <div key={i} style={{ height: LINE_H, background: byLine.has(i + 1) ? tint : 'transparent' }} />)}
            </div>
          </div>
          <textarea
            ref={area}
            data-testid="write-part-toml"
            value={value}
            wrap="off"
            onChange={e => { onChange(e.target.value); caret(e.target) }}
            onSelect={e => caret(e.currentTarget)}
            onScroll={e => setScroll(e.currentTarget.scrollTop)}
            disabled={disabled}
            aria-busy={busy}
            aria-invalid={issues.length > 0 || undefined}
            spellCheck={false}
            className="relative block w-full text-[12px] outline-none"
            style={{
              minHeight: 240, fontFamily: 'var(--font-mono)', lineHeight: `${LINE_H}px`, padding: PAD,
              background: 'transparent', color: 'var(--silk)', border: 'none', resize: 'vertical', whiteSpace: 'pre',
            }}
          />
        </div>
      </div>
      {shown && (
        <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-[12px]" data-testid="toml-issue" data-line={shown.line} style={{ color }}>
          <button type="button" className="hb-press tnum underline" style={{ background: 'none', border: 'none', padding: 0, color }} onClick={() => goTo(shown.line)}>
            Line {shown.line}
          </button>
          <span style={{ color: 'var(--silk)' }}>{shown.message}</span>
          {shown.fix && (
            <button type="button" data-testid="toml-fix" className="hb-chip hb-press px-2 text-[12px]" style={{ height: 24 }}
              onClick={() => onChange(shown.fix!.apply(value), true)}>
              {shown.fix.label}
            </button>
          )}
          {issues.length > 1 && <span style={{ color: 'var(--silk-faint)' }}>{issues.length - 1} more marked in the margin</span>}
        </div>
      )}
    </>
  )
}

export function WritePart({ onSaved, suggested, openSignal, boardLabel }: {
  onSaved?: () => void
  suggested?: WebOpenPart | ModelAuthoringSuggestion
  /** Changing this explicit signal opens the editor. Merely suggesting a part
   *  must not pop a panel open while a report arrives. */
  openSignal?: string | null
  boardLabel?: string
}) {
  const extendable = !!suggested && 'model_id' in suggested && !!suggested.model_id &&
    !!suggested.model_kind && !!suggested.stage?.startsWith('executable_')
  const draftable = !!suggested && !('bound' in suggested && (suggested.bound || suggested.rename_fix))
  const [open, setOpen] = useState(false)
  const [tab, setTab] = useState<ModelTab>('write')
  const [format, setFormat] = useState<Format>('toml')
  const [texts, setTexts] = useState({ extend: '', write: identityScaffold(suggested) })
  const [spice, setSpice] = useState(SPICE_STARTER)
  const toml = tab === 'extend' ? texts.extend : texts.write
  const body = format === 'toml' || tab === 'extend' ? toml : spice
  const bodyFormat: Format = tab === 'extend' ? 'toml' : format
  const [part, setPart] = useState(() => suggested?.value.trim() ?? '')
  const [check, setCheck] = useState<CheckState>({ phase: 'idle' })
  // Whether the author has touched the text. Until they have, a gap in a
  // server scaffold is a to-do the page already told them about, not an error.
  const [edited, setEdited] = useState(false)
  const [kind, setKind] = useState<string | null>(null)
  const [saveMsg, setSaveMsg] = useState<string | null>(null)
  const [drafting, setDrafting] = useState(false)
  const timer = useRef<number | null>(null)
  const checkEpoch = useRef(0)
  const setToml = useCallback((text: string) => setTexts(t => (tab === 'extend' ? { ...t, extend: text } : { ...t, write: text })), [tab])

  const openPart: WebOpenPart | null = useMemo(() => !suggested ? null : 'reason' in suggested ? suggested
    : { reference: suggested.reference, value: suggested.value, reason: '', consequence: '', active_ic: false, bound: false }, [suggested])

  // Extend: copy the exact resolved behaviour into a local extension.
  const copyExisting = useCallback(async (cancelled: () => boolean) => {
    if (!suggested) return
    setDrafting(true)
    setSaveMsg('Copying the exact resolved behavior into a local extension …')
    try {
      const result = await api.modelsDraft(draftRequest(suggested, boardLabel))
      if (cancelled()) return
      if (!result.ok || !result.toml) setSaveMsg(result.error ?? 'the server could not prepare the resolved model')
      else {
        setTexts(t => ({ ...t, extend: result.toml! }))
        setSaveMsg('Copied the existing executable model. Review the declared gaps, edit, validate, then choose Save.')
      }
    } catch (error) {
      if (!cancelled()) setSaveMsg(errorText(error))
    }
    if (!cancelled()) setDrafting(false)
  }, [suggested, boardLabel])

  useEffect(() => {
    if (!openSignal || !suggested) return
    const asked = requestedTab
    requestedTab = null
    const start: ModelTab = asked === 'draft' && draftable ? 'draft'
      : (asked === 'extend' || asked === null) && extendable ? 'extend' : 'write'
    setTab(start)
    setFormat('toml')
    setPart(suggested.value.trim() || suggested.reference)
    setTexts({ extend: '', write: identityScaffold(suggested) })
    setKind(null)
    setCheck({ phase: 'idle' })
    setSaveMsg(null)
    setEdited(false)
    setOpen(true)
    if (start !== 'extend') return
    let cancelled = false
    void copyExisting(() => cancelled)
    return () => { cancelled = true }
  }, [openSignal, suggested, draftable, extendable, copyExisting])

  // The author chose a kind: the server's scaffold for it replaces the
  // identity-only one, drawn from the board's own identity for this part.
  const pick = useCallback(async (chosen: string) => {
    setKind(chosen)
    setEdited(false)
    setCheck({ phase: 'idle' })
    if (!suggested) {
      setTexts(t => ({ ...t, write: identityScaffold().replace('kind = "digital"', `kind = "${chosen}"`) }))
      return
    }
    setDrafting(true)
    setSaveMsg(null)
    try {
      const result = await api.modelsDraft(draftRequest(suggested, boardLabel, chosen))
      if (result.ok && result.toml) setTexts(t => ({ ...t, write: result.toml! }))
      else setSaveMsg(result.error ?? 'the server could not prepare a scaffold')
    } catch (error) {
      setSaveMsg(errorText(error))
    }
    setDrafting(false)
  }, [suggested, boardLabel])

  // Debounced, because this runs the real validator on the server and a request
  // per keystroke would queue behind itself while someone types a paragraph.
  useEffect(() => {
    const epoch = ++checkEpoch.current
    if (!open || tab === 'draft') return
    if (timer.current) window.clearTimeout(timer.current)
    const checkedBody = body
    const checkedFormat = bodyFormat
    setCheck({ phase: 'checking', body: checkedBody, format: checkedFormat })
    timer.current = window.setTimeout(() => {
      void (async () => {
        type Verdict = { phase: 'ok'; summary: string } | { phase: 'bad'; error: string }
        const settle = (next: Verdict) => {
          // A result for text the author has already moved past is dropped:
          // Save must never be enabled for bytes the server has not seen.
          if (epoch === checkEpoch.current) setCheck({ ...next, body: checkedBody, format: checkedFormat })
        }
        try {
          const j = await api.modelsCheck({ toml: checkedBody, format: checkedFormat })
          settle(j.ok
            ? { phase: 'ok', summary: j.summary ?? 'valid' }
            : { phase: 'bad', error: j.error ?? 'the check did not say why' })
        } catch (e) {
          settle({ phase: 'bad', error: errorText(e) })
        }
      })()
    }, 400)
    return () => { if (timer.current) window.clearTimeout(timer.current) }
  }, [body, bodyFormat, open, tab])

  // Effects run after render. Treat a result for older text as checking even
  // during that one-render gap, so Save can never be enabled for bytes the
  // server has not validated.
  const visibleCheck: CheckState = isModelCheckCurrent(check, body, bodyFormat)
    ? check
    : { phase: 'checking', body, format: bodyFormat }
  const issues = visibleCheck.phase === 'bad' && bodyFormat === 'toml' ? tomlIssues(body, visibleCheck.error) : []

  // Escape closes the editor, the same key that dismisses every other in-page
  // surface. Bound on the document, only while open, and an Escape a native
  // picker already handled (`defaultPrevented`) is left alone.
  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented) return
      setOpen(false)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [open])

  const save = useCallback(async () => {
    setSaveMsg(null)
    try {
      const j = await api.modelsSave({ part: part.trim() || 'model', kind: '', toml: body })
      if (j.ok === false) setSaveMsg(j.error ?? 'the save failed and did not say why')
      else {
        const where = j.path ? tildeHome(j.path) : 'your model directory'
        setSaveMsg(j.coverage_note
          ? `Saved to ${where}. Coverage unchanged: ${j.coverage_note} Re-analysing this board now.`
          : `Saved to ${where}. Re-analysing this board now.`)
        onSaved?.()
      }
    } catch (e) {
      setSaveMsg(errorText(e))
    }
  }, [part, body, onSaved])

  if (!open) {
    return (
      <button
        type="button"
        data-testid="write-part-open"
        onClick={() => { setTab(extendable ? 'extend' : 'write'); setOpen(true); if (extendable && !texts.extend) void copyExisting(() => false) }}
        className="hb-btn hb-press px-3 text-[12px]"
        style={{ height: 30 }}
      >
        {suggested ? `Model ${suggested.reference}` : 'Write a part model'}
      </button>
    )
  }

  const tabs: [ModelTab, string][] = [
    ...(extendable ? [['extend', 'Extend model'] as [ModelTab, string]] : []),
    ...(draftable ? [['draft', 'Draft from datasheet'] as [ModelTab, string]] : []),
    ['write', 'Write it'],
  ]
  const switchTo = (next: ModelTab) => {
    setTab(next)
    setEdited(false)
    if (next === 'extend' && !texts.extend) void copyExisting(() => false)
  }
  const quiet = !edited && kind !== null

  return (
    <div data-testid="write-part" className="rounded-xl px-4 py-3.5 mt-3" style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}>
      <div className="flex flex-wrap items-center justify-between gap-2 mb-2">
        <span className="text-[13px] font-semibold min-w-0" style={{ color: 'var(--silk)' }}>
          {suggested ? <>Model {suggested.reference}{suggested.value && <span style={{ color: 'var(--silk-dim)', fontWeight: 400 }}> · {suggested.value}</span>}</> : 'Write a part model'}
        </span>
        <button type="button" data-testid="write-part-close" onClick={() => setOpen(false)} className="hb-btn hb-press px-2.5 text-[12px]" style={{ height: 26 }}>
          Close
        </button>
      </div>

      <div role="tablist" aria-label="How to model it" className="flex flex-wrap gap-1 mb-3" style={{ borderBottom: '1px solid var(--hairline)' }}>
        {tabs.map(([id, label]) => (
          <button
            key={id}
            type="button"
            role="tab"
            aria-selected={tab === id}
            data-testid={`model-tab-${id}`}
            onClick={() => switchTo(id)}
            className="hb-press px-2.5 py-1 text-[12px] text-left max-w-full"
            style={{
              minHeight: 30, background: 'none', border: 'none', marginBottom: -1,
              borderBottom: `2px solid ${tab === id ? 'var(--copper)' : 'transparent'}`,
              color: tab === id ? 'var(--silk)' : 'var(--silk-faint)', fontWeight: tab === id ? 600 : 400,
            }}
          >
            {label}
          </button>
        ))}
      </div>

      {tab === 'draft' && openPart ? (
        <DatasheetExtract focus openParts={[openPart]} onSaved={onSaved} />
      ) : (<>
        {tab === 'write' && (
          <div className="flex flex-wrap items-center gap-1.5 mb-2" data-testid="kind-picker">
            <span className="text-[12px]" style={{ color: 'var(--silk-dim)' }}>Simulate it as</span>
            <select
              className="hb-input text-[12px] w-full truncate"
              aria-label="Kind of part"
              data-testid="kind-select"
              value={format === 'spice' ? 'spice' : kind ?? ''}
              onChange={e => {
                const v = e.target.value
                if (v === 'spice') { setFormat('spice'); return }
                setFormat('toml')
                if (v === '') { setKind(null); setTexts(t => ({ ...t, write: identityScaffold(suggested) })) } else void pick(v)
              }}
              style={{ height: 28, maxWidth: 260 }}
            >
              <option value="">identity only</option>
              {KIND_CHOICES.map(([id, what]) => <option key={id} value={id}>{what}</option>)}
              <option value="spice">a SPICE deck</option>
            </select>
          </div>
        )}

        {bodyFormat === 'spice' ? (
          <>
            <p className="text-[12px] mb-2" style={{ color: 'var(--silk-dim)' }}>
              Paste a vendor model or deck, or load the file. Subcircuits are flattened at load; the answer below is the real loader&apos;s.
            </p>
            <input
              data-testid="write-part-spice-file"
              type="file"
              accept=".lib,.mod,.cir,.sp,.txt,.spi,.ckt"
              className="text-[12px] block w-full max-w-full mb-2"
              style={{ color: 'var(--silk-dim)' }}
              onChange={e => { const f = e.target.files?.[0]; if (f) void f.text().then(t => setSpice(t)) }}
            />
            <MarkedEditor value={spice} onChange={t => { setEdited(true); setSpice(t) }} issues={[]} tone="err" disabled={false} busy={false} />
          </>
        ) : (
          <MarkedEditor
            value={toml}
            onChange={(t, fixed) => { if (!fixed) setEdited(true); setToml(t) }}
            issues={issues}
            tone={quiet ? 'warn' : 'err'}
            disabled={drafting}
            busy={drafting}
          />
        )}

        <div
          data-testid="write-part-status"
          data-quiet={quiet || undefined}
          className="text-[12px] mt-1.5"
          style={{ color: visibleCheck.phase === 'ok' ? 'var(--ok)' : 'var(--silk-faint)' }}
        >
          {visibleCheck.phase === 'checking' && 'Checking …'}
          {visibleCheck.phase === 'ok' && `Valid: ${visibleCheck.summary}`}
          {visibleCheck.phase === 'bad' && bodyFormat === 'spice' && <span style={{ color: 'var(--err)' }}>{visibleCheck.error}</span>}
          {visibleCheck.phase === 'bad' && bodyFormat === 'toml' && (quiet ? 'Fill in the marked lines from the datasheet, then it will save.' : 'Fix the marked lines to save.')}
        </div>

        {bodyFormat === 'toml' && (
          <div className="flex flex-wrap items-end gap-2 mt-2.5">
            <label className="text-[12px] flex-1 min-w-0" style={{ color: 'var(--silk-faint)', maxWidth: '17rem' }}>
              <span className="block mb-1">Part number (names the saved file)</span>
              <input data-testid="write-part-name" className="hb-input text-[12px] block w-full" style={{ height: 30 }}
                value={part} onChange={e => setPart(e.target.value)} placeholder="e.g. BC847B" />
            </label>
            <button
              type="button"
              data-testid="write-part-save"
              disabled={drafting || visibleCheck.phase !== 'ok' || part.trim().length === 0}
              onClick={() => void save()}
              className="hb-press rounded-lg px-3.5 py-1 text-[12px] font-semibold max-w-full disabled:opacity-40 disabled:cursor-not-allowed"
              style={{ minHeight: 30, background: 'linear-gradient(180deg, var(--copper-hi), var(--copper))', color: 'var(--on-copper)', border: 'none' }}
            >
              Save to my models
            </button>
          </div>
        )}
        {bodyFormat === 'spice' && (
          <p className="text-[12px] mt-2" style={{ color: 'var(--silk-faint)' }}>
            A deck is checked here, not saved as a model. Pick a kind above to write the entry that claims your part.
          </p>
        )}
      </>)}

      {saveMsg && tab !== 'draft' && (
        <div className="text-[12px] mt-2" data-testid="write-part-message" style={{ color: 'var(--silk-dim)' }}>
          {saveMsg}
        </div>
      )}
    </div>
  )
}
