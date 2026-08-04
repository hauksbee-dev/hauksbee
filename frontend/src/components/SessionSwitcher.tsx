import { useEffect, useRef, useState } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import type { SessionsState } from '../hooks/useSessions'
import { hasDefaultName } from '../lib/session-store'
import type { SessionRow } from '../lib/session-store'
import { CloseIcon, HistoryIcon, PencilIcon, TrashIcon } from './Icons'
import { relTime } from '../lib/rel-time'
import { ARRIVE, LEAVE } from '../motion'

// The saved-session indicator in the left rail, and the switcher it opens.
//
// The indicator exists so that "it saved my work" is something the user can SEE
// rather than discover by accident on their next visit; it names the session and
// when it was last written, and it is also the honest place to say when nothing
// is being saved at all (private browsing, storage full). The switcher is a
// list, not a dashboard: hauksbee has no accounts and no projects, so a session
// is a board, its report, and the checks composed against it.
//
// Neither surface pretends a session holds the uploaded file. Every row says
// what resuming it will actually do, because the alternative is a report that
// looks live until the first button that needs the bytes.

function outcomeNote(state: SessionsState): { tone: 'ok' | 'warn' | 'err'; text: string } | null {
  const o = state.outcome
  if (!o) return null
  switch (o.kind) {
    case 'saved':
      return null
    case 'trimmed':
      return {
        tone: 'warn',
        text: 'Saved without the component positions (storage was tight), so a resumed session '
          + 'has no board map until the file is dropped again.',
      }
    case 'metadata-only':
      return {
        tone: 'warn',
        text: 'The report was too large for this browser’s storage. The session keeps its name, '
          + 'its board and its checks; the findings will need a re-run.',
      }
    case 'blocked':
      return { tone: 'err', text: `Nothing is being saved: ${o.reason}.` }
  }
}

const TONE: Record<'ok' | 'warn' | 'err', { bg: string; border: string; color: string }> = {
  ok: { bg: 'var(--ok-bg)', border: 'var(--ok-border)', color: 'var(--ok)' },
  warn: { bg: 'var(--warn-bg)', border: 'var(--warn-border)', color: 'var(--warn-strong)' },
  err: { bg: 'var(--err-bg)', border: 'var(--err-border)', color: 'var(--err-strong)' },
}

