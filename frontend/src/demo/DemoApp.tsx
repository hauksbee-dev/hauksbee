import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import SimView from '../SimView'
import { useTheme } from '../hooks/useTheme'
import { SimSourceContext } from './simSource'
import { useReplaySimulation } from './useReplaySimulation'
import { parseSession } from './manifest'
import type { DemoManifest, LoadedSession, ManifestSession } from './manifest'
import { WaitlistCard } from './WaitlistCard'
import { PlayIcon, SunIcon, MoonIcon, BoltIcon, CpuIcon, BoardTargetIcon } from '../components/Icons'
import type { Startup } from '../types/report'

// The hauksbee.dev demo shell (VITE_DEMO=1 build): pick a recorded session,
// replay it through the SAME SimView the live app uses. No engine runs
// anywhere: the honesty banner never hides, upload becomes the install CTA,
// and the Hauksbee Cloud waitlist is the only thing that talks to a server.

const BANNER = 'A recorded run of the real engine, byte for byte. Install to run your own boards.'
const INSTALL_CMD =
  'export HAUKSBEE_GITHUB_TOKEN="$(gh auth token)"\n' +
  'printf \'header = "Authorization: Bearer %s"\\n\' "$HAUKSBEE_GITHUB_TOKEN" ' +
  '| curl --config - -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash'

/** The one line that must never leave the screen, on every demo surface. */
function HonestyBanner() {
  return (
    <div
      data-testid="demo-banner"
      className="flex items-center gap-2 px-4 shrink-0 text-[11px]"
      style={{
        height: 30,
        background: 'var(--copper-tint)',
        borderBottom: '1px solid var(--copper-deep)',
        color: 'var(--copper-hi)',
      }}
    >
      <span className="run-dot" style={{ width: 6, height: 6, borderRadius: 3, background: 'currentColor' }} />
      <span className="truncate">{BANNER}</span>
    </div>
  )
}

/** The install call-to-action that replaces every upload surface. */
function InstallCta({ compact }: { compact?: boolean }) {
  const [copied, setCopied] = useState(false)
  const copy = useCallback(() => {
    void navigator.clipboard?.writeText(INSTALL_CMD).then(() => {
      setCopied(true)
      setTimeout(() => setCopied(false), 1600)
    })
  }, [])
  return (
    <section
      data-testid="install-cta"
      className="hb-card px-4 py-3.5"
      style={{ borderRadius: 10, textAlign: 'left' }}
    >
      <div className="text-[13px] font-semibold mb-1" style={{ color: 'var(--silk)' }}>
        Your board needs the real engine
      </div>
      {!compact && (
        <p className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)', margin: '0 0 10px' }}>
          This demo replays recorded runs; it cannot take an upload. The copied
          command uses your authorized GitHub CLI token to install hauksbee,
          then <code>hauksbee run your_board.kicad_pcb</code>{' '}
          opens this exact surface on a live simulation of your board.
        </p>
      )}
      <button
        type="button"
        onClick={copy}
        title="Copy the install command"
        className="hb-press w-full text-left px-3 py-2 rounded-lg text-[11px]"
        style={{
          background: 'var(--instrument)',
          border: '1px solid var(--hairline)',
          color: 'var(--term-tx)',
          fontFamily: 'var(--font-mono)',
          cursor: 'pointer',
          wordBreak: 'break-all',
        }}
      >
        {copied ? 'copied to the clipboard' : INSTALL_CMD}
      </button>
    </section>
  )
}

const fmtDate = (iso: string) => {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toISOString().slice(0, 10)
}

/** Per-session identity block for the sidebar footer: what was recorded,
 *  from which engine build, when. The honesty rules live or die here. */
function SessionInfo({ entry }: { entry: ManifestSession }) {
  const row = (label: string, value: string) => (
    <div className="flex gap-2 text-[10px]" style={{ fontFamily: 'var(--font-mono)' }}>
      <span style={{ color: 'var(--silk-faint)', minWidth: 52 }}>{label}</span>
      <span className="truncate" style={{ color: 'var(--silk-dim)' }} title={value}>{value}</span>
    </div>
  )
  return (
    <div
      data-testid="session-info"
      className="mx-2.5 mb-2.5 px-3 py-2.5 rounded-[10px] flex flex-col gap-1"
      style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}
    >
      <div className="text-[12px] font-semibold truncate" style={{ color: 'var(--silk)' }} title={entry.board_name}>
        {entry.board_name}
      </div>
      {row('firmware', entry.firmware ?? 'none')}
      {row('engine', entry.engine_commit.slice(0, 12))}
      {row('captured', fmtDate(entry.captured_at))}
      {row('length', `${entry.duration_s}s · ${entry.frames} frames`)}
      {entry.thinned_keep_every > 1 &&
        row('cadence', `thinned to 1 in ${entry.thinned_keep_every} frames for size`)}
    </div>
  )
}

/** Provides the replay hook to everything under it. The parent keys this by
 *  session id, so a scenario switch is a REMOUNT and the hook identity the
 *  SimSource contract requires stays constant per mount. */
