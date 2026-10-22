import { useState, useCallback, useEffect, useRef } from 'react'
import type { StatusMsg, ClientMessage } from '../types/protocol'
import type { ReplayTransport } from '../hooks/useSimulation'
import { PlayIcon, PauseIcon, StepIcon, StepBackIcon, ResetIcon } from './Icons'
import { useDismissable } from '../hooks/useDismissable'
import { PHONE_WIDTH, useMediaQuery } from '../hooks/useMediaQuery'
import { formatClock, type RunState } from './sim/run-state'
import {
  SPEED_HARD_MAX, SPEED_HARD_MIN, SPEED_PRESETS, SPEED_TICKS,
  factorToTick, formatFactor, isPreset, speedText, tickToFactor, tidyFactor,
} from './sim/speed'

/** Stepping back through the frames the client retained. The engine has no
 *  reverse: nothing in the solver, the server, or the wire can un-step a
 *  simulation, and pretending otherwise would be a lie about what was computed.
 *  What the client DOES have is the last N frames it was sent, each a complete
 *  state, so this walks that buffer. It is a review of what already happened,
 *  bounded by the buffer, and the bar says so. */
export interface HistoryReview {
  /** Frames currently retained, i.e. how far back review can reach. */
  retained: number
  /** Position in the retained window, or null while showing the live frame. */
  index: number | null
  /** Sim time of the frame on screen while reviewing. */
  t: number | null
  /** False once the oldest retained frame is on screen: the buffer ends here. */
  canStepBack: boolean
  stepBack: () => void
  /** Forward one retained frame; past the newest, hands back to the live feed. */
  stepForward: () => void
  /** Abandon the review and return to the live frame. */
  resume: () => void
}

// The live sim's transport row: play/pause, step, restart, speed, elapsed sim
// time and the realtime factor, with the connection state at the far end. The
// shell's sidebar owns navigation and the wordmark, so this row is purely the
// instrument's controls. With a replay source the row gains what only a
// finite recording can offer: a scrub slider over the whole timeline.
//
// There is ONE run status here, and it is the play button: it reads `runState`,
// which is derived from the same stream as the clock (see sim/run-state.ts), so
// the button, the header chip and the clock cannot disagree.

interface TransportBarProps {
  connected: boolean
  status: StatusMsg | null
  /** What the sim is doing, derived from the clock rather than the loop flag. */
  runState: RunState
  /** The rate the loop ACHIEVED (`SimFrame.realtime_factor`). */
  realtimeFactor: number | null
  /** The rate the sim was ASKED for (`SimFrame.requested_factor`). Kept apart
   *  from the achieved one on purpose: the bar shows both rather than one
   *  number that could be either. */
  requestedFactor?: number | null
  /** The loop is pacing below the request because that rate is not sustainable
   *  here (`SimFrame.rate_limited`). */
  rateLimited?: boolean
  send: (msg: ClientMessage) => void
  /** Present when the source is a recorded replay: enables the scrubber and
   *  relabels the connection dot (a loaded file is not a live link). */
  replay?: ReplayTransport
  /** Stepping back through retained frames. Absent for a replay source, which
   *  has a real timeline to scrub instead. */
  history?: HistoryReview
  /** The session's own tools (supply, run length, faults, export), after the
   *  sim time. On a phone they sit behind one "Tools" button, so the transport
   *  is two rows rather than five. */
  tools?: React.ReactNode
  /** Something in the tools needs eyes even while they are folded away (parts
   *  faulted). Printed on the phone's Tools button; the full chip is inside. */
  toolsAlert?: string | null
}

