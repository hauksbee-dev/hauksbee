import type { WebReport } from '../types/report'
import { reportVerdictTone } from '../lib/report-verdict'
import type { SimShellStatus } from '../SimView'
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
}

function ChipButton({ chip }: { chip: Chip }) {
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
  serverLiveActive, goTo, onOpenLiveSession, onDriveLive,
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
  serverLiveActive: boolean
  goTo: (view: AppView) => void
  onOpenLiveSession: () => void
  onDriveLive: () => void
}) {
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
      chips.push({
        testid: 'chip-findings',
        onClick: () => goTo('board'),
        ...(tone === 'error'
          ? { tone: 'err' as const, label: report.serious > 0 ? `${report.serious} serious` : 'analysis failed' }
          : tone === 'warning'
            ? { tone: 'warn' as const, label: report.total > 0 ? `${report.total} findings` : 'analysis qualified' }
            : { tone: 'ok' as const, label: 'analysis clean' }),
      })
    }
    if (checks) {
      const { passed, failed, invalid } = checks
      chips.push({
        testid: 'chip-checks',
        onClick: () => goTo('checks'),
        ...(failed > 0
          ? { tone: 'err' as const, label: `checks ${failed} failed` }
          : invalid > 0
            ? { tone: 'warn' as const, label: `checks ${invalid} invalid` }
            : { tone: 'ok' as const, label: `checks ${passed} passed` }),
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
      chips.push({
        label: `another session is live: ${sessionBoard}`,
        tone: 'warn',
        testid: 'chip-sim',
        onClick: simMounted ? () => goTo('sim') : onOpenLiveSession,
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
