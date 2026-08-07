import { expect, test } from 'bun:test'
import { assumptionsForEvidence, describeModelSource, summarizeEvidence } from '../src/lib/evidence'

test('evidence summary counts each derived status and every caveated assertion', () => {
  const summary = summarizeEvidence([
    { assertion: 'VBUS', status: 'clean' },
    { assertion: '3V3', status: 'qualified', assumptions: ['reader:a'] },
    { assertion: 'RESET', status: 'undermined', assumptions: ['open-part:U1'] },
  ])

  expect(summary).toEqual({ clean: 1, qualified: 1, undermined: 1, caveated: 2 })
})

test('frontend renders canonical model source and unknown accuracy without guessing a range', () => {
  expect(describeModelSource({
    reference: 'U2',
    model_id: 'xc6206',
    confidence: 'exact',
    source: {
      tier: 'datasheet-derived',
      layer: 'user-dir',
      origin: 'xc6206.toml',
      validation: 'physical-bounds-only',
      uncertainty: [{
        status: 'unknown',
        parameter: 'U2.model',
        reason: 'the source publishes no validated error interval',
      }],
    },
  })).toContain('datasheet-derived · physical-bounds-only · uncertainty unknown')
})

test('frontend never presents a typical-only range as validated accuracy', () => {
  expect(describeModelSource({
    reference: 'U1',
    model_id: 'switch',
    confidence: 'exact',
    source: {
      tier: 'datasheet-derived',
      layer: 'user-dir',
      origin: 'switch.toml',
      validation: 'physical-bounds-only',
      uncertainty: [{
        status: 'interval',
        parameter: 'U1.ilim',
        low: 0.75,
        high: 1.0,
        unit: 'A',
        kind: 'typical-range',
        basis: 'datasheet min/typ row with no maximum',
      }],
    },
  })).toContain('non-guaranteed typical/estimated range')
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
