import { useCallback, useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { WebReport } from '../types/report'
import { boardVerdict, TONE_RANK, TONE_STYLE, useStatusLabel } from '../lib/status-vocab'
import type { ChipTone, StatusTerm } from '../lib/status-vocab'
import type { SimShellStatus } from '../SimView'
import { useGlossary, useTermEntry } from './ui'
import type { ChecksSummary } from './ChecksView'
import type { AppView } from './Sidebar'

// The header's ONE status chip. What is on screen is a single claim, the worst
// thing true of the surface you are on, in the engine glossary's own words
// (lib/status-vocab); hovering it lists the parts (board verdict, checks, live
// session) as rows that each go to their surface. The sim view is bound to the
// SESSION's board while every other view is about the analyzed one, and
// Environment and Settings are about this machine, so they carry no chip.

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
  /** The plain-words hover text, for markup that has no popover (print, tests). */
  title?: string
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
        className="inline-flex min-w-0 max-w-full"
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
      title={chip.explain ? undefined : chip.title}
      onClick={chip.onClick}
      disabled={!chip.onClick}
      // A chip is a pill with one line in it. Left shrinkable, "checks 2 passed"
      // wraps to two lines inside a 24px pill and spills out the bottom of it,
      // so the chip keeps its own width and the row clips whole chips instead.
      className="hb-press inline-flex items-center gap-1.5 px-2.5 rounded-full text-[11px] font-semibold tnum whitespace-nowrap min-w-0 max-w-full"
      style={{ ...TONE_STYLE[chip.tone], height: 24, cursor: chip.onClick ? 'pointer' : 'default' }}
    >
      {chip.pulse && (
        <span className="run-dot" style={{ width: 6, height: 6, borderRadius: 3, background: 'currentColor', display: 'inline-block' }} />
      )}
      <span className="truncate">{chip.label}</span>
    </button>
  )
}

/** One thing a surface has to say: a board verdict, the checks, or the live
 *  session. The header shows the worst of them as its one chip. */
interface Part {
  key: 'board' | 'checks' | 'sim'
  tone: ChipTone
  /** The glossary term the verdict word is, when the state is one. */
  term?: StatusTerm
  /** The short fact beside the verdict word ("3 findings", "2 checks failed"). */
  detail: string
  view: AppView
  pulse?: boolean
  /** A longer sentence for the hover, replacing the glossary's. */
  explain?: string
  actions?: { label: string; testid: string; onClick: () => void }[]
}

const PART_NAME: Record<Part['key'], string> = { board: 'Board', checks: 'Checks', sim: 'Live sim' }

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
  const word = useStatusLabel()
  // Environment and Settings are about this machine, not a board.
  if (view === 'env' || view === 'settings') return null

  const parts: Part[] = []
  const faultDetail = `${sim.faults} part${sim.faults === 1 ? '' : 's'} faulted`
  const simPart = (): Part => sim.faults > 0
    ? { key: 'sim', tone: 'err', term: 'fail', detail: faultDetail, view: 'sim' }
    : { key: 'sim', tone: sim.running ? 'ok' : 'quiet', detail: sim.running ? 'sim running' : 'sim paused', view: 'sim', pulse: sim.running }

  if (view === 'sim') {
    // The Live Sim view describes THE SESSION's board only: the analyzed board's
    // verdict belongs to the Board and Checks surfaces.
    if (simOffline) {
      parts.push({ key: 'sim', tone: 'err', detail: 'sim offline', view: 'sim', explain: 'The live session lost its connection. Reload the sim to reconnect.' })
    } else if (sessionMatchesCurrent) {
      parts.push(simPart())
    } else if (sessionBoard) {
      parts.push({ key: 'sim', tone: 'quiet', detail: `live: ${sessionBoard}`, view: 'sim', pulse: sim.running })
    }
  } else {
    const board = report ? boardVerdict(report) : null
    if (board) parts.push({ key: 'board', view: 'board', term: board.term, detail: board.detail, tone: board.tone })
    if (checks) {
      const { passed, failed, invalid, measured = 0 } = checks
      // A green checks result must never sit beside the report's own serious
      // findings: a spec that asserts nothing about them has not cleared them.
      const uncovered = report?.ok === true ? report.serious : 0
      const counts = [passed > 0 || measured === 0 ? `${passed} passed` : '', measured > 0 ? `${measured} measured` : ''].filter(Boolean).join(', ')
      const green = passed > 0 || measured === 0
      parts.push(failed > 0
        ? { key: 'checks', tone: 'err', term: 'fail', detail: `${failed} ${failed === 1 ? 'check' : 'checks'} failed`, view: 'checks' }
        : invalid > 0
          ? { key: 'checks', tone: 'warn', term: 'invalid', detail: `${invalid} ${invalid === 1 ? 'check' : 'checks'} invalid`, view: 'checks' }
          : uncovered > 0
            ? { key: 'checks', tone: 'warn', term: 'not_checked', detail: `checks ${counts}, ${uncovered} serious uncovered`, view: 'checks' }
            : { key: 'checks', tone: green ? 'ok' : 'quiet', term: green ? 'pass' : undefined, detail: `checks ${counts}`, view: 'checks' })
    }
    if (simOffline) {
      parts.push({ key: 'sim', tone: 'err', detail: 'sim offline', view: 'sim' })
    } else if (simMounted && sessionMatchesCurrent) {
      parts.push(simPart())
    } else if ((simMounted || serverLiveActive) && sessionBoard) {
      // A session is live for some OTHER board (or a pre-reload launch of this
      // one): never show "sim running" as if it were this board's. Another tab,
      // an earlier page load or a CLI launch can all have started it, and a
      // same-named file may differ in content, so the chip only claims what it
      // knows and says in one line what that means.
      const openIt = simMounted ? () => goTo('sim') : onOpenLiveSession
      const sameName = sessionBoard === currentBoard
      parts.push({
        key: 'sim', tone: 'warn', view: 'sim',
        detail: sameName ? `live session not from this tab: ${sessionBoard}` : `another session is live: ${sessionBoard}`,
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
      parts.push({ key: 'sim', tone: 'ok', detail: 'live session ready', view: 'sim' })
    }
  }

  if (parts.length === 0) return null

  // The worst part is the chip; ties keep the order board, checks, sim.
  const primary = parts.reduce((a, b) => (TONE_RANK[b.tone] > TONE_RANK[a.tone] ? b : a))
  const label = (p: Part) => [p.term ? word(p.term) : '', p.detail].filter(Boolean).join(' · ')
  const chip: Chip = {
    testid: 'chip-status',
    label: label(primary),
    tone: primary.tone,
    pulse: primary.pulse,
    onClick: () => goTo(primary.view),
    term: primary.explain ? undefined : primary.term,
    explain: primary.explain,
    title: parts.map(p => `${PART_NAME[p.key]}: ${label(p)}`).join('. '),
    // One row per part, each going to its own surface; the primary's own
    // actions (switch to / replace a foreign session) come with it.
    actions: [
      ...(primary.actions ?? []),
      ...(parts.length > 1
        ? parts.map(p => ({ label: `${PART_NAME[p.key]}: ${label(p)}`, testid: `chip-part-${p.key}`, onClick: () => goTo(p.view) }))
        : []),
    ],
  }
  return <ChipButton chip={chip} />
}
