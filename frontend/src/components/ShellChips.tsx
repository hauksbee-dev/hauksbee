import { useCallback, useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { WebReport } from '../types/report'
import { plural, reportVerdictTone, summarizeEvidence } from '../lib/report-view'
import type { SimShellStatus } from '../SimView'
import { useGlossary, useTermEntry } from './ui'
import type { ChecksSummary } from './ChecksView'
import type { AppView } from './Sidebar'

// The header's status chips. Each one is a claim about a surface, and clicking
// it goes there; what is claimed depends on which surface you are already on,
// because the sim view is bound to the SESSION's board while every other view
// is about the analyzed one.

type ChipTone = 'ok' | 'err' | 'warn' | 'quiet'

const TONES: Record<ChipTone, React.CSSProperties> = {
  ok: { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' },
  err: { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)' },
  warn: { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--warn)' },
  quiet: { background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' },
}

interface Chip {
  label: string
  tone: ChipTone
  testid: string
  onClick?: () => void
  pulse?: boolean
  /** One line saying what the chip means, shown on hover or focus together
   *  with `actions`. */
  explain?: string
  actions?: { label: string; testid: string; onClick: () => void }[]
  /** A glossary term the chip's verdict word is: its meaning becomes `explain`. */
  term?: string
}

/** The chip's explanation and actions. Portalled and fixed-positioned: the
 *  chip row scrolls horizontally, which would clip an absolutely positioned
 *  child. Stays open while the pointer or focus is on the chip or the popover. */
function ExplainedChip({ chip }: { chip: Chip }) {
  const id = useId()
  const anchor = useRef<HTMLSpanElement>(null)
  const closeTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null)
  const open = useCallback(() => {
    clearTimeout(closeTimer.current)
    const r = anchor.current?.getBoundingClientRect()
    if (r) setPos({ top: r.bottom + 6, left: Math.max(8, Math.min(r.left, window.innerWidth - 328)) })
  }, [])
  const close = useCallback(() => {
    clearTimeout(closeTimer.current)
    closeTimer.current = setTimeout(() => setPos(null), 150)
  }, [])
  useEffect(() => () => clearTimeout(closeTimer.current), [])
  useEffect(() => {
    if (!pos) return
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setPos(null) }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [pos])
  return (
    <>
      <span
        ref={anchor}
        className="inline-flex shrink-0"
        aria-describedby={pos ? id : undefined}
        onMouseEnter={open}
        onMouseLeave={close}
        onFocus={open}
        onBlur={close}
      >
        <ChipPill chip={chip} />
      </span>
      {pos && createPortal(
        <div
          id={id}
          role="tooltip"
          data-testid={`${chip.testid}-explain`}
          onMouseEnter={open}
          onMouseLeave={close}
          onFocus={open}
          onBlur={close}
          className="rounded-lg px-3 py-2.5 text-[12px] leading-relaxed"
          style={{
            position: 'fixed', top: pos.top, left: pos.left, width: 320, zIndex: 60,
            background: 'var(--surface)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)',
            boxShadow: '0 8px 24px rgba(0,0,0,0.35)',
          }}
        >
          {chip.explain}
          <div className="mt-2 flex flex-wrap gap-2">
            {chip.actions?.map(a => (
              <button
                key={a.testid}
                type="button"
                data-testid={a.testid}
                onClick={() => { setPos(null); a.onClick() }}
                className="hb-btn hb-press px-2.5 text-[12px]"
                style={{ height: 28 }}
              >
                {a.label}
              </button>
            ))}
          </div>
        </div>,
        document.body,
      )}
    </>
  )
}

function ChipButton({ chip }: { chip: Chip }) {
  const entry = useTermEntry(chip.term)
  const shown = entry && !chip.explain ? { ...chip, explain: `${entry.short} What to do: ${entry.what_to_do}` } : chip
  return shown.explain ? <ExplainedChip chip={shown} /> : <ChipPill chip={shown} />
}

function ChipPill({ chip }: { chip: Chip }) {
  return (
    <button
      type="button"
      data-testid={chip.testid}
      onClick={chip.onClick}
      disabled={!chip.onClick}
      // A chip is a pill with one line in it. Left shrinkable, "checks 2 passed"
      // wraps to two lines inside a 24px pill and spills out the bottom of it,
      // so the chip keeps its own width and the row clips whole chips instead.
      className="hb-press inline-flex items-center gap-1.5 px-2.5 rounded-full text-[11px] font-semibold tnum whitespace-nowrap shrink-0"
      style={{ ...TONES[chip.tone], height: 24, cursor: chip.onClick ? 'pointer' : 'default' }}
    >
      {chip.pulse && (
        <span className="run-dot" style={{ width: 6, height: 6, borderRadius: 3, background: 'currentColor', display: 'inline-block' }} />
      )}
      {chip.label}
    </button>
  )
}

export function ShellChips({
  view, report, checks, sim, simMounted, simOffline, sessionBoard, sessionMatchesCurrent,
  currentBoard, serverLiveActive, goTo, onOpenLiveSession, onDriveLive,
}: {
  view: AppView
  report: WebReport | null
  checks: ChecksSummary | null
  sim: SimShellStatus
  simMounted: boolean
  /** A session that was live and lost its socket. Not "paused": that reads as
   *  a sim sitting there waiting for a play button. `simMounted` is what proves
   *  there WAS a session to lose. */
  simOffline: boolean
  /** The live session's identity, as the session itself reports it. */
  sessionBoard: string | null
  /** THIS page launched (or preloaded) the session for the analyzed board. A
   *  same-named session from a previous page-load is deliberately foreign. */
  sessionMatchesCurrent: boolean
  /** The analyzed board's name, to tell "the same file, started elsewhere" from
   *  "a different board". */
  currentBoard: string | null
  serverLiveActive: boolean
  goTo: (view: AppView) => void
  onOpenLiveSession: () => void
  onDriveLive: () => void
}) {
  useGlossary(report?.glossary)
  const chips: Chip[] = []
  // Fault chips count faulted PARTS (the card in the sim rail counts its own
  // fault conditions and labels itself); every surface must say what it counts.
  const faultChip: Chip = {
    label: `${sim.faults} part${sim.faults === 1 ? '' : 's'} faulted`,
    tone: 'err',
    testid: 'chip-faults',
    onClick: () => goTo('sim'),
  }
  const runningChip = (onClick?: () => void): Chip => ({
    label: sim.running ? 'sim running' : 'sim paused',
    tone: sim.running ? 'ok' : 'quiet',
    testid: 'chip-sim',
    onClick,
    pulse: sim.running,
  })

  if (view === 'sim') {
    // On the Live Sim view the header describes THE SESSION's board only: the
    // analyzed board's findings/checks chips belong to the Board/Checks
    // surfaces, and "another session is live: X" while viewing exactly that
    // session mislabels the very surface the user is on.
    if (simOffline) {
      chips.push({ label: 'sim offline', tone: 'err', testid: 'chip-sim' })
    } else if (sessionMatchesCurrent) {
      if (sim.faults > 0) chips.push(faultChip)
      chips.push(runningChip())
    } else if (sessionBoard) {
      chips.push({ label: `live: ${sessionBoard}`, tone: 'quiet', testid: 'chip-sim', pulse: sim.running })
    }
  } else {
    if (report?.ok === true) {
      const tone = reportVerdictTone(report)
      const conditional = report.verdict === 'pass' && (report.verdict_conditions ?? []).some(c => c.trim() !== '')
      chips.push({
        testid: 'chip-findings',
        onClick: () => goTo('board'),
        ...(tone === 'error'
          ? { tone: 'err' as const, label: report.serious > 0 ? `${report.serious} serious` : report.fix_first?.notice ? 'not routed yet' : 'analysis failed' }
          : tone === 'warning'
            // An undermined run-level claim is what the body headline leads
            // with ("invalid for analysis"); "qualified" beside it reads as a
            // weaker verdict than the report gives.
            ? report.partly_judged
              ? { tone: 'warn' as const, label: 'partly judged, not a pass' }
              : summarizeEvidence(report.evidence).undermined > 0
              ? { tone: 'warn' as const, label: 'analysis undermined', term: 'invalid' }
              : { tone: 'warn' as const, label: report.total > 0 ? plural(report.total, 'finding') : conditional ? 'clean, conditionally' : 'analysis qualified' }
            // A pass that rests on conditions is never the plain "clean" the chip says for none.
            : { tone: 'ok' as const, label: conditional ? 'clean, conditionally' : 'analysis clean' }),
      })
    }
    if (checks) {
      const { passed, failed, invalid, measured = 0 } = checks
      const counts = [passed > 0 || measured === 0 ? `${passed} passed` : '', measured > 0 ? `${measured} measured` : '']
      const label = `checks ${counts.filter(Boolean).join(', ')}`
      // A green checks chip must never sit beside the report's own serious
      // findings: a spec that asserts nothing about them has not cleared them.
      const uncovered = report?.ok === true ? report.serious : 0
      chips.push({
        testid: 'chip-checks',
        onClick: () => goTo('checks'),
        ...(failed > 0
          ? { tone: 'err' as const, label: `checks ${failed} failed`, term: 'fail' }
          : invalid > 0
            ? { tone: 'warn' as const, label: `checks ${invalid} invalid`, term: 'invalid' }
            : uncovered > 0
              ? { tone: 'warn' as const, label: `${label}, ${uncovered} serious uncovered` }
              : { tone: passed > 0 || measured === 0 ? 'ok' as const : 'quiet' as const, label }),
      })
    }
    if (simOffline) {
      chips.push({ label: 'sim offline', tone: 'err', testid: 'chip-sim', onClick: () => goTo('sim') })
    } else if (simMounted && sessionMatchesCurrent) {
      if (sim.faults > 0) chips.push(faultChip)
      chips.push(runningChip(() => goTo('sim')))
    } else if ((simMounted || serverLiveActive) && sessionBoard) {
      // A session is live for some OTHER board (or a pre-reload launch of this
      // one): never show "sim running" as if it were this board's. The chip
      // names the session's board and opens it.
      // Another tab, an earlier page load or a CLI launch can all have started
      // it, and a same-named file may differ in content, so the chip only
      // claims what it knows and says in one line what that means.
      const openIt = simMounted ? () => goTo('sim') : onOpenLiveSession
      const sameName = sessionBoard === currentBoard
      chips.push({
        label: sameName ? `live session not from this tab: ${sessionBoard}` : `another session is live: ${sessionBoard}`,
        tone: 'warn',
        testid: 'chip-sim',
        onClick: openIt,
        explain: sameName
          ? `The live simulator runs one board at a time, and it is running a ${sessionBoard} that another tab or an earlier page started. This tab cannot tell whether it is the same file.`
          : `The live simulator runs one board at a time, and it is running ${sessionBoard}, not the board you analyzed.`,
        actions: [
          { label: 'Switch to it', testid: 'chip-sim-switch', onClick: openIt },
          ...(report?.ok === true && currentBoard
            ? [{ label: `Replace with ${currentBoard}`, testid: 'chip-sim-replace', onClick: onDriveLive }]
            : []),
        ],
      })
    } else if (report?.ok === true && sessionMatchesCurrent) {
      chips.push({ label: 'live session ready', tone: 'ok', testid: 'chip-sim', onClick: onDriveLive })
    }
  }

  return (
    <>
      {chips.map((chip, i) => <ChipButton key={`${chip.testid}:${i}`} chip={chip} />)}
    </>
  )
}
