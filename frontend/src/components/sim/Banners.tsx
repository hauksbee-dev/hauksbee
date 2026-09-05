import type { ShortsDisclosure } from '../../types/protocol'

/** A full-width strip above the board, in one of the two banner tones. */
function Strip({ testId, tone, children }: {
  testId: string
  tone: 'warn' | 'note'
  children: React.ReactNode
}) {
  return (
    <div
      data-testid={testId}
      className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5 text-[12px] shrink-0"
      style={tone === 'warn'
        ? { background: 'var(--warn-bg)', borderBottom: '1px solid var(--warn-border)', color: 'var(--silk)' }
        : {
            background: 'var(--surface)',
            borderBottom: '1px solid var(--hairline)',
            borderLeft: '4px solid var(--copper)',
            color: 'var(--silk)',
          }}
    >
      {children}
    </div>
  )
}

/**
 * This view is bound to what /ws streams. When that session was not launched by
 * this page for the analyzed board (a different board, a stale tab, a launch
 * from before a reload), say so explicitly and offer the relaunch, instead of
 * letting the canvas silently show one board under a header claiming another.
 */
export function ForeignSessionBanner({ sessionBoard, expectedBoard, onRelaunch }: {
  sessionBoard: string
  expectedBoard?: string | null
  onRelaunch?: () => void
}) {
  return (
    <Strip testId="sim-foreign-session" tone="warn">
      <span>
        This live session is running{' '}
        <b style={{ fontFamily: 'var(--font-mono)', fontWeight: 600 }}>{sessionBoard}</b>
        {expectedBoard && expectedBoard !== sessionBoard ? (
          <>
            , not the board you analyzed (
            <b style={{ fontFamily: 'var(--font-mono)', fontWeight: 600 }}>{expectedBoard}</b>).
          </>
        ) : expectedBoard ? (
          <>, launched before this page load; it may not match the file you just analyzed.</>
        ) : (
          <>, launched before this page load.</>
        )}
      </span>
      {onRelaunch && expectedBoard && (
        <button
          type="button"
          data-testid="sim-relaunch-current"
          onClick={onRelaunch}
          className="hb-btn hb-press px-2.5 text-[11px]"
          style={{ height: 26 }}
        >
          Relaunch with {expectedBoard}
        </button>
      )}
    </Strip>
  )
}

/**
 * This session's engine either bridged the DRC's detected shorts (so the rails
 * reflect the board as built, matching the report's co-sim block) or refused to
 * (unvalidated layout version). Either way the surface must say so instead of
 * letting idealised or sagged rails pass without explanation.
 */
export function ShortsBanner({ shorts }: { shorts: ShortsDisclosure }) {
  return (
    <Strip testId="sim-shorts-note" tone="note">
      {shorts.bridged > 0 ? (
        <span>
          {shorts.bridged} copper short{shorts.bridged === 1 ? '' : 's'} bridged into the live
          circuit; voltages reflect the board as built, not an idealised un-shorted version
          (the report's co-sim ran with the same bridge).
        </span>
      ) : (
        <span>
          {shorts.detected} copper short{shorts.detected === 1 ? '' : 's'} detected but NOT
          bridged ({shorts.unapplied_reason ?? 'could not be applied'}); the live voltages
          show the un-shorted board.
        </span>
      )}
    </Strip>
  )
}

/**
 * The server stopped this session (a dead analog solve, an engine crash) and
 * said why. The reason must be ON the surface: a stop message that only reaches
 * the devtools console leaves the user watching a frozen clock.
 */
export function ServerErrorBanner({ message }: { message: string }) {
  return <Strip testId="sim-server-error" tone="warn">{message}</Strip>
}

/** The board area while there is no session to draw: a wait on the first
 *  connect, a loss to report once there has been one. */
export function BoardPlaceholder({ sessionLost, expectedBoard, onRelaunch }: {
  sessionLost: boolean
  expectedBoard?: string | null
  onRelaunch?: () => void
}) {
  return (
    <div
      className="absolute inset-0 flex items-center justify-center px-6"
      style={{ background: 'var(--instrument)' }}
      data-testid={sessionLost ? 'sim-session-lost' : 'sim-waiting'}
    >
      {sessionLost ? (
        <div className="text-center" style={{ maxWidth: '28rem' }}>
          <div className="text-[15px] font-semibold" style={{ color: 'var(--err-strong)' }}>
            The live session ended.
          </div>
          <div className="mt-2 text-[13px] leading-relaxed" style={{ color: 'var(--overlay-chip-text)' }}>
            The connection to <code className="hb-inline">/ws</code> dropped, so the board,
            the rails and the scope below have nothing to read. Everything this page
            already recorded is kept; nothing new arrives until a session is back.
          </div>
          <div className="mt-2 text-[12px] flex items-center justify-center gap-2" style={{ color: 'var(--overlay-hint-dim)' }}>
            <span className="slot-spin" /> Retrying every 2 seconds.
          </div>
          {onRelaunch && expectedBoard && (
            <button
              type="button"
              data-testid="sim-relaunch-after-loss"
              onClick={onRelaunch}
              className="hb-btn-primary hb-press px-3.5 text-[12px] mt-3"
              style={{ height: 30 }}
            >
              Launch {expectedBoard} again
            </button>
          )}
          <div className="mt-2.5 text-[12px]" style={{ color: 'var(--overlay-hint-dim)' }}>
            If the server itself is gone, restart it with{' '}
            <code className="hb-inline">hauksbee serve</code> and reload this page.
          </div>
        </div>
      ) : (
        <div className="flex items-center gap-2 text-sm" style={{ color: 'var(--overlay-chip-text)' }}>
          <span className="slot-spin" /> Waiting for the live session ...
        </div>
      )}
    </div>
  )
}
