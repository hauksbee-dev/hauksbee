// A catalogue example is appended to the raw TOML once per block, and the first
// required value is selected so typing replaces it.

import { describe, expect, test } from 'bun:test'
import { appendExample } from '../src/lib/ci-catalog'

const SPEC = 'name = "t"\nboard = "b.kicad_pcb"\n\n[[assert]]\nkind = "no_faults"\n'
const AC = '[ac]\nfstart = 10\nfstop = 1e6\npoints = 20\n\n[[assert]]\nkind = "ac_gain"\nnet = "+5V"\nmin = -3\n'

describe('appendExample', () => {
  test('appends after a blank line and selects the first value to fill', () => {
    const { text, select } = appendExample(SPEC, AC, ['net', 'min'])
    expect(text.startsWith(SPEC)).toBe(true)
    expect(text).toContain('\n\n[ac]\nfstart = 10')
    expect(select && text.slice(select.start, select.end)).toBe('"+5V"')
  })

  test('a block the spec already has is not repeated, an assertion always is', () => {
    const once = appendExample(SPEC, AC, ['net']).text
    const twice = appendExample(once, AC, ['net']).text
    expect(twice.match(/^\[ac\]$/gm)?.length).toBe(1)
    expect(twice.match(/kind = "ac_gain"/g)?.length).toBe(2)
  })

  test('an example with nothing to fill leaves no selection', () => {
    expect(appendExample('', '[[assert]]\nkind = "no_faults"\n', []).select).toBeNull()
  })
})
