import { useState, useCallback, useEffect } from 'react'
import type { BoardInfoMsg, StatusMsg, ClientMessage } from '../types/protocol'
import type { ReplayTransport } from '../hooks/useSimulation'
import { PlayIcon, PauseIcon, StepIcon, StepBackIcon, ResetIcon } from './Icons'

// ── The speed scale ──────────────────────────────────────────────────────────
//
// A board that outruns real time is the exception; almost every reason to touch
// this control is to slow the sim down until a transient is legible. The old
// control was linear from 0.1x to 10x, which spent 90% of its travel on speeds
// nobody asks for and made 0.05x unreachable at any position.
//
// So: logarithmic, four decades, 0.001x to 10x. Realtime sits at 75% of the
// track and every one of the first three quarters is a decade of slowing down.
// The server accepts 0.001..1000; the slider covers the useful part of that and
// the typed box below reaches the rest, because "exactly 0.0125x" is a thing
// people want and no slider hits it.
const SPEED_MIN_LOG = -3 // 0.001x
const SPEED_MAX_LOG = 1 // 10x
const SPEED_TICKS = 1000
/** What the server itself will accept (`factor.clamp(0.001, 1000.0)`). */
const SPEED_HARD_MIN = 0.001
const SPEED_HARD_MAX = 1000

const factorToTick = (f: number) => Math.round(
  ((Math.log10(f) - SPEED_MIN_LOG) / (SPEED_MAX_LOG - SPEED_MIN_LOG)) * SPEED_TICKS,
)
const tickToFactor = (t: number) =>
  10 ** (SPEED_MIN_LOG + (t / SPEED_TICKS) * (SPEED_MAX_LOG - SPEED_MIN_LOG))

/** Enough significant figures to read, without a wall of zeros, and WITHOUT
 *  rounding away what the reader typed: a box that answers "0.0125" with
 *  "0.013" has silently disagreed with the rate it is actually running. */
function formatFactor(f: number): string {
  if (f >= 100) return f.toFixed(0)
  if (f >= 10) return Number(f.toPrecision(3)).toString()
  // toPrecision then back through Number drops trailing zeros, so 1 stays "1"
  // and 0.0125 stays "0.0125".
  return Number(f.toPrecision(3)).toString()
}

/** Rounded to something a person would have typed, so nudging the slider does
 *  not send 0.09999999999999998 to the server or show it back to the reader. */
function tidyFactor(f: number): number {
  const clamped = Math.min(SPEED_HARD_MAX, Math.max(SPEED_HARD_MIN, f))
  return Number(clamped.toPrecision(3))
}

const SPEED_PRESETS = [0.001, 0.01, 0.1, 1] as const

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

interface TransportBarProps {
  connected: boolean
  boardInfo: BoardInfoMsg | null
  status: StatusMsg | null
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
}

