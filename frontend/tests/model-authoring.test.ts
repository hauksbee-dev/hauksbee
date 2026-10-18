import { describe, expect, test } from 'bun:test'
import { inferPartClass, kindLabel } from '../src/components/editor/part-class'
import {
  describesBehaviour, missingParams, missingSentence, readModel, readSource, setKey, tomlNumber, writeSource,
} from '../src/components/editor/toml-edit'
import { plainStatus } from '../src/components/WritePart'

describe('choosing the kind of a part from the board', () => {
  test('a BS250 on a Q reference is a P-channel MOSFET, with the reason', () => {
    const found = inferPartClass({ reference: 'Q1', value: 'BS250' })
    expect(found.kind).toBe('pmos')
    expect(found.why).toContain('P-channel MOSFET')
    expect(found.why).toContain('BS250')
    expect(found.why).toContain('reference Q1')
  })

  test('the symbol and footprint go into the reason when the board has them', () => {
    const found = inferPartClass({ reference: 'Q1', value: 'BS250', lib_id: 'Transistor_FET:BS250', footprint: 'Package_TO_SOT_THT:TO-92_Inline' })
    expect(found.kind).toBe('pmos')
    expect(found.why).toContain('Transistor_FET:BS250')
    expect(found.why).toContain('TO-92_Inline')
  })

  test('N-channel, NPN and PNP part numbers are told apart', () => {
    expect(inferPartClass({ reference: 'Q2', value: '2N7002' }).kind).toBe('nmos')
    expect(inferPartClass({ reference: 'Q3', value: 'BC847B' }).kind).toBe('bjt_npn')
    expect(inferPartClass({ reference: 'Q4', value: 'BC857' }).kind).toBe('bjt_pnp')
  })

  test('a transistor nothing identifies is a question, never a guess', () => {
    const found = inferPartClass({ reference: 'Q9', value: 'XYZ123', footprint: 'Package_TO_SOT_SMD:SOT-23' })
    expect(found.kind).toBeNull()
    expect(found.why).toContain('MOSFET or a bipolar')
    const fet = inferPartClass({ reference: 'Q9', value: 'XYZ123', lib_id: 'Transistor_FET:Generic' })
    expect(fet.kind).toBeNull()
    expect(fet.why).toContain('polarity')
  })

  test('reference letters settle passives, diodes and connectors; ICs go by part number', () => {
    expect(inferPartClass({ reference: 'R12', value: '10k' }).kind).toBe('passive')
    expect(inferPartClass({ reference: 'C3', value: '100n' }).kind).toBe('passive')
    expect(inferPartClass({ reference: 'D2', value: 'LED' }).kind).toBe('diode')
    expect(inferPartClass({ reference: 'J2', value: 'USB_C' }).kind).toBe('connector')
    expect(inferPartClass({ reference: 'U3', value: 'AMS1117-3.3' }).kind).toBe('vreg')
    expect(inferPartClass({ reference: 'U1', value: 'LM358' }).kind).toBe('opamp')
    expect(inferPartClass({ reference: 'U5', value: 'ATmega328P-PU' }).kind).toBe('mcu')
    expect(inferPartClass({ reference: 'U4', value: 'CH340C' }).kind).toBeNull()
  })

  test('the engine\'s own kind wins when it has one', () => {
    expect(inferPartClass({ reference: 'U9', value: 'WIDGET', model_kind: 'comparator' }).kind).toBe('comparator')
  })

  test('kinds are named in plain words outside the TOML', () => {
    expect(kindLabel('pmos')).toBe('P-channel MOSFET')
    expect(kindLabel('i2c_sensor')).toBe('I2C sensor')
    expect(kindLabel('some_new_kind')).toBe('some new kind')
  })
})

const SCAFFOLD = `[[models]]
id = "q1_bs250"
kind = "pmos"
description = "BS250"

[models.match]
value_re = "^BS250$"

# [models.params]

# [models.ratings]

[models.source]
tier = "user-model"
# [[models.source.references]]
# url = "https://vendor.example/device.pdf"
[[models.source.uncertainty]]
status = "unknown"
`

