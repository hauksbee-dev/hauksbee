// A run that could not be judged gathers what blocks it into one list grouped
// by cause behind one entry chip, each row of checks carries its own result,
// and the builder can say what the fixes write: a bench or USB source, and a
// part left out as not fitted.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { blockingCauses, causeGroups, openPartCount, OpenPartsSheet, withoutIds } from '../src/components/checks/Remedies'
import type { RemedyActions } from '../src/components/checks/Remedies'
import { RunResults } from '../src/components/checks/RunResults'
import { VerdictBand } from '../src/components/checks/VerdictBand'
import { AssertionEvidence, parseMeasures } from '../src/components/checks/pieces'
import { identityScaffold, tomlIssues } from '../src/components/WritePart'
import { buildToml, scaffoldRows, supplyToml } from '../src/lib/check-spec'
import { tomlToBuilder } from '../src/lib/spec-model'
import type { EvidenceAssumption, Remedy, RunResponse } from '../src/types/report'

const partScope = (id: string) => ({ type: 'subjects', value: [{ kind: 'part', id }] })

const assumption = (
  id: string, kind: string, remedy: Remedy | undefined, replacement = 'do the thing', scope: EvidenceAssumption['scope'] = { type: 'board' },
): EvidenceAssumption => ({
  id, kind, source: 's', scope, statement: `${id} is assumed`, because: '', consequence: '',
  replacement, remedy,
})

const invalidRun = (assumptions: EvidenceAssumption[], undermined: string[]): RunResponse => ({
  ok: true,
  passed: false,
  verdict_reason: 'Q1 has no model.',
  refusal: undefined,
  assumptions,
  export_id: 'abc-1',
  results: [{
    label: 'no faults', kind: 'no_faults', passed: false, invalid: true, detail: '',
    evidence: { assertion: 'no_faults', status: 'undermined', assumptions: undermined },
  }],
} as unknown as RunResponse)

const actions: RemedyActions = {
  addSupply: () => {}, markNotFitted: () => {}, openModel: () => {}, applied: () => false,
}

const registry = [
  assumption('open-part:Q1', 'open_part', { type: 'add_model', reference: 'Q1' }),
  assumption('open-part:C2', 'open_part', undefined, 'do the thing', partScope('C2')),
  assumption('inferred-pin-role:D2/1', 'inferred_pin_role', undefined, 'do the thing', partScope('D2')),
  assumption('unpowered:/VIN', 'unpowered_rail', { type: 'add_supply', net: '/VIN', volts_hint: 7 }),
  assumption('note', 'note', { type: 'other' }),
]

describe('what blocks judgement', () => {
  test('is grouped by cause, with every part, pin and net it touches', () => {
    const groups = causeGroups(registry)
    expect(groups.map(g => g.kind)).toEqual(['open_part', 'inferred_pin_role', 'unpowered_rail', 'note'])
    expect(groups[0].parts).toEqual(['Q1', 'C2'])
    expect(groups[1].pins).toEqual(['D2/1'])
    expect(groups[2].nets).toEqual(['/VIN'])
    expect(openPartCount(groups)).toBe(2)
  })

  test('a part is named by its remedy or its scope, never by parsing the id', () => {
    const groups = causeGroups([
      assumption('open-part:ignored-name', 'open_part', { type: 'add_model', reference: 'U9' }),
      assumption('open-part:also-ignored', 'open_part', undefined, 'x', partScope('U7')),
      assumption('open-part:U1', 'open_part', undefined),
    ])
    expect(groups[0].parts).toEqual(['U9', 'U7'])
  })

  test('drops causes that name nothing to act on', () => {
    expect(blockingCauses(invalidRun(registry, [])).map(g => g.kind)).not.toContain('note')
  })

  test('never shows internal ids', () => {
    expect(withoutIds('open-part:C2 and inferred-pin-role:D2/1')).toBe('C2 and D2/1')
  })

  test('the sheet offers the five fixes for parts and the supply fix for nets', () => {
    const html = renderToStaticMarkup(<OpenPartsSheet groups={causeGroups(registry).slice(0, 3)} actions={actions} onClose={() => {}} />)
    for (const id of ['cause-draft', 'cause-write', 'cause-dnp', 'cause-ignore', 'cause-extend', 'cause-supply']) {
      expect(html).toContain(`data-testid="${id}"`)
    }
    expect(html).toContain('Add a supply on /VIN')
    expect(html).not.toContain('open-part:')
  })
})

