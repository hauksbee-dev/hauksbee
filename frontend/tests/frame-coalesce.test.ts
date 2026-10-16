// Frames superseded before a paint must not take their drain-once contents
// with them: a fault the server sent once, UART bytes and chunk extremes.

import { expect, test } from 'bun:test'
import { coalesceFrames } from '../src/lib/frame-coalesce'
import type { SimFrame } from '../src/types/protocol'

const frame = (t: number, extra: Partial<SimFrame> = {}): SimFrame => ({
  type: 'SimFrame', t, realtime_factor: 1, net_voltages: { A: t }, component_states: {}, uart: {}, ...extra,
})

test('a fault in a superseded frame survives into the painted one', () => {
  const fault = { component: 'R1', kind: 'overcurrent', value: 2, limit: 1, t: 1 }
  const painted = coalesceFrames(coalesceFrames(null, frame(1, { faults: [fault] })), frame(2))
  expect(painted.t).toBe(2)
  expect(painted.net_voltages).toEqual({ A: 2 })
  expect(painted.faults).toEqual([fault])
})

test('UART bytes concatenate per MCU in arrival order', () => {
  const painted = coalesceFrames(frame(1, { uart: { U1: [104], U2: [1] } }), frame(2, { uart: { U1: [105] } }))
  expect(painted.uart).toEqual({ U1: [104, 105], U2: [1] })
})

test('chunk extremes widen to cover every coalesced chunk', () => {
  const painted = coalesceFrames(frame(1, { net_v_extremes: { A: [0, 3.3], B: [1, 1] } }),
    frame(2, { net_v_extremes: { A: [0.5, 5] } }))
  expect(painted.net_v_extremes).toEqual({ A: [0, 5], B: [1, 1] })
})

test('a frame with nothing pending passes through unchanged', () => {
  const next = frame(3)
  expect(coalesceFrames(null, next)).toBe(next)
  expect(coalesceFrames(frame(2), next).faults).toBeUndefined()
})
