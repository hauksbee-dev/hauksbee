// What a count gauge (a ring with a number in it) prints, for any count. The
// ring's hole is about 33 px across, so only a short fraction fits inside it;
// a long one ("5000/5000") is clipped to a wrong-looking "000/500". Past that
// width the ring shows a percentage and the exact numbers go beside it.

/** The widest fraction text that fits the ring's hole at its font size. */
export const RING_FRACTION_MAX = 4

export interface RingLabel {
  /** What is drawn inside the ring. */
  center: string
  /** The exact "N of M", shown beside the ring when `center` is not already it. */
  exact: string | null
}

/** Thousands-grouped, so six digits stay readable: 123,456. */
export const grouped = (n: number): string => n.toLocaleString('en-US')

/** The share done, rounded DOWN: an unfinished gauge never reads "100%", and a
 *  non-empty one never reads "0%". */
export function percentOf(value: number, total: number): string {
  if (!(total > 0)) return '0%'
  const share = Math.max(0, Math.min(1, value / total))
  if (share === 1) return '100%'
  if (share > 0 && share < 0.01) return '<1%'
  return `${Math.floor(share * 100)}%`
}

export function ringLabel(value: number, total: number): RingLabel {
  const fraction = `${value}/${total}`
  if (fraction.length <= RING_FRACTION_MAX) return { center: fraction, exact: null }
  return { center: percentOf(value, total), exact: `${grouped(value)} of ${grouped(total)}` }
}
