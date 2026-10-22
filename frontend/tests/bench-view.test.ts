// The bench strip's view-model and the seed logic that reads the same report:
// chips and headline from sample BenchReports, the run view's lane choice, and
// the probes a run starts with. The two sample captures (blinky, thermostat) may
// be replaced by real engine output, so those tests assert on names and shapes,
// never on their numbers.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
  benchLanes, benchView, decodedItems, digitalNets, eventMarkers, eventTone, laneOf, laneRows, liveLane, mcuStrip, pinChip, uartChip,
} from '../src/lib/bench-view'
import { benchSeeds, indicatorNets, seedFrom } from '../src/components/sim/session-seed'
import { reportView } from '../src/lib/report-view'
import type { BenchReport, PinRow, UartRow } from '../src/types/bench'
import type { WebReport } from '../src/types/report'

const fixture = (name: string): BenchReport =>
  JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), 'utf8')) as BenchReport
const blinky = fixture('bench-stm32-blinky.json')
const thermostat = fixture('bench-i2c-thermostat.json')
const parked = fixture('bench-parked.json')

const watchy = JSON.parse(
  readFileSync(new URL('./visual-lint/fixtures/analyze-watchy.json', import.meta.url), 'utf8'),
) as WebReport

const row = (over: Partial<PinRow>): PinRow => ({ pin: 'PA0', state: 'pin_input', edges: 0, summary: 'input / Hi-Z', ...over })

describe('pin chips', () => {
  test('a toggling pin carries the engine summary and its net', () => {
    const c = pinChip(row({ pin: 'PC13', net: 'LED', state: 'pin_toggling', edges: 20, summary: 'toggling 5.0 Hz, 50% duty' }), 'U1')
    expect(c.pin).toBe('PC13')
    expect(c.summary).toBe('toggling 5.0 Hz, 50% duty')
    expect(c.net).toBe('LED')
    expect(c.muted).toBe(false)
    expect(c.title).toContain('LED')
  })

  test('a static pin is a plain chip', () => {
    const c = pinChip(row({ pin: 'PA5', net: 'EN', state: 'pin_static_high', summary: 'static high' }), 'U1')
    expect(c.summary).toBe('static high')
    expect(c.muted).toBe(false)
  })

  test('an input pin is muted', () => {
    const c = pinChip(row({ pin: 'PB0', net: 'BTN' }), 'U1')
    expect(c.muted).toBe(true)
    expect(c.summary).toBe('input / Hi-Z')
  })

  test('a pin with no net cannot be clicked and says why', () => {
    const c = pinChip(row({ pin: 'PB1', state: 'pin_static_low', summary: 'static low' }), 'U1')
    expect(c.net).toBeNull()
    expect(c.title).toContain('not wired to a net')
  })

  test('chip keys are unique across the MCU', () => {
    for (const b of [blinky, thermostat, parked]) {
      const keys = benchView(b).mcus.flatMap(m => m.pins.map(p => p.key))
      expect(new Set(keys).size).toBe(keys.length)
    }
  })
})

describe('uart rows', () => {
  const base: UartRow = { port: 'UART', net: 'TX', bytes: 24, printable_ratio: 1, first_line: 'hello', baud: 115200 }

  test('a decoded baud is stated in baud', () => {
    const c = uartChip(base, 'U1')
    expect(c.baud).toBe('115200 baud')
    expect(c.bytes).toBe('24 bytes')
    expect(c.firstLine).toBe('hello')
    expect(c.garbled).toBeNull()
  })

  test('without a baud the engine note goes in the tooltip, not the chip', () => {
    const c = uartChip({ ...base, baud: null, baud_note: 'baud not decoded yet' }, 'U1')
    expect(c.baud).toBeNull()
    expect(c.baudNote).toBe('baud not decoded yet')
    expect(c.title).toContain('baud not decoded yet')
  })

  test('without a baud or a note the tooltip still says so', () => {
    expect(uartChip({ ...base, baud: undefined }, 'U1').title).toContain('baud')
  })

  test('one byte is singular and a missing first line is null', () => {
    const c = uartChip({ ...base, bytes: 1, first_line: undefined }, 'U1')
    expect(c.bytes).toBe('1 byte')
    expect(c.firstLine).toBeNull()
  })

  test('mostly non-text bytes are called out (what a wrong baud looks like)', () => {
    const c = uartChip({ ...base, printable_ratio: 0.2 }, 'U1')
    expect(c.garbled).toContain('20%')
    expect(uartChip({ ...base, bytes: 0, printable_ratio: 0 }, 'U1').garbled).toBeNull()
  })

  test('a row with no net says why a click does nothing', () => {
    expect(uartChip({ ...base, net: null }, 'U1').title).toContain('not tied to a net')
  })
})

