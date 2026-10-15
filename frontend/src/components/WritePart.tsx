import { useCallback, useEffect, useRef, useState } from 'react'
import type { WebOpenPart } from '../types/report'
import { api, errorText } from '../lib/api'
import { tildeHome } from '../lib/display-path'

// Write a part by hand, in hauksbee's native TOML, with the real validator
// answering as you type.
//
// Datasheet extraction covers "I have a PDF and want a draft". It does nothing
// for someone who already knows their part, whose only other route is editing
// a file on disk and restarting the server.
//
// The validation comes from POST /api/models/check, which runs the SAME checks
// the save path runs, never a friendlier client-side copy: an editor that
// accepts what the save refuses teaches an author their model is fine and then
// loses their work at the last step.

/** A starting point that validates, so the first thing someone sees is a
 *  working model rather than an empty box and a schema to guess at. */
type Format = 'toml' | 'spice'

/** A deck that loads, so the SPICE box also opens on something that works
 *  rather than an empty area and a format to guess at. The title line is not
 *  decoration: the first line of a SPICE deck is always a comment, so a deck
 *  without one silently loses its first real card. */
const SPICE_STARTER = `* my divider
.subckt divider in out
R1 in out 1k
R2 out 0 1k
.ends
V1 vin 0 5
X1 vin mid divider
`

const STARTER = `[[models]]
id = "my_resistor"
kind = "passive"
description = "what this part is, in a few words"

# Which parts on a board this entry claims. Regexes over the value field.
[models.match]
value_re = "^10k$"
`

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
 *  so the author chooses before any TOML appears. */
