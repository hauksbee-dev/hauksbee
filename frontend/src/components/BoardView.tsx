import { useCallback, useEffect, useRef, useState } from 'react'
import type { WebSection, WebFinding, WebHeadsUp, WebComponent, WebCosimSection } from '../types/report'
import type { BoardSession } from '../hooks/useBoardSession'
import { CheckIcon } from './Icons'
import { BoardViewer } from './BoardViewer'
import { SelectionCard } from './SelectionCard'
import { FirmwareJack } from './FirmwareJack'

// The Board view with a report in hand: the viewer as the hero surface (with
// its toolbar and layers panel), the plain-language verdict, and the findings.
// Landing/UploadView owns getting a board in; everything here owns saying what
// came back and letting the board be explored.

const LEVEL_ACCENT: Record<string, string> = {
  serious: 'var(--err)',
  warning: 'var(--warn)',
  note: 'var(--note-accent)',
}

const LEVEL_TEXT: Record<string, string> = {
  serious: 'var(--err-strong)',
  warning: 'var(--warn-strong)',
  note: 'var(--note)',
}

// Copy-to-clipboard button for the CLI fallback path.
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
      className="hb-press ml-2 rounded px-2 py-0.5 text-[11px] font-semibold cursor-pointer"
      style={{
        background: copied ? 'var(--ok-bg)' : 'var(--copper-tint)',
        border: `1px solid ${copied ? 'var(--ok-border)' : 'var(--copper-deep)'}`,
        color: copied ? 'var(--ok)' : 'var(--copper-hi)',
        whiteSpace: 'nowrap',
      }}
    >
      {copied ? (
        <span className="inline-flex items-center gap-1"><CheckIcon size={11} /> Copied</span>
      ) : label}
    </button>
  )
}

