import { expect, test } from 'bun:test'
import { assumptionsForEvidence, summarizeEvidence } from '../src/lib/evidence'

test('evidence summary counts each derived status and every caveated assertion', () => {
  const summary = summarizeEvidence([
    { assertion: 'VBUS', status: 'clean' },
    { assertion: '3V3', status: 'qualified', assumptions: ['reader:a'] },
    { assertion: 'RESET', status: 'undermined', assumptions: ['open-part:U1'] },
  ])

  expect(summary).toEqual({ clean: 1, qualified: 1, undermined: 1, caveated: 2 })
})

test('CI assertion evidence resolves canonical assumption records without inventing prose', () => {
  const map = {
    assertion: 'RAIL stays up',
    status: 'qualified' as const,
    assumptions: ['reduced-fidelity:RAIL', 'missing:id'],
  }
  const registry = [{
    id: 'reduced-fidelity:RAIL',
    kind: 'reduced_fidelity',
    source: 'check',
    scope: { type: 'nets' },
    statement: 'Net RAIL is held by an ideal source.',
    because: 'Nothing on the board sets its voltage in this run: a stimulus does.',
    consequence: 'Passing vouches for nothing about the board.',
    replacement: 'Model the real supply path and re-run.',
  }]

  expect(assumptionsForEvidence(map, registry)).toEqual(registry)
})
