import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { nextKind, suggestions } from '../src/components/report/NextStep'
import { reportView } from '../src/lib/report-view'
import type { WebReport } from '../src/types/report'

const report = (cosim: Partial<NonNullable<WebReport['cosim']>> | null, supplies: WebReport['supplies'] = []) => ({
  supplies,
  cosim: cosim && { ran: true, seconds_simulated: 5, uart_output: '', findings: [], analog_valid: true, ...cosim },
}) as unknown as WebReport

describe('next-step suggestions come only from what the co-sim observed', () => {
  test('nothing without a co-sim', () => {
    expect(suggestions(report(null))).toEqual([])
    expect(suggestions(report({ ran: false, uart_output: 'x' }))).toEqual([])
  })
  test('the first printed line becomes a UART check', () => {
    const [s] = suggestions(report({ uart_output: '\nhauksbee-demo v1\r\nok' }))
    expect(s.request).toEqual({ type: 'check', kind: 'uart', contains: 'hauksbee-demo v1' })
  })
  test('a blink rate is toggles over two edges per period, with a run length for five periods', () => {
    const [s] = suggestions(report({ gpio_nets: [{ name: 'D13', volts: 0, driven: true, toggles: 50 }] }))
    expect(s.request).toMatchObject({ kind: 'toggle', net: 'D13', freq_hz: '5', run_ms: '1000' })
  })
  test('too few edges to time a rate offers the plain observation, with the co-sim window as run length', () => {
    const [s] = suggestions(report({ seconds_simulated: 0.2, gpio_nets: [{ name: 'D13', volts: 0, driven: true, toggles: 1 }] }))
    expect(s.text).toBe('D13 toggled once in 0.2 s')
    expect(s.request).toMatchObject({ kind: 'toggle', net: 'D13', min_toggles: '1', run_ms: '200' })
  })
  test('a rate the 1 ms grid cannot time is not offered as a rate, and an idle net as nothing', () => {
    const [s] = suggestions(report({ gpio_nets: [{ name: 'A', volts: 0, driven: true, toggles: 5000 }] }))
    expect(s.request).not.toHaveProperty('freq_hz')
    expect(suggestions(report({ gpio_nets: [{ name: 'A', volts: 0, driven: false, toggles: 0 }, { name: 'B', volts: 0, driven: false }] }))).toEqual([])
  })
  test('a held rail suggests nominal less 5% and says so; a sagging rail is not offered', () => {
    const supplies = [{ net: '+5V', volts: 5 }]
    const [s] = suggestions(report({ gpio_nets: [{ name: '+5V', volts: 4.996, driven: false, toggles: 0 }] }, supplies))
    expect(s.request).toMatchObject({ kind: 'voltage', net: '+5V', min: '4.75' })
    expect(s.note).toContain('less 5%')
    expect(suggestions(report({ gpio_nets: [{ name: '+5V', volts: 4.2, driven: false }] }, supplies))).toEqual([])
  })
})

describe('the one next step follows the board state', () => {
  const fixture = (name: string): WebReport =>
    JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport
  const kind = (r: WebReport, firmwareName: string | null = null, checks: { passed: number; failed: number; invalid: number } | null = null) =>
    nextKind(r, reportView(r), { firmwareName, checks })

  test('an unrouted board is routed first, never offered firmware', () => {
    expect(kind(fixture('analyze-unrouted.json'))).toBe('route')
    expect(kind(fixture('analyze-unrouted.json'), 'blink.elf')).toBe('route')
  })
  test('a partly judged board is given the missing model', () => {
    expect(kind(fixture('analyze-partly-judged.json'))).toBe('model')
  })
  test('a checks run leads to CI, whatever the board', () => {
    expect(kind(fixture('analyze-unrouted.json'), null, { passed: 1, failed: 0, invalid: 0 })).toBe('ci')
  })
  test('a clean pass is offered checks and firmware', () => {
    expect(kind(fixture('analyze-conditional-pass.json'))).toBe('checks')
  })
})
