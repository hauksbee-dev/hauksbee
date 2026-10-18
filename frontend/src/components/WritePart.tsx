import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { WebOpenPart } from '../types/report'
import { api, errorText } from '../lib/api'
import { tildeHome } from '../lib/display-path'
import { DatasheetExtract } from './DatasheetExtract'
import { ModelForm } from './editor/ModelForm'
import { KIND_CHOICES, inferPartClass, kindLabel } from './editor/part-class'
import type { PartFacts } from './editor/part-class'
import { describesBehaviour, isIdentityOnly, missingParams, missingSentence, readModel, setKey, tomlString } from './editor/toml-edit'
import type { ParsedModel } from './editor/toml-edit'

// Model this part: one surface with three ways in, as tabs. Extend copies the
// part's existing executable model to edit; Draft from datasheet runs the
// extraction for this one part; Fill in by hand opens on a guided form for the
// kind of part the board says it is (chosen from its reference, symbol, footprint
// and part number, with the reason shown) and offers the TOML as a mode switch
// over the same text, so an edit in either view shows in the other.
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

/** A starting file for a part, used when the server has not drawn a richer one
 *  (no kind chosen yet, or the standalone editor). With a kind it validates as an
 *  identity-only entry of THAT kind: the part binds by name and reports say what
 *  it does not simulate yet. Without one the kind is left as a placeholder the
 *  checker refuses, because a wrong default kind is worse than none: a MOSFET
 *  must never validate as "a digital part". */
