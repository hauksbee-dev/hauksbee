// A large report arrives with tables and shared values (the engine's
// web_transport module). Putting them back must give exactly the document the
// engine would have sent whole, and a small report must pass through untouched.

import { describe, expect, test } from 'bun:test'
import { restoreRepeats } from '../src/lib/intern'
import { describeModelGroup, modelProvenanceLines } from '../src/lib/report-view'
import type { EvidenceMap, ModelGroup } from '../src/types/report'

const source = {
  tier: 'estimated-fallback',
  layer: 'engine-fallback',
  origin: 'engine-fallback',
  validation: 'unvalidated',
  uncertainty: [{ status: 'unknown' as const, parameter: 'r_fallback.model', reason: 'no validated error interval' }],
}

describe('restoreRepeats', () => {
  test('a report without tables or shared values is returned as it came', () => {
    const report = { ok: true, board_name: 'b', components: [{ reference: 'R1', x: 1, y: 2 }] }
    const copy = structuredClone(report)
    expect(restoreRepeats(report)).toEqual(copy)
  })

  test('references are replaced by the value they name, including nested ones', () => {
    const wire = {
      ok: true,
      rows: [{ $ref: 1 }, { $ref: 1 }, { reference: 'R9', note: { $ref: 0 } }],
      $defs: [
        'Reference, value, pins and board location were recovered from the input.',
        { reference: 'R1', note: { $ref: 0 } },
      ],
    }
    const restored = restoreRepeats(wire) as unknown as { rows: Array<{ reference: string; note: string }>; $defs?: unknown }
    const note = 'Reference, value, pins and board location were recovered from the input.'
    expect(restored.rows).toEqual([
      { reference: 'R1', note },
      { reference: 'R1', note },
      { reference: 'R9', note },
    ])
    expect(restored.$defs).toBeUndefined()
  })

  test('a table becomes its records, with references inside its cells resolved', () => {
    const wire = {
      components: {
        $table: ['reference', 'source', 'x'],
        rows: [['U1', { $ref: 0 }, 1], ['U2', { $ref: 0 }, 2], ['U3', { $ref: 0 }, 3]],
      },
      $defs: [source],
    }
    const restored = restoreRepeats(wire) as unknown as { components: Array<Record<string, unknown>> }
    expect(restored.components).toEqual([
      { reference: 'U1', source, x: 1 },
      { reference: 'U2', source, x: 2 },
      { reference: 'U3', source, x: 3 },
    ])
  })

  test('a table nested inside a table is restored at both levels', () => {
    const wire = {
      parts: {
        $table: ['reference', 'pins'],
        rows: [
          ['U1', { $table: ['number', 'net'], rows: [['1', 'GND'], ['2', 'VCC']] }],
          ['U2', { $table: ['number', 'net'], rows: [['1', 'GND'], ['2', 'VCC']] }],
        ],
      },
    }
    const restored = restoreRepeats(wire) as unknown as { parts: Array<{ pins: unknown[] }> }
    expect(restored.parts[1].pins).toEqual([{ number: '1', net: 'GND' }, { number: '2', net: 'VCC' }])
  })

  test('a field stated once under const is on every record', () => {
    const wire = {
      objects: { $table: ['id', 'x'], rows: [['U1', 1], ['U2', 2], ['U3', 3]], const: { status: 'recovered', nets: ['GND'] } },
    }
    const restored = restoreRepeats(wire) as unknown as { objects: Array<Record<string, unknown>> }
    expect(restored.objects).toEqual([
      { id: 'U1', x: 1, status: 'recovered', nets: ['GND'] },
      { id: 'U2', x: 2, status: 'recovered', nets: ['GND'] },
      { id: 'U3', x: 3, status: 'recovered', nets: ['GND'] },
    ])
  })

  test('an object that only looks like a table or reference is left alone', () => {
    const odd = { $table: ['a'], rows: [['x', 'y']], extra: 1, ref: { $ref: 'nope' }, out: { $ref: 5 } }
    expect(restoreRepeats({ odd, $defs: [] }).odd).toEqual(odd)
  })
})

describe('evidence groups', () => {
  const group: ModelGroup = {
    model_id: 'r_fallback',
    layer: 'engine-fallback',
    confidence: 'guessed',
    source,
    references: ['R1', 'R2', 'R3', 'R4', 'R5', 'R6', 'R7', 'R8'],
  }

  test('a group is one line naming the first parts and the count of the rest', () => {
    const line = describeModelGroup(group)
    expect(line).toContain('R1, R2, R3, R4, R5, R6 and 2 more r_fallback')
    expect(line).toContain('(8 parts)')
    expect(line).toContain('uncertainty unknown')
  })

  test('a map reports its models whichever shape it arrived in', () => {
    const rows: EvidenceMap = {
      assertion: 'a', status: 'clean',
      models: [{ reference: 'U1', model_id: 'x', confidence: 'exact', source }],
    }
    expect(modelProvenanceLines(rows)).toHaveLength(1)
    expect(modelProvenanceLines({ assertion: 'a', status: 'clean', model_groups: [group] })).toHaveLength(1)
    expect(modelProvenanceLines(undefined)).toEqual([])
  })
})
