// What the live sim is DOING, in words.
//
// The truth about "is it running" is one derivation, `useAdvancingStatus` in
// SimView: `status.running` is true while the server's loop is on OR sim time
// has advanced recently ("Run 100 ms" and Step move the clock with the loop
// off). This adds only the vocabulary around it, so the play button, the header
// chip and the MCU chips cannot disagree with the clock or with each other.

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
  /** `status.running` as SimView derives it from the clock. */
  running: boolean
  simTime: number
}): RunState {
  if (!input.connected) return 'offline'
  if (input.running) return 'running'
  return input.simTime > 0 ? 'paused' : 'ready'
}

/** The sim clock as the transport bar prints it. Finer than a tick label on
 *  purpose: a 1 ms step at t = 2.5 s must change the text, or the clock looks
 *  frozen while it moves. */
export function formatClock(t: number): string {
  if (t < 0.001) return `${(t * 1e6).toFixed(1)} µs`
  if (t < 1) return `${(t * 1000).toFixed(2)} ms`
  return `${t.toFixed(3)} s`
}