export function BoardView({ session, onQueueCheck, onDriveLive, simMounted }: {
  session: BoardSession
  onQueueCheck: (check: { kind: string; net?: string; ref?: string }) => void
  onDriveLive: () => void
  simMounted: boolean
}) {
  const r = session.report!
  const {
    boardUrl, selectedNet, selectedComponent, setSelectedNet, setSelectedComponent,
    busy, uploadError, firmwareFile, handleFirmware, boardFile, boardLabel, liveMode, onEmptyBoard,
  } = session

  if (!r.ok) {
    return (
      <div className="h-full overflow-y-auto view-enter">
        <div className="max-w-3xl mx-auto px-6 pt-8 pb-16">
          <div
            data-testid="report-verdict"
            className="rounded-lg px-4 py-3.5"
            style={{ border: '1px solid var(--err-border)', background: 'var(--err-bg)', color: 'var(--err-strong)' }}
          >
            {r.error || 'Could not read the file.'}
          </div>
          {/* The dead end must not be dead: offer the retry inline instead of
              sending the user hunting for the header button. */}
          <div className="mt-4 flex flex-wrap items-center gap-3">
            <label
              htmlFor="board-file"
              data-testid="try-another-file"
              role="button"
              tabIndex={0}
              onKeyDown={e => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault()
                  document.getElementById('board-file')?.click()
                }
              }}
              className="hb-btn-primary hb-press inline-flex items-center px-3.5 text-[13px] cursor-pointer"
              style={{ height: 32 }}
            >
              Try another file
            </label>
            <span className="text-[12px]" style={{ color: 'var(--silk-dim)' }}>
              accepted: KiCad, Eagle, Altium, IPC-D-356, gerber zip, .board
            </span>
          </div>
        </div>
      </div>
    )
  }

  // "Show on board": pan/zoom the map to a finding's board location and drop
  // a labeled marker there. Only wired when the real renderer is drawing
  // (the dot map has no camera to move).
  const [focusPoint, setFocusPoint] = useState<{ x: number; y: number; label: string; seq: number } | null>(null)
  const focusSeq = useRef(0)
  const mapRef = useRef<HTMLDivElement>(null)
  const locate = useCallback((x: number, y: number, label: string) => {
    focusSeq.current += 1
    setFocusPoint({ x, y, label, seq: focusSeq.current })
    mapRef.current?.scrollIntoView({ behavior: 'smooth', block: 'center' })
  }, [])

  const bindOpen = !!(r.bind?.active_path_unresolved?.length)
  const hasHeadsUp = (r.sections || []).some(s => s.heads_up?.length)
  const runCommand = `hauksbee run ${boardLabel ?? r.file_name} --serve`
  let verdictBorder = 'var(--ok-border)', verdictBg = 'var(--ok-bg)'
  if (r.serious > 0) { verdictBorder = 'var(--err-border)'; verdictBg = 'var(--err-bg)' }
  else if (r.total > 0 || bindOpen || hasHeadsUp) { verdictBorder = 'var(--warn-border)'; verdictBg = 'var(--warn-bg)' }

  return (
    <div className="h-full overflow-y-auto view-enter" data-testid="report">
      <div className="max-w-4xl mx-auto px-6 pt-5 pb-16">
        {/* Re-analysis in progress (firmware added or swapped): the report
            stays visible, this line says what is happening. */}
        {busy && (
          <div
            role="status"
            aria-live="polite"
            className="mb-4 text-sm flex items-center justify-center gap-2"
            style={{ color: 'var(--copper-hi)' }}
          >
            <span className="slot-spin" />
            {busy.firmware
              ? <>Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span> + co-sim of <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.firmware}</span> ...</>
              : <>Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span> ...</>}
          </div>
        )}
        {uploadError && (
          <div
            aria-live="polite"
            className="mb-4 rounded-lg px-4 py-3 text-sm text-center"
            style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
          >
            {uploadError}
          </div>
        )}

        {/* Verdict headline */}
        <div
          data-testid="report-verdict"
          className="rounded-xl px-4 py-3.5"
          style={{ border: `1px solid ${verdictBorder}`, background: verdictBg, fontSize: 15.5 }}
        >
          {r.headline}
          <div className="text-xs mt-1.5 tnum" style={{ color: 'var(--silk-dim)' }}>
            {(r.board_name || r.file_name)} · {r.num_components}{' '}
            {r.num_components === 1 ? 'part' : 'parts'} · {r.num_nets}{' '}
            {r.num_nets === 1 ? 'net' : 'nets'}
          </div>
        </div>

        {/* No live capability registered: the CLI hint remains (the header's
            primary action covers the launch/reconnect cases). */}
        {liveMode === 'none' && (
          <div
            data-testid="run-it-hint"
            className="mt-3 rounded-lg px-4 py-3 text-xs"
            style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
          >
            <div>To bring this board to life (live scope, 2D/3D view, transport controls) run:</div>
            <div className="mt-1.5 flex items-center flex-wrap">
              <code className="hb-code" style={{ padding: '2px 6px', fontSize: 11 }}>
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
            style={{ border: '1px solid var(--warn-border)', background: 'var(--warn-bg)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--warn-strong)' }}>
              Active parts unresolved
            </span>
            <div className="text-sm mt-1" style={{ color: 'var(--silk)' }}>
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
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--note)' }}>Note</span>
            <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{n.message}</div>
          </div>
        ))}

        {/* Board map: the real renderer (pads, outline, pan/zoom, layers)
            whenever the uploaded file is KiCad layout text; the dot map only
            as the fallback for formats the client cannot draw. */}
        {boardUrl ? (
          <section className="mt-6">
            <div
              ref={mapRef}
              className="rounded-xl overflow-hidden"
              style={{
                height: 'clamp(420px, 52vh, 620px)',
                position: 'relative',
                border: '1px solid var(--hairline)',
                boxShadow: 'var(--shadow-card)',
              }}
            >
              <BoardViewer
                boardFile={boardUrl}
                frame={null}
                selectedNet={selectedNet}
                netOptions={r.nets}
                focusPoint={focusPoint}
                onNetClick={setSelectedNet}
                onFootprintClick={fp => setSelectedComponent({
                  ref: fp.ref, value: fp.value, lib_id: fp.lib_id,
                  padNet: fp.padNet, padNets: fp.padNets,
                })}
                onEmptyBoard={onEmptyBoard}
              />
              {/* Floating selection card, same language as the live sim. */}
              {(selectedNet || selectedComponent) && (
                <div className="absolute bottom-3 left-3 z-10">
                  <SelectionCard
                    net={selectedNet}
                    component={selectedComponent}
                    boundKind={selectedComponent ? r.component_kinds?.[selectedComponent.ref] ?? null : null}
                    onQueueCheck={onQueueCheck}
                    onClose={() => { setSelectedNet(null); setSelectedComponent(null) }}
                    onPickNet={setSelectedNet}
                  />
                </div>
              )}
            </div>
            <div className="mt-1.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
              Scroll to zoom · drag to pan · hover a trace to see its net · click a trace or a
              part to start a check on it
            </div>
          </section>
        ) : r.components?.length > 0 ? (
          <section className="mt-6">
            <h2 className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: 'var(--silk-faint)' }}>
              Board map (2D)
            </h2>
            <BoardMap components={r.components} />
          </section>
        ) : null}

        {/* Check sections */}
        {r.sections.map((s, i) => (
          <SectionBlock key={i} section={s} onLocate={boardUrl ? locate : undefined} />
        ))}

        {/* Firmware co-sim */}
        {r.cosim && (
          <CosimBlock
            cosim={r.cosim}
            liveAvailable={liveMode !== 'none'}
            onDriveLive={onDriveLive}
            simMounted={simMounted}
          />
        )}

        {/* The board file is still in hand, so firmware can be added or
            swapped without starting the board over. */}
        {boardFile && !busy && (
          <div className="mt-5">
            <FirmwareJack firmware={firmwareFile} placement="report" onFile={handleFirmware} locked={!!busy} />
          </div>
        )}
      </div>
    </div>
  )
}

