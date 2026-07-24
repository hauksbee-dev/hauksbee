import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { parse as parseToml } from 'smol-toml'
import type { WebReport } from '../types/report'

// The web checks builder: compose the body of a hauksbee-ci spec with plain
// language, run it through the REAL hauksbee-ci binary (`POST /api/check`
// shells the sibling install), and keep the artifact: download the exact
// spec.toml a pipeline would run, or the GitHub workflow that runs it on every
// push. State auto-saves per board (localStorage) and auto-restores when the
// same board is analyzed again; a dropped/edited raw TOML takes over when the
// builder's vocabulary runs out.

interface SupplyRow {
  net: string
  volts: string
}

// One plain-language check. `kind` maps 1:1 onto the spec's [[assert]] kinds;
// fields are kept as strings so the inputs stay honest about what was typed.
interface CheckRow {
  id: number
  kind: string
  net: string
  ref: string
  min: string
  max: string
  after_ms: string
  deadline_ms: string
  contains: string
  freq_hz: string
  tolerance: string
  min_toggles: string
  amps: string
  celsius: string
  dip_below: string
  for_max_ms: string
  recover_to: string
  recover_within_ms: string
}

const emptyCheck = (id: number, kind: string, net = ''): CheckRow => ({
  id, kind, net,
  ref: '', min: '', max: '', after_ms: '', deadline_ms: '', contains: '',
  freq_hz: '', tolerance: '', min_toggles: '', amps: '', celsius: '',
  dip_below: '', for_max_ms: '', recover_to: '', recover_within_ms: '',
})

// The add-a-check menu: plain words first, the TOML kind in small print.
const CHECK_KINDS: { kind: string; label: string; hint: string }[] = [
  { kind: 'voltage', label: 'A net must sit at a voltage', hint: 'min/max volts, optionally after a settle time' },
  { kind: 'no_faults', label: 'Nothing over-stressed', hint: 'no component beyond its ratings at any point' },
  { kind: 'uart', label: 'The firmware must print', hint: 'serial output contains a string' },
  { kind: 'toggle', label: 'A net must blink', hint: 'toggle frequency or a minimum toggle count' },
  { kind: 'boot-coverage', label: 'Firmware must drive a net by a deadline', hint: 'a gate/enable must be actively driven after reset' },
  { kind: 'max_current', label: 'A part must stay under a current', hint: 'ceiling in amps for one component' },
  { kind: 'max_temp', label: 'A part must stay cool', hint: 'junction temperature ceiling (or the part’s own rating)' },
  { kind: 'rail_window', label: 'A rail may only dip briefly', hint: 'bound brownout depth, duration and recovery' },
]

// Kinds that carry a net / a component ref. Shared by the TOML composer, the
// raw parser's round-trip check, and the per-kind field pickers so the three
// cannot drift apart.
const NET_KINDS = ['voltage', 'toggle', 'boot-coverage', 'rail_window']
const REF_KINDS = ['max_current', 'max_temp']

// The published GitHub action, pinned to a release tag (the version in
// Cargo.toml), never a moving branch: a workflow generated today must not
// change behavior when main moves. Bump on each hauksbee release.
const ACTION_REF = 'ETM-Code/hauksbee/integrations/github-action@v0.1.0'

interface CheckResult {
  label: string
  kind: string
  passed: boolean
  invalid: boolean
  detail: string
}

interface RunResponse {
  ok: boolean
  error?: string
  passed?: boolean
  exit_code?: number
  analog_abort?: boolean
  coverage?: string | null
  substitutions?: string[]
  coverage_warnings?: string[]
  results?: CheckResult[]
}

function tomlString(v: string): string {
  return JSON.stringify(v)
}

function numOr(v: string): string | null {
  const t = v.trim()
  if (t === '') return null
  return Number.isFinite(Number(t)) ? t : null
}

