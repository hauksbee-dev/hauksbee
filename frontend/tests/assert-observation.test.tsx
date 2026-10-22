// "Assert this" / "Assert what I expected": a scope trace or a printed UART line
// becomes a check through the engine's own catalogue (the committed fixture is
// the real `hauksbee-ci explain --json` answer), so the kinds, fields, defaults
// and words are the engine's and nothing here keeps a vocabulary of its own.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { readFileSync } from 'node:fs'
import { AssertThis } from '../src/components/checks/CheckThis'
import {
  assertBlock, blockedBy, groupOf, KIND_GROUPS, missingRequired, needMet, observationKind, observed,
  offerFromObservation, prefill, resolveDefault, traceFacts, wouldFail,
  type CatalogKind, type Observation, type TracePoint,
} from '../src/lib/check-catalog'

const catalog: CatalogKind[] = JSON.parse(
  readFileSync(new URL('./visual-lint/fixtures/ci-catalog.json', import.meta.url), 'utf8'),
).kinds
const entry = (kind: string): CatalogKind => {
  const found = catalog.find(k => k.kind === kind)
  if (!found) throw new Error(`${kind} is not in the catalogue`)
  return found
}
const SPEC = { toml: 'board = "b.kicad_pcb"\nfirmware = "fw.hex"\n', hasFirmware: true }

/** A square wave between `lo` and `hi` at `hz`, sampled every `dt` seconds. */
function square(hz: number, seconds: number, dt = 0.005, lo = 0, hi = 3.3): TracePoint[] {
  const out: TracePoint[] = []
  for (let t = 0; t < seconds; t += dt) out.push({ t, v: Math.floor(t * hz * 2) % 2 === 0 ? lo : hi })
  return out
}

describe('the observation kinds are in the engine catalogue and the menu', () => {
  test('every catalogue kind sits in a named group of the Add a check menu', () => {
    const ungrouped = catalog.map(k => k.kind).filter(k => !KIND_GROUPS.some(([, kinds]) => kinds.includes(k)))
    // Coverage is the catch-all group; the kinds listed here are the ones it holds by default.
    expect(ungrouped.filter(k => groupOf(k) !== 'Coverage')).toEqual([])
    for (const kind of ['bus', 'resets', 'reaches', 'uart_baud']) expect(groupOf(kind)).toBe('Firmware')
  })

  test('the kinds that need the bench record say so and wait for it', () => {
    for (const kind of ['bus', 'resets', 'uart_baud']) {
      const blocked = blockedBy(entry(kind), SPEC, catalog)
      expect(blocked?.need?.need).toBe('bench')
      expect(blocked?.reason).toContain('bench')
    }
    expect(needMet('bench', SPEC)).toBe(false)
    // `reaches` judges the entry point the run already reports.
    expect(blockedBy(entry('reaches'), SPEC, catalog)).toBeUndefined()
  })

  test('bench-sourced defaults fill from the bench record when it has the key', () => {
    const field = entry('uart_baud').fields.find(f => f.key === 'baud')
    expect(field?.default_from?.source).toBe('bench')
    const from = field!.default_from
    expect(resolveDefault(from, { reading: null, nominal: null, ratings: undefined })).toBeNull()
    expect(resolveDefault(from, { reading: null, nominal: null, ratings: undefined, bench: { [from!.key!]: 115200 } })?.value)
      .toBe('115200')
  })

  test('the first printed line fills a uart check once a run has printed one', () => {
    const field = entry('uart').fields.find(f => f.key === 'contains')!
    expect(resolveDefault(field.default_from, { reading: null, nominal: null, ratings: undefined })).toBeNull()
    expect(resolveDefault(field.default_from, {
      reading: null, nominal: null, ratings: undefined, firstUartLine: 'hauksbee-demo v1',
    })).toEqual({ value: 'hauksbee-demo v1', note: 'the first line the firmware printed' })
  })

  test('trace and uart observations are offered by the engine in its own words', () => {
    expect(entry('toggle').offer.trace).toContain('{net}')
    expect(entry('voltage').offer.trace).toContain('{net}')
    expect(entry('uart').offer.uart).toBeTruthy()
    expect(entry('uart_baud').offer.uart).toBeTruthy()
  })
})

describe('what a trace says', () => {
  test('a 5 Hz blink is read as 5 Hz', () => {
    const facts = traceFacts(square(5, 3))!
    expect(facts.edges).toBeGreaterThan(20)
    expect(facts.hz).toBeCloseTo(5, 1)
  })

  test('a rail with ripple is a level, not a toggle', () => {
    const rail = Array.from({ length: 200 }, (_, i) => ({ t: i * 0.01, v: 5 + 0.05 * Math.sin(i) }))
    const facts = traceFacts(rail)!
    expect(facts.edges).toBe(0)
    expect(facts.hz).toBeNull()
  })

  test('two edges are counted, not timed', () => {
    const facts = traceFacts(square(1, 1.2))!
    expect(facts.edges).toBeGreaterThanOrEqual(1)
    expect(facts.hz).toBeNull()
  })

  test('under two samples says nothing', () => {
    expect(traceFacts([{ t: 0, v: 1 }])).toBeNull()
    expect(observed({ kind: 'trace', net: 'D13', points: [] }, 'as_seen')).toBeNull()
  })
})