/** A run of findings that share level + why + fix (same-shaped): the DRC
 *  clearance case where 128 warnings differ only in which net-pair/location.
 *  Each item keeps its own `what` AND its own board location (if any). */
interface FindingGroup {
  level: string
  why: string
  fix: string
  items: { what: string; x?: number; y?: number }[]
}

/** Pan-the-map callback for findings that carry board coordinates. */
type LocateFn = (x: number, y: number, label: string) => void

/** Collapse same-shaped findings so the shared explanation is shown ONCE.
 *  Order-independent: any findings with identical level/why/fix merge, no
 *  matter where they sit in the list. Nothing is hidden; every individual
 *  `what` is still listed, just under one explanation. */
function groupFindings(findings: WebFinding[]): FindingGroup[] {
  const groups: FindingGroup[] = []
  for (const f of findings) {
    const item = { what: f.what, x: f.x, y: f.y }
    const g = groups.find(x => x.level === f.level && x.why === f.why && x.fix === f.fix)
    if (g) g.items.push(item)
    else groups.push({ level: f.level, why: f.why, fix: f.fix, items: [item] })
  }
  return groups
}

function SectionBlock({ section: s, onLocate }: { section: WebSection; onLocate?: LocateFn }) {
  const groups = groupFindings(s.findings)
  return (
    <section className="mt-7">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--silk-faint)' }}>
        {s.title}
      </h2>
      <div className="text-sm mb-2" style={{ color: 'var(--silk-dim)' }}>{s.verdict}</div>
      {groups.map((g, i) =>
        g.items.length === 1
          ? (
            <FindingCard
              key={i}
              finding={{ level: g.level, what: g.items[0].what, why: g.why, fix: g.fix, x: g.items[0].x, y: g.items[0].y }}
              onLocate={onLocate}
            />
          )
          : <GroupedFindingCard key={i} group={g} onLocate={onLocate} />
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
      style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--copper)', background: 'var(--surface)' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--copper)' }}>Heads up</span>
      <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{h.what}</div>
      {h.why && (
        <div className="text-sm mt-1" style={{ color: 'var(--silk)' }}>
          <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>Why it matters:</b> {h.why}
        </div>
      )}
      {h.fix && (
        <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>
          <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>What to do:</b> {h.fix}
        </div>
      )}
    </div>
  )
}

