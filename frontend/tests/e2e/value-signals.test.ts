import { describe, expect, test } from 'bun:test'
import {
  canonicalJson,
  cosimDrovePin,
  cosimFailures,
  cosimPrinted,
  parseFirmwarePlan,
  parseSimTime,
} from './value-signals'

describe('parseSimTime', () => {
  test('reads every unit the transport bar renders', () => {
    expect(parseSimTime('250 µs')).toBeCloseTo(0.00025, 9)
    expect(parseSimTime('12.5 ms')).toBeCloseTo(0.0125, 9)
    expect(parseSimTime('1.5 s')).toBeCloseTo(1.5, 9)
  })

  test('refuses to read a clock it does not understand', () => {
    // Returning 0 here would read downstream as "the simulation made no
    // progress", which is a different and much worse claim than "unreadable".
    expect(() => parseSimTime('—')).toThrow(/unrecognised simulation time/)
    expect(() => parseSimTime('12 minutes')).toThrow(/unrecognised simulation time/)
  })
})

describe('canonicalJson', () => {
  test('compares on content rather than key order', () => {
    expect(canonicalJson({ b: 1, a: [2, { d: 3, c: 4 }] }))
      .toBe(canonicalJson({ a: [2, { c: 4, d: 3 }], b: 1 }))
  })

  test('still separates genuinely different reports', () => {
    expect(canonicalJson({ nets: 1 })).not.toBe(canonicalJson({ nets: 2 }))
  })
})

describe('parseFirmwarePlan', () => {
  test('an absent plan leaves every board unpaired', () => {
    expect(parseFirmwarePlan('[]', 3)).toEqual([null, null, null])
  })

  test('reads a per-board plan and defaults the expectation to cosim', () => {
    const plan = parseFirmwarePlan('[{"path":"/tmp/fw.elf"},null]', 2)
    expect(plan[0]).toEqual({ path: '/tmp/fw.elf', expect: 'cosim' })
    expect(plan[1]).toBeNull()
  })

  test('reads an explicit load-only expectation', () => {
    expect(parseFirmwarePlan('[{"path":"/tmp/fw.hex","expect":"load-only"}]', 1)[0])
      .toEqual({ path: '/tmp/fw.hex', expect: 'load-only' })
  })

  test('refuses a plan that does not line up with the boards', () => {
    // Silently dropping the third board's firmware would report firmware
    // coverage the run never exercised.
    expect(() => parseFirmwarePlan('[{"path":"/tmp/fw.elf"}]', 3))
      .toThrow(/1 entries for 3 boards/)
  })

  test('refuses a relative path and an unknown expectation', () => {
    expect(() => parseFirmwarePlan('[{"path":"fw.elf"}]', 1)).toThrow(/absolute path/)
    expect(() => parseFirmwarePlan('[{"path":"/tmp/fw.elf","expect":"maybe"}]', 1))
      .toThrow(/unknown expect/)
  })

  test('refuses anything that is not a JSON array', () => {
    expect(() => parseFirmwarePlan('not json', 1)).toThrow(/valid JSON/)
    expect(() => parseFirmwarePlan('{"path":"/tmp/fw.elf"}', 1)).toThrow(/JSON array/)
  })
})

describe('cosimDrovePin and cosimPrinted', () => {
  test('a driven pin is pin activity and not serial activity', () => {
    const cosim = { gpio_nets: [{ name: 'PB0', driven: true }] }
    expect(cosimDrovePin(cosim)).toBe(true)
    expect(cosimPrinted(cosim)).toBe(false)
  })

  test('serial output is serial activity and not pin activity', () => {
    // Folding both into one boolean would record pin activity for a run that
    // only wrote to a UART, which is what the field name would then be lying
    // about.
    const cosim = { uart_output: 'boot\n' }
    expect(cosimPrinted(cosim)).toBe(true)
    expect(cosimDrovePin(cosim)).toBe(false)
  })

  test('a run that touched nothing observable shows neither', () => {
    const cosim = { uart_output: '   ', gpio_nets: [{ name: 'PB0', driven: false }] }
    expect(cosimDrovePin(cosim)).toBe(false)
    expect(cosimPrinted(cosim)).toBe(false)
  })
})

describe('cosimFailures', () => {
  test('a firmware that co-simulated is accepted', () => {
    expect(cosimFailures({ ran: true, seconds_simulated: 0.25 }, 'cosim')).toEqual([])
  })

  test('a missing section fails under either expectation', () => {
    expect(cosimFailures(null, 'cosim')).toEqual([
      'firmware was staged but the report carried no co-sim section',
    ])
    expect(cosimFailures(null, 'load-only')).toEqual([
      'firmware was staged but the report carried no co-sim section',
    ])
  })

  test('a section that never ran fails only where a co-sim was demanded', () => {
    expect(cosimFailures({ ran: false }, 'cosim')).toEqual([
      'firmware was staged but the report co-simulated nothing',
    ])
    expect(cosimFailures({ ran: false }, 'load-only')).toEqual([])
  })

  test('a co-sim of zero seconds fails', () => {
    expect(cosimFailures({ ran: true, seconds_simulated: 0 }, 'cosim')).toEqual([
      'firmware co-sim reported zero simulated seconds',
    ])
  })

  test('an inert co-sim is a grading signal, not a journey failure', () => {
    // Not judged here. The value contract fails it (no upload fixes a co-sim
    // that drove no pin, so there is no unlock to name), but that call belongs
    // to the grader, which has the report's own sentences to hand.
    expect(cosimFailures(
      { ran: true, seconds_simulated: 0.25, gpio_nets: [{ driven: false }] },
      'cosim',
    )).toEqual([])
  })

  test('an invalidated analog solve is a grading signal too', () => {
    expect(cosimFailures(
      { ran: true, seconds_simulated: 0.25, analog_valid: false, gpio_nets: [{ driven: true }] },
      'cosim',
    )).toEqual([])
  })
})