/** Compose the spec BODY (no board/firmware keys — the server injects those
 *  from the uploaded files). */
function buildToml(name: string, duration: string, supplies: SupplyRow[], checks: CheckRow[]): string {
  let out = `name = ${tomlString(name)}\n`
  const dur = numOr(duration)
  if (dur) out += `duration_ms = ${dur}\n`
  for (const s of supplies) {
    if (!s.net.trim()) continue
    out += `\n[[supply]]\nnet = ${tomlString(s.net.trim())}\nkind = "ideal"\nvolts = ${numOr(s.volts) ?? '5.0'}\n`
  }
  for (const c of checks) {
    out += `\n[[assert]]\nkind = ${tomlString(c.kind)}\n`
    const put = (key: string, v: string, quote = false) => {
      const t = v.trim()
      if (!t) return
      out += quote ? `${key} = ${tomlString(t)}\n` : (numOr(v) ? `${key} = ${numOr(v)}\n` : '')
    }
    if (NET_KINDS.includes(c.kind)) put('net', c.net, true)
    if (REF_KINDS.includes(c.kind)) put('ref', c.ref, true)
    if (c.kind === 'uart') put('contains', c.contains, true)
    put('min', c.min)
    put('max', c.max)
    put('after_ms', c.after_ms)
    put('deadline_ms', c.deadline_ms)
    put('freq_hz', c.freq_hz)
    put('tolerance', c.tolerance)
    put('min_toggles', c.min_toggles)
    put('amps', c.amps)
    put('celsius', c.celsius)
    put('dip_below', c.dip_below)
    put('for_max_ms', c.for_max_ms)
    put('recover_to', c.recover_to)
    put('recover_within_ms', c.recover_within_ms)
  }
  return out
}

// Fields the builder round-trips on a [[supply]] / an [[assert]]. Anything
// outside these sets (an assertion scenario, a ripple spec, a sensor...) would
// be silently destroyed on the way back to the builder, so its presence makes
// tomlToBuilder refuse the conversion.
const SUPPLY_FIELDS = new Set(['net', 'kind', 'volts'])
const ASSERT_FIELDS = new Set([
  'kind', 'net', 'ref', 'min', 'max', 'after_ms', 'deadline_ms', 'contains',
  'freq_hz', 'tolerance', 'min_toggles', 'amps', 'celsius', 'dip_below',
  'for_max_ms', 'recover_to', 'recover_within_ms',
])

/** Best-effort: load a raw TOML back into builder rows. Returns null when the
 *  spec uses vocabulary the builder doesn't cover (an unknown top-level key,
 *  OR any nested supply/assert field the builder would not write back out);
 *  the caller then stays in raw mode, or warns before discarding. */
function tomlToBuilder(raw: string): { name: string; duration: string; supplies: SupplyRow[]; checks: CheckRow[] } | null {
  let doc: Record<string, unknown>
  try {
    doc = parseToml(raw) as Record<string, unknown>
  } catch {
    return null
  }
  const KNOWN = new Set(['name', 'duration_ms', 'supply', 'assert', 'board', 'firmware', 'mcu'])
  if (Object.keys(doc).some(k => !KNOWN.has(k))) return null
  const supplies: SupplyRow[] = []
  for (const s of (doc.supply as Record<string, unknown>[] | undefined) ?? []) {
    if (Object.keys(s).some(k => !SUPPLY_FIELDS.has(k))) return null
    if (s.kind && s.kind !== 'ideal') return null
    supplies.push({ net: String(s.net ?? ''), volts: String(s.volts ?? '') })
  }
  const checks: CheckRow[] = []
  let id = 1
  for (const a of (doc.assert as Record<string, unknown>[] | undefined) ?? []) {
    const kind = String(a.kind ?? '')
    if (!CHECK_KINDS.some(k => k.kind === kind)) return null
    // Refuse any field the composer would not re-emit for this kind: it would
    // survive the parse but vanish from the round-tripped spec.
    for (const key of Object.keys(a)) {
      if (!ASSERT_FIELDS.has(key)) return null
      if (key === 'net' && !NET_KINDS.includes(kind)) return null
      if (key === 'ref' && !REF_KINDS.includes(kind)) return null
      if (key === 'contains' && kind !== 'uart') return null
    }
    const row = emptyCheck(id++, kind)
    const grab = (key: keyof CheckRow, tomlKey?: string) => {
      const v = a[tomlKey ?? key]
      if (v !== undefined) (row[key] as string) = String(v)
    }
    grab('net'); grab('ref'); grab('min'); grab('max'); grab('after_ms'); grab('deadline_ms')
    grab('contains'); grab('freq_hz'); grab('tolerance'); grab('min_toggles'); grab('amps')
    grab('celsius'); grab('dip_below'); grab('for_max_ms'); grab('recover_to'); grab('recover_within_ms')
    checks.push(row)
  }
  return {
    name: String(doc.name ?? 'board checks'),
    duration: String(doc.duration_ms ?? '100'),
    supplies,
    checks,
  }
}