// A collapsed group of same-shaped findings: the shared level + explanation are
// shown once, and the individual items live inside an expandable list so a long
// run (e.g. 128 clearance warnings) never walls the page, yet hides nothing.
function GroupedFindingCard({ group: g, onLocate }: { group: FindingGroup; onLocate?: LocateFn }) {
  const accent = LEVEL_ACCENT[g.level] ?? 'var(--note-accent)'
  const tagColor = LEVEL_TEXT[g.level] ?? 'var(--note)'
  const n = g.items.length
  return (
    <div
      data-testid="grouped-finding"
      className="rounded-lg px-4 py-3 mb-2"
      style={{ border: '1px solid var(--hairline)', borderLeft: `4px solid ${accent}`, background: 'var(--surface)' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor }}>
        {g.level} · {n} similar
      </span>
      <div className="font-semibold text-sm mt-1 mb-1.5">
        {n} similar findings, same cause, listed once below.
      </div>
      <details className="mb-1.5">
        <summary className="text-sm cursor-pointer" style={{ color: 'var(--silk-dim)' }}>
          Show all {n}
        </summary>
        <ul className="mt-1.5 pl-4 text-sm" style={{ color: 'var(--silk)', listStyleType: 'disc' }}>
          {g.items.map((it, i) => (
            <li key={i} className="my-0.5">
              {it.what}
              {onLocate && it.x !== undefined && it.y !== undefined && (
                <button
                  type="button"
                  data-testid="finding-locate"
                  onClick={() => onLocate(it.x!, it.y!, it.what)}
                  className="hb-press ml-2 cursor-pointer text-[11px]"
                  style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)', textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)' }}
                >
                  show on board
                </button>
              )}
            </li>
          ))}
        </ul>
      </details>
      {g.why && (
        <div className="text-sm my-0.5">
          <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>Why it matters:</b> {g.why}
        </div>
      )}
      {g.fix && (
        <div className="text-sm my-0.5">
          <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>What to do:</b> {g.fix}
        </div>
      )}
    </div>
  )
}

function FindingCard({ finding: f, onLocate }: { finding: WebFinding; onLocate?: LocateFn }) {
  const accent = LEVEL_ACCENT[f.level] ?? 'var(--note-accent)'
  const tagColor = LEVEL_TEXT[f.level] ?? 'var(--note)'
  const locatable = onLocate !== undefined && f.x !== undefined && f.y !== undefined
  const locate = locatable ? () => onLocate!(f.x!, f.y!, f.what) : undefined
  return (
    <div
      data-testid="finding-card"
      onClick={locate}
      className="rounded-lg px-4 py-3 mb-2"
      style={{
        border: '1px solid var(--hairline)', borderLeft: `4px solid ${accent}`,
        background: 'var(--surface)', cursor: locatable ? 'pointer' : undefined,
      }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor }}>
        {f.level}
      </span>
      {locatable && (
        <button
          type="button"
          data-testid="finding-locate"
          onClick={e => { e.stopPropagation(); locate!() }}
          className="hb-press ml-2 cursor-pointer text-[11px]"
          style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)', textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)' }}
        >
          show on board
        </button>
      )}
      <div className="font-semibold text-sm mt-1 mb-1.5">{f.what}</div>
      <div className="text-sm my-0.5">
        <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>Why it matters:</b> {f.why}
      </div>
      <div className="text-sm my-0.5">
        <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>What to do:</b> {f.fix}
      </div>
    </div>
  )
}

