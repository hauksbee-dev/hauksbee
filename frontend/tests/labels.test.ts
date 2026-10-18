// The label table: frontend/src/lib/labels.ts is generated from hauksbee-ir
// (crates/hauksbee-ir/src/labels.rs). The Rust drift test holds the file equal
// to what the crate generates; this holds it equal to the vocabulary JSON the
// crate commits (the other direction), and holds the hand-written helpers to
// the Rust ones through the examples the JSON carries.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { ACRONYMS, BACKEND_KINDS, explainFor, humanise, LABEL_ENTRIES, labelFor } from '../src/lib/labels'

interface VocabularyJson {
  severities: Array<{ id: string; label: string; meaning: string }>
  verdicts: Array<{ id: string; label: string }>
  checks: Array<{ id: string; label: string }>
  glossary: Array<{ term: string; label: string; short: string }>
  labels: {
    entries: Array<{ id: string; group: string; label: string; explain?: string }>
    acronyms: Array<[string, string]>
    backend_kinds: Array<{ kind: string; label: string }>
    humanise_examples: Array<{ input: string; output: string }>
  }
}

const vocabulary = JSON.parse(readFileSync(
  new URL('../../crates/hauksbee-ir/schemas/hauksbee-vocabulary.json', import.meta.url), 'utf8',
)) as VocabularyJson

describe('labels.ts against the vocabulary JSON', () => {
  test('has every row the engine has, and nothing else', () => {
    expect(LABEL_ENTRIES.map(e => ({ ...e }))).toEqual(vocabulary.labels.entries)
  })

  test('has the same acronyms and backend kinds', () => {
    expect(ACRONYMS).toEqual(Object.fromEntries(vocabulary.labels.acronyms))
    expect(BACKEND_KINDS).toEqual(Object.fromEntries(vocabulary.labels.backend_kinds.map(k => [k.kind, k.label])))
  })

  test('humanises every example exactly as the Rust helper does', () => {
    expect(vocabulary.labels.humanise_examples.length).toBeGreaterThan(5)
    for (const { input, output } of vocabulary.labels.humanise_examples) {
      expect([input, humanise(input)]).toEqual([input, output])
    }
  })

  test('reuses the wording of the vocabulary and the glossary instead of restating it', () => {
    for (const s of vocabulary.severities) expect(labelFor(s.id)).toBe(s.label)
    for (const s of vocabulary.severities) expect(explainFor(s.id)).toBe(s.meaning)
    for (const v of vocabulary.verdicts) expect(labelFor(v.id)).toBe(v.label)
    for (const c of vocabulary.checks) expect(labelFor(c.id)).toBe(c.label)
    // `pass` and `fail` are both a verdict word and a glossary term; the verdict wins.
    const verdictIds = new Set(vocabulary.verdicts.map(v => v.id))
    for (const g of vocabulary.glossary.filter(t => !verdictIds.has(t.term))) {
      expect([g.term, labelFor(g.term)]).toEqual([g.term, g.label])
    }
  })

  test('has no raw id, underscore or empty text in any label', () => {
    for (const e of LABEL_ENTRIES) {
      expect(e.label.trim()).not.toBe('')
      expect(e.label).not.toContain('_')
      if (e.id.includes('_') || e.id.includes(':')) expect(e.label).not.toBe(e.id)
    }
    expect(new Set(LABEL_ENTRIES.map(e => e.id)).size).toBe(LABEL_ENTRIES.length)
  })
})

describe('labelFor', () => {
  test('names what the audit found as raw ids', () => {
    expect(labelFor('board_crystal_frequency_binding')).toBe('Crystal frequency from the board')
    expect(labelFor('usb_device_controller')).toBe('USB device controller')
    expect(labelFor('silicon_power_current_profile')).toBe('Supply current by state')
    expect(labelFor('silicon_timing_and_errata')).toBe('Silicon timing and errata')
    expect(labelFor('executable_scope_unspecified')).toBe('Scope not stated')
    expect(labelFor('executable_partial')).toBe('Partly modelled')
    expect(labelFor('identity_only')).toBe('Identity only')
    expect(labelFor('datasheet-curves')).toBe('Checked against datasheet curves')
    expect(labelFor('simavr:atmega328p')).toBe('ATmega328P (simavr)')
    // The inspector's "bound model: vreg", in both spellings the engine sends.
    expect(labelFor('vreg')).toBe('Voltage regulator')
    expect(labelFor('bjt_npn')).toBe('NPN transistor')
    expect(labelFor('bjtnpn')).toBe('NPN transistor')
    expect(labelFor('analogswitch')).toBe('Analog switch')
  })

  test('explains a known id in one sentence, and an unknown one not at all', () => {
    expect(explainFor('board_crystal_frequency_binding')).toMatch(/crystal/)
    expect(explainFor('not_in_the_table')).toBeUndefined()
    expect(explainFor('')).toBeUndefined()
    expect(explainFor(null)).toBeUndefined()
  })

  test('humanises an unknown id: never snake_case, sentence case, acronyms kept', () => {
    expect(labelFor('some_new_capability')).toBe('Some new capability')
    expect(labelFor('spi_dual_quad_io')).toBe('SPI dual quad I/O')
    expect(labelFor('led_generic_corner_high_vf')).toBe('LED generic corner high Vf')
    expect(labelFor('some-kebab-case')).toBe('Some kebab case')
    for (const id of ['a_b_c', '__x__', 'ends_with_', '_leading']) expect(labelFor(id)).not.toContain('_')
  })

  test('reads an unlisted emulator backend as a chip and its kind', () => {
    expect(labelFor('simavr:atmega2560')).toBe('ATMEGA2560 (simavr)')
    expect(labelFor('renode:stm32h5')).toBe('STM32H5 (Renode)')
    expect(labelFor('none:samd21')).toBe('SAMD21 (no emulator)')
    // Not a backend kind: humanised, so it still has no raw punctuation id feel.
    expect(labelFor('bogus:thing')).toBe('Bogus:thing')
    expect(labelFor('simavr:')).toBe('Simavr:')
  })

  test('an empty id is empty text, and object-prototype names are only words', () => {
    expect(labelFor('')).toBe('')
    expect(labelFor(undefined)).toBe('')
    expect(labelFor(null)).toBe('')
    for (const id of ['constructor', '__proto__', 'toString', 'hasOwnProperty']) {
      expect(typeof labelFor(id)).toBe('string')
      expect(labelFor(id)).not.toContain('_')
    }
    expect(labelFor('constructor')).toBe('Constructor')
  })
})
