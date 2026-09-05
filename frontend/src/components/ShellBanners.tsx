import type { LaunchState } from '../hooks/useBoardSession'
import { Callout } from './ui'

// The three things the shell has to say wherever you are: a live session that
// would be replaced, a launch that failed, and a saved session that could not
// be reopened.

// Name the emulator a launch failure is missing, or null when the failure is
// something the Environment page cannot fix. Matched on the backends' own
// "not found" wording (hauksbee-mcu's `find_qemu` / `find_renode`), which is
// the text the launch error carries verbatim.
function missingEmulator(error: string): 'Espressif QEMU' | 'Renode' | null {
  if (!/not found/i.test(error)) return null
  if (/espressif qemu|qemu-system-/i.test(error)) return 'Espressif QEMU'
  if (/renode/i.test(error)) return 'Renode'
  return null
}

/** Replace-the-running-session confirmation, in-app: a native window.confirm
 *  is auto-dismissed by automation and unstylable. Covers ALL foreign-session
 *  cases: another board, a stale tab, a launch from before a reload. */
export function ReplaceLiveConfirm({ launch, onReplace, onOpenRunning, onCancel }: {
  launch: Extract<LaunchState, { phase: 'confirm' }>
  onReplace: () => void
  onOpenRunning: () => void
  onCancel: () => void
}) {
  return (
    <Callout
      tone="warn"
      testId="live-replace-confirm"
      className="mx-5 mt-3"
      title="A live session is already running"
    >
      The server is running a live session for{' '}
      <span style={{ fontFamily: 'var(--font-mono)' }}>{launch.activeBoard}</span>
      {launch.activeBoard === launch.targetBoard
        ? ' (launched before this page, so it may not match what you just analyzed)'
        : ''}
      . One session runs at a time.
      <div className="mt-2.5 flex flex-wrap gap-2">
        <button type="button" data-testid="confirm-replace-live" onClick={onReplace} className="hb-btn-primary hb-press px-3 text-[12px]" style={{ height: 30 }}>
          Replace it with {launch.targetBoard}
        </button>
        <button type="button" data-testid="open-running-live" onClick={onOpenRunning} className="hb-btn hb-press px-3 text-[12px]" style={{ height: 30 }}>
          Open the running session
        </button>
        <button type="button" data-testid="cancel-replace-live" onClick={onCancel} className="hb-btn hb-press px-3 text-[12px]" style={{ height: 30 }}>
          Cancel
        </button>
      </div>
    </Callout>
  )
}

/** Launch failures surface verbatim, wherever you are. A missing emulator is
 *  the one launch failure this app can fix by itself, so it offers the
 *  Environment page rather than a release page to find, unpack and PATH. */
export function LaunchErrorBanner({ error, onOpenEnv }: { error: string; onOpenEnv: () => void }) {
  const missing = missingEmulator(error)
  return (
    <Callout
      tone="err"
      testId="live-launch-error"
      className="mx-5 mt-3 py-2.5"
      title="Live launch failed"
    >
      {error}
      {missing && (
        <div className="mt-2.5">
          <button
            type="button"
            data-testid="launch-error-open-env"
            onClick={onOpenEnv}
            className="hb-btn-primary hb-press px-3 text-[12px]"
            style={{ height: 30 }}
          >
            Install {missing} on the Environment page
          </button>
        </div>
      )}
    </Callout>
  )
}

/** A session that could not be reopened. Same place, and the same plainness, as
 *  a launch failure: it says what is missing rather than leaving a click that
 *  did nothing. */
export function ResumeErrorBanner({ reason, onDismiss }: { reason: string; onDismiss: () => void }) {
  return (
    <Callout
      tone="warn"
      testId="resume-error"
      className="mx-5 mt-3 py-2.5"
      title="Could not reopen that session"
    >
      {reason}. Drop the board again and everything you composed against it is still here.
      <div className="mt-2">
        <button
          type="button"
          data-testid="resume-error-dismiss"
          onClick={onDismiss}
          className="hb-btn hb-press px-3 text-[12px]"
          style={{ height: 28 }}
        >
          Got it
        </button>
      </div>
    </Callout>
  )
}
