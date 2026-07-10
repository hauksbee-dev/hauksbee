import { useCallback, useEffect, useRef, useState } from 'react'
import type { WebReport, WebSection, WebFinding, WebHeadsUp, WebComponent, WebCosimSection } from '../types/report'
import { CheckIcon, PlayIcon, BoardTargetIcon } from './Icons'

// The landing state (W6 §1): the drop-a-board flow and plain-language report,
// absorbed from the old server-rendered front door into the React app. Renders
// the WebReport JSON from /api/analyze (or the preloaded report from
// /api/startup when the server was launched as `hauksbee run <board> --serve`).
// The "run it" affordance expands the report into the live-sim view (SimView).

interface LandingProps {
  /** Report preloaded by the server (`run --serve`), if any. */
  preloadedReport: WebReport | null
  /** Board name the server preloaded, if any. */
  preloadedBoardName: string | null
  /** True when a live sim is being served on /ws (run --serve). */
  canRunLive: boolean
  /** Expand into the live-sim view. */
  onRunIt: () => void
}

const LEVEL_COLORS: Record<string, string> = {
  serious: '#ef4444',
  warning: '#eab308',
  note: '#475569',
}

const LEVEL_TEXT: Record<string, string> = {
  serious: '#fca5a5',
  warning: '#fde047',
  note: '#94a3b8',
}

// Copy-to-clipboard button (persona-panel fix #8): the "bring it to life" path
// was a bare, un-actionable CLI string. A one-click copy is the minimum real
// affordance short of an in-page launch (which stays out of scope).
function CopyButton({ text, label = 'Copy' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false)
  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(text)
    } catch {
      // Fallback for insecure contexts / older browsers.
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
      data-testid="copy-cli"
      onClick={copy}
      className="ml-2 rounded px-2 py-0.5 text-[11px] font-semibold cursor-pointer transition-all hover:opacity-80"
      style={{
        background: copied ? 'rgba(87,224,160,0.14)' : 'rgba(224,138,78,0.12)',
        border: `1px solid ${copied ? '#2f7d5b' : 'var(--copper-deep)'}`,
        color: copied ? 'var(--live)' : 'var(--copper-hi)',
        whiteSpace: 'nowrap',
      }}
    >
      {copied ? (
        <span className="inline-flex items-center gap-1"><CheckIcon size={11} /> Copied</span>
      ) : label}
    </button>
  )
}

// The signature: a copper trace with pad nodes and a signal pulse that runs
// along it on load — the page's one orchestrated moment, evoking "the circuit
// the copper actually implements". Quiet everywhere else (Chanel's rule).
function TraceSignature() {
  return (
    <svg
      viewBox="0 0 640 40"
      preserveAspectRatio="none"
      width="100%"
      height="40"
      aria-hidden="true"
      style={{ display: 'block' }}
    >
      {/* the trace itself (dim copper) */}
      <path
        d="M0 20 H150 L172 8 H360 L382 32 H520 L542 20 H640"
        fill="none"
        stroke="var(--copper-deep)"
        strokeWidth="2"
        opacity="0.5"
      />
      {/* the traveling pulse (bright copper) */}
      <path
        className="trace-pulse"
        d="M0 20 H150 L172 8 H360 L382 32 H520 L542 20 H640"
        fill="none"
        stroke="var(--copper-hi)"
        strokeWidth="2.5"
        strokeDasharray="60 640"
        style={{ animation: 'traceflow 4.2s cubic-bezier(0.4,0,0.2,1) infinite' }}
      />
      {/* pad nodes */}
      {[150, 360, 520].map((cx, i) => (
        <rect
          key={cx}
          className="trace-pad"
          x={cx - 4}
          y={i === 1 ? 4 : 16}
          width="8"
          height="8"
          rx="1.5"
          fill="var(--copper-deep)"
          style={{ animation: `padglow 4.2s ease-in-out ${i * 0.5}s infinite` }}
        />
      ))}
    </svg>
  )
}

