import { useCallback, useEffect, useRef, useState } from 'react'

// Write a part by hand, in hauksbee's native TOML, with the real validator
// answering as you type.
//
// Datasheet extraction covers "I have a PDF and want a draft". It does nothing
// for someone who already knows their part, and that person previously had no
// route at all except editing a file on disk and restarting the server.
//
// The validation comes from POST /api/models/check, which runs the SAME checks
// the save path runs. Writing a friendlier client-side validator was the
// tempting shortcut and would have been the wrong one: an editor that accepts
// what the save refuses teaches an author their model is fine and then loses
// their work at the last step.

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
value = ["^10k$"]
`

type CheckState =
  | { phase: 'idle' }
  | { phase: 'checking' }
  | { phase: 'ok'; summary: string }
  | { phase: 'bad'; error: string }

export function WritePart({ onSaved }: { onSaved?: () => void }) {
  const [open, setOpen] = useState(false)
  const [format, setFormat] = useState<Format>('toml')
  const [toml, setToml] = useState(STARTER)
  const [spice, setSpice] = useState(SPICE_STARTER)
  const body = format === 'toml' ? toml : spice
  const setBody = format === 'toml' ? setToml : setSpice
  const [part, setPart] = useState('')
  const [check, setCheck] = useState<CheckState>({ phase: 'idle' })
  const [saveMsg, setSaveMsg] = useState<string | null>(null)
  const timer = useRef<number | null>(null)

  // Debounced, because this runs the real validator on the server and a request
  // per keystroke would queue behind itself while someone types a paragraph.
  useEffect(() => {
    if (!open) return
    if (timer.current) window.clearTimeout(timer.current)
    setCheck({ phase: 'checking' })
    timer.current = window.setTimeout(() => {
      void (async () => {
        try {
          const res = await fetch('/api/models/check', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ toml: body, format }),
          })
          const j = (await res.json()) as { ok?: boolean; summary?: string; error?: string }
          if (j.ok) setCheck({ phase: 'ok', summary: j.summary ?? 'valid' })
          else setCheck({ phase: 'bad', error: j.error ?? 'the check did not say why' })
        } catch (e) {
          setCheck({ phase: 'bad', error: e instanceof Error ? e.message : String(e) })
        }
      })()
    }, 400)
    return () => { if (timer.current) window.clearTimeout(timer.current) }
  }, [body, format, open])

  const save = useCallback(async () => {
    setSaveMsg(null)
    try {
      const res = await fetch('/api/models/save', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ part: part.trim() || 'model', kind: '', toml: body }),
      })
      const j = (await res.json()) as { ok?: boolean; path?: string; error?: string }
      if (j.ok === false) setSaveMsg(j.error ?? 'the save failed and did not say why')
      else {
        setSaveMsg(`Saved to ${j.path ?? 'your model directory'}. Re-analyze the board to use it.`)
        onSaved?.()
      }
    } catch (e) {
      setSaveMsg(e instanceof Error ? e.message : String(e))
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
        Write a part yourself
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
          onClick={() => setOpen(false)}
          className="text-[11px] rounded px-2 py-1 cursor-pointer"
          style={{ border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
        >
          Close
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
        <input
          data-testid="write-part-name"
          className="hb-input text-[12px]"
          style={{ height: 30, width: '17rem' }}
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
            className="text-[12px]"
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

      <textarea
        data-testid="write-part-toml"
        value={body}
        onChange={e => setBody(e.target.value)}
        spellCheck={false}
        className="hb-input w-full text-[12px]"
        style={{ minHeight: 220, fontFamily: 'var(--font-mono)', lineHeight: 1.5, padding: 10 }}
      />

      <div
        data-testid="write-part-status"
        className="text-[11px] mt-2 rounded-lg px-2.5 py-2 leading-relaxed"
        style={{
          background: check.phase === 'bad' ? 'var(--warn-bg)' : 'var(--code-bg)',
          border: `1px solid ${check.phase === 'bad' ? 'var(--warn-border)' : 'var(--hairline)'}`,
          color: check.phase === 'bad' ? 'var(--silk)' : 'var(--silk-dim)',
          fontFamily: 'var(--font-mono)',
        }}
      >
        {check.phase === 'checking' && 'checking ...'}
        {check.phase === 'ok' && check.summary}
        {check.phase === 'bad' && check.error}
        {check.phase === 'idle' && 'start typing to check it'}
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
          disabled={check.phase !== 'ok' || part.trim().length === 0}
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
        {format === 'toml' && check.phase !== 'ok' && (
          <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            the model has to check clean before it can be saved
          </span>
        )}
      </div>

      {saveMsg && (
        <div className="text-[11px] mt-2" style={{ color: 'var(--silk-dim)' }}>
          {saveMsg}
        </div>
      )}
    </div>
  )
}
