// The sim speed scale, kept apart from the transport bar so it can be tested.
//
// A board that outruns real time is the exception; almost every reason to touch
// the speed is to slow the sim down until a transient is legible. A linear
// 0.1x..10x track spends 90% of its travel on speeds nobody asks for.
//
// So the fine control is logarithmic, four decades, 0.001x to 10x, with real
// time at 75% of the track. The server accepts 0.001..1000: the slider covers
// the useful part and the typed box reaches the rest, because "exactly
// 0.0125x" is a thing people want and no slider hits it.

const SPEED_MIN_LOG = -3 // 0.001x
const SPEED_MAX_LOG = 1 // 10x
export const SPEED_TICKS = 1000
/** What the server itself will accept (`factor.clamp(0.001, 1000.0)`). */
export const SPEED_HARD_MIN = 0.001
export const SPEED_HARD_MAX = 1000

export const factorToTick = (f: number) => Math.round(
  ((Math.log10(Math.min(SPEED_HARD_MAX, Math.max(SPEED_HARD_MIN, f))) - SPEED_MIN_LOG)
    / (SPEED_MAX_LOG - SPEED_MIN_LOG)) * SPEED_TICKS,
)
export const tickToFactor = (t: number) =>
  10 ** (SPEED_MIN_LOG + (t / SPEED_TICKS) * (SPEED_MAX_LOG - SPEED_MIN_LOG))

/** Enough significant figures to read, without a wall of zeros, and WITHOUT
 *  rounding away what the reader typed: a box that answers "0.0125" with
 *  "0.013" has silently disagreed with the rate it is actually running. */
export function formatFactor(f: number): string {
  if (f >= 100) return f.toFixed(0)
  // toPrecision then back through Number drops trailing zeros, so 1 stays "1"
  // and 0.0125 stays "0.0125".
  return Number(f.toPrecision(3)).toString()
}

/** Rounded to something a person would have typed, so nudging the slider does
 *  not send 0.09999999999999998 to the server or show it back to the reader. */
export function tidyFactor(f: number): number {
  const clamped = Math.min(SPEED_HARD_MAX, Math.max(SPEED_HARD_MIN, f))
  return Number(clamped.toPrecision(3))
}

/** The one-click speeds. Real time is the rightmost: everything else is slow
 *  motion, which is what people reach for. */
export const SPEED_PRESETS = [0.001, 0.01, 0.1, 1] as const

/** "0.1×": the speed as a person would say it. */
export const speedText = (f: number) => `${formatFactor(f)}×`

/** Whether `speed` is one of the presets (so the control can show a custom
 *  value as its own selected segment instead of selecting nothing). */
export const isPreset = (speed: number) => SPEED_PRESETS.some(p => p === speed)
