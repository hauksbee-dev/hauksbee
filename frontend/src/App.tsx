import { useEffect, useState } from 'react'
import { Landing } from './components/Landing'
import SimView from './SimView'
import type { Startup, WebReport } from './types/report'

// One web experience (W6 §1). The app asks the server how it was launched
// (`/api/startup`) and lands accordingly:
//   - `hauksbee serve`            -> { preloaded: false }: the drop-a-board
//     landing (drop zone + plain-language report).
//   - `hauksbee run <b> --serve`  -> { preloaded: true, report }: the same
//     landing, opened on that board's report, with a "run it" affordance that
//     expands into the live-sim view (scope, viewers, transport).
// A server without the endpoint (the standalone hauksbee-server demo binary)
// gets the historical behaviour: straight to the live-sim view.

type View =
  | { kind: 'loading' }
  | { kind: 'landing'; report: WebReport | null; boardName: string | null; canRunLive: boolean }
  | { kind: 'sim' }

export default function App() {
  const [view, setView] = useState<View>({ kind: 'loading' })

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const res = await fetch('/api/startup')
        if (!res.ok) throw new Error(`startup ${res.status}`)
        const startup = await res.json() as Startup
        if (!alive) return
        if (startup.preloaded) {
          setView({
            kind: 'landing',
            report: startup.report,
            boardName: startup.board_name,
            canRunLive: true,
          })
        } else {
          setView({ kind: 'landing', report: null, boardName: null, canRunLive: false })
        }
      } catch {
        // No startup endpoint: a bare sim server (demo binary / old deployments).
        if (alive) setView({ kind: 'sim' })
      }
    })()
    return () => { alive = false }
  }, [])

  if (view.kind === 'loading') {
    return (
      <div
        className="flex items-center justify-center h-screen text-sm"
        style={{ background: '#020617', color: '#475569', fontFamily: 'system-ui, sans-serif' }}
      >
        hauksbee ...
      </div>
    )
  }

  if (view.kind === 'sim') return <SimView />

  return (
    <Landing
      preloadedReport={view.report}
      preloadedBoardName={view.boardName}
      canRunLive={view.canRunLive}
      onRunIt={() => setView({ kind: 'sim' })}
    />
  )
}
