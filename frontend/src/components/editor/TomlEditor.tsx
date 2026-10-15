import { useEffect, useRef } from 'react'
import { Annotation, EditorState, RangeSet, StateEffect, StateField } from '@codemirror/state'
import type { Extension } from '@codemirror/state'
import {
  drawSelection, EditorView, gutter, GutterMarker, highlightActiveLine, highlightSpecialChars,
  hoverTooltip, keymap, lineNumbers,
} from '@codemirror/view'
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from '@codemirror/language'
import { toml } from '@codemirror/legacy-modes/mode/toml'
import { tags } from '@lezer/highlight'
import { linter, lintKeymap } from '@codemirror/lint'
import type { Diagnostic } from '@codemirror/lint'
import { autocompletion, completionKeymap } from '@codemirror/autocomplete'
import type { CompletionContext, CompletionResult } from '@codemirror/autocomplete'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { searchKeymap } from '@codemirror/search'
import { assertHeaderOffsets, readCst } from '../../lib/toml-cst'
import { hoverAt, suggest } from '../../lib/toml-schema-assist'
import type { AssistData, AssistKind, JsonSchema } from '../../lib/toml-schema-assist'

// The spec editor: CodeMirror 6 on the spec text, with schema completion, the
// server's lint (and its one-click fixes) inline, hover help in the CLI's
// words, and the last run's verdict per [[assert]] in the gutter. Loaded
// lazily (`React.lazy`), so the app's first paint never waits for it. The text
// is owned by the caller: edits go out through `onChange`, and a new `value`
// comes in as a minimal change, so the caret, the selection, the undo history
// and focus all survive a builder edit landing in the text.

/** One [[assert]]'s last result, for the gutter. */
export interface GutterResult {
  status: 'pass' | 'fail' | 'invalid' | 'measured'
  label: string
  /** True when the spec changed since that run. */
  stale?: boolean
}

export interface TomlEditorProps {
  value: string
  onChange: (text: string) => void
  /** JSON Schema for completion and hover (the CI spec schema by default). */
  schemaUrl?: string
  /** `POST {toml}` lint endpoint answering `{diagnostics:[...]}`; null turns server lint off. */
  lintUrl?: string | null
  /** Assertion catalogue for `kind =` completion; null for a non-spec file. */
  catalogUrl?: string | null
  nets?: readonly string[]
  refs?: readonly string[]
  /** Per [[assert]] index. */
  results?: readonly (GutterResult | null)[]
  onResultClick?: (assertIndex: number) => void
  /** A range to select and scroll to; a new `seq` re-applies it. */
  select?: { start: number; end: number; seq: number } | null
  testId?: string
  ariaLabel?: string
  /** CSS max-height of the editor (it scrolls inside). */
  maxHeight?: string
}

interface LintAnswer {
  ok?: boolean
  diagnostics?: {
    severity: string; code?: string; start: number; end: number; message: string
    fix?: { replace_start: number; replace_end: number; text: string; label: string } | null
  }[]
}

const external = Annotation.define<boolean>()

// Schema and catalogue are the same for every editor in the page: fetch each once.
const cache = new Map<string, Promise<unknown>>()
function fetchOnce<T>(url: string): Promise<T | null> {
  if (!cache.has(url)) {
    cache.set(url, fetch(url).then(r => (r.ok ? r.json() : null)).catch(() => null))
  }
  return cache.get(url) as Promise<T | null>
}

const setResults = StateEffect.define<readonly (GutterResult | null)[]>()
const resultsField = StateField.define<readonly (GutterResult | null)[]>({
  create: () => [],
  update: (value, tr) => tr.effects.reduce((v, e) => (e.is(setResults) ? e.value : v), value),
})

class ResultMarker extends GutterMarker {
  constructor(readonly result: GutterResult) { super() }
  eq(other: ResultMarker) {
    return other.result.status === this.result.status && other.result.label === this.result.label
      && other.result.stale === this.result.stale
  }
  toDOM() {
    // No text node: the gutter must not add characters to the spec's text.
    const el = document.createElement('span')
    el.className = `cm-hb-result cm-hb-result-${this.result.status}${this.result.stale ? ' cm-hb-stale' : ''}`
    el.title = `${this.result.label}${this.result.stale ? ' (from a previous run)' : ''}. Click to see the result.`
    el.setAttribute('aria-label', el.title)
    el.dataset.testid = 'gutter-result'
    return el
  }
}

function resultMarkers(view: EditorView): RangeSet<GutterMarker> {
  const results = view.state.field(resultsField)
  const text = view.state.doc.toString()
  const marks = assertHeaderOffsets(text).flatMap((at, i) => {
    const r = results[i]
    return r ? [new ResultMarker(r).range(at)] : []
  })
  return RangeSet.of(marks, true)
}

const highlight = HighlightStyle.define([
  { tag: tags.comment, color: 'var(--silk-faint)', fontStyle: 'italic' },
  { tag: tags.propertyName, color: 'var(--copper-hi)' },
  { tag: tags.string, color: 'var(--ok)' },
  { tag: [tags.number, tags.atom, tags.bool], color: 'var(--warn)' },
  { tag: tags.bracket, color: 'var(--silk-dim)' },
])

