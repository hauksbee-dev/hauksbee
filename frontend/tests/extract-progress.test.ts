import { expect, test } from 'bun:test'
import { progressAt, remainingLabel } from '../src/components/extract/ExtractProgress'

test('the bar eases to 90% at the estimate, then creeps but never fills', () => {
  const at = (t: number) => progressAt(t, 120)
  expect(at(0)).toBe(0)
  expect(at(120)).toBeCloseTo(0.9, 6)
  const late = [130, 240, 600, 3600].map(at)
  late.reduce((prev, p) => { expect(p).toBeGreaterThan(prev); return p }, at(120))
  expect(Math.max(...late)).toBeLessThan(1)
})

test('the label claims only whole minutes, then admits the overrun', () => {
  expect(remainingLabel(0, 180)).toBe('about 3 min left (estimate)')
  expect(remainingLabel(150, 180)).toBe('under a minute left (estimate)')
  expect(remainingLabel(181, 180)).toBe('taking longer than usual')
})
