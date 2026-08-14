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
  expect(canReanalyzeSavedSession({ firmwareName: 'boot.hex', schematicName: null, report: report() })).toBe(false)
  expect(canReanalyzeSavedSession({ firmwareName: null, schematicName: 'board.kicad_sch', report: report() })).toBe(false)
  expect(canReanalyzeSavedSession({ firmwareName: null, schematicName: null, report: report() })).toBe(true)
})

test('a legacy schematic session without schematicName still restores report-only', () => {
  const legacy = report('a'.repeat(64))
  legacy.inventory!.push({
    path: 'board.kicad_sch', kind: 'ki_cad_schematic', role: 'schematic', sha256: 'b'.repeat(64),
  })
  expect(canReanalyzeSavedSession({ firmwareName: null, schematicName: undefined, report: legacy })).toBe(false)
})

test('a primary schematic is not mistaken for a missing companion', () => {
  const primary = report('a'.repeat(64))
  primary.file_name = 'board.kicad_sch'
  primary.inventory = [{
    path: 'board.kicad_sch', kind: 'ki_cad_schematic', role: 'schematic', sha256: 'a'.repeat(64),
  }]
  expect(canReanalyzeSavedSession({ firmwareName: null, schematicName: null, report: primary })).toBe(true)
  expect(expectedBoardSha256(primary, primary.file_name)).toBe('a'.repeat(64))
})

test('an authenticated netlist can resume from the exact retained bytes', () => {
  const netlist = report('a'.repeat(64))
  netlist.file_name = 'board.d356'
  netlist.inventory = [{
    path: 'board.d356', kind: 'ipc_d_356', role: 'netlist', sha256: 'c'.repeat(64),
  }]
  expect(expectedBoardSha256(netlist, netlist.file_name)).toBe('c'.repeat(64))
})

test('resume fails closed when no filename selects between two authenticated inputs', () => {
  const ambiguous = report('a'.repeat(64))
  ambiguous.inventory!.push({
    path: 'companion.kicad_sch', kind: 'ki_cad_schematic', role: 'schematic', sha256: 'b'.repeat(64),
  })
  expect(expectedBoardSha256(ambiguous, 'renamed.kicad_pcb')).toBeNull()
})

test('a saved session authenticates re-fetched board bytes with the primary input hash', () => {
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
