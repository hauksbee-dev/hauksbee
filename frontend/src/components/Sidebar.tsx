import { useEffect, useState } from 'react'
import type { WebReport } from '../types/report'
import type { Theme } from '../hooks/useTheme'
import {
  BoardTargetIcon, ChecksIcon, LiveIcon, WrenchIcon, SunIcon, MoonIcon,
} from './Icons'
import { relTime } from '../lib/rel-time'

// The app shell's left rail: wordmark + mark, the four real destinations
// (Board, Checks, Live Sim, Environment), and the active board's identity
// pinned in the footer. No search, no user menu, no fake nav: hauksbee has no
// accounts and no multi-project dashboard, so the rail holds only what exists.

export type AppView = 'board' | 'checks' | 'sim' | 'env'

export interface NavState {
  view: AppView
  setView: (v: AppView) => void
  /** Checks needs an ok report; Live Sim needs a live-capable session. */
  checksEnabled: boolean
  simEnabled: boolean
  /** True while the sim session is running (the Live Sim dot pulses). */
  simRunning: boolean
  faultCount: number
}

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
      hint: nav.checksEnabled ? undefined : 'Analyze a board first',
    },
    {
      id: 'sim', label: 'Live Sim', icon: <LiveIcon size={16} />,
      disabled: !nav.simEnabled,
      hint: nav.simEnabled ? undefined : 'Analyze a board, then drive it live',
      badge: nav.faultCount > 0 ? nav.faultCount : undefined,
    },
    { id: 'env', label: 'Environment', icon: <WrenchIcon size={16} /> },
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
      <div className="flex items-center gap-2.5 px-4" style={{ height: 56, borderBottom: '1px solid var(--hairline)' }}>
        <img src="/favicon.svg" alt="" width={24} height={24} style={{ borderRadius: 6, flexShrink: 0 }} />
        <span
          className="sidebar-label text-[13px] font-semibold tracking-[0.22em]"
          style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
        >
          HAUKSBEE
        </span>
      </div>

      {/* Nav */}
      <div className="flex flex-col gap-1 px-2.5 pt-3">
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
            <span className="sidebar-label flex-1">{item.label}</span>
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

      <div className="flex-1" />

      {/* Saved sessions, then the active board's identity: what is remembered,
          then what is loaded. */}
      {sessions}

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
            {report.num_components} {report.num_components === 1 ? 'part' : 'parts'} ·{' '}
            {report.num_nets} {report.num_nets === 1 ? 'net' : 'nets'}
          </div>
          {analyzedAt && (
            <div className="text-[10px] mt-0.5" style={{ color: 'var(--silk-faint)' }}>
              analyzed {relTime(analyzedAt, now)}
            </div>
          )}
        </div>
      )}

      {/* Theme toggle */}
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
  )
}