describe('the headline and its issue', () => {
  test('a running core is a plain headline, no issue', () => {
    const m = mcuStrip(blinky.mcus[0])
    expect(m.ref).toBe('U1')
    expect(m.tone).toBe('ok')
    expect(m.issue).toBe(false)
    expect(m.headline).toBe(blinky.mcus[0].headline)
  })

  test('a core parked in an error trap is an error-tone issue with where it sits', () => {
    const m = mcuStrip(parked.mcus[0])
    expect(m.tone).toBe('err')
    expect(m.issue).toBe(true)
    expect(m.headline).toContain('Error_Handler')
    expect(m.detail.join(' ')).toContain('Error_Handler')
    expect(m.detail.join(' ')).toContain('0x08001234')
    expect(m.facts).toContain('2 resets')
  })

  test('a parked core is a warning, a restart alone too', () => {
    const base = { ...parked.mcus[0], park: null }
    expect(mcuStrip({ ...base, core: 'core_parked' }).tone).toBe('warn')
    expect(mcuStrip({ ...base, core: 'core_faulted' }).tone).toBe('err')
    const restarted = mcuStrip({ ...base, core: 'core_running', headline: 'restarted 2 times', resets: 2 })
    expect(restarted.tone).toBe('warn')
    expect(restarted.issue).toBe(true)
    expect(mcuStrip({ ...base, core: 'core_unknown', resets: 0 }).issue).toBe(false)
  })

  test('the core state and the backend come from the label table, never raw ids', () => {
    const m = mcuStrip(parked.mcus[0])
    expect(m.coreLabel).not.toContain('_')
    expect(m.backend).not.toContain('renode:')
  })

  test('one reset is singular; no activity is stated', () => {
    const m = mcuStrip({ ...parked.mcus[0], resets: 1, first_activity_s: null })
    expect(m.facts).toContain('1 reset')
    expect(m.facts.join(' ')).toContain('no activity')
  })

  test('first activity is stated in simulated time', () => {
    expect(mcuStrip(blinky.mcus[0]).facts.join(' ')).toMatch(/first activity at \d/)
  })

  test('the view carries every MCU and the recorder cap', () => {
    const v = benchView({ ...blinky, truncated: true })
    expect(v.mcus).toHaveLength(blinky.mcus.length)
    expect(v.truncated).toBe(true)
    expect(v.durationS).toBe(blinky.duration_s)
  })
})