export function TransportBar({
  connected, status, runState, realtimeFactor, requestedFactor, rateLimited,
  send, replay, history, tools, toolsAlert,
}: TransportBarProps) {
  const phone = useMediaQuery(PHONE_WIDTH)
  // The rate this client asked for. Seeded from the session's own requested
  // factor when the server reports one, so joining a sim already set to 0.01x
  // shows 0.01x rather than this component's local default.
  const [speed, setSpeed] = useState(1.0)
  const [typed, setTyped] = useState<string | null>(null)
  const [moreOpen, setMoreOpen] = useState(false)
  const moreRef = useRef<HTMLSpanElement>(null)
  useDismissable(moreOpen, moreRef, setMoreOpen)
  const serverRequested = status?.requested_factor
  useEffect(() => {
    if (serverRequested && serverRequested > 0) setSpeed(serverRequested)
  }, [serverRequested])

  const running = runState === 'running'
  const simTime = status?.sim_time ?? 0
  const reviewing = history?.index != null

  const handlePlayPause = useCallback(() => {
    // Leaving the review is what "play" means while reviewing: the live frame
    // has to be back on screen before the sim starts moving again.
    if (reviewing) history?.resume()
    send({ type: running ? 'Pause' : 'Play' })
  }, [running, send, reviewing, history])

  const handleStep = useCallback(() => {
    if (reviewing) { history!.stepForward(); return }
    send({ type: 'Step', dt: 0.001 })
  }, [send, reviewing, history])

  const handleReset = useCallback(() => send({ type: 'Reset' }), [send])

  const applySpeed = useCallback((f: number) => {
    const tidy = tidyFactor(f)
    setSpeed(tidy)
    setTyped(null)
    send({ type: 'SetSpeed', factor: tidy })
  }, [send])

  const commitTyped = useCallback(() => {
    if (typed === null) return
    const v = parseFloat(typed)
    if (Number.isFinite(v) && v > 0) applySpeed(v)
    else setTyped(null)
  }, [typed, applySpeed])

  const quietBtn: React.CSSProperties = {
    background: 'var(--surface-2)',
    border: '1px solid var(--hairline)',
    color: 'var(--silk-dim)',
    cursor: 'pointer',
  }

  const ready = runState === 'ready'
  const rateMissed = realtimeFactor !== null && requestedFactor != null && requestedFactor > 0
    && Math.abs(requestedFactor - realtimeFactor) > requestedFactor * 0.05

  return (
    <div
      className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 max-sm:px-3 py-2 min-h-[46px] shrink-0 select-none xl:py-0"
      style={{
        background: 'var(--surface)',
        borderBottom: '1px solid var(--hairline)',
      }}
    >
      {/* Transport controls */}
      <div className="flex items-center gap-1.5 shrink-0 max-sm:order-1">
        <button
          onClick={handlePlayPause}
          data-testid="transport-play"
          data-run-state={runState}
          title={running ? 'Pause (Space)' : 'Play (Space)'}
          aria-label={running ? 'Pause' : 'Play'}
          className="hb-press flex items-center justify-center gap-1.5 rounded-lg text-[12px] font-semibold"
          style={{
            minWidth: 34, height: 30, padding: '0 10px 0 9px', cursor: 'pointer',
            // running → live green; ready-to-play → copper (the action accent),
            // with a halo until the first press so "what now?" has one answer.
            background: running ? 'var(--ok-bg)' : 'var(--copper-tint-strong)',
            border: running ? '1px solid var(--ok-border)' : '1px solid var(--copper-deep)',
            color: running ? 'var(--ok)' : 'var(--copper-hi)',
            boxShadow: ready ? '0 0 0 3px var(--copper-tint)' : undefined,
          }}
        >
          {/* Play triangle sits optically left of center; nudge it right. */}
          {running ? <PauseIcon size={13} /> : <PlayIcon size={13} style={{ marginLeft: 1 }} />}
          <span aria-hidden="true">{running ? 'Pause' : 'Play'}</span>
        </button>

        {history && (
          <button
            onClick={history.stepBack}
            disabled={!history.canStepBack}
            data-testid="transport-step-back"
            title={history.canStepBack
              ? `Back one retained frame (${history.retained} kept)`
              : history.retained === 0
                // Two different reasons to be unavailable, and saying the wrong
                // one would send the reader looking for a bug.
                ? 'Nothing retained yet: run the sim and frames will accumulate'
                : 'No further back: this is the oldest retained frame'}
            aria-label="Step back one retained frame"
            className="hb-press flex items-center justify-center rounded-lg"
            style={{
              ...quietBtn,
              width: 34,
              height: 30,
              opacity: history.canStepBack ? 1 : 0.35,
              cursor: history.canStepBack ? 'pointer' : 'not-allowed',
              ...(reviewing ? { borderColor: 'var(--copper-deep)', color: 'var(--copper-hi)' } : null),
            }}
          >
            <StepBackIcon size={13} />
          </button>
        )}

        <button
          onClick={handleStep}
          title={reviewing ? 'Forward one retained frame' : 'Step 1 ms (N)'}
          aria-label={reviewing ? 'Step forward one retained frame' : 'Step one millisecond'}
          className="hb-press flex items-center justify-center rounded-lg"
          style={{ ...quietBtn, width: 34, height: 30 }}
        >
          <StepIcon size={13} />
        </button>

        <button
          onClick={handleReset}
          title="Restart the simulation"
          aria-label="Restart the simulation"
          className="hb-press flex items-center justify-center rounded-lg"
          style={{ ...quietBtn, width: 34, height: 30 }}
        >
          <ResetIcon size={13} />
        </button>
      </div>

      <div className="hidden xl:block w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />

      {/* Scrub, replay only: live sim time cannot be dragged backwards. */}
      {replay && replay.duration > 0 && (
        <>
          {/* A floor under the scrub, not just a ceiling. `flex-1 min-w-0` in a
              crowded bar resolves to zero, and a range input at zero width is a
              focusable control with nothing to aim at: it was 0 by 4 pixels
              inside the embedded widget. The bar gives up elsewhere first. */}
          <div className="flex items-center gap-2 flex-1 min-w-[110px]" style={{ maxWidth: 320 }}>
            <input
              type="range"
              min={0}
              max={replay.duration}
              step={replay.duration / 500}
              value={replay.position}
              onChange={e => replay.seek(parseFloat(e.target.value))}
              aria-label="Scrub the recording"
              data-testid="replay-scrub"
              className="w-full h-1 rounded cursor-pointer"
              style={{ accentColor: 'var(--copper)' }}
            />
            <span
              className="text-[10px] tnum shrink-0"
              style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
            >
              / {replay.duration.toFixed(1)}s
            </span>
          </div>
          <div className="hidden xl:block w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />
        </>
      )}

      {/* Reviewing a retained frame: the sim is not where the screen is. */}
      {reviewing && history && (
        <>
          <div
            data-testid="transport-review"
            className="flex flex-wrap items-center gap-2 px-2.5 py-1 rounded-lg max-w-full min-w-0"
            style={{
              border: '1px solid var(--copper-deep)',
              background: 'var(--copper-tint-strong)',
              color: 'var(--copper-hi)',
            }}
          >
            <span className="text-[11px] tnum min-w-0 break-words" style={{ fontFamily: 'var(--font-mono)' }}>
              reviewing t={formatClock(history.t ?? 0)} ·{' '}
              {history.retained - (history.index ?? 0)} back of {history.retained} kept
            </span>
            <button
              type="button"
              onClick={history.resume}
              className="hb-press text-[11px] underline"
              style={{ background: 'none', border: 'none', color: 'inherit', cursor: 'pointer', padding: 0 }}
            >
              live
            </button>
          </div>
          <div className="hidden xl:block w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />
        </>
      )}

      {/* Speed: ONE control. The segments are the speeds people reach for
          (everything left of 1× is slow motion); an exact rate, or one the
          segments do not hold, lives under "More". */}
      <div className="flex items-center gap-2 min-w-0 max-w-full max-sm:order-4">
        <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }} title="How much sim time passes per second of real time">Speed</span>
        <div
          role="group"
          aria-label="Simulation speed"
          data-testid="transport-speed"
          className="inline-flex rounded-md overflow-hidden"
          style={{ border: '1px solid var(--hairline)' }}
        >
          {SPEED_PRESETS.map(p => (
            <SpeedSegment key={p} on={speed === p} onClick={() => applySpeed(p)} title={`Run at ${speedText(p)} real time`}>
              {speedText(p)}
            </SpeedSegment>
          ))}
          {!isPreset(speed) && (
            <SpeedSegment on onClick={() => setMoreOpen(true)} title="A custom speed. Open More to change it.">
              {speedText(speed)}
            </SpeedSegment>
          )}
        </div>
        <span ref={moreRef} className="relative shrink-0">
          <button
            type="button"
            data-testid="transport-speed-more"
            aria-expanded={moreOpen}
            onClick={() => setMoreOpen(o => !o)}
            className="hb-press text-[11px] rounded-md cursor-pointer"
            style={{ ...quietBtn, height: 24, padding: '0 8px' }}
          >
            More ▾
          </button>
          {moreOpen && (
            <div
              className="hb-card absolute z-30 p-3 flex flex-col gap-2"
              style={{ top: 'calc(100% + 4px)', left: 0, width: 260, maxWidth: '80vw', boxShadow: 'var(--shadow-pop)' }}
            >
              <div className="text-[11px]" style={{ color: 'var(--silk-dim)' }}>
                Sim seconds per real second. Below 1× slows the board down until a fast transient is readable.
              </div>
              <input
                type="range"
                min={0}
                max={SPEED_TICKS}
                step={1}
                value={factorToTick(speed)}
                onChange={e => applySpeed(tickToFactor(parseFloat(e.target.value)))}
                aria-label="Simulation speed, logarithmic from 0.001× to 10×"
                className="w-full h-1 rounded cursor-pointer"
                style={{ accentColor: 'var(--copper)' }}
              />
              <div className="flex items-center gap-1.5">
                <label htmlFor="transport-speed-exact" className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>Exact</label>
                <input
                  id="transport-speed-exact"
                  type="text"
                  inputMode="decimal"
                  value={typed ?? formatFactor(speed)}
                  onChange={e => setTyped(e.target.value)}
                  onBlur={commitTyped}
                  onKeyDown={e => {
                    if (e.key === 'Enter') { commitTyped(); (e.target as HTMLInputElement).blur() }
                    if (e.key === 'Escape') setTyped(null)
                  }}
                  aria-label="Exact simulation speed"
                  data-testid="transport-speed-exact"
                  title={`Type an exact speed (${SPEED_HARD_MIN}× to ${SPEED_HARD_MAX}×)`}
                  className="hb-input text-[11px] tnum text-right"
                  style={{ width: 72, fontFamily: 'var(--font-mono)', padding: '2px 5px' }}
                />
                <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>×</span>
              </div>
            </div>
          )}
        </span>
      </div>

      <div className="hidden xl:block w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />

      {/* Elapsed sim time, and the two rates kept apart. Asked-for and achieved
          are different numbers and the bar never prints one as the other. */}
      <div className="flex flex-wrap items-baseline gap-1.5 min-w-0 max-w-full max-sm:order-2" style={{ fontFamily: 'var(--font-mono)' }}>
        <span className="text-[10px] uppercase tracking-wider" style={{ color: 'var(--silk-faint)' }}>t</span>
        <span
          className="text-[13px] font-semibold tnum"
          data-testid="transport-time"
          style={{ color: 'var(--silk)' }}
        >
          {formatClock(simTime)}
        </span>
        {/* The achieved rate, compact: "real time" is what the Speed control
            already says, and the long form (with the asked-for rate) is in the
            hover. It wrapped the toolbar at 1440 once the asked-for text
            joined it. Only a rate the loop CANNOT reach is worth more of the
            bar, because that one changes what the user should conclude. A
            phone has no room for it at all, so it is not rendered there (a
            CSS-hidden node would still sit in the bar, zero-sized and outside
            every clip box, whenever the advancing-status grace flips running). */}
        {!phone && running && realtimeFactor !== null && Number.isFinite(realtimeFactor) && (
          <span
            className="text-[11px] tnum"
            data-testid="transport-rate"
            title={[
              `Running at ${speedText(realtimeFactor)} real time (measured over a rolling window).`,
              rateMissed
                ? (rateLimited
                  ? `The requested ${speedText(requestedFactor ?? 0)} is not sustainable on this board and emulator; the loop is pacing at what it can really deliver.`
                  : `Asked for ${speedText(requestedFactor ?? 0)}.`)
                : '',
            ].filter(Boolean).join(' ')}
            style={{ color: rateLimited ? 'var(--warn-strong)' : 'var(--silk-faint)' }}
          >
            · {speedText(realtimeFactor)}
            {rateLimited && rateMissed && <> of {speedText(requestedFactor ?? 0)} asked</>}
          </span>
        )}
      </div>

      {tools && (phone
        ? <ToolsPopover alert={toolsAlert ?? null}>{tools}</ToolsPopover>
        : <div className="flex flex-wrap items-center gap-1.5 min-w-0 max-w-full">{tools}</div>)}

      <div className="flex-1 max-sm:order-3" />

      {/* Connection only: the board's name is the page title's, not repeated. */}
      <div className="flex items-center gap-1.5 text-[11px] max-sm:order-3">
        <div
          className={connected ? 'run-dot' : undefined}
          style={{
            width: 8, height: 8, borderRadius: 4,
            background: connected ? 'var(--ok)' : 'var(--err)',
          }}
          role="img"
          aria-label={connected ? 'Connected to the simulator' : 'Not connected'}
          title={connected ? 'Connected to the simulator' : 'Not connected'}
        />
        {/* A healthy live link is the dot alone: the word cost a second toolbar
            row while the rate text was showing. Anything else is spelled out. */}
        {(!connected || replay) && (
          <span style={{ color: connected ? 'var(--ok)' : 'var(--err)' }}>
            {connected ? 'replay' : 'offline'}
          </span>
        )}
      </div>
    </div>
  )
}

