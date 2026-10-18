import { useEffect, useRef, useState } from 'react'

// What the live sim is DOING, derived from the one stream the clock comes from.
//
// The server's `running` flag says whether the free-running loop is on. It does
// not say whether sim time is moving: "Run 100 ms" and the step button advance
// the clock while the loop is off, so a status read from the flag alone said
// "paused" over a clock that was counting up. Every status word in the live view
// (the play button, the header chip, the MCU chips) reads this instead, so none
// of them can disagree with T.

/** How long after the clock last moved the sim still counts as advancing. A
 *  free-running sim advances every ~33 ms, a run-for step a few times a second. */
export const ADVANCE_WINDOW_MS = 750

export type RunState =
  /** The session is gone: nothing is running or paused. */
  | 'offline'
  /** Sim time is advancing (free-running, or being stepped through a run). */
  | 'running'
  /** The clock has not moved and never has: waiting for the first Play. */
  | 'ready'
  /** The clock stopped somewhere past zero. */
  | 'paused'

export function deriveRunState(input: {
  connected: boolean
  /** The server's free-running flag. */
  loopRunning: boolean
  /** The clock moved within ADVANCE_WINDOW_MS. */
  advancing: boolean
  simTime: number
}): RunState {
  if (!input.connected) return 'offline'
  if (input.loopRunning || input.advancing) return 'running'
  return input.simTime > 0 ? 'paused' : 'ready'
}

/** True while `simTime` has changed within the last ADVANCE_WINDOW_MS. A restart
 *  (time going back) counts as movement; a first reading does not. */
export function useClockAdvancing(simTime: number | null): boolean {
  const last = useRef<number | null>(null)
  const [advancing, setAdvancing] = useState(false)
  useEffect(() => {
    if (simTime === null) {
      last.current = null
      setAdvancing(false)
      return
    }
    const moved = last.current !== null && simTime !== last.current
    last.current = simTime
    if (!moved) return
    setAdvancing(true)
    const timer = setTimeout(() => setAdvancing(false), ADVANCE_WINDOW_MS)
    return () => clearTimeout(timer)
  }, [simTime])
  return advancing
}

/** The sim clock as the transport bar prints it. Finer than a tick label on
 *  purpose: a 1 ms step at t = 2.5 s must change the text, or the clock looks
 *  frozen while it moves. */
export function formatClock(t: number): string {
  if (t < 0.001) return `${(t * 1e6).toFixed(1)} µs`
  if (t < 1) return `${(t * 1000).toFixed(2)} ms`
  return `${t.toFixed(3)} s`
}

/** The words for a state: the play button's name and the header chip's text
 *  agree because both come from here. */
export const RUN_STATE_TEXT: Record<RunState, string> = {
  offline: 'offline',
  running: 'running',
  ready: 'ready',
  paused: 'paused',
}