describe('the run view', () => {
  test('an input pin has no lane; the toggling pin comes first', () => {
    const { rows } = laneRows(blinky, null)
    expect(rows.some(r => r.state === 'pin_input')).toBe(false)
    expect(rows.map(r => r.pin)).toContain('PC13')
  })

  test('the focused pin is always kept, even when lanes are folded', () => {
    const many: BenchReport = {
      ...blinky,
      mcus: [{
        ...blinky.mcus[0],
        pins: Array.from({ length: 14 }, (_, i) => row({
          pin: `PA${i}`, net: `N${i}`, state: i === 13 ? 'pin_static_low' : 'pin_toggling', edges: 2, summary: 's',
          edge_samples: [{ t_s: 0.1, high: true }],
        })),
      }],
    }
    const out = laneRows(many, 'N13', 5)
    expect(out.rows).toHaveLength(5)
    expect(out.folded).toBe(9)
    expect(out.rows.map(r => r.net)).toContain('N13')
    // The board's own order among the lanes that stay.
    const order = out.rows.map(r => Number(r.pin.slice(2)))
    expect(order).toEqual([...order].sort((a, b) => a - b))
  })

  test('benchLanes marks the focused lane and gives each lane its own colour slot', () => {
    const { lanes, rows } = benchLanes(thermostat, 'I2C_SCL')
    expect(lanes).toHaveLength(rows.length)
    expect(lanes.filter(l => l.focused).map(l => l.label)).toEqual(['PB6'])
    expect(new Set(lanes.map(l => l.colorToken)).size).toBe(lanes.length)
  })

  test('a lane starts at the level before its first edge', () => {
    const l = laneOf(row({ pin: 'PA5', state: 'pin_static_high', edges: 1, edge_samples: [{ t_s: 0.1, high: true }] }), 0, null)
    expect(l.initial).toBe(false)
    const none = laneOf(row({ pin: 'PA6', state: 'pin_static_high', final_high: true }), 0, null)
    expect(none.edges).toEqual([])
    expect(none.initial).toBe(true)
  })

  test('truncated edge lists say so; complete ones do not', () => {
    const cut = laneOf(row({ pin: 'PB6', state: 'pin_toggling', edges: 5230, samples_truncated: true, edge_samples: [{ t_s: 0, high: true }] }), 0, null)
    expect(cut.totalEdges).toBeGreaterThan(cut.edges.length)
    const whole = laneOf(row({ pin: 'PB7', state: 'pin_toggling', edges: 1, edge_samples: [{ t_s: 0, high: true }] }), 0, null)
    expect(whole.totalEdges).toBe(whole.edges.length)
  })

  test('a NACKed transaction is an error box, an ACKed one is not', () => {
    const items = decodedItems(thermostat.bus_log)
    expect(items).toHaveLength(thermostat.bus_log!.length)
    const nack = items.find(i => i.err)
    expect(nack).toBeDefined()
    expect(nack!.summary).toContain('NACK')
    expect(items.filter(i => !i.err).length).toBeGreaterThan(0)
    expect(decodedItems(undefined)).toEqual([])
  })

  test('event rules are labelled with the vocabulary word, toned by how bad they are', () => {
    const marks = eventMarkers(parked.events)
    expect(marks).toHaveLength(parked.events!.length)
    for (const m of marks) expect(m.label).not.toContain('_')
    expect(eventTone('event_fault')).toBe('err')
    expect(eventTone('event_reset')).toBe('warn')
    expect(eventTone('event_first_edge')).toBe('note')
    expect(eventMarkers(undefined)).toEqual([])
  })
})

describe('the live scope reading a probed pin', () => {
  test('a net that crosses half the supply becomes edges', () => {
    const lane = liveLane('LED', 0, [
      { t: 0, v: 0 }, { t: 0.1, v: 3.3 }, { t: 0.2, v: 3.3 }, { t: 0.3, v: 0 }, { t: 0.4, v: 3.3 },
    ])
    expect(lane.initial).toBe(false)
    expect(lane.edges).toEqual([{ t_s: 0.1, high: true }, { t_s: 0.3, high: false }, { t_s: 0.4, high: true }])
  })

  test('a 5 V pin is judged against 2.5 V, a held level has no edges', () => {
    const five = liveLane('P', 1, [{ t: 0, v: 5 }, { t: 1, v: 2 }, { t: 2, v: 5 }])
    expect(five.initial).toBe(true)
    expect(five.edges.map(e => e.high)).toEqual([false, true])
    expect(liveLane('P', 0, [{ t: 0, v: 3.3 }, { t: 1, v: 3.3 }]).edges).toEqual([])
    expect(liveLane('P', 0, []).edges).toEqual([])
  })

  test('long names are cut for the gutter', () => {
    expect(liveLane('A_VERY_LONG_NET_NAME_INDEED', 0, []).label.length).toBeLessThanOrEqual(12)
  })

  test('the digital nets are the ones MCU pins and the serial line drive', () => {
    const nets = digitalNets(blinky)
    const named = blinky.mcus[0].pins.filter(p => p.net && p.state !== 'pin_input').map(p => p.net!)
    for (const n of named) expect(nets.has(n)).toBe(true)
    expect(nets.has(blinky.mcus[0].pins.find(p => p.state === 'pin_input')!.net!)).toBe(false)
    expect(digitalNets(null).size).toBe(0)
  })
})

