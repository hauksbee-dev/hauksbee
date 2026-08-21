import { describe, expect, test } from 'bun:test'
import { buildBoardUpload, buildCheckUpload, buildPortableCheckSpec } from '../src/lib/board-upload'

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

  test('omits staged firmware from a portable spec when the run choice is off', () => {
    const spec = buildPortableCheckSpec(
      'design.brd',
      new File(['firmware'], 'app.elf'),
      null,
      'name = "board checks"\nfirmware = "../firmware/old.elf"\n\n[[assert]]\nkind = "no_faults"\n',
      false,
    )
    expect(spec).not.toContain('firmware =')
    expect(spec).toContain('board = "../hardware/design.brd"')
  })

  test('a null firmware prop produces a check upload without firmware', () => {
    const form = buildCheckUpload(
      new File(['board'], 'design.brd'),
      null,
      null,
      'name = "board checks"',
    )
    expect(form.has('firmware')).toBeFalse()
  })

  test('threads the same companion inputs through checks', () => {
    const board = new File(['board'], 'design.brd')
    const firmware = new File(['firmware'], 'app.elf')
    const schematic = new File(['schematic'], 'design.sch')
    const form = buildCheckUpload(board, firmware, schematic, 'duration_ms = 1')

    expect((form.get('board') as File).name).toBe('design.brd')
    expect((form.get('firmware') as File).name).toBe('app.elf')
    expect((form.get('schematic') as File).name).toBe('design.sch')
    expect((form.get('spec') as File).name).toBe('spec.toml')
  })

  test('threads manufacturing and model evidence through every multipart surface', () => {
    const supplemental = {
      bom: new File(['bom'], 'bom.csv'),
      placement: new File(['placement'], 'board.pos'),
      variant: new File(['variant'], 'production.variant.toml'),
      asbuilt: new File(['overlay'], 'unit.asbuilt.toml'),
      models: [new File(['model a'], 'a.toml'), new File(['model b'], 'b.toml')],
    }
    for (const form of [
      buildBoardUpload(new File(['board'], 'design.brd'), null, null, supplemental),
      buildCheckUpload(new File(['board'], 'design.brd'), null, null, 'duration_ms = 1', supplemental),
    ]) {
      expect((form.get('bom') as File).name).toBe('bom.csv')
      expect((form.get('placement') as File).name).toBe('board.pos')
      expect((form.get('variant') as File).name).toBe('production.variant.toml')
      expect((form.get('asbuilt') as File).name).toBe('unit.asbuilt.toml')
      expect(form.getAll('model_file').map(file => (file as File).name)).toEqual(['a.toml', 'b.toml'])
    }
  })

  test('the exported pipeline spec names every staged design input', () => {
    const spec = buildPortableCheckSpec(
      'design.brd',
      new File(['firmware'], 'app.elf'),
      new File(['schematic'], 'design.sch'),
      'duration_ms = 1',
    )
    expect(spec).toContain('board = "../hardware/design.brd"')
    expect(spec).toContain('firmware = "../firmware/app.elf"')
    expect(spec).toContain('schematic = "../hardware/design.sch"')
    expect(spec).toContain('duration_ms = 1')
  })

})
