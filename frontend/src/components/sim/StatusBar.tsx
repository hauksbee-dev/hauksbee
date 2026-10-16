import type { BoardInfoMsg, SimFrame } from '../../types/protocol'

/** The vertical rule between two readouts. */
const Bar = () => <span style={{ color: 'var(--hairline)' }}>|</span>

/** The sim view's bottom bar: the counts that describe what is on screen.
 *  Run state, sim time and rate belong to the transport bar above and are
 *  not repeated here; only a lost session (which the transport cannot show
 *  once the socket is gone) is named. */
export function SimStatusBar({ sessionLost, frame, probes, faultCount, boardInfo }: {
  /** The socket dropped after a real session. "Paused" is a claim about a sim
   *  that is still there, so a lost session must not wear it. */
  sessionLost: boolean
  frame: SimFrame | null
  probes: string[]
  /** Faulted PARTS. The rail's faults card counts fault CONDITIONS and labels
   *  itself; one part can trip several limits. */
  faultCount: number
  boardInfo: BoardInfoMsg | null
}) {
  return (
    <div
      data-testid="sim-status-bar"
      className="flex items-center gap-x-3 gap-y-0.5 px-3 py-1 shrink-0 text-[10px] overflow-hidden tnum"
      style={{
        minHeight: 26,
        background: 'var(--surface)',
        borderTop: '1px solid var(--hairline)',
        fontFamily: 'var(--font-mono)',
        // Wrap rather than clip at narrow widths: every readout here is
        // information, so none may be silently cut off the right edge.
        flexWrap: 'wrap',
        color: 'var(--silk-faint)',
      }}
    >
      {sessionLost && <span data-testid="sim-session-ended" style={{ color: 'var(--err)' }}>session ended</span>}

      {frame && (
        <span>nets: <span style={{ color: 'var(--silk-dim)' }}>{Object.keys(frame.net_voltages).length}</span></span>
      )}

      {probes.length > 0 && (
        <>
          <Bar />
          <span className="overflow-hidden" title={`probes: ${probes.join(', ')}`} style={{ maxWidth: 180, whiteSpace: 'nowrap', textOverflow: 'ellipsis' }}>
            probes: <span style={{ color: 'var(--copper)' }}>{probes.join(', ')}</span>
          </span>
        </>
      )}

      {faultCount > 0 && (
        <>
          <Bar />
          <span className="overflow-hidden" title={`FAULTS: ${faultCount} part${faultCount === 1 ? '' : 's'}`}
            style={{ color: 'var(--err)', minWidth: 0, maxWidth: '100%', whiteSpace: 'nowrap', textOverflow: 'ellipsis' }}>
            {`FAULTS: ${faultCount} part${faultCount === 1 ? '' : 's'}`}
          </span>
        </>
      )}

      {boardInfo && (
        <span className="overflow-hidden ml-auto"
          title={`${boardInfo.num_components} components · ${boardInfo.num_nets} nets`}
          style={{ minWidth: 0, maxWidth: '100%', whiteSpace: 'nowrap', textOverflow: 'ellipsis' }}>
          {boardInfo.num_components} comp · {boardInfo.num_nets} nets
        </span>
      )}
    </div>
  )
}