function ReplayProvider({ session, children }: { session: LoadedSession; children: React.ReactNode }) {
  // This is not a hook call, it is a hook DEFINITION handed down the tree: the
  // SimSource contract is "give me a hook", and the consumer calls it from its own
  // component body. rules-of-hooks cannot tell the two apart, and the remount-per-
  // session keying above is what keeps the identity stable.
  // eslint-disable-next-line react-hooks/rules-of-hooks
  const hook = useMemo(() => () => useReplaySimulation(session), [session])
  return <SimSourceContext.Provider value={hook}>{children}</SimSourceContext.Provider>
}

type Load =
  | { kind: 'idle' }
  | { kind: 'loading'; id: string }
  | { kind: 'ready'; session: LoadedSession }
  | { kind: 'error'; message: string }

export default function DemoApp() {
  const { theme, toggleTheme } = useTheme()
  const [manifest, setManifest] = useState<DemoManifest | null>(null)
  const [manifestError, setManifestError] = useState<string | null>(null)
  const [load, setLoad] = useState<Load>({ kind: 'idle' })
  const cache = useRef<Map<string, LoadedSession>>(new Map())

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const res = await fetch('/sessions/manifest.json')
        if (!res.ok) throw new Error(`manifest.json answered ${res.status}`)
        const m = await res.json() as DemoManifest
        if (alive) setManifest(m)
      } catch (e) {
        if (alive) setManifestError(e instanceof Error ? e.message : String(e))
      }
    })()
    return () => { alive = false }
  }, [])

  const pick = useCallback(async (entry: ManifestSession) => {
    const cached = cache.current.get(entry.id)
    if (cached) { setLoad({ kind: 'ready', session: cached }); return }
    setLoad({ kind: 'loading', id: entry.id })
    try {
      const [sessRes, repRes] = await Promise.all([
        fetch(`/${entry.session}`),
        fetch(`/${entry.report}`),
      ])
      if (!sessRes.ok) throw new Error(`${entry.session} answered ${sessRes.status}`)
      const startup = repRes.ok ? await repRes.json() as Startup : null
      const session = parseSession(entry, await sessRes.text(), startup)
      cache.current.set(entry.id, session)
      setLoad({ kind: 'ready', session })
    } catch (e) {
      setLoad({ kind: 'error', message: e instanceof Error ? e.message : String(e) })
    }
  }, [])

  return (
    <div className="flex flex-col h-screen overflow-hidden" style={{ background: 'var(--canvas)', color: 'var(--silk)' }}>
      <HonestyBanner />
      {load.kind === 'ready' ? (
        <Player
          manifest={manifest}
          session={load.session}
          onPick={entry => void pick(entry)}
          onBack={() => setLoad({ kind: 'idle' })}
          theme={theme}
          onToggleTheme={toggleTheme}
        />
      ) : (
        <Picker
          manifest={manifest}
          manifestError={manifestError}
          load={load}
          onPick={entry => void pick(entry)}
          onDismissError={() => setLoad({ kind: 'idle' })}
        />
      )}
    </div>
  )
}

function Picker({ manifest, manifestError, load, onPick, onDismissError }: {
  manifest: DemoManifest | null
  manifestError: string | null
  load: Load
  onPick: (e: ManifestSession) => void
  onDismissError: () => void
}) {
  const icons: Record<string, React.ReactNode> = {
    // Every scenario card gets a face that says what it demonstrates.
    'boot_gate-overvolt': <BoltIcon size={15} />,
    'watchy-display_init': <CpuIcon size={15} />,
  }
  return (
    <div className="flex-1 overflow-y-auto">
      <div className="max-w-3xl mx-auto px-6 py-10">
        <div className="flex items-center gap-3 mb-2">
          <img src="/favicon.svg" alt="" width={30} height={30} style={{ borderRadius: 7 }} />
          <span className="text-[16px] font-semibold tracking-[0.22em]" style={{ fontFamily: 'var(--font-mono)' }}>
            HAUKSBEE
          </span>
          <span
            className="text-[10px] font-bold px-2 py-0.5 rounded-full tracking-widest"
            style={{ background: 'var(--copper-tint)', border: '1px solid var(--copper-deep)', color: 'var(--copper-hi)' }}
          >
            DEMO
          </span>
        </div>
        <p className="text-[13px] leading-relaxed mb-7" style={{ color: 'var(--silk-dim)', maxWidth: 560 }}>
          Pick a board and watch a session the real engine actually ran:
          scrub anywhere, probe any net, read the recorded faults and UART.
          Nothing is simulated in your browser.
        </p>

        {manifestError && (
          <div className="rounded-lg px-4 py-3 mb-5 text-[12px]" style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}>
            Could not load the session manifest: {manifestError}
          </div>
        )}
        {load.kind === 'error' && (
          <div className="rounded-lg px-4 py-3 mb-5 text-[12px]" style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}>
            Could not load the session: {load.message}{' '}
            <button type="button" onClick={onDismissError} className="underline cursor-pointer" style={{ background: 'none', border: 'none', color: 'inherit' }}>
              dismiss
            </button>
          </div>
        )}

        <div className="grid gap-3 mb-8" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(300px, 1fr))' }} data-testid="demo-picker">
          {manifest === null && !manifestError && (
            <div className="text-[12px]" style={{ color: 'var(--silk-faint)' }}>loading sessions ...</div>
          )}
          {manifest?.sessions.map(entry => (
            <button
              key={entry.id}
              type="button"
              data-testid={`demo-session-${entry.id}`}
              onClick={() => onPick(entry)}
              disabled={load.kind === 'loading'}
              className="hb-card hb-press px-4 py-3.5 text-left cursor-pointer"
              style={{ borderRadius: 10 }}
            >
              <div className="flex items-center gap-2 mb-1">
                <span style={{ color: 'var(--copper)', display: 'inline-flex' }}>
                  {icons[entry.id] ?? <BoardTargetIcon size={15} />}
                </span>
                <span className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>{entry.title}</span>
              </div>
              <p className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)', margin: '0 0 8px' }}>
                {entry.description}
              </p>
              <div className="flex items-center gap-2 text-[10px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                <PlayIcon size={10} />
                {load.kind === 'loading' && load.id === entry.id
                  ? 'loading ...'
                  : `${entry.duration_s}s recording · ${(entry.bytes / 1024).toFixed(0)} KiB`}
              </div>
            </button>
          ))}
        </div>

        <div className="grid gap-3 pb-12" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(300px, 1fr))' }}>
          <InstallCta />
          <WaitlistCard />
        </div>
      </div>
    </div>
  )
}