const inputStyle: React.CSSProperties = {
  background: '#0a0f1e',
  border: '1px solid #1e293b',
  borderRadius: 6,
  color: '#cbd5e1',
  padding: '4px 8px',
  fontSize: 13,
}

function Field({ label, value, onChange, width = 90, placeholder }: {
  label: string
  value: string
  onChange: (v: string) => void
  width?: number
  placeholder?: string
}) {
  return (
    <label className="inline-flex items-center gap-1.5 text-[12px]" style={{ color: '#64748b' }}>
      {label}
      <input
        style={{ ...inputStyle, width }}
        value={value}
        placeholder={placeholder}
        onChange={e => onChange(e.target.value)}
      />
    </label>
  )
}

/** Shape of the autosaved localStorage payload (all fields best-effort: a
 *  corrupt or partial save must never break the report). */
interface SavedChecksState {
  specName?: string
  duration?: string
  supplies?: SupplyRow[]
  checks?: CheckRow[]
  rawMode?: boolean
  rawText?: string
}

/** Storage key for a board's saved checks. The file name alone collides for
 *  common names (every project has a board.kicad_pcb), so a cheap fingerprint
 *  from the report disambiguates. Landing also uses this as the panel's React
 *  key so the panel remounts per board: the mount-time restore is then
 *  authoritative and one board's state can never leak into another's. */
export function checksStorageKey(report: WebReport): string {
  return `hauksbee.checks.${report.file_name}:${report.num_components}:${report.num_nets}`
}

