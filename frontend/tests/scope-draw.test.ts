// The live scope's pure helpers: the y axis must contain the data, time labels
// must read in engineering units, a lone sample must be drawn. Also the live
// view's other pure helpers (run state, speed scale, MCU labels, net trace).
// Run with: bun test  (from frontend/)

import { expect, test } from 'bun:test'
import {
  formatSimTime, leftPadFor, planYAxis, tickStep, ticksAcross, timeTickLabeler, traceShape,
} from '../src/components/scope-draw'
import { deriveRunState, formatClock } from '../src/components/sim/run-state'
import {
  factorToTick, formatFactor, isPreset, speedText, tickToFactor, tidyFactor, SPEED_TICKS,
} from '../src/components/sim/speed'
import { chipName, mcuBackendText } from '../src/components/sim/mcu-label'
import { netBehaviour, netTrace } from '../src/components/sim/LiveNetCard'
import type { SimFrame } from '../src/types/protocol'

// ---- y axis ---------------------------------------------------------------

test('a 5 V trace sits inside the axis with headroom, not on its top edge', () => {
  // The audit's bug: the axis stopped at 4 V and clipped a 5.000 V probe away.
  const axis = planYAxis(5, 5, [0, 3.3])!
  expect(axis.lo).toBe(0)
  expect(axis.hi).toBeGreaterThan(5)
  expect(axis.ticks[0]).toBe(axis.lo)
  expect(axis.ticks[axis.ticks.length - 1]).toBe(axis.hi)
})

test('the axis always contains the data, wherever it is', () => {
  const cases: [number, number][] = [
    [0, 5], [-0.3, 3.3], [-12, 12], [0.001, 0.002], [4.9999, 5.0001], [100, 250], [-5, -1],
  ]
  for (const [lo, hi] of cases) {
    const a = planYAxis(lo, hi)!
    expect(a.lo).toBeLessThanOrEqual(lo)
    expect(a.hi).toBeGreaterThanOrEqual(hi)
    // Rounded OUT to a tick: the ends are tick values.
    expect(a.ticks[0]).toBe(a.lo)
    expect(a.ticks[a.ticks.length - 1]).toBe(a.hi)
  }
})

test('a value already on a tick gets headroom rather than sitting on the edge', () => {
  const a = planYAxis(0, 4, [0, 3.3])!
  expect(a.hi).toBeGreaterThan(4)
})

test('a floor keeps a quiet rail on a stable scale and the zero baseline', () => {
  const a = planYAxis(3.29, 3.31, [0, 3.3])!
  expect(a.lo).toBe(0)
  expect(a.hi).toBeGreaterThanOrEqual(3.3)
})

test('negative data takes the baseline down with it', () => {
  const a = planYAxis(-0.3, 3.3, [0, 3.3])!
  expect(a.lo).toBeLessThan(-0.3)
  expect(a.ticks).toContain(0)
})

test('a flat or lone value still gets a frame to sit in', () => {
  const a = planYAxis(2, 2)!
  expect(a.lo).toBeLessThan(2)
  expect(a.hi).toBeGreaterThan(2)
})

test('no finite data and no floor plans no axis', () => {
  expect(planYAxis(Infinity, -Infinity)).toBeNull()
})

test('ticks are round, ascending and about five', () => {
  expect(tickStep(5)).toBe(1)
  expect(ticksAcross(0, 5)).toEqual([0, 1, 2, 3, 4, 5])
  const t = planYAxis(0, 5, [0, 3.3])!.ticks
  expect(t.length).toBeGreaterThanOrEqual(3)
  expect(t.length).toBeLessThanOrEqual(12)
  for (let i = 1; i < t.length; i++) expect(t[i]).toBeGreaterThan(t[i - 1])
})

test('the left pad still grows with the widest label', () => {
  // leftPadFor measures through a canvas context; a fake one is enough.
  const ctx = { measureText: (s: string) => ({ width: s.length * 6 }) } as unknown as CanvasRenderingContext2D
  expect(leftPadFor(ctx, ['0', '5'], 400)).toBeLessThan(leftPadFor(ctx, ['-12.500', '12.500'], 400))
})

// ---- time labels ----------------------------------------------------------

test('sim time reads in the unit a person would say', () => {
  expect(formatSimTime(2.509611)).toBe('2.51 s')
  expect(formatSimTime(0.0125)).toBe('12.5 ms')
  expect(formatSimTime(0.00084)).toBe('840 µs')
  expect(formatSimTime(0)).toBe('0 s')
  // Never the raw microsecond count the audit caught.
  expect(formatSimTime(2.509611)).not.toContain('µs')
})

test('a 1 ms window at 2.5 s has one unit and no duplicate tick labels', () => {
  const lo = 2.5096
  const hi = 2.5106
  const label = timeTickLabeler(lo, hi)
  const labels = ticksAcross(lo, hi).map(label)
  expect(new Set(labels).size).toBe(labels.length)
  for (const l of labels) expect(l.endsWith(' s')).toBe(true)
  expect(labels.join(' ')).not.toContain('µs')
})