function CosimBlock({ cosim: c, liveAvailable, onDriveLive, simMounted }: {
  cosim: WebCosimSection
  liveAvailable: boolean
  onDriveLive: () => void
  simMounted: boolean
}) {
  return (
    <section className="mt-7" data-testid="cosim-section">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--silk-faint)' }}>
        Firmware co-sim
      </h2>
      {c.ran ? (
        <>
          <div className="text-sm mb-2" style={{ color: 'var(--silk-dim)' }}>
            Ran the firmware for {(c.seconds_simulated || 0).toFixed(3)}s on the board's microcontroller.
          </div>
          {(!c.findings || c.findings.length === 0) && (
            <div
              className="rounded-lg px-4 py-2.5 mb-2"
              style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)' }}
            >
              <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--note)' }}>Note</span>
              <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>
                No electrical-stress faults during the run.
              </div>
            </div>
          )}
          {(c.findings || []).map((f, i) => <FindingCard key={i} finding={f} />)}
          {c.uart_output && (
            <>
              <div className="text-sm mb-1"><b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>UART output:</b></div>
              <pre
                className="rounded-lg px-3 py-2.5 mb-2 text-xs overflow-x-auto whitespace-pre-wrap"
                style={{
                  background: 'var(--instrument)',
                  border: '1px solid var(--instrument-edge)',
                  color: '#cbd5e1',
                  fontFamily: 'var(--font-mono)',
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
                      style={{ color: 'var(--silk-dim)', borderBottom: '1px solid var(--hairline)' }}
                    >
                      {h}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {c.gpio_nets.map((g, i) => (
                  <tr key={i}>
                    <td className="px-2 py-1" style={{ borderBottom: '1px solid var(--rule)' }}>{g.name}</td>
                    <td className="px-2 py-1 tnum" style={{ borderBottom: '1px solid var(--rule)', fontFamily: 'var(--font-mono)' }}>
                      {(g.volts || 0).toFixed(3)}
                    </td>
                    <td
                      className="px-2 py-1"
                      style={{ borderBottom: '1px solid var(--rule)', color: g.driven ? 'var(--ok)' : 'var(--silk-faint)' }}
                    >
                      {g.driven ? 'driven' : 'idle'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {/* The story continues past the summary: what to do with a finished
              co-sim, so landing firmware is a beginning, not a dead end. */}
          <div
            data-testid="cosim-next"
            className="mt-3 rounded-lg px-4 py-2.5 text-[13px]"
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--copper)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
          >
            <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>Where to go from here:</b>{' '}
            {liveAvailable ? (
              <>
                <button
                  type="button"
                  onClick={onDriveLive}
                  className="hb-press cursor-pointer"
                  style={{
                    background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)',
                    textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)', fontSize: 13,
                  }}
                >
                  {simMounted ? 'open the live sim' : 'drive it live'}
                </button>
                {' '}to boot this firmware interactively (scope, serial console, sliders), or{' '}
              </>
            ) : ''}
            turn what you just saw into repeatable checks in the Checks view; a UART print, a
            blink, a rail that must hold. The same spec then runs in CI on every push.
          </div>
        </>
      ) : (
        (c.findings && c.findings.length > 0 ? c.findings : [{
          level: 'note', what: 'Co-sim not available for this board.', why: '', fix: '',
        }]).map((f, i) => (
          <div
            key={i}
            className="rounded-lg px-4 py-2.5 mb-2"
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--note)' }}>
              Co-sim not available
            </span>
            <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{`${f.what} ${f.why}`.trim()}</div>
          </div>
        ))
      )}
    </section>
  )
}

// Simple 2D footprint dot map, drawn from the report's component positions
// (board mm), for formats the client-side renderer cannot draw. Stays on the
// dark instrument surface in both themes.
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
    // Labels only while they can be read: past a few hundred parts every
    // reference overlaps its neighbours and the map collapses into a grey
    // smear. Dense boards get clean position dots; the real geometry lives in
    // the BoardViewer path.
    const drawLabels = components.length <= 300
    const dotR = components.length > 1000 ? 1.5 : 3
    for (const c of components) {
      const x = pad + (c.x - minX) * scale
      const y = pad + (c.y - minY) * scale
      ctx.fillStyle = '#e08a4e'
      ctx.beginPath(); ctx.arc(x, y, dotR, 0, Math.PI * 2); ctx.fill()
      if (drawLabels) {
        ctx.fillStyle = '#8fa0b3'; ctx.font = '10px sans-serif'
        ctx.fillText(c.reference, x + 5, y + 3)
      }
    }
    if (!drawLabels) {
      ctx.fillStyle = '#475569'; ctx.font = '11px sans-serif'
      ctx.fillText(`${components.length} parts (labels hidden at this density)`, pad, H - 10)
    }
  }, [components])

  return (
    <canvas
      ref={canvasRef}
      width={760}
      height={460}
      className="w-full rounded-lg block"
      style={{ background: 'var(--instrument)', border: '1px solid var(--instrument-edge)' }}
    />
  )
}