const theme = EditorView.theme({
  '&': { background: 'var(--code-bg)', color: 'var(--silk)', fontSize: '12px', maxHeight: 'var(--hb-editor-max, 46vh)' },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller, .cm-scroller *': { fontFamily: 'var(--font-mono)' },
  '.cm-scroller': { overflow: 'auto', lineHeight: '1.5' },
  '.cm-content': { caretColor: 'var(--copper)', padding: '10px 0' },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--copper)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { background: 'var(--copper-tint-strong)' },
  '.cm-activeLine': { background: 'var(--copper-tint)' },
  '.cm-gutters': { background: 'var(--code-bg)', color: 'var(--silk-faint)', border: 'none' },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 6px 0 10px', minWidth: '28px' },
  '.cm-hb-results': { width: '14px' },
  '.cm-hb-results .cm-gutterElement': { display: 'flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer' },
  '.cm-hb-result': { width: '8px', height: '8px', borderRadius: '4px', display: 'inline-block' },
  '.cm-hb-result-pass': { background: 'var(--ok)' },
  '.cm-hb-result-fail': { background: 'var(--err)' },
  '.cm-hb-result-invalid': { background: 'var(--warn)' },
  '.cm-hb-result-measured': { background: 'var(--note)' },
  '.cm-hb-stale': { opacity: '0.45' },
  '.cm-tooltip': {
    background: 'var(--surface)', color: 'var(--silk)', border: '1px solid var(--hairline)',
    borderRadius: '8px', boxShadow: 'var(--shadow-pop)', fontFamily: 'var(--font-sans)', fontSize: '12px',
  },
  '.cm-tooltip-autocomplete > ul > li[aria-selected]': { background: 'var(--copper-tint-strong)', color: 'var(--silk)' },
  '.cm-completionDetail': { color: 'var(--silk-faint)', fontStyle: 'normal', marginLeft: '8px' },
  '.cm-completionInfo': { maxWidth: '320px', padding: '6px 8px', lineHeight: '1.45' },
  '.cm-hb-hover': { maxWidth: '340px', padding: '8px 10px', lineHeight: '1.45' },
  '.cm-hb-hover b': { display: 'block', marginBottom: '2px', fontWeight: '600' },
  '.cm-diagnostic': { padding: '6px 8px', borderLeftWidth: '3px' },
  '.cm-diagnostic-error': { borderLeftColor: 'var(--err)' },
  '.cm-diagnostic-warning': { borderLeftColor: 'var(--warn)' },
  '.cm-diagnosticAction': {
    background: 'var(--copper)', color: 'var(--on-copper)', borderRadius: '6px', padding: '2px 8px', marginLeft: '8px',
    font: 'inherit', fontSize: '12px', cursor: 'pointer',
  },
  '.cm-lintRange-error': { backgroundImage: 'none', textDecoration: 'underline wavy var(--err)', textUnderlineOffset: '3px' },
  '.cm-lintRange-warning': { backgroundImage: 'none', textDecoration: 'underline wavy var(--warn)', textUnderlineOffset: '3px' },
})

const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))