const KIND_CHOICES: [id: string, what: string][] = [
  ['passive', 'resistor, capacitor, inductor'], ['diode', 'diode, Schottky, Zener, LED'],
  ['bjt_npn', 'NPN transistor'], ['bjt_pnp', 'PNP transistor'],
  ['nmos', 'N-channel MOSFET'], ['pmos', 'P-channel MOSFET'],
  ['vreg', 'regulator, LDO, charger'], ['opamp', 'op-amp'], ['comparator', 'comparator'],
  ['timer', '555-class timer'], ['analog_switch', 'analogue or load switch'],
  ['digital', 'logic gate or digital part'], ['dac', 'DAC'], ['adc', 'ADC'],
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

export function WritePart({ onSaved, suggested, openSignal, boardLabel }: {
  onSaved?: () => void
  suggested?: WebOpenPart | ModelAuthoringSuggestion
  /** Changing this explicit signal opens the editor. Merely suggesting a part
   *  must not pop a panel open while a report arrives. */
  openSignal?: string | null
  boardLabel?: string
}) {
  const [open, setOpen] = useState(false)
  const [format, setFormat] = useState<Format>('toml')
  const [toml, setToml] = useState(STARTER)
  const [spice, setSpice] = useState(SPICE_STARTER)
  const body = format === 'toml' ? toml : spice
  const setBody = format === 'toml' ? setToml : setSpice
  const [part, setPart] = useState(() => suggested?.value.trim() ?? '')
  const [check, setCheck] = useState<CheckState>({ phase: 'idle' })
  // The kind chosen on the picker (null: still choosing), and whether the author
  // has touched the text. Until they have, a gap in the scaffold is a to-do the
  // page already told them about, not an error.
  const [kind, setKind] = useState<string | null>(null)
  const [edited, setEdited] = useState(false)
  const [saveMsg, setSaveMsg] = useState<string | null>(null)
  const [drafting, setDrafting] = useState(false)
  const timer = useRef<number | null>(null)
  const checkEpoch = useRef(0)

  useEffect(() => {
    if (!openSignal || !suggested) return
    setFormat('toml')
    setPart(suggested.value.trim() || suggested.reference)
    setToml(STARTER)
    setCheck({ phase: 'idle' })
    setSaveMsg(null)
    setOpen(true)
    setEdited(false)
    const upgradesExisting = 'model_id' in suggested && !!suggested.model_id &&
      !!suggested.model_kind && suggested.stage?.startsWith('executable_')
    // A new model starts on the kind picker; only extending an existing one has a kind already.
    setKind(upgradesExisting ? suggested.model_kind ?? null : null)
    if (!upgradesExisting) return
    setDrafting(true)
    let cancelled = false
    setSaveMsg('Copying the exact resolved behavior into a local extension …')
    void (async () => {
      try {
        const result = await api.modelsDraft(draftRequest(suggested, boardLabel))
        if (cancelled) return
        if (!result.ok || !result.toml) {
          setSaveMsg(result.error ?? 'the server could not prepare the resolved model')
          setDrafting(false)
          return
        }
        setToml(result.toml)
        setCheck({ phase: 'idle' })
        setDrafting(false)
        setSaveMsg('Copied the existing executable model. Review the declared gaps, edit, validate, then choose Save.')
      } catch (error) {
        if (!cancelled) {
          setSaveMsg(errorText(error))
          setDrafting(false)
        }
      }
    })()
    return () => { cancelled = true }
  }, [openSignal, suggested, boardLabel])

  // The author chose a kind: the scaffold for it replaces the placeholder. For a part on
  // the board the server draws it from the board's own identity; otherwise it is the starter.
  const pick = useCallback(async (chosen: string) => {
    setKind(chosen)
    setEdited(false)
    setCheck({ phase: 'idle' })
    if (!suggested) {
      setToml(STARTER.replace('kind = "passive"', `kind = "${chosen}"`))
      return
    }
    setDrafting(true)
    setSaveMsg(null)
    try {
      const result = await api.modelsDraft(draftRequest(suggested, boardLabel, chosen))
      if (result.ok && result.toml) setToml(result.toml)
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
    if (!open) return
    if (timer.current) window.clearTimeout(timer.current)
    const checkedBody = body
    const checkedFormat = format
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
  }, [body, format, open])

  // Effects run after render. Treat a result for older text as checking even
  // during that one-render gap, so Save can never be enabled for bytes the
  // server has not validated.
  const visibleCheck: CheckState = isModelCheckCurrent(check, body, format)
    ? check
    : { phase: 'checking', body, format }

  // A fresh scaffold's gaps are to-dos the page has already announced, so they stay amber and
  // quiet; the validator's word counts as an error only once the author has edited.
  const todo = !edited && visibleCheck.phase === 'bad' && format === 'toml'

  // Escape closes the editor, the same key that dismisses every other in-page
  // surface in this app (the layers panel, the fullscreen map, the add menu).
  //
  // Bound on the document rather than the panel, because the panel is not a
  // focus trap: the file input, the format buttons and the textarea can all
  // hold focus, and a keydown on the textarea does not reach a container
  // handler unless it is bubbled all the way. Two guards keep it from stealing
  // the key: it only listens while open, and it ignores an Escape that came
  // from a native picker or an autocomplete (`defaultPrevented`).
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
        setSaveMsg(j.coverage_note
          ? `Saved to ${j.path ? tildeHome(j.path) : 'your model directory'}. Coverage unchanged: ${j.coverage_note} Re-analyzing this board now.`
          : `Saved to ${j.path ? tildeHome(j.path) : 'your model directory'}. Re-analyzing this board now.`)
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
        onClick={() => setOpen(true)}
        className="text-[12px] rounded-lg px-3 py-1.5 cursor-pointer transition-all hover:opacity-80"
        style={{
          border: '1px solid var(--hairline)',
          background: 'var(--surface)',
          color: 'var(--silk-dim)',
        }}
      >
        {suggested ? `Write ${suggested.reference} yourself` : 'Write a part yourself'}
      </button>
    )
  }

  return (
    <div
      data-testid="write-part"
      className="rounded-xl px-4 py-3.5 mt-3"
      style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
    >
      <div className="flex items-center justify-between mb-2">
        <span className="text-[12px] font-semibold" style={{ color: 'var(--silk)' }}>
          Write a part
        </span>
        <button
          type="button"
          data-testid="write-part-close"
          onClick={() => setOpen(false)}
          className="text-[11px] rounded px-2 py-1 cursor-pointer"
          style={{ border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
        >
          Close <span style={{ color: 'var(--silk-faint)' }}>Esc</span>
        </button>
      </div>

      <div className="flex items-center gap-1.5 mb-2">
        {(['toml', 'spice'] as Format[]).map(f => (
          <button
            key={f}
            type="button"
            data-testid={`write-part-format-${f}`}
            onClick={() => setFormat(f)}
            className="text-[11px] rounded px-2.5 py-1 cursor-pointer transition-all"
            style={{
              border: `1px solid ${format === f ? 'var(--copper-deep)' : 'var(--hairline)'}`,
              background: format === f ? 'rgba(224,138,78,0.12)' : 'transparent',
              color: format === f ? 'var(--copper-hi)' : 'var(--silk-faint)',
            }}
          >
            {f === 'toml' ? 'hauksbee model' : 'SPICE'}
          </button>
        ))}
      </div>

      {format === 'toml' ? (
        <p className="text-[11px] mb-2 leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
          {suggested && <>Starting from {suggested.reference} ({suggested.value}). </>}
          Hauksbee's own model format, and the one the binder and the checks reason
          about directly. `id` names the entry, `kind` says what sort of device it is,
          and `[models.match]` decides which parts on a board it claims. Everything is
          checked as you type by the same validator that runs when you save, so what
          passes here will save.
        </p>
      ) : (
        <p className="text-[11px] mb-2 leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
          Paste a vendor model or a whole deck. Subcircuits are supported: hauksbee
          flattens a `.subckt` at load, mapping its ports to your nodes and recursing
          through nested calls, so a vendor part that ships as a subcircuit runs like
          any other. The first line of a SPICE file is its title and is always treated
          as a comment. What you see below is the real loader's answer, so a refusal
          names the line in your own file.
        </p>
      )}

      <label className="text-[11px] block mb-2" style={{ color: 'var(--silk-faint)' }}>
        <span className="block mb-1">Part number (names the saved file)</span>
        {/* 17rem is what a part number wants, not what the card always has: on a
            phone the card is narrower than that, so the width is a ceiling and the
            field takes the column below it. */}
        <input
          data-testid="write-part-name"
          className="hb-input text-[12px] block w-full"
          style={{ height: 30, maxWidth: '17rem' }}
          value={part}
          onChange={e => setPart(e.target.value)}
          placeholder="e.g. BC847B"
        />
      </label>

      {format === 'spice' && (
        <label className="text-[11px] block mb-2" style={{ color: 'var(--silk-faint)' }}>
          <span className="block mb-1">Or load a file (.lib, .mod, .cir, .sp, .txt)</span>
          <input
            data-testid="write-part-spice-file"
            type="file"
            accept=".lib,.mod,.cir,.sp,.txt,.spi,.ckt"
            // A file input's intrinsic width is its button plus the file name the
            // browser chooses to show, which is wider than a phone's card. Held to
            // the column, the UA shortens the name instead of the card losing its edge.
            className="text-[12px] block w-full max-w-full"
            style={{ color: 'var(--silk-dim)' }}
            onChange={e => {
              const f = e.target.files?.[0]
              if (!f) return
              // Read it into the same box the paste path uses, so one editor
              // and one check answer for both routes in.
              void f.text().then(t => setSpice(t))
            }}
          />
        </label>
      )}

      {format === 'toml' && kind === null && (
        <div data-testid="kind-picker">
          <div className="text-[12px] mb-1.5" style={{ color: 'var(--silk)' }}>
            What kind of part {suggested ? `is ${suggested.reference}${suggested.value ? ` (${suggested.value})` : ''}` : 'is it'}?
          </div>
          <div className="flex flex-wrap gap-1.5">
            {KIND_CHOICES.map(([id, what]) => (
              <button
                key={id}
                type="button"
                data-testid={`kind-${id}`}
                onClick={() => void pick(id)}
                className="hb-btn hb-press px-2.5 py-1 text-[11px] text-left"
                title={id}
              >
                {what}
              </button>
            ))}
          </div>
          <p className="text-[11px] mt-2" style={{ color: 'var(--silk-faint)' }}>
            A reference letter does not say what a part does, so you choose. You get a scaffold that
            is valid apart from the values only the datasheet can give.
          </p>
        </div>
      )}

      {(format === 'spice' || kind !== null) && (<>
      <textarea
        data-testid="write-part-toml"
        value={body}
        onChange={e => { setEdited(true); setBody(e.target.value) }}
        disabled={format === 'toml' && drafting}
        aria-busy={format === 'toml' && drafting}
        spellCheck={false}
        className="hb-input w-full text-[12px]"
        style={{ minHeight: 220, fontFamily: 'var(--font-mono)', lineHeight: 1.5, padding: 10 }}
      />

      <div
        data-testid="write-part-status"
        data-quiet={todo || undefined}
        className="text-[11px] mt-2 rounded-lg px-2.5 py-2 leading-relaxed"
        style={{
          background: visibleCheck.phase === 'bad' ? 'var(--warn-bg)' : 'var(--code-bg)',
          border: `1px solid ${visibleCheck.phase === 'bad' ? 'var(--warn-border)' : 'var(--hairline)'}`,
          color: visibleCheck.phase === 'bad' ? 'var(--silk)' : 'var(--silk-dim)',
          fontFamily: 'var(--font-mono)',
        }}
      >
        {visibleCheck.phase === 'checking' && 'checking ...'}
        {visibleCheck.phase === 'ok' && visibleCheck.summary}
        {visibleCheck.phase === 'bad' && (todo ? `Fill these in from the datasheet, then it will save: ${visibleCheck.error}` : visibleCheck.error)}
        {visibleCheck.phase === 'idle' && 'start typing to check it'}
      </div>

      <div className="flex items-center gap-2 mt-2.5">
        {format === 'spice' ? (
          <span className="text-[11px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
            A SPICE deck is checked here, not saved as a model. Point a board or a CI
            spec at the file to simulate it. To make a reusable part from it, switch to
            the hauksbee format and write the entry that claims your component.
          </span>
        ) : (
        <button
          type="button"
          data-testid="write-part-save"
          disabled={drafting || visibleCheck.phase !== 'ok' || part.trim().length === 0}
          onClick={() => void save()}
          className="rounded-lg px-3.5 py-1.5 text-[12px] font-semibold cursor-pointer transition-all hover:opacity-90 disabled:opacity-40 disabled:cursor-not-allowed"
          style={{
            background: 'linear-gradient(180deg, var(--copper-hi), var(--copper))',
            color: 'var(--on-copper)',
          }}
        >
          Save to my models
        </button>
        )}
        {format === 'toml' && visibleCheck.phase !== 'ok' && (
          <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            the model has to check clean before it can be saved
          </span>
        )}
      </div>

      </>)}

      {saveMsg && (
        <div className="text-[11px] mt-2" style={{ color: 'var(--silk-dim)' }}>
          {saveMsg}
        </div>
      )}
    </div>
  )
}
