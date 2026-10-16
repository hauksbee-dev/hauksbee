/**
 * frame-coalesce.ts
 *
 * The live socket paints at most one SimFrame per animation frame, so a frame
 * can be superseded before it is ever committed. Most of a frame is a
 * snapshot, where latest-wins is right. Three fields are not: the server
 * drains each fault into exactly one frame, UART bytes are "since the last
 * frame", and per-net extremes summarise one chunk. Dropping a superseded
 * frame's copy of those loses a fault the user must see, so they are carried
 * into the frame that wins.
 */

import type { SimFrame } from '../types/protocol'

/** `next`, carrying the drain-once fields of the unpainted `pending` frame. */
export function coalesceFrames(pending: SimFrame | null, next: SimFrame): SimFrame {
  if (!pending) return next
  const merged: SimFrame = { ...next }
  const faults = [...(pending.faults ?? []), ...(next.faults ?? [])]
  if (faults.length > 0) merged.faults = faults
  const uart: Record<string, number[]> = { ...pending.uart }
  for (const [mcu, bytes] of Object.entries(next.uart)) uart[mcu] = [...(uart[mcu] ?? []), ...bytes]
  merged.uart = uart
  if (pending.net_v_extremes || next.net_v_extremes) {
    const extremes: Record<string, [number, number]> = { ...pending.net_v_extremes }
    for (const [net, [lo, hi]] of Object.entries(next.net_v_extremes ?? {})) {
      const prev = extremes[net]
      extremes[net] = prev ? [Math.min(prev[0], lo), Math.max(prev[1], hi)] : [lo, hi]
    }
    merged.net_v_extremes = extremes
  }
  return merged
}
