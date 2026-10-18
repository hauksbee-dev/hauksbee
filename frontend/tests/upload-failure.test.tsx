// A refused board says, in plain words, what is wrong with the file and the
// exact thing to do. The engine's own sentence is kept (behind "Technical
// detail"), but it is never the only thing the person is given.

import { describe, expect, test } from 'bun:test'
import { explainBoardFailure } from '../src/components/UploadFailure'

// The engine's wording for the audit's three bad files.
const TRUNCATED = "Could not read this board file: 'truncated.kicad_pcb': parse error at line 1 (byte 24): unclosed list"
const WRONG_KICAD = "Could not read this board file: 'broken.kicad_pcb': unrecognized board format"
const WRONG_TXT = "Could not read this board file: 'notes.txt': unrecognized board format"

describe('explainBoardFailure', () => {
  test('a cut-off file says it ends early and tells you to re-save it', () => {
    const e = explainBoardFailure(TRUNCATED, 'truncated.kicad_pcb')
    expect(e.title).toBe('This file ends early')
    expect(e.cause).toContain('truncated.kicad_pcb')
    expect(e.cause).toContain('cut off')
    expect(e.steps.join(' ')).toContain('File > Save')
  })

  test('a KiCad-named file with other contents says what a board starts with', () => {
    const e = explainBoardFailure(WRONG_KICAD, 'broken.kicad_pcb')
    expect(e.title).toBe('This is not a KiCad board')
    expect(e.cause).toContain('(kicad_pcb')
    expect(e.steps.join(' ')).toContain('.kicad_pro')
  })

  test('another extension names that extension and the formats to drop instead', () => {
    const e = explainBoardFailure(WRONG_TXT, 'notes.txt')
    expect(e.title).toBe('Hauksbee cannot read a .txt file')
    expect(e.steps.join(' ')).toContain('.kicad_pcb')
    expect(e.steps.join(' ')).toContain('zip')
  })

  test('a syntax error that is not a cut-off names its line', () => {
    const e = explainBoardFailure("Could not read this board file: 'a.kicad_pcb': parse error at line 41 (byte 900): unexpected token", 'a.kicad_pcb')
    expect(e.title).toBe('This file has a syntax error')
    expect(e.cause).toContain('line 41')
    expect(e.steps[0]).toContain('line 41')
  })

  test('any other refusal keeps the engine sentence and still gives a step', () => {
    const e = explainBoardFailure("Could not read this board file: this is a pre-Eagle-6 binary drawing", 'old.brd')
    expect(e.cause).toContain('pre-Eagle-6 binary drawing')
    expect(e.steps.length).toBeGreaterThan(0)
  })

  test('every explanation has at least one concrete step', () => {
    for (const m of [TRUNCATED, WRONG_KICAD, WRONG_TXT, '', 'boom']) {
      expect(explainBoardFailure(m, 'x.bin').steps.length).toBeGreaterThan(0)
    }
  })
})