describe('an unjudged run', () => {
  const run = invalidRun(registry, ['open-part:Q1'])
  const html = renderToStaticMarkup(
    <VerdictBand
      result={run} runKey="k" stale={false} running={false} canRun onRun={() => {}} validationCount={0}
      reportSerious={0} onJump={() => {}} specFileName="spec.toml" specToml={'[[assert]]\nkind = "no_faults"\n'}
      rawMode={false} preLayoutNotice={null} remedyActions={actions} exportMenu={<span data-testid="export-slot" />}
    />,
  )

  test('says once that it was not judged, and the one thing to do, with the backend count', () => {
    expect(html).toContain('data-testid="run-tag"')
    expect(html).toContain('Not judged yet: 1 check waits on models for 2 parts.')
    expect(html).toContain('data-testid="open-parts-entry"')
    expect(html).toContain('Model the 2 open parts')
  })

  test('sits Run and the export menu beside the verdict', () => {
    expect(html).toContain('data-testid="run-checks"')
    expect(html).toContain('data-testid="export-slot"')
  })

  test('keeps the engine sentence one click away, in Details', () => {
    expect(html).toContain('data-testid="run-details-toggle"')
    const details = renderToStaticMarkup(<RunResults result={run} overall="invalid" stale={false} rawMode={false} />)
    expect(details).toMatch(/<details[^>]*data-testid="explain-verdict"/)
    expect(details).toContain('Q1 has no model.')
  })

  test('the first-run whole-board check explains why it waits, on its row', () => {
    const row = renderToStaticMarkup(
      <AssertionEvidence result={run.results![0]} assumptions={registry} inventory={[]} repeats={new Set()} stale={false} />,
    )
    expect(row).toContain('data-testid="not-judged"')
    expect(row).toContain('2 parts have none yet')
    // The parts are named, each one a link to the board, with the fix beside them.
    expect(row).toContain('data-testid="not-judged-parts"')
    expect(row).toContain('Q1')
    expect(row).toContain('data-testid="not-judged-open"')
    // The engine's verdict sentence is not shown raw on the row.
    expect(row).not.toContain('INVALID evidence')
  })
})

describe('measured against its limits', () => {
  test('reads the runner\'s detail shapes', () => {
    expect(parseMeasures('GND: max=0.000V (<= 0.75V)')).toEqual([{ values: [0], hi: 0.75, unit: 'V' }])
    expect(parseMeasures('+3V3 window: min=3.100V < required 3.2V, max=3.400V (<= 3.6V) [min=3.100V max=3.400V]'))
      .toEqual([{ values: [3.1, 3.4], lo: 3.2, hi: 3.6, unit: 'V' }])
    expect(parseMeasures('LED: 4 toggles (need >= 2)')).toEqual([{ values: [4], lo: 2, unit: 'toggles' }])
    expect(parseMeasures('boot 1.20ms (<= 2ms)')).toEqual([{ values: [1.2], hi: 2, unit: 'ms' }])
    expect(parseMeasures('no stress faults raised')).toEqual([])
  })
})

describe('writing a model', () => {
  test('starts on an identity-only scaffold of the part\'s own kind', () => {
    const toml = identityScaffold({ reference: 'U1', value: 'XLOGIC9999' }, 'digital')
    expect(toml).toContain('id = "u1_xlogic9999"')
    expect(toml).toContain("value_re = '^XLOGIC9999$'")
    expect(toml).toContain('identity_only = true')
    expect(toml).toContain('kind = "digital"')
  })

  test('never claims a kind it was not told: no default "digital"', () => {
    const toml = identityScaffold({ reference: 'Q1', value: 'BS250' })
    expect(toml).not.toContain('kind = "digital"')
    expect(toml).toContain('kind = "choose_kind"')
    expect(identityScaffold({ reference: 'Q1', value: 'BS250' }, 'pmos')).toContain('kind = "pmos"')
  })

  test('places validator complaints on their lines, with a fix where one is honest', () => {
    const toml = '[[models]]\nid = "x"\nkind = "digital"\n[models.match]\nvalue_re = "^X$"\n[models.params]\nidentity_only = true\nwarning = "w"\n'
    const [issue] = tomlIssues(toml, "entry 'x': identity-only model requires non-empty params.unlocked_by so reports state both")
    expect(issue.line).toBe(6)
    expect(issue.fix?.apply(toml)).toContain('[models.params]\nunlocked_by = ')
    const parse = tomlIssues(toml, 'this is not valid model TOML: TOML parse error at line 3, column 1')
    expect(parse[0].line).toBe(3)
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
