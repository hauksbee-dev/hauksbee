// A run that could not be judged names its fixes as buttons, and the builder
// can say what those fixes write: a bench or USB source, and a part left out
// as not fitted.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { blockingRemedies } from '../src/components/checks/Remedies'
import type { RemedyActions } from '../src/components/checks/Remedies'
import { RunResults } from '../src/components/checks/RunResults'
import { buildToml, scaffoldRows, supplyToml } from '../src/lib/check-spec'
import { tomlToBuilder } from '../src/lib/spec-model'
import type { EvidenceAssumption, Remedy, RunResponse } from '../src/types/report'

const assumption = (id: string, remedy: Remedy | undefined, replacement = 'do the thing'): EvidenceAssumption => ({
  id, kind: 'k', source: 's', scope: { type: 'board' }, statement: '', because: '', consequence: '',
  replacement, remedy,
})

const invalidRun = (assumptions: EvidenceAssumption[], undermined: string[]): RunResponse => ({
  ok: true,
  passed: false,
  verdict_reason: 'Q1 has no model.',
  refusal: undefined,
  assumptions,
  results: [{
    label: 'no faults', passed: false, invalid: true, detail: '',
    evidence: { assertion: 'no_faults', status: 'undermined', assumptions: undermined },
  }],
} as unknown as RunResponse)

const actions: RemedyActions = {
  addSupply: () => {}, markNotFitted: () => {}, openModel: () => {}, applied: () => false,
}

describe('blocking remedies', () => {
  const run = invalidRun([
    assumption('a1', { type: 'add_supply', net: '/VIN', volts_hint: 7 }),
    assumption('a2', { type: 'add_supply', net: '/VIN', volts_hint: 5 }),
    assumption('a3', { type: 'add_model', reference: 'Q1' }),
    assumption('a4', { type: 'other' }),
    assumption('a5', { type: 'mark_dnp', reference: 'R9' }),
    assumption('a6', undefined),
  ], ['a1', 'a2', 'a3', 'a4', 'a6'])

  test('are the typed ones behind the unjudged results, once per target', () => {
    expect(blockingRemedies(run).map(r => r.key)).toEqual(['add_supply:/VIN', 'add_model:Q1'])
  })

  test('with no result naming them, every typed remedy in the registry is offered', () => {
    const refused = { ...run, results: [] }
    expect(blockingRemedies(refused).map(r => r.key)).toEqual(['add_supply:/VIN', 'add_model:Q1', 'mark_dnp:R9'])
  })
})

describe('an unjudged run', () => {
  const run = invalidRun([
    assumption('a1', { type: 'add_supply', net: '/VIN', volts_hint: 7 }, 'Power /VIN from the adapter.'),
    assumption('a3', { type: 'add_model', reference: 'Q1' }),
    assumption('a5', { type: 'mark_dnp', reference: 'R9' }),
  ], ['a1', 'a3', 'a5'])
  const html = renderToStaticMarkup(
    <RunResults result={run} runKey="k" stale={false} rawMode={false} reportSerious={0} remedyActions={actions} />,
  )

  test('leads with what to fix, not the refusal contract', () => {
    expect(html).toContain('Not judged yet: 3 things to fix first')
    expect(html).not.toContain('Some checks could not be judged.')
  })

  test('shows one button per remedy', () => {
    expect(html).toContain('data-testid="remedy-add-supply"')
    expect(html).toContain('Add a supply on /VIN')
    expect(html).toContain('Write or draft a model for Q1')
    expect(html).toContain('R9 is not fitted: leave it out')
  })

  test('keeps the engine sentence one click away', () => {
    expect(html).toMatch(/<details[^>]*data-testid="explain-verdict"/)
    expect(html).toContain('Q1 has no model.')
  })

  test('an applied remedy stops asking for a click', () => {
    const done = renderToStaticMarkup(
      <RunResults
        result={run} runKey="k" stale rawMode={false} reportSerious={0}
        remedyActions={{ ...actions, applied: r => r.type === 'add_supply' }}
      />,
    )
    expect(done).toContain('Done: run the checks again')
  })

  test('with no typed remedy the older wording stands and the reason is open', () => {
    const plain = renderToStaticMarkup(
      <RunResults
        result={invalidRun([assumption('a4', { type: 'other' })], ['a4'])}
        runKey="k" stale={false} rawMode={false} reportSerious={0} remedyActions={actions}
      />,
    )
    expect(plain).toContain('Some checks could not be judged.')
    expect(plain).toMatch(/<details[^>]*open/)
  })
})

describe('supply sources in the builder', () => {
  test('a scaffold seeds bench and usb rows that write what the CLI writes', () => {
    const rows = scaffoldRows([
      { net: '/VBUS', kind: 'usb', volts: 5, usb: '5v1.5a', origin: 'rail' },
      { net: '/VIN', kind: 'bench', volts: 7, current_limit_a: 2, origin: 'regulator_input' },
    ])
    const toml = buildToml('n', '100', rows, [], [])
    expect(toml).toContain('net = "/VBUS"\nkind = "usb"\nusb = "5v1.5a"\n')
    expect(toml).toContain('net = "/VIN"\nkind = "bench"\nvolts = 7\ncurrent_limit_a = 2\n')
    expect(tomlToBuilder(toml)?.supplies).toEqual([
      // A USB port's voltage is its profile's, so the file does not carry one.
      { net: '/VBUS', volts: '', kind: 'usb', usb: '5v1.5a' },
      { net: '/VIN', volts: '7', kind: 'bench', limit: '2' },
    ])
  })

  test('an ideal row stays byte-for-byte what it was', () => {
    expect(supplyToml({ net: '+5V', volts: '5' })).toBe('\n[[supply]]\nnet = "+5V"\nkind = "ideal"\nvolts = 5\n')
  })

  test('parts left out as not fitted round-trip through no_fit', () => {
    const toml = buildToml('n', '100', [], [], [], [], [], ['R9', 'C4'])
    expect(toml).toContain('no_fit = ["R9", "C4"]\n')
    expect(tomlToBuilder(toml)?.noFit).toEqual(['R9', 'C4'])
    expect(tomlToBuilder('name = "n"\nno_fit = [1]\n')).toBeNull()
  })
})
