import { useState } from 'react'
import type { CosimView } from '../../lib/report-view'
import type { CosimJob } from '../../hooks/useCosimJob'
import { displayNet } from '../../lib/net-name'
import { BenchScope } from './BenchScope'
import { BenchStrip } from './BenchStrip'
import { FindingCard, NoteBlock, SectionHeading } from './Findings'

/** A collapsible strip of one-line qualifications about the run. */
function Detail({ title, tone = 'quiet', testId, lines }: {
  title: string
  tone?: 'quiet' | 'warn'
  testId?: string
  lines: string[]
}) {
  if (lines.length === 0) return null
  const warn = tone === 'warn'
  return (
    <details
      className="rounded-lg px-3 py-2 mb-2 text-xs"
      style={{
        border: `1px solid ${warn ? 'var(--warn-border)' : 'var(--hairline)'}`,
        background: warn ? 'var(--warn-bg)' : 'var(--surface-2)',
        color: 'var(--silk-dim)',
      }}
    >
      <summary className="cursor-pointer font-semibold" style={{ color: warn ? 'var(--warn-strong)' : 'var(--silk)' }}>
        {title}
      </summary>
      <div className="mt-1.5" data-testid={testId}>{lines.map((line, i) => <div key={i}>{line}</div>)}</div>
    </details>
  )
}

const cell = { borderBottom: '1px solid var(--rule)' }

/** The way to run a board whose MCU needs Renode or QEMU: a button, the job's
 *  progress in simulated seconds, and the server's own words if it fails. */
function EmulatorRun({ job }: { job: CosimJob }) {
  const { state, start } = job
  const running = state.phase === 'running'
  return (
    <div className="mt-2 flex flex-wrap items-center gap-3" data-testid="cosim-emulator">
      <button
        type="button"
        data-testid="cosim-emulator-run"
        onClick={start}
        disabled={running}
        className="hb-btn hb-press inline-flex items-center gap-1.5 px-3 text-[13px]"
        style={{ height: 30, opacity: running ? 0.6 : 1 }}
      >
        {state.phase === 'failed' ? 'Try again on Renode/QEMU' : 'Run on Renode/QEMU'}
      </button>
      {state.phase === 'running' && (
        <span className="text-[12px] tnum" role="status" data-testid="cosim-emulator-progress" style={{ color: 'var(--silk-dim)' }}>
          {state.line}
        </span>
      )}
      {state.phase === 'failed' && (
        <span className="text-[12px]" role="alert" data-testid="cosim-emulator-error" style={{ color: 'var(--err-strong)' }}>
          {state.error}
        </span>
      )}
    </div>
  )
}

export function CosimBlock({ cosim: c, liveAvailable, onDriveLive, simMounted, emulator }: {
  cosim: CosimView
  liveAvailable: boolean
  onDriveLive: () => void
  simMounted: boolean
  /** The emulator job, when the page still holds the files to start it with. */
  emulator?: CosimJob | null
}) {
  // The pin the reader is looking at: the chip last clicked, the lane it lights.
  const [focusNet, setFocusNet] = useState<string | null>(null)
  return (
    <section className="mt-7" data-testid="cosim-section">
      <SectionHeading title="Firmware co-sim" verdict={c.ran ? c.ranLine : undefined} />
      {c.ran ? (
        <>
          {c.bench && c.benchReport && (
            <>
              <BenchStrip view={c.bench} focusNet={focusNet} onFocus={setFocusNet} />
              <BenchScope bench={c.benchReport} focusNet={focusNet} onFocus={setFocusNet} />
            </>
          )}
          <Detail title="Timing coverage" lines={c.timingLines} />
          {c.timingRefusals.length > 0 && (
            <div
              className="rounded-lg px-4 py-2.5 mb-2"
              style={{ border: '1px solid var(--err-border)', borderLeft: '4px solid var(--err)', background: 'var(--err-bg)' }}
            >
              <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--err-strong)' }}>
                TIMING INVALID
              </span>
              {c.timingRefusals.map((line, i) => <div key={i} className="text-sm mt-1">{line}</div>)}
            </div>
          )}
          <Detail title="Fallback-qualified windows" tone="warn" lines={c.fallbackLines} />
          <Detail title="Numerical qualification" testId="cosim-error-budget" lines={c.budgetLines} />
          {c.groups.length === 0 && <NoteBlock>No electrical-stress faults during the run.</NoteBlock>}
          {c.groups.map((g, i) => <FindingCard key={i} group={g} />)}
          {c.uart && (
            <>
              <div className="text-sm mb-1"><b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>UART output:</b></div>
              <pre
                className="rounded-lg px-3 py-2.5 mb-2 text-xs overflow-x-auto whitespace-pre-wrap"
                style={{
                  background: 'var(--instrument)', border: '1px solid var(--instrument-edge)',
                  color: 'var(--instrument-text)', fontFamily: 'var(--font-mono)',
                }}
              >
                {c.uart}
              </pre>
            </>
          )}
          {!c.bench && c.gpio.length > 0 && (
            <table className="w-full text-xs mt-1" style={{ borderCollapse: 'collapse' }}>
              <thead>
                <tr>
                  {['Net', 'Volts', 'Activity'].map(h => (
                    <th key={h} className="text-left px-2 py-1 font-semibold" style={{ color: 'var(--silk-dim)', borderBottom: '1px solid var(--hairline)' }}>
                      {h}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {c.gpio.map((g, i) => (
                  <tr key={i}>
                    <td className="px-2 py-1" style={cell}>{displayNet(g.name)}</td>
                    <td className="px-2 py-1 tnum" style={{ ...cell, fontFamily: 'var(--font-mono)' }}>{(g.volts || 0).toFixed(3)}</td>
                    <td className="px-2 py-1" style={{ ...cell, color: g.driven ? 'var(--ok)' : 'var(--silk-faint)' }}>
                      {g.driven ? 'driven' : 'idle'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {liveAvailable && (
            <div data-testid="cosim-next" className="mt-3 text-[13px]" style={{ color: 'var(--silk-dim)' }}>
              <button
                type="button"
                onClick={onDriveLive}
                className="hb-press cursor-pointer"
                style={{
                  background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)',
                  textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)', fontSize: 13,
                }}
              >
                {simMounted ? 'Open the live sim' : 'Drive it live'}
              </button>
              {' '}to boot this firmware interactively (scope, serial console, sliders).
            </div>
          )}
        </>
      ) : (
        <>
          {c.notRanLines.map((line, i) => <NoteBlock key={i} tag="Co-sim not available">{line}</NoteBlock>)}
          {c.externalBackend && emulator && <EmulatorRun job={emulator} />}
        </>
      )}
    </section>
  )
}
