import { describe, expect, test } from 'bun:test'
import {
  assertBlock, badNumbers, countAsserts, fixFor, groupOf, isReferenceNet, isSkippedNet, KIND_GROUPS, measureAs, missingRequired,
  needMet, nominalFromName, offersForNet, offersForPart, prefill, rankNets, requestFor, resolveDefault, runWindowMs, scenarioIds,
  specBody, wouldFail,
  type CatalogField, type CatalogKind, type OfferContext,
} from '../src/lib/check-catalog'
import { checkKind } from '../src/lib/check-spec'
import { tomlToBuilder } from '../src/lib/spec-model'

// A catalogue shaped like `GET /api/ci/catalog` (version 2), trimmed to the
// fields the click-to-assert logic reads.
const f = (key: string, extra: Partial<CatalogField> = {}): CatalogField => ({
  key, label: key, unit: null, type: 'number', required: false, help: '', if_absent: null, default_from: null, ...extra,
})
const kind = (
  name: string, targets: string[], offer: Record<string, string>, fields: CatalogField[],
  prerequisites: CatalogKind['prerequisites'] = [], example = '',
): CatalogKind => ({
  kind: name, title: name, summary: '', example, fails_because: '', targets, offer, fields, prerequisites,
})

const NET = f('net', { type: 'string', required: true })
const REF = f('ref', { type: 'string', required: true })
const CATALOG: CatalogKind[] = [
  kind('voltage', ['net'], { net: 'Keep {net} within a voltage range' }, [
    NET,
    f('min', { required: true, help: 'lowest allowed volts (or give max)', default_from: { source: 'measured', pct: -5 } }),
    f('max', { default_from: { source: 'measured', pct: 5 } }),
  ]),
  kind('rail_window', ['net'], { net: '{net} stays inside a window' }, [
    NET, f('scenario', { type: 'string', required: true }),
    f('min', { required: true, default_from: { source: 'measured', pct: -5 } }),
  ], [{ need: 'scenario', label: 'a scenario', fix: 'Add a [[scenario]] to the spec.', action: { id: 'add_scenario', label: 'Add a load-step test' } }],
  '[[scenario]]\nid = "boot"\n\n[[assert]]\nkind = "rail_window"\n'),
  kind('logic_level', ['net'], { net: '{net} reaches a valid logic level' }, [NET]),
  kind('toggle', ['net'], { net: '{net} blinks' }, [
    NET, f('freq_hz', { required: true, default_from: { source: 'measured', pct: 0 } }),
  ], [{ need: 'firmware', label: 'firmware', fix: 'Attach firmware.', action: { id: 'add_firmware', label: 'Add firmware' } }]),
  kind('boot_coverage', ['net'], { net: 'Firmware drives {net} in time' }, [
    NET, f('min', { required: true, default_from: { source: 'measured', pct: -10 } }), f('deadline_ms', { required: true }),
  ], [{ need: 'firmware', label: 'firmware', fix: 'Attach firmware.', action: { id: 'add_firmware', label: 'Add firmware' } }]),
  kind('protection_trip', ['net'], { net: 'The cutoff on {net} trips' }, [
    f('supply_net', { type: 'string', required: true }), f('expect_trip', { type: 'boolean', required: true }),
  ], [{ need: 'supply', label: 'a battery supply', fix: 'Declare a battery [[supply]].', action: { id: 'add_supply', label: 'Add a supply' } }]),
  kind('hwtrace', ['net'], { net: 'trace' }, [f('trace', { type: 'string', required: true })],
    [{ need: 'trace', label: 'a captured trace', fix: 'Capture a trace.' }]),
  kind('ac_gain', ['net'], { net: 'Gain at {net}' }, [NET, f('min', { required: true, help: 'lowest gain (or max)' })],
    [{ need: 'ac', label: 'an [ac] block', fix: 'Add [ac].', action: { id: 'add_ac_sweep', label: 'Add a frequency sweep' } }], '[ac]\nsource = "VIN"\n\n[[assert]]\nkind = "ac_gain"\n'),
  kind('max_current', ['part'], { part: 'Set a current limit for {ref}' }, [
    REF, f('amps', {
      required: true,
      default_from: { source: 'rating', key: 'max_current_a', derate: 0.8, fallback: { source: 'measured_peak', factor: 2 } },
    }),
  ]),
  kind('max_temp', ['part'], { part: 'Set a temperature limit for {ref}' }, [REF, f('celsius')]),
  kind('fires', ['part'], { part: '{ref} fires' }, [REF]),
  kind('no_faults', ['board'], { board: 'Nothing over-stressed' }, []),
]

