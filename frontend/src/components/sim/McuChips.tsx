import type { SimFrame } from '../../types/protocol'
import { Term } from '../ui'

/** One small-caps chip in the MCU card. */
function Chip({ on, tone, title, children }: {
  on: boolean
  tone: 'ok' | 'copper' | 'plain'
  title?: string
  children: React.ReactNode
}) {
  const lit = on && tone !== 'plain'
  return (
    <span
      className="text-[10px] px-1.5 py-0.5 rounded inline-flex items-center gap-1"
      title={title}
      style={{
        background: lit ? (tone === 'ok' ? 'var(--ok-bg)' : 'var(--copper-tint)') : 'var(--surface-2)',
        border: `1px solid ${lit ? (tone === 'ok' ? 'var(--ok-border)' : 'var(--copper-deep)') : 'var(--hairline)'}`,
        color: lit ? (tone === 'ok' ? 'var(--ok)' : 'var(--copper-hi)') : tone === 'plain' ? 'var(--silk-dim)' : 'var(--silk-faint)',
        fontFamily: tone === 'plain' ? 'var(--font-mono)' : undefined,
      }}
    >
      {children}
    </span>
  )
}

/** MCU stat chips, the live dock's header: what the co-sim is actually doing
 *  per MCU (backend, run state, recent UART traffic), each state word a
 *  glossary term. Only real signals from the wire; no invented
 *  frequency counters. */
export function McuChips({ mcus, frame, uartActive, running: simRunning }: {
  mcus: [string, string][]
  frame: SimFrame | null
  uartActive: Set<string>
  /** The sim clock is running. Paused, every MCU is paused with it: the last
   *  frame's run flag would otherwise keep reading "running". */
  running: boolean
}) {
  return (
    <div className="px-3 py-2 flex flex-col gap-1.5 shrink-0" data-testid="mcu-chips" style={{ borderBottom: '1px solid var(--hairline)' }}>
      {mcus.map(([ref, backend]) => {
        const cpuRunning = (frame?.component_states?.[ref]?.['running'] ?? 0) > 0
        const running = simRunning && cpuRunning
        const uart = uartActive.has(ref)
        return (
          <div key={ref} className="flex items-center gap-2 flex-wrap">
            <span className="text-[12px] font-bold" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
              {ref}
            </span>
            <Chip on tone="plain" title="Emulation backend">{backend}</Chip>
            <Chip on={running} tone="ok">
              {running && <span className="run-dot" style={{ width: 5, height: 5, borderRadius: 3, background: 'currentColor' }} />}
              {simRunning
                ? <Term id="mcu_halted" bare>{running ? 'running' : 'halted'}</Term>
                : 'paused'}
            </Chip>
            <Chip on={uart} tone="copper">
              <Term id="uart_activity" bare>{uart ? 'serial active' : 'serial quiet'}</Term>
            </Chip>
          </div>
        )
      })}
    </div>
  )
}