export default function TomlEditor(props: TomlEditorProps) {
  const host = useRef<HTMLDivElement>(null)
  const view = useRef<EditorView | null>(null)
  // The latest props, read by the extensions without rebuilding the editor.
  const live = useRef(props)
  live.current = props
  const assist = useRef<AssistData>({ schema: null, kinds: [], nets: [], refs: [] })

  useEffect(() => {
    const p = live.current
    const schemaUrl = p.schemaUrl ?? '/api/ci/schema'
    const catalogUrl = p.catalogUrl === undefined ? '/api/ci/catalog' : p.catalogUrl
    void fetchOnce<JsonSchema>(schemaUrl).then(schema => {
      if (schema && typeof schema === 'object') assist.current = { ...assist.current, schema }
    })
    if (catalogUrl) {
      void fetchOnce<{ kinds?: AssistKind[] }>(catalogUrl).then(cat => {
        if (Array.isArray(cat?.kinds)) assist.current = { ...assist.current, kinds: cat.kinds }
      })
    }
  }, [])

  useEffect(() => {
    if (!host.current) return
    const data = () => ({ ...assist.current, nets: live.current.nets ?? [], refs: live.current.refs ?? [] })

    const complete = (ctx: CompletionContext): CompletionResult | null => {
      const text = ctx.state.doc.toString()
      const hit = suggest(text, ctx.pos, data())
      if (!hit) return null
      // Unprompted, only once a word or an opening quote has been typed.
      if (!ctx.explicit && hit.from === ctx.pos && !/["']$/.test(text.slice(0, hit.from))) return null
      return {
        from: hit.from,
        options: hit.options.map(o => ({ label: o.label, apply: o.apply, detail: o.detail, info: o.info, type: o.type })),
        validFor: /^[^\s"',=[\]]*$/,
      }
    }

    const lint = async (v: EditorView): Promise<Diagnostic[]> => {
      const state = v.state
      const text = state.doc.toString()
      const read = readCst(text)
      if (!read.ok) {
        const from = Math.min(read.error.offset, text.length)
        return [{ from, to: Math.min(from + 1, text.length), severity: 'error', source: 'toml', message: read.error.message }]
      }
      const url = live.current.lintUrl === undefined ? '/api/ci/lint' : live.current.lintUrl
      if (!url) return []
      // Parse errors show at once; the server's checks wait for a pause in typing.
      await sleep(250)
      if (v.state.doc !== state.doc) return []
      let answer: LintAnswer | null = null
      try {
        const res = await fetch(url, {
          method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ toml: text }),
        })
        answer = res.ok ? await res.json() as LintAnswer : null
      } catch { answer = null }
      const clamp = (n: number) => Math.max(0, Math.min(n, text.length))
      return (answer?.diagnostics ?? []).map(d => {
        const from = clamp(d.start)
        const fix = d.fix
        return {
          from,
          to: Math.max(from, clamp(d.end)),
          severity: d.severity === 'warning' ? 'warning' : 'error',
          source: d.code,
          message: d.message,
          actions: fix ? [{
            name: fix.label,
            apply: (target: EditorView) => target.dispatch({
              changes: { from: clamp(fix.replace_start), to: clamp(fix.replace_end), insert: fix.text },
            }),
          }] : [],
        } satisfies Diagnostic
      })
    }

    const hover = hoverTooltip((v, pos) => {
      const hit = hoverAt(v.state.doc.toString(), pos, data())
      if (!hit) return null
      return {
        pos: hit.from,
        end: hit.to,
        above: true,
        create: () => {
          const dom = document.createElement('div')
          dom.className = 'cm-hb-hover'
          dom.dataset.testid = 'toml-hover'
          const title = document.createElement('b')
          title.textContent = hit.title
          const body = document.createElement('div')
          body.textContent = hit.body
          dom.append(title, body)
          return { dom }
        },
      }
    })

    const extensions: Extension[] = [
      lineNumbers(),
      gutter({
        class: 'cm-hb-results',
        markers: resultMarkers,
        initialSpacer: () => new ResultMarker({ status: 'pass', label: '' }),
        domEventHandlers: {
          mousedown: (v, line) => {
            const starts = assertHeaderOffsets(v.state.doc.toString())
            const i = starts.findIndex(at => at >= line.from && at <= line.to)
            if (i < 0 || !v.state.field(resultsField)[i]) return false
            live.current.onResultClick?.(i)
            return true
          },
        },
      }),
      resultsField,
      highlightSpecialChars(),
      history(),
      drawSelection(),
      highlightActiveLine(),
      StreamLanguage.define(toml),
      syntaxHighlighting(highlight),
      autocompletion({ override: [complete], icons: false }),
      linter(lint, { delay: 50 }),
      hover,
      keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, ...completionKeymap, ...lintKeymap]),
      theme,
      EditorView.contentAttributes.of({ 'aria-label': props.ariaLabel ?? 'Spec TOML', spellcheck: 'false' }),
      EditorView.updateListener.of(update => {
        if (!update.docChanged || update.transactions.every(tr => tr.annotation(external))) return
        live.current.onChange(update.state.doc.toString())
      }),
    ]
    const v = new EditorView({
      state: EditorState.create({ doc: live.current.value, extensions }),
      parent: host.current,
    })
    view.current = v
    v.dispatch({ effects: setResults.of(live.current.results ?? []) })
    return () => { v.destroy(); view.current = null }
    // The editor is built once; props are read through `live`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // A new value from outside (a builder edit) lands as the smallest change, so
  // the caret, the selection and the undo history keep their places.
  useEffect(() => {
    const v = view.current
    if (!v) return
    const current = v.state.doc.toString()
    if (current === props.value) return
    let start = 0
    const max = Math.min(current.length, props.value.length)
    while (start < max && current.charCodeAt(start) === props.value.charCodeAt(start)) start++
    let endA = current.length
    let endB = props.value.length
    while (endA > start && endB > start && current.charCodeAt(endA - 1) === props.value.charCodeAt(endB - 1)) { endA--; endB-- }
    v.dispatch({
      changes: { from: start, to: endA, insert: props.value.slice(start, endB) },
      annotations: external.of(true),
    })
  }, [props.value])

  useEffect(() => {
    view.current?.dispatch({ effects: setResults.of(props.results ?? []) })
  }, [props.results])

  useEffect(() => {
    const v = view.current
    const sel = props.select
    if (!v || !sel) return
    const len = v.state.doc.length
    v.dispatch({
      selection: { anchor: Math.min(sel.start, len), head: Math.min(sel.end, len) },
      scrollIntoView: true,
    })
    v.focus()
  }, [props.select?.seq]) // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div
      ref={host}
      data-testid={props.testId ?? 'toml-editor'}
      style={{ ['--hb-editor-max' as string]: props.maxHeight ?? '46vh' }}
    />
  )
}
