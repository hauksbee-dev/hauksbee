import { useState } from 'react'
import type { BoardRequest, WebReport } from '../../types/report'
import type { ReportView } from '../../lib/report-view'
import { displayNet } from '../../lib/net-name'
import { TOGGLE_PERIODS as PERIODS } from '../../lib/check-catalog'
import { DropBoardAgain } from '../UploadFailure'
import type { LocateFn } from './Findings'
import { runnableActions, type ActionHost, type RunnableAction } from '../../lib/web-actions'
import { ActionButtons } from './ActionButtons'

/** One observation from the co-sim, offered as a check. Every field comes from
 *  the report; the only derived numbers (a rate, a rail floor, a run length)
 *  say how they were derived in `note`. */
export interface Suggestion {
  key: string
  testId: string
  text: string
  note?: string
  request: BoardRequest
}

/** Above this the 1 ms check grid cannot time a period (it needs 10 samples). */
const MAX_HZ = 100
/** A rail floor suggestion: the rail's nominal less this fraction. */
const RAIL_FLOOR = 0.05
const trim = (n: number, digits: number) => String(Number(n.toFixed(digits)))

export function suggestions(r: WebReport): Suggestion[] {
  const c = r.cosim
  if (!c?.ran) return []
  const out: Suggestion[] = []
  // A print the firmware made, not the whole log: one line is a stable substring.
  const line = c.uart_output.split('\n').map(s => s.replace(/[^\x20-\x7e]/g, '').trim()).find(Boolean)
  if (line) {
    out.push({
      key: `uart:${line}`, testId: 'suggest-uart', text: `UART printed "${line}"`,
      request: { type: 'check', kind: 'uart', contains: line },
    })
  }
  const gpio = c.gpio_nets ?? []
  const secs = c.seconds_simulated
  for (const g of gpio.filter(n => (n.toggles ?? 0) > 0 && secs > 0).slice(0, 2)) {
    const n = g.toggles ?? 0
    const hz = n / 2 / secs
    const base = { key: `toggle:${g.name}`, testId: `suggest-toggle-${g.name}`, net: displayNet(g.name) }
    if (n >= 2 * PERIODS && hz <= MAX_HZ) {
      const runMs = Math.ceil((PERIODS * 1000) / hz / 100) * 100
      const freq = trim(hz, hz < 10 ? 1 : 0)
      out.push({
        key: base.key, testId: base.testId, text: `${base.net} toggles at about ${freq} Hz`,
        note: `suggested: ${PERIODS} periods needs a ${runMs} ms run length (the Checks default is shorter)`,
        request: { type: 'check', kind: 'toggle', net: g.name, freq_hz: freq, run_ms: String(runMs) },
      })
    } else {
      // Too few edges in the co-sim window to time a rate honestly: offer what was seen.
      out.push({
        key: base.key, testId: base.testId, text: `${base.net} toggled ${n === 1 ? 'once' : `${n} times`} in ${trim(secs, 3)} s`,
        note: `suggested: at least once, over a run as long as the co-sim's (${Math.ceil(secs * 1000)} ms)`,
        request: { type: 'check', kind: 'toggle', net: g.name, min_toggles: '1', run_ms: String(Math.ceil(secs * 1000)) },
      })
    }
  }
  for (const s of r.supplies ?? []) {
    const g = gpio.find(n => n.name === s.net)
    const floor = s.volts * (1 - RAIL_FLOOR)
    if (!g || g.driven || s.volts <= 0 || g.volts < floor) continue
    out.push({
      key: `rail:${s.net}`, testId: `suggest-rail-${s.net}`,
      text: `${displayNet(s.net)} held ${g.volts.toFixed(3)} V`,
      note: `suggested min ${trim(floor, 3)} V (nominal ${trim(s.volts, 3)} V less 5%)`,
      request: { type: 'check', kind: 'voltage', net: s.net, min: trim(floor, 3) },
    })
  }
  return out
}

/** Which one next step the board's state calls for, in order: a run of
 *  checks goes to CI; a board with no copper is routed first; the causes to
 *  fix first; any other failure; key parts with no usable model (the rest of
 *  the board cannot be judged without them); then what firmware did or did
 *  not do; and only then checks or firmware for a board with nothing in the
 *  way. Never two of these at once. */
export type NextKind = 'ci' | 'route' | 'fix-first' | 'fix' | 'model' | 'cosim' | 'firmware-failed' | 'checks'

