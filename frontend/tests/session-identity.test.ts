import { expect, test } from 'bun:test'
import type { WebReport } from '../src/types/report'

Object.assign(globalThis, {
  __APP_VERSION__: '0.1.0',
  __RELEASE_COMMIT__: '0'.repeat(40),
  __RELEASE_TAG__: 'v0.1.0',
})

const { checksStorageKey } = await import('../src/components/ChecksView')
const { boardBytesMatchExpected, canReanalyzeSavedSession, expectedBoardSha256 } = await import('../src/hooks/useSessions')
const { sessionIdFor } = await import('../src/lib/session-store')
const { isModelCheckCurrent } = await import('../src/components/WritePart')

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
  expect(canReanalyzeSavedSession({ firmwareName: 'boot.hex', schematicName: null })).toBe(false)
  expect(canReanalyzeSavedSession({ firmwareName: null, schematicName: 'board.kicad_sch' })).toBe(false)
  expect(canReanalyzeSavedSession({ firmwareName: null, schematicName: null })).toBe(true)
})

test('a saved session authenticates re-fetched board bytes with the layout hash', () => {
  expect(expectedBoardSha256(report('A'.repeat(64)), 'board.kicad_pcb')).toBe('a'.repeat(64))
  expect(expectedBoardSha256(report(), 'board.kicad_pcb')).toBeNull()
})

test('same-name replacement bytes cannot masquerade as the saved board revision', async () => {
  const original = new TextEncoder().encode('(kicad_pcb (version 1))')
  const replacement = new TextEncoder().encode('(kicad_pcb (version 2))')
  const expected = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', original)))
    .map(byte => byte.toString(16).padStart(2, '0'))
    .join('')
  expect(await boardBytesMatchExpected(original, expected)).toBe(true)
  expect(await boardBytesMatchExpected(replacement, expected)).toBe(false)
})

test('model validation can only enable save for the exact checked text and format', () => {
  const checked = { phase: 'ok' as const, summary: 'valid', body: 'old', format: 'toml' as const }
  expect(isModelCheckCurrent(checked, 'old', 'toml')).toBe(true)
  expect(isModelCheckCurrent(checked, 'new edit', 'toml')).toBe(false)
  expect(isModelCheckCurrent(checked, 'old', 'spice')).toBe(false)
})