export function SessionRail({ state, onResume }: {
  state: SessionsState
  onResume: (id: string) => void
}) {
  const [open, setOpen] = useState(false)
  const [now, setNow] = useState(Date.now())
  const reduced = useReducedMotion()
  const wrap = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000)
    return () => clearInterval(t)
  }, [])

  // Same dismissal as the export menu: an outside click or Escape. The panel is
  // fixed and covers part of the work surface, so re-finding the one button that
  // closes it is not an acceptable way out.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const note = outcomeNote(state)
  const current = state.current
  // The board identity card sits directly under this row, so an auto-named
  // session would print the same file name twice in two adjacent boxes. Until it
  // is renamed, this row says what it IS (the way into the sessions) instead.
  const label = current && !hasDefaultName(current)
    ? current.name
    : state.rows.length > 0 ? 'Sessions' : 'No saved session yet'
  const sub = current
    ? `${state.rows.length > 1 ? `${state.rows.length} saved · ` : ''}saved ${relTime(current.updatedAt, now)}`
    : `${state.rows.length} saved`

  return (
    <div ref={wrap}>
      <div className="px-2.5 pb-1">
        <button
          type="button"
          data-testid="session-indicator"
          className="nav-item"
          onClick={() => setOpen(o => !o)}
          aria-haspopup="dialog"
          aria-expanded={open}
          title={`${label} · ${sub} · open the session switcher`}
          aria-label={`Sessions: ${label}, ${sub}`}
        >
          <span style={{ display: 'inline-flex', flexShrink: 0, color: note ? TONE[note.tone].color : undefined }}>
            <HistoryIcon size={15} />
          </span>
          {/* Two lines inside the rail's row: what the session is called, and
              when it was last written. `min-w-0` on both so a long board name
              ellipses instead of widening the rail. */}
          <span className="sidebar-label flex-1 min-w-0">
            <span className="block truncate text-[12px]" style={{ color: 'var(--silk)' }}>{label}</span>
            <span className="block truncate text-[10px]" style={{ color: 'var(--silk-faint)' }}>{sub}</span>
          </span>
        </button>
      </div>

      <AnimatePresence>
        {open && (
          <motion.div
            data-testid="session-switcher"
            role="dialog"
            aria-label="Saved sessions"
            className="session-panel hb-card"
            initial={reduced ? { opacity: 1 } : { opacity: 0, y: 6 }}
            animate={{ opacity: 1, y: 0 }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, y: 4, transition: LEAVE }}
            transition={reduced ? { duration: 0 } : ARRIVE}
          >
            <div
              className="flex items-center justify-between gap-2 px-3 py-2.5"
              style={{ borderBottom: '1px solid var(--hairline)' }}
            >
              <div className="text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
                Sessions
              </div>
              <button
                type="button"
                data-testid="session-switcher-close"
                onClick={() => setOpen(false)}
                aria-label="Close the session switcher"
                className="hb-press inline-flex items-center justify-center cursor-pointer shrink-0"
                style={{ background: 'none', border: 'none', color: 'var(--silk-faint)', width: 22, height: 22 }}
              >
                <CloseIcon size={13} />
              </button>
            </div>

            <div className="session-panel-body">
              {note && (
                <div
                  data-testid="session-save-note"
                  className="mx-2.5 mt-2.5 rounded-lg px-2.5 py-2 text-[11px] leading-relaxed"
                  style={{
                    background: TONE[note.tone].bg,
                    border: `1px solid ${TONE[note.tone].border}`,
                    color: TONE[note.tone].color,
                  }}
                >
                  {note.text}
                </div>
              )}

              {state.rows.length === 0 ? (
                <div className="px-3 py-3 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
                  Nothing saved yet. Analyze a board and this browser keeps the report and the
                  checks you compose against it, under a name you can change.
                </div>
              ) : (
                state.rows.map(row => (
                  <SessionRowView
                    key={row.id}
                    row={row}
                    now={now}
                    isCurrent={row.id === current?.id}
                    onOpen={() => { setOpen(false); onResume(row.id) }}
                    onRename={name => state.rename(row.id, name)}
                    onDelete={() => state.remove(row.id)}
                  />
                ))
              )}
            </div>

            <div
              className="px-3 py-2.5 text-[11px] leading-relaxed"
              style={{ borderTop: '1px solid var(--hairline)', color: 'var(--silk-faint)' }}
            >
              A session holds the report, which firmware was staged, and the checks you composed.
              It cannot hold the board file itself: browsers do not keep one between visits.
              Opening a session brings everything else back, and re-running it needs the file again
              unless this server still has the board loaded.
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  )
}