export function nextKind(r: WebReport, v: Pick<ReportView, 'fixFirst' | 'unmodelled'>, state: {
  firmwareName: string | null
  checks: { passed: number; failed: number; invalid: number } | null
}): NextKind {
  if (state.checks) return 'ci'
  if (r.fix_first?.notice) return 'route'
  if (v.fixFirst) return 'fix-first'
  if (r.verdict === 'fail' || r.serious > 0) return 'fix'
  if (modelRefs(v).length > 0) return 'model'
  if (r.cosim?.ran) return 'cosim'
  if (state.firmwareName) return 'firmware-failed'
  return 'checks'
}

/** The key parts a model would let the run judge. */
const modelRefs = (v: Pick<ReportView, 'unmodelled'>) => v.unmodelled.key

/** The kinds whose card offers the engine's `next_actions`: the others (routing,
 *  CI, a co-sim's own suggestions) have one dedicated step. */
const OFFERS_ACTIONS: readonly NextKind[] = ['fix-first', 'fix', 'model', 'checks']

/** What each kind's own buttons already are, so the engine's matching action is
 *  not a second button for the same step. */
const OWN_STEPS: Partial<Record<NextKind, readonly string[]>> = { checks: ['start_checks', 'add_firmware'] }

/** The engine's next steps this app can run, as the buttons of the card's one
 *  Next step. An id the host has no handler for is left out, not shown dead. */
export function nextStepActions(r: WebReport, kind: NextKind, host: ActionHost): RunnableAction[] {
  if (!OFFERS_ACTIONS.includes(kind)) return []
  const own = OWN_STEPS[kind] ?? []
  return runnableActions(r.next_actions, host).filter(a => !own.includes(a.id))
}

/** Buttons shown at once; the rest sit behind "More steps". */
const SHOWN = 3

const Lead = ({ children }: { children: React.ReactNode }) => (
  <div className="text-[11px] font-bold tracking-widest uppercase mb-0.5" style={{ color: 'var(--copper-hi)' }}>{children}</div>
)
const Buttons = ({ children }: { children: React.ReactNode }) => (
  <div className="mt-2 flex flex-wrap items-center gap-2">{children}</div>
)

/** The card's one Next step: one sentence for the board's state and, where
 *  the app can do it, the one button that does it. */
