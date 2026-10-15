import { useEffect, useState } from 'react'
import { plural } from '../lib/report-view'
import type { WebReport } from '../types/report'
import type { Theme } from '../hooks/useTheme'
import {
  BoardTargetIcon, ChecksIcon, GearIcon, LiveIcon, WrenchIcon, SunIcon, MoonIcon,
} from './Icons'
import { relTime } from '../lib/rel-time'

// The app shell's left rail: wordmark + mark, the five real destinations
// (Board, Checks, Live Sim, Environment, Settings), and the active board's
// identity pinned in the footer. No search, no user menu, no fake nav: hauksbee
// has no accounts and no multi-project dashboard, so the rail holds only what
// exists.

export type AppView = 'board' | 'checks' | 'sim' | 'env' | 'settings'

interface NavState {
  view: AppView
  setView: (v: AppView) => void
  /** Checks needs an ok report; Live Sim needs a live-capable session. */
  checksEnabled: boolean
  simEnabled: boolean
  /** True while the sim session is running (the Live Sim dot pulses). */
  simRunning: boolean
  faultCount: number
}

/** Why a destination is greyed out, said on the item (a tooltip alone is
 *  invisible on a touch screen) and as its hover and accessible hint. */
const NEEDS_BOARD = 'load a board first'

export function Sidebar({
  nav, report, boardLabel, analyzedAt, theme, onToggleTheme, sessions,
}: {
  nav: NavState
  report: WebReport | null
  boardLabel: string | null
  analyzedAt: number | null
  theme: Theme
  onToggleTheme: () => void
  /** The saved-session indicator, pinned above the board identity card. Passed
   *  as a node rather than as data: the rail's job is where things sit, and the
   *  switcher owns a dialog, a rename and a delete of its own. */
  sessions?: React.ReactNode
}) {
  // Re-render on a slow clock so "analyzed N min ago" stays honest.
  const [now, setNow] = useState(Date.now())
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000)
    return () => clearInterval(t)
  }, [])

  const items: {
    id: AppView
    label: string
    icon: React.ReactNode
    disabled?: boolean
    hint?: string
    badge?: number
  }[] = [
    { id: 'board', label: 'Board', icon: <BoardTargetIcon size={16} /> },
    {
      id: 'checks', label: 'Checks', icon: <ChecksIcon size={16} />,
      disabled: !nav.checksEnabled,
      hint: nav.checksEnabled ? undefined : NEEDS_BOARD,
    },
    {
      id: 'sim', label: 'Live Sim', icon: <LiveIcon size={16} />,
      disabled: !nav.simEnabled,
      hint: nav.simEnabled ? undefined : NEEDS_BOARD,
      badge: nav.faultCount > 0 ? nav.faultCount : undefined,
    },
    { id: 'env', label: 'Environment', icon: <WrenchIcon size={16} /> },
    { id: 'settings', label: 'Settings', icon: <GearIcon size={16} /> },
  ]

  return (
    <nav
      aria-label="Main"
      className="sidebar flex flex-col shrink-0 select-none"
      style={{
        width: 212,
        borderRight: '1px solid var(--hairline)',
        background: 'var(--surface)',
      }}
    >
      {/* Wordmark + mark */}
      <div className="sidebar-brand flex items-center gap-2.5 px-4" style={{ height: 56, borderBottom: '1px solid var(--hairline)' }}>
        <img src="/favicon.svg" alt="" width={24} height={24} style={{ borderRadius: 6, flexShrink: 0 }} />
        <span
          className="sidebar-label text-[13px] font-semibold tracking-[0.22em]"
          style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
        >
          HAUKSBEE
        </span>
      </div>

      {/* Nav */}
      <div className="nav-list flex flex-col gap-1 px-2.5 pt-3">
        {items.map(item => (
          <button
            key={item.id}
            type="button"
            data-testid={`nav-${item.id}`}
            className="nav-item"
            data-active={nav.view === item.id}
            disabled={item.disabled}
            title={item.hint}
            aria-label={item.hint ? `${item.label}: ${item.hint}` : item.label}
            onClick={() => nav.setView(item.id)}
            aria-current={nav.view === item.id ? 'page' : undefined}
          >
            <span style={{ display: 'inline-flex', flexShrink: 0 }}>{item.icon}</span>
            <span className="nav-label flex-1 min-w-0">
              <span className="nav-name block">{item.label}</span>
              {item.disabled && item.hint && (
                <span data-testid={`nav-${item.id}-note`} className="nav-note sidebar-label block text-[10px]" style={{ color: 'var(--silk-faint)' }}>
                  {item.hint}
                </span>
              )}
            </span>
            {item.id === 'sim' && nav.simRunning && (
              <span
                className="run-dot sidebar-label"
                aria-label="Simulation running"
                style={{
                  width: 7, height: 7, borderRadius: 4, background: 'var(--ok)',
                  display: 'inline-block', flexShrink: 0,
                }}
              />
            )}
            {item.badge !== undefined && (
              <span
                className="sidebar-label text-[10px] font-bold px-1.5 rounded-full tnum whitespace-nowrap"
                title={`${item.badge} part${item.badge === 1 ? '' : 's'} faulted`}
                style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)', minWidth: 17, textAlign: 'center' }}
              >
                {/* Says what it counts: faulted PARTS (the faults card counts
                    its own conditions separately). */}
                {item.badge} {item.badge === 1 ? 'part' : 'parts'}
              </span>
            )}
          </button>
        ))}
      </div>

      <div className="sidebar-spacer flex-1" />

      {/* Saved sessions, then the active board's identity: what is remembered,
          then what is loaded. On a phone the rail is the bottom tab bar, which
          has no room for either (the landing page offers the last session). */}
      <div className="sidebar-extra">{sessions}</div>

      {/* Active board identity, pinned */}
      {report?.ok && (
        <div
          data-testid="sidebar-board"
          className="sidebar-label mx-2.5 mb-2.5 px-3 py-2.5 rounded-[10px]"
          style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}
        >
          <div
            className="text-[12px] font-semibold truncate"
            title={boardLabel ?? report.board_name}
            style={{ color: 'var(--silk)' }}
          >
            {report.board_name || boardLabel || report.file_name}
          </div>
          <div className="text-[11px] mt-0.5 tnum" style={{ color: 'var(--silk-faint)' }}>
            {plural(report.num_components, 'part')} · {plural(report.num_nets, 'net')}
          </div>
          {analyzedAt && (
            <div className="text-[10px] mt-0.5" style={{ color: 'var(--silk-faint)' }}>
              analyzed {relTime(analyzedAt, now)}
            </div>
          )}
        </div>
      )}

      <ThemeToggle theme={theme} onToggle={onToggleTheme} />
    </nav>
  )
}

/** The rail's footer button that flips between the light and dark themes. */
export function ThemeToggle({ theme, onToggle }: { theme: Theme; onToggle: () => void }) {
  return (
    <div className="theme-cell px-2.5 pb-3">
      <button
        type="button"
        data-testid="theme-toggle"
        className="nav-item"
        onClick={onToggle}
        aria-label={theme === 'dark' ? 'Switch to the light theme' : 'Switch to the dark theme'}
      >
        <span style={{ display: 'inline-flex', flexShrink: 0 }}>
          {theme === 'dark' ? <SunIcon size={15} /> : <MoonIcon size={15} />}
        </span>
        <span className="sidebar-label">{theme === 'dark' ? 'Light theme' : 'Dark theme'}</span>
      </button>
    </div>
  )
}