export function ChecksPanel({ report, boardFile, firmwareFile, selectedNet }: {
  report: WebReport
  boardFile: File | null
  firmwareFile: File | null
  /** Net last clicked on the board render — offered as a one-click check. */
  selectedNet: string | null
}) {
  const storageKey = checksStorageKey(report)
  // ── Restore a saved session for this board (auto-load). Because the panel
  // remounts per board (keyed on storageKey by the parent), restoring in the
  // state initializers is race-free: the autosave effect cannot fire before
  // the restore has happened, and a board with no saved state starts from the
  // report's own defaults instead of inheriting the previous board's rows. ──
  const saved = useMemo<SavedChecksState | null>(() => {
    try {
      const raw = localStorage.getItem(storageKey)
      return raw ? (JSON.parse(raw) as SavedChecksState) : null
    } catch {
      return null
    }
  }, [storageKey])
  const savedSupplies = saved?.supplies
  const savedChecks = saved?.checks
  const initialChecks = Array.isArray(savedChecks) && savedChecks.length
    ? savedChecks
    : [emptyCheck(1, 'no_faults')]
  const [specName, setSpecName] = useState(saved?.specName || `${report.board_name || report.file_name} checks`)
  const [duration, setDuration] = useState(saved?.duration || '100')
  const [supplies, setSupplies] = useState<SupplyRow[]>(
    () => (Array.isArray(savedSupplies) && savedSupplies.length
      ? savedSupplies
      : (report.supplies ?? []).map(s => ({ net: s.net, volts: String(s.volts) }))),
  )
  const [checks, setChecks] = useState<CheckRow[]>(initialChecks)
  const [rawMode, setRawMode] = useState(!!(saved?.rawMode && typeof saved.rawText === 'string'))
  const [rawText, setRawText] = useState(typeof saved?.rawText === 'string' ? saved.rawText : '')
  const [addOpen, setAddOpen] = useState(false)
  const [running, setRunning] = useState(false)
  const [result, setResult] = useState<RunResponse | null>(null)
  const [ciOpen, setCiOpen] = useState(false)
  const nextId = useRef(initialChecks.reduce((m, c) => Math.max(m, c.id), 0) + 1)

  const builtToml = useMemo(
    () => buildToml(specName, duration, supplies, checks),
    [specName, duration, supplies, checks],
  )
  const effectiveToml = rawMode ? rawText : builtToml

  // ── Auto-save (the "things auto load in future" contract). ──
  useEffect(() => {
    try {
      localStorage.setItem(storageKey, JSON.stringify({ specName, duration, supplies, checks, rawMode, rawText }))
    } catch { /* storage full/blocked: the session still works */ }
  }, [storageKey, specName, duration, supplies, checks, rawMode, rawText])

  const addCheck = (kind: string, net = '') => {
    setChecks(cs => [...cs, emptyCheck(nextId.current++, kind, net)])
    setAddOpen(false)
  }

  const update = (id: number, patch: Partial<CheckRow>) => {
    setChecks(cs => cs.map(c => (c.id === id ? { ...c, ...patch } : c)))
  }

  const runChecks = useCallback(async () => {
    if (!boardFile) return
    setRunning(true)
    setResult(null)
    try {
      const fd = new FormData()
      fd.append('board', boardFile, boardFile.name)
      if (firmwareFile) fd.append('firmware', firmwareFile, firmwareFile.name)
      fd.append('spec', new Blob([effectiveToml], { type: 'text/plain' }), 'spec.toml')
      const res = await fetch('/api/check', { method: 'POST', body: fd })
      const text = await res.text()
      try {
        setResult(JSON.parse(text) as RunResponse)
      } catch {
        setResult({ ok: false, error: text.trim().slice(0, 400) || `${res.status} ${res.statusText}` })
      }
    } catch (e) {
      setResult({ ok: false, error: e instanceof Error ? e.message : String(e) })
    } finally {
      setRunning(false)
    }
  }, [boardFile, firmwareFile, effectiveToml])

  const specStem = (report.file_name || 'board').replace(/\.[^.]+$/, '')
  const specForDownload = () => {
    // The downloadable spec IS runnable: it carries the board/firmware paths
    // relative to the recommended repo layout. The builder composes a
    // fragment (it never writes board/firmware lines), but a raw spec may be
    // a full file that already names its paths; prepend a key only when the
    // spec text does not already carry it, so nothing is doubled.
    const hasKey = (key: string) => new RegExp(`^\\s*${key}\\s*=`, 'm').test(effectiveToml)
    let head = ''
    if (!hasKey('board')) head += `board = ${tomlString(`../hardware/${report.file_name}`)}\n`
    if (firmwareFile && !hasKey('firmware')) head += `firmware = ${tomlString(`../firmware/${firmwareFile.name}`)}\n`
    return head ? `${head}\n${effectiveToml}` : effectiveToml
  }

  const download = (name: string, contents: string) => {
    const a = document.createElement('a')
    a.href = URL.createObjectURL(new Blob([contents], { type: 'text/plain' }))
    a.download = name
    a.click()
    URL.revokeObjectURL(a.href)
  }

  const workflowYml = `# .github/workflows/hauksbee-ci.yml
name: hardware-ci
on:
  push:
    paths: ["hardware/**", "firmware/**", "ci/**.toml"]
  pull_request:
    paths: ["hardware/**", "firmware/**", "ci/**.toml"]
jobs:
  checks:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: hauksbee-ci
        uses: ${ACTION_REF} # pin to a release tag
        with:
          spec: ci/${specStem}.toml
          junit: hauksbee-ci-results.xml
`

  const netOptions = report.nets ?? []

  return (
    <section className="mt-7" data-testid="checks-panel">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: '#64748b' }}>
        Checks — make this board's rules repeatable
      </h2>
      <div className="rounded-lg px-4 py-4" style={{ background: '#0a0f1e', border: '1px solid #1e293b' }}>
        <div className="text-[13px] leading-relaxed mb-3" style={{ color: '#94a3b8' }}>
          Pick what must hold (a rail voltage, a blink, a print, nothing over-stressed), run it
          here, then take the spec file with you — the same file <code>hauksbee-ci</code> runs in a
          pipeline. Click a trace on the board map to start a check on that net.
        </div>

        {/* datalist for every net picker */}
        <datalist id="net-options">
          {netOptions.map(n => <option key={n} value={n} />)}
        </datalist>

        {selectedNet && !rawMode && (
          <button
            type="button"
            data-testid="quick-add-net"
            onClick={() => addCheck('voltage', selectedNet)}
            className="mb-3 px-3 py-1.5 rounded-lg text-[12px] cursor-pointer"
            style={{ background: 'rgba(224,138,78,0.12)', border: '1px solid #7c4a1e', color: '#ffb072' }}
          >
            + Check a voltage on “{selectedNet}” (clicked on the map)
          </button>
        )}

        {!rawMode && (
          <>
            {/* Power */}
            <div className="text-[12px] font-semibold mb-1.5" style={{ color: '#8fa0b3' }}>
              Power (detected from the board — adjust if wrong)
            </div>
            {supplies.map((s, i) => (
              <div key={i} className="flex items-center gap-2 mb-1.5">
                <input
                  style={{ ...inputStyle, width: 180 }}
                  list="net-options"
                  value={s.net}
                  placeholder="net (e.g. +5V)"
                  onChange={e => setSupplies(ss => ss.map((x, j) => (j === i ? { ...x, net: e.target.value } : x)))}
                />
                <Field label="volts" value={s.volts} width={70}
                  onChange={v => setSupplies(ss => ss.map((x, j) => (j === i ? { ...x, volts: v } : x)))} />
                <button type="button" className="text-[12px] cursor-pointer" style={{ color: '#475569', background: 'none', border: 'none' }}
                  onClick={() => setSupplies(ss => ss.filter((_, j) => j !== i))}>remove</button>
              </div>
            ))}
            <button type="button" className="text-[12px] cursor-pointer mb-3" style={{ color: '#64748b', background: 'none', border: 'none' }}
              onClick={() => setSupplies(ss => [...ss, { net: '', volts: '5.0' }])}>+ add a supply</button>

            {/* Checks */}
            <div className="text-[12px] font-semibold mb-1.5" style={{ color: '#8fa0b3' }}>Checks</div>
            {checks.map(c => {
              const meta = CHECK_KINDS.find(k => k.kind === c.kind)
              return (
                <div key={c.id} className="rounded-lg px-3 py-2.5 mb-2" style={{ background: '#0d1526', border: '1px solid #1e293b' }}>
                  <div className="flex items-center justify-between">
                    <div className="text-[13px]" style={{ color: '#cbd5e1' }}>
                      {meta?.label ?? c.kind} <code className="ml-1 text-[11px]" style={{ color: '#475569' }}>{c.kind}</code>
                    </div>
                    <button type="button" className="text-[12px] cursor-pointer" style={{ color: '#475569', background: 'none', border: 'none' }}
                      onClick={() => setChecks(cs => cs.filter(x => x.id !== c.id))}>remove</button>
                  </div>
                  <div className="mt-2 flex flex-wrap gap-x-4 gap-y-2">
                    {NET_KINDS.includes(c.kind) && (
                      <label className="inline-flex items-center gap-1.5 text-[12px]" style={{ color: '#64748b' }}>
                        net
                        <input style={{ ...inputStyle, width: 170 }} list="net-options" value={c.net}
                          onChange={e => update(c.id, { net: e.target.value })} />
                      </label>
                    )}
                    {REF_KINDS.includes(c.kind) && (
                      <Field label="part (ref)" value={c.ref} width={90} placeholder="U1"
                        onChange={v => update(c.id, { ref: v })} />
                    )}
                    {c.kind === 'voltage' && (
                      <>
                        <Field label="min V" value={c.min} width={64} onChange={v => update(c.id, { min: v })} />
                        <Field label="max V" value={c.max} width={64} onChange={v => update(c.id, { max: v })} />
                        <Field label="after ms" value={c.after_ms} width={64} onChange={v => update(c.id, { after_ms: v })} />
                      </>
                    )}
                    {c.kind === 'uart' && (
                      <Field label="must print" value={c.contains} width={220} placeholder="hello"
                        onChange={v => update(c.id, { contains: v })} />
                    )}
                    {c.kind === 'toggle' && (
                      <>
                        <Field label="freq Hz" value={c.freq_hz} width={64} onChange={v => update(c.id, { freq_hz: v })} />
                        <Field label="±tol" value={c.tolerance} width={56} onChange={v => update(c.id, { tolerance: v })} />
                        <Field label="or min toggles" value={c.min_toggles} width={64} onChange={v => update(c.id, { min_toggles: v })} />
                      </>
                    )}
                    {c.kind === 'boot-coverage' && (
                      <>
                        <Field label="reach V" value={c.min} width={64} onChange={v => update(c.id, { min: v })} />
                        <Field label="within ms" value={c.deadline_ms} width={64} onChange={v => update(c.id, { deadline_ms: v })} />
                      </>
                    )}
                    {c.kind === 'max_current' && (
                      <Field label="max A" value={c.amps} width={64} onChange={v => update(c.id, { amps: v })} />
                    )}
                    {c.kind === 'max_temp' && (
                      <Field label="max °C (blank = part rating)" value={c.celsius} width={70}
                        onChange={v => update(c.id, { celsius: v })} />
                    )}
                    {c.kind === 'rail_window' && (
                      <>
                        <Field label="dip below V" value={c.dip_below} width={64} onChange={v => update(c.id, { dip_below: v })} />
                        <Field label="for max ms" value={c.for_max_ms} width={64} onChange={v => update(c.id, { for_max_ms: v })} />
                        <Field label="recover to V" value={c.recover_to} width={64} onChange={v => update(c.id, { recover_to: v })} />
                        <Field label="within ms" value={c.recover_within_ms} width={64} onChange={v => update(c.id, { recover_within_ms: v })} />
                      </>
                    )}
                  </div>
                </div>
              )
            })}

            <div className="relative">
              <button type="button" data-testid="add-check" className="px-3 py-1.5 rounded-lg text-[13px] cursor-pointer"
                style={{ background: '#0d1526', border: '1px solid #1e293b', color: '#94a3b8' }}
                onClick={() => setAddOpen(o => !o)}>+ Add a check</button>
              {addOpen && (
                <div className="absolute z-10 mt-1 rounded-lg overflow-hidden" style={{ background: '#0d1526', border: '1px solid #1e293b', width: 380 }}>
                  {CHECK_KINDS.map(k => (
                    <button key={k.kind} type="button" onClick={() => addCheck(k.kind)}
                      className="block w-full text-left px-3 py-2 cursor-pointer"
                      style={{ background: 'none', border: 'none' }}>
                      <div className="text-[13px]" style={{ color: '#cbd5e1' }}>{k.label}</div>
                      <div className="text-[11px]" style={{ color: '#475569' }}>{k.hint}</div>
                    </button>
                  ))}
                </div>
              )}
            </div>

            <div className="mt-3">
              <Field label="run length (ms)" value={duration} width={70} onChange={setDuration} />
              <span className="ml-3 text-[12px]" style={{ color: '#475569' }}>
                {firmwareFile ? `firmware: ${firmwareFile.name} (co-simulated)` : 'no firmware loaded: add it just below this panel to check UART/blink/boot'}
              </span>
            </div>
          </>
        )}

        {/* TOML: the artifact itself. Builder mode previews it; raw mode owns it. */}
        <div className="mt-4">
          <div className="flex items-center justify-between mb-1">
            <div className="text-[12px] font-semibold" style={{ color: '#8fa0b3' }}>
              The spec (TOML) — this exact text is what runs
            </div>
            <button type="button" data-testid="raw-toggle" className="text-[12px] cursor-pointer"
              style={{ color: '#64748b', background: 'none', border: 'none' }}
              onClick={() => {
                if (!rawMode) {
                  setRawText(builtToml)
                  setRawMode(true)
                } else {
                  const parsed = tomlToBuilder(rawText)
                  if (parsed) {
                    // Raw is the source of truth on a successful parse: an
                    // intentionally emptied list clears the builder rows too.
                    setSpecName(parsed.name)
                    setDuration(parsed.duration)
                    setSupplies(parsed.supplies)
                    setChecks(parsed.checks)
                    nextId.current = parsed.checks.reduce((m, c) => Math.max(m, c.id), 0) + 1
                    setRawMode(false)
                  } else if (window.confirm(
                    'This TOML uses features the visual builder does not cover '
                    + '(tolerances, scenarios, overrides, sensors...). Going back to the '
                    + 'builder will DISCARD the raw text. Continue?')) {
                    setRawMode(false)
                  }
                }
              }}>
              {rawMode ? '← back to the builder' : 'edit the TOML directly →'}
            </button>
          </div>
          {rawMode ? (
            <textarea
              data-testid="raw-toml"
              value={rawText}
              onChange={e => setRawText(e.target.value)}
              spellCheck={false}
              className="w-full rounded-lg p-3"
              style={{ ...inputStyle, minHeight: 220, fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: 1.5 }}
            />
          ) : (
            <pre className="rounded-lg p-3 overflow-x-auto text-[12px] leading-relaxed"
              style={{ background: '#050d1a', border: '1px solid #1e293b', color: '#7d8ca0', fontFamily: 'var(--font-mono)' }}>
              {builtToml}
            </pre>
          )}
        </div>

        {/* Actions */}
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <button type="button" data-testid="run-checks" disabled={!boardFile || running}
            onClick={() => void runChecks()}
            className="px-4 py-2 rounded-lg text-[13px] font-semibold cursor-pointer"
            style={{
              background: boardFile && !running ? 'linear-gradient(180deg, var(--copper-hi), var(--copper))' : '#1e293b',
              color: boardFile && !running ? '#2a1c0f' : '#475569',
              border: 'none',
            }}>
            {running ? 'Running…' : 'Run these checks now'}
          </button>
          <button type="button" className="px-3 py-2 rounded-lg text-[13px] cursor-pointer"
            style={{ background: '#0d1526', border: '1px solid #1e293b', color: '#94a3b8' }}
            onClick={() => download(`${specStem}.toml`, specForDownload())}>
            Download {specStem}.toml
          </button>
          <button type="button" className="px-3 py-2 rounded-lg text-[13px] cursor-pointer"
            style={{ background: '#0d1526', border: '1px solid #1e293b', color: '#94a3b8' }}
            onClick={() => setCiOpen(o => !o)}>
            {ciOpen ? 'Hide GitHub CI setup' : 'Set up GitHub CI'}
          </button>
          {!boardFile && (
            <span className="text-[12px]" style={{ color: '#475569' }}>
              (running from here needs the board uploaded in this session)
            </span>
          )}
        </div>

        {/* GitHub CI setup: the two files and where they go. */}
        {ciOpen && (
          <div className="mt-3 rounded-lg px-3 py-3" style={{ background: '#0d1526', border: '1px solid #1e293b' }}>
            <div className="text-[13px] mb-2" style={{ color: '#94a3b8' }}>
              Two files make this run on every push. Recommended layout: board in{' '}
              <code>hardware/</code>, firmware in <code>firmware/</code>, this spec in <code>ci/</code>.
            </div>
            <div className="text-[12px] mb-1" style={{ color: '#8fa0b3' }}>
              1. <code>ci/{specStem}.toml</code> — the Download button above produces it (paths
              already relative to that layout).
            </div>
            <div className="text-[12px] mb-1.5" style={{ color: '#8fa0b3' }}>
              2. <code>.github/workflows/hauksbee-ci.yml</code>:
            </div>
            <pre className="rounded-lg p-3 overflow-x-auto text-[11px] leading-relaxed"
              style={{ background: '#050d1a', border: '1px solid #1e293b', color: '#7d8ca0', fontFamily: 'var(--font-mono)' }}>
              {workflowYml}
            </pre>
            <button type="button" className="mt-1 px-3 py-1.5 rounded-lg text-[12px] cursor-pointer"
              style={{ background: '#0a0f1e', border: '1px solid #1e293b', color: '#94a3b8' }}
              onClick={() => download('hauksbee-ci.yml', workflowYml)}>
              Download hauksbee-ci.yml
            </button>
          </div>
        )}

        {/* Results */}
        {result && (
          <div className="mt-3" data-testid="check-results" aria-live="polite">
            {result.ok === false ? (
              <div className="rounded-lg px-3 py-2.5 text-[13px]"
                style={{ background: '#160b0b', border: '1px solid #7f1d1d', color: '#fca5a5' }}>
                {result.error}
              </div>
            ) : (
              <>
                <div className="rounded-lg px-3 py-2.5 text-[14px] font-semibold"
                  style={result.passed
                    ? { background: '#08130c', border: '1px solid #14532d', color: '#86efac' }
                    : { background: '#160b0b', border: '1px solid #7f1d1d', color: '#fca5a5' }}>
                  {result.passed ? 'All checks passed.' : 'Checks failed.'}
                  {result.analog_abort && ' (analog solve aborted — results not trustworthy)'}
                </div>
                {(result.results ?? []).map((r, i) => (
                  <div key={i} className="mt-1.5 rounded-lg px-3 py-2 text-[13px] flex gap-2"
                    style={{ background: '#0d1526', border: '1px solid #1e293b' }}>
                    <span style={{ color: r.invalid ? '#fbbf24' : r.passed ? '#4ade80' : '#f87171', fontWeight: 700 }}>
                      {r.invalid ? 'INVALID' : r.passed ? 'PASS' : 'FAIL'}
                    </span>
                    <span style={{ color: '#cbd5e1' }}>{r.label}</span>
                    <span style={{ color: '#64748b' }}>{r.detail}</span>
                  </div>
                ))}
                {(result.substitutions ?? []).map((s, i) => (
                  <div key={`s${i}`} className="mt-1.5 text-[12px]" style={{ color: '#fbbf24' }}>substitute core: {s}</div>
                ))}
                {(result.coverage_warnings ?? []).map((s, i) => (
                  <div key={`c${i}`} className="mt-1 text-[12px]" style={{ color: '#fbbf24' }}>coverage: {s}</div>
                ))}
              </>
            )}
          </div>
        )}
      </div>
    </section>
  )
}