const ctx = (over: Partial<OfferContext> = {}): OfferContext => ({
  kinds: CATALOG, spec: { toml: '', hasFirmware: false }, supplyNets: new Set(['/+3V3']),
  capabilities: ['max_current', 'max_temp'], boundKind: null, ...over,
})

describe('nets', () => {
  test('reference nets are never offered a voltage', () => {
    for (const net of ['GND', '/GND', 'AGND', 'DGND', '/PGND', 'VSS', '0V', 'EARTH']) expect(isReferenceNet(net)).toBe(true)
    for (const net of ['/+3V3', '/D13', 'VBUS']) expect(isReferenceNet(net)).toBe(false)
  })

  test('pins the layout left unconnected are never offered either', () => {
    expect(isSkippedNet('unconnected-(U1-NC-Pad4)')).toBe(true)
    expect(isSkippedNet('/+3V3')).toBe(false)
    expect(rankNets(['unconnected-(U1-NC-Pad4)', '/+3V3'], new Set(['/+3V3']))).toEqual(['/+3V3'])
  })

  test('a rail name states its voltage', () => {
    expect(nominalFromName('/+3V3')).toBe(3.3)
    expect(nominalFromName('+5V')).toBe(5)
    expect(nominalFromName('-12V')).toBe(-12)
    expect(nominalFromName('/D13')).toBeNull()
  })

  test('a part lists supply nets first, then outputs, then inputs, and never ground', () => {
    expect(rankNets(['GND', '/SENSE', '/LED_OUT', '/+3V3'], new Set(['/+3V3']))).toEqual(['/+3V3', '/LED_OUT', '/SENSE'])
  })
})

describe('offers', () => {
  test('a rail offers a window and a logic level; ground offers nothing', () => {
    const kinds = offersForNet('/+3V3', ctx()).map(o => o.kind)
    expect(kinds).toContain('voltage')
    expect(kinds).toContain('rail_window')
    expect(kinds).toContain('logic_level')
    expect(offersForNet('GND', ctx())).toEqual([])
  })

  test('a kind whose prerequisite the spec lacks is blocked with its reason, and sorted last', () => {
    const offers = offersForNet('/+3V3', ctx())
    const window = offers.find(o => o.kind === 'rail_window')!
    expect(window.blocked?.reason).toContain('[[scenario]]')
    expect(window.blocked?.fix?.toml).toContain('[[scenario]]')
    expect(window.blocked?.fix?.toml).not.toContain('[[assert]]')
    expect(offers.findIndex(o => o.blocked)).toBeGreaterThan(offers.findIndex(o => o.kind === 'voltage'))
    const met = offersForNet('/+3V3', ctx({ spec: { toml: '[[scenario]]\nid = "boot"\n', hasFirmware: false } }))
    expect(met.find(o => o.kind === 'rail_window')?.blocked).toBeUndefined()
  })

  test('the firmware prerequisite is fixed by the firmware picker, a trace has no fix', () => {
    const blocked = offersForNet('/D13', ctx()).find(o => o.kind === 'toggle')!.blocked!
    expect(blocked.fix?.pickFirmware).toBe(true)
    expect(fixFor({ need: 'trace', label: 'a trace', fix: '' }, CATALOG)).toBeNull()
    expect(fixFor({ need: 'ac', label: 'an [ac] block', fix: '', action: { id: 'add_ac_sweep', label: 'Add a frequency sweep' } }, CATALOG)?.toml).toBe('[ac]\nsource = "VIN"\n')
  })

  test('a protection cutoff is offered on supplies only', () => {
    expect(offersForNet('/+3V3', ctx()).some(o => o.kind === 'protection_trip')).toBe(true)
    expect(offersForNet('/D13', ctx()).some(o => o.kind === 'protection_trip')).toBe(false)
  })

  test('a part says what is measured, lists its best nets, and leaves ground out', () => {
    const offers = offersForPart('U1', ['GND', '/SENSE', '/+3V3', '/LED_OUT', '/SDA'], ctx())
    expect(offers.length).toBeGreaterThanOrEqual(3)
    expect(offers.find(o => o.kind === 'max_current')?.text).toBe('Set a current limit for U1')
    const nets = offers.filter(o => o.kind === 'voltage').map(o => o.net)
    expect(nets).toEqual(['/+3V3', '/LED_OUT', '/SDA'])
    expect(offers.map(o => o.text).join(' ')).not.toContain('GND')
  })

  test('a current or temperature ceiling the circuit cannot evaluate is blocked, with the reason', () => {
    const offers = offersForPart('U2', ['/+3V3'], ctx({ capabilities: ['max_temp'] }))
    const current = offers.find(o => o.kind === 'max_current')!
    expect(current.blocked?.reason).toContain('no measurable through-current')
    expect(offers.find(o => o.kind === 'max_temp')?.blocked).toBeUndefined()
  })

  test('comparator checks appear only for a comparator', () => {
    expect(offersForPart('U1', [], ctx()).some(o => o.kind === 'fires')).toBe(false)
    expect(offersForPart('U1', [], ctx({ boundKind: 'comparator' })).some(o => o.kind === 'fires')).toBe(true)
  })

  test('a pad the user pointed at comes first and names the pad', () => {
    const offers = offersForPart('U3', ['/+3V3', '/SDA', '/LED_OUT'], ctx(), 3, '/SDA')
    const first = offers.filter(o => o.kind === 'voltage')[0]
    expect(first.net).toBe('/SDA')
    expect(first.text).toContain('Pad of U3')
  })

  test('the menu groups cover every kind once', () => {
    const all = KIND_GROUPS.flatMap(([, kinds]) => kinds)
    expect(all).toHaveLength(23)
    expect(new Set(all).size).toBe(23)
    expect(groupOf('rail_window')).toBe('Voltages')
    expect(groupOf('phase_margin')).toBe('Small-signal')
  })
})