describe('the report view carries the bench', () => {
  const withBench = (bench?: BenchReport | null, extra: object = {}): WebReport => ({
    ...watchy,
    cosim: { ran: true, seconds_simulated: 2, uart_output: '', findings: [], analog_valid: true, bench, ...extra },
  })

  test('a section with a bench gets a strip; one without keeps the table fallback', () => {
    const c = reportView(withBench(blinky)).cosim!
    expect(c.bench).not.toBeNull()
    expect(c.benchReport).toBe(blinky)
    const none = reportView(withBench(null, { gpio_nets: [{ name: 'A', volts: 3.3, driven: true }] })).cosim!
    expect(none.bench).toBeNull()
    expect(none.gpio).toHaveLength(1)
  })

  test('external_backend is passed through', () => {
    const r = reportView(withBench(undefined, { ran: false, external_backend: true })).cosim!
    expect(r.externalBackend).toBe(true)
    expect(reportView(withBench(blinky)).cosim!.externalBackend).toBe(false)
  })
})

describe('auto-seeded probes', () => {
  const nets = ['GND', 'LED_PC13', 'UART_TX', 'BUZZER_EN', 'I2C_SCL', 'I2C_SDA', 'MISO', 'NRST', 'HEATER_EN']
  const report = (bench: BenchReport): WebReport => ({
    ...watchy,
    nets,
    model_coverage: undefined,
    cosim: { ran: true, seconds_simulated: 2, uart_output: '', findings: [], analog_valid: true, bench },
  })

  test('a toggling pin is seeded with the reason and its summary', () => {
    const seeds = benchSeeds(report(blinky), blinky)
    const led = seeds.find(s => s.net === 'LED_PC13')
    expect(led).toBeDefined()
    expect(led!.why).toContain('PC13')
    expect(led!.why).toContain('5.0 Hz')
    // Not a raw id.
    expect(led!.why).not.toContain('probe_seed')
  })

  test('the serial transmit line is seeded', () => {
    const seeds = benchSeeds(report(blinky), blinky)
    expect(seeds.some(s => s.net === 'UART_TX')).toBe(true)
  })

  test('a static or input pin is not seeded', () => {
    const nets2 = benchSeeds(report(blinky), blinky).map(s => s.net)
    expect(nets2).not.toContain('BUZZER_EN')
  })

  test('the bus lines of a bus that carried traffic are seeded; an idle bus adds none', () => {
    const seeds = benchSeeds(report(thermostat), thermostat).map(s => s.net)
    expect(seeds).toContain('I2C_SCL')
    expect(seeds).toContain('I2C_SDA')
    expect(seeds).not.toContain('MISO')
    const idle = benchSeeds(report({ ...thermostat, bus_log: [] }), { ...thermostat, bus_log: [] })
    expect(idle.some(s => s.net === 'MISO')).toBe(false)
  })

  test('the reset line is seeded only for a board that restarted', () => {
    expect(benchSeeds(report(blinky), blinky).some(s => s.net === 'NRST')).toBe(false)
    const restarted = benchSeeds(report(parked), parked)
    expect(restarted.some(s => s.net === 'NRST')).toBe(true)
  })

  test('an indicator is reached through its series resistor, and rails never are', () => {
    const r = {
      ...watchy,
      nets: ['GND', 'LED_A', 'MCU_PIN'],
      supplies: [],
      model_coverage: {
        ...watchy.model_coverage!,
        components: [
          { reference: 'D1', value: 'LED', lib_id: 'Device:LED', footprint: 'LED_0603', pins: [{ number: '1', function: 'K', kind: 'p', net: 'GND' }, { number: '2', function: 'A', kind: 'p', net: 'LED_A' }] },
          { reference: 'R1', value: '330', lib_id: 'Device:R', footprint: 'R_0603', pins: [{ number: '1', function: '', kind: 'p', net: 'LED_A' }, { number: '2', function: '', kind: 'p', net: 'MCU_PIN' }] },
          { reference: 'R2', value: '10k', lib_id: 'Device:R', footprint: 'R_0603', pins: [{ number: '1', function: '', kind: 'p', net: 'GND' }, { number: '2', function: '', kind: 'p', net: 'OTHER' }] },
        ],
      },
    } as unknown as WebReport
    const leds = indicatorNets(r)
    expect(leds.has('LED_A')).toBe(true)
    expect(leds.has('MCU_PIN')).toBe(true)
    expect(leds.has('GND')).toBe(false)
    expect(leds.has('OTHER')).toBe(false)
  })

  test('an indicator net gets the indicator reason, the others the toggling one', () => {
    const b: BenchReport = {
      ...blinky,
      mcus: [{
        ...blinky.mcus[0],
        pins: [
          row({ pin: 'PA1', net: 'MOTOR', state: 'pin_toggling', summary: 'toggling 1.0 kHz, 50% duty' }),
          row({ pin: 'PC13', net: 'LED_PC13', state: 'pin_toggling', summary: 'toggling 5.0 Hz, 50% duty' }),
        ],
        uarts: [],
      }],
    }
    const seeds = benchSeeds(report(b), b)
    expect(seeds[0].net).toBe('LED_PC13')
    expect(seeds[0].why).not.toBe(seeds[1].why.replace('PA1', 'PC13').replace('1.0 kHz', '5.0 Hz'))
  })

  test('seedFrom puts the bench probes in, capped at six, checks first', () => {
    const b: BenchReport = {
      ...blinky,
      mcus: [{
        ...blinky.mcus[0],
        pins: Array.from({ length: 8 }, (_, i) => row({ pin: `PA${i}`, net: `N${i}`, state: 'pin_toggling', summary: 'toggling' })),
        uarts: [],
      }],
    }
    const r = { ...report(b), nets: Array.from({ length: 8 }, (_, i) => `N${i}`) }
    const seed = seedFrom(r, 'duration_ms = 10\n[[assert]]\nkind = "voltage"\nnet = "N7"\n')
    expect(seed.probes.length).toBeLessThanOrEqual(6)
    expect(seed.probes[0].net).toBe('N7')
    expect(seed.probes[0].why).toBe('a check names it')
    expect(seed.probes.slice(1).every(p => p.why.length > 0)).toBe(true)
    expect(new Set(seed.probes.map(p => p.net)).size).toBe(seed.probes.length)
  })

  test('a run with no bench seeds exactly what it did before', () => {
    const seed = seedFrom({ ...watchy, cosim: { ran: true, seconds_simulated: 1, uart_output: '', findings: [], analog_valid: true, gpio_nets: [{ name: 'X', volts: 3, driven: true, toggles: 4 }] } }, null)
    expect(seed.probes).toEqual([{ net: 'X', why: 'toggled 4x in the analysis' }])
  })
})

