import { expect, test } from 'bun:test'
import { summarizeEvidence } from '../src/lib/evidence'

test('evidence summary counts each derived status and every caveated assertion', () => {
  const summary = summarizeEvidence([
    { assertion: 'VBUS', status: 'clean' },
    { assertion: '3V3', status: 'qualified', assumptions: ['reader:a'] },
    { assertion: 'RESET', status: 'undermined', assumptions: ['open-part:U1'] },
  ])

  expect(summary).toEqual({ clean: 1, qualified: 1, undermined: 1, caveated: 2 })
})