function Player({ manifest, session, onPick, onBack, theme, onToggleTheme }: {
  manifest: DemoManifest | null
  session: LoadedSession
  onPick: (e: ManifestSession) => void
  onBack: () => void
  theme: 'light' | 'dark'
  onToggleTheme: () => void
}) {
  const entry = session.entry
  return (
    <div className="flex flex-1 min-h-0 overflow-hidden">
      {/* Demo sidebar: same geometry as the live shell's rail, but its
          contents are the demo's truth: sessions, not launchable views. */}
      <nav
        aria-label="Demo sessions"
        className="flex flex-col shrink-0 select-none"
        style={{ width: 232, borderRight: '1px solid var(--hairline)', background: 'var(--surface)' }}
      >
        <div className="flex items-center gap-2.5 px-4" style={{ height: 50, borderBottom: '1px solid var(--hairline)' }}>
          <img src="/favicon.svg" alt="" width={22} height={22} style={{ borderRadius: 6, flexShrink: 0 }} />
          <span className="text-[12px] font-semibold tracking-[0.22em]" style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
            HAUKSBEE
          </span>
          <span
            className="text-[9px] font-bold px-1.5 py-0.5 rounded-full tracking-widest"
            style={{ background: 'var(--copper-tint)', border: '1px solid var(--copper-deep)', color: 'var(--copper-hi)' }}
          >
            DEMO
          </span>
        </div>

        <div className="flex flex-col gap-1 px-2.5 pt-3">
          <button type="button" className="nav-item" onClick={onBack} data-testid="demo-back">
            <span style={{ display: 'inline-flex', flexShrink: 0 }}><BoardTargetIcon size={15} /></span>
            <span className="sidebar-label flex-1">All boards</span>
          </button>
          <div className="text-[9px] font-bold tracking-[0.14em] uppercase px-3 pt-2 pb-1" style={{ color: 'var(--silk-faint)' }}>
            Recorded sessions
          </div>
          {manifest?.sessions.map(e => (
            <button
              key={e.id}
              type="button"
              className="nav-item"
              data-active={e.id === entry.id}
              data-testid={`demo-nav-${e.id}`}
              onClick={() => { if (e.id !== entry.id) onPick(e) }}
            >
              <span style={{ display: 'inline-flex', flexShrink: 0 }}><PlayIcon size={13} /></span>
              <span className="sidebar-label flex-1 truncate">{e.title}</span>
            </button>
          ))}
        </div>

        <div className="flex-1" />

        <div className="px-2.5 pb-2.5 flex flex-col gap-2.5">
          <InstallCta compact />
        </div>
        <SessionInfo entry={entry} />

        <div className="px-2.5 pb-3">
          <button
            type="button"
            data-testid="theme-toggle"
            className="nav-item"
            onClick={onToggleTheme}
            aria-label={theme === 'dark' ? 'Switch to the light theme' : 'Switch to the dark theme'}
          >
            <span style={{ display: 'inline-flex', flexShrink: 0 }}>
              {theme === 'dark' ? <SunIcon size={15} /> : <MoonIcon size={15} />}
            </span>
            <span className="sidebar-label">{theme === 'dark' ? 'Light theme' : 'Dark theme'}</span>
          </button>
        </div>
      </nav>

      <main className="flex-1 min-w-0 min-h-0">
        {/* Keyed by session: a scenario switch remounts the replay source,
            which the SimSource hook-identity contract requires. */}
        <ReplayProvider key={entry.id} session={session}>
          <SimView />
        </ReplayProvider>
      </main>
    </div>
  )
}
