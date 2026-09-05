import type { BoardInfoMsg, SimFrame } from '../../types/protocol'

/** The vertical rule between two readouts. */
const Bar = () => <span style={{ color: 'var(--hairline)' }}>|</span>

/** The sim view's bottom bar: run state, clock, and the counts that describe
 *  what is on screen. */
export function SimStatusBar({ running, sessionLost, simTime, frame, replay, probes, faultCount, boardInfo }: {
  running: boolean
  /** The socket dropped after a real session. "Paused" is a claim about a sim
   *  that is still there, so a lost session must not wear it. */
  sessionLost: boolean
  simTime: number
  frame: SimFrame | null
  replay: boolean
  probes: string[]
  /** Faulted PARTS. The rail's faults card counts fault CONDITIONS and labels
   *  itself; one part can trip several limits. */
  faultCount: number
  boardInfo: BoardInfoMsg | null
}) {
  const stateColor = sessionLost ? 'var(--err)' : running ? 'var(--ok)' : 'var(--silk-faint)'
  return (
    <div
      className="flex items-center gap-3 px-3 shrink-0 text-[10px] overflow-hidden tnum"
      style={{
        minHeight: 26,
        height: 26,
        background: 'var(--surface)',
        borderTop: '1px solid var(--hairline)',
        fontFamily: 'var(--font-mono)',
        flexWrap: 'nowrap',
        color: 'var(--silk-faint)',
      }}
    >
      <div className="flex items-center gap-1.5">
        <div
          className={running ? 'run-dot' : undefined}
          style={{ width: 6, height: 6, borderRadius: 3, background: stateColor }}
        />
        <span data-testid="sim-run-state" style={{ color: stateColor }}>
          {sessionLost ? 'session ended' : running ? 'running' : 'paused'}
        </span>
      </div>

      <Bar />
      <span>sim: <span style={{ color: 'var(--silk-dim)' }}>{simTime.toFixed(4)}s</span></span>

      {frame && (
        <>
          {/* Capture-time throughput would masquerade as playback rate in a
              replay; the transport's speed control owns that number there. */}
          {!replay && (
            <>
              <Bar />
              <span>rt: <span style={{ color: 'var(--silk-dim)' }}>{frame.realtime_factor.toFixed(2)}x</span></span>
            </>
          )}
          <Bar />
          <span>nets: <span style={{ color: 'var(--silk-dim)' }}>{Object.keys(frame.net_voltages).length}</span></span>
        </>
      )}

      {probes.length > 0 && (
        <>
          <Bar />
          <span className="overflow-hidden" style={{ maxWidth: 180, whiteSpace: 'nowrap', textOverflow: 'ellipsis' }}>
            probes: <span style={{ color: 'var(--copper)' }}>{probes.join(', ')}</span>
          </span>
        </>
      )}

      {faultCount > 0 && (
        <>
          <Bar />
          <span style={{ color: 'var(--err)' }}>
            FAULTS: {faultCount} part{faultCount === 1 ? '' : 's'}
          </span>
        </>
      )}

      <div className="flex-1" />

      {boardInfo && <span>{boardInfo.num_components} comp · {boardInfo.num_nets} nets</span>}
    </div>
  )
}