function SessionRowView({ row, now, isCurrent, onOpen, onRename, onDelete }: {
  row: SessionRow
  now: number
  isCurrent: boolean
  onOpen: () => void
  onRename: (name: string) => void
  onDelete: () => void
}) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(row.name)
  // Two-step delete, inline. `window.confirm` is auto-dismissed by every
  // automation driver and unstylable besides, which is the same reason the live
  // replace confirmation is in-app.
  const [confirming, setConfirming] = useState(false)

  const facts = [
    `${row.board.numComponents} ${row.board.numComponents === 1 ? 'part' : 'parts'}`,
    `${row.board.numNets} ${row.board.numNets === 1 ? 'net' : 'nets'}`,
    row.checkCount > 0 ? `${row.checkCount} ${row.checkCount === 1 ? 'check' : 'checks'}` : null,
    row.firmwareName ? 'firmware staged' : null,
  ].filter(Boolean).join(' · ')

  return (
    <div
      data-testid="session-row"
      data-current={isCurrent}
      className="px-3 py-2.5"
      style={{ borderTop: '1px solid var(--rule)' }}
    >
      {editing ? (
        <div className="flex flex-wrap items-center gap-1.5">
          <input
            data-testid="session-rename-input"
            className="hb-input min-w-0 flex-1"
            style={{ minWidth: 110 }}
            value={draft}
            autoFocus
            onChange={e => setDraft(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Enter') { onRename(draft); setEditing(false) }
              if (e.key === 'Escape') { setDraft(row.name); setEditing(false) }
            }}
            aria-label="Session name"
          />
          <button
            type="button"
            data-testid="session-rename-save"
            onClick={() => { onRename(draft); setEditing(false) }}
            className="hb-btn-primary hb-press px-2.5 text-[11px] shrink-0"
            style={{ height: 26 }}
          >
            Save
          </button>
          <button
            type="button"
            onClick={() => { setDraft(row.name); setEditing(false) }}
            className="hb-btn hb-press px-2.5 text-[11px] shrink-0"
            style={{ height: 26 }}
          >
            Cancel
          </button>
        </div>
      ) : (
        <>
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0">
              <div
                data-testid="session-row-name"
                className="text-[12.5px] font-semibold truncate"
                title={row.name}
                // Still auto-named, so the name IS a file name and reads as one.
                style={{
                  color: 'var(--silk)',
                  fontFamily: hasDefaultName(row) ? 'var(--font-mono)' : undefined,
                }}
              >
                {row.name}
              </div>
              {/* The board file, when the name is not already it. */}
              {!hasDefaultName(row) && (
                <div
                  className="text-[11px] truncate"
                  title={row.board.fileName}
                  style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}
                >
                  {row.board.fileName}
                </div>
              )}
            </div>
            {isCurrent && (
              <span
                data-testid="session-row-current"
                className="text-[9px] font-bold tracking-widest uppercase px-1.5 rounded-full shrink-0 whitespace-nowrap"
                style={{
                  background: 'var(--copper-tint)', border: '1px solid var(--copper-deep)',
                  color: 'var(--copper-hi)', lineHeight: '16px',
                }}
              >
                open
              </span>
            )}
          </div>
          <div className="text-[10.5px] mt-0.5 tnum" style={{ color: 'var(--silk-faint)' }}>
            {facts} &middot; saved {relTime(row.updatedAt, now)}
          </div>
          {!row.hasReport && (
            <div className="text-[10.5px] mt-0.5" style={{ color: 'var(--warn)' }}>
              report not kept (too large); needs the board file again
            </div>
          )}
          <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
            {!isCurrent && (
              <button
                type="button"
                data-testid="session-open"
                onClick={onOpen}
                disabled={!row.hasReport}
                title={row.hasReport ? undefined : 'This session has no stored report to open'}
                className="hb-btn hb-press px-2.5 text-[11px] shrink-0"
                style={{ height: 26 }}
              >
                Open
              </button>
            )}
            <button
              type="button"
              data-testid="session-rename"
              onClick={() => { setDraft(row.name); setEditing(true) }}
              aria-label={`Rename ${row.name}`}
              className="hb-btn hb-press inline-flex items-center gap-1 px-2 text-[11px] shrink-0"
              style={{ height: 26 }}
            >
              <PencilIcon size={11} /> Rename
            </button>
            {confirming ? (
              <>
                <button
                  type="button"
                  data-testid="session-delete-confirm"
                  onClick={onDelete}
                  className="hb-press px-2 text-[11px] rounded-lg cursor-pointer shrink-0"
                  style={{
                    height: 26, background: 'var(--err-bg)',
                    border: '1px solid var(--err-border)', color: 'var(--err-strong)',
                  }}
                >
                  Delete for good
                </button>
                <button
                  type="button"
                  onClick={() => setConfirming(false)}
                  className="hb-btn hb-press px-2 text-[11px] shrink-0"
                  style={{ height: 26 }}
                >
                  Keep
                </button>
              </>
            ) : (
              <button
                type="button"
                data-testid="session-delete"
                onClick={() => setConfirming(true)}
                aria-label={`Delete ${row.name}`}
                className="hb-btn hb-press inline-flex items-center gap-1 px-2 text-[11px] shrink-0"
                style={{ height: 26 }}
              >
                <TrashIcon size={11} /> Delete
              </button>
            )}
          </div>
        </>
      )}
    </div>
  )
}