export function Landing({ preloadedReport, preloadedBoardName, canRunLive, onRunIt }: LandingProps) {
  const [report, setReport] = useState<WebReport | null>(preloadedReport)
  const [busy, setBusy] = useState<string | null>(null)
  const [uploadError, setUploadError] = useState<string | null>(null)
  const [dragOver, setDragOver] = useState(false)
  const [firmwareFile, setFirmwareFile] = useState<File | null>(null)
  const lastBoardFile = useRef<File | null>(null)

  const analyze = useCallback(async (board: File, firmware: File | null) => {
    setUploadError(null)
    setBusy(firmware ? `Analyzing ${board.name} + co-sim of ${firmware.name} ...` : `Analyzing ${board.name} ...`)
    try {
      let res: Response
      if (firmware) {
        const fd = new FormData()
        fd.append('board', board, board.name)
        fd.append('firmware', firmware, firmware.name)
        res = await fetch('/api/analyze-with-firmware', { method: 'POST', body: fd })
      } else {
        res = await fetch('/api/analyze', {
          method: 'POST',
          headers: { 'X-Board-Filename': board.name, 'Content-Type': 'application/octet-stream' },
          body: await board.arrayBuffer(),
        })
      }
      // Read the body ONCE as text, then parse defensively. The server always
      // returns a WebReport JSON (even for a bad board: `{ok:false,...}`), but a
      // stale build, a proxy, or a body-limit/panic can return plaintext ("413
      // Payload Too Large", a "Failed to ..." message). Parsing that with
      // res.json() throws a cryptic "Unexpected token 'F'" SyntaxError; reading
      // text first lets us show the real message verbatim.
      const text = await res.text()
      if (!res.ok) {
        throw new Error(text.trim().slice(0, 400) || `${res.status} ${res.statusText}`)
      }
      let parsed: WebReport
      try {
        parsed = JSON.parse(text) as WebReport
      } catch {
        throw new Error(
          text.trim().slice(0, 400) ||
            'the server returned an empty or non-JSON response',
        )
      }
      setReport(parsed)
    } catch (e) {
      setUploadError(`Analysis failed: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setBusy(null)
    }
  }, [])

  const looksLikeFirmware = (name: string) => /\.(elf|hex)$/i.test(name)

  const handleFirmware = useCallback((f: File) => {
    setFirmwareFile(f)
    if (lastBoardFile.current) void analyze(lastBoardFile.current, f)
  }, [analyze])

  const handleBoard = useCallback((f: File) => {
    // A firmware file in the board slot is a mis-drop, not a board: route it to
    // the firmware jack instead of sending an ELF to the board extractor.
    if (looksLikeFirmware(f.name) && !lastBoardFile.current) {
      handleFirmware(f)
      return
    }
    lastBoardFile.current = f
    void analyze(f, firmwareFile)
  }, [analyze, firmwareFile, handleFirmware])

  return (
    <div
      className="min-h-screen overflow-y-auto"
      style={{
        background:
          'radial-gradient(120% 80% at 50% -10%, #0e1622 0%, var(--void) 60%)',
        color: 'var(--silk)',
      }}
    >
      <div className="max-w-3xl mx-auto px-6 pt-12 pb-20">
        {/* Wordmark + eyebrow */}
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2.5">
            <span
              style={{
                width: 9, height: 9, borderRadius: 2, background: 'var(--copper)',
                boxShadow: '0 0 10px var(--copper-hi)', display: 'inline-block',
              }}
            />
            <span className="text-sm font-semibold tracking-[0.28em]" style={{ color: 'var(--silk)' }}>
              HAUKSBEE
            </span>
          </div>
          <span
            className="text-[10px] font-semibold tracking-[0.22em] px-2.5 py-1 rounded"
            style={{ color: 'var(--copper)', border: '1px solid var(--rule)', background: 'rgba(224,138,78,0.05)' }}
          >
            CI FOR HARDWARE
          </span>
        </div>

        {/* Thesis (hero) */}
        <h1
          className="mt-9 text-[1.9rem] leading-[1.18] font-bold"
          style={{ color: 'var(--silk)', letterSpacing: '-0.01em' }}
        >
          Point it at a real board.<br />
          <span style={{ color: 'var(--copper)' }}>It works out the circuit the copper implements</span>
          <span style={{ color: 'var(--silk-dim)' }}> — then runs it.</span>
        </h1>
        <p className="mt-4 text-sm leading-relaxed max-w-xl" style={{ color: 'var(--silk-dim)' }}>
          Not a design-rule check, not schematic SPICE. Hauksbee extracts the as-built
          circuit from the PCB, co-simulates the firmware on an emulated MCU, and checks
          the board the way a test suite checks a function.
          <span style={{ color: 'var(--copper-hi)' }}> Nothing leaves this machine.</span>
        </p>

        {/* Signature trace */}
        <div className="mt-7 -mb-1">
          <TraceSignature />
        </div>

        {/* The board-input "slot" — an instrument affordance, not a dashed box. */}
        <label
          data-testid="drop-zone"
          htmlFor="board-file"
          onDragEnter={e => { e.preventDefault(); setDragOver(true) }}
          onDragOver={e => { e.preventDefault(); setDragOver(true) }}
          onDragLeave={e => { e.preventDefault(); setDragOver(false) }}
          onDrop={e => {
            e.preventDefault()
            setDragOver(false)
            const f = e.dataTransfer.files[0]
            if (f) handleBoard(f)
          }}
          className="slot block cursor-pointer transition-all px-6 py-9"
          data-active={dragOver ? 'true' : 'false'}
        >
          <div className="flex items-center gap-4">
            <span style={{ color: dragOver ? 'var(--copper-hi)' : 'var(--copper)', display: 'inline-flex', flexShrink: 0 }}>
              <BoardTargetIcon size={30} />
            </span>
            <div>
              <div className="text-sm font-semibold" style={{ color: 'var(--silk)' }}>
                Load a board — drop it here, or click to choose
              </div>
              <div className="text-xs mt-1" style={{ color: 'var(--silk-faint)' }}>
                KiCad <code>.kicad_pcb</code> / <code>.kicad_sch</code> · Eagle <code>.brd</code> ·
                Altium <code>.PcbDoc</code> · IPC <code>.d356</code> · a gerber <code>.zip</code>
              </div>
            </div>
          </div>
        </label>
        <input
          id="board-file"
          type="file"
          accept=".kicad_pcb,.kicad_sch,.brd,.PcbDoc,.d356,.zip,.txt"
          className="hidden"
          onChange={e => { const f = e.target.files?.[0]; if (f) handleBoard(f) }}
        />

        {/* Firmware — a secondary jack on the same instrument. */}
        <label
          data-testid="firmware-zone"
          htmlFor="firmware-file"
          onDragEnter={e => e.preventDefault()}
          onDragOver={e => e.preventDefault()}
          onDrop={e => {
            e.preventDefault()
            const f = e.dataTransfer.files[0]
            if (f) handleFirmware(f)
          }}
          className="slot slot-sub block mt-3 cursor-pointer transition-all px-6 py-4 text-sm"
          data-active={firmwareFile ? 'true' : 'false'}
        >
          <span style={{ color: firmwareFile ? 'var(--live)' : 'var(--silk-dim)' }}>
            {firmwareFile ? (
              <><CheckIcon size={12} style={{ display: 'inline', verticalAlign: '-1px' }} /> Firmware: <strong>{firmwareFile.name}</strong> — click to change</>
            ) : (
              <>+ optional firmware (<code>.elf</code> / <code>.hex</code>) — co-simulate it on the board&rsquo;s MCU</>
            )}
          </span>
        </label>
        <input
          id="firmware-file"
          type="file"
          accept=".elf,.hex"
          className="hidden"
          onChange={e => { const f = e.target.files?.[0]; if (f) handleFirmware(f) }}
        />

        {/* Progress / error */}
        {busy && (
          <div className="mt-5 text-sm flex items-center gap-2" style={{ color: 'var(--copper-hi)' }}>
            <span className="slot-spin" /> {busy}
          </div>
        )}
        {uploadError && <div className="mt-5 text-sm" style={{ color: '#fca5a5' }}>{uploadError}</div>}

        {/* Report */}
        {report && (
          <ReportView
            report={report}
            boardLabel={preloadedBoardName}
            canRunLive={canRunLive}
            onRunIt={onRunIt}
          />
        )}

        {/* What it checks — differentiators first (not DRC/SPICE). */}
        {!report && (
          <div className="mt-12 grid grid-cols-1 sm:grid-cols-3 gap-3">
            {[
              ['The as-built circuit', 'Reconstructed from the copper the board actually ships — not the schematic you drew.'],
              ['Firmware co-simulation', 'Your firmware on an emulated MCU, coupled to the live analog solve.'],
              ['Checks a test suite can’t', 'Rails, faults, shorts, USB-C, signal integrity, thermal, loop stability.'],
            ].map(([h, b]) => (
              <div key={h} className="rounded-lg p-4" style={{ background: 'var(--void-2)', border: '1px solid var(--rule)' }}>
                <div className="text-[11px] font-semibold tracking-wide mb-1.5" style={{ color: 'var(--copper)' }}>{h}</div>
                <div className="text-xs leading-relaxed" style={{ color: 'var(--silk-dim)' }}>{b}</div>
              </div>
            ))}
          </div>
        )}

        <div className="mt-10 text-xs" style={{ color: 'var(--silk-faint)' }}>
          Running locally via <code>hauksbee serve</code>. The same engine as
          <code> hauksbee run</code> on the command line and <code>hauksbee-ci</code> in a pipeline.
        </div>
      </div>
    </div>
  )
}

function ReportView({ report: r, boardLabel, canRunLive, onRunIt }: {
  report: WebReport
  boardLabel: string | null
  canRunLive: boolean
  onRunIt: () => void
}) {
  if (!r.ok) {
    return (
      <div
        data-testid="report-verdict"
        className="mt-6 rounded-lg px-4 py-3.5"
        style={{ border: '1px solid #7f1d1d', background: '#160b0b', color: '#fca5a5' }}
      >
        {r.error || 'Could not read the file.'}
      </div>
    )
  }

  const bindOpen = !!(r.bind?.active_path_unresolved?.length)
  const hasHeadsUp = (r.sections || []).some(s => s.heads_up?.length)
  const runCommand = `hauksbee run ${boardLabel ?? r.file_name} --serve`
  let verdictBorder = '#14532d', verdictBg = '#08130c'
  if (r.serious > 0) { verdictBorder = '#7f1d1d'; verdictBg = '#160b0b' }
  else if (r.total > 0 || bindOpen || hasHeadsUp) { verdictBorder = '#713f12'; verdictBg = '#141004' }

  return (
    <div data-testid="report" className="mt-6">
      {/* Verdict headline */}
      <div
        data-testid="report-verdict"
        className="rounded-lg px-4 py-3.5"
        style={{ border: `1px solid ${verdictBorder}`, background: verdictBg, fontSize: 16 }}
      >
        {r.headline}
        <div className="text-xs mt-1.5" style={{ color: '#94a3b8' }}>
          {(r.board_name || r.file_name)} · {r.num_components} parts · {r.num_nets} nets
        </div>
      </div>

      {/* Run it: expand the report into the live sim (run --serve only). */}
      {canRunLive ? (
        <button
          data-testid="run-it"
          onClick={onRunIt}
          className="mt-3 w-full rounded-lg px-4 py-3 text-sm font-bold tracking-wide cursor-pointer transition-all hover:opacity-90 flex items-center justify-center gap-2"
          style={{
            background: 'rgba(224,138,78,0.12)',
            border: '1px solid var(--copper-deep)',
            color: 'var(--copper-hi)',
            boxShadow: '0 0 14px rgba(224,138,78,0.18)',
          }}
        >
          <PlayIcon size={13} /> Drive it live — the board, a scope, and firmware in real time
        </button>
      ) : (
        <div
          data-testid="run-it-hint"
          className="mt-3 rounded-lg px-4 py-3 text-xs"
          style={{ border: '1px solid #1e293b', background: '#0a0f1e', color: '#64748b' }}
        >
          <div>To bring this board to life (live scope, 2D/3D view, transport controls) run:</div>
          <div className="mt-1.5 flex items-center flex-wrap">
            <code style={{ color: '#94a3b8', background: '#0f172a', padding: '2px 6px', borderRadius: 4 }}>
              {runCommand}
            </code>
            <CopyButton text={runCommand} />
          </div>
        </div>
      )}

      {/* Bind-honesty banner */}
      {bindOpen && (
        <div
          className="mt-3 rounded-lg px-4 py-3"
          style={{ border: '1px solid #713f12', background: '#141004' }}
        >
          <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: '#fde047' }}>
            Active parts unresolved
          </span>
          <div className="text-sm mt-1" style={{ color: '#e7d8ad' }}>
            {r.bind!.active_path_unresolved!.join(', ')} could not be bound or are left open on the
            live circuit. Analog / AC / thermal results on their nets are not trustworthy.
          </div>
        </div>
      )}

      {/* Top-level honesty notes */}
      {(r.notes || []).map((n, i) => (
        <div
          key={i}
          className="mt-3 rounded-lg px-4 py-2.5"
          style={{ border: '1px solid #1e293b', borderLeft: '4px solid #475569', background: '#0a0f1e' }}
        >
          <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: '#94a3b8' }}>Note</span>
          <div className="text-sm mt-0.5" style={{ color: '#cbd5e1' }}>{n.message}</div>
        </div>
      ))}

      {/* Check sections */}
      {r.sections.map((s, i) => <SectionBlock key={i} section={s} />)}

      {/* Firmware co-sim */}
      {r.cosim && <CosimBlock cosim={r.cosim} />}

      {/* 2D board map */}
      {r.components?.length > 0 && (
        <section className="mt-7">
          <h2 className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: '#64748b' }}>
            Board map (2D)
          </h2>
          <BoardMap components={r.components} />
        </section>
      )}
    </div>
  )
}

/** A run of findings that share level + why + fix (same-shaped): the DRC
 *  clearance case where 128 warnings differ only in which net-pair/location. */
interface FindingGroup {
  level: string
  why: string
  fix: string
  whats: string[]
}

/** Collapse same-shaped findings so the shared explanation is shown ONCE
 *  (persona-panel fix #6). Order-independent: any findings with identical
 *  level/why/fix merge, no matter where they sit in the list. Nothing is hidden
 *  — every individual `what` is still listed, just under one explanation. */
function groupFindings(findings: WebFinding[]): FindingGroup[] {
  const groups: FindingGroup[] = []
  for (const f of findings) {
    const g = groups.find(x => x.level === f.level && x.why === f.why && x.fix === f.fix)
    if (g) g.whats.push(f.what)
    else groups.push({ level: f.level, why: f.why, fix: f.fix, whats: [f.what] })
  }
  return groups
}

function SectionBlock({ section: s }: { section: WebSection }) {
  const groups = groupFindings(s.findings)
  return (
    <section className="mt-7">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: '#64748b' }}>
        {s.title}
      </h2>
      <div className="text-sm mb-2" style={{ color: '#94a3b8' }}>{s.verdict}</div>
      {groups.map((g, i) =>
        g.whats.length === 1
          ? <FindingCard key={i} finding={{ level: g.level, what: g.whats[0], why: g.why, fix: g.fix }} />
          : <GroupedFindingCard key={i} group={g} />
      )}
      {(s.heads_up || []).map((h, i) => <HeadsUpCard key={i} note={h} />)}
    </section>
  )
}

// A heads-up note with the finding's what / why / what-to-do gloss. `why`/`fix`
// render only when present (self-contained notes carry just `what`).
function HeadsUpCard({ note: h }: { note: WebHeadsUp }) {
  return (
    <div
      className="rounded-lg px-4 py-2.5 mb-2"
      style={{ border: '1px solid var(--rule)', borderLeft: '4px solid var(--copper)', background: 'var(--void-2)' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--copper)' }}>Heads up</span>
      <div className="text-sm mt-0.5" style={{ color: '#cbd5e1' }}>{h.what}</div>
      {h.why && (
        <div className="text-sm mt-1" style={{ color: '#cbd5e1' }}>
          <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>Why it matters:</b> {h.why}
        </div>
      )}
      {h.fix && (
        <div className="text-sm mt-0.5" style={{ color: '#cbd5e1' }}>
          <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>What to do:</b> {h.fix}
        </div>
      )}
    </div>
  )
}

// A collapsed group of same-shaped findings: the shared level + explanation are
// shown once, and the individual items live inside an expandable list so a long
// run (e.g. 128 clearance warnings) never walls the page, yet hides nothing.
function GroupedFindingCard({ group: g }: { group: FindingGroup }) {
  const accent = LEVEL_COLORS[g.level] ?? '#475569'
  const tagColor = LEVEL_TEXT[g.level] ?? '#94a3b8'
  const n = g.whats.length
  return (
    <div
      data-testid="grouped-finding"
      className="rounded-lg px-4 py-3 mb-2"
      style={{ border: '1px solid #1e293b', borderLeft: `4px solid ${accent}`, background: '#0a0f1e' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor }}>
        {g.level} · {n} similar
      </span>
      <div className="font-semibold text-sm mt-1 mb-1.5">
        {n} similar findings — same cause, listed once below.
      </div>
      <details className="mb-1.5">
        <summary className="text-sm cursor-pointer" style={{ color: '#94a3b8' }}>
          Show all {n}
        </summary>
        <ul className="mt-1.5 pl-4 text-sm" style={{ color: '#cbd5e1', listStyleType: 'disc' }}>
          {g.whats.map((w, i) => <li key={i} className="my-0.5">{w}</li>)}
        </ul>
      </details>
      {g.why && (
        <div className="text-sm my-0.5">
          <b style={{ color: '#94a3b8', fontWeight: 600 }}>Why it matters:</b> {g.why}
        </div>
      )}
      {g.fix && (
        <div className="text-sm my-0.5">
          <b style={{ color: '#94a3b8', fontWeight: 600 }}>What to do:</b> {g.fix}
        </div>
      )}
    </div>
  )
}

function FindingCard({ finding: f }: { finding: WebFinding }) {
  const accent = LEVEL_COLORS[f.level] ?? '#475569'
  const tagColor = LEVEL_TEXT[f.level] ?? '#94a3b8'
  return (
    <div
      className="rounded-lg px-4 py-3 mb-2"
      style={{ border: '1px solid #1e293b', borderLeft: `4px solid ${accent}`, background: '#0a0f1e' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor }}>
        {f.level}
      </span>
      <div className="font-semibold text-sm mt-1 mb-1.5">{f.what}</div>
      <div className="text-sm my-0.5">
        <b style={{ color: '#94a3b8', fontWeight: 600 }}>Why it matters:</b> {f.why}
      </div>
      <div className="text-sm my-0.5">
        <b style={{ color: '#94a3b8', fontWeight: 600 }}>What to do:</b> {f.fix}
      </div>
    </div>
  )
}

function CosimBlock({ cosim: c }: { cosim: WebCosimSection }) {
  return (
    <section className="mt-7" data-testid="cosim-section">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: '#64748b' }}>
        Firmware co-sim
      </h2>
      {c.ran ? (
        <>
          <div className="text-sm mb-2" style={{ color: '#94a3b8' }}>
            Ran the firmware for {(c.seconds_simulated || 0).toFixed(3)}s on the board's microcontroller.
          </div>
          {(!c.findings || c.findings.length === 0) && (
            <div
              className="rounded-lg px-4 py-2.5 mb-2"
              style={{ border: '1px solid #1e293b', borderLeft: '4px solid #475569', background: '#0a0f1e' }}
            >
              <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: '#94a3b8' }}>Note</span>
              <div className="text-sm mt-0.5" style={{ color: '#cbd5e1' }}>
                No electrical-stress faults during the run.
              </div>
            </div>
          )}
          {(c.findings || []).map((f, i) => <FindingCard key={i} finding={f} />)}
          {c.uart_output && (
            <>
              <div className="text-sm mb-1"><b style={{ color: '#94a3b8', fontWeight: 600 }}>UART output:</b></div>
              <pre
                className="rounded-lg px-3 py-2.5 mb-2 text-xs overflow-x-auto whitespace-pre-wrap"
                style={{
                  background: '#050d1a',
                  border: '1px solid #1e293b',
                  color: '#cbd5e1',
                  fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
                }}
              >
                {c.uart_output}
              </pre>
            </>
          )}
          {c.gpio_nets && c.gpio_nets.length > 0 && (
            <table className="w-full text-xs mt-1" style={{ borderCollapse: 'collapse' }}>
              <thead>
                <tr>
                  {['Net', 'Volts', 'Activity'].map(h => (
                    <th
                      key={h}
                      className="text-left px-2 py-1 font-semibold"
                      style={{ color: '#94a3b8', borderBottom: '1px solid #1e293b' }}
                    >
                      {h}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {c.gpio_nets.map((g, i) => (
                  <tr key={i}>
                    <td className="px-2 py-1" style={{ borderBottom: '1px solid #0f172a' }}>{g.name}</td>
                    <td className="px-2 py-1 font-mono" style={{ borderBottom: '1px solid #0f172a' }}>
                      {(g.volts || 0).toFixed(3)}
                    </td>
                    <td
                      className="px-2 py-1"
                      style={{ borderBottom: '1px solid #0f172a', color: g.driven ? '#4ade80' : '#475569' }}
                    >
                      {g.driven ? 'driven' : 'idle'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </>
      ) : (
        (c.findings && c.findings.length > 0 ? c.findings : [{
          level: 'note', what: 'Co-sim not available for this board.', why: '', fix: '',
        }]).map((f, i) => (
          <div
            key={i}
            className="rounded-lg px-4 py-2.5 mb-2"
            style={{ border: '1px solid #1e293b', borderLeft: '4px solid #475569', background: '#0a0f1e' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: '#94a3b8' }}>
              Co-sim not available
            </span>
            <div className="text-sm mt-0.5" style={{ color: '#cbd5e1' }}>{`${f.what} ${f.why}`.trim()}</div>
          </div>
        ))
      )}
    </section>
  )
}

// Simple 2D footprint dot map, drawn from the report's component positions
// (board mm). Same rendering the old front door page did on its canvas.
function BoardMap({ components }: { components: WebComponent[] }) {
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const cv = canvasRef.current
    if (!cv) return
    const ctx = cv.getContext('2d')
    if (!ctx) return
    const W = cv.width, H = cv.height, pad = 28
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity
    for (const c of components) {
      minX = Math.min(minX, c.x); minY = Math.min(minY, c.y)
      maxX = Math.max(maxX, c.x); maxY = Math.max(maxY, c.y)
    }
    const spanX = Math.max(1e-6, maxX - minX)
    const spanY = Math.max(1e-6, maxY - minY)
    const scale = Math.min((W - 2 * pad) / spanX, (H - 2 * pad) / spanY)
    ctx.clearRect(0, 0, W, H)
    for (const c of components) {
      const x = pad + (c.x - minX) * scale
      const y = pad + (c.y - minY) * scale
      ctx.fillStyle = '#3b82f6'
      ctx.beginPath(); ctx.arc(x, y, 3, 0, Math.PI * 2); ctx.fill()
      ctx.fillStyle = '#64748b'; ctx.font = '10px sans-serif'
      ctx.fillText(c.reference, x + 5, y + 3)
    }
  }, [components])

  return (
    <canvas
      ref={canvasRef}
      width={760}
      height={460}
      className="w-full rounded-lg block"
      style={{ background: '#050d1a', border: '1px solid #1e293b' }}
    />
  )
}