describe('the form and the TOML are one text', () => {
  test('a value switches the commented-out table on and writes one line', () => {
    const out = setKey(SCAFFOLD, '[models.params]', 'vto', '-1.5')
    expect(out).toContain('\n[models.params]\nvto = -1.5\n')
    const model = readModel(out)
    expect(model.ok && (model.model.params as Record<string, number>).vto).toBe(-1.5)
    // Everything else is untouched.
    expect(out).toContain('# [models.ratings]')
    expect(out).toContain('value_re = "^BS250$"')
  })

  test('a second edit replaces the line and an empty one removes it', () => {
    const one = setKey(SCAFFOLD, '[models.params]', 'vto', '-1.5')
    const two = setKey(one, '[models.params]', 'vto', '-2.0')
    expect(two.match(/vto =/g)?.length).toBe(1)
    expect(two).toContain('vto = -2.0')
    expect(setKey(two, '[models.params]', 'vto', null)).not.toContain('vto =')
  })

  test('the kind and description are edited in place', () => {
    const out = setKey(setKey(SCAFFOLD, '[[models]]', 'kind', '"nmos"'), '[[models]]', 'description', '"BS170 \\"x\\""')
    const model = readModel(out)
    expect(model.ok && model.model.kind).toBe('nmos')
    expect(model.ok && model.model.description).toBe('BS170 "x"')
  })

  test('a table the file lacks is appended', () => {
    const out = setKey('[[models]]\nid = "x"\nkind = "pmos"\n', '[models.params]', 'kp', '0.1')
    expect(out).toContain('\n[models.params]\nkp = 0.1\n')
  })

  test('a source reference is written only whole, and reads back', () => {
    const out = writeSource(SCAFFOLD, { url: 'https://example.com/bs250.pdf', title: 'BS250 datasheet rev 3', locator: 'Table 2' })
    const model = readModel(out)
    expect(model.ok).toBe(true)
    if (model.ok) expect(readSource(model.model)).toEqual({ url: 'https://example.com/bs250.pdf', title: 'BS250 datasheet rev 3', locator: 'Table 2' })
    expect(out).toContain('[[models.source.uncertainty]]')
  })

  test('numbers are written as TOML floats, and junk is refused', () => {
    expect(tomlNumber('-1.5')).toBe('-1.5')
    expect(tomlNumber('200')).toBe('200.0')
    expect(tomlNumber('3e-6')).toBe('0.000003')
    expect(tomlNumber('1,5')).toBeNull()
    expect(tomlNumber('abc')).toBeNull()
    expect(tomlNumber('')).toBeNull()
  })

  test('behaviour counts only when something simulates', () => {
    const bare = readModel(SCAFFOLD)
    expect(bare.ok && describesBehaviour(bare.model)).toBe(false)
    const withParams = readModel(setKey(SCAFFOLD, '[models.params]', 'vto', '-1.5'))
    expect(withParams.ok && describesBehaviour(withParams.model)).toBe(true)
  })
})

describe('the validator\'s answer in plain words', () => {
  const missingError = "entry 'q1_bs250': model 'q1_bs250': missing required param 'vto'; model 'q1_bs250': missing required param 'kp'"
  const base = { toml: true, kindChosen: true, model: readModel(SCAFFOLD), view: 'form' as const }

  test('names the values still needed, with their engineering names', () => {
    expect(missingParams(missingError).map(m => m.key)).toEqual(['vto', 'kp'])
    expect(missingSentence(missingParams(missingError))).toBe('Threshold voltage (Vto), Transconductance (Kp)')
    const s = plainStatus({ phase: 'bad', error: missingError, body: '', format: 'toml' }, base)
    expect(s.state).toBe('missing')
    expect(s.text).toBe('Still needed: Threshold voltage (Vto), Transconductance (Kp).')
  })

  test('an unchosen kind asks for the kind, not a parse error', () => {
    const s = plainStatus({ phase: 'bad', error: 'unknown variant `choose_kind`', body: '', format: 'toml' }, { ...base, kindChosen: false })
    expect(s.state).toBe('choose')
    expect(s.text).toContain('Choose what kind of part it is')
  })

  test('a valid file says it is ready, without the engine\'s TOML jargon in the line', () => {
    const summary = 'valid: q1_bs250 is a Pmos (BS250) · valid but incomplete: no model declares [models.coverage]; saving this file will leave executable scope unchanged.'
    const withValues = readModel(setKey(SCAFFOLD, '[models.params]', 'vto', '-1.5'))
    const s = plainStatus({ phase: 'ok', summary, body: '', format: 'toml' }, { ...base, model: withValues })
    expect(s.state).toBe('ok')
    expect(s.text).toStartWith('Ready to save.')
    expect(s.text + (s.note ?? '')).not.toContain('[models.coverage]')
    expect(s.text).not.toContain('Digital')
  })

  test('an identity-only file says the part stays open', () => {
    const identity = readModel(setKey(setKey(SCAFFOLD, '[models.params]', 'identity_only', 'true'), '[models.params]', 'warning', '"w"'))
    const s = plainStatus({ phase: 'ok', summary: 'valid: x', body: '', format: 'toml' }, { ...base, model: identity })
    expect(s.note).toContain('stays open')
  })
})