export function TransportBar({
  connected, boardInfo, status, realtimeFactor, requestedFactor, rateLimited,
  send, replay, history,
}: TransportBarProps) {
  // The rate this client asked for. Seeded from the session's own requested
  // factor when the server reports one, so joining a sim already set to 0.01x
  // shows 0.01x rather than this component's local default.
  const [speed, setSpeed] = useState(1.0)
  const [typed, setTyped] = useState<string | null>(null)
  const serverRequested = status?.requested_factor
  useEffect(() => {
    if (serverRequested && serverRequested > 0) setSpeed(serverRequested)
  }, [serverRequested])

  const running = status?.running ?? false
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

  const handleReset = useCallback(() => {
    history?.resume()
    send({ type: 'Reset' })
  }, [send, history])

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

  const formatTime = (t: number) => {
    if (t < 0.001) return `${(t * 1e6).toFixed(1)} µs`
    if (t < 1) return `${(t * 1000).toFixed(2)} ms`
    return `${t.toFixed(3)} s`
  }

  const quietBtn: React.CSSProperties = {
    background: 'var(--surface-2)',
    border: '1px solid var(--hairline)',
    color: 'var(--silk-dim)',
    cursor: 'pointer',
  }

  return (
    <div
      className="flex items-center gap-3 px-4 shrink-0 select-none"
      style={{
        background: 'var(--surface)',
        borderBottom: '1px solid var(--hairline)',
        height: 46,
      }}
    >
      {/* Transport controls */}
      <div className="flex items-center gap-1.5">
        <button
          onClick={handlePlayPause}
          title={running ? 'Pause (Space)' : 'Play (Space)'}
          aria-label={running ? 'Pause' : 'Play'}
          className="hb-press flex items-center justify-center rounded-lg"
          style={{
            width: 34, height: 30, cursor: 'pointer',
            // running → live green; ready-to-play → copper (the action accent)
            background: running ? 'var(--ok-bg)' : 'var(--copper-tint-strong)',
            border: running ? '1px solid var(--ok-border)' : '1px solid var(--copper-deep)',
            color: running ? 'var(--ok)' : 'var(--copper-hi)',
          }}
        >
          {/* Play triangle sits optically left of center; nudge it right. */}
          {running ? <PauseIcon size={13} /> : <PlayIcon size={13} style={{ marginLeft: 1 }} />}
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

      <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />

      {/* Scrub, replay only: live sim time cannot be dragged backwards. */}
      {replay && replay.duration > 0 && (
        <>
          <div className="flex items-center gap-2 flex-1 min-w-0" style={{ maxWidth: 320 }}>
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
          <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />
        </>
      )}

      {/* Reviewing a retained frame: the sim is not where the screen is. */}
      {reviewing && history && (
        <>
          <div
            data-testid="transport-review"
            className="flex items-center gap-2 px-2.5 py-1 rounded-lg shrink-0"
            style={{
              border: '1px solid var(--copper-deep)',
              background: 'var(--copper-tint-strong)',
              color: 'var(--copper-hi)',
            }}
          >
            <span className="text-[11px] tnum" style={{ fontFamily: 'var(--font-mono)' }}>
              reviewing t={formatTime(history.t ?? 0)} ·{' '}
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
          <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />
        </>
      )}

      {/* Speed: a log scale whose first three quarters are all slower than
          realtime, the presets for the decade marks, and a box for an exact
          rate the slider cannot land on. */}
      <div className="flex items-center gap-2 shrink-0">
        <span className="text-[10px] uppercase tracking-wider" style={{ color: 'var(--silk-faint)' }}>speed</span>
        <input
          type="range"
          min={0}
          max={SPEED_TICKS}
          step={1}
          value={factorToTick(speed)}
          onChange={e => applySpeed(tickToFactor(parseFloat(e.target.value)))}
          aria-label="Simulation speed factor (logarithmic, 0.001x to 10x)"
          data-testid="transport-speed"
          className="w-24 h-1 rounded cursor-pointer"
          style={{ accentColor: 'var(--copper)' }}
        />
        <input
          type="text"
          inputMode="decimal"
          value={typed ?? formatFactor(speed)}
          onChange={e => setTyped(e.target.value)}
          onBlur={commitTyped}
          onKeyDown={e => {
            if (e.key === 'Enter') { commitTyped(); (e.target as HTMLInputElement).blur() }
            if (e.key === 'Escape') setTyped(null)
          }}
          aria-label="Exact simulation speed factor"
          data-testid="transport-speed-exact"
          title={`Type an exact rate (${SPEED_HARD_MIN}x to ${SPEED_HARD_MAX}x)`}
          className="hb-input text-[11px] tnum text-right"
          style={{ width: 58, fontFamily: 'var(--font-mono)', padding: '2px 5px' }}
        />
        <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>x</span>
        <div className="flex items-center gap-0.5">
          {SPEED_PRESETS.map(p => (
            <button
              key={p}
              type="button"
              onClick={() => applySpeed(p)}
              title={`Set the rate to ${formatFactor(p)}x`}
              className="hb-press text-[10px] rounded"
              style={{
                padding: '2px 5px',
                fontFamily: 'var(--font-mono)',
                background: speed === p ? 'var(--copper-tint-strong)' : 'transparent',
                border: `1px solid ${speed === p ? 'var(--copper-deep)' : 'var(--hairline)'}`,
                color: speed === p ? 'var(--copper-hi)' : 'var(--silk-faint)',
                cursor: 'pointer',
              }}
            >
              {formatFactor(p)}
            </button>
          ))}
        </div>
      </div>

      <div className="w-px h-5 shrink-0" style={{ background: 'var(--hairline)' }} />

      {/* Elapsed sim time, and the two rates kept apart. Asked-for and achieved
          are different numbers and the bar never prints one as the other. */}
      <div className="flex items-baseline gap-1.5 shrink-0" style={{ fontFamily: 'var(--font-mono)' }}>
        <span className="text-[10px] uppercase tracking-wider" style={{ color: 'var(--silk-faint)' }}>t</span>
        <span className="text-[13px] font-semibold tnum" style={{ color: 'var(--silk)' }}>{formatTime(simTime)}</span>
        {realtimeFactor !== null && (
          <span
            className="text-[11px] tnum"
            data-testid="transport-rate"
            title={rateLimited
              ? 'The requested rate is not sustainable on this board and backend; the loop is pacing at what it can really deliver.'
              : 'Achieved rate, measured over a rolling window.'}
            style={{ color: rateLimited ? 'var(--warn-strong)' : 'var(--silk-faint)' }}
          >
            · achieving {formatFactor(realtimeFactor)}x
            {requestedFactor != null && requestedFactor > 0
              && Math.abs(requestedFactor - realtimeFactor) > requestedFactor * 0.05 && (
              <> of {formatFactor(requestedFactor)}x asked</>
            )}
            {rateLimited && <> · capped</>}
          </span>
        )}
      </div>

      <div className="flex-1" />

      {/* Board identity + connection */}
      {boardInfo && (
        <span className="text-[11px] truncate" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
          {boardInfo.name}
        </span>
      )}
      <div className="flex items-center gap-1.5 text-[11px]">
        <div
          className={connected ? 'run-dot' : undefined}
          style={{
            width: 8, height: 8, borderRadius: 4,
            background: connected ? 'var(--ok)' : 'var(--err)',
          }}
        />
        <span style={{ color: connected ? 'var(--ok)' : 'var(--err)' }}>
          {connected ? (replay ? 'replay' : 'connected') : 'offline'}
        </span>
      </div>
    </div>
  )
}