describe('defaults', () => {
  const find = (name: string) => CATALOG.find(k => k.kind === name)!
  const none = { reading: null, nominal: null, ratings: undefined }

  test('a voltage window opens around the simulated value', () => {
    const { values, notes } = prefill(find('voltage'), { net: '/+5V' }, { ...none, reading: { value: 5, min: 5, max: 5 } }, { toml: '', hasFirmware: false })
    expect(values).toEqual({ net: '/+5V', min: '4.75', max: '5.25' })
    expect(notes.min).toContain('5 %')
  })

  test('without a reading the rail name stands in, and says so', () => {
    const { values, notes } = prefill(find('voltage'), { net: '+3V3' }, { ...none, nominal: 3.3 }, { toml: '', hasFirmware: false })
    expect(values.min).toBe('3.135')
    expect(notes.min).toContain('nominal')
  })

  test('a width or a frequency is not guessed from a rail name', () => {
    const { values } = prefill(find('toggle'), { net: '+3V3' }, { ...none, nominal: 3.3 }, { toml: '', hasFirmware: true })
    expect(values.freq_hz).toBe('')
  })

  test('a current ceiling is derated from the stated rating, else from the measured peak', () => {
    const rated = prefill(find('max_current'), { ref: 'R1' }, { ...none, ratings: { max_current_a: 0.5 } }, { toml: '', hasFirmware: false })
    expect(rated.values.amps).toBe('0.4')
    expect(rated.notes.amps).toBe('80 % of the 500 mA rating')
    const measured = prefill(find('max_current'), { ref: 'R1' }, { ...none, reading: { value: 0.1, min: null, max: null } }, { toml: '', hasFirmware: false })
    expect(measured.values.amps).toBe('0.2')
    expect(measured.notes.amps).toContain('measured peak')
    expect(prefill(find('max_current'), { ref: 'R1' }, none, { toml: '', hasFirmware: false }).values.amps).toBe('')
  })

  test('fields the catalogue leaves without a source still get an honest starting value', () => {
    const boot = prefill(find('boot_coverage'), { net: '/EN' }, { ...none, nominal: 3.3 }, { toml: '', hasFirmware: true })
    expect(boot.values.deadline_ms).toBe('20')
    expect(boot.notes.deadline_ms).toContain('starting value')
    const trip = prefill(find('protection_trip'), { net: '/VBAT' }, none, { toml: '', hasFirmware: false })
    expect(trip.values).toEqual({ supply_net: '/VBAT', expect_trip: 'true' })
    const window = prefill(find('rail_window'), { net: '/+3V3' }, none, { toml: '[[scenario]]\nid = "brownout"\n', hasFirmware: false })
    expect(window.values.scenario).toBe('brownout')
  })

  test('sources that need a running firmware stay empty', () => {
    expect(resolveDefault({ source: 'card_logic_levels' }, { reading: null, nominal: null, ratings: undefined })).toBeNull()
    expect(resolveDefault({ source: 'first_uart_line' }, { reading: null, nominal: null, ratings: undefined })).toBeNull()
    expect(resolveDefault({ source: 'constant', value: 0.1 }, { reading: null, nominal: null, ratings: undefined })?.value).toBe('0.1')
  })
})

