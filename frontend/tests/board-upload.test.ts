import { describe, expect, test } from 'bun:test'
import { buildBoardUpload, buildCheckUpload } from '../src/lib/board-upload'

describe('browser board upload contract', () => {
  test('threads the optional Eagle schematic through analysis and live forms', () => {
    const board = new File(['board'], 'design.brd')
    const firmware = new File(['firmware'], 'app.elf')
    const schematic = new File(['schematic'], 'design.sch')

    for (const form of [
      buildBoardUpload(board, firmware, schematic),
      buildBoardUpload(board, firmware, schematic),
    ]) {
      expect((form.get('board') as File).name).toBe('design.brd')
      expect((form.get('firmware') as File).name).toBe('app.elf')
      expect((form.get('schematic') as File).name).toBe('design.sch')
    }
  })

  test('does not invent absent companion inputs', () => {
    const form = buildBoardUpload(new File(['board'], 'design.brd'), null, null)
    expect(form.has('firmware')).toBeFalse()
    expect(form.has('schematic')).toBeFalse()
  })

  test('uses the same schematic in the Checks multipart request', () => {
    const form = buildCheckUpload(
      new File(['board'], 'design.brd'),
      new File(['firmware'], 'app.elf'),
      new File(['schematic'], 'design.sch'),
      'name = "browser"',
    )
    expect((form.get('schematic') as File).name).toBe('design.sch')
    expect((form.get('spec') as File).name).toBe('spec.toml')
  })
})