test('a whole-second window needs no decimals; a millisecond one needs a unit change', () => {
  expect(timeTickLabeler(0, 5)(2)).toBe('2 s')
  expect(timeTickLabeler(0, 0.005)(0.002)).toBe('2 ms')
  expect(timeTickLabeler(0, 0.000005)(0.000002)).toBe('2 µs')
})

// ---- the shape of a trace -------------------------------------------------

test('a trace is nothing, one held sample, or a line', () => {
  expect(traceShape([], 0, 1).kind).toBe('none')
  expect(traceShape([{ x: 5, y: 1 }], 0, 1).kind).toBe('none')
  const one = traceShape([{ x: 0.5, y: 5 }], 0, 1)
  expect(one.kind).toBe('point')
  const line = traceShape([{ x: 0.1, y: 1 }, { x: 0.2, y: 2 }, { x: 9, y: 3 }], 0, 1)
  expect(line.kind).toBe('line')
  if (line.kind === 'line') expect(line.points).toHaveLength(2)
})

// ---- run state ------------------------------------------------------------

test('status words follow the derived running flag and the clock', () => {
  const base = { connected: true, running: false, simTime: 0 }
  expect(deriveRunState(base)).toBe('ready')
  expect(deriveRunState({ ...base, simTime: 2.5 })).toBe('paused')
  expect(deriveRunState({ ...base, simTime: 2.5, running: true })).toBe('running')
  expect(deriveRunState({ ...base, connected: false, running: true })).toBe('offline')
})

test('the clock text keeps changing on a 1 ms step late in a run', () => {
  expect(formatClock(2.5)).not.toBe(formatClock(2.501))
  expect(formatClock(0.0005)).toBe('500.0 µs')
  expect(formatClock(0.0125)).toBe('12.50 ms')
})

// ---- speed ----------------------------------------------------------------

test('the speed scale round-trips and real time is a preset', () => {
  expect(factorToTick(0.001)).toBe(0)
  expect(factorToTick(10)).toBe(SPEED_TICKS)
  expect(factorToTick(1)).toBe(750)
  expect(tickToFactor(750)).toBeCloseTo(1, 9)
  expect(isPreset(1)).toBe(true)
  expect(isPreset(0.0125)).toBe(false)
})

test('speeds print as a person would type them', () => {
  expect(formatFactor(0.0125)).toBe('0.0125')
  expect(formatFactor(1)).toBe('1')
  expect(formatFactor(250)).toBe('250')
  expect(speedText(0.1)).toBe('0.1×')
  expect(tidyFactor(0.09999999999999998)).toBe(0.1)
  expect(tidyFactor(1e9)).toBe(1000)
  expect(tidyFactor(0)).toBe(0.001)
})

// ---- MCU names ------------------------------------------------------------

test('backend ids become chip and emulator names', () => {
  expect(mcuBackendText('simavr:atmega328p')).toBe('ATmega328P (AVR emulator)')
  expect(mcuBackendText('qemu:esp32c3')).toBe('ESP32-C3 (QEMU emulator)')
  expect(mcuBackendText('renode:stm32f072')).toBe('STM32F072 (Renode emulator)')
  // An id the table does not know is shown as the chip alone, never the raw id.
  expect(mcuBackendText('mystery:rp2040')).toBe('RP2040')
  expect(chipName('attiny85')).toBe('ATtiny85')
})

// ---- the live net card ----------------------------------------------------

const f = (t: number, v: number | undefined): SimFrame => ({
  t, net_voltages: v === undefined ? {} : { VCC: v },
} as unknown as SimFrame)

test('a net trace is the measured frames, oldest first, and stops at a restart', () => {
  const frames = [f(5, 1), f(6, 1), f(0.1, 2), f(0.2, 3)]
  expect(netTrace(frames, 'VCC')).toEqual([{ x: 0.1, y: 2 }, { x: 0.2, y: 3 }])
})

test('a net with no value in a frame draws no point for it', () => {
  expect(netTrace([f(0.1, 5), f(0.2, undefined), f(0.3, 5)], 'VCC').map(p => p.x)).toEqual([0.1, 0.3])
})

test('the behaviour sentence is honest about what was measured', () => {
  const steady = { kind: 'measured', volts: 5, min: 5, max: 5, moving: false } as const
  const moving = { kind: 'measured', volts: 1, min: 0, max: 5, moving: true } as const
  expect(netBehaviour(steady, 0.5)).toBe('Steady over the last 500 ms.')
  expect(netBehaviour(moving, null)).toBe('Moving: 0.00 V to 5.00 V.')
  expect(netBehaviour({ kind: 'unobserved' }, null)).toContain('no measured level')
  expect(netBehaviour({ kind: 'unsupported' }, null)).toContain('cannot model')
  expect(netBehaviour({ kind: 'absent' }, null)).toContain('Press Play')
})