export function identityScaffold(part?: { reference: string; value: string }, kind?: string): string {
  const value = (part?.value.trim() || 'MY_PART').replace(/["\\']/g, '')
  const id = `${(part?.reference ?? 'part').toLowerCase()}_${value.toLowerCase()}`.replace(/[^a-z0-9]+/g, '_').replace(/^_|_$/g, '')
  const re = value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  return `[[models]]
id = "${id}"
${kind ? `kind = "${kind}"` : '# Choose what the part is: the checker refuses this placeholder.\nkind = "choose_kind"'}
description = "${value}: identified on the board, behaviour not modelled yet"

# Which parts on a board this entry claims: a regex over the value field.
[models.match]
value_re = '^${re}$'

# Identity only: the part binds by name and stays open in the simulation.
# Add the values for its kind to replace this with a model that simulates.
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

/** What the board says about a part, for choosing its kind. A report's open-part
 *  row carries only the reference and value; a coverage row adds the symbol and
 *  footprint, which sharpen the answer. */
function factsOf(part: WebOpenPart | ModelAuthoringSuggestion): PartFacts {
  return {
    reference: part.reference,
    value: part.value,
    lib_id: 'lib_id' in part ? part.lib_id : undefined,
    footprint: 'footprint' in part ? part.footprint : undefined,
    model_kind: 'model_kind' in part ? part.model_kind : undefined,
    properties: 'properties' in part ? part.properties : undefined,
  }
}

type CheckState =
  | { phase: 'idle' }
  | { phase: 'checking'; body: string; format: Format }
  | { phase: 'ok'; summary: string; body: string; format: Format }
  | { phase: 'bad'; error: string; body: string; format: Format }

export function isModelCheckCurrent(check: CheckState, body: string, format: Format): boolean {
  return check.phase !== 'idle' && check.body === body && check.format === format
}

export interface PlainStatus {
  state: 'checking' | 'choose' | 'ok' | 'missing' | 'fix' | 'deck-ok' | 'deck-bad'
  text: string
  note?: string
}

/** The checker's answer in plain words for the author. The checker's own words
 *  stay one click away ("What the checker said"); this line says what is wrong
 *  and what to do, never "valid but incomplete" in a success colour. */
export function plainStatus(
  check: CheckState,
  ctx: { toml: boolean; kindChosen: boolean; model: ParsedModel | null; view: 'form' | 'toml' },
): PlainStatus {
  if (check.phase === 'checking') return { state: 'checking', text: 'Checking …' }
  if (!ctx.toml) {
    return check.phase === 'ok'
      ? { state: 'deck-ok', text: `Valid: ${check.summary.replace(/^valid:\s*/i, '')}` }
      : { state: 'deck-bad', text: check.phase === 'bad' ? check.error : '' }
  }
  if (!ctx.kindChosen) return { state: 'choose', text: 'Choose what kind of part it is, then it can be saved.' }
  if (check.phase === 'bad') {
    const missing = missingParams(check.error)
    if (missing.length > 0) {
      return {
        state: 'missing',
        text: `Still needed: ${missingSentence(missing)}.`,
        note: 'Enter them from the datasheet, or tick "keep this part open for now" to save without values.',
      }
    }
    if (ctx.view === 'toml') return { state: 'fix', text: 'Fix the marked lines to save.' }
    const first = tomlIssues('', check.error)[0]?.message ?? check.error
    return { state: 'fix', text: `It cannot be saved yet: ${first}` }
  }
  if (check.phase !== 'ok') return { state: 'checking', text: 'Checking …' }
  const model = ctx.model?.ok ? ctx.model.model : null
  const text = 'Ready to save. A saved model is used the next time the board is analysed.'
  if (model && isIdentityOnly(model)) {
    return { state: 'ok', text, note: 'The part stays open: its behaviour is not simulated until you add values.' }
  }
  if (model && !describesBehaviour(model)) {
    return { state: 'ok', text, note: 'It names the part but describes no behaviour yet, so simulations will not change.' }
  }
  if (/no model declares \[models\.coverage\]/.test(check.summary)) {
    return { state: 'ok', text, note: 'It does not say which behaviours it covers, so reports keep showing this part’s coverage as it is.' }
  }
  return { state: 'ok', text }
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
        {/* The gutter takes its height from the text box, never from its own
            line count: it is a clipped window onto the numbers, scrolled in step
            with the text, so 38 lines of numbers cannot grow a 13-line box. */}
        <div aria-hidden="true" className="relative shrink-0 select-none text-right tnum" style={{ width: 34, background: 'var(--surface-2)', borderRight: '1px solid var(--hairline)' }}>
          <div className="absolute inset-0 overflow-hidden">
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
              // Grows with the text up to a cap, then scrolls with the gutter.
              height: Math.min(Math.max(lines.length * LINE_H + 2 * PAD + 14, 200), 460),
              minHeight: 200, fontFamily: 'var(--font-mono)', lineHeight: `${LINE_H}px`, padding: PAD,
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

export function WritePart({ onSaved, suggested, openSignal, boardLabel, onOpenSettings }: {
  onSaved?: () => void
  onOpenSettings?: () => void
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
  // The guided form or the TOML, over the same text. Extend starts on the TOML
  // (an existing model is rarely a handful of values); a new part starts on the form.
  const [view, setView] = useState<'form' | 'toml'>('form')
  // Why the starting kind was chosen, shown under the kind picker until the
  // author picks one themselves.
  const [why, setWhy] = useState<string | null>(null)
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

  // A kind was chosen, by the author or from the board's evidence. Untouched
  // text is replaced by the server's scaffold for it, drawn from the board's own
  // identity for this part; text the author has already edited keeps its lines
  // and only its kind changes.
  const pick = useCallback(async (chosen: string, opts: { why?: string | null; fresh?: boolean } = {}) => {
    setWhy(opts.why ?? null)
    setCheck({ phase: 'idle' })
    if (edited && !opts.fresh) {
      setTexts(t => ({ ...t, write: setKey(t.write, '[[models]]', 'kind', tomlString(chosen)) }))
      return
    }
    setEdited(false)
    if (!suggested) {
      setTexts(t => ({ ...t, write: identityScaffold(undefined, chosen) }))
      return
    }
    setDrafting(true)
    setSaveMsg(null)
    try {
      const result = await api.modelsDraft(draftRequest(suggested, boardLabel, chosen))
      if (result.ok && result.toml) setTexts(t => ({ ...t, write: result.toml! }))
      else {
        // The server could not draw one: a local scaffold of the same kind.
        setTexts(t => ({ ...t, write: identityScaffold(suggested, chosen) }))
        setSaveMsg(result.error ?? 'the server could not prepare a scaffold')
      }
    } catch (error) {
      setTexts(t => ({ ...t, write: identityScaffold(suggested, chosen) }))
      setSaveMsg(errorText(error))
    }
    setDrafting(false)
  }, [suggested, boardLabel, edited])

  // Starting a new part: the kind the board's evidence settles, with its reason,
  // or a question when it does not.
  const startWrite = useCallback(() => {
    if (!suggested) return
    const found = inferPartClass(factsOf(suggested))
    setWhy(found.why)
    if (found.kind) void pick(found.kind, { why: found.why, fresh: true })
  }, [suggested, pick])

  useEffect(() => {
    if (!openSignal || !suggested) return
    const asked = requestedTab
    requestedTab = null
    const start: ModelTab = asked === 'draft' && draftable ? 'draft'
      : (asked === 'extend' || asked === null) && extendable ? 'extend' : 'write'
    setTab(start)
    setFormat('toml')
    setView(start === 'extend' ? 'toml' : 'form')
    setPart(suggested.value.trim() || suggested.reference)
    setTexts({ extend: '', write: identityScaffold(suggested) })
    setWhy(null)
    setCheck({ phase: 'idle' })
    setSaveMsg(null)
    setEdited(false)
    setOpen(true)
    if (start === 'write') startWrite()
    if (start !== 'extend') return
    let cancelled = false
    void copyExisting(() => cancelled)
    return () => { cancelled = true }
  // `startWrite` follows `pick`, which follows `edited`; an edit must not
  // restart the part, so only the open signal and the part itself re-run this.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openSignal, suggested, draftable, extendable, copyExisting])

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

  // The kind the TOML currently names, read from the text itself so a kind typed
  // in the TOML view and a kind picked in the form are the same thing.
  const parsedModel = bodyFormat === 'toml' ? readModel(body) : null
  const parsedKind = parsedModel?.ok && typeof parsedModel.model.kind === 'string' ? parsedModel.model.kind : ''
  const kindChosen = parsedKind !== '' && parsedKind !== 'choose_kind'
  const writeKindChosen = (() => {
    const m = readModel(texts.write)
    return m.ok && typeof m.model.kind === 'string' && m.model.kind !== 'choose_kind'
  })()

  if (!open) {
    return (
      <button
        type="button"
        data-testid="write-part-open"
        onClick={() => {
          const start: ModelTab = extendable ? 'extend' : 'write'
          setTab(start)
          setView(start === 'extend' ? 'toml' : 'form')
          setOpen(true)
          if (extendable && !texts.extend) void copyExisting(() => false)
          if (start === 'write' && !writeKindChosen && !edited) startWrite()
        }}
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
    ['write', 'Fill in by hand'],
  ]
  const switchTo = (next: ModelTab) => {
    setTab(next)
    setEdited(false)
    setView(next === 'extend' ? 'toml' : 'form')
    if (next === 'extend' && !texts.extend) void copyExisting(() => false)
    if (next === 'write' && !writeKindChosen) startWrite()
  }
  const quiet = !edited && kindChosen
  const status = plainStatus(visibleCheck, { toml: bodyFormat === 'toml', kindChosen, model: parsedModel, view })
  const kindKnown = KIND_CHOICES.some(([id]) => id === parsedKind)

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
        <DatasheetExtract openParts={[openPart]} onSaved={onSaved} onOpenSettings={onOpenSettings} onCancel={() => switchTo('write')} />
      ) : (<>
        <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1.5 mb-2">
          {tab === 'write' ? (
            <label className="flex flex-wrap items-center gap-1.5 text-[12px] min-w-0" data-testid="kind-picker" style={{ color: 'var(--silk-dim)' }}>
              <span>What it is</span>
              <select
                className="hb-input text-[12px] truncate"
                aria-label="Kind of part"
                data-testid="kind-select"
                value={format === 'spice' ? 'spice' : kindChosen ? parsedKind : ''}
                onChange={e => {
                  const v = e.target.value
                  if (v === 'spice') { setFormat('spice'); return }
                  setFormat('toml')
                  void pick(v)
                }}
                style={{ height: 28, maxWidth: 260 }}
              >
                <option value="" disabled>choose what it is</option>
                {kindChosen && !kindKnown && <option value={parsedKind}>{kindLabel(parsedKind)}</option>}
                {KIND_CHOICES.map(([id, what]) => <option key={id} value={id}>{what}</option>)}
                <option value="spice">a SPICE deck</option>
              </select>
            </label>
          ) : <span />}
          {bodyFormat === 'toml' && (
            <div role="tablist" aria-label="How to edit the model" className="inline-flex items-center gap-0.5 p-0.5 rounded-lg" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}>
              {(['form', 'toml'] as const).map(v => (
                <button
                  key={v}
                  type="button"
                  role="tab"
                  aria-selected={view === v}
                  data-testid={`model-view-${v}`}
                  onClick={() => setView(v)}
                  className="hb-press px-3 py-1 text-[12px] cursor-pointer rounded-md"
                  style={{
                    border: 'none',
                    background: view === v ? 'var(--copper-tint-strong)' : 'transparent',
                    color: view === v ? 'var(--silk)' : 'var(--silk-dim)',
                    fontWeight: view === v ? 600 : 400,
                  }}
                >
                  {v === 'form' ? 'Form' : 'TOML'}
                </button>
              ))}
            </div>
          )}
        </div>
        {tab === 'write' && format !== 'spice' && why && (
          <div data-testid="kind-why" className="text-[12px] mb-2 leading-relaxed" style={{ color: 'var(--silk-faint)' }}>{why}</div>
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
        ) : view === 'toml' ? (
          <MarkedEditor
            value={toml}
            onChange={(t, fixed) => { if (!fixed) setEdited(true); setToml(t) }}
            issues={issues}
            tone={quiet ? 'warn' : 'err'}
            disabled={drafting}
            busy={drafting}
          />
        ) : kindChosen || tab === 'extend' ? (
          <fieldset disabled={drafting} aria-busy={drafting} className="min-w-0" style={{ border: 'none', padding: 0, margin: 0 }}>
            <ModelForm
              key={`${tab}:${drafting}`}
              text={toml}
              onChange={t => { setEdited(true); setToml(t) }}
              error={visibleCheck.phase === 'bad' ? visibleCheck.error : null}
              onShowToml={() => setView('toml')}
            />
          </fieldset>
        ) : (
          <div data-testid="model-form-choose" className="text-[12px] rounded-lg px-3 py-2.5" style={{ border: '1px dashed var(--hairline)', color: 'var(--silk-dim)' }}>
            Choose what the part is above, and the form asks for the values that kind of part needs.
          </div>
        )}

        <div
          data-testid="write-part-status"
          data-state={status.state}
          data-quiet={quiet || undefined}
          className="text-[12px] mt-2"
          style={{ color: status.state === 'ok' ? 'var(--ok)' : status.state === 'fix' ? 'var(--err)' : 'var(--silk-dim)' }}
        >
          <div>{status.text}</div>
          {status.note && <div style={{ color: 'var(--silk-faint)' }}>{status.note}</div>}
          {bodyFormat === 'toml' && (visibleCheck.phase === 'ok' || visibleCheck.phase === 'bad') && (
            <details className="mt-1" data-testid="write-part-checker-detail">
              <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>What the checker said</summary>
              <div className="mt-1 whitespace-pre-wrap break-words" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                {visibleCheck.phase === 'ok' ? visibleCheck.summary : visibleCheck.error}
              </div>
            </details>
          )}
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
