/** The sim view's bottom bar. It used to repeat counts that already have a home
 *  (net count three times over, the probe list under the scope, the fault count
 *  on the toolbar chip, the board's name twice), so it now carries only what
 *  has no other place:
 *
 *  - a lost session, which the transport cannot show once the socket is gone;
 *  - the keyboard shortcuts, on a screen that has a keyboard. While a text box
 *    has focus the shortcuts are suppressed, so the line dims and says why.
 *
 *  Run state, sim time and rate belong to the transport bar above. */
export function SimStatusBar({ sessionLost, inputFocused = false }: {
  /** The socket dropped after a real session. "Paused" is a claim about a sim
   *  that is still there, so a lost session must not wear it. */
  sessionLost: boolean
  /** A text box has focus: Space and N type instead of driving the sim. */
  inputFocused?: boolean
}) {
  return (
    <div
      data-testid="sim-status-bar"
      // No keyboard on a phone, so there is nothing to say there unless the
      // session was lost.
      className={`${sessionLost ? 'flex' : 'hidden sm:flex'} items-center gap-x-3 gap-y-0.5 px-3 py-1 shrink-0 text-[10px] overflow-hidden`}
      style={{
        minHeight: 26,
        background: 'var(--surface)',
        borderTop: '1px solid var(--hairline)',
        flexWrap: 'wrap',
        color: 'var(--silk-faint)',
        opacity: inputFocused ? 0.65 : 1,
        transition: 'opacity 0.2s ease',
      }}
    >
      {sessionLost && <span data-testid="sim-session-ended" style={{ color: 'var(--err)' }}>session ended</span>}
      <span data-testid="sim-shortcuts" className="max-sm:hidden">
        {inputFocused
          ? 'Shortcuts are paused while you type. Click away to use Space and N.'
          : 'Space plays or pauses · N steps 1 ms · scroll zooms · drag pans · hover probes'}
      </span>
    </div>
  )
}