export function NextStep({
  report, view, firmwareName, checks, specFile, onQueue, onOpenChecks, host, onLocate, quiet = false,
}: {
  report: WebReport
  view: Pick<ReportView, 'fixFirst' | 'unmodelled'>
  firmwareName: string | null
  checks: { passed: number; failed: number; invalid: number } | null
  specFile: string | null
  onQueue: (request: BoardRequest) => void
  onOpenChecks: () => void
  /** What the engine's `next_actions` can do here (open the model flow, the
   *  evidence slots, a check, the board). */
  host: ActionHost
  onLocate?: LocateFn
  /** Another surface on screen holds the one primary button (an open
   *  inspector card), so this step's buttons are all secondary. */
  quiet?: boolean
}) {
  const PRIMARY = quiet ? 'hb-btn' : 'hb-btn-primary'
  const [added, setAdded] = useState<ReadonlySet<string>>(new Set())
  const kind = nextKind(report, view, { firmwareName, checks })
  const actions = nextStepActions(report, kind, host)
  const moreSteps = actions.slice(SHOWN)
  // The engine's steps follow the card's own: primary only where the card has no
  // button of its own (the model step, where they ARE the step).
  const stepButtons = (primary: boolean) => (
    <>
      <ActionButtons actions={actions.slice(0, SHOWN)} size="step" primary={primary && !quiet} />
      {moreSteps.length > 0 && (
        <details data-testid="more-steps" className="w-full text-[12px]">
          <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>{moreSteps.length} more {moreSteps.length === 1 ? 'step' : 'steps'}</summary>
          <div className="mt-1.5 flex flex-wrap gap-2"><ActionButtons actions={moreSteps} size="step" /></div>
        </details>
      )}
    </>
  )
  const add = (s: Suggestion) => {
    onQueue(s.request)
    setAdded(prev => new Set(prev).add(s.key))
    onOpenChecks()
  }
  const checksButton = (primary: boolean) => (
    <button
      type="button" data-testid="open-checks-next" onClick={onOpenChecks}
      className={`${primary ? PRIMARY : 'hb-btn'} hb-press px-3 py-1.5 text-[12px]`}
    >
      Set up checks
    </button>
  )
  let body: React.ReactNode
  switch (kind) {
    case 'ci':
      body = (
        <>
          <Lead>Next step</Lead>Run these checks on every push.{' '}
          <span style={{ color: 'var(--silk-faint)' }}>
            (<code className="hb-inline break-all">ci/{specFile ?? '<name>.toml'}</code>)
          </span>
          <Buttons>
            <button type="button" data-testid="ci-next" onClick={onOpenChecks} className={`${PRIMARY} hb-press px-3 py-1.5 text-[12px]`}>
              Set up GitHub CI
            </button>
          </Buttons>
        </>
      )
      break
    case 'route':
      body = (
        <>
          <Lead>Next step</Lead>Route the board in your layout editor, then drop it here again. The copper checks run once it is routed.
          <Buttons>
            <DropBoardAgain testId="next-redrop" label="Drop the routed board" height={30} primary={!quiet} />
          </Buttons>
        </>
      )
      break
    case 'fix-first': {
      // A pointer at the first cause, not a second list: every cause is a
      // card in the findings beside the board.
      const ff = view.fixFirst!
      const first = ff.items[0]
      const rest = ff.items.length - 1 + ff.more
      body = (
        <div data-testid="fix-first">
          <Lead>Next step</Lead>
          Fix this first: <span data-testid="fix-first-item" className="font-semibold" style={{ color: 'var(--silk)' }}>{first.what}</span>
          {first.fix && <> {first.fix}</>}
          {rest > 0 && <> Then the {rest === 1 ? 'other serious finding' : `other ${rest} serious findings`} beside the board.</>}
          {((onLocate && first.x !== undefined && first.y !== undefined) || actions.length > 0) && (
            <Buttons>
              {onLocate && first.x !== undefined && first.y !== undefined && (
                <button
                  type="button" data-testid="finding-locate"
                  onClick={() => onLocate(first.x!, first.y!, first.what)}
                  className="hb-btn hb-press px-3 py-1.5 text-[12px]"
                >
                  Show it on the board
                </button>
              )}
              {stepButtons(false)}
            </Buttons>
          )}
        </div>
      )
      break
    }
    case 'fix':
      body = (
        <>
          <Lead>Next step</Lead>Fix the serious {report.serious === 1 ? 'finding' : 'findings'} beside the board, then drop it here again.
          {actions.length > 0 && <Buttons>{stepButtons(false)}</Buttons>}
        </>
      )
      break
    case 'model': {
      const refs = modelRefs(view)
      body = (
        <>
          <Lead>Next step</Lead>Give {refs.join(', ')} {refs.length === 1 ? 'a model' : 'models'} so the rest of the board can be judged.
          {actions.length > 0 && <Buttons>{stepButtons(true)}</Buttons>}
        </>
      )
      break
    }
    case 'cosim': {
      const chips = suggestions(report)
      body = (
        <>
          <Lead>Next step</Lead>Turn what it did into checks:
          <ul className="mt-1.5 flex flex-col gap-1.5">
            {chips.map(s => (
              <li key={s.key} className="flex flex-wrap items-center gap-x-2" data-testid="suggestion">
                <span className="tnum" style={{ color: 'var(--silk)' }}>{s.text}</span>
                <button type="button" data-testid={s.testId} onClick={() => add(s)} className="hb-btn hb-press px-2 py-0.5 text-[11px]">
                  {added.has(s.key) ? 'Added, add again' : 'Add'}
                </button>
                {s.note && <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{s.note}</span>}
              </li>
            ))}
          </ul>
          <Buttons>{checksButton(true)}</Buttons>
        </>
      )
      break
    }
    case 'firmware-failed':
      body = (
        <>
          <Lead>Next step</Lead>{firmwareName} did not run on this board; the reason is under Firmware co-sim.
          <Buttons>{checksButton(true)}</Buttons>
        </>
      )
      break
    default:
      body = (
        <>
          <Lead>Next step</Lead>Say what the board should do, or add firmware to see it run. This check is static: it does not
          prove powered behaviour, brownout, overheating or firmware timing.
          <Buttons>
            {checksButton(true)}
            {host.openEvidence && (
              <button type="button" data-testid="open-evidence-next" onClick={host.openEvidence} className="hb-btn hb-press px-3 py-1.5 text-[12px]">
                Add firmware
              </button>
            )}
            {stepButtons(false)}
          </Buttons>
        </>
      )
  }
  return (
    <div data-testid="static-next-step" data-next={kind} data-quiet={quiet || undefined} className="text-[13px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
      {body}
    </div>
  )
}