describe('required fields and the assertion text', () => {
  const voltage = CATALOG[0]

  test('an either/or requirement is met by the alternative', () => {
    expect(missingRequired(voltage, { net: '+3V3', min: '', max: '' }).map(x => x.key)).toEqual(['min'])
    expect(missingRequired(voltage, { net: '+3V3', min: '', max: '3.6' })).toEqual([])
    expect(missingRequired(voltage, { net: '', min: '3', max: '' }).map(x => x.key)).toEqual(['net'])
  })

  test('a non-number is named', () => {
    expect(badNumbers(voltage, { net: '+3V3', min: 'abc', max: '' }).map(x => x.key)).toEqual(['min'])
  })

  test('the table has the kind, then the filled fields in catalogue order, typed', () => {
    expect(assertBlock(voltage, { net: '/+3V3', min: '3.135', max: '', extra: 'ignored' }))
      .toBe('[[assert]]\nkind = "voltage"\nnet = "/+3V3"\nmin = 3.135\n')
    const trip = CATALOG.find(k => k.kind === 'protection_trip')!
    expect(assertBlock(trip, { supply_net: '/VBAT', expect_trip: 'true' }))
      .toBe('[[assert]]\nkind = "protection_trip"\nsupply_net = "/VBAT"\nexpect_trip = true\n')
    expect(requestFor(voltage, { net: 'A', min: '1' })).toEqual({ type: 'assert', kind: 'voltage', toml: '[[assert]]\nkind = "voltage"\nnet = "A"\nmin = 1\n' })
    // A blink rate needs its periods in the run: 5 Hz is 5 periods in 1 s, and no assert asks for less than that.
    const toggle = CATALOG.find(k => k.kind === 'toggle')!
    expect(runWindowMs('toggle', { freq_hz: '5' })).toBe(1000)
    expect(runWindowMs('toggle', { freq_hz: '0.5' })).toBe(10000)
    expect(runWindowMs('toggle', { min_toggles: '4' })).toBeNull()
    expect(runWindowMs('voltage', { freq_hz: '5' })).toBeNull()
    expect(requestFor(toggle, { net: 'D13', freq_hz: '5' })).toMatchObject({ type: 'assert', kind: 'toggle', run_ms: '1000' })
    expect(requestFor(toggle, { net: 'D13', freq_hz: '5' }, 2400)).toMatchObject({ run_ms: '2400' })
    expect('run_ms' in requestFor(voltage, { net: 'A', min: '1' })).toBe(false)
  })

  test('a builder kind survives the round trip into a row, boot coverage under either spelling', () => {
    const parsed = tomlToBuilder(assertBlock(voltage, { net: '/+3V3', min: '3.1', max: '3.5' }))
    expect(parsed?.checks).toHaveLength(1)
    expect(parsed?.checks[0]).toMatchObject({ kind: 'voltage', net: '/+3V3', min: '3.1', max: '3.5' })
    expect(checkKind('boot-coverage')?.kind).toBe('boot_coverage')
    const boot = tomlToBuilder('[[assert]]\nkind = "boot_coverage"\nnet = "EN"\nmin = 3\ndeadline_ms = 20\n')
    expect(boot?.checks[0]).toMatchObject({ kind: 'boot_coverage', net: 'EN', deadline_ms: '20' })
  })
})

describe('the preview', () => {
  test('which kinds are read off the board', () => {
    expect(measureAs('voltage')).toBe('voltage')
    expect(measureAs('rail_window')).toBe('voltage')
    expect(measureAs('max_temp')).toBeNull()
  })

  test('a window the rail already leaves would fail today', () => {
    const rail = { value: 5, min: 4.99, max: 5.01 }
    expect(wouldFail('voltage', { min: '5.5', max: '' }, rail)).toBe(true)
    expect(wouldFail('voltage', { min: '4.75', max: '5.25' }, rail)).toBe(false)
    expect(wouldFail('voltage', { min: '', max: '' }, rail)).toBeNull()
    expect(wouldFail('max_current', { amps: '0.1' }, { value: 0.3, min: null, max: null })).toBe(true)
    expect(wouldFail('toggle', { freq_hz: '1' }, { value: 1.1, min: null, max: null })).toBe(false)
  })
})

describe('the spec as offers read it', () => {
  test('counts assertions, finds scenarios, and strips the portable header', () => {
    const toml = 'board = "../hardware/x.kicad_pcb"\nfirmware = "../firmware/x.hex"\nname = "n"\n\n[[scenario]]\nid = "boot"\n\n[[assert]]\nkind = "voltage"\n\n[[assert]]\nkind = "voltage"\n'
    expect(countAsserts(toml)).toBe(2)
    expect(scenarioIds(toml)).toEqual(['boot'])
    expect(specBody(toml)).toBe('name = "n"\n\n[[scenario]]\nid = "boot"\n\n[[assert]]\nkind = "voltage"\n\n[[assert]]\nkind = "voltage"\n')
    expect(needMet('scenario', { toml, hasFirmware: true })).toBe(true)
    expect(needMet('ac', { toml, hasFirmware: true })).toBe(false)
    expect(needMet('firmware', { toml, hasFirmware: false })).toBe(false)
  })
})
