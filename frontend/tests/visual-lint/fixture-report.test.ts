import { expect, test } from 'bun:test'
import { withBoardIdentity } from './fixture-report'

test('the fixture keeps each uploaded board as a distinct report/session', () => {
  const captured = { file_name: 'watchy.kicad_pcb', board_name: '', total: 50 }

  const watchy = withBoardIdentity(captured, 'watchy.kicad_pcb')
  const blinky = withBoardIdentity(captured, 'blinky.kicad_pcb')

  expect(watchy.file_name).toBe('watchy.kicad_pcb')
  expect(blinky.file_name).toBe('blinky.kicad_pcb')
  expect(blinky).not.toEqual(watchy)
  expect(captured.file_name).toBe('watchy.kicad_pcb')
})
