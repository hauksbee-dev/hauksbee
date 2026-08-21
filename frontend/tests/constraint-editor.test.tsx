import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { ConstraintModal } from '../src/components/ConstraintModal'
import { constraintIssues, emptyConstraint } from '../src/components/ConstraintEditor'
import { buildToml, tomlToBuilder } from '../src/components/ChecksView'

describe('shared constraint editor', () => {
  test('uses the Checks preflight for a voltage constraint', () => {
    const issues = constraintIssues(emptyConstraint('voltage'))
    expect(issues.map(issue => issue.field)).toEqual(['net', 'min'])
    const filled = emptyConstraint('voltage', '+3V3')
    filled.min = '3.1'
    expect(constraintIssues(filled)).toEqual([])
  })

  test('board-side modal exposes save and full Checks without navigation', () => {
    const html = renderToStaticMarkup(
      <ConstraintModal
        initial={{ kind: 'voltage', net: '+3V3' }}
        onSave={() => {}}
        onClose={() => {}}
        onOpenChecks={() => {}}
      />,
    )
    expect(html).toContain('data-testid="constraint-modal"')
    expect(html).toContain('Add to Checks spec')
    expect(html).toContain('Open full Checks')
    expect(html).toContain('value="+3V3"')
    expect(html).toContain('Nothing runs until you choose Run in Checks')
  })

  test('supports strict over-voltage rail windows with the shared polarity control', () => {
    const spike = emptyConstraint('rail_window', '+3V3')
    spike.rail_polarity = 'spike'
    expect(constraintIssues(spike).map(issue => issue.field)).toEqual(['spike_above'])
    spike.spike_above = '3.6'
    expect(constraintIssues(spike).map(issue => issue.field)).toEqual(['spike_for_max_ms'])
    spike.spike_for_max_ms = '5'
    expect(constraintIssues(spike)).toEqual([])
    const toml = buildToml('spike test', '100', [], [], [{ ...spike, id: 1 }])
    expect(toml).toContain('spike_above = 3.6')
    expect(toml).toContain('spike_for_max_ms = 5')
    expect(toml).not.toContain('dip_below')
    const roundTripped = tomlToBuilder(toml)?.checks[0]
    expect(roundTripped?.rail_polarity).toBe('spike')
    expect(roundTripped?.spike_above).toBe('3.6')
    expect(tomlToBuilder(toml.replace('spike_above = 3.6\n', ''))).toBeNull()
    expect(tomlToBuilder(toml.replace('spike_for_max_ms = 5\n', 'dip_below = 2.8\n'))).toBeNull()
    const html = renderToStaticMarkup(
      <ConstraintModal initial={{ ...spike }} onSave={() => {}} onClose={() => {}} onOpenChecks={() => {}} />,
    )
    expect(html).toContain('data-testid="rail-polarity"')
    expect(html).toContain('spike above')
  })

  test('highlights an incomplete recovery or settling pair at the missing deadline field', () => {
    const dip = emptyConstraint('rail_window', '+3V3')
    dip.dip_below = '3.0'
    dip.for_max_ms = '2'
    dip.recover_to = '3.2'
    expect(constraintIssues(dip)).toEqual([{
      field: 'recover_within_ms',
      message: 'within ms is empty (needed with recover to V)',
    }])

    const spike = emptyConstraint('rail_window', '+3V3')
    spike.rail_polarity = 'spike'
    spike.spike_above = '3.6'
    spike.spike_for_max_ms = '2'
    spike.settle_to = '3.4'
    expect(constraintIssues(spike)).toEqual([{
      field: 'settle_within_ms',
      message: 'within ms is empty (needed with settle to V)',
    }])
  })

  test('an older saved rail row without spike fields renders as a dip instead of crashing', () => {
    const legacy = {
      kind: 'rail_window', net: '+3V3', ref: '', min: '', max: '', after_ms: '',
      deadline_ms: '', contains: '', freq_hz: '', tolerance: '', min_toggles: '',
      amps: '', celsius: '', dip_below: '3.0', for_max_ms: '2', recover_to: '',
      recover_within_ms: '',
    } as unknown as ReturnType<typeof emptyConstraint>
    expect(constraintIssues(legacy)).toEqual([])
    const html = renderToStaticMarkup(
      <ConstraintModal initial={legacy} onSave={() => {}} onClose={() => {}} onOpenChecks={() => {}} />,
    )
    expect(html).toContain('value="dip" selected=""')
    expect(html).toContain('value="3.0"')
  })
})