function SpeedSegment({ on, onClick, title, children }: {
  on: boolean
  onClick: () => void
  title: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={on}
      title={title}
      className="hb-press text-[11px] tnum cursor-pointer"
      style={{
        height: 24,
        padding: '0 8px',
        fontFamily: 'var(--font-mono)',
        background: on ? 'var(--copper-tint-strong)' : 'transparent',
        border: 'none',
        borderRight: '1px solid var(--hairline)',
        color: on ? 'var(--copper-hi)' : 'var(--silk-dim)',
      }}
    >
      {children}
    </button>
  )
}

/** A phone's folded session tools: one button, and the chips (supply, run
 *  length, faults, export) stacked in a small sheet under it. Closes on an
 *  outside tap, Escape, or after a chip does its job (the Export menu keeps it
 *  open while its own list is showing). */
function ToolsPopover({ alert, children }: { alert: string | null; children: React.ReactNode }) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLSpanElement>(null)
  useDismissable(open, wrap, setOpen)
  return (
    <span ref={wrap} className="relative shrink-0 max-sm:order-5 max-sm:ml-auto">
      <button
        type="button"
        data-testid="transport-tools"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
        className="hb-press text-[11px] rounded-md cursor-pointer inline-flex items-center gap-1.5"
        style={{
          height: 24, padding: '0 8px',
          background: alert ? 'var(--err-bg)' : 'var(--surface-2)',
          border: `1px solid ${alert ? 'var(--err-border)' : 'var(--hairline)'}`,
          color: alert ? 'var(--err)' : 'var(--silk-dim)',
        }}
      >
        {alert ?? 'Tools'} ▾
      </button>
      {open && (
        <div
          data-testid="transport-tools-sheet"
          className="hb-card absolute right-0 z-30 p-2 flex flex-col items-stretch gap-2"
          style={{ top: 'calc(100% + 4px)', width: 'min(300px, calc(100vw - 24px))', boxShadow: 'var(--shadow-pop)' }}
          onClickCapture={e => {
            const hit = (e.target as HTMLElement).closest('button')
            if (hit && hit.dataset.testid !== 'sim-export') setOpen(false)
          }}
        >
          {children}
        </div>
      )}
    </span>
  )
}
