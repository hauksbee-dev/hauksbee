// The count rings on the verdict card. A ring's hole is about 29 px across; the
// card once printed "5000/5000" in it and the ring clipped it to "000/500", so a
// 5000-part board read as a different number. Whatever the count, what is drawn
// inside the ring must fit, and the exact numbers must still be on the page.

import { describe, expect, test } from 'bun:test'
import { percentOf, RING_FRACTION_MAX, ringLabel } from '../src/lib/gauge'

const DIGITS = [1, 3, 4, 5, 6].map(d => 10 ** d - 1)

describe('ringLabel', () => {
  test('a short fraction stays the fraction, with nothing repeated beside it', () => {
    expect(ringLabel(1, 1)).toEqual({ center: '1/1', exact: null })
    expect(ringLabel(4, 5)).toEqual({ center: '4/5', exact: null })
  })

  test('whatever the digit count, the inside of the ring fits its hole', () => {
    for (const total of DIGITS) {
      for (const value of [0, 1, Math.floor(total / 2), total - 1, total]) {
        const { center } = ringLabel(value, total)
        expect(center.length).toBeLessThanOrEqual(RING_FRACTION_MAX)
      }
    }
  })

  test('a long count puts its exact numbers beside the ring, never a clipped fraction', () => {
    expect(ringLabel(5000, 5000)).toEqual({ center: '100%', exact: '5,000 of 5,000' })
    expect(ringLabel(123456, 999999).exact).toBe('123,456 of 999,999')
    for (const total of DIGITS.filter(t => `${t}/${t}`.length > RING_FRACTION_MAX)) {
      const { exact } = ringLabel(total, total)
      expect(exact).toBe(`${total.toLocaleString('en-US')} of ${total.toLocaleString('en-US')}`)
    }
  })

  test('the percentage never claims more than is done', () => {
    expect(percentOf(4999, 5000)).toBe('99%')
    expect(percentOf(1, 5000)).toBe('<1%')
    expect(percentOf(0, 5000)).toBe('0%')
    expect(percentOf(5000, 5000)).toBe('100%')
    expect(percentOf(0, 0)).toBe('0%')
    expect(percentOf(9, 3)).toBe('100%')
  })
})
