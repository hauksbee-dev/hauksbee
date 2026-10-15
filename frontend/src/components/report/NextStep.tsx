import { useState } from 'react'
import type { BoardRequest, WebReport } from '../../types/report'
import { displayNet } from '../../lib/net-name'

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
/** Periods a toggle check needs in its run to time an edge-to-edge rate. */
const PERIODS = 5

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

const BOX = { border: '1px solid var(--hairline)', borderLeft: '4px solid var(--copper)', background: 'var(--surface)', color: 'var(--silk-dim)' }
const Lead = ({ children }: { children: string }) => <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>{children}</b>

/** The Next card. It follows the session: no firmware, firmware that ran, checks
 *  that ran. Once firmware has run it never asks for firmware again. */
export function NextStep({ report, firmwareName, boardName, checks, specFile, onQueue, onOpenChecks, onOpenEvidence }: {
  report: WebReport
  firmwareName: string | null
  /** The board file the terminal scaffold names; absent when restored. */
  boardName: string | null
  checks: { passed: number; failed: number; invalid: number } | null
  specFile: string | null
  onQueue: (request: BoardRequest) => void
  onOpenChecks: () => void
  onOpenEvidence: () => void
}) {
  const [added, setAdded] = useState<ReadonlySet<string>>(new Set())
  const ran = report.cosim?.ran === true
  const chips = suggestions(report)
  const add = (s: Suggestion) => {
    onQueue(s.request)
    setAdded(prev => new Set(prev).add(s.key))
    onOpenChecks()
  }
  const checksButton = (
    <button type="button" data-testid="open-checks-next" onClick={onOpenChecks} className="hb-btn-primary hb-press px-3 py-1.5 text-[12px]">
      Set up checks
    </button>
  )
  return (
    <div data-testid="static-next-step" className="mt-3 rounded-lg px-4 py-2.5 text-[13px] leading-relaxed" style={BOX}>
      {checks ? (
        <>
          <Lead>Next:</Lead> Run these in CI:{' '}
          <code className="hb-inline break-all">hauksbee-ci run ci/{specFile ?? '<name>.toml'}</code>
          <div className="mt-2">
            <button type="button" data-testid="ci-next" onClick={onOpenChecks} className="hb-btn-primary hb-press px-3 py-1.5 text-[12px]">
              Set up GitHub CI
            </button>
          </div>
        </>
      ) : ran ? (
        <>
          <Lead>Next:</Lead> Turn what it did into checks:
          <ul className="mt-1.5 flex flex-col gap-1.5">
            {chips.map(s => (
              <li key={s.key} className="flex flex-wrap items-center gap-x-2" data-testid="suggestion">
                <span className="tnum" style={{ color: 'var(--silk)' }}>{s.text}</span>
                <button
                  type="button" data-testid={s.testId} onClick={() => add(s)}
                  className="hb-btn hb-press px-2 py-0.5 text-[11px]"
                >
                  {added.has(s.key) ? 'Added, add again' : 'Add'}
                </button>
                {s.note && <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{s.note}</span>}
              </li>
            ))}
          </ul>
          <div className="mt-2">{checksButton}</div>
        </>
      ) : firmwareName ? (
        <>
          <Lead>Next:</Lead> {firmwareName} did not run on this board; the reason is under Firmware co-sim below.
          <div className="mt-2">{checksButton}</div>
        </>
      ) : (
        <>
          <Lead>Next:</Lead> Add firmware to see it run. This static report does not prove powered
          behavior, brownout, overheating, or firmware timing.
          {boardName && <> Terminal scaffold: <code className="hb-inline break-all">hauksbee-ci init {boardName}</code>.</>}
          <div className="mt-2">
            {checksButton}
            {boardName && (
              <button type="button" data-testid="open-evidence-next" onClick={onOpenEvidence} className="hb-btn hb-press ml-2 px-3 py-1.5 text-[12px]">
                Add firmware or other evidence
              </button>
            )}
          </div>
        </>
      )}
    </div>
  )
}