describe('real engine captures', () => {
  // Captured from live Renode runs by crates/hauksbee-engine/tests/bench_observability.rs
  // (HAUKSBEE_DUMP_BENCH=<dir>): they prove the view-model accepts what the engine really emits.
  const real = ['bench-real-stm32-blinky.json', 'bench-real-i2c-thermostat.json', 'bench-real-rp2040-blink-uart.json']

  test.each(real)('%s makes a strip, lanes and decoded items', name => {
    const b = fixture(name)
    const v = benchView(b)
    expect(v.mcus.length).toBeGreaterThan(0)
    const { lanes } = benchLanes(b, null)
    expect(lanes.length).toBeGreaterThan(0)
    expect(decodedItems(b.bus_log).length).toBe(b.bus_log?.length ?? 0)
  })

  test('the blinky capture has PC13 toggling on the net it drives', () => {
    const pc13 = fixture('bench-real-stm32-blinky.json').mcus[0]?.pins.find(p => p.pin === 'PC13')
    expect(pc13?.state).toBe('pin_toggling')
    expect(pc13?.net).toBeTruthy()
  })

  test('the thermostat capture lists acknowledged I2C transactions to 0x48', () => {
    const i2c = (fixture('bench-real-i2c-thermostat.json').bus_log ?? []).filter(t => t.bus === 'bus_i2c')
    expect(i2c.length).toBeGreaterThan(1)
    expect(i2c.every(t => t.addr === 0x48 && t.acked)).toBe(true)
  })

  test('the rp2040 capture carries its first serial line', () => {
    const u = fixture('bench-real-rp2040-blink-uart.json').mcus[0].uarts?.[0]
    expect(u?.first_line).toBe('hauksbee rp2040: main reached')
  })
})