describe('Assert this, on a scope trace', () => {
  const obs: Observation = { kind: 'trace', net: 'D13', points: square(5, 3) }

  test('a blink opens the toggle check with the observed rate filled in', () => {
    expect(observationKind(obs)).toBe('toggle')
    const seen = observed(obs, 'as_seen')!
    const offer = offerFromObservation(obs, 'as_seen', catalog, SPEC)!
    expect(offer.blocked).toBeUndefined()
    expect(offer.text).toContain('D13')
    const filled = prefill(offer.entry, { net: 'D13' }, {
      reading: { value: seen.reading.value, min: null, max: null }, nominal: null, ratings: undefined, origin: 'this trace',
    }, SPEC)
    expect(Number(filled.values.freq_hz)).toBeCloseTo(5, 1)
    expect(filled.notes.freq_hz).toBe('from this trace')
    expect(missingRequired(offer.entry, filled.values)).toEqual([])
    const block = assertBlock(offer.entry, filled.values)
    expect(block).toContain('kind = "toggle"')
    expect(block).toContain('net = "D13"')
    expect(block).toMatch(/freq_hz = 4\.9|freq_hz = 5/)
  })

  test('Assert what I expected leaves the number to the user and says if it would fail', () => {
    const seen = observed(obs, 'expected')!
    expect(seen.fillReading).toBe(false)
    expect(seen.seed.values).toEqual({})
    // The user expects 10 Hz; the trace says 5: "would FAIL today".
    expect(wouldFail('toggle', { freq_hz: '10', tolerance: '0.1' }, seen.reading)).toBe(true)
    expect(wouldFail('toggle', { freq_hz: '5', tolerance: '0.1' }, seen.reading)).toBe(false)
  })

  test('a trace too short to time a rate asks for a count, and says why', () => {
    const short: Observation = { kind: 'trace', net: 'D13', points: square(1, 1.2) }
    const seen = observed(short, 'as_seen')!
    expect(Number(seen.seed.values.min_toggles)).toBeGreaterThanOrEqual(1)
    expect(seen.seed.notes.min_toggles).toContain('too few to time a rate')
    expect(seen.note).toContain('too few')
  })

  test('a steady rail opens the voltage check around the level it sits at', () => {
    const rail: Observation = { kind: 'trace', net: '+3V3', points: [{ t: 0, v: 3.3 }, { t: 1, v: 3.3 }, { t: 2, v: 3.3 }] }
    expect(observationKind(rail)).toBe('voltage')
    const seen = observed(rail, 'as_seen')!
    const offer = offerFromObservation(rail, 'as_seen', catalog, SPEC)!
    const filled = prefill(offer.entry, { net: '+3V3' }, {
      reading: seen.reading, nominal: null, ratings: undefined, origin: 'this trace',
    }, { toml: '', hasFirmware: false })
    expect(Number(filled.values.min)).toBeLessThan(3.3)
    expect(Number(filled.values.max)).toBeGreaterThan(3.3)
    expect(wouldFail('voltage', { min: '3.1', max: '3.5' }, seen.reading)).toBe(false)
    expect(wouldFail('voltage', { min: '4.0' }, seen.reading)).toBe(true)
  })
})

describe('Assert this, on a UART line', () => {
  const obs: Observation = { kind: 'uart', mcu: 'U1', line: 'hauksbee-demo v1', transcript: 'hauksbee-demo v1\nup\n' }

  test('the line becomes the text the firmware must print', () => {
    const seen = observed(obs, 'as_seen')!
    expect(seen.seed.values.contains).toBe('hauksbee-demo v1')
    expect(seen.seed.values.mcu).toBe('U1')
    expect(seen.seen).toContain('hauksbee-demo v1')
    expect(seen.note).toBe('2 lines printed so far')
    const offer = offerFromObservation(obs, 'as_seen', catalog, SPEC)!
    expect(offer.kind).toBe('uart')
    expect(offer.blocked).toBeUndefined()
    const block = assertBlock(offer.entry, { ...seen.seed.values })
    expect(block).toContain('contains = "hauksbee-demo v1"')
    expect(block).toContain('mcu = "U1"')
  })

  test('without firmware in the spec the offer waits, with the engine\'s reason', () => {
    const offer = offerFromObservation(obs, 'as_seen', catalog, { toml: 'board = "b"\n', hasFirmware: false })!
    expect(offer.blocked?.need?.need).toBe('firmware')
    expect(offer.blocked?.fix?.pickFirmware).toBe(true)
  })

  test('a text the output lacks would fail today, one it holds would pass', () => {
    const { reading } = observed(obs, 'expected')!
    expect(wouldFail('uart', { contains: 'booted ok' }, reading)).toBe(true)
    expect(wouldFail('uart', { contains: 'demo v1' }, reading)).toBe(false)
    expect(wouldFail('uart', { contains: '' }, reading)).toBeNull()
  })

  test('a line with a changing number says to keep the fixed part', () => {
    const counted: Observation = { kind: 'uart', line: 'tick 004211', transcript: 'tick 004211' }
    expect(observed(counted, 'as_seen')!.seed.notes.contains).toContain('may change')
  })

  test('nothing printed yet: nothing to assert as seen, but the user can still write what to expect', () => {
    const none: Observation = { kind: 'uart', line: '', transcript: '' }
    expect(observed(none, 'as_seen')).toBeNull()
    expect(observed(none, 'expected')?.seen).toBe('Nothing printed yet')
  })
})

describe('the entry point', () => {
  test('renders nothing where there is no Checks view to add to', () => {
    const html = renderToStaticMarkup(<AssertThis observation={{ kind: 'uart', line: 'x', transcript: 'x' }} />)
    expect(html).toBe('')
  })
})
