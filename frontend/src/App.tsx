import { useCallback, useEffect, useRef, useState } from 'react'
import { Landing } from './components/Landing'
import SimView from './SimView'
import type { QueuedCheck, Startup, WebReport } from './types/report'

// One web experience (W6 §1). The app asks the server how it was launched
// (`/api/startup`) and lands accordingly:
//   - `hauksbee serve`            -> { preloaded: false, live: true }: the
//     drop-a-board landing; an uploaded board's report can launch a live sim
//     server-side (`/api/live/launch`) and expand into the sim view.
//   - `hauksbee run <b> --serve`  -> { preloaded: true, report, live: true }:
//     the same landing, opened on that board's report, with the live session
//     already running on /ws.
// A server without the endpoint (the standalone hauksbee-server demo binary)
// gets the historical behaviour: straight to the live-sim view.
//
// The landing STAYS MOUNTED (hidden) while the sim view is open, so leaving
// the sim returns to the exact report (uploaded files, queued checks and all)
// instead of a blank drop zone.

type Boot =
  | { kind: 'loading' }
  | { kind: 'landing'; report: WebReport | null; boardName: string | null; canLaunchLive: boolean }

export default function App() {
  const [boot, setBoot] = useState<Boot>({ kind: 'loading' })
  const [simOpen, setSimOpen] = useState(false)

  // Checks queued from the live-sim surface (net/component clicks) for the
  // checks builder on the report page. Owned here because the two surfaces
  // are siblings; the builder consumes by `seq` so nothing applies twice.
  const [queuedChecks, setQueuedChecks] = useState<QueuedCheck[]>([])
  const seqRef = useRef(0)
  const queueCheck = useCallback((check: Omit<QueuedCheck, 'seq'>) => {
    seqRef.current += 1
    setQueuedChecks(prev => [...prev, { ...check, seq: seqRef.current }])
  }, [])
  const consumeChecks = useCallback((upToSeq: number) => {
    setQueuedChecks(prev => prev.filter(c => c.seq > upToSeq))
  }, [])

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const res = await fetch('/api/startup')
        if (!res.ok) throw new Error(`startup ${res.status}`)
        const startup = await res.json() as Startup
        if (!alive) return
        if (startup.preloaded) {
          setBoot({
            kind: 'landing',
            report: startup.report,
            boardName: startup.board_name,
            canLaunchLive: startup.live === true,
          })
        } else {
          setBoot({ kind: 'landing', report: null, boardName: null, canLaunchLive: startup.live === true })
        }
      } catch {
        // No startup endpoint (a stale/odd deployment). Degrade to the
        // drop-a-board Landing, never the live-sim view, which would sit
        // "offline" with no way to load a board. The Landing's own upload
        // path (/api/analyze) is the recovery affordance.
        if (alive) setBoot({ kind: 'landing', report: null, boardName: null, canLaunchLive: false })
      }
    })()
    return () => { alive = false }
  }, [])

  if (boot.kind === 'loading') {
    return (
      <div
        className="flex items-center justify-center h-screen text-sm"
        style={{ background: '#020617', color: '#475569', fontFamily: 'system-ui, sans-serif' }}
      >
        hauksbee ...
      </div>
    )
  }

  return (
    <>
      {simOpen && (
        <SimView
          onExit={() => setSimOpen(false)}
          onQueueCheck={queueCheck}
        />
      )}
      <div style={{ display: simOpen ? 'none' : undefined, height: '100%' }}>
        <Landing
          preloadedReport={boot.report}
          preloadedBoardName={boot.boardName}
          sessionPreloaded={boot.report !== null}
          canLaunchLive={boot.canLaunchLive}
          onRunIt={() => setSimOpen(true)}
          queuedChecks={queuedChecks}
          onQueueCheck={queueCheck}
          onQueuedConsumed={consumeChecks}
        />
      </div>
    </>
  )
}
