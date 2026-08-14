import { expect, test } from 'bun:test'
import { checksStorageKey } from '../src/components/ChecksView'
import { canReanalyzeSavedSession } from '../src/hooks/useSessions'
import { sessionIdFor } from '../src/lib/session-store'
import { isModelCheckCurrent } from '../src/components/WritePart'
import type { WebReport } from '../src/types/report'

function report(hash?: string): WebReport {
  return {
    ok: true,
    board_name: 'board',
    file_name: 'board.kicad_pcb',
    num_components: 2,
    num_nets: 3,
    headline: 'done',
    serious: 0,
    total: 0,
    sections: [],
    components: [],
    inventory: hash
      ? [{ path: 'board.kicad_pcb', kind: 'ki_cad_pcb', role: 'layout', sha256: hash }]
      : undefined,
  }
}

test('session and checks identity follow authenticated input bytes', () => {
  const first = report('a'.repeat(64))
  const revised = report('b'.repeat(64))
  expect(sessionIdFor(first)).not.toBe(sessionIdFor(revised))
  expect(checksStorageKey(first)).not.toBe(checksStorageKey(revised))
})

test('legacy reports without inventory keep the old stable identity', () => {
  expect(sessionIdFor(report())).toBe('board.kicad_pcb:2:3')
})

test('a firmware-backed saved report never masquerades as a fresh board-only run', () => {
  expect(canReanalyzeSavedSession({ firmwareName: 'boot.hex' })).toBe(false)
  expect(canReanalyzeSavedSession({ firmwareName: null })).toBe(true)
})

test('model validation can only enable save for the exact checked text and format', () => {
  const checked = { phase: 'ok' as const, summary: 'valid', body: 'old', format: 'toml' as const }
  expect(isModelCheckCurrent(checked, 'old', 'toml')).toBe(true)
  expect(isModelCheckCurrent(checked, 'new edit', 'toml')).toBe(false)
  expect(isModelCheckCurrent(checked, 'old', 'spice')).toBe(false)
})
